//! FrameCompositor — YUV 上传、叠加层合成与节拍状态。
//!
//! 由 `ExternalRenderer`（external_surface）与 `dyl-player`（host_compositor）
//! 共用：GPU 资源只由持有者所在
//! 线程访问（external 是渲染线程，host 是 shell 主线程），本类型不碰 Surface，
//! 也不决定 present 时机。调用方负责 acquire / submit / present，`render_tick`
//! 只把视频与叠加层画进传入的 encoder。

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::Arc;

use dyl_core::{DylError, Result};
use dyl_ffmpeg::{PixelFormat, VideoFrame};
use wgpu;

use crate::clock::OverlayClock;
use crate::overlay::{Overlay, OverlayArea, OverlayFrame, OverlayStack};
use crate::video_output::{OutputStats, OutputStatsSnapshot};
use crate::yuv_pipeline::{color_params_bytes, interleave_uv_planes, setup_yuv_pipeline};

/// pending 帧队列深度：一个节拍只用一帧，多余的帧保最新
pub const FRAME_QUEUE_CAPACITY: usize = 2;

/// 视频帧尺寸是否落在设备纹理上限内（ADR-14）
///
/// 合成通路按视频原生分辨率建 Y/UV 纹理，既不缩放也不分块，所以超限必须是
/// `render` 错误而不是悄悄把画面缩小。external 与 host 两条路径都调这个判断，
/// 天花板同源于 `device.limits()`，不存在一条路能播、另一条路不能播。
/// UV 平面是 `width/2 x height/2`，因此绑定约束的永远是 Y 的边长。
pub fn frame_size_error(width: u32, height: u32, max_dimension: u32) -> Option<DylError> {
    if max_dimension == 0 {
        // 设备还没创建（无 Surface 的输出通路），无从判断
        return None;
    }
    if width > max_dimension || height > max_dimension {
        return Some(DylError::Render(format!(
            "video frame {width}x{height} exceeds the device texture limit {max_dimension}x{max_dimension}; \
             the compositor neither scales nor splits frames"
        )));
    }
    None
}

struct FrameQueue {
    frames: VecDeque<VideoFrame>,
}

impl FrameQueue {
    fn new() -> Self {
        Self {
            frames: VecDeque::with_capacity(FRAME_QUEUE_CAPACITY),
        }
    }

    /// 队列满时丢最旧的一帧，返回是否发生了丢弃
    fn push(&mut self, frame: VideoFrame) -> bool {
        let dropped = if self.frames.len() == FRAME_QUEUE_CAPACITY {
            self.frames.pop_front();
            true
        } else {
            false
        };
        self.frames.push_back(frame);
        dropped
    }

    fn pop(&mut self) -> Option<VideoFrame> {
        self.frames.pop_front()
    }

    fn len(&self) -> usize {
        self.frames.len()
    }

    fn clear(&mut self) {
        self.frames.clear();
    }
}

pub struct FrameCompositor {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_format: wgpu::TextureFormat,
    render_pipeline: wgpu::RenderPipeline,
    y_texture: wgpu::Texture,
    uv_texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    color_params: wgpu::Buffer,
    video_width: u32,
    video_height: u32,
    /// 显示尺寸（SAR 已应用）；几何与叠加层用这份，纹理仍按编码尺寸
    display_width: u32,
    display_height: u32,
    pending_frames: FrameQueue,
    last_frame: Option<VideoFrame>,
    overlay_stack: OverlayStack,
    overlay_clock: Arc<OverlayClock>,
    stats: Arc<OutputStats>,
    /// 本合成器（= 本 wgpu 设备）的代号，叠加层据此丢弃上一设备的缓存
    gpu_epoch: u64,
    /// 上一次绘制的叠加层时间，用于跳过静止帧
    last_pts: f64,
    /// 内容变化（加载字幕/弹幕等）强制重绘一次
    invalidated: bool,
}

impl FrameCompositor {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        surface_format: wgpu::TextureFormat,
        overlay_clock: Arc<OverlayClock>,
        stats: Arc<OutputStats>,
    ) -> Self {
        // 1x1 占位纹理；真实尺寸在首帧上传时重建
        let (render_pipeline, bind_group, y_texture, uv_texture, color_params) =
            setup_yuv_pipeline(device, surface_format, 1, 1, "compositor");
        Self {
            device: device.clone(),
            queue: queue.clone(),
            surface_format,
            render_pipeline,
            y_texture,
            uv_texture,
            bind_group,
            color_params,
            video_width: 0,
            video_height: 0,
            display_width: 0,
            display_height: 0,
            pending_frames: FrameQueue::new(),
            last_frame: None,
            overlay_stack: OverlayStack::new(),
            overlay_clock,
            stats,
            gpu_epoch: crate::overlay::next_gpu_epoch(),
            last_pts: 0.0,
            invalidated: false,
        }
    }

    pub fn set_overlays(&mut self, overlays: Vec<Box<dyn Overlay>>) {
        self.overlay_stack = OverlayStack::new();
        for overlay in overlays {
            self.overlay_stack.push(overlay);
        }
        self.invalidated = true;
    }

    /// 收下 pipeline 提交的视频帧，等下一个节拍再上屏；不在此处 reanchor
    ///
    /// 超过设备纹理上限的帧直接拒绝：宁可报错，也不建一张建不出来的纹理，
    /// 也不把画面悄悄缩放（ADR-14）
    pub fn enqueue_frame(&mut self, frame: VideoFrame) -> Result<()> {
        if let Some(err) = frame_size_error(
            frame.width,
            frame.height,
            self.device.limits().max_texture_dimension_2d,
        ) {
            return Err(err);
        }
        if self.pending_frames.push(frame) {
            self.stats.record_dropped();
        }
        Ok(())
    }

    /// 强制下一拍重绘（加载字幕/弹幕、窗口内容变化等）
    pub fn invalidate(&mut self) {
        self.invalidated = true;
    }

    /// 视频显示分辨率（像素宽高比 SAR 已应用）；0x0 表示尚未收到任何帧
    ///
    /// 纹理与上传仍按编码分辨率（`frame.width/height`），这里只回答
    /// "画面该按什么宽高比摆放"——非方形像素的视频两者不相等。
    pub fn video_size(&self) -> (u32, u32) {
        (self.display_width, self.display_height)
    }

    pub fn has_frame(&self) -> bool {
        self.last_frame.is_some()
    }

    pub fn pending_len(&self) -> usize {
        self.pending_frames.len()
    }

    /// 是否需要一次 acquire + present
    pub fn needs_present(&self) -> bool {
        if self.invalidated || !self.pending_frames.frames.is_empty() {
            return true;
        }
        if self.last_frame.is_none()
            || self.overlay_stack.is_empty()
            || !self.overlay_clock.is_live()
        {
            return false;
        }
        self.overlay_clock.now_pts() != self.last_pts
    }

    /// 把视频与叠加层画进 `encoder`；返回是否真的记录了绘制。
    ///
    /// 新帧先上传再绘制；没有新帧时按挂钟外推叠加层时间，静止则返回 `false`。
    pub fn render_tick(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        area: OverlayArea,
    ) -> bool {
        let pts = match self.pending_frames.pop() {
            Some(frame) => {
                self.upload_frame(&frame);
                let frame_pts = frame.pts_secs().unwrap_or(self.last_pts);
                self.last_frame = Some(frame);
                self.overlay_clock.now_pts().max(frame_pts)
            }
            None if self.invalidated => self.overlay_clock.now_pts().max(self.last_pts),
            None => {
                if self.last_frame.is_none()
                    || self.overlay_stack.is_empty()
                    || !self.overlay_clock.is_live()
                {
                    return false;
                }
                let pts = self.overlay_clock.now_pts();
                if pts == self.last_pts {
                    return false;
                }
                pts
            }
        };

        self.render_scene(encoder, view, area, pts);
        self.last_pts = pts;
        self.invalidated = false;
        self.stats.record_rendered();
        true
    }

    pub fn freeze_overlay_clock(&self) {
        self.overlay_clock.freeze();
    }

    pub fn stats(&self) -> OutputStatsSnapshot {
        self.stats.snapshot()
    }

    /// Surface 分离：丢弃上一帧，避免在已失效的窗口上重绘
    pub fn clear_frame(&mut self) {
        self.pending_frames.clear();
        self.last_frame = None;
        self.last_pts = 0.0;
        self.invalidated = false;
    }

    fn upload_frame(&mut self, frame: &VideoFrame) {
        if frame.width != self.video_width || frame.height != self.video_height {
            self.video_width = frame.width;
            self.video_height = frame.height;
            let (pipeline, bg, yt, uvt, cp) = setup_yuv_pipeline(
                &self.device,
                self.surface_format,
                frame.width,
                frame.height,
                "compositor",
            );
            self.render_pipeline = pipeline;
            self.bind_group = bg;
            self.y_texture = yt;
            self.uv_texture = uvt;
            self.color_params = cp;
        }
        // 显示尺寸可能只随 SAR 变化（编码尺寸不变），每次上帧都同步
        self.display_width = if frame.display_width > 0 {
            frame.display_width
        } else {
            frame.width
        };
        self.display_height = if frame.display_height > 0 {
            frame.display_height
        } else {
            frame.height
        };

        // 矩阵系数与量程每帧更新：写错是画质错误而不是抖动
        self.queue
            .write_buffer(&self.color_params, 0, &color_params_bytes(frame));

        if let Some(y_plane) = frame.planes.first() {
            let (y_data, y_pitch) = aligned_rows(
                &y_plane.data,
                y_plane.stride,
                frame.width as usize,
                frame.height as usize,
            );
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.y_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &y_data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(y_pitch as u32),
                    rows_per_image: Some(frame.height),
                },
                wgpu::Extent3d {
                    width: frame.width,
                    height: frame.height,
                    depth_or_array_layers: 1,
                },
            );
        }

        match frame.format {
            PixelFormat::Nv12 => {
                if let Some(uv_plane) = frame.planes.get(1) {
                    let (uv_data, uv_pitch) = aligned_rows(
                        &uv_plane.data,
                        uv_plane.stride,
                        (frame.width / 2) as usize * 2,
                        (frame.height / 2) as usize,
                    );
                    self.queue.write_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture: &self.uv_texture,
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        &uv_data,
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(uv_pitch as u32),
                            rows_per_image: Some(frame.height / 2),
                        },
                        wgpu::Extent3d {
                            width: frame.width / 2,
                            height: frame.height / 2,
                            depth_or_array_layers: 1,
                        },
                    );
                }
            }
            _ => {
                let (u_data, u_stride) = frame
                    .planes
                    .get(1)
                    .map(|p| (p.data.as_slice(), p.stride))
                    .unwrap_or((&[], 0));
                let (v_data, v_stride) = frame
                    .planes
                    .get(2)
                    .map(|p| (p.data.as_slice(), p.stride))
                    .unwrap_or((&[], 0));
                let uv_w = (frame.width / 2) as usize;
                let uv_h = (frame.height / 2) as usize;
                let uv_interleaved =
                    interleave_uv_planes(u_data, u_stride, v_data, v_stride, uv_w, uv_h);
                // 交织结果是紧凑行（pitch = 宽度），仍需补齐到 wgpu 的行对齐
                let (uv_data, uv_pitch) = aligned_rows(&uv_interleaved, uv_w * 2, uv_w * 2, uv_h);
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &self.uv_texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &uv_data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(uv_pitch as u32),
                        rows_per_image: Some(frame.height / 2),
                    },
                    wgpu::Extent3d {
                        width: frame.width / 2,
                        height: frame.height / 2,
                        depth_or_array_layers: 1,
                    },
                );
            }
        }
    }

    fn render_scene(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        area: OverlayArea,
        pts: f64,
    ) {
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("compositor_video_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
            if self.video_width > 0 && self.video_height > 0 {
                let (x, y, w, h) = area.video_rect;
                pass.set_pipeline(&self.render_pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_viewport(x, y, w, h, 0.0, 1.0);
                pass.draw(0..4, 0..1);
            }
        }

        self.overlay_stack.update_all(pts);
        if !self.overlay_stack.is_empty() {
            let frame = OverlayFrame {
                device: &self.device,
                queue: &self.queue,
                target: view,
                surface_format: self.surface_format,
                gpu_epoch: self.gpu_epoch,
            };
            self.overlay_stack.render_all(&frame, encoder, area);
        }
    }
}

/// 把平面行宽补齐到 wgpu 要求的 `bytes_per_row` 对齐（256 的倍数）
///
/// FFmpeg 只把 linesize 对齐到 32/64，所以 1080p 的 Y 行宽正好是 1920 —— 不是
/// 256 的倍数，直接交给 `write_texture` 会触发校验失败。已对齐时零拷贝返回。
fn aligned_rows(
    data: &[u8],
    stride: usize,
    row_bytes: usize,
    rows: usize,
) -> (Cow<'_, [u8]>, usize) {
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;
    let pitch = stride.max(row_bytes).div_ceil(align) * align;
    if pitch == stride {
        return (Cow::Borrowed(data), stride);
    }
    let mut padded = vec![0u8; pitch * rows];
    for row in 0..rows {
        let src = row * stride;
        let end = (src + row_bytes).min(data.len());
        if src < end {
            padded[row * pitch..row * pitch + (end - src)].copy_from_slice(&data[src..end]);
        }
    }
    (Cow::Owned(padded), pitch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dyl_ffmpeg::{ColorPrimaries, ColorRange, ColorSpace};

    fn frame(pts: Option<i64>) -> VideoFrame {
        VideoFrame {
            width: 2,
            height: 2,
            display_width: 2,
            display_height: 2,
            format: PixelFormat::Nv12,
            color_space: ColorSpace::Unspecified,
            color_primaries: ColorPrimaries::Unspecified,
            color_range: ColorRange::Unspecified,
            planes: vec![],
            pts,
            time_base: dyl_ffmpeg::time_base(1, 1000),
        }
    }

    /// 1080p 的 Y 行宽是 1920（FFmpeg 只按 32/64 对齐），不补齐会被 wgpu 拒绝
    #[test]
    fn unaligned_rows_are_padded_to_wgpu_alignment() {
        let stride = 1920usize;
        let rows = 4usize;
        let mut data = vec![0u8; stride * rows];
        for (r, row) in data.chunks_mut(stride).enumerate() {
            row[0] = 10 + r as u8;
            row[1919] = 200;
        }

        let (padded, pitch) = aligned_rows(&data, stride, 1920, rows);
        assert_eq!(pitch, 2048);
        assert_eq!(padded.len(), 2048 * rows);
        for (r, row) in padded.chunks_exact(pitch).enumerate() {
            assert_eq!(row[0], 10 + r as u8, "行首像素被错位");
            assert_eq!(row[1919], 200, "行尾像素被错位");
            assert!(row[1920..].iter().all(|&b| b == 0), "填充区必须为 0");
        }
    }

    /// 已经对齐的行不能白白拷一遍
    #[test]
    fn aligned_rows_are_passed_through_without_copy() {
        let data = vec![7u8; 1536 * 2];
        let (out, pitch) = aligned_rows(&data, 1536, 1470, 2);
        assert_eq!(pitch, 1536);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(&*out, &data[..]);
    }

    #[test]
    fn frame_queue_keeps_newest_and_counts_overflow() {
        let mut queue = FrameQueue::new();
        assert!(!queue.push(frame(Some(1))));
        assert!(!queue.push(frame(Some(2))));
        assert!(queue.push(frame(Some(3))));
        assert_eq!(queue.len(), FRAME_QUEUE_CAPACITY);
        assert_eq!(queue.pop().and_then(|f| f.pts), Some(2));
        assert_eq!(queue.pop().and_then(|f| f.pts), Some(3));
        assert!(queue.pop().is_none());
    }

    /// ADR-14：帧尺寸超出设备纹理上限时报错，而不是缩放或让 wgpu 在建纹理时炸
    #[test]
    fn oversized_frame_is_reported_not_scaled_down() {
        assert!(frame_size_error(1920, 1080, 2048).is_none());
        assert!(
            frame_size_error(2048, 2048, 2048).is_none(),
            "边界内等于上限"
        );
        let err = frame_size_error(3840, 2160, 2048).expect("4K over a 2048 ceiling");
        assert!(matches!(err, DylError::Render(_)), "{err}");
        // 绑定约束的是 Y 平面边长；UV 是半尺寸，永远不会先越界
        assert!(frame_size_error(4096, 8, 2048).is_some());
        assert!(frame_size_error(8, 4096, 2048).is_some());
        // 没有设备（NullOutput 通路）时无从判断，不能因此拒绝帧
        assert!(frame_size_error(3840, 2160, 0).is_none());
    }

    #[test]
    fn output_stats_are_shared_through_arc() {
        let stats = Arc::new(OutputStats::new());
        let clone = Arc::clone(&stats);
        clone.record_rendered();
        clone.add_dropped(2);
        assert_eq!(
            stats.snapshot(),
            OutputStatsSnapshot {
                rendered_frames: 1,
                dropped_frames: 2
            }
        );
    }
}

//! 无头 GPU 冒烟测试：验证字幕 overlay 管线（含 alpha uniform）真正可渲染，
//! 以及 \move 定位与 \fad 淡入淡出在像素层面的效果。
//! 无可用 GPU / 字体时软跳过。

use dyl_render::{Overlay, OverlayArea};
use dyl_subtitle::{load_system_font, AssEvent, AssParser, SubtitleConfig, SubtitleEngine};

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const W: u32 = 320;
const H: u32 = 240;

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// 与 `FrameCompositor` 一样，每次创建设备发一个新代号
    epoch: u64,
}

fn try_gpu() -> Option<Gpu> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        flags: wgpu::InstanceFlags::default(),
        memory_budget_thresholds: Default::default(),
        backend_options: Default::default(),
        display: None,
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::default(),
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .ok()?;
    let usages = adapter.get_texture_format_features(FORMAT).allowed_usages;
    if !usages.contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
        || !usages.contains(wgpu::TextureUsages::COPY_SRC)
    {
        return None;
    }
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("subtitle_smoke_device"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        experimental_features: Default::default(),
        memory_hints: Default::default(),
        trace: Default::default(),
    }))
    .ok()?;
    Some(Gpu {
        device,
        queue,
        epoch: dyl_render::next_gpu_epoch(),
    })
}

fn engine_with_ass(src: &str) -> Option<SubtitleEngine> {
    let mut engine = SubtitleEngine::new(SubtitleConfig {
        canvas_width: W,
        canvas_height: H,
        ..Default::default()
    });
    let font = load_system_font()?;
    engine.set_font(font);
    let doc = AssParser::new().parse_document(src.as_bytes());
    {
        let shared = engine.shared_state();
        let mut st = shared.lock();
        st.events = doc.events.iter().map(|e| e.base.clone()).collect();
        st.ass_events = doc.events;
        st.styles = doc.styles;
        st.play_res = (doc.play_res_x, doc.play_res_y);
    }
    Some(engine)
}

/// 渲染一帧到离屏纹理，返回 (非零 alpha 像素数, 最左非零像素 x 坐标)
fn render_frame(engine: &mut SubtitleEngine, gpu: &Gpu, pts: f64) -> (usize, Option<u32>) {
    engine.update(pts);
    let dbg_text = engine.shared_state().lock().current_text.clone();
    assert!(
        !dbg_text.is_empty() || pts > 5.5,
        "no active items at pts={pts}"
    );

    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("smoke_target"),
        size: wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("smoke_encoder"),
        });
    // 无视频信息（video=0）→ 画面矩形退化到全屏，字号系数回落到 1.0
    let frame = dyl_render::OverlayFrame {
        device: &gpu.device,
        queue: &gpu.queue,
        target: &view,
        surface_format: FORMAT,
        gpu_epoch: gpu.epoch,
    };
    engine.render(&frame, &mut encoder, OverlayArea::new(W, H, 0, 0));

    let bytes_per_row = W * 4; // 1280，满足 256 对齐
    let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("smoke_readback"),
        size: u64::from(bytes_per_row) * u64::from(H),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(H),
            },
        },
        wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
    );
    gpu.queue.submit(std::iter::once(encoder.finish()));

    let slice = buffer.slice(..);
    let (sender, receiver) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = sender.send(r);
    });
    let _ = gpu.device.poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: None,
    });
    receiver.recv().unwrap().expect("map readback");
    let data = slice.get_mapped_range();

    let mut painted = 0usize;
    let mut min_x: Option<u32> = None;
    for row in data.chunks(bytes_per_row as usize) {
        for (x, px) in row.chunks(4).enumerate() {
            if px[3] > 0 {
                painted += 1;
                min_x = Some(min_x.map_or(x as u32, |m| m.min(x as u32)));
            }
        }
    }
    drop(data);
    buffer.unmap();
    (painted, min_x)
}

#[test]
fn gpu_smoke_static_and_inactive() {
    let Some(gpu) = try_gpu() else { return };
    let Some(mut engine) = engine_with_ass(
        r#"[Script Info]
PlayResX: 320
PlayResY: 240

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:05.00,Default,,0,0,0,,字幕测试
"#,
    ) else {
        return;
    };
    let (painted, _) = render_frame(&mut engine, &gpu, 2.0);
    assert!(painted > 50, "expected glyph pixels, got {painted}");
    let (painted, _) = render_frame(&mut engine, &gpu, 6.0);
    assert_eq!(painted, 0, "inactive subtitle must not paint");
}

#[test]
fn gpu_smoke_move_shifts_pixels_right() {
    let Some(gpu) = try_gpu() else { return };
    let Some(mut engine) = engine_with_ass(
        r#"[Script Info]
PlayResX: 320
PlayResY: 240

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:00.00,0:00:10.00,Default,,0,0,0,,{\an7\move(10,100,250,100)}i
"#,
    ) else {
        return;
    };
    let (_, left_early) = render_frame(&mut engine, &gpu, 0.5);
    let (_, left_late) = render_frame(&mut engine, &gpu, 9.0);
    assert!(
        left_early.is_some() && left_late.is_some(),
        "both frames must paint the glyph"
    );
    assert!(
        left_late > left_early,
        "moved glyph must shift right: {left_early:?} -> {left_late:?}"
    );
}

#[test]
fn gpu_smoke_fade_in_visible_after_invisible() {
    let Some(gpu) = try_gpu() else { return };
    let Some(mut engine) = engine_with_ass(
        r#"[Script Info]
PlayResX: 320
PlayResY: 240

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:05.00,Default,,0,0,0,,{\fad(2000,0)}fade
"#,
    ) else {
        return;
    };
    // t=start：alpha=0 → 完全不可见
    let (painted, _) = render_frame(&mut engine, &gpu, 1.0);
    assert_eq!(painted, 0, "alpha 0 must not paint");
    // 淡入中段：可见
    let (painted, _) = render_frame(&mut engine, &gpu, 2.0);
    assert!(painted > 50, "faded-in text must paint, got {painted}");
}

/// 回归：Surface 重建会换一个 `wgpu::Device`，共享的叠加层引擎不能把旧设备的
/// 管线/纹理带过去
///
/// 没有按设备失效缓存时，第二次 `render` 会在新设备上 `set_pipeline` 旧管线，
/// wgpu 直接报 `RenderPipeline does not exist`（surface 重新 attach 时必现）。
#[test]
fn gpu_smoke_engine_survives_device_change() {
    let Some(gpu_a) = try_gpu() else { return };
    let Some(mut engine) = engine_with_ass(
        r#"[Script Info]
PlayResX: 320
PlayResY: 240

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:05.00,Default,,0,0,0,,字幕测试
"#,
    ) else {
        return;
    };
    let (painted, _) = render_frame(&mut engine, &gpu_a, 2.0);
    assert!(painted > 50, "expected glyph pixels, got {painted}");

    // 第二个设备模拟 surface detach/attach 后的新渲染器；同一份文本命中旧缓存，
    // 正好覆盖"旧纹理被绑进新设备 bind group"的路径
    let Some(gpu_b) = try_gpu() else { return };
    assert_ne!(gpu_a.epoch, gpu_b.epoch);
    let (painted, _) = render_frame(&mut engine, &gpu_b, 2.1);
    assert!(
        painted > 50,
        "engine must rebuild device-bound resources, got {painted}"
    );
}

/// AssEvent 段折叠不依赖样式表缺失崩溃
#[test]
fn missing_style_falls_back_without_panic() {
    let src = r#"[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:05.00,Nonexistent,,0,0,0,,{\an5}orphan style
"#;
    let doc = AssParser::new().parse_document(src.as_bytes());
    assert!(doc.styles.is_empty());
    let ev: &AssEvent = &doc.events[0];
    assert_eq!(ev.base.text, "orphan style");
}

/// 行内 `{\r样式名}` 拆出来的多块位图必须真的走通 GPU
///
/// 逐行成图后拼成一张纹理：两行的像素都要在（否则是拼接把某块丢了），并且样式
/// 点了系统里根本没有的 `Fontname` 时只回退内置字体，不能画成空白或触发校验错误。
#[test]
fn gpu_smoke_inline_style_reset_paints_both_lines() {
    let Some(gpu) = try_gpu() else { return };
    let head = "[Script Info]\nPlayResX: 320\nPlayResY: 240\n\n\
[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, OutlineColour, Outline, Shadow, Alignment, MarginL, MarginR, MarginV\n\
Style: Default,Dyl Font That Does Not Exist,20,&H00FFFFFF,&H00000000,2,0,2,10,10,10\n\
Style: Sign,Arial,28,&H000000FF,&H00000000,2,0,2,10,10,10\n\n\
[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n";

    let painted_of = |src: &str, pts: f64| -> usize {
        let Some(mut engine) = engine_with_ass(src) else {
            return 0;
        };
        let (n, min_x) = render_frame(&mut engine, &gpu, pts);
        assert!(n > 20, "nothing painted for {src:?}");
        assert!(min_x.is_some(), "no pixel column for {src:?}");
        n
    };

    let one = head.to_owned() + "Dialogue: 0,0:00:01.00,0:00:05.00,Default,,0,0,10,,上白\n";
    let two = head.to_owned() + "Dialogue: 0,0:00:01.00,0:00:05.00,Sign,,0,0,10,,下红\n";
    let mixed =
        head.to_owned() + r"Dialogue: 0,0:00:01.00,0:00:05.00,Default,,0,0,10,,上白{\rSign}\N下红";

    let p_one = painted_of(&one, 2.0);
    let p_two = painted_of(&two, 2.0);
    let p_mixed = painted_of(&mixed, 2.0);

    assert!(
        p_mixed > p_one.max(p_two),
        "换样式的第二行没画上：mixed={p_mixed} 单行={p_one}/{p_two}"
    );
    // 两块的像素叠在一起，只会被抗锯齿/描边交叠吃掉一点
    assert!(
        p_mixed as f64 >= (p_one + p_two) as f64 * 0.7,
        "mixed={p_mixed} 远小于两行之和 {}",
        p_one + p_two
    );
}

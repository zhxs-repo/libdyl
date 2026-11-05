//! 由 `dyl-render` 声明的能力值（规范 ADR-13）。
//!
//! 这些值是 GPU 后端、原生 Surface 句柄与纹理尺寸的实际实现结果，所以放在渲染 crate 里，
//! 不在 `dyl-core` 的契约类型里抄一份清单：契约只定义形状，值跟着代码走。

use dyl_core::contract::SurfaceKind;

/// 本构建编译进来的 wgpu 后端，按声明顺序输出。
///
/// 取自 `wgpu::Instance::enabled_backend_features()` —— 它是编译期常量，由 `Cargo.toml`
/// 的 wgpu features 决定，因此关掉某个 feature，能力 JSON 会自动少掉对应一项，不存在
/// 手抄清单与实际构建漂移的空间。`Backend::Gl` 在契约词表里叫 `gles`。
pub fn gpu_backends() -> Vec<String> {
    const TABLE: &[(wgpu::Backends, &str)] = &[
        (wgpu::Backends::VULKAN, "vulkan"),
        (wgpu::Backends::METAL, "metal"),
        (wgpu::Backends::DX12, "dx12"),
        (wgpu::Backends::GL, "gles"),
        (wgpu::Backends::BROWSER_WEBGPU, "webgpu"),
    ];
    let enabled = wgpu::Instance::enabled_backend_features();
    TABLE
        .iter()
        .filter(|(backends, _)| enabled.contains(*backends))
        .map(|(_, name)| (*name).to_string())
        .collect()
}

/// 本平台的原生句柄能否被 [`external_renderer`](crate::external_renderer) 翻译成一个
/// `wgpu::Surface`。
///
/// `ExternalRenderer::create_surface_from_handle` 的 match 覆盖全部五种句柄，但只有当前
/// 平台那一种真的可用（其余句柄在 attach 时失败），所以能力位仍按平台收窄。
pub fn supported_surface_kinds() -> Vec<SurfaceKind> {
    match std::env::consts::OS {
        "windows" => vec![SurfaceKind::WindowsHwnd],
        "macos" => vec![SurfaceKind::MacosNsview],
        "linux" => vec![SurfaceKind::X11Window],
        "android" => vec![SurfaceKind::AndroidNativeWindow],
        _ => vec![],
    }
}

/// 原生子视图嵌入：external_surface 模式由内核在宿主句柄上建 swapchain，恒为真。
pub const EMBEDS_NATIVE_VIEW: bool = true;

/// 是否允许降级为独立顶层窗口。Android 的 `SurfaceView` 生命周期由 Java 侧决定，
/// 没有这条退路；桌面平台可以在嵌入失败时另开窗口。
pub const ALLOWS_DETACHED_WINDOW: bool = !cfg!(target_os = "android");

/// 字幕/弹幕由内核画在视频 Surface 内（`FrameCompositor` 的叠加通道），
/// 宿主无法把 UI 控件叠在视频之上。
pub const KERNEL_RENDERS_OVERLAYS: bool = true;

/// 内核是否提供 `external_surface` 呈现模式（本 crate 的 `ExternalSurfaceOutput`）。
pub const EXTERNAL_SURFACE: bool = true;

/// Flutter Texture 共享：产品决策里明确不做（规范 2.2），没有 `texture_id` 路径。
pub const TEXTURE_SHARING: bool = false;

/// 两条呈现路径都至少能保证的纹理边长下界。
///
/// 写成从 `downlevel_defaults()` 推导，而不是抄一个 `2048` 常量：external 路径的设备以它为
/// 特性基线创建（见 `external_renderer::device_limits`），因此能力值与真实下限同源。
/// 实际上限按 adapter 上调，所以这个数字是**保证的下界**，不是产品天花板，也不是拒收大帧的依据。
pub const GUARANTEED_MAX_TEXTURE_DIMENSION_2D: u32 =
    wgpu::Limits::downlevel_defaults().max_texture_dimension_2d;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_backends_is_derived_from_compiled_features() {
        let backends = gpu_backends();
        assert!(
            !backends.is_empty(),
            "wgpu 至少要编译进一个后端，否则能力位会谎报为零"
        );
        assert!(
            backends.iter().all(|name| !name.is_empty()),
            "契约词表不允许空串"
        );
        // `Backend::Gl` 必须输出契约用的名字，而不是 wgpu 自己的 "gl"
        if backends.contains(&"gles".to_string()) {
            assert!(!backends.contains(&"gl".to_string()));
        }
        // 顺序稳定：同一构建两次调用必须逐字节相同
        assert_eq!(backends, gpu_backends());
    }

    #[test]
    fn surface_kinds_is_exactly_one_per_supported_platform() {
        let kinds = supported_surface_kinds();
        if matches!(
            std::env::consts::OS,
            "windows" | "macos" | "linux" | "android"
        ) {
            assert_eq!(kinds.len(), 1, "每种平台只声明自己能创建的那一种句柄");
        } else {
            assert!(kinds.is_empty());
        }
    }

    #[test]
    fn guaranteed_dimension_matches_the_limits_the_renderer_requests() {
        // 能力位声明的下界不得超过 external 路径真正请求的设备上限
        let adapter_limits = wgpu::Limits::default();
        let requested = crate::external_renderer::device_limits(adapter_limits);
        assert!(
            requested.max_texture_dimension_2d >= GUARANTEED_MAX_TEXTURE_DIMENSION_2D,
            "device_limits 若把分辨率压到保证下界以下，能力位就成了谎报"
        );
    }
}

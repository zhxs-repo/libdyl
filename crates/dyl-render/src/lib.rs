pub mod capabilities;
pub mod clock;
pub mod compositor;
pub mod external_renderer;
pub mod font_util;
pub mod overlay;
pub mod video_output;
pub mod wgpu_instance;
mod yuv_pipeline;

pub use clock::OverlayClock;
pub use compositor::{frame_size_error, FrameCompositor, FRAME_QUEUE_CAPACITY};
pub use external_renderer::ExternalRenderer;
pub use font_util::extract_font_from_ttc;
pub use overlay::{
    fit_video_rect, next_gpu_epoch, Overlay, OverlayArea, OverlayFrame, OverlayStack,
    SharedOverlay, VideoRect,
};
pub use video_output::{
    NativeSurfaceKind, NullOutput, OutputSlot, OutputStats, OutputStatsSnapshot, SurfaceHandle,
    VideoOutput,
};
pub use wgpu_instance::install_nonfatal_error_handler;

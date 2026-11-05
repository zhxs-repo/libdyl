//! Dyl 播放内核 — 编排层
//!
//! 持有状态机、pipeline 生命周期、字幕/弹幕引擎与 Surface 生命周期。
//! C ABI 适配留在 `dyl-ffi`，本 crate 不依赖任何 FFI 类型。

pub mod capabilities;
mod control;
mod engine;
mod error;
mod events;
mod load;
mod output;
mod overlays;
mod state;

pub use dyl_ffmpeg::VideoFrame;
pub use engine::Engine;
pub use error::EngineError;
pub use output::{HostFrameSink, HostOutputHandles};
pub use overlays::OverlayHandles;

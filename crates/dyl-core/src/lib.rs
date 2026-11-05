//! Dyl 播放内核的纯类型层。
//!
//! 公开契约（命令、事件、快照、Surface、能力、错误码）全部在 [`contract`]，
//! 其余类型只服务内部解码/渲染管线。

pub mod contract;
pub mod error;
pub mod event;

pub use error::{DylError, Result};
pub use event::{AudioInfo, VideoInfo};

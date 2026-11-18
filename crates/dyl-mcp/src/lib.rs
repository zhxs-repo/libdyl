//! Dyl MCP Server — 为 agent 提供播放内核调试工具
//!
//! 通过 MCP 协议暴露 Engine 的命令、快照、诊断探针与事件流，
//! 使 agent 能在进程内直接操控与观测播放器内核，无需编译/运行独立二进制。

pub mod server;
pub mod session;
pub mod tools;
pub mod trace_buffer;

pub use server::DylServer;
pub use session::SessionManager;
pub use tools::DylTools;

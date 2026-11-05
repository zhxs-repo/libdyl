//! Pipeline 内部事件 — 只面向引擎，不是对外协议
//!
//! 工作线程不写状态、不发公共事件；它们把播放结束、seek 应用完成和致命错误
//! 报告给引擎控制线程，由引擎决定状态转换、快照与契约事件。

use dyl_core::contract::ErrorCode;

/// 会话标识 + 事件。引擎用 session 丢弃重载后迟到的旧管线事件。
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineEventMsg {
    pub session: u64,
    pub event: PipelineEvent,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PipelineEvent {
    /// 播放结束（EOF）
    Ended,
    /// demuxer 已接受并应用 seek；generation 与 `seek_generation` 对应，
    /// tag 为引擎下发请求时携带的不透明标记
    SeekApplied {
        tag: u64,
        generation: u64,
        position_secs: f64,
    },
    /// seek 请求未能应用；引擎据此以错误结局完成对应命令，但不进入 error 状态
    SeekFailed { tag: u64, message: String },
    /// 致命错误：线程失败或看门狗判定停摆
    Error { code: ErrorCode, message: String },
}

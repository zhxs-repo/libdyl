//! Engine 错误类型 — 统一成契约错误码，供 FFI 映射为数字返回值。

use dyl_core::contract::ErrorCode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineError {
    pub code: ErrorCode,
    pub message: String,
}

impl EngineError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub(crate) fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArgument, message)
    }

    pub(crate) fn invalid_state(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidState, message)
    }
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for EngineError {}

impl From<dyl_core::DylError> for EngineError {
    fn from(e: dyl_core::DylError) -> Self {
        Self::new(ErrorCode::from(&e), e.to_string())
    }
}

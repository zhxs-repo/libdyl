use thiserror::Error;

/// Dyl 错误类型
///
/// 契约错误码见 [`crate::contract::ErrorCode`]；引擎负责把这里的变体映射为
/// 契约错误码，`dyl-core` 不再维护第二套错误码枚举。
#[derive(Debug, Error)]
pub enum DylError {
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("invalid state: {0}")]
    InvalidState(String),
    #[error("file not found: {0}")]
    FileNotFound(String),
    #[error("demux error: {0}")]
    Demux(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("render error: {0}")]
    Render(String),
    #[error("audio error: {0}")]
    Audio(String),
    #[error("pipeline error: {0}")]
    Pipeline(String),
    /// 能力性拒绝，与「参数不合法」不同：请求本身写得对，但内核做不到
    #[error("not supported: {0}")]
    NotSupported(String),
    #[error("internal error: {0}")]
    Internal(String),
}

/// Dyl Result 类型
pub type Result<T> = std::result::Result<T, DylError>;

impl From<&DylError> for crate::contract::ErrorCode {
    fn from(e: &DylError) -> Self {
        match e {
            DylError::InvalidArgument(_) => Self::InvalidArgument,
            DylError::InvalidState(_) => Self::InvalidState,
            DylError::FileNotFound(_) => Self::FileNotFound,
            DylError::Demux(_) => Self::Demux,
            DylError::Decode(_) => Self::Decode,
            DylError::Render(_) => Self::Render,
            DylError::Audio(_) => Self::Audio,
            DylError::Pipeline(_) => Self::Pipeline,
            DylError::NotSupported(_) => Self::NotSupported,
            DylError::Internal(_) => Self::Internal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display() {
        let e = DylError::FileNotFound("test.mp4".into());
        assert!(format!("{}", e).contains("test.mp4"));
    }
}

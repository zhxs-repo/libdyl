use serde::{Deserialize, Serialize};

/// 契约错误码。字符串形式用于事件/快照，数字形式对应 C ABI 返回值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Ok,
    InvalidArgument,
    InvalidState,
    FileNotFound,
    Demux,
    Decode,
    Render,
    Audio,
    Pipeline,
    NotSupported,
    Surface,
    Busy,
    Timeout,
    Io,
    Cancelled,
    Internal,
}

impl ErrorCode {
    pub const ALL: [Self; 16] = [
        Self::Ok,
        Self::InvalidArgument,
        Self::InvalidState,
        Self::FileNotFound,
        Self::Demux,
        Self::Decode,
        Self::Render,
        Self::Audio,
        Self::Pipeline,
        Self::NotSupported,
        Self::Surface,
        Self::Busy,
        Self::Timeout,
        Self::Io,
        Self::Cancelled,
        Self::Internal,
    ];

    pub const fn as_i32(self) -> i32 {
        match self {
            Self::Ok => 0,
            Self::InvalidArgument => -1,
            Self::InvalidState => -2,
            Self::FileNotFound => -3,
            Self::Demux => -4,
            Self::Decode => -5,
            Self::Render => -6,
            Self::Audio => -7,
            Self::Pipeline => -8,
            Self::NotSupported => -9,
            Self::Surface => -10,
            Self::Busy => -11,
            Self::Timeout => -12,
            Self::Io => -13,
            Self::Cancelled => -14,
            Self::Internal => -99,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::InvalidArgument => "invalid_argument",
            Self::InvalidState => "invalid_state",
            Self::FileNotFound => "file_not_found",
            Self::Demux => "demux",
            Self::Decode => "decode",
            Self::Render => "render",
            Self::Audio => "audio",
            Self::Pipeline => "pipeline",
            Self::NotSupported => "not_supported",
            Self::Surface => "surface",
            Self::Busy => "busy",
            Self::Timeout => "timeout",
            Self::Io => "io",
            Self::Cancelled => "cancelled",
            Self::Internal => "internal",
        }
    }

    pub const fn constant_name(self) -> &'static str {
        match self {
            Self::Ok => "DYL_OK",
            Self::InvalidArgument => "DYL_ERR_INVALID_ARGUMENT",
            Self::InvalidState => "DYL_ERR_INVALID_STATE",
            Self::FileNotFound => "DYL_ERR_FILE_NOT_FOUND",
            Self::Demux => "DYL_ERR_DEMUX",
            Self::Decode => "DYL_ERR_DECODE",
            Self::Render => "DYL_ERR_RENDER",
            Self::Audio => "DYL_ERR_AUDIO",
            Self::Pipeline => "DYL_ERR_PIPELINE",
            Self::NotSupported => "DYL_ERR_NOT_SUPPORTED",
            Self::Surface => "DYL_ERR_SURFACE",
            Self::Busy => "DYL_ERR_BUSY",
            Self::Timeout => "DYL_ERR_TIMEOUT",
            Self::Io => "DYL_ERR_IO",
            Self::Cancelled => "DYL_ERR_CANCELLED",
            Self::Internal => "DYL_ERR_INTERNAL",
        }
    }

    pub fn from_i32(code: i32) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_i32() == code)
    }
}

/// `dyl_error_string` 的字符串结果；未知码返回 `"unknown"`。
pub fn error_string(code: i32) -> &'static str {
    ErrorCode::from_i32(code)
        .map(ErrorCode::as_str)
        .unwrap_or("unknown")
}

/// `contract/errors.json` 的类型化镜像。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorCatalog {
    pub schema: u32,
    pub errors: Vec<ErrorCatalogEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorCatalogEntry {
    pub code_num: i32,
    pub code: ErrorCode,
    pub constant: String,
}

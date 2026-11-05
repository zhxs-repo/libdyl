use serde::{Deserialize, Serialize};

use super::SurfaceKind;

/// Surface 描述符。句柄一律用字符串，避免 64 位指针经 JSON number 丢精度。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SurfaceDescriptor {
    WindowsHwnd {
        schema: u32,
        handle: String,
        width: u32,
        height: u32,
        scale_factor: f64,
    },
    MacosNsview {
        schema: u32,
        handle: String,
        width: u32,
        height: u32,
        scale_factor: f64,
    },
    X11Window {
        schema: u32,
        handle: String,
        display: String,
        width: u32,
        height: u32,
        scale_factor: f64,
    },
    WaylandSurface {
        schema: u32,
        surface: String,
        display: String,
        width: u32,
        height: u32,
        scale_factor: f64,
    },
    AndroidNativeWindow {
        schema: u32,
        handle: String,
        width: u32,
        height: u32,
        scale_factor: f64,
    },
}

impl SurfaceDescriptor {
    pub const fn kind(&self) -> SurfaceKind {
        match self {
            Self::WindowsHwnd { .. } => SurfaceKind::WindowsHwnd,
            Self::MacosNsview { .. } => SurfaceKind::MacosNsview,
            Self::X11Window { .. } => SurfaceKind::X11Window,
            Self::WaylandSurface { .. } => SurfaceKind::WaylandSurface,
            Self::AndroidNativeWindow { .. } => SurfaceKind::AndroidNativeWindow,
        }
    }

    pub const fn size(&self) -> (u32, u32) {
        match self {
            Self::WindowsHwnd { width, height, .. }
            | Self::MacosNsview { width, height, .. }
            | Self::X11Window { width, height, .. }
            | Self::WaylandSurface { width, height, .. }
            | Self::AndroidNativeWindow { width, height, .. } => (*width, *height),
        }
    }

    pub const fn scale_factor(&self) -> f64 {
        match self {
            Self::WindowsHwnd { scale_factor, .. }
            | Self::MacosNsview { scale_factor, .. }
            | Self::X11Window { scale_factor, .. }
            | Self::WaylandSurface { scale_factor, .. }
            | Self::AndroidNativeWindow { scale_factor, .. } => *scale_factor,
        }
    }

    pub const fn schema(&self) -> u32 {
        match self {
            Self::WindowsHwnd { schema, .. }
            | Self::MacosNsview { schema, .. }
            | Self::X11Window { schema, .. }
            | Self::WaylandSurface { schema, .. }
            | Self::AndroidNativeWindow { schema, .. } => *schema,
        }
    }
}

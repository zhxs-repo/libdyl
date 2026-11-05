//! Android 日志桥接：tracing → logcat
//!
//! Android 上没有 `RUST_LOG`，stdout 也被 app sandbox 丢弃，所以 `tracing`
//! 的事件必须写进 logcat（`adb logcat -s dyl`）。
//!
//! 为什么要自己写 `MakeWriter`：`android_logger` 0.13 只实现了 `log::Log`
//! facade，没有 `MakeWriter`（0.12 还有，0.13 移除了）。而 `tracing_log::LogTracer`
//! 的方向是 **log → tracing**，把它设成全局 logger 只会让 `log::` 的调用变成
//! tracing 事件，`tracing::` 的事件依然走 fmt subscriber 的 stdout。
//!
//! 所以这里让 fmt subscriber 格式化后写进 `log`，由 android_logger 落到 logcat。
//! 注意**不要**再装 `LogTracer`，否则会形成
//! tracing → log → LogTracer → tracing 的无限递归。

use std::io::{self, Write};

use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::EnvFilter;

/// 安装全局日志。重复调用无害（第一次生效）。
pub fn init() {
    android_logger::init_once(
        android_logger::Config::default()
            .with_tag("dyl")
            .with_max_level(log::LevelFilter::Info),
    );

    // NOT `EnvFilter::from_default_env()`: with RUST_LOG unset (always the case
    // on Android -- there is no environment to set it in) that yields an empty
    // directive list, whose max_level_hint is OFF, so *nothing* is recorded and
    // logcat stays silent. Fall back to "info" instead.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(LogcatWriter(String::new()))
        .try_init();
}

/// 把 fmt 的输出按行转成 `log` 记录。
struct LogcatWriter(String);

impl<'a> MakeWriter<'a> for LogcatWriter {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        // fmt 每次事件都会新建 writer，行缓冲因此无法跨 writer 保留。
        // 单次 write 一般就是一整行，尾部残留的半行在 flush 时补发。
        LogcatWriter(String::new())
    }
}

impl Write for LogcatWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.push_str(&String::from_utf8_lossy(buf));
        while let Some(idx) = self.0.find('\n') {
            let line: String = self.0.drain(..=idx).collect();
            emit(line.trim_end());
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.0.trim().is_empty() {
            let pending = std::mem::take(&mut self.0);
            emit(pending.trim_end());
        }
        Ok(())
    }
}

fn emit(line: &str) {
    if !line.is_empty() {
        log::info!("{line}");
    }
}

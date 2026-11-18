//! 结构化 tracing 缓冲区 — 按会话隔离的日志捕获
//!
//! 复用 bench_matrix.rs 的 WarnLog Layer 模式，但泛化为可查询的结构化缓冲区。
//! 每个 Session 持有自己的 TraceBuffer，通过 install() 注册为全局 tracing Layer。

use std::sync::OnceLock;

use parking_lot::Mutex;
use serde_json::{json, Value};

/// 单条 tracing 记录
#[derive(Debug, Clone)]
struct TraceEntry {
    level: String,
    target: String,
    message: String,
    fields: Vec<(String, String)>,
}

/// 会话级 tracing 缓冲区
pub struct TraceBuffer {
    entries: Mutex<Vec<TraceEntry>>,
    capacity: usize,
}

impl TraceBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(Vec::with_capacity(capacity)),
            capacity,
        }
    }

    fn push(&self, entry: TraceEntry) {
        let mut entries = self.entries.lock();
        if entries.len() >= self.capacity {
            entries.remove(0);
        }
        entries.push(entry);
    }

    /// 取出指定级别以上的日志，保留低于该级别的条目供后续 drain
    pub fn drain(&self, min_level: &str) -> Vec<Value> {
        let threshold = match min_level.to_ascii_lowercase().as_str() {
            "error" => 0,
            "warn" => 1,
            "info" => 2,
            "debug" => 3,
            "trace" => 4,
            _ => 1,
        };
        let mut entries = self.entries.lock();
        let (matching, rest): (Vec<_>, Vec<_>) = entries
            .drain(..)
            .partition(|e| level_num(&e.level) <= threshold);
        *entries = rest;
        matching
            .into_iter()
            .map(|e| {
                json!({
                    "level": e.level,
                    "target": e.target,
                    "message": e.message,
                    "fields": e.fields.into_iter()
                        .map(|(k, v)| (k, json!(v)))
                        .collect::<serde_json::Map<String, Value>>(),
                })
            })
            .collect()
    }
}

/// 全局 tracing Layer 路由：将事件分发到当前活跃的 TraceBuffer
pub struct TraceRouter {
    buffers: Mutex<Vec<Arc<TraceBuffer>>>,
}

static ROUTER: OnceLock<TraceRouter> = OnceLock::new();

pub fn router() -> &'static TraceRouter {
    ROUTER.get_or_init(|| TraceRouter {
        buffers: Mutex::new(Vec::new()),
    })
}

/// 注册一个 TraceBuffer 到全局路由
pub fn install(buffer: Arc<TraceBuffer>) {
    router().buffers.lock().push(buffer);
}

/// 从全局路由注销一个 TraceBuffer
pub fn uninstall(buffer: &Arc<TraceBuffer>) {
    let ptr = Arc::as_ptr(buffer) as usize;
    router()
        .buffers
        .lock()
        .retain(|b| Arc::as_ptr(b) as usize != ptr);
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for &'static TraceRouter {
    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        (**self).on_event(event, ctx)
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for TraceRouter {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let level = event.metadata().level().as_str().to_ascii_lowercase();
        let target = event.metadata().target().to_string();
        let mut message = String::new();
        let mut fields = Vec::new();
        event.record(&mut TraceVisitor {
            message: &mut message,
            fields: &mut fields,
        });
        let entry = TraceEntry {
            level,
            target,
            message,
            fields,
        };
        let buffers = self.buffers.lock();
        for buf in buffers.iter() {
            buf.push(entry.clone());
        }
    }
}

struct TraceVisitor<'a> {
    message: &'a mut String,
    fields: &'a mut Vec<(String, String)>,
}

impl tracing::field::Visit for TraceVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message.push_str(&format!("{value:?}"));
        } else {
            self.fields
                .push((field.name().to_string(), format!("{value:?}")));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }
    }
}

use std::sync::Arc;
fn level_num(level: &str) -> u8 {
    match level {
        "error" => 0,
        "warn" => 1,
        "info" => 2,
        "debug" => 3,
        "trace" => 4,
        _ => 4,
    }
}

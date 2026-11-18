//! MCP 工具定义 — 手写 schema 的调试工具集

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use serde_json::Value;

use crate::session::SessionManager;

type McpResult<T> = std::result::Result<T, McpError>;

fn map_session_error(prefix: &str, e: String) -> McpError {
    if e.contains("not found") || e.contains("limit reached") {
        McpError::invalid_params(format!("{prefix}: {e}"), None)
    } else {
        McpError::internal_error(format!("{prefix}: {e}"), None)
    }
}

pub struct DylTools {
    sessions: Arc<SessionManager>,
}

impl DylTools {
    pub fn new(sessions: Arc<SessionManager>) -> Self {
        Self { sessions }
    }
}

impl DylTools {
    pub fn tool_create_session(&self, source: String) -> McpResult<Value> {
        self.sessions
            .create(&source)
            .map_err(|e| map_session_error("create_session", e))
    }

    pub fn tool_command(&self, session_id: String, command: Value) -> McpResult<Value> {
        let cmd: dyl_core::contract::Command = serde_json::from_value(command)
            .map_err(|e| McpError::invalid_params(format!("command parse: {e}"), None))?;
        self.sessions
            .command(&session_id, cmd)
            .map(|rid| serde_json::json!({ "request_id": rid }))
            .map_err(|e| map_session_error("command", e))
    }

    pub fn tool_snapshot(&self, session_id: String) -> McpResult<Value> {
        self.sessions
            .snapshot(&session_id)
            .map_err(|e| map_session_error("snapshot", e))
    }

    pub fn tool_inspect(&self, session_id: String) -> McpResult<Value> {
        self.sessions
            .inspect(&session_id)
            .and_then(|diag| serde_json::to_value(&diag).map_err(|e| format!("serialize: {e}")))
            .map_err(|e| map_session_error("inspect", e))
    }

    pub fn tool_events(&self, session_id: String) -> McpResult<Value> {
        self.sessions
            .drain_events(&session_id)
            .map(|events| serde_json::json!({ "events": events }))
            .map_err(|e| map_session_error("events", e))
    }

    pub fn tool_trace(&self, session_id: String, level: Option<String>) -> McpResult<Value> {
        let level = level.unwrap_or_else(|| "warn".to_string());
        self.sessions
            .drain_trace(&session_id, &level)
            .map(|entries| serde_json::json!({ "entries": entries }))
            .map_err(|e| map_session_error("trace", e))
    }

    pub fn tool_list_sessions(&self) -> McpResult<Value> {
        let sessions = self.sessions.list();
        serde_json::to_value(&sessions)
            .map_err(|e| McpError::internal_error(format!("list_sessions: {e}"), None))
    }

    pub fn tool_destroy_session(&self, session_id: String) -> McpResult<Value> {
        self.sessions
            .destroy(&session_id)
            .map(|_| serde_json::json!({ "destroyed": true }))
            .map_err(|e| map_session_error("destroy_session", e))
    }
}

pub fn tool_schemas() -> Vec<Value> {
    vec![
        json_tool_schema(
            "dyl_create_session",
            "创建调试会话并加载媒体文件。加载是异步的——返回 state=Loading 时须通过 dyl_events 或 dyl_snapshot 确认加载结果",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "source": { "type": "string", "description": "媒体文件路径或URL" }
                },
                "required": ["source"]
            }),
        ),
        json_tool_schema(
            "dyl_command",
            "发送命令到播放内核",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "command": { "type": "object", "description": "Command JSON (op + 参数)" }
                },
                "required": ["session_id", "command"]
            }),
        ),
        json_tool_schema(
            "dyl_snapshot",
            "获取只读快照",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" }
                },
                "required": ["session_id"]
            }),
        ),
        json_tool_schema(
            "dyl_inspect",
            "深度诊断探针：AVSync/Pipeline/ControlState 内部状态",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" }
                },
                "required": ["session_id"]
            }),
        ),
        json_tool_schema(
            "dyl_events",
            "收集自上次以来的事件流（drain 语义：取走后不再可用；PositionChanged 只保留最新一条；最多返回100条）",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" }
                },
                "required": ["session_id"]
            }),
        ),
        json_tool_schema(
            "dyl_trace",
            "收集 tracing 日志（drain 语义：取走后不再可用；低于指定级别的日志保留不取）",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "level": { "type": "string", "description": "最低级别: error/warn/info/debug/trace", "default": "warn" }
                },
                "required": ["session_id"]
            }),
        ),
        json_tool_schema(
            "dyl_list_sessions",
            "列出所有活跃会话",
            serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        ),
        json_tool_schema(
            "dyl_destroy_session",
            "销毁调试会话",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" }
                },
                "required": ["session_id"]
            }),
        ),
    ]
}

fn json_tool_schema(name: &str, description: &str, input_schema: Value) -> Value {
    serde_json::json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema
    })
}

pub fn dispatch_tool(tools: &DylTools, name: &str, args: &Value) -> McpResult<Value> {
    match name {
        "dyl_create_session" => {
            let source = args
                .get("source")
                .and_then(|v| v.as_str())
                .ok_or_else(|| McpError::invalid_params("source is required", None))?
                .to_string();
            tools.tool_create_session(source)
        }
        "dyl_command" => {
            let session_id = args
                .get("session_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| McpError::invalid_params("session_id is required", None))?
                .to_string();
            let command = args
                .get("command")
                .ok_or_else(|| McpError::invalid_params("command is required", None))?
                .clone();
            tools.tool_command(session_id, command)
        }
        "dyl_snapshot" => {
            let session_id = args
                .get("session_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| McpError::invalid_params("session_id is required", None))?
                .to_string();
            tools.tool_snapshot(session_id)
        }
        "dyl_inspect" => {
            let session_id = args
                .get("session_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| McpError::invalid_params("session_id is required", None))?
                .to_string();
            tools.tool_inspect(session_id)
        }
        "dyl_events" => {
            let session_id = args
                .get("session_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| McpError::invalid_params("session_id is required", None))?
                .to_string();
            tools.tool_events(session_id)
        }
        "dyl_trace" => {
            let session_id = args
                .get("session_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| McpError::invalid_params("session_id is required", None))?
                .to_string();
            let level = args
                .get("level")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            tools.tool_trace(session_id, level)
        }
        "dyl_list_sessions" => tools.tool_list_sessions(),
        "dyl_destroy_session" => {
            let session_id = args
                .get("session_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| McpError::invalid_params("session_id is required", None))?
                .to_string();
            tools.tool_destroy_session(session_id)
        }
        _ => Err(McpError::invalid_params(
            format!("unknown tool: {name}"),
            None,
        )),
    }
}

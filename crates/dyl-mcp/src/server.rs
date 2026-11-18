//! MCP Server — 实现 ServerHandler trait，通过 stdio 传输与 MCP 客户端通信

use std::future::Future;
use std::sync::Arc;

use rmcp::{
    handler::server::ServerHandler,
    model::*,
    service::{RequestContext, RoleServer},
    ErrorData as McpError,
};

use crate::session::SessionManager;
use crate::tools;

pub struct DylServer {
    sessions: Arc<SessionManager>,
}

impl DylServer {
    pub fn new(sessions: Arc<SessionManager>) -> Self {
        Self { sessions }
    }
}

impl ServerHandler for DylServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            server_info: Implementation {
                name: "dyl-mcp".into(),
                version: "0.1.0".into(),
            },
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability {
                    list_changed: Some(false),
                }),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        let tools: Vec<Tool> = tools::tool_schemas()
            .into_iter()
            .filter_map(|v| match serde_json::from_value::<Tool>(v) {
                Ok(tool) => Some(tool),
                Err(e) => {
                    tracing::warn!("tool schema deserialize failed: {e}");
                    None
                }
            })
            .collect();
        std::future::ready(Ok(ListToolsResult {
            next_cursor: None,
            tools,
        }))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResult, McpError>> + Send + '_ {
        let name = request.name.to_string();
        let args = request
            .arguments
            .map(serde_json::Value::Object)
            .unwrap_or(serde_json::Value::Null);
        let result =
            tools::dispatch_tool(&tools::DylTools::new(self.sessions.clone()), &name, &args);
        match result {
            Ok(v) => std::future::ready(Ok(CallToolResult {
                content: vec![Content::text(v.to_string())],
                is_error: Some(false),
            })),
            Err(e) => std::future::ready(Ok(CallToolResult {
                content: vec![Content::text(format!("{e}"))],
                is_error: Some(true),
            })),
        }
    }
}

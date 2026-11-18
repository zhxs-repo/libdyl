//! dyl-mcp 二进制入口 — 通过 stdio 传输启动 MCP 服务器
//!
//! 用法: dyl-mcp.exe
//! 配置: 在 MCP 客户端的配置中添加此二进制路径作为 stdio 服务器

use std::sync::Arc;

use rmcp::service::ServiceExt;
use rmcp::transport::io::stdio;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;

use dyl_mcp::server::DylServer;
use dyl_mcp::trace_buffer;
use dyl_mcp::SessionManager;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fmt_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt_layer)
        .with(trace_buffer::router().with_filter(LevelFilter::TRACE))
        .init();

    let sessions = Arc::new(SessionManager::new());
    let server = DylServer::new(sessions);
    let (stdin, stdout) = stdio();

    tracing::info!("dyl-mcp server starting on stdio");

    let running = server.serve((stdin, stdout)).await?;

    running.waiting().await?;

    tracing::info!("dyl-mcp server stopped");
    Ok(())
}

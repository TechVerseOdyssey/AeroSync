mod server;

use rmcp::{ServiceExt, transport::stdio};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 日志必须写 stderr，不能写 stdout（MCP stdio 传输占用 stdout）
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!("AeroSync MCP server starting (stdio transport)");

    // 初始化审计日志（写入 ~/.aerosync/audit_mcp.log，进程重启后追加）
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let audit_path = std::path::PathBuf::from(&home).join(".aerosync").join("audit_mcp.log");

    let mut builder = server::AeroSyncMcpServer::new();
    if let Ok(logger) = aerosync_core::audit::AuditLogger::new(&audit_path).await {
        builder = builder.with_audit(Arc::new(logger));
        tracing::info!("MCP audit log: {}", audit_path.display());
    } else {
        tracing::warn!("Failed to open MCP audit log at {}, proceeding without audit", audit_path.display());
    }

    let service = builder
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("MCP server error: {:?}", e))?;

    service.waiting().await?;
    Ok(())
}

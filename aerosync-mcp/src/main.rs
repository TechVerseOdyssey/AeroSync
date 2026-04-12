mod server;

use rmcp::{ServiceExt, transport::stdio};

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

    let server = server::AeroSyncMcpServer::new();
    let service = server
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("MCP server error: {:?}", e))?;

    service.waiting().await?;
    Ok(())
}

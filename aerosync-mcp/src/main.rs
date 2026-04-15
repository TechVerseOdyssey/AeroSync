mod server;
mod task_store;

use rmcp::{ServiceExt, transport::stdio};
use std::sync::Arc;
use task_store::TaskStore;

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

    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let aerosync_dir = std::path::PathBuf::from(&home).join(".aerosync");

    // 初始化审计日志（写入 ~/.aerosync/audit_mcp.log，进程重启后追加）
    let audit_path = aerosync_dir.join("audit_mcp.log");

    // 初始化 SQLite 任务持久化（~/.aerosync/tasks.db）
    let db_path = aerosync_dir.join("tasks.db");

    let mut builder = server::AeroSyncMcpServer::new();

    if let Ok(logger) = aerosync_core::audit::AuditLogger::new(&audit_path).await {
        builder = builder.with_audit(Arc::new(logger));
        tracing::info!("MCP audit log: {}", audit_path.display());
    } else {
        tracing::warn!("Failed to open MCP audit log at {}, proceeding without audit", audit_path.display());
    }

    match TaskStore::open(&db_path) {
        Ok(store) => {
            let store = Arc::new(store);
            // Restore previous tasks into memory (pending/running → failed)
            let restored = store.load_all().await;
            // Remove old completed/failed entries (>24 h)
            store.evict_old(86400).await;
            let count = restored.len();
            builder = builder.with_task_store(Arc::clone(&store));
            builder.restore_tasks(restored).await;
            tracing::info!(
                "Task store: {} tasks restored from {}",
                count,
                db_path.display()
            );
        }
        Err(e) => {
            tracing::warn!("Failed to open task store at {}: {}, proceeding without persistence", db_path.display(), e);
        }
    }

    let service = builder
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("MCP server error: {:?}", e))?;

    service.waiting().await?;
    Ok(())
}

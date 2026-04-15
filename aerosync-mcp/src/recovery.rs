//! 断点续传恢复：进程重启后，从 TaskStore 中找到所有未完成且有 resume_json_path
//! 的任务，重新加载 ResumeState，并在后台重新启动传输。
//!
//! 调用方在 main.rs 中完成 TaskStore 初始化后立即调用 `recover_pending_transfers`。

use std::sync::Arc;
use std::time::Instant;

use aerosync_core::resume::{ResumeStore, ResumeState};
use aerosync_core::transfer::{ProtocolAdapter, TransferTask};
use aerosync_protocols::adapter::AutoAdapter;
use aerosync_protocols::http::HttpConfig;
use aerosync_protocols::quic::QuicConfig;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::server::{AeroSyncMcpServer, BackgroundTaskStatus, TaskEntry};
use crate::task_store::TaskStore;

/// 恢复所有未完成的断点续传任务。
///
/// 1. 调用 `task_store.load_resumable()` 获取有 resume JSON 的 pending/running 任务
/// 2. 对每个任务通过 `ResumeStore.load(task_id)` 加载 `ResumeState`
/// 3. 构建 `AutoAdapter`（带 `ResumeStore`），在后台 spawn 传输
/// 4. 更新任务状态为 Running
///
/// 返回成功恢复（spawn）的任务数量。
pub async fn recover_pending_transfers(
    server: &AeroSyncMcpServer,
    task_store: Arc<TaskStore>,
    resume_base_dir: &std::path::Path,
) -> usize {
    let resumable = task_store.load_resumable().await;
    if resumable.is_empty() {
        tracing::info!("Recovery: no resumable tasks found");
        return 0;
    }

    tracing::info!("Recovery: {} resumable task(s) found, attempting restart", resumable.len());
    let resume_store = Arc::new(ResumeStore::new(resume_base_dir));
    let mut recovered = 0usize;

    for (task_id, entry, resume_json_path) in resumable {
        // 加载 ResumeState — 通过直接读取 JSON 文件（resume_json_path 是完整路径）
        let state: ResumeState = match tokio::fs::read_to_string(&resume_json_path).await {
            Ok(content) => match serde_json::from_str(&content) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        "Recovery: failed to parse ResumeState for task {} at {}: {}",
                        task_id, resume_json_path.display(), e
                    );
                    mark_failed(server, &task_store, task_id, &entry.description, &format!("parse error: {}", e)).await;
                    continue;
                }
            },
            Err(e) => {
                tracing::warn!(
                    "Recovery: failed to read resume file for task {} at {}: {}",
                    task_id, resume_json_path.display(), e
                );
                mark_failed(server, &task_store, task_id, &entry.description, &format!("read error: {}", e)).await;
                continue;
            }
        };

        // 若所有分片已完成，直接标记 Completed
        if state.is_complete() {
            tracing::info!("Recovery: task {} already complete, marking done", task_id);
            let done_entry = TaskEntry {
                status: BackgroundTaskStatus::Completed {
                    files: 1,
                    bytes: state.total_size,
                    speed_mbs: 0.0,
                },
                description: entry.description.clone(),
                last_updated: Instant::now(),
            };
            task_store.upsert_with_resume(task_id, &done_entry, None).await;
            insert_task(server, task_id, done_entry).await;
            let _ = resume_store.delete(task_id).await;
            recovered += 1;
            continue;
        }

        let store_clone = Arc::clone(&resume_store);
        let http_config = HttpConfig {
            max_reconnect_attempts: 5,
            reconnect_base_delay_ms: 3_000,
            ..HttpConfig::default()
        };
        let adapter = Arc::new(
            AutoAdapter::new(http_config, QuicConfig::default())
                .with_resume_store(Arc::clone(&store_clone)),
        );

        let file_path = state.source_path.clone();
        let base_url = state.destination.clone();
        let total_size = state.total_size;
        let task_store_bg = Arc::clone(&task_store);
        let registry = Arc::clone(server.tasks_registry());
        let description = entry.description.clone();
        let resume_store_bg = Arc::clone(&resume_store);

        // 更新状态为 Running
        let running_entry = TaskEntry {
            status: BackgroundTaskStatus::Running,
            description: description.clone(),
            last_updated: Instant::now(),
        };
        task_store.upsert_with_resume(task_id, &running_entry, Some(&resume_json_path)).await;
        registry.lock().await.insert(task_id, running_entry);

        tokio::spawn(async move {
            let (progress_tx, _progress_rx) = mpsc::unbounded_channel();
            let mut resume_state = state;

            let transfer_task = TransferTask::new_upload(file_path.clone(), base_url.clone(), total_size);
            let result = adapter
                .upload_chunked(&transfer_task, &mut resume_state, progress_tx)
                .await;

            let final_entry = match result {
                Ok(()) => {
                    tracing::info!("Recovery: task {} completed successfully", task_id);
                    let _ = resume_store_bg.delete(task_id).await;
                    TaskEntry {
                        status: BackgroundTaskStatus::Completed {
                            files: 1,
                            bytes: total_size,
                            speed_mbs: 0.0,
                        },
                        description,
                        last_updated: Instant::now(),
                    }
                }
                Err(e) => {
                    tracing::warn!("Recovery: task {} failed: {}", task_id, e);
                    TaskEntry {
                        status: BackgroundTaskStatus::Failed(e.to_string()),
                        description,
                        last_updated: Instant::now(),
                    }
                }
            };

            task_store_bg.upsert(task_id, &final_entry).await;
            registry.lock().await.insert(task_id, final_entry);
        });

        recovered += 1;
    }

    tracing::info!("Recovery: spawned {} transfer(s)", recovered);
    recovered
}

// ─── helpers ────────────────────────────────────────────────────────────────

async fn mark_failed(
    server: &AeroSyncMcpServer,
    task_store: &TaskStore,
    task_id: Uuid,
    description: &str,
    reason: &str,
) {
    let entry = TaskEntry {
        status: BackgroundTaskStatus::Failed(format!("recovery failed: {}", reason)),
        description: description.to_string(),
        last_updated: Instant::now(),
    };
    task_store.upsert(task_id, &entry).await;
    insert_task(server, task_id, entry).await;
}

async fn insert_task(server: &AeroSyncMcpServer, task_id: Uuid, entry: TaskEntry) {
    server.tasks_registry().lock().await.insert(task_id, entry);
}

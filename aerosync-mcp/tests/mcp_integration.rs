//! Integration tests for MCP get_transfer_status and SQLite TaskStore.
//!
//! 4. MCP task registry — unknown id, completed, failed, running states
//! 5. TaskStore round-trip — persist → reload → states survive simulated restart

use aerosync_mcp::{
    recovery::recover_pending_transfers,
    server::{AeroSyncMcpServer, BackgroundTaskStatus, McpConfig, SendFileParams, TaskEntry},
    task_store::TaskStore,
};
use std::{sync::Arc, time::Instant};
use tempfile::tempdir;
use uuid::Uuid;

// ── 4. MCP task registry ──────────────────────────────────────────────────────

/// Brand-new server has an empty task registry.
#[tokio::test]
async fn test_task_registry_starts_empty() {
    let server = AeroSyncMcpServer::new();
    let registry = server.tasks_registry();
    let guard = registry.lock().await;
    assert_eq!(guard.len(), 0, "new server should have empty task registry");
}

/// Unknown task_id returns None from the registry.
#[tokio::test]
async fn test_task_registry_unknown_id_returns_none() {
    let server = AeroSyncMcpServer::new();
    let unknown = Uuid::new_v4();
    let guard = server.tasks_registry().lock().await;
    assert!(guard.get(&unknown).is_none());
}

/// Pre-inserted Completed task returns correct fields.
#[tokio::test]
async fn test_task_registry_completed_fields() {
    let server = AeroSyncMcpServer::new();
    let id = Uuid::new_v4();

    server.tasks_registry().lock().await.insert(
        id,
        TaskEntry {
            status: BackgroundTaskStatus::Completed {
                files: 7,
                bytes: 204800,
                speed_mbs: 12.3,
            },
            description: "send_file: /tmp/a → http://host/upload".to_string(),
            last_updated: Instant::now(),
        },
    );

    let guard = server.tasks_registry().lock().await;
    let entry = guard.get(&id).expect("task should be present");
    match &entry.status {
        BackgroundTaskStatus::Completed {
            files,
            bytes,
            speed_mbs,
        } => {
            assert_eq!(*files, 7);
            assert_eq!(*bytes, 204800);
            assert!((*speed_mbs - 12.3).abs() < 0.001);
        }
        other => panic!("expected Completed, got {:?}", other),
    }
    assert!(entry.description.contains("send_file"));
}

/// Pre-inserted Failed task preserves the error message.
#[tokio::test]
async fn test_task_registry_failed_message_preserved() {
    let server = AeroSyncMcpServer::new();
    let id = Uuid::new_v4();

    server.tasks_registry().lock().await.insert(
        id,
        TaskEntry {
            status: BackgroundTaskStatus::Failed(
                "connection refused to 192.168.1.1:7788".to_string(),
            ),
            description: "send_file: /tmp/b → http://192.168.1.1:7788/upload".to_string(),
            last_updated: Instant::now(),
        },
    );

    let guard = server.tasks_registry().lock().await;
    match &guard.get(&id).unwrap().status {
        BackgroundTaskStatus::Failed(msg) => {
            assert!(
                msg.contains("connection refused"),
                "unexpected msg: {}",
                msg
            );
        }
        other => panic!("expected Failed, got {:?}", other),
    }
}

/// Pre-inserted Running task shows running state.
#[tokio::test]
async fn test_task_registry_running_state() {
    let server = AeroSyncMcpServer::new();
    let id = Uuid::new_v4();

    server.tasks_registry().lock().await.insert(
        id,
        TaskEntry {
            status: BackgroundTaskStatus::Running,
            description: "send_directory: /data → http://host/upload".to_string(),
            last_updated: Instant::now(),
        },
    );

    let guard = server.tasks_registry().lock().await;
    assert!(matches!(
        guard.get(&id).unwrap().status,
        BackgroundTaskStatus::Running
    ));
}

/// Pending → Running → Completed state progression in the registry.
#[tokio::test]
async fn test_task_registry_state_progression() {
    let server = AeroSyncMcpServer::new();
    let id = Uuid::new_v4();

    // Insert as Pending
    server.tasks_registry().lock().await.insert(
        id,
        TaskEntry {
            status: BackgroundTaskStatus::Pending,
            description: "progression test".to_string(),
            last_updated: Instant::now(),
        },
    );
    assert!(matches!(
        server
            .tasks_registry()
            .lock()
            .await
            .get(&id)
            .unwrap()
            .status,
        BackgroundTaskStatus::Pending
    ));

    // Advance to Running
    server
        .tasks_registry()
        .lock()
        .await
        .get_mut(&id)
        .unwrap()
        .status = BackgroundTaskStatus::Running;
    assert!(matches!(
        server
            .tasks_registry()
            .lock()
            .await
            .get(&id)
            .unwrap()
            .status,
        BackgroundTaskStatus::Running
    ));

    // Advance to Completed
    server
        .tasks_registry()
        .lock()
        .await
        .get_mut(&id)
        .unwrap()
        .status = BackgroundTaskStatus::Completed {
        files: 1,
        bytes: 1024,
        speed_mbs: 5.0,
    };
    assert!(matches!(
        server
            .tasks_registry()
            .lock()
            .await
            .get(&id)
            .unwrap()
            .status,
        BackgroundTaskStatus::Completed { .. }
    ));
}

/// Invalid UUID string fails to parse — handled gracefully.
#[tokio::test]
async fn test_invalid_uuid_parse_fails_cleanly() {
    let result = "not-a-uuid".parse::<Uuid>();
    assert!(result.is_err(), "invalid UUID should not parse");
}

// ── 5. TaskStore round-trip ───────────────────────────────────────────────────

/// Completed and Failed tasks survive a simulated process restart; Pending → Failed.
#[tokio::test]
async fn test_task_store_survives_restart() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("tasks.db");

    let completed_id = Uuid::new_v4();
    let failed_id = Uuid::new_v4();
    let pending_id = Uuid::new_v4();
    let running_id = Uuid::new_v4();

    // "first process": write tasks
    {
        let store = TaskStore::open(&db_path).unwrap();

        store
            .upsert(
                completed_id,
                &TaskEntry {
                    status: BackgroundTaskStatus::Completed {
                        files: 3,
                        bytes: 9000,
                        speed_mbs: 15.0,
                    },
                    description: "completed task".to_string(),
                    last_updated: Instant::now(),
                },
            )
            .await;

        store
            .upsert(
                failed_id,
                &TaskEntry {
                    status: BackgroundTaskStatus::Failed("disk full".to_string()),
                    description: "failed task".to_string(),
                    last_updated: Instant::now(),
                },
            )
            .await;

        store
            .upsert(
                pending_id,
                &TaskEntry {
                    status: BackgroundTaskStatus::Pending,
                    description: "pending — never completed".to_string(),
                    last_updated: Instant::now(),
                },
            )
            .await;

        store
            .upsert(
                running_id,
                &TaskEntry {
                    status: BackgroundTaskStatus::Running,
                    description: "running — interrupted by shutdown".to_string(),
                    last_updated: Instant::now(),
                },
            )
            .await;
    } // store dropped — simulates process exit

    // "second process": reload
    let store = TaskStore::open(&db_path).unwrap();
    let restored = store.load_all().await;
    assert_eq!(restored.len(), 4);

    let find = |id: Uuid| {
        restored
            .iter()
            .find(|(rid, _)| *rid == id)
            .map(|(_, e)| e.status.clone())
            .expect("task not found after reload")
    };

    // Completed survives as-is
    match find(completed_id) {
        BackgroundTaskStatus::Completed {
            files,
            bytes,
            speed_mbs,
        } => {
            assert_eq!(files, 3);
            assert_eq!(bytes, 9000);
            assert!((speed_mbs - 15.0).abs() < 0.001);
        }
        other => panic!("completed task: expected Completed, got {:?}", other),
    }

    // Failed survives with original message
    match find(failed_id) {
        BackgroundTaskStatus::Failed(msg) => assert_eq!(msg, "disk full"),
        other => panic!("failed task: expected Failed, got {:?}", other),
    }

    // Pending at shutdown → Failed("process restarted…")
    match find(pending_id) {
        BackgroundTaskStatus::Failed(msg) => assert!(
            msg.contains("process restarted"),
            "expected 'process restarted', got: {}",
            msg
        ),
        other => panic!(
            "pending task: expected Failed(process restarted), got {:?}",
            other
        ),
    }

    // Running at shutdown → also Failed("process restarted…")
    match find(running_id) {
        BackgroundTaskStatus::Failed(msg) => assert!(
            msg.contains("process restarted"),
            "expected 'process restarted', got: {}",
            msg
        ),
        other => panic!(
            "running task: expected Failed(process restarted), got {:?}",
            other
        ),
    }
}

/// upsert overwrites an existing task — only the latest state is kept.
#[tokio::test]
async fn test_task_store_upsert_overwrites() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("tasks.db");
    let store = TaskStore::open(&db_path).unwrap();

    let id = Uuid::new_v4();

    store
        .upsert(
            id,
            &TaskEntry {
                status: BackgroundTaskStatus::Pending,
                description: "evolving task".to_string(),
                last_updated: Instant::now(),
            },
        )
        .await;

    store
        .upsert(
            id,
            &TaskEntry {
                status: BackgroundTaskStatus::Completed {
                    files: 2,
                    bytes: 500,
                    speed_mbs: 3.0,
                },
                description: "evolving task".to_string(),
                last_updated: Instant::now(),
            },
        )
        .await;

    let all = store.load_all().await;
    assert_eq!(all.len(), 1, "upsert should not create a duplicate row");
    assert!(
        matches!(all[0].1.status, BackgroundTaskStatus::Completed { .. }),
        "status should be Completed after upsert, got {:?}",
        all[0].1.status
    );
}

/// restore_tasks() pre-populates the server's in-memory registry from persisted data.
#[tokio::test]
async fn test_restore_tasks_populates_registry() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("tasks.db");
    let store = Arc::new(TaskStore::open(&db_path).unwrap());

    let id = Uuid::new_v4();
    store
        .upsert(
            id,
            &TaskEntry {
                status: BackgroundTaskStatus::Completed {
                    files: 1,
                    bytes: 100,
                    speed_mbs: 1.0,
                },
                description: "restored task".to_string(),
                last_updated: Instant::now(),
            },
        )
        .await;

    let server = AeroSyncMcpServer::new().with_task_store(Arc::clone(&store));
    let restored = store.load_all().await;
    server.restore_tasks(restored).await;

    let guard = server.tasks_registry().lock().await;
    assert!(
        guard.contains_key(&id),
        "registry should contain restored task after restore_tasks()"
    );
    assert!(
        matches!(
            guard.get(&id).unwrap().status,
            BackgroundTaskStatus::Completed { .. }
        ),
        "restored status should be Completed"
    );
}

/// evict_old keeps recently-upserted tasks and removes ancient ones.
#[tokio::test]
async fn test_evict_old_keeps_fresh_removes_ancient() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("tasks.db");

    // Manually insert an ancient row (updated_at = 0 = Unix epoch)
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY, description TEXT NOT NULL,
                status TEXT NOT NULL, files INTEGER, bytes INTEGER,
                speed_mbs REAL, error TEXT, updated_at INTEGER NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tasks VALUES ('ancient', 'ancient task', 'completed', 1, 100, 1.0, NULL, 0)",
            [],
        ).unwrap();
    }

    let store = TaskStore::open(&db_path).unwrap();

    // Insert a fresh task
    let fresh_id = Uuid::new_v4();
    store
        .upsert(
            fresh_id,
            &TaskEntry {
                status: BackgroundTaskStatus::Completed {
                    files: 1,
                    bytes: 50,
                    speed_mbs: 1.0,
                },
                description: "fresh".to_string(),
                last_updated: Instant::now(),
            },
        )
        .await;

    // Evict tasks older than 1 second — ancient (epoch 0) should be removed
    store.evict_old(1).await;

    let remaining = store.load_all().await;
    assert_eq!(
        remaining.len(),
        1,
        "only fresh task should remain after eviction"
    );
    assert_eq!(
        remaining[0].0, fresh_id,
        "remaining task should be the fresh one"
    );
}

// ── 6. P1 修复回归测试：send_file 必须把 resume_json_path 写入 SQLite ──────────
//
// 缺陷：在 P1 修复之前，`send_file` 只调用 `task_store.upsert(...)`，
// 从不写入 `resume_json_path`，导致 `load_resumable()` 永远返回 0 条，
// 重启后断点续传恢复路径成"悬空"。本测试锁死该回归。

/// send_file 同步阶段必须把 resume_json_path 写入 SQLite，
/// 路径必须落在 `aerosync_dir/.aerosync/{task_id}.json`。
#[tokio::test]
async fn test_send_file_records_resume_path_in_sqlite() {
    let dir = tempdir().unwrap();
    let aerosync_dir = dir.path().to_path_buf();
    let db_path = aerosync_dir.join("tasks.db");
    let store = Arc::new(TaskStore::open(&db_path).unwrap());

    // 准备一个小源文件（避免触发实际网络 IO 的开销）
    let src = aerosync_dir.join("payload.bin");
    tokio::fs::write(&src, b"hello aerosync").await.unwrap();

    let server = AeroSyncMcpServer::new()
        .with_task_store(Arc::clone(&store))
        .with_aerosync_dir(aerosync_dir.clone());

    // 目标使用明确的 http:// 前缀，让 negotiate_protocol 直接返回不发探针，
    // 后台传输会失败（无监听者），但同步阶段写库已发生。
    let params = SendFileParams {
        source: src.to_string_lossy().to_string(),
        destination: "http://127.0.0.1:1/upload".to_string(),
        no_verify: Some(true),
        ..Default::default()
    };

    let _ = server
        .send_file(rmcp::handler::server::wrapper::Parameters(params))
        .await
        .expect("send_file should succeed at sync stage even if bg fails");

    // 直接读 SQLite，验证恰好一条任务且 resume_json_path 非空
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE resume_json_path IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "send_file 必须把 resume_json_path 写入 SQLite（P1 回归）"
    );

    let resume_path: String = conn
        .query_row("SELECT resume_json_path FROM tasks LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let expected_prefix = aerosync_dir
        .join(".aerosync")
        .to_string_lossy()
        .into_owned();
    assert!(
        resume_path.starts_with(&expected_prefix),
        "resume_json_path 应位于 {{aerosync_dir}}/.aerosync 下，实际为：{}",
        resume_path
    );
    assert!(
        resume_path.ends_with(".json"),
        "resume_json_path 应以 .json 结尾，实际为：{}",
        resume_path
    );
}

/// 即使后台任务最终失败，TaskStore 中的 resume_json_path 也必须保留，
/// 这样下次进程启动时 recovery 能尝试断点续传。
#[tokio::test]
async fn test_send_file_keeps_resume_path_after_failure() {
    let dir = tempdir().unwrap();
    let aerosync_dir = dir.path().to_path_buf();
    let db_path = aerosync_dir.join("tasks.db");
    let store = Arc::new(TaskStore::open(&db_path).unwrap());

    let src = aerosync_dir.join("payload.bin");
    tokio::fs::write(&src, b"x").await.unwrap();

    let server = AeroSyncMcpServer::new()
        .with_task_store(Arc::clone(&store))
        .with_aerosync_dir(aerosync_dir.clone());

    let params = SendFileParams {
        source: src.to_string_lossy().to_string(),
        destination: "http://127.0.0.1:1/upload".to_string(),
        no_verify: Some(true),
        ..Default::default()
    };
    let _ = server
        .send_file(rmcp::handler::server::wrapper::Parameters(params))
        .await
        .expect("send_file sync stage should not error");

    // 等待后台任务收敛到失败状态（端口 1 不可达，几乎立即失败）
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let status: String = conn
            .query_row("SELECT status FROM tasks LIMIT 1", [], |r| r.get(0))
            .unwrap();
        if status == "failed" || status == "completed" {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("background task did not converge to failed within 15s");
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // 失败后 resume_json_path 必须保留（让重启后 recovery 可识别）
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let resume_path: Option<String> = conn
        .query_row("SELECT resume_json_path FROM tasks LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(
        resume_path.is_some(),
        "失败状态下 resume_json_path 必须保留，便于下次启动续传"
    );
}

// ── P3b recovery.rs 单测：进程重启后的断点续传恢复 ──────────────────────────

/// 空 TaskStore 调用 recover 必须返回 0，且不 panic。
#[tokio::test]
async fn test_recover_empty_store_returns_zero() {
    let dir = tempdir().unwrap();
    let aerosync_dir = dir.path().to_path_buf();
    let db_path = aerosync_dir.join("tasks.db");
    let store = Arc::new(TaskStore::open(&db_path).unwrap());

    let server = AeroSyncMcpServer::new()
        .with_task_store(Arc::clone(&store))
        .with_aerosync_dir(aerosync_dir.clone());

    let count = recover_pending_transfers(&server, store, &aerosync_dir).await;
    assert_eq!(count, 0);
}

/// 已经全部完成的 ResumeState（所有分片 done）：recover 必须直接标记 Completed
/// 并删除 JSON 文件，避免重复传输。
#[tokio::test]
async fn test_recover_marks_already_complete_state_as_completed() {
    use aerosync::core::resume::{ResumeState, ResumeStore};

    let dir = tempdir().unwrap();
    let aerosync_dir = dir.path().to_path_buf();
    let db_path = aerosync_dir.join("tasks.db");
    let store = Arc::new(TaskStore::open(&db_path).unwrap());

    // 构造一个"已完成"的 ResumeState（单分片 + 已 mark_chunk_done(0)）
    let task_id = Uuid::new_v4();
    let chunk_size = 32 * 1024 * 1024_u64;
    let mut state = ResumeState::new(
        task_id,
        aerosync_dir.join("dummy.bin"),
        "http://h/upload".to_string(),
        chunk_size,
        chunk_size,
        None,
    );
    state.mark_chunk_done(0);
    assert!(state.is_complete());

    // 用 ResumeStore 落盘到 {aerosync_dir}/.aerosync/{task_id}.json
    let resume_store = ResumeStore::new(&aerosync_dir);
    resume_store.save(&state).await.unwrap();
    let resume_json = aerosync_dir
        .join(".aerosync")
        .join(format!("{}.json", task_id));
    assert!(resume_json.exists(), "fixture: ResumeStore 应已写入 JSON");

    // TaskStore 注册一个 Pending 任务并指向该 JSON
    store
        .upsert_with_resume(
            task_id,
            &TaskEntry {
                status: BackgroundTaskStatus::Pending,
                description: "recover-complete fixture".to_string(),
                last_updated: Instant::now(),
            },
            Some(&resume_json),
        )
        .await;

    let server = AeroSyncMcpServer::new()
        .with_task_store(Arc::clone(&store))
        .with_aerosync_dir(aerosync_dir.clone());

    let count = recover_pending_transfers(&server, Arc::clone(&store), &aerosync_dir).await;
    assert_eq!(count, 1, "已完成的任务也算一次 recovered（直接归档）");

    // registry 中状态必须是 Completed
    let guard = server.tasks_registry().lock().await;
    match &guard
        .get(&task_id)
        .expect("task should be in registry")
        .status
    {
        BackgroundTaskStatus::Completed { bytes, .. } => {
            assert_eq!(*bytes, chunk_size, "Completed.bytes 应等于 total_size");
        }
        other => panic!("expected Completed, got {:?}", other),
    }
    drop(guard);

    // JSON 文件必须已被删除（防止重复扫描）
    assert!(
        !resume_json.exists(),
        "已完成任务的 ResumeState JSON 必须被清理"
    );
}

/// 损坏的 ResumeState JSON（无法解析）：recover 必须将任务标为 Failed
/// 并附带 "parse error" 错误信息，且不计入 recovered 数量（无 spawn）。
#[tokio::test]
async fn test_recover_handles_malformed_resume_json() {
    let dir = tempdir().unwrap();
    let aerosync_dir = dir.path().to_path_buf();
    let db_path = aerosync_dir.join("tasks.db");
    let store = Arc::new(TaskStore::open(&db_path).unwrap());

    // 写入一个非法 JSON 到预期的 resume 路径
    let resume_subdir = aerosync_dir.join(".aerosync");
    tokio::fs::create_dir_all(&resume_subdir).await.unwrap();
    let task_id = Uuid::new_v4();
    let resume_json = resume_subdir.join(format!("{}.json", task_id));
    tokio::fs::write(&resume_json, b"this is not valid json {")
        .await
        .unwrap();

    store
        .upsert_with_resume(
            task_id,
            &TaskEntry {
                status: BackgroundTaskStatus::Pending,
                description: "malformed-json fixture".to_string(),
                last_updated: Instant::now(),
            },
            Some(&resume_json),
        )
        .await;

    let server = AeroSyncMcpServer::new()
        .with_task_store(Arc::clone(&store))
        .with_aerosync_dir(aerosync_dir.clone());

    let count = recover_pending_transfers(&server, Arc::clone(&store), &aerosync_dir).await;
    assert_eq!(count, 0, "解析失败的任务不应计入 recovered（未 spawn）");

    let guard = server.tasks_registry().lock().await;
    match &guard
        .get(&task_id)
        .expect("task should still be tracked")
        .status
    {
        BackgroundTaskStatus::Failed(msg) => {
            assert!(
                msg.contains("recovery failed") && msg.contains("parse error"),
                "失败信息应明确指出 parse error，实际：{}",
                msg
            );
        }
        other => panic!("expected Failed, got {:?}", other),
    }
}

/// JSON 文件已被外部清理：load_resumable 已经过滤；recover 应得到 0。
/// 这一行为锁定了 TaskStore 与 recovery 之间的契约。
#[tokio::test]
async fn test_recover_skips_tasks_with_missing_resume_json() {
    let dir = tempdir().unwrap();
    let aerosync_dir = dir.path().to_path_buf();
    let db_path = aerosync_dir.join("tasks.db");
    let store = Arc::new(TaskStore::open(&db_path).unwrap());

    let task_id = Uuid::new_v4();
    let ghost_path = aerosync_dir
        .join(".aerosync")
        .join(format!("{}.json", task_id));
    // 故意不创建文件
    store
        .upsert_with_resume(
            task_id,
            &TaskEntry {
                status: BackgroundTaskStatus::Pending,
                description: "ghost fixture".to_string(),
                last_updated: Instant::now(),
            },
            Some(&ghost_path),
        )
        .await;

    let server = AeroSyncMcpServer::new()
        .with_task_store(Arc::clone(&store))
        .with_aerosync_dir(aerosync_dir.clone());

    let count = recover_pending_transfers(&server, Arc::clone(&store), &aerosync_dir).await;
    assert_eq!(count, 0, "JSON 文件不存在的任务必须被跳过");
}

// ── P3a 配置统一：超时/TTL 默认值与环境变量覆盖 ──────────────────────────────

/// 默认 transfer_timeout=1h、task_ttl=24h；TTL 必须 ≥ transfer_timeout，
/// 否则会出现"任务还在跑就被 evict"的不一致。
#[test]
fn test_mcp_config_defaults_are_consistent() {
    let cfg = McpConfig::default();
    assert_eq!(cfg.transfer_timeout.as_secs(), 3600, "默认传输超时应为 1h");
    assert_eq!(cfg.task_ttl.as_secs(), 86400, "默认任务 TTL 应为 24h");
    assert!(
        cfg.task_ttl >= cfg.transfer_timeout,
        "task_ttl({:?}) 必须 ≥ transfer_timeout({:?})，否则进行中任务可能被误清",
        cfg.task_ttl,
        cfg.transfer_timeout
    );
}

/// 配置 from_env 必须能正确解析环境变量。这里通过 Server::with_config
/// 来间接验证字段链路（避免 env 全局污染影响并发测试）。
#[test]
fn test_mcp_config_with_config_overrides_defaults() {
    let custom = McpConfig {
        transfer_timeout: std::time::Duration::from_secs(120),
        task_ttl: std::time::Duration::from_secs(600),
    };
    let server = AeroSyncMcpServer::new().with_config(custom.clone());
    assert_eq!(server.config().transfer_timeout, custom.transfer_timeout);
    assert_eq!(server.config().task_ttl, custom.task_ttl);
}

/// 内存 registry TTL 与 SQLite evict_old 必须使用相同的 task_ttl_secs，
/// 避免出现"内存里查不到、磁盘里还在"的不一致。
/// 当前 main.rs 直接使用 mcp_config.task_ttl 派生 evict_old 参数，本测试
/// 锁死该值在 McpConfig 中以秒为单位可被序列化。
#[test]
fn test_mcp_config_task_ttl_is_in_seconds() {
    let cfg = McpConfig::default();
    let secs = cfg.task_ttl.as_secs();
    assert_eq!(secs, 86400);
    let cfg = McpConfig {
        task_ttl: std::time::Duration::from_secs(7200),
        ..McpConfig::default()
    };
    assert_eq!(cfg.task_ttl.as_secs(), 7200);
}

// ── P3d Tool-level E2E：通过 rmcp client 调真实路由 ──────────────────────────
//
// 使用 tokio::io::duplex 在内存中建立双向通道，server 与 client 各持一端。
// 这样可以验证 #[tool_router] 宏注册到 ServerHandler 的完整链路：
//   client.call_tool → JSON-RPC frame → server 路由 → tool fn → response

mod e2e {
    use super::*;
    use rmcp::{
        model::{CallToolRequestParams, ClientInfo},
        ClientHandler, ServiceExt,
    };

    #[derive(Debug, Clone, Default)]
    struct DummyClient;
    impl ClientHandler for DummyClient {
        fn get_info(&self) -> ClientInfo {
            ClientInfo::default()
        }
    }

    /// 启动 server 与 client 的辅助函数，返回 client 句柄与 server 任务句柄。
    async fn spawn_pair(
        server: AeroSyncMcpServer,
    ) -> (
        rmcp::service::RunningService<rmcp::RoleClient, DummyClient>,
        tokio::task::JoinHandle<()>,
    ) {
        let (server_t, client_t) = tokio::io::duplex(8192);
        let server_handle = tokio::spawn(async move {
            if let Ok(running) = server.serve(server_t).await {
                let _ = running.waiting().await;
            }
        });
        let client = DummyClient
            .serve(client_t)
            .await
            .expect("client should connect to in-memory server");
        (client, server_handle)
    }

    /// 提取 CallTool 响应里的纯文本内容（所有工具都返回 JSON 字符串）。
    fn first_text(result: &rmcp::model::CallToolResult) -> &str {
        result
            .content
            .first()
            .and_then(|c| c.raw.as_text())
            .map(|t| t.text.as_str())
            .expect("expected text content in CallToolResult")
    }

    /// list_tools 必须返回 8 个工具，名字与文档约定一致。
    /// 这是 #[tool_router] 宏注册链路的核心契约。
    #[tokio::test]
    async fn test_e2e_lists_all_8_tools() {
        // Historical name kept for git-blame continuity; the count
        // is now 10 after RFC-002 §14 #10 added wait_receipt and
        // cancel_receipt.
        let dir = tempdir().unwrap();
        let server = AeroSyncMcpServer::new().with_aerosync_dir(dir.path().to_path_buf());
        let (client, _h) = spawn_pair(server).await;

        let tools = client
            .list_tools(None)
            .await
            .expect("list_tools should succeed");
        let names: Vec<String> = tools.tools.iter().map(|t| t.name.to_string()).collect();

        let expected = [
            "send_file",
            "send_directory",
            "start_receiver",
            "stop_receiver",
            "get_receiver_status",
            "list_history",
            "discover_receivers",
            "get_transfer_status",
            "wait_receipt",
            "cancel_receipt",
        ];
        for name in expected {
            assert!(
                names.iter().any(|n| n == name),
                "工具 `{}` 未注册到 ServerHandler。已注册：{:?}",
                name,
                names
            );
        }
        assert_eq!(
            names.len(),
            expected.len(),
            "工具数量不匹配，实际：{:?}",
            names
        );

        client.cancel().await.unwrap();
    }

    /// get_transfer_status 调用未知 task_id 必须返回 success: false 且消息包含 "not found"。
    /// 锁定 AI 客户端可解析的错误约定。
    #[tokio::test]
    async fn test_e2e_get_transfer_status_unknown_id() {
        let dir = tempdir().unwrap();
        let server = AeroSyncMcpServer::new().with_aerosync_dir(dir.path().to_path_buf());
        let (client, _h) = spawn_pair(server).await;

        let args = serde_json::json!({ "task_id": Uuid::new_v4().to_string() })
            .as_object()
            .unwrap()
            .clone();
        let result = client
            .call_tool(CallToolRequestParams::new("get_transfer_status").with_arguments(args))
            .await
            .expect("call should succeed");

        let parsed: serde_json::Value =
            serde_json::from_str(first_text(&result)).expect("response must be valid JSON");
        assert_eq!(parsed["success"], serde_json::json!(false));
        let err = parsed["error"].as_str().unwrap_or("");
        assert!(
            err.to_ascii_lowercase().contains("not found"),
            "未知 task_id 应返回 'not found' 错误，实际：{}",
            err
        );

        client.cancel().await.unwrap();
    }

    /// 设置了 mcp_auth_token 密钥但客户端未传时，必须返回 Unauthorized。
    /// 这是 P2 修复后的鉴权链路 E2E 覆盖。
    #[tokio::test]
    async fn test_e2e_call_without_mcp_auth_token_is_rejected() {
        let dir = tempdir().unwrap();
        let server = AeroSyncMcpServer::new()
            .with_aerosync_dir(dir.path().to_path_buf())
            .with_secret("test-secret-xyz".to_string());
        let (client, _h) = spawn_pair(server).await;

        // 调 get_receiver_status（最轻量的工具），不带 mcp_auth_token
        let result = client
            .call_tool(CallToolRequestParams::new("get_receiver_status"))
            .await
            .expect("call should succeed at protocol level");

        let parsed: serde_json::Value = serde_json::from_str(first_text(&result)).unwrap();
        assert_eq!(parsed["success"], serde_json::json!(false));
        let err = parsed["error"].as_str().unwrap_or("");
        assert!(
            err.contains("Unauthorized") && err.contains("mcp_auth_token"),
            "缺失 mcp_auth_token 应返回 Unauthorized 错误，实际：{}",
            err
        );

        client.cancel().await.unwrap();
    }

    /// 携带正确 mcp_auth_token 时鉴权放行；同时验证旧字段名 _auth_token alias 也能用。
    #[tokio::test]
    async fn test_e2e_call_with_correct_auth_passes() {
        let dir = tempdir().unwrap();
        let server = AeroSyncMcpServer::new()
            .with_aerosync_dir(dir.path().to_path_buf())
            .with_secret("good-secret".to_string());
        let (client, _h) = spawn_pair(server).await;

        // 新字段名
        let args_new = serde_json::json!({ "mcp_auth_token": "good-secret" })
            .as_object()
            .unwrap()
            .clone();
        let result = client
            .call_tool(CallToolRequestParams::new("get_receiver_status").with_arguments(args_new))
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(first_text(&result)).unwrap();
        assert_eq!(
            parsed["success"],
            serde_json::json!(true),
            "正确密钥应放行，实际响应：{}",
            first_text(&result)
        );

        // 旧字段名 alias
        let args_old = serde_json::json!({ "_auth_token": "good-secret" })
            .as_object()
            .unwrap()
            .clone();
        let result = client
            .call_tool(CallToolRequestParams::new("get_receiver_status").with_arguments(args_old))
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(first_text(&result)).unwrap();
        assert_eq!(
            parsed["success"],
            serde_json::json!(true),
            "旧字段名 _auth_token alias 必须仍可鉴权，实际：{}",
            first_text(&result)
        );

        client.cancel().await.unwrap();
    }
}

// ── 7. P2 改名向后兼容：_auth_token alias 必须仍然可解析 ──────────────────────

/// 旧客户端使用 `_auth_token` 字段（P2 之前的命名），通过 serde alias
/// 应仍能解析为 `mcp_auth_token`。
#[test]
fn test_legacy_auth_token_alias_still_deserializes() {
    use aerosync_mcp::server::SendFileParams;

    let json_old = r#"{
        "source": "/tmp/x",
        "destination": "http://h/upload",
        "_auth_token": "secret-from-old-client"
    }"#;
    let params: SendFileParams = serde_json::from_str(json_old)
        .expect("legacy _auth_token field should still deserialize via serde alias");
    assert_eq!(
        params.mcp_auth_token.as_deref(),
        Some("secret-from-old-client")
    );
    assert_eq!(params.source, "/tmp/x");

    // 新命名同样工作
    let json_new = r#"{
        "source": "/tmp/x",
        "destination": "http://h/upload",
        "mcp_auth_token": "secret-from-new-client"
    }"#;
    let params: SendFileParams = serde_json::from_str(json_new).unwrap();
    assert_eq!(
        params.mcp_auth_token.as_deref(),
        Some("secret-from-new-client")
    );
}

/// Multiple tasks with different statuses — all persisted and loaded correctly.
#[tokio::test]
async fn test_task_store_multiple_tasks() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("tasks.db");
    let store = TaskStore::open(&db_path).unwrap();

    let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();

    store
        .upsert(
            ids[0],
            &TaskEntry {
                status: BackgroundTaskStatus::Completed {
                    files: 1,
                    bytes: 100,
                    speed_mbs: 1.0,
                },
                description: "task 0".to_string(),
                last_updated: Instant::now(),
            },
        )
        .await;
    store
        .upsert(
            ids[1],
            &TaskEntry {
                status: BackgroundTaskStatus::Failed("err".to_string()),
                description: "task 1".to_string(),
                last_updated: Instant::now(),
            },
        )
        .await;
    store
        .upsert(
            ids[2],
            &TaskEntry {
                status: BackgroundTaskStatus::Pending,
                description: "task 2".to_string(),
                last_updated: Instant::now(),
            },
        )
        .await;
    store
        .upsert(
            ids[3],
            &TaskEntry {
                status: BackgroundTaskStatus::Running,
                description: "task 3".to_string(),
                last_updated: Instant::now(),
            },
        )
        .await;
    store
        .upsert(
            ids[4],
            &TaskEntry {
                status: BackgroundTaskStatus::Completed {
                    files: 10,
                    bytes: 99999,
                    speed_mbs: 50.0,
                },
                description: "task 4".to_string(),
                last_updated: Instant::now(),
            },
        )
        .await;

    let all = store.load_all().await;
    assert_eq!(all.len(), 5);

    // Two completed, one failed(original), two failed(process restarted)
    let completed = all
        .iter()
        .filter(|(_, e)| matches!(e.status, BackgroundTaskStatus::Completed { .. }))
        .count();
    let failed = all
        .iter()
        .filter(|(_, e)| matches!(e.status, BackgroundTaskStatus::Failed(_)))
        .count();
    assert_eq!(completed, 2);
    assert_eq!(failed, 3); // 1 original + 2 from pending/running → "process restarted"
}

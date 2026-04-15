//! Integration tests for MCP get_transfer_status and SQLite TaskStore.
//!
//! 4. MCP task registry — unknown id, completed, failed, running states
//! 5. TaskStore round-trip — persist → reload → states survive simulated restart

use aerosync_mcp::{
    server::{AeroSyncMcpServer, BackgroundTaskStatus, TaskEntry},
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

    server.tasks_registry().lock().await.insert(id, TaskEntry {
        status: BackgroundTaskStatus::Completed { files: 7, bytes: 204800, speed_mbs: 12.3 },
        description: "send_file: /tmp/a → http://host/upload".to_string(),
        last_updated: Instant::now(),
    });

    let guard = server.tasks_registry().lock().await;
    let entry = guard.get(&id).expect("task should be present");
    match &entry.status {
        BackgroundTaskStatus::Completed { files, bytes, speed_mbs } => {
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

    server.tasks_registry().lock().await.insert(id, TaskEntry {
        status: BackgroundTaskStatus::Failed("connection refused to 192.168.1.1:7788".to_string()),
        description: "send_file: /tmp/b → http://192.168.1.1:7788/upload".to_string(),
        last_updated: Instant::now(),
    });

    let guard = server.tasks_registry().lock().await;
    match &guard.get(&id).unwrap().status {
        BackgroundTaskStatus::Failed(msg) => {
            assert!(msg.contains("connection refused"), "unexpected msg: {}", msg);
        }
        other => panic!("expected Failed, got {:?}", other),
    }
}

/// Pre-inserted Running task shows running state.
#[tokio::test]
async fn test_task_registry_running_state() {
    let server = AeroSyncMcpServer::new();
    let id = Uuid::new_v4();

    server.tasks_registry().lock().await.insert(id, TaskEntry {
        status: BackgroundTaskStatus::Running,
        description: "send_directory: /data → http://host/upload".to_string(),
        last_updated: Instant::now(),
    });

    let guard = server.tasks_registry().lock().await;
    assert!(matches!(guard.get(&id).unwrap().status, BackgroundTaskStatus::Running));
}

/// Pending → Running → Completed state progression in the registry.
#[tokio::test]
async fn test_task_registry_state_progression() {
    let server = AeroSyncMcpServer::new();
    let id = Uuid::new_v4();

    // Insert as Pending
    server.tasks_registry().lock().await.insert(id, TaskEntry {
        status: BackgroundTaskStatus::Pending,
        description: "progression test".to_string(),
        last_updated: Instant::now(),
    });
    assert!(matches!(
        server.tasks_registry().lock().await.get(&id).unwrap().status,
        BackgroundTaskStatus::Pending
    ));

    // Advance to Running
    server.tasks_registry().lock().await.get_mut(&id).unwrap().status =
        BackgroundTaskStatus::Running;
    assert!(matches!(
        server.tasks_registry().lock().await.get(&id).unwrap().status,
        BackgroundTaskStatus::Running
    ));

    // Advance to Completed
    server.tasks_registry().lock().await.get_mut(&id).unwrap().status =
        BackgroundTaskStatus::Completed { files: 1, bytes: 1024, speed_mbs: 5.0 };
    assert!(matches!(
        server.tasks_registry().lock().await.get(&id).unwrap().status,
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

        store.upsert(completed_id, &TaskEntry {
            status: BackgroundTaskStatus::Completed { files: 3, bytes: 9000, speed_mbs: 15.0 },
            description: "completed task".to_string(),
            last_updated: Instant::now(),
        }).await;

        store.upsert(failed_id, &TaskEntry {
            status: BackgroundTaskStatus::Failed("disk full".to_string()),
            description: "failed task".to_string(),
            last_updated: Instant::now(),
        }).await;

        store.upsert(pending_id, &TaskEntry {
            status: BackgroundTaskStatus::Pending,
            description: "pending — never completed".to_string(),
            last_updated: Instant::now(),
        }).await;

        store.upsert(running_id, &TaskEntry {
            status: BackgroundTaskStatus::Running,
            description: "running — interrupted by shutdown".to_string(),
            last_updated: Instant::now(),
        }).await;
    } // store dropped — simulates process exit

    // "second process": reload
    let store = TaskStore::open(&db_path).unwrap();
    let restored = store.load_all().await;
    assert_eq!(restored.len(), 4);

    let find = |id: Uuid| {
        restored.iter()
            .find(|(rid, _)| *rid == id)
            .map(|(_, e)| e.status.clone())
            .expect("task not found after reload")
    };

    // Completed survives as-is
    match find(completed_id) {
        BackgroundTaskStatus::Completed { files, bytes, speed_mbs } => {
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
            "expected 'process restarted', got: {}", msg
        ),
        other => panic!("pending task: expected Failed(process restarted), got {:?}", other),
    }

    // Running at shutdown → also Failed("process restarted…")
    match find(running_id) {
        BackgroundTaskStatus::Failed(msg) => assert!(
            msg.contains("process restarted"),
            "expected 'process restarted', got: {}", msg
        ),
        other => panic!("running task: expected Failed(process restarted), got {:?}", other),
    }
}

/// upsert overwrites an existing task — only the latest state is kept.
#[tokio::test]
async fn test_task_store_upsert_overwrites() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("tasks.db");
    let store = TaskStore::open(&db_path).unwrap();

    let id = Uuid::new_v4();

    store.upsert(id, &TaskEntry {
        status: BackgroundTaskStatus::Pending,
        description: "evolving task".to_string(),
        last_updated: Instant::now(),
    }).await;

    store.upsert(id, &TaskEntry {
        status: BackgroundTaskStatus::Completed { files: 2, bytes: 500, speed_mbs: 3.0 },
        description: "evolving task".to_string(),
        last_updated: Instant::now(),
    }).await;

    let all = store.load_all().await;
    assert_eq!(all.len(), 1, "upsert should not create a duplicate row");
    assert!(
        matches!(all[0].1.status, BackgroundTaskStatus::Completed { .. }),
        "status should be Completed after upsert, got {:?}", all[0].1.status
    );
}

/// restore_tasks() pre-populates the server's in-memory registry from persisted data.
#[tokio::test]
async fn test_restore_tasks_populates_registry() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("tasks.db");
    let store = Arc::new(TaskStore::open(&db_path).unwrap());

    let id = Uuid::new_v4();
    store.upsert(id, &TaskEntry {
        status: BackgroundTaskStatus::Completed { files: 1, bytes: 100, speed_mbs: 1.0 },
        description: "restored task".to_string(),
        last_updated: Instant::now(),
    }).await;

    let server = AeroSyncMcpServer::new().with_task_store(Arc::clone(&store));
    let restored = store.load_all().await;
    server.restore_tasks(restored).await;

    let guard = server.tasks_registry().lock().await;
    assert!(guard.contains_key(&id), "registry should contain restored task after restore_tasks()");
    assert!(
        matches!(guard.get(&id).unwrap().status, BackgroundTaskStatus::Completed { .. }),
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
        ).unwrap();
        conn.execute(
            "INSERT INTO tasks VALUES ('ancient', 'ancient task', 'completed', 1, 100, 1.0, NULL, 0)",
            [],
        ).unwrap();
    }

    let store = TaskStore::open(&db_path).unwrap();

    // Insert a fresh task
    let fresh_id = Uuid::new_v4();
    store.upsert(fresh_id, &TaskEntry {
        status: BackgroundTaskStatus::Completed { files: 1, bytes: 50, speed_mbs: 1.0 },
        description: "fresh".to_string(),
        last_updated: Instant::now(),
    }).await;

    // Evict tasks older than 1 second — ancient (epoch 0) should be removed
    store.evict_old(1).await;

    let remaining = store.load_all().await;
    assert_eq!(remaining.len(), 1, "only fresh task should remain after eviction");
    assert_eq!(remaining[0].0, fresh_id, "remaining task should be the fresh one");
}

/// Multiple tasks with different statuses — all persisted and loaded correctly.
#[tokio::test]
async fn test_task_store_multiple_tasks() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("tasks.db");
    let store = TaskStore::open(&db_path).unwrap();

    let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();

    store.upsert(ids[0], &TaskEntry {
        status: BackgroundTaskStatus::Completed { files: 1, bytes: 100, speed_mbs: 1.0 },
        description: "task 0".to_string(), last_updated: Instant::now(),
    }).await;
    store.upsert(ids[1], &TaskEntry {
        status: BackgroundTaskStatus::Failed("err".to_string()),
        description: "task 1".to_string(), last_updated: Instant::now(),
    }).await;
    store.upsert(ids[2], &TaskEntry {
        status: BackgroundTaskStatus::Pending,
        description: "task 2".to_string(), last_updated: Instant::now(),
    }).await;
    store.upsert(ids[3], &TaskEntry {
        status: BackgroundTaskStatus::Running,
        description: "task 3".to_string(), last_updated: Instant::now(),
    }).await;
    store.upsert(ids[4], &TaskEntry {
        status: BackgroundTaskStatus::Completed { files: 10, bytes: 99999, speed_mbs: 50.0 },
        description: "task 4".to_string(), last_updated: Instant::now(),
    }).await;

    let all = store.load_all().await;
    assert_eq!(all.len(), 5);

    // Two completed, one failed(original), two failed(process restarted)
    let completed = all.iter().filter(|(_, e)| matches!(e.status, BackgroundTaskStatus::Completed { .. })).count();
    let failed = all.iter().filter(|(_, e)| matches!(e.status, BackgroundTaskStatus::Failed(_))).count();
    assert_eq!(completed, 2);
    assert_eq!(failed, 3); // 1 original + 2 from pending/running → "process restarted"
}

//! SQLite-backed persistence for background transfer tasks.
//!
//! Survives process restarts: tasks in Pending/Running state at shutdown are
//! reloaded as Failed("process restarted") so callers learn why they stopped.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{params, Connection};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::server::{BackgroundTaskStatus, TaskEntry};

/// Thread-safe SQLite task store.
#[derive(Clone)]
pub struct TaskStore {
    /// The database file path (kept for diagnostics).
    pub path: PathBuf,
    /// Serialises all writes through a single async mutex wrapping a blocking conn.
    conn: Arc<Mutex<Connection>>,
}

impl TaskStore {
    /// Open (or create) the SQLite database at `path`.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        // WAL mode: better read/write concurrency, crash-safe
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tasks (
                id          TEXT PRIMARY KEY,
                description TEXT NOT NULL,
                status      TEXT NOT NULL,  -- 'pending'|'running'|'completed'|'failed'
                files       INTEGER,
                bytes       INTEGER,
                speed_mbs   REAL,
                error       TEXT,
                updated_at  INTEGER NOT NULL   -- Unix seconds
            );",
        )?;
        Ok(Self {
            path: path.to_owned(),
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Persist a task entry (upsert).
    pub async fn upsert(&self, id: Uuid, entry: &TaskEntry) {
        let (status, files, bytes, speed_mbs, error) = encode_status(&entry.status);
        let id_str = id.to_string();
        let description = entry.description.clone();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let conn = self.conn.lock().await;
        let result = conn.execute(
            "INSERT INTO tasks (id, description, status, files, bytes, speed_mbs, error, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                description = excluded.description,
                status      = excluded.status,
                files       = excluded.files,
                bytes       = excluded.bytes,
                speed_mbs   = excluded.speed_mbs,
                error       = excluded.error,
                updated_at  = excluded.updated_at",
            params![id_str, description, status, files, bytes, speed_mbs, error, now],
        );
        if let Err(e) = result {
            tracing::warn!("TaskStore: failed to upsert task {}: {}", id_str, e);
        }
    }

    /// Load all tasks from the database.
    ///
    /// Tasks that were Pending or Running at last shutdown are returned as
    /// Failed("process restarted") — the process can no longer drive them.
    pub async fn load_all(&self) -> Vec<(Uuid, TaskEntry)> {
        let conn = self.conn.lock().await;
        let mut stmt = match conn.prepare(
            "SELECT id, description, status, files, bytes, speed_mbs, error FROM tasks",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("TaskStore: failed to prepare load query: {}", e);
                return vec![];
            }
        };

        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,   // id
                row.get::<_, String>(1)?,   // description
                row.get::<_, String>(2)?,   // status
                row.get::<_, Option<i64>>(3)?,  // files
                row.get::<_, Option<i64>>(4)?,  // bytes
                row.get::<_, Option<f64>>(5)?,  // speed_mbs
                row.get::<_, Option<String>>(6)?, // error
            ))
        });

        let rows = match rows {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("TaskStore: failed to query tasks: {}", e);
                return vec![];
            }
        };

        let mut result = Vec::new();
        for row in rows.flatten() {
            let (id_str, description, status_str, files, bytes, speed_mbs, error) = row;
            let id = match id_str.parse::<Uuid>() {
                Ok(u) => u,
                Err(_) => continue,
            };

            let bg_status = match status_str.as_str() {
                "completed" => BackgroundTaskStatus::Completed {
                    files: files.unwrap_or(0) as usize,
                    bytes: bytes.unwrap_or(0) as u64,
                    speed_mbs: speed_mbs.unwrap_or(0.0),
                },
                "failed" => BackgroundTaskStatus::Failed(
                    error.unwrap_or_else(|| "unknown error".to_string()),
                ),
                // pending/running at shutdown → failed (process can't resume them)
                _ => BackgroundTaskStatus::Failed(
                    "process restarted before task completed".to_string(),
                ),
            };

            result.push((
                id,
                TaskEntry {
                    status: bg_status,
                    description,
                    last_updated: std::time::Instant::now(),
                },
            ));
        }

        tracing::info!("TaskStore: loaded {} tasks from {}", result.len(), self.path.display());
        result
    }

    /// Delete tasks older than `max_age_secs` seconds.
    pub async fn evict_old(&self, max_age_secs: u64) {
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
            - max_age_secs as i64;

        let conn = self.conn.lock().await;
        if let Err(e) = conn.execute(
            "DELETE FROM tasks WHERE updated_at < ?1",
            params![cutoff],
        ) {
            tracing::warn!("TaskStore: evict_old failed: {}", e);
        }
    }
}

// ─────────────────────────── helpers ───────────────────────────────────────

fn encode_status(
    s: &BackgroundTaskStatus,
) -> (&'static str, Option<i64>, Option<i64>, Option<f64>, Option<String>) {
    match s {
        BackgroundTaskStatus::Pending => ("pending", None, None, None, None),
        BackgroundTaskStatus::Running => ("running", None, None, None, None),
        BackgroundTaskStatus::Completed { files, bytes, speed_mbs } => (
            "completed",
            Some(*files as i64),
            Some(*bytes as i64),
            Some(*speed_mbs),
            None,
        ),
        BackgroundTaskStatus::Failed(msg) => ("failed", None, None, None, Some(msg.clone())),
    }
}

// ─────────────────────────── tests ─────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_entry(status: BackgroundTaskStatus) -> TaskEntry {
        TaskEntry {
            status,
            description: "test task".to_string(),
            last_updated: std::time::Instant::now(),
        }
    }

    #[tokio::test]
    async fn test_upsert_and_load_completed() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("tasks.db");
        let store = TaskStore::open(&db_path).unwrap();

        let id = Uuid::new_v4();
        let entry = make_entry(BackgroundTaskStatus::Completed {
            files: 3,
            bytes: 1024,
            speed_mbs: 12.5,
        });
        store.upsert(id, &entry).await;

        let all = store.load_all().await;
        assert_eq!(all.len(), 1);
        let (loaded_id, loaded_entry) = &all[0];
        assert_eq!(*loaded_id, id);
        match &loaded_entry.status {
            BackgroundTaskStatus::Completed { files, bytes, speed_mbs } => {
                assert_eq!(*files, 3);
                assert_eq!(*bytes, 1024);
                assert!((speed_mbs - 12.5).abs() < 0.001);
            }
            other => panic!("unexpected status: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_pending_becomes_failed_on_reload() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("tasks.db");
        let store = TaskStore::open(&db_path).unwrap();

        let id = Uuid::new_v4();
        store.upsert(id, &make_entry(BackgroundTaskStatus::Pending)).await;
        store.upsert(Uuid::new_v4(), &make_entry(BackgroundTaskStatus::Running)).await;

        let all = store.load_all().await;
        assert_eq!(all.len(), 2);
        for (_, entry) in &all {
            assert!(matches!(entry.status, BackgroundTaskStatus::Failed(_)));
        }
    }

    #[tokio::test]
    async fn test_upsert_updates_existing() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("tasks.db");
        let store = TaskStore::open(&db_path).unwrap();

        let id = Uuid::new_v4();
        store.upsert(id, &make_entry(BackgroundTaskStatus::Pending)).await;
        store.upsert(id, &make_entry(BackgroundTaskStatus::Completed {
            files: 1, bytes: 500, speed_mbs: 5.0,
        })).await;

        let all = store.load_all().await;
        assert_eq!(all.len(), 1);
        assert!(matches!(all[0].1.status, BackgroundTaskStatus::Completed { .. }));
    }

    #[tokio::test]
    async fn test_evict_old_removes_expired() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("tasks.db");

        // Insert with a manual backdated timestamp
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS tasks (
                    id TEXT PRIMARY KEY, description TEXT NOT NULL,
                    status TEXT NOT NULL, files INTEGER, bytes INTEGER,
                    speed_mbs REAL, error TEXT, updated_at INTEGER NOT NULL
                );",
            ).unwrap();
            conn.execute(
                "INSERT INTO tasks VALUES ('old-id', 'old task', 'completed', 1, 100, 1.0, NULL, 0)",
                [],
            ).unwrap();
        }

        let store = TaskStore::open(&db_path).unwrap();
        // Evict anything older than 1 second — the old task has updated_at=0 (epoch)
        store.evict_old(1).await;

        let all = store.load_all().await;
        assert!(all.is_empty(), "expected old task to be evicted");
    }

    #[tokio::test]
    async fn test_failed_status_preserved() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("tasks.db");
        let store = TaskStore::open(&db_path).unwrap();

        let id = Uuid::new_v4();
        store.upsert(id, &make_entry(BackgroundTaskStatus::Failed("disk full".to_string()))).await;

        let all = store.load_all().await;
        match &all[0].1.status {
            BackgroundTaskStatus::Failed(msg) => assert_eq!(msg, "disk full"),
            other => panic!("unexpected: {:?}", other),
        }
    }
}

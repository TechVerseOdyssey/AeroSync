//! SQLite-backed transfer history ([`aerosync_domain::storage::HistoryStorage`]).
//!
//! Each row stores a full [`HistoryEntry`] as JSON. Mutating methods mirror
//! the JSONL store: read–modify–write, using `DELETE` + re-`INSERT` in a
//! single transaction for bulk replace. For parity with
//! [`crate::history::HistoryStore`], [`HistoryStorage::query`] funnels
//! through [`crate::history::filter_history_entries`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aerosync_domain::metadata::MetadataJson;
use aerosync_domain::storage::{HistoryEntry, HistoryQuery, HistoryStorage, ReceiptStateLabel};
use aerosync_domain::{AeroSyncError, Result};
use aerosync_proto::Metadata;
use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::history::filter_history_entries;

/// On-disk history database implementing [`HistoryStorage`].
#[derive(Clone)]
pub struct SqliteHistoryStore {
    path: PathBuf,
    conn: Arc<Mutex<Connection>>,
}

impl SqliteHistoryStore {
    /// Open or create `path`. Parent directories are created as needed.
    pub async fn open(path: &Path) -> Result<Self> {
        let p = path.to_path_buf();
        let conn = tokio::task::spawn_blocking(move || open_conn(&p))
            .await
            .map_err(|e| AeroSyncError::System(format!("history sqlite join: {e}")))??;
        Ok(Self {
            path: path.to_path_buf(),
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// `~/.config/aerosync/history.db` — next to the JSONL default
    /// [`crate::history::HistoryStore::default_path`].
    pub fn default_path() -> PathBuf {
        dirs_next::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("aerosync")
            .join("history.db")
    }

    /// On-disk path (diagnostics / tests).
    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn read_all(&self) -> Result<Vec<HistoryEntry>> {
        let conn = self.conn.clone();
        async move {
            let c = conn.lock().await;
            let mut stmt = c
                .prepare("SELECT body FROM history_entries")
                .map_err(map_sql_err)?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(map_sql_err)?;
            let mut out = Vec::new();
            for r in rows {
                let s = r.map_err(map_sql_err)?;
                let e: HistoryEntry = serde_json::from_str(&s)
                    .map_err(|e| AeroSyncError::System(format!("history entry json: {e}")))?;
                out.push(e);
            }
            Ok(out)
        }
        .await
    }

    async fn replace_all(&self, entries: &[HistoryEntry]) -> Result<()> {
        let conn = self.conn.clone();
        let entries: Vec<HistoryEntry> = entries.to_vec();
        async move {
            let c = conn.lock().await;
            c.execute("BEGIN IMMEDIATE", []).map_err(map_sql_err)?;
            let r: Result<()> = (|| {
                c.execute("DELETE FROM history_entries", [])
                    .map_err(map_sql_err)?;
                for e in &entries {
                    let line = serde_json::to_string(e)
                        .map_err(|e| AeroSyncError::System(format!("history ser: {e}")))?;
                    c.execute(
                        "INSERT INTO history_entries (id, body) VALUES (?1, ?2)",
                        params![e.id.to_string(), line],
                    )
                    .map_err(map_sql_err)?;
                }
                c.execute("COMMIT", []).map_err(map_sql_err)?;
                Ok(())
            })();
            if r.is_err() {
                let _ = c.execute("ROLLBACK", []);
            }
            r
        }
        .await
    }
}

fn open_conn(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            AeroSyncError::System(format!("create history dir {}: {e}", parent.display()))
        })?;
    }
    let conn = Connection::open(path).map_err(map_sql_err)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
        .map_err(map_sql_err)?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS history_entries (
            id TEXT PRIMARY KEY NOT NULL,
            body TEXT NOT NULL
        )",
        [],
    )
    .map_err(map_sql_err)?;
    Ok(conn)
}

fn map_sql_err(e: rusqlite::Error) -> AeroSyncError {
    AeroSyncError::System(format!("sqlite history: {e}"))
}

#[async_trait]
impl HistoryStorage for SqliteHistoryStore {
    async fn append(&self, entry: HistoryEntry) -> Result<()> {
        let line = serde_json::to_string(&entry)
            .map_err(|e| AeroSyncError::System(format!("History serialize: {e}")))?;
        let id = entry.id.to_string();
        let conn = self.conn.clone();
        async move {
            let c = conn.lock().await;
            c.execute(
                "INSERT INTO history_entries (id, body) VALUES (?1, ?2)",
                params![id, line],
            )
            .map_err(map_sql_err)?;
            Ok::<(), AeroSyncError>(())
        }
        .await
    }

    async fn query(&self, q: &HistoryQuery) -> Result<Vec<HistoryEntry>> {
        let all = self.read_all().await?;
        Ok(filter_history_entries(all, q))
    }

    async fn recent(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        self.query(&HistoryQuery {
            limit,
            ..Default::default()
        })
        .await
    }

    async fn write_metadata(&self, record_id: Uuid, metadata: &Metadata) -> Result<bool> {
        let mut all = self.read_all().await?;
        let json_form = MetadataJson::from_proto(metadata);
        let mut patched = false;
        for entry in all.iter_mut() {
            if entry.id == record_id || entry.receipt_id == Some(record_id) {
                entry.metadata = Some(json_form.clone());
                patched = true;
            }
        }
        if !patched {
            return Ok(false);
        }
        self.replace_all(&all).await?;
        Ok(true)
    }

    async fn record_receipt_terminal(
        &self,
        record_id: Uuid,
        state: ReceiptStateLabel,
        reason: Option<String>,
    ) -> Result<bool> {
        let mut all = self.read_all().await?;
        let mut patched = false;
        for entry in all.iter_mut() {
            if entry.id == record_id || entry.receipt_id == Some(record_id) {
                entry.receipt_id.get_or_insert(record_id);
                entry.receipt_state = Some(state);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                match state {
                    ReceiptStateLabel::Acked => {
                        entry.acked_at = Some(now);
                    }
                    ReceiptStateLabel::Nacked => {
                        entry.nack_reason = reason.clone();
                    }
                    ReceiptStateLabel::Cancelled => {
                        entry.cancel_reason = reason.clone();
                    }
                    ReceiptStateLabel::Errored | ReceiptStateLabel::StreamLost => {
                        if let Some(ref r) = reason {
                            entry.error = Some(r.clone());
                        }
                    }
                    ReceiptStateLabel::Initiated => {}
                }
                patched = true;
            }
        }
        if !patched {
            return Ok(false);
        }
        self.replace_all(&all).await?;
        Ok(true)
    }

    async fn iter_unfinished_receipts(&self) -> Result<Vec<HistoryEntry>> {
        let all = self.read_all().await?;
        Ok(all
            .into_iter()
            .filter(|e| {
                e.receipt_id.is_some()
                    && match e.receipt_state {
                        Some(s) => !s.is_terminal(),
                        None => true,
                    }
            })
            .collect())
    }

    async fn query_by_receipt(&self, receipt_id: Uuid) -> Result<Option<HistoryEntry>> {
        let needle = receipt_id.to_string();
        let conn = self.conn.clone();
        async move {
            let c = conn.lock().await;
            if let Some(body) = c
                .query_row(
                    "SELECT body FROM history_entries WHERE json_extract(body, '$.receipt_id') = ?1",
                    params![&needle],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(map_sql_err)?
            {
                let e: HistoryEntry = serde_json::from_str(&body)
                    .map_err(|e| AeroSyncError::System(format!("history json: {e}")))?;
                return Ok(Some(e));
            }
            if let Some(body) = c
                .query_row(
                    "SELECT body FROM history_entries WHERE id = ?1",
                    params![&needle],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(map_sql_err)?
            {
                let e: HistoryEntry = serde_json::from_str(&body)
                    .map_err(|e| AeroSyncError::System(format!("history json: {e}")))?;
                return Ok(Some(e));
            }
            Ok(None)
        }
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> (SqliteHistoryStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("h.db");
        (SqliteHistoryStore::open(&p).await.unwrap(), dir)
    }

    #[tokio::test]
    async fn append_query_round_trip() {
        let (s, _d) = store().await;
        let e = {
            let mut t = HistoryEntry::success("a.bin", None, 3, None, None, "http", "send", 1);
            t.id = Uuid::new_v4();
            t
        };
        s.append(e).await.unwrap();
        let got = s.recent(10).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].filename, "a.bin");
    }

    #[tokio::test]
    async fn write_metadata_patches() {
        let (s, _d) = store().await;
        let id = Uuid::new_v4();
        let e = HistoryEntry {
            id,
            ..HistoryEntry::success("x", None, 0, None, None, "http", "send", 0)
        };
        s.append(e).await.unwrap();
        let m = Metadata {
            id: id.to_string(),
            from_node: "a".into(),
            ..Default::default()
        };
        assert!(s.write_metadata(id, &m).await.unwrap());
        let row = s.recent(1).await.unwrap().into_iter().next().unwrap();
        assert_eq!(row.metadata.as_ref().unwrap().from_node, "a");
    }

    #[tokio::test]
    async fn query_by_receipt_uses_receipt_id() {
        let (s, _d) = store().await;
        let rid = Uuid::new_v4();
        let e =
            HistoryEntry::success("f", None, 1, None, None, "http", "send", 1).with_receipt_id(rid);
        s.append(e).await.unwrap();
        let h = s.query_by_receipt(rid).await.unwrap();
        assert!(h.is_some());
        assert_eq!(h.unwrap().receipt_id, Some(rid));
    }
}

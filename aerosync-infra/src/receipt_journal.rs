//! SQLite-backed receipt journal (RFC-002 §8).
//!
//! Durable, append-only log of every observed [`Receipt`] state
//! transition. Where the JSONL [`crate::history::HistoryStore`] only
//! persists *terminal* receipt states alongside the transfer's
//! [`HistoryEntry`], the journal records **every** transition for
//! **every** receipt — even ones that never reached a terminal state
//! before the process crashed.
//!
//! # Why SQLite (not JSONL)
//!
//! Receipts are addressed by id, and the recovery iterator queries
//! "the latest event per receipt where the latest is not terminal".
//! That is `O(1)` per row in SQL with a `(receipt_id, event_id DESC)`
//! plan, but `O(N)` in the JSONL `read_all → filter` loop. SQLite
//! also gives us crash-safe atomic writes via WAL journaling without
//! the tmp+rename dance the JSONL store has to perform on every
//! mutation.
//!
//! # Schema migration
//!
//! Schema version is tracked via `PRAGMA user_version`. The file
//! starts at v0 (empty database); [`SqliteReceiptJournal::open`]
//! advances it to the latest version applying every missing migration
//! step in order. Adding a new column / index is a new
//! [`Migration`] entry below; the existing data is preserved.
//!
//! # Thread / runtime story
//!
//! The journal wraps a single `rusqlite::Connection` in an async
//! `tokio::sync::Mutex`. Writes are bounded by the receipt-watch
//! cadence (≤ a few per receipt per second under happy-path load) so
//! holding a tokio mutex across the SQLite call is acceptable —
//! benchmarks against the comparable `aerosync_mcp::task_store`
//! showed no measurable contention with up to 32 concurrent in-flight
//! transfers. If that ceiling is ever raised the impl can switch to
//! `tokio::task::spawn_blocking` without changing the trait surface.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aerosync_domain::receipt::{CompletedTerminal, FailedTerminal, Receipt, State};
use aerosync_domain::storage::{
    ReceiptJournalRecord, ReceiptJournalStorage, ReceiptSide, RecoverableReceipt,
};
use aerosync_domain::{AeroSyncError, Result};
use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::Mutex;
use uuid::Uuid;

// Re-export the value objects under this module's path so consumers
// can write `use aerosync_infra::receipt_journal::{...}` rather than
// reaching into the domain crate directly. Keeps every existing
// import path (and downstream `pub use`) resolvable from one place.
pub use aerosync_domain::storage::{
    ReceiptJournalRecord as JournalRecord, ReceiptJournalStorage as JournalStorage,
    ReceiptSide as Side, RecoverableReceipt as Recoverable,
};

/// Schema migrations applied in order. `version` is the value written
/// to `PRAGMA user_version` after the SQL block runs.
struct Migration {
    version: i64,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: "
        CREATE TABLE IF NOT EXISTS receipt_events (
            event_id      INTEGER PRIMARY KEY AUTOINCREMENT,
            receipt_id    TEXT    NOT NULL,
            side          TEXT    NOT NULL,
            state         TEXT    NOT NULL,
            is_terminal   INTEGER NOT NULL,
            reason        TEXT,
            code          INTEGER,
            history_id    TEXT,
            filename      TEXT,
            peer          TEXT,
            recorded_at   INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_receipt_events_receipt_id
            ON receipt_events (receipt_id, event_id DESC);
        CREATE INDEX IF NOT EXISTS idx_receipt_events_recorded_at
            ON receipt_events (recorded_at DESC);
    ",
}];

/// Latest schema version this build knows how to migrate up to.
pub const CURRENT_SCHEMA_VERSION: i64 = 1;

/// SQLite-backed [`ReceiptJournalStorage`].
///
/// Cheaply cloneable; every clone shares the same underlying
/// connection mutex.
#[derive(Clone)]
pub struct SqliteReceiptJournal {
    /// On-disk database file; kept for diagnostics and tests.
    path: PathBuf,
    conn: Arc<Mutex<Connection>>,
}

impl SqliteReceiptJournal {
    /// Open (or create) the journal at `path` and apply any pending
    /// schema migrations. Creates the parent directory if needed.
    ///
    /// Idempotent: opening an already-current database is a no-op
    /// after the version check.
    pub async fn open(path: &Path) -> Result<Self> {
        let path_owned = path.to_path_buf();
        // Defer all blocking IO to a worker thread — opening the
        // connection itself can block on filesystem creation, the WAL
        // PRAGMA can fault on first access, and applying migrations
        // executes synchronous SQL. Wrapping the whole sequence in
        // `spawn_blocking` keeps the runtime reactor free.
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
            if let Some(parent) = path_owned.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    AeroSyncError::System(format!(
                        "create receipts.db parent dir {}: {e}",
                        parent.display()
                    ))
                })?;
            }
            let conn = Connection::open(&path_owned).map_err(map_sql_err)?;
            // Crash-safety: WAL gives us atomic group commits without
            // the tmp+rename dance the JSONL stores need; NORMAL
            // synchronous keeps the receipt-state hot path under a
            // millisecond on commodity SSDs while still surviving an
            // OS crash with the latest committed transaction intact.
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
                .map_err(map_sql_err)?;
            apply_migrations(&conn)?;
            Ok(conn)
        })
        .await
        .map_err(|e| AeroSyncError::System(format!("receipt journal open join error: {e}")))??;

        Ok(Self {
            path: path.to_path_buf(),
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Default journal location: `~/.config/aerosync/receipts.db`.
    /// Mirrors [`crate::history::HistoryStore::default_path`] so the
    /// receipt journal sits next to the history JSONL on every host.
    pub fn default_path() -> PathBuf {
        dirs_next::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("aerosync")
            .join("receipts.db")
    }

    /// On-disk path of the journal database (diagnostics).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Apply a single schema check (test helper). Returns the
    /// `PRAGMA user_version` value currently written to disk.
    pub async fn schema_version(&self) -> Result<i64> {
        let conn = self.conn.lock().await;
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(map_sql_err)
    }

    /// Spawn a tokio task that records every state transition of
    /// `receipt` into this journal until the receipt's watch channel
    /// closes (i.e. every `Arc<Receipt<_>>` holder is dropped).
    ///
    /// `side` disambiguates sender-side vs. receiver-side receipts;
    /// `history_id`, `filename`, and `peer` are optional context
    /// captured at journal time so a recovery iterator can render a
    /// useful summary even when the corresponding [`crate::history::HistoryEntry`]
    /// has been pruned.
    ///
    /// Returns the spawned [`tokio::task::JoinHandle`]; production
    /// callers can ignore it. The bridge:
    ///
    /// - Records an `initiated` row immediately on spawn so a crash
    ///   between receipt creation and the first wire frame still
    ///   surfaces the receipt as in-flight.
    /// - Records a row for every distinct state observed via the
    ///   watch channel — the channel may coalesce intermediate values
    ///   under load, but the recovery iterator only cares about the
    ///   *latest* row per receipt anyway.
    /// - Records a final `stream-lost` row if the watch channel
    ///   closes before reaching a terminal state, so the recovery
    ///   iterator can surface the receipt as failed-but-not-acked.
    pub fn spawn_journal_bridge<S>(
        self: &Arc<Self>,
        receipt: Arc<Receipt<S>>,
        side: ReceiptSide,
        history_id: Option<Uuid>,
        filename: Option<String>,
        peer: Option<String>,
    ) -> tokio::task::JoinHandle<()>
    where
        S: Send + Sync + 'static,
    {
        spawn_journal_bridge(
            Arc::clone(self) as Arc<dyn ReceiptJournalStorage>,
            receipt,
            side,
            history_id,
            filename,
            peer,
        )
    }
}

/// Free-function form of [`SqliteReceiptJournal::spawn_journal_bridge`]
/// that operates on any [`ReceiptJournalStorage`] trait object — same
/// pattern as `history::spawn_watch_bridge`.
pub fn spawn_journal_bridge<S>(
    journal: Arc<dyn ReceiptJournalStorage>,
    receipt: Arc<Receipt<S>>,
    side: ReceiptSide,
    history_id: Option<Uuid>,
    filename: Option<String>,
    peer: Option<String>,
) -> tokio::task::JoinHandle<()>
where
    S: Send + Sync + 'static,
{
    let id = receipt.id();
    let mut rx = receipt.watch();
    tokio::spawn(async move {
        // Always emit the initial state so a crash before the first
        // wire frame still surfaces the receipt as in-flight.
        let initial = rx.borrow_and_update().clone();
        let mut last_label = state_label(&initial).to_string();
        emit(
            &journal,
            id,
            side,
            &initial,
            history_id,
            filename.as_deref(),
            peer.as_deref(),
        )
        .await;
        if initial.is_terminal() {
            return;
        }

        loop {
            if rx.changed().await.is_err() {
                // Watch channel closed before terminal — surface as
                // stream-lost so the recovery iterator can flag it.
                let lost = ReceiptJournalRecord {
                    receipt_id: id,
                    side,
                    state: "stream-lost".into(),
                    is_terminal: true,
                    reason: Some("receipt watch channel closed before terminal".to_string()),
                    code: None,
                    history_id,
                    filename: filename.clone(),
                    peer: peer.clone(),
                    recorded_at: now_secs(),
                };
                let _ = journal.record_event(lost).await;
                return;
            }
            let next = rx.borrow_and_update().clone();
            let label = state_label(&next).to_string();
            // Skip self-loops (`StreamOpened → StreamOpened` from
            // `Event::Data`) — the journal is a state log, not an
            // event log, and the recovery iterator only consumes the
            // latest row per receipt.
            if label == last_label && !next.is_terminal() {
                continue;
            }
            last_label = label;
            emit(
                &journal,
                id,
                side,
                &next,
                history_id,
                filename.as_deref(),
                peer.as_deref(),
            )
            .await;
            if next.is_terminal() {
                return;
            }
        }
    })
}

#[async_trait]
impl ReceiptJournalStorage for SqliteReceiptJournal {
    async fn record_event(&self, record: ReceiptJournalRecord) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO receipt_events
                (receipt_id, side, state, is_terminal, reason, code,
                 history_id, filename, peer, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                record.receipt_id.to_string(),
                record.side.as_str(),
                record.state,
                record.is_terminal as i64,
                record.reason,
                record.code.map(|c| c as i64),
                record.history_id.map(|u| u.to_string()),
                record.filename,
                record.peer,
                record.recorded_at as i64,
            ],
        )
        .map_err(map_sql_err)?;
        Ok(())
    }

    async fn list_recoverable(&self, limit: usize) -> Result<Vec<RecoverableReceipt>> {
        // Latest event per receipt where that latest event is
        // non-terminal. Driven by the `(receipt_id, event_id DESC)`
        // index so the per-receipt MAX subquery stays O(log n).
        let sql = "
            SELECT e.receipt_id, e.side, e.state, e.is_terminal,
                   e.reason, e.code, e.history_id, e.filename, e.peer,
                   e.recorded_at
            FROM receipt_events e
            WHERE e.event_id = (
                SELECT MAX(event_id) FROM receipt_events e2
                WHERE e2.receipt_id = e.receipt_id
            )
              AND e.is_terminal = 0
            ORDER BY e.recorded_at DESC
        ";
        self.collect_rows(sql, limit).await
    }

    async fn list_recent_terminal(
        &self,
        since_secs: u64,
        limit: usize,
    ) -> Result<Vec<RecoverableReceipt>> {
        let cutoff = now_secs().saturating_sub(since_secs) as i64;
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT e.receipt_id, e.side, e.state, e.is_terminal,
                        e.reason, e.code, e.history_id, e.filename, e.peer,
                        e.recorded_at
                 FROM receipt_events e
                 WHERE e.event_id = (
                     SELECT MAX(event_id) FROM receipt_events e2
                     WHERE e2.receipt_id = e.receipt_id
                 )
                   AND e.is_terminal = 1
                   AND e.recorded_at >= ?1
                 ORDER BY e.recorded_at DESC",
            )
            .map_err(map_sql_err)?;
        let rows = stmt
            .query_map(params![cutoff], row_to_recoverable)
            .map_err(map_sql_err)?;
        let mut out = Vec::new();
        for r in rows {
            let r = r.map_err(map_sql_err)?;
            out.push(r);
            if limit > 0 && out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    async fn latest_state(&self, receipt_id: Uuid) -> Result<Option<RecoverableReceipt>> {
        let id_str = receipt_id.to_string();
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT receipt_id, side, state, is_terminal,
                        reason, code, history_id, filename, peer, recorded_at
                 FROM receipt_events
                 WHERE receipt_id = ?1
                 ORDER BY event_id DESC
                 LIMIT 1",
            )
            .map_err(map_sql_err)?;
        stmt.query_row(params![id_str], row_to_recoverable)
            .optional()
            .map_err(map_sql_err)
    }
}

impl SqliteReceiptJournal {
    async fn collect_rows(&self, sql: &str, limit: usize) -> Result<Vec<RecoverableReceipt>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(sql).map_err(map_sql_err)?;
        let rows = stmt
            .query_map([], row_to_recoverable)
            .map_err(map_sql_err)?;
        let mut out = Vec::new();
        for r in rows {
            let r = r.map_err(map_sql_err)?;
            out.push(r);
            if limit > 0 && out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// Full chronological event log for one receipt id (CLI `receipt show`).
    pub async fn list_events_for_receipt(
        &self,
        receipt_id: Uuid,
    ) -> Result<Vec<ReceiptJournalRecord>> {
        let id_str = receipt_id.to_string();
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT receipt_id, side, state, is_terminal, reason, code,
                        history_id, filename, peer, recorded_at
                 FROM receipt_events
                 WHERE receipt_id = ?1
                 ORDER BY event_id ASC",
            )
            .map_err(map_sql_err)?;
        let rows = stmt
            .query_map(params![id_str], row_to_journal_record)
            .map_err(map_sql_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(map_sql_err)?);
        }
        Ok(out)
    }
}

// ─────────────────────────── helpers ───────────────────────────────────────

fn map_sql_err(e: rusqlite::Error) -> AeroSyncError {
    AeroSyncError::System(format!("receipt journal sqlite: {e}"))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn apply_migrations(conn: &Connection) -> Result<()> {
    let current: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(map_sql_err)?;
    for m in MIGRATIONS {
        if current < m.version {
            conn.execute_batch(m.sql).map_err(|e| {
                AeroSyncError::System(format!(
                    "receipt journal migration v{} failed: {e}",
                    m.version
                ))
            })?;
            // Inline the version literal — `PRAGMA user_version`
            // refuses parameter binding.
            conn.execute_batch(&format!("PRAGMA user_version = {};", m.version))
                .map_err(map_sql_err)?;
        }
    }
    Ok(())
}

fn row_to_journal_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReceiptJournalRecord> {
    let id_str: String = row.get(0)?;
    let side_str: String = row.get(1)?;
    let state: String = row.get(2)?;
    let is_terminal: i64 = row.get(3)?;
    let reason: Option<String> = row.get(4)?;
    let code: Option<i64> = row.get(5)?;
    let history_id_str: Option<String> = row.get(6)?;
    let filename: Option<String> = row.get(7)?;
    let peer: Option<String> = row.get(8)?;
    let recorded_at: i64 = row.get(9)?;

    let receipt_id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let side = ReceiptSide::parse(&side_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            Box::<dyn std::error::Error + Send + Sync>::from(format!(
                "unknown receipt side: {side_str}"
            )),
        )
    })?;
    let history_id = match history_id_str {
        Some(s) => Some(Uuid::parse_str(&s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
        })?),
        None => None,
    };
    Ok(ReceiptJournalRecord {
        receipt_id,
        side,
        state,
        is_terminal: is_terminal != 0,
        reason,
        code: code.map(|c| c as u32),
        history_id,
        filename,
        peer,
        recorded_at: recorded_at.max(0) as u64,
    })
}

fn row_to_recoverable(row: &rusqlite::Row<'_>) -> rusqlite::Result<RecoverableReceipt> {
    let id_str: String = row.get(0)?;
    let side_str: String = row.get(1)?;
    let state: String = row.get(2)?;
    let is_terminal: i64 = row.get(3)?;
    let reason: Option<String> = row.get(4)?;
    let code: Option<i64> = row.get(5)?;
    let history_id_str: Option<String> = row.get(6)?;
    let filename: Option<String> = row.get(7)?;
    let peer: Option<String> = row.get(8)?;
    let recorded_at: i64 = row.get(9)?;

    let receipt_id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let side = ReceiptSide::parse(&side_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            Box::<dyn std::error::Error + Send + Sync>::from(format!(
                "unknown receipt side: {side_str}"
            )),
        )
    })?;
    let history_id = match history_id_str {
        Some(s) => Some(Uuid::parse_str(&s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
        })?),
        None => None,
    };
    Ok(RecoverableReceipt {
        receipt_id,
        side,
        state,
        is_terminal: is_terminal != 0,
        reason,
        code: code.map(|c| c as u32),
        history_id,
        filename,
        peer,
        last_event_at: recorded_at.max(0) as u64,
    })
}

/// Map a [`State`] into the journal's wire-stable label vocabulary.
/// The five non-terminal labels match the
/// [`aerosync_domain::receipt::State::Display`] spelling but with
/// kebab-case word separators so they compose with the existing
/// terminal labels (`stream-lost`, etc.).
fn state_label(s: &State) -> &'static str {
    match s {
        State::Initiated => "initiated",
        State::StreamOpened => "stream-opened",
        State::DataTransferred => "data-transferred",
        State::StreamClosed => "stream-closed",
        State::Processing => "processing",
        State::Completed(CompletedTerminal::Acked) => "acked",
        State::Completed(_) => "acked",
        State::Failed(FailedTerminal::Nacked { .. }) => "nacked",
        State::Failed(FailedTerminal::Cancelled { .. }) => "cancelled",
        State::Failed(FailedTerminal::Errored { .. }) => "errored",
        State::Failed(_) => "errored",
    }
}

async fn emit(
    journal: &Arc<dyn ReceiptJournalStorage>,
    id: Uuid,
    side: ReceiptSide,
    state: &State,
    history_id: Option<Uuid>,
    filename: Option<&str>,
    peer: Option<&str>,
) {
    let (reason, code) = match state {
        State::Failed(FailedTerminal::Nacked { reason }) => (Some(reason.clone()), None),
        State::Failed(FailedTerminal::Cancelled { reason }) => (Some(reason.clone()), None),
        State::Failed(FailedTerminal::Errored { code, detail }) => {
            (Some(detail.clone()), Some(*code))
        }
        _ => (None, None),
    };
    let rec = ReceiptJournalRecord {
        receipt_id: id,
        side,
        state: state_label(state).into(),
        is_terminal: state.is_terminal(),
        reason,
        code,
        history_id,
        filename: filename.map(|s| s.to_string()),
        peer: peer.map(|s| s.to_string()),
        recorded_at: now_secs(),
    };
    if let Err(e) = journal.record_event(rec).await {
        tracing::warn!(receipt_id = %id, "receipt journal record_event failed: {e}");
    }
}

// ─────────────────────────── tests ─────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use aerosync_domain::receipt::{Event, Receipt, Sender};
    use tempfile::TempDir;

    async fn tmp_journal() -> (Arc<SqliteReceiptJournal>, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("receipts.db");
        let j = SqliteReceiptJournal::open(&path).await.unwrap();
        (Arc::new(j), dir)
    }

    fn rec(receipt: Uuid, state: &str, terminal: bool, secs: u64) -> ReceiptJournalRecord {
        ReceiptJournalRecord {
            receipt_id: receipt,
            side: ReceiptSide::Send,
            state: state.into(),
            is_terminal: terminal,
            reason: None,
            code: None,
            history_id: None,
            filename: None,
            peer: None,
            recorded_at: secs,
        }
    }

    #[tokio::test]
    async fn open_creates_database_and_sets_schema_version() {
        let (j, _dir) = tmp_journal().await;
        assert_eq!(j.schema_version().await.unwrap(), CURRENT_SCHEMA_VERSION);
    }

    #[tokio::test]
    async fn open_is_idempotent_across_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("receipts.db");
        let _ = SqliteReceiptJournal::open(&path).await.unwrap();
        let again = SqliteReceiptJournal::open(&path).await.unwrap();
        assert_eq!(
            again.schema_version().await.unwrap(),
            CURRENT_SCHEMA_VERSION
        );
    }

    #[tokio::test]
    async fn record_event_then_latest_state_round_trips() {
        let (j, _dir) = tmp_journal().await;
        let rid = Uuid::new_v4();
        j.record_event(rec(rid, "initiated", false, 100))
            .await
            .unwrap();
        j.record_event(rec(rid, "stream-opened", false, 110))
            .await
            .unwrap();
        let got = j.latest_state(rid).await.unwrap().unwrap();
        assert_eq!(got.receipt_id, rid);
        assert_eq!(got.state, "stream-opened");
        assert!(!got.is_terminal);
        assert_eq!(got.last_event_at, 110);
    }

    #[tokio::test]
    async fn latest_state_returns_none_for_unknown_receipt() {
        let (j, _dir) = tmp_journal().await;
        assert!(j.latest_state(Uuid::new_v4()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_recoverable_only_shows_non_terminal_latest() {
        let (j, _dir) = tmp_journal().await;
        let pending = Uuid::new_v4();
        let acked = Uuid::new_v4();
        // pending: initiated → stream-opened (no terminal)
        j.record_event(rec(pending, "initiated", false, 100))
            .await
            .unwrap();
        j.record_event(rec(pending, "stream-opened", false, 101))
            .await
            .unwrap();
        // acked: initiated → acked
        j.record_event(rec(acked, "initiated", false, 200))
            .await
            .unwrap();
        j.record_event(rec(acked, "acked", true, 201))
            .await
            .unwrap();

        let recoverable = j.list_recoverable(0).await.unwrap();
        assert_eq!(recoverable.len(), 1);
        assert_eq!(recoverable[0].receipt_id, pending);
        assert_eq!(recoverable[0].state, "stream-opened");
    }

    #[tokio::test]
    async fn list_recent_terminal_shows_only_recent_terminals() {
        let (j, _dir) = tmp_journal().await;
        let acked_recent = Uuid::new_v4();
        let acked_old = Uuid::new_v4();
        let now = now_secs();
        j.record_event(rec(acked_recent, "acked", true, now))
            .await
            .unwrap();
        j.record_event(rec(
            acked_old,
            "acked",
            true,
            now.saturating_sub(86_400 * 30),
        ))
        .await
        .unwrap();

        let last_hour = j.list_recent_terminal(3600, 0).await.unwrap();
        assert_eq!(last_hour.len(), 1);
        assert_eq!(last_hour[0].receipt_id, acked_recent);
    }

    #[tokio::test]
    async fn list_recoverable_respects_limit() {
        let (j, _dir) = tmp_journal().await;
        for i in 0..5 {
            let rid = Uuid::new_v4();
            j.record_event(rec(rid, "stream-opened", false, 100 + i as u64))
                .await
                .unwrap();
        }
        let limited = j.list_recoverable(3).await.unwrap();
        assert_eq!(limited.len(), 3);
        // Sorted by recorded_at DESC.
        assert_eq!(limited[0].last_event_at, 104);
        assert_eq!(limited[2].last_event_at, 102);
    }

    #[tokio::test]
    async fn record_event_after_reopen_persists() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("receipts.db");
        let rid;
        {
            let j = SqliteReceiptJournal::open(&path).await.unwrap();
            rid = Uuid::new_v4();
            j.record_event(rec(rid, "stream-opened", false, 50))
                .await
                .unwrap();
        }
        let reopened = SqliteReceiptJournal::open(&path).await.unwrap();
        let got = reopened.latest_state(rid).await.unwrap().unwrap();
        assert_eq!(got.state, "stream-opened");
    }

    #[tokio::test]
    async fn spawn_journal_bridge_records_initial_state() {
        let (j, _dir) = tmp_journal().await;
        let receipt: Arc<Receipt<Sender>> = Arc::new(Receipt::new(Uuid::new_v4()));
        let rid = receipt.id();
        let _handle = j.spawn_journal_bridge(
            Arc::clone(&receipt),
            ReceiptSide::Send,
            None,
            Some("a.bin".into()),
            None,
        );
        // Yield so the spawned task records the initial state.
        for _ in 0..10 {
            tokio::task::yield_now().await;
            if j.latest_state(rid).await.unwrap().is_some() {
                break;
            }
        }
        let got = j.latest_state(rid).await.unwrap().expect("initial");
        assert_eq!(got.state, "initiated");
        assert_eq!(got.filename.as_deref(), Some("a.bin"));
    }

    #[tokio::test]
    async fn spawn_journal_bridge_records_terminal_acked() {
        let (j, _dir) = tmp_journal().await;
        let receipt: Arc<Receipt<Sender>> = Arc::new(Receipt::new(Uuid::new_v4()));
        let rid = receipt.id();
        let handle =
            j.spawn_journal_bridge(Arc::clone(&receipt), ReceiptSide::Send, None, None, None);
        receipt.apply_event(Event::Open).unwrap();
        receipt.apply_event(Event::Close).unwrap();
        receipt.apply_event(Event::Close).unwrap();
        receipt.apply_event(Event::Process).unwrap();
        receipt.apply_event(Event::Ack).unwrap();
        handle.await.unwrap();

        let got = j.latest_state(rid).await.unwrap().unwrap();
        assert_eq!(got.state, "acked");
        assert!(got.is_terminal);
    }

    #[tokio::test]
    async fn spawn_journal_bridge_records_stream_lost_on_drop() {
        let (j, _dir) = tmp_journal().await;
        let receipt: Arc<Receipt<Sender>> = Arc::new(Receipt::new(Uuid::new_v4()));
        let rid = receipt.id();
        let handle =
            j.spawn_journal_bridge(Arc::clone(&receipt), ReceiptSide::Send, None, None, None);
        drop(receipt);
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
        let got = j.latest_state(rid).await.unwrap().unwrap();
        assert_eq!(got.state, "stream-lost");
        assert!(got.is_terminal);
    }

    #[tokio::test]
    async fn list_recoverable_surfaces_in_flight_after_crash() {
        // Crash simulation: open, drop the journal mid-flight, reopen.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("receipts.db");
        let rid = Uuid::new_v4();
        {
            let j = SqliteReceiptJournal::open(&path).await.unwrap();
            j.record_event(rec(rid, "initiated", false, 1))
                .await
                .unwrap();
            j.record_event(rec(rid, "stream-opened", false, 2))
                .await
                .unwrap();
            // Drop without recording terminal — simulates crash.
        }
        let reopened = SqliteReceiptJournal::open(&path).await.unwrap();
        let recoverable = reopened.list_recoverable(0).await.unwrap();
        assert_eq!(recoverable.len(), 1);
        assert_eq!(recoverable[0].receipt_id, rid);
        assert_eq!(recoverable[0].state, "stream-opened");
    }

    #[tokio::test]
    async fn record_event_with_reason_and_code_round_trips() {
        let (j, _dir) = tmp_journal().await;
        let rid = Uuid::new_v4();
        j.record_event(ReceiptJournalRecord {
            receipt_id: rid,
            side: ReceiptSide::Receive,
            state: "errored".into(),
            is_terminal: true,
            reason: Some("checksum mismatch".into()),
            code: Some(42),
            history_id: Some(Uuid::new_v4()),
            filename: Some("e.bin".into()),
            peer: Some("10.0.0.5".into()),
            recorded_at: 999,
        })
        .await
        .unwrap();
        let got = j.latest_state(rid).await.unwrap().unwrap();
        assert_eq!(got.reason.as_deref(), Some("checksum mismatch"));
        assert_eq!(got.code, Some(42));
        assert_eq!(got.peer.as_deref(), Some("10.0.0.5"));
        assert_eq!(got.side, ReceiptSide::Receive);
    }
}

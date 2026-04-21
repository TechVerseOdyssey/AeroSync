//! 传输历史持久化模块
//!
//! 每条记录追加到 `~/.config/aerosync/history.jsonl`（JSONL 格式），
//! 提供按方向、协议、时间范围过滤的查询接口。
//!
//! # RFC-002 §8.2 receipt-state extension
//!
//! History records gain optional `receipt_id`, `receipt_state`,
//! `acked_at`, `nack_reason`, `cancel_reason` fields populated when
//! the transfer carried a [`Receipt`](crate::core::receipt::Receipt).
//! The store tracks terminal state per receipt via the
//! [`HistoryStore::record_receipt_terminal`] hook and exposes a
//! recovery iterator [`HistoryStore::iter_unfinished_receipts`] for
//! startup observability.
//!
//! ## v0.3.0 Phase 2.1b migration
//!
//! The pure-data types ([`ReceiptStateLabel`], [`HistoryEntry`],
//! [`HistoryQuery`], [`HistoryFilter`]) moved to
//! [`aerosync_domain::storage`] so the new
//! [`aerosync_domain::storage::HistoryStorage`] async trait can
//! reference them. This module re-exports them under their original
//! paths so every existing caller (`aerosync::core::history::*`,
//! `aerosync::core::*`) keeps resolving without source changes.
//!
//! ## v0.3.0 Phase 2.3 status (file-move deferred)
//!
//! [`HistoryStore`] now `impl`s
//! [`aerosync_domain::storage::HistoryStorage`] below — that part
//! of Phase 2.3 lands here in commit-form so Phase 2.4 can wire
//! `Arc<dyn HistoryStorage>` through the engine. The wholesale
//! `git mv src/core/history.rs → aerosync-infra/src/history.rs`
//! is **deferred to Phase 3** because [`HistoryStore::spawn_watch_bridge`]
//! takes `Arc<aerosync::core::receipt::Receipt<S>>`, and `receipt`
//! still lives in this root crate. Moving `history` first would
//! create an `aerosync-infra → aerosync` reverse dependency that
//! Cargo refuses (the root `aerosync` already depends on
//! `aerosync-infra`). Phase 3 promotes `Receipt` /
//! `TransferSession` to `aerosync-domain`, at which point the
//! `git mv` becomes free of the cycle and lands as Phase 3 follow-up.

pub use aerosync_domain::storage::{
    HistoryEntry, HistoryFilter, HistoryQuery, HistoryStorage, ReceiptStateLabel,
};

use crate::core::metadata::MetadataJson;
use crate::{AeroSyncError, Result};
use aerosync_proto::{Lifecycle, Metadata};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use uuid::Uuid;

// Test-only: HashMap is used by metadata-filter unit tests below.
// (After Phase 2.1b moved the HistoryQuery struct out of this file,
// HashMap is no longer needed in lib code.)
#[cfg(test)]
use std::collections::HashMap;

/// JSONL 格式传输历史存储
pub struct HistoryStore {
    path: PathBuf,
    file: Arc<Mutex<tokio::fs::File>>,
}

impl HistoryStore {
    /// 打开或新建历史文件
    pub async fn new(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        Ok(Self {
            path: path.to_path_buf(),
            file: Arc::new(Mutex::new(file)),
        })
    }

    /// 默认历史文件路径：`~/.config/aerosync/history.jsonl`
    pub fn default_path() -> PathBuf {
        dirs_next::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("aerosync")
            .join("history.jsonl")
    }

    /// 追加一条记录（fire-and-forget 版本，忽略写入错误）
    pub async fn append_silent(&self, entry: HistoryEntry) {
        let _ = self.append(entry).await;
    }

    /// 追加一条记录（返回 Result）
    pub async fn append(&self, entry: HistoryEntry) -> Result<()> {
        let mut line = serde_json::to_string(&entry)
            .map_err(|e| AeroSyncError::System(format!("History serialize error: {}", e)))?;
        line.push('\n');
        let mut file = self.file.lock().await;
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    /// 读取全部记录
    ///
    /// Holds the file mutex for the duration of the read so the
    /// caller can never observe a half-rewritten JSONL — both
    /// [`Self::append`] and [`Self::rewrite_locked`] take the same
    /// mutex when mutating the file. Combined with the
    /// `tmp + rename` strategy in `rewrite_locked`, this means
    /// concurrent readers always see either the OLD complete file or
    /// the NEW complete file, never a truncated window.
    pub async fn read_all(&self) -> Result<Vec<HistoryEntry>> {
        let _guard = self.file.lock().await;
        let content = tokio::fs::read_to_string(&self.path)
            .await
            .unwrap_or_default();
        let entries = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        Ok(entries)
    }

    /// 按条件查询历史记录（结果按 completed_at 降序）
    ///
    /// Linear scan over the JSONL file. Filters are applied in the
    /// order: direction → protocol → success → metadata → time
    /// window, then sorted by `completed_at` descending and
    /// truncated to `limit`. See [`HistoryQuery`] for the per-field
    /// semantics and the SQLite-migration trade-off note.
    pub async fn query(&self, q: &HistoryQuery) -> Result<Vec<HistoryEntry>> {
        let mut all = self.read_all().await?;

        if let Some(ref dir) = q.direction {
            all.retain(|e| &e.direction == dir);
        }
        if let Some(ref proto) = q.protocol {
            all.retain(|e| &e.protocol == proto);
        }
        if q.success_only {
            all.retain(|e| e.success);
        }
        if !q.metadata_eq.is_empty() {
            all.retain(|e| match &e.metadata {
                Some(m) => q
                    .metadata_eq
                    .iter()
                    .all(|(k, v)| m.user_metadata.get(k).map(|x| x == v).unwrap_or(false)),
                None => false,
            });
        }
        if let Some(ref tid) = q.trace_id {
            all.retain(|e| {
                e.metadata
                    .as_ref()
                    .and_then(|m| m.trace_id.as_deref())
                    .map(|x| x == tid)
                    .unwrap_or(false)
            });
        }
        if let Some(lc) = q.lifecycle {
            let target_label = match lc {
                Lifecycle::Unspecified => None,
                Lifecycle::Transient => Some("transient"),
                Lifecycle::Durable => Some("durable"),
                Lifecycle::Ephemeral => Some("ephemeral"),
            };
            if let Some(label) = target_label {
                all.retain(|e| {
                    e.metadata
                        .as_ref()
                        .and_then(|m| m.lifecycle.as_deref())
                        .map(|x| x == label)
                        .unwrap_or(false)
                });
            } else {
                // `Lifecycle::Unspecified` is a "match anything with
                // no lifecycle hint" probe — treat as no-op rather
                // than silently dropping every record. This mirrors
                // the proto-level meaning (default value).
            }
        }
        if let Some(since) = q.since {
            let cutoff = since.timestamp().max(0) as u64;
            all.retain(|e| e.completed_at >= cutoff);
        }
        if let Some(until) = q.until {
            let cutoff = until.timestamp().max(0) as u64;
            all.retain(|e| e.completed_at <= cutoff);
        }

        all.sort_by_key(|e| std::cmp::Reverse(e.completed_at));

        if q.limit > 0 && all.len() > q.limit {
            all.truncate(q.limit);
        }
        Ok(all)
    }

    /// Patch the [`Metadata`] envelope onto an existing record
    /// identified by either its `HistoryEntry::id` or its
    /// `receipt_id`. Returns `Ok(true)` when a record was updated,
    /// `Ok(false)` when no record matched. `O(N)` rewrite, like
    /// [`Self::record_receipt_terminal`] — same cost-and-cap caveat.
    pub async fn write_metadata(&self, record_id: Uuid, metadata: &Metadata) -> Result<bool> {
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
        self.rewrite_locked(&all).await?;
        Ok(true)
    }

    /// Re-write the JSONL file with `entries` while holding the
    /// append-handle lock. Used internally by
    /// [`Self::record_receipt_terminal`] and [`Self::write_metadata`].
    async fn rewrite_locked(&self, entries: &[HistoryEntry]) -> Result<()> {
        // Atomic-rename strategy: write the full new content to a
        // sibling `.tmp` file, fsync it, then rename it over the real
        // path. This eliminates the truncate-then-write window in
        // which a concurrent reader (e.g. `query` from another tokio
        // task, or another aerosync process sharing the same JSONL)
        // could observe a 0-byte file. Surfaced as a Windows-only
        // smoke-test flake before this change because NTFS makes the
        // truncate+write window much wider than ext4/APFS.
        let mut guard = self.file.lock().await;

        let tmp_path = self.path.with_extension("jsonl.tmp");
        {
            let mut tmp = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)
                .await?;
            for e in entries {
                let mut line = serde_json::to_string(e)
                    .map_err(|err| AeroSyncError::System(format!("History serialize: {err}")))?;
                line.push('\n');
                tmp.write_all(line.as_bytes()).await?;
            }
            tmp.flush().await?;
            // sync_all so a crash between rename and the next open
            // can't yield a 0-byte file in place of the real history.
            tmp.sync_all().await?;
        }

        // Windows refuses to rename over a path whose existing handle
        // was opened without FILE_SHARE_DELETE (which Rust's std does
        // not set). Drop the existing append handle BEFORE the
        // rename. To preserve the mutex invariant ("the guarded slot
        // always holds an open File"), we swap in a throwaway handle
        // on the tmp file just long enough to perform the rename,
        // then replace it with the freshly-opened append handle.
        let throwaway = tokio::fs::OpenOptions::new()
            .read(true)
            .open(&tmp_path)
            .await?;
        let _old_append = std::mem::replace(&mut *guard, throwaway);
        drop(_old_append);

        tokio::fs::rename(&tmp_path, &self.path).await?;

        *guard = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;
        Ok(())
    }

    /// 最近 N 条记录
    pub async fn recent(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        self.query(&HistoryQuery {
            limit,
            ..Default::default()
        })
        .await
    }

    // ── RFC-002 §8.2: receipt-state hooks ────────────────────────────

    /// Record a receipt-side terminal state transition for the
    /// transfer identified by `record_id` (which may be either the
    /// transfer's own [`HistoryEntry::id`] OR the receipt id, since
    /// some call sites only know the latter).
    ///
    /// The current store is JSONL append-only with no random update —
    /// to mutate a record we re-write the file with the patched entry.
    /// This is `O(N)` on history size and is meant for *infrequent*
    /// terminal transitions (≤ 10 / s typical), not chunk progress.
    /// If history grows past ~10K records this should be migrated to
    /// SQLite (RFC-002 §11 deferred to v0.2.1).
    ///
    /// `reason` populates `nack_reason` for `Nacked`, `cancel_reason`
    /// for `Cancelled`, and is concatenated into `error` for
    /// `Errored` / `StreamLost`. For `Acked` it is ignored.
    /// Returns `Ok(true)` if a record was patched, `Ok(false)` if no
    /// record was found (so callers can decide whether to insert a
    /// stub or just log).
    pub async fn record_receipt_terminal(
        &self,
        record_id: Uuid,
        state: ReceiptStateLabel,
        reason: Option<String>,
    ) -> Result<bool> {
        let mut all = self.read_all().await?;
        // Match by either history.id OR receipt_id; the watch-bridge
        // always knows the receipt_id but not necessarily the history
        // entry's own UUID.
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

        // Re-serialise the whole store. Hold the file lock across the
        // truncate+write so concurrent appends can't interleave a
        // half-written line.
        self.rewrite_locked(&all).await?;
        Ok(true)
    }

    /// Snapshot every record whose receipt is in a non-terminal state
    /// (or carries a receipt id but no recorded state) at the time of
    /// call. Useful at startup to surface "interrupted while waiting
    /// for ack" transfers via tracing.
    ///
    /// Returns an `IntoIterator` over owned [`HistoryEntry`]s rather
    /// than a borrowing iterator because the underlying storage is
    /// re-read each time and we don't want to hold a lock across the
    /// caller's iteration.
    pub async fn iter_unfinished_receipts(&self) -> Result<Vec<HistoryEntry>> {
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

    /// Locate a single history record by its receipt id. Returns
    /// `Ok(None)` if no record carries that id.
    pub async fn query_by_receipt(&self, receipt_id: Uuid) -> Result<Option<HistoryEntry>> {
        let all = self.read_all().await?;
        Ok(all.into_iter().find(|e| e.receipt_id == Some(receipt_id)))
    }

    /// Spawn a tokio task that watches a [`Receipt`] and persists
    /// every terminal transition via [`Self::record_receipt_terminal`].
    ///
    /// The task ends when the watch channel closes (i.e. the Receipt
    /// is dropped from the registry). The task is spawned with
    /// `tokio::spawn` and detached; the returned `JoinHandle` is
    /// returned for tests / explicit cancellation, but production
    /// callers can simply ignore it.
    ///
    /// The bridge uses the receipt id as the `record_id` argument to
    /// `record_receipt_terminal` — the call site is responsible for
    /// having previously appended a history entry whose `receipt_id`
    /// field matches.
    pub fn spawn_watch_bridge<S>(
        self: &Arc<Self>,
        receipt: Arc<crate::core::receipt::Receipt<S>>,
    ) -> tokio::task::JoinHandle<()>
    where
        S: Send + Sync + 'static,
    {
        use crate::core::receipt::{CompletedTerminal, FailedTerminal, State};

        let store = Arc::clone(self);
        let id = receipt.id();
        let mut rx = receipt.watch();
        tokio::spawn(async move {
            // Drive until terminal or watch channel closes.
            loop {
                let snapshot = rx.borrow_and_update().clone();
                if let Some((label, reason)) = match snapshot {
                    State::Initiated
                    | State::StreamOpened
                    | State::DataTransferred
                    | State::StreamClosed
                    | State::Processing => None,
                    State::Completed(CompletedTerminal::Acked) => {
                        Some((ReceiptStateLabel::Acked, None))
                    }
                    // Forward-compat wildcard required because
                    // `CompletedTerminal` is `#[non_exhaustive]` and
                    // this match is now in a different crate from the
                    // enum (post-Phase 3.4a `Receipt` move). Treat
                    // any future Completed variant as "acked" — it's
                    // by definition a successful terminal.
                    State::Completed(_) => Some((ReceiptStateLabel::Acked, None)),
                    State::Failed(FailedTerminal::Nacked { reason }) => {
                        Some((ReceiptStateLabel::Nacked, Some(reason)))
                    }
                    State::Failed(FailedTerminal::Cancelled { reason }) => {
                        Some((ReceiptStateLabel::Cancelled, Some(reason)))
                    }
                    State::Failed(FailedTerminal::Errored { code, detail }) => Some((
                        ReceiptStateLabel::Errored,
                        Some(format!("code={code} {detail}")),
                    )),
                    // Same forward-compat rationale as above. Unknown
                    // failed variants fall under `Errored` so the audit
                    // trail still records a useful diagnostic.
                    State::Failed(_) => Some((
                        ReceiptStateLabel::Errored,
                        Some("unknown failed terminal variant".to_string()),
                    )),
                } {
                    let _ = store.record_receipt_terminal(id, label, reason).await;
                    return;
                }

                if rx.changed().await.is_err() {
                    // Watch channel closed without reaching terminal.
                    let _ = store
                        .record_receipt_terminal(
                            id,
                            ReceiptStateLabel::StreamLost,
                            Some("receipt watch channel closed before terminal".to_string()),
                        )
                        .await;
                    return;
                }
            }
        })
    }
}

// ──────────────────────────── HistoryStorage impl ─────────────────────────────
//
// Bridges the inherent `HistoryStore` API to the
// `aerosync_domain::storage::HistoryStorage` trait introduced in
// Phase 2.1b. Each method just delegates to the inherent method of
// the same name. Inherent methods stay because some workspace
// consumers (e.g. `transfer.rs` line 220 `Option<Arc<HistoryStore>>`,
// `mcp/server.rs`) currently hold a concrete `HistoryStore` rather
// than `Arc<dyn HistoryStorage>`. Phase 2.4 will migrate those call
// sites to the trait-object form.

#[async_trait]
impl HistoryStorage for HistoryStore {
    async fn append(&self, entry: HistoryEntry) -> Result<()> {
        HistoryStore::append(self, entry).await
    }

    async fn query(&self, q: &HistoryQuery) -> Result<Vec<HistoryEntry>> {
        HistoryStore::query(self, q).await
    }

    async fn recent(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        HistoryStore::recent(self, limit).await
    }

    async fn write_metadata(&self, record_id: Uuid, metadata: &Metadata) -> Result<bool> {
        HistoryStore::write_metadata(self, record_id, metadata).await
    }

    async fn record_receipt_terminal(
        &self,
        record_id: Uuid,
        state: ReceiptStateLabel,
        reason: Option<String>,
    ) -> Result<bool> {
        HistoryStore::record_receipt_terminal(self, record_id, state, reason).await
    }

    async fn iter_unfinished_receipts(&self) -> Result<Vec<HistoryEntry>> {
        HistoryStore::iter_unfinished_receipts(self).await
    }

    async fn query_by_receipt(&self, receipt_id: Uuid) -> Result<Option<HistoryEntry>> {
        HistoryStore::query_by_receipt(self, receipt_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn tmp_store() -> (HistoryStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("history.jsonl");
        let store = HistoryStore::new(&path).await.unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn test_append_and_read_all() {
        let (store, _dir) = tmp_store().await;
        let entry = HistoryEntry::success("file.bin", None, 1024, None, None, "http", "send", 100);
        store.append(entry.clone()).await.unwrap();

        let all = store.read_all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].filename, "file.bin");
        assert_eq!(all[0].size, 1024);
        assert!(all[0].success);
    }

    #[tokio::test]
    async fn test_failure_entry() {
        let (store, _dir) = tmp_store().await;
        let entry = HistoryEntry::failure("bad.bin", 512, None, "quic", "receive", 50, "timeout");
        store.append(entry).await.unwrap();

        let all = store.read_all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert!(!all[0].success);
        assert_eq!(all[0].error.as_deref(), Some("timeout"));
    }

    #[tokio::test]
    async fn test_query_direction_filter() {
        let (store, _dir) = tmp_store().await;
        store
            .append(HistoryEntry::success(
                "a.bin", None, 1, None, None, "http", "send", 10,
            ))
            .await
            .unwrap();
        store
            .append(HistoryEntry::success(
                "b.bin", None, 2, None, None, "http", "receive", 10,
            ))
            .await
            .unwrap();

        let q = HistoryQuery {
            direction: Some("send".into()),
            ..Default::default()
        };
        let result = store.query(&q).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].filename, "a.bin");
    }

    #[tokio::test]
    async fn test_query_protocol_filter() {
        let (store, _dir) = tmp_store().await;
        store
            .append(HistoryEntry::success(
                "a.bin", None, 1, None, None, "http", "send", 10,
            ))
            .await
            .unwrap();
        store
            .append(HistoryEntry::success(
                "b.bin", None, 2, None, None, "quic", "send", 10,
            ))
            .await
            .unwrap();

        let q = HistoryQuery {
            protocol: Some("quic".into()),
            ..Default::default()
        };
        let result = store.query(&q).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].filename, "b.bin");
    }

    #[tokio::test]
    async fn test_query_success_only() {
        let (store, _dir) = tmp_store().await;
        store
            .append(HistoryEntry::success(
                "ok.bin", None, 1, None, None, "http", "send", 10,
            ))
            .await
            .unwrap();
        store
            .append(HistoryEntry::failure(
                "fail.bin", 1, None, "http", "send", 10, "err",
            ))
            .await
            .unwrap();

        let q = HistoryQuery {
            success_only: true,
            ..Default::default()
        };
        let result = store.query(&q).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].filename, "ok.bin");
    }

    #[tokio::test]
    async fn test_query_limit() {
        let (store, _dir) = tmp_store().await;
        for i in 0..5 {
            store
                .append(HistoryEntry::success(
                    format!("f{}.bin", i),
                    None,
                    i,
                    None,
                    None,
                    "http",
                    "send",
                    10,
                ))
                .await
                .unwrap();
        }
        let result = store.recent(3).await.unwrap();
        assert_eq!(result.len(), 3);
    }

    #[tokio::test]
    async fn test_avg_speed_bps() {
        let entry =
            HistoryEntry::success("x.bin", None, 1_000_000, None, None, "http", "send", 1000);
        assert_eq!(entry.avg_speed_bps, 1_000_000); // 1MB / 1s = 1MB/s
    }

    #[tokio::test]
    async fn test_jsonl_one_line_per_record() {
        let (store, _dir) = tmp_store().await;
        store
            .append(HistoryEntry::success(
                "a.bin", None, 1, None, None, "http", "send", 1,
            ))
            .await
            .unwrap();
        store
            .append(HistoryEntry::success(
                "b.bin", None, 2, None, None, "http", "send", 2,
            ))
            .await
            .unwrap();

        let content = tokio::fs::read_to_string(&store.path).await.unwrap();
        let lines: Vec<_> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        // 每行都是合法 JSON
        for line in &lines {
            assert!(serde_json::from_str::<serde_json::Value>(line).is_ok());
        }
    }

    #[tokio::test]
    async fn test_empty_store_returns_empty() {
        let (store, _dir) = tmp_store().await;
        let all = store.read_all().await.unwrap();
        assert!(all.is_empty());
    }

    // ── RFC-002 §8.2 receipt-state extension tests ───────────────────

    use crate::core::receipt::{Event, Receipt, Sender};

    /// Build a history entry pre-tagged with a receipt id and append
    /// it; returns the receipt id used.
    async fn append_with_receipt(store: &HistoryStore, name: &str) -> Uuid {
        let rid = Uuid::new_v4();
        let entry = HistoryEntry::success(name, None, 1, None, None, "quic", "send", 1)
            .with_receipt_id(rid);
        store.append(entry).await.unwrap();
        rid
    }

    #[tokio::test]
    async fn test_record_receipt_terminal_acked_sets_acked_at() {
        let (store, _dir) = tmp_store().await;
        let rid = append_with_receipt(&store, "a.bin").await;
        let patched = store
            .record_receipt_terminal(rid, ReceiptStateLabel::Acked, None)
            .await
            .unwrap();
        assert!(patched);

        let got = store.query_by_receipt(rid).await.unwrap().unwrap();
        assert_eq!(got.receipt_state, Some(ReceiptStateLabel::Acked));
        assert!(got.acked_at.is_some());
        assert!(got.nack_reason.is_none());
        assert!(got.cancel_reason.is_none());
    }

    #[tokio::test]
    async fn test_record_receipt_terminal_nacked_stores_reason() {
        let (store, _dir) = tmp_store().await;
        let rid = append_with_receipt(&store, "n.bin").await;
        store
            .record_receipt_terminal(
                rid,
                ReceiptStateLabel::Nacked,
                Some("checksum mismatch".to_string()),
            )
            .await
            .unwrap();

        let got = store.query_by_receipt(rid).await.unwrap().unwrap();
        assert_eq!(got.receipt_state, Some(ReceiptStateLabel::Nacked));
        assert_eq!(got.nack_reason.as_deref(), Some("checksum mismatch"));
    }

    #[tokio::test]
    async fn test_record_receipt_terminal_cancelled_stores_reason() {
        let (store, _dir) = tmp_store().await;
        let rid = append_with_receipt(&store, "c.bin").await;
        store
            .record_receipt_terminal(
                rid,
                ReceiptStateLabel::Cancelled,
                Some("user abort".to_string()),
            )
            .await
            .unwrap();
        let got = store.query_by_receipt(rid).await.unwrap().unwrap();
        assert_eq!(got.receipt_state, Some(ReceiptStateLabel::Cancelled));
        assert_eq!(got.cancel_reason.as_deref(), Some("user abort"));
    }

    #[tokio::test]
    async fn test_record_receipt_terminal_errored_overwrites_error() {
        let (store, _dir) = tmp_store().await;
        let rid = append_with_receipt(&store, "e.bin").await;
        store
            .record_receipt_terminal(
                rid,
                ReceiptStateLabel::Errored,
                Some("transport reset".to_string()),
            )
            .await
            .unwrap();
        let got = store.query_by_receipt(rid).await.unwrap().unwrap();
        assert_eq!(got.receipt_state, Some(ReceiptStateLabel::Errored));
        assert_eq!(got.error.as_deref(), Some("transport reset"));
    }

    #[tokio::test]
    async fn test_record_receipt_terminal_unknown_returns_false() {
        let (store, _dir) = tmp_store().await;
        let unknown = Uuid::new_v4();
        let patched = store
            .record_receipt_terminal(unknown, ReceiptStateLabel::Acked, None)
            .await
            .unwrap();
        assert!(!patched);
    }

    #[tokio::test]
    async fn test_iter_unfinished_returns_only_pending() {
        let (store, _dir) = tmp_store().await;
        // One entry with no receipt at all → not "unfinished".
        store
            .append(HistoryEntry::success(
                "plain.bin",
                None,
                1,
                None,
                None,
                "http",
                "send",
                1,
            ))
            .await
            .unwrap();
        // One pending receipt entry.
        let pending = append_with_receipt(&store, "pending.bin").await;
        // One acked receipt entry.
        let acked = append_with_receipt(&store, "acked.bin").await;
        store
            .record_receipt_terminal(acked, ReceiptStateLabel::Acked, None)
            .await
            .unwrap();

        let unfinished = store.iter_unfinished_receipts().await.unwrap();
        assert_eq!(unfinished.len(), 1);
        assert_eq!(unfinished[0].receipt_id, Some(pending));
    }

    #[tokio::test]
    async fn test_iter_unfinished_treats_explicit_initiated_as_pending() {
        let (store, _dir) = tmp_store().await;
        let rid = append_with_receipt(&store, "i.bin").await;
        store
            .record_receipt_terminal(rid, ReceiptStateLabel::Initiated, None)
            .await
            .unwrap();

        let unfinished = store.iter_unfinished_receipts().await.unwrap();
        assert_eq!(unfinished.len(), 1);
        assert_eq!(unfinished[0].receipt_id, Some(rid));
    }

    #[tokio::test]
    async fn test_iter_unfinished_empty_when_all_terminal() {
        let (store, _dir) = tmp_store().await;
        let rid = append_with_receipt(&store, "x.bin").await;
        store
            .record_receipt_terminal(rid, ReceiptStateLabel::Acked, None)
            .await
            .unwrap();
        let unfinished = store.iter_unfinished_receipts().await.unwrap();
        assert!(unfinished.is_empty());
    }

    #[tokio::test]
    async fn test_query_by_receipt_returns_some() {
        let (store, _dir) = tmp_store().await;
        let rid = append_with_receipt(&store, "q.bin").await;
        let got = store.query_by_receipt(rid).await.unwrap();
        assert!(got.is_some());
        assert_eq!(got.unwrap().filename, "q.bin");
    }

    #[tokio::test]
    async fn test_query_by_receipt_returns_none_for_unknown() {
        let (store, _dir) = tmp_store().await;
        append_with_receipt(&store, "q.bin").await;
        let got = store.query_by_receipt(Uuid::new_v4()).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn test_record_receipt_terminal_persists_across_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("history.jsonl");
        let store = HistoryStore::new(&path).await.unwrap();
        let rid = append_with_receipt(&store, "p.bin").await;
        store
            .record_receipt_terminal(rid, ReceiptStateLabel::Acked, None)
            .await
            .unwrap();
        drop(store);

        let reopened = HistoryStore::new(&path).await.unwrap();
        let got = reopened.query_by_receipt(rid).await.unwrap().unwrap();
        assert_eq!(got.receipt_state, Some(ReceiptStateLabel::Acked));
    }

    #[tokio::test]
    async fn test_append_after_record_terminal_does_not_lose_data() {
        let (store, _dir) = tmp_store().await;
        let rid = append_with_receipt(&store, "first.bin").await;
        store
            .record_receipt_terminal(rid, ReceiptStateLabel::Acked, None)
            .await
            .unwrap();
        // After the rewrite, the append handle should still extend the
        // current file rather than the stale fd.
        store
            .append(HistoryEntry::success(
                "second.bin",
                None,
                1,
                None,
                None,
                "http",
                "send",
                1,
            ))
            .await
            .unwrap();
        let all = store.read_all().await.unwrap();
        assert_eq!(all.len(), 2);
        let names: Vec<_> = all.iter().map(|e| e.filename.as_str()).collect();
        assert!(names.contains(&"first.bin"));
        assert!(names.contains(&"second.bin"));
    }

    #[tokio::test]
    async fn test_watch_bridge_persists_acked_terminal() {
        let (store, _dir) = tmp_store().await;
        let store = Arc::new(store);

        let receipt: Arc<Receipt<Sender>> = Arc::new(Receipt::new(Uuid::new_v4()));
        let rid = receipt.id();
        let entry = HistoryEntry::success("w.bin", None, 1, None, None, "quic", "send", 1)
            .with_receipt_id(rid);
        store.append(entry).await.unwrap();

        let handle = store.spawn_watch_bridge(Arc::clone(&receipt));

        // Drive the receipt through the happy path.
        receipt.apply_event(Event::Open).unwrap();
        receipt.apply_event(Event::Close).unwrap();
        receipt.apply_event(Event::Close).unwrap();
        receipt.apply_event(Event::Process).unwrap();
        receipt.apply_event(Event::Ack).unwrap();

        handle.await.unwrap();

        let got = store.query_by_receipt(rid).await.unwrap().unwrap();
        assert_eq!(got.receipt_state, Some(ReceiptStateLabel::Acked));
        assert!(got.acked_at.is_some());
    }

    #[tokio::test]
    async fn test_watch_bridge_persists_nacked_terminal() {
        let (store, _dir) = tmp_store().await;
        let store = Arc::new(store);

        let receipt: Arc<Receipt<Sender>> = Arc::new(Receipt::new(Uuid::new_v4()));
        let rid = receipt.id();
        let entry = HistoryEntry::success("n.bin", None, 1, None, None, "quic", "send", 1)
            .with_receipt_id(rid);
        store.append(entry).await.unwrap();

        let handle = store.spawn_watch_bridge(Arc::clone(&receipt));
        receipt.apply_event(Event::Open).unwrap();
        receipt.apply_event(Event::Close).unwrap();
        receipt.apply_event(Event::Close).unwrap();
        receipt.apply_event(Event::Process).unwrap();
        receipt
            .apply_event(Event::Nack {
                reason: "bad payload".to_string(),
            })
            .unwrap();

        handle.await.unwrap();

        let got = store.query_by_receipt(rid).await.unwrap().unwrap();
        assert_eq!(got.receipt_state, Some(ReceiptStateLabel::Nacked));
        assert_eq!(got.nack_reason.as_deref(), Some("bad payload"));
    }

    // ── RFC-003 Group B: metadata persistence + filtering tests ──────

    use crate::core::metadata::MetadataBuilder;

    fn meta_with(trace: &str, lifecycle: Lifecycle, k: &str, v: &str) -> Metadata {
        let mut m = MetadataBuilder::new()
            .trace_id(trace)
            .lifecycle(lifecycle)
            .user(k, v)
            .build()
            .unwrap();
        m.id = Uuid::new_v4().to_string();
        m.from_node = "alice".into();
        m.to_node = "bob".into();
        m.protocol = "quic".into();
        m
    }

    #[tokio::test]
    async fn test_old_jsonl_records_load_with_metadata_none() {
        // Backfill safety: a JSONL line written before Group B (no
        // `metadata` key) must deserialize cleanly with metadata =
        // None. We hand-write the line so the test is independent of
        // the current `success()` constructor's default.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("history.jsonl");
        let pre_b = format!(
            "{{\"id\":\"{}\",\"filename\":\"old.bin\",\"saved_path\":null,\"size\":1,\"sha256\":null,\"remote_ip\":null,\"protocol\":\"http\",\"direction\":\"send\",\"completed_at\":1700000000,\"duration_ms\":1,\"avg_speed_bps\":1000,\"success\":true,\"error\":null}}\n",
            Uuid::new_v4()
        );
        tokio::fs::write(&path, pre_b).await.unwrap();
        let store = HistoryStore::new(&path).await.unwrap();
        let all = store.read_all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].metadata.is_none());
    }

    #[tokio::test]
    async fn test_with_metadata_round_trips_through_jsonl() {
        let (store, _dir) = tmp_store().await;
        let m = meta_with("run-1", Lifecycle::Transient, "tenant", "acme");
        let entry = HistoryEntry::success("a.bin", None, 1, None, None, "quic", "send", 1)
            .with_receipt_id(Uuid::new_v4())
            .with_metadata_proto(&m);
        store.append(entry.clone()).await.unwrap();

        let reopened = HistoryStore::new(&store.path).await.unwrap();
        let all = reopened.read_all().await.unwrap();
        assert_eq!(all.len(), 1);
        let got = all[0].metadata.as_ref().unwrap();
        assert_eq!(got.trace_id.as_deref(), Some("run-1"));
        assert_eq!(got.lifecycle.as_deref(), Some("transient"));
        assert_eq!(got.user_metadata["tenant"], "acme");
    }

    #[tokio::test]
    async fn test_query_filter_by_trace_id() {
        let (store, _dir) = tmp_store().await;
        for i in 0..3 {
            let m = meta_with(
                if i == 1 { "target" } else { "other" },
                Lifecycle::Durable,
                "k",
                &format!("v{i}"),
            );
            let entry =
                HistoryEntry::success(format!("f{i}.bin"), None, 1, None, None, "quic", "send", 1)
                    .with_metadata_proto(&m);
            store.append(entry).await.unwrap();
        }

        let q = HistoryFilter {
            trace_id: Some("target".into()),
            ..Default::default()
        };
        let res = store.query(&q).await.unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].filename, "f1.bin");
    }

    #[tokio::test]
    async fn test_query_filter_by_lifecycle() {
        let (store, _dir) = tmp_store().await;
        for (i, lc) in [
            Lifecycle::Transient,
            Lifecycle::Durable,
            Lifecycle::Ephemeral,
        ]
        .into_iter()
        .enumerate()
        {
            let m = meta_with("trace", lc, "k", "v");
            let entry =
                HistoryEntry::success(format!("f{i}.bin"), None, 1, None, None, "quic", "send", 1)
                    .with_metadata_proto(&m);
            store.append(entry).await.unwrap();
        }
        let res = store
            .query(&HistoryFilter {
                lifecycle: Some(Lifecycle::Durable),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].filename, "f1.bin");
    }

    #[tokio::test]
    async fn test_query_filter_by_metadata_eq_requires_all_pairs() {
        let (store, _dir) = tmp_store().await;
        let mk = |i: usize, k1: &str, v1: &str, k2: &str, v2: &str| {
            let m = MetadataBuilder::new()
                .user(k1, v1)
                .user(k2, v2)
                .build()
                .unwrap();
            HistoryEntry::success(format!("f{i}.bin"), None, 1, None, None, "quic", "send", 1)
                .with_metadata_proto(&m)
        };
        store.append(mk(0, "a", "1", "b", "2")).await.unwrap();
        store.append(mk(1, "a", "1", "b", "3")).await.unwrap();
        store.append(mk(2, "a", "9", "b", "2")).await.unwrap();

        let mut want = HashMap::new();
        want.insert("a".into(), "1".into());
        want.insert("b".into(), "2".into());
        let res = store
            .query(&HistoryFilter {
                metadata_eq: want,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].filename, "f0.bin");
    }

    #[tokio::test]
    async fn test_query_filter_since_until_window() {
        let (store, _dir) = tmp_store().await;
        // Hand-craft entries with explicit completed_at so the test
        // is deterministic regardless of wall-clock at write time.
        for ts in [1_700_000_000u64, 1_700_001_000, 1_700_002_000] {
            let mut e =
                HistoryEntry::success(format!("t{ts}"), None, 1, None, None, "h", "send", 1);
            e.completed_at = ts;
            store.append(e).await.unwrap();
        }
        let since = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_500, 0).unwrap();
        let until = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_001_500, 0).unwrap();
        let res = store
            .query(&HistoryFilter {
                since: Some(since),
                until: Some(until),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].filename, "t1700001000");
    }

    #[tokio::test]
    async fn test_query_filter_no_metadata_does_not_match_trace_id() {
        let (store, _dir) = tmp_store().await;
        store
            .append(HistoryEntry::success(
                "plain.bin",
                None,
                1,
                None,
                None,
                "h",
                "send",
                1,
            ))
            .await
            .unwrap();
        let res = store
            .query(&HistoryFilter {
                trace_id: Some("anything".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(res.is_empty());
    }

    #[tokio::test]
    async fn test_write_metadata_patches_existing_record() {
        let (store, _dir) = tmp_store().await;
        let rid = Uuid::new_v4();
        let entry = HistoryEntry::success("x.bin", None, 1, None, None, "quic", "send", 1)
            .with_receipt_id(rid);
        store.append(entry).await.unwrap();

        let m = meta_with("late-trace", Lifecycle::Ephemeral, "k", "v");
        let patched = store.write_metadata(rid, &m).await.unwrap();
        assert!(patched);

        let got = store.query_by_receipt(rid).await.unwrap().unwrap();
        let meta = got.metadata.as_ref().unwrap();
        assert_eq!(meta.trace_id.as_deref(), Some("late-trace"));
        assert_eq!(meta.lifecycle.as_deref(), Some("ephemeral"));
    }

    #[tokio::test]
    async fn test_write_metadata_returns_false_for_unknown_id() {
        let (store, _dir) = tmp_store().await;
        let m = meta_with("t", Lifecycle::Durable, "k", "v");
        let patched = store.write_metadata(Uuid::new_v4(), &m).await.unwrap();
        assert!(!patched);
    }

    #[tokio::test]
    async fn test_write_metadata_then_query_by_metadata_eq() {
        // End-to-end: a transfer is written without metadata first
        // (e.g. legacy adapter), Group B's `write_metadata` adds it
        // after the fact, and a metadata-aware query then surfaces
        // the patched record.
        let (store, _dir) = tmp_store().await;
        let rid = Uuid::new_v4();
        let entry = HistoryEntry::success("late.bin", None, 1, None, None, "quic", "send", 1)
            .with_receipt_id(rid);
        store.append(entry).await.unwrap();

        let m = MetadataBuilder::new()
            .trace_id("after")
            .user("tenant", "acme")
            .build()
            .unwrap();
        store.write_metadata(rid, &m).await.unwrap();

        let mut want = HashMap::new();
        want.insert("tenant".into(), "acme".into());
        let res = store
            .query(&HistoryFilter {
                metadata_eq: want,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].filename, "late.bin");
    }

    #[tokio::test]
    async fn test_watch_bridge_persists_stream_lost_when_dropped() {
        // When every Receipt holder is dropped before any terminal
        // event lands, the watch channel closes and the bridge must
        // record `StreamLost` so the recovery iterator surfaces it.
        let (store, _dir) = tmp_store().await;
        let store = Arc::new(store);

        let receipt: Arc<Receipt<Sender>> = Arc::new(Receipt::new(Uuid::new_v4()));
        let rid = receipt.id();
        let entry = HistoryEntry::success("d.bin", None, 1, None, None, "quic", "send", 1)
            .with_receipt_id(rid);
        store.append(entry).await.unwrap();

        // `spawn_watch_bridge` only retains the watch::Receiver, not
        // the Arc<Receipt> — so dropping our last reference closes
        // the channel and fires the StreamLost branch.
        let handle = store.spawn_watch_bridge(Arc::clone(&receipt));
        drop(receipt);

        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("watch bridge did not terminate within timeout")
            .unwrap();

        let got = store.query_by_receipt(rid).await.unwrap().unwrap();
        assert_eq!(got.receipt_state, Some(ReceiptStateLabel::StreamLost));
    }
}

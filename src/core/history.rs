/// 传输历史持久化模块
///
/// 每条记录追加到 `~/.config/aerosync/history.jsonl`（JSONL 格式），
/// 提供按方向、协议、时间范围过滤的查询接口。
///
/// # RFC-002 §8.2 receipt-state extension
///
/// History records gain optional `receipt_id`, `receipt_state`,
/// `acked_at`, `nack_reason`, `cancel_reason` fields populated when
/// the transfer carried a [`Receipt`](crate::core::receipt::Receipt).
/// The store tracks terminal state per receipt via the
/// [`HistoryStore::record_receipt_terminal`] hook and exposes a
/// recovery iterator [`HistoryStore::iter_unfinished_receipts`] for
/// startup observability. RFC-002 §11 ResumeStore-side persistence
/// is explicitly OUT OF SCOPE for v0.2.0 (deferred to v0.2.1) — this
/// module persists *history*, not resumable state.
use crate::core::metadata::MetadataJson;
use crate::{AeroSyncError, Result};
use aerosync_proto::{Lifecycle, Metadata};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use uuid::Uuid;

/// String label of a receipt's terminal-or-pending state, persisted
/// alongside the [`HistoryEntry`].
///
/// Matches the canonical wire spelling of the seven generic states
/// in [`crate::core::receipt::State`] but flattens the terminal
/// payloads (Acked / Nacked / Cancelled / Errored) since the
/// reason / detail strings already live in dedicated columns.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReceiptStateLabel {
    /// Receipt freshly created; nothing on the wire yet.
    Initiated,
    /// Receipt acked by the receiver application (terminal success).
    Acked,
    /// Receipt nacked by the receiver application (terminal failure).
    Nacked,
    /// Cancelled from either side (terminal failure).
    Cancelled,
    /// Transport / verification error (terminal failure).
    Errored,
    /// Receipt stream went silent before terminal (terminal failure).
    StreamLost,
}

impl ReceiptStateLabel {
    /// True when the label represents a terminal lifecycle state.
    pub fn is_terminal(self) -> bool {
        !matches!(self, ReceiptStateLabel::Initiated)
    }
}

/// 单条传输历史记录
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistoryEntry {
    pub id: Uuid,
    pub filename: String,
    /// 接收方保存路径（可选，发送方为 None）
    pub saved_path: Option<PathBuf>,
    pub size: u64,
    pub sha256: Option<String>,
    /// 对端 IP
    pub remote_ip: Option<String>,
    /// "http" / "quic" / "s3" / "ftp"
    pub protocol: String,
    /// "send" / "receive"
    pub direction: String,
    /// Unix 时间戳（秒）
    pub completed_at: u64,
    /// 传输耗时（毫秒）
    pub duration_ms: u64,
    /// 平均速度（bytes/s）
    pub avg_speed_bps: u64,
    /// 是否成功
    pub success: bool,
    /// 失败原因（success=false 时非空）
    pub error: Option<String>,
    /// RFC-002 §8.2: receipt id when the transfer carried a Receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_id: Option<Uuid>,
    /// RFC-002 §8.2: most-recently-observed receipt state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_state: Option<ReceiptStateLabel>,
    /// Unix timestamp (seconds) when ack landed; `None` if not acked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acked_at: Option<u64>,
    /// Reason string when `receipt_state == Nacked`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nack_reason: Option<String>,
    /// Reason string when `receipt_state == Cancelled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_reason: Option<String>,
    /// RFC-003 metadata envelope captured at send/receive time.
    /// Stored as the JSON-shaped [`MetadataJson`] adapter so the
    /// on-disk format is decoupled from the wire protobuf and remains
    /// human-readable. Old JSONL records (pre-Week-4) round-trip
    /// safely: the `serde(default)` makes the field implicit `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<MetadataJson>,
}

impl HistoryEntry {
    /// 创建一条成功的传输记录
    #[allow(clippy::too_many_arguments)]
    pub fn success(
        filename: impl Into<String>,
        saved_path: Option<PathBuf>,
        size: u64,
        sha256: Option<String>,
        remote_ip: Option<String>,
        protocol: impl Into<String>,
        direction: impl Into<String>,
        duration_ms: u64,
    ) -> Self {
        let avg_speed_bps = size
            .saturating_mul(1000)
            .checked_div(duration_ms)
            .unwrap_or(size);
        Self {
            id: Uuid::new_v4(),
            filename: filename.into(),
            saved_path,
            size,
            sha256,
            remote_ip,
            protocol: protocol.into(),
            direction: direction.into(),
            completed_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            duration_ms,
            avg_speed_bps,
            success: true,
            error: None,
            receipt_id: None,
            receipt_state: None,
            acked_at: None,
            nack_reason: None,
            cancel_reason: None,
            metadata: None,
        }
    }

    /// 创建一条失败的传输记录
    pub fn failure(
        filename: impl Into<String>,
        size: u64,
        remote_ip: Option<String>,
        protocol: impl Into<String>,
        direction: impl Into<String>,
        duration_ms: u64,
        error: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            filename: filename.into(),
            saved_path: None,
            size,
            sha256: None,
            remote_ip,
            protocol: protocol.into(),
            direction: direction.into(),
            completed_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            duration_ms,
            avg_speed_bps: 0,
            success: false,
            error: Some(error.into()),
            receipt_id: None,
            receipt_state: None,
            acked_at: None,
            nack_reason: None,
            cancel_reason: None,
            metadata: None,
        }
    }

    /// Builder-style helper: attach a receipt id to a freshly built
    /// entry. Used by the watch-bridge so the recovery iterator can
    /// query by receipt later.
    pub fn with_receipt_id(mut self, id: Uuid) -> Self {
        self.receipt_id = Some(id);
        self
    }

    /// Builder-style helper: attach the RFC-003 metadata envelope
    /// (proto shape) to a freshly built entry. Internally projected
    /// to [`MetadataJson`] for serde-friendly persistence.
    pub fn with_metadata_proto(mut self, m: &Metadata) -> Self {
        self.metadata = Some(MetadataJson::from_proto(m));
        self
    }

    /// Builder-style helper: attach a [`MetadataJson`] directly. Used
    /// by code paths that already hold the JSON shape (e.g. when
    /// re-emitting a historical record).
    pub fn with_metadata_json(mut self, m: MetadataJson) -> Self {
        self.metadata = Some(m);
        self
    }
}

/// 传输历史查询过滤器
///
/// RFC-003 Group B extends this with metadata-aware filters
/// (`metadata_eq`, `trace_id`, `lifecycle`, `since`, `until`). The
/// query engine performs a **linear scan** over the JSONL file —
/// `O(N)` on history size. This is acceptable up to the ~10K-record
/// horizon at which RFC-003 §7 says we should migrate to SQLite. The
/// SQLite migration is deferred to v0.2.1 by the w4-metadata plan;
/// see `docs/protocol/metadata-v1.md` for the trade-off discussion.
#[derive(Debug, Default, Clone)]
pub struct HistoryQuery {
    /// 过滤方向（"send" / "receive"），None = 全部
    pub direction: Option<String>,
    /// 过滤协议，None = 全部
    pub protocol: Option<String>,
    /// 只返回成功记录
    pub success_only: bool,
    /// 最多返回 N 条（0 = 不限）
    pub limit: usize,
    /// All `(k, v)` pairs in this map MUST be present in the entry's
    /// `metadata.user_metadata` for the entry to match. Empty map ⇒
    /// no constraint.
    pub metadata_eq: HashMap<String, String>,
    /// Match only entries whose `metadata.trace_id` equals this. None
    /// ⇒ no constraint. An entry with no metadata never matches a
    /// non-`None` value.
    pub trace_id: Option<String>,
    /// Match only entries whose `metadata.lifecycle` equals this.
    pub lifecycle: Option<Lifecycle>,
    /// Match only entries with `completed_at >= since` (inclusive).
    /// Compared against the chrono UTC timestamp; the underlying
    /// `completed_at` is a Unix-second `u64`.
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    /// Match only entries with `completed_at <= until` (inclusive).
    pub until: Option<chrono::DateTime<chrono::Utc>>,
}

/// Alias for [`HistoryQuery`] under the RFC-003 plan name. Both
/// names reach the same type so callers can pick whichever reads
/// better in context (`HistoryFilter` for metadata-driven searches,
/// `HistoryQuery` for the legacy direction/protocol/success filters).
pub type HistoryFilter = HistoryQuery;

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
    pub async fn read_all(&self) -> Result<Vec<HistoryEntry>> {
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
        let mut guard = self.file.lock().await;
        let mut new = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)
            .await?;
        for e in entries {
            let mut line = serde_json::to_string(e)
                .map_err(|err| AeroSyncError::System(format!("History serialize: {err}")))?;
            line.push('\n');
            new.write_all(line.as_bytes()).await?;
        }
        new.flush().await?;
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

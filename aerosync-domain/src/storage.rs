//! Storage abstractions and pure-data types for the resume + history
//! persistence layers.
//!
//! Per `docs/v0.3.0-refactor-plan.md` §3 Phase 2, this module owns:
//!
//! 1. The **value objects** ([`crate::storage::ChunkState`],
//!    [`crate::storage::ResumeState`], plus history-side types added
//!    in a follow-up sub-commit) that transit between application
//!    code and any persistent backend. They live in `aerosync-domain`
//!    so they have zero IO/networking deps and can be referenced by
//!    the trait signatures below.
//! 2. The **storage traits** ([`crate::storage::ResumeStorage`], plus
//!    [`crate::storage::HistoryStorage`] in a follow-up sub-commit)
//!    that `TransferEngine` (see `aerosync` main crate) and friends
//!    consume via `Arc<dyn …>`. Concrete implementations live in
//!    `aerosync-infra::resume` (file-backed JSON; Phase 2.2) and
//!    `aerosync-infra::history` (file-backed JSONL; Phase 2.3).
//!
//! ## Why split data ↔ trait ↔ impl across three crates?
//!
//! - Lets us swap the on-disk JSON impl for an in-memory mock during
//!   tests without touching consumers.
//! - Lets a future cluster-mode (RFC-005?) substitute Redis-backed
//!   resume storage by writing a new `aerosync-cluster` crate that
//!   only depends on `aerosync-domain` — no need to fork the engine.
//! - Keeps `cargo doc --open` on the public crate surface tight: the
//!   domain trait surface is the contract; impls are deliberately
//!   internal.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use aerosync_proto::{Lifecycle, Metadata};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::metadata::MetadataJson;
use crate::Result;

// ──────────────────────────── Resume value objects ─────────────────────────────
//
// Source: lifted verbatim from `src/core/resume.rs` (lines 14-137,
// pre-v0.3.0). Method bodies, derives, doc strings, and field
// visibilities are unchanged. The only difference is the home
// crate — see the `pub use aerosync_domain::storage::{ChunkState,
// ResumeState, DEFAULT_CHUNK_SIZE};` re-export in
// `aerosync::core::resume` that keeps the legacy import paths
// `aerosync::core::resume::ResumeState` resolving for downstream
// callers.

/// 默认分片大小：32 MB。
///
/// Re-exported as `aerosync::core::DEFAULT_CHUNK_SIZE` (and via the
/// resume module shim) so existing call sites in
/// `core::transfer::TransferEngine` and the Python binding keep
/// resolving without changes.
pub const DEFAULT_CHUNK_SIZE: u64 = 32 * 1024 * 1024;

/// 单个分片的状态
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChunkState {
    /// 分片序号（0-based）
    pub index: u32,
    /// 该分片在文件中的起始字节偏移
    pub offset: u64,
    /// 该分片的实际大小（最后一片可能 < chunk_size）
    pub size: u64,
    /// 该分片的 SHA-256（可选，用于单分片校验）
    pub sha256: Option<String>,
}

/// 整个传输任务的续传状态
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeState {
    /// 任务唯一 ID
    pub task_id: Uuid,
    /// 本地源文件绝对路径
    pub source_path: PathBuf,
    /// 目标地址（URL）
    pub destination: String,
    /// 文件总大小（bytes）
    pub total_size: u64,
    /// 分片大小（bytes）
    pub chunk_size: u64,
    /// 总分片数
    pub total_chunks: u32,
    /// 已成功完成的分片序号列表
    pub completed_chunks: Vec<u32>,
    /// 整文件 SHA-256（预计算，用于最终校验）
    pub sha256: Option<String>,
    /// 创建时间（Unix timestamp seconds）
    pub created_at: u64,
    /// 最后更新时间（Unix timestamp seconds）
    pub updated_at: u64,
    /// 协议特定的扩展元数据（如 S3 UploadId、已完成 parts）
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

impl ResumeState {
    /// 根据文件大小和分片大小计算分片列表
    pub fn new(
        task_id: Uuid,
        source_path: PathBuf,
        destination: String,
        total_size: u64,
        chunk_size: u64,
        sha256: Option<String>,
    ) -> Self {
        let total_chunks = if total_size == 0 {
            1
        } else {
            total_size.div_ceil(chunk_size) as u32
        };
        let now = now_secs();
        Self {
            task_id,
            source_path,
            destination,
            total_size,
            chunk_size,
            total_chunks,
            completed_chunks: Vec::new(),
            sha256,
            created_at: now,
            updated_at: now,
            metadata: HashMap::new(),
        }
    }

    /// 返回尚未完成的分片序号（按顺序）
    pub fn pending_chunks(&self) -> Vec<u32> {
        (0..self.total_chunks)
            .filter(|i| !self.completed_chunks.contains(i))
            .collect()
    }

    /// 标记分片完成
    pub fn mark_chunk_done(&mut self, index: u32) {
        if !self.completed_chunks.contains(&index) {
            self.completed_chunks.push(index);
            self.completed_chunks.sort_unstable();
            self.updated_at = now_secs();
        }
    }

    /// 是否全部完成
    pub fn is_complete(&self) -> bool {
        self.completed_chunks.len() == self.total_chunks as usize
    }

    /// 已传输字节数（估算，基于已完成分片）
    pub fn bytes_transferred(&self) -> u64 {
        self.completed_chunks
            .iter()
            .map(|&i| self.chunk_size_of(i))
            .sum()
    }

    /// 计算指定分片的实际大小
    pub fn chunk_size_of(&self, index: u32) -> u64 {
        let last = self.total_chunks.saturating_sub(1);
        if index == last && !self.total_size.is_multiple_of(self.chunk_size) {
            self.total_size % self.chunk_size
        } else {
            self.chunk_size
        }
    }

    /// 计算指定分片的文件偏移
    pub fn chunk_offset(&self, index: u32) -> u64 {
        index as u64 * self.chunk_size
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ──────────────────────────── Resume storage trait ─────────────────────────────

/// Async storage abstraction for [`ResumeState`].
///
/// Concrete impls live in `aerosync-infra` (today: file-backed JSON
/// at `{base_dir}/.aerosync/{task_id}.json`; future: Redis for
/// cluster mode). Consumers (`TransferEngine`, the resume CLI
/// command) take `Arc<dyn ResumeStorage>` so the IO mechanism is
/// swappable per-deployment.
///
/// ## Implementor contract
///
/// - All methods MUST be **idempotent**. `save()` overwrites; `delete()`
///   on a missing task returns `Ok(())`; `load()` on a missing task
///   returns `Ok(None)`. The engine relies on this to recover from
///   crashes mid-transfer.
/// - `save()` SHOULD be **crash-safe**: write to `{path}.tmp`, fsync,
///   then `rename()`. The legacy `JsonResumeStore::save()` did NOT
///   do this — Phase 2.2 fixes it as part of the migration.
/// - `list_pending()` MAY be O(n) over disk entries; the engine only
///   calls it on startup and via the `aerosync resume list` CLI
///   command, never on the hot path.
/// - All methods MUST be cancel-safe (i.e. dropping the future before
///   completion MUST NOT leave the backing store in a partially
///   updated state). The atomic-write fix above guarantees this for
///   `save()`; `load()` / `list_pending()` are read-only so trivially
///   safe.
///
/// ## Why async?
///
/// Per refactor plan §4 D4, the trait is async (rather than sync
/// with a `spawn_blocking` adapter) because the most likely impl is
/// `tokio::fs`, which already lives in async land. A sync trait
/// would force every caller to wrap calls in `spawn_blocking`,
/// hurting performance on the hot resume path (every chunk save
/// goes through here).
#[async_trait::async_trait]
pub trait ResumeStorage: Send + Sync + 'static {
    /// Persist the given state, overwriting any existing entry for
    /// the same `state.task_id`.
    async fn save(&self, state: &ResumeState) -> Result<()>;

    /// Load the state for `task_id`, returning `None` when absent.
    async fn load(&self, task_id: Uuid) -> Result<Option<ResumeState>>;

    /// Remove the state for `task_id`. Idempotent: returns `Ok(())`
    /// when the entry does not exist.
    async fn delete(&self, task_id: Uuid) -> Result<()>;

    /// List all incomplete (`!state.is_complete()`) entries, sorted
    /// by `created_at` ascending. Used by startup recovery and the
    /// `aerosync resume list` CLI subcommand.
    async fn list_pending(&self) -> Result<Vec<ResumeState>>;

    /// Find an incomplete entry whose `source_path` and
    /// `destination` match the given pair — used by automatic
    /// resume to detect that a previous attempt for the same
    /// (file, target) tuple is still recoverable.
    async fn find_by_file(
        &self,
        source_path: &Path,
        destination: &str,
    ) -> Result<Option<ResumeState>>;
}

// ──────────────────────────── History value objects ───────────────────────────
//
// Source: lifted verbatim from `src/core/history.rs` (lines 28-253,
// pre-v0.3.0). Method bodies, derives, doc strings, and field
// visibilities are unchanged. The original module's leading rustdoc
// header now lives on `aerosync-infra::history` (Phase 2.3) where
// the JSONL persistence impl will move.

/// String label of a receipt's terminal-or-pending state, persisted
/// alongside the [`HistoryEntry`].
///
/// Matches the canonical wire spelling of the seven generic states
/// in `aerosync::core::receipt::State` but flattens the terminal
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
    /// History-record UUID. Distinct from `receipt_id`; one history
    /// record corresponds to exactly one transferred file but may or
    /// may not have been issued a [`crate::receipt::Receipt`]. New
    /// records get a fresh `Uuid::new_v4()` from the
    /// [`HistoryEntry::success`] / [`HistoryEntry::failure`] ctors.
    pub id: Uuid,
    /// File-system basename of the transferred file (e.g.
    /// `"report.pdf"`). For directory transfers, each contained file
    /// gets its own [`HistoryEntry`] keyed by the file's basename.
    pub filename: String,
    /// 接收方保存路径（可选，发送方为 None）
    pub saved_path: Option<PathBuf>,
    /// Total file size in bytes as observed at the start of the
    /// transfer. For zero-byte files this is `0` and the transfer is
    /// still recorded.
    pub size: u64,
    /// Hex-encoded SHA-256 of the file body (lowercase, no prefix).
    /// `None` when the transfer skipped hashing (e.g. directory
    /// metadata-only sync, or the transport disabled hashing).
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
            completed_at: now_secs(),
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
            completed_at: now_secs(),
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

// ──────────────────────────── History storage trait ───────────────────────────

/// Async storage abstraction for the transfer history log.
///
/// Concrete impls live in `aerosync-infra::history` (today: JSONL
/// append at `~/.config/aerosync/history.jsonl`; future: SQLite at
/// the ~10K-record horizon per RFC-003 §7). Consumers
/// (`TransferEngine`, the `aerosync history` CLI, the receipts-HTTP
/// recovery iterator) will take `Arc<dyn HistoryStorage>` so the
/// backend is swappable per-deployment.
///
/// ## Implementor contract
///
/// - **`append()`** MUST be **append-only crash-safe**: serialize
///   the entry to one JSON line, write+fsync atomically, never
///   truncate or rewrite earlier records. Concurrent appends from
///   multiple async tasks MUST serialize via the impl's internal
///   lock — the existing `JsonlHistoryStore` uses
///   `Arc<Mutex<tokio::fs::File>>` for this.
/// - **`query()` / `read_all()` / `recent()`** MAY perform a linear
///   scan (the legacy JSONL impl does); they MUST NOT mutate any
///   on-disk state.
/// - **`write_metadata()`** rewrites a single record in place by id.
///   Crash-safety is best-effort (the legacy JSONL impl reads the
///   whole file, mutates the matching record, writes to a tempfile,
///   then renames). Returns `Ok(true)` on hit, `Ok(false)` on miss.
/// - **`record_receipt_terminal()`** rewrites a single record in
///   place by receipt id, identical safety to `write_metadata`.
/// - **`iter_unfinished_receipts()`** returns every entry whose
///   `receipt_state` is non-terminal — used by startup recovery to
///   surface receipts that lost the wire mid-transfer.
/// - All methods MUST be cancel-safe.
///
/// ## Why the read methods are on the trait
///
/// `query` / `recent` / `query_by_receipt` are needed by the CLI
/// (`aerosync history`) and the MCP `list_history` tool. Putting
/// them on the trait lets a future SQLite impl push the `WHERE`
/// clause into SQL instead of streaming every record into memory —
/// the existing JSONL `query()` is `O(N)` but the trait surface
/// permits an `O(log N)` upgrade later without API churn.
#[async_trait::async_trait]
pub trait HistoryStorage: Send + Sync + 'static {
    /// Append a fully-formed history entry. The receipt-state /
    /// metadata fields on `entry` are persisted as-is.
    async fn append(&self, entry: HistoryEntry) -> Result<()>;

    /// Run a filter query, returning matching entries newest-first
    /// up to `q.limit` (0 = no cap).
    async fn query(&self, q: &HistoryQuery) -> Result<Vec<HistoryEntry>>;

    /// Convenience: return the most recent `limit` entries
    /// regardless of filter.
    async fn recent(&self, limit: usize) -> Result<Vec<HistoryEntry>>;

    /// Mutate the metadata field of the entry whose `id == record_id`.
    /// Returns `Ok(true)` on hit, `Ok(false)` if no such entry
    /// exists. Used by the metadata write-back path so the on-disk
    /// JSONL record reflects the final sealed envelope.
    async fn write_metadata(&self, record_id: Uuid, metadata: &Metadata) -> Result<bool>;

    /// Mutate the receipt-state fields of the entry matched by
    /// `record_id` — that argument is matched against either the
    /// entry's own `id` OR its `receipt_id`, so the watch-bridge can
    /// pass the receipt UUID directly without having to first look up
    /// the history record id.
    ///
    /// `acked_at` is auto-stamped to "now" (Unix seconds) when
    /// `state == Acked`. `reason` is routed to `nack_reason` when
    /// `state == Nacked`, `cancel_reason` when `state == Cancelled`,
    /// and ignored otherwise. Returns `Ok(true)` on hit, `Ok(false)`
    /// on miss.
    ///
    /// The signature matches the inherent
    /// `HistoryStore::record_receipt_terminal` shape that has shipped
    /// since v0.2 — a richer split into separate `acked_at` /
    /// `nack_reason` / `cancel_reason` arguments was considered for
    /// the trait but rejected: the routing logic is purely a
    /// projection from `(state, reason)` and lives more naturally on
    /// the impl side.
    async fn record_receipt_terminal(
        &self,
        record_id: Uuid,
        state: ReceiptStateLabel,
        reason: Option<String>,
    ) -> Result<bool>;

    /// Return every entry whose `receipt_state` is non-terminal —
    /// startup recovery uses this to detect receipts that were lost
    /// mid-transfer (process crash, network drop, etc.).
    async fn iter_unfinished_receipts(&self) -> Result<Vec<HistoryEntry>>;

    /// Look up a single entry by receipt id. `None` when no entry
    /// has the matching `receipt_id`.
    async fn query_by_receipt(&self, receipt_id: Uuid) -> Result<Option<HistoryEntry>>;

    /// Fire-and-forget [`Self::append`]: write `entry` and silently
    /// drop any error. Used on the `TransferEngine::send` hot path
    /// where a missed history record must not surface as a transfer
    /// failure (the in-memory receipt is the source of truth — the
    /// JSONL log is an audit aid). Default impl just calls
    /// [`Self::append`] and discards the [`Result`]; concrete impls
    /// may override to skip the serialize cost when persistence is
    /// disabled (e.g. an in-memory mock).
    async fn append_silent(&self, entry: HistoryEntry) {
        let _ = self.append(entry).await;
    }
}

// ──────────────────────────── Receipt journal types ───────────────────────────
//
// RFC-002 §8 (Persistence) — durable, append-only log of every receipt
// state transition. The JSONL `HistoryStore` only persists *terminal*
// transitions (and only when a `HistoryEntry` for the transfer exists),
// so a crash between `Initiated` and a terminal state would lose every
// observation about an in-flight receipt. The journal closes that gap:
// every event flows through an append-only SQLite log keyed by
// `receipt_id`, enabling crash-recovery enumeration of in-flight
// receipts and re-emission of the latest terminal state for receipts
// the application missed because the process was down.
//
// The implementation lives in `aerosync_infra::receipt_journal`; this
// crate only owns the trait shape and the value objects so the engine,
// CLI, MCP, and Python bindings can reference them without depending
// on any specific persistence backend.

/// Which side of the wire owns a receipt event — used by
/// [`ReceiptJournalStorage`] to disambiguate sender-side vs.
/// receiver-side receipts that happen to share an id (e.g. a loopback
/// integration test) and to drive the recovery iterator's filter.
///
/// The string spellings (`"send"` / `"receive"`) match the existing
/// [`HistoryEntry::direction`] vocabulary so a downstream UI can
/// render either source without translating.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReceiptSide {
    /// Sender side (originator of the transfer).
    Send,
    /// Receiver side (consumer of the transfer).
    Receive,
}

impl ReceiptSide {
    /// Stable wire string. Matches [`HistoryEntry::direction`].
    pub fn as_str(self) -> &'static str {
        match self {
            ReceiptSide::Send => "send",
            ReceiptSide::Receive => "receive",
        }
    }

    /// Parse a side string. Accepts `"send"` and `"receive"` only.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "send" => Some(ReceiptSide::Send),
            "receive" => Some(ReceiptSide::Receive),
            _ => None,
        }
    }
}

/// One row of the receipt journal — a single observed state transition.
///
/// The row is built by the watch-bridge in
/// `aerosync_infra::receipt_journal::spawn_journal_bridge`; the
/// [`ReceiptJournalStorage`] impl persists it as-is. `recorded_at` is
/// set by the bridge (Unix seconds) so the persistence layer stays
/// time-source-agnostic.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReceiptJournalRecord {
    /// The receipt being observed.
    pub receipt_id: Uuid,
    /// Which side of the wire owns this receipt.
    pub side: ReceiptSide,
    /// The state observed by this event. Spelled with the wire-stable
    /// strings used by the recovery API — the seven generic states
    /// (`initiated` / `stream-opened` / `data-transferred` /
    /// `stream-closed` / `processing`) plus the terminal labels
    /// shared with [`ReceiptStateLabel`] (`acked` / `nacked` /
    /// `cancelled` / `errored` / `stream-lost`).
    pub state: String,
    /// Whether this state is terminal (no further transitions
    /// expected). Pre-computed so the recovery iterator does not have
    /// to re-derive it per row.
    pub is_terminal: bool,
    /// Free-form reason carried by `Nacked` / `Cancelled` / `Errored`
    /// / `StreamLost`; absent for non-terminal transitions and for
    /// `Acked`.
    pub reason: Option<String>,
    /// Numeric error code carried by `Errored`; absent otherwise.
    pub code: Option<u32>,
    /// History-record id this receipt was attached to, when known —
    /// gives the recovery iterator a path back to the durable
    /// [`HistoryEntry`] for filename / size / metadata context.
    pub history_id: Option<Uuid>,
    /// Filename context (snapshot of [`HistoryEntry::filename`] at
    /// journal time); allows the recovery iterator to render a useful
    /// summary even when the history record has been pruned.
    pub filename: Option<String>,
    /// Peer context (snapshot of [`HistoryEntry::remote_ip`] at
    /// journal time).
    pub peer: Option<String>,
    /// Wall-clock Unix seconds when the bridge recorded the event.
    pub recorded_at: u64,
}

/// View of the most recent state observed for a single receipt —
/// returned by [`ReceiptJournalStorage::list_recoverable`] and
/// [`ReceiptJournalStorage::list_recent_terminal`]. One row per
/// `receipt_id`, projecting the latest [`ReceiptJournalRecord`].
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecoverableReceipt {
    /// Stable receipt id.
    pub receipt_id: Uuid,
    /// Which side of the wire owns this receipt.
    pub side: ReceiptSide,
    /// Latest observed state spelling. Same vocabulary as
    /// [`ReceiptJournalRecord::state`].
    pub state: String,
    /// `true` when [`Self::state`] is terminal.
    pub is_terminal: bool,
    /// Reason / code carried by the latest terminal event, when
    /// applicable.
    pub reason: Option<String>,
    /// Numeric error code carried by the latest `Errored` event.
    pub code: Option<u32>,
    /// History record this receipt was attached to.
    pub history_id: Option<Uuid>,
    /// Filename context for diagnostic display.
    pub filename: Option<String>,
    /// Peer context for diagnostic display.
    pub peer: Option<String>,
    /// Wall-clock Unix seconds of the latest observation.
    pub last_event_at: u64,
}

/// Async storage abstraction for the receipt journal (RFC-002 §8).
///
/// The journal is an **append-only** log of every observed receipt
/// state transition. It complements [`HistoryStorage`], which only
/// records terminal states alongside file-transfer rows: the journal
/// is the durable source of truth for "which receipts are still
/// in-flight after a crash" and "what was the last terminal state
/// the application observed before the process restarted".
///
/// ## Implementor contract
///
/// - **`record_event`** MUST be append-only: an event is never
///   rewritten or deleted (terminal transitions still append a new
///   row, they do not patch the previous one). Concurrent calls from
///   multiple async tasks MUST serialize internally.
/// - **`record_event`** SHOULD be **crash-safe** (the canonical impl
///   wraps a `rusqlite::Connection` in an async mutex with WAL
///   journaling so a torn write either lands fully or not at all).
/// - **`list_recoverable`** returns one row per receipt whose latest
///   journal row is **non-terminal**, sorted by `last_event_at`
///   descending. Used by startup recovery and by the `aerosync
///   receipt recover` CLI / Python `aerosync.recover()` surface.
/// - **`list_recent_terminal`** returns one row per receipt whose
///   latest row is terminal, sorted by `last_event_at` descending.
///   Used to re-emit the terminal verdict to the application after a
///   restart.
/// - **`latest_state`** returns the most recent journal row for one
///   receipt, or `Ok(None)` when no row exists.
/// - All methods MUST be cancel-safe.
#[async_trait::async_trait]
pub trait ReceiptJournalStorage: Send + Sync + 'static {
    /// Append `record` to the journal. Errors propagate to the caller
    /// so the watch-bridge can downgrade them to a `tracing::warn!`
    /// rather than panicking the spawning runtime.
    async fn record_event(&self, record: ReceiptJournalRecord) -> Result<()>;

    /// Snapshot every receipt whose latest observation is
    /// **non-terminal**. `limit == 0` means "no cap"; non-zero values
    /// truncate to the most recent N rows.
    async fn list_recoverable(&self, limit: usize) -> Result<Vec<RecoverableReceipt>>;

    /// Snapshot every receipt whose latest observation is **terminal**
    /// and was recorded within the last `since_secs` seconds. Used by
    /// the recovery surface to re-emit terminal verdicts the
    /// application missed because the process was down. `limit == 0`
    /// means "no cap".
    async fn list_recent_terminal(
        &self,
        since_secs: u64,
        limit: usize,
    ) -> Result<Vec<RecoverableReceipt>>;

    /// Return the most-recent journal projection for `receipt_id`, or
    /// `Ok(None)` when the receipt has never been recorded.
    async fn latest_state(&self, receipt_id: Uuid) -> Result<Option<RecoverableReceipt>>;
}

// ──────────────────────────── Tests ────────────────────────────────────────────
//
// Pure-data tests for ResumeState (chunk math, mark_chunk_done
// idempotence, etc.) lifted verbatim from `src/core/resume.rs`. The
// store-side tests (`test_save_and_load`, `test_list_pending_*`)
// stay in `src/core/resume.rs` for now — they will move to
// `aerosync-infra/src/resume.rs` alongside the impl in Phase 2.2.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_total_chunks_calculated_correctly() {
        let s = ResumeState::new(
            Uuid::new_v4(),
            PathBuf::from("/f"),
            "h".to_string(),
            100 * 1024 * 1024,
            DEFAULT_CHUNK_SIZE,
            None,
        );
        assert_eq!(s.total_chunks, 4);
    }

    #[test]
    fn test_total_chunks_exact_multiple() {
        let s = ResumeState::new(
            Uuid::new_v4(),
            PathBuf::from("/f"),
            "h".to_string(),
            64 * 1024 * 1024,
            DEFAULT_CHUNK_SIZE,
            None,
        );
        assert_eq!(s.total_chunks, 2);
    }

    #[test]
    fn test_empty_file_has_one_chunk() {
        let s = ResumeState::new(
            Uuid::new_v4(),
            PathBuf::from("/f"),
            "h".to_string(),
            0,
            DEFAULT_CHUNK_SIZE,
            None,
        );
        assert_eq!(s.total_chunks, 1);
    }

    #[test]
    fn test_chunk_offset() {
        let s = ResumeState::new(
            Uuid::new_v4(),
            PathBuf::from("/f"),
            "h".to_string(),
            100 * 1024 * 1024,
            DEFAULT_CHUNK_SIZE,
            None,
        );
        assert_eq!(s.chunk_offset(0), 0);
        assert_eq!(s.chunk_offset(1), DEFAULT_CHUNK_SIZE);
        assert_eq!(s.chunk_offset(2), 2 * DEFAULT_CHUNK_SIZE);
    }

    #[test]
    fn test_last_chunk_size_is_remainder() {
        let s = ResumeState::new(
            Uuid::new_v4(),
            PathBuf::from("/f"),
            "h".to_string(),
            100 * 1024 * 1024,
            DEFAULT_CHUNK_SIZE,
            None,
        );
        assert_eq!(s.total_chunks, 4);
        assert_eq!(s.chunk_size_of(3), 4 * 1024 * 1024);
        assert_eq!(s.chunk_size_of(0), DEFAULT_CHUNK_SIZE);
    }

    #[test]
    fn test_mark_chunk_done() {
        let mut s = ResumeState::new(
            Uuid::new_v4(),
            PathBuf::from("/f"),
            "h".to_string(),
            64 * 1024 * 1024,
            DEFAULT_CHUNK_SIZE,
            None,
        );
        assert_eq!(s.pending_chunks(), vec![0, 1]);
        s.mark_chunk_done(0);
        assert_eq!(s.pending_chunks(), vec![1]);
        s.mark_chunk_done(1);
        assert!(s.is_complete());
    }

    #[test]
    fn test_mark_chunk_done_idempotent() {
        let mut s = ResumeState::new(
            Uuid::new_v4(),
            PathBuf::from("/f"),
            "h".to_string(),
            DEFAULT_CHUNK_SIZE,
            DEFAULT_CHUNK_SIZE,
            None,
        );
        s.mark_chunk_done(0);
        s.mark_chunk_done(0);
        assert_eq!(s.completed_chunks.len(), 1);
    }

    #[test]
    fn receipt_side_round_trips_through_str() {
        for side in [ReceiptSide::Send, ReceiptSide::Receive] {
            let s = side.as_str();
            assert_eq!(ReceiptSide::parse(s), Some(side));
        }
        assert!(ReceiptSide::parse("nope").is_none());
        assert!(ReceiptSide::parse("").is_none());
    }

    #[test]
    fn receipt_side_serde_uses_lowercase_send_receive() {
        let s = serde_json::to_string(&ReceiptSide::Send).unwrap();
        assert_eq!(s, "\"send\"");
        let r = serde_json::to_string(&ReceiptSide::Receive).unwrap();
        assert_eq!(r, "\"receive\"");
        let back: ReceiptSide = serde_json::from_str("\"send\"").unwrap();
        assert_eq!(back, ReceiptSide::Send);
    }

    #[test]
    fn receipt_journal_record_round_trips_through_serde() {
        let rec = ReceiptJournalRecord {
            receipt_id: Uuid::nil(),
            side: ReceiptSide::Receive,
            state: "processing".into(),
            is_terminal: false,
            reason: None,
            code: None,
            history_id: Some(Uuid::nil()),
            filename: Some("a.bin".into()),
            peer: Some("10.0.0.5".into()),
            recorded_at: 1_700_000_000,
        };
        let s = serde_json::to_string(&rec).unwrap();
        let back: ReceiptJournalRecord = serde_json::from_str(&s).unwrap();
        assert_eq!(back, rec);
    }

    #[test]
    fn test_bytes_transferred() {
        let mut s = ResumeState::new(
            Uuid::new_v4(),
            PathBuf::from("/f"),
            "h".to_string(),
            100 * 1024 * 1024,
            DEFAULT_CHUNK_SIZE,
            None,
        );
        assert_eq!(s.bytes_transferred(), 0);
        s.mark_chunk_done(0);
        assert_eq!(s.bytes_transferred(), DEFAULT_CHUNK_SIZE);
        s.mark_chunk_done(1);
        assert_eq!(s.bytes_transferred(), 2 * DEFAULT_CHUNK_SIZE);
    }
}

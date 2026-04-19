//! Storage abstractions and pure-data types for the resume + history
//! persistence layers.
//!
//! Per `docs/v0.3.0-refactor-plan.md` §3 Phase 2, this module owns:
//!
//! 1. The **value objects** ([`ChunkState`], [`ResumeState`], plus
//!    history-side types added in a follow-up sub-commit) that
//!    transit between application code and any persistent backend.
//!    They live in `aerosync-domain` so they have zero IO/networking
//!    deps and can be referenced by the trait signatures below.
//! 2. The **storage traits** ([`ResumeStorage`], plus
//!    [`HistoryStorage`] in a follow-up sub-commit) that
//!    `aerosync::core::transfer::TransferEngine` and friends consume
//!    via `Arc<dyn …>`. Concrete implementations live in
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

use serde::{Deserialize, Serialize};
use uuid::Uuid;

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

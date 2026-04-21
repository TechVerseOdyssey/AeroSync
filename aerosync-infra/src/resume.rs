//! 断点续传状态管理（file-backed JSON impl of [`ResumeStorage`]）。
//!
//! 状态文件存储路径：`{state_dir}/.aerosync/{task_id}.json`
//! 每个传输任务对应一个 JSON 文件，记录已完成的分片列表。
//! 完成后删除文件；重启后自动检测并恢复。
//!
//! ## v0.3.0 Phase 2.2 migration
//!
//! Source moved verbatim from `src/core/resume.rs` to
//! `aerosync-infra/src/resume.rs`. Two semantic changes:
//!
//! 1. `ResumeStore::save` is now **crash-safe** via tmp+rename
//!    (write to `{path}.tmp`, fsync, rename). The pre-v0.3.0
//!    direct `tokio::fs::write` was vulnerable to torn writes if
//!    the process crashed mid-fsync — partially-written JSON would
//!    fail to deserialize on next startup, causing silent loss of
//!    resume state. The `pub async fn save` signature is unchanged.
//! 2. `ResumeStore` now `impl aerosync_domain::storage::ResumeStorage`
//!    so consumers can hold it as `Arc<dyn ResumeStorage>` for
//!    swappable backends (Phase 2.4 wires the engine through the
//!    trait). The trait impl just delegates to the inherent methods.
//!
//! Re-exports below preserve the legacy import path
//! `aerosync::core::resume::{ResumeState, ChunkState,
//! DEFAULT_CHUNK_SIZE}` (the root crate forwards to this module via
//! `pub use aerosync_infra::resume` in `src/core/mod.rs`).

/// Re-exports of the [`aerosync_domain::storage`] value objects and
/// the trait this module's [`ResumeStore`] implements. Hoisted to the
/// [`aerosync_infra::resume`] module-root so the legacy
/// `aerosync::core::resume::*` import path keeps resolving — the root
/// crate forwards via `pub use aerosync_infra::resume;` in
/// `src/core/mod.rs`. Field-level docs for `ChunkState` / `ResumeState`
/// live with their canonical definition in `aerosync_domain::storage`.
pub use aerosync_domain::storage::{ChunkState, ResumeState, ResumeStorage, DEFAULT_CHUNK_SIZE};

use aerosync_domain::{AeroSyncError, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// 状态文件存放子目录名
const STATE_SUBDIR: &str = ".aerosync";

// ──────────────────────────── ResumeStore ─────────────────────────────────────

/// 负责持久化 ResumeState 到本地 JSON 文件的存储层
///
/// Implements [`aerosync_domain::storage::ResumeStorage`]. The trait
/// methods just delegate to the inherent methods of the same name —
/// keeping inherent methods means existing callers that hold a
/// concrete `ResumeStore` (rather than `Arc<dyn ResumeStorage>`)
/// don't break during the Phase 2.4 trait-object migration.
pub struct ResumeStore {
    /// 状态文件存放目录（= `base_dir/.aerosync/`）
    state_dir: PathBuf,
}

impl ResumeStore {
    /// 创建 ResumeStore，`base_dir` 通常为发送目录或当前工作目录
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        Self {
            state_dir: base_dir.as_ref().join(STATE_SUBDIR),
        }
    }

    /// 确保状态目录存在
    async fn ensure_dir(&self) -> Result<()> {
        tokio::fs::create_dir_all(&self.state_dir).await?;
        Ok(())
    }

    fn state_path(&self, task_id: Uuid) -> PathBuf {
        self.state_dir.join(format!("{}.json", task_id))
    }

    /// 保存（新建或更新）状态文件 —— 通过 tmp+rename 保证 crash-safe。
    ///
    /// ## v0.3.0 atomic-write upgrade
    ///
    /// Pre-v0.3.0 this method called `tokio::fs::write` directly, which
    /// is `truncate + write + close`. A crash between the truncate and
    /// the final write would leave a zero-length or partially-written
    /// JSON file — `serde_json::from_str` would then fail on next
    /// startup, silently dropping resume state for that task.
    ///
    /// The new path:
    ///   1. Serialize `state` to bytes.
    ///   2. Write the bytes to `{path}.tmp` (a sibling of the final
    ///      path so `rename` stays on the same filesystem).
    ///   3. `fsync` the tmp file's data + metadata so the kernel
    ///      page cache hits disk before we expose it.
    ///   4. `rename(tmp, path)` — POSIX rename within a single
    ///      directory is atomic, so observers see either the old
    ///      content or the new content, never a torn middle.
    ///
    /// Failure of step 4 leaves the previous content intact and
    /// the tmp file behind; next save attempt overwrites the same
    /// tmp path. We tolerate that leak rather than racing a cleanup.
    pub async fn save(&self, state: &ResumeState) -> Result<()> {
        self.ensure_dir().await?;
        let final_path = self.state_path(state.task_id);
        let tmp_path = {
            let mut p = final_path.clone();
            let mut name = p.file_name().map(|n| n.to_os_string()).unwrap_or_default();
            name.push(".tmp");
            p.set_file_name(name);
            p
        };

        let json = serde_json::to_string_pretty(state).map_err(|e| {
            AeroSyncError::Protocol(format!("Failed to serialize resume state: {}", e))
        })?;

        // Step 2: write bytes to tmp.
        let mut file = tokio::fs::File::create(&tmp_path).await?;
        use tokio::io::AsyncWriteExt;
        file.write_all(json.as_bytes()).await?;
        // Step 3: fsync data+metadata so the kernel page cache reaches
        // disk before we expose the new file under its final name.
        file.sync_all().await?;
        drop(file);

        // Step 4: atomic rename. POSIX guarantees the rename is atomic
        // within a single filesystem; observers see either the old or
        // the new content but never a torn middle.
        tokio::fs::rename(&tmp_path, &final_path).await?;
        Ok(())
    }

    /// 读取指定 task 的状态，不存在返回 None
    pub async fn load(&self, task_id: Uuid) -> Result<Option<ResumeState>> {
        let path = self.state_path(task_id);
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                let state: ResumeState = serde_json::from_str(&content).map_err(|e| {
                    AeroSyncError::Protocol(format!("Failed to parse resume state: {}", e))
                })?;
                Ok(Some(state))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// 删除指定 task 的状态文件（传输完成后调用）
    pub async fn delete(&self, task_id: Uuid) -> Result<()> {
        let path = self.state_path(task_id);
        match tokio::fs::remove_file(&path).await {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// 列出所有未完成的续传状态
    pub async fn list_pending(&self) -> Result<Vec<ResumeState>> {
        let mut result = Vec::new();

        let mut entries = match tokio::fs::read_dir(&self.state_dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(result),
            Err(e) => return Err(e.into()),
        };

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match tokio::fs::read_to_string(&path).await {
                Ok(content) => {
                    if let Ok(state) = serde_json::from_str::<ResumeState>(&content) {
                        if !state.is_complete() {
                            result.push(state);
                        }
                    }
                }
                Err(_) => continue,
            }
        }

        result.sort_by_key(|s| s.created_at);
        Ok(result)
    }

    /// 查找与给定文件路径和目标地址匹配的未完成续传（用于自动恢复）
    pub async fn find_by_file(
        &self,
        source_path: &Path,
        destination: &str,
    ) -> Result<Option<ResumeState>> {
        let pending = self.list_pending().await?;
        Ok(pending
            .into_iter()
            .find(|s| s.source_path == source_path && s.destination == destination))
    }
}

// ──────────────────────────── ResumeStorage impl ──────────────────────────────
//
// All trait methods just delegate to the inherent methods. We keep the
// inherent methods (rather than only the trait methods) because some
// existing callers in the workspace hold a concrete `ResumeStore`
// rather than `Arc<dyn ResumeStorage>`. Phase 2.4 will migrate them
// to the trait-object form, at which point the inherent methods can
// be retired in v0.4.

#[async_trait]
impl ResumeStorage for ResumeStore {
    async fn save(&self, state: &ResumeState) -> Result<()> {
        ResumeStore::save(self, state).await
    }

    async fn load(&self, task_id: Uuid) -> Result<Option<ResumeState>> {
        ResumeStore::load(self, task_id).await
    }

    async fn delete(&self, task_id: Uuid) -> Result<()> {
        ResumeStore::delete(self, task_id).await
    }

    async fn list_pending(&self) -> Result<Vec<ResumeState>> {
        ResumeStore::list_pending(self).await
    }

    async fn find_by_file(
        &self,
        source_path: &Path,
        destination: &str,
    ) -> Result<Option<ResumeState>> {
        ResumeStore::find_by_file(self, source_path, destination).await
    }
}

// ──────────────────────────── 测试 ────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_state(dir: &Path) -> ResumeState {
        ResumeState::new(
            Uuid::new_v4(),
            dir.join("file.bin"),
            "http://host/upload".to_string(),
            100 * 1024 * 1024, // 100 MB
            DEFAULT_CHUNK_SIZE,
            Some("abc123".to_string()),
        )
    }

    // ── 1. ResumeState 分片计算 ───────────────────────────────────────────────
    #[test]
    fn test_total_chunks_calculated_correctly() {
        let s = ResumeState::new(
            Uuid::new_v4(),
            PathBuf::from("/f"),
            "h".to_string(),
            100 * 1024 * 1024,
            DEFAULT_CHUNK_SIZE, // 32MB → ceil(100/32) = 4
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
            DEFAULT_CHUNK_SIZE, // 64/32 = 2
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

    // ── 2. chunk_offset / chunk_size_of ──────────────────────────────────────
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
        // 100MB 文件，32MB 分片 → 最后一片 = 100 - 3*32 = 4 MB
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

    // ── 3. mark_chunk_done / pending_chunks / is_complete ────────────────────
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
        s.mark_chunk_done(0); // 重复标记不应产生重复
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

    // ── 4. ResumeStore save / load / delete ───────────────────────────────────
    #[tokio::test]
    async fn test_save_and_load() {
        let dir = tempdir().unwrap();
        let store = ResumeStore::new(dir.path());
        let state = make_state(dir.path());
        let id = state.task_id;

        store.save(&state).await.unwrap();
        let loaded = store.load(id).await.unwrap();
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.task_id, id);
        assert_eq!(loaded.total_size, state.total_size);
        assert_eq!(loaded.sha256, state.sha256);
    }

    #[tokio::test]
    async fn test_load_nonexistent_returns_none() {
        let dir = tempdir().unwrap();
        let store = ResumeStore::new(dir.path());
        let result = store.load(Uuid::new_v4()).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_delete_removes_file() {
        let dir = tempdir().unwrap();
        let store = ResumeStore::new(dir.path());
        let state = make_state(dir.path());
        let id = state.task_id;

        store.save(&state).await.unwrap();
        assert!(store.load(id).await.unwrap().is_some());

        store.delete(id).await.unwrap();
        assert!(store.load(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_delete_nonexistent_is_ok() {
        let dir = tempdir().unwrap();
        let store = ResumeStore::new(dir.path());
        let result = store.delete(Uuid::new_v4()).await;
        assert!(result.is_ok());
    }

    // ── 5. list_pending ───────────────────────────────────────────────────────
    #[tokio::test]
    async fn test_list_pending_empty_dir() {
        let dir = tempdir().unwrap();
        let store = ResumeStore::new(dir.path());
        let pending = store.list_pending().await.unwrap();
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn test_list_pending_returns_incomplete_tasks() {
        let dir = tempdir().unwrap();
        let store = ResumeStore::new(dir.path());

        // 保存 3 个任务
        for _ in 0..3 {
            let s = make_state(dir.path());
            store.save(&s).await.unwrap();
        }

        let pending = store.list_pending().await.unwrap();
        assert_eq!(pending.len(), 3);
    }

    #[tokio::test]
    async fn test_list_pending_excludes_completed() {
        let dir = tempdir().unwrap();
        let store = ResumeStore::new(dir.path());

        // 1 个已完成（单分片文件）
        let mut done = ResumeState::new(
            Uuid::new_v4(),
            dir.path().join("done.bin"),
            "h".to_string(),
            DEFAULT_CHUNK_SIZE,
            DEFAULT_CHUNK_SIZE,
            None,
        );
        done.mark_chunk_done(0);
        assert!(done.is_complete());
        store.save(&done).await.unwrap();

        // 1 个未完成
        let pending_state = make_state(dir.path());
        store.save(&pending_state).await.unwrap();

        let pending = store.list_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].task_id, pending_state.task_id);
    }

    // ── 6. find_by_file ───────────────────────────────────────────────────────
    #[tokio::test]
    async fn test_find_by_file_matches_path_and_destination() {
        let dir = tempdir().unwrap();
        let store = ResumeStore::new(dir.path());

        let src = dir.path().join("bigfile.bin");
        let dst = "http://remote/upload";
        let state = ResumeState::new(
            Uuid::new_v4(),
            src.clone(),
            dst.to_string(),
            DEFAULT_CHUNK_SIZE * 3,
            DEFAULT_CHUNK_SIZE,
            None,
        );
        store.save(&state).await.unwrap();

        let found = store.find_by_file(&src, dst).await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().task_id, state.task_id);
    }

    #[tokio::test]
    async fn test_find_by_file_returns_none_when_no_match() {
        let dir = tempdir().unwrap();
        let store = ResumeStore::new(dir.path());
        let found = store
            .find_by_file(Path::new("/nonexistent.bin"), "http://host/upload")
            .await
            .unwrap();
        assert!(found.is_none());
    }

    // ── 7. 更新已有状态文件 ───────────────────────────────────────────────────
    #[tokio::test]
    async fn test_save_overwrites_existing_state() {
        let dir = tempdir().unwrap();
        let store = ResumeStore::new(dir.path());
        let mut state = make_state(dir.path());
        let id = state.task_id;

        store.save(&state).await.unwrap();
        state.mark_chunk_done(0);
        store.save(&state).await.unwrap();

        let loaded = store.load(id).await.unwrap().unwrap();
        assert_eq!(loaded.completed_chunks, vec![0]);
    }
}

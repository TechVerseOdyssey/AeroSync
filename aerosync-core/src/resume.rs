/// 断点续传状态管理
///
/// 状态文件存储路径：`{state_dir}/.aerosync/{task_id}.json`
/// 每个传输任务对应一个 JSON 文件，记录已完成的分片列表。
/// 完成后删除文件；重启后自动检测并恢复。

use crate::{AeroSyncError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

// ──────────────────────────── 常量 ────────────────────────────────────────────

/// 默认分片大小：32 MB
pub const DEFAULT_CHUNK_SIZE: u64 = 32 * 1024 * 1024;

/// 状态文件存放子目录名
const STATE_SUBDIR: &str = ".aerosync";

// ──────────────────────────── 数据结构 ────────────────────────────────────────

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
            ((total_size + chunk_size - 1) / chunk_size) as u32
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
        self.completed_chunks.iter().map(|&i| self.chunk_size_of(i)).sum()
    }

    /// 计算指定分片的实际大小
    pub fn chunk_size_of(&self, index: u32) -> u64 {
        let last = self.total_chunks.saturating_sub(1);
        if index == last && self.total_size % self.chunk_size != 0 {
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

// ──────────────────────────── ResumeStore ─────────────────────────────────────

/// 负责持久化 ResumeState 到本地 JSON 文件的存储层
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

    /// 保存（新建或更新）状态文件
    pub async fn save(&self, state: &ResumeState) -> Result<()> {
        self.ensure_dir().await?;
        let path = self.state_path(state.task_id);
        let json = serde_json::to_string_pretty(state)
            .map_err(|e| AeroSyncError::Protocol(format!("Failed to serialize resume state: {}", e)))?;
        tokio::fs::write(&path, json).await?;
        Ok(())
    }

    /// 读取指定 task 的状态，不存在返回 None
    pub async fn load(&self, task_id: Uuid) -> Result<Option<ResumeState>> {
        let path = self.state_path(task_id);
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                let state: ResumeState = serde_json::from_str(&content)
                    .map_err(|e| AeroSyncError::Protocol(format!("Failed to parse resume state: {}", e)))?;
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
        Ok(pending.into_iter().find(|s| {
            s.source_path == source_path && s.destination == destination
        }))
    }
}

// ──────────────────────────── 辅助函数 ────────────────────────────────────────

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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
        let s = ResumeState::new(Uuid::new_v4(), PathBuf::from("/f"), "h".to_string(), 0, DEFAULT_CHUNK_SIZE, None);
        assert_eq!(s.total_chunks, 1);
    }

    // ── 2. chunk_offset / chunk_size_of ──────────────────────────────────────
    #[test]
    fn test_chunk_offset() {
        let s = ResumeState::new(Uuid::new_v4(), PathBuf::from("/f"), "h".to_string(), 100 * 1024 * 1024, DEFAULT_CHUNK_SIZE, None);
        assert_eq!(s.chunk_offset(0), 0);
        assert_eq!(s.chunk_offset(1), DEFAULT_CHUNK_SIZE);
        assert_eq!(s.chunk_offset(2), 2 * DEFAULT_CHUNK_SIZE);
    }

    #[test]
    fn test_last_chunk_size_is_remainder() {
        // 100MB 文件，32MB 分片 → 最后一片 = 100 - 3*32 = 4 MB
        let s = ResumeState::new(Uuid::new_v4(), PathBuf::from("/f"), "h".to_string(), 100 * 1024 * 1024, DEFAULT_CHUNK_SIZE, None);
        assert_eq!(s.total_chunks, 4);
        assert_eq!(s.chunk_size_of(3), 4 * 1024 * 1024);
        assert_eq!(s.chunk_size_of(0), DEFAULT_CHUNK_SIZE);
    }

    // ── 3. mark_chunk_done / pending_chunks / is_complete ────────────────────
    #[test]
    fn test_mark_chunk_done() {
        let mut s = ResumeState::new(Uuid::new_v4(), PathBuf::from("/f"), "h".to_string(), 64 * 1024 * 1024, DEFAULT_CHUNK_SIZE, None);
        assert_eq!(s.pending_chunks(), vec![0, 1]);
        s.mark_chunk_done(0);
        assert_eq!(s.pending_chunks(), vec![1]);
        s.mark_chunk_done(1);
        assert!(s.is_complete());
    }

    #[test]
    fn test_mark_chunk_done_idempotent() {
        let mut s = ResumeState::new(Uuid::new_v4(), PathBuf::from("/f"), "h".to_string(), DEFAULT_CHUNK_SIZE, DEFAULT_CHUNK_SIZE, None);
        s.mark_chunk_done(0);
        s.mark_chunk_done(0); // 重复标记不应产生重复
        assert_eq!(s.completed_chunks.len(), 1);
    }

    #[test]
    fn test_bytes_transferred() {
        let mut s = ResumeState::new(Uuid::new_v4(), PathBuf::from("/f"), "h".to_string(), 100 * 1024 * 1024, DEFAULT_CHUNK_SIZE, None);
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

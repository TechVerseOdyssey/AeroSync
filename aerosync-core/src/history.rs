/// 传输历史持久化模块
///
/// 每条记录追加到 `~/.config/aerosync/history.jsonl`（JSONL 格式），
/// 提供按方向、协议、时间范围过滤的查询接口。
use crate::{AeroSyncError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use uuid::Uuid;

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
        let avg_speed_bps = if duration_ms > 0 {
            size * 1000 / duration_ms
        } else {
            size
        };
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
        }
    }
}

/// 传输历史查询过滤器
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
}

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
        let content = tokio::fs::read_to_string(&self.path).await.unwrap_or_default();
        let entries = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        Ok(entries)
    }

    /// 按条件查询历史记录（结果按 completed_at 降序）
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

        // 按时间降序
        all.sort_by(|a, b| b.completed_at.cmp(&a.completed_at));

        if q.limit > 0 && all.len() > q.limit {
            all.truncate(q.limit);
        }
        Ok(all)
    }

    /// 最近 N 条记录
    pub async fn recent(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        self.query(&HistoryQuery {
            limit,
            ..Default::default()
        })
        .await
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
        store.append(HistoryEntry::success("a.bin", None, 1, None, None, "http", "send", 10)).await.unwrap();
        store.append(HistoryEntry::success("b.bin", None, 2, None, None, "http", "receive", 10)).await.unwrap();

        let q = HistoryQuery { direction: Some("send".into()), ..Default::default() };
        let result = store.query(&q).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].filename, "a.bin");
    }

    #[tokio::test]
    async fn test_query_protocol_filter() {
        let (store, _dir) = tmp_store().await;
        store.append(HistoryEntry::success("a.bin", None, 1, None, None, "http", "send", 10)).await.unwrap();
        store.append(HistoryEntry::success("b.bin", None, 2, None, None, "quic", "send", 10)).await.unwrap();

        let q = HistoryQuery { protocol: Some("quic".into()), ..Default::default() };
        let result = store.query(&q).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].filename, "b.bin");
    }

    #[tokio::test]
    async fn test_query_success_only() {
        let (store, _dir) = tmp_store().await;
        store.append(HistoryEntry::success("ok.bin", None, 1, None, None, "http", "send", 10)).await.unwrap();
        store.append(HistoryEntry::failure("fail.bin", 1, None, "http", "send", 10, "err")).await.unwrap();

        let q = HistoryQuery { success_only: true, ..Default::default() };
        let result = store.query(&q).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].filename, "ok.bin");
    }

    #[tokio::test]
    async fn test_query_limit() {
        let (store, _dir) = tmp_store().await;
        for i in 0..5 {
            store.append(HistoryEntry::success(
                format!("f{}.bin", i), None, i, None, None, "http", "send", 10
            )).await.unwrap();
        }
        let result = store.recent(3).await.unwrap();
        assert_eq!(result.len(), 3);
    }

    #[tokio::test]
    async fn test_avg_speed_bps() {
        let entry = HistoryEntry::success("x.bin", None, 1_000_000, None, None, "http", "send", 1000);
        assert_eq!(entry.avg_speed_bps, 1_000_000); // 1MB / 1s = 1MB/s
    }

    #[tokio::test]
    async fn test_jsonl_one_line_per_record() {
        let (store, _dir) = tmp_store().await;
        store.append(HistoryEntry::success("a.bin", None, 1, None, None, "http", "send", 1)).await.unwrap();
        store.append(HistoryEntry::success("b.bin", None, 2, None, None, "http", "send", 2)).await.unwrap();

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
}

//! 审计日志模块
//!
//! 将所有传输操作（发送/接收/失败/认证拒绝）以 JSONL 格式追加写入磁盘，
//! 进程重启后记录不丢失。
//!
//! # 日志格式
//!
//! 每行一条 JSON：
//! ```json
//! {"timestamp":1775903419,"event":"TransferCompleted","direction":"Receive","protocol":"http",
//!  "filename":"data.bin","size":157286400,"sha256":"204be31...","remote_ip":"192.168.1.5","result":"Ok"}
//! ```
//!
//! # 使用示例
//!
//! ```no_run
//! use aerosync_core::audit::{AuditLogger, AuditEntry, AuditEvent, Direction, AuditResult};
//! use std::path::Path;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let logger = AuditLogger::new(Path::new("/var/log/aerosync/audit.log")).await?;
//!
//! logger.log(AuditEntry {
//!     event: AuditEvent::TransferCompleted,
//!     direction: Direction::Receive,
//!     protocol: "http".to_string(),
//!     filename: "data.bin".to_string(),
//!     size: 1024,
//!     sha256: Some("abc123".to_string()),
//!     remote_ip: Some("192.168.1.5".to_string()),
//!     result: AuditResult::Ok,
//! }).await?;
//! # Ok(())
//! # }
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::error::Result;
use crate::AeroSyncError;

// ──────────────────────────────── types ─────────────────────────────────────

/// 审计事件类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuditEvent {
    /// 传输开始
    TransferStarted,
    /// 传输成功完成
    TransferCompleted,
    /// 传输失败
    TransferFailed,
    /// 认证失败（Token 无效/过期/缺失）
    AuthFailed,
    /// 单个分片上传完成（仅用于调试日志级别）
    ChunkUploaded { index: u32, total: u32 },
    /// MCP 工具调用
    McpToolCall,
}

/// 传输方向
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Send,
    Receive,
}

/// 操作结果
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AuditResult {
    Ok,
    Err(String),
}

impl AuditResult {
    pub fn is_ok(&self) -> bool {
        matches!(self, AuditResult::Ok)
    }
}

/// 一条审计记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    /// 事件类型
    pub event: AuditEvent,
    /// 传输方向
    pub direction: Direction,
    /// 使用的协议（"http" / "quic" / "s3" / "ftp"）
    pub protocol: String,
    /// 文件名（含相对路径）
    pub filename: String,
    /// 文件大小（字节）
    pub size: u64,
    /// SHA-256 哈希（十六进制），可选
    pub sha256: Option<String>,
    /// 对端 IP 地址，可选
    pub remote_ip: Option<String>,
    /// 操作结果
    pub result: AuditResult,
}

/// 写入磁盘的完整审计条目（含时间戳）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    /// Unix timestamp（秒）
    pub timestamp: u64,
    #[serde(flatten)]
    pub entry: AuditEntry,
}

impl AuditRecord {
    fn new(entry: AuditEntry) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self { timestamp, entry }
    }
}

// ──────────────────────────────── logger ────────────────────────────────────

/// 审计日志写入器（线程安全，支持多任务并发写入）
///
/// 每条记录追加为一行 JSON（JSONL 格式），文件不存在时自动创建。
#[derive(Clone)]
pub struct AuditLogger {
    file: Arc<Mutex<tokio::fs::File>>,
    path: PathBuf,
}

impl AuditLogger {
    /// 打开或创建审计日志文件
    pub async fn new(path: &Path) -> Result<Self> {
        // 确保父目录存在
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
            .map_err(|e| AeroSyncError::FileIo(e))?;

        Ok(Self {
            file: Arc::new(Mutex::new(file)),
            path: path.to_path_buf(),
        })
    }

    /// 写入一条审计记录
    pub async fn log(&self, entry: AuditEntry) -> Result<()> {
        let record = AuditRecord::new(entry);
        let mut line = serde_json::to_string(&record)
            .map_err(|e| AeroSyncError::Unknown(format!("Audit serialize error: {}", e)))?;
        line.push('\n');

        let mut file = self.file.lock().await;
        file.write_all(line.as_bytes())
            .await
            .map_err(|e| AeroSyncError::FileIo(e))?;
        file.flush()
            .await
            .map_err(|e| AeroSyncError::FileIo(e))?;

        Ok(())
    }

    /// 返回日志文件路径
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读取所有审计记录（用于查询/审查）
    pub async fn read_all(&self) -> Result<Vec<AuditRecord>> {
        let content = tokio::fs::read_to_string(&self.path)
            .await
            .map_err(|e| AeroSyncError::FileIo(e))?;

        let records = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|line| {
                serde_json::from_str::<AuditRecord>(line)
                    .map_err(|e| {
                        tracing::warn!("Audit: failed to parse record: {} — {}", e, line);
                        e
                    })
                    .ok()
            })
            .collect();

        Ok(records)
    }

    /// 读取最近 N 条记录
    pub async fn read_recent(&self, limit: usize) -> Result<Vec<AuditRecord>> {
        let all = self.read_all().await?;
        let start = all.len().saturating_sub(limit);
        Ok(all[start..].to_vec())
    }
}

// ──────────────────────────── convenience helpers ────────────────────────────

impl AuditLogger {
    /// 记录传输成功
    pub async fn log_completed(
        &self,
        direction: Direction,
        protocol: &str,
        filename: &str,
        size: u64,
        sha256: Option<&str>,
        remote_ip: Option<&str>,
    ) {
        let _ = self
            .log(AuditEntry {
                event: AuditEvent::TransferCompleted,
                direction,
                protocol: protocol.to_string(),
                filename: filename.to_string(),
                size,
                sha256: sha256.map(|s| s.to_string()),
                remote_ip: remote_ip.map(|s| s.to_string()),
                result: AuditResult::Ok,
            })
            .await;
    }

    /// 记录传输失败
    pub async fn log_failed(
        &self,
        direction: Direction,
        protocol: &str,
        filename: &str,
        size: u64,
        remote_ip: Option<&str>,
        error: &str,
    ) {
        let _ = self
            .log(AuditEntry {
                event: AuditEvent::TransferFailed,
                direction,
                protocol: protocol.to_string(),
                filename: filename.to_string(),
                size,
                sha256: None,
                remote_ip: remote_ip.map(|s| s.to_string()),
                result: AuditResult::Err(error.to_string()),
            })
            .await;
    }

    /// 记录认证失败
    pub async fn log_auth_failed(&self, protocol: &str, remote_ip: Option<&str>, reason: &str) {
        let _ = self
            .log(AuditEntry {
                event: AuditEvent::AuthFailed,
                direction: Direction::Receive,
                protocol: protocol.to_string(),
                filename: String::new(),
                size: 0,
                sha256: None,
                remote_ip: remote_ip.map(|s| s.to_string()),
                result: AuditResult::Err(reason.to_string()),
            })
            .await;
    }

    /// 记录 MCP 工具调用
    ///
    /// `tool_name`: MCP 工具名称（如 "send_file"）  
    /// `params_summary`: 参数摘要字符串（不含敏感信息，如 Token）
    pub async fn log_tool_call(&self, tool_name: &str, params_summary: &str) {
        let _ = self
            .log(AuditEntry {
                event: AuditEvent::McpToolCall,
                direction: Direction::Send,
                protocol: "mcp".to_string(),
                filename: tool_name.to_string(),
                size: 0,
                sha256: None,
                remote_ip: None,
                result: AuditResult::Ok,
            })
            .await;
        tracing::debug!(tool = tool_name, params = params_summary, "MCP tool call");
    }
}

// ─────────────────────────────────── tests ───────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn make_logger() -> (AuditLogger, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("audit.log");
        let logger = AuditLogger::new(&path).await.unwrap();
        (logger, dir)
    }

    #[tokio::test]
    async fn test_log_and_read_back() {
        let (logger, _dir) = make_logger().await;

        logger
            .log(AuditEntry {
                event: AuditEvent::TransferCompleted,
                direction: Direction::Receive,
                protocol: "http".to_string(),
                filename: "test.bin".to_string(),
                size: 1024,
                sha256: Some("abc123".to_string()),
                remote_ip: Some("127.0.0.1".to_string()),
                result: AuditResult::Ok,
            })
            .await
            .unwrap();

        let records = logger.read_all().await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].entry.filename, "test.bin");
        assert_eq!(records[0].entry.size, 1024);
        assert!(records[0].timestamp > 0);
    }

    #[tokio::test]
    async fn test_multiple_entries_appended() {
        let (logger, _dir) = make_logger().await;

        for i in 0..5u64 {
            logger
                .log_completed(
                    Direction::Receive,
                    "http",
                    &format!("file_{}.bin", i),
                    i * 1024,
                    None,
                    Some("10.0.0.1"),
                )
                .await;
        }

        let records = logger.read_all().await.unwrap();
        assert_eq!(records.len(), 5);
        assert_eq!(records[2].entry.filename, "file_2.bin");
    }

    #[tokio::test]
    async fn test_read_recent_limits_results() {
        let (logger, _dir) = make_logger().await;

        for i in 0..10u64 {
            logger
                .log_completed(
                    Direction::Send,
                    "quic",
                    &format!("f{}.bin", i),
                    i,
                    None,
                    None,
                )
                .await;
        }

        let recent = logger.read_recent(3).await.unwrap();
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[2].entry.filename, "f9.bin");
    }

    #[tokio::test]
    async fn test_auth_failed_event() {
        let (logger, _dir) = make_logger().await;

        logger
            .log_auth_failed("http", Some("192.168.1.1"), "Invalid token")
            .await;

        let records = logger.read_all().await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].entry.event, AuditEvent::AuthFailed);
        assert!(matches!(records[0].entry.result, AuditResult::Err(_)));
    }

    #[tokio::test]
    async fn test_log_failed_event() {
        let (logger, _dir) = make_logger().await;

        logger
            .log_failed(
                Direction::Receive,
                "http",
                "broken.bin",
                0,
                Some("10.0.0.2"),
                "SHA-256 mismatch",
            )
            .await;

        let records = logger.read_all().await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].entry.event, AuditEvent::TransferFailed);
        assert_eq!(
            records[0].entry.result,
            AuditResult::Err("SHA-256 mismatch".to_string())
        );
    }

    #[tokio::test]
    async fn test_jsonl_format_one_line_per_record() {
        let (logger, _dir) = make_logger().await;

        logger.log_completed(Direction::Receive, "ftp", "a.txt", 100, None, None).await;
        logger.log_completed(Direction::Send, "s3", "b.txt", 200, None, None).await;

        let content = tokio::fs::read_to_string(logger.path()).await.unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        // 每行都是合法 JSON
        for line in lines {
            assert!(serde_json::from_str::<serde_json::Value>(line).is_ok());
        }
    }

    #[tokio::test]
    async fn test_file_created_on_new() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sub/audit.log");
        // 父目录不存在，应自动创建
        let logger = AuditLogger::new(&path).await.unwrap();
        assert!(logger.path().exists());
    }

    #[tokio::test]
    async fn test_concurrent_writes() {
        let (logger, _dir) = make_logger().await;
        let logger = Arc::new(logger);

        let handles: Vec<_> = (0..20)
            .map(|i| {
                let l = Arc::clone(&logger);
                tokio::spawn(async move {
                    l.log_completed(
                        Direction::Receive,
                        "http",
                        &format!("file_{}.bin", i),
                        i as u64 * 512,
                        None,
                        Some("127.0.0.1"),
                    )
                    .await;
                })
            })
            .collect();

        for h in handles {
            h.await.unwrap();
        }

        let records = logger.read_all().await.unwrap();
        assert_eq!(records.len(), 20);
    }
}

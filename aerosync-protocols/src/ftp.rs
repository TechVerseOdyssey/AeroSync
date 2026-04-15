//! FTP 协议适配器
//!
//! 使用 suppaftp（tokio 异步后端）实现 FTP/FTPS 文件传输。
//! URL 格式：ftp://host:port/path/to/file

use crate::traits::{TransferProtocol, TransferProgress};
use aerosync_core::{AeroSyncError, Result, TransferTask};
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::time::Instant;

#[derive(Debug, Clone)]
pub struct FtpConfig {
    /// FTP 服务器主机名或 IP
    pub host: String,
    /// FTP 端口，默认 21
    pub port: u16,
    /// 登录用户名
    pub username: String,
    /// 登录密码
    pub password: String,
    /// 被动模式（防火墙穿透），默认 true
    pub passive_mode: bool,
}

impl Default for FtpConfig {
    fn default() -> Self {
        Self {
            host: "localhost".to_string(),
            port: 21,
            username: "anonymous".to_string(),
            password: String::new(),
            passive_mode: true,
        }
    }
}

pub struct FtpTransfer {
    config: FtpConfig,
}

impl FtpTransfer {
    pub fn new(config: FtpConfig) -> Self {
        Self { config }
    }

    /// 从 ftp://host:port/path 格式解析主机、端口和远程路径
    pub fn parse_ftp_url(url: &str) -> Result<(String, u16, String)> {
        let stripped = url
            .strip_prefix("ftp://")
            .ok_or_else(|| AeroSyncError::InvalidConfig(format!("Not an FTP URL: {}", url)))?;

        let (host_port, path) = stripped.split_once('/').unwrap_or((stripped, ""));
        let (host, port) = if let Some((h, p)) = host_port.split_once(':') {
            let port: u16 = p
                .parse()
                .map_err(|_| AeroSyncError::InvalidConfig(format!("Invalid FTP port: {}", p)))?;
            (h.to_string(), port)
        } else {
            (host_port.to_string(), 21u16)
        };

        if host.is_empty() {
            return Err(AeroSyncError::InvalidConfig(format!(
                "FTP URL has empty host: {}",
                url
            )));
        }

        Ok((host, port, path.to_string()))
    }

    /// 建立 FTP 连接并登录
    async fn connect(&self) -> Result<suppaftp::tokio::AsyncFtpStream> {
        let addr = format!("{}:{}", self.config.host, self.config.port);
        let mut stream = suppaftp::tokio::AsyncFtpStream::connect(&addr)
            .await
            .map_err(|e| AeroSyncError::Network(format!("FTP connect failed to {}: {}", addr, e)))?;

        stream
            .login(&self.config.username, &self.config.password)
            .await
            .map_err(|e| AeroSyncError::Network(format!("FTP login failed: {}", e)))?;

        if self.config.passive_mode {
            stream
                .transfer_type(suppaftp::types::FileType::Binary)
                .await
                .map_err(|e| AeroSyncError::Network(format!("FTP set binary mode failed: {}", e)))?;
        }

        Ok(stream)
    }
}

#[async_trait]
impl TransferProtocol for FtpTransfer {
    async fn upload_file(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let remote_path = if task.destination.starts_with("ftp://") {
            let (_, _, path) = Self::parse_ftp_url(&task.destination)?;
            if path.is_empty() {
                task.source_path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "upload".to_string())
            } else {
                path
            }
        } else {
            task.destination.clone()
        };

        let data = tokio::fs::read(&task.source_path).await?;
        let file_size = data.len() as u64;
        let start = Instant::now();

        let mut stream = self.connect().await?;

        // 确保远端目录存在（尽力而为，失败不报错）
        if let Some(parent) = std::path::Path::new(&remote_path).parent() {
            if parent != std::path::Path::new("") {
                let _ = stream
                    .mkdir(&parent.to_string_lossy())
                    .await;
            }
        }

        let mut cursor = std::io::Cursor::new(data);
        stream
            .put_file(&remote_path, &mut cursor)
            .await
            .map_err(|e| AeroSyncError::Network(format!("FTP STOR failed: {}", e)))?;

        let _ = stream.quit().await;

        let elapsed = start.elapsed().as_secs_f64();
        let speed = if elapsed > 0.0 {
            file_size as f64 / elapsed
        } else {
            0.0
        };
        let _ = progress_tx.send(TransferProgress {
            bytes_transferred: file_size,
            transfer_speed: speed,
        });

        tracing::info!("FTP: Upload OK: {} → {} ({} bytes)", task.source_path.display(), remote_path, file_size);
        Ok(())
    }

    async fn download_file(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let remote_path = if task.destination.starts_with("ftp://") {
            let (_, _, path) = Self::parse_ftp_url(&task.destination)?;
            path
        } else {
            task.destination.clone()
        };

        if remote_path.is_empty() {
            return Err(AeroSyncError::InvalidConfig(
                "FTP download: empty remote path".to_string(),
            ));
        }

        let start = Instant::now();
        let mut stream = self.connect().await?;

        let mut ftp_stream = stream
            .retr_as_stream(&remote_path)
            .await
            .map_err(|e| AeroSyncError::Network(format!("FTP RETR failed: {}", e)))?;

        use tokio::io::AsyncReadExt;
        let mut data = Vec::new();
        ftp_stream
            .read_to_end(&mut data)
            .await
            .map_err(|e| AeroSyncError::Network(format!("FTP read failed: {}", e)))?;

        // 关闭数据连接
        stream
            .finalize_retr_stream(ftp_stream)
            .await
            .map_err(|e| AeroSyncError::Network(format!("FTP finalize failed: {}", e)))?;

        let _ = stream.quit().await;

        let file_size = data.len() as u64;
        tokio::fs::write(&task.source_path, &data).await?;

        let elapsed = start.elapsed().as_secs_f64();
        let speed = if elapsed > 0.0 {
            file_size as f64 / elapsed
        } else {
            0.0
        };
        let _ = progress_tx.send(TransferProgress {
            bytes_transferred: file_size,
            transfer_speed: speed,
        });

        tracing::info!("FTP: Download OK: {} → {} ({} bytes)", remote_path, task.source_path.display(), file_size);
        Ok(())
    }

    async fn resume_transfer(
        &self,
        task: &TransferTask,
        _offset: u64,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        // FTP 不支持断点续传，重新上传
        self.upload_file(task, progress_tx).await
    }

    fn supports_resume(&self) -> bool {
        false
    }

    fn protocol_name(&self) -> &'static str {
        "FTP"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 1. FtpConfig defaults ─────────────────────────────────────────────────
    #[test]
    fn test_ftp_config_defaults() {
        let cfg = FtpConfig::default();
        assert_eq!(cfg.port, 21);
        assert_eq!(cfg.username, "anonymous");
        assert!(cfg.passive_mode);
    }

    // ── 2. FtpTransfer construction ───────────────────────────────────────────
    #[test]
    fn test_ftp_transfer_new() {
        let ft = FtpTransfer::new(FtpConfig::default());
        assert_eq!(ft.protocol_name(), "FTP");
        assert!(!ft.supports_resume());
    }

    // ── 3. parse_ftp_url: with port ───────────────────────────────────────────
    #[test]
    fn test_parse_ftp_url_with_port() {
        let (host, port, path) = FtpTransfer::parse_ftp_url("ftp://myhost:2121/uploads/file.txt").unwrap();
        assert_eq!(host, "myhost");
        assert_eq!(port, 2121);
        assert_eq!(path, "uploads/file.txt");
    }

    // ── 4. parse_ftp_url: default port ───────────────────────────────────────
    #[test]
    fn test_parse_ftp_url_default_port() {
        let (host, port, path) = FtpTransfer::parse_ftp_url("ftp://ftpserver/data/file.bin").unwrap();
        assert_eq!(host, "ftpserver");
        assert_eq!(port, 21);
        assert_eq!(path, "data/file.bin");
    }

    // ── 5. parse_ftp_url: wrong scheme → Err ─────────────────────────────────
    #[test]
    fn test_parse_ftp_url_wrong_scheme() {
        let result = FtpTransfer::parse_ftp_url("http://host/path");
        assert!(result.is_err());
    }

    // ── 6. parse_ftp_url: empty host → Err ───────────────────────────────────
    #[test]
    fn test_parse_ftp_url_empty_host() {
        let result = FtpTransfer::parse_ftp_url("ftp:///path");
        assert!(result.is_err());
    }

    // ── 7. upload to non-existent server returns Network error ────────────────
    #[tokio::test]
    async fn test_ftp_upload_connection_refused() {
        use aerosync_core::TransferTask;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let file = dir.path().join("test.bin");
        tokio::fs::write(&file, b"data").await.unwrap();

        let cfg = FtpConfig {
            host: "127.0.0.1".to_string(),
            port: 19876, // unused port
            ..FtpConfig::default()
        };
        let ft = FtpTransfer::new(cfg);
        let task = TransferTask::new_upload(file, "ftp://127.0.0.1:19876/test.bin".to_string(), 4);
        let (tx, _rx) = mpsc::unbounded_channel();

        let result = ft.upload_file(&task, tx).await;
        assert!(result.is_err(), "should fail with connection refused");
        match result {
            Err(AeroSyncError::Network(_)) => {}
            Err(e) => panic!("Expected Network error, got {:?}", e),
            Ok(_) => panic!("Should not succeed"),
        }
    }
}

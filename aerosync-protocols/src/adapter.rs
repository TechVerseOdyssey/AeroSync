/// AutoAdapter: 根据 destination URL 自动选择 HTTP 或 QUIC 协议，
/// 实现 aerosync-core 的 ProtocolAdapter trait，由 main.rs 注入。

use aerosync_core::resume::ResumeState;
use aerosync_core::transfer::{ProtocolAdapter, ProtocolProgress, TransferTask};
use aerosync_core::{AeroSyncError, Result};
use async_trait::async_trait;
use reqwest::Client;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::ftp::{FtpConfig, FtpTransfer};
use crate::http::{HttpConfig, HttpTransfer};
use crate::quic::{QuicConfig, QuicTransfer};
use crate::s3::{S3Config, S3Transfer};
use crate::traits::{TransferProtocol, TransferProgress as ProtoProgress};

pub struct AutoAdapter {
    http_config: HttpConfig,
    quic_config_base: QuicConfig,
    shared_client: Arc<Client>,
    s3_config: Option<S3Config>,
    ftp_config: Option<FtpConfig>,
}

impl AutoAdapter {
    pub fn new(http_config: HttpConfig, quic_config_base: QuicConfig) -> Self {
        // 构建一个共享的 reqwest::Client，连接池在所有上传/下载请求间复用
        let client = Client::builder()
            .timeout(Duration::from_secs(http_config.timeout_seconds))
            .build()
            .unwrap_or_default();
        Self {
            http_config,
            quic_config_base,
            shared_client: Arc::new(client),
            s3_config: None,
            ftp_config: None,
        }
    }

    /// Builder: 配置 S3 协议支持（MinIO 或 AWS S3）
    pub fn with_s3(mut self, cfg: S3Config) -> Self {
        self.s3_config = Some(cfg);
        self
    }

    /// Builder: 配置 FTP 协议支持
    pub fn with_ftp(mut self, cfg: FtpConfig) -> Self {
        self.ftp_config = Some(cfg);
        self
    }
}

#[async_trait]
impl ProtocolAdapter for AutoAdapter {
    async fn upload(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<ProtocolProgress>,
    ) -> Result<()> {
        // S3 路由
        if task.destination.starts_with("s3://") {
            let cfg = self.s3_config.clone().ok_or_else(|| {
                AeroSyncError::InvalidConfig("S3 config not set; use AutoAdapter::with_s3()".to_string())
            })?;
            let s3 = S3Transfer::new(cfg)?;
            let (tx, mut rx) = mpsc::unbounded_channel::<ProtoProgress>();
            let ptx = progress_tx.clone();
            tokio::spawn(async move {
                while let Some(p) = rx.recv().await {
                    let _ = ptx.send(ProtocolProgress {
                        bytes_transferred: p.bytes_transferred,
                        transfer_speed: p.transfer_speed,
                    });
                }
            });
            return s3.upload_file(task, tx).await;
        }

        // FTP 路由
        if task.destination.starts_with("ftp://") {
            let cfg = self.ftp_config.clone().ok_or_else(|| {
                AeroSyncError::InvalidConfig("FTP config not set; use AutoAdapter::with_ftp()".to_string())
            })?;
            let ft = FtpTransfer::new(cfg);
            let (tx, mut rx) = mpsc::unbounded_channel::<ProtoProgress>();
            let ptx = progress_tx.clone();
            tokio::spawn(async move {
                while let Some(p) = rx.recv().await {
                    let _ = ptx.send(ProtocolProgress {
                        bytes_transferred: p.bytes_transferred,
                        transfer_speed: p.transfer_speed,
                    });
                }
            });
            return ft.upload_file(task, tx).await;
        }

        if task.destination.starts_with("quic://") {
            let qc = resolve_quic_config(&task.destination, &self.quic_config_base)?;
            let qt = QuicTransfer::new(qc)?;
            let (tx, mut rx) = mpsc::unbounded_channel::<ProtoProgress>();
            // 桥接进度
            let ptx = progress_tx.clone();
            tokio::spawn(async move {
                while let Some(p) = rx.recv().await {
                    let _ = ptx.send(ProtocolProgress {
                        bytes_transferred: p.bytes_transferred,
                        transfer_speed: p.transfer_speed,
                    });
                }
            });
            qt.upload_file(task, tx).await
        } else {
            // HTTP（包括 http:// 和 host:port 规范化后的地址）
            let ht = HttpTransfer::new_with_client(Arc::clone(&self.shared_client), self.http_config.clone());
            let (tx, mut rx) = mpsc::unbounded_channel::<ProtoProgress>();
            let ptx = progress_tx.clone();
            tokio::spawn(async move {
                while let Some(p) = rx.recv().await {
                    let _ = ptx.send(ProtocolProgress {
                        bytes_transferred: p.bytes_transferred,
                        transfer_speed: p.transfer_speed,
                    });
                }
            });
            ht.upload_file(task, tx).await
        }
    }

    async fn download(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<ProtocolProgress>,
    ) -> Result<()> {
        // S3 路由
        if task.destination.starts_with("s3://") {
            let cfg = self.s3_config.clone().ok_or_else(|| {
                AeroSyncError::InvalidConfig("S3 config not set".to_string())
            })?;
            let s3 = S3Transfer::new(cfg)?;
            let (tx, mut rx) = mpsc::unbounded_channel::<ProtoProgress>();
            let ptx = progress_tx.clone();
            tokio::spawn(async move {
                while let Some(p) = rx.recv().await {
                    let _ = ptx.send(ProtocolProgress {
                        bytes_transferred: p.bytes_transferred,
                        transfer_speed: p.transfer_speed,
                    });
                }
            });
            return s3.download_file(task, tx).await;
        }

        // FTP 路由
        if task.destination.starts_with("ftp://") {
            let cfg = self.ftp_config.clone().ok_or_else(|| {
                AeroSyncError::InvalidConfig("FTP config not set".to_string())
            })?;
            let ft = FtpTransfer::new(cfg);
            let (tx, mut rx) = mpsc::unbounded_channel::<ProtoProgress>();
            let ptx = progress_tx.clone();
            tokio::spawn(async move {
                while let Some(p) = rx.recv().await {
                    let _ = ptx.send(ProtocolProgress {
                        bytes_transferred: p.bytes_transferred,
                        transfer_speed: p.transfer_speed,
                    });
                }
            });
            return ft.download_file(task, tx).await;
        }

        if task.destination.starts_with("quic://") {
            let qc = resolve_quic_config(&task.destination, &self.quic_config_base)?;
            let qt = QuicTransfer::new(qc)?;
            let (tx, mut rx) = mpsc::unbounded_channel::<ProtoProgress>();
            let ptx = progress_tx.clone();
            tokio::spawn(async move {
                while let Some(p) = rx.recv().await {
                    let _ = ptx.send(ProtocolProgress {
                        bytes_transferred: p.bytes_transferred,
                        transfer_speed: p.transfer_speed,
                    });
                }
            });
            qt.download_file(task, tx).await
        } else {
            let ht = HttpTransfer::new_with_client(Arc::clone(&self.shared_client), self.http_config.clone());
            let (tx, mut rx) = mpsc::unbounded_channel::<ProtoProgress>();
            let ptx = progress_tx.clone();
            tokio::spawn(async move {
                while let Some(p) = rx.recv().await {
                    let _ = ptx.send(ProtocolProgress {
                        bytes_transferred: p.bytes_transferred,
                        transfer_speed: p.transfer_speed,
                    });
                }
            });
            ht.download_file(task, tx).await
        }
    }

    fn protocol_name(&self) -> &'static str {
        "auto"
    }

    async fn upload_chunked(
        &self,
        task: &TransferTask,
        state: &mut ResumeState,
        progress_tx: mpsc::UnboundedSender<ProtocolProgress>,
    ) -> Result<()> {
        // S3/FTP/QUIC 不支持分片，fallback 到普通上传
        if task.destination.starts_with("s3://")
            || task.destination.starts_with("ftp://")
            || task.destination.starts_with("quic://")
        {
            return self.upload(task, progress_tx).await;
        }

        // 从完整 URL 中提取 base_url（scheme + host + port）
        let base_url = extract_base_url(&task.destination)?;
        let ht = HttpTransfer::new_with_client(Arc::clone(&self.shared_client), self.http_config.clone());

        let (tx, mut rx) = mpsc::unbounded_channel::<ProtoProgress>();
        let ptx = progress_tx.clone();
        tokio::spawn(async move {
            while let Some(p) = rx.recv().await {
                let _ = ptx.send(ProtocolProgress {
                    bytes_transferred: p.bytes_transferred,
                    transfer_speed: p.transfer_speed,
                });
            }
        });

        ht.upload_chunked(&task.source_path, &base_url, state, tx).await
    }
}

fn extract_base_url(url: &str) -> Result<String> {
    // 提取 scheme://host:port，去掉路径部分
    // 例: http://host:7788/upload → http://host:7788
    let url = url::Url::parse(url)
        .map_err(|e| AeroSyncError::InvalidConfig(format!("Invalid URL '{}': {}", url, e)))?;
    let base = format!(
        "{}://{}",
        url.scheme(),
        url.host_str().unwrap_or("localhost")
    );
    Ok(if let Some(port) = url.port() {
        format!("{}:{}", base, port)
    } else {
        base
    })
}

fn resolve_quic_config(destination: &str, base: &QuicConfig) -> Result<QuicConfig> {
    let stripped = destination
        .strip_prefix("quic://")
        .ok_or_else(|| AeroSyncError::InvalidConfig(format!("Invalid QUIC URL: {}", destination)))?;

    let host_port = stripped.split('/').next().unwrap_or(stripped);
    let (host, port_str) = host_port.split_once(':').ok_or_else(|| {
        AeroSyncError::InvalidConfig(format!("QUIC URL missing port: {}", destination))
    })?;

    let port: u16 = port_str
        .trim()
        .parse()
        .map_err(|_| AeroSyncError::InvalidConfig(format!("Invalid port: {}", destination)))?;

    let addr = format!("{}:{}", host.trim(), port)
        .parse()
        .map_err(|_| AeroSyncError::InvalidConfig(format!("Invalid addr: {}:{}", host, port)))?;

    Ok(QuicConfig {
        server_name: host.trim().to_string(),
        server_addr: addr,
        alpn_protocols: base.alpn_protocols.clone(),
        max_idle_timeout: base.max_idle_timeout,
        keep_alive_interval: base.keep_alive_interval,
        auth_token: base.auth_token.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aerosync_core::transfer::TransferTask;

    fn default_quic_config() -> QuicConfig {
        QuicConfig::default()
    }

    // ── 1. resolve_quic_config: valid URL ────────────────────────────────────
    #[test]
    fn test_resolve_quic_config_valid() {
        let base = default_quic_config();
        let result = resolve_quic_config("quic://127.0.0.1:7789", &base);
        assert!(result.is_ok());
        let cfg = result.unwrap();
        assert_eq!(cfg.server_name, "127.0.0.1");
        assert_eq!(cfg.server_addr.port(), 7789);
    }

    // ── 2. resolve_quic_config: path suffix stripped ──────────────────────────
    #[test]
    fn test_resolve_quic_config_with_path() {
        let base = default_quic_config();
        let result = resolve_quic_config("quic://192.168.1.5:9000/upload", &base);
        assert!(result.is_ok());
        let cfg = result.unwrap();
        assert_eq!(cfg.server_name, "192.168.1.5");
        assert_eq!(cfg.server_addr.port(), 9000);
    }

    // ── 3. resolve_quic_config: missing port → Err ───────────────────────────
    #[test]
    fn test_resolve_quic_config_missing_port() {
        let base = default_quic_config();
        let result = resolve_quic_config("quic://127.0.0.1", &base);
        assert!(result.is_err());
    }

    // ── 4. resolve_quic_config: invalid prefix → Err ─────────────────────────
    #[test]
    fn test_resolve_quic_config_bad_prefix() {
        let base = default_quic_config();
        let result = resolve_quic_config("http://127.0.0.1:7789", &base);
        assert!(result.is_err());
    }

    // ── 5. resolve_quic_config: inherits base auth_token ─────────────────────
    #[test]
    fn test_resolve_quic_config_inherits_auth_token() {
        let mut base = default_quic_config();
        base.auth_token = Some("secret-token".to_string());
        let result = resolve_quic_config("quic://127.0.0.1:7789", &base).unwrap();
        assert_eq!(result.auth_token, Some("secret-token".to_string()));
    }

    // ── 6. AutoAdapter construction ───────────────────────────────────────────
    #[test]
    fn test_auto_adapter_new() {
        let adapter = AutoAdapter::new(HttpConfig::default(), default_quic_config());
        assert_eq!(adapter.protocol_name(), "auto");
    }

    // ── 7. HTTP routing: http:// destination uses HTTP protocol ───────────────
    #[tokio::test]
    async fn test_auto_adapter_routes_http() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.bin");
        tokio::fs::write(&file_path, b"data").await.unwrap();

        let task = TransferTask {
            id: uuid::Uuid::new_v4(),
            source_path: file_path,
            destination: "http://127.0.0.1:19999/upload".to_string(),
            is_upload: true,
            file_size: 4,
            sha256: None,
        };
        let adapter = AutoAdapter::new(HttpConfig::default(), default_quic_config());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let result = adapter.upload(&task, tx).await;
        // Should fail with a Network error (connection refused), not a config/parse error
        match result {
            Err(aerosync_core::AeroSyncError::Network(_)) => {} // expected
            Err(e) => panic!("Unexpected error type: {:?}", e),
            Ok(_) => panic!("Should not succeed without a real server"),
        }
    }

    // ── 8. QUIC routing: quic:// destination uses QUIC protocol ──────────────
    #[tokio::test]
    async fn test_auto_adapter_routes_quic() {
        // Only test URL parsing / config resolution — don't actually connect
        let base = default_quic_config();
        let result = resolve_quic_config("quic://127.0.0.1:19998", &base);
        assert!(result.is_ok(), "valid quic:// URL should resolve OK");
        let cfg = result.unwrap();
        assert_eq!(cfg.server_addr.port(), 19998);
        assert_eq!(cfg.server_name, "127.0.0.1");
    }

    // ── 9. Download routes to HTTP ────────────────────────────────────────────
    #[tokio::test]
    async fn test_auto_adapter_download_routes_http() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let dest_path = dir.path().join("out.bin");

        let task = TransferTask {
            id: uuid::Uuid::new_v4(),
            source_path: dest_path,
            destination: "http://127.0.0.1:19997/file.bin".to_string(),
            is_upload: false,
            file_size: 0,
            sha256: None,
        };
        let adapter = AutoAdapter::new(HttpConfig::default(), default_quic_config());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let result = adapter.download(&task, tx).await;
        match result {
            Err(aerosync_core::AeroSyncError::Network(_)) => {}
            Err(e) => panic!("Unexpected error: {:?}", e),
            Ok(_) => panic!("Should not succeed without server"),
        }
    }
}

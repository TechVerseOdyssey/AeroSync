use crate::protocols::traits::{TransferProgress, TransferProtocol};
use crate::core::{AeroSyncError, Result, TransferTask};
use async_trait::async_trait;
use quinn::{ClientConfig, Connection, Endpoint};
use rustls::client::{ServerCertVerified, ServerCertVerifier};
use rustls::{Certificate, ClientConfig as TlsClientConfig, ServerName};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::Instant;
use zeroize::Zeroizing;

/// 证书固定验证器：只接受指定指纹的证书，而非完全跳过验证。
/// 用于开发/自签名场景。生产环境应使用系统证书库。
struct PinnedCertVerifier {
    /// 接受的证书 DER 字节集合（为空时表示开发模式，接受任何证书并打印警告）
    accepted_certs: Vec<Vec<u8>>,
    dev_mode: bool,
}

impl PinnedCertVerifier {
    /// 开发模式：接受任何自签名证书，但打印安全警告。
    fn dev_mode() -> Arc<Self> {
        tracing::warn!(
            "QUIC: Using development certificate mode — \
            do NOT use in production. Set accepted_certs for proper pinning."
        );
        Arc::new(Self {
            accepted_certs: vec![],
            dev_mode: true,
        })
    }

    /// 生产模式：只接受固定的证书列表。
    fn with_pinned(certs: Vec<Vec<u8>>) -> Arc<Self> {
        Arc::new(Self {
            accepted_certs: certs,
            dev_mode: false,
        })
    }
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &Certificate,
        _intermediates: &[Certificate],
        _server_name: &ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: SystemTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        if self.dev_mode {
            // 开发模式：接受但警告
            return Ok(ServerCertVerified::assertion());
        }
        // 生产模式：检查证书是否在固定列表中
        if self
            .accepted_certs
            .iter()
            .any(|c| c == end_entity.0.as_slice())
        {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "Certificate not in pinned list".to_string(),
            ))
        }
    }
}

pub struct QuicTransfer {
    endpoint: Endpoint,
    config: QuicConfig,
}

#[derive(Debug, Clone)]
pub struct QuicConfig {
    pub server_name: String,
    pub server_addr: SocketAddr,
    pub alpn_protocols: Vec<String>,
    pub max_idle_timeout: u64,
    pub keep_alive_interval: u64,
    /// 发送方认证 Token（在 UPLOAD 消息头中传递）
    pub auth_token: Option<Zeroizing<String>>,
    /// 服务端 DER 格式证书文件路径列表（用于证书钉扎）。
    /// 非空时只信任列表中的证书，替代开发模式。
    pub pinned_server_certs: Vec<PathBuf>,
}

impl Default for QuicConfig {
    fn default() -> Self {
        Self {
            server_name: "localhost".to_string(),
            server_addr: "127.0.0.1:7789".parse().unwrap(),
            alpn_protocols: vec!["aerosync".to_string()],
            max_idle_timeout: 60_000,
            keep_alive_interval: 5_000,
            auth_token: None,
            pinned_server_certs: vec![],
        }
    }
}

impl QuicTransfer {
    pub fn new(config: QuicConfig) -> Result<Self> {
        let verifier = if !config.pinned_server_certs.is_empty() {
            // 生产钉扎模式：加载 DER 文件
            let certs: Result<Vec<Vec<u8>>> = config
                .pinned_server_certs
                .iter()
                .map(|p| {
                    std::fs::read(p).map_err(|e| {
                        AeroSyncError::InvalidConfig(format!(
                            "Cannot read pinned cert {}: {}",
                            p.display(),
                            e
                        ))
                    })
                })
                .collect();
            PinnedCertVerifier::with_pinned(certs?)
        } else {
            // 开发模式 fallback（打印警告）
            PinnedCertVerifier::dev_mode()
        };

        let mut tls_config = TlsClientConfig::builder()
            .with_safe_defaults()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();

        tls_config.alpn_protocols = config
            .alpn_protocols
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect();

        let mut transport = quinn::TransportConfig::default();
        transport.max_idle_timeout(Some(
            std::time::Duration::from_millis(config.max_idle_timeout)
                .try_into()
                .unwrap(),
        ));
        transport.keep_alive_interval(Some(std::time::Duration::from_millis(
            config.keep_alive_interval,
        )));

        let mut client_config = ClientConfig::new(Arc::new(tls_config));
        client_config.transport_config(Arc::new(transport));

        let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap())
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;
        endpoint.set_default_client_config(client_config);

        Ok(Self { endpoint, config })
    }

    async fn establish_connection(&self) -> Result<Connection> {
        let connection = self
            .endpoint
            .connect(self.config.server_addr, &self.config.server_name)
            .map_err(|e| AeroSyncError::Network(e.to_string()))?
            .await
            .map_err(|e| AeroSyncError::Network(format!("QUIC connect failed: {}", e)))?;
        Ok(connection)
    }

    async fn upload_with_progress(
        &self,
        connection: &Connection,
        file_path: &std::path::Path,
        sha256: Option<&str>,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let mut file = File::open(file_path).await?;
        let file_size = file.metadata().await?.len();

        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");

        let (mut send, _recv) = connection
            .open_bi()
            .await
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;

        // 格式: UPLOAD:<filename>:<size>[:<token>]\n
        let header = if let Some(token) = &self.config.auth_token {
            format!("UPLOAD:{}:{}:{}\n", file_name, file_size, token.as_str())
        } else {
            format!("UPLOAD:{}:{}\n", file_name, file_size)
        };

        // 附加 SHA-256（追加在 header 后的独立行）
        let metadata = if let Some(hash) = sha256 {
            format!("{}HASH:{}\n", header, hash)
        } else {
            header
        };

        send.write_all(metadata.as_bytes())
            .await
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;

        let mut buf = vec![0u8; 64 * 1024];
        let mut transferred = 0u64;
        let start = Instant::now();

        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            send.write_all(&buf[..n])
                .await
                .map_err(|e| AeroSyncError::Network(e.to_string()))?;
            transferred += n as u64;
            let elapsed = start.elapsed().as_secs_f64();
            let speed = if elapsed > 0.0 {
                transferred as f64 / elapsed
            } else {
                0.0
            };
            let _ = progress_tx.send(TransferProgress {
                bytes_transferred: transferred,
                transfer_speed: speed,
            });
        }

        send.finish()
            .await
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;

        tracing::info!(
            "QUIC: Uploaded '{}' ({} bytes) at {:.2} MB/s",
            file_name,
            file_size,
            transferred as f64 / start.elapsed().as_secs_f64() / 1_048_576.0
        );
        Ok(())
    }

    async fn download_with_progress(
        &self,
        connection: &Connection,
        file_path: &std::path::Path,
        file_name: &str,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;

        let request = format!("DOWNLOAD:{}", file_name);
        send.write_all(request.as_bytes())
            .await
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;
        send.finish()
            .await
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;

        let mut file = File::create(file_path).await?;
        let mut transferred = 0u64;
        let start = Instant::now();
        let mut buf = vec![0u8; 64 * 1024];

        loop {
            match recv.read(&mut buf).await {
                Ok(Some(n)) => {
                    file.write_all(&buf[..n]).await?;
                    transferred += n as u64;
                    let elapsed = start.elapsed().as_secs_f64();
                    let speed = if elapsed > 0.0 {
                        transferred as f64 / elapsed
                    } else {
                        0.0
                    };
                    let _ = progress_tx.send(TransferProgress {
                        bytes_transferred: transferred,
                        transfer_speed: speed,
                    });
                }
                Ok(None) => break,
                Err(e) => return Err(AeroSyncError::Network(e.to_string())),
            }
        }
        file.flush().await?;
        Ok(())
    }
}

#[async_trait]
impl TransferProtocol for QuicTransfer {
    async fn upload_file(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let connection = self.establish_connection().await?;
        self.upload_with_progress(
            &connection,
            &task.source_path,
            task.sha256.as_deref(),
            progress_tx,
        )
        .await
    }

    async fn download_file(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let connection = self.establish_connection().await?;
        let file_name = task
            .source_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        self.download_with_progress(&connection, &task.source_path, &file_name, progress_tx)
            .await
    }

    async fn resume_transfer(
        &self,
        task: &TransferTask,
        _offset: u64,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        // Phase 2: 实现分片续传
        if task.is_upload {
            self.upload_file(task, progress_tx).await
        } else {
            self.download_file(task, progress_tx).await
        }
    }

    fn supports_resume(&self) -> bool {
        true
    }

    fn protocol_name(&self) -> &'static str {
        "QUIC"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 1. QuicConfig defaults ────────────────────────────────────────────────
    #[test]
    fn test_quic_pinned_cert_field_in_default() {
        let cfg = QuicConfig::default();
        assert!(cfg.pinned_server_certs.is_empty());
    }

    // ── 2. Certificate pinning: nonexistent file ──────────────────────────────
    #[test]
    fn test_quic_new_with_nonexistent_cert_returns_err() {
        let config = QuicConfig {
            pinned_server_certs: vec![PathBuf::from("/nonexistent/server.der")],
            ..QuicConfig::default()
        };
        let result = QuicTransfer::new(config);
        assert!(result.is_err());
        match result {
            Err(AeroSyncError::InvalidConfig(msg)) => {
                assert!(msg.contains("Cannot read pinned cert"), "Got: {}", msg);
            }
            Err(e) => panic!("Wrong error type: {:?}", e),
            Ok(_) => panic!("Should have failed"),
        }
    }
}

use crate::traits::{TransferProtocol, TransferProgress};
use aerosync_core::{AeroSyncError, Result, TransferTask};
use async_trait::async_trait;
use quinn::{ClientConfig, Connection, Endpoint};
use rustls::{ClientConfig as TlsClientConfig, Certificate, ServerName};
use rustls::client::{ServerCertVerifier, ServerCertVerified};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::Instant;

struct SkipServerVerification;

impl SkipServerVerification {
    fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &Certificate,
        _intermediates: &[Certificate],
        _server_name: &ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: SystemTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
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
}

impl Default for QuicConfig {
    fn default() -> Self {
        Self {
            server_name: "localhost".to_string(),
            server_addr: "127.0.0.1:4433".parse().unwrap(),
            alpn_protocols: vec!["aerosync".to_string()],
            max_idle_timeout: 30000, // 30 seconds
            keep_alive_interval: 5000, // 5 seconds
        }
    }
}

impl QuicTransfer {
    pub fn new(config: QuicConfig) -> Result<Self> {
        // Build TLS client config with ALPN to match server
        // For development: accept self-signed certificates by using a custom verifier
        // that skips verification.
        let mut tls_config = TlsClientConfig::builder()
            .with_safe_defaults()
            .with_custom_certificate_verifier(SkipServerVerification::new())
            .with_no_client_auth();
        
        // Set ALPN protocol to match server (must match server's "aerosync")
        let alpn_protocols: Vec<Vec<u8>> = config.alpn_protocols.iter()
            .map(|s| s.as_bytes().to_vec())
            .collect();
        tls_config.alpn_protocols = alpn_protocols;
        
        // Create new ClientConfig with the modified TLS config
        let client_config = ClientConfig::new(Arc::new(tls_config));

        let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap())
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;
        endpoint.set_default_client_config(client_config);

        Ok(Self { endpoint, config })
    }

    async fn establish_connection(&self) -> Result<Connection> {
        let connection = self.endpoint
            .connect(self.config.server_addr, &self.config.server_name)
            .map_err(|e| AeroSyncError::Network(e.to_string()))?
            .await
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;

        Ok(connection)
    }

    async fn upload_with_progress(
        &self,
        connection: &Connection,
        file_path: &std::path::Path,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let mut file = File::open(file_path).await?;
        let file_size = file.metadata().await?.len();

        let (mut send_stream, _recv_stream) = connection
            .open_bi()
            .await
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;

        // Send file metadata first (format: UPLOAD:filename:size)
        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        let metadata = format!("UPLOAD:{}:{}\n", file_name, file_size);
        send_stream.write_all(metadata.as_bytes()).await
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;

        let mut buffer = vec![0u8; 64 * 1024]; // 64KB chunks
        let mut bytes_transferred = 0u64;
        let start_time = Instant::now();

        loop {
            let bytes_read = file.read(&mut buffer).await?;
            if bytes_read == 0 {
                break;
            }

            send_stream.write_all(&buffer[..bytes_read]).await
                .map_err(|e| AeroSyncError::Network(e.to_string()))?;

            bytes_transferred += bytes_read as u64;
            let elapsed = start_time.elapsed().as_secs_f64();
            let speed = if elapsed > 0.0 { bytes_transferred as f64 / elapsed } else { 0.0 };

            let _ = progress_tx.send(TransferProgress {
                bytes_transferred,
                transfer_speed: speed,
            });
        }

        send_stream.finish().await
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;

        Ok(())
    }

    async fn download_with_progress(
        &self,
        connection: &Connection,
        file_path: &std::path::Path,
        file_name: &str,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let (mut send_stream, mut recv_stream) = connection
            .open_bi()
            .await
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;

        // Send download request
        let request = format!("DOWNLOAD:{}", file_name);
        send_stream.write_all(request.as_bytes()).await
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;
        send_stream.finish().await
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;

        let mut file = File::create(file_path).await?;
        let mut bytes_transferred = 0u64;
        let start_time = Instant::now();

        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            match recv_stream.read(&mut buffer).await {
                Ok(Some(bytes_read)) => {
                    file.write_all(&buffer[..bytes_read]).await?;
                    bytes_transferred += bytes_read as u64;
                    
                    let elapsed = start_time.elapsed().as_secs_f64();
                    let speed = if elapsed > 0.0 { bytes_transferred as f64 / elapsed } else { 0.0 };

                    let _ = progress_tx.send(TransferProgress {
                        bytes_transferred,
                        transfer_speed: speed,
                    });
                }
                Ok(None) => break, // Stream ended
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
        self.upload_with_progress(&connection, &task.source_path, progress_tx).await
    }

    async fn download_file(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let connection = self.establish_connection().await?;
        let file_name = task.source_path.file_name()
            .unwrap_or_default()
            .to_string_lossy();
        self.download_with_progress(&connection, &task.source_path, &file_name, progress_tx).await
    }

    async fn resume_transfer(
        &self,
        task: &TransferTask,
        offset: u64,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        // TODO: Implement QUIC resume functionality with offset
        if task.is_upload {
            self.upload_file(task, progress_tx).await
        } else {
            self.download_file(task, progress_tx).await
        }
    }

    fn supports_resume(&self) -> bool {
        true // QUIC can support resume with custom protocol
    }

    fn protocol_name(&self) -> &'static str {
        "QUIC"
    }
}
use crate::{AeroSyncError, Result, ProgressMonitor, TransferProgress};
use crate::progress::TransferStatus;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use uuid::Uuid;
use rustls::{ClientConfig as TlsClientConfig, Certificate, ServerName};
use rustls::client::{ServerCertVerifier, ServerCertVerified};
use std::time::SystemTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferConfig {
    pub max_concurrent_transfers: usize,
    pub chunk_size: usize,
    pub retry_attempts: u32,
    pub timeout_seconds: u64,
    pub use_quic: bool,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            max_concurrent_transfers: 4,
            chunk_size: 1024 * 1024, // 1MB
            retry_attempts: 3,
            timeout_seconds: 30,
            use_quic: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TransferTask {
    pub id: Uuid,
    pub source_path: PathBuf,
    pub destination: String, // URL or path
    pub file_size: u64,
    pub is_upload: bool,
}

impl TransferTask {
    pub fn new_upload(source_path: PathBuf, destination: String, file_size: u64) -> Self {
        Self {
            id: Uuid::new_v4(),
            source_path,
            destination,
            file_size,
            is_upload: true,
        }
    }

    pub fn new_download(source_url: String, destination_path: PathBuf, file_size: u64) -> Self {
        Self {
            id: Uuid::new_v4(),
            source_path: destination_path,
            destination: source_url,
            file_size,
            is_upload: false,
        }
    }
}

pub struct TransferEngine {
    config: TransferConfig,
    progress_monitor: Arc<RwLock<ProgressMonitor>>,
    task_sender: Option<mpsc::UnboundedSender<TransferTask>>,
    cancel_sender: Option<mpsc::UnboundedSender<Uuid>>,
    task_receiver: Arc<RwLock<Option<mpsc::UnboundedReceiver<TransferTask>>>>,
    cancel_receiver: Arc<RwLock<Option<mpsc::UnboundedReceiver<Uuid>>>>,
    _task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
}

impl TransferEngine {
    pub fn new(config: TransferConfig) -> Self {
        let (task_sender, task_receiver) = mpsc::unbounded_channel();
        let (cancel_sender, cancel_receiver) = mpsc::unbounded_channel();
        let progress_monitor = Arc::new(RwLock::new(ProgressMonitor::new()));
        
        Self {
            config,
            progress_monitor,
            task_sender: Some(task_sender),
            cancel_sender: Some(cancel_sender),
            task_receiver: Arc::new(RwLock::new(Some(task_receiver))),
            cancel_receiver: Arc::new(RwLock::new(Some(cancel_receiver))),
            _task_handle: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn start(&self) -> Result<()> {
        // Take the receivers and spawn the worker task
        let task_receiver = self.task_receiver.write().await.take();
        let cancel_receiver = self.cancel_receiver.write().await.take();
        
        if let (Some(task_receiver), Some(cancel_receiver)) = (task_receiver, cancel_receiver) {
            let worker_progress_monitor = Arc::clone(&self.progress_monitor);
            let worker_config = self.config.clone();
            
            let task_handle = tokio::spawn(async move {
                transfer_worker(
                    task_receiver,
                    cancel_receiver,
                    worker_progress_monitor,
                    worker_config,
                ).await;
            });
            
            *self._task_handle.write().await = Some(task_handle);
            tracing::info!("Transfer engine started");
        }
        
        Ok(())
    }

    pub async fn add_transfer(&self, task: TransferTask) -> Result<()> {
        // Better handling for file names with non-ASCII characters (e.g., Chinese)
        let file_name = task.source_path.file_name()
            .and_then(|os_str| os_str.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                task.source_path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            });
        
        let progress = TransferProgress {
            task_id: task.id,
            file_name,
            bytes_transferred: 0,
            total_bytes: task.file_size,
            transfer_speed: 0.0,
            elapsed_time: std::time::Duration::new(0, 0),
            estimated_remaining: None,
            status: TransferStatus::Pending,
        };

        {
            let mut monitor = self.progress_monitor.write().await;
            monitor.add_transfer(progress);
        }

        if let Some(ref sender) = self.task_sender {
            sender.send(task)
                .map_err(|_| AeroSyncError::System("Failed to queue transfer task".to_string()))?;
        } else {
            return Err(AeroSyncError::System("Transfer engine not started".to_string()));
        }

        Ok(())
    }

    pub async fn cancel_transfer(&self, task_id: Uuid) -> Result<()> {
        if let Some(ref sender) = self.cancel_sender {
            sender.send(task_id)
                .map_err(|_| AeroSyncError::System("Failed to send cancel signal".to_string()))?;
        } else {
            return Err(AeroSyncError::System("Transfer engine not started".to_string()));
        }
        Ok(())
    }

    pub async fn get_progress_monitor(&self) -> Arc<RwLock<ProgressMonitor>> {
        Arc::clone(&self.progress_monitor)
    }
}

async fn transfer_worker(
    mut task_receiver: mpsc::UnboundedReceiver<TransferTask>,
    mut _cancel_receiver: mpsc::UnboundedReceiver<Uuid>,
    progress_monitor: Arc<RwLock<ProgressMonitor>>,
    config: TransferConfig,
) {
    tracing::info!("Transfer worker started");
    
    while let Some(task) = task_receiver.recv().await {
        tracing::info!("Processing transfer task: {} -> {}", 
                      task.source_path.display(), task.destination);
        
        // Update task status to in progress
        {
            let mut monitor = progress_monitor.write().await;
            monitor.update_progress(task.id, 0, 0.0);
        }
        
        // Perform actual transfer based on destination URL protocol
        // Check URL protocol first, then fall back to config
        let result = if task.destination.starts_with("quic://") {
            tracing::info!("Using QUIC protocol for transfer");
            perform_quic_transfer(&task, &config, progress_monitor.clone()).await
        } else if task.destination.starts_with("http://") || task.destination.starts_with("https://") {
            tracing::info!("Using HTTP protocol for transfer");
            perform_http_transfer(&task, &config, progress_monitor.clone()).await
        } else if config.use_quic {
            // If no protocol specified but config says use QUIC, try to convert HTTP URL to QUIC
            tracing::warn!("No protocol in URL, but use_quic=true. Attempting to use HTTP instead.");
            perform_http_transfer(&task, &config, progress_monitor.clone()).await
        } else {
            // Default to HTTP
            tracing::info!("Using HTTP protocol for transfer (default)");
            perform_http_transfer(&task, &config, progress_monitor.clone()).await
        };
        
        match result {
            Ok(_) => {
                let mut monitor = progress_monitor.write().await;
                monitor.complete_transfer(task.id);
                tracing::info!("Transfer completed: {}", task.source_path.display());
            }
            Err(e) => {
                let mut monitor = progress_monitor.write().await;
                monitor.fail_transfer(task.id, e.to_string());
                tracing::error!("Transfer failed: {} - {}", task.source_path.display(), e);
            }
        }
    }
    
    tracing::info!("Transfer worker stopped");
}

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

async fn perform_quic_transfer(
    task: &TransferTask,
    _config: &TransferConfig,
    progress_monitor: Arc<RwLock<ProgressMonitor>>,
) -> Result<()> {
    use quinn::{ClientConfig, Endpoint};
    use tokio::fs::File;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use std::net::SocketAddr;
    use tokio::time::Instant;
    
    // Parse QUIC URL (format: quic://host:port or quic://host:port/path)
    let url = task.destination.strip_prefix("quic://")
        .ok_or_else(|| AeroSyncError::Network(format!("Invalid QUIC URL format. Expected quic://host:port, got: {}", task.destination)))?;
    
    // Remove path if present (e.g., quic://host:port/path -> host:port)
    let url_without_path = url.split('/').next().unwrap_or(url).trim();
    
    if url_without_path.is_empty() {
        return Err(AeroSyncError::Network(format!("Invalid QUIC URL: missing host and port in '{}'", task.destination)));
    }
    
    // Parse host and port
    let (host, port_str) = url_without_path.split_once(':')
        .ok_or_else(|| AeroSyncError::Network(format!("Invalid QUIC URL format. Expected quic://host:port, got: {}. Missing port number?", task.destination)))?;
    
    let host = host.trim();
    let port_str = port_str.trim();
    
    if host.is_empty() {
        return Err(AeroSyncError::Network(format!("Invalid QUIC URL: missing host in '{}'", task.destination)));
    }
    
    if port_str.is_empty() {
        return Err(AeroSyncError::Network(format!("Invalid QUIC URL: missing port in '{}'", task.destination)));
    }
    
    let port: u16 = port_str.parse()
        .map_err(|e| AeroSyncError::Network(format!("Invalid port number '{}' in URL '{}': {}", port_str, task.destination, e)))?;
    
    // Try to parse as IP address first, then resolve hostname
    let server_addr: SocketAddr = if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        // It's an IP address
        SocketAddr::new(ip, port)
    } else if host.eq_ignore_ascii_case("localhost") {
        // Special case for localhost - use 127.0.0.1 directly
        SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)), port)
    } else {
        // It's a hostname, resolve it using blocking DNS lookup in async context
        use std::net::ToSocketAddrs;
        let addr_string = format!("{}:{}", host, port);
        tracing::debug!("QUIC: Resolving hostname '{}'", host);
        // Use tokio's blocking task for DNS resolution
        tokio::task::spawn_blocking(move || {
            addr_string.to_socket_addrs()
        })
        .await
        .map_err(|e| AeroSyncError::Network(format!("DNS resolution task failed: {}", e)))?
        .map_err(|e| AeroSyncError::Network(format!("Failed to resolve hostname '{}' in URL '{}': {}", host, task.destination, e)))?
        .next()
        .ok_or_else(|| AeroSyncError::Network(format!("No address found for hostname '{}' in URL '{}'", host, task.destination)))?
    };
    
    tracing::info!("QUIC: Connecting to {}:{}", host, port);
    
    // Create QUIC client endpoint with ALPN protocol matching server
    // We need to create a custom TLS config with ALPN protocol matching the server
    let mut tls_config = TlsClientConfig::builder()
        .with_safe_defaults()
        .with_custom_certificate_verifier(SkipServerVerification::new())
        .with_no_client_auth();
    
    // Set ALPN protocol to match server (must match server's "aerosync")
    tls_config.alpn_protocols = vec![b"aerosync".to_vec()];
    
    // Create new ClientConfig with the modified TLS config
    let client_config = ClientConfig::new(Arc::new(tls_config));
    
    let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap())
        .map_err(|e| AeroSyncError::Network(format!("Failed to create QUIC endpoint: {}", e)))?;
    endpoint.set_default_client_config(client_config);
    
    // Establish connection
    let connection = endpoint
        .connect(server_addr, host)
        .map_err(|e| AeroSyncError::Network(format!("Failed to initiate QUIC connection: {}", e)))?
        .await
        .map_err(|e| AeroSyncError::Network(format!("Failed to establish QUIC connection: {}", e)))?;
    
    tracing::info!("QUIC: Connection established");
    
    // Open bidirectional stream
    let (mut send_stream, _recv_stream) = connection
        .open_bi()
        .await
        .map_err(|e| AeroSyncError::Network(format!("Failed to open QUIC stream: {}", e)))?;
    
    // Open file
    let mut file = File::open(&task.source_path).await?;
    let file_size = file.metadata().await?.len();
    
    // Get file name
    let file_name = task.source_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();
    
    // Send file metadata (format: UPLOAD:filename:size\n)
    let metadata = format!("UPLOAD:{}:{}\n", file_name, file_size);
    send_stream.write_all(metadata.as_bytes()).await
        .map_err(|e| AeroSyncError::Network(format!("Failed to send QUIC metadata: {}", e)))?;
    
    tracing::info!("QUIC: Uploading file '{}' ({} bytes)", file_name, file_size);
    
    // Transfer file data
    let mut buffer = vec![0u8; 64 * 1024]; // 64KB chunks
    let mut bytes_transferred = 0u64;
    let start_time = Instant::now();
    
    loop {
        let bytes_read = file.read(&mut buffer).await?;
        if bytes_read == 0 {
            break;
        }
        
        send_stream.write_all(&buffer[..bytes_read]).await
            .map_err(|e| AeroSyncError::Network(format!("Failed to send QUIC data: {}", e)))?;
        
        bytes_transferred += bytes_read as u64;
        let elapsed = start_time.elapsed().as_secs_f64();
        let speed = if elapsed > 0.0 { bytes_transferred as f64 / elapsed } else { 0.0 };
        
        // Update progress
        {
            let mut monitor = progress_monitor.write().await;
            monitor.update_progress(task.id, bytes_transferred, speed);
        }
    }
    
    // Finish stream
    send_stream.finish().await
        .map_err(|e| AeroSyncError::Network(format!("Failed to finish QUIC stream: {}", e)))?;
    
    let elapsed = start_time.elapsed().as_secs_f64();
    let speed = if elapsed > 0.0 { file_size as f64 / elapsed } else { 0.0 };
    
    // Update final progress
    {
        let mut monitor = progress_monitor.write().await;
        monitor.update_progress(task.id, file_size, speed);
    }
    
    tracing::info!("QUIC: Upload completed successfully: {} bytes at {:.2} MB/s", 
                 file_size, speed / (1024.0 * 1024.0));
    
    Ok(())
}

async fn perform_http_transfer(
    task: &TransferTask,
    config: &TransferConfig,
    progress_monitor: Arc<RwLock<ProgressMonitor>>,
) -> Result<()> {
    use reqwest::Client;
    use tokio::fs::File;
    use tokio::io::AsyncReadExt;
    use std::time::Duration;
    use tokio::time::Instant;
    
    let client = Client::builder()
        .timeout(Duration::from_secs(config.timeout_seconds))
        .build()
        .map_err(|e| AeroSyncError::Network(format!("Failed to create HTTP client: {}", e)))?;
    
    let mut file = File::open(&task.source_path).await?;
    let file_size = file.metadata().await?.len();
    
    let start_time = Instant::now();
    
    // Get file name for multipart form
    let file_name = task.source_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();
    
    // Read file contents
    let mut file_contents = Vec::new();
    file.read_to_end(&mut file_contents).await?;
    
    // Create multipart form with file
    let mut form = reqwest::multipart::Form::new();
    let file_part = reqwest::multipart::Part::bytes(file_contents)
        .file_name(file_name.clone())
        .mime_str("application/octet-stream")
        .map_err(|e| AeroSyncError::Network(format!("Failed to create multipart part: {}", e)))?;
    
    form = form.part("file", file_part);
    
    tracing::info!("HTTP: Uploading file '{}' to {}", file_name, task.destination);
    
    // Send request and update progress
    let response = client
        .post(&task.destination)
        .multipart(form)
        .send()
        .await
        .map_err(|e| AeroSyncError::Network(format!("Upload request failed: {}", e)))?;
    
    if response.status().is_success() {
        let elapsed = start_time.elapsed().as_secs_f64();
        let speed = if elapsed > 0.0 { file_size as f64 / elapsed } else { 0.0 };
        
        // Update final progress
        {
            let mut monitor = progress_monitor.write().await;
            monitor.update_progress(task.id, file_size, speed);
        }
        
        tracing::info!("HTTP: Upload completed successfully: {} bytes at {:.2} MB/s", 
                     file_size, speed / (1024.0 * 1024.0));
        Ok(())
    } else {
        let status = response.status();
        let error_text = response.text().await.unwrap_or_default();
        tracing::error!("HTTP: Upload failed with status {}: {}", status, error_text);
        Err(AeroSyncError::Network(format!(
            "Upload failed with status: {} - {}",
            status, error_text
        )))
    }
}

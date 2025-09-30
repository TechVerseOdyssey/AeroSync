use crate::{AeroSyncError, Result};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub http_port: u16,
    pub quic_port: u16,
    pub bind_address: String,
    pub receive_directory: PathBuf,
    pub max_file_size: u64, // bytes
    pub allow_overwrite: bool,
    pub enable_http: bool,
    pub enable_quic: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            http_port: 8080,
            quic_port: 4433,
            bind_address: "0.0.0.0".to_string(),
            receive_directory: PathBuf::from("./received_files"),
            max_file_size: 1024 * 1024 * 1024, // 1GB
            allow_overwrite: false,
            enable_http: true,
            enable_quic: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceivedFile {
    pub id: Uuid,
    pub original_name: String,
    pub saved_path: PathBuf,
    pub size: u64,
    pub received_at: std::time::SystemTime,
    pub sender_info: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerStatus {
    Stopped,
    Starting,
    Running,
    Error(String),
}

pub struct FileReceiver {
    config: Arc<RwLock<ServerConfig>>,
    status: Arc<RwLock<ServerStatus>>,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    http_handle: Option<tokio::task::JoinHandle<()>>,
    quic_handle: Option<tokio::task::JoinHandle<()>>,
}

impl FileReceiver {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            status: Arc::new(RwLock::new(ServerStatus::Stopped)),
            received_files: Arc::new(RwLock::new(Vec::new())),
            http_handle: None,
            quic_handle: None,
        }
    }

    pub async fn start(&mut self) -> Result<()> {
        let config = self.config.read().await;
        
        tracing::info!("Starting file receiver server...");
        tracing::info!("Configuration:");
        tracing::info!("  - HTTP port: {}", config.http_port);
        tracing::info!("  - QUIC port: {}", config.quic_port);
        tracing::info!("  - Bind address: {}", config.bind_address);
        tracing::info!("  - Receive directory: {}", config.receive_directory.display());
        tracing::info!("  - Max file size: {} MB", config.max_file_size / (1024 * 1024));
        tracing::info!("  - Allow overwrite: {}", config.allow_overwrite);
        tracing::info!("  - HTTP enabled: {}", config.enable_http);
        tracing::info!("  - QUIC enabled: {}", config.enable_quic);
        
        // Ensure receive directory exists
        tracing::info!("Creating receive directory: {}", config.receive_directory.display());
        tokio::fs::create_dir_all(&config.receive_directory).await?;
        tracing::info!("Receive directory created successfully");
        
        *self.status.write().await = ServerStatus::Starting;
        
        // Start HTTP server if enabled
        if config.enable_http {
            let http_config = config.clone();
            let status = Arc::clone(&self.status);
            let received_files = Arc::clone(&self.received_files);
            
            let handle = tokio::spawn(async move {
                tracing::info!("HTTP server task started");
                if let Err(e) = start_http_server(http_config, status.clone(), received_files).await {
                    tracing::error!("HTTP server error: {}", e);
                    tracing::error!("HTTP server task will terminate");
                    *status.write().await = ServerStatus::Error(e.to_string());
                } else {
                    tracing::info!("HTTP server task completed normally");
                }
            });
            
            self.http_handle = Some(handle);
        }
        
        // Start QUIC server if enabled
        if config.enable_quic {
            let quic_config = config.clone();
            let status = Arc::clone(&self.status);
            let received_files = Arc::clone(&self.received_files);
            
            let handle = tokio::spawn(async move {
                tracing::info!("QUIC server task started");
                if let Err(e) = start_quic_server(quic_config, status.clone(), received_files).await {
                    tracing::error!("QUIC server error: {}", e);
                    tracing::error!("QUIC server task will terminate");
                    *status.write().await = ServerStatus::Error(e.to_string());
                } else {
                    tracing::info!("QUIC server task completed normally");
                }
            });
            
            self.quic_handle = Some(handle);
        }
        
        *self.status.write().await = ServerStatus::Running;
        
        tracing::info!("File receiver started on HTTP:{} QUIC:{}", 
                      config.http_port, config.quic_port);
        
        Ok(())
    }

    pub async fn stop(&mut self) -> Result<()> {
        tracing::info!("Stopping file receiver server...");
        *self.status.write().await = ServerStatus::Stopped;
        
        if let Some(handle) = self.http_handle.take() {
            tracing::info!("Stopping HTTP server task");
            handle.abort();
            tracing::debug!("HTTP server task aborted");
        } else {
            tracing::debug!("No HTTP server task to stop");
        }
        
        if let Some(handle) = self.quic_handle.take() {
            tracing::info!("Stopping QUIC server task");
            handle.abort();
            tracing::debug!("QUIC server task aborted");
        } else {
            tracing::debug!("No QUIC server task to stop");
        }
        
        tracing::info!("✅ File receiver stopped successfully");
        Ok(())
    }

    pub async fn get_status(&self) -> ServerStatus {
        self.status.read().await.clone()
    }

    pub async fn get_config(&self) -> ServerConfig {
        self.config.read().await.clone()
    }

    pub async fn update_config(&self, new_config: ServerConfig) -> Result<()> {
        *self.config.write().await = new_config;
        Ok(())
    }

    pub async fn update_config_and_restart(&mut self, new_config: ServerConfig) -> Result<()> {
        // Check if server is currently running
        let was_running = matches!(self.get_status().await, ServerStatus::Running);
        
        tracing::info!("Updating server configuration...");
        tracing::info!("Server was running: {}", was_running);
        
        // Stop the server if it's running
        if was_running {
            tracing::info!("Stopping server for configuration update");
            self.stop().await?;
        }
        
        // Log configuration changes
        let old_config = self.config.read().await.clone();
        tracing::info!("Configuration changes:");
        if old_config.receive_directory != new_config.receive_directory {
            tracing::info!("  - Receive directory: {} -> {}", 
                old_config.receive_directory.display(), new_config.receive_directory.display());
        }
        if old_config.http_port != new_config.http_port {
            tracing::info!("  - HTTP port: {} -> {}", old_config.http_port, new_config.http_port);
        }
        if old_config.quic_port != new_config.quic_port {
            tracing::info!("  - QUIC port: {} -> {}", old_config.quic_port, new_config.quic_port);
        }
        if old_config.max_file_size != new_config.max_file_size {
            tracing::info!("  - Max file size: {} MB -> {} MB", 
                old_config.max_file_size / (1024 * 1024), new_config.max_file_size / (1024 * 1024));
        }
        if old_config.allow_overwrite != new_config.allow_overwrite {
            tracing::info!("  - Allow overwrite: {} -> {}", old_config.allow_overwrite, new_config.allow_overwrite);
        }
        
        // Update the configuration
        *self.config.write().await = new_config;
        tracing::info!("Configuration updated in memory");
        
        // Restart the server if it was running
        if was_running {
            tracing::info!("Restarting server with new configuration");
            self.start().await?;
            tracing::info!("✅ Server restarted successfully with new configuration");
        } else {
            tracing::info!("✅ Configuration updated (server was not running)");
        }
        
        Ok(())
    }

    pub async fn get_received_files(&self) -> Vec<ReceivedFile> {
        self.received_files.read().await.clone()
    }

    pub async fn clear_received_files(&self) {
        self.received_files.write().await.clear();
    }

    pub async fn get_server_urls(&self) -> Vec<String> {
        let config = self.config.read().await;
        let mut urls = Vec::new();
        
        if config.enable_http {
            urls.push(format!("http://{}:{}/upload", 
                             if config.bind_address == "0.0.0.0" { "localhost" } else { &config.bind_address },
                             config.http_port));
        }
        
        if config.enable_quic {
            urls.push(format!("quic://{}:{}", 
                             if config.bind_address == "0.0.0.0" { "localhost" } else { &config.bind_address },
                             config.quic_port));
        }
        
        urls
    }
}

async fn start_http_server(
    config: ServerConfig,
    status: Arc<RwLock<ServerStatus>>,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
) -> Result<()> {
    use warp::Filter;
    
    let receive_dir = config.receive_directory.clone();
    let max_size = config.max_file_size;
    let allow_overwrite = config.allow_overwrite;
    
    // Clone for the filter
    let received_files_filter = received_files.clone();
    
    let upload = warp::path("upload")
        .and(warp::post())
        .and(warp::multipart::form().max_length(max_size))
        .and(warp::any().map(move || receive_dir.clone()))
        .and(warp::any().map(move || allow_overwrite))
        .and(warp::any().map(move || received_files_filter.clone()))
        .and_then(handle_file_upload);

    let status_route = warp::path("status")
        .and(warp::get())
        .and(warp::any().map(move || received_files.clone()))
        .and_then(handle_status_request);

    let cors = warp::cors()
        .allow_any_origin()
        .allow_headers(vec!["content-type"])
        .allow_methods(vec!["GET", "POST", "OPTIONS"]);

    let routes = upload.or(status_route).with(cors);

    let addr: SocketAddr = format!("{}:{}", config.bind_address, config.http_port)
        .parse()
        .map_err(|e| {
            tracing::error!("Invalid HTTP server address: {}:{} - {}", config.bind_address, config.http_port, e);
            AeroSyncError::InvalidConfig(format!("Invalid address: {}", e))
        })?;

    tracing::info!("Starting HTTP server on {}", addr);
    tracing::info!("HTTP server configuration:");
    tracing::info!("  - Receive directory: {}", config.receive_directory.display());
    tracing::info!("  - Max file size: {} MB", config.max_file_size / (1024 * 1024));
    tracing::info!("  - Allow overwrite: {}", config.allow_overwrite);
    
    tracing::info!("HTTP server is ready to accept file uploads");
    warp::serve(routes)
        .run(addr)
        .await;
    
    tracing::info!("HTTP server has stopped");

    Ok(())
}

async fn handle_file_upload(
    mut form: warp::multipart::FormData,
    receive_dir: PathBuf,
    allow_overwrite: bool,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
) -> std::result::Result<impl warp::Reply, warp::Rejection> {
    use futures::TryStreamExt;
    use tokio::io::AsyncWriteExt;
    use bytes::Buf;
    
    tracing::info!("HTTP: Received file upload request");
    tracing::debug!("HTTP: Receive directory: {}", receive_dir.display());
    tracing::debug!("HTTP: Allow overwrite: {}", allow_overwrite);
    
    while let Some(part) = form.try_next().await.map_err(|e| {
        tracing::error!("HTTP: Failed to read multipart form: {}", e);
        warp::reject::reject()
    })? {
        tracing::debug!("HTTP: Processing form part: {}", part.name());
        
        if part.name() == "file" {
            let filename = part.filename()
                .unwrap_or("unknown_file")
                .to_string();
            
            tracing::info!("HTTP: Processing file upload: '{}'", filename);
            
            let file_id = Uuid::new_v4();
            let safe_filename = sanitize_filename(&filename);
            let file_path = get_unique_file_path(&receive_dir, &safe_filename, allow_overwrite);
            
            tracing::info!("HTTP: File ID: {}", file_id);
            tracing::info!("HTTP: Safe filename: '{}'", safe_filename);
            tracing::info!("HTTP: Target file path: {}", file_path.display());
            
            // Ensure the receive directory exists
            tracing::debug!("HTTP: Ensuring receive directory exists: {}", receive_dir.display());
            if let Err(e) = tokio::fs::create_dir_all(&receive_dir).await {
                tracing::error!("HTTP: Failed to create receive directory '{}': {}", receive_dir.display(), e);
                return Err(warp::reject::reject());
            }
            tracing::debug!("HTTP: Receive directory confirmed");
            
            // Write file
            tracing::info!("HTTP: Creating file: {}", file_path.display());
            let mut file = tokio::fs::File::create(&file_path).await
                .map_err(|e| {
                    tracing::error!("HTTP: Failed to create file '{}': {}", file_path.display(), e);
                    warp::reject::reject()
                })?;
            tracing::debug!("HTTP: File created successfully");
            
            let mut size = 0u64;
            let mut stream = part.stream();
            let mut chunk_count = 0;
            
            tracing::info!("HTTP: Starting file data transfer");
            
            while let Some(chunk) = stream.try_next().await.map_err(|e| {
                tracing::error!("HTTP: Failed to read chunk: {}", e);
                warp::reject::reject()
            })? {
                let chunk_bytes = chunk.chunk();
                let chunk_size = chunk_bytes.len() as u64;
                size += chunk_size;
                chunk_count += 1;
                
                tracing::debug!("HTTP: Writing chunk {} ({} bytes, total: {} bytes)", chunk_count, chunk_size, size);
                
                file.write_all(chunk_bytes).await.map_err(|e| {
                    tracing::error!("HTTP: Failed to write chunk to file: {}", e);
                    warp::reject::reject()
                })?;
            }
            
            tracing::info!("HTTP: File data transfer completed. Total size: {} bytes in {} chunks", size, chunk_count);
            
            file.flush().await.map_err(|e| {
                tracing::error!("HTTP: Failed to flush file: {}", e);
                warp::reject::reject()
            })?;
            tracing::debug!("HTTP: File flushed to disk");
            
            // Record received file
            let received_file = ReceivedFile {
                id: file_id,
                original_name: filename.clone(),
                saved_path: file_path.clone(),
                size,
                received_at: std::time::SystemTime::now(),
                sender_info: None,
            };
            
            tracing::info!("HTTP: Recording received file in memory");
            received_files.write().await.push(received_file);
            tracing::debug!("HTTP: File record added to received files list");
            
            tracing::info!("HTTP: ✅ File upload completed successfully!");
            tracing::info!("HTTP:   - Original name: '{}'", filename);
            tracing::info!("HTTP:   - Safe name: '{}'", safe_filename);
            tracing::info!("HTTP:   - Size: {} bytes ({:.2} KB)", size, size as f64 / 1024.0);
            tracing::info!("HTTP:   - Saved to: {}", file_path.display());
            tracing::info!("HTTP:   - File ID: {}", file_id);
            
            return Ok(warp::reply::json(&serde_json::json!({
                "success": true,
                "file_id": file_id,
                "filename": safe_filename,
                "size": size,
                "path": file_path.to_string_lossy()
            })));
        }
    }
    
    Err(warp::reject::reject())
}

async fn handle_status_request(
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
) -> std::result::Result<impl warp::Reply, warp::Rejection> {
    let files = received_files.read().await;
    let total_files = files.len();
    let total_size: u64 = files.iter().map(|f| f.size).sum();
    
    Ok(warp::reply::json(&serde_json::json!({
        "status": "running",
        "total_files": total_files,
        "total_size": total_size,
        "files": files.iter().map(|f| serde_json::json!({
            "id": f.id,
            "name": f.original_name,
            "size": f.size,
            "received_at": f.received_at.duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default().as_secs()
        })).collect::<Vec<_>>()
    })))
}

async fn start_quic_server(
    config: ServerConfig,
    _status: Arc<RwLock<ServerStatus>>,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
) -> Result<()> {
    use quinn::Endpoint;
    
    // Create server configuration
    let server_config = configure_quic_server()?;
    
    let addr: SocketAddr = format!("{}:{}", config.bind_address, config.quic_port)
        .parse()
        .map_err(|e| {
            tracing::error!("Invalid QUIC server address: {}:{} - {}", config.bind_address, config.quic_port, e);
            AeroSyncError::InvalidConfig(format!("Invalid QUIC address: {}", e))
        })?;
    
    let endpoint = Endpoint::server(server_config, addr)
        .map_err(|e| {
            tracing::error!("Failed to create QUIC endpoint: {}", e);
            AeroSyncError::Network(format!("Failed to create QUIC endpoint: {}", e))
        })?;
    
    tracing::info!("QUIC server listening on {}", addr);
    tracing::info!("QUIC server configuration:");
    tracing::info!("  - Receive directory: {}", config.receive_directory.display());
    tracing::info!("  - Max file size: {} MB", config.max_file_size / (1024 * 1024));
    tracing::info!("  - Allow overwrite: {}", config.allow_overwrite);
    tracing::info!("QUIC server is ready to accept connections");
    
    while let Some(conn) = endpoint.accept().await {
        tracing::debug!("QUIC: Incoming connection attempt");
        let connection = conn.await
            .map_err(|e| {
                tracing::error!("QUIC: Connection failed: {}", e);
                AeroSyncError::Network(format!("Connection failed: {}", e))
            })?;
        
        let remote_addr = connection.remote_address();
        tracing::info!("QUIC: New connection accepted from {}", remote_addr);
        
        let receive_dir = config.receive_directory.clone();
        let allow_overwrite = config.allow_overwrite;
        let max_size = config.max_file_size;
        let files = received_files.clone();
        
        tokio::spawn(async move {
            if let Err(e) = handle_quic_connection(connection, receive_dir, allow_overwrite, max_size, files).await {
                tracing::error!("QUIC connection error from {}: {}", remote_addr, e);
            } else {
                tracing::info!("QUIC: Connection from {} completed successfully", remote_addr);
            }
        });
    }
    
    tracing::info!("QUIC server has stopped accepting connections");
    
    Ok(())
}

fn configure_quic_server() -> Result<quinn::ServerConfig> {
    use rustls::{Certificate, PrivateKey, ServerConfig as TlsServerConfig};
    use std::sync::Arc;
    
    // Generate self-signed certificate for development
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])
        .map_err(|e| AeroSyncError::System(format!("Failed to generate certificate: {}", e)))?;
    
    let cert_der = cert.serialize_der()
        .map_err(|e| AeroSyncError::System(format!("Failed to serialize certificate: {}", e)))?;
    
    let key_der = cert.serialize_private_key_der();
    
    let cert_chain = vec![Certificate(cert_der)];
    let private_key = PrivateKey(key_der);
    
    let mut tls_config = TlsServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(cert_chain, private_key)
        .map_err(|e| AeroSyncError::System(format!("TLS config error: {}", e)))?;
    
    tls_config.alpn_protocols = vec![b"aerosync".to_vec()];
    
    let server_config = quinn::ServerConfig::with_crypto(Arc::new(tls_config));
    
    Ok(server_config)
}

async fn handle_quic_connection(
    connection: quinn::Connection,
    receive_dir: PathBuf,
    allow_overwrite: bool,
    max_size: u64,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    
    tracing::info!("QUIC: New connection established from {}", connection.remote_address());
    tracing::debug!("QUIC: Receive directory: {}", receive_dir.display());
    tracing::debug!("QUIC: Allow overwrite: {}", allow_overwrite);
    tracing::debug!("QUIC: Max file size: {} MB", max_size / (1024 * 1024));
    
    while let Ok((mut send_stream, mut recv_stream)) = connection.accept_bi().await {
        tracing::debug!("QUIC: New bidirectional stream opened");
        // Read the request header
        tracing::debug!("QUIC: Reading request header");
        let mut header_buf = [0u8; 1024];
        let header_len = recv_stream.read(&mut header_buf).await
            .map_err(|e| {
                tracing::error!("QUIC: Failed to read header: {}", e);
                AeroSyncError::Network(format!("Failed to read header: {}", e))
            })?
            .unwrap_or(0);
        
        let header = String::from_utf8_lossy(&header_buf[..header_len]);
        tracing::info!("QUIC: Received header: '{}'", header.trim());
        
        if header.starts_with("UPLOAD:") {
            tracing::info!("QUIC: Processing upload request");
            let parts: Vec<&str> = header.splitn(3, ':').collect();
            if parts.len() >= 3 {
                let filename = parts[1];
                let file_size: u64 = parts[2].trim().parse().unwrap_or(0);
                
                tracing::info!("QUIC: Upload request details:");
                tracing::info!("QUIC:   - Filename: '{}'", filename);
                tracing::info!("QUIC:   - File size: {} bytes ({:.2} KB)", file_size, file_size as f64 / 1024.0);
                
                if file_size > max_size {
                    let error_msg = format!("File too large: {} > {}", file_size, max_size);
                    tracing::warn!("QUIC: {}", error_msg);
                    let _ = send_stream.write_all(format!("ERROR:{}", error_msg).as_bytes()).await;
                    continue;
                }
                
                tracing::info!("QUIC: File size check passed");
                
                // Handle file upload
                tracing::info!("QUIC: Starting file upload process");
                match handle_quic_file_upload(
                    &mut recv_stream,
                    &mut send_stream,
                    filename,
                    file_size,
                    &receive_dir,
                    allow_overwrite,
                    received_files.clone(),
                ).await {
                    Ok(_) => {
                        tracing::info!("QUIC: ✅ File upload completed successfully");
                        let _ = send_stream.write_all(b"SUCCESS").await;
                    }
                    Err(e) => {
                        let error_msg = format!("Upload failed: {}", e);
                        tracing::error!("QUIC: ❌ {}", error_msg);
                        let _ = send_stream.write_all(format!("ERROR:{}", error_msg).as_bytes()).await;
                    }
                }
            } else {
                tracing::warn!("QUIC: Invalid upload header format: '{}'", header.trim());
            }
        } else {
            tracing::warn!("QUIC: Unknown request type: '{}'", header.trim());
        }
        
        let _ = send_stream.finish().await;
    }
    
    Ok(())
}

async fn handle_quic_file_upload(
    recv_stream: &mut quinn::RecvStream,
    _send_stream: &mut quinn::SendStream,
    filename: &str,
    expected_size: u64,
    receive_dir: &PathBuf,
    allow_overwrite: bool,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    
    tracing::info!("QUIC: Processing file upload: '{}'", filename);
    
    let file_id = Uuid::new_v4();
    let safe_filename = sanitize_filename(filename);
    let file_path = get_unique_file_path(receive_dir, &safe_filename, allow_overwrite);
    
    tracing::info!("QUIC: File ID: {}", file_id);
    tracing::info!("QUIC: Safe filename: '{}'", safe_filename);
    tracing::info!("QUIC: Target file path: {}", file_path.display());
    tracing::info!("QUIC: Expected size: {} bytes", expected_size);
    
    // Ensure the receive directory exists
    tracing::debug!("QUIC: Ensuring receive directory exists: {}", receive_dir.display());
    tokio::fs::create_dir_all(receive_dir).await?;
    tracing::debug!("QUIC: Receive directory confirmed");
    
    // Create and write file
    tracing::info!("QUIC: Creating file: {}", file_path.display());
    let mut file = tokio::fs::File::create(&file_path).await?;
    tracing::debug!("QUIC: File created successfully");
    let mut buffer = vec![0u8; 64 * 1024]; // 64KB buffer
    let mut total_received = 0u64;
    let mut chunk_count = 0;
    
    tracing::info!("QUIC: Starting file data transfer");
    
    loop {
        match recv_stream.read(&mut buffer).await {
            Ok(Some(bytes_read)) => {
                total_received += bytes_read as u64;
                chunk_count += 1;
                
                tracing::debug!("QUIC: Received chunk {} ({} bytes, total: {} bytes, progress: {:.1}%)", 
                    chunk_count, bytes_read, total_received, 
                    (total_received as f64 / expected_size as f64) * 100.0);
                
                file.write_all(&buffer[..bytes_read]).await?;
                
                if total_received >= expected_size {
                    tracing::info!("QUIC: Expected file size reached, stopping transfer");
                    break;
                }
            }
            Ok(None) => {
                tracing::info!("QUIC: Stream ended, transfer complete");
                break; // Stream ended
            }
            Err(e) => {
                tracing::error!("QUIC: Read error: {}", e);
                return Err(AeroSyncError::Network(format!("Read error: {}", e)));
            }
        }
    }
    
    tracing::info!("QUIC: File data transfer completed. Received: {} bytes in {} chunks", total_received, chunk_count);
    
    tracing::debug!("QUIC: Flushing file to disk");
    file.flush().await?;
    tracing::debug!("QUIC: File flushed successfully");
    
    // Verify file size
    if total_received != expected_size {
        tracing::warn!("QUIC: File size mismatch! Expected: {} bytes, Received: {} bytes", 
            expected_size, total_received);
    }
    
    // Record received file
    let received_file = ReceivedFile {
        id: file_id,
        original_name: filename.to_string(),
        saved_path: file_path.clone(),
        size: total_received,
        received_at: std::time::SystemTime::now(),
        sender_info: None,
    };
    
    tracing::info!("QUIC: Recording received file in memory");
    received_files.write().await.push(received_file);
    tracing::debug!("QUIC: File record added to received files list");
    
    tracing::info!("QUIC: ✅ File upload completed successfully!");
    tracing::info!("QUIC:   - Original name: '{}'", filename);
    tracing::info!("QUIC:   - Safe name: '{}'", safe_filename);
    tracing::info!("QUIC:   - Size: {} bytes ({:.2} KB)", total_received, total_received as f64 / 1024.0);
    tracing::info!("QUIC:   - Saved to: {}", file_path.display());
    tracing::info!("QUIC:   - File ID: {}", file_id);
    
    Ok(())
}

fn sanitize_filename(filename: &str) -> String {
    filename
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn get_unique_file_path(receive_dir: &PathBuf, safe_filename: &str, allow_overwrite: bool) -> PathBuf {
    let mut file_path = receive_dir.join(safe_filename);
    
    tracing::debug!("Generating file path for: '{}'", safe_filename);
    tracing::debug!("Initial path: {}", file_path.display());
    tracing::debug!("Allow overwrite: {}", allow_overwrite);
    
    if !allow_overwrite && file_path.exists() {
        tracing::info!("File already exists, generating unique name");
        let stem = file_path.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let ext = file_path.extension().unwrap_or_default().to_string_lossy().to_string();
        let mut counter = 1;
        
        loop {
            let new_name = if ext.is_empty() {
                format!("{}_{}", stem, counter)
            } else {
                format!("{}_{}.{}", stem, counter, ext)
            };
            
            file_path = receive_dir.join(&new_name);
            tracing::debug!("Trying path: {}", file_path.display());
            
            if !file_path.exists() {
                tracing::info!("Found unique filename: '{}'", new_name);
                break;
            }
            counter += 1;
            
            if counter > 1000 {
                tracing::warn!("Too many file name collisions, using counter {}", counter);
                break;
            }
        }
    } else if file_path.exists() {
        tracing::info!("File exists but overwrite is allowed");
    } else {
        tracing::debug!("File does not exist, using original name");
    }
    
    tracing::info!("Final file path: {}", file_path.display());
    file_path
}
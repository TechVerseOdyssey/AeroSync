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
        
        // Ensure receive directory exists
        tokio::fs::create_dir_all(&config.receive_directory).await?;
        
        *self.status.write().await = ServerStatus::Starting;
        
        // Start HTTP server if enabled
        if config.enable_http {
            let http_config = config.clone();
            let status = Arc::clone(&self.status);
            let received_files = Arc::clone(&self.received_files);
            
            let handle = tokio::spawn(async move {
                if let Err(e) = start_http_server(http_config, status.clone(), received_files).await {
                    tracing::error!("HTTP server error: {}", e);
                    *status.write().await = ServerStatus::Error(e.to_string());
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
                if let Err(e) = start_quic_server(quic_config, status.clone(), received_files).await {
                    tracing::error!("QUIC server error: {}", e);
                    *status.write().await = ServerStatus::Error(e.to_string());
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
        *self.status.write().await = ServerStatus::Stopped;
        
        if let Some(handle) = self.http_handle.take() {
            handle.abort();
        }
        
        if let Some(handle) = self.quic_handle.take() {
            handle.abort();
        }
        
        tracing::info!("File receiver stopped");
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
        .map_err(|e| AeroSyncError::InvalidConfig(format!("Invalid address: {}", e)))?;

    tracing::info!("Starting HTTP server on {}", addr);
    
    warp::serve(routes)
        .run(addr)
        .await;

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
    
    while let Some(part) = form.try_next().await.map_err(|_| warp::reject::reject())? {
        if part.name() == "file" {
            let filename = part.filename()
                .unwrap_or("unknown_file")
                .to_string();
            
            let file_id = Uuid::new_v4();
            let safe_filename = sanitize_filename(&filename);
            let file_path = get_unique_file_path(&receive_dir, &safe_filename, allow_overwrite);
            
            // Write file
            let mut file = tokio::fs::File::create(&file_path).await
                .map_err(|_| warp::reject::reject())?;
            
            let mut size = 0u64;
            let mut stream = part.stream();
            
            while let Some(chunk) = stream.try_next().await.map_err(|_| warp::reject::reject())? {
                let chunk_bytes = chunk.chunk();
                size += chunk_bytes.len() as u64;
                file.write_all(chunk_bytes).await.map_err(|_| warp::reject::reject())?;
            }
            
            file.flush().await.map_err(|_| warp::reject::reject())?;
            
            // Record received file
            let received_file = ReceivedFile {
                id: file_id,
                original_name: filename,
                saved_path: file_path.clone(),
                size,
                received_at: std::time::SystemTime::now(),
                sender_info: None,
            };
            
            received_files.write().await.push(received_file);
            
            tracing::info!("Received file: {} ({} bytes) -> {}", 
                          safe_filename, size, file_path.display());
            
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
        .map_err(|e| AeroSyncError::InvalidConfig(format!("Invalid QUIC address: {}", e)))?;
    
    let endpoint = Endpoint::server(server_config, addr)
        .map_err(|e| AeroSyncError::Network(format!("Failed to create QUIC endpoint: {}", e)))?;
    
    tracing::info!("QUIC server listening on {}", addr);
    
    while let Some(conn) = endpoint.accept().await {
        let connection = conn.await
            .map_err(|e| AeroSyncError::Network(format!("Connection failed: {}", e)))?;
        
        let receive_dir = config.receive_directory.clone();
        let allow_overwrite = config.allow_overwrite;
        let max_size = config.max_file_size;
        let files = received_files.clone();
        
        tokio::spawn(async move {
            if let Err(e) = handle_quic_connection(connection, receive_dir, allow_overwrite, max_size, files).await {
                tracing::error!("QUIC connection error: {}", e);
            }
        });
    }
    
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
    
    while let Ok((mut send_stream, mut recv_stream)) = connection.accept_bi().await {
        // Read the request header
        let mut header_buf = [0u8; 1024];
        let header_len = recv_stream.read(&mut header_buf).await
            .map_err(|e| AeroSyncError::Network(format!("Failed to read header: {}", e)))?
            .unwrap_or(0);
        
        let header = String::from_utf8_lossy(&header_buf[..header_len]);
        
        if header.starts_with("UPLOAD:") {
            let parts: Vec<&str> = header.splitn(3, ':').collect();
            if parts.len() >= 3 {
                let filename = parts[1];
                let file_size: u64 = parts[2].trim().parse().unwrap_or(0);
                
                if file_size > max_size {
                    let error_msg = format!("File too large: {} > {}", file_size, max_size);
                    let _ = send_stream.write_all(format!("ERROR:{}", error_msg).as_bytes()).await;
                    continue;
                }
                
                // Handle file upload
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
                        let _ = send_stream.write_all(b"SUCCESS").await;
                    }
                    Err(e) => {
                        let error_msg = format!("Upload failed: {}", e);
                        let _ = send_stream.write_all(format!("ERROR:{}", error_msg).as_bytes()).await;
                    }
                }
            }
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
    use tokio::io::AsyncWriteExt;
    
    let file_id = Uuid::new_v4();
    let safe_filename = sanitize_filename(filename);
    let file_path = get_unique_file_path(receive_dir, &safe_filename, allow_overwrite);
    
    // Create and write file
    let mut file = tokio::fs::File::create(&file_path).await?;
    let mut buffer = vec![0u8; 64 * 1024]; // 64KB buffer
    let mut total_received = 0u64;
    
    loop {
        match recv_stream.read(&mut buffer).await {
            Ok(Some(bytes_read)) => {
                total_received += bytes_read as u64;
                file.write_all(&buffer[..bytes_read]).await?;
                
                if total_received >= expected_size {
                    break;
                }
            }
            Ok(None) => break, // Stream ended
            Err(e) => return Err(AeroSyncError::Network(format!("Read error: {}", e))),
        }
    }
    
    file.flush().await?;
    
    // Record received file
    let received_file = ReceivedFile {
        id: file_id,
        original_name: filename.to_string(),
        saved_path: file_path.clone(),
        size: total_received,
        received_at: std::time::SystemTime::now(),
        sender_info: None,
    };
    
    received_files.write().await.push(received_file);
    
    tracing::info!("QUIC: Received file {} ({} bytes) -> {}", 
                  safe_filename, total_received, file_path.display());
    
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
    
    if !allow_overwrite && file_path.exists() {
        let stem = file_path.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let ext = file_path.extension().unwrap_or_default().to_string_lossy().to_string();
        let mut counter = 1;
        
        loop {
            let new_name = if ext.is_empty() {
                format!("{}_{}", stem, counter)
            } else {
                format!("{}_{}.{}", stem, counter, ext)
            };
            
            file_path = receive_dir.join(new_name);
            if !file_path.exists() {
                break;
            }
            counter += 1;
        }
    }
    
    file_path
}
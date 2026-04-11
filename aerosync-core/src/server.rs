use crate::auth::{AuthConfig, AuthManager, AuthMiddleware};
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
    pub max_file_size: u64,
    pub allow_overwrite: bool,
    pub enable_http: bool,
    pub enable_quic: bool,
    /// 认证配置，None 表示不启用认证
    pub auth: Option<AuthConfig>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            http_port: 7788,
            quic_port: 7789,
            bind_address: "0.0.0.0".to_string(),
            receive_directory: PathBuf::from("./received"),
            max_file_size: 100 * 1024 * 1024 * 1024, // 100GB
            allow_overwrite: false,
            enable_http: true,
            enable_quic: true,
            auth: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceivedFile {
    pub id: Uuid,
    pub original_name: String,
    pub saved_path: PathBuf,
    pub size: u64,
    pub sha256: Option<String>,
    pub received_at: std::time::SystemTime,
    pub sender_ip: Option<String>,
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
        let config = self.config.read().await.clone();

        tracing::info!(
            "Starting file receiver: HTTP={} QUIC={} dir={}",
            config.http_port,
            config.quic_port,
            config.receive_directory.display()
        );

        tokio::fs::create_dir_all(&config.receive_directory).await?;
        *self.status.write().await = ServerStatus::Starting;

        // 构建 AuthManager（如果配置了认证）
        let auth_manager = config
            .auth
            .clone()
            .map(|auth_cfg| {
                AuthManager::new(auth_cfg)
                    .map(|m| Arc::new(m))
                    .map_err(|e| tracing::warn!("Auth init failed: {}", e))
                    .ok()
            })
            .flatten();

        if config.enable_http {
            let http_cfg = config.clone();
            let status = Arc::clone(&self.status);
            let received_files = Arc::clone(&self.received_files);
            let auth = auth_manager.clone();

            let handle = tokio::spawn(async move {
                if let Err(e) =
                    start_http_server(http_cfg, status.clone(), received_files, auth).await
                {
                    tracing::error!("HTTP server error: {}", e);
                    *status.write().await = ServerStatus::Error(e.to_string());
                }
            });
            self.http_handle = Some(handle);
        }

        if config.enable_quic {
            let quic_cfg = config.clone();
            let status = Arc::clone(&self.status);
            let received_files = Arc::clone(&self.received_files);
            let auth = auth_manager.clone();

            let handle = tokio::spawn(async move {
                if let Err(e) =
                    start_quic_server(quic_cfg, status.clone(), received_files, auth).await
                {
                    tracing::error!("QUIC server error: {}", e);
                    *status.write().await = ServerStatus::Error(e.to_string());
                }
            });
            self.quic_handle = Some(handle);
        }

        *self.status.write().await = ServerStatus::Running;
        tracing::info!(
            "File receiver started on HTTP:{} QUIC:{}",
            config.http_port,
            config.quic_port
        );
        Ok(())
    }

    pub async fn stop(&mut self) -> Result<()> {
        *self.status.write().await = ServerStatus::Stopped;
        if let Some(h) = self.http_handle.take() {
            h.abort();
        }
        if let Some(h) = self.quic_handle.take() {
            h.abort();
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

    pub async fn get_server_urls(&self) -> Vec<String> {
        let config = self.config.read().await;
        let host = if config.bind_address == "0.0.0.0" {
            "localhost"
        } else {
            &config.bind_address
        };
        let mut urls = Vec::new();
        if config.enable_http {
            urls.push(format!("http://{}:{}/upload", host, config.http_port));
        }
        if config.enable_quic {
            urls.push(format!("quic://{}:{}", host, config.quic_port));
        }
        urls
    }
}

// ─────────────────────────────── HTTP server ────────────────────────────────

async fn start_http_server(
    config: ServerConfig,
    status: Arc<RwLock<ServerStatus>>,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    auth_manager: Option<Arc<AuthManager>>,
) -> Result<()> {
    use warp::Filter;

    let receive_dir = config.receive_directory.clone();
    let max_size = config.max_file_size;
    let allow_overwrite = config.allow_overwrite;

    // 认证中间件（可选）
    let auth_mw = auth_manager.map(|m| Arc::new(AuthMiddleware::new(m)));

    // ── POST /upload ────────────────────────────────────────────────────────
    let auth_mw_upload = auth_mw.clone();
    let received_files_upload = received_files.clone();
    let receive_dir_upload = receive_dir.clone();

    let upload = warp::path("upload")
        .and(warp::post())
        .and(warp::header::optional::<String>("authorization"))
        .and(warp::header::optional::<String>("x-file-hash"))
        .and(warp::addr::remote())
        .and(warp::multipart::form().max_length(max_size))
        .and(warp::any().map(move || receive_dir_upload.clone()))
        .and(warp::any().map(move || allow_overwrite))
        .and(warp::any().map(move || received_files_upload.clone()))
        .and(warp::any().map(move || auth_mw_upload.clone()))
        .and_then(handle_file_upload);

    // ── GET /health ──────────────────────────────────────────────────────────
    let received_files_health = received_files.clone();
    let health = warp::path("health")
        .and(warp::get())
        .and(warp::any().map(move || received_files_health.clone()))
        .and_then(handle_health);

    // ── GET /status ──────────────────────────────────────────────────────────
    let status_route = warp::path("status")
        .and(warp::get())
        .and(warp::any().map(move || received_files.clone()))
        .and_then(handle_status_request);

    let cors = warp::cors()
        .allow_any_origin()
        .allow_headers(vec!["content-type", "authorization", "x-file-hash"])
        .allow_methods(vec!["GET", "POST", "OPTIONS"]);

    let routes = upload.or(health).or(status_route).with(cors);

    let addr: SocketAddr = format!("{}:{}", config.bind_address, config.http_port)
        .parse()
        .map_err(|e| AeroSyncError::InvalidConfig(format!("Invalid address: {}", e)))?;

    tracing::info!("HTTP server listening on {}", addr);
    warp::serve(routes).run(addr).await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_file_upload(
    auth_header: Option<String>,
    expected_hash: Option<String>,
    remote_addr: Option<SocketAddr>,
    mut form: warp::multipart::FormData,
    receive_dir: PathBuf,
    allow_overwrite: bool,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    auth_mw: Option<Arc<AuthMiddleware>>,
) -> std::result::Result<warp::reply::Response, warp::Rejection> {
    use bytes::Buf;
    use futures::TryStreamExt;
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncWriteExt;
    use warp::Reply;

    let client_ip = remote_addr
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // ── 认证 ──────────────────────────────────────────────────────────────────
    if let Some(ref mw) = auth_mw {
        let auth_str = auth_header.as_deref();
        match mw.authenticate_http_request(auth_str, &client_ip) {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!("HTTP: Unauthorized upload attempt from {}", client_ip);
                let resp = mw.unauthorized_response();
                return Ok(warp::reply::with_status(
                    warp::reply::json(&serde_json::json!({
                        "error": resp.message
                    })),
                    warp::http::StatusCode::UNAUTHORIZED,
                ).into_response());
            }
            Err(e) => {
                tracing::error!("HTTP: Auth error: {}", e);
                return Err(warp::reject::reject());
            }
        }
    }

    // ── 接收文件 ──────────────────────────────────────────────────────────────
    while let Some(part) = form.try_next().await.map_err(|_| warp::reject::reject())? {
        if part.name() != "file" {
            continue;
        }

        let filename = part.filename().unwrap_or("unknown").to_string();
        let file_id = Uuid::new_v4();
        let safe_name = sanitize_filename(&filename);
        let file_path = get_unique_file_path(&receive_dir, &safe_name, allow_overwrite);

        tokio::fs::create_dir_all(&receive_dir)
            .await
            .map_err(|_| warp::reject::reject())?;

        let mut file = tokio::fs::File::create(&file_path)
            .await
            .map_err(|_| warp::reject::reject())?;

        let mut size = 0u64;
        let mut hasher = Sha256::new();
        let mut stream = part.stream();

        while let Some(chunk) = stream.try_next().await.map_err(|_| warp::reject::reject())? {
            let data = chunk.chunk();
            hasher.update(data);
            size += data.len() as u64;
            file.write_all(data)
                .await
                .map_err(|_| warp::reject::reject())?;
        }
        file.flush().await.map_err(|_| warp::reject::reject())?;

        let actual_hash = hex::encode(hasher.finalize());

        // ── SHA-256 校验 ─────────────────────────────────────────────────────
        if let Some(ref expected) = expected_hash {
            if &actual_hash != expected {
                tracing::error!(
                    "HTTP: Hash mismatch for '{}': expected={} actual={}",
                    filename,
                    expected,
                    actual_hash
                );
                let _ = tokio::fs::remove_file(&file_path).await;
                use warp::Reply;
                return Ok(warp::reply::with_status(
                    warp::reply::json(&serde_json::json!({
                        "error": "SHA-256 mismatch",
                        "expected": expected,
                        "actual": actual_hash
                    })),
                    warp::http::StatusCode::BAD_REQUEST,
                ).into_response());
            }
        }

        let received_file = ReceivedFile {
            id: file_id,
            original_name: filename.clone(),
            saved_path: file_path.clone(),
            size,
            sha256: Some(actual_hash.clone()),
            received_at: std::time::SystemTime::now(),
            sender_ip: Some(client_ip.clone()),
        };
        received_files.write().await.push(received_file);

        tracing::info!(
            "HTTP: Received '{}' ({} bytes) sha256={} from {}",
            filename,
            size,
            &actual_hash[..8],
            client_ip
        );

        use warp::Reply;
        return Ok(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({
                "success": true,
                "file_id": file_id,
                "filename": safe_name,
                "size": size,
                "sha256": actual_hash,
            })),
            warp::http::StatusCode::OK,
        ).into_response());
    }

    Err(warp::reject::reject())
}

async fn handle_health(
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
) -> std::result::Result<impl warp::Reply, warp::Rejection> {
    let count = received_files.read().await.len();
    Ok(warp::reply::json(&serde_json::json!({
        "status": "ok",
        "received_files": count,
    })))
}

async fn handle_status_request(
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
) -> std::result::Result<impl warp::Reply, warp::Rejection> {
    let files = received_files.read().await;
    let total_size: u64 = files.iter().map(|f| f.size).sum();
    Ok(warp::reply::json(&serde_json::json!({
        "status": "running",
        "total_files": files.len(),
        "total_size": total_size,
        "files": files.iter().map(|f| serde_json::json!({
            "id": f.id,
            "name": f.original_name,
            "size": f.size,
            "sha256": f.sha256,
            "received_at": f.received_at
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        })).collect::<Vec<_>>()
    })))
}

// ─────────────────────────────── QUIC server ────────────────────────────────

async fn start_quic_server(
    config: ServerConfig,
    status: Arc<RwLock<ServerStatus>>,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    auth_manager: Option<Arc<AuthManager>>,
) -> Result<()> {
    use quinn::Endpoint;

    let server_config = configure_quic_server()?;
    let addr: SocketAddr = format!("{}:{}", config.bind_address, config.quic_port)
        .parse()
        .map_err(|e| AeroSyncError::InvalidConfig(format!("Invalid QUIC address: {}", e)))?;

    let endpoint = Endpoint::server(server_config, addr)
        .map_err(|e| AeroSyncError::Network(format!("Failed to create QUIC endpoint: {}", e)))?;

    tracing::info!("QUIC server listening on {}", addr);

    while let Some(conn) = endpoint.accept().await {
        let connection = match conn.await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("QUIC: Connection handshake failed: {}", e);
                continue;
            }
        };

        let remote = connection.remote_address();
        let receive_dir = config.receive_directory.clone();
        let allow_overwrite = config.allow_overwrite;
        let max_size = config.max_file_size;
        let files = received_files.clone();
        let auth = auth_manager.clone();
        let _status = status.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_quic_connection(
                connection,
                receive_dir,
                allow_overwrite,
                max_size,
                files,
                auth,
            )
            .await
            {
                tracing::error!("QUIC connection error from {}: {}", remote, e);
            }
        });
    }

    Ok(())
}

fn configure_quic_server() -> Result<quinn::ServerConfig> {
    use rustls::{Certificate, PrivateKey, ServerConfig as TlsServerConfig};

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])
        .map_err(|e| AeroSyncError::System(format!("Failed to generate certificate: {}", e)))?;

    let cert_der = cert
        .serialize_der()
        .map_err(|e| AeroSyncError::System(format!("Failed to serialize certificate: {}", e)))?;
    let key_der = cert.serialize_private_key_der();

    let mut tls_config = TlsServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(vec![Certificate(cert_der)], PrivateKey(key_der))
        .map_err(|e| AeroSyncError::System(format!("TLS config error: {}", e)))?;

    tls_config.alpn_protocols = vec![b"aerosync".to_vec()];

    Ok(quinn::ServerConfig::with_crypto(Arc::new(tls_config)))
}

async fn handle_quic_connection(
    connection: quinn::Connection,
    receive_dir: PathBuf,
    allow_overwrite: bool,
    max_size: u64,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    auth_manager: Option<Arc<AuthManager>>,
) -> Result<()> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncWriteExt;

    let remote_ip = connection.remote_address().ip().to_string();

    // QUIC 连接层认证：读取第一条消息作为 Token
    if let Some(ref auth) = auth_manager {
        // Auth will be validated per-stream below
        tracing::debug!("QUIC: Auth enabled for connection from {}", remote_ip);
    }

    while let Ok((mut send, mut recv)) = connection.accept_bi().await {
        let mut header_buf = vec![0u8; 4096];
        let header_len = recv
            .read(&mut header_buf)
            .await
            .map_err(|e| AeroSyncError::Network(e.to_string()))?
            .unwrap_or(0);

        let header = String::from_utf8_lossy(&header_buf[..header_len]);
        let header_str = header.trim_end_matches('\n').trim_end_matches('\r');

        // 格式: UPLOAD:<filename>:<size>[:<token>]
        if !header_str.starts_with("UPLOAD:") {
            tracing::warn!("QUIC: Unknown command: {}", &header_str[..header_str.len().min(64)]);
            continue;
        }

        let parts: Vec<&str> = header_str.splitn(5, ':').collect();
        if parts.len() < 3 {
            tracing::warn!("QUIC: Malformed UPLOAD header");
            continue;
        }

        let filename = parts[1];
        let file_size: u64 = parts[2].trim().parse().unwrap_or(0);
        let token = parts.get(3).map(|t| *t);

        // 认证（若启用）
        if let Some(ref auth) = auth_manager {
            let auth_mw = AuthMiddleware::new(Arc::clone(auth));
            let token_header = token.map(|t| format!("Bearer {}", t));
            match auth_mw.authenticate_http_request(token_header.as_deref(), &remote_ip) {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!("QUIC: Unauthorized from {}", remote_ip);
                    let _ = send.write_all(b"ERROR:Unauthorized").await;
                    let _ = send.finish().await;
                    continue;
                }
                Err(e) => {
                    tracing::error!("QUIC: Auth error: {}", e);
                    continue;
                }
            }
        }

        if file_size > max_size {
            let _ = send
                .write_all(format!("ERROR:File too large: {}", file_size).as_bytes())
                .await;
            let _ = send.finish().await;
            continue;
        }

        // 找到 header 结束位置，保存 header 后的初始数据
        let header_end_in_buf = header_buf[..header_len]
            .iter()
            .position(|&b| b == b'\n')
            .map(|p| p + 1)
            .unwrap_or(header_len);
        let initial_data = if header_end_in_buf < header_len {
            Some(header_buf[header_end_in_buf..header_len].to_vec())
        } else {
            None
        };

        match handle_quic_file_upload(
            &mut recv,
            filename,
            file_size,
            &receive_dir,
            allow_overwrite,
            received_files.clone(),
            initial_data,
            &remote_ip,
        )
        .await
        {
            Ok(_) => {
                let _ = send.write_all(b"SUCCESS").await;
                tracing::info!("QUIC: Sent SUCCESS response for '{}'", filename);
            }
            Err(e) => {
                let _ = send
                    .write_all(format!("ERROR:{}", e).as_bytes())
                    .await;
            }
        }
        let _ = send.finish().await;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_quic_file_upload(
    recv: &mut quinn::RecvStream,
    filename: &str,
    expected_size: u64,
    receive_dir: &PathBuf,
    allow_overwrite: bool,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    initial_data: Option<Vec<u8>>,
    sender_ip: &str,
) -> Result<()> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncWriteExt;

    let file_id = Uuid::new_v4();
    let safe_name = sanitize_filename(filename);
    let file_path = get_unique_file_path(receive_dir, &safe_name, allow_overwrite);

    tokio::fs::create_dir_all(receive_dir).await?;
    let mut file = tokio::fs::File::create(&file_path).await?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;

    // 先写入 header buffer 中剩余的初始数据
    if let Some(data) = initial_data {
        if !data.is_empty() {
            hasher.update(&data);
            file.write_all(&data).await?;
            total += data.len() as u64;
        }
    }

    let mut buf = vec![0u8; 64 * 1024];
    while total < expected_size {
        match recv.read(&mut buf).await {
            Ok(Some(n)) => {
                hasher.update(&buf[..n]);
                file.write_all(&buf[..n]).await?;
                total += n as u64;
            }
            Ok(None) => break,
            Err(e) => return Err(AeroSyncError::Network(e.to_string())),
        }
    }
    file.flush().await?;

    let actual_hash = hex::encode(hasher.finalize());

    if total != expected_size {
        tracing::warn!(
            "QUIC: Size mismatch for '{}': expected={} actual={}",
            filename,
            expected_size,
            total
        );
    }

    received_files.write().await.push(ReceivedFile {
        id: file_id,
        original_name: filename.to_string(),
        saved_path: file_path.clone(),
        size: total,
        sha256: Some(actual_hash.clone()),
        received_at: std::time::SystemTime::now(),
        sender_ip: Some(sender_ip.to_string()),
    });

    tracing::info!(
        "QUIC: Received '{}' ({} bytes) sha256={} from {}",
        filename,
        total,
        &actual_hash[..8],
        sender_ip
    );
    Ok(())
}

// ─────────────────────────────── helpers ────────────────────────────────────

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

fn get_unique_file_path(
    receive_dir: &PathBuf,
    safe_name: &str,
    allow_overwrite: bool,
) -> PathBuf {
    let mut path = receive_dir.join(safe_name);
    if allow_overwrite || !path.exists() {
        return path;
    }
    let stem = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let ext = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    for i in 1..=9999 {
        let new_name = if ext.is_empty() {
            format!("{}_{}", stem, i)
        } else {
            format!("{}_{}.{}", stem, i, ext)
        };
        path = receive_dir.join(new_name);
        if !path.exists() {
            break;
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── sanitize_filename ─────────────────────────────────────────────────────

    #[test]
    fn test_sanitize_filename_normal() {
        assert_eq!(sanitize_filename("hello-world_v1.2.bin"), "hello-world_v1.2.bin");
    }

    #[test]
    fn test_sanitize_filename_replaces_spaces() {
        let result = sanitize_filename("my file name.txt");
        assert_eq!(result, "my_file_name.txt");
    }

    #[test]
    fn test_sanitize_filename_replaces_slashes() {
        let result = sanitize_filename("path/to/file.bin");
        assert_eq!(result, "path_to_file.bin");
    }

    #[test]
    fn test_sanitize_filename_preserves_alphanumeric() {
        let result = sanitize_filename("ABC123.tar.gz");
        assert_eq!(result, "ABC123.tar.gz");
    }

    // ── get_unique_file_path ──────────────────────────────────────────────────

    #[test]
    fn test_get_unique_path_no_collision() {
        let dir = TempDir::new().unwrap();
        let path = get_unique_file_path(&dir.path().to_path_buf(), "new.bin", false);
        assert_eq!(path, dir.path().join("new.bin"));
    }

    #[test]
    fn test_get_unique_path_with_collision_appends_counter() {
        let dir = TempDir::new().unwrap();
        // Create the file so it exists
        std::fs::write(dir.path().join("existing.bin"), b"data").unwrap();
        let path = get_unique_file_path(&dir.path().to_path_buf(), "existing.bin", false);
        assert_eq!(path, dir.path().join("existing_1.bin"));
    }

    #[test]
    fn test_get_unique_path_overwrite_returns_original() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("file.bin"), b"data").unwrap();
        let path = get_unique_file_path(&dir.path().to_path_buf(), "file.bin", true);
        assert_eq!(path, dir.path().join("file.bin"));
    }

    #[test]
    fn test_get_unique_path_multiple_collisions() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("data.bin"), b"a").unwrap();
        std::fs::write(dir.path().join("data_1.bin"), b"b").unwrap();
        let path = get_unique_file_path(&dir.path().to_path_buf(), "data.bin", false);
        assert_eq!(path, dir.path().join("data_2.bin"));
    }

    // ── ServerConfig ─────────────────────────────────────────────────────────

    #[test]
    fn test_server_config_default() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.http_port, 7788);
        assert_eq!(cfg.quic_port, 7789);
        assert_eq!(cfg.bind_address, "0.0.0.0");
        assert!(cfg.enable_http);
        assert!(cfg.enable_quic);
        assert!(!cfg.allow_overwrite);
        assert!(cfg.auth.is_none());
    }

    // ── FileReceiver lifecycle ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_receiver_initial_status_is_stopped() {
        let cfg = ServerConfig::default();
        let receiver = FileReceiver::new(cfg);
        assert!(matches!(receiver.get_status().await, ServerStatus::Stopped));
    }

    #[tokio::test]
    async fn test_receiver_get_config() {
        let mut cfg = ServerConfig::default();
        cfg.http_port = 9999;
        let receiver = FileReceiver::new(cfg);
        assert_eq!(receiver.get_config().await.http_port, 9999);
    }

    #[tokio::test]
    async fn test_receiver_update_config() {
        let cfg = ServerConfig::default();
        let receiver = FileReceiver::new(cfg);
        let mut new_cfg = ServerConfig::default();
        new_cfg.http_port = 8888;
        receiver.update_config(new_cfg).await.unwrap();
        assert_eq!(receiver.get_config().await.http_port, 8888);
    }

    #[tokio::test]
    async fn test_receiver_get_server_urls_http_only() {
        let mut cfg = ServerConfig::default();
        cfg.bind_address = "0.0.0.0".to_string();
        cfg.http_port = 7788;
        cfg.enable_quic = false;
        let receiver = FileReceiver::new(cfg);
        let urls = receiver.get_server_urls().await;
        assert_eq!(urls.len(), 1);
        assert!(urls[0].contains("7788"));
        assert!(urls[0].starts_with("http://"));
    }

    #[tokio::test]
    async fn test_receiver_get_server_urls_both_protocols() {
        let cfg = ServerConfig::default();
        let receiver = FileReceiver::new(cfg);
        let urls = receiver.get_server_urls().await;
        assert_eq!(urls.len(), 2);
    }

    // ── HTTP server integration ───────────────────────────────────────────────

    /// Find a free port for testing
    fn free_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn test_http_server_health_endpoint() {
        let dir = TempDir::new().unwrap();
        let port = free_port();

        let mut cfg = ServerConfig {
            http_port: port,
            bind_address: "127.0.0.1".to_string(),
            receive_directory: dir.path().to_path_buf(),
            enable_quic: false,
            ..ServerConfig::default()
        };

        let mut receiver = FileReceiver::new(cfg);
        receiver.start().await.unwrap();

        // Give server time to start
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{}/health", port))
            .send()
            .await
            .unwrap();

        assert!(resp.status().is_success());
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["received_files"], 0);

        receiver.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_http_server_upload_file() {
        let dir = TempDir::new().unwrap();
        let port = free_port();

        let mut cfg = ServerConfig {
            http_port: port,
            bind_address: "127.0.0.1".to_string(),
            receive_directory: dir.path().to_path_buf(),
            enable_quic: false,
            ..ServerConfig::default()
        };

        let mut receiver = FileReceiver::new(cfg);
        receiver.start().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let content = b"hello from test";
        let part = reqwest::multipart::Part::bytes(content.to_vec())
            .file_name("test_upload.bin")
            .mime_str("application/octet-stream")
            .unwrap();
        let form = reqwest::multipart::Form::new().part("file", part);

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{}/upload", port))
            .multipart(form)
            .send()
            .await
            .unwrap();

        assert!(resp.status().is_success());
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["success"], true);
        assert_eq!(body["size"], content.len());
        assert!(body["sha256"].as_str().is_some());

        // File should exist in receive dir
        let files = receiver.get_received_files().await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].size, content.len() as u64);
        assert!(files[0].sha256.is_some());
        assert!(files[0].saved_path.exists());

        receiver.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_http_server_upload_sha256_mismatch_rejected() {
        let dir = TempDir::new().unwrap();
        let port = free_port();

        let mut cfg = ServerConfig {
            http_port: port,
            bind_address: "127.0.0.1".to_string(),
            receive_directory: dir.path().to_path_buf(),
            enable_quic: false,
            ..ServerConfig::default()
        };

        let mut receiver = FileReceiver::new(cfg);
        receiver.start().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let part = reqwest::multipart::Part::bytes(b"real content".to_vec())
            .file_name("tampered.bin")
            .mime_str("application/octet-stream")
            .unwrap();
        let form = reqwest::multipart::Form::new().part("file", part);

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{}/upload", port))
            .header("X-File-Hash", "0000000000000000000000000000000000000000000000000000000000000000")
            .multipart(form)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 400);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(body["error"].as_str().unwrap().contains("SHA-256"));

        // File should NOT be saved
        let files = receiver.get_received_files().await;
        assert_eq!(files.len(), 0);

        receiver.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_http_server_unauthorized_without_token() {
        let dir = TempDir::new().unwrap();
        let port = free_port();

        let auth_cfg = AuthConfig {
            enable_auth: true,
            secret_key: "test-secret-key-1234567890".to_string(),
            token_lifetime_hours: 24,
            allowed_ips: vec![],
        };

        let cfg = ServerConfig {
            http_port: port,
            bind_address: "127.0.0.1".to_string(),
            receive_directory: dir.path().to_path_buf(),
            enable_quic: false,
            auth: Some(auth_cfg),
            ..ServerConfig::default()
        };

        let mut receiver = FileReceiver::new(cfg);
        receiver.start().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let part = reqwest::multipart::Part::bytes(b"data".to_vec())
            .file_name("file.bin")
            .mime_str("application/octet-stream")
            .unwrap();
        let form = reqwest::multipart::Form::new().part("file", part);

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{}/upload", port))
            .multipart(form)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 401);

        receiver.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_http_server_status_endpoint() {
        let dir = TempDir::new().unwrap();
        let port = free_port();

        let cfg = ServerConfig {
            http_port: port,
            bind_address: "127.0.0.1".to_string(),
            receive_directory: dir.path().to_path_buf(),
            enable_quic: false,
            ..ServerConfig::default()
        };

        let mut receiver = FileReceiver::new(cfg);
        receiver.start().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{}/status", port))
            .send()
            .await
            .unwrap();

        assert!(resp.status().is_success());
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["status"], "running");
        assert_eq!(body["total_files"], 0);

        receiver.stop().await.unwrap();
    }
}

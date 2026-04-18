use crate::core::audit::{AuditLogger, Direction};
use crate::core::auth::{AuthConfig, AuthManager, AuthMiddleware};
use crate::core::discovery::{AeroSyncMdns, MdnsHandle};
use crate::core::metrics::Metrics;
use crate::core::routing::{Router, RouterConfig};
use crate::{AeroSyncError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::{broadcast, RwLock};
use uuid::Uuid;

/// 外部 TLS 证书配置（用于 QUIC 服务端）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    /// PEM 格式证书文件路径
    pub cert_path: PathBuf,
    /// PEM 格式私钥文件路径
    pub key_path: PathBuf,
}

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
    /// 审计日志文件路径，None 表示不记录审计日志
    pub audit_log: Option<PathBuf>,
    /// 外部 TLS 证书，None 时自动生成自签名证书
    pub tls: Option<TlsConfig>,
    /// 启用 Prometheus /metrics 端点
    pub enable_metrics: bool,
    /// 启用 WebSocket /ws 进度推送
    pub enable_ws: bool,
    /// WebSocket 广播通道缓冲大小
    pub ws_event_buffer: usize,
    /// 多目录路由规则（None 表示不启用路由）
    pub routing: Option<RouterConfig>,
    /// 是否启用 HTTPS（自动生成自签名证书，或使用 tls 字段指定外部证书）
    pub enable_https: bool,
    /// HTTPS 监听端口（默认 7790）
    pub https_port: u16,
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
            audit_log: None,
            tls: None,
            enable_metrics: true,
            enable_ws: true,
            ws_event_buffer: 256,
            routing: None,
            enable_https: false,
            https_port: 7790,
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

/// Custom rejection for payload too large (Content-Length exceeds max_file_size)
#[derive(Debug)]
struct PayloadTooLarge;
impl warp::reject::Reject for PayloadTooLarge {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerStatus {
    Stopped,
    Starting,
    Running,
    Error(String),
}

/// WebSocket broadcast event
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WsEvent {
    TransferStarted {
        filename: String,
        size: u64,
        sender_ip: String,
    },
    Progress {
        filename: String,
        bytes: u64,
        total: u64,
    },
    Completed {
        filename: String,
        size: u64,
        sha256: String,
    },
    Failed {
        filename: String,
        reason: String,
    },
}

pub type WsBroadcast = broadcast::Sender<WsEvent>;

pub struct FileReceiver {
    config: Arc<RwLock<ServerConfig>>,
    status: Arc<RwLock<ServerStatus>>,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    http_handle: Option<tokio::task::JoinHandle<()>>,
    https_handle: Option<tokio::task::JoinHandle<()>>,
    quic_handle: Option<tokio::task::JoinHandle<()>>,
    reload_handle: Option<tokio::task::JoinHandle<()>>,
    audit_logger: Option<Arc<AuditLogger>>,
    metrics: Arc<Metrics>,
    ws_tx: WsBroadcast,
    /// 每个 task_id 的分片到达计数器（HTTP+HTTPS 共享）
    chunk_arrivals: ChunkArrivalMap,
    /// mDNS 广播句柄 — Some 表示正在广播，drop 时自动注销
    mdns_handle: Option<MdnsHandle>,
}

impl FileReceiver {
    pub fn new(config: ServerConfig) -> Self {
        let ws_buf = config.ws_event_buffer.max(1);
        let (ws_tx, _) = broadcast::channel(ws_buf);
        Self {
            config: Arc::new(RwLock::new(config)),
            status: Arc::new(RwLock::new(ServerStatus::Stopped)),
            received_files: Arc::new(RwLock::new(Vec::new())),
            http_handle: None,
            https_handle: None,
            quic_handle: None,
            reload_handle: None,
            audit_logger: None,
            metrics: Metrics::new(),
            ws_tx,
            chunk_arrivals: Arc::new(Mutex::new(HashMap::new())),
            mdns_handle: None,
        }
    }

    /// Expose the WebSocket broadcast sender so callers can subscribe
    pub fn ws_sender(&self) -> WsBroadcast {
        self.ws_tx.clone()
    }

    /// Expose shared metrics handle
    pub fn metrics(&self) -> Arc<Metrics> {
        Arc::clone(&self.metrics)
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

        // 构建 AuditLogger（如果配置了审计日志路径）
        let audit_logger: Option<Arc<AuditLogger>> = if let Some(ref log_path) = config.audit_log {
            match AuditLogger::new(log_path).await {
                Ok(logger) => {
                    tracing::info!("Audit log: {}", log_path.display());
                    Some(Arc::new(logger))
                }
                Err(e) => {
                    tracing::warn!("Failed to open audit log {}: {}", log_path.display(), e);
                    None
                }
            }
        } else {
            None
        };
        self.audit_logger = audit_logger.clone();

        // 构建 AuthManager（如果配置了认证）
        let auth_manager = config.auth.clone().and_then(|auth_cfg| {
            AuthManager::new(auth_cfg)
                .map(Arc::new)
                .map_err(|e| tracing::warn!("Auth init failed: {}", e))
                .ok()
        });

        if config.enable_http {
            let http_cfg = config.clone();
            let status = Arc::clone(&self.status);
            let received_files = Arc::clone(&self.received_files);
            let auth = auth_manager.clone();
            let audit_http = audit_logger.clone();
            let metrics_http = Arc::clone(&self.metrics);
            let ws_tx_http = self.ws_tx.clone();
            let chunk_arrivals_http = Arc::clone(&self.chunk_arrivals);

            let handle = tokio::spawn(async move {
                if let Err(e) = start_http_server(
                    http_cfg,
                    status.clone(),
                    received_files,
                    auth,
                    audit_http,
                    metrics_http,
                    ws_tx_http,
                    chunk_arrivals_http,
                )
                .await
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
            let audit_quic = audit_logger.clone();

            let handle = tokio::spawn(async move {
                if let Err(e) =
                    start_quic_server(quic_cfg, status.clone(), received_files, auth, audit_quic)
                        .await
                {
                    tracing::error!("QUIC server error: {}", e);
                    *status.write().await = ServerStatus::Error(e.to_string());
                }
            });
            self.quic_handle = Some(handle);
        }

        if config.enable_https {
            let https_cfg = config.clone();
            let status = Arc::clone(&self.status);
            let received_files = Arc::clone(&self.received_files);
            let auth = auth_manager.clone();
            let audit_https = audit_logger.clone();
            let metrics_https = Arc::clone(&self.metrics);
            let ws_tx_https = self.ws_tx.clone();
            let chunk_arrivals_https = Arc::clone(&self.chunk_arrivals);

            let handle = tokio::spawn(async move {
                if let Err(e) = start_https_server(
                    https_cfg,
                    status.clone(),
                    received_files,
                    auth,
                    audit_https,
                    metrics_https,
                    ws_tx_https,
                    chunk_arrivals_https,
                )
                .await
                {
                    tracing::error!("HTTPS server error: {}", e);
                    *status.write().await = ServerStatus::Error(e.to_string());
                }
            });
            self.https_handle = Some(handle);
        }

        *self.status.write().await = ServerStatus::Running;
        if config.enable_https {
            tracing::info!(
                "File receiver started on HTTP:{} QUIC:{} HTTPS:{}",
                config.http_port,
                config.quic_port,
                config.https_port
            );
        } else {
            tracing::info!(
                "File receiver started on HTTP:{} QUIC:{}",
                config.http_port,
                config.quic_port
            );
        }

        // mDNS 广播（局域网自动发现）
        let instance_name = hostname_for_mdns();
        let auth_required = config.auth.is_some();
        let ws_enabled = config.enable_ws;
        match AeroSyncMdns::register(
            &instance_name,
            config.http_port,
            env!("CARGO_PKG_VERSION"),
            ws_enabled,
            auth_required,
        ) {
            Ok(handle) => {
                self.mdns_handle = Some(handle);
                tracing::info!(
                    "mDNS: broadcasting as '{}' on port {}",
                    instance_name,
                    config.http_port
                );
            }
            Err(e) => {
                tracing::warn!("mDNS broadcast unavailable (non-fatal): {}", e);
            }
        }

        Ok(())
    }

    pub async fn stop(&mut self) -> Result<()> {
        *self.status.write().await = ServerStatus::Stopped;
        if let Some(h) = self.http_handle.take() {
            h.abort();
        }
        if let Some(h) = self.https_handle.take() {
            h.abort();
        }
        if let Some(h) = self.quic_handle.take() {
            h.abort();
        }
        if let Some(h) = self.reload_handle.take() {
            h.abort();
        }
        // drop MdnsHandle → 自动注销 mDNS 广播
        self.mdns_handle = None;
        tracing::info!("File receiver stopped");
        Ok(())
    }

    /// Start watching for SIGHUP to hot-reload config from a TOML file.
    /// Only available on Unix. On other platforms this is a no-op.
    pub fn watch_config_reload(&mut self, config_path: std::path::PathBuf) {
        let config_arc = Arc::clone(&self.config);
        let handle = tokio::spawn(async move {
            watch_config_reload_task(config_arc, config_path).await;
        });
        self.reload_handle = Some(handle);
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
        if config.enable_https {
            urls.push(format!("https://{}:{}/upload", host, config.https_port));
        }
        if config.enable_quic {
            urls.push(format!("quic://{}:{}", host, config.quic_port));
        }
        urls
    }
}

// ─────────────────────────────── config hot-reload ─────────────────────────

/// Fields that can be updated without a restart (used by watch_config_reload_task)
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct HotReloadableConfig {
    max_file_size: Option<u64>,
    allow_overwrite: Option<bool>,
    auth: Option<Option<AuthConfig>>,
    routing: Option<Option<RouterConfig>>,
}

async fn watch_config_reload_task(
    config_arc: Arc<RwLock<ServerConfig>>,
    config_path: std::path::PathBuf,
) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut stream = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Failed to set up SIGHUP handler: {}", e);
                return;
            }
        };
        loop {
            stream.recv().await;
            tracing::info!(
                "SIGHUP received — reloading config from {}",
                config_path.display()
            );
            match tokio::fs::read_to_string(&config_path).await {
                Ok(contents) => {
                    match toml::from_str::<ServerConfig>(&contents) {
                        Ok(new_cfg) => {
                            let mut cfg = config_arc.write().await;
                            // Only apply hot-reloadable fields
                            cfg.max_file_size = new_cfg.max_file_size;
                            cfg.allow_overwrite = new_cfg.allow_overwrite;
                            cfg.auth = new_cfg.auth;
                            cfg.routing = new_cfg.routing;
                            cfg.audit_log = new_cfg.audit_log;
                            // Warn about non-reloadable fields if changed
                            if cfg.http_port != new_cfg.http_port {
                                tracing::warn!("http_port change ignored (requires restart)");
                            }
                            if cfg.quic_port != new_cfg.quic_port {
                                tracing::warn!("quic_port change ignored (requires restart)");
                            }
                            if cfg.bind_address != new_cfg.bind_address {
                                tracing::warn!("bind_address change ignored (requires restart)");
                            }
                            tracing::info!("Config reloaded successfully");
                        }
                        Err(e) => tracing::error!("Config parse error: {}", e),
                    }
                }
                Err(e) => tracing::error!("Failed to read config file: {}", e),
            }
        }
    }
    #[cfg(not(unix))]
    {
        tracing::debug!("Config hot-reload not supported on this platform");
        let _ = config_arc;
        let _ = config_path;
    }
}

// ─────────────────────────────── TLS helpers ────────────────────────────────

/// 生成自签名证书，返回 (cert_pem_bytes, key_pem_bytes)
fn generate_self_signed_pem() -> Result<(Vec<u8>, Vec<u8>)> {
    let cert = rcgen::generate_simple_self_signed(vec![
        "localhost".into(),
        "127.0.0.1".into(),
        "0.0.0.0".into(),
    ])
    .map_err(|e| AeroSyncError::System(format!("Failed to generate self-signed cert: {}", e)))?;

    let cert_pem = cert
        .serialize_pem()
        .map_err(|e| AeroSyncError::System(format!("Failed to serialize cert PEM: {}", e)))?
        .into_bytes();
    let key_pem = cert.serialize_private_key_pem().into_bytes();
    Ok((cert_pem, key_pem))
}

// ─────────────────────────────── HTTPS server ───────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn start_https_server(
    config: ServerConfig,
    _status: Arc<RwLock<ServerStatus>>,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    auth_manager: Option<Arc<AuthManager>>,
    audit_logger: Option<Arc<AuditLogger>>,
    metrics: Arc<Metrics>,
    ws_tx: WsBroadcast,
    chunk_arrivals: ChunkArrivalMap,
) -> Result<()> {
    // 获取 TLS 证书材料（PEM 格式，warp 接受 PEM）
    let (cert_pem, key_pem) = if let Some(ref tls_cfg) = config.tls {
        // 从外部 PEM 文件读取
        let cert = tokio::fs::read(&tls_cfg.cert_path).await.map_err(|e| {
            AeroSyncError::System(format!(
                "Cannot read HTTPS cert {}: {}",
                tls_cfg.cert_path.display(),
                e
            ))
        })?;
        let key = tokio::fs::read(&tls_cfg.key_path).await.map_err(|e| {
            AeroSyncError::System(format!(
                "Cannot read HTTPS key {}: {}",
                tls_cfg.key_path.display(),
                e
            ))
        })?;
        (cert, key)
    } else {
        // 自动生成自签名证书
        generate_self_signed_pem()?
    };

    let receive_dir = config.receive_directory.clone();
    let max_size = config.max_file_size;
    let allow_overwrite = config.allow_overwrite;
    let enable_metrics = config.enable_metrics;
    let enable_ws = config.enable_ws;

    let router: Option<Arc<Router>> = config
        .routing
        .clone()
        .map(|routing_cfg| Arc::new(Router::new(routing_cfg, receive_dir.clone())));
    let auth_mw = auth_manager.map(|m| Arc::new(AuthMiddleware::new(m)));

    let routes = build_warp_routes(
        receive_dir,
        max_size,
        allow_overwrite,
        enable_metrics,
        enable_ws,
        router,
        auth_mw,
        audit_logger,
        metrics,
        ws_tx,
        received_files,
        chunk_arrivals,
    );

    let addr: SocketAddr = format!("{}:{}", config.bind_address, config.https_port)
        .parse()
        .map_err(|e| AeroSyncError::InvalidConfig(format!("Invalid HTTPS address: {}", e)))?;

    tracing::info!(
        "HTTPS server listening on https://{} ({})",
        addr,
        if config.tls.is_some() {
            "external cert"
        } else {
            "self-signed cert"
        }
    );

    warp::serve(routes)
        .tls()
        .cert(cert_pem)
        .key(key_pem)
        .run(addr)
        .await;

    Ok(())
}

// ─────────────────────────────── HTTP server ────────────────────────────────

/// 构建所有 warp 路由（HTTP 和 HTTPS 共用）
#[allow(clippy::too_many_arguments)]
fn build_warp_routes(
    receive_dir: PathBuf,
    max_size: u64,
    allow_overwrite: bool,
    enable_metrics: bool,
    enable_ws: bool,
    router: Option<Arc<Router>>,
    auth_mw: Option<Arc<AuthMiddleware>>,
    audit_logger: Option<Arc<AuditLogger>>,
    metrics: Arc<Metrics>,
    ws_tx: WsBroadcast,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    chunk_arrivals: ChunkArrivalMap,
) -> impl warp::Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    use warp::Filter;

    // ── POST /upload ────────────────────────────────────────────────────────
    let auth_mw_upload = auth_mw.clone();
    let received_files_upload = received_files.clone();
    let receive_dir_upload = receive_dir.clone();
    let audit_upload = audit_logger.clone();
    let metrics_upload = Arc::clone(&metrics);
    let ws_tx_upload = ws_tx.clone();
    let router_upload = router.clone();
    let upload = warp::path("upload")
        .and(warp::path::tail())
        .and(warp::post())
        // Content-Length 预检：超出 max_file_size 直接 413，避免读取请求体
        .and(
            warp::header::optional::<u64>("content-length")
                .and_then(move |content_len: Option<u64>| async move {
                    if let Some(len) = content_len {
                        if len > max_size {
                            return Err(warp::reject::custom(PayloadTooLarge));
                        }
                    }
                    Ok(())
                })
                .untuple_one(),
        )
        .and(warp::header::optional::<String>("authorization"))
        .and(warp::header::optional::<String>("x-file-hash"))
        .and(warp::header::optional::<String>("x-aerosync-tag"))
        .and(warp::addr::remote())
        .and(warp::multipart::form().max_length(max_size))
        .and(warp::any().map(move || receive_dir_upload.clone()))
        .and(warp::any().map(move || allow_overwrite))
        .and(warp::any().map(move || received_files_upload.clone()))
        .and(warp::any().map(move || auth_mw_upload.clone()))
        .and(warp::any().map(move || audit_upload.clone()))
        .and(warp::any().map(move || metrics_upload.clone()))
        .and(warp::any().map(move || ws_tx_upload.clone()))
        .and(warp::any().map(move || router_upload.clone()))
        .and_then(handle_file_upload);

    // ── GET /health ──────────────────────────────────────────────────────────
    let received_files_health = received_files.clone();
    let metrics_health = metrics.clone();
    let health = warp::path("health")
        .and(warp::get())
        .and(warp::any().map(move || received_files_health.clone()))
        .and(warp::any().map(move || metrics_health.clone()))
        .and_then(handle_health);

    // ── GET /status ──────────────────────────────────────────────────────────
    let received_files_status = received_files.clone();
    let status_route = warp::path("status")
        .and(warp::get())
        .and(warp::any().map(move || received_files_status.clone()))
        .and_then(handle_status_request);

    // ── POST /upload/chunk ────────────────────────────────────────────────────
    let auth_mw_chunk = auth_mw.clone();
    let receive_dir_chunk = receive_dir.clone();
    let received_files_chunk = received_files.clone();
    let audit_chunk = audit_logger.clone();
    let metrics_chunk = Arc::clone(&metrics);
    let ws_tx_chunk = ws_tx.clone();
    let router_chunk = router.clone();
    let chunk_arrivals_chunk = chunk_arrivals.clone();
    let upload_chunk = warp::path!("upload" / "chunk")
        .and(warp::post())
        .and(warp::header::optional::<String>("authorization"))
        .and(warp::query::<ChunkQuery>())
        .and(warp::addr::remote())
        .and(warp::body::bytes())
        .and(warp::any().map(move || receive_dir_chunk.clone()))
        .and(warp::any().map(move || allow_overwrite))
        .and(warp::any().map(move || received_files_chunk.clone()))
        .and(warp::any().map(move || auth_mw_chunk.clone()))
        .and(warp::any().map(move || audit_chunk.clone()))
        .and(warp::any().map(move || metrics_chunk.clone()))
        .and(warp::any().map(move || ws_tx_chunk.clone()))
        .and(warp::any().map(move || router_chunk.clone()))
        .and(warp::any().map(move || chunk_arrivals_chunk.clone()))
        .and_then(handle_chunk_upload);

    // ── POST /upload/complete ─────────────────────────────────────────────────
    let auth_mw_complete = auth_mw.clone();
    let receive_dir_complete = receive_dir.clone();
    let received_files_complete = received_files.clone();
    let audit_complete = audit_logger.clone();
    let metrics_complete = Arc::clone(&metrics);
    let ws_tx_complete = ws_tx.clone();
    let router_complete = router.clone();
    let chunk_arrivals_complete = chunk_arrivals.clone();
    let upload_complete = warp::path!("upload" / "complete")
        .and(warp::post())
        .and(warp::header::optional::<String>("authorization"))
        .and(warp::query::<CompleteQuery>())
        .and(warp::any().map(move || receive_dir_complete.clone()))
        .and(warp::any().map(move || allow_overwrite))
        .and(warp::any().map(move || received_files_complete.clone()))
        .and(warp::any().map(move || auth_mw_complete.clone()))
        .and(warp::any().map(move || audit_complete.clone()))
        .and(warp::any().map(move || metrics_complete.clone()))
        .and(warp::any().map(move || ws_tx_complete.clone()))
        .and(warp::any().map(move || router_complete.clone()))
        .and(warp::any().map(move || chunk_arrivals_complete.clone()))
        .and_then(handle_chunk_complete);

    // ── POST /upload/batch ────────────────────────────────────────────────────
    let auth_mw_batch = auth_mw.clone();
    let receive_dir_batch = receive_dir.clone();
    let received_files_batch = received_files.clone();
    let audit_batch = audit_logger.clone();
    let metrics_batch = Arc::clone(&metrics);
    let ws_tx_batch = ws_tx.clone();
    let upload_batch = warp::path!("upload" / "batch")
        .and(warp::post())
        .and(warp::header::optional::<String>("authorization"))
        .and(warp::addr::remote())
        .and(warp::multipart::form().max_length(512 * 1024 * 1024)) // 512 MB per batch
        .and(warp::any().map(move || receive_dir_batch.clone()))
        .and(warp::any().map(move || allow_overwrite))
        .and(warp::any().map(move || received_files_batch.clone()))
        .and(warp::any().map(move || auth_mw_batch.clone()))
        .and(warp::any().map(move || audit_batch.clone()))
        .and(warp::any().map(move || metrics_batch.clone()))
        .and(warp::any().map(move || ws_tx_batch.clone()))
        .and_then(handle_batch_upload);

    // ── GET /metrics ──────────────────────────────────────────────────────────
    let metrics_arc = Arc::clone(&metrics);
    let receive_dir_m = receive_dir.clone();
    let metrics_route = warp::path("metrics")
        .and(warp::get())
        .and(warp::any().map(move || Arc::clone(&metrics_arc)))
        .and(warp::any().map(move || receive_dir_m.clone()))
        .and(warp::any().map(move || enable_metrics))
        .and_then(|m: Arc<Metrics>, dir: PathBuf, enabled: bool| async move {
            use warp::Reply;
            if !enabled {
                return Err::<warp::reply::Response, _>(warp::reject::not_found());
            }
            let (free, total) = get_disk_space(&dir);
            let body = m.render(free, total);
            Ok(warp::reply::with_header(
                body,
                "Content-Type",
                "text/plain; version=0.0.4; charset=utf-8",
            )
            .into_response())
        });

    // ── GET /ws (WebSocket) ───────────────────────────────────────────────────
    let ws_tx_ws = ws_tx.clone();
    let metrics_ws = Arc::clone(&metrics);
    let ws_route =
        warp::path("ws")
            .and(warp::get())
            .and(warp::ws())
            .and(warp::any().map(move || ws_tx_ws.subscribe()))
            .and(warp::any().map(move || Arc::clone(&metrics_ws)))
            .and(warp::any().map(move || enable_ws))
            .and_then(
                |ws: warp::ws::Ws,
                 rx: broadcast::Receiver<WsEvent>,
                 m: Arc<Metrics>,
                 enabled: bool| async move {
                    if !enabled {
                        return Err::<warp::reply::Response, _>(warp::reject::not_found());
                    }
                    use warp::Reply;
                    Ok(ws
                        .on_upgrade(move |socket| handle_ws_client(socket, rx, m))
                        .into_response())
                },
            );

    let cors = warp::cors()
        .allow_any_origin()
        .allow_headers(vec![
            "content-type",
            "authorization",
            "x-file-hash",
            "x-aerosync-tag",
        ])
        .allow_methods(vec!["GET", "POST", "OPTIONS"]);

    upload_chunk
        .or(upload_complete)
        .or(upload_batch)
        .or(upload)
        .or(health)
        .or(status_route)
        .or(metrics_route)
        .or(ws_route)
        .recover(|err: warp::Rejection| async move {
            use warp::Reply;
            if err.find::<PayloadTooLarge>().is_some() {
                Ok::<_, warp::Rejection>(
                    warp::reply::with_status(
                        warp::reply::json(&serde_json::json!({
                            "error": "Payload Too Large: file exceeds server limit"
                        })),
                        warp::http::StatusCode::PAYLOAD_TOO_LARGE,
                    )
                    .into_response(),
                )
            } else {
                Err(err)
            }
        })
        .with(cors)
}

#[allow(clippy::too_many_arguments)]
async fn start_http_server(
    config: ServerConfig,
    _status: Arc<RwLock<ServerStatus>>,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    auth_manager: Option<Arc<AuthManager>>,
    audit_logger: Option<Arc<AuditLogger>>,
    metrics: Arc<Metrics>,
    ws_tx: WsBroadcast,
    chunk_arrivals: ChunkArrivalMap,
) -> Result<()> {
    let receive_dir = config.receive_directory.clone();
    let max_size = config.max_file_size;
    let allow_overwrite = config.allow_overwrite;
    let enable_metrics = config.enable_metrics;
    let enable_ws = config.enable_ws;

    let router: Option<Arc<Router>> = config
        .routing
        .clone()
        .map(|routing_cfg| Arc::new(Router::new(routing_cfg, receive_dir.clone())));
    let auth_mw = auth_manager.map(|m| Arc::new(AuthMiddleware::new(m)));

    let routes = build_warp_routes(
        receive_dir,
        max_size,
        allow_overwrite,
        enable_metrics,
        enable_ws,
        router,
        auth_mw,
        audit_logger,
        metrics,
        ws_tx,
        received_files,
        chunk_arrivals,
    );

    let addr: SocketAddr = format!("{}:{}", config.bind_address, config.http_port)
        .parse()
        .map_err(|e| AeroSyncError::InvalidConfig(format!("Invalid address: {}", e)))?;

    tracing::info!("HTTP server listening on {}", addr);
    warp::serve(routes).run(addr).await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_file_upload(
    path_tail: warp::path::Tail,
    auth_header: Option<String>,
    expected_hash: Option<String>,
    tag: Option<String>,
    remote_addr: Option<SocketAddr>,
    mut form: warp::multipart::FormData,
    receive_dir: PathBuf,
    allow_overwrite: bool,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    auth_mw: Option<Arc<AuthMiddleware>>,
    audit_logger: Option<Arc<AuditLogger>>,
    metrics: Arc<Metrics>,
    ws_tx: WsBroadcast,
    router: Option<Arc<Router>>,
) -> std::result::Result<warp::reply::Response, warp::Rejection> {
    use bytes::Buf;
    use futures::TryStreamExt;
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncWriteExt;
    use warp::Reply;

    let client_ip = remote_addr
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // 认证
    if let Some(ref mw) = auth_mw {
        let auth_str = auth_header.as_deref();
        match mw.authenticate_http_request(auth_str, &client_ip) {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!("HTTP: Unauthorized upload attempt from {}", client_ip);
                if let Some(ref al) = audit_logger {
                    al.log_auth_failed("http", Some(&client_ip), "Unauthorized")
                        .await;
                }
                let resp = mw.unauthorized_response();
                return Ok(warp::reply::with_status(
                    warp::reply::json(&serde_json::json!({
                        "error": resp.message
                    })),
                    warp::http::StatusCode::UNAUTHORIZED,
                )
                .into_response());
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

        // 优先从 URL 路径尾提取文件名（保留子目录结构），
        // 降级到 multipart filename
        let url_tail = percent_decode(path_tail.as_str());
        let filename = if !url_tail.is_empty() {
            url_tail
        } else {
            part.filename().unwrap_or("unknown").to_string()
        };
        let file_id = Uuid::new_v4();
        let safe_name = sanitize_filename(&filename);

        // 路由：根据规则选择目标目录
        let dest_dir = if let Some(ref r) = router {
            r.resolve(&client_ip, tag.as_deref(), &safe_name)
        } else {
            receive_dir.clone()
        };

        let file_path = get_unique_file_path(&dest_dir, &safe_name, allow_overwrite);

        // 确保父目录存在（支持子目录结构）
        if let Some(parent) = file_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|_| warp::reject::reject())?;
        }

        let mut file = tokio::fs::File::create(&file_path)
            .await
            .map_err(|_| warp::reject::reject())?;

        let mut size = 0u64;
        let mut hasher = Sha256::new();
        let mut stream = part.stream();

        while let Some(chunk) = stream
            .try_next()
            .await
            .map_err(|_| warp::reject::reject())?
        {
            let data = chunk.chunk();
            hasher.update(data);
            size += data.len() as u64;
            file.write_all(data)
                .await
                .map_err(|_| warp::reject::reject())?;
        }
        file.flush().await.map_err(|_| warp::reject::reject())?;

        let actual_hash = hex::encode(hasher.finalize());

        // SHA-256 校验
        if let Some(ref expected) = expected_hash {
            if &actual_hash != expected {
                tracing::error!(
                    "HTTP: Hash mismatch for '{}': expected={} actual={}",
                    filename,
                    expected,
                    actual_hash
                );
                let _ = tokio::fs::remove_file(&file_path).await;
                metrics.inc_upload_errors();
                let _ = ws_tx.send(WsEvent::Failed {
                    filename: filename.clone(),
                    reason: "SHA-256 mismatch".to_string(),
                });
                if let Some(ref al) = audit_logger {
                    al.log_failed(
                        Direction::Receive,
                        "http",
                        &filename,
                        size,
                        Some(&client_ip),
                        "SHA-256 mismatch",
                    )
                    .await;
                }
                use warp::Reply;
                return Ok(warp::reply::with_status(
                    warp::reply::json(&serde_json::json!({
                        "error": "SHA-256 mismatch",
                        "expected": expected,
                        "actual": actual_hash
                    })),
                    warp::http::StatusCode::BAD_REQUEST,
                )
                .into_response());
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

        metrics.inc_files_received();
        metrics.add_bytes_received(size);
        let _ = ws_tx.send(WsEvent::Completed {
            filename: filename.clone(),
            size,
            sha256: actual_hash.clone(),
        });

        if let Some(ref al) = audit_logger {
            al.log_completed(
                Direction::Receive,
                "http",
                &filename,
                size,
                Some(&actual_hash),
                Some(&client_ip),
            )
            .await;
        }

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
        )
        .into_response());
    }

    Err(warp::reject::reject())
}

async fn handle_health(
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    metrics: Arc<Metrics>,
) -> std::result::Result<impl warp::Reply, warp::Rejection> {
    let count = received_files.read().await.len();

    // 获取当前目录磁盘空间（跨平台）
    let (free_bytes, total_bytes) = get_disk_space(std::path::Path::new("."));

    let reply = warp::reply::json(&serde_json::json!({
        "status": "ok",
        "received_files": count,
        "free_bytes": free_bytes,
        "total_bytes": total_bytes,
        "active_transfers": metrics.active_transfers(),
        "queue_depth": metrics.queue_depth(),
        "protocols": ["http", "quic"],
        "version": env!("CARGO_PKG_VERSION"),
    }));
    // X-AeroSync header 让客户端识别对端为 AeroSync 服务，触发 QUIC 自动升级
    Ok(warp::reply::with_header(reply, "X-AeroSync", "true"))
}

/// 获取指定路径所在文件系统的磁盘空闲/总量（字节）
/// 返回 (free_bytes, total_bytes)；获取失败返回 (0, 0)
fn get_disk_space(path: &std::path::Path) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::mem::MaybeUninit;
        let c_path = match CString::new(path.to_string_lossy().as_bytes()) {
            Ok(p) => p,
            Err(_) => return (0, 0),
        };
        unsafe {
            let mut stat: libc::statvfs = MaybeUninit::zeroed().assume_init();
            if libc::statvfs(c_path.as_ptr(), &mut stat) == 0 {
                #[allow(clippy::unnecessary_cast)]
                let free = stat.f_bavail as u64 * stat.f_bsize;
                #[allow(clippy::unnecessary_cast)]
                let total = stat.f_blocks as u64 * stat.f_bsize;
                return (free, total);
            }
        }
        (0, 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        (0, 0)
    }
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

// ─────────────────────────── 分片上传 handlers ───────────────────────────────

/// 每个 task_id 的已到达分片计数器
type ChunkArrivalMap = Arc<Mutex<HashMap<Uuid, Arc<AtomicU32>>>>;

#[derive(Debug, serde::Deserialize)]
struct ChunkQuery {
    task_id: Uuid,
    chunk_index: u32,
    total_chunks: u32,
    filename: String,
    /// 文件总字节数（用于预分配）
    total_size: u64,
    /// 本分片字节数（用于计算 offset）
    chunk_size: u64,
}

#[derive(Debug, serde::Deserialize)]
struct CompleteQuery {
    task_id: Uuid,
    filename: String,
    #[allow(dead_code)]
    total_chunks: u32,
    #[allow(dead_code)]
    total_size: u64,
    sha256: Option<String>,
}

/// 批量接收小文件：multipart 中每个 part 的 name 作为相对路径文件名
/// 路由：POST /upload/batch
#[allow(clippy::too_many_arguments)]
async fn handle_batch_upload(
    auth_header: Option<String>,
    remote_addr: Option<SocketAddr>,
    mut form: warp::multipart::FormData,
    receive_dir: PathBuf,
    allow_overwrite: bool,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    auth_mw: Option<Arc<AuthMiddleware>>,
    audit_logger: Option<Arc<AuditLogger>>,
    metrics: Arc<Metrics>,
    ws_tx: WsBroadcast,
) -> std::result::Result<impl warp::Reply, warp::Rejection> {
    use bytes::Buf;
    use futures::TryStreamExt;
    use sha2::{Digest, Sha256};
    use warp::Reply;

    let client_ip = remote_addr
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // 认证
    if let Some(ref mw) = auth_mw {
        let auth_str = auth_header.as_deref();
        match mw.authenticate_http_request(auth_str, &client_ip) {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!("HTTP batch: Unauthorized attempt from {}", client_ip);
                if let Some(ref al) = audit_logger {
                    al.log_auth_failed("http", Some(&client_ip), "Unauthorized")
                        .await;
                }
                let resp = mw.unauthorized_response();
                return Ok(warp::reply::with_status(
                    warp::reply::json(&serde_json::json!({"error": resp.message})),
                    warp::http::StatusCode::UNAUTHORIZED,
                )
                .into_response());
            }
            Err(_) => return Err(warp::reject::reject()),
        }
    }

    let mut saved = 0u32;
    let mut errors: Vec<String> = Vec::new();

    // 每个 part 的 name 是相对路径文件名
    while let Some(part) = form.try_next().await.map_err(|_| warp::reject::reject())? {
        let filename = sanitize_filename(part.name());
        if filename.is_empty() || filename == "." {
            continue;
        }

        let file_path = get_unique_file_path(&receive_dir, &filename, allow_overwrite);
        if let Some(parent) = file_path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                errors.push(format!("{}: mkdir failed: {}", filename, e));
                continue;
            }
        }

        // 收集 part 数据
        let data: bytes::Bytes = match part
            .stream()
            .try_fold(bytes::BytesMut::new(), |mut acc, chunk| async move {
                acc.extend_from_slice(chunk.chunk());
                Ok(acc)
            })
            .await
        {
            Ok(b) => b.freeze(),
            Err(e) => {
                errors.push(format!("{}: read error: {}", filename, e));
                continue;
            }
        };

        let size = data.len() as u64;

        // 写文件
        let mut hash = Sha256::new();
        hash.update(&data);
        let sha256 = hex::encode(hash.finalize());

        match tokio::fs::write(&file_path, &data).await {
            Ok(_) => {
                tracing::debug!("Batch: saved {} ({} bytes)", filename, size);
                metrics.inc_files_received();
                metrics.add_bytes_received(size);
                let received_file = ReceivedFile {
                    id: Uuid::new_v4(),
                    original_name: filename.clone(),
                    size,
                    sha256: Some(sha256.clone()),
                    received_at: std::time::SystemTime::now(),
                    sender_ip: Some(client_ip.clone()),
                    saved_path: file_path.clone(),
                };
                received_files.write().await.push(received_file.clone());
                let _ = ws_tx.send(WsEvent::Completed {
                    filename: filename.clone(),
                    size,
                    sha256: sha256.clone(),
                });
                if let Some(ref al) = audit_logger {
                    al.log_completed(
                        crate::core::audit::Direction::Receive,
                        "http",
                        &filename,
                        size,
                        Some(&sha256),
                        Some(&client_ip),
                    )
                    .await;
                }
                saved += 1;
            }
            Err(e) => {
                errors.push(format!("{}: write failed: {}", filename, e));
                metrics.inc_upload_errors();
            }
        }
    }

    tracing::info!(
        "Batch upload: {} saved, {} errors from {}",
        saved,
        errors.len(),
        client_ip
    );
    Ok(warp::reply::json(&serde_json::json!({
        "saved": saved,
        "errors": errors,
    }))
    .into_response())
}

/// 接收单个分片，直接 seek 写入最终文件（边传边写，无需合并阶段）。
/// 第一个分片到达时用 set_len 预分配文件空间；所有分片到齐后不做额外操作，
/// 由客户端调用 /upload/complete 进行 SHA-256 校验。
#[allow(clippy::too_many_arguments)]
async fn handle_chunk_upload(
    auth_header: Option<String>,
    query: ChunkQuery,
    remote_addr: Option<SocketAddr>,
    body: bytes::Bytes,
    receive_dir: PathBuf,
    allow_overwrite: bool,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    auth_mw: Option<Arc<AuthMiddleware>>,
    audit_logger: Option<Arc<AuditLogger>>,
    metrics: Arc<Metrics>,
    ws_tx: WsBroadcast,
    router: Option<Arc<Router>>,
    chunk_arrivals: ChunkArrivalMap,
) -> std::result::Result<warp::reply::Response, warp::Rejection> {
    use std::io::SeekFrom;
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};
    use warp::Reply;

    let client_ip = remote_addr
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // 认证
    if let Some(ref mw) = auth_mw {
        match mw.authenticate_http_request(auth_header.as_deref(), &client_ip) {
            Ok(true) => {}
            _ => {
                if let Some(ref al) = audit_logger {
                    al.log_auth_failed("http", Some(&client_ip), "Unauthorized")
                        .await;
                }
                return Ok(warp::reply::with_status(
                    warp::reply::json(&serde_json::json!({"error": "Unauthorized"})),
                    warp::http::StatusCode::UNAUTHORIZED,
                )
                .into_response());
            }
        }
    }

    // 确定目标文件路径
    let safe_name = sanitize_filename(&query.filename);
    let dest_dir = if let Some(ref r) = router {
        r.resolve("chunk", None, &safe_name)
    } else {
        receive_dir.clone()
    };
    // 所有分片写同一个文件（用 task_id 命名临时文件，complete 时重命名）
    let tmp_path = dest_dir
        .join(".aerosync")
        .join("inprogress")
        .join(query.task_id.to_string());

    if let Some(parent) = tmp_path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            tracing::error!("Failed to create inprogress dir: {}", e);
            return Ok(warp::reply::with_status(
                warp::reply::json(&serde_json::json!({"error": "server error"})),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            )
            .into_response());
        }
    }

    // 打开文件（第一个分片预分配，后续直接追写）
    let file = if query.chunk_index == 0 {
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                tracing::error!("Failed to create inprogress file: {}", e);
                return Ok(warp::reply::with_status(
                    warp::reply::json(&serde_json::json!({"error": "write failed"})),
                    warp::http::StatusCode::INTERNAL_SERVER_ERROR,
                )
                .into_response());
            }
        }
    } else {
        match tokio::fs::OpenOptions::new()
            .write(true)
            .open(&tmp_path)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                tracing::error!(
                    "Failed to open inprogress file for chunk {}: {}",
                    query.chunk_index,
                    e
                );
                return Ok(warp::reply::with_status(
                    warp::reply::json(&serde_json::json!({"error": "write failed"})),
                    warp::http::StatusCode::INTERNAL_SERVER_ERROR,
                )
                .into_response());
            }
        }
    };

    // 第一个分片：fallocate 预分配整个文件大小，避免并发写时扩展竞争
    if query.chunk_index == 0 {
        if let Err(e) = file.set_len(query.total_size).await {
            tracing::warn!("set_len({}) failed (non-fatal): {}", query.total_size, e);
        }
    }

    // 计算写入偏移并 seek
    let offset = query.chunk_index as u64 * query.chunk_size;
    let mut file = file;
    if let Err(e) = file.seek(SeekFrom::Start(offset)).await {
        tracing::error!("seek to offset {} failed: {}", offset, e);
        return Ok(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({"error": "seek failed"})),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        )
        .into_response());
    }
    if let Err(e) = file.write_all(&body).await {
        tracing::error!("write_all chunk {} failed: {}", query.chunk_index, e);
        return Ok(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({"error": "write failed"})),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        )
        .into_response());
    }
    drop(file);

    metrics.add_bytes_received(body.len() as u64);

    tracing::debug!(
        "Chunk {}/{} written for task {} (offset={}, {} bytes)",
        query.chunk_index + 1,
        query.total_chunks,
        query.task_id,
        offset,
        body.len()
    );

    // 原子递增到达计数
    let counter = {
        let mut map = chunk_arrivals.lock().unwrap();
        Arc::clone(
            map.entry(query.task_id)
                .or_insert_with(|| Arc::new(AtomicU32::new(0))),
        )
    };
    let arrived = counter.fetch_add(1, Ordering::AcqRel) + 1;

    // 所有分片已到齐 → 重命名为最终文件
    let all_done = arrived >= query.total_chunks;
    if all_done {
        // 清理计数器
        chunk_arrivals.lock().unwrap().remove(&query.task_id);

        let final_path = get_unique_file_path(&dest_dir, &safe_name, allow_overwrite);
        if let Some(parent) = final_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        if let Err(e) = tokio::fs::rename(&tmp_path, &final_path).await {
            tracing::error!("Failed to rename inprogress file: {}", e);
            // 不返回错误，让 complete 端点处理
        } else {
            tracing::info!(
                "All {} chunks received for task {}, file ready: {}",
                query.total_chunks,
                query.task_id,
                final_path.display()
            );
            // 记录接收文件（size 未校验，complete 端点会再次确认）
            let record = ReceivedFile {
                id: Uuid::new_v4(),
                original_name: query.filename.clone(),
                saved_path: final_path.clone(),
                size: query.total_size,
                sha256: None, // complete 端点填充
                received_at: std::time::SystemTime::now(),
                sender_ip: remote_addr.map(|a| a.ip().to_string()),
            };
            received_files.write().await.push(record);
            metrics.inc_files_received();

            if let Some(ref al) = audit_logger {
                al.log_completed(
                    Direction::Receive,
                    "http-chunk",
                    &query.filename,
                    query.total_size,
                    None,
                    remote_addr.map(|a| a.ip().to_string()).as_deref(),
                )
                .await;
            }
            let _ = ws_tx.send(WsEvent::Completed {
                filename: query.filename.clone(),
                size: query.total_size,
                sha256: String::new(), // SHA256 由 /upload/complete 填充
            });
        }
    }

    Ok(warp::reply::with_status(
        warp::reply::json(&serde_json::json!({
            "task_id": query.task_id,
            "chunk_index": query.chunk_index,
            "received": body.len(),
            "all_chunks_done": all_done,
        })),
        warp::http::StatusCode::OK,
    )
    .into_response())
}

/// 所有分片到齐后由客户端调用，进行 SHA-256 校验并更新记录。
/// 文件已在 handle_chunk_upload 中合并完毕，本端点只做校验。
#[allow(clippy::too_many_arguments)]
async fn handle_chunk_complete(
    auth_header: Option<String>,
    query: CompleteQuery,
    receive_dir: PathBuf,
    allow_overwrite: bool,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    auth_mw: Option<Arc<AuthMiddleware>>,
    audit_logger: Option<Arc<AuditLogger>>,
    metrics: Arc<Metrics>,
    ws_tx: WsBroadcast,
    router: Option<Arc<Router>>,
    _chunk_arrivals: ChunkArrivalMap,
) -> std::result::Result<warp::reply::Response, warp::Rejection> {
    use sha2::{Digest, Sha256};
    use warp::Reply;

    // 认证
    if let Some(ref mw) = auth_mw {
        match mw.authenticate_http_request(auth_header.as_deref(), "chunk-complete") {
            Ok(true) => {}
            _ => {
                if let Some(ref al) = audit_logger {
                    al.log_auth_failed("http", None, "Unauthorized").await;
                }
                return Ok(warp::reply::with_status(
                    warp::reply::json(&serde_json::json!({"error": "Unauthorized"})),
                    warp::http::StatusCode::UNAUTHORIZED,
                )
                .into_response());
            }
        }
    }

    let safe_name = sanitize_filename(&query.filename);
    let dest_dir = if let Some(ref r) = router {
        r.resolve("chunk-complete", None, &safe_name)
    } else {
        receive_dir.clone()
    };

    // 查找已写好的文件（可能被 rename 为 safe_name 或带序号的变体）
    let candidate = dest_dir.join(&safe_name);
    // 如果文件还没被重命名完成（极端情况：complete 先于最后一片），等一下
    let final_path = if candidate.exists() {
        candidate
    } else {
        // 尝试找带 task_id 的 inprogress 文件
        let tmp = dest_dir
            .join(".aerosync")
            .join("inprogress")
            .join(query.task_id.to_string());
        if tmp.exists() {
            // 还没 rename，现在重命名
            let fp = get_unique_file_path(&dest_dir, &safe_name, allow_overwrite);
            if let Some(parent) = fp.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            if let Err(e) = tokio::fs::rename(&tmp, &fp).await {
                tracing::error!("complete: rename failed: {}", e);
                return Ok(warp::reply::with_status(
                    warp::reply::json(&serde_json::json!({"error": "file not ready"})),
                    warp::http::StatusCode::INTERNAL_SERVER_ERROR,
                )
                .into_response());
            }
            fp
        } else {
            return Ok(warp::reply::with_status(
                warp::reply::json(&serde_json::json!({"error": "file not found"})),
                warp::http::StatusCode::NOT_FOUND,
            )
            .into_response());
        }
    };

    // 计算 SHA-256
    let data = match tokio::fs::read(&final_path).await {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("complete: failed to read file for sha256: {}", e);
            return Ok(warp::reply::with_status(
                warp::reply::json(&serde_json::json!({"error": "read failed"})),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            )
            .into_response());
        }
    };
    let actual_sha256 = hex::encode(Sha256::digest(&data));
    let total_size = data.len() as u64;

    // SHA-256 校验
    if let Some(ref expected) = query.sha256 {
        if &actual_sha256 != expected {
            tracing::error!(
                "Chunk complete: SHA-256 mismatch for {} (expected={}, got={})",
                query.filename,
                expected,
                actual_sha256
            );
            let _ = tokio::fs::remove_file(&final_path).await;
            metrics.inc_upload_errors();
            let _ = ws_tx.send(WsEvent::Failed {
                filename: query.filename.clone(),
                reason: "SHA-256 mismatch".to_string(),
            });
            return Ok(warp::reply::with_status(
                warp::reply::json(&serde_json::json!({
                    "error": "SHA-256 mismatch",
                    "expected": expected,
                    "actual": actual_sha256,
                })),
                warp::http::StatusCode::BAD_REQUEST,
            )
            .into_response());
        }
    }

    // 更新 received_files 中的 sha256（如果 handle_chunk_upload 已插入记录）
    {
        let mut files = received_files.write().await;
        if let Some(rec) = files.iter_mut().find(|r| r.saved_path == final_path) {
            rec.sha256 = Some(actual_sha256.clone());
            rec.size = total_size;
        }
    }

    tracing::info!(
        "Chunked upload verified: {} ({} bytes, sha256={})",
        safe_name,
        total_size,
        actual_sha256
    );

    // 发送带 sha256 的完成事件
    let _ = ws_tx.send(WsEvent::Completed {
        filename: query.filename.clone(),
        size: total_size,
        sha256: actual_sha256.clone(),
    });

    if let Some(ref al) = audit_logger {
        al.log_completed(
            Direction::Receive,
            "http-chunk-complete",
            &query.filename,
            total_size,
            Some(&actual_sha256),
            None,
        )
        .await;
    }

    Ok(warp::reply::with_status(
        warp::reply::json(&serde_json::json!({
            "status": "complete",
            "filename": query.filename,
            "size": total_size,
            "sha256": actual_sha256,
        })),
        warp::http::StatusCode::OK,
    )
    .into_response())
}

// ─────────────────────────────── WebSocket handler ──────────────────────────

async fn handle_ws_client(
    ws: warp::ws::WebSocket,
    mut rx: broadcast::Receiver<WsEvent>,
    metrics: Arc<Metrics>,
) {
    use futures::{SinkExt, StreamExt};
    use warp::ws::Message;

    metrics.inc_ws_connections();
    tracing::debug!(
        "WebSocket client connected (active={})",
        metrics.active_ws()
    );

    let (mut tx, mut client_rx) = ws.split();

    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Ok(ev) => {
                        let json = match serde_json::to_string(&ev) {
                            Ok(s) => s,
                            Err(_) => continue,
                        };
                        if tx.send(Message::text(json)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("WS client lagged behind by {} messages", n);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = client_rx.next() => {
                // Client disconnected or sent close frame
                match msg {
                    Some(Ok(m)) if m.is_close() => break,
                    None => break,
                    _ => {}
                }
            }
        }
    }

    metrics.dec_ws_connections();
    tracing::debug!(
        "WebSocket client disconnected (active={})",
        metrics.active_ws()
    );
}

// ─────────────────────────────── QUIC server ────────────────────────────────

async fn start_quic_server(
    config: ServerConfig,
    status: Arc<RwLock<ServerStatus>>,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    auth_manager: Option<Arc<AuthManager>>,
    audit_logger: Option<Arc<AuditLogger>>,
) -> Result<()> {
    use quinn::Endpoint;

    let server_config = configure_quic_server(config.tls.as_ref())?;
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
        let audit_quic_conn = audit_logger.clone();
        let _status = status.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_quic_connection(
                connection,
                receive_dir,
                allow_overwrite,
                max_size,
                files,
                auth,
                audit_quic_conn,
            )
            .await
            {
                tracing::error!("QUIC connection error from {}: {}", remote, e);
            }
        });
    }

    Ok(())
}

fn configure_quic_server(tls: Option<&TlsConfig>) -> Result<quinn::ServerConfig> {
    use rustls::{Certificate, PrivateKey, ServerConfig as TlsServerConfig};

    let (certs, key) = if let Some(tls_cfg) = tls {
        // 加载外部 PEM 文件
        load_tls_from_pem(&tls_cfg.cert_path, &tls_cfg.key_path)?
    } else {
        // 自动生成自签名证书
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])
            .map_err(|e| AeroSyncError::System(format!("Failed to generate certificate: {}", e)))?;
        let cert_der = cert.serialize_der().map_err(|e| {
            AeroSyncError::System(format!("Failed to serialize certificate: {}", e))
        })?;
        let key_der = cert.serialize_private_key_der();
        (vec![Certificate(cert_der)], PrivateKey(key_der))
    };

    let mut tls_config = TlsServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| AeroSyncError::System(format!("TLS config error: {}", e)))?;

    tls_config.alpn_protocols = vec![b"aerosync".to_vec()];

    Ok(quinn::ServerConfig::with_crypto(Arc::new(tls_config)))
}

/// 从 PEM 文件加载证书链和私钥
fn load_tls_from_pem(
    cert_path: &PathBuf,
    key_path: &PathBuf,
) -> Result<(Vec<rustls::Certificate>, rustls::PrivateKey)> {
    use std::io::BufReader;

    let cert_file = std::fs::File::open(cert_path).map_err(|e| {
        AeroSyncError::System(format!(
            "Cannot open cert file {}: {}",
            cert_path.display(),
            e
        ))
    })?;
    let mut cert_reader = BufReader::new(cert_file);
    let certs: Vec<rustls::Certificate> = rustls_pemfile::certs(&mut cert_reader)
        .map_err(|e| AeroSyncError::System(format!("Failed to parse cert PEM: {}", e)))?
        .into_iter()
        .map(rustls::Certificate)
        .collect();

    if certs.is_empty() {
        return Err(AeroSyncError::System(format!(
            "No certificates found in {}",
            cert_path.display()
        )));
    }

    let key_file = std::fs::File::open(key_path).map_err(|e| {
        AeroSyncError::System(format!(
            "Cannot open key file {}: {}",
            key_path.display(),
            e
        ))
    })?;
    let mut key_reader = BufReader::new(key_file);

    // 尝试 PKCS8，再尝试 RSA
    let key = rustls_pemfile::pkcs8_private_keys(&mut key_reader)
        .map_err(|e| AeroSyncError::System(format!("Failed to parse key PEM: {}", e)))?
        .into_iter()
        .next()
        .map(rustls::PrivateKey)
        .or_else(|| {
            // 重新打开文件读 RSA 格式
            let key_file2 = std::fs::File::open(key_path).ok()?;
            let mut kr = BufReader::new(key_file2);
            rustls_pemfile::rsa_private_keys(&mut kr)
                .ok()?
                .into_iter()
                .next()
                .map(rustls::PrivateKey)
        })
        .ok_or_else(|| {
            AeroSyncError::System(format!("No private key found in {}", key_path.display()))
        })?;

    Ok((certs, key))
}

async fn handle_quic_connection(
    connection: quinn::Connection,
    receive_dir: PathBuf,
    allow_overwrite: bool,
    max_size: u64,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    auth_manager: Option<Arc<AuthManager>>,
    audit_logger: Option<Arc<AuditLogger>>,
) -> Result<()> {
    let remote_ip = connection.remote_address().ip().to_string();

    // QUIC 连接层认证：读取第一条消息作为 Token
    if let Some(ref _auth) = auth_manager {
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
            tracing::warn!(
                "QUIC: Unknown command: {}",
                &header_str[..header_str.len().min(64)]
            );
            continue;
        }

        let parts: Vec<&str> = header_str.splitn(5, ':').collect();
        if parts.len() < 3 {
            tracing::warn!("QUIC: Malformed UPLOAD header");
            continue;
        }

        let filename = parts[1];
        let file_size: u64 = parts[2].trim().parse().unwrap_or(0);
        let token = parts.get(3).copied();

        // 认证（若启用）
        if let Some(ref auth) = auth_manager {
            let auth_mw = AuthMiddleware::new(Arc::clone(auth));
            let token_header = token.map(|t| format!("Bearer {}", t));
            match auth_mw.authenticate_http_request(token_header.as_deref(), &remote_ip) {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!("QUIC: Unauthorized from {}", remote_ip);
                    if let Some(ref al) = audit_logger {
                        al.log_auth_failed("quic", Some(&remote_ip), "Unauthorized")
                            .await;
                    }
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
            audit_logger.clone(),
        )
        .await
        {
            Ok(_) => {
                let _ = send.write_all(b"SUCCESS").await;
                tracing::info!("QUIC: Sent SUCCESS response for '{}'", filename);
            }
            Err(e) => {
                if let Some(ref al) = audit_logger {
                    al.log_failed(
                        Direction::Receive,
                        "quic",
                        filename,
                        file_size,
                        Some(&remote_ip),
                        &e.to_string(),
                    )
                    .await;
                }
                let _ = send.write_all(format!("ERROR:{}", e).as_bytes()).await;
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
    audit_logger: Option<Arc<AuditLogger>>,
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

    if let Some(ref al) = audit_logger {
        al.log_completed(
            Direction::Receive,
            "quic",
            filename,
            total,
            Some(&actual_hash),
            Some(sender_ip),
        )
        .await;
    }

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

/// 将 URL percent-encoding 解码为 UTF-8 字符串（如 %E4%B8%AD → 中）
fn percent_decode(input: &str) -> String {
    let bytes: Vec<u8> = {
        let mut out = Vec::with_capacity(input.len());
        let mut chars = input.bytes().peekable();
        while let Some(b) = chars.next() {
            if b == b'%' {
                // 读取接下来两个十六进制字符
                let h1 = chars.next();
                let h2 = chars.next();
                if let (Some(h1), Some(h2)) = (h1, h2) {
                    let hex = [h1, h2];
                    if let Ok(s) = std::str::from_utf8(&hex) {
                        if let Ok(byte) = u8::from_str_radix(s, 16) {
                            out.push(byte);
                            continue;
                        }
                    }
                }
                // 解析失败，原样保留 '%'
                out.push(b'%');
            } else if b == b'+' {
                out.push(b' ');
            } else {
                out.push(b);
            }
        }
        out
    };
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

fn sanitize_filename(filename: &str) -> String {
    // 拒绝路径穿越和绝对路径：有 ".." 或以 "/" 开头，则只取最后一段
    let filename = if filename.contains("..") || filename.starts_with('/') {
        // 仅保留最后一个路径段
        filename
            .split('/')
            .rfind(|s| !s.is_empty() && *s != ".." && *s != ".")
            .unwrap_or("file")
    } else {
        filename
    };

    // 按 "/" 分割，对每段分别 sanitize，然后重新用 "/" 拼接（保留子目录结构）
    filename
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|segment| {
            segment
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn get_unique_file_path(receive_dir: &Path, safe_name: &str, allow_overwrite: bool) -> PathBuf {
    let mut path = receive_dir.join(safe_name);
    if allow_overwrite || !path.exists() {
        return path;
    }
    // 分离父目录、文件名主体和扩展名，仅在文件名主体后追加计数器
    let parent = path.parent().unwrap_or(receive_dir).to_path_buf();
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
        path = parent.join(new_name);
        if !path.exists() {
            break;
        }
    }
    path
}

/// 获取本机主机名用于 mDNS 实例名，失败时回退到 "aerosync"
fn hostname_for_mdns() -> String {
    hostname::get()
        .ok()
        .and_then(|s| s.into_string().ok())
        .unwrap_or_else(|| "aerosync".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── sanitize_filename ─────────────────────────────────────────────────────

    #[test]
    fn test_sanitize_filename_normal() {
        assert_eq!(
            sanitize_filename("hello-world_v1.2.bin"),
            "hello-world_v1.2.bin"
        );
    }

    #[test]
    fn test_sanitize_filename_preserves_subpath() {
        // 子路径分隔符应保留
        let result = sanitize_filename("subdir/file.bin");
        assert_eq!(result, "subdir/file.bin");
    }

    #[test]
    fn test_sanitize_filename_replaces_slashes_spaces() {
        let result = sanitize_filename("my file name.txt");
        assert_eq!(result, "my_file_name.txt");
    }

    #[test]
    fn test_sanitize_filename_path_traversal_stripped() {
        // ".." 路径穿越：只保留最后一个段
        let result = sanitize_filename("../../etc/passwd");
        assert!(
            !result.contains(".."),
            "result should not contain ..: {}",
            result
        );
        assert!(
            !result.starts_with('/'),
            "result should not start with /: {}",
            result
        );
    }

    #[test]
    fn test_sanitize_filename_absolute_path_stripped() {
        let result = sanitize_filename("/etc/passwd");
        assert!(
            !result.starts_with('/'),
            "result should not start with /: {}",
            result
        );
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
        let path = get_unique_file_path(dir.path(), "new.bin", false);
        assert_eq!(path, dir.path().join("new.bin"));
    }

    #[test]
    fn test_get_unique_path_with_collision_appends_counter() {
        let dir = TempDir::new().unwrap();
        // Create the file so it exists
        std::fs::write(dir.path().join("existing.bin"), b"data").unwrap();
        let path = get_unique_file_path(dir.path(), "existing.bin", false);
        assert_eq!(path, dir.path().join("existing_1.bin"));
    }

    #[test]
    fn test_get_unique_path_overwrite_returns_original() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("file.bin"), b"data").unwrap();
        let path = get_unique_file_path(dir.path(), "file.bin", true);
        assert_eq!(path, dir.path().join("file.bin"));
    }

    #[test]
    fn test_get_unique_path_multiple_collisions() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("data.bin"), b"a").unwrap();
        std::fs::write(dir.path().join("data_1.bin"), b"b").unwrap();
        let path = get_unique_file_path(dir.path(), "data.bin", false);
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
        let receiver = FileReceiver::new(ServerConfig {
            http_port: 9999,
            ..ServerConfig::default()
        });
        assert_eq!(receiver.get_config().await.http_port, 9999);
    }

    #[tokio::test]
    async fn test_receiver_update_config() {
        let receiver = FileReceiver::new(ServerConfig::default());
        let new_cfg = ServerConfig {
            http_port: 8888,
            ..ServerConfig::default()
        };
        receiver.update_config(new_cfg).await.unwrap();
        assert_eq!(receiver.get_config().await.http_port, 8888);
    }

    #[tokio::test]
    async fn test_receiver_get_server_urls_http_only() {
        let receiver = FileReceiver::new(ServerConfig {
            bind_address: "0.0.0.0".to_string(),
            http_port: 7788,
            enable_quic: false,
            ..ServerConfig::default()
        });
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

        let cfg = ServerConfig {
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

        let part = reqwest::multipart::Part::bytes(b"real content".to_vec())
            .file_name("tampered.bin")
            .mime_str("application/octet-stream")
            .unwrap();
        let form = reqwest::multipart::Form::new().part("file", part);

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{}/upload", port))
            .header(
                "X-File-Hash",
                "0000000000000000000000000000000000000000000000000000000000000000",
            )
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

    // ── 分片上传集成测试 ──────────────────────────────────────────────────────

    async fn start_test_receiver() -> (FileReceiver, u16) {
        // 随机端口
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let cfg = ServerConfig {
            http_port: port,
            bind_address: "127.0.0.1".to_string(),
            enable_quic: false,
            receive_directory: tempfile::tempdir().unwrap().keep(),
            ..ServerConfig::default()
        };
        let mut recv = FileReceiver::new(cfg);
        recv.start().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (recv, port)
    }

    #[tokio::test]
    async fn test_chunk_upload_and_complete() {
        use sha2::{Digest, Sha256};

        let (mut receiver, port) = start_test_receiver().await;
        let base = format!("http://127.0.0.1:{}", port);
        let client = reqwest::Client::new();
        let task_id = uuid::Uuid::new_v4();
        let filename = "chunked_test.bin";
        let data = b"CHUNK_DATA_BLOCK"; // 16 bytes
        let total_chunks = 3u32;
        let total_size = (data.len() * total_chunks as usize) as u64;
        let chunk_size = data.len() as u64;

        // Upload 3 chunks with new API (total_size + chunk_size required)
        for i in 0..total_chunks {
            let url = format!(
                "{}/upload/chunk?task_id={}&chunk_index={}&total_chunks={}&filename={}&total_size={}&chunk_size={}",
                base, task_id, i, total_chunks, filename, total_size, chunk_size
            );
            let resp = client.post(&url).body(data.to_vec()).send().await.unwrap();
            assert!(
                resp.status().is_success(),
                "chunk {} failed: {:?}",
                i,
                resp.status()
            );
        }

        // /upload/complete: SHA-256 校验 + 更新记录
        let mut hasher = Sha256::new();
        for _ in 0..total_chunks {
            hasher.update(data);
        }
        let sha = hex::encode(hasher.finalize());

        let complete_url = format!(
            "{}/upload/complete?task_id={}&filename={}&total_chunks={}&total_size={}&sha256={}",
            base, task_id, filename, total_chunks, total_size, sha
        );
        let resp = client.post(&complete_url).send().await.unwrap();
        assert!(
            resp.status().is_success(),
            "complete failed: {:?}",
            resp.status()
        );

        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["status"], "complete");
        assert_eq!(body["size"].as_u64().unwrap(), total_size);

        receiver.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_chunk_complete_missing_chunk_returns_404() {
        // New behavior: if no chunks were uploaded (inprogress file missing), complete returns 404
        let (mut receiver, port) = start_test_receiver().await;
        let base = format!("http://127.0.0.1:{}", port);
        let client = reqwest::Client::new();
        let task_id = uuid::Uuid::new_v4();

        // Do NOT upload any chunks — complete should 404
        let complete_url = format!(
            "{}/upload/complete?task_id={}&filename=missing.bin&total_chunks=2&total_size=100",
            base, task_id
        );
        let resp = client.post(&complete_url).send().await.unwrap();
        // File not found → 404
        assert_eq!(resp.status().as_u16(), 404);

        receiver.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_chunk_complete_sha256_mismatch_returns_error() {
        let (mut receiver, port) = start_test_receiver().await;
        let base = format!("http://127.0.0.1:{}", port);
        let client = reqwest::Client::new();
        let task_id = uuid::Uuid::new_v4();
        let data = b"hello";
        let total_size = data.len() as u64;

        // Upload 1 chunk (single chunk, completes the file)
        let url = format!(
            "{}/upload/chunk?task_id={}&chunk_index=0&total_chunks=1&filename=hash_test.bin&total_size={}&chunk_size={}",
            base, task_id, total_size, total_size
        );
        client.post(&url).body(data.to_vec()).send().await.unwrap();

        // Wait for rename to complete
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let complete_url = format!(
            "{}/upload/complete?task_id={}&filename=hash_test.bin&total_chunks=1&total_size={}&sha256=wrong_hash",
            base, task_id, total_size
        );
        let resp = client.post(&complete_url).send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 400);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(body["error"].as_str().unwrap().contains("SHA-256"));

        receiver.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_chunk_complete_with_correct_sha256() {
        use sha2::{Digest, Sha256};

        let (mut receiver, port) = start_test_receiver().await;
        let base = format!("http://127.0.0.1:{}", port);
        let client = reqwest::Client::new();
        let task_id = uuid::Uuid::new_v4();
        let data = b"verified content";
        let total_size = data.len() as u64;

        // Upload single chunk
        let url = format!(
            "{}/upload/chunk?task_id={}&chunk_index=0&total_chunks=1&filename=verified.bin&total_size={}&chunk_size={}",
            base, task_id, total_size, total_size
        );
        client.post(&url).body(data.to_vec()).send().await.unwrap();

        // Wait for rename
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut hasher = Sha256::new();
        hasher.update(data);
        let sha = hex::encode(hasher.finalize());

        let complete_url = format!(
            "{}/upload/complete?task_id={}&filename=verified.bin&total_chunks=1&total_size={}&sha256={}",
            base, task_id, total_size, sha
        );
        let resp = client.post(&complete_url).send().await.unwrap();
        assert!(resp.status().is_success());
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["sha256"].as_str().unwrap(), sha);

        receiver.stop().await.unwrap();
    }

    // ── HTTPS 集成测试 ────────────────────────────────────────────────────────

    /// 启动一个带自签名证书的 HTTPS 接收端，返回 (receiver, https_port, temp_dir)
    async fn start_https_test_receiver() -> (FileReceiver, u16, tempfile::TempDir) {
        let https_port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let dir = tempfile::tempdir().unwrap();
        let cfg = ServerConfig {
            bind_address: "127.0.0.1".to_string(),
            enable_http: false,
            enable_quic: false,
            enable_https: true,
            https_port,
            receive_directory: dir.path().to_path_buf(),
            ..ServerConfig::default()
        };
        let mut recv = FileReceiver::new(cfg);
        recv.start().await.unwrap();
        // warp TLS 握手需要稍多初始化时间
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        (recv, https_port, dir)
    }

    /// 构建一个接受自签名证书的 reqwest 客户端（仅供测试，rustls-tls 与 warp TLS 兼容）
    fn insecure_https_client() -> reqwest::Client {
        reqwest::ClientBuilder::new()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn test_https_server_starts_and_health_responds() {
        let (mut receiver, https_port, _dir) = start_https_test_receiver().await;

        let client = insecure_https_client();
        let url = format!("https://127.0.0.1:{}/health", https_port);
        let resp = client
            .get(&url)
            .send()
            .await
            .expect("HTTPS /health request failed");

        assert!(
            resp.status().is_success(),
            "expected 200, got {}",
            resp.status()
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(
            body.get("status").is_some(),
            "/health should contain 'status' field"
        );

        receiver.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_https_upload_file() {
        let (mut receiver, https_port, dir) = start_https_test_receiver().await;

        let client = insecure_https_client();
        let base = format!("https://127.0.0.1:{}", https_port);
        let data = b"hello from HTTPS upload test";

        // 单文件上传（multipart）
        let form = reqwest::multipart::Form::new().part(
            "file",
            reqwest::multipart::Part::bytes(data.to_vec()).file_name("https_test.txt"),
        );
        let resp = client
            .post(format!("{}/upload", base))
            .multipart(form)
            .send()
            .await
            .expect("HTTPS upload request failed");

        assert!(
            resp.status().is_success(),
            "upload failed: {}",
            resp.status()
        );

        // 验证文件落盘
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let saved = dir.path().join("https_test.txt");
        assert!(saved.exists(), "uploaded file should exist on disk");
        assert_eq!(std::fs::read(&saved).unwrap(), data);

        receiver.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_https_external_cert_loads_correctly() {
        // 用 generate_self_signed_pem 生成证书写入临时文件，模拟"外部证书"路径
        let cert_dir = tempfile::tempdir().unwrap();
        let (cert_pem, key_pem) = super::generate_self_signed_pem().unwrap();
        let cert_path = cert_dir.path().join("test.crt");
        let key_path = cert_dir.path().join("test.key");
        std::fs::write(&cert_path, &cert_pem).unwrap();
        std::fs::write(&key_path, &key_pem).unwrap();

        let https_port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let recv_dir = tempfile::tempdir().unwrap();
        let cfg = ServerConfig {
            bind_address: "127.0.0.1".to_string(),
            enable_http: false,
            enable_quic: false,
            enable_https: true,
            https_port,
            tls: Some(TlsConfig {
                cert_path,
                key_path,
            }),
            receive_directory: recv_dir.path().to_path_buf(),
            ..ServerConfig::default()
        };
        let mut recv = FileReceiver::new(cfg);
        recv.start().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let client = insecure_https_client();
        let url = format!("https://127.0.0.1:{}/health", https_port);
        let resp = client
            .get(&url)
            .send()
            .await
            .expect("external cert HTTPS /health request failed");
        assert!(
            resp.status().is_success(),
            "external cert HTTPS should serve /health"
        );

        recv.stop().await.unwrap();
    }
}

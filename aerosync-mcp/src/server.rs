use std::sync::Arc;
use zeroize::Zeroizing;
use tokio::sync::Mutex;

use aerosync_core::{
    auth::AuthConfig,
    discovery::AeroSyncMdns,
    server::{FileReceiver, ServerConfig},
    transfer::{TransferConfig, TransferEngine, TransferTask},
    FileManager, HistoryQuery, HistoryStore,
};
use aerosync_protocols::{http::HttpConfig, quic::QuicConfig, AutoAdapter};
use rmcp::{
    model::*,
    schemars,
    tool, tool_handler, tool_router, ServerHandler,
};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

// ─────────────────────── 工具参数结构体 ────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendFileParams {
    /// 源文件的绝对或相对路径
    pub source: String,
    /// 目标地址：host:port 或 http://host:port/upload 或 s3://bucket/key
    pub destination: String,
    /// 认证 Token（可选）
    pub token: Option<String>,
    /// 跳过 SHA-256 完整性校验（默认 false）
    pub no_verify: Option<bool>,
    /// 上传限速，如 "10MB"、"512KB"（可选）
    pub limit: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendDirectoryParams {
    /// 源目录的绝对或相对路径
    pub source: String,
    /// 目标地址
    pub destination: String,
    /// 认证 Token（可选）
    pub token: Option<String>,
    /// 跳过 SHA-256 完整性校验（默认 false）
    pub no_verify: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StartReceiverParams {
    /// HTTP 监听端口（默认 7788）
    pub port: Option<u16>,
    /// QUIC 监听端口（默认 7789）
    pub quic_port: Option<u16>,
    /// 文件保存目录（默认 ./received）
    pub save_to: Option<String>,
    /// 要求发送方携带此 Token（留空不启用认证）
    pub auth_token: Option<String>,
    /// 允许覆盖同名文件（默认 false）
    pub overwrite: Option<bool>,
    /// 启用 HTTPS（自动生成自签名证书，默认 false）
    pub https: Option<bool>,
    /// HTTPS 监听端口（默认 7790）
    pub https_port: Option<u16>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListHistoryParams {
    /// 最多返回 N 条（默认 20）
    pub limit: Option<usize>,
    /// 只显示成功记录
    pub success_only: Option<bool>,
    /// 只显示发送记录
    pub sent: Option<bool>,
    /// 只显示接收记录
    pub received: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DiscoverParams {
    /// 扫描等待时间（秒，默认 3）
    pub timeout: Option<u64>,
}

// ────────────────────────── MCP Server ─────────────────────────────────────

#[derive(Clone)]
pub struct AeroSyncMcpServer {
    receiver: Arc<Mutex<Option<FileReceiver>>>,
}

#[tool_router]
impl AeroSyncMcpServer {
    pub fn new() -> Self {
        Self {
            receiver: Arc::new(Mutex::new(None)),
        }
    }

    /// 发送单个文件到指定地址（自动协商 QUIC/HTTP 协议）
    #[tool(description = "Send a single file to a remote address. Automatically negotiates QUIC or HTTP protocol. Supports S3 and FTP destinations.")]
    async fn send_file(
        &self,
        rmcp::handler::server::wrapper::Parameters(params): rmcp::handler::server::wrapper::Parameters<SendFileParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let source = std::path::PathBuf::from(&params.source);

        let meta = match tokio::fs::metadata(&source).await {
            Ok(m) if m.is_file() => m,
            Ok(_) => {
                return Ok(CallToolResult::success(vec![Content::text(
                    json!({"success": false, "error": format!("'{}' is a directory, use send_directory instead", params.source)}).to_string()
                )]));
            }
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(
                    json!({"success": false, "error": format!("Cannot read source file: {}", e)}).to_string()
                )]));
            }
        };

        let file_size = meta.len();
        let dest_url = negotiate_protocol(&params.destination).await;
        let no_verify = params.no_verify.unwrap_or(false);
        let upload_limit_bps = params.limit.as_deref()
            .and_then(|s| aerosync_protocols::ratelimit::parse_limit(s))
            .unwrap_or(0);

        let http_config = HttpConfig {
            auth_token: params.token.clone().map(Zeroizing::new),
            upload_limit_bps,
            ..HttpConfig::default()
        };
        let quic_config = QuicConfig {
            auth_token: params.token.clone().map(Zeroizing::new),
            ..QuicConfig::default()
        };
        let adapter = Arc::new(AutoAdapter::new(http_config, quic_config));
        let config = TransferConfig {
            auth_token: params.token.clone().map(Zeroizing::new),
            ..TransferConfig::default()
        };
        let engine = TransferEngine::new(config);
        engine.start(adapter).await.map_err(|e| {
            rmcp::ErrorData::internal_error(format!("Failed to start transfer engine: {}", e), None)
        })?;

        let sha256 = if !no_verify {
            FileManager::compute_sha256(&source).await.ok()
        } else {
            None
        };

        let rel = source.file_name()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| source.clone());
        let task_dest = format!("{}/{}", dest_url.trim_end_matches('/'), rel.display());

        let mut task = TransferTask::new_upload(source.clone(), task_dest, file_size);
        task.sha256 = sha256;
        let task_id = task.id;

        engine.add_transfer(task).await.map_err(|e| {
            rmcp::ErrorData::internal_error(format!("Failed to add transfer: {}", e), None)
        })?;

        let monitor = engine.get_progress_monitor().await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(600);

        loop {
            let done = {
                let m = monitor.read().await;
                let stats = m.get_stats();
                stats.completed_files + stats.failed_files >= stats.total_files
            };
            if done { break; }
            if tokio::time::Instant::now() >= deadline {
                return Ok(CallToolResult::success(vec![Content::text(
                    json!({"success": false, "error": "Transfer timed out after 10 minutes"}).to_string()
                )]));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        let m = monitor.read().await;
        let stats = m.get_stats();

        if stats.failed_files > 0 {
            let error_msg = m.get_active_transfers().iter()
                .find(|t| t.task_id == task_id)
                .and_then(|t| match &t.status {
                    aerosync_core::progress::TransferStatus::Failed(e) => Some(e.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| "Unknown error".to_string());

            Ok(CallToolResult::success(vec![Content::text(
                json!({
                    "success": false,
                    "error": error_msg,
                    "file": params.source,
                    "destination": params.destination
                }).to_string()
            )]))
        } else {
            let speed_mb = stats.overall_speed / 1_048_576.0;
            Ok(CallToolResult::success(vec![Content::text(
                json!({
                    "success": true,
                    "data": {
                        "file": params.source,
                        "destination": params.destination,
                        "protocol": if dest_url.starts_with("quic") { "QUIC" } else { "HTTP" },
                        "size_bytes": file_size,
                        "avg_speed_mbs": format!("{:.2}", speed_mb)
                    }
                }).to_string()
            )]))
        }
    }

    /// 递归发送整个目录到远端，保留子目录结构
    #[tool(description = "Recursively send an entire directory to a remote address. Preserves directory structure. All files are sent concurrently.")]
    async fn send_directory(
        &self,
        rmcp::handler::server::wrapper::Parameters(params): rmcp::handler::server::wrapper::Parameters<SendDirectoryParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let source = std::path::PathBuf::from(&params.source);

        match tokio::fs::metadata(&source).await {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Ok(CallToolResult::success(vec![Content::text(
                    json!({"success": false, "error": format!("'{}' is a file, use send_file instead", params.source)}).to_string()
                )]));
            }
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(
                    json!({"success": false, "error": format!("Cannot read source directory: {}", e)}).to_string()
                )]));
            }
        }

        let files = match collect_files_recursive(&source, &source).await {
            Ok(f) => f,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(
                    json!({"success": false, "error": format!("Failed to list directory: {}", e)}).to_string()
                )]));
            }
        };

        if files.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                json!({"success": true, "data": {"message": "Directory is empty, nothing to send", "files_sent": 0}}).to_string()
            )]));
        }

        let total_size: u64 = files.iter().map(|f| f.2).sum();
        let dest_url = negotiate_protocol(&params.destination).await;
        let no_verify = params.no_verify.unwrap_or(false);

        let http_config = HttpConfig {
            auth_token: params.token.clone().map(Zeroizing::new),
            ..HttpConfig::default()
        };
        let quic_config = QuicConfig {
            auth_token: params.token.clone().map(Zeroizing::new),
            ..QuicConfig::default()
        };
        let adapter = Arc::new(AutoAdapter::new(http_config, quic_config));
        let config = TransferConfig {
            auth_token: params.token.clone().map(Zeroizing::new),
            ..TransferConfig::default()
        };
        let engine = TransferEngine::new(config);
        engine.start(adapter).await.map_err(|e| {
            rmcp::ErrorData::internal_error(format!("Failed to start transfer engine: {}", e), None)
        })?;

        for (path, rel, size) in &files {
            let sha256 = if !no_verify {
                FileManager::compute_sha256(path).await.ok()
            } else {
                None
            };
            let task_dest = format!("{}/{}", dest_url.trim_end_matches('/'), rel.display());
            let mut task = TransferTask::new_upload(path.clone(), task_dest, *size);
            task.sha256 = sha256;
            engine.add_transfer(task).await.map_err(|e| {
                rmcp::ErrorData::internal_error(format!("Failed to add transfer: {}", e), None)
            })?;
        }

        let monitor = engine.get_progress_monitor().await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3600);

        loop {
            let done = {
                let m = monitor.read().await;
                let stats = m.get_stats();
                stats.completed_files + stats.failed_files >= stats.total_files
            };
            if done { break; }
            if tokio::time::Instant::now() >= deadline {
                return Ok(CallToolResult::success(vec![Content::text(
                    json!({"success": false, "error": "Transfer timed out"}).to_string()
                )]));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        let m = monitor.read().await;
        let stats = m.get_stats();
        let speed_mb = stats.overall_speed / 1_048_576.0;

        Ok(CallToolResult::success(vec![Content::text(
            json!({
                "success": stats.failed_files == 0,
                "data": {
                    "directory": params.source,
                    "destination": params.destination,
                    "total_files": stats.total_files,
                    "completed": stats.completed_files,
                    "failed": stats.failed_files,
                    "total_size_bytes": total_size,
                    "avg_speed_mbs": format!("{:.2}", speed_mb)
                }
            }).to_string()
        )]))
    }

    /// 启动文件接收端服务器（后台持续运行，直到调用 stop_receiver）
    #[tool(description = "Start a file receiver server in the background. The server listens for incoming files and saves them to the specified directory. Use stop_receiver to shut it down.")]
    async fn start_receiver(
        &self,
        rmcp::handler::server::wrapper::Parameters(params): rmcp::handler::server::wrapper::Parameters<StartReceiverParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let mut lock = self.receiver.lock().await;

        if lock.is_some() {
            return Ok(CallToolResult::success(vec![Content::text(
                json!({"success": false, "error": "Receiver is already running. Call stop_receiver first."}).to_string()
            )]));
        }

        let port = params.port.unwrap_or(7788);
        let quic_port = params.quic_port.unwrap_or(7789);
        let save_to = std::path::PathBuf::from(
            params.save_to.as_deref().unwrap_or("./received")
        );
        let overwrite = params.overwrite.unwrap_or(false);

        let auth_cfg = params.auth_token.map(|token| AuthConfig {
            enable_auth: true,
            secret_key: token,
            token_lifetime_hours: 24,
            allowed_ips: vec![],
        });

        let config = ServerConfig {
            http_port: port,
            quic_port,
            bind_address: "0.0.0.0".to_string(),
            receive_directory: save_to.clone(),
            allow_overwrite: overwrite,
            auth: auth_cfg,
            enable_http: true,
            enable_quic: true,
            enable_metrics: true,
            enable_ws: true,
            enable_https: params.https.unwrap_or(false),
            https_port: params.https_port.unwrap_or(7790),
            ..ServerConfig::default()
        };

        let mut receiver = FileReceiver::new(config);
        match receiver.start().await {
            Ok(_) => {
                *lock = Some(receiver);
                Ok(CallToolResult::success(vec![Content::text(
                    json!({
                        "success": true,
                        "data": {
                            "http_port": port,
                            "quic_port": quic_port,
                            "save_to": save_to.display().to_string(),
                            "overwrite": overwrite,
                            "http_url": format!("http://0.0.0.0:{}", port),
                            "message": "Receiver started. Use get_receiver_status to check received files."
                        }
                    }).to_string()
                )]))
            }
            Err(e) => Ok(CallToolResult::success(vec![Content::text(
                json!({"success": false, "error": format!("Failed to start receiver: {}", e)}).to_string()
            )])),
        }
    }

    /// 停止当前运行的接收端服务器
    #[tool(description = "Stop the currently running file receiver server.")]
    async fn stop_receiver(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let mut lock = self.receiver.lock().await;

        match lock.take() {
            Some(mut receiver) => {
                match receiver.stop().await {
                    Ok(_) => Ok(CallToolResult::success(vec![Content::text(
                        json!({"success": true, "data": {"message": "Receiver stopped"}}).to_string()
                    )])),
                    Err(e) => Ok(CallToolResult::success(vec![Content::text(
                        json!({"success": false, "error": format!("Failed to stop receiver: {}", e)}).to_string()
                    )])),
                }
            }
            None => Ok(CallToolResult::success(vec![Content::text(
                json!({"success": false, "error": "No receiver is running"}).to_string()
            )])),
        }
    }

    /// 查询接收端状态和已接收的文件列表
    #[tool(description = "Get the status of the receiver server and list all files received so far.")]
    async fn get_receiver_status(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let lock = self.receiver.lock().await;

        match &*lock {
            None => Ok(CallToolResult::success(vec![Content::text(
                json!({
                    "success": true,
                    "data": {
                        "running": false,
                        "message": "No receiver is running. Use start_receiver to start one."
                    }
                }).to_string()
            )])),
            Some(receiver) => {
                let files = receiver.get_received_files().await;
                let file_list: Vec<serde_json::Value> = files.iter().map(|f| {
                    json!({
                        "name": f.original_name,
                        "path": f.saved_path.display().to_string(),
                        "size_bytes": f.size,
                        "sha256_prefix": f.sha256.as_deref().map(|h| &h[..h.len().min(16)])
                    })
                }).collect();

                Ok(CallToolResult::success(vec![Content::text(
                    json!({
                        "success": true,
                        "data": {
                            "running": true,
                            "received_files_count": files.len(),
                            "files": file_list
                        }
                    }).to_string()
                )]))
            }
        }
    }

    /// 查询传输历史记录
    #[tool(description = "List file transfer history. Returns recent send/receive records with speed, status, and error info.")]
    async fn list_history(
        &self,
        rmcp::handler::server::wrapper::Parameters(params): rmcp::handler::server::wrapper::Parameters<ListHistoryParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let store_path = HistoryStore::default_path();
        if !store_path.exists() {
            return Ok(CallToolResult::success(vec![Content::text(
                json!({"success": true, "data": {"records": [], "message": "No transfer history yet"}}).to_string()
            )]));
        }

        let store = match HistoryStore::new(&store_path).await {
            Ok(s) => s,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(
                    json!({"success": false, "error": format!("Cannot open history: {}", e)}).to_string()
                )]));
            }
        };

        let direction = if params.sent.unwrap_or(false) {
            Some("send".to_string())
        } else if params.received.unwrap_or(false) {
            Some("receive".to_string())
        } else {
            None
        };

        let q = HistoryQuery {
            direction,
            success_only: params.success_only.unwrap_or(false),
            limit: params.limit.unwrap_or(20),
            ..Default::default()
        };

        match store.query(&q).await {
            Ok(entries) => {
                let records: Vec<serde_json::Value> = entries.iter().map(|e| {
                    json!({
                        "filename": e.filename,
                        "direction": e.direction,
                        "protocol": e.protocol,
                        "success": e.success,
                        "avg_speed_kbs": e.avg_speed_bps as f64 / 1024.0,
                        "remote_ip": e.remote_ip,
                        "error": e.error
                    })
                }).collect();

                Ok(CallToolResult::success(vec![Content::text(
                    json!({
                        "success": true,
                        "data": {
                            "total": records.len(),
                            "records": records
                        }
                    }).to_string()
                )]))
            }
            Err(e) => Ok(CallToolResult::success(vec![Content::text(
                json!({"success": false, "error": format!("Query failed: {}", e)}).to_string()
            )])),
        }
    }

    /// 扫描局域网内的 AeroSync 接收端（通过 mDNS 发现）
    #[tool(description = "Scan the local network for AeroSync receivers using mDNS discovery. Returns a list of available receivers with their addresses and capabilities.")]
    async fn discover_receivers(
        &self,
        rmcp::handler::server::wrapper::Parameters(params): rmcp::handler::server::wrapper::Parameters<DiscoverParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let timeout = Duration::from_secs(params.timeout.unwrap_or(3));
        let mut peers = AeroSyncMdns::discover(timeout).await;
        peers.sort_by(|a, b| a.host.cmp(&b.host).then(a.port.cmp(&b.port)));

        let receivers: Vec<serde_json::Value> = peers.iter().map(|p| {
            json!({
                "name": p.name,
                "host": p.host,
                "port": p.port,
                "address": p.addr(),
                "version": p.version,
                "ws_enabled": p.ws_enabled,
                "auth_required": p.auth_required
            })
        }).collect();

        Ok(CallToolResult::success(vec![Content::text(
            json!({
                "success": true,
                "data": {
                    "found": receivers.len(),
                    "receivers": receivers
                }
            }).to_string()
        )]))
    }
}

#[tool_handler(
    name = "aerosync-mcp",
    instructions = "AeroSync file transfer tools. Use send_file or send_directory to transfer files to a remote host. Use start_receiver to listen for incoming files, get_receiver_status to check received files, and stop_receiver when done. Use discover_receivers to find AeroSync peers on the local network. Use list_history to view past transfers."
)]
impl ServerHandler for AeroSyncMcpServer {}

// ──────────────────────── 辅助函数 ─────────────────────────────────────────

/// 自动协商协议：如果对端是 AeroSync 则升级 QUIC，否则降级 HTTP
async fn negotiate_protocol(dest: &str) -> String {
    if dest.starts_with("http://")
        || dest.starts_with("https://")
        || dest.starts_with("quic://")
        || dest.starts_with("s3://")
        || dest.starts_with("ftp://")
    {
        return dest.to_string();
    }

    let health_url = format!("http://{}/health", dest);
    let probe = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build();

    if let Ok(client) = probe {
        if let Ok(resp) = client.get(&health_url).send().await {
            let is_aerosync = resp
                .headers()
                .get("x-aerosync")
                .and_then(|v| v.to_str().ok())
                .map(|v| v == "true")
                .unwrap_or(false);

            if is_aerosync {
                let quic_dest = if let Some(colon_pos) = dest.rfind(':') {
                    let host = &dest[..colon_pos];
                    let http_port: u16 = dest[colon_pos + 1..].parse().unwrap_or(7788);
                    format!("quic://{}:{}/upload", host, http_port + 1)
                } else {
                    format!("quic://{}:7789/upload", dest)
                };
                return quic_dest;
            }
        }
    }

    format!("http://{}/upload", dest)
}

/// 递归收集目录下所有文件，返回 (绝对路径, 相对路径, 大小) 三元组
async fn collect_files_recursive(
    base: &std::path::Path,
    dir: &std::path::Path,
) -> anyhow::Result<Vec<(std::path::PathBuf, std::path::PathBuf, u64)>> {
    let mut result = Vec::new();
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let meta = entry.metadata().await?;
        if meta.is_file() {
            let rel = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
            result.push((path, rel, meta.len()));
        } else if meta.is_dir() {
            let sub = collect_files_recursive_boxed(base.to_path_buf(), path);
            result.extend(sub.await?);
        }
    }
    Ok(result)
}

fn collect_files_recursive_boxed(
    base: std::path::PathBuf,
    dir: std::path::PathBuf,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Vec<(std::path::PathBuf, std::path::PathBuf, u64)>>> + Send>> {
    Box::pin(async move {
        collect_files_recursive(&base, &dir).await
    })
}

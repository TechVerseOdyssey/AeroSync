//! AutoAdapter: 根据 destination URL 自动选择 HTTP 或 QUIC 协议，
//! 实现 aerosync-core 的 ProtocolAdapter trait，由 main.rs 注入。

use crate::core::resume::{ResumeState, ResumeStore};
use crate::core::transfer::{ProtocolAdapter, ProtocolProgress, TransferTask};
use crate::core::{AeroSyncError, Result};
use aerosync_domain::storage::ResumeStorage;
use async_trait::async_trait;
use reqwest::Client;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

#[cfg(feature = "ftp")]
use crate::protocols::ftp::{FtpConfig, FtpTransfer};
use crate::protocols::http::{HttpConfig, HttpReceiptAck, HttpTransfer};
#[cfg(feature = "quic")]
use crate::protocols::quic::{QuicConfig, QuicTransfer};
#[cfg(feature = "s3")]
use crate::protocols::s3::{S3Config, S3Transfer};
use crate::protocols::traits::{TransferProgress as ProtoProgress, TransferProtocol};
#[cfg(feature = "wan-rendezvous")]
use crate::wan::rendezvous::RendezvousClient;

pub struct AutoAdapter {
    http_config: HttpConfig,
    #[cfg(feature = "quic")]
    quic_config_base: QuicConfig,
    shared_client: Arc<Client>,
    #[cfg(feature = "s3")]
    s3_config: Option<S3Config>,
    #[cfg(feature = "ftp")]
    ftp_config: Option<FtpConfig>,
    /// 可选的断点续传持久化存储，注入后每完成一个分片自动保存进度。
    /// Phase 3.4d-ii: stored as `Arc<dyn ResumeStorage>` so this
    /// adapter can host any storage backend (in-memory, SQLite,
    /// future cloud-shim) — the concrete [`ResumeStore`] still works
    /// thanks to the generic [`Self::with_resume_store`] builder.
    resume_store: Option<Arc<dyn ResumeStorage>>,
    /// Optional inbox stitched into every HTTP `HttpTransfer` this
    /// adapter constructs (via [`HttpTransfer::with_receipt_sink`])
    /// so receiver-side application acks parsed off the `/upload`
    /// JSON response (`receipt_ack` field, RFC-002 §6.4) are
    /// forwarded to the engine's
    /// [`crate::core::transfer::TransferEngine::http_receipt_inbox`]
    /// forwarder task, which in turn drives the matching
    /// `Receipt<Sender>` to its terminal state. `None` (default)
    /// keeps the legacy behaviour where the receipt parks at
    /// `Processing` and a separate orchestration layer is expected
    /// to apply the terminal event by hand.
    engine_receipt_inbox: Option<mpsc::UnboundedSender<HttpReceiptAck>>,
    #[cfg(feature = "wan-rendezvous")]
    rendezvous: Option<RendezvousClient>,
}

impl AutoAdapter {
    #[cfg(feature = "quic")]
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
            #[cfg(feature = "s3")]
            s3_config: None,
            #[cfg(feature = "ftp")]
            ftp_config: None,
            resume_store: None,
            engine_receipt_inbox: None,
            #[cfg(feature = "wan-rendezvous")]
            rendezvous: None,
        }
    }

    /// HTTP-only constructor (used when the `quic` feature is disabled).
    /// Mirrors [`Self::new`] but omits the QUIC base config since the
    /// QUIC transport types are not compiled in.
    #[cfg(not(feature = "quic"))]
    pub fn new(http_config: HttpConfig) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(http_config.timeout_seconds))
            .build()
            .unwrap_or_default();
        Self {
            http_config,
            shared_client: Arc::new(client),
            #[cfg(feature = "s3")]
            s3_config: None,
            #[cfg(feature = "ftp")]
            ftp_config: None,
            resume_store: None,
            engine_receipt_inbox: None,
            #[cfg(feature = "wan-rendezvous")]
            rendezvous: None,
        }
    }

    /// Builder: 注入文件后端 [`ResumeStore`]。Concrete-typed for
    /// back-compat with existing call sites (`src/main.rs`, integration
    /// tests). See [`Self::with_resume_storage`] for the trait-object
    /// variant added in Phase 3.4d-ii.
    pub fn with_resume_store(mut self, store: Arc<ResumeStore>) -> Self {
        self.resume_store = Some(store as Arc<dyn ResumeStorage>);
        self
    }

    /// Builder: 注入任意 [`ResumeStorage`] 实现。Phase 3.4d-ii
    /// companion to [`Self::with_resume_store`] for tests and future
    /// backends that don't want to materialise a concrete
    /// [`ResumeStore`].
    pub fn with_resume_storage(mut self, store: Arc<dyn ResumeStorage>) -> Self {
        self.resume_store = Some(store);
        self
    }

    /// Builder: stitch the engine's HTTP receipt inbox (obtained from
    /// [`crate::core::transfer::TransferEngine::http_receipt_inbox`])
    /// into every `HttpTransfer` this adapter constructs. See
    /// `AutoAdapter::engine_receipt_inbox` for the full contract.
    /// This is the canonical hook the Python `Client.__aenter__` uses
    /// to wire receiver→sender ack propagation on the HTTP transport.
    pub fn with_engine_receipt_inbox(mut self, tx: mpsc::UnboundedSender<HttpReceiptAck>) -> Self {
        self.engine_receipt_inbox = Some(tx);
        self
    }

    /// Internal: build an [`HttpTransfer`] off the shared reqwest
    /// client + the adapter's HTTP config, optionally stitching in
    /// the resume store and the engine receipt inbox sink. Centralised
    /// so every upload path picks up both knobs uniformly without
    /// repeating the construction boilerplate at each call site.
    fn build_http_transfer(&self, with_resume: bool) -> HttpTransfer {
        let mut ht = if with_resume {
            if let Some(store) = &self.resume_store {
                HttpTransfer::new_with_client_and_resume_storage(
                    Arc::clone(&self.shared_client),
                    self.http_config.clone(),
                    Arc::clone(store),
                )
            } else {
                HttpTransfer::new_with_client(
                    Arc::clone(&self.shared_client),
                    self.http_config.clone(),
                )
            }
        } else {
            HttpTransfer::new_with_client(Arc::clone(&self.shared_client), self.http_config.clone())
        };
        if let Some(inbox) = &self.engine_receipt_inbox {
            ht = ht.with_receipt_sink(inbox.clone());
        }
        ht
    }

    /// Builder: 配置 S3 协议支持（MinIO 或 AWS S3）
    #[cfg(feature = "s3")]
    pub fn with_s3(mut self, cfg: S3Config) -> Self {
        self.s3_config = Some(cfg);
        self
    }

    /// Builder: 配置 FTP 协议支持
    #[cfg(feature = "ftp")]
    pub fn with_ftp(mut self, cfg: FtpConfig) -> Self {
        self.ftp_config = Some(cfg);
        self
    }

    /// RFC-004: Bearer JWT used for `GET /v1/peers/{name}` (lookup scope).
    #[cfg(feature = "wan-rendezvous")]
    pub fn with_rendezvous_token(mut self, bearer_token: String) -> Self {
        self.rendezvous = Some(RendezvousClient::new(
            Arc::clone(&self.shared_client),
            bearer_token,
        ));
        self
    }

    /// Install rendezvous lookup when `AEROSYNC_RENDEZVOUS_TOKEN` is non-empty.
    #[cfg(feature = "wan-rendezvous")]
    pub fn with_rendezvous_token_from_env(self) -> Self {
        match std::env::var("AEROSYNC_RENDEZVOUS_TOKEN") {
            Ok(token) if !token.trim().is_empty() => self.with_rendezvous_token(token),
            _ => self,
        }
    }

    async fn resolve_task_destination(&self, task: &TransferTask) -> Result<TransferTask> {
        #[cfg(feature = "wan-rendezvous")]
        if let Some(rv) = &self.rendezvous {
            let dest = task.destination.trim();
            let (prefix, rel_path) = match dest.find('/') {
                Some(i) => (&dest[..i], &dest[i + 1..]),
                None => (dest, ""),
            };
            if let Some((peer_name, authority)) =
                crate::wan::rendezvous::parse_peer_at_rendezvous(prefix)
            {
                let rendezvous_base = format!("http://{}", authority.trim_start_matches('/'));
                let info = rv.lookup_peer(&rendezvous_base, &peer_name).await?;
                let addr = info.observed_addr.ok_or_else(|| {
                    AeroSyncError::InvalidConfig(
                        "rendezvous: peer has no observed_addr yet".to_string(),
                    )
                })?;
                let addr = addr
                    .trim()
                    .trim_start_matches("http://")
                    .trim_start_matches("https://");
                let mut out = task.clone();
                out.destination = if rel_path.is_empty() {
                    format!("http://{addr}/upload")
                } else {
                    format!("http://{addr}/upload/{rel_path}")
                };
                return Ok(out);
            }
        }
        Ok(task.clone())
    }
}

#[async_trait]
impl ProtocolAdapter for AutoAdapter {
    async fn upload(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<ProtocolProgress>,
    ) -> Result<()> {
        let task = self.resolve_task_destination(task).await?;
        // S3 路由
        #[cfg(feature = "s3")]
        if task.destination.starts_with("s3://") {
            let cfg = self.s3_config.clone().ok_or_else(|| {
                AeroSyncError::InvalidConfig(
                    "S3 config not set; use AutoAdapter::with_s3()".to_string(),
                )
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
            return s3.upload_file(&task, tx).await;
        }

        // FTP 路由
        #[cfg(feature = "ftp")]
        if task.destination.starts_with("ftp://") {
            let cfg = self.ftp_config.clone().ok_or_else(|| {
                AeroSyncError::InvalidConfig(
                    "FTP config not set; use AutoAdapter::with_ftp()".to_string(),
                )
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
            return ft.upload_file(&task, tx).await;
        }

        #[cfg(feature = "quic")]
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
            return qt.upload_file(&task, tx).await;
        }

        // HTTP（包括 http:// 和 host:port 规范化后的地址）— also the
        // fallback when a quic://, s3://, or ftp:// URL arrives but
        // the corresponding feature was compiled out (the request will
        // simply fail at the HTTP layer with an unparseable URL).
        let ht = self.build_http_transfer(false);
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
        let normalized = normalize_http_upload_task(&task);
        ht.upload_file(&normalized, tx).await
    }

    async fn download(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<ProtocolProgress>,
    ) -> Result<()> {
        let task = self.resolve_task_destination(task).await?;
        // S3 路由
        #[cfg(feature = "s3")]
        if task.destination.starts_with("s3://") {
            let cfg = self
                .s3_config
                .clone()
                .ok_or_else(|| AeroSyncError::InvalidConfig("S3 config not set".to_string()))?;
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
            return s3.download_file(&task, tx).await;
        }

        // FTP 路由
        #[cfg(feature = "ftp")]
        if task.destination.starts_with("ftp://") {
            let cfg = self
                .ftp_config
                .clone()
                .ok_or_else(|| AeroSyncError::InvalidConfig("FTP config not set".to_string()))?;
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
            return ft.download_file(&task, tx).await;
        }

        #[cfg(feature = "quic")]
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
            return qt.download_file(&task, tx).await;
        }

        let ht = HttpTransfer::new_with_client(
            Arc::clone(&self.shared_client),
            self.http_config.clone(),
        );
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
        ht.download_file(&task, tx).await
        // NB: download path deliberately doesn't wire the receipt
        // sink — receipt acks are an upload-side concept (RFC-002 §6).
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
        let task = self.resolve_task_destination(task).await?;
        // S3/FTP/QUIC 不支持分片，fallback 到普通上传。
        // The matched-scheme list is gated so we don't false-positive
        // on a feature that isn't compiled in (the upload() fallback
        // would surface a clearer error in that case).
        let needs_fallback = task.destination.starts_with("quic://") && cfg!(feature = "quic")
            || task.destination.starts_with("s3://") && cfg!(feature = "s3")
            || task.destination.starts_with("ftp://") && cfg!(feature = "ftp");
        if needs_fallback {
            return self.upload(&task, progress_tx).await;
        }

        // 从完整 URL 中提取 base_url（scheme + host + port）
        let base_url = extract_base_url(&task.destination)?;
        let ht = self.build_http_transfer(true);

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

        ht.upload_chunked(
            &task.source_path,
            &base_url,
            state,
            task.metadata.as_ref(),
            tx,
        )
        .await
    }
}

/// Normalise the [`TransferTask::destination`] into a fully-qualified
/// HTTP `/upload` URL when the caller provided a bare `host:port` (the
/// shape that `aerosync.receiver(listen=...).address` exposes to the
/// Python SDK and that the README quickstart uses verbatim). Existing
/// destinations that already carry an `http://` / `https://` scheme
/// are returned untouched, so callers who construct full URLs by hand
/// (e.g. `tests/metadata_http_propagation.rs`) keep their semantics.
///
/// The returned task is a shallow clone with only `destination`
/// rewritten — the engine bridge keys off `task.id` and reads
/// `task.metadata`, both of which we preserve.
fn normalize_http_upload_task(task: &TransferTask) -> TransferTask {
    let dest = &task.destination;
    if dest.starts_with("http://") || dest.starts_with("https://") {
        return task.clone();
    }
    let mut normalized = task.clone();
    normalized.destination = format!("http://{}/upload", dest.trim_start_matches('/'));
    normalized
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

#[cfg(feature = "quic")]
fn resolve_quic_config(destination: &str, base: &QuicConfig) -> Result<QuicConfig> {
    let stripped = destination.strip_prefix("quic://").ok_or_else(|| {
        AeroSyncError::InvalidConfig(format!("Invalid QUIC URL: {}", destination))
    })?;

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
        pinned_server_certs: base.pinned_server_certs.clone(),
    })
}

#[cfg(all(test, feature = "quic"))]
mod tests {
    use super::*;
    use crate::core::transfer::TransferTask;

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
        base.auth_token = Some("secret-token".to_string().into());
        let result = resolve_quic_config("quic://127.0.0.1:7789", &base).unwrap();
        assert_eq!(result.auth_token, Some("secret-token".to_string().into()));
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
            metadata: None,
        };
        let adapter = AutoAdapter::new(HttpConfig::default(), default_quic_config());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let result = adapter.upload(&task, tx).await;
        // Should fail with a Network error (connection refused), not a config/parse error
        match result {
            Err(crate::core::AeroSyncError::Network(_)) => {} // expected
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
            metadata: None,
        };
        let adapter = AutoAdapter::new(HttpConfig::default(), default_quic_config());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let result = adapter.download(&task, tx).await;
        match result {
            Err(crate::core::AeroSyncError::Network(_)) => {}
            Err(e) => panic!("Unexpected error: {:?}", e),
            Ok(_) => panic!("Should not succeed without server"),
        }
    }
}

//! AutoAdapter: 根据 destination URL 自动选择 HTTP 或 QUIC 协议，
//! 实现 aerosync-core 的 ProtocolAdapter trait，由 main.rs 注入。

use crate::core::resume::{ResumeState, ResumeStore};
use crate::core::transfer::{ProtocolAdapter, ProtocolProgress, TransferTask};
use crate::core::{AeroSyncError, Result};
use aerosync_domain::storage::ResumeStorage;
use async_trait::async_trait;
use reqwest::Client;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use std::time::{Duration as StdDuration, SystemTime, UNIX_EPOCH};
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
#[cfg(all(feature = "quic", feature = "wan-rendezvous"))]
use crate::wan::{
    hole_punch::udp_punch_warmup,
    punch_signaling::{exchange_candidates_and_wait_punch_with_timeouts, RemoteCandidates},
};

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
    #[cfg(all(feature = "quic", feature = "wan-rendezvous"))]
    const R2_INITIATE_TIMEOUT: StdDuration = StdDuration::from_secs(8);
    #[cfg(all(feature = "quic", feature = "wan-rendezvous"))]
    const R2_WS_CONNECT_TIMEOUT: StdDuration = StdDuration::from_secs(5);
    #[cfg(all(feature = "quic", feature = "wan-rendezvous"))]
    const R2_PUNCH_TIMEOUT: StdDuration = StdDuration::from_secs(8);
    #[cfg(all(feature = "quic", feature = "wan-rendezvous"))]
    const R2_CONNECT_TIMEOUT: StdDuration = StdDuration::from_secs(8);

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

    /// Same as [`Self::with_rendezvous_token`] but sets `X-AeroSync-Namespace` (must match JWT `ns`).
    #[cfg(feature = "wan-rendezvous")]
    pub fn with_rendezvous_token_and_namespace(
        mut self,
        bearer_token: String,
        namespace: impl Into<String>,
    ) -> Self {
        self.rendezvous = Some(RendezvousClient::new_with_namespace(
            Arc::clone(&self.shared_client),
            bearer_token,
            namespace,
        ));
        self
    }

    /// Install rendezvous lookup when `AEROSYNC_RENDEZVOUS_TOKEN` is non-empty.
    /// Reads optional `AEROSYNC_RENDEZVOUS_NAMESPACE` for P2 multitenant lookup.
    #[cfg(feature = "wan-rendezvous")]
    pub fn with_rendezvous_token_from_env(mut self) -> Self {
        let ns = std::env::var("AEROSYNC_RENDEZVOUS_NAMESPACE").unwrap_or_default();
        if let Ok(token) = std::env::var("AEROSYNC_RENDEZVOUS_TOKEN") {
            if !token.trim().is_empty() {
                self.rendezvous = Some(RendezvousClient::new_with_namespace(
                    Arc::clone(&self.shared_client),
                    token,
                    ns,
                ));
            }
        }
        self
    }

    async fn resolve_task_destination(&self, task: &TransferTask) -> Result<TransferTask> {
        #[cfg(feature = "wan-rendezvous")]
        {
            let dest = task.destination.trim();
            let (prefix, rel_path) = split_destination_prefix_and_path(dest);
            if let Some((peer_name, authority)) =
                crate::wan::rendezvous::parse_peer_at_rendezvous(prefix)
            {
                let Some(rv) = &self.rendezvous else {
                    return Err(AeroSyncError::InvalidConfig(
                        "[R2_NO_TOKEN] destination uses peer@rendezvous but no lookup token is configured; set AEROSYNC_RENDEZVOUS_TOKEN".to_string(),
                    ));
                };
                let rendezvous_base = format!("http://{}", authority.trim_start_matches('/'));
                let info = rv.lookup_peer(&rendezvous_base, &peer_name).await?;
                let addr = info.observed_addr.ok_or_else(|| {
                    AeroSyncError::InvalidConfig(
                        format!(
                            "[R2_PEER_UNSEEN] rendezvous peer `{peer_name}` has no observed_addr yet; ensure receiver is online and heartbeating"
                        ),
                    )
                })?;
                let addr = addr
                    .trim()
                    .trim_start_matches("http://")
                    .trim_start_matches("https://");
                let mut out = task.clone();
                // Explicit path-suffix fallback semantics:
                // - `peer@rv-host`           -> `http://<observed>/upload`
                // - `peer@rv-host/path/tail` -> `http://<observed>/upload/path/tail`
                //
                // R2 punch/QUIC path intentionally only handles the no-suffix shape
                // (see `try_r2_upload`), so any suffix keeps the historical HTTP upload
                // behavior through rendezvous lookup without changing user-visible URLs.
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

    /// RFC-004 R2 upload path (phase-1): for `peer@rendezvous-host:port` without
    /// a path suffix, attempt `initiate_session -> WS candidates/punch_at -> QUIC`.
    /// Returns `None` when the task is not an R2 candidate so the caller can keep
    /// existing routing/fallback logic untouched.
    #[cfg(all(feature = "quic", feature = "wan-rendezvous"))]
    async fn try_r2_upload(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<ProtocolProgress>,
    ) -> Option<Result<()>> {
        let rv = self.rendezvous.as_ref()?;
        let dest = task.destination.trim();
        let (prefix, rel_path) = split_destination_prefix_and_path(dest);
        let (peer_name, authority) = crate::wan::rendezvous::parse_peer_at_rendezvous(prefix)?;
        if !rel_path.is_empty() {
            // Any `peer@host/path` destination is explicitly NOT an R2
            // punch candidate. We return `None` so caller falls back to
            // `resolve_task_destination` and preserves `/upload/{path}`.
            return None;
        }

        let rendezvous_base = format!("http://{}", authority.trim_start_matches('/'));
        let fut = async {
            let init = run_r2_stage_with_timeout(
                Self::R2_INITIATE_TIMEOUT,
                "R2_TIMEOUT_INITIATE",
                format!("initiate_session timed out for `{peer_name}`"),
                rv.initiate_session(&rendezvous_base, &peer_name),
            )
            .await
            .map_err(|e| match e {
                AeroSyncError::Network(s) if s.contains("[R2_TIMEOUT_INITIATE]") => {
                    AeroSyncError::Network(s)
                }
                other => AeroSyncError::Network(format!(
                    "[R2_INITIATE] initiate_session failed for `{peer_name}`: {other}"
                )),
            })?;
            let ws_url = rv
                .signaling_websocket_url(&rendezvous_base, &init.signaling.websocket_path)
                .map_err(|e| {
                    AeroSyncError::Network(format!("R2 signaling URL build failed: {e}"))
                })?;

            let socket = std::net::UdpSocket::bind("0.0.0.0:0")
                .map_err(|e| AeroSyncError::Network(format!("R2 UDP bind: {e}")))?;
            socket
                .set_nonblocking(true)
                .map_err(|e| AeroSyncError::Network(format!("R2 UDP nonblocking: {e}")))?;
            let local = socket
                .local_addr()
                .map_err(|e| AeroSyncError::Network(format!("R2 UDP local addr: {e}")))?;
            let local_s = local.to_string();
            let (punch, remotes) = exchange_candidates_and_wait_punch_with_timeouts(
                &ws_url,
                &local_s,
                vec![local_s.clone()],
                Self::R2_WS_CONNECT_TIMEOUT,
                Self::R2_PUNCH_TIMEOUT,
            )
            .await
            .map_err(|e| {
                AeroSyncError::Network(format!(
                    "[R2_SIGNALING] signaling/punch failed: {e}. The receiver must also participate in signaling."
                ))
            })?;

            let remote = pick_remote_candidate(&remotes).ok_or_else(|| {
                AeroSyncError::Network(
                    "[R2_CANDIDATE_EMPTY] signaling returned no parseable remote candidate addr"
                        .to_string(),
                )
            })?;
            wait_until_punch_time_ms(punch.timestamp_ms).await;
            let warm_socket = socket
                .try_clone()
                .map_err(|e| AeroSyncError::Network(format!("[R2_SOCKET] UDP clone: {e}")))?;
            udp_punch_warmup(&warm_socket, &[remote], 3)
                .map_err(|e| AeroSyncError::Network(format!("[R2_WARMUP] UDP warmup: {e}")))?;

            let mut qc = self.quic_config_base.clone();
            qc.server_addr = remote;
            qc.server_name = remote.ip().to_string();
            let qt = QuicTransfer::new_with_socket(qc, socket)?;
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
            run_r2_stage_with_timeout(
                Self::R2_CONNECT_TIMEOUT,
                "R2_TIMEOUT_CONNECT",
                "QUIC connect/upload did not complete".to_string(),
                qt.upload_file(task, tx),
            )
            .await
        };
        Some(fut.await)
    }
}

#[async_trait]
impl ProtocolAdapter for AutoAdapter {
    async fn upload(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<ProtocolProgress>,
    ) -> Result<()> {
        #[cfg(all(feature = "quic", feature = "wan-rendezvous"))]
        if let Some(r) = self.try_r2_upload(task, progress_tx.clone()).await {
            return r;
        }

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

/// Split destination into `prefix` and optional `/path/suffix`.
/// Returns `(prefix, rel_path_without_leading_slash)`.
///
/// Examples:
/// - `alice@rv.example:8787` -> (`alice@rv.example:8787`, ``)
/// - `alice@rv.example:8787/team/docs` -> (`alice@rv.example:8787`, `team/docs`)
fn split_destination_prefix_and_path(dest: &str) -> (&str, &str) {
    match dest.find('/') {
        Some(i) => (&dest[..i], &dest[i + 1..]),
        None => (dest, ""),
    }
}

#[cfg(all(feature = "quic", feature = "wan-rendezvous"))]
fn pick_remote_candidate(remotes: &[RemoteCandidates]) -> Option<SocketAddr> {
    let mut first_reflexive: Option<SocketAddr> = None;
    let mut first_local: Option<SocketAddr> = None;

    // Policy:
    // 1) Prefer any parseable IPv6 candidate.
    // 2) Otherwise prefer server-reflexive.
    // 3) Finally fall back to first parseable local candidate.
    for r in remotes {
        if let Ok(a) = r.server_reflexive.trim().parse::<SocketAddr>() {
            if a.is_ipv6() {
                return Some(a);
            }
            if first_reflexive.is_none() {
                first_reflexive = Some(a);
            }
        }
        for l in &r.local {
            if let Ok(a) = l.trim().parse::<SocketAddr>() {
                if a.is_ipv6() {
                    return Some(a);
                }
                if first_local.is_none() {
                    first_local = Some(a);
                }
            }
        }
    }
    first_reflexive.or(first_local)
}

#[cfg(all(feature = "quic", feature = "wan-rendezvous"))]
async fn wait_until_punch_time_ms(target_ms: u64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if target_ms <= now {
        return;
    }
    let delta = target_ms - now;
    let capped = delta.min(2_000);
    tokio::time::sleep(StdDuration::from_millis(capped)).await;
}

#[cfg(all(feature = "quic", feature = "wan-rendezvous"))]
async fn run_r2_stage_with_timeout<T, F>(
    timeout: StdDuration,
    timeout_code: &'static str,
    timeout_message: String,
    fut: F,
) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    tokio::time::timeout(timeout, fut).await.map_err(|_| {
        AeroSyncError::Network(format!(
            "[{timeout_code}] {timeout_message} after {} ms",
            timeout.as_millis()
        ))
    })?
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

    #[cfg(feature = "wan-rendezvous")]
    #[test]
    fn test_pick_remote_candidate_prefers_server_reflexive() {
        let remotes = vec![RemoteCandidates {
            from: "target".to_string(),
            server_reflexive: "127.0.0.1:7789".to_string(),
            local: vec!["10.0.0.1:9999".to_string()],
        }];
        let picked = pick_remote_candidate(&remotes).unwrap();
        assert_eq!(picked, "127.0.0.1:7789".parse().unwrap());
    }

    #[cfg(feature = "wan-rendezvous")]
    #[test]
    fn test_pick_remote_candidate_prefers_ipv6_over_ipv4_reflexive() {
        let remotes = vec![RemoteCandidates {
            from: "target".to_string(),
            server_reflexive: "127.0.0.1:7789".to_string(),
            local: vec!["[2001:db8::10]:9999".to_string()],
        }];
        let picked = pick_remote_candidate(&remotes).unwrap();
        assert_eq!(picked, "[2001:db8::10]:9999".parse().unwrap());
    }

    #[cfg(feature = "wan-rendezvous")]
    #[test]
    fn test_pick_remote_candidate_falls_back_to_local_parseable() {
        let remotes = vec![RemoteCandidates {
            from: "target".to_string(),
            server_reflexive: "not-an-addr".to_string(),
            local: vec!["bad".to_string(), "10.0.0.6:4567".to_string()],
        }];
        let picked = pick_remote_candidate(&remotes).unwrap();
        assert_eq!(picked, "10.0.0.6:4567".parse().unwrap());
    }

    #[test]
    fn test_split_destination_prefix_and_path() {
        assert_eq!(
            split_destination_prefix_and_path("alice@rv.example.com:8787"),
            ("alice@rv.example.com:8787", "")
        );
        assert_eq!(
            split_destination_prefix_and_path("alice@rv.example.com:8787/folder/file.bin"),
            ("alice@rv.example.com:8787", "folder/file.bin")
        );
    }

    #[cfg(feature = "wan-rendezvous")]
    #[tokio::test]
    async fn test_peer_at_destination_without_token_is_config_error() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("r2.bin");
        tokio::fs::write(&file_path, b"data").await.unwrap();
        let task = TransferTask {
            id: uuid::Uuid::new_v4(),
            source_path: file_path,
            destination: "alice@127.0.0.1:8787".to_string(),
            is_upload: true,
            file_size: 4,
            sha256: None,
            metadata: None,
        };
        let adapter = AutoAdapter::new(HttpConfig::default(), default_quic_config());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let err = adapter.upload(&task, tx).await.unwrap_err();
        match err {
            crate::core::AeroSyncError::InvalidConfig(s) => {
                assert!(s.contains("[R2_NO_TOKEN]"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[cfg(feature = "wan-rendezvous")]
    #[derive(Clone, Copy)]
    enum InitiateScript {
        Success,
        Fail,
    }

    #[cfg(feature = "wan-rendezvous")]
    #[derive(Clone, Copy)]
    enum WsScript {
        CloseImmediately,
        CandidateEmptyThenPunch,
    }

    #[cfg(feature = "wan-rendezvous")]
    #[derive(Clone)]
    struct MockRvState {
        initiate: InitiateScript,
        ws: WsScript,
        observed_addr: Option<String>,
    }

    #[cfg(feature = "wan-rendezvous")]
    async fn spawn_mock_rendezvous(state: MockRvState) -> String {
        use axum::extract::ws::{Message as WsMessage, WebSocketUpgrade};
        use axum::extract::{Path, State};
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::{get, post};
        use axum::{Json, Router};
        use serde_json::json;

        async fn lookup_peer(
            Path(name): Path<String>,
            State(st): State<MockRvState>,
        ) -> impl IntoResponse {
            Json(json!({
                "peer_id": "peer-test",
                "namespace": "",
                "name": name,
                "public_key": "pubkey",
                "capabilities": 0u64,
                "observed_addr": st.observed_addr,
                "last_seen_at": 0i64
            }))
        }

        async fn initiate(State(st): State<MockRvState>) -> impl IntoResponse {
            match st.initiate {
                InitiateScript::Fail => (
                    StatusCode::GATEWAY_TIMEOUT,
                    Json(json!({ "error": "upstream timeout while brokering session" })),
                )
                    .into_response(),
                InitiateScript::Success => Json(json!({
                    "session_id": "sess-1",
                    "implementation_status": {},
                    "signaling": {
                        "websocket_path": "/v1/sessions/sess-1/ws",
                        "quic_data_plane": "draft"
                    },
                    "stun": null
                }))
                .into_response(),
            }
        }

        async fn ws_handler(
            ws: WebSocketUpgrade,
            State(st): State<MockRvState>,
        ) -> impl IntoResponse {
            ws.on_upgrade(move |mut socket| async move {
                if let Some(Ok(WsMessage::Text(_))) = socket.recv().await {
                    match st.ws {
                        WsScript::CloseImmediately => {
                            let _ = socket.close().await;
                        }
                        WsScript::CandidateEmptyThenPunch => {
                            let _ = socket
                                .send(WsMessage::Text(
                                    json!({
                                        "type": "remote.candidates",
                                        "from": "target",
                                        "server_reflexive": "not-a-socket-addr",
                                        "local": ["also-not-an-addr"]
                                    })
                                    .to_string(),
                                ))
                                .await;
                            let _ = socket
                                .send(WsMessage::Text(
                                    json!({
                                        "type": "punch_at",
                                        "timestamp_ms": 0u64
                                    })
                                    .to_string(),
                                ))
                                .await;
                            let _ = socket.close().await;
                        }
                    }
                }
            })
        }

        let app = Router::new()
            .route("/v1/peers/:name", get(lookup_peer))
            .route("/v1/sessions/initiate", post(initiate))
            .route("/v1/sessions/:id/ws", get(ws_handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[cfg(feature = "wan-rendezvous")]
    async fn make_upload_task(file_name: &str, destination: String) -> TransferTask {
        use tokio::io::AsyncWriteExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(file_name);
        let mut file = tokio::fs::File::create(&path).await.unwrap();
        file.write_all(b"r2-test").await.unwrap();
        file.flush().await.unwrap();
        // Keep the tempdir alive by moving the file out to a stable temp path.
        let persisted = std::env::temp_dir().join(format!(
            "aerosync-r2-test-{}-{}.bin",
            file_name,
            uuid::Uuid::new_v4()
        ));
        tokio::fs::copy(&path, &persisted).await.unwrap();
        TransferTask {
            id: uuid::Uuid::new_v4(),
            source_path: persisted,
            destination,
            is_upload: true,
            file_size: 7,
            sha256: None,
            metadata: None,
        }
    }

    #[cfg(feature = "wan-rendezvous")]
    #[tokio::test]
    async fn test_r2_peer_unseen_with_path_suffix_is_tagged() {
        let base = spawn_mock_rendezvous(MockRvState {
            initiate: InitiateScript::Success,
            ws: WsScript::CloseImmediately,
            observed_addr: None,
        })
        .await;
        let authority = base.trim_start_matches("http://");
        let task = make_upload_task(
            "peer-unseen.bin",
            format!("alice@{authority}/nested/path.bin"),
        )
        .await;
        let adapter = AutoAdapter::new(HttpConfig::default(), default_quic_config())
            .with_rendezvous_token("token".to_string());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let err = adapter.upload(&task, tx).await.unwrap_err();
        match err {
            crate::core::AeroSyncError::InvalidConfig(s) => {
                assert!(s.contains("[R2_PEER_UNSEEN]"), "got: {s}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[cfg(feature = "wan-rendezvous")]
    #[tokio::test]
    async fn test_r2_initiate_failure_is_tagged() {
        let base = spawn_mock_rendezvous(MockRvState {
            initiate: InitiateScript::Fail,
            ws: WsScript::CloseImmediately,
            observed_addr: Some("127.0.0.1:9000".to_string()),
        })
        .await;
        let authority = base.trim_start_matches("http://");
        let task = make_upload_task("initiate-fail.bin", format!("alice@{authority}")).await;
        let adapter = AutoAdapter::new(HttpConfig::default(), default_quic_config())
            .with_rendezvous_token("token".to_string());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let err = adapter.upload(&task, tx).await.unwrap_err();
        match err {
            crate::core::AeroSyncError::Network(s) => {
                assert!(s.contains("[R2_INITIATE]"), "got: {s}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[cfg(feature = "wan-rendezvous")]
    #[tokio::test]
    async fn test_r2_signaling_close_before_punch_is_tagged() {
        let base = spawn_mock_rendezvous(MockRvState {
            initiate: InitiateScript::Success,
            ws: WsScript::CloseImmediately,
            observed_addr: Some("127.0.0.1:9000".to_string()),
        })
        .await;
        let authority = base.trim_start_matches("http://");
        let task = make_upload_task("signaling-close.bin", format!("alice@{authority}")).await;
        let adapter = AutoAdapter::new(HttpConfig::default(), default_quic_config())
            .with_rendezvous_token("token".to_string());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let err = adapter.upload(&task, tx).await.unwrap_err();
        match err {
            crate::core::AeroSyncError::Network(s) => {
                assert!(s.contains("[R2_SIGNALING]"), "got: {s}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[cfg(feature = "wan-rendezvous")]
    #[tokio::test]
    async fn test_r2_candidate_empty_is_tagged() {
        let base = spawn_mock_rendezvous(MockRvState {
            initiate: InitiateScript::Success,
            ws: WsScript::CandidateEmptyThenPunch,
            observed_addr: Some("127.0.0.1:9000".to_string()),
        })
        .await;
        let authority = base.trim_start_matches("http://");
        let task = make_upload_task("candidate-empty.bin", format!("alice@{authority}")).await;
        let adapter = AutoAdapter::new(HttpConfig::default(), default_quic_config())
            .with_rendezvous_token("token".to_string());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let err = adapter.upload(&task, tx).await.unwrap_err();
        match err {
            crate::core::AeroSyncError::Network(s) => {
                assert!(s.contains("[R2_CANDIDATE_EMPTY]"), "got: {s}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[cfg(feature = "wan-rendezvous")]
    #[tokio::test]
    async fn test_r2_stage_timeout_error_code_is_stable() {
        let err = run_r2_stage_with_timeout(
            StdDuration::from_millis(10),
            "R2_TIMEOUT_INITIATE",
            "initiate_session timed out for `alice`".to_string(),
            async {
                tokio::time::sleep(StdDuration::from_millis(50)).await;
                Ok::<(), AeroSyncError>(())
            },
        )
        .await
        .unwrap_err();
        match err {
            AeroSyncError::Network(s) => {
                assert!(s.contains("[R2_TIMEOUT_INITIATE]"), "got: {s}");
                assert!(s.contains("after"), "got: {s}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}

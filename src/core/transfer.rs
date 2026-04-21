use crate::core::audit::{AuditLogger, Direction};
use crate::core::history::HistoryEntry;
use crate::core::history::HistoryStore;
use aerosync_domain::storage::HistoryStorage;
use aerosync_infra::history::spawn_watch_bridge as history_spawn_watch_bridge;
use crate::core::metadata::{
    datetime_to_proto_ts, empty_metadata, validate_sealed as validate_metadata,
};
use crate::core::progress::TransferStatus;
use crate::core::receipt::{Event, Receipt, Sender, State};
use crate::core::receipt_registry::ReceiptRegistry;
use crate::core::resume::{ResumeState, ResumeStore, DEFAULT_CHUNK_SIZE};
use crate::core::sniff::{sniff_content_type, SNIFF_PEEK_BYTES};
use crate::core::{AeroSyncError, ProgressMonitor, Result, TransferProgress};
use crate::protocols::http::{HttpReceiptAck, HttpReceiptDecision};
use aerosync_proto::Metadata;
use futures::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, RwLock, Semaphore};
use uuid::Uuid;
use zeroize::Zeroizing;

// ────────────────────────── configuration ───────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferConfig {
    /// 最大并发传输任务数（Semaphore permits）
    pub max_concurrent_transfers: usize,
    /// 分片上传时每个分片的大小（bytes），默认 4MB，取值范围 1 ~ chunked_threshold
    pub chunk_size: usize,
    /// 单次传输失败后的最大重试次数，默认 3，0 表示不重试
    pub retry_attempts: u32,
    /// 单次请求超时（秒），默认 60，0 表示不超时
    pub timeout_seconds: u64,
    /// 是否优先使用 QUIC 协议（默认 true；远端不支持时自动降级到 HTTP）
    pub use_quic: bool,
    /// 发送方认证 Token（drop 时自动清零内存）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<Zeroizing<String>>,
    /// 大于此阈值（bytes）时使用分片上传，默认 64MB
    pub chunked_threshold: u64,
    /// 续传状态文件存放目录（默认为当前工作目录）
    pub resume_state_dir: PathBuf,
    /// 是否启用断点续传（默认 true）
    pub enable_resume: bool,
    /// 小文件（< small_file_threshold）并发上限，默认 16
    pub small_file_concurrency: usize,
    /// 中等文件（< chunked_threshold）并发上限，默认 8
    pub medium_file_concurrency: usize,
    /// 小文件阈值（bytes），默认 1MB
    pub small_file_threshold: u64,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            max_concurrent_transfers: 4,
            chunk_size: 4 * 1024 * 1024,
            retry_attempts: 3,
            timeout_seconds: 60,
            use_quic: true,
            auth_token: None,
            chunked_threshold: 64 * 1024 * 1024, // 64MB
            resume_state_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            enable_resume: true,
            small_file_concurrency: 16,
            medium_file_concurrency: 8,
            small_file_threshold: 1024 * 1024, // 1MB
        }
    }
}

impl TransferConfig {
    /// 根据文件大小返回自适应并发数（供上层 UI 参考）
    pub fn concurrency_for_size(&self, file_size: u64) -> usize {
        if file_size < self.small_file_threshold {
            self.small_file_concurrency
        } else if file_size < self.chunked_threshold {
            self.medium_file_concurrency
        } else {
            1
        }
    }

    /// 校验配置合法性，返回第一个违规字段的错误描述。
    ///
    /// 规则：
    /// - `max_concurrent_transfers` 必须 > 0
    /// - `chunk_size` 必须 > 0
    /// - `chunk_size` 必须 <= `chunked_threshold`
    pub fn validate(&self) -> Result<()> {
        if self.max_concurrent_transfers == 0 {
            return Err(AeroSyncError::InvalidConfig(
                "max_concurrent_transfers must be > 0".to_string(),
            ));
        }
        if self.chunk_size == 0 {
            return Err(AeroSyncError::InvalidConfig(
                "chunk_size must be > 0".to_string(),
            ));
        }
        if self.chunk_size as u64 > self.chunked_threshold {
            return Err(AeroSyncError::InvalidConfig(format!(
                "chunk_size ({}) must be <= chunked_threshold ({})",
                self.chunk_size, self.chunked_threshold
            )));
        }
        Ok(())
    }
}

// ────────────────────────── task ────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TransferTask {
    pub id: Uuid,
    pub source_path: PathBuf,
    pub destination: String,
    pub file_size: u64,
    pub is_upload: bool,
    /// 预计算的 SHA-256（可选）
    pub sha256: Option<String>,
    /// RFC-003 sealed metadata envelope, attached by
    /// [`TransferEngine::send_with_metadata`] just before the task is
    /// queued. `None` for legacy callers (`new_upload` / `new_download`
    /// constructors and direct `add_transfer` callers) — the engine and
    /// adapters MUST treat the absence as "no metadata" rather than
    /// faulting. The HTTP adapter forwards this verbatim as the
    /// base64-encoded `X-Aerosync-Metadata` header (RFC-003 §8.4);
    /// the QUIC adapter folds it into
    /// `ControlFrame::TransferStart.metadata` on the bidi receipt
    /// control stream (RFC-002 §6.3, wired in v0.2.1 batch C).
    pub metadata: Option<Metadata>,
}

impl TransferTask {
    pub fn new_upload(source_path: PathBuf, destination: String, file_size: u64) -> Self {
        Self {
            id: Uuid::new_v4(),
            source_path,
            destination,
            file_size,
            is_upload: true,
            sha256: None,
            metadata: None,
        }
    }

    pub fn new_download(source_url: String, destination_path: PathBuf, file_size: u64) -> Self {
        Self {
            id: Uuid::new_v4(),
            source_path: destination_path,
            destination: source_url,
            file_size,
            is_upload: false,
            sha256: None,
            metadata: None,
        }
    }
}

// ────────────────────────── protocol trait (core-side) ──────────────────────

/// 进度更新消息（核心层自有类型，不依赖 protocols crate）
#[derive(Debug, Clone)]
pub struct ProtocolProgress {
    pub bytes_transferred: u64,
    pub transfer_speed: f64,
}

/// 协议适配器 trait（由 aerosync-protocols 实现，注入到 TransferEngine）
#[async_trait::async_trait]
pub trait ProtocolAdapter: Send + Sync {
    async fn upload(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<ProtocolProgress>,
    ) -> Result<()>;

    async fn download(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<ProtocolProgress>,
    ) -> Result<()>;

    /// 分片上传（支持断点续传）
    async fn upload_chunked(
        &self,
        task: &TransferTask,
        state: &mut ResumeState,
        progress_tx: mpsc::UnboundedSender<ProtocolProgress>,
    ) -> Result<()>;

    fn protocol_name(&self) -> &'static str;
}

// ────────────────────────── engine ──────────────────────────────────────────

pub struct TransferEngine {
    pub config: TransferConfig,
    progress_monitor: Arc<RwLock<ProgressMonitor>>,
    task_sender: Option<mpsc::UnboundedSender<TransferTask>>,
    cancel_sender: Option<mpsc::UnboundedSender<Uuid>>,
    task_receiver: Arc<RwLock<Option<mpsc::UnboundedReceiver<TransferTask>>>>,
    cancel_receiver: Arc<RwLock<Option<mpsc::UnboundedReceiver<Uuid>>>>,
    _task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    audit_logger: Option<Arc<AuditLogger>>,
    /// RFC-002 §6.4 sender-side receipt registry. Populated by
    /// [`TransferEngine::send`]; the in-process `ReceiptRegistry` is
    /// the lookup point for `wait_receipt`/`cancel_receipt` and for
    /// QUIC frame routing once the receipt stream is wired into the
    /// transport (deferred from this task — see `quic_receipt`).
    receipt_registry: Arc<ReceiptRegistry<Sender>>,
    /// Optional [`HistoryStorage`] backend auto-paired with every
    /// `send` call: when set, each freshly-issued [`Receipt`] is
    /// connected via the
    /// [`aerosync_infra::history::spawn_watch_bridge`] free function
    /// so terminal transitions land in the configured store. Stored
    /// as a trait object (Phase 3.4d-ii) so alternative backends
    /// (in-memory mock, future SQLite, aerosync.cloud HTTP shim) can
    /// drop in without engine churn — the concrete
    /// [`HistoryStore`] still works thanks to the
    /// [`Arc<HistoryStore>`] → [`Arc<dyn HistoryStorage>`] coercion in
    /// [`Self::with_history_store`].
    history_store: Option<Arc<dyn HistoryStorage>>,
    /// Map of QUEUE-side `task_id → receipt_id` so the bridge spawned
    /// by `send` can drive the right receipt when the worker reports
    /// completion.
    task_to_receipt: Arc<RwLock<std::collections::HashMap<Uuid, Uuid>>>,
    /// Optional sender-side node identifier stamped into
    /// [`Metadata::from_node`] during
    /// [`Self::send_with_metadata`]. Defaults to the OS hostname (or
    /// the empty string if unavailable). RFC-005 will replace this
    /// with a signed identity; for v0.2 it is purely informational.
    node_id: Arc<RwLock<String>>,
    /// Sealed [`Metadata`] keyed by receipt id. Populated by
    /// [`Self::send_with_metadata`] right before the transfer is
    /// queued so the QUIC adapter can read it back when assembling
    /// `TransferStart`. The wire-side plumbing through the adapter
    /// lands as part of `quic_receipt`; until then this map is the
    /// canonical source of truth on the sender.
    sealed_metadata: Arc<RwLock<std::collections::HashMap<Uuid, Metadata>>>,
}

impl TransferEngine {
    pub fn new(config: TransferConfig) -> Self {
        let (task_sender, task_receiver) = mpsc::unbounded_channel();
        let (cancel_sender, cancel_receiver) = mpsc::unbounded_channel();

        Self {
            config,
            progress_monitor: Arc::new(RwLock::new(ProgressMonitor::new())),
            task_sender: Some(task_sender),
            cancel_sender: Some(cancel_sender),
            task_receiver: Arc::new(RwLock::new(Some(task_receiver))),
            cancel_receiver: Arc::new(RwLock::new(Some(cancel_receiver))),
            _task_handle: Arc::new(RwLock::new(None)),
            audit_logger: None,
            receipt_registry: Arc::new(ReceiptRegistry::new()),
            history_store: None,
            task_to_receipt: Arc::new(RwLock::new(std::collections::HashMap::new())),
            node_id: Arc::new(RwLock::new(default_node_id())),
            sealed_metadata: Arc::new(RwLock::new(std::collections::HashMap::new())),
        }
    }

    /// Builder: attach the file-backed [`HistoryStore`] so future
    /// [`Self::send`] calls auto-persist receipt terminals via the
    /// [`aerosync_infra::history::spawn_watch_bridge`] free function.
    /// Concrete-typed for back-compat (call-sites that already pass
    /// `Arc<HistoryStore>` keep working); use
    /// [`Self::with_history_storage`] to plug in an alternative
    /// [`HistoryStorage`] backend (in-memory mock, future SQLite,
    /// aerosync.cloud HTTP shim).
    pub fn with_history_store(mut self, store: Arc<HistoryStore>) -> Self {
        self.history_store = Some(store as Arc<dyn HistoryStorage>);
        self
    }

    /// Builder: attach any [`HistoryStorage`] implementation as the
    /// receipt-terminal sink. Companion to [`Self::with_history_store`]
    /// — Phase 3.4d-ii introduced this opaque-trait variant so test
    /// mocks and future cloud backends can drop in without depending
    /// on the concrete [`HistoryStore`] file format.
    pub fn with_history_storage(mut self, store: Arc<dyn HistoryStorage>) -> Self {
        self.history_store = Some(store);
        self
    }

    /// Builder: override the sender-side node id stamped into
    /// [`Metadata::from_node`]. Defaults to the OS hostname.
    pub fn with_node_id(self, id: impl Into<String>) -> Self {
        // Synchronous best-effort write into the RwLock. The engine is
        // single-threaded at this point in its construction.
        if let Ok(mut guard) = self.node_id.try_write() {
            *guard = id.into();
        }
        self
    }

    /// Read the currently-configured sender node id.
    pub async fn node_id(&self) -> String {
        self.node_id.read().await.clone()
    }

    /// Look up the sealed metadata that was stamped by
    /// [`Self::send_with_metadata`] for `receipt_id`. Returns `None`
    /// when no `send` has been issued for that receipt or when it was
    /// pruned after a terminal transition.
    pub async fn metadata_for(&self, receipt_id: Uuid) -> Option<Metadata> {
        self.sealed_metadata.read().await.get(&receipt_id).cloned()
    }

    /// Borrow the engine's sender-side [`ReceiptRegistry`]. MCP tools
    /// (`wait_receipt`, `cancel_receipt`) hold a clone of this Arc to
    /// look up live receipts by id.
    pub fn receipt_registry(&self) -> Arc<ReceiptRegistry<Sender>> {
        Arc::clone(&self.receipt_registry)
    }

    /// Returns an inbox channel that consumes [`HttpReceiptAck`]
    /// frames parsed off the receiver's `/upload` JSON response and
    /// applies the corresponding [`Event::Ack`] / [`Event::Nack`] to
    /// the matching `Receipt<Sender>` in [`Self::receipt_registry`].
    ///
    /// The HTTP path's parallel to the QUIC bidi receipt control
    /// stream (RFC-002 §6.3 / §6.4): `AutoAdapter::with_engine_receipt_inbox`
    /// stitches the returned sender into every `HttpTransfer` it
    /// constructs so the engine bridge can drive the sender-side
    /// receipt past `Processing` to a terminal state without a
    /// dedicated wire.
    ///
    /// Each call spawns a fresh forwarder task on the current tokio
    /// runtime; the task lives until the returned sender (and all
    /// its clones) are dropped, at which point the channel closes
    /// and the task exits naturally. Apply-event errors (terminal
    /// race, invalid transition) are logged at `tracing::debug!` and
    /// dropped — the contract here is best-effort, mirroring the
    /// QUIC drain loop in `protocols::quic::drain_receipt_stream`.
    ///
    /// **Must be called from inside a tokio runtime context** (the
    /// implementation uses `tokio::spawn`). Calling it from a
    /// non-tokio thread panics; in practice every caller routes
    /// through the engine's own tokio runtime (the Python binding's
    /// shared runtime, or the `#[tokio::test]` runtime in
    /// integration tests).
    pub fn http_receipt_inbox(&self) -> mpsc::UnboundedSender<HttpReceiptAck> {
        let (tx, mut rx) = mpsc::unbounded_channel::<HttpReceiptAck>();
        let registry = Arc::clone(&self.receipt_registry);
        tokio::spawn(async move {
            while let Some(ack) = rx.recv().await {
                let HttpReceiptAck {
                    receipt_id,
                    decision,
                    reason,
                } = ack;
                let Some(receipt) = registry.get(receipt_id) else {
                    tracing::debug!(
                        %receipt_id,
                        "http_receipt_inbox: no receipt registered (likely already pruned), dropping ack"
                    );
                    continue;
                };
                let event = match decision {
                    HttpReceiptDecision::Ack => Event::Ack,
                    HttpReceiptDecision::Nack => Event::Nack {
                        reason: reason.unwrap_or_default(),
                    },
                };
                // Race: `HttpTransfer` parses `receipt_ack` off the
                // `/upload` response body **before** the upload-side
                // worker reports `TransferStatus::Completed` to the
                // progress monitor — and therefore before the engine
                // bridge in `send_with_metadata` walks the receipt
                // through `Open → Close → Close → Process`. Applying
                // `Event::Ack` while the receipt is still in
                // `StreamOpened` / `DataTransferred` / `StreamClosed`
                // would be rejected as `InvalidTransition` and the
                // ack would be silently lost. Spawn a per-ack task
                // that awaits the bridge to land the receipt in
                // `Processing` (or any terminal — handles the
                // double-ack and engine-already-failed cases) and
                // then applies the event. Bounded by a 30 s cap so a
                // wedged engine bridge can't leak the task.
                let receipt_clone = Arc::clone(&receipt);
                tokio::spawn(async move {
                    let wait = receipt_clone
                        .await_state(|s| matches!(s, State::Processing) || s.is_terminal());
                    let waited = tokio::time::timeout(Duration::from_secs(30), wait).await;
                    if waited.is_err() {
                        tracing::debug!(
                            %receipt_id,
                            "http_receipt_inbox: precondition wait timed out (engine bridge stuck), dropping ack"
                        );
                        return;
                    }
                    if let Err(e) = receipt_clone.apply_event(event) {
                        tracing::debug!(
                            %receipt_id,
                            error = ?e,
                            "http_receipt_inbox: apply_event rejected (likely terminal-state race), dropping ack"
                        );
                    }
                });
            }
            tracing::debug!("http_receipt_inbox: forwarder task exiting (sender dropped)");
        });
        tx
    }

    /// 启动引擎，注入协议适配器
    #[tracing::instrument(skip(self, adapter))]
    pub async fn start(&self, adapter: Arc<dyn ProtocolAdapter>) -> Result<()> {
        self.start_inner(adapter, self.audit_logger.clone()).await
    }

    /// 启动引擎，同时指定审计日志
    pub async fn start_with_audit(
        &mut self,
        adapter: Arc<dyn ProtocolAdapter>,
        audit_logger: Arc<AuditLogger>,
    ) -> Result<()> {
        self.audit_logger = Some(audit_logger.clone());
        self.start_inner(adapter, Some(audit_logger)).await
    }

    async fn start_inner(
        &self,
        adapter: Arc<dyn ProtocolAdapter>,
        audit_logger: Option<Arc<AuditLogger>>,
    ) -> Result<()> {
        let task_rx = self.task_receiver.write().await.take();
        let cancel_rx = self.cancel_receiver.write().await.take();

        if let (Some(task_rx), Some(cancel_rx)) = (task_rx, cancel_rx) {
            let monitor = Arc::clone(&self.progress_monitor);
            let config = self.config.clone();

            let handle = tokio::spawn(async move {
                transfer_worker(task_rx, cancel_rx, monitor, adapter, config, audit_logger).await;
            });
            *self._task_handle.write().await = Some(handle);
            tracing::info!("Transfer engine started");
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, task), fields(task_id = %task.id, file = ?task.source_path))]
    pub async fn add_transfer(&self, task: TransferTask) -> Result<()> {
        let file_name = task
            .source_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let progress = TransferProgress {
            task_id: task.id,
            file_name,
            bytes_transferred: 0,
            total_bytes: task.file_size,
            transfer_speed: 0.0,
            elapsed_time: std::time::Duration::ZERO,
            estimated_remaining: None,
            status: TransferStatus::Pending,
        };

        self.progress_monitor.write().await.add_transfer(progress);

        self.task_sender
            .as_ref()
            .ok_or_else(|| AeroSyncError::System("Transfer engine not started".to_string()))?
            .send(task)
            .map_err(|_| AeroSyncError::System("Failed to queue transfer task".to_string()))?;

        Ok(())
    }

    #[tracing::instrument(skip(self), fields(task_id = %task_id))]
    pub async fn cancel_transfer(&self, task_id: Uuid) -> Result<()> {
        // Mirror the cancel signal into the per-task Receipt (if any)
        // so observers of `Receipt::watch` see a Cancelled terminal
        // rather than waiting forever. The cancel event is rejected
        // silently if the receipt is already terminal.
        if let Some(receipt_id) = self.task_to_receipt.read().await.get(&task_id).copied() {
            if let Some(receipt) = self.receipt_registry.get(receipt_id) {
                let _ = receipt.apply_event(Event::Cancel {
                    reason: "transfer cancelled by sender".to_string(),
                });
            }
        }
        self.cancel_sender
            .as_ref()
            .ok_or_else(|| AeroSyncError::System("Transfer engine not started".to_string()))?
            .send(task_id)
            .map_err(|_| AeroSyncError::System("Failed to send cancel signal".to_string()))?;
        Ok(())
    }

    /// RFC-002 §6.4 sender API: queue a transfer and return an
    /// [`Arc<Receipt<Sender>>`] that observes its lifecycle.
    ///
    /// The returned receipt starts in [`State::Initiated`] and is
    /// driven by an internal bridge task through
    /// `StreamOpened → DataTransferred → StreamClosed → Processing`
    /// once the worker reports the local transfer succeeded. The final
    /// `Acked` / `Nacked` transition is **not** synthesised by the
    /// engine — that event must come from the receiver-side, either
    /// via a wired QUIC receipt-stream reader (see
    /// `protocols::quic_receipt`) or via an explicit
    /// `apply_event(Event::Ack)` from a test or local handler.
    ///
    /// Failure paths: a worker `Failed` status maps to
    /// `Event::Error{code, detail}`; an explicit `cancel_transfer`
    /// maps to `Event::Cancel{reason}`.
    ///
    /// The receipt is registered in
    /// [`Self::receipt_registry`] before this function returns so MCP
    /// `wait_receipt`/`cancel_receipt` can find it immediately.
    /// If a [`HistoryStore`] was attached via
    /// [`Self::with_history_store`], a stub history entry tagged with
    /// the receipt id is appended and a `spawn_watch_bridge` is
    /// installed so terminals land in JSONL.
    #[tracing::instrument(skip(self, task), fields(task_id = %task.id))]
    pub async fn send(&self, task: TransferTask) -> Result<Arc<Receipt<Sender>>> {
        // Backwards-compatible path: equivalent to
        // `send_with_metadata(task, empty_metadata())`. The engine
        // still stamps system fields; the absence of well-known and
        // user fields is what makes this the "no metadata" call.
        self.send_with_metadata(task, empty_metadata()).await
    }

    /// RFC-003 §8.2 sender API: queue a transfer carrying a
    /// [`Metadata`] envelope built via
    /// [`crate::core::metadata::MetadataBuilder`]. The engine fills
    /// in the **system** half of the envelope (id, from_node, to_node,
    /// created_at, content_type, size_bytes, sha256, file_name,
    /// protocol) just before sealing it; the caller's well-known and
    /// `user_metadata` entries are preserved verbatim.
    ///
    /// Validation: the sealed envelope is re-checked against
    /// RFC-003 §6 size limits via [`validate_metadata`] and an
    /// `AeroSyncError::InvalidConfig` is returned if it overshoots.
    ///
    /// On success the sealed envelope is stored in
    /// [`Self::metadata_for`] keyed by the returned receipt's id, so
    /// the QUIC adapter (and history-store bridge) can read it back.
    #[tracing::instrument(skip(self, task, metadata), fields(task_id = %task.id))]
    pub async fn send_with_metadata(
        &self,
        mut task: TransferTask,
        metadata: Metadata,
    ) -> Result<Arc<Receipt<Sender>>> {
        let receipt_id = Uuid::new_v4();
        let sealed = self.seal_system_fields(&task, metadata, receipt_id).await?;
        validate_metadata(&sealed)
            .map_err(|e| AeroSyncError::InvalidConfig(format!("metadata validation: {e}")))?;
        // Stash the sealed envelope on the task itself so the adapter
        // (HTTP today, QUIC after batch C) can read it back without
        // having to reach into the engine's `sealed_metadata` map. The
        // map is still kept as the canonical store keyed by
        // `receipt_id` for `metadata_for(...)` lookups.
        task.metadata = Some(sealed.clone());
        self.sealed_metadata
            .write()
            .await
            .insert(receipt_id, sealed);

        let receipt: Arc<Receipt<Sender>> = Arc::new(Receipt::<Sender>::new(receipt_id));
        self.receipt_registry.insert(Arc::clone(&receipt));

        // Move to StreamOpened immediately so observers see at least
        // one transition before the queued worker picks the task up.
        let _ = receipt.apply_event(Event::Open);

        // Optional history-store integration.
        if let Some(store) = &self.history_store {
            let filename = task
                .source_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let mut entry = HistoryEntry::success(
                filename,
                None,
                task.file_size,
                task.sha256.clone(),
                None,
                if task.destination.starts_with("quic://") {
                    "quic"
                } else {
                    "http"
                },
                "send",
                0,
            )
            .with_receipt_id(receipt_id);
            // Attach the sealed metadata envelope so `aerosync history`
            // and the MCP `query_history` tool can filter by trace id /
            // user_metadata even after the in-engine `sealed_metadata`
            // map has been GC'd at terminal.
            if let Some(sealed) = self.sealed_metadata.read().await.get(&receipt_id) {
                entry = entry.with_metadata_proto(sealed);
            }
            store.append_silent(entry).await;
            history_spawn_watch_bridge(Arc::clone(store), Arc::clone(&receipt));
        }

        let task_id = task.id;
        self.task_to_receipt
            .write()
            .await
            .insert(task_id, receipt_id);

        // Bridge: poll the ProgressMonitor for terminal status and
        // mirror it into the Receipt state machine. Polling at 20ms
        // is acceptable because (a) transfers are typically
        // seconds-long, (b) the bridge exits as soon as terminal is
        // observed. A push-based replacement awaits ProgressMonitor
        // gaining a watch channel — out of scope for RFC-002 §14 #8.
        let monitor = Arc::clone(&self.progress_monitor);
        let registry = Arc::clone(&self.receipt_registry);
        let task_to_receipt = Arc::clone(&self.task_to_receipt);
        let sealed_metadata = Arc::clone(&self.sealed_metadata);
        let bridge_receipt = Arc::clone(&receipt);
        tokio::spawn(async move {
            // Hard upper bound so a stuck monitor can't leak the task.
            let deadline = std::time::Instant::now() + Duration::from_secs(60 * 60 * 24);
            loop {
                tokio::time::sleep(Duration::from_millis(20)).await;
                if std::time::Instant::now() >= deadline {
                    let _ = bridge_receipt.apply_event(Event::Error {
                        code: 504,
                        detail: "transfer-engine bridge deadline exceeded".to_string(),
                    });
                    break;
                }
                let snapshot = {
                    let m = monitor.read().await;
                    m.get_transfer(&task_id).cloned()
                };
                match snapshot.map(|p| p.status) {
                    Some(TransferStatus::Completed) => {
                        // Drive Open/StreamOpened → DataTransferred → StreamClosed → Processing.
                        // Each apply_event is a no-op if already past
                        // that state (e.g. on retry).
                        let _ = bridge_receipt.apply_event(Event::Close);
                        let _ = bridge_receipt.apply_event(Event::Close);
                        let _ = bridge_receipt.apply_event(Event::Process);
                        break;
                    }
                    Some(TransferStatus::Failed(reason)) => {
                        let _ = bridge_receipt.apply_event(Event::Error {
                            code: 1,
                            detail: reason,
                        });
                        break;
                    }
                    Some(TransferStatus::Cancelled) => {
                        let _ = bridge_receipt.apply_event(Event::Cancel {
                            reason: "transfer cancelled".to_string(),
                        });
                        break;
                    }
                    _ => continue,
                }
            }
            // Cleanup: remove the task→receipt map entry. The receipt
            // itself stays in the registry until pruned (so callers
            // can still observe the terminal state) — `prune_terminal`
            // is the GC hook for that.
            let _ = registry; // explicit borrow so we keep the Arc alive
            task_to_receipt.write().await.remove(&task_id);
            // Sealed metadata is no longer needed once the receipt
            // reaches a terminal state — the QUIC adapter only reads
            // it for live transfers. Drop it here to keep memory
            // bounded for long-running senders. Callers that want a
            // post-terminal copy should fetch it via `metadata_for`
            // before the bridge reaches this point.
            sealed_metadata.write().await.remove(&receipt_id);
        });

        // Queue the actual transfer. We do this AFTER spawning the
        // bridge so the bridge can never miss a fast completion.
        self.add_transfer(task).await?;

        Ok(receipt)
    }

    pub async fn get_progress_monitor(&self) -> Arc<RwLock<ProgressMonitor>> {
        Arc::clone(&self.progress_monitor)
    }

    /// Stamp RFC-003 §4 system fields onto a caller-supplied
    /// [`Metadata`] just before queueing. Caller-supplied values for
    /// system fields (id, sha256, …) are **always overridden** —
    /// only well-known and `user_metadata` entries survive untouched.
    /// `content_type` is preserved when the caller provided one
    /// (typed `MetadataBuilder::content_type` path), otherwise it is
    /// sniffed from the first [`SNIFF_PEEK_BYTES`] of the file.
    async fn seal_system_fields(
        &self,
        task: &TransferTask,
        mut metadata: Metadata,
        receipt_id: Uuid,
    ) -> Result<Metadata> {
        let file_name = task
            .source_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let protocol = match &task.destination {
            d if d.starts_with("quic://") => "quic",
            d if d.starts_with("s3://") => "s3",
            d if d.starts_with("ftp://") || d.starts_with("ftps://") => "ftp",
            _ => "http",
        }
        .to_string();

        let to_node = parse_to_node(&task.destination);

        // Sniff the content type when the caller did not supply one.
        // We tolerate IO errors here (the file may not yet exist for
        // synthetic tests) and fall through to `application/octet-stream`.
        if metadata.content_type.is_empty() {
            metadata.content_type = sniff_for_path(&task.source_path).await;
        }

        metadata.id = receipt_id.to_string();
        metadata.from_node = self.node_id.read().await.clone();
        metadata.to_node = to_node;
        metadata.created_at = Some(datetime_to_proto_ts(chrono::Utc::now()));
        metadata.size_bytes = task.file_size;
        metadata.sha256 = task.sha256.clone().unwrap_or_default();
        metadata.file_name = file_name;
        metadata.protocol = protocol;

        Ok(metadata)
    }
}

/// OS-hostname best-effort default for `from_node`. RFC-005 will
/// replace this with a signed identity. Empty string when the OS
/// hostname is not retrievable (e.g. sandboxed CI).
fn default_node_id() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_default()
}

/// Extract a "to node" label from a `TransferTask::destination`.
///
/// - `quic://host:port/...` and `http(s)://host:port/...` → `host`
/// - `s3://bucket/key`                                   → `bucket`
/// - `ftp://host/...`                                    → `host`
/// - bare strings (mDNS rendezvous names like `archiver`) → returned verbatim
fn parse_to_node(destination: &str) -> String {
    if let Some((scheme, rest)) = destination.split_once("://") {
        let host_and_path = rest.split('/').next().unwrap_or(rest);
        // Strip optional `:port` so the node label stays clean.
        let host = host_and_path.split(':').next().unwrap_or(host_and_path);
        if !host.is_empty() {
            return host.to_string();
        }
        return scheme.to_string();
    }
    destination.to_string()
}

/// Read up to [`SNIFF_PEEK_BYTES`] bytes from `path` and run the
/// content-type sniffer. Best-effort: any IO error falls through to
/// the extension-only path inside [`sniff_content_type`].
async fn sniff_for_path(path: &std::path::Path) -> String {
    let mut buf = Vec::with_capacity(SNIFF_PEEK_BYTES);
    if let Ok(mut f) = tokio::fs::File::open(path).await {
        let mut chunk = vec![0u8; SNIFF_PEEK_BYTES];
        if let Ok(n) = f.read(&mut chunk).await {
            chunk.truncate(n);
            buf = chunk;
        }
    }
    sniff_content_type(path, &buf)
}

// ────────────────────────── 并发 worker ──────────────────────────────────────
//
// 设计：
//  - Semaphore 控制最大并发数（max_concurrent_transfers）
//  - FuturesUnordered 并发驱动所有运行中任务
//  - select! 同时监听「新任务入队」和「已有任务完成」
//  - 每个任务持有 OwnedSemaphorePermit，执行完自动释放

async fn transfer_worker(
    mut task_rx: mpsc::UnboundedReceiver<TransferTask>,
    mut cancel_rx: mpsc::UnboundedReceiver<Uuid>,
    monitor: Arc<RwLock<ProgressMonitor>>,
    adapter: Arc<dyn ProtocolAdapter>,
    config: TransferConfig,
    audit_logger: Option<Arc<AuditLogger>>,
) {
    tracing::info!(
        "Transfer worker started (protocol: {}, max_concurrent: {})",
        adapter.protocol_name(),
        config.max_concurrent_transfers
    );

    let sem = Arc::new(Semaphore::new(config.max_concurrent_transfers));
    let mut running: FuturesUnordered<
        tokio::task::JoinHandle<(Uuid, std::result::Result<(), String>)>,
    > = FuturesUnordered::new();

    loop {
        tokio::select! {
            // ── 取消信号 ────────────────────────────────────────────────────
            Some(cancel_id) = cancel_rx.recv() => {
                tracing::info!("Cancel requested: {}", cancel_id);
                monitor.write().await.cancel_transfer(cancel_id);
            }

            // ── 接收新任务 ──────────────────────────────────────────────────
            maybe_task = task_rx.recv() => {
                match maybe_task {
                    Some(task) => {
                        let task_id = task.id;
                        tracing::debug!(
                            "Queuing: {} (size={}B)",
                            task.source_path.display(),
                            task.file_size
                        );
                        monitor.write().await.update_progress(task_id, 0, 0.0);

                        // 任务入队前已被取消（cancel 信号先于 task 到达）
                        if monitor.read().await.is_cancelled(&task_id) {
                            tracing::info!("Task {} cancelled before start, skipping", task_id);
                            continue;
                        }

                        let permit = match Arc::clone(&sem).acquire_owned().await {
                            Ok(p) => p,
                            Err(_) => {
                                tracing::error!("Semaphore closed, stopping transfer worker");
                                break;
                            }
                        };
                        let adapter_ref = Arc::clone(&adapter);
                        let config_ref = config.clone();
                        let audit_ref = audit_logger.clone();
                        let task_filename = task.source_path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| "unknown".to_string());
                        let task_size = task.file_size;
                        let task_dest_proto = if task.destination.starts_with("quic://") { "quic" }
                            else if task.destination.starts_with("s3://") { "s3" }
                            else if task.destination.starts_with("ftp://") { "ftp" }
                            else { "http" };
                        let task_proto_str = task_dest_proto.to_string();

                        running.push(tokio::spawn(async move {
                            let (progress_tx, mut progress_rx) =
                                mpsc::unbounded_channel::<ProtocolProgress>();

                            let result = if task.is_upload {
                                execute_upload(&task, &adapter_ref, &config_ref, progress_tx).await
                            } else {
                                adapter_ref.download(&task, progress_tx).await
                            };

                            // 审计日志
                            if let Some(ref al) = audit_ref {
                                match &result {
                                    Ok(_) => al.log_completed(
                                        Direction::Send,
                                        &task_proto_str,
                                        &task_filename,
                                        task_size,
                                        None,
                                        None,
                                    ).await,
                                    Err(e) => al.log_failed(
                                        Direction::Send,
                                        &task_proto_str,
                                        &task_filename,
                                        task_size,
                                        None,
                                        &e.to_string(),
                                    ).await,
                                }
                            }

                            while progress_rx.try_recv().is_ok() {}
                            drop(permit); // 释放 Semaphore
                            (task_id, result.map_err(|e| e.to_string()))
                        }));
                    }
                    None => {
                        // 发送端关闭，等待剩余任务完成
                        while let Some(join_result) = running.next().await {
                            handle_join_result(join_result, &monitor).await;
                        }
                        break;
                    }
                }
            }

            // ── 处理已完成任务 ──────────────────────────────────────────────
            Some(join_result) = running.next() => {
                handle_join_result(join_result, &monitor).await;
            }
        }
    }

    tracing::info!("Transfer worker stopped");
}

async fn handle_join_result(
    join_result: std::result::Result<
        (Uuid, std::result::Result<(), String>),
        tokio::task::JoinError,
    >,
    monitor: &Arc<RwLock<ProgressMonitor>>,
) {
    match join_result {
        Ok((task_id, Ok(()))) => {
            monitor.write().await.complete_transfer(task_id);
            tracing::debug!("Transfer completed: {}", task_id);
        }
        Ok((task_id, Err(e))) => {
            monitor.write().await.fail_transfer(task_id, e.clone());
            tracing::error!("Transfer failed: {} — {}", task_id, e);
        }
        Err(join_err) => {
            tracing::error!("Task panicked: {}", join_err);
        }
    }
}

// ────────────────────────── execute_upload ───────────────────────────────────

async fn execute_upload(
    task: &TransferTask,
    adapter: &Arc<dyn ProtocolAdapter>,
    config: &TransferConfig,
    progress_tx: mpsc::UnboundedSender<ProtocolProgress>,
) -> Result<()> {
    let use_chunked = task.file_size >= config.chunked_threshold && task.file_size > 0;

    if !use_chunked {
        return adapter.upload(task, progress_tx).await;
    }

    let store = ResumeStore::new(&config.resume_state_dir);

    let mut state = if config.enable_resume {
        match store
            .find_by_file(&task.source_path, &task.destination)
            .await
        {
            Ok(Some(existing)) => {
                // 校验源文件大小未变更，防止文件被修改后用旧分片状态续传
                let current_size = tokio::fs::metadata(&task.source_path)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                if current_size != existing.total_size {
                    tracing::warn!(
                        "Source file size changed ({} -> {}), discarding resume state",
                        existing.total_size,
                        current_size
                    );
                    let s = ResumeState::new(
                        task.id,
                        task.source_path.clone(),
                        task.destination.clone(),
                        task.file_size,
                        DEFAULT_CHUNK_SIZE,
                        task.sha256.clone(),
                    );
                    if let Err(e) = store.save(&s).await {
                        tracing::warn!("Failed to save resume state: {}", e);
                    }
                    s
                } else {
                    tracing::info!(
                        "Resuming upload: {}/{} chunks done",
                        existing.completed_chunks.len(),
                        existing.total_chunks
                    );
                    existing
                }
            }
            _ => {
                let s = ResumeState::new(
                    task.id,
                    task.source_path.clone(),
                    task.destination.clone(),
                    task.file_size,
                    DEFAULT_CHUNK_SIZE,
                    task.sha256.clone(),
                );
                if let Err(e) = store.save(&s).await {
                    tracing::warn!("Failed to save resume state: {}", e);
                }
                s
            }
        }
    } else {
        ResumeState::new(
            task.id,
            task.source_path.clone(),
            task.destination.clone(),
            task.file_size,
            DEFAULT_CHUNK_SIZE,
            task.sha256.clone(),
        )
    };

    let result = adapter.upload_chunked(task, &mut state, progress_tx).await;

    if config.enable_resume {
        if result.is_ok() {
            if let Err(e) = store.delete(state.task_id).await {
                tracing::warn!("Failed to delete resume state: {}", e);
            }
        } else if let Err(e) = store.save(&state).await {
            tracing::warn!("Failed to save resume state after failure: {}", e);
        }
    }

    result
}

// ────────────────────────── tests ────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::time::sleep;

    // ── Mock Adapters ─────────────────────────────────────────────────────────

    struct SuccessAdapter {
        upload_count: Arc<AtomicUsize>,
        download_count: Arc<AtomicUsize>,
    }

    impl SuccessAdapter {
        fn new() -> (Arc<Self>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
            let up = Arc::new(AtomicUsize::new(0));
            let down = Arc::new(AtomicUsize::new(0));
            (
                Arc::new(Self {
                    upload_count: Arc::clone(&up),
                    download_count: Arc::clone(&down),
                }),
                up,
                down,
            )
        }
    }

    #[async_trait::async_trait]
    impl ProtocolAdapter for SuccessAdapter {
        async fn upload(
            &self,
            _: &TransferTask,
            _: mpsc::UnboundedSender<ProtocolProgress>,
        ) -> Result<()> {
            self.upload_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn download(
            &self,
            _: &TransferTask,
            _: mpsc::UnboundedSender<ProtocolProgress>,
        ) -> Result<()> {
            self.download_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn upload_chunked(
            &self,
            _: &TransferTask,
            _: &mut ResumeState,
            _: mpsc::UnboundedSender<ProtocolProgress>,
        ) -> Result<()> {
            self.upload_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn protocol_name(&self) -> &'static str {
            "mock-success"
        }
    }

    struct FailAdapter;
    #[async_trait::async_trait]
    impl ProtocolAdapter for FailAdapter {
        async fn upload(
            &self,
            _: &TransferTask,
            _: mpsc::UnboundedSender<ProtocolProgress>,
        ) -> Result<()> {
            Err(AeroSyncError::Network(
                "simulated upload failure".to_string(),
            ))
        }
        async fn download(
            &self,
            _: &TransferTask,
            _: mpsc::UnboundedSender<ProtocolProgress>,
        ) -> Result<()> {
            Err(AeroSyncError::Network(
                "simulated download failure".to_string(),
            ))
        }
        async fn upload_chunked(
            &self,
            _: &TransferTask,
            _: &mut ResumeState,
            _: mpsc::UnboundedSender<ProtocolProgress>,
        ) -> Result<()> {
            Err(AeroSyncError::Network(
                "simulated chunked failure".to_string(),
            ))
        }
        fn protocol_name(&self) -> &'static str {
            "mock-fail"
        }
    }

    /// 有延迟的 adapter，用于验证 Semaphore 实际限流效果
    struct SlowAdapter {
        upload_count: Arc<AtomicUsize>,
        max_concurrent_seen: Arc<AtomicUsize>,
        current_concurrent: Arc<AtomicUsize>,
        delay_ms: u64,
    }

    impl SlowAdapter {
        fn new(delay_ms: u64) -> (Arc<Self>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
            let count = Arc::new(AtomicUsize::new(0));
            let max_seen = Arc::new(AtomicUsize::new(0));
            let current = Arc::new(AtomicUsize::new(0));
            (
                Arc::new(Self {
                    upload_count: Arc::clone(&count),
                    max_concurrent_seen: Arc::clone(&max_seen),
                    current_concurrent: Arc::clone(&current),
                    delay_ms,
                }),
                count,
                max_seen,
            )
        }
    }

    #[async_trait::async_trait]
    impl ProtocolAdapter for SlowAdapter {
        async fn upload(
            &self,
            _: &TransferTask,
            _: mpsc::UnboundedSender<ProtocolProgress>,
        ) -> Result<()> {
            let c = self.current_concurrent.fetch_add(1, Ordering::SeqCst) + 1;
            let mut max = self.max_concurrent_seen.load(Ordering::SeqCst);
            while c > max {
                match self.max_concurrent_seen.compare_exchange(
                    max,
                    c,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(actual) => max = actual,
                }
            }
            sleep(Duration::from_millis(self.delay_ms)).await;
            self.current_concurrent.fetch_sub(1, Ordering::SeqCst);
            self.upload_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn download(
            &self,
            _: &TransferTask,
            _: mpsc::UnboundedSender<ProtocolProgress>,
        ) -> Result<()> {
            Ok(())
        }
        async fn upload_chunked(
            &self,
            task: &TransferTask,
            _: &mut ResumeState,
            tx: mpsc::UnboundedSender<ProtocolProgress>,
        ) -> Result<()> {
            self.upload(task, tx).await
        }
        fn protocol_name(&self) -> &'static str {
            "mock-slow"
        }
    }

    // ── TransferTask ──────────────────────────────────────────────────────────

    #[test]
    fn test_new_upload_task() {
        let task = TransferTask::new_upload(
            PathBuf::from("/src/file.bin"),
            "http://host/upload".to_string(),
            1024,
        );
        assert!(task.is_upload);
        assert_eq!(task.file_size, 1024);
        assert!(task.sha256.is_none());
    }

    #[test]
    fn test_new_download_task() {
        let task = TransferTask::new_download(
            "http://host/file.bin".to_string(),
            PathBuf::from("/dst/file.bin"),
            4096,
        );
        assert!(!task.is_upload);
        assert_eq!(task.file_size, 4096);
    }

    #[test]
    fn test_task_ids_are_unique() {
        let t1 = TransferTask::new_upload(PathBuf::from("/a"), "h".to_string(), 1);
        let t2 = TransferTask::new_upload(PathBuf::from("/b"), "h".to_string(), 1);
        assert_ne!(t1.id, t2.id);
    }

    // ── TransferConfig ────────────────────────────────────────────────────────

    #[test]
    fn test_transfer_config_default() {
        let cfg = TransferConfig::default();
        assert_eq!(cfg.max_concurrent_transfers, 4);
        assert_eq!(cfg.chunk_size, 4 * 1024 * 1024);
        assert_eq!(cfg.chunked_threshold, 64 * 1024 * 1024);
        assert_eq!(cfg.small_file_concurrency, 16);
        assert_eq!(cfg.medium_file_concurrency, 8);
        assert_eq!(cfg.small_file_threshold, 1024 * 1024);
        assert!(cfg.enable_resume);
        assert_eq!(cfg.retry_attempts, 3);
        assert!(cfg.use_quic);
        assert!(cfg.auth_token.is_none());
    }

    #[test]
    fn test_concurrency_for_size_small() {
        let cfg = TransferConfig::default();
        assert_eq!(cfg.concurrency_for_size(512 * 1024), 16);
    }

    #[test]
    fn test_concurrency_for_size_medium() {
        let cfg = TransferConfig::default();
        assert_eq!(cfg.concurrency_for_size(5 * 1024 * 1024), 8);
    }

    #[test]
    fn test_concurrency_for_size_large() {
        let cfg = TransferConfig::default();
        assert_eq!(cfg.concurrency_for_size(100 * 1024 * 1024), 1);
    }

    // ── TransferEngine 基础 ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_engine_add_transfer_registers_progress() {
        let engine = TransferEngine::new(TransferConfig::default());
        let task =
            TransferTask::new_upload(PathBuf::from("/file.bin"), "http://h".to_string(), 512);
        let task_id = task.id;
        engine.add_transfer(task).await.unwrap();
        let monitor = engine.get_progress_monitor().await;
        let m = monitor.read().await;
        assert_eq!(m.get_stats().total_files, 1);
        assert!(m.get_transfer(&task_id).is_some());
    }

    #[tokio::test]
    async fn test_engine_successful_upload_completes() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let file_path = dir.path().join("upload.bin");
        tokio::fs::write(&file_path, b"test data")
            .await
            .expect("failed to write temp file");

        let engine = TransferEngine::new(TransferConfig::default());
        let (adapter, up_count, _) = SuccessAdapter::new();
        engine.start(adapter).await.expect("engine start failed");

        let task = TransferTask::new_upload(file_path, "http://host/upload".to_string(), 9);
        let task_id = task.id;
        engine
            .add_transfer(task)
            .await
            .expect("add_transfer failed");
        sleep(Duration::from_millis(200)).await;

        let monitor = engine.get_progress_monitor().await;
        let m = monitor.read().await;
        assert_eq!(m.get_stats().completed_files, 1);
        assert_eq!(m.get_stats().failed_files, 0);
        assert_eq!(up_count.load(Ordering::SeqCst), 1);
        assert!(matches!(
            m.get_transfer(&task_id).unwrap().status,
            TransferStatus::Completed
        ));
    }

    #[tokio::test]
    async fn test_engine_failed_upload_records_failure() {
        let engine = TransferEngine::new(TransferConfig::default());
        engine.start(Arc::new(FailAdapter)).await.unwrap();

        let task = TransferTask::new_upload(
            PathBuf::from("/any/path.bin"),
            "http://host/upload".to_string(),
            0,
        );
        let task_id = task.id;
        engine.add_transfer(task).await.unwrap();
        sleep(Duration::from_millis(200)).await;

        let monitor = engine.get_progress_monitor().await;
        let m = monitor.read().await;
        assert_eq!(m.get_stats().failed_files, 1);
        assert!(matches!(
            m.get_transfer(&task_id).unwrap().status,
            TransferStatus::Failed(_)
        ));
    }

    // ── 并发测试 ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_engine_100_tasks_all_complete() {
        let cfg = TransferConfig {
            max_concurrent_transfers: 16,
            ..TransferConfig::default()
        };
        let engine = TransferEngine::new(cfg);
        let (adapter, up_count, _) = SuccessAdapter::new();
        engine.start(adapter).await.unwrap();

        for i in 0..100 {
            let task = TransferTask::new_upload(
                PathBuf::from(format!("/file{}.bin", i)),
                "http://host/upload".to_string(),
                100,
            );
            engine.add_transfer(task).await.unwrap();
        }
        sleep(Duration::from_millis(500)).await;

        let m = engine.get_progress_monitor().await;
        assert_eq!(m.read().await.get_stats().completed_files, 100);
        assert_eq!(up_count.load(Ordering::SeqCst), 100);
    }

    #[tokio::test]
    async fn test_semaphore_limits_concurrency() {
        let max_concurrent = 4usize;
        let cfg = TransferConfig {
            max_concurrent_transfers: max_concurrent,
            ..TransferConfig::default()
        };
        let engine = TransferEngine::new(cfg);
        let (adapter, up_count, max_seen) = SlowAdapter::new(30);
        engine.start(adapter).await.unwrap();

        for i in 0..20 {
            let task = TransferTask::new_upload(
                PathBuf::from(format!("/f{}.bin", i)),
                "http://host/upload".to_string(),
                100,
            );
            engine.add_transfer(task).await.unwrap();
        }
        sleep(Duration::from_millis(1500)).await;

        assert_eq!(
            up_count.load(Ordering::SeqCst),
            20,
            "all tasks should complete"
        );
        let observed_max = max_seen.load(Ordering::SeqCst);
        assert!(
            observed_max <= max_concurrent,
            "max concurrent {} should be <= semaphore limit {}",
            observed_max,
            max_concurrent
        );
    }

    #[tokio::test]
    async fn test_engine_small_files_high_concurrency() {
        let cfg = TransferConfig {
            max_concurrent_transfers: 16,
            ..TransferConfig::default()
        };
        let engine = TransferEngine::new(cfg);
        let (adapter, up_count, _) = SuccessAdapter::new();
        engine.start(adapter).await.unwrap();

        for i in 0..50 {
            let task = TransferTask::new_upload(
                PathBuf::from(format!("/small{}.txt", i)),
                "http://host/upload".to_string(),
                512,
            );
            engine.add_transfer(task).await.unwrap();
        }
        sleep(Duration::from_millis(500)).await;
        assert_eq!(up_count.load(Ordering::SeqCst), 50);
    }

    #[tokio::test]
    async fn test_engine_download_calls_download_on_adapter() {
        let engine = TransferEngine::new(TransferConfig::default());
        let (adapter, _, down_count) = SuccessAdapter::new();
        engine.start(adapter).await.unwrap();

        let task = TransferTask::new_download(
            "http://host/file.bin".to_string(),
            PathBuf::from("/dst/file.bin"),
            0,
        );
        engine.add_transfer(task).await.unwrap();
        sleep(Duration::from_millis(200)).await;
        assert_eq!(down_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_engine_multiple_uploads_all_complete() {
        let engine = TransferEngine::new(TransferConfig::default());
        let (adapter, up_count, _) = SuccessAdapter::new();
        engine.start(adapter).await.unwrap();

        for i in 0..5 {
            let task = TransferTask::new_upload(
                PathBuf::from(format!("/file{}.bin", i)),
                "http://host/upload".to_string(),
                0,
            );
            engine.add_transfer(task).await.unwrap();
        }
        sleep(Duration::from_millis(300)).await;
        let m = engine.get_progress_monitor().await;
        assert_eq!(m.read().await.get_stats().completed_files, 5);
        assert_eq!(up_count.load(Ordering::SeqCst), 5);
    }

    // ── RFC-002 §14 Task #8: TransferEngine::send → Receipt ─────────

    use crate::core::receipt::{CompletedTerminal, FailedTerminal, State};

    /// Wait up to `timeout_ms` for the receipt's state to satisfy
    /// `pred`, polling its `watch` channel. Panics on timeout with a
    /// message including the last-observed state.
    async fn wait_for_receipt<F>(
        receipt: &Arc<Receipt<Sender>>,
        timeout_ms: u64,
        pred: F,
        label: &str,
    ) -> State
    where
        F: Fn(&State) -> bool,
    {
        let start = std::time::Instant::now();
        let mut rx = receipt.watch();
        loop {
            {
                let snapshot = rx.borrow_and_update().clone();
                if pred(&snapshot) {
                    return snapshot;
                }
                if start.elapsed() > Duration::from_millis(timeout_ms) {
                    panic!(
                        "wait_for_receipt({label}) timed out after {timeout_ms}ms; last={snapshot:?}"
                    );
                }
            }
            let _ = tokio::time::timeout(Duration::from_millis(50), rx.changed()).await;
        }
    }

    #[tokio::test]
    async fn test_send_returns_receipt_registered_in_registry() {
        let engine = TransferEngine::new(TransferConfig::default());
        let (adapter, _, _) = SuccessAdapter::new();
        engine.start(adapter).await.unwrap();

        let task = TransferTask::new_upload(
            PathBuf::from("/no-such-file.bin"),
            "http://h/upload".to_string(),
            0,
        );
        let receipt = engine.send(task).await.expect("send must succeed");
        assert!(engine.receipt_registry().contains(receipt.id()));
    }

    #[tokio::test]
    async fn test_send_drives_receipt_through_processing_then_external_ack() {
        // Without a wired QUIC receipt-stream reader the engine drives
        // the receipt only as far as `Processing`; the receiver-side
        // Ack is applied externally to demonstrate the full happy path.
        let engine = TransferEngine::new(TransferConfig::default());
        let (adapter, up_count, _) = SuccessAdapter::new();
        engine.start(adapter).await.unwrap();

        let task =
            TransferTask::new_upload(PathBuf::from("/some.bin"), "http://h/upload".to_string(), 0);
        let receipt = engine.send(task).await.unwrap();

        wait_for_receipt(
            &receipt,
            2_000,
            |s| matches!(s, State::Processing),
            "Processing",
        )
        .await;
        assert_eq!(up_count.load(Ordering::SeqCst), 1);

        receipt.apply_event(Event::Ack).expect("ack must succeed");
        let final_state = receipt.state();
        assert!(matches!(
            final_state,
            State::Completed(CompletedTerminal::Acked)
        ));
    }

    #[tokio::test]
    async fn test_send_failed_transfer_drives_receipt_to_errored() {
        let engine = TransferEngine::new(TransferConfig::default());
        engine.start(Arc::new(FailAdapter)).await.unwrap();

        let task =
            TransferTask::new_upload(PathBuf::from("/no.bin"), "http://h/upload".to_string(), 0);
        let receipt = engine.send(task).await.unwrap();

        let terminal = wait_for_receipt(
            &receipt,
            2_000,
            |s| matches!(s, State::Failed(FailedTerminal::Errored { .. })),
            "Errored",
        )
        .await;
        match terminal {
            State::Failed(FailedTerminal::Errored { detail, .. }) => {
                assert!(detail.contains("simulated"), "detail={detail}");
            }
            other => panic!("expected Errored, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_cancel_transfer_drives_receipt_to_cancelled() {
        let cfg = TransferConfig {
            max_concurrent_transfers: 1,
            ..TransferConfig::default()
        };
        let engine = TransferEngine::new(cfg);
        let (adapter, _, _) = SlowAdapter::new(500);
        engine.start(adapter).await.unwrap();

        let task =
            TransferTask::new_upload(PathBuf::from("/slow.bin"), "http://h/upload".to_string(), 0);
        let task_id = task.id;
        let receipt = engine.send(task).await.unwrap();
        // Cancel before the slow upload completes.
        sleep(Duration::from_millis(50)).await;
        engine.cancel_transfer(task_id).await.unwrap();

        let terminal = wait_for_receipt(
            &receipt,
            2_000,
            |s| matches!(s, State::Failed(FailedTerminal::Cancelled { .. })),
            "Cancelled",
        )
        .await;
        match terminal {
            State::Failed(FailedTerminal::Cancelled { reason }) => {
                assert!(!reason.is_empty());
            }
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_send_with_history_store_persists_terminal() {
        let dir = TempDir::new().unwrap();
        let history = Arc::new(
            HistoryStore::new(&dir.path().join("h.jsonl"))
                .await
                .unwrap(),
        );
        let engine =
            TransferEngine::new(TransferConfig::default()).with_history_store(history.clone());
        let (adapter, _, _) = SuccessAdapter::new();
        engine.start(adapter).await.unwrap();

        let task =
            TransferTask::new_upload(PathBuf::from("/h.bin"), "http://h/upload".to_string(), 0);
        let receipt = engine.send(task).await.unwrap();
        wait_for_receipt(
            &receipt,
            2_000,
            |s| matches!(s, State::Processing),
            "Processing",
        )
        .await;
        receipt.apply_event(Event::Ack).unwrap();

        // Give the watch-bridge a moment to land the terminal.
        for _ in 0..40 {
            if history
                .query_by_receipt(receipt.id())
                .await
                .unwrap()
                .and_then(|e| e.receipt_state)
                .is_some()
            {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
        let entry = history
            .query_by_receipt(receipt.id())
            .await
            .unwrap()
            .expect("history entry must exist");
        assert_eq!(
            entry.receipt_state,
            Some(crate::core::ReceiptStateLabel::Acked)
        );
    }

    #[tokio::test]
    async fn test_protocol_progress_bridging() {
        struct ProgressAdapter;
        #[async_trait::async_trait]
        impl ProtocolAdapter for ProgressAdapter {
            async fn upload(
                &self,
                task: &TransferTask,
                tx: mpsc::UnboundedSender<ProtocolProgress>,
            ) -> Result<()> {
                let total = task.file_size;
                for step in [25u64, 50, 75, 100] {
                    let _ = tx.send(ProtocolProgress {
                        bytes_transferred: total * step / 100,
                        transfer_speed: 1024.0 * step as f64,
                    });
                }
                Ok(())
            }
            async fn download(
                &self,
                _: &TransferTask,
                _: mpsc::UnboundedSender<ProtocolProgress>,
            ) -> Result<()> {
                Ok(())
            }
            async fn upload_chunked(
                &self,
                task: &TransferTask,
                _: &mut ResumeState,
                tx: mpsc::UnboundedSender<ProtocolProgress>,
            ) -> Result<()> {
                self.upload(task, tx).await
            }
            fn protocol_name(&self) -> &'static str {
                "mock-progress"
            }
        }

        let engine = TransferEngine::new(TransferConfig::default());
        engine.start(Arc::new(ProgressAdapter)).await.unwrap();
        let task = TransferTask::new_upload(
            PathBuf::from("/big.bin"),
            "http://host/upload".to_string(),
            1024 * 1024,
        );
        engine.add_transfer(task).await.unwrap();
        sleep(Duration::from_millis(200)).await;
        let m = engine.get_progress_monitor().await;
        assert_eq!(m.read().await.get_stats().completed_files, 1);
    }

    // ── RFC-003 Group A: send_with_metadata + system-field stamping ──

    use crate::core::metadata::MetadataBuilder;
    use aerosync_proto::Lifecycle;

    #[tokio::test]
    async fn test_send_with_metadata_seals_system_fields() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("hello.txt");
        tokio::fs::write(&path, b"hello world\n").await.unwrap();

        let engine = TransferEngine::new(TransferConfig::default()).with_node_id("alice");
        let (adapter, _, _) = SuccessAdapter::new();
        engine.start(adapter).await.unwrap();

        let mut task =
            TransferTask::new_upload(path.clone(), "http://archiver/upload".to_string(), 12);
        task.sha256 = Some("ab".repeat(32));

        let user_envelope = MetadataBuilder::new()
            .trace_id("run-7")
            .lifecycle(Lifecycle::Durable)
            .user("tenant", "acme")
            .build()
            .unwrap();

        let receipt = engine
            .send_with_metadata(task, user_envelope)
            .await
            .expect("send_with_metadata");
        let sealed = engine
            .metadata_for(receipt.id())
            .await
            .expect("sealed metadata present");

        assert_eq!(sealed.id, receipt.id().to_string());
        assert_eq!(sealed.from_node, "alice");
        assert_eq!(sealed.to_node, "archiver");
        assert_eq!(sealed.protocol, "http");
        assert_eq!(sealed.size_bytes, 12);
        assert_eq!(sealed.sha256, "ab".repeat(32));
        assert_eq!(sealed.file_name, "hello.txt");
        // Sniffed from the file extension (no magic bytes match for
        // ASCII text).
        assert_eq!(sealed.content_type, "text/plain");
        assert!(sealed.created_at.is_some());

        // Caller-supplied well-known + user fields survive intact.
        assert_eq!(sealed.trace_id.as_deref(), Some("run-7"));
        assert_eq!(sealed.lifecycle, Some(Lifecycle::Durable as i32));
        assert_eq!(sealed.user_metadata["tenant"], "acme");
    }

    #[tokio::test]
    async fn test_send_uses_empty_metadata_but_still_seals_system_fields() {
        let engine = TransferEngine::new(TransferConfig::default()).with_node_id("alice");
        let (adapter, _, _) = SuccessAdapter::new();
        engine.start(adapter).await.unwrap();
        let task = TransferTask::new_upload(
            PathBuf::from("/no-such.bin"),
            "quic://bob/upload".to_string(),
            0,
        );
        let receipt = engine.send(task).await.unwrap();
        let sealed = engine.metadata_for(receipt.id()).await.unwrap();
        assert_eq!(sealed.from_node, "alice");
        assert_eq!(sealed.to_node, "bob");
        assert_eq!(sealed.protocol, "quic");
        assert_eq!(sealed.file_name, "no-such.bin");
        // No user-supplied content_type, no readable bytes → fallback.
        assert_eq!(sealed.content_type, "application/octet-stream");
        assert!(sealed.trace_id.is_none());
        assert!(sealed.user_metadata.is_empty());
    }

    #[tokio::test]
    async fn test_send_with_metadata_content_type_override_wins() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("data.bin");
        // PNG magic bytes — sniffer would normally return image/png.
        tokio::fs::write(
            &path,
            [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0],
        )
        .await
        .unwrap();

        let engine = TransferEngine::new(TransferConfig::default());
        let (adapter, _, _) = SuccessAdapter::new();
        engine.start(adapter).await.unwrap();

        let envelope = MetadataBuilder::new()
            .content_type("application/x-custom")
            .build()
            .unwrap();
        let task = TransferTask::new_upload(path, "http://h/upload".to_string(), 12);
        let receipt = engine.send_with_metadata(task, envelope).await.unwrap();
        let sealed = engine.metadata_for(receipt.id()).await.unwrap();
        assert_eq!(sealed.content_type, "application/x-custom");
    }

    #[tokio::test]
    async fn test_send_with_metadata_overrides_user_supplied_system_fields() {
        // RFC-003 §4.1: system fields cannot be spoofed. Even if the
        // caller pre-populates `id`/`sha256`/etc on the envelope they
        // hand to the engine, the engine MUST overwrite them.
        let engine = TransferEngine::new(TransferConfig::default()).with_node_id("alice");
        let (adapter, _, _) = SuccessAdapter::new();
        engine.start(adapter).await.unwrap();
        let mut spoofed = empty_metadata();
        spoofed.id = "fake-id".into();
        spoofed.sha256 = "fake-hash".into();
        spoofed.from_node = "mallory".into();
        spoofed.protocol = "ftp".into();

        let mut task =
            TransferTask::new_upload(PathBuf::from("/x.bin"), "http://h/upload".to_string(), 7);
        task.sha256 = Some("ab".repeat(32));
        let receipt = engine.send_with_metadata(task, spoofed).await.unwrap();
        let sealed = engine.metadata_for(receipt.id()).await.unwrap();
        assert_eq!(sealed.id, receipt.id().to_string());
        assert_eq!(sealed.sha256, "ab".repeat(32));
        assert_eq!(sealed.from_node, "alice");
        assert_eq!(sealed.protocol, "http");
    }

    #[tokio::test]
    async fn test_send_rejects_oversize_metadata() {
        // Engineer an envelope that *barely* fits builder validation
        // (because the builder runs the cap on the application-shaped
        // metadata) but pushes past 64 KiB once system fields are
        // stamped in. Easiest path: bypass MetadataBuilder, hand a
        // raw Metadata that already exceeds the cap.
        let mut huge = empty_metadata();
        for i in 0..6 {
            huge.user_metadata
                .insert(format!("k{i}"), "x".repeat(16 * 1024));
        }
        let engine = TransferEngine::new(TransferConfig::default());
        let (adapter, _, _) = SuccessAdapter::new();
        engine.start(adapter).await.unwrap();
        let task =
            TransferTask::new_upload(PathBuf::from("/x.bin"), "http://h/upload".to_string(), 0);
        let err = engine.send_with_metadata(task, huge).await.unwrap_err();
        assert!(
            matches!(err, AeroSyncError::InvalidConfig(_)),
            "expected InvalidConfig, got {err:?}"
        );
    }

    #[test]
    fn test_parse_to_node_strips_scheme_and_port() {
        assert_eq!(parse_to_node("http://archiver:8080/u"), "archiver");
        assert_eq!(parse_to_node("https://h.example.com/x"), "h.example.com");
        assert_eq!(parse_to_node("quic://bob/upload"), "bob");
        assert_eq!(parse_to_node("s3://my-bucket/k"), "my-bucket");
        assert_eq!(parse_to_node("ftp://ftp.example.com"), "ftp.example.com");
        assert_eq!(parse_to_node("archiver"), "archiver");
        assert_eq!(parse_to_node(""), "");
    }

    #[tokio::test]
    async fn test_with_node_id_overrides_default_hostname() {
        let engine = TransferEngine::new(TransferConfig::default()).with_node_id("custom-node");
        assert_eq!(engine.node_id().await, "custom-node");
    }

    #[tokio::test]
    async fn test_metadata_pruned_after_terminal() {
        // After the receipt reaches a terminal state the watch-bridge
        // GCs `sealed_metadata`; this keeps long-lived senders from
        // leaking a copy of every envelope they ever sent.
        let engine = TransferEngine::new(TransferConfig::default());
        let (adapter, _, _) = SuccessAdapter::new();
        engine.start(adapter).await.unwrap();
        let task =
            TransferTask::new_upload(PathBuf::from("/x.bin"), "http://h/upload".to_string(), 0);
        let receipt = engine
            .send_with_metadata(task, empty_metadata())
            .await
            .unwrap();
        // Drive to terminal so the bridge runs its cleanup.
        wait_for_receipt(
            &receipt,
            2_000,
            |s| matches!(s, State::Processing),
            "Processing",
        )
        .await;
        receipt.apply_event(Event::Ack).unwrap();
        // Allow the bridge a few ticks to GC.
        for _ in 0..40 {
            if engine.metadata_for(receipt.id()).await.is_none() {
                return;
            }
            sleep(Duration::from_millis(25)).await;
        }
        panic!("sealed_metadata still present after receipt terminal");
    }
}

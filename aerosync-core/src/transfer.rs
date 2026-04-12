use crate::audit::{AuditLogger, Direction};
use crate::{AeroSyncError, Result, ProgressMonitor, TransferProgress};
use crate::progress::TransferStatus;
use crate::resume::{ResumeState, ResumeStore, DEFAULT_CHUNK_SIZE};
use futures::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock, Semaphore};
use uuid::Uuid;
use zeroize::Zeroizing;

// ────────────────────────── configuration ───────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferConfig {
    /// 最大并发传输任务数（Semaphore permits）
    pub max_concurrent_transfers: usize,
    pub chunk_size: usize,
    pub retry_attempts: u32,
    pub timeout_seconds: u64,
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
            chunked_threshold: 64 * 1024 * 1024,  // 64MB
            resume_state_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            enable_resume: true,
            small_file_concurrency: 16,
            medium_file_concurrency: 8,
            small_file_threshold: 1024 * 1024,     // 1MB
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
        }
    }

    /// 启动引擎，注入协议适配器
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

    pub async fn cancel_transfer(&self, task_id: Uuid) -> Result<()> {
        self.cancel_sender
            .as_ref()
            .ok_or_else(|| AeroSyncError::System("Transfer engine not started".to_string()))?
            .send(task_id)
            .map_err(|_| AeroSyncError::System("Failed to send cancel signal".to_string()))?;
        Ok(())
    }

    pub async fn get_progress_monitor(&self) -> Arc<RwLock<ProgressMonitor>> {
        Arc::clone(&self.progress_monitor)
    }
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
    join_result: std::result::Result<(Uuid, std::result::Result<(), String>), tokio::task::JoinError>,
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
        match store.find_by_file(&task.source_path, &task.destination).await {
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
        async fn upload(&self, _: &TransferTask, _: mpsc::UnboundedSender<ProtocolProgress>) -> Result<()> {
            self.upload_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn download(&self, _: &TransferTask, _: mpsc::UnboundedSender<ProtocolProgress>) -> Result<()> {
            self.download_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn upload_chunked(&self, _: &TransferTask, _: &mut ResumeState, _: mpsc::UnboundedSender<ProtocolProgress>) -> Result<()> {
            self.upload_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn protocol_name(&self) -> &'static str { "mock-success" }
    }

    struct FailAdapter;
    #[async_trait::async_trait]
    impl ProtocolAdapter for FailAdapter {
        async fn upload(&self, _: &TransferTask, _: mpsc::UnboundedSender<ProtocolProgress>) -> Result<()> {
            Err(AeroSyncError::Network("simulated upload failure".to_string()))
        }
        async fn download(&self, _: &TransferTask, _: mpsc::UnboundedSender<ProtocolProgress>) -> Result<()> {
            Err(AeroSyncError::Network("simulated download failure".to_string()))
        }
        async fn upload_chunked(&self, _: &TransferTask, _: &mut ResumeState, _: mpsc::UnboundedSender<ProtocolProgress>) -> Result<()> {
            Err(AeroSyncError::Network("simulated chunked failure".to_string()))
        }
        fn protocol_name(&self) -> &'static str { "mock-fail" }
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
        async fn upload(&self, _: &TransferTask, _: mpsc::UnboundedSender<ProtocolProgress>) -> Result<()> {
            let c = self.current_concurrent.fetch_add(1, Ordering::SeqCst) + 1;
            let mut max = self.max_concurrent_seen.load(Ordering::SeqCst);
            while c > max {
                match self.max_concurrent_seen.compare_exchange(max, c, Ordering::SeqCst, Ordering::SeqCst) {
                    Ok(_) => break,
                    Err(actual) => max = actual,
                }
            }
            sleep(Duration::from_millis(self.delay_ms)).await;
            self.current_concurrent.fetch_sub(1, Ordering::SeqCst);
            self.upload_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn download(&self, _: &TransferTask, _: mpsc::UnboundedSender<ProtocolProgress>) -> Result<()> { Ok(()) }
        async fn upload_chunked(&self, task: &TransferTask, _: &mut ResumeState, tx: mpsc::UnboundedSender<ProtocolProgress>) -> Result<()> {
            self.upload(task, tx).await
        }
        fn protocol_name(&self) -> &'static str { "mock-slow" }
    }

    // ── TransferTask ──────────────────────────────────────────────────────────

    #[test]
    fn test_new_upload_task() {
        let task = TransferTask::new_upload(PathBuf::from("/src/file.bin"), "http://host/upload".to_string(), 1024);
        assert!(task.is_upload);
        assert_eq!(task.file_size, 1024);
        assert!(task.sha256.is_none());
    }

    #[test]
    fn test_new_download_task() {
        let task = TransferTask::new_download("http://host/file.bin".to_string(), PathBuf::from("/dst/file.bin"), 4096);
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
        let task = TransferTask::new_upload(PathBuf::from("/file.bin"), "http://h".to_string(), 512);
        let task_id = task.id;
        engine.add_transfer(task).await.unwrap();
        let monitor = engine.get_progress_monitor().await;
        let m = monitor.read().await;
        assert_eq!(m.get_stats().total_files, 1);
        assert!(m.get_transfer(&task_id).is_some());
    }

    #[tokio::test]
    async fn test_engine_successful_upload_completes() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("upload.bin");
        tokio::fs::write(&file_path, b"test data").await.unwrap();

        let engine = TransferEngine::new(TransferConfig::default());
        let (adapter, up_count, _) = SuccessAdapter::new();
        engine.start(adapter).await.unwrap();

        let task = TransferTask::new_upload(file_path, "http://host/upload".to_string(), 9);
        let task_id = task.id;
        engine.add_transfer(task).await.unwrap();
        sleep(Duration::from_millis(200)).await;

        let monitor = engine.get_progress_monitor().await;
        let m = monitor.read().await;
        assert_eq!(m.get_stats().completed_files, 1);
        assert_eq!(m.get_stats().failed_files, 0);
        assert_eq!(up_count.load(Ordering::SeqCst), 1);
        assert!(matches!(m.get_transfer(&task_id).unwrap().status, TransferStatus::Completed));
    }

    #[tokio::test]
    async fn test_engine_failed_upload_records_failure() {
        let engine = TransferEngine::new(TransferConfig::default());
        engine.start(Arc::new(FailAdapter)).await.unwrap();

        let task = TransferTask::new_upload(PathBuf::from("/any/path.bin"), "http://host/upload".to_string(), 0);
        let task_id = task.id;
        engine.add_transfer(task).await.unwrap();
        sleep(Duration::from_millis(200)).await;

        let monitor = engine.get_progress_monitor().await;
        let m = monitor.read().await;
        assert_eq!(m.get_stats().failed_files, 1);
        assert!(matches!(m.get_transfer(&task_id).unwrap().status, TransferStatus::Failed(_)));
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

        assert_eq!(up_count.load(Ordering::SeqCst), 20, "all tasks should complete");
        let observed_max = max_seen.load(Ordering::SeqCst);
        assert!(
            observed_max <= max_concurrent,
            "max concurrent {} should be <= semaphore limit {}",
            observed_max, max_concurrent
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

        let task = TransferTask::new_download("http://host/file.bin".to_string(), PathBuf::from("/dst/file.bin"), 0);
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
            let task = TransferTask::new_upload(PathBuf::from(format!("/file{}.bin", i)), "http://host/upload".to_string(), 0);
            engine.add_transfer(task).await.unwrap();
        }
        sleep(Duration::from_millis(300)).await;
        let m = engine.get_progress_monitor().await;
        assert_eq!(m.read().await.get_stats().completed_files, 5);
        assert_eq!(up_count.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn test_protocol_progress_bridging() {
        struct ProgressAdapter;
        #[async_trait::async_trait]
        impl ProtocolAdapter for ProgressAdapter {
            async fn upload(&self, task: &TransferTask, tx: mpsc::UnboundedSender<ProtocolProgress>) -> Result<()> {
                let total = task.file_size;
                for step in [25u64, 50, 75, 100] {
                    let _ = tx.send(ProtocolProgress {
                        bytes_transferred: total * step / 100,
                        transfer_speed: 1024.0 * step as f64,
                    });
                }
                Ok(())
            }
            async fn download(&self, _: &TransferTask, _: mpsc::UnboundedSender<ProtocolProgress>) -> Result<()> { Ok(()) }
            async fn upload_chunked(&self, task: &TransferTask, _: &mut ResumeState, tx: mpsc::UnboundedSender<ProtocolProgress>) -> Result<()> {
                self.upload(task, tx).await
            }
            fn protocol_name(&self) -> &'static str { "mock-progress" }
        }

        let engine = TransferEngine::new(TransferConfig::default());
        engine.start(Arc::new(ProgressAdapter)).await.unwrap();
        let task = TransferTask::new_upload(PathBuf::from("/big.bin"), "http://host/upload".to_string(), 1024 * 1024);
        engine.add_transfer(task).await.unwrap();
        sleep(Duration::from_millis(200)).await;
        let m = engine.get_progress_monitor().await;
        assert_eq!(m.read().await.get_stats().completed_files, 1);
    }
}

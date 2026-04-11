use crate::{AeroSyncError, Result, ProgressMonitor, TransferProgress};
use crate::progress::TransferStatus;
use crate::resume::{ResumeState, ResumeStore, DEFAULT_CHUNK_SIZE};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use uuid::Uuid;

// ────────────────────────── configuration ───────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferConfig {
    pub max_concurrent_transfers: usize,
    pub chunk_size: usize,
    pub retry_attempts: u32,
    pub timeout_seconds: u64,
    pub use_quic: bool,
    /// 发送方认证 Token
    pub auth_token: Option<String>,
    /// 大于此阈值（bytes）时使用分片上传，默认 64MB
    pub chunked_threshold: u64,
    /// 续传状态文件存放目录（默认为当前工作目录）
    pub resume_state_dir: PathBuf,
    /// 是否启用断点续传（默认 true）
    pub enable_resume: bool,
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
// 这是 aerosync-core 自己定义的轻量 trait，aerosync-protocols 实现它。
// 由外层（main.rs / app crate）注入具体实现，避免循环依赖。

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
    /// `base_url`: 形如 `http://host:port`，不含路径
    /// `state`: 当前续传状态（会被修改以记录已完成分片）
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
        }
    }

    /// 启动引擎，注入协议适配器
    pub async fn start(&self, adapter: Arc<dyn ProtocolAdapter>) -> Result<()> {
        let task_rx = self.task_receiver.write().await.take();
        let cancel_rx = self.cancel_receiver.write().await.take();

        if let (Some(task_rx), Some(cancel_rx)) = (task_rx, cancel_rx) {
            let monitor = Arc::clone(&self.progress_monitor);
            let config = self.config.clone();

            let handle = tokio::spawn(async move {
                transfer_worker(task_rx, cancel_rx, monitor, adapter, config).await;
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

// ────────────────────────── worker ──────────────────────────────────────────

async fn transfer_worker(
    mut task_rx: mpsc::UnboundedReceiver<TransferTask>,
    mut _cancel_rx: mpsc::UnboundedReceiver<Uuid>,
    monitor: Arc<RwLock<ProgressMonitor>>,
    adapter: Arc<dyn ProtocolAdapter>,
    config: TransferConfig,
) {
    tracing::info!("Transfer worker started (protocol: {})", adapter.protocol_name());

    while let Some(task) = task_rx.recv().await {
        tracing::info!(
            "Transfer: {} -> {}",
            task.source_path.display(),
            task.destination
        );

        monitor.write().await.update_progress(task.id, 0, 0.0);

        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<ProtocolProgress>();

        let result = if task.is_upload {
            execute_upload(&task, &adapter, &config, progress_tx).await
        } else {
            adapter.download(&task, progress_tx).await
        };

        // 排空进度 channel
        while progress_rx.try_recv().is_ok() {}

        match result {
            Ok(_) => {
                monitor.write().await.complete_transfer(task.id);
                tracing::info!("Transfer completed: {}", task.source_path.display());
            }
            Err(e) => {
                monitor.write().await.fail_transfer(task.id, e.to_string());
                tracing::error!("Transfer failed: {} — {}", task.source_path.display(), e);
            }
        }
    }

    tracing::info!("Transfer worker stopped");
}

/// 执行上传：根据文件大小自动选择普通上传或分片续传
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

    // 分片上传路径
    let store = ResumeStore::new(&config.resume_state_dir);

    // 查找已有续传状态
    let mut state = if config.enable_resume {
        match store.find_by_file(&task.source_path, &task.destination).await {
            Ok(Some(existing)) => {
                tracing::info!(
                    "Resuming upload: {}/{} chunks done",
                    existing.completed_chunks.len(),
                    existing.total_chunks
                );
                existing
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
                store.save(&s).await.ok();
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

    // 每完成一片立即持久化
    let result = adapter.upload_chunked(task, &mut state, progress_tx).await;

    if config.enable_resume {
        if result.is_ok() {
            store.delete(state.task_id).await.ok();
        } else {
            // 保存最新进度（部分完成）
            store.save(&state).await.ok();
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    // ── Mock Adapter ─────────────────────────────────────────────────────────

    /// 成功的 Mock adapter，记录调用次数
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
            _task: &TransferTask,
            _tx: mpsc::UnboundedSender<ProtocolProgress>,
        ) -> Result<()> {
            self.upload_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn download(
            &self,
            _task: &TransferTask,
            _tx: mpsc::UnboundedSender<ProtocolProgress>,
        ) -> Result<()> {
            self.download_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn upload_chunked(
            &self,
            _task: &TransferTask,
            _state: &mut ResumeState,
            _tx: mpsc::UnboundedSender<ProtocolProgress>,
        ) -> Result<()> {
            self.upload_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn protocol_name(&self) -> &'static str {
            "mock-success"
        }
    }

    /// 总是失败的 Mock adapter
    struct FailAdapter;

    #[async_trait::async_trait]
    impl ProtocolAdapter for FailAdapter {
        async fn upload(
            &self,
            _task: &TransferTask,
            _tx: mpsc::UnboundedSender<ProtocolProgress>,
        ) -> Result<()> {
            Err(AeroSyncError::Network("simulated upload failure".to_string()))
        }

        async fn download(
            &self,
            _task: &TransferTask,
            _tx: mpsc::UnboundedSender<ProtocolProgress>,
        ) -> Result<()> {
            Err(AeroSyncError::Network("simulated download failure".to_string()))
        }

        async fn upload_chunked(
            &self,
            _task: &TransferTask,
            _state: &mut ResumeState,
            _tx: mpsc::UnboundedSender<ProtocolProgress>,
        ) -> Result<()> {
            Err(AeroSyncError::Network("simulated chunked failure".to_string()))
        }

        fn protocol_name(&self) -> &'static str {
            "mock-fail"
        }
    }

    // ── TransferTask ─────────────────────────────────────────────────────────

    #[test]
    fn test_new_upload_task() {
        let task = TransferTask::new_upload(
            PathBuf::from("/src/file.bin"),
            "http://host/upload".to_string(),
            1024,
        );
        assert!(task.is_upload);
        assert_eq!(task.file_size, 1024);
        assert_eq!(task.destination, "http://host/upload");
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

    #[test]
    fn test_transfer_config_default() {
        let cfg = TransferConfig::default();
        assert_eq!(cfg.max_concurrent_transfers, 4);
        assert_eq!(cfg.chunk_size, 4 * 1024 * 1024);
        assert_eq!(cfg.chunked_threshold, 64 * 1024 * 1024);
        assert!(cfg.enable_resume);
        assert_eq!(cfg.retry_attempts, 3);
        assert!(cfg.use_quic);
        assert!(cfg.auth_token.is_none());
    }

    // ── TransferEngine ───────────────────────────────────────────────────────

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

        // give the worker time to process
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let monitor = engine.get_progress_monitor().await;
        let m = monitor.read().await;
        let stats = m.get_stats();
        assert_eq!(stats.completed_files, 1);
        assert_eq!(stats.failed_files, 0);
        assert_eq!(up_count.load(Ordering::SeqCst), 1);

        let t = m.get_transfer(&task_id).unwrap();
        assert!(matches!(t.status, crate::progress::TransferStatus::Completed));
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

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let monitor = engine.get_progress_monitor().await;
        let m = monitor.read().await;
        let stats = m.get_stats();
        assert_eq!(stats.completed_files, 0);
        assert_eq!(stats.failed_files, 1);

        let t = m.get_transfer(&task_id).unwrap();
        assert!(matches!(t.status, crate::progress::TransferStatus::Failed(_)));
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

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let monitor = engine.get_progress_monitor().await;
        let m = monitor.read().await;
        assert_eq!(m.get_stats().completed_files, 5);
        assert_eq!(up_count.load(Ordering::SeqCst), 5);
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

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(down_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_protocol_progress_bridging() {
        // Adapter that sends progress events
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
                _task: &TransferTask,
                _tx: mpsc::UnboundedSender<ProtocolProgress>,
            ) -> Result<()> {
                Ok(())
            }

            async fn upload_chunked(
                &self,
                task: &TransferTask,
                _state: &mut ResumeState,
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

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let monitor = engine.get_progress_monitor().await;
        let m = monitor.read().await;
        assert_eq!(m.get_stats().completed_files, 1);
    }
}

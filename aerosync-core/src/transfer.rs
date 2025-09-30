use crate::{AeroSyncError, Result, ProgressMonitor, TransferProgress};
use crate::progress::TransferStatus;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferConfig {
    pub max_concurrent_transfers: usize,
    pub chunk_size: usize,
    pub retry_attempts: u32,
    pub timeout_seconds: u64,
    pub use_quic: bool,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            max_concurrent_transfers: 4,
            chunk_size: 1024 * 1024, // 1MB
            retry_attempts: 3,
            timeout_seconds: 30,
            use_quic: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TransferTask {
    pub id: Uuid,
    pub source_path: PathBuf,
    pub destination: String, // URL or path
    pub file_size: u64,
    pub is_upload: bool,
}

impl TransferTask {
    pub fn new_upload(source_path: PathBuf, destination: String, file_size: u64) -> Self {
        Self {
            id: Uuid::new_v4(),
            source_path,
            destination,
            file_size,
            is_upload: true,
        }
    }

    pub fn new_download(source_url: String, destination_path: PathBuf, file_size: u64) -> Self {
        Self {
            id: Uuid::new_v4(),
            source_path: destination_path,
            destination: source_url,
            file_size,
            is_upload: false,
        }
    }
}

pub struct TransferEngine {
    config: TransferConfig,
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
        let progress_monitor = Arc::new(RwLock::new(ProgressMonitor::new()));
        
        Self {
            config,
            progress_monitor,
            task_sender: Some(task_sender),
            cancel_sender: Some(cancel_sender),
            task_receiver: Arc::new(RwLock::new(Some(task_receiver))),
            cancel_receiver: Arc::new(RwLock::new(Some(cancel_receiver))),
            _task_handle: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn start(&self) -> Result<()> {
        // Take the receivers and spawn the worker task
        let task_receiver = self.task_receiver.write().await.take();
        let cancel_receiver = self.cancel_receiver.write().await.take();
        
        if let (Some(task_receiver), Some(cancel_receiver)) = (task_receiver, cancel_receiver) {
            let worker_progress_monitor = Arc::clone(&self.progress_monitor);
            let worker_config = self.config.clone();
            
            let task_handle = tokio::spawn(async move {
                transfer_worker(
                    task_receiver,
                    cancel_receiver,
                    worker_progress_monitor,
                    worker_config,
                ).await;
            });
            
            *self._task_handle.write().await = Some(task_handle);
            tracing::info!("Transfer engine started");
        }
        
        Ok(())
    }

    pub async fn add_transfer(&self, task: TransferTask) -> Result<()> {
        let progress = TransferProgress {
            task_id: task.id,
            file_name: task.source_path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            bytes_transferred: 0,
            total_bytes: task.file_size,
            transfer_speed: 0.0,
            elapsed_time: std::time::Duration::new(0, 0),
            estimated_remaining: None,
            status: TransferStatus::Pending,
        };

        {
            let mut monitor = self.progress_monitor.write().await;
            monitor.add_transfer(progress);
        }

        if let Some(ref sender) = self.task_sender {
            sender.send(task)
                .map_err(|_| AeroSyncError::System("Failed to queue transfer task".to_string()))?;
        } else {
            return Err(AeroSyncError::System("Transfer engine not started".to_string()));
        }

        Ok(())
    }

    pub async fn cancel_transfer(&self, task_id: Uuid) -> Result<()> {
        if let Some(ref sender) = self.cancel_sender {
            sender.send(task_id)
                .map_err(|_| AeroSyncError::System("Failed to send cancel signal".to_string()))?;
        } else {
            return Err(AeroSyncError::System("Transfer engine not started".to_string()));
        }
        Ok(())
    }

    pub async fn get_progress_monitor(&self) -> Arc<RwLock<ProgressMonitor>> {
        Arc::clone(&self.progress_monitor)
    }
}

async fn transfer_worker(
    mut task_receiver: mpsc::UnboundedReceiver<TransferTask>,
    mut _cancel_receiver: mpsc::UnboundedReceiver<Uuid>,
    progress_monitor: Arc<RwLock<ProgressMonitor>>,
    _config: TransferConfig,
) {
    tracing::info!("Transfer worker started");
    
    while let Some(task) = task_receiver.recv().await {
        tracing::info!("Processing transfer task: {} -> {}", 
                      task.source_path.display(), task.destination);
        
        // Update task status to in progress
        {
            let mut monitor = progress_monitor.write().await;
            monitor.update_progress(task.id, 0, 0.0);
        }
        
        // Simulate transfer for now (TODO: implement actual transfer)
        let result = simulate_transfer(task.clone(), progress_monitor.clone()).await;
        
        match result {
            Ok(_) => {
                let mut monitor = progress_monitor.write().await;
                monitor.complete_transfer(task.id);
                tracing::info!("Transfer completed: {}", task.source_path.display());
            }
            Err(e) => {
                let mut monitor = progress_monitor.write().await;
                monitor.fail_transfer(task.id, e.to_string());
                tracing::error!("Transfer failed: {} - {}", task.source_path.display(), e);
            }
        }
    }
    
    tracing::info!("Transfer worker stopped");
}

async fn simulate_transfer(
    task: TransferTask,
    progress_monitor: Arc<RwLock<ProgressMonitor>>,
) -> Result<()> {
    // Simulate file transfer with progress updates
    let total_bytes = task.file_size;
    let chunk_size = 64 * 1024; // 64KB chunks
    let total_chunks = (total_bytes + chunk_size - 1) / chunk_size;
    
    for chunk in 0..total_chunks {
        // Simulate some work
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        
        let bytes_transferred = std::cmp::min((chunk + 1) * chunk_size, total_bytes);
        let speed = chunk_size as f64 / 0.05; // 64KB per 50ms = speed
        
        // Update progress
        {
            let mut monitor = progress_monitor.write().await;
            monitor.update_progress(task.id, bytes_transferred, speed);
        }
        
        tracing::debug!("Transfer progress: {}/{} bytes", bytes_transferred, total_bytes);
    }
    
    Ok(())
}
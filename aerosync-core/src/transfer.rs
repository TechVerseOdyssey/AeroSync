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
    task_sender: mpsc::UnboundedSender<TransferTask>,
    cancel_sender: mpsc::UnboundedSender<Uuid>,
}

impl TransferEngine {
    pub fn new(config: TransferConfig) -> Self {
        let (task_sender, _task_receiver) = mpsc::unbounded_channel();
        let (cancel_sender, _cancel_receiver) = mpsc::unbounded_channel();
        
        Self {
            config,
            progress_monitor: Arc::new(RwLock::new(ProgressMonitor::new())),
            task_sender,
            cancel_sender,
        }
    }

    pub async fn start(&self) -> Result<()> {
        // TODO: Implement the main transfer loop
        // This will handle incoming tasks and manage concurrent transfers
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

        self.task_sender.send(task)
            .map_err(|_| AeroSyncError::System("Failed to queue transfer task".to_string()))?;

        Ok(())
    }

    pub async fn cancel_transfer(&self, task_id: Uuid) -> Result<()> {
        self.cancel_sender.send(task_id)
            .map_err(|_| AeroSyncError::System("Failed to send cancel signal".to_string()))?;
        Ok(())
    }

    pub async fn get_progress_monitor(&self) -> Arc<RwLock<ProgressMonitor>> {
        Arc::clone(&self.progress_monitor)
    }
}
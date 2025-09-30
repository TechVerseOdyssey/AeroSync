use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferProgress {
    pub task_id: Uuid,
    pub file_name: String,
    pub bytes_transferred: u64,
    pub total_bytes: u64,
    pub transfer_speed: f64, // bytes per second
    pub elapsed_time: Duration,
    pub estimated_remaining: Option<Duration>,
    pub status: TransferStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TransferStatus {
    Pending,
    InProgress,
    Paused,
    Completed,
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferStats {
    pub total_files: usize,
    pub completed_files: usize,
    pub failed_files: usize,
    pub total_bytes: u64,
    pub transferred_bytes: u64,
    pub overall_speed: f64,
    #[serde(skip, default = "Instant::now")]
    pub start_time: Instant,
}

pub struct ProgressMonitor {
    stats: TransferStats,
    active_transfers: Vec<TransferProgress>,
}

impl ProgressMonitor {
    pub fn new() -> Self {
        Self {
            stats: TransferStats {
                total_files: 0,
                completed_files: 0,
                failed_files: 0,
                total_bytes: 0,
                transferred_bytes: 0,
                overall_speed: 0.0,
                start_time: Instant::now(),
            },
            active_transfers: Vec::new(),
        }
    }

    pub fn add_transfer(&mut self, progress: TransferProgress) {
        self.stats.total_files += 1;
        self.stats.total_bytes += progress.total_bytes;
        self.active_transfers.push(progress);
    }

    pub fn update_progress(&mut self, task_id: Uuid, bytes_transferred: u64, speed: f64) {
        if let Some(transfer) = self.active_transfers.iter_mut().find(|t| t.task_id == task_id) {
            let old_bytes = transfer.bytes_transferred;
            transfer.bytes_transferred = bytes_transferred;
            transfer.transfer_speed = speed;
            transfer.elapsed_time = transfer.elapsed_time + Duration::from_millis(100);
            
            if bytes_transferred > 0 && transfer.total_bytes > 0 {
                let remaining_bytes = transfer.total_bytes - bytes_transferred;
                transfer.estimated_remaining = Some(Duration::from_secs_f64(remaining_bytes as f64 / speed));
            }

            self.stats.transferred_bytes += bytes_transferred - old_bytes;
            self.update_overall_speed();
        }
    }

    pub fn complete_transfer(&mut self, task_id: Uuid) {
        if let Some(pos) = self.active_transfers.iter().position(|t| t.task_id == task_id) {
            self.active_transfers.remove(pos);
            self.stats.completed_files += 1;
        }
    }

    pub fn fail_transfer(&mut self, task_id: Uuid, error: String) {
        if let Some(transfer) = self.active_transfers.iter_mut().find(|t| t.task_id == task_id) {
            transfer.status = TransferStatus::Failed(error);
            self.stats.failed_files += 1;
        }
    }

    pub fn get_stats(&self) -> &TransferStats {
        &self.stats
    }

    pub fn get_active_transfers(&self) -> &[TransferProgress] {
        &self.active_transfers
    }

    fn update_overall_speed(&mut self) {
        let elapsed = self.stats.start_time.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            self.stats.overall_speed = self.stats.transferred_bytes as f64 / elapsed;
        }
    }
}
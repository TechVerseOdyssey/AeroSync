use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    active_transfers: HashMap<Uuid, TransferProgress>,
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
            active_transfers: HashMap::new(),
        }
    }

    pub fn add_transfer(&mut self, progress: TransferProgress) {
        self.stats.total_files += 1;
        self.stats.total_bytes += progress.total_bytes;
        self.active_transfers.insert(progress.task_id, progress);
    }

    pub fn update_progress(&mut self, task_id: Uuid, bytes_transferred: u64, speed: f64) {
        if let Some(transfer) = self.active_transfers.get_mut(&task_id) {
            let old_bytes = transfer.bytes_transferred;
            transfer.bytes_transferred = bytes_transferred;
            transfer.transfer_speed = speed;
            transfer.status = TransferStatus::InProgress;
            transfer.elapsed_time += Duration::from_millis(100);

            if speed > 0.0 && bytes_transferred < transfer.total_bytes {
                let remaining_bytes = transfer.total_bytes - bytes_transferred;
                transfer.estimated_remaining =
                    Some(Duration::from_secs_f64(remaining_bytes as f64 / speed));
            }

            if bytes_transferred > old_bytes {
                self.stats.transferred_bytes += bytes_transferred - old_bytes;
            }
            self.update_overall_speed();
        }
    }

    pub fn complete_transfer(&mut self, task_id: Uuid) {
        if let Some(transfer) = self.active_transfers.get_mut(&task_id) {
            transfer.status = TransferStatus::Completed;
            self.stats.completed_files += 1;
        }
    }

    pub fn fail_transfer(&mut self, task_id: Uuid, error: String) {
        if let Some(transfer) = self.active_transfers.get_mut(&task_id) {
            transfer.status = TransferStatus::Failed(error);
            self.stats.failed_files += 1;
        }
    }

    pub fn cancel_transfer(&mut self, task_id: Uuid) {
        if let Some(transfer) = self.active_transfers.get_mut(&task_id) {
            // 只取消尚未完成的任务
            if !matches!(
                transfer.status,
                TransferStatus::Completed | TransferStatus::Failed(_) | TransferStatus::Cancelled
            ) {
                transfer.status = TransferStatus::Cancelled;
                self.stats.failed_files += 1;
            }
        }
    }

    /// 返回任务是否已被取消（供 worker 在启动前检查）
    pub fn is_cancelled(&self, task_id: &Uuid) -> bool {
        self.active_transfers
            .get(task_id)
            .map(|t| matches!(t.status, TransferStatus::Cancelled))
            .unwrap_or(false)
    }

    pub fn get_stats(&self) -> &TransferStats {
        &self.stats
    }

    pub fn get_active_transfers(&self) -> Vec<&TransferProgress> {
        self.active_transfers.values().collect()
    }

    pub fn get_transfer(&self, task_id: &Uuid) -> Option<&TransferProgress> {
        self.active_transfers.get(task_id)
    }

    fn update_overall_speed(&mut self) {
        let elapsed = self.stats.start_time.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            self.stats.overall_speed = self.stats.transferred_bytes as f64 / elapsed;
        }
    }
}

impl Default for ProgressMonitor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_progress(task_id: Uuid, file_name: &str, total_bytes: u64) -> TransferProgress {
        TransferProgress {
            task_id,
            file_name: file_name.to_string(),
            bytes_transferred: 0,
            total_bytes,
            transfer_speed: 0.0,
            elapsed_time: Duration::ZERO,
            estimated_remaining: None,
            status: TransferStatus::Pending,
        }
    }

    #[test]
    fn test_add_transfer_updates_stats() {
        let mut monitor = ProgressMonitor::new();
        let id = Uuid::new_v4();
        monitor.add_transfer(make_progress(id, "a.bin", 1024));

        let stats = monitor.get_stats();
        assert_eq!(stats.total_files, 1);
        assert_eq!(stats.total_bytes, 1024);
        assert_eq!(stats.completed_files, 0);
    }

    #[test]
    fn test_update_progress_changes_status_to_in_progress() {
        let mut monitor = ProgressMonitor::new();
        let id = Uuid::new_v4();
        monitor.add_transfer(make_progress(id, "b.bin", 2048));
        monitor.update_progress(id, 512, 1024.0);

        let t = monitor.get_transfer(&id).unwrap();
        assert_eq!(t.bytes_transferred, 512);
        assert!(matches!(t.status, TransferStatus::InProgress));
        assert!(t.transfer_speed > 0.0);
    }

    #[test]
    fn test_update_progress_computes_eta() {
        let mut monitor = ProgressMonitor::new();
        let id = Uuid::new_v4();
        // 1MB file, transferred 256KB at 256KB/s → ETA ~3s
        monitor.add_transfer(make_progress(id, "c.bin", 1024 * 1024));
        monitor.update_progress(id, 256 * 1024, 256.0 * 1024.0);

        let t = monitor.get_transfer(&id).unwrap();
        let eta = t.estimated_remaining.unwrap();
        // remaining = 768KB / 256KB/s = 3s, allow ±1s
        assert!(eta.as_secs() >= 2 && eta.as_secs() <= 4, "ETA was {:?}", eta);
    }

    #[test]
    fn test_update_progress_no_eta_when_complete() {
        let mut monitor = ProgressMonitor::new();
        let id = Uuid::new_v4();
        monitor.add_transfer(make_progress(id, "d.bin", 1024));
        // bytes_transferred == total_bytes: remaining is 0, ETA should be None or Duration::ZERO
        monitor.update_progress(id, 1024, 1024.0);

        let t = monitor.get_transfer(&id).unwrap();
        // When transferred >= total, no ETA is set
        assert!(
            t.estimated_remaining.is_none()
                || t.estimated_remaining.unwrap() == Duration::ZERO,
            "expected no ETA, got {:?}",
            t.estimated_remaining
        );
    }

    #[test]
    fn test_complete_transfer() {
        let mut monitor = ProgressMonitor::new();
        let id = Uuid::new_v4();
        monitor.add_transfer(make_progress(id, "e.bin", 512));
        monitor.complete_transfer(id);

        let t = monitor.get_transfer(&id).unwrap();
        assert!(matches!(t.status, TransferStatus::Completed));
        assert_eq!(monitor.get_stats().completed_files, 1);
    }

    #[test]
    fn test_fail_transfer() {
        let mut monitor = ProgressMonitor::new();
        let id = Uuid::new_v4();
        monitor.add_transfer(make_progress(id, "f.bin", 512));
        monitor.fail_transfer(id, "network timeout".to_string());

        let t = monitor.get_transfer(&id).unwrap();
        assert!(matches!(t.status, TransferStatus::Failed(ref e) if e == "network timeout"));
        assert_eq!(monitor.get_stats().failed_files, 1);
    }

    #[test]
    fn test_multiple_transfers_aggregate_bytes() {
        let mut monitor = ProgressMonitor::new();
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        monitor.add_transfer(make_progress(id1, "x.bin", 1000));
        monitor.add_transfer(make_progress(id2, "y.bin", 2000));

        monitor.update_progress(id1, 400, 100.0);
        monitor.update_progress(id2, 800, 200.0);

        let stats = monitor.get_stats();
        assert_eq!(stats.total_files, 2);
        assert_eq!(stats.total_bytes, 3000);
        assert_eq!(stats.transferred_bytes, 1200);
    }

    #[test]
    fn test_update_nonexistent_task_is_noop() {
        let mut monitor = ProgressMonitor::new();
        let fake_id = Uuid::new_v4();
        // Should not panic
        monitor.update_progress(fake_id, 100, 50.0);
        monitor.complete_transfer(fake_id);
        monitor.fail_transfer(fake_id, "err".to_string());
        assert_eq!(monitor.get_stats().total_files, 0);
    }

    #[test]
    fn test_get_active_transfers_returns_all() {
        let mut monitor = ProgressMonitor::new();
        for i in 0..5 {
            monitor.add_transfer(make_progress(Uuid::new_v4(), &format!("file{}.bin", i), 100));
        }
        assert_eq!(monitor.get_active_transfers().len(), 5);
    }

    #[test]
    fn test_incremental_byte_counting() {
        let mut monitor = ProgressMonitor::new();
        let id = Uuid::new_v4();
        monitor.add_transfer(make_progress(id, "g.bin", 1000));

        // 三次递增更新，每次 transferred_bytes 只增加增量
        monitor.update_progress(id, 100, 10.0);
        assert_eq!(monitor.get_stats().transferred_bytes, 100);

        monitor.update_progress(id, 300, 10.0);
        assert_eq!(monitor.get_stats().transferred_bytes, 300);

        monitor.update_progress(id, 600, 10.0);
        assert_eq!(monitor.get_stats().transferred_bytes, 600);
    }
}

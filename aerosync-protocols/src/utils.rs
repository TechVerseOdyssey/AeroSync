use crate::traits::TransferProgress;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// 根据已传输字节数和开始时间计算当前速度（bytes/s）
#[inline]
pub fn calc_speed(bytes_done: u64, start: &Instant) -> f64 {
    let elapsed = start.elapsed().as_secs_f64();
    if elapsed > 0.0 {
        bytes_done as f64 / elapsed
    } else {
        0.0
    }
}

/// 发送一次进度更新，忽略接收端已关闭的错误
#[inline]
pub fn send_progress(
    tx: &mpsc::UnboundedSender<TransferProgress>,
    bytes_transferred: u64,
    start: &Instant,
) {
    let _ = tx.send(TransferProgress {
        bytes_transferred,
        transfer_speed: calc_speed(bytes_transferred, start),
    });
}

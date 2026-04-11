/// Prometheus exposition format metrics exporter
///
/// Maintains atomic counters for all transfer events and exposes them
/// as a `GET /metrics` endpoint in the standard Prometheus text format.
///
/// Design: zero-lock counters using `AtomicU64` and `AtomicI64`.
/// The `render()` method accepts current disk space values (obtained just
/// before calling) to avoid storing mutable gauge state.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Central metrics store shared across all request handlers via `Arc<Metrics>`.
#[derive(Debug, Default)]
pub struct Metrics {
    /// Total number of successfully received files
    pub files_received_total: AtomicU64,
    /// Total bytes of successfully received data
    pub bytes_received_total: AtomicU64,
    /// Total number of failed uploads (hash mismatch, auth failure, etc.)
    pub upload_errors_total: AtomicU64,
    /// Total number of WebSocket connections accepted
    pub ws_connections_total: AtomicU64,
    /// Current active WebSocket connections (incremented on connect, decremented on disconnect)
    active_ws_connections: AtomicU64,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    // ── counter helpers ───────────────────────────────────────────────────────

    pub fn inc_files_received(&self) {
        self.files_received_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_bytes_received(&self, n: u64) {
        self.bytes_received_total.fetch_add(n, Ordering::Relaxed);
    }

    pub fn inc_upload_errors(&self) {
        self.upload_errors_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_ws_connections(&self) {
        self.ws_connections_total.fetch_add(1, Ordering::Relaxed);
        self.active_ws_connections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec_ws_connections(&self) {
        // saturating sub to avoid underflow in tests
        let _ = self.active_ws_connections.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |v| Some(v.saturating_sub(1)),
        );
    }

    pub fn active_ws(&self) -> u64 {
        self.active_ws_connections.load(Ordering::Relaxed)
    }

    // ── Prometheus text format renderer ──────────────────────────────────────

    /// Render all metrics in Prometheus exposition format (text/plain; version=0.0.4).
    ///
    /// `free_bytes` and `total_bytes` are injected at render time (from `get_disk_space`)
    /// so we don't need to store mutable gauge state here.
    pub fn render(&self, free_bytes: u64, total_bytes: u64) -> String {
        let files_received = self.files_received_total.load(Ordering::Relaxed);
        let bytes_received = self.bytes_received_total.load(Ordering::Relaxed);
        let upload_errors = self.upload_errors_total.load(Ordering::Relaxed);
        let ws_total = self.ws_connections_total.load(Ordering::Relaxed);
        let ws_active = self.active_ws();

        let mut out = String::with_capacity(512);

        // files_received_total
        out.push_str("# HELP aerosync_files_received_total Total number of successfully received files\n");
        out.push_str("# TYPE aerosync_files_received_total counter\n");
        out.push_str(&format!("aerosync_files_received_total {}\n", files_received));

        // bytes_received_total
        out.push_str("# HELP aerosync_bytes_received_total Total bytes received\n");
        out.push_str("# TYPE aerosync_bytes_received_total counter\n");
        out.push_str(&format!("aerosync_bytes_received_total {}\n", bytes_received));

        // upload_errors_total
        out.push_str("# HELP aerosync_upload_errors_total Total number of upload errors\n");
        out.push_str("# TYPE aerosync_upload_errors_total counter\n");
        out.push_str(&format!("aerosync_upload_errors_total {}\n", upload_errors));

        // ws_connections_total
        out.push_str("# HELP aerosync_ws_connections_total Total WebSocket connections accepted\n");
        out.push_str("# TYPE aerosync_ws_connections_total counter\n");
        out.push_str(&format!("aerosync_ws_connections_total {}\n", ws_total));

        // ws_active_connections (gauge)
        out.push_str("# HELP aerosync_ws_active_connections Current active WebSocket connections\n");
        out.push_str("# TYPE aerosync_ws_active_connections gauge\n");
        out.push_str(&format!("aerosync_ws_active_connections {}\n", ws_active));

        // disk_free_bytes (gauge)
        out.push_str("# HELP aerosync_disk_free_bytes Free disk space in bytes at the receive directory\n");
        out.push_str("# TYPE aerosync_disk_free_bytes gauge\n");
        out.push_str(&format!("aerosync_disk_free_bytes {}\n", free_bytes));

        // disk_total_bytes (gauge)
        out.push_str("# HELP aerosync_disk_total_bytes Total disk capacity in bytes at the receive directory\n");
        out.push_str("# TYPE aerosync_disk_total_bytes gauge\n");
        out.push_str(&format!("aerosync_disk_total_bytes {}\n", total_bytes));

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 1. render() produces valid Prometheus format ──────────────────────────
    #[test]
    fn test_render_format() {
        let m = Metrics::new();
        let output = m.render(1024, 4096);

        // Must contain required keys
        assert!(output.contains("aerosync_files_received_total 0"));
        assert!(output.contains("aerosync_bytes_received_total 0"));
        assert!(output.contains("aerosync_upload_errors_total 0"));
        assert!(output.contains("aerosync_disk_free_bytes 1024"));
        assert!(output.contains("aerosync_disk_total_bytes 4096"));

        // Every metric must have a HELP and TYPE comment
        let help_count = output.matches("# HELP").count();
        let type_count = output.matches("# TYPE").count();
        assert_eq!(help_count, type_count, "HELP and TYPE counts must match");
        assert!(help_count >= 5);
    }

    // ── 2. Counters increment correctly ──────────────────────────────────────
    #[test]
    fn test_counters() {
        let m = Metrics::new();
        m.inc_files_received();
        m.inc_files_received();
        m.add_bytes_received(512);
        m.inc_upload_errors();

        assert_eq!(m.files_received_total.load(Ordering::Relaxed), 2);
        assert_eq!(m.bytes_received_total.load(Ordering::Relaxed), 512);
        assert_eq!(m.upload_errors_total.load(Ordering::Relaxed), 1);
    }

    // ── 3. render() reflects updated counter values ───────────────────────────
    #[test]
    fn test_render_reflects_counters() {
        let m = Metrics::new();
        m.inc_files_received();
        m.add_bytes_received(1000);

        let output = m.render(0, 0);
        assert!(output.contains("aerosync_files_received_total 1"));
        assert!(output.contains("aerosync_bytes_received_total 1000"));
    }

    // ── 4. Disk space gauges correct ─────────────────────────────────────────
    #[test]
    fn test_render_disk_gauges() {
        let m = Metrics::new();
        let free = 10_000_000_000u64;
        let total = 100_000_000_000u64;
        let output = m.render(free, total);
        assert!(output.contains(&format!("aerosync_disk_free_bytes {}", free)));
        assert!(output.contains(&format!("aerosync_disk_total_bytes {}", total)));
    }

    // ── 5. WebSocket connection tracking ─────────────────────────────────────
    #[test]
    fn test_ws_connection_tracking() {
        let m = Metrics::new();
        m.inc_ws_connections();
        m.inc_ws_connections();
        assert_eq!(m.active_ws(), 2);
        assert_eq!(m.ws_connections_total.load(Ordering::Relaxed), 2);

        m.dec_ws_connections();
        assert_eq!(m.active_ws(), 1);
        // total should not decrease
        assert_eq!(m.ws_connections_total.load(Ordering::Relaxed), 2);
    }

    // ── 6. dec_ws_connections saturates at 0 ──────────────────────────────────
    #[test]
    fn test_ws_dec_saturates() {
        let m = Metrics::new();
        m.dec_ws_connections(); // should not panic or underflow
        assert_eq!(m.active_ws(), 0);
    }
}

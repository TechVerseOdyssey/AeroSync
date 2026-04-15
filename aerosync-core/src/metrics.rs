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
    /// Number of transfers currently in progress
    active_transfers: AtomicU64,
    /// Number of transfers waiting to be processed
    queue_depth: AtomicU64,
    // ── send-side counters ─────────────────────────────────────────────────
    /// Total number of files successfully sent
    pub files_sent_total: AtomicU64,
    /// Total bytes successfully sent
    pub bytes_sent_total: AtomicU64,
    /// Total number of send-side errors
    pub send_errors_total: AtomicU64,
    /// Files sent via HTTP protocol
    pub files_sent_http: AtomicU64,
    /// Files sent via QUIC protocol
    pub files_sent_quic: AtomicU64,
    // ── file size histogram (5 buckets) ───────────────────────────────────
    /// < 1 KB
    pub hist_lt_1kb: AtomicU64,
    /// 1 KB – 64 KB
    pub hist_1kb_64kb: AtomicU64,
    /// 64 KB – 1 MB
    pub hist_64kb_1mb: AtomicU64,
    /// 1 MB – 100 MB
    pub hist_1mb_100mb: AtomicU64,
    /// > 100 MB
    pub hist_gt_100mb: AtomicU64,
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

    pub fn inc_active_transfers(&self) {
        self.active_transfers.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec_active_transfers(&self) {
        let _ = self.active_transfers.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |v| Some(v.saturating_sub(1)),
        );
    }

    pub fn active_transfers(&self) -> u64 {
        self.active_transfers.load(Ordering::Relaxed)
    }

    pub fn set_queue_depth(&self, n: u64) {
        self.queue_depth.store(n, Ordering::Relaxed);
    }

    pub fn queue_depth(&self) -> u64 {
        self.queue_depth.load(Ordering::Relaxed)
    }

    // ── send-side helpers ─────────────────────────────────────────────────────

    pub fn inc_files_sent(&self, protocol: &str) {
        self.files_sent_total.fetch_add(1, Ordering::Relaxed);
        match protocol {
            "quic" => { self.files_sent_quic.fetch_add(1, Ordering::Relaxed); }
            _ => { self.files_sent_http.fetch_add(1, Ordering::Relaxed); }
        }
    }

    pub fn add_bytes_sent(&self, n: u64) {
        self.bytes_sent_total.fetch_add(n, Ordering::Relaxed);
    }

    pub fn inc_send_errors(&self) {
        self.send_errors_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a file size in the appropriate histogram bucket
    pub fn observe_file_size(&self, bytes: u64) {
        if bytes < 1_024 {
            self.hist_lt_1kb.fetch_add(1, Ordering::Relaxed);
        } else if bytes < 64 * 1_024 {
            self.hist_1kb_64kb.fetch_add(1, Ordering::Relaxed);
        } else if bytes < 1_024 * 1_024 {
            self.hist_64kb_1mb.fetch_add(1, Ordering::Relaxed);
        } else if bytes < 100 * 1_024 * 1_024 {
            self.hist_1mb_100mb.fetch_add(1, Ordering::Relaxed);
        } else {
            self.hist_gt_100mb.fetch_add(1, Ordering::Relaxed);
        }
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

        // active_transfers (gauge)
        let active_tr = self.active_transfers();
        out.push_str("# HELP aerosync_active_transfers Current number of in-progress file transfers\n");
        out.push_str("# TYPE aerosync_active_transfers gauge\n");
        out.push_str(&format!("aerosync_active_transfers {}\n", active_tr));

        // queue_depth (gauge)
        let qd = self.queue_depth();
        out.push_str("# HELP aerosync_queue_depth Number of transfers waiting in queue\n");
        out.push_str("# TYPE aerosync_queue_depth gauge\n");
        out.push_str(&format!("aerosync_queue_depth {}\n", qd));

        // files_sent_total (counter)
        let files_sent = self.files_sent_total.load(Ordering::Relaxed);
        out.push_str("# HELP aerosync_files_sent_total Total files successfully sent\n");
        out.push_str("# TYPE aerosync_files_sent_total counter\n");
        out.push_str(&format!("aerosync_files_sent_total {}\n", files_sent));

        // bytes_sent_total (counter)
        let bytes_sent = self.bytes_sent_total.load(Ordering::Relaxed);
        out.push_str("# HELP aerosync_bytes_sent_total Total bytes successfully sent\n");
        out.push_str("# TYPE aerosync_bytes_sent_total counter\n");
        out.push_str(&format!("aerosync_bytes_sent_total {}\n", bytes_sent));

        // send_errors_total (counter)
        let send_errors = self.send_errors_total.load(Ordering::Relaxed);
        out.push_str("# HELP aerosync_send_errors_total Total send-side errors\n");
        out.push_str("# TYPE aerosync_send_errors_total counter\n");
        out.push_str(&format!("aerosync_send_errors_total {}\n", send_errors));

        // per-protocol send counters (counter)
        let sent_http = self.files_sent_http.load(Ordering::Relaxed);
        let sent_quic = self.files_sent_quic.load(Ordering::Relaxed);
        out.push_str("# HELP aerosync_files_sent_by_protocol Total files sent per protocol\n");
        out.push_str("# TYPE aerosync_files_sent_by_protocol counter\n");
        out.push_str(&format!("aerosync_files_sent_by_protocol{{protocol=\"http\"}} {}\n", sent_http));
        out.push_str(&format!("aerosync_files_sent_by_protocol{{protocol=\"quic\"}} {}\n", sent_quic));

        // file size histogram (bucket counts)
        let h0 = self.hist_lt_1kb.load(Ordering::Relaxed);
        let h1 = self.hist_1kb_64kb.load(Ordering::Relaxed);
        let h2 = self.hist_64kb_1mb.load(Ordering::Relaxed);
        let h3 = self.hist_1mb_100mb.load(Ordering::Relaxed);
        let h4 = self.hist_gt_100mb.load(Ordering::Relaxed);
        out.push_str("# HELP aerosync_file_size_bucket File size distribution (count per bucket)\n");
        out.push_str("# TYPE aerosync_file_size_bucket gauge\n");
        out.push_str(&format!("aerosync_file_size_bucket{{le=\"1024\"}} {}\n", h0));
        out.push_str(&format!("aerosync_file_size_bucket{{le=\"65536\"}} {}\n", h1));
        out.push_str(&format!("aerosync_file_size_bucket{{le=\"1048576\"}} {}\n", h2));
        out.push_str(&format!("aerosync_file_size_bucket{{le=\"104857600\"}} {}\n", h3));
        out.push_str(&format!("aerosync_file_size_bucket{{le=\"+Inf\"}} {}\n", h4));

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

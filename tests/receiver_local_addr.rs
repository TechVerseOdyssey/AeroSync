//! Integration tests for `FileReceiver::local_http_addr` /
//! `FileReceiver::local_quic_addr` (P0.3).
//!
//! These getters were added so callers who bind to port `0` (let the
//! OS pick) can discover the actual port after `start()` completes.
//! Without them the README quickstart pattern — sender connects to
//! `r.address` after the receiver bound — was unrunnable for the
//! Python SDK and forced Rust callers to grep tracing output.
//!
//! The test bodies intentionally exercise the *engine* surface
//! directly (not via the Python binding): the binding-level coverage
//! lives in `aerosync-py/tests/test_receiver_lifecycle.py`.

use aerosync::core::server::{FileReceiver, ServerConfig};
use tempfile::TempDir;

/// HTTP-only path: bind to `127.0.0.1:0`, prove `local_http_addr`
/// returns the OS-assigned port (non-zero) and that `local_quic_addr`
/// stays `None` because QUIC is disabled.
#[tokio::test]
async fn local_http_addr_returns_os_assigned_port() {
    let dir = TempDir::new().unwrap();
    let cfg = ServerConfig {
        http_port: 0,
        bind_address: "127.0.0.1".to_string(),
        receive_directory: dir.path().to_path_buf(),
        enable_http: true,
        enable_quic: false,
        ..ServerConfig::default()
    };

    let mut receiver = FileReceiver::new(cfg);

    // Pre-start contract: both getters must report `None` so the
    // Python `Receiver.address` getter can fall back to the
    // user-supplied `listen=` string before `__aenter__` resolves.
    assert!(receiver.local_http_addr().is_none());
    assert!(receiver.local_quic_addr().is_none());

    receiver.start().await.expect("server start failed");

    let bound = receiver
        .local_http_addr()
        .expect("local_http_addr() must be Some after a successful start");
    assert_eq!(
        bound.ip().to_string(),
        "127.0.0.1",
        "bound to the requested host"
    );
    assert_ne!(
        bound.port(),
        0,
        "OS must have assigned a real port (got 0, which means we surfaced the request not the bind)"
    );
    assert!(
        receiver.local_quic_addr().is_none(),
        "QUIC was disabled — local_quic_addr() must stay None"
    );

    receiver.stop().await.expect("server stop failed");

    // Post-stop contract: address is cleared so a subsequent
    // start (or a stale read) sees `None`, not a freed port.
    assert!(receiver.local_http_addr().is_none());
}

/// QUIC-enabled path: same shape, but assert both addresses are
/// surfaced and that they sit on independent OS-assigned ports.
#[tokio::test]
async fn local_quic_addr_returns_os_assigned_port() {
    let dir = TempDir::new().unwrap();
    let cfg = ServerConfig {
        http_port: 0,
        quic_port: 0,
        bind_address: "127.0.0.1".to_string(),
        receive_directory: dir.path().to_path_buf(),
        enable_http: true,
        enable_quic: true,
        ..ServerConfig::default()
    };

    let mut receiver = FileReceiver::new(cfg);
    receiver.start().await.expect("server start failed");

    let http_bound = receiver.local_http_addr().expect("HTTP must be bound");
    let quic_bound = receiver.local_quic_addr().expect("QUIC must be bound");

    assert_ne!(http_bound.port(), 0, "HTTP got an OS-assigned port");
    assert_ne!(quic_bound.port(), 0, "QUIC got an OS-assigned port");
    assert_ne!(
        http_bound.port(),
        quic_bound.port(),
        "HTTP (TCP) and QUIC (UDP) bind to independent OS-assigned ports"
    );

    receiver.stop().await.expect("server stop failed");
    assert!(receiver.local_http_addr().is_none());
    assert!(receiver.local_quic_addr().is_none());
}

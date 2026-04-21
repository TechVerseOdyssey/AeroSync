//! HTTP metadata propagation — end-to-end integration tests
//! (RFC-003 §8.4 / w0.2.1 batch B "P0.1").
//!
//! These tests exercise the full HTTP path of the metadata wire
//! envelope: a real [`TransferEngine`] + [`AutoAdapter`] sender
//! drives a real [`FileReceiver`] HTTP listener, with the sealed
//! [`Metadata`] envelope crossing the wire as the
//! `X-Aerosync-Metadata` header (`base64(protobuf(Metadata))`).
//!
//! ## Scope
//!
//! 1. **Happy path** — a fully-populated envelope round-trips through
//!    `/upload` and lands on `ReceivedFile.metadata` byte-equal to
//!    what the engine sealed (well-known fields + `user_metadata` +
//!    system fields stamped by the engine).
//! 2. **Absent header** — a sender that does not call
//!    `send_with_metadata` produces a `ReceivedFile` with
//!    `metadata = None` and the upload still succeeds (legacy
//!    senders / smoke tests must keep working).
//! 3. **Malformed base64** — a request with a non-base64
//!    `X-Aerosync-Metadata` header is rejected with `400` and the
//!    canonical JSON error envelope; no `ReceivedFile` is created.
//! 4. **Malformed protobuf** — a request whose header decodes to
//!    valid base64 but invalid protobuf is rejected with `400` for
//!    the same reason; no `ReceivedFile` is created.
//!
//! The QUIC path is intentionally NOT covered here; the matching
//! QUIC end-to-end metadata propagation is exercised by its own
//! integration test in batch C (the historical
//! `w3c-quic-receipt-wiring` TODO was closed in v0.2.1 by commit
//! `41c7235`).

use aerosync::core::metadata::MetadataBuilder;
use aerosync::core::server::{FileReceiver, ServerConfig};
use aerosync::core::transfer::{TransferConfig, TransferEngine, TransferTask};
use aerosync::protocols::http::HttpConfig;
#[cfg(feature = "quic")]
use aerosync::protocols::quic::QuicConfig;
use aerosync::protocols::AutoAdapter;
use aerosync_proto::{Lifecycle, Metadata};
use base64::engine::general_purpose::STANDARD as B64_STD;
use base64::Engine as _;
use prost::Message as _;
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;

/// Boot an HTTP-only `FileReceiver` on an OS-assigned port and
/// return `(receiver, http_addr, save_dir)`. We hold onto the
/// `TempDir` so the receiver's `receive_directory` outlives the
/// test body.
async fn boot_http_receiver() -> (FileReceiver, std::net::SocketAddr, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let cfg = ServerConfig {
        http_port: 0,
        bind_address: "127.0.0.1".to_string(),
        receive_directory: dir.path().to_path_buf(),
        enable_http: true,
        enable_quic: false,
        enable_ws: false,
        ..ServerConfig::default()
    };
    let mut receiver = FileReceiver::new(cfg);
    receiver.start().await.expect("FileReceiver start");
    let addr = receiver
        .local_http_addr()
        .expect("local_http_addr after start");
    (receiver, addr, dir)
}

/// Build an `AutoAdapter` configured for HTTP-only happy-path
/// uploads (short timeout, no retries — these tests do not
/// exercise the retry surface).
fn build_adapter() -> Arc<AutoAdapter> {
    let http_config = HttpConfig {
        timeout_seconds: 10,
        max_retries: 0,
        ..HttpConfig::default()
    };
    #[cfg(feature = "quic")]
    {
        Arc::new(AutoAdapter::new(http_config, QuicConfig::default()))
    }
    #[cfg(not(feature = "quic"))]
    {
        Arc::new(AutoAdapter::new(http_config))
    }
}

/// Poll `get_received_files()` until it returns a record (or the
/// deadline expires). Returns the first record. The HTTP receive
/// loop runs on its own task; we cannot drive it synchronously.
async fn wait_for_received_file(
    receiver: &FileReceiver,
    deadline: Duration,
) -> aerosync::core::server::ReceivedFile {
    let stop_at = tokio::time::Instant::now() + deadline;
    loop {
        let files = receiver.get_received_files().await;
        if let Some(f) = files.into_iter().next() {
            return f;
        }
        if tokio::time::Instant::now() >= stop_at {
            panic!("no ReceivedFile arrived within {deadline:?}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// ── 1. Happy path ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_upload_propagates_metadata_envelope_end_to_end() {
    let (receiver, addr, dir) = boot_http_receiver().await;

    // Sender side: real TransferEngine + AutoAdapter, real wire.
    let engine = TransferEngine::new(TransferConfig::default()).with_node_id("propagate-sender");
    engine.start(build_adapter()).await.unwrap();

    let payload = dir.path().join("payload.txt");
    let body = b"hello metadata over http\n";
    tokio::fs::write(&payload, body).await.unwrap();

    let metadata = MetadataBuilder::new()
        .trace_id("trace-http-1")
        .conversation_id("conv-http-1")
        .correlation_id("corr-http-1")
        .lifecycle(Lifecycle::Durable)
        .user("tenant", "acme")
        .user("agent_id", "indexer")
        .build()
        .expect("envelope must build");

    let task = TransferTask::new_upload(
        payload.clone(),
        format!("http://{addr}/upload"),
        body.len() as u64,
    );
    let _receipt = engine
        .send_with_metadata(task, metadata.clone())
        .await
        .expect("send_with_metadata must succeed for a fresh receiver");

    let received = wait_for_received_file(&receiver, Duration::from_secs(10)).await;
    let got = received
        .metadata
        .as_ref()
        .expect("ReceivedFile.metadata must be populated from the X-Aerosync-Metadata header");

    // Well-known and user fields round-trip byte-equal to what the
    // sender built.
    assert_eq!(got.trace_id.as_deref(), Some("trace-http-1"));
    assert_eq!(got.conversation_id.as_deref(), Some("conv-http-1"));
    assert_eq!(got.correlation_id.as_deref(), Some("corr-http-1"));
    assert_eq!(got.lifecycle, Some(Lifecycle::Durable as i32));
    assert_eq!(
        got.user_metadata.get("tenant").map(String::as_str),
        Some("acme")
    );
    assert_eq!(
        got.user_metadata.get("agent_id").map(String::as_str),
        Some("indexer")
    );

    // System fields stamped by the engine survived the wire.
    assert_eq!(got.from_node, "propagate-sender");
    assert_eq!(got.size_bytes, body.len() as u64);
    assert_eq!(got.file_name, "payload.txt");
    assert!(got.created_at.is_some());

    drop(receiver);
}

// ── 2. Absent header ────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_upload_without_metadata_succeeds_with_metadata_none() {
    let (receiver, addr, dir) = boot_http_receiver().await;

    let engine = TransferEngine::new(TransferConfig::default());
    engine.start(build_adapter()).await.unwrap();

    let payload = dir.path().join("plain.bin");
    let body = b"plain payload";
    tokio::fs::write(&payload, body).await.unwrap();

    // `add_transfer` (no metadata) — exercises the legacy code path.
    let task = TransferTask::new_upload(
        payload.clone(),
        format!("http://{addr}/upload"),
        body.len() as u64,
    );
    engine.add_transfer(task).await.unwrap();

    let received = wait_for_received_file(&receiver, Duration::from_secs(10)).await;
    assert!(
        received.metadata.is_none(),
        "transfers without an envelope must leave ReceivedFile.metadata = None"
    );

    drop(receiver);
}

// ── 3. Malformed base64 → 400 ───────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_upload_rejects_malformed_base64_metadata_header_with_400() {
    let (receiver, addr, _dir) = boot_http_receiver().await;

    // Hand-rolled multipart body so we can control exactly which
    // headers go on the request. AutoAdapter would always send a
    // valid base64 header.
    let boundary = "boundarytest123";
    let body = format!(
        "--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"x.bin\"\r\nContent-Type: application/octet-stream\r\n\r\nhi\r\n--{b}--\r\n",
        b = boundary
    );

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/upload"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        // `!!!` is not a valid base64 alphabet sequence under
        // STANDARD, STANDARD_NO_PAD, URL_SAFE, or URL_SAFE_NO_PAD,
        // so all four decode strategies in
        // `extract_metadata_header` must fail before we ever try
        // the protobuf decode.
        .header("X-Aerosync-Metadata", "!!!not-base64!!!")
        .body(body)
        .send()
        .await
        .expect("request should reach the receiver");

    assert_eq!(
        resp.status().as_u16(),
        400,
        "malformed base64 in X-Aerosync-Metadata must 400, got {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.expect("400 body must be JSON");
    let err = body
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        err.starts_with("invalid metadata header:"),
        "400 body must carry the canonical 'invalid metadata header' prefix, got: {err}"
    );

    // Sanity: the receiver must NOT have materialised a file from
    // the rejected request.
    let files = receiver.get_received_files().await;
    assert!(
        files.is_empty(),
        "rejected upload must not produce a ReceivedFile, got {files:?}"
    );

    drop(receiver);
}

// ── 4. Malformed protobuf → 400 ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_upload_rejects_malformed_protobuf_metadata_header_with_400() {
    let (receiver, addr, _dir) = boot_http_receiver().await;

    // A buffer that base64-decodes cleanly but is NOT a valid
    // wire-format `Metadata` message. `0xff` repeated yields a
    // tag value that prost cannot route to any field number.
    let bogus = vec![0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
    let header = B64_STD.encode(&bogus);
    // Sanity guard: round-tripping through STANDARD must succeed
    // so we know the failure below comes from prost, not base64.
    assert!(B64_STD.decode(&header).is_ok());
    // Sanity guard: prost itself must reject these bytes — if a
    // future libprost relaxes this we want the test to fail loudly
    // rather than silently turn into a happy-path test.
    assert!(Metadata::decode(bogus.as_slice()).is_err());

    let boundary = "boundaryprost456";
    let body = format!(
        "--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"y.bin\"\r\nContent-Type: application/octet-stream\r\n\r\nhi\r\n--{b}--\r\n",
        b = boundary
    );

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/upload"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("X-Aerosync-Metadata", header)
        .body(body)
        .send()
        .await
        .expect("request should reach the receiver");

    assert_eq!(resp.status().as_u16(), 400);
    let files = receiver.get_received_files().await;
    assert!(files.is_empty());

    drop(receiver);
}

//! HTTP wire-level receipt acknowledgement — end-to-end integration
//! tests (RFC-002 §6.4 application-level ACK on the HTTP transport,
//! v0.2.1 batch D.5).
//!
//! The receiver-side `/upload` JSON response now embeds a structured
//! `receipt_ack` object whenever the sender stamped a parseable
//! `Metadata.id`; the sender's [`HttpTransfer`] parses that envelope
//! off the response body and forwards it through the
//! [`TransferEngine::http_receipt_inbox`] forwarder task, which in
//! turn drives the matching `Receipt<Sender>` past `Processing` to a
//! terminal state. This is the HTTP analogue of the QUIC bidi
//! receipt control stream proven in `tests/receipts_e2e.rs`.
//!
//! ## Scope
//!
//! 1. **Happy path** — `TransferEngine + AutoAdapter::with_engine_receipt_inbox`
//!    on the sender, real `FileReceiver` HTTP listener: send a file
//!    with a sealed [`Metadata`] envelope, observe the sender-side
//!    `Receipt<Sender>` reach `Completed(Acked)`.
//! 2. **Backward compatibility — no inbox wired** — same engine, same
//!    receiver, same sealed envelope, but the adapter is constructed
//!    *without* `with_engine_receipt_inbox`. The receiver still
//!    echoes `receipt_ack` (additive on the wire) but the sender
//!    has nowhere to forward it; the receipt must park at
//!    `Processing` exactly as in v0.2.0.
//! 3. **Backward compatibility — no metadata sent** — a hand-rolled
//!    multipart upload with no `X-Aerosync-Metadata` header gets a
//!    200 OK whose JSON body does **not** contain a `receipt_ack`
//!    key. Proves the server-side emission is gated on a
//!    parseable `Metadata.id`, so legacy v0.2.0 senders see a
//!    byte-identical response shape.

use std::sync::Arc;
use std::time::Duration;

use aerosync::core::metadata::MetadataBuilder;
use aerosync::core::receipt::{CompletedTerminal, State};
use aerosync::core::server::{FileReceiver, ServerConfig};
use aerosync::core::transfer::{TransferConfig, TransferEngine, TransferTask};
use aerosync::protocols::http::HttpConfig;
#[cfg(feature = "quic")]
use aerosync::protocols::quic::QuicConfig;
use aerosync::protocols::AutoAdapter;
use aerosync_proto::Lifecycle;
use tempfile::tempdir;

/// Boot an HTTP-only `FileReceiver` on an OS-assigned port. Mirrors
/// the helper in `tests/metadata_http_propagation.rs`; we keep our
/// own copy so this test file is self-contained and independently
/// runnable via `cargo test --test http_receipt_ack_e2e`.
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

/// HTTP-only `AutoAdapter` builder. `wire_inbox` controls whether
/// the engine's [`TransferEngine::http_receipt_inbox`] is stitched
/// into the adapter — the toggle that distinguishes the happy-path
/// test (wired) from the backward-compat test (unwired).
fn build_adapter(engine: &TransferEngine, wire_inbox: bool) -> Arc<AutoAdapter> {
    let http_config = HttpConfig {
        timeout_seconds: 10,
        max_retries: 0,
        ..HttpConfig::default()
    };
    #[cfg(feature = "quic")]
    let mut adapter = AutoAdapter::new(http_config, QuicConfig::default());
    #[cfg(not(feature = "quic"))]
    let mut adapter = AutoAdapter::new(http_config);
    if wire_inbox {
        adapter = adapter.with_engine_receipt_inbox(engine.http_receipt_inbox());
    }
    Arc::new(adapter)
}

/// Block up to `timeout` for the receipt's state to satisfy `pred`.
/// Returns the matching state, or panics with the last-observed
/// state on timeout. We poll the `watch` channel rather than spinning
/// on `state()` so completions wake us promptly.
async fn wait_for_state<F>(
    receipt: &Arc<aerosync::core::receipt::Receipt<aerosync::core::receipt::Sender>>,
    timeout: Duration,
    pred: F,
    label: &str,
) -> State
where
    F: Fn(&State) -> bool,
{
    let start = std::time::Instant::now();
    let mut rx = receipt.watch();
    loop {
        let snapshot = rx.borrow_and_update().clone();
        if pred(&snapshot) {
            return snapshot;
        }
        if start.elapsed() > timeout {
            panic!("wait_for({label}) timed out after {timeout:?}; last={snapshot:?}");
        }
        let _ = tokio::time::timeout(Duration::from_millis(50), rx.changed()).await;
    }
}

// ── 1. Happy path: receipt_ack drives sender to Completed(Acked) ─────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_upload_with_inbox_drives_sender_receipt_to_acked() {
    let (receiver, addr, dir) = boot_http_receiver().await;

    let engine = TransferEngine::new(TransferConfig::default()).with_node_id("ack-sender");
    engine
        .start(build_adapter(&engine, true))
        .await
        .expect("engine.start");

    let payload = dir.path().join("ack-payload.bin");
    let body = b"the receiver shall echo our receipt id\n";
    tokio::fs::write(&payload, body).await.unwrap();

    let metadata = MetadataBuilder::new()
        .trace_id("trace-ack-1")
        .lifecycle(Lifecycle::Durable)
        .user("agent_id", "ack-test")
        .build()
        .expect("metadata builds");

    let task = TransferTask::new_upload(
        payload.clone(),
        format!("http://{addr}/upload"),
        body.len() as u64,
    );
    let receipt = engine
        .send_with_metadata(task, metadata)
        .await
        .expect("send_with_metadata");

    // The forwarder task wakes on the unbounded inbox channel — no
    // arbitrary `sleep` here; we drive purely off the receipt's
    // watch channel. 10s upper bound matches the surrounding e2e
    // suite's posture for "real wire, real listener" tests.
    let state = wait_for_state(
        &receipt,
        Duration::from_secs(10),
        |s| matches!(s, State::Completed(CompletedTerminal::Acked)),
        "sender Acked",
    )
    .await;
    assert_eq!(state, State::Completed(CompletedTerminal::Acked));

    drop(receiver);
}

// ── 2. Backward compat: adapter without inbox parks at Processing ────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_upload_without_inbox_parks_receipt_at_processing() {
    let (receiver, addr, dir) = boot_http_receiver().await;

    let engine = TransferEngine::new(TransferConfig::default()).with_node_id("noinbox-sender");
    engine
        .start(build_adapter(&engine, false))
        .await
        .expect("engine.start");

    let payload = dir.path().join("noinbox-payload.bin");
    let body = b"sender ignores the wire-level ack\n";
    tokio::fs::write(&payload, body).await.unwrap();

    let metadata = MetadataBuilder::new()
        .trace_id("trace-noinbox-1")
        .build()
        .expect("metadata builds");

    let task = TransferTask::new_upload(
        payload.clone(),
        format!("http://{addr}/upload"),
        body.len() as u64,
    );
    let receipt = engine
        .send_with_metadata(task, metadata)
        .await
        .expect("send_with_metadata");

    // The engine bridge walks the receipt to `Processing` once the
    // worker reports `TransferStatus::Completed`; without an inbox
    // wired the receipt must STAY there (no terminal). Wait for
    // Processing, then dwell long enough that any spurious Ack
    // would surface.
    wait_for_state(
        &receipt,
        Duration::from_secs(10),
        |s| matches!(s, State::Processing),
        "sender Processing",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let state = receipt.state();
    assert!(
        matches!(state, State::Processing),
        "receipt must remain at Processing without an inbox; got {state:?}"
    );

    drop(receiver);
}

// ── 3. Backward compat: 200 body without metadata omits receipt_ack ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_upload_without_metadata_omits_receipt_ack_in_response_body() {
    let (receiver, addr, _dir) = boot_http_receiver().await;

    // Hand-rolled multipart so we can guarantee NO `X-Aerosync-Metadata`
    // header on the wire — `AutoAdapter` always emits it whenever
    // metadata was sealed by `send_with_metadata`.
    let boundary = "boundaryreceiptack789";
    let body = format!(
        "--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"plain.bin\"\r\nContent-Type: application/octet-stream\r\n\r\nhi\r\n--{b}--\r\n",
        b = boundary
    );

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/upload"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .expect("request reaches receiver");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "no-metadata upload must still 200, got {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.expect("response is JSON");
    assert!(
        body.get("receipt_ack").is_none(),
        "200 body without sender-supplied metadata must not contain a `receipt_ack` key; got {body}"
    );
    assert_eq!(body.get("success").and_then(|v| v.as_bool()), Some(true));

    drop(receiver);
}

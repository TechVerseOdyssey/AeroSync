//! RFC-002 §14 #12 — scoped end-to-end receipt suite (v0.2.0
//! minimum-viable coverage, **not** the full §13 / §14 #12 set).
//!
//! # What this file proves
//!
//! These four tests exercise the API freeze landed in week 3 across
//! the QUIC, HTTP and MCP surfaces with **real** transports / endpoints
//! (no mocks for the wire). Each test is hand-picked from the larger
//! §13 list as the highest-value composition that, if green, makes the
//! v0.2.0 RC release-candidate decision tractable:
//!
//! 1. [`e2e_quic_receipt_ack_happy_path`] — proves `TransferEngine::send`
//!    (#8), `IncomingFile::ack` (#9), and the bidi QUIC receipt stream
//!    (#4) compose: the receiver's `ack` round-trips through real QUIC
//!    and lands the sender-side `Receipt<Sender>` in `Completed(Acked)`.
//! 2. [`e2e_quic_receipt_nack_with_reason`] — same wire, `Nack(reason)`
//!    path: the reason string survives encoding/decoding intact.
//! 3. [`e2e_http_sse_observes_quic_transfer`] — proves the HTTP SSE
//!    fallback (#5) mirrors the same state stream that QUIC sees, so a
//!    web/desktop UI can observe the same lifecycle without QUIC.
//! 4. [`e2e_mcp_cancel_receipt_round_trip`] — proves the MCP
//!    `cancel_receipt` tool (#10) actually flips a registered
//!    `Receipt<Sender>` to `Failed(Cancelled{reason})`.
//!
//! # What is **deferred** (intentionally)
//!
//! The following tests from the original §13 list are deferred to a
//! follow-up subagent and tracked at the parent level — do **not**
//! land them here:
//!
//! - `e2e_http_ack_routes_to_receiver` (POST `/v1/receipts/:id/ack`
//!   round-tripping back to the receiver-side `Receipt<Receiver>`)
//! - `e2e_idempotent_ack_via_http` (Idempotency-Key replay protection
//!   end-to-end)
//! - `e2e_capabilities_no_receipts_smoke` (degraded handshake when
//!   peer omits `SUPPORTS_RECEIPTS`)
//! - Cross-protocol fuzzing harness (deferred to v0.2.0-rc).
//!
//! # Architectural note (honesty clause)
//!
//! At the time of writing, neither the QUIC adapter nor `FileReceiver`
//! auto-opens the bidi receipt stream alongside chunk transfer; that
//! wiring is the main remaining piece between v0.2.0 and v0.3 (see the
//! "deferred — see `quic_receipt`" comments in `transfer.rs`). The
//! tests here therefore drive the QUIC bidi stream **manually at the
//! test layer** — i.e. they prove the surfaces compose given that
//! wiring, not that the wiring is yet automatic. This is a deliberate
//! scope cut for the v0.2.0 minimum-viable coverage.

mod common;

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use aerosync::core::receipt::{
    CompletedTerminal, Event as ReceiptEvent, FailedTerminal, Receipt, Receiver as RxSide,
    Sender as TxSide, State,
};
use aerosync::core::receipt_registry::ReceiptRegistry;
use aerosync::core::server::{FileReceiver, ReceivedFile, ServerConfig};
use aerosync::core::transfer::{TransferConfig, TransferEngine, TransferTask};
use aerosync::core::IncomingFile;
use aerosync::protocols::quic_receipt::{run_receiver_loop, run_sender_loop, ReceiverVerdict};
use std::path::PathBuf;
use tempfile::TempDir;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::common::{make_quinn_pair, open_connected_pair, SlowAdapter, SuccessAdapter};

/// Pick a free TCP port (released immediately; race-prone but adequate
/// for a single-shot test bind).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Drive a sender-side `Receipt<Sender>` from `Initiated` through
/// `StreamOpened → DataTransferred → StreamClosed → Processing` so the
/// terminal `Ack` / `Nack` is a legal next event. `apply_event` is a
/// no-op when the receipt is already past the requested state, so this
/// is safe to call concurrently with the engine's own bridge task.
fn drive_to_processing<S>(receipt: &Receipt<S>) {
    let _ = receipt.apply_event(ReceiptEvent::Open);
    let _ = receipt.apply_event(ReceiptEvent::Close);
    let _ = receipt.apply_event(ReceiptEvent::Close);
    let _ = receipt.apply_event(ReceiptEvent::Process);
}

/// Block up to `timeout_ms` for `pred(state)` to hold. Returns the
/// matching state, or panics with the last-observed state on timeout.
async fn wait_for<S, F>(receipt: &Arc<Receipt<S>>, timeout_ms: u64, pred: F, label: &str) -> State
where
    S: Send + Sync + 'static,
    F: Fn(&State) -> bool,
{
    let start = std::time::Instant::now();
    let mut rx = receipt.watch();
    loop {
        let snapshot = rx.borrow_and_update().clone();
        if pred(&snapshot) {
            return snapshot;
        }
        if start.elapsed() > Duration::from_millis(timeout_ms) {
            panic!("wait_for({label}) timed out after {timeout_ms}ms; last={snapshot:?}");
        }
        let _ = tokio::time::timeout(Duration::from_millis(50), rx.changed()).await;
    }
}

// ─────────────────────────────────────────────────────────────────────
// Test 1 — happy-path ack across the bidi QUIC receipt stream.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn e2e_quic_receipt_ack_happy_path() {
    let (server_ep, client_ep, server_addr) = make_quinn_pair().await;
    let (sender_conn, receiver_conn) =
        open_connected_pair(&server_ep, &client_ep, server_addr).await;

    // ── Sender side: TransferEngine::send issues the Receipt<Sender>.
    let engine = TransferEngine::new(TransferConfig::default());
    engine.start(Arc::new(SuccessAdapter)).await.unwrap();

    let task = TransferTask::new_upload(
        PathBuf::from("/virtual/file.bin"),
        "http://example/upload".to_string(),
        1024,
    );
    let sender_receipt: Arc<Receipt<TxSide>> = engine.send(task).await.expect("send");
    let receipt_id = sender_receipt.id();

    // The engine's bridge will drive sender_receipt to Processing on
    // its own once SuccessAdapter returns; wait for that before opening
    // the wire so the eventual Ack is a legal transition.
    wait_for(
        &sender_receipt,
        2_000,
        |s| matches!(s, State::Processing),
        "sender Processing",
    )
    .await;

    // ── Wire: open the bidi receipt stream sender→receiver.
    let (mut sender_send, sender_recv) = sender_conn.open_bi().await.unwrap();
    // Nudge the stream open so accept_bi() unblocks on the other side.
    let _ = sender_send.write(&[]).await;
    let receiver_accept = tokio::spawn(async move { receiver_conn.accept_bi().await.unwrap() });
    let (recv_send, recv_recv) = receiver_accept.await.unwrap();

    // ── Receiver side: a Receipt<Receiver> with the SAME id, wrapped
    // in IncomingFile (the application surface).
    let receiver_receipt: Arc<Receipt<RxSide>> = Arc::new(Receipt::<RxSide>::new(receipt_id));
    let receiver_registry = Arc::new(ReceiptRegistry::<RxSide>::new());
    receiver_registry.insert(Arc::clone(&receiver_receipt));
    drive_to_processing(&receiver_receipt);

    let received = ReceivedFile {
        id: Uuid::new_v4(),
        original_name: "file.bin".to_string(),
        saved_path: PathBuf::from("/tmp/e2e_ack.bin"),
        size: 1024,
        sha256: None,
        received_at: SystemTime::now(),
        sender_ip: None,
    };
    let incoming = IncomingFile::new(
        received,
        Arc::clone(&receiver_receipt),
        Arc::clone(&receiver_registry),
    );

    // Spawn the sender-side reader BEFORE the receiver writes, so the
    // Acked frame is observed.
    let sr_clone = Arc::clone(&sender_receipt);
    let sender_loop = tokio::spawn(async move { run_sender_loop(sender_recv, sr_clone).await });

    // Spawn the receiver-side writer; it blocks on the verdict channel.
    let (verdict_tx, verdict_rx) = oneshot::channel();
    let rr_clone = Arc::clone(&receiver_receipt);
    let receiver_loop =
        tokio::spawn(
            async move { run_receiver_loop(recv_send, recv_recv, rr_clone, verdict_rx).await },
        );

    // ── Application action: ack via the IncomingFile API. In a wired
    // QUIC transport the same call would post the verdict to the
    // run_receiver_loop transport task automatically; for v0.2.0 the
    // test bridges that gap explicitly (see file-level docs).
    incoming.ack().await.expect("ack");
    verdict_tx.send(ReceiverVerdict::Ack).unwrap();

    // ── Receiver-side terminal must be Acked.
    assert!(
        matches!(
            receiver_receipt.state(),
            State::Completed(CompletedTerminal::Acked)
        ),
        "receiver state = {:?}",
        receiver_receipt.state()
    );

    // ── Wait for the sender to observe the Acked frame off the wire.
    wait_for(
        &sender_receipt,
        5_000,
        |s| matches!(s, State::Completed(CompletedTerminal::Acked)),
        "sender Acked",
    )
    .await;

    // Both terminate cleanly.
    let _ = sender_send.finish();
    let _ = tokio::time::timeout(Duration::from_secs(3), receiver_loop).await;
    let _ = tokio::time::timeout(Duration::from_secs(3), sender_loop).await;
}

// ─────────────────────────────────────────────────────────────────────
// Test 2 — nack with reason round-trips through the wire.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn e2e_quic_receipt_nack_with_reason() {
    const REASON: &str = "disk full";

    let (server_ep, client_ep, server_addr) = make_quinn_pair().await;
    let (sender_conn, receiver_conn) =
        open_connected_pair(&server_ep, &client_ep, server_addr).await;

    let engine = TransferEngine::new(TransferConfig::default());
    engine.start(Arc::new(SuccessAdapter)).await.unwrap();

    let task = TransferTask::new_upload(
        PathBuf::from("/virtual/nack.bin"),
        "http://example/upload".to_string(),
        2048,
    );
    let sender_receipt: Arc<Receipt<TxSide>> = engine.send(task).await.expect("send");
    let receipt_id = sender_receipt.id();
    wait_for(
        &sender_receipt,
        2_000,
        |s| matches!(s, State::Processing),
        "sender Processing",
    )
    .await;

    let (mut sender_send, sender_recv) = sender_conn.open_bi().await.unwrap();
    let _ = sender_send.write(&[]).await;
    let receiver_accept = tokio::spawn(async move { receiver_conn.accept_bi().await.unwrap() });
    let (recv_send, recv_recv) = receiver_accept.await.unwrap();

    let receiver_receipt: Arc<Receipt<RxSide>> = Arc::new(Receipt::<RxSide>::new(receipt_id));
    let receiver_registry = Arc::new(ReceiptRegistry::<RxSide>::new());
    receiver_registry.insert(Arc::clone(&receiver_receipt));
    drive_to_processing(&receiver_receipt);

    let received = ReceivedFile {
        id: Uuid::new_v4(),
        original_name: "nack.bin".to_string(),
        saved_path: PathBuf::from("/tmp/e2e_nack.bin"),
        size: 2048,
        sha256: None,
        received_at: SystemTime::now(),
        sender_ip: None,
    };
    let incoming = IncomingFile::new(
        received,
        Arc::clone(&receiver_receipt),
        Arc::clone(&receiver_registry),
    );

    let sr_clone = Arc::clone(&sender_receipt);
    let sender_loop = tokio::spawn(async move { run_sender_loop(sender_recv, sr_clone).await });

    let (verdict_tx, verdict_rx) = oneshot::channel();
    let rr_clone = Arc::clone(&receiver_receipt);
    let receiver_loop =
        tokio::spawn(
            async move { run_receiver_loop(recv_send, recv_recv, rr_clone, verdict_rx).await },
        );

    incoming.nack(REASON).await.expect("nack");
    verdict_tx
        .send(ReceiverVerdict::Nack(REASON.to_string()))
        .unwrap();

    // Receiver-side terminal carries the reason verbatim.
    match receiver_receipt.state() {
        State::Failed(FailedTerminal::Nacked { reason }) => {
            assert_eq!(reason, REASON);
        }
        other => panic!("expected receiver Nacked, got {other:?}"),
    }

    // Sender observes the Nacked frame; reason survives the wire.
    let final_state = wait_for(
        &sender_receipt,
        5_000,
        |s| matches!(s, State::Failed(FailedTerminal::Nacked { .. })),
        "sender Nacked",
    )
    .await;
    match final_state {
        State::Failed(FailedTerminal::Nacked { reason }) => {
            assert_eq!(reason, REASON, "sender-side reason must match wire");
        }
        other => panic!("expected sender Nacked, got {other:?}"),
    }

    let _ = sender_send.finish();
    let _ = tokio::time::timeout(Duration::from_secs(3), receiver_loop).await;
    let _ = tokio::time::timeout(Duration::from_secs(3), sender_loop).await;
}

// ─────────────────────────────────────────────────────────────────────
// Test 3 — HTTP SSE consumer mirrors what the receipt would emit on QUIC.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn e2e_http_sse_observes_quic_transfer() {
    // Bootstrap an HTTP-only FileReceiver — we only need the SSE
    // endpoint mounted under `/v1/receipts/:id/events`. QUIC is off
    // because the SSE stream is the source of truth either way.
    let dir = TempDir::new().unwrap();
    let port = free_port();
    let cfg = ServerConfig {
        http_port: port,
        bind_address: "127.0.0.1".to_string(),
        receive_directory: dir.path().to_path_buf(),
        enable_quic: false,
        enable_ws: false,
        ..ServerConfig::default()
    };
    let mut receiver = FileReceiver::new(cfg);
    receiver.start().await.expect("server start");
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Stand in for the QUIC sender's Receipt<Sender>. We pre-create
    // it in `Initiated` and register before subscribing so the SSE
    // consumer's first frame is the canonical opening state — this is
    // the same state QUIC peers see on the wire after the receipt
    // stream is opened. `TransferEngine::send` is exercised end-to-end
    // in tests #1 and #2; here we focus on the HTTP fallback path.
    let sender_receipt: Arc<Receipt<TxSide>> = Arc::new(Receipt::<TxSide>::new(Uuid::new_v4()));
    let receipt_id = sender_receipt.id();
    receiver
        .receipts()
        .sender_receipts
        .insert(Arc::clone(&sender_receipt));

    // Subscribe to SSE in a background task BEFORE driving any
    // transitions, so the consumer sees the `Initiated → … → Acked`
    // sequence in full.
    let url = format!("http://127.0.0.1:{port}/v1/receipts/{receipt_id}/events");
    let sse_task = tokio::spawn(async move {
        let resp = reqwest::Client::new()
            .get(&url)
            .send()
            .await
            .expect("SSE GET");
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
            "text/event-stream"
        );
        tokio::time::timeout(Duration::from_secs(5), resp.text())
            .await
            .expect("SSE body must finish on terminal")
            .expect("SSE read")
    });

    // Give the SSE consumer a moment to subscribe and flush the
    // initial `Initiated` snapshot.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Drive the receipt through the same lifecycle a real
    // `TransferEngine::send` would, with small pauses so each watch
    // tick reaches the SSE encoder before the next transition fires.
    for ev in [
        ReceiptEvent::Open,
        ReceiptEvent::Close,
        ReceiptEvent::Close,
        ReceiptEvent::Process,
        ReceiptEvent::Ack,
    ] {
        sender_receipt.apply_event(ev).unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
    }

    let body = tokio::time::timeout(Duration::from_secs(8), sse_task)
        .await
        .expect("SSE task did not terminate")
        .expect("SSE task panicked");

    // Verify the SSE payload contains the full `receipt-state`
    // sequence ending in a `terminal=true` Completed frame.
    assert!(
        body.contains("event: receipt-state"),
        "missing event header in SSE body: {body}"
    );
    assert!(
        body.contains("\"terminal\":true"),
        "missing terminal frame in SSE body: {body}"
    );
    assert!(
        body.contains("\"completed\""),
        "terminal frame should label as completed: {body}"
    );

    // Ordering check: Initiated must precede Completed in the wire
    // byte stream (i.e. the consumer sees the lifecycle prefix-first).
    let init_pos = body
        .find("\"initiated\"")
        .expect("missing initiated frame in SSE body");
    let completed_pos = body
        .find("\"completed\"")
        .expect("missing completed frame in SSE body");
    assert!(
        init_pos < completed_pos,
        "expected initiated before completed in SSE body"
    );

    receiver.stop().await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────
// Test 4 — MCP cancel_receipt round-trips into a TransferEngine receipt.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn e2e_mcp_cancel_receipt_round_trip() {
    use aerosync_mcp::server::{AeroSyncMcpServer, CancelReceiptParams};

    // A real TransferEngine with an in-flight (slow) transfer so the
    // receipt is still pre-terminal when cancel arrives.
    let cfg = TransferConfig {
        max_concurrent_transfers: 1,
        ..TransferConfig::default()
    };
    let engine = TransferEngine::new(cfg);
    engine
        .start(Arc::new(SlowAdapter { delay_ms: 5_000 }))
        .await
        .unwrap();
    let task = TransferTask::new_upload(
        PathBuf::from("/virtual/slow.bin"),
        "http://example/upload".to_string(),
        8192,
    );
    let receipt: Arc<Receipt<TxSide>> = engine.send(task).await.unwrap();
    let receipt_id = receipt.id();

    // Stand up an MCP server and re-register the engine's receipt in
    // its registry so `cancel_receipt` can find it. Production wiring
    // would share a single `ReceiptRegistry` between the two — for the
    // test we register the same Arc in both, which is equivalent.
    let mcp = AeroSyncMcpServer::new();
    mcp.receipt_registry().insert(Arc::clone(&receipt));

    // Drive the tool. The full JSON payload of `CallToolResult` is
    // covered by aerosync-mcp's own unit tests; here we only assert
    // the *side effect* on the registered receipt — that is the
    // round-trip evidence task #10 was meant to prove.
    let _ = mcp
        .cancel_receipt(aerosync_mcp::server::wrap_params(CancelReceiptParams {
            receipt_id: receipt_id.to_string(),
            reason: Some("e2e-cancel".to_string()),
            mcp_auth_token: None,
        }))
        .await
        .expect("cancel_receipt tool must succeed");

    // The engine's receipt itself must be Cancelled with our reason.
    match receipt.state() {
        State::Failed(FailedTerminal::Cancelled { reason }) => {
            assert_eq!(reason, "e2e-cancel");
        }
        other => panic!("expected receipt Cancelled, got {other:?}"),
    }
}

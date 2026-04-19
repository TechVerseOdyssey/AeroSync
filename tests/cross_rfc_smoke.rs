//! Cross-RFC smoke test (Week 8) — exercises **all three** v0.2.0 RFCs
//! in a single, intentionally explicit end-to-end scenario:
//!
//! - **RFC-001 (Python SDK)** — through its underlying Rust building
//!   blocks: `TransferEngine::send_with_metadata` is the same call the
//!   PyO3 binding makes; `IncomingFile::ack` is the same call the
//!   `aerosync.IncomingFile.ack` binding makes.
//! - **RFC-002 (Receipt Protocol)** — the `Receipt<Sender>` returned
//!   by the engine, the `Receipt<Receiver>` paired with the
//!   `IncomingFile`, the bidi state mirror, and the HTTP SSE control
//!   plane (`GET /v1/receipts/:id/events`).
//! - **RFC-003 (Metadata Envelope)** — a fully-populated `Metadata`
//!   built via `MetadataBuilder`, sealed by the engine, observed at
//!   the receiver via `IncomingFile::metadata()`, and queryable via
//!   `HistoryStore::query`.
//!
//! # Honesty clause — manually-bridged paths
//!
//! At the time of writing, two pieces of automatic plumbing remain
//! deferred to v0.2.1 and are bridged at the test layer here:
//!
//! 1. **w3c-quic-receipt-wiring** — neither `QuicTransfer::upload` nor
//!    the `FileReceiver` QUIC accept loop opens the bidi receipt
//!    stream automatically. We therefore (a) drive the sender
//!    receipt's terminal `Ack` event manually after the receiver's
//!    `IncomingFile::ack()` returns, instead of relying on the wire
//!    to ferry the Acked frame back, and (b) construct the
//!    receiver-side `IncomingFile` directly from the `ReceivedFile`
//!    stamped by the same metadata envelope. The sender's
//!    `Receipt::processed()` is awaited on the real local state
//!    machine — only the cross-process hand-off is bridged.
//! 2. **w2c-resume-sqlite** — the `ResumeStore` is JSON-file based
//!    and is not exercised here (this smoke test does not crash the
//!    process; the deferral is documented in `CHANGELOG.md`).
//!
//! Run-time on a quiet laptop is ~1 second; well under the 30s CI cap.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use aerosync::core::history::{HistoryQuery, HistoryStore};
use aerosync::core::metadata::MetadataBuilder;
use aerosync::core::receipt::{
    CompletedTerminal, Event as ReceiptEvent, Outcome, Receipt, Receiver as RxSide,
    Sender as TxSide, State,
};
use aerosync::core::receipt_registry::ReceiptRegistry;
use aerosync::core::server::{FileReceiver, ReceivedFile, ServerConfig};
use aerosync::core::transfer::{TransferConfig, TransferEngine, TransferTask};
use aerosync::core::IncomingFile;
use aerosync_proto::Lifecycle;
use tempfile::tempdir;
use uuid::Uuid;

use crate::common::SuccessAdapter;

/// Pick a free TCP port. Race-prone in theory but adequate for a
/// single-shot bind in a test process. Mirrors the helper in
/// `tests/receipts_e2e.rs`.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Drive a sender `Receipt` from any pre-Ack state through the legal
/// transitions to `Processing`, then absorb the terminal `Ack`. Used
/// in lieu of the deferred QUIC receipt-stream wiring (see file-level
/// "Honesty clause"). All `apply_event` calls are no-ops once the
/// receipt is past the requested phase, so this is safe to call after
/// the engine bridge has already advanced state on its own.
fn manually_drive_to_acked<S>(receipt: &Receipt<S>) {
    let _ = receipt.apply_event(ReceiptEvent::Open);
    let _ = receipt.apply_event(ReceiptEvent::Close);
    let _ = receipt.apply_event(ReceiptEvent::Close);
    let _ = receipt.apply_event(ReceiptEvent::Process);
    let _ = receipt.apply_event(ReceiptEvent::Ack);
}

/// THE cross-RFC end-to-end smoke. Single big test on purpose: it is
/// the integration witness that v0.2.0 ships a coherent system, not
/// three independent features.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cross_rfc_smoke_001_002_003_end_to_end() {
    // ── Setup: tempdir, on-disk JSONL history, HTTP-only FileReceiver
    //    for the SSE control plane (RFC-002 §5).
    let dir = tempdir().unwrap();
    let src = dir.path().join("smoke-payload.csv");
    tokio::fs::write(&src, b"hello,world\n1,2\n").await.unwrap();
    let history_path = dir.path().join("history.jsonl");
    let history = Arc::new(HistoryStore::new(&history_path).await.unwrap());

    let port = free_port();
    let cfg = ServerConfig {
        http_port: port,
        bind_address: "127.0.0.1".to_string(),
        receive_directory: dir.path().to_path_buf(),
        enable_quic: false,
        enable_ws: false,
        ..ServerConfig::default()
    };
    let mut server = FileReceiver::new(cfg);
    server.start().await.expect("FileReceiver start");
    tokio::time::sleep(Duration::from_millis(100)).await;

    // ── RFC-001 + RFC-003: build a fully-populated Metadata envelope
    //    (well-known + user fields) and send it via the same engine
    //    entry point the Python `Client.send(metadata=...)` binding
    //    drives.
    let engine = TransferEngine::new(TransferConfig::default())
        .with_history_store(Arc::clone(&history))
        .with_node_id("smoke-sender");
    engine.start(Arc::new(SuccessAdapter)).await.unwrap();

    let metadata = MetadataBuilder::new()
        .trace_id("smoke-trace-1")
        .conversation_id("smoke-conv-1")
        .lifecycle(Lifecycle::Transient)
        .user("agent_id", "smoke-test")
        .user("tenant", "alpha")
        .build()
        .expect("metadata builder must accept this envelope");

    let task = TransferTask::new_upload(src.clone(), format!("http://127.0.0.1:{port}/upload"), 16);
    let sender_receipt: Arc<Receipt<TxSide>> = engine
        .send_with_metadata(task, metadata.clone())
        .await
        .expect("send_with_metadata must succeed");
    let receipt_id = sender_receipt.id();

    // RFC-002 §7: state must be a legal transient/initial value right
    // after `send` returns. `send_with_metadata` immediately applies
    // `Event::Open`, so the receipt is at-or-past `StreamOpened`.
    let post_send = sender_receipt.state();
    assert!(
        matches!(
            post_send,
            State::Initiated
                | State::StreamOpened
                | State::DataTransferred
                | State::StreamClosed
                | State::Processing
        ),
        "unexpected post-send state: {post_send:?}"
    );

    // Engine sealed the system half of the envelope on top of the
    // user-supplied envelope (RFC-003 §4.1).
    let sealed = engine
        .metadata_for(receipt_id)
        .await
        .expect("sealed metadata must exist before terminal");
    assert_eq!(sealed.trace_id.as_deref(), Some("smoke-trace-1"));
    assert_eq!(sealed.conversation_id.as_deref(), Some("smoke-conv-1"));
    assert_eq!(sealed.lifecycle, Some(Lifecycle::Transient as i32));
    assert_eq!(
        sealed.user_metadata.get("agent_id").map(String::as_str),
        Some("smoke-test")
    );
    assert_eq!(
        sealed.user_metadata.get("tenant").map(String::as_str),
        Some("alpha")
    );
    assert_eq!(sealed.id, receipt_id.to_string());
    assert_eq!(sealed.from_node, "smoke-sender");
    assert_eq!(sealed.size_bytes, 16);
    assert_eq!(sealed.file_name, "smoke-payload.csv");
    assert!(sealed.created_at.is_some(), "engine must stamp created_at");

    // ── RFC-002 §5: register the sender receipt with the
    //    FileReceiver's HTTP control plane so the SSE endpoint can
    //    serve its state stream. In production this hand-off happens
    //    at QUIC accept time; here we wire it directly.
    server
        .receipts()
        .sender_receipts
        .insert(Arc::clone(&sender_receipt));

    // Subscribe to SSE in a background task BEFORE the receipt
    // reaches a terminal so the consumer captures the full lifecycle.
    let url = format!("http://127.0.0.1:{port}/v1/receipts/{receipt_id}/events");
    let sse_task = tokio::spawn(async move {
        let resp = reqwest::Client::new()
            .get(&url)
            .send()
            .await
            .expect("SSE GET must succeed");
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
            "text/event-stream",
            "SSE content-type must be text/event-stream"
        );
        tokio::time::timeout(Duration::from_secs(5), resp.text())
            .await
            .expect("SSE body must finish on terminal")
            .expect("SSE read")
    });

    // Brief pause so the SSE consumer sees the engine-applied
    // pre-terminal frames before we drive the terminal Ack.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // ── Receiver-side (RFC-002 §6.4 + RFC-003 §4): construct an
    //    IncomingFile carrying the SAME metadata envelope, paired
    //    with a Receipt<Receiver> that owns the same receipt id, and
    //    call ack(). In a fully wired QUIC setup the receiver loop
    //    would build this from the on-wire `TransferStart`; here we
    //    bridge it directly (w3c-deferred bridging — see file-level
    //    "Honesty clause").
    let receiver_receipt: Arc<Receipt<RxSide>> = Arc::new(Receipt::<RxSide>::new(receipt_id));
    let receiver_registry = Arc::new(ReceiptRegistry::<RxSide>::new());
    receiver_registry.insert(Arc::clone(&receiver_receipt));
    // Drive receiver receipt to Processing so the Ack is a legal
    // next event, mirroring what the wire-side Process frame would
    // produce.
    let _ = receiver_receipt.apply_event(ReceiptEvent::Open);
    let _ = receiver_receipt.apply_event(ReceiptEvent::Close);
    let _ = receiver_receipt.apply_event(ReceiptEvent::Close);
    let _ = receiver_receipt.apply_event(ReceiptEvent::Process);

    let received = ReceivedFile {
        id: Uuid::new_v4(),
        original_name: "smoke-payload.csv".to_string(),
        saved_path: src.clone(),
        size: 16,
        sha256: None,
        received_at: SystemTime::now(),
        sender_ip: None,
        metadata: None,
    };
    let incoming = IncomingFile::new(
        received,
        Arc::clone(&receiver_receipt),
        Arc::clone(&receiver_registry),
    )
    .with_metadata(sealed.clone());

    // RFC-003 §4 + §6: the receiver-side metadata getter exposes the
    // same envelope the sender built.
    assert_eq!(
        incoming.metadata().trace_id.as_deref(),
        Some("smoke-trace-1")
    );
    assert_eq!(
        incoming.metadata().user_metadata.get("agent_id"),
        Some(&"smoke-test".to_string())
    );
    assert_eq!(
        incoming.metadata().user_metadata.get("tenant"),
        Some(&"alpha".to_string())
    );

    // RFC-001 / RFC-002 §6.4: receiver application acks. This drives
    // the *receiver-side* receipt to Acked locally; the sender side
    // is acked via the manual bridge below (deferred wiring).
    incoming
        .ack()
        .await
        .expect("receiver-side ack must succeed");
    assert!(
        matches!(
            receiver_receipt.state(),
            State::Completed(CompletedTerminal::Acked)
        ),
        "receiver receipt must terminal at Acked, got {:?}",
        receiver_receipt.state()
    );

    // ── w3c-deferred bridging: drive the sender-side receipt to
    //    Acked manually. With the QUIC receipt stream wired, this
    //    transition would arrive automatically off the wire.
    manually_drive_to_acked(&sender_receipt);

    // RFC-002 §10: `processed()` must resolve to the Acked outcome.
    let outcome = tokio::time::timeout(Duration::from_secs(5), sender_receipt.processed())
        .await
        .expect("processed() did not resolve within 5s");
    match outcome {
        Outcome::Acked => {}
        other => panic!("expected Acked outcome, got {other:?}"),
    }

    // ── RFC-002 §5: the SSE consumer saw the full state sequence,
    //    ending in a `terminal=true` Completed frame.
    let body = tokio::time::timeout(Duration::from_secs(8), sse_task)
        .await
        .expect("SSE task did not terminate")
        .expect("SSE task panicked");
    assert!(
        body.contains("event: receipt-state"),
        "SSE body missing event header: {body}"
    );
    assert!(
        body.contains("\"terminal\":true"),
        "SSE body missing terminal frame: {body}"
    );
    assert!(
        body.contains("\"completed\""),
        "SSE body missing completed terminal label: {body}"
    );

    // ── RFC-003 §8: HistoryStore.query must find the record by
    //    metadata. Use both `trace_id` shortcut and `metadata_eq` to
    //    cover both filter shapes.
    // The history bridge runs on a tokio task; give it a moment to
    // flush the terminal mirror to disk.
    let by_trace = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let entries = history
                .query(&HistoryQuery {
                    trace_id: Some("smoke-trace-1".into()),
                    limit: 10,
                    ..Default::default()
                })
                .await
                .expect("history.query must succeed");
            if !entries.is_empty() {
                return entries;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("history record never appeared");
    assert_eq!(by_trace.len(), 1, "expected exactly one history record");
    let mref = by_trace[0]
        .metadata
        .as_ref()
        .expect("history record must carry metadata");
    assert_eq!(mref.trace_id.as_deref(), Some("smoke-trace-1"));
    assert_eq!(
        mref.user_metadata.get("agent_id").map(String::as_str),
        Some("smoke-test")
    );
    assert_eq!(
        mref.user_metadata.get("tenant").map(String::as_str),
        Some("alpha")
    );
    assert_eq!(mref.lifecycle.as_deref(), Some("transient"));
    assert_eq!(
        by_trace[0].receipt_id,
        Some(receipt_id),
        "history record must reference the sender receipt id"
    );

    // metadata_eq AND-semantics over user fields (RFC-003 §8 query
    // shape — the same one Python `Client.history(metadata_filter=)`
    // and the MCP `list_history` tool drive).
    let mut want = HashMap::new();
    want.insert("agent_id".into(), "smoke-test".into());
    want.insert("tenant".into(), "alpha".into());
    let by_kv = history
        .query(&HistoryQuery {
            metadata_eq: want,
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("metadata_eq query must succeed");
    assert_eq!(by_kv.len(), 1, "metadata_eq AND-filter must hit one record");

    // ── Tear down. Failure to stop the server here would leak the
    //    bound port across cargo-test invocations.
    server.stop().await.expect("server stop");
}

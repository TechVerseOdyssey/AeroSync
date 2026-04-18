//! End-to-end integration tests for the RFC-002 §6.4 receipt HTTP
//! control surface (Week 2 task #5).
//!
//! These tests spin up a real `FileReceiver`, register a `Receipt`
//! into the shared `ReceiptHttpState`, and exercise the four
//! endpoints over actual `reqwest` HTTP — proving the routes are
//! mounted into `build_axum_router` (not just unit-test-only) and
//! that ack/nack/cancel/SSE all reach the right registry.

use aerosync::core::receipt::{
    CompletedTerminal, Event as ReceiptEvent, FailedTerminal, Receipt, Receiver as RxSide,
    Sender as TxSide, State,
};
use aerosync::core::server::{FileReceiver, ServerConfig};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use uuid::Uuid;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn start_http_receiver() -> (FileReceiver, u16, TempDir) {
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
    receiver.start().await.expect("server start failed");
    tokio::time::sleep(Duration::from_millis(100)).await;
    (receiver, port, dir)
}

fn drive_receiver_to_processing(r: &Receipt<RxSide>) {
    r.apply_event(ReceiptEvent::Open).unwrap();
    r.apply_event(ReceiptEvent::Close).unwrap();
    r.apply_event(ReceiptEvent::Close).unwrap();
    r.apply_event(ReceiptEvent::Process).unwrap();
}

#[tokio::test]
async fn ack_endpoint_round_trip_over_real_http() {
    let (mut receiver, port, _dir) = start_http_receiver().await;
    let receipts = receiver.receipts();
    let r = Arc::new(Receipt::<RxSide>::new(Uuid::new_v4()));
    drive_receiver_to_processing(&r);
    let id = r.id();
    receipts.receiver_receipts.insert(Arc::clone(&r));

    let url = format!("http://127.0.0.1:{port}/v1/receipts/{id}/ack");
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({"idempotency_key":"k1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["state"]["state"], "completed");
    assert!(matches!(
        r.state(),
        State::Completed(CompletedTerminal::Acked)
    ));

    receiver.stop().await.unwrap();
}

#[tokio::test]
async fn nack_endpoint_returns_failed_nacked_state() {
    let (mut receiver, port, _dir) = start_http_receiver().await;
    let receipts = receiver.receipts();
    let r = Arc::new(Receipt::<RxSide>::new(Uuid::new_v4()));
    drive_receiver_to_processing(&r);
    let id = r.id();
    receipts.receiver_receipts.insert(Arc::clone(&r));

    let url = format!("http://127.0.0.1:{port}/v1/receipts/{id}/nack");
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({
            "reason": "checksum-mismatch",
            "idempotency_key": "k1"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    match r.state() {
        State::Failed(FailedTerminal::Nacked { reason }) => {
            assert_eq!(reason, "checksum-mismatch");
        }
        other => panic!("expected nacked, got {other:?}"),
    }

    receiver.stop().await.unwrap();
}

#[tokio::test]
async fn cancel_endpoint_routes_to_sender_registry() {
    let (mut receiver, port, _dir) = start_http_receiver().await;
    let receipts = receiver.receipts();
    let r = Arc::new(Receipt::<TxSide>::new(Uuid::new_v4()));
    let id = r.id();
    receipts.sender_receipts.insert(Arc::clone(&r));

    let url = format!("http://127.0.0.1:{port}/v1/receipts/{id}/cancel");
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({
            "reason": "user-stop",
            "idempotency_key": "k1"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    match r.state() {
        State::Failed(FailedTerminal::Cancelled { reason }) => {
            assert_eq!(reason, "user-stop");
        }
        other => panic!("expected cancelled, got {other:?}"),
    }

    receiver.stop().await.unwrap();
}

#[tokio::test]
async fn idempotency_key_replay_returns_200_and_does_not_double_apply() {
    let (mut receiver, port, _dir) = start_http_receiver().await;
    let receipts = receiver.receipts();
    let r = Arc::new(Receipt::<RxSide>::new(Uuid::new_v4()));
    drive_receiver_to_processing(&r);
    let id = r.id();
    receipts.receiver_receipts.insert(Arc::clone(&r));

    let url = format!("http://127.0.0.1:{port}/v1/receipts/{id}/ack");
    let body = serde_json::json!({"idempotency_key":"same-key"});

    let r1 = reqwest::Client::new()
        .post(&url)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r1.status(), 200);

    let r2 = reqwest::Client::new()
        .post(&url)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r2.status(), 200, "replay should be 200 OK");
    let json: serde_json::Value = r2.json().await.unwrap();
    assert_eq!(json["replayed"], true);

    assert!(matches!(
        r.state(),
        State::Completed(CompletedTerminal::Acked)
    ));

    receiver.stop().await.unwrap();
}

#[tokio::test]
async fn sse_endpoint_streams_state_then_closes_on_terminal() {
    let (mut receiver, port, _dir) = start_http_receiver().await;
    let receipts = receiver.receipts();
    let r = Arc::new(Receipt::<TxSide>::new(Uuid::new_v4()));
    let id = r.id();
    receipts.sender_receipts.insert(Arc::clone(&r));

    // Background task drives the receipt to terminal a beat after the
    // SSE stream subscribes.
    let r_clone = Arc::clone(&r);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        r_clone
            .apply_event(ReceiptEvent::Cancel {
                reason: "external-stop".into(),
            })
            .unwrap();
    });

    let url = format!("http://127.0.0.1:{port}/v1/receipts/{id}/events");
    let resp = reqwest::Client::new().get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "text/event-stream"
    );

    let body = tokio::time::timeout(Duration::from_secs(3), resp.text())
        .await
        .expect("sse body must finish after terminal")
        .expect("read body");
    assert!(
        body.contains("event: receipt-state"),
        "missing event header: {body}"
    );
    assert!(
        body.contains("\"terminal\":true"),
        "missing terminal frame: {body}"
    );

    receiver.stop().await.unwrap();
}

#[tokio::test]
async fn sse_unknown_id_returns_404() {
    let (mut receiver, port, _dir) = start_http_receiver().await;
    let url = format!(
        "http://127.0.0.1:{port}/v1/receipts/{}/events",
        Uuid::new_v4()
    );
    let resp = reqwest::Client::new().get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 404);
    receiver.stop().await.unwrap();
}

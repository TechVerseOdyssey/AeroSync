//! QUIC receipt-stream end-to-end integration test
//! (RFC-002 §6.3 — w0.2.1 batch C "P0.2").
//!
//! Sister test to [`tests/quic_metadata_propagation.rs`]. Where that
//! file proves the **forward direction** (sender → receiver
//! `Metadata` envelope), this file proves the **reverse direction**
//! (receiver → sender `ReceiptFrame` acknowledgement) on the bidi
//! control stream.
//!
//! The test boots a real [`FileReceiver`] QUIC listener, sends a real
//! file via [`QuicTransfer`] with a sender-side receipt sink wired up
//! through [`QuicTransfer::with_receipt_sink`], and asserts the sink
//! observes both `Received` and `Acked` frames before the upload
//! call returns.

#![cfg(feature = "quic")]

use std::time::Duration;

use aerosync::core::server::{FileReceiver, ServerConfig};
use aerosync::core::transfer::TransferTask;
use aerosync::protocols::quic::{QuicConfig, QuicTransfer};
use aerosync::protocols::traits::{TransferProgress, TransferProtocol};
use aerosync_proto::{receipt_frame, ReceiptFrame};
use tempfile::TempDir;
use tokio::sync::mpsc;

async fn boot_quic_receiver() -> (FileReceiver, std::net::SocketAddr, TempDir) {
    let dir = TempDir::new().unwrap();
    let cfg = ServerConfig {
        http_port: 0,
        quic_port: 0,
        bind_address: "127.0.0.1".to_string(),
        receive_directory: dir.path().to_path_buf(),
        enable_http: false,
        enable_quic: true,
        enable_ws: false,
        ..ServerConfig::default()
    };
    let mut receiver = FileReceiver::new(cfg);
    receiver.start().await.expect("FileReceiver QUIC start");
    let addr = receiver
        .local_quic_addr()
        .expect("local_quic_addr after start");
    (receiver, addr, dir)
}

/// Drain the sender-side receipt sink, returning every frame it saw
/// up to `deadline`. The QUIC receiver auto-acks on successful
/// checksum (see `handle_quic_file_upload` in `core::server`), so a
/// healthy run yields one `Received` followed by one `Acked` and then
/// the channel closes when the drainer task exits on the terminal
/// frame.
async fn collect_frames(
    rx: &mut mpsc::UnboundedReceiver<ReceiptFrame>,
    deadline: Duration,
) -> Vec<ReceiptFrame> {
    let mut out = Vec::new();
    let stop_at = tokio::time::Instant::now() + deadline;
    loop {
        let now = tokio::time::Instant::now();
        if now >= stop_at {
            break;
        }
        let remaining = stop_at - now;
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(frame)) => {
                let terminal = matches!(
                    frame.body.as_ref(),
                    Some(receipt_frame::Body::Acked(_))
                        | Some(receipt_frame::Body::Nacked(_))
                        | Some(receipt_frame::Body::Failed(_))
                );
                out.push(frame);
                if terminal {
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quic_sender_observes_receipt_frames_after_upload() {
    let (receiver, addr, dir) = boot_quic_receiver().await;

    let payload = dir.path().join("receipt.bin");
    let body = b"hello QUIC receipt stream\n";
    tokio::fs::write(&payload, body).await.unwrap();

    let mut task = TransferTask::new_upload(
        payload.clone(),
        format!("quic://{}", addr),
        body.len() as u64,
    );
    task.metadata = Some(aerosync_proto::Metadata {
        trace_id: Some("rcpt-1".into()),
        ..Default::default()
    });

    let (rx_tx, mut rx_rx) = mpsc::unbounded_channel::<ReceiptFrame>();
    let cfg = QuicConfig {
        server_name: "localhost".to_string(),
        server_addr: addr,
        ..QuicConfig::default()
    };
    let sender = QuicTransfer::new(cfg)
        .expect("QuicTransfer::new")
        .with_receipt_sink(rx_tx);

    let (progress_tx, _progress_rx) = mpsc::unbounded_channel::<TransferProgress>();
    sender
        .upload_file(&task, progress_tx)
        .await
        .expect("QUIC upload must succeed");

    // The sender's drainer task is given a 5s window inside
    // `upload_with_progress` to surface terminal frames before the
    // call returns; allow a generous grace period here on top of that
    // so a slow CI runner can flush the channel.
    let frames = collect_frames(&mut rx_rx, Duration::from_secs(10)).await;

    assert!(
        !frames.is_empty(),
        "sender-side receipt sink must have observed at least one ReceiptFrame; \
         the receiver auto-acks on successful checksum"
    );
    let saw_received = frames
        .iter()
        .any(|f| matches!(f.body.as_ref(), Some(receipt_frame::Body::Received(_))));
    let saw_acked = frames
        .iter()
        .any(|f| matches!(f.body.as_ref(), Some(receipt_frame::Body::Acked(_))));
    assert!(
        saw_received,
        "sender must observe a `Received` frame from the wire (RFC-002 §6.3) — saw {:?}",
        frames
    );
    assert!(
        saw_acked,
        "sender must observe an `Acked` frame after auto-ack — saw {:?}",
        frames
    );

    // All terminal frames carry the sender-issued receipt_id (which
    // is the task UUID stringified). This guards against a
    // future refactor that breaks the receipt_id round-trip.
    let expected_id = task.id.to_string();
    for f in &frames {
        assert_eq!(
            f.receipt_id, expected_id,
            "every receipt frame must carry the sender's receipt_id"
        );
    }

    drop(receiver);
}

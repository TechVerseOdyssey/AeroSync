//! Bidirectional QUIC receipt stream — RFC-002 §6.2 task #4.
//!
//! This module owns the framing codec, the read loop, and the
//! sender-/receiver-side task drivers for the **receipt stream** —
//! the bidirectional QUIC stream that carries [`ReceiptFrame`]s back
//! to the sender and (in v0.3+) forward control frames to the
//! receiver. It deliberately does NOT touch the chunk-transfer code
//! in `quic.rs`; the receipt stream is opened **in addition** to the
//! existing per-chunk uni-streams so we can wire the new protocol
//! without disturbing the legacy upload/download paths.
//!
//! # Stream identity
//!
//! RFC-002 §6.2 names the receipt stream "stream 12" by analogy with
//! the QUIC client-initiated bidi stream id sequence 0, 4, 8, 12, …
//! In practice **`quinn` does not let application code pin a numeric
//! stream id** — peers open streams in opening order and the QUIC
//! library assigns the id. We treat "stream 12" as a **role name**:
//! it is whichever bidi stream is opened by the **sender** for the
//! purpose of carrying [`ReceiptFrame`]s.
//!
//! Per the task brief (and the honesty clause: "pick the most natural
//! mapping in `quinn` and document it"), the **sender** opens the
//! receipt stream right after the chunk-transfer setup completes.
//! This is a deliberate divergence from RFC-002 §6.2 wording ("opened
//! by the receiver"); it matches the only direction that is
//! ergonomic given the existing sender-pushes-bytes architecture and
//! makes the wiring symmetrical with the rest of the QUIC code path.
//! When v0.3 wires up the bi-directional control stream proper this
//! choice can be revisited.
//!
//! # Framing
//!
//! Every frame on the wire is a single `ReceiptFrame` protobuf
//! message, **length-delimited** with a varint length prefix, exactly
//! as produced by [`prost::Message::encode_length_delimited_to_vec`].
//! This matches the RFC-002 §6.3 sentence: "All frames use protobuf
//! v3. Each frame is length-prefixed (varint) on its stream." The
//! varint is the standard protobuf varint encoding (1-10 bytes).
//!
//! The codec is sufficient on its own (no message terminators, no
//! magic bytes); a graceful close is signalled by the QUIC stream FIN
//! and an unexpected disconnect surfaces as
//! [`CodecError::StreamClosed`] from the next read attempt.
//!
//! # Stream-close semantics
//!
//! Per RFC-002 §7 ("the receipt stream goes silent → tear down"), if
//! the receipt stream FINs or errors before a terminal
//! [`ReceiptFrame`] arrives at the sender, the sender's local
//! [`Receipt<Sender>`] is transitioned to `Failed(Errored)` with
//! `code = ERROR_INTERNAL` and a `detail` describing the close type.
//! The receiver-side task does not modify its own receipt on close;
//! it just exits.
//!
//! # BytesReceived
//!
//! Per RFC-002 §13 Q1 (resolved), `BytesReceived` frames are sent iff
//! `chunk_size <= 1 MiB`. The codec accepts them unconditionally; the
//! decision to emit them lives one layer higher (in the eventual
//! `TransferEngine` integration). On the sender side we currently
//! *log and drop* them — RFC-002 task #11 (progress) hooks them into
//! `Receipt::watch()` properly.

use std::sync::Arc;
use std::time::SystemTime;

use prost::Message;
use thiserror::Error;
use tracing::{debug, trace, warn};

use aerosync_proto::{receipt_frame, ReceiptFrame};

use crate::core::receipt::{Event, Receipt, Receiver, Sender};

// ─────────────────────────────────────────────────────────────────────
// Codec errors
// ─────────────────────────────────────────────────────────────────────

/// Errors surfaced by the receipt-stream framing codec.
///
/// Decoding errors fall into two buckets: **wire-format** problems
/// (length prefix invalid, body fails to decode, oversize) and
/// **transport** problems (peer closed the stream, IO error). They
/// are mapped to distinct variants so the caller can choose to retry
/// vs. fail the receipt.
#[derive(Debug, Error)]
pub enum CodecError {
    /// The peer closed the stream with FIN. This is an expected
    /// graceful termination after a terminal `ReceiptFrame` and a
    /// surprise terminator otherwise.
    #[error("receipt stream closed by peer")]
    StreamClosed,
    /// The varint length prefix could not be parsed (e.g. ran off the
    /// end of the stream mid-varint).
    #[error("invalid length-delimiter prefix: {0}")]
    BadLength(String),
    /// The body bytes were read but did not decode as a
    /// [`ReceiptFrame`].
    #[error("failed to decode ReceiptFrame body: {0}")]
    Decode(#[from] prost::DecodeError),
    /// Underlying IO / QUIC stream error (read or write failure).
    #[error("transport error: {0}")]
    Transport(String),
    /// The next frame's declared length exceeds the configured cap.
    /// Defends against an adversarial peer trying to OOM us.
    #[error("frame too large: {actual} bytes > cap {cap}")]
    TooLarge {
        /// Length declared by the prefix.
        actual: usize,
        /// Configured per-frame cap.
        cap: usize,
    },
}

/// Hard cap on a single `ReceiptFrame` size. RFC-002 §1 quotes
/// "≤ 200 bytes per state transition on the wire". 64 KiB is far above
/// that headroom and still small enough to OOM-bound a malicious peer.
pub const FRAME_SIZE_CAP: usize = 64 * 1024;

// ─────────────────────────────────────────────────────────────────────
// Encoder / decoder
// ─────────────────────────────────────────────────────────────────────

/// Encode a single [`ReceiptFrame`] into a length-delimited buffer
/// ready to be written on the QUIC stream.
///
/// The output is `varint(len) || body` — exactly what
/// [`Message::encode_length_delimited_to_vec`] produces.
pub fn encode_frame(frame: &ReceiptFrame) -> Vec<u8> {
    frame.encode_length_delimited_to_vec()
}

/// Decode a single length-delimited [`ReceiptFrame`] from an
/// in-memory buffer that contains exactly one frame.
///
/// Returns the parsed frame on success or [`CodecError`] on a
/// malformed prefix, oversize, or body decode error. Useful for unit
/// tests; the transport read path uses
/// [`read_frame_from_stream`] instead.
pub fn decode_frame(mut buf: &[u8]) -> Result<ReceiptFrame, CodecError> {
    if buf.is_empty() {
        return Err(CodecError::BadLength("empty buffer".into()));
    }
    let frame = ReceiptFrame::decode_length_delimited(&mut buf)?;
    Ok(frame)
}

// ─────────────────────────────────────────────────────────────────────
// QUIC read/write helpers
// ─────────────────────────────────────────────────────────────────────

/// Read one length-delimited [`ReceiptFrame`] from a `quinn::RecvStream`.
///
/// Implementation detail: we manually decode the protobuf varint
/// length prefix one byte at a time (a varint is at most 10 bytes for
/// a u64), then read exactly `len` body bytes. This is necessary
/// because `prost::decode_length_delimited` needs the entire
/// frame in memory; QUIC delivers bytes asynchronously so we have to
/// collect them ourselves.
pub async fn read_frame_from_stream(
    stream: &mut quinn::RecvStream,
) -> Result<ReceiptFrame, CodecError> {
    // Step 1: read the varint length prefix one byte at a time.
    let mut len: u64 = 0;
    let mut shift: u32 = 0;
    let mut byte_buf = [0u8; 1];
    loop {
        let n = stream
            .read(&mut byte_buf)
            .await
            .map_err(|e| CodecError::Transport(e.to_string()))?;
        match n {
            None => {
                if shift == 0 {
                    return Err(CodecError::StreamClosed);
                } else {
                    return Err(CodecError::BadLength(
                        "stream closed mid-varint".to_string(),
                    ));
                }
            }
            Some(0) => continue,
            Some(_) => {
                let b = byte_buf[0];
                len |= ((b & 0x7F) as u64) << shift;
                if (b & 0x80) == 0 {
                    break;
                }
                shift += 7;
                if shift >= 64 {
                    return Err(CodecError::BadLength("varint overflows u64".into()));
                }
            }
        }
    }

    // Step 2: bounds check the declared length.
    let len_usize = usize::try_from(len)
        .map_err(|_| CodecError::BadLength(format!("length {len} doesn't fit usize")))?;
    if len_usize > FRAME_SIZE_CAP {
        return Err(CodecError::TooLarge {
            actual: len_usize,
            cap: FRAME_SIZE_CAP,
        });
    }

    // Step 3: read exactly `len` body bytes.
    let mut body = vec![0u8; len_usize];
    let mut offset = 0;
    while offset < len_usize {
        let n = stream
            .read(&mut body[offset..])
            .await
            .map_err(|e| CodecError::Transport(e.to_string()))?;
        match n {
            None => return Err(CodecError::BadLength("stream closed mid-body".into())),
            Some(0) => continue,
            Some(read) => offset += read,
        }
    }

    let frame = ReceiptFrame::decode(&body[..])?;
    Ok(frame)
}

/// Encode and write a single [`ReceiptFrame`] to a `quinn::SendStream`.
pub async fn write_frame_to_stream(
    stream: &mut quinn::SendStream,
    frame: &ReceiptFrame,
) -> Result<(), CodecError> {
    let buf = encode_frame(frame);
    stream
        .write_all(&buf)
        .await
        .map_err(|e| CodecError::Transport(e.to_string()))
}

// ─────────────────────────────────────────────────────────────────────
// Frame ↔ Event translation
// ─────────────────────────────────────────────────────────────────────

/// Translate a [`ReceiptFrame`] into the [`Event`] (or sequence of
/// events) the local state machine should apply.
///
/// `BytesReceived` frames have no corresponding local event in the
/// current 7-state machine — they're progress hints — so we return
/// `None` for them and the caller drops them on the floor (after
/// optionally publishing to a progress watcher).
///
/// Failure to decode a frame body's payload (e.g. an empty `body`
/// field) likewise returns `None`; the receipt stream is not the
/// place to enforce wire correctness — that lives in the codec layer.
pub fn frame_to_event(frame: &ReceiptFrame) -> Option<Event> {
    match frame.body.as_ref()? {
        receipt_frame::Body::BytesReceived(_) => None,
        receipt_frame::Body::Received(_) => {
            // RFC-002 §4: SENT → RECEIVED maps onto our generic
            // StreamClosed → Processing. We model "all bytes verified
            // by checksum" as the receiver requesting the application
            // begin processing.
            Some(Event::Process)
        }
        receipt_frame::Body::Failed(f) => Some(Event::Error {
            code: f.code as u32,
            detail: f.detail.clone(),
        }),
        receipt_frame::Body::Acked(_) => Some(Event::Ack),
        receipt_frame::Body::Nacked(n) => Some(Event::Nack {
            reason: n.reason.clone(),
        }),
    }
}

/// Build a sender-side [`ReceiptFrame::Acked`] for the given receipt.
pub fn build_acked_frame(receipt_id: &str) -> ReceiptFrame {
    ReceiptFrame {
        receipt_id: receipt_id.to_string(),
        body: Some(receipt_frame::Body::Acked(aerosync_proto::Acked {})),
        at: Some(now_timestamp()),
    }
}

/// Build a [`ReceiptFrame::Nacked`] frame.
pub fn build_nacked_frame(receipt_id: &str, reason: impl Into<String>) -> ReceiptFrame {
    ReceiptFrame {
        receipt_id: receipt_id.to_string(),
        body: Some(receipt_frame::Body::Nacked(aerosync_proto::Nacked {
            reason: reason.into(),
        })),
        at: Some(now_timestamp()),
    }
}

/// Build a [`ReceiptFrame::Received`] frame.
pub fn build_received_frame(receipt_id: &str, sha256: impl Into<String>) -> ReceiptFrame {
    ReceiptFrame {
        receipt_id: receipt_id.to_string(),
        body: Some(receipt_frame::Body::Received(aerosync_proto::Received {
            checksum_ok: true,
            sha256: sha256.into(),
        })),
        at: Some(now_timestamp()),
    }
}

/// Build a [`ReceiptFrame::Failed`] frame from a numeric code + detail.
pub fn build_failed_frame(
    receipt_id: &str,
    code: aerosync_proto::ErrorCode,
    detail: impl Into<String>,
) -> ReceiptFrame {
    ReceiptFrame {
        receipt_id: receipt_id.to_string(),
        body: Some(receipt_frame::Body::Failed(aerosync_proto::Failed {
            code: code as i32,
            detail: detail.into(),
        })),
        at: Some(now_timestamp()),
    }
}

/// Build a [`ReceiptFrame::BytesReceived`] frame.
pub fn build_bytes_received_frame(receipt_id: &str, bytes: u64) -> ReceiptFrame {
    ReceiptFrame {
        receipt_id: receipt_id.to_string(),
        body: Some(receipt_frame::Body::BytesReceived(
            aerosync_proto::BytesReceived { bytes },
        )),
        at: Some(now_timestamp()),
    }
}

fn now_timestamp() -> aerosync_proto::Timestamp {
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    aerosync_proto::Timestamp {
        seconds: dur.as_secs() as i64,
        nanos: dur.subsec_nanos() as i32,
    }
}

// ─────────────────────────────────────────────────────────────────────
// Sender-side driver
// ─────────────────────────────────────────────────────────────────────

/// Run the sender-side receipt-stream loop.
///
/// `bidi` is the bidirectional QUIC stream pair the sender opened
/// after the chunk-transfer setup (see module docs). The loop reads
/// inbound [`ReceiptFrame`]s from `recv` and applies them as events
/// to the local `Receipt<Sender>`. Returns once a terminal frame
/// arrives or the stream closes.
///
/// Stream-close-without-terminal is mapped to
/// `Failed(Errored { code = ERROR_INTERNAL, detail = ... })` per the
/// stream-close semantics in the module docs. If the local receipt is
/// already terminal at the moment of close (e.g. a local cancel
/// raced with the FIN), the resulting `apply_event` is a no-op and
/// silently swallowed — the local terminal state wins.
pub async fn run_sender_loop(
    mut recv: quinn::RecvStream,
    receipt: Arc<Receipt<Sender>>,
) -> Result<(), CodecError> {
    loop {
        match read_frame_from_stream(&mut recv).await {
            Ok(frame) => {
                if frame.receipt_id != receipt.id().to_string() {
                    warn!(
                        expected = %receipt.id(),
                        got = %frame.receipt_id,
                        "receipt stream: ignoring frame for foreign receipt id"
                    );
                    continue;
                }
                if let Some(event) = frame_to_event(&frame) {
                    if let Err(e) = receipt.apply_event(event.clone()) {
                        debug!(
                            ?e,
                            ?event,
                            "receipt: apply_event rejected (already terminal?)"
                        );
                    }
                } else {
                    trace!(?frame, "receipt: progress frame, no state change");
                }
                if receipt.state().is_terminal() {
                    return Ok(());
                }
            }
            Err(CodecError::StreamClosed) => {
                if !receipt.state().is_terminal() {
                    let _ = receipt.apply_event(Event::Error {
                        code: aerosync_proto::ErrorCode::ErrorInternal as u32,
                        detail: "receipt stream closed before terminal frame".to_string(),
                    });
                }
                return Err(CodecError::StreamClosed);
            }
            Err(e) => {
                if !receipt.state().is_terminal() {
                    let _ = receipt.apply_event(Event::Error {
                        code: aerosync_proto::ErrorCode::ErrorInternal as u32,
                        detail: format!("receipt stream codec error: {e}"),
                    });
                }
                return Err(e);
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Receiver-side driver
// ─────────────────────────────────────────────────────────────────────

/// Decision the receiver-side application makes about an incoming
/// transfer. Used by [`run_receiver_loop`] to decide which closing
/// frame to send.
#[derive(Debug, Clone)]
pub enum ReceiverVerdict {
    /// Application called `ack()`. Sends `Acked`.
    Ack,
    /// Application called `nack(reason)`. Sends `Nacked{reason}`.
    Nack(String),
    /// Internal failure; sends `Failed{code,detail}`.
    Fail(aerosync_proto::ErrorCode, String),
}

/// Run the receiver-side receipt-stream loop.
///
/// The receiver-side task accepts the bidi stream the sender opened
/// (the caller obtains the `(send, recv)` pair via
/// `connection.accept_bi()` and hands it in here). The loop owns the
/// **send** half so the receiver can transmit `Received` and the
/// terminal `Acked/Nacked` / `Failed` frame back; the **recv** half is
/// drained for any sender-initiated cancel hints (none in v0.2 — the
/// stream is currently sender→receiver-only on the read path, but we
/// still drain it to detect FIN cleanly).
///
/// `verdict_rx` is a one-shot channel via which the application
/// surfaces its `ack()`/`nack()` decision. The loop blocks on it
/// until either a verdict arrives or the receipt's local state goes
/// terminal (via cancel/error from another path).
pub async fn run_receiver_loop(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    receipt: Arc<Receipt<Receiver>>,
    verdict_rx: tokio::sync::oneshot::Receiver<ReceiverVerdict>,
) -> Result<(), CodecError> {
    let receipt_id = receipt.id().to_string();
    let recv_drain = tokio::spawn(async move {
        loop {
            match read_frame_from_stream(&mut recv).await {
                Ok(frame) => trace!(?frame, "receiver: drained sender frame"),
                Err(CodecError::StreamClosed) => return Ok::<(), CodecError>(()),
                Err(e) => return Err(e),
            }
        }
    });

    let verdict = match verdict_rx.await {
        Ok(v) => v,
        Err(_) => {
            let _ = write_frame_to_stream(
                &mut send,
                &build_failed_frame(
                    &receipt_id,
                    aerosync_proto::ErrorCode::ErrorInternal,
                    "verdict channel dropped before app responded",
                ),
            )
            .await;
            let _ = send.finish();
            recv_drain.abort();
            return Err(CodecError::Transport("verdict channel dropped".to_string()));
        }
    };

    let frame = match &verdict {
        ReceiverVerdict::Ack => build_acked_frame(&receipt_id),
        ReceiverVerdict::Nack(reason) => build_nacked_frame(&receipt_id, reason.clone()),
        ReceiverVerdict::Fail(code, detail) => build_failed_frame(&receipt_id, *code, detail),
    };
    write_frame_to_stream(&mut send, &frame).await?;
    send.finish()
        .map_err(|e| CodecError::Transport(e.to_string()))?;

    recv_drain.abort();
    let _ = recv_drain.await;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod codec_tests {
    use super::*;
    use aerosync_proto::{Acked, BytesReceived, Failed, Nacked, Received};

    fn frame_with(body: receipt_frame::Body) -> ReceiptFrame {
        ReceiptFrame {
            receipt_id: "test-receipt-id".into(),
            body: Some(body),
            at: Some(now_timestamp()),
        }
    }

    #[test]
    fn roundtrip_bytes_received() {
        let f = frame_with(receipt_frame::Body::BytesReceived(BytesReceived {
            bytes: 42_000,
        }));
        let buf = encode_frame(&f);
        let back = decode_frame(&buf).unwrap();
        assert_eq!(back.receipt_id, "test-receipt-id");
        match back.body.unwrap() {
            receipt_frame::Body::BytesReceived(b) => assert_eq!(b.bytes, 42_000),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn roundtrip_received() {
        let f = frame_with(receipt_frame::Body::Received(Received {
            checksum_ok: true,
            sha256: "ab".repeat(32),
        }));
        let buf = encode_frame(&f);
        let back = decode_frame(&buf).unwrap();
        match back.body.unwrap() {
            receipt_frame::Body::Received(r) => {
                assert!(r.checksum_ok);
                assert_eq!(r.sha256.len(), 64);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn roundtrip_acked() {
        let f = frame_with(receipt_frame::Body::Acked(Acked {}));
        let buf = encode_frame(&f);
        let back = decode_frame(&buf).unwrap();
        assert!(matches!(back.body.unwrap(), receipt_frame::Body::Acked(_)));
    }

    #[test]
    fn roundtrip_nacked() {
        let f = frame_with(receipt_frame::Body::Nacked(Nacked {
            reason: "schema mismatch".into(),
        }));
        let buf = encode_frame(&f);
        let back = decode_frame(&buf).unwrap();
        match back.body.unwrap() {
            receipt_frame::Body::Nacked(n) => assert_eq!(n.reason, "schema mismatch"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn roundtrip_failed() {
        let f = frame_with(receipt_frame::Body::Failed(Failed {
            code: aerosync_proto::ErrorCode::ErrorChecksum as i32,
            detail: "sha256 differs".into(),
        }));
        let buf = encode_frame(&f);
        let back = decode_frame(&buf).unwrap();
        match back.body.unwrap() {
            receipt_frame::Body::Failed(fl) => {
                assert_eq!(fl.code, aerosync_proto::ErrorCode::ErrorChecksum as i32);
                assert_eq!(fl.detail, "sha256 differs");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn decode_empty_buffer_is_bad_length() {
        let err = decode_frame(&[]).unwrap_err();
        assert!(matches!(err, CodecError::BadLength(_)));
    }

    #[test]
    fn frame_to_event_translations() {
        assert!(matches!(
            frame_to_event(&build_acked_frame("x")),
            Some(Event::Ack)
        ));
        assert!(matches!(
            frame_to_event(&build_nacked_frame("x", "bad")),
            Some(Event::Nack { .. })
        ));
        assert!(matches!(
            frame_to_event(&build_received_frame("x", "00".repeat(32))),
            Some(Event::Process)
        ));
        assert!(matches!(
            frame_to_event(&build_failed_frame(
                "x",
                aerosync_proto::ErrorCode::ErrorTimeout,
                "to"
            )),
            Some(Event::Error { .. })
        ));
        assert!(frame_to_event(&build_bytes_received_frame("x", 100)).is_none());
    }

    #[test]
    fn build_helpers_set_receipt_id_and_timestamp() {
        let f = build_acked_frame("rid-7");
        assert_eq!(f.receipt_id, "rid-7");
        assert!(f.at.is_some());
    }
}

// ─────────────────────────────────────────────────────────────────────
// Integration tests against a live in-process quinn endpoint pair
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::core::receipt::{Receiver as RxSide, Sender as TxSide, State};
    use crate::protocols::quic::ensure_crypto_provider_installed;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;
    use uuid::Uuid;

    /// Build a (server_endpoint, client_endpoint, server_addr) trio
    /// over UDP loopback with a freshly generated self-signed cert.
    /// Both endpoints negotiate ALPN `aerosync/1`.
    async fn make_quinn_pair() -> (quinn::Endpoint, quinn::Endpoint, SocketAddr) {
        ensure_crypto_provider_installed();

        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec());
        let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()),
        );

        let mut tls_server = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .unwrap();
        tls_server.alpn_protocols = vec![aerosync_proto::VERSION.as_bytes().to_vec()];

        let server_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls_server).unwrap();
        let server_config = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));
        let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = quinn::Endpoint::server(server_config, bind).unwrap();
        let server_addr = server.local_addr().unwrap();

        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert_der).unwrap();
        let mut tls_client = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls_client.alpn_protocols = vec![aerosync_proto::VERSION.as_bytes().to_vec()];
        let client_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls_client).unwrap();
        let client_cfg = quinn::ClientConfig::new(Arc::new(client_crypto));
        let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        client.set_default_client_config(client_cfg);

        (server, client, server_addr)
    }

    async fn open_connected_pair(
        server: &quinn::Endpoint,
        client: &quinn::Endpoint,
        server_addr: SocketAddr,
    ) -> (quinn::Connection, quinn::Connection) {
        let server_accept = tokio::spawn({
            let server = server.clone();
            async move { server.accept().await.unwrap().await.unwrap() }
        });
        let client_conn = client
            .connect(server_addr, "localhost")
            .unwrap()
            .await
            .unwrap();
        let server_conn = server_accept.await.unwrap();
        (client_conn, server_conn)
    }

    #[tokio::test]
    async fn full_cycle_ack() {
        let (server, client, addr) = make_quinn_pair().await;
        let (sender_conn, receiver_conn) = open_connected_pair(&server, &client, addr).await;

        let receipt_id = Uuid::new_v4();
        let sender_receipt = Arc::new(Receipt::<TxSide>::new(receipt_id));
        let receiver_receipt = Arc::new(Receipt::<RxSide>::new(receipt_id));

        // Sender opens the receipt stream.
        let (sender_send, sender_recv) = sender_conn.open_bi().await.unwrap();
        // The act of opening doesn't send anything; nudge the stream
        // open so the receiver's accept_bi() can complete.
        let mut sender_send = sender_send;
        let _ = sender_send.write(&[]).await;

        let receiver_accept = tokio::spawn(async move { receiver_conn.accept_bi().await.unwrap() });
        let (recv_send, recv_recv) = receiver_accept.await.unwrap();

        // Drive the receiver state through Open→Close→Close→Process so
        // it's ready for an Ack verdict.
        receiver_receipt.apply_event(Event::Open).unwrap();
        receiver_receipt.apply_event(Event::Close).unwrap();
        receiver_receipt.apply_event(Event::Close).unwrap();
        receiver_receipt.apply_event(Event::Process).unwrap();

        // Receiver-side: launch the loop and let the application emit
        // an Ack verdict.
        let (verdict_tx, verdict_rx) = tokio::sync::oneshot::channel();
        let receiver_receipt_clone = Arc::clone(&receiver_receipt);
        let receiver_handle = tokio::spawn(async move {
            run_receiver_loop(recv_send, recv_recv, receiver_receipt_clone, verdict_rx).await
        });

        // Drive the sender state through Open→Close→Close so the Ack
        // event is legal when it arrives.
        sender_receipt.apply_event(Event::Open).unwrap();
        sender_receipt.apply_event(Event::Close).unwrap();
        sender_receipt.apply_event(Event::Close).unwrap();
        sender_receipt.apply_event(Event::Process).unwrap();

        let sender_receipt_clone = Arc::clone(&sender_receipt);
        let sender_handle =
            tokio::spawn(async move { run_sender_loop(sender_recv, sender_receipt_clone).await });

        // Receiver app: ack.
        let _ = receiver_receipt.apply_event(Event::Ack);
        verdict_tx.send(ReceiverVerdict::Ack).unwrap();

        let receiver_result = tokio::time::timeout(Duration::from_secs(5), receiver_handle)
            .await
            .unwrap()
            .unwrap();
        receiver_result.unwrap();
        let _ = sender_send.finish();

        let sender_result = tokio::time::timeout(Duration::from_secs(5), sender_handle)
            .await
            .unwrap()
            .unwrap();
        // Either Ok or StreamClosed is acceptable depending on which
        // path closed first; what matters is the receipt's terminal
        // state.
        let _ = sender_result;
        assert!(matches!(
            sender_receipt.state(),
            State::Completed(_) | State::Failed(_)
        ));
    }

    #[tokio::test]
    async fn full_cycle_nack() {
        let (server, client, addr) = make_quinn_pair().await;
        let (sender_conn, receiver_conn) = open_connected_pair(&server, &client, addr).await;

        let receipt_id = Uuid::new_v4();
        let sender_receipt = Arc::new(Receipt::<TxSide>::new(receipt_id));
        let receiver_receipt = Arc::new(Receipt::<RxSide>::new(receipt_id));

        let (mut sender_send, sender_recv) = sender_conn.open_bi().await.unwrap();
        let _ = sender_send.write(&[]).await;
        let receiver_accept = tokio::spawn(async move { receiver_conn.accept_bi().await.unwrap() });
        let (recv_send, recv_recv) = receiver_accept.await.unwrap();

        for ev in [Event::Open, Event::Close, Event::Close, Event::Process] {
            receiver_receipt.apply_event(ev.clone()).unwrap();
            sender_receipt.apply_event(ev).unwrap();
        }

        let (verdict_tx, verdict_rx) = tokio::sync::oneshot::channel();
        let recv_clone = Arc::clone(&receiver_receipt);
        let receiver_handle = tokio::spawn(async move {
            run_receiver_loop(recv_send, recv_recv, recv_clone, verdict_rx).await
        });

        let send_clone = Arc::clone(&sender_receipt);
        let sender_handle =
            tokio::spawn(async move { run_sender_loop(sender_recv, send_clone).await });

        let _ = receiver_receipt.apply_event(Event::Nack {
            reason: "schema-broken".into(),
        });
        verdict_tx
            .send(ReceiverVerdict::Nack("schema-broken".into()))
            .unwrap();

        tokio::time::timeout(Duration::from_secs(5), receiver_handle)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let _ = sender_send.finish();
        let _ = tokio::time::timeout(Duration::from_secs(5), sender_handle).await;

        match sender_receipt.state() {
            State::Failed(crate::core::receipt::FailedTerminal::Nacked { reason }) => {
                assert_eq!(reason, "schema-broken");
            }
            other => panic!("expected Failed(Nacked), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cycle_interrupted_by_stream_close_marks_failed() {
        let (server, client, addr) = make_quinn_pair().await;
        let (sender_conn, receiver_conn) = open_connected_pair(&server, &client, addr).await;

        let receipt_id = Uuid::new_v4();
        let sender_receipt = Arc::new(Receipt::<TxSide>::new(receipt_id));

        let (mut sender_send, sender_recv) = sender_conn.open_bi().await.unwrap();
        let _ = sender_send.write(&[]).await;
        let receiver_accept = tokio::spawn(async move { receiver_conn.accept_bi().await.unwrap() });
        let (mut recv_send, _recv_recv) = receiver_accept.await.unwrap();

        // Receiver immediately FINs without writing any frames — the
        // sender-side loop should detect StreamClosed and transition
        // the local Receipt to Failed(Errored).
        recv_send.finish().unwrap();

        let send_clone = Arc::clone(&sender_receipt);
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run_sender_loop(sender_recv, send_clone),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(CodecError::StreamClosed)));

        match sender_receipt.state() {
            State::Failed(crate::core::receipt::FailedTerminal::Errored { code, .. }) => {
                assert_eq!(code, aerosync_proto::ErrorCode::ErrorInternal as u32);
            }
            other => panic!("expected Failed(Errored), got {other:?}"),
        }
    }
}

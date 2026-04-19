//! QUIC metadata propagation — end-to-end integration tests
//! (RFC-002 §6.3 / RFC-003 §8.4 — w0.2.1 batch C "P0.2").
//!
//! These tests close the metadata-propagation gap that batch B left
//! open on the QUIC path. They drive a real [`FileReceiver`] QUIC
//! listener and send a real file from a real [`QuicTransfer`] sender;
//! the received file's `metadata` field must surface byte-equal to
//! what the sender sealed into `TransferTask.metadata`.
//!
//! Scope:
//! 1. **Happy path** — a populated envelope round-trips through
//!    `TransferStart.metadata` on the new bidi control stream and
//!    lands on `ReceivedFile.metadata`.
//! 2. **Backward compat** — a synthetic v0.2.0-style sender that
//!    opens only the legacy data stream (no control stream, no
//!    `receipt_id` in the UPLOAD header) is still accepted; the
//!    resulting `ReceivedFile.metadata` is `None`.

#![cfg(feature = "quic")]

use std::sync::Arc;
use std::time::Duration;

use aerosync::core::server::{FileReceiver, ReceivedFile, ServerConfig};
use aerosync::core::transfer::TransferTask;
use aerosync::protocols::quic::{QuicConfig, QuicTransfer};
use aerosync::protocols::traits::{TransferProgress, TransferProtocol};
use aerosync_proto::{Lifecycle, Metadata};
use tempfile::TempDir;
use tokio::sync::mpsc;

/// Boot a QUIC-enabled `FileReceiver` on `127.0.0.1:0`. Returns the
/// (receiver, quic_addr, save_dir) trio. The temp dir owns the save
/// directory and must live as long as the test body.
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

/// Build a `QuicTransfer` configured to dial `server_addr` in dev
/// (accept-any-cert) mode. ALPN is the canonical `aerosync/1`
/// inherited from `QuicConfig::default()`.
fn build_sender(server_addr: std::net::SocketAddr) -> QuicTransfer {
    let cfg = QuicConfig {
        server_name: "localhost".to_string(),
        server_addr,
        ..QuicConfig::default()
    };
    QuicTransfer::new(cfg).expect("QuicTransfer::new")
}

async fn wait_for_received_file(
    receiver: &FileReceiver,
    deadline: Duration,
) -> Option<ReceivedFile> {
    let stop_at = tokio::time::Instant::now() + deadline;
    loop {
        let files = receiver.get_received_files().await;
        if let Some(f) = files.into_iter().next() {
            return Some(f);
        }
        if tokio::time::Instant::now() >= stop_at {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

// ── 1. Happy path: metadata propagates on the QUIC wire ────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quic_upload_propagates_metadata_envelope_end_to_end() {
    let (receiver, addr, dir) = boot_quic_receiver().await;

    let payload = dir.path().join("payload.bin");
    let body = b"hello metadata over QUIC\n";
    tokio::fs::write(&payload, body).await.unwrap();

    let metadata = Metadata {
        id: uuid::Uuid::new_v4().to_string(),
        from_node: "qa-sender".into(),
        to_node: "qa-receiver".into(),
        size_bytes: body.len() as u64,
        sha256: String::new(),
        file_name: "payload.bin".into(),
        protocol: "quic".into(),
        trace_id: Some("qt-1".into()),
        user_metadata: std::collections::HashMap::from([
            ("agent_id".to_string(), "qa-1".to_string()),
            ("tenant".to_string(), "acme".to_string()),
        ]),
        lifecycle: Some(Lifecycle::Durable as i32),
        ..Default::default()
    };

    let mut task = TransferTask::new_upload(
        payload.clone(),
        format!("quic://{}", addr),
        body.len() as u64,
    );
    task.metadata = Some(metadata.clone());

    let sender = build_sender(addr);
    let (tx, _rx) = mpsc::unbounded_channel::<TransferProgress>();
    sender
        .upload_file(&task, tx)
        .await
        .expect("QUIC upload must succeed");

    let received = wait_for_received_file(&receiver, Duration::from_secs(10))
        .await
        .expect("ReceivedFile must arrive within 10s");

    let got = received
        .metadata
        .as_ref()
        .expect("ReceivedFile.metadata must be populated from TransferStart on QUIC");

    assert_eq!(
        got.trace_id.as_deref(),
        Some("qt-1"),
        "trace_id must round-trip through the bidi control stream"
    );
    assert_eq!(
        got.user_metadata.get("agent_id").map(String::as_str),
        Some("qa-1")
    );
    assert_eq!(
        got.user_metadata.get("tenant").map(String::as_str),
        Some("acme")
    );
    assert_eq!(got.lifecycle, Some(Lifecycle::Durable as i32));
    assert_eq!(got.from_node, "qa-sender");

    drop(receiver);
}

// ── 2. Backward compatibility: a v0.2.0-style sender with no control
//      stream and no receipt_id in the UPLOAD header is still accepted
//      and the receiver leaves ReceivedFile.metadata as None. ─────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quic_legacy_v020_sender_accepted_with_metadata_none() {
    use quinn::Endpoint;
    use rustls::pki_types::CertificateDer;
    use rustls::ClientConfig as TlsClientConfig;
    use std::sync::Arc as StdArc;

    let (receiver, addr, dir) = boot_quic_receiver().await;

    // Build a minimal "legacy" QUIC client that mirrors what v0.2.0
    // wrote on the wire: ONE bidi stream per transfer, opened with
    // `UPLOAD:<filename>:<size>\n<data>` (no token, no receipt_id, no
    // control stream). The `crate::core::tls` installer is
    // `pub(crate)` and unreachable from `tests/`, so we install the
    // ring provider directly here — it is a no-op when the receiver's
    // `start()` already installed one.
    let _ = rustls::crypto::ring::default_provider().install_default();

    #[derive(Debug)]
    struct AnyCert;
    impl rustls::client::danger::ServerCertVerifier for AnyCert {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &rustls::pki_types::ServerName<'_>,
            _: &[u8],
            _: rustls::pki_types::UnixTime,
        ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error>
        {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
        {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
        {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            vec![
                rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
                rustls::SignatureScheme::ED25519,
                rustls::SignatureScheme::RSA_PKCS1_SHA256,
            ]
        }
    }

    let mut tls_client = TlsClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(StdArc::new(AnyCert))
        .with_no_client_auth();
    tls_client.alpn_protocols = vec![aerosync_proto::VERSION.as_bytes().to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls_client).unwrap();
    let client_cfg = quinn::ClientConfig::new(StdArc::new(crypto));
    let mut endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(client_cfg);

    let connection = endpoint
        .connect(addr, "localhost")
        .unwrap()
        .await
        .expect("legacy QUIC connect");

    let body = b"legacy v0.2.0 payload";
    let payload = dir.path().join("legacy.bin");
    tokio::fs::write(&payload, body).await.unwrap();

    let (mut send, mut recv) = connection.open_bi().await.expect("open_bi data");
    // EXACT v0.2.0 header shape — no token, no receipt_id.
    let header = format!("UPLOAD:legacy.bin:{}\n", body.len());
    send.write_all(header.as_bytes()).await.unwrap();
    send.write_all(body).await.unwrap();
    send.finish().expect("finish");

    // Drain the SUCCESS reply so the receiver finalises the file.
    let mut reply = vec![0u8; 64];
    let _ = tokio::time::timeout(Duration::from_secs(5), recv.read(&mut reply)).await;

    let received = wait_for_received_file(&receiver, Duration::from_secs(10))
        .await
        .expect("legacy QUIC upload must still produce a ReceivedFile");

    assert_eq!(received.original_name, "legacy.bin");
    assert_eq!(received.size, body.len() as u64);
    assert!(
        received.metadata.is_none(),
        "v0.2.0-style transfers (no control stream) must leave \
         ReceivedFile.metadata = None — got {:?}",
        received.metadata
    );

    // Hold these so they outlive the connection close above.
    drop(connection);
    drop(endpoint);
    drop(receiver);
    let _ = Arc::new(()); // keep the use of std::sync::Arc explicit; not strictly required
}

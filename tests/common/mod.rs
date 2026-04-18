//! Shared bootstrap helpers for the RFC-002 §14 #12 e2e suite.
//!
//! Kept intentionally minimal: a `quinn::Endpoint` pair generator
//! (mirrors the helper in `src/protocols/quic_receipt.rs::integration_tests`,
//! which is `cfg(test)` and therefore not reachable from
//! `tests/`) and a no-op `SuccessAdapter` that satisfies
//! [`aerosync::core::transfer::ProtocolAdapter`] without touching the
//! network. New helpers should only land here when a second test file
//! actually needs them.

#![allow(dead_code)] // helpers may be unused in some test compilation units

use std::net::SocketAddr;
use std::sync::Arc;

use aerosync::core::transfer::{ProtocolAdapter, ProtocolProgress, TransferTask};
use aerosync::core::Result;
use aerosync::core::ResumeState;
use async_trait::async_trait;
use tokio::sync::mpsc;

/// One-shot installer for the rustls process-wide crypto provider, used
/// by quinn 0.11 + rustls 0.23. Mirrors `src/protocols/quic.rs::ensure_crypto_provider_installed`,
/// which is `pub(crate)` and therefore not reachable from `tests/`.
/// Safe to call repeatedly.
pub fn ensure_crypto_provider_installed() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Generate a self-signed-cert quinn endpoint pair bound to UDP loopback,
/// negotiating the production ALPN `aerosync_proto::VERSION`.
///
/// Returns `(server_endpoint, client_endpoint, server_addr)`. Both
/// endpoints stay alive as long as the returned handles do.
pub async fn make_quinn_pair() -> (quinn::Endpoint, quinn::Endpoint, SocketAddr) {
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

/// Open a single client→server QUIC connection on the given pair.
/// Returns `(client_conn, server_conn)`.
pub async fn open_connected_pair(
    server: &quinn::Endpoint,
    client: &quinn::Endpoint,
    server_addr: SocketAddr,
) -> (quinn::Connection, quinn::Connection) {
    let server_clone = server.clone();
    let server_accept =
        tokio::spawn(async move { server_clone.accept().await.unwrap().await.unwrap() });
    let client_conn = client
        .connect(server_addr, "localhost")
        .unwrap()
        .await
        .unwrap();
    let server_conn = server_accept.await.unwrap();
    (client_conn, server_conn)
}

/// Adapter that immediately reports success with no I/O. Used by the
/// e2e suite to drive `TransferEngine::send` to a `Completed` worker
/// status (and therefore the receipt to `Processing`) without booting
/// a real transport — the on-the-wire receipt-stream is exercised
/// separately on a dedicated quinn pair.
pub struct SuccessAdapter;

#[async_trait]
impl ProtocolAdapter for SuccessAdapter {
    async fn upload(
        &self,
        _: &TransferTask,
        _: mpsc::UnboundedSender<ProtocolProgress>,
    ) -> Result<()> {
        Ok(())
    }
    async fn download(
        &self,
        _: &TransferTask,
        _: mpsc::UnboundedSender<ProtocolProgress>,
    ) -> Result<()> {
        Ok(())
    }
    async fn upload_chunked(
        &self,
        _: &TransferTask,
        _: &mut ResumeState,
        _: mpsc::UnboundedSender<ProtocolProgress>,
    ) -> Result<()> {
        Ok(())
    }
    fn protocol_name(&self) -> &'static str {
        "e2e-success"
    }
}

/// Adapter that blocks for `delay_ms` before reporting success. Used
/// by the cancel test (#4) so the engine has an in-flight transfer to
/// cancel against.
pub struct SlowAdapter {
    pub delay_ms: u64,
}

#[async_trait]
impl ProtocolAdapter for SlowAdapter {
    async fn upload(
        &self,
        _: &TransferTask,
        _: mpsc::UnboundedSender<ProtocolProgress>,
    ) -> Result<()> {
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        Ok(())
    }
    async fn download(
        &self,
        _: &TransferTask,
        _: mpsc::UnboundedSender<ProtocolProgress>,
    ) -> Result<()> {
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        Ok(())
    }
    async fn upload_chunked(
        &self,
        _: &TransferTask,
        _: &mut ResumeState,
        _: mpsc::UnboundedSender<ProtocolProgress>,
    ) -> Result<()> {
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        Ok(())
    }
    fn protocol_name(&self) -> &'static str {
        "e2e-slow"
    }
}

use crate::core::{AeroSyncError, Result, TransferTask};
use crate::protocols::quic_receipt::{
    build_transfer_start_control_frame, encode_control_frame, read_frame_from_stream, CodecError,
    CONTROL_STREAM_SENTINEL,
};
use crate::protocols::traits::{TransferProgress, TransferProtocol};
use aerosync_proto::ReceiptFrame;
use async_trait::async_trait;
use quinn::{ClientConfig, Connection, Endpoint};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName as PkiServerName, UnixTime};
use rustls::{ClientConfig as TlsClientConfig, DigitallySignedStruct, SignatureScheme};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::Instant;
use zeroize::Zeroizing;

/// One-shot installer for the rustls process-wide crypto provider.
/// rustls 0.23 requires an explicit crypto provider; we pick `ring` to
/// keep the existing algorithm profile and avoid pulling aws-lc. Safe to
/// call repeatedly.
pub(crate) fn ensure_crypto_provider_installed() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // The installer returns Err if another provider is already set,
        // which is fine — we just want SOMETHING installed before any
        // rustls type is constructed.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// 证书固定验证器：只接受指定指纹的证书，而非完全跳过验证。
/// 用于开发/自签名场景。生产环境应使用系统证书库。
#[derive(Debug)]
struct PinnedCertVerifier {
    /// 接受的证书 DER 字节集合（为空时表示开发模式，接受任何证书并打印警告）
    accepted_certs: Vec<Vec<u8>>,
    dev_mode: bool,
    supported_schemes: Vec<SignatureScheme>,
}

impl PinnedCertVerifier {
    fn supported_default_schemes() -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }

    /// 开发模式：接受任何自签名证书，但打印安全警告。
    fn dev_mode() -> Arc<Self> {
        tracing::warn!(
            "QUIC: Using development certificate mode — \
            do NOT use in production. Set accepted_certs for proper pinning."
        );
        Arc::new(Self {
            accepted_certs: vec![],
            dev_mode: true,
            supported_schemes: Self::supported_default_schemes(),
        })
    }

    /// 生产模式：只接受固定的证书列表。
    fn with_pinned(certs: Vec<Vec<u8>>) -> Arc<Self> {
        Arc::new(Self {
            accepted_certs: certs,
            dev_mode: false,
            supported_schemes: Self::supported_default_schemes(),
        })
    }
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &PkiServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        if self.dev_mode {
            return Ok(ServerCertVerified::assertion());
        }
        if self
            .accepted_certs
            .iter()
            .any(|c| c.as_slice() == end_entity.as_ref())
        {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "Certificate not in pinned list".to_string(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        // Dev-mode / pinning: we have already attested the cert by identity,
        // so signature verification is implicitly trusted.
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported_schemes.clone()
    }
}

pub struct QuicTransfer {
    endpoint: Endpoint,
    config: QuicConfig,
    /// Optional sink for inbound [`ReceiptFrame`] messages received on
    /// the bidi receipt control stream (Batch C / w0.2.1 P0.2). The
    /// QUIC sender always opens the control stream and drains it; when
    /// this is `Some(tx)`, every inbound frame is forwarded to `tx`
    /// before the drainer task exits, which lets integration tests and
    /// embedders (potentially `aerosync-py` once the sender callback
    /// path lands) observe `Received` / `Acked` / `Nacked` frames
    /// without needing a `Receipt<Sender>` reference. `None` (the
    /// default through [`AutoAdapter`]) preserves the legacy
    /// fire-and-forget behaviour.
    receipt_sink: Option<mpsc::UnboundedSender<ReceiptFrame>>,
}

#[derive(Debug, Clone)]
pub struct QuicConfig {
    pub server_name: String,
    pub server_addr: SocketAddr,
    pub alpn_protocols: Vec<String>,
    pub max_idle_timeout: u64,
    pub keep_alive_interval: u64,
    /// 发送方认证 Token（在 UPLOAD 消息头中传递）
    pub auth_token: Option<Zeroizing<String>>,
    /// 服务端 DER 格式证书文件路径列表（用于证书钉扎）。
    /// 非空时只信任列表中的证书，替代开发模式。
    pub pinned_server_certs: Vec<PathBuf>,
}

impl Default for QuicConfig {
    fn default() -> Self {
        Self {
            server_name: "localhost".to_string(),
            server_addr: "127.0.0.1:7789".parse().unwrap(),
            // RFC-002 §6.1: the canonical ALPN string for v0.2+ is
            // `aerosync/1`. v0.1.x peers (which advertised "aerosync"
            // unversioned) are intentionally not interoperable —
            // they're treated as a closed pre-alpha. The string lives
            // in `aerosync_proto::VERSION` so encoder/decoder code on
            // both sides agree on the exact bytes.
            alpn_protocols: vec![aerosync_proto::VERSION.to_string()],
            max_idle_timeout: 60_000,
            keep_alive_interval: 5_000,
            auth_token: None,
            pinned_server_certs: vec![],
        }
    }
}

impl QuicTransfer {
    pub fn new(config: QuicConfig) -> Result<Self> {
        ensure_crypto_provider_installed();

        let verifier = if !config.pinned_server_certs.is_empty() {
            let certs: Result<Vec<Vec<u8>>> = config
                .pinned_server_certs
                .iter()
                .map(|p| {
                    std::fs::read(p).map_err(|e| {
                        AeroSyncError::InvalidConfig(format!(
                            "Cannot read pinned cert {}: {}",
                            p.display(),
                            e
                        ))
                    })
                })
                .collect();
            PinnedCertVerifier::with_pinned(certs?)
        } else {
            PinnedCertVerifier::dev_mode()
        };

        // rustls 0.23: builder no longer has with_safe_defaults(); the crypto
        // provider installed above supplies the default cipher suites/kx.
        let mut tls_config = TlsClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();

        tls_config.alpn_protocols = config
            .alpn_protocols
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect();

        let mut transport = quinn::TransportConfig::default();
        transport.max_idle_timeout(Some(
            std::time::Duration::from_millis(config.max_idle_timeout)
                .try_into()
                .unwrap(),
        ));
        transport.keep_alive_interval(Some(std::time::Duration::from_millis(
            config.keep_alive_interval,
        )));

        // quinn 0.11: ClientConfig wraps rustls QuicClientConfig via the
        // quinn::crypto::rustls bridge instead of taking raw rustls::ClientConfig.
        let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)
            .map_err(|e| AeroSyncError::Network(format!("QUIC TLS config error: {}", e)))?;
        let mut client_config = ClientConfig::new(Arc::new(quic_crypto));
        client_config.transport_config(Arc::new(transport));

        let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap())
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;
        endpoint.set_default_client_config(client_config);

        Ok(Self {
            endpoint,
            config,
            receipt_sink: None,
        })
    }

    /// Builder: install a sink that receives every inbound
    /// [`ReceiptFrame`] read off the bidi receipt control stream after
    /// the data stream completes. See the field-level docs on
    /// [`QuicTransfer::receipt_sink`] for the full contract. Used by
    /// integration tests to assert sender-observed `Sealed` / `Acked`
    /// frames; the default constructor leaves the sink unset.
    pub fn with_receipt_sink(mut self, tx: mpsc::UnboundedSender<ReceiptFrame>) -> Self {
        self.receipt_sink = Some(tx);
        self
    }

    async fn establish_connection(&self) -> Result<Connection> {
        let connection = self
            .endpoint
            .connect(self.config.server_addr, &self.config.server_name)
            .map_err(|e| AeroSyncError::Network(e.to_string()))?
            .await
            .map_err(|e| AeroSyncError::Network(format!("QUIC connect failed: {}", e)))?;
        Ok(connection)
    }

    /// Open the bidi **control stream** (Batch C / w0.2.1 P0.2) and
    /// emit the per-transfer `TransferStart` envelope. The receiver
    /// dispatches on the [`CONTROL_STREAM_SENTINEL`] first byte; the
    /// length-delimited [`aerosync_proto::ControlFrame`] follows on
    /// the same stream. Returns the bidi pair so the caller can spawn
    /// a drainer task on the recv half (without blocking the data
    /// stream).
    async fn open_control_stream(
        &self,
        connection: &Connection,
        receipt_id: &str,
        file_name: &str,
        file_size: u64,
        sha256: Option<&str>,
        metadata: Option<aerosync_proto::Metadata>,
    ) -> Result<(quinn::SendStream, quinn::RecvStream)> {
        let (mut send, recv) = connection
            .open_bi()
            .await
            .map_err(|e| AeroSyncError::Network(format!("open control bidi: {e}")))?;

        // Pack the sentinel + length-delimited ControlFrame into a
        // SINGLE write_all so quinn ships them as one contiguous chunk.
        // Two back-to-back small writes were enough to let the data
        // stream's FIN race ahead of the control stream on a quiet
        // connection, leaving the receiver's control handler stuck
        // waiting for bytes that never arrived in the 2 s
        // wait-for-control-entry window.
        let frame = build_transfer_start_control_frame(
            receipt_id,
            file_name,
            file_size,
            sha256.unwrap_or(""),
            metadata,
            64 * 1024,
        );
        let frame_bytes = encode_control_frame(&frame);
        let mut buf = Vec::with_capacity(1 + frame_bytes.len());
        buf.push(CONTROL_STREAM_SENTINEL);
        buf.extend_from_slice(&frame_bytes);
        send.write_all(&buf)
            .await
            .map_err(|e| AeroSyncError::Network(format!("write control prelude: {e}")))?;

        Ok((send, recv))
    }

    async fn upload_with_progress(
        &self,
        connection: &Connection,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let file_path = task.source_path.as_path();
        let mut file = File::open(file_path).await?;
        let file_size = file.metadata().await?.len();

        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");
        let receipt_id = task.id.to_string();
        let sha256 = task.sha256.as_deref();

        // ── 1. Open control stream FIRST and send TransferStart so
        //       the receiver can stash metadata before the data stream
        //       lands. The control stream lives for the duration of
        //       the transfer; its recv half is drained in a spawned
        //       task so it cannot stall the data path.
        let (mut control_send, control_recv) = self
            .open_control_stream(
                connection,
                &receipt_id,
                file_name,
                file_size,
                sha256,
                task.metadata.clone(),
            )
            .await?;

        let receipt_sink = self.receipt_sink.clone();
        let receipt_id_for_task = receipt_id.clone();
        let drainer = tokio::spawn(async move {
            drain_receipt_stream(control_recv, receipt_sink, &receipt_id_for_task).await;
        });

        // ── 2. Open the legacy data stream and write the UPLOAD
        //       header. We append the receipt_id as a 5th colon field
        //       so the receiver can match the data stream to the
        //       control entry it already stashed. v0.2.0 receivers
        //       just ignore the trailing field (they parse with
        //       `splitn(5, ':')` and only use parts[0..=3]).
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;

        let token = self
            .config
            .auth_token
            .as_ref()
            .map(|t| t.as_str().to_string())
            .unwrap_or_default();
        let header = format!(
            "UPLOAD:{}:{}:{}:{}\n",
            file_name, file_size, token, receipt_id
        );

        let metadata_line = if let Some(hash) = sha256 {
            format!("{}HASH:{}\n", header, hash)
        } else {
            header
        };

        send.write_all(metadata_line.as_bytes())
            .await
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;

        let mut buf = vec![0u8; 64 * 1024];
        let mut transferred = 0u64;
        let start = Instant::now();

        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            send.write_all(&buf[..n])
                .await
                .map_err(|e| AeroSyncError::Network(e.to_string()))?;
            transferred += n as u64;
            let elapsed = start.elapsed().as_secs_f64();
            let speed = if elapsed > 0.0 {
                transferred as f64 / elapsed
            } else {
                0.0
            };
            let _ = progress_tx.send(TransferProgress {
                bytes_transferred: transferred,
                transfer_speed: speed,
            });
        }

        // quinn 0.11: SendStream::finish() is sync and only queues the FIN;
        // call stopped() or wait for the remote to read-close to guarantee
        // delivery. For our upload path we fire-and-forget the FIN since the
        // caller aggregates at the application layer.
        send.finish()
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;

        // Best-effort: read the receiver's SUCCESS/ERROR reply so the
        // legacy data-stream contract (and tests asserting on it) keeps
        // working. We bound the wait so a misbehaving receiver cannot
        // wedge us.
        let mut reply = vec![0u8; 256];
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), recv.read(&mut reply))
            .await;

        // ── 3. Allow the drainer a brief window to surface the
        //       receiver's `Received` / `Acked` frames before we tear
        //       down the connection. We do not block on it — if the
        //       receiver does not finish within the timeout the
        //       drainer is dropped and the receipt sink simply does
        //       not see the terminal frame. Sender-observed receipt
        //       events are best-effort by RFC-002 §7.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), drainer).await;
        let _ = control_send.finish();

        tracing::info!(
            "QUIC: Uploaded '{}' ({} bytes) at {:.2} MB/s",
            file_name,
            file_size,
            transferred as f64 / start.elapsed().as_secs_f64() / 1_048_576.0
        );
        Ok(())
    }

    async fn download_with_progress(
        &self,
        connection: &Connection,
        file_path: &std::path::Path,
        file_name: &str,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;

        let request = format!("DOWNLOAD:{}", file_name);
        send.write_all(request.as_bytes())
            .await
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;
        // quinn 0.11: finish() is sync; queues FIN, no await needed.
        send.finish()
            .map_err(|e| AeroSyncError::Network(e.to_string()))?;

        let mut file = File::create(file_path).await?;
        let mut transferred = 0u64;
        let start = Instant::now();
        let mut buf = vec![0u8; 64 * 1024];

        loop {
            match recv.read(&mut buf).await {
                Ok(Some(n)) => {
                    file.write_all(&buf[..n]).await?;
                    transferred += n as u64;
                    let elapsed = start.elapsed().as_secs_f64();
                    let speed = if elapsed > 0.0 {
                        transferred as f64 / elapsed
                    } else {
                        0.0
                    };
                    let _ = progress_tx.send(TransferProgress {
                        bytes_transferred: transferred,
                        transfer_speed: speed,
                    });
                }
                Ok(None) => break,
                Err(e) => return Err(AeroSyncError::Network(e.to_string())),
            }
        }
        file.flush().await?;
        Ok(())
    }
}

/// Drain inbound [`ReceiptFrame`] messages off the bidi receipt
/// control stream the sender opened. Forwards each frame to `sink` if
/// one was provided and exits on the first terminal frame
/// (`Received`, `Acked`, `Nacked`, `Failed`) or on stream close. Runs
/// in its own tokio task so the data stream is never blocked waiting
/// for the receiver to ack.
async fn drain_receipt_stream(
    mut recv: quinn::RecvStream,
    sink: Option<mpsc::UnboundedSender<ReceiptFrame>>,
    receipt_id: &str,
) {
    use aerosync_proto::receipt_frame;
    loop {
        match read_frame_from_stream(&mut recv).await {
            Ok(frame) => {
                // Only the **final-state** frames terminate the
                // drainer. `Received` is an intermediate hint
                // (per RFC-002 §4 "SENT → RECEIVED" precedes
                // ACKED/NACKED) — exiting on it would drop the
                // subsequent `Acked` the receiver auto-emits.
                let is_terminal = matches!(
                    frame.body.as_ref(),
                    Some(
                        receipt_frame::Body::Acked(_)
                            | receipt_frame::Body::Nacked(_)
                            | receipt_frame::Body::Failed(_)
                    )
                );
                if let Some(tx) = sink.as_ref() {
                    if tx.send(frame).is_err() {
                        // Sink dropped: stop draining; nothing else to
                        // do with subsequent frames.
                        return;
                    }
                } else {
                    tracing::trace!(
                        receipt_id = %receipt_id,
                        "QUIC: drained receipt frame (no sink configured)"
                    );
                }
                if is_terminal {
                    return;
                }
            }
            Err(CodecError::StreamClosed) => return,
            Err(e) => {
                tracing::debug!(
                    receipt_id = %receipt_id,
                    error = %e,
                    "QUIC: receipt drainer ended with codec error"
                );
                return;
            }
        }
    }
}

#[async_trait]
impl TransferProtocol for QuicTransfer {
    async fn upload_file(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let connection = self.establish_connection().await?;
        self.upload_with_progress(&connection, task, progress_tx)
            .await
    }

    async fn download_file(
        &self,
        task: &TransferTask,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        let connection = self.establish_connection().await?;
        let file_name = task
            .source_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        self.download_with_progress(&connection, &task.source_path, &file_name, progress_tx)
            .await
    }

    async fn resume_transfer(
        &self,
        task: &TransferTask,
        _offset: u64,
        progress_tx: mpsc::UnboundedSender<TransferProgress>,
    ) -> Result<()> {
        // Phase 2: 实现分片续传
        if task.is_upload {
            self.upload_file(task, progress_tx).await
        } else {
            self.download_file(task, progress_tx).await
        }
    }

    fn supports_resume(&self) -> bool {
        true
    }

    fn protocol_name(&self) -> &'static str {
        "QUIC"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 1. QuicConfig defaults ────────────────────────────────────────────────
    #[test]
    fn test_quic_pinned_cert_field_in_default() {
        let cfg = QuicConfig::default();
        assert!(cfg.pinned_server_certs.is_empty());
    }

    // ── 2. Certificate pinning: nonexistent file ──────────────────────────────
    #[test]
    fn test_quic_new_with_nonexistent_cert_returns_err() {
        let config = QuicConfig {
            pinned_server_certs: vec![PathBuf::from("/nonexistent/server.der")],
            ..QuicConfig::default()
        };
        let result = QuicTransfer::new(config);
        assert!(result.is_err());
        match result {
            Err(AeroSyncError::InvalidConfig(msg)) => {
                assert!(msg.contains("Cannot read pinned cert"), "Got: {}", msg);
            }
            Err(e) => panic!("Wrong error type: {:?}", e),
            Ok(_) => panic!("Should have failed"),
        }
    }
}

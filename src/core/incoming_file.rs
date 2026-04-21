//! Receiver-side wrapper around a [`ReceivedFile`] that carries the
//! associated [`Receipt<Receiver>`] (when the sender advertised
//! `SUPPORTS_RECEIPTS` in its `HandshakeFrame`) and exposes the
//! application-facing `ack` / `nack` / `into_receipt` API mandated by
//! RFC-002 §6.4.
//!
//! # Wire-protocol scope
//!
//! `ack` / `nack` apply the receiver-side state-machine event
//! ([`Event::Ack`] / [`Event::Nack`]) on the local [`Receipt`]. Wiring
//! the resulting [`aerosync_proto::ReceiptFrame`] over
//! the QUIC receipt stream is performed by the QUIC transport layer
//! once it is connected to the per-receiver [`ReceiptRegistry`]; this
//! struct does **not** itself call into the transport. That separation
//! keeps the application API testable without a live QUIC endpoint and
//! mirrors the way [`crate::core::transfer::TransferEngine::send`]
//! abstracts away the transport.
//!
//! # Pre-receipt-protocol senders
//!
//! When `IncomingFile::new_without_receipt` is used (the normal path
//! today, since handshake feature negotiation is still landing),
//! `receipt_id()` returns `None` and `ack()` / `nack()` are silent
//! no-ops that emit a `tracing::debug!` to make the downgrade visible.
//! Callers can therefore use the same code path on both legacy and
//! receipt-aware peers.
//!
//! # Idempotency
//!
//! Once a receipt has reached a terminal state, subsequent `ack` /
//! `nack` calls return `Ok(())` without re-applying the event. This
//! matches the wire-side idempotency provided by
//! [`crate::core::receipts_http::IdempotencyCache`] and the spec text
//! "second call returns same result as first".

use std::sync::Arc;

use aerosync_proto::Metadata;
use serde_json::Value;
use uuid::Uuid;

use crate::core::metadata::empty_metadata;
use crate::core::receipt::{Event, Receipt, Receiver};
use crate::core::receipt_registry::ReceiptRegistry;
use crate::core::server::ReceivedFile;
use crate::core::Result;

/// Receiver-side handle to a freshly-received file plus its optional
/// Receipt. See module-level docs for the full contract.
#[derive(Clone)]
pub struct IncomingFile {
    /// Underlying received-file metadata. Re-exported via
    /// `into_inner` and accessible directly as a public field for
    /// ergonomic destructuring in callers.
    pub received: ReceivedFile,
    receipt: Option<Arc<Receipt<Receiver>>>,
    /// Held so a successful `ack`/`nack` can drop the receipt from
    /// the registry once terminal — keeps long-running receivers
    /// from leaking. `None` when no receipt is attached.
    registry: Option<Arc<ReceiptRegistry<Receiver>>>,
    /// RFC-003 metadata envelope for this transfer. Defaults to an
    /// empty [`Metadata`] for legacy senders that pre-date Week 4 or
    /// for code paths where the wire-side parse has not happened
    /// yet. Populated by the QUIC adapter via
    /// [`Self::with_metadata`] before the file is handed to the
    /// application stream.
    metadata: Metadata,
}

impl std::fmt::Debug for IncomingFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IncomingFile")
            .field("received", &self.received)
            .field("receipt_id", &self.receipt_id())
            .field("has_registry", &self.registry.is_some())
            .finish()
    }
}

impl IncomingFile {
    /// Construct from a [`ReceivedFile`] + a registered
    /// [`Receipt<Receiver>`] when the sender opted in to RFC-002.
    pub fn new(
        received: ReceivedFile,
        receipt: Arc<Receipt<Receiver>>,
        registry: Arc<ReceiptRegistry<Receiver>>,
    ) -> Self {
        Self {
            received,
            receipt: Some(receipt),
            registry: Some(registry),
            metadata: empty_metadata(),
        }
    }

    /// Construct without a receipt, for legacy senders that do not
    /// advertise `SUPPORTS_RECEIPTS` in their handshake. `ack`/`nack`
    /// then silently no-op as documented.
    pub fn new_without_receipt(received: ReceivedFile) -> Self {
        Self {
            received,
            receipt: None,
            registry: None,
            metadata: empty_metadata(),
        }
    }

    /// Builder: attach the RFC-003 [`Metadata`] envelope decoded from
    /// the sender's `TransferStart`. The QUIC adapter calls this
    /// once, before the file is yielded on the receiver stream.
    pub fn with_metadata(mut self, metadata: Metadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Borrow the RFC-003 metadata envelope. Returns the empty
    /// default for legacy senders that did not include a Metadata
    /// frame.
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// Receipt id when the transfer carried a [`Receipt`], else
    /// `None`. The id is the canonical key for `wait_receipt` /
    /// `cancel_receipt` and for joining against
    /// [`crate::core::history::HistoryEntry::receipt_id`].
    pub fn receipt_id(&self) -> Option<Uuid> {
        self.receipt.as_ref().map(|r| r.id())
    }

    /// Borrow the underlying receipt, if any.
    pub fn receipt(&self) -> Option<&Arc<Receipt<Receiver>>> {
        self.receipt.as_ref()
    }

    /// Consume `self`, returning the inner [`Arc<Receipt<Receiver>>`]
    /// so the caller can hold the receipt past the lifetime of this
    /// `IncomingFile`. Returns `None` for legacy peers.
    pub fn into_receipt(self) -> Option<Arc<Receipt<Receiver>>> {
        self.receipt
    }

    /// Take the underlying [`ReceivedFile`] by value, dropping the
    /// receipt handle. Useful for callers that want the legacy struct
    /// after they've already issued the ack/nack.
    pub fn into_inner(self) -> ReceivedFile {
        self.received
    }

    /// Acknowledge the payload. Drives the receipt state machine
    /// `Processing → Completed(Acked)` and (in a future revision)
    /// writes a `ReceiptFrame{ack:{}}` over the QUIC receipt stream.
    ///
    /// No-op for legacy peers (`receipt_id() == None`).
    /// Idempotent: returns `Ok(())` when already terminal.
    #[tracing::instrument(skip(self), fields(receipt_id = ?self.receipt_id()))]
    pub async fn ack(&self) -> Result<()> {
        self.ack_inner(None).await
    }

    /// Acknowledge with an application-supplied JSON metadata payload.
    /// The metadata is logged at INFO; the wire-level
    /// `AckFrame.metadata` slot will carry it once the QUIC stream
    /// wiring lands.
    ///
    /// Same no-op / idempotency semantics as [`Self::ack`].
    #[tracing::instrument(skip(self, metadata), fields(receipt_id = ?self.receipt_id()))]
    pub async fn ack_with_metadata(&self, metadata: Value) -> Result<()> {
        self.ack_inner(Some(metadata)).await
    }

    async fn ack_inner(&self, metadata: Option<Value>) -> Result<()> {
        let Some(receipt) = &self.receipt else {
            tracing::debug!("ack(): no receipt attached (legacy sender), silently no-op");
            return Ok(());
        };

        if receipt.state().is_terminal() {
            tracing::debug!(
                receipt_id = %receipt.id(),
                state = ?receipt.state(),
                "ack(): receipt already terminal, idempotent no-op"
            );
            return Ok(());
        }

        // Drive any outstanding pre-Process transitions so callers
        // can ack on a freshly-handed-off file without first walking
        // the state machine themselves. apply_event is no-op when
        // already past that state.
        let _ = receipt.apply_event(Event::Open);
        let _ = receipt.apply_event(Event::Close);
        let _ = receipt.apply_event(Event::Close);
        let _ = receipt.apply_event(Event::Process);

        match receipt.apply_event(Event::Ack) {
            Ok(_) => {
                if let Some(meta) = &metadata {
                    tracing::info!(
                        receipt_id = %receipt.id(),
                        metadata = %meta,
                        "ack(): receipt acked with metadata"
                    );
                }
                if let Some(reg) = &self.registry {
                    reg.remove(receipt.id());
                }
                Ok(())
            }
            Err(_) if receipt.state().is_terminal() => {
                // Race: another caller drove the receipt terminal
                // between our state() check and apply_event. Treat
                // as idempotent success.
                Ok(())
            }
            Err(e) => Err(crate::AeroSyncError::System(format!(
                "ack(): receipt rejected event: {e}"
            ))),
        }
    }

    /// Reject the payload with a reason string. Drives the receipt
    /// state machine to `Failed(Nacked{reason})`.
    /// No-op / idempotent under the same rules as [`Self::ack`].
    pub async fn nack(&self, reason: impl Into<String>) -> Result<()> {
        let reason: String = reason.into();
        self.nack_inner(reason).await
    }

    #[tracing::instrument(skip(self), fields(receipt_id = ?self.receipt_id()))]
    async fn nack_inner(&self, reason: String) -> Result<()> {
        let Some(receipt) = &self.receipt else {
            tracing::debug!(
                reason = %reason,
                "nack(): no receipt attached (legacy sender), silently no-op"
            );
            return Ok(());
        };

        if receipt.state().is_terminal() {
            tracing::debug!(
                receipt_id = %receipt.id(),
                state = ?receipt.state(),
                "nack(): receipt already terminal, idempotent no-op"
            );
            return Ok(());
        }

        let _ = receipt.apply_event(Event::Open);
        let _ = receipt.apply_event(Event::Close);
        let _ = receipt.apply_event(Event::Close);
        let _ = receipt.apply_event(Event::Process);

        match receipt.apply_event(Event::Nack {
            reason: reason.clone(),
        }) {
            Ok(_) => {
                if let Some(reg) = &self.registry {
                    reg.remove(receipt.id());
                }
                Ok(())
            }
            Err(_) if receipt.state().is_terminal() => Ok(()),
            Err(e) => Err(crate::AeroSyncError::System(format!(
                "nack(): receipt rejected event: {e}"
            ))),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::receipt::{CompletedTerminal, FailedTerminal, State};
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn dummy_received() -> ReceivedFile {
        ReceivedFile {
            id: Uuid::new_v4(),
            original_name: "f.bin".to_string(),
            saved_path: PathBuf::from("/tmp/f.bin"),
            size: 42,
            sha256: None,
            received_at: SystemTime::now(),
            sender_ip: None,
            metadata: None,
        }
    }

    fn fresh_incoming() -> (
        IncomingFile,
        Arc<Receipt<Receiver>>,
        Arc<ReceiptRegistry<Receiver>>,
    ) {
        let receipt = Arc::new(Receipt::<Receiver>::new(Uuid::new_v4()));
        let registry = Arc::new(ReceiptRegistry::<Receiver>::new());
        registry.insert(Arc::clone(&receipt));
        let f = IncomingFile::new(
            dummy_received(),
            Arc::clone(&receipt),
            Arc::clone(&registry),
        );
        (f, receipt, registry)
    }

    #[tokio::test]
    async fn test_ack_drives_receipt_to_acked() {
        let (f, r, reg) = fresh_incoming();
        f.ack().await.unwrap();
        assert!(matches!(
            r.state(),
            State::Completed(CompletedTerminal::Acked)
        ));
        // Registry GC: terminal entry was removed.
        assert!(reg.get(r.id()).is_none());
    }

    #[tokio::test]
    async fn test_nack_drives_receipt_to_nacked_with_reason() {
        let (f, r, _reg) = fresh_incoming();
        f.nack("checksum mismatch").await.unwrap();
        match r.state() {
            State::Failed(FailedTerminal::Nacked { reason }) => {
                assert_eq!(reason, "checksum mismatch");
            }
            other => panic!("expected Nacked, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_ack_with_metadata_drives_to_acked() {
        let (f, r, _reg) = fresh_incoming();
        let meta = serde_json::json!({"file_id": "abc", "checked": true});
        f.ack_with_metadata(meta).await.unwrap();
        assert!(matches!(
            r.state(),
            State::Completed(CompletedTerminal::Acked)
        ));
    }

    #[tokio::test]
    async fn test_double_ack_is_idempotent() {
        let (f, r, _reg) = fresh_incoming();
        f.ack().await.unwrap();
        // Second call must succeed with no error and no state change.
        f.ack().await.unwrap();
        assert!(matches!(
            r.state(),
            State::Completed(CompletedTerminal::Acked)
        ));
    }

    #[tokio::test]
    async fn test_ack_after_nack_is_idempotent_noop() {
        // Once a receipt is Nacked, a subsequent ack must NOT mutate
        // the terminal — it returns Ok and the state stays Nacked.
        let (f, r, _reg) = fresh_incoming();
        f.nack("nope").await.unwrap();
        f.ack().await.unwrap();
        match r.state() {
            State::Failed(FailedTerminal::Nacked { reason }) => {
                assert_eq!(reason, "nope");
            }
            other => panic!("expected Nacked still, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_legacy_sender_ack_is_silent_noop() {
        let f = IncomingFile::new_without_receipt(dummy_received());
        assert!(f.receipt_id().is_none());
        // Both must succeed without panicking.
        f.ack().await.unwrap();
        f.nack("ignored").await.unwrap();
    }

    #[test]
    fn test_receipt_id_returns_inner_id() {
        let receipt = Arc::new(Receipt::<Receiver>::new(Uuid::new_v4()));
        let registry = Arc::new(ReceiptRegistry::<Receiver>::new());
        let expected = receipt.id();
        let f = IncomingFile::new(dummy_received(), receipt, registry);
        assert_eq!(f.receipt_id(), Some(expected));
    }

    #[test]
    fn test_into_receipt_returns_inner_arc() {
        let receipt = Arc::new(Receipt::<Receiver>::new(Uuid::new_v4()));
        let id = receipt.id();
        let registry = Arc::new(ReceiptRegistry::<Receiver>::new());
        let f = IncomingFile::new(dummy_received(), Arc::clone(&receipt), registry);
        let extracted = f.into_receipt().unwrap();
        assert_eq!(extracted.id(), id);
        assert!(Arc::ptr_eq(&extracted, &receipt));
    }

    #[test]
    fn test_into_receipt_returns_none_for_legacy() {
        let f = IncomingFile::new_without_receipt(dummy_received());
        assert!(f.into_receipt().is_none());
    }

    #[test]
    fn test_into_inner_returns_received_file() {
        let received = dummy_received();
        let id = received.id;
        let f = IncomingFile::new_without_receipt(received);
        assert_eq!(f.into_inner().id, id);
    }

    #[test]
    fn test_metadata_default_is_empty() {
        let f = IncomingFile::new_without_receipt(dummy_received());
        let meta = f.metadata();
        assert!(meta.id.is_empty());
        assert!(meta.user_metadata.is_empty());
        assert!(meta.trace_id.is_none());
    }

    #[test]
    fn test_with_metadata_attaches_envelope() {
        use crate::core::metadata::MetadataBuilder;
        use aerosync_proto::Lifecycle;
        let envelope = MetadataBuilder::new()
            .trace_id("run-99")
            .lifecycle(Lifecycle::Transient)
            .user("tenant", "acme")
            .build()
            .unwrap();
        let f = IncomingFile::new_without_receipt(dummy_received()).with_metadata(envelope);
        assert_eq!(f.metadata().trace_id.as_deref(), Some("run-99"));
        assert_eq!(f.metadata().user_metadata["tenant"], "acme");
        assert_eq!(f.metadata().lifecycle, Some(Lifecycle::Transient as i32));
    }
}

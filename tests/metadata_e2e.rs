//! RFC-003 Metadata Envelope — end-to-end integration tests (Group D).
//!
//! These tests cover the **integration** of the metadata envelope across
//! the public Rust surface — `MetadataBuilder`, `TransferEngine`,
//! `HistoryStore`, and the JSON-on-disk shape — without booting a real
//! QUIC/HTTP transport. The goal is to prove the building blocks landed
//! in Groups A/B compose correctly when wired through `send_with_metadata`,
//! complementing the per-module unit tests that already live next to
//! each component.
//!
//! ## Scope (intentional)
//!
//! 1. Roundtrip: a fully-populated envelope → `send_with_metadata` →
//!    `metadata_for(receipt_id)` is byte-equal to the input *plus* the
//!    sealed system fields (RFC-003 §4.1).
//! 2. Persistence: the same envelope survives `HistoryStore`'s JSONL
//!    append + reload cycle and is reachable via `query` filters
//!    (`trace_id`, `metadata_eq`, `lifecycle`).
//! 3. Lineage: `parent_file_ids` round-trips through both the engine
//!    and the history store.
//! 4. Oversize rejection: building an envelope that exceeds the
//!    RFC-003 §6 64 KiB cap is a `MetadataError::OversizeEnvelope`,
//!    NOT a wire-time failure.
//!
//! ## Out of scope (deferred)
//!
//! - On-the-wire metadata round-trip via QUIC: the `TransferStart`
//!   wire-frame integration is deferred to the `quic_receipt` task
//!   (see `src/core/transfer.rs` "deferred — see `quic_receipt`"
//!   comments). The receiver-side `IncomingFile::metadata()` is
//!   exercised by `src/core/incoming_file.rs::tests`; the hand-off
//!   through the QUIC adapter will land alongside that wiring.
//! - SQLite-backed history storage (RFC-003 task #5 SQLite branch is
//!   deferred to v0.2.1).

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use aerosync::core::history::{HistoryQuery, HistoryStore};
use aerosync::core::metadata::{MetadataBuilder, MetadataError, MAX_USER_METADATA_VALUE_BYTES};
use aerosync::core::transfer::{TransferConfig, TransferEngine, TransferTask};
use aerosync_proto::Lifecycle;
use common::SuccessAdapter;
use tempfile::tempdir;

/// Wait for the `TransferEngine`'s in-engine `sealed_metadata` map to
/// drop the entry for `receipt_id` (the bridge GCs it on terminal).
/// Polled rather than driven by a watch because the bridge runs on
/// its own task and we don't want to leak a 24h timeout into the
/// test suite.
async fn wait_until_sealed_dropped(engine: &TransferEngine, receipt_id: uuid::Uuid) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if engine.metadata_for(receipt_id).await.is_none() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// (1) Roundtrip — a fully populated envelope flows from `send_with_metadata`
///     into the engine's sealed_metadata map with system fields stamped.
#[tokio::test]
async fn metadata_envelope_roundtrips_through_send_with_metadata() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("payload.txt");
    tokio::fs::write(&src, b"hello world").await.unwrap();

    let engine = TransferEngine::new(TransferConfig::default());
    engine.start(Arc::new(SuccessAdapter)).await.unwrap();

    let metadata = MetadataBuilder::new()
        .trace_id("trace-roundtrip-1")
        .conversation_id("conv-1")
        .lifecycle(Lifecycle::Durable)
        .user("tenant", "acme")
        .user("agent", "cleaner")
        .build()
        .unwrap();

    let task = TransferTask::new_upload(src.clone(), "http://localhost:1/upload".into(), 11);
    let receipt = engine
        .send_with_metadata(task, metadata.clone())
        .await
        .expect("send_with_metadata must succeed for a valid envelope");

    // Sealed copy must include system fields stamped by the engine
    // AND preserve the user/well-known fields verbatim.
    let sealed = engine
        .metadata_for(receipt.id())
        .await
        .expect("sealed metadata must exist before terminal");
    assert_eq!(sealed.trace_id.as_deref(), Some("trace-roundtrip-1"));
    assert_eq!(sealed.conversation_id.as_deref(), Some("conv-1"));
    assert_eq!(sealed.lifecycle, Some(Lifecycle::Durable as i32));
    assert_eq!(
        sealed.user_metadata.get("tenant").map(String::as_str),
        Some("acme")
    );
    assert_eq!(
        sealed.user_metadata.get("agent").map(String::as_str),
        Some("cleaner")
    );
    // System fields stamped by the engine.
    assert_eq!(sealed.id, receipt.id().to_string());
    assert_eq!(sealed.size_bytes, 11);
    assert_eq!(sealed.file_name, "payload.txt");
    assert!(!sealed.from_node.is_empty(), "from_node must be populated");
    assert!(
        sealed.created_at.is_some(),
        "created_at must be populated by engine"
    );

    // Bridge eventually drops the sealed copy (memory bound for
    // long-running senders).
    assert!(
        wait_until_sealed_dropped(&engine, receipt.id()).await,
        "sealed metadata should be GC'd once the receipt is terminal"
    );
}

/// (2) Persistence — same envelope survives the `HistoryStore` JSONL
///     round-trip and is reachable via metadata-aware filters.
#[tokio::test]
async fn metadata_persists_in_history_and_is_queryable() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("payload.bin");
    tokio::fs::write(&src, b"x").await.unwrap();
    let history_path = dir.path().join("history.jsonl");
    let store = Arc::new(HistoryStore::new(&history_path).await.unwrap());

    let engine =
        TransferEngine::new(TransferConfig::default()).with_history_store(Arc::clone(&store));
    engine.start(Arc::new(SuccessAdapter)).await.unwrap();

    let metadata = MetadataBuilder::new()
        .trace_id("trace-persist-1")
        .lifecycle(Lifecycle::Transient)
        .user("tenant", "acme")
        .user("agent", "indexer")
        .build()
        .unwrap();
    let task = TransferTask::new_upload(src, "http://localhost:1/upload".into(), 1);
    let _ = engine
        .send_with_metadata(task, metadata)
        .await
        .expect("send_with_metadata succeeds");

    // Drop and reopen the store to prove the on-disk JSONL form is
    // self-contained (no in-memory state required).
    drop(engine);
    drop(store);
    let store = HistoryStore::new(&history_path).await.unwrap();

    // Filter by trace_id.
    let by_trace = store
        .query(&HistoryQuery {
            trace_id: Some("trace-persist-1".into()),
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(by_trace.len(), 1, "trace_id filter must hit the record");
    let m = by_trace[0]
        .metadata
        .as_ref()
        .expect("metadata must be persisted on disk");
    assert_eq!(m.trace_id.as_deref(), Some("trace-persist-1"));
    assert_eq!(m.lifecycle.as_deref(), Some("transient"));
    assert_eq!(
        m.user_metadata.get("tenant").map(String::as_str),
        Some("acme")
    );

    // Filter by metadata_eq (AND semantics across pairs).
    let mut want = HashMap::new();
    want.insert("tenant".into(), "acme".into());
    want.insert("agent".into(), "indexer".into());
    let by_kv = store
        .query(&HistoryQuery {
            metadata_eq: want,
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(by_kv.len(), 1);

    // Filter by lifecycle.
    let by_lc = store
        .query(&HistoryQuery {
            lifecycle: Some(Lifecycle::Transient),
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(by_lc.len(), 1);
}

/// (3) Lineage — `parent_file_ids` survives both the engine and the
///     history store.
#[tokio::test]
async fn metadata_lineage_survives_engine_and_history() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("derived.bin");
    tokio::fs::write(&src, b"y").await.unwrap();
    let history_path = dir.path().join("history.jsonl");
    let store = Arc::new(HistoryStore::new(&history_path).await.unwrap());

    let engine =
        TransferEngine::new(TransferConfig::default()).with_history_store(Arc::clone(&store));
    engine.start(Arc::new(SuccessAdapter)).await.unwrap();

    let metadata = MetadataBuilder::new()
        .trace_id("trace-lineage-1")
        .parent("blob-source-a")
        .parent("blob-source-b")
        .build()
        .unwrap();
    let task = TransferTask::new_upload(src, "http://localhost:1/upload".into(), 1);
    let receipt = engine
        .send_with_metadata(task, metadata)
        .await
        .expect("send succeeds");

    // Live in-engine view.
    let sealed = engine.metadata_for(receipt.id()).await.expect("sealed");
    assert_eq!(
        sealed.parent_file_ids,
        vec!["blob-source-a", "blob-source-b"]
    );

    // Post-terminal disk view.
    drop(engine);
    drop(store);
    let store = HistoryStore::new(&history_path).await.unwrap();
    let entries = store
        .query(&HistoryQuery {
            trace_id: Some("trace-lineage-1".into()),
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    let m = entries[0].metadata.as_ref().unwrap();
    assert_eq!(m.parent_file_ids, vec!["blob-source-a", "blob-source-b"]);
}

/// (4) Oversize rejection — `MetadataBuilder::build` enforces the
///     RFC-003 §6 64 KiB cap and we never reach the engine.
#[tokio::test]
async fn oversize_envelope_rejected_at_build_not_at_send() {
    // Five entries × ~16 KiB each = ~80 KiB → over the 64 KiB cap.
    let big = "x".repeat(MAX_USER_METADATA_VALUE_BYTES);
    let mut builder = MetadataBuilder::new();
    for i in 0..5 {
        builder = builder.user(format!("k{i}"), big.clone());
    }
    let err = builder.build().expect_err("oversize must be rejected");
    assert!(
        matches!(err, MetadataError::OversizeEnvelope { .. }),
        "expected OversizeEnvelope, got {err:?}"
    );
}

/// (5) System-field anti-spoofing — values the caller writes into
///     system slots must be overwritten by the engine when the
///     envelope is sealed. We exercise the whole path: pass a bogus
///     id/sha256/from_node into `user_metadata` (those are never
///     accepted as system fields anyway, but we want to make sure
///     they don't *replace* the engine-stamped system fields either).
#[tokio::test]
async fn engine_stamps_system_fields_even_when_user_supplies_them() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("payload.bin");
    tokio::fs::write(&src, b"hello").await.unwrap();

    let engine = TransferEngine::new(TransferConfig::default()).with_node_id("alice");
    engine.start(Arc::new(SuccessAdapter)).await.unwrap();

    // Caller tries to spoof system fields via user_metadata. The
    // builder logs a warn but keeps the entries; the engine still
    // populates the canonical system slots from the real task.
    let metadata = MetadataBuilder::new()
        .user("sha256", "deadbeef")
        .user("from_node", "mallory")
        .build()
        .unwrap();
    let mut task = TransferTask::new_upload(src, "http://localhost:1/upload".into(), 5);
    // The engine treats `task.sha256` as the canonical hash source
    // for the envelope (it does NOT recompute from disk). Provide
    // a real-looking value so we can prove the spoof from
    // user_metadata is ignored when the engine seals system fields.
    task.sha256 = Some("a".repeat(64));
    let receipt = engine
        .send_with_metadata(task, metadata)
        .await
        .expect("send succeeds");

    let sealed = engine.metadata_for(receipt.id()).await.expect("sealed");
    assert_eq!(sealed.from_node, "alice", "from_node MUST come from engine");
    assert_eq!(
        sealed.sha256,
        "a".repeat(64),
        "sha256 must come from the canonical task field, not user_metadata"
    );
    assert_ne!(
        sealed.sha256, "deadbeef",
        "sha256 must NOT be the user-supplied bogus value"
    );
    // The user_metadata entries themselves are preserved verbatim
    // (RFC-003 §4.1 round-trip fidelity).
    assert_eq!(
        sealed.user_metadata.get("sha256").map(String::as_str),
        Some("deadbeef")
    );
    assert_eq!(
        sealed.user_metadata.get("from_node").map(String::as_str),
        Some("mallory")
    );
}

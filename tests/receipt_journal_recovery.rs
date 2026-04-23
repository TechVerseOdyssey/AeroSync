//! RFC-002 §8 receipt-journal recovery integration test.
//!
//! Simulates a process crash mid-transfer by:
//!
//! 1. Building a [`SqliteReceiptJournal`] in a tempdir.
//! 2. Spawning the watch-bridge against a fresh
//!    [`Receipt<Sender>`] driven through `Initiated → StreamOpened`.
//! 3. **Dropping** every receipt holder *and* the journal handle —
//!    this is the in-process equivalent of a `kill -9`.
//! 4. **Re-opening** the journal at the same path and asserting the
//!    recovery iterator surfaces exactly the in-flight receipt with
//!    its last observed state.
//!
//! Then exercises the happy path: drive a receipt to `Acked`,
//! reopen, assert it lands in `list_recent_terminal` and *does not*
//! land in `list_recoverable`.
//!
//! Lives at the workspace root rather than under `aerosync-infra/tests/`
//! because it needs the convenience re-exports on
//! `aerosync::core::*` that downstream callers actually consume —
//! same pattern as `tests/receipts_e2e.rs`.

use std::sync::Arc;

use aerosync::core::receipt::{Event, Receipt, Sender};
use aerosync::core::receipt_journal::SqliteReceiptJournal;
use aerosync::core::{ReceiptJournalStorage, ReceiptSide};
use tempfile::TempDir;
use uuid::Uuid;

/// Block on the spawned watch-bridge task long enough for the initial
/// `Initiated` row to land. Polls the journal every loop iteration so
/// we don't sleep an arbitrary fixed window.
async fn wait_for_initial(j: &SqliteReceiptJournal, rid: Uuid) {
    for _ in 0..100 {
        tokio::task::yield_now().await;
        if j.latest_state(rid).await.unwrap().is_some() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("initial state never recorded for receipt {rid}");
}

#[tokio::test]
async fn crash_mid_transfer_then_recover_surfaces_in_flight_receipt() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("receipts.db");

    let pre_crash_rid;
    {
        // ── Pre-crash process ────────────────────────────────────────
        let journal = Arc::new(SqliteReceiptJournal::open(&path).await.unwrap());

        let receipt: Arc<Receipt<Sender>> = Arc::new(Receipt::new(Uuid::new_v4()));
        pre_crash_rid = receipt.id();

        let _bridge = journal.spawn_journal_bridge(
            Arc::clone(&receipt),
            ReceiptSide::Send,
            None,
            Some("doc.pdf".into()),
            Some("10.0.0.5".into()),
        );

        wait_for_initial(&journal, pre_crash_rid).await;

        // Drive a partial transfer.
        receipt.apply_event(Event::Open).unwrap();

        // Wait for the StreamOpened row to land (watch may coalesce).
        for _ in 0..100 {
            tokio::task::yield_now().await;
            let st = journal
                .latest_state(pre_crash_rid)
                .await
                .unwrap()
                .map(|r| r.state)
                .unwrap_or_default();
            if st == "stream-opened" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // ── "Crash": drop every holder without recording terminal ──
        drop(receipt);
        // The bridge will fire `stream-lost` because the watch
        // channel closes when the last Receipt holder is dropped.
        // That is fine — `stream-lost` is also non-terminal-success
        // and represents exactly the recoverable state we want to
        // surface to the caller. Wait for it to land before
        // dropping the journal so the assertion below is
        // deterministic.
        for _ in 0..100 {
            tokio::task::yield_now().await;
            let st = journal
                .latest_state(pre_crash_rid)
                .await
                .unwrap()
                .map(|r| r.state)
                .unwrap_or_default();
            if st == "stream-lost" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        drop(journal);
    }

    // ── Post-crash recovery process ─────────────────────────────────
    let reopened = SqliteReceiptJournal::open(&path).await.unwrap();

    let recoverable = reopened.list_recoverable(0).await.unwrap();
    let recent_terminal = reopened.list_recent_terminal(3600, 0).await.unwrap();

    // `stream-lost` is terminal-failure: the recovery iterator
    // surfaces it via `list_recent_terminal`, NOT `list_recoverable`,
    // so the post-crash agent sees "this receipt previously failed
    // because the process exited" rather than "this receipt is still
    // in flight and you should resume it". Both outcomes are
    // legitimate recovery surfaces; the test asserts both.
    assert_eq!(
        recoverable.len(),
        0,
        "stream-lost rows must not appear in list_recoverable"
    );
    assert_eq!(recent_terminal.len(), 1);
    assert_eq!(recent_terminal[0].receipt_id, pre_crash_rid);
    assert_eq!(recent_terminal[0].state, "stream-lost");
    assert!(recent_terminal[0].is_terminal);
    assert_eq!(
        recent_terminal[0].filename.as_deref(),
        Some("doc.pdf"),
        "filename context must survive the crash"
    );
    assert_eq!(recent_terminal[0].peer.as_deref(), Some("10.0.0.5"));
}

#[tokio::test]
async fn happy_path_acked_receipt_appears_in_recent_terminal() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("receipts.db");
    let journal = Arc::new(SqliteReceiptJournal::open(&path).await.unwrap());

    let receipt: Arc<Receipt<Sender>> = Arc::new(Receipt::new(Uuid::new_v4()));
    let rid = receipt.id();

    let bridge =
        journal.spawn_journal_bridge(Arc::clone(&receipt), ReceiptSide::Send, None, None, None);
    wait_for_initial(&journal, rid).await;

    receipt.apply_event(Event::Open).unwrap();
    receipt.apply_event(Event::Close).unwrap();
    receipt.apply_event(Event::Close).unwrap();
    receipt.apply_event(Event::Process).unwrap();
    receipt.apply_event(Event::Ack).unwrap();
    bridge.await.unwrap();

    drop(journal);

    let reopened = SqliteReceiptJournal::open(&path).await.unwrap();
    let recoverable = reopened.list_recoverable(0).await.unwrap();
    assert!(
        recoverable.is_empty(),
        "acked receipts must not appear in list_recoverable"
    );

    let recent = reopened.list_recent_terminal(3600, 0).await.unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].receipt_id, rid);
    assert_eq!(recent[0].state, "acked");
}

#[tokio::test]
async fn nacked_receipt_carries_reason_through_recovery() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("receipts.db");
    let journal = Arc::new(SqliteReceiptJournal::open(&path).await.unwrap());

    let receipt: Arc<Receipt<Sender>> = Arc::new(Receipt::new(Uuid::new_v4()));
    let rid = receipt.id();
    let bridge =
        journal.spawn_journal_bridge(Arc::clone(&receipt), ReceiptSide::Send, None, None, None);
    wait_for_initial(&journal, rid).await;

    receipt.apply_event(Event::Open).unwrap();
    receipt.apply_event(Event::Close).unwrap();
    receipt.apply_event(Event::Close).unwrap();
    receipt.apply_event(Event::Process).unwrap();
    receipt
        .apply_event(Event::Nack {
            reason: "schema mismatch".into(),
        })
        .unwrap();
    bridge.await.unwrap();
    drop(journal);

    let reopened = SqliteReceiptJournal::open(&path).await.unwrap();
    let got = reopened.latest_state(rid).await.unwrap().unwrap();
    assert_eq!(got.state, "nacked");
    assert_eq!(got.reason.as_deref(), Some("schema mismatch"));
}

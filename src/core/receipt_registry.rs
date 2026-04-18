//! In-process registry of live [`Receipt`] objects, keyed by `receipt_id`.
//!
//! Both ends of an `aerosync/1` peering need a way to look up the
//! [`Receipt`] for an incoming wire frame (QUIC `ReceiptFrame` or HTTP
//! `POST /v1/receipts/:id/ack`) and route the corresponding [`Event`]
//! into the right state machine. This registry is the side-agnostic
//! storage layer that does the lookup.
//!
//! # Side parameterisation
//!
//! [`Receipt<Side>`] is generic over a phantom side marker
//! ([`Sender`](super::receipt::Sender) /
//! [`Receiver`](super::receipt::Receiver)) so the type system catches
//! "wrong side of the wire" misuse at compile time. The registry
//! preserves that distinction by being itself parameterised:
//! `ReceiptRegistry<Sender>` and `ReceiptRegistry<Receiver>` are
//! distinct types and a process running both sides (e.g. an integration
//! test) holds two independent registries.
//!
//! # Concurrency
//!
//! The registry is internally synchronised behind a `RwLock` and is
//! cheaply cloneable (it's an `Arc` under the hood). Callers can hand
//! it across tasks freely:
//!
//! ```
//! # use std::sync::Arc;
//! # use aerosync::core::receipt::{Receipt, Sender};
//! # use aerosync::core::receipt_registry::ReceiptRegistry;
//! # use uuid::Uuid;
//! let registry: ReceiptRegistry<Sender> = ReceiptRegistry::new();
//! let r = Arc::new(Receipt::<Sender>::new(Uuid::new_v4()));
//! registry.insert(Arc::clone(&r));
//! assert!(registry.get(r.id()).is_some());
//! ```

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use uuid::Uuid;

use crate::core::receipt::Receipt;

/// Side-parameterised registry of live [`Receipt`]s.
///
/// `Side` is the same phantom marker used by [`Receipt`]; see the
/// module-level docs.
pub struct ReceiptRegistry<Side> {
    inner: Arc<RwLock<HashMap<Uuid, Arc<Receipt<Side>>>>>,
}

impl<Side> ReceiptRegistry<Side> {
    /// Construct a new empty registry.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Insert (or replace) a receipt. Returns the previous value, if
    /// any, in case the caller wants to detect duplicate registration.
    pub fn insert(&self, receipt: Arc<Receipt<Side>>) -> Option<Arc<Receipt<Side>>> {
        let id = receipt.id();
        let mut guard = self.inner.write().expect("ReceiptRegistry lock poisoned");
        guard.insert(id, receipt)
    }

    /// Insert only if there is no existing entry. Returns `true` if
    /// the receipt was inserted, `false` if `id` was already present.
    pub fn insert_if_absent(&self, receipt: Arc<Receipt<Side>>) -> bool {
        let id = receipt.id();
        let mut guard = self.inner.write().expect("ReceiptRegistry lock poisoned");
        if let std::collections::hash_map::Entry::Vacant(slot) = guard.entry(id) {
            slot.insert(receipt);
            true
        } else {
            false
        }
    }

    /// Look up a receipt by id.
    pub fn get(&self, id: Uuid) -> Option<Arc<Receipt<Side>>> {
        let guard = self.inner.read().expect("ReceiptRegistry lock poisoned");
        guard.get(&id).cloned()
    }

    /// Remove a receipt by id, returning the removed value.
    pub fn remove(&self, id: Uuid) -> Option<Arc<Receipt<Side>>> {
        let mut guard = self.inner.write().expect("ReceiptRegistry lock poisoned");
        guard.remove(&id)
    }

    /// True if the registry contains a receipt with the given id.
    pub fn contains(&self, id: Uuid) -> bool {
        let guard = self.inner.read().expect("ReceiptRegistry lock poisoned");
        guard.contains_key(&id)
    }

    /// Number of receipts currently registered.
    pub fn len(&self) -> usize {
        let guard = self.inner.read().expect("ReceiptRegistry lock poisoned");
        guard.len()
    }

    /// True if the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Snapshot of every registered receipt id. Acquires a read lock
    /// briefly; safe to call from any task.
    pub fn ids(&self) -> Vec<Uuid> {
        let guard = self.inner.read().expect("ReceiptRegistry lock poisoned");
        guard.keys().copied().collect()
    }

    /// Snapshot of every registered receipt as `(id, receipt)` pairs.
    /// The `Arc` clones are cheap; the snapshot is a point-in-time
    /// view and the underlying map may have been mutated by the time
    /// the caller iterates.
    pub fn snapshot(&self) -> Vec<(Uuid, Arc<Receipt<Side>>)> {
        let guard = self.inner.read().expect("ReceiptRegistry lock poisoned");
        guard.iter().map(|(k, v)| (*k, Arc::clone(v))).collect()
    }

    /// Drop every entry whose receipt has reached a terminal state.
    /// Returns the number of entries removed. Useful as a periodic GC
    /// hook so the registry doesn't grow unbounded over the lifetime
    /// of a long-running process.
    pub fn prune_terminal(&self) -> usize {
        let mut guard = self.inner.write().expect("ReceiptRegistry lock poisoned");
        let before = guard.len();
        guard.retain(|_, r| !r.state().is_terminal());
        before - guard.len()
    }
}

impl<Side> Clone for ReceiptRegistry<Side> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<Side> Default for ReceiptRegistry<Side> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Side> std::fmt::Debug for ReceiptRegistry<Side> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let len = self.len();
        f.debug_struct("ReceiptRegistry")
            .field("len", &len)
            .finish()
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::receipt::{Event, Receiver, Sender};
    use std::thread;

    fn arc_sender() -> Arc<Receipt<Sender>> {
        Arc::new(Receipt::<Sender>::new(Uuid::new_v4()))
    }

    fn arc_receiver() -> Arc<Receipt<Receiver>> {
        Arc::new(Receipt::<Receiver>::new(Uuid::new_v4()))
    }

    #[test]
    fn empty_registry_has_zero_len_and_is_empty() {
        let reg: ReceiptRegistry<Sender> = ReceiptRegistry::new();
        assert_eq!(reg.len(), 0);
        assert!(reg.is_empty());
        assert!(reg.ids().is_empty());
    }

    #[test]
    fn insert_then_get_returns_same_arc() {
        let reg: ReceiptRegistry<Sender> = ReceiptRegistry::new();
        let r = arc_sender();
        let id = r.id();
        assert!(reg.insert(Arc::clone(&r)).is_none());
        let fetched = reg.get(id).expect("should find");
        // Same Arc allocation: pointer-equality.
        assert!(Arc::ptr_eq(&r, &fetched));
    }

    #[test]
    fn insert_replaces_and_returns_previous() {
        let reg: ReceiptRegistry<Sender> = ReceiptRegistry::new();
        let id = Uuid::new_v4();
        let a = Arc::new(Receipt::<Sender>::new(id));
        let b = Arc::new(Receipt::<Sender>::new(id));
        assert!(reg.insert(Arc::clone(&a)).is_none());
        let prev = reg.insert(Arc::clone(&b)).expect("previous present");
        assert!(Arc::ptr_eq(&prev, &a));
        let now = reg.get(id).expect("now should be b");
        assert!(Arc::ptr_eq(&now, &b));
    }

    #[test]
    fn insert_if_absent_keeps_first_writer() {
        let reg: ReceiptRegistry<Sender> = ReceiptRegistry::new();
        let id = Uuid::new_v4();
        let a = Arc::new(Receipt::<Sender>::new(id));
        let b = Arc::new(Receipt::<Sender>::new(id));
        assert!(reg.insert_if_absent(Arc::clone(&a)));
        assert!(!reg.insert_if_absent(Arc::clone(&b)));
        let fetched = reg.get(id).expect("a should still win");
        assert!(Arc::ptr_eq(&fetched, &a));
    }

    #[test]
    fn remove_returns_value_and_subsequent_get_is_none() {
        let reg: ReceiptRegistry<Sender> = ReceiptRegistry::new();
        let r = arc_sender();
        let id = r.id();
        reg.insert(Arc::clone(&r));
        let removed = reg.remove(id).expect("removed");
        assert!(Arc::ptr_eq(&removed, &r));
        assert!(reg.get(id).is_none());
        assert!(reg.remove(id).is_none(), "double-remove returns None");
    }

    #[test]
    fn contains_tracks_lifecycle() {
        let reg: ReceiptRegistry<Sender> = ReceiptRegistry::new();
        let r = arc_sender();
        let id = r.id();
        assert!(!reg.contains(id));
        reg.insert(Arc::clone(&r));
        assert!(reg.contains(id));
        reg.remove(id);
        assert!(!reg.contains(id));
    }

    #[test]
    fn ids_and_snapshot_reflect_inserts() {
        let reg: ReceiptRegistry<Sender> = ReceiptRegistry::new();
        let r1 = arc_sender();
        let r2 = arc_sender();
        let r3 = arc_sender();
        reg.insert(Arc::clone(&r1));
        reg.insert(Arc::clone(&r2));
        reg.insert(Arc::clone(&r3));
        assert_eq!(reg.len(), 3);
        let mut ids = reg.ids();
        ids.sort();
        let mut expected = vec![r1.id(), r2.id(), r3.id()];
        expected.sort();
        assert_eq!(ids, expected);
        assert_eq!(reg.snapshot().len(), 3);
    }

    #[test]
    fn clone_shares_storage() {
        let reg: ReceiptRegistry<Sender> = ReceiptRegistry::new();
        let cloned = reg.clone();
        let r = arc_sender();
        reg.insert(Arc::clone(&r));
        // Mutation via `reg` is visible via `cloned` because both
        // share the same inner Arc<RwLock<...>>.
        assert!(cloned.contains(r.id()));
        assert_eq!(cloned.len(), 1);
    }

    #[test]
    fn registry_works_for_receiver_side_too() {
        let reg: ReceiptRegistry<Receiver> = ReceiptRegistry::new();
        let r = arc_receiver();
        reg.insert(Arc::clone(&r));
        assert!(reg.contains(r.id()));
    }

    #[test]
    fn prune_terminal_drops_only_terminal_entries() {
        let reg: ReceiptRegistry<Sender> = ReceiptRegistry::new();
        let live = arc_sender();
        let dead = arc_sender();
        // Drive `dead` to a terminal state.
        dead.apply_event(Event::Cancel {
            reason: "test".into(),
        })
        .unwrap();
        assert!(dead.state().is_terminal());

        reg.insert(Arc::clone(&live));
        reg.insert(Arc::clone(&dead));
        assert_eq!(reg.len(), 2);

        let pruned = reg.prune_terminal();
        assert_eq!(pruned, 1);
        assert_eq!(reg.len(), 1);
        assert!(reg.contains(live.id()));
        assert!(!reg.contains(dead.id()));
    }

    #[test]
    fn concurrent_inserts_and_reads_are_safe() {
        let reg: ReceiptRegistry<Sender> = ReceiptRegistry::new();
        let mut handles = Vec::new();
        for _ in 0..16 {
            let reg = reg.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..32 {
                    let r = arc_sender();
                    let id = r.id();
                    reg.insert(r);
                    let _ = reg.get(id);
                    let _ = reg.contains(id);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(reg.len(), 16 * 32);
    }

    #[test]
    fn default_constructs_empty_registry() {
        let reg: ReceiptRegistry<Sender> = ReceiptRegistry::default();
        assert!(reg.is_empty());
    }

    #[test]
    fn debug_includes_len() {
        let reg: ReceiptRegistry<Sender> = ReceiptRegistry::new();
        reg.insert(arc_sender());
        let s = format!("{:?}", reg);
        assert!(s.contains("len"));
        assert!(s.contains('1'));
    }
}

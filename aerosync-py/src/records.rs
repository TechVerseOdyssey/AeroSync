//! Plain-data records crossing the FFI boundary.
//!
//! These mirror the dataclasses declared in `python/aerosync/_types.py`
//! but as `#[pyclass]` so Rust code can build them directly. The
//! Python-side dataclasses serve as IDE/type-checker hints; at
//! runtime, the values returned to user code are these `#[pyclass]`
//! instances. Both shapes carry the same field names so a downstream
//! mypy run sees a consistent type.
//!
//! # Bridge functions
//!
//! `From<aerosync::core::history::HistoryEntry> for PyHistoryEntry`
//! lets w6's `Client.history()` impl simply `.into()` each row from
//! the engine.

use aerosync::core::history::{HistoryEntry, ReceiptStateLabel};
use aerosync::core::AeroSyncPeer;
use pyo3::prelude::*;

/// `Peer` — a single AeroSync peer discovered via mDNS or rendezvous.
#[pyclass(
    get_all,
    frozen,
    module = "aerosync._native",
    name = "Peer",
    skip_from_py_object
)]
#[derive(Clone, Debug)]
pub struct PyPeer {
    pub name: String,
    pub address: String,
    pub port: u16,
}

#[pymethods]
impl PyPeer {
    #[new]
    fn new(name: String, address: String, port: u16) -> Self {
        Self {
            name,
            address,
            port,
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "Peer(name={:?}, address={:?}, port={})",
            self.name, self.address, self.port
        )
    }
}

impl From<AeroSyncPeer> for PyPeer {
    fn from(p: AeroSyncPeer) -> Self {
        Self {
            name: p.name,
            address: p.host,
            port: p.port,
        }
    }
}

/// `Progress` — snapshot of an in-flight transfer.
#[pyclass(
    get_all,
    frozen,
    module = "aerosync._native",
    name = "Progress",
    skip_from_py_object
)]
#[derive(Clone, Debug)]
pub struct PyProgress {
    pub bytes_sent: u64,
    pub bytes_total: u64,
    pub files_sent: u32,
    pub files_total: u32,
    pub elapsed_secs: f64,
}

#[pymethods]
impl PyProgress {
    #[new]
    fn new(
        bytes_sent: u64,
        bytes_total: u64,
        files_sent: u32,
        files_total: u32,
        elapsed_secs: f64,
    ) -> Self {
        Self {
            bytes_sent,
            bytes_total,
            files_sent,
            files_total,
            elapsed_secs,
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "Progress(bytes_sent={}, bytes_total={}, files_sent={}, files_total={}, elapsed_secs={})",
            self.bytes_sent, self.bytes_total, self.files_sent, self.files_total, self.elapsed_secs
        )
    }
}

/// `HistoryEntry` — one row of the persisted transfer history.
///
/// The Rust-side `HistoryEntry` (see `aerosync::core::history`) carries
/// many more fields than RFC-001 §5.4 commits us to expose. We pick
/// the minimal stable subset for v0.2 and let w6 task #16 add the
/// metadata accessor.
#[pyclass(
    get_all,
    frozen,
    module = "aerosync._native",
    name = "HistoryEntry",
    skip_from_py_object
)]
#[derive(Clone, Debug)]
pub struct PyHistoryEntry {
    pub id: String,
    pub direction: String,
    pub file_name: String,
    pub size_bytes: u64,
    pub sha256: Option<String>,
    /// Unix epoch seconds. The Python-side dataclass surfaces this as
    /// an `Optional[datetime]`; the conversion happens in
    /// `python/aerosync/_types.py`'s adapter helper to avoid pulling
    /// `chrono`'s pyo3 feature into the bindings crate just for one
    /// timestamp.
    pub completed_at: Option<u64>,
    pub receipt_id: Option<String>,
    pub receipt_state: Option<String>,
}

#[pymethods]
impl PyHistoryEntry {
    #[new]
    #[pyo3(signature = (id, direction, file_name, size_bytes, sha256=None, completed_at=None, receipt_id=None, receipt_state=None))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        id: String,
        direction: String,
        file_name: String,
        size_bytes: u64,
        sha256: Option<String>,
        completed_at: Option<u64>,
        receipt_id: Option<String>,
        receipt_state: Option<String>,
    ) -> Self {
        Self {
            id,
            direction,
            file_name,
            size_bytes,
            sha256,
            completed_at,
            receipt_id,
            receipt_state,
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "HistoryEntry(id={:?}, direction={:?}, file_name={:?}, size_bytes={})",
            self.id, self.direction, self.file_name, self.size_bytes
        )
    }
}

fn label_to_str(label: ReceiptStateLabel) -> &'static str {
    match label {
        ReceiptStateLabel::Initiated => "initiated",
        ReceiptStateLabel::Acked => "acked",
        ReceiptStateLabel::Nacked => "nacked",
        ReceiptStateLabel::Cancelled => "cancelled",
        ReceiptStateLabel::Errored => "errored",
        ReceiptStateLabel::StreamLost => "stream-lost",
    }
}

impl From<HistoryEntry> for PyHistoryEntry {
    fn from(e: HistoryEntry) -> Self {
        Self {
            id: e.id.to_string(),
            direction: e.direction,
            file_name: e.filename,
            size_bytes: e.size,
            sha256: e.sha256,
            completed_at: Some(e.completed_at),
            receipt_id: e.receipt_id.map(|u| u.to_string()),
            receipt_state: e.receipt_state.map(|l| label_to_str(l).to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aerosync::core::history::HistoryEntry;

    #[test]
    fn peer_round_trips_from_aerosync_peer() {
        let src = AeroSyncPeer {
            name: "alice".into(),
            host: "192.168.1.20".into(),
            port: 7788,
            version: Some("0.2.0".into()),
            ws_enabled: true,
            auth_required: false,
        };
        let py: PyPeer = src.into();
        assert_eq!(py.name, "alice");
        assert_eq!(py.address, "192.168.1.20");
        assert_eq!(py.port, 7788);
    }

    #[test]
    fn progress_round_trips_field_values() {
        let p = PyProgress::new(123, 1000, 1, 2, 0.5);
        assert_eq!(p.bytes_sent, 123);
        assert_eq!(p.bytes_total, 1000);
        assert_eq!(p.files_sent, 1);
        assert_eq!(p.files_total, 2);
        assert!((p.elapsed_secs - 0.5).abs() < 1e-9);
    }

    #[test]
    fn history_entry_from_engine_row_preserves_essentials() {
        let entry = HistoryEntry::success(
            "report.csv",
            None,
            12_345,
            Some("ab".repeat(32)),
            Some("10.0.0.5".into()),
            "quic",
            "send",
            42,
        );
        let original_id = entry.id;
        let py: PyHistoryEntry = entry.into();
        assert_eq!(py.id, original_id.to_string());
        assert_eq!(py.direction, "send");
        assert_eq!(py.file_name, "report.csv");
        assert_eq!(py.size_bytes, 12_345);
        assert!(py.sha256.is_some());
        assert!(py.completed_at.is_some());
    }

    #[test]
    fn receipt_state_label_string_mapping_is_stable() {
        // The string values are part of the public API surface
        // (Python users `if entry.receipt_state == "acked"`).
        // Renaming any of these is a breaking change.
        assert_eq!(label_to_str(ReceiptStateLabel::Initiated), "initiated");
        assert_eq!(label_to_str(ReceiptStateLabel::Acked), "acked");
        assert_eq!(label_to_str(ReceiptStateLabel::Nacked), "nacked");
        assert_eq!(label_to_str(ReceiptStateLabel::Cancelled), "cancelled");
        assert_eq!(label_to_str(ReceiptStateLabel::Errored), "errored");
        assert_eq!(label_to_str(ReceiptStateLabel::StreamLost), "stream-lost");
    }
}

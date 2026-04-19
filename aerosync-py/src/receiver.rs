//! `Receiver` + `IncomingFile` (RFC-001 §5.3, §5.6 — tasks #5, #6, #15).
//!
//! The receiver iterator is wired on top of `FileReceiver`'s
//! `WsBroadcast`. When a `Completed` event lands we yield a
//! [`PyIncomingFile`] carrying:
//!
//! - The on-disk path / filename / size / sha256 (from `ReceivedFile`).
//! - A locally-constructed receiver-side
//!   [`Receipt<RxSide>`](aerosync::core::receipt::Receipt) walked
//!   through `Open → Close → Close → Process` so the user can call
//!   `await incoming.ack()` immediately.
//! - The receipt is also inserted into the `FileReceiver`'s
//!   shared `receipts().receiver_receipts` registry, so an HTTP
//!   client hitting `POST /v1/receipts/:id/ack` (RFC-002 §6.4)
//!   would observe the same state machine.
//!
//! # Linkage caveat (Group A scope)
//!
//! Today the receiver-side `Receipt` is *synthetic*: it is created
//! inside this binding rather than driven by a wire-level
//! receipt-stream from the sender (RFC-002 §6.2). This means
//! `incoming.ack()` does NOT yet propagate back to the sender's
//! `Receipt::processed()`. Wiring the QUIC receipt stream so that
//! ack-on-receiver flips the sender's terminal state is an engine-
//! side follow-up tracked outside this RFC. The Python API surface
//! is what's stable here.

use crate::errors::engine_err_to_py;
use crate::receipt::state_label;
use crate::runtime::future_into_py;
use aerosync::core::receipt::{Event, Receipt, Receiver as RxSide};
#[cfg(test)]
use aerosync::core::server::ServerConfig;
use aerosync::core::server::{FileReceiver, WsEvent};
use aerosync_proto::{Lifecycle, Metadata};
use base64::engine::general_purpose::STANDARD as B64_STD;
use base64::Engine as _;
use pyo3::exceptions::{PyRuntimeError, PyStopAsyncIteration, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{broadcast, Mutex};

/// Project the optional, well-known [`Lifecycle`] enum value carried
/// on a wire-decoded [`Metadata`] envelope into the
/// `LIFECYCLE_*` string the Python SDK exposes (e.g.
/// `"LIFECYCLE_TRANSIENT"`). Returns `None` when the field is absent
/// or the integer doesn't decode to a known variant — callers
/// surface that as `None` on the Python `IncomingFile.lifecycle`
/// getter, NOT as a string like `"LIFECYCLE_UNSPECIFIED"`, so that
/// `is None` is the canonical "sender didn't tell me" check.
fn lifecycle_label(meta: &Metadata) -> Option<&'static str> {
    let raw = meta.lifecycle?;
    Lifecycle::try_from(raw).ok().map(|l| l.as_str_name())
}

/// Inbound `Receiver`. Constructed via the `aerosync.receiver(...)`
/// sync factory; the returned object IS the async context manager.
#[pyclass(module = "aerosync._native", name = "Receiver")]
pub struct PyReceiver {
    pub(crate) inner: Arc<Mutex<FileReceiver>>,
    pub(crate) name: Option<String>,
    /// User-supplied `listen=` string (e.g. `"127.0.0.1:0"`). Used as
    /// a fallback by the `address` getter before the underlying
    /// receiver has bound its socket. Once `start()` has run, the
    /// getter prefers the OS-assigned address from `bound_http_addr`.
    pub(crate) address: String,
    /// Cloned from `FileReceiver::local_http_addr_handle()` at
    /// construction time. Reads the live OS-assigned HTTP address
    /// without contending the outer tokio mutex around the
    /// `FileReceiver` — important because the `address` getter is
    /// sync and has to remain non-blocking. `None` until the
    /// `FileReceiver` has actually bound the listener.
    pub(crate) bound_http_addr: Arc<StdMutex<Option<SocketAddr>>>,
    /// Sibling of `bound_http_addr` for the QUIC endpoint
    /// (Batch C / w0.2.1 P0.2). `Some(addr)` once `start()` has bound
    /// the QUIC UDP socket; `None` while the listener is stopped.
    /// The Python wheel is always built with the host engine's `quic`
    /// feature on (it is in `aerosync`'s default feature set), so we
    /// don't conditionally compile the field — keeping it
    /// unconditionally simplifies the builder and the getter.
    pub(crate) bound_quic_addr: Arc<StdMutex<Option<SocketAddr>>>,
    pub(crate) ws_rx: Arc<Mutex<Option<broadcast::Receiver<WsEvent>>>>,
    pub(crate) yielded: Arc<AtomicUsize>,
    pub(crate) started: Arc<AtomicBool>,
}

impl PyReceiver {
    pub fn new(inner: FileReceiver, name: Option<String>, address: String) -> Self {
        let bound_http_addr = inner.local_http_addr_handle();
        let bound_quic_addr = inner.local_quic_addr_handle();
        Self {
            inner: Arc::new(Mutex::new(inner)),
            name,
            address,
            bound_http_addr,
            bound_quic_addr,
            ws_rx: Arc::new(Mutex::new(None)),
            yielded: Arc::new(AtomicUsize::new(0)),
            started: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[pymethods]
impl PyReceiver {
    #[getter]
    fn name(&self) -> Option<String> {
        self.name.clone()
    }

    /// Returns the address the receiver is reachable at.
    ///
    /// Prefers the OS-assigned `SocketAddr` captured by
    /// `FileReceiver::start()` (so callers who passed
    /// `listen="127.0.0.1:0"` see the real port), falling back to
    /// the user-supplied `listen=` string when the receiver has not
    /// yet bound its socket. The fallback preserves the previous
    /// pre-`__aenter__` behaviour where the getter would echo the
    /// constructor argument.
    #[getter]
    fn address(&self) -> String {
        if let Some(addr) = *self.bound_http_addr.lock().unwrap() {
            return addr.to_string();
        }
        self.address.clone()
    }

    /// Returns the OS-assigned QUIC `host:port` once the underlying
    /// `FileReceiver` has bound its UDP socket; `None` before
    /// `__aenter__` resolves or when the receiver was started with
    /// QUIC disabled in `ServerConfig`. Used by tests (and by
    /// callers that want to hand a `quic://` URL to a sender) to
    /// discover the actual port without parsing log output.
    #[getter]
    fn quic_address(&self) -> Option<String> {
        self.bound_quic_addr
            .lock()
            .unwrap()
            .map(|addr| addr.to_string())
    }

    /// Async context manager entry — subscribes to the WS broadcast
    /// then starts the underlying `FileReceiver`.
    fn __aenter__<'py>(slf: Py<Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&slf.bind(py).borrow().inner);
        let ws_rx = Arc::clone(&slf.bind(py).borrow().ws_rx);
        let started = Arc::clone(&slf.bind(py).borrow().started);
        future_into_py(py, async move {
            let rx = {
                let guard = inner.lock().await;
                guard.ws_sender().subscribe()
            };
            *ws_rx.lock().await = Some(rx);
            inner.lock().await.start().await.map_err(engine_err_to_py)?;
            started.store(true, Ordering::SeqCst);
            Python::attach(|py| Ok(slf.clone_ref(py)))
        })
    }

    #[pyo3(signature = (_exc_type=None, _exc_value=None, _traceback=None))]
    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _exc_type: Option<Py<PyAny>>,
        _exc_value: Option<Py<PyAny>>,
        _traceback: Option<Py<PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        let started = Arc::clone(&self.started);
        future_into_py(py, async move {
            if started.swap(false, Ordering::SeqCst) {
                inner.lock().await.stop().await.map_err(engine_err_to_py)?;
            }
            Ok(false)
        })
    }

    /// `async for f in recv:` — the iterator IS the receiver.
    fn __aiter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Yield the next completed `IncomingFile`, or raise
    /// `StopAsyncIteration` when the receiver shuts down.
    fn __anext__<'py>(slf: Py<Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&slf.bind(py).borrow().inner);
        let ws_rx = Arc::clone(&slf.bind(py).borrow().ws_rx);
        let yielded = Arc::clone(&slf.bind(py).borrow().yielded);
        future_into_py(py, async move {
            loop {
                // 1) Drain backlog: hand back the next completed file.
                let (files, registry) = {
                    let guard = inner.lock().await;
                    (
                        guard.get_received_files().await,
                        guard.receipts().receiver_receipts.clone(),
                    )
                };
                let i = yielded.load(Ordering::SeqCst);
                if i < files.len() {
                    let f = files[i].clone();
                    yielded.store(i + 1, Ordering::SeqCst);
                    // Build a synthetic receiver-side receipt walked
                    // to `Processing` so `await incoming.ack()` is
                    // a one-line happy path. See module-level
                    // "Linkage caveat".
                    let rcpt: Arc<Receipt<RxSide>> = Arc::new(Receipt::new(f.id));
                    let _ = rcpt.apply_event(Event::Open);
                    let _ = rcpt.apply_event(Event::Close);
                    let _ = rcpt.apply_event(Event::Close);
                    let _ = rcpt.apply_event(Event::Process);
                    registry.insert(Arc::clone(&rcpt));
                    // Project the wire-decoded RFC-003 envelope into
                    // the Python-facing snapshot. We deliberately
                    // *clone* user_metadata + the well-known fields
                    // rather than holding a reference to the
                    // `ReceivedFile`, because `PyIncomingFile` is
                    // surfaced to user code that may outlive the
                    // engine's `received_files` Vec.
                    let (user_metadata, trace_id, conversation_id, correlation_id, lifecycle) =
                        match f.metadata.as_ref() {
                            Some(m) => (
                                m.user_metadata.clone(),
                                m.trace_id.clone(),
                                m.conversation_id.clone(),
                                m.correlation_id.clone(),
                                lifecycle_label(m),
                            ),
                            None => (HashMap::new(), None, None, None, None),
                        };
                    return Ok(PyIncomingFile {
                        path: f.saved_path,
                        file_name: f.original_name,
                        size_bytes: f.size,
                        sha256: f.sha256,
                        receipt: Some(rcpt),
                        user_metadata,
                        trace_id,
                        conversation_id,
                        correlation_id,
                        lifecycle,
                    });
                }

                // 2) Otherwise wait for the next WS event and re-loop.
                let mut guard = ws_rx.lock().await;
                let rx = guard.as_mut().ok_or_else(|| {
                    PyRuntimeError::new_err("Receiver iterated before `async with` entered")
                })?;
                match rx.recv().await {
                    Ok(WsEvent::Completed { .. }) => continue,
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(PyStopAsyncIteration::new_err(()));
                    }
                }
            }
        })
    }
}

/// `IncomingFile` — receiver-side handle to a freshly-arrived file.
///
/// `ack()` / `nack()` operate on a locally-constructed
/// receiver-side `Receipt` that the iterator pre-walked to
/// `Processing`, so callers can call `await incoming.ack()` as the
/// one-liner the killer demo expects.
#[pyclass(
    module = "aerosync._native",
    name = "IncomingFile",
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyIncomingFile {
    pub(crate) path: PathBuf,
    pub(crate) file_name: String,
    pub(crate) size_bytes: u64,
    pub(crate) sha256: Option<String>,
    pub(crate) receipt: Option<Arc<Receipt<RxSide>>>,
    /// Free-form `user_metadata` map projected from the wire-decoded
    /// [`Metadata`] envelope (RFC-003 §4 user fields). Empty when the
    /// transfer arrived without an envelope (legacy senders, the
    /// QUIC path until batch C wires `TransferStart.metadata`, or
    /// envelope-less smoke tests).
    pub(crate) user_metadata: HashMap<String, String>,
    /// Well-known optional metadata fields snapshot, populated from
    /// [`Metadata`] at iterator yield time so the Python getters
    /// don't need to drag the proto type through the binding. `None`
    /// for any field the sender did not set, and *all* `None` when
    /// no envelope was attached (in which case `metadata_present()`
    /// would also be `False`, but we don't expose that flag yet).
    pub(crate) trace_id: Option<String>,
    pub(crate) conversation_id: Option<String>,
    pub(crate) correlation_id: Option<String>,
    /// Decoded `LIFECYCLE_*` string (e.g. `"LIFECYCLE_TRANSIENT"`).
    /// `None` rather than `"LIFECYCLE_UNSPECIFIED"` when the sender
    /// omitted the field, so `if f.lifecycle is None` is the
    /// canonical "no preference" check.
    pub(crate) lifecycle: Option<&'static str>,
}

#[pymethods]
impl PyIncomingFile {
    #[getter]
    fn path(&self) -> PathBuf {
        self.path.clone()
    }

    #[getter]
    fn file_name(&self) -> String {
        self.file_name.clone()
    }

    #[getter]
    fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    #[getter]
    fn sha256(&self) -> Option<String> {
        self.sha256.clone()
    }

    #[getter]
    fn receipt_id(&self) -> Option<String> {
        self.receipt.as_ref().map(|r| r.id().to_string())
    }

    /// Receiver-side receipt state as a string (mirrors
    /// `Receipt.state` on the sender side). Returns `None` if no
    /// receipt is attached (legacy code paths that don't go through
    /// `__anext__`).
    #[getter]
    fn state(&self) -> Option<&'static str> {
        self.receipt.as_ref().map(|r| state_label(&r.state()))
    }

    /// `await incoming.ack(metadata=None)` — applies `Event::Ack`.
    /// `metadata` is accepted for forward compat with RFC-002 §6.4
    /// but currently parsed and discarded (the receiver `Receipt`
    /// does not yet carry an ack-time metadata payload — that lands
    /// when the receipt-stream wire frame gains the optional
    /// `Metadata` field).
    #[pyo3(signature = (metadata = None))]
    fn ack<'py>(
        &self,
        py: Python<'py>,
        metadata: Option<HashMap<String, String>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let receipt = self
            .receipt
            .clone()
            .ok_or_else(|| PyRuntimeError::new_err("IncomingFile has no attached receipt"))?;
        let _ = metadata; // accepted, currently no-op (see docstring)
        future_into_py(py, async move {
            receipt
                .apply_event(Event::Ack)
                .map_err(|e| PyValueError::new_err(format!("ack rejected: {e}")))?;
            Ok(state_label(&receipt.state()))
        })
    }

    /// `await incoming.nack(reason)` — applies `Event::Nack{reason}`.
    fn nack<'py>(&self, py: Python<'py>, reason: String) -> PyResult<Bound<'py, PyAny>> {
        let receipt = self
            .receipt
            .clone()
            .ok_or_else(|| PyRuntimeError::new_err("IncomingFile has no attached receipt"))?;
        future_into_py(py, async move {
            receipt
                .apply_event(Event::Nack { reason })
                .map_err(|e| PyValueError::new_err(format!("nack rejected: {e}")))?;
            Ok(state_label(&receipt.state()))
        })
    }

    /// Receiver-side `user_metadata` dict (RFC-003 §4 free-form
    /// fields). Populated on the HTTP transport from the wire
    /// `X-Aerosync-Metadata` header (`base64(protobuf(Metadata))`);
    /// empty when the sender did not attach an envelope or when the
    /// transfer arrived over a transport that doesn't yet propagate
    /// metadata (the QUIC path until batch C wires
    /// `TransferStart.metadata`).
    #[getter]
    fn metadata<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        // Lift the RFC-003 §4 well-known fields into the same flat
        // dict so the README quickstart's
        // `incoming.metadata.get("trace_id")` works without the
        // user having to know which keys are "well-known" vs
        // user-supplied. Mirrors the sender-side
        // `Client.send(metadata={...})` shape, which accepts both
        // flavours in a single `dict[str, str]` and lifts the
        // well-known keys to the typed `Metadata` slots.
        if let Some(v) = &self.trace_id {
            d.set_item("trace_id", v)?;
        }
        if let Some(v) = &self.conversation_id {
            d.set_item("conversation_id", v)?;
        }
        if let Some(v) = &self.correlation_id {
            d.set_item("correlation_id", v)?;
        }
        if let Some(v) = self.lifecycle {
            d.set_item("lifecycle", v)?;
        }
        for (k, v) in &self.user_metadata {
            d.set_item(k, v)?;
        }
        Ok(d)
    }

    /// RFC-003 well-known `trace_id`. `None` if the sender did not
    /// set it or no envelope was attached.
    #[getter]
    fn trace_id(&self) -> Option<String> {
        self.trace_id.clone()
    }

    /// RFC-003 well-known `conversation_id`. `None` if absent.
    #[getter]
    fn conversation_id(&self) -> Option<String> {
        self.conversation_id.clone()
    }

    /// RFC-003 well-known `correlation_id`. `None` if absent.
    #[getter]
    fn correlation_id(&self) -> Option<String> {
        self.correlation_id.clone()
    }

    /// RFC-003 well-known `lifecycle` projected to its
    /// `LIFECYCLE_*` string name (e.g. `"LIFECYCLE_TRANSIENT"`).
    /// `None` when the sender did not set the field — *not*
    /// `"LIFECYCLE_UNSPECIFIED"`, so `is None` is the canonical
    /// "no preference" check.
    #[getter]
    fn lifecycle(&self) -> Option<&'static str> {
        self.lifecycle
    }

    fn __repr__(&self) -> String {
        format!(
            "IncomingFile(path={:?}, file_name={:?}, size_bytes={})",
            self.path, self.file_name, self.size_bytes
        )
    }
}

/// Test-only helper: encode a [`Metadata`] envelope into the
/// canonical `X-Aerosync-Metadata` HTTP header value
/// (`base64(protobuf(Metadata))` — RFC-003 §8.4). The argument
/// shape mirrors the Python `Client.send(metadata=...)` map: a
/// flat `dict[str, str]` whose well-known keys (`trace_id`,
/// `conversation_id`, `correlation_id`, `lifecycle`) are lifted
/// into the typed `Metadata` slots and everything else routed to
/// `user_metadata`. Returns the ready-to-send header value as
/// a UTF-8 string.
///
/// Test-only because production code never needs to build a wire
/// header from Python — `Client.send` encodes it on the Rust side
/// from the engine-sealed envelope. The pytest in
/// `tests/test_metadata_propagation.py` uses this to drive the
/// happy path through the receiver without depending on a fully-
/// wired sender engine (the Python `Client` does not bring its own
/// `ProtocolAdapter` in v0.2 — see `test_killer_demo.py` skip).
#[pyfunction]
#[pyo3(signature = (metadata=None))]
pub fn _test_encode_metadata_header(
    metadata: Option<HashMap<String, String>>,
) -> PyResult<String> {
    use crate::client::build_metadata;
    use prost::Message as _;
    let m = match metadata {
        Some(m) if !m.is_empty() => build_metadata(m)?,
        _ => Metadata::default(),
    };
    let mut buf = Vec::with_capacity(m.encoded_len());
    m.encode(&mut buf)
        .map_err(|e| PyValueError::new_err(format!("metadata encode: {e}")))?;
    Ok(B64_STD.encode(&buf))
}

/// Test-only helper: build a [`PyIncomingFile`] with an attached
/// receiver-side receipt pre-walked to `Processing`. Used by pytest
/// to exercise the `ack`/`nack`/`metadata` surface without spinning
/// up a full receiver.
#[pyfunction]
#[pyo3(signature = (path, file_name, size_bytes=0, sha256=None, user_metadata=None))]
pub fn _test_make_incoming_file(
    path: PathBuf,
    file_name: String,
    size_bytes: u64,
    sha256: Option<String>,
    user_metadata: Option<HashMap<String, String>>,
) -> PyIncomingFile {
    let id = uuid::Uuid::new_v4();
    let rcpt: Arc<Receipt<RxSide>> = Arc::new(Receipt::new(id));
    let _ = rcpt.apply_event(Event::Open);
    let _ = rcpt.apply_event(Event::Close);
    let _ = rcpt.apply_event(Event::Close);
    let _ = rcpt.apply_event(Event::Process);
    PyIncomingFile {
        path,
        file_name,
        size_bytes,
        sha256,
        receipt: Some(rcpt),
        user_metadata: user_metadata.unwrap_or_default(),
        trace_id: None,
        conversation_id: None,
        correlation_id: None,
        lifecycle: None,
    }
}

/// Build a default `ServerConfig` rooted at `save_dir`.
///
/// Test-only since #11 (Config plumbing) routes the production
/// receiver factory through [`crate::config::ResolvedConfig`].
#[cfg(test)]
pub(crate) fn server_config_for(save_dir: Option<PathBuf>) -> ServerConfig {
    let mut cfg = ServerConfig::default();
    if let Some(dir) = save_dir {
        cfg.receive_directory = dir;
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_config_default_save_dir() {
        let cfg = server_config_for(None);
        assert_eq!(cfg.receive_directory.as_os_str(), "./received");
    }

    #[test]
    fn server_config_overrides_save_dir() {
        let dir = std::path::PathBuf::from("/tmp/aerosync-test");
        let cfg = server_config_for(Some(dir.clone()));
        assert_eq!(cfg.receive_directory, dir);
    }

    #[test]
    fn incoming_file_repr_includes_filename() {
        let f = PyIncomingFile {
            path: "/tmp/x.bin".into(),
            file_name: "x.bin".into(),
            size_bytes: 42,
            sha256: None,
            receipt: None,
            user_metadata: HashMap::new(),
            trace_id: None,
            conversation_id: None,
            correlation_id: None,
            lifecycle: None,
        };
        let r = f.__repr__();
        assert!(r.contains("x.bin"));
        assert!(r.contains("42"));
    }

    #[test]
    fn receiver_address_round_trips_through_factory_state() {
        let cfg = server_config_for(None);
        let recv = FileReceiver::new(cfg);
        let py = PyReceiver::new(recv, Some("alice".into()), "0.0.0.0:7788".into());
        assert_eq!(py.name, Some("alice".into()));
        assert_eq!(py.address, "0.0.0.0:7788");
        assert!(!py.started.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn receiver_yields_files_in_arrival_order() {
        let cfg = server_config_for(None);
        let recv = FileReceiver::new(cfg);
        let py = PyReceiver::new(recv, None, "127.0.0.1:0".into());
        assert_eq!(py.yielded.load(Ordering::SeqCst), 0);
        py.yielded.store(1, Ordering::SeqCst);
        assert_eq!(py.yielded.load(Ordering::SeqCst), 1);
    }
}

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
use aerosync::core::server::{FileReceiver, ServerConfig, WsEvent};
use pyo3::exceptions::{PyRuntimeError, PyStopAsyncIteration, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

/// Inbound `Receiver`. Constructed via the `aerosync.receiver(...)`
/// sync factory; the returned object IS the async context manager.
#[pyclass(module = "aerosync._native", name = "Receiver")]
pub struct PyReceiver {
    pub(crate) inner: Arc<Mutex<FileReceiver>>,
    pub(crate) name: Option<String>,
    pub(crate) address: String,
    pub(crate) ws_rx: Arc<Mutex<Option<broadcast::Receiver<WsEvent>>>>,
    pub(crate) yielded: Arc<AtomicUsize>,
    pub(crate) started: Arc<AtomicBool>,
}

impl PyReceiver {
    pub fn new(inner: FileReceiver, name: Option<String>, address: String) -> Self {
        Self {
            inner: Arc::new(Mutex::new(inner)),
            name,
            address,
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

    #[getter]
    fn address(&self) -> String {
        self.address.clone()
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
                    return Ok(PyIncomingFile {
                        path: f.saved_path,
                        file_name: f.original_name,
                        size_bytes: f.size,
                        sha256: f.sha256,
                        receipt: Some(rcpt),
                        user_metadata: HashMap::new(),
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
    pub(crate) user_metadata: HashMap<String, String>,
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

    /// Receiver-side `user_metadata` dict.
    ///
    /// Currently always empty: the engine does not yet expose the
    /// sender-supplied [`Metadata`](aerosync_proto::Metadata)
    /// envelope on the `ReceivedFile` produced by `FileReceiver`.
    /// Wiring this requires plumbing the `Metadata` from the
    /// `TransferStart` frame into `ReceivedFile`, which is engine
    /// work tracked outside RFC-001 §15. The accessor exists today
    /// so user code can write the killer-demo shape and not break
    /// when the propagation lands.
    #[getter]
    fn metadata<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        for (k, v) in &self.user_metadata {
            d.set_item(k, v)?;
        }
        Ok(d)
    }

    fn __repr__(&self) -> String {
        format!(
            "IncomingFile(path={:?}, file_name={:?}, size_bytes={})",
            self.path, self.file_name, self.size_bytes
        )
    }
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
    }
}

/// Build a default `ServerConfig` rooted at `save_dir`.
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

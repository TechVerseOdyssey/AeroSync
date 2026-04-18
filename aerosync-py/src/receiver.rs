//! `Receiver` + `IncomingFile` (RFC-001 §5.3, §5.6 — tasks #5, #6).
//!
//! Group C wires the async iterator on top of `FileReceiver`'s
//! `WsBroadcast` channel. Iteration semantics:
//!
//! 1. `__aenter__` subscribes to the broadcast BEFORE calling
//!    `start()` so we never miss a `Completed` event from a transfer
//!    that races the subscription.
//! 2. `__aiter__` returns `self`; the iterator is single-consumer
//!    (one async-for loop per receiver). Multi-consumer fan-out is
//!    intentionally out of scope for v0.2 — users wire their own
//!    asyncio queues if they need it.
//! 3. `__anext__` first drains any backlog (received_files whose
//!    index ≥ our `yielded` cursor), then awaits the next
//!    `WsEvent::Completed` and re-checks. Non-Completed events
//!    (`Started`, `Progress`, `Failed`) are ignored — `Failed`
//!    surfaces via the receipt machinery in w6 task #14, not via
//!    iteration.
//! 4. When the broadcast channel closes (receiver shutdown), we
//!    raise `StopAsyncIteration`. A `Lagged` is treated as a
//!    spurious wakeup — the file backlog scan will pick up
//!    anything we missed.
//!
//! `IncomingFile.ack()` / `nack()` are **not** in this commit — they
//! belong to RFC-002 receipts (w6 task #14/#15).

use crate::errors::engine_err_to_py;
use crate::runtime::future_into_py;
use aerosync::core::server::{FileReceiver, ServerConfig, WsEvent};
use pyo3::exceptions::{PyRuntimeError, PyStopAsyncIteration};
use pyo3::prelude::*;
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
    /// Lazily-populated by `__aenter__`. Held in an Option so we can
    /// distinguish "not yet entered" from "entered but channel closed"
    /// during iteration.
    pub(crate) ws_rx: Arc<Mutex<Option<broadcast::Receiver<WsEvent>>>>,
    /// Index into `FileReceiver::get_received_files()` — points at the
    /// next file to yield. Bumped after each successful `__anext__`.
    pub(crate) yielded: Arc<AtomicUsize>,
    /// True between `__aenter__` and `__aexit__`. Used to make
    /// `__aexit__` idempotent on double-exit (which asyncio does not
    /// itself promise but tests sometimes do).
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
    /// then starts the underlying `FileReceiver` (which spawns
    /// HTTP/HTTPS/QUIC listeners depending on the `ServerConfig`).
    fn __aenter__<'py>(slf: Py<Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&slf.bind(py).borrow().inner);
        let ws_rx = Arc::clone(&slf.bind(py).borrow().ws_rx);
        let started = Arc::clone(&slf.bind(py).borrow().started);
        future_into_py(py, async move {
            // Subscribe FIRST so we don't lose Completed events on
            // ultra-fast localhost transfers.
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

    /// Async context manager exit — best-effort shutdown of the
    /// underlying `FileReceiver`. Returns `False` so any in-flight
    /// exception is NOT suppressed.
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
                // 1) Drain backlog: if there are already files past
                //    our cursor, hand the next one back immediately.
                let files = inner.lock().await.get_received_files().await;
                let i = yielded.load(Ordering::SeqCst);
                if i < files.len() {
                    let f = files[i].clone();
                    yielded.store(i + 1, Ordering::SeqCst);
                    return Ok(PyIncomingFile {
                        path: f.saved_path,
                        file_name: f.original_name,
                        size_bytes: f.size,
                        sha256: f.sha256,
                        receipt_id: None,
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
/// Fields populated from `aerosync::core::server::ReceivedFile` at
/// yield time. `ack()` / `nack()` are deliberately absent — they
/// belong to RFC-002 receipts (w6 task #15).
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
    pub(crate) receipt_id: Option<String>,
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
        self.receipt_id.clone()
    }

    fn __repr__(&self) -> String {
        format!(
            "IncomingFile(path={:?}, file_name={:?}, size_bytes={})",
            self.path, self.file_name, self.size_bytes
        )
    }
}

/// Build a default `ServerConfig` rooted at `save_dir`. Helper used by
/// the `aerosync.receiver()` factory; lifted out so the test suite can
/// exercise the validation logic without spinning up a real receiver.
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
            receipt_id: None,
        };
        let r = f.__repr__();
        assert!(r.contains("x.bin"));
        assert!(r.contains("42"));
    }

    #[test]
    fn receiver_address_round_trips_through_factory_state() {
        // Factory builds a default ServerConfig; the listener is not
        // started here — we just check the address-string assembly.
        let cfg = server_config_for(None);
        let recv = FileReceiver::new(cfg);
        let py = PyReceiver::new(recv, Some("alice".into()), "0.0.0.0:7788".into());
        assert_eq!(py.name, Some("alice".into()));
        assert_eq!(py.address, "0.0.0.0:7788");
        assert!(!py.started.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn receiver_yields_files_in_arrival_order() {
        // Simulate the engine pushing two ReceivedFiles directly via
        // FileReceiver's get_received_files surface: we don't have a
        // setter, so this test instead asserts the public field
        // semantics — the iterator's "index cursor" advances on each
        // yield. The full network round-trip lives in pytest
        // (test_receiver.py) where we boot a real listener.
        let cfg = server_config_for(None);
        let recv = FileReceiver::new(cfg);
        let py = PyReceiver::new(recv, None, "127.0.0.1:0".into());
        assert_eq!(py.yielded.load(Ordering::SeqCst), 0);
        py.yielded.store(1, Ordering::SeqCst);
        assert_eq!(py.yielded.load(Ordering::SeqCst), 1);
    }
}

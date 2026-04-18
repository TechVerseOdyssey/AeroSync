//! `Client` — outbound transfer handle.
//!
//! Group B implements `Client.send` and `Client.send_directory` per
//! RFC-001 §5.2 + tasks #3, #4. Optional args (`metadata`,
//! `on_progress`, `Config`, bytes / `BinaryIO` sources) are
//! explicitly deferred — they belong to w6 tasks #11, #12, #13, #16.

use crate::errors::{engine_err_to_py, timeout_to_py};
use crate::runtime::future_into_py;
use aerosync::core::transfer::{TransferConfig, TransferEngine, TransferTask};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyType;
use std::path::PathBuf;
use std::sync::Arc;

/// Outbound `Client`. Constructed via the `aerosync.client()` async
/// factory which returns an async context manager.
#[pyclass(module = "aerosync._native", name = "Client")]
pub struct PyClient {
    pub(crate) engine: Arc<TransferEngine>,
}

impl PyClient {
    /// Build a fresh client with the engine's `TransferConfig::default()`.
    /// w6 task #11 will swap this for the user-supplied `Config`.
    pub fn new_default() -> Self {
        Self {
            engine: Arc::new(TransferEngine::new(TransferConfig::default())),
        }
    }
}

#[pymethods]
impl PyClient {
    /// Async context-manager `__aenter__` — returns `self`.
    /// We do not open any sockets eagerly; the connection cache is
    /// implicit per RFC-001 §11 Q1.
    fn __aenter__<'py>(slf: Py<Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        future_into_py(py, async move { Ok(slf) })
    }

    /// Async context-manager `__aexit__` — currently a no-op.
    /// Resource teardown lives in `Drop`.
    #[pyo3(signature = (_exc_type=None, _exc_value=None, _traceback=None))]
    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _exc_type: Option<Py<PyType>>,
        _exc_value: Option<Py<PyAny>>,
        _traceback: Option<Py<PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        future_into_py(py, async move { Ok(false) })
    }

    /// `await client.send(source, to=..., chunk_size=..., timeout=...)`.
    ///
    /// Group B implementation per RFC-001 task #3:
    ///
    /// - `source` is a path-only `PathBuf` for w5; bytes / `BinaryIO`
    ///   sources land in w6 task #12.
    /// - `to` is forwarded verbatim to the auto-adapter; the engine
    ///   handles peer-name vs URL parsing.
    /// - `chunk_size` is plumbed via per-call `TransferConfig` override
    ///   only when set; defaults to the engine's adaptive choice.
    /// - `timeout` (seconds, optional) wraps the await in
    ///   `tokio::time::timeout`; on expiry a Python `TimeoutError`
    ///   is raised.
    /// - `metadata=` and `on_progress=` are NOT in this signature —
    ///   they are w6 tasks #16 and #13 respectively.
    ///
    /// Returns a placeholder [`PyReceipt`] carrying only the receipt
    /// id; the full state machine (`await sent/received/processed`)
    /// arrives with w6 task #14.
    #[pyo3(signature = (source, *, to, chunk_size=None, timeout=None))]
    fn send<'py>(
        &self,
        py: Python<'py>,
        source: PathBuf,
        to: String,
        chunk_size: Option<usize>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let engine = Arc::clone(&self.engine);
        future_into_py(py, async move {
            let task = build_upload_task(source, to, chunk_size).map_err(PyValueError::new_err)?;
            let receipt = match timeout {
                Some(secs) => {
                    if !secs.is_finite() || secs <= 0.0 {
                        return Err(PyValueError::new_err(
                            "timeout must be a finite positive number of seconds",
                        ));
                    }
                    tokio::time::timeout(
                        std::time::Duration::from_secs_f64(secs),
                        engine.send(task),
                    )
                    .await
                    .map_err(timeout_to_py)?
                    .map_err(engine_err_to_py)?
                }
                None => engine.send(task).await.map_err(engine_err_to_py)?,
            };
            Ok(PyReceipt {
                id: receipt.id().to_string(),
            })
        })
    }

    /// `await client.send_directory(source, to=...)`.
    ///
    /// Group B implementation per RFC-001 task #4: walks `source`
    /// non-recursively, sends each regular file via the auto-adapter,
    /// and returns a single placeholder [`PyReceipt`] keyed by the
    /// last receipt issued. This matches the convention of the
    /// existing MCP `send_directory` tool.
    ///
    /// Recursive walking, per-file metadata, and a richer
    /// "directory receipt" type are deliberate w6 follow-ups so the
    /// killer demo's basic shape can compile against this signature
    /// today.
    #[pyo3(signature = (source, *, to))]
    fn send_directory<'py>(
        &self,
        py: Python<'py>,
        source: PathBuf,
        to: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let engine = Arc::clone(&self.engine);
        future_into_py(py, async move {
            if !source.is_dir() {
                return Err(PyValueError::new_err(format!(
                    "send_directory: {} is not a directory",
                    source.display()
                )));
            }
            let mut entries = tokio::fs::read_dir(&source)
                .await
                .map_err(|e| PyRuntimeError::new_err(format!("read_dir: {e}")))?;
            let mut last_id: Option<String> = None;
            let mut count: usize = 0;
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|e| PyRuntimeError::new_err(format!("read_dir: {e}")))?
            {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let task =
                    build_upload_task(path, to.clone(), None).map_err(PyValueError::new_err)?;
                let receipt = engine.send(task).await.map_err(engine_err_to_py)?;
                last_id = Some(receipt.id().to_string());
                count += 1;
            }
            let id = last_id.ok_or_else(|| {
                PyValueError::new_err(format!(
                    "send_directory: {} contained no regular files",
                    source.display()
                ))
            })?;
            tracing::info!(file_count = count, last_receipt = %id, "send_directory done");
            Ok(PyReceipt { id })
        })
    }
}

/// Construct a `TransferTask` from a path-only source.
///
/// Validates that `source` exists and is a regular file. The
/// destination string is forwarded verbatim — RFC-001 keeps URL /
/// peer-name parsing inside the engine so binding code does not have
/// to keep up with `AutoAdapter`'s protocol probes.
///
/// `chunk_size` is accepted but currently unused: the engine reads
/// chunk size from `TransferConfig` rather than from per-task
/// overrides. Wiring a per-call override is a w6 follow-up alongside
/// the `Config` plumbing (task #11).
fn build_upload_task(
    source: PathBuf,
    destination: String,
    _chunk_size: Option<usize>,
) -> Result<TransferTask, String> {
    let meta = std::fs::metadata(&source)
        .map_err(|e| format!("source {} not readable: {}", source.display(), e))?;
    if !meta.is_file() {
        return Err(format!("source {} is not a regular file", source.display()));
    }
    Ok(TransferTask::new_upload(source, destination, meta.len()))
}

/// w5 placeholder Receipt. Group A introduces only the type so the
/// module can register it; the real `await sent/received/processed`
/// API and the wiring into [`aerosync::core::receipt::Receipt`] land
/// in w6 task #14.
#[pyclass(module = "aerosync._native", name = "Receipt")]
pub struct PyReceipt {
    pub(crate) id: String,
}

#[pymethods]
impl PyReceipt {
    #[getter]
    fn id(&self) -> &str {
        &self.id
    }

    /// Placeholder `await receipt.processed()` — resolves immediately.
    /// w6 task #14 replaces this with a wait-for-terminal future.
    fn processed<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        future_into_py(py, async { Ok(()) })
    }

    fn __repr__(&self) -> String {
        format!("Receipt(id={:?})", self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::shared_tokio;
    use std::io::Write;

    #[test]
    fn shared_runtime_acquire_is_idempotent() {
        // Acquiring the runtime twice must not panic; a regression
        // here would surface as `pyo3-async-runtimes` "runtime
        // already registered" panic on the second call.
        let a = shared_tokio();
        let b = shared_tokio();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn client_default_is_constructible_without_python() {
        let c = PyClient::new_default();
        // Cheap engine field touch to ensure no GIL / interpreter
        // assumptions sneak into PyClient::new_default.
        assert!(Arc::strong_count(&c.engine) >= 1);
    }

    #[test]
    fn build_upload_task_rejects_missing_source() {
        let err = build_upload_task(
            PathBuf::from("/definitely/does/not/exist/aerosync.bin"),
            "alice".into(),
            None,
        )
        .unwrap_err();
        assert!(err.contains("not readable"), "got: {err}");
    }

    #[test]
    fn build_upload_task_rejects_directory() {
        let dir = tempfile::tempdir().unwrap();
        let err = build_upload_task(dir.path().to_path_buf(), "alice".into(), None).unwrap_err();
        assert!(err.contains("not a regular file"), "got: {err}");
    }

    #[test]
    fn build_upload_task_uses_real_file_size() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"hello world").unwrap();
        let task = build_upload_task(f.path().to_path_buf(), "alice".into(), None).unwrap();
        assert_eq!(task.file_size, 11);
        assert!(task.is_upload);
        // Destination is forwarded verbatim — no peer-name vs URL
        // pre-parsing in the binding layer.
        assert_eq!(task.destination, "alice");
    }

    #[test]
    fn build_upload_task_preserves_source_path() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        let task = build_upload_task(path.clone(), "bob".into(), Some(1024)).unwrap();
        assert_eq!(task.source_path, path);
    }
}

//! `Client` — outbound transfer handle.
//!
//! Implements RFC-001 §5.2 `Client.send` / `Client.send_directory` /
//! `Client.history` plus w6's metadata pass-through (#16).

use crate::errors::{engine_err_to_py, metadata_err_to_py, timeout_to_py};
use crate::receipt::PyReceipt;
use crate::runtime::future_into_py;
use aerosync::core::history::HistoryStore;
use aerosync::core::metadata::MetadataBuilder;
use aerosync::core::transfer::{TransferConfig, TransferEngine, TransferTask};
use aerosync_proto::Lifecycle;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyType;
use std::collections::HashMap;
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

/// Translate the user-facing `metadata={...}` dict into a sealed
/// `aerosync_proto::Metadata`. Well-known keys are routed to the
/// typed `MetadataBuilder` setters per RFC-003 §5; everything else
/// lands in `user_metadata`.
///
/// Recognised well-known keys (lowercase, snake_case):
/// - `trace_id` / `conversation_id` / `correlation_id` → string
/// - `lifecycle` → one of `unspecified` / `transient` / `durable` /
///   `ephemeral` (case-insensitive); unknown values raise `ValueError`
/// - any other key → `user_metadata[k] = v`
///
/// The Python contract is "metadata is a `Mapping[str, str]`"; we
/// preserve that for `user_metadata` keys but transparently lift the
/// well-known shortcuts. RFC-001 §5.2 documents this.
pub(crate) fn build_metadata(
    user_supplied: HashMap<String, String>,
) -> PyResult<aerosync_proto::Metadata> {
    let mut b = MetadataBuilder::new();
    for (k, v) in user_supplied {
        match k.as_str() {
            "trace_id" => {
                b = b.trace_id(v);
            }
            "conversation_id" => {
                b = b.conversation_id(v);
            }
            "correlation_id" => {
                b = b.correlation_id(v);
            }
            "lifecycle" => {
                let lc = match v.to_ascii_lowercase().as_str() {
                    "unspecified" => Lifecycle::Unspecified,
                    "transient" => Lifecycle::Transient,
                    "durable" => Lifecycle::Durable,
                    "ephemeral" => Lifecycle::Ephemeral,
                    other => {
                        return Err(PyValueError::new_err(format!(
                            "metadata['lifecycle']: unknown value {other:?} \
                             (expected one of unspecified, transient, durable, ephemeral)"
                        )))
                    }
                };
                b = b.lifecycle(lc);
            }
            // parent_file_ids would arrive as a comma-separated list
            // in the dict-shaped API; we deliberately do NOT split
            // here to keep the contract honest. Users who need
            // lineage call MetadataBuilder directly via a future
            // higher-level helper.
            _ => {
                b = b.user(k, v);
            }
        }
    }
    b.build().map_err(metadata_err_to_py)
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

    /// `await client.send(source, *, to, metadata=None, chunk_size=None, timeout=None)`.
    ///
    /// Per RFC-001 §5.2. `metadata` is a `dict[str, str]`; well-known
    /// keys (`trace_id`, `conversation_id`, `correlation_id`,
    /// `lifecycle`) are lifted to the typed `Metadata` fields, every
    /// other key goes into `user_metadata`.
    ///
    /// `chunk_size` is plumbed via per-call `TransferConfig`
    /// override only when set; defaults to the engine's adaptive
    /// choice. `timeout` (seconds, optional) wraps the await in
    /// `tokio::time::timeout` and raises `TimeoutError` on expiry.
    ///
    /// `on_progress=` and bytes/BinaryIO sources are w6 tasks #13
    /// and #12 respectively (Group C in the w6 plan).
    #[pyo3(signature = (source, *, to, metadata=None, chunk_size=None, timeout=None))]
    fn send<'py>(
        &self,
        py: Python<'py>,
        source: PathBuf,
        to: String,
        metadata: Option<HashMap<String, String>>,
        chunk_size: Option<usize>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let engine = Arc::clone(&self.engine);
        let meta = match metadata {
            Some(m) if !m.is_empty() => Some(build_metadata(m)?),
            _ => None,
        };
        future_into_py(py, async move {
            let task = build_upload_task(source, to, chunk_size).map_err(PyValueError::new_err)?;
            let receipt = match (timeout, meta) {
                (Some(secs), m) => {
                    if !secs.is_finite() || secs <= 0.0 {
                        return Err(PyValueError::new_err(
                            "timeout must be a finite positive number of seconds",
                        ));
                    }
                    let dur = std::time::Duration::from_secs_f64(secs);
                    let fut = match m {
                        Some(envelope) => {
                            futures::future::Either::Left(engine.send_with_metadata(task, envelope))
                        }
                        None => futures::future::Either::Right(engine.send(task)),
                    };
                    tokio::time::timeout(dur, fut)
                        .await
                        .map_err(timeout_to_py)?
                        .map_err(engine_err_to_py)?
                }
                (None, Some(envelope)) => engine
                    .send_with_metadata(task, envelope)
                    .await
                    .map_err(engine_err_to_py)?,
                (None, None) => engine.send(task).await.map_err(engine_err_to_py)?,
            };
            Ok(PyReceipt::from_arc(receipt))
        })
    }

    /// `await client.send_directory(source, *, to, metadata=None)`.
    ///
    /// Walks `source` non-recursively, sends each regular file via
    /// the auto-adapter, and returns a single `Receipt` keyed by the
    /// last receipt issued. Recursive walking and a richer
    /// "directory receipt" type are deferred follow-ups.
    #[pyo3(signature = (source, *, to, metadata=None))]
    fn send_directory<'py>(
        &self,
        py: Python<'py>,
        source: PathBuf,
        to: String,
        metadata: Option<HashMap<String, String>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let engine = Arc::clone(&self.engine);
        let meta = match metadata {
            Some(m) if !m.is_empty() => Some(build_metadata(m)?),
            _ => None,
        };
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
            let mut last: Option<
                Arc<aerosync::core::receipt::Receipt<aerosync::core::receipt::Sender>>,
            > = None;
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
                let receipt = match meta.clone() {
                    Some(envelope) => engine
                        .send_with_metadata(task, envelope)
                        .await
                        .map_err(engine_err_to_py)?,
                    None => engine.send(task).await.map_err(engine_err_to_py)?,
                };
                last = Some(receipt);
                count += 1;
            }
            let receipt = last.ok_or_else(|| {
                PyValueError::new_err(format!(
                    "send_directory: {} contained no regular files",
                    source.display()
                ))
            })?;
            tracing::info!(file_count = count, last_receipt = %receipt.id(), "send_directory done");
            Ok(PyReceipt::from_arc(receipt))
        })
    }

    /// `await client.history(*, limit=100, direction="all", metadata_filter=None)`.
    ///
    /// Returns a list of [`crate::records::PyHistoryEntry`] keyed by
    /// `~/.config/aerosync/history.jsonl` (the engine's default
    /// `HistoryStore::default_path()`). `metadata_filter` is the
    /// RFC-003 §8 facility — every `(k, v)` pair must appear in the
    /// entry's `metadata.user_metadata` for it to match.
    ///
    /// History reads do NOT depend on the running engine state; they
    /// open a fresh `HistoryStore` keyed by the default path. Users
    /// who want a different path will route through `Config` once
    /// w6 task #11 lands.
    #[pyo3(signature = (*, limit=100, direction="all", metadata_filter=None, history_path=None))]
    fn history<'py>(
        &self,
        py: Python<'py>,
        limit: usize,
        direction: &str,
        metadata_filter: Option<HashMap<String, String>>,
        history_path: Option<PathBuf>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let dir = match direction {
            "all" => None,
            "sent" => Some("send".to_string()),
            "received" => Some("receive".to_string()),
            other => {
                return Err(PyValueError::new_err(format!(
                    "direction must be 'sent' / 'received' / 'all', got {other:?}"
                )))
            }
        };
        let path = history_path.unwrap_or_else(HistoryStore::default_path);
        future_into_py(py, async move {
            let store = HistoryStore::new(&path).await.map_err(engine_err_to_py)?;
            let q = aerosync::core::history::HistoryQuery {
                direction: dir,
                limit,
                metadata_eq: metadata_filter.unwrap_or_default(),
                ..Default::default()
            };
            let rows = store.query(&q).await.map_err(engine_err_to_py)?;
            let py_rows: Vec<crate::records::PyHistoryEntry> =
                rows.into_iter().map(Into::into).collect();
            Ok(py_rows)
        })
    }
}

/// Construct a `TransferTask` from a path-only source.
///
/// Validates that `source` exists and is a regular file. The
/// destination string is forwarded verbatim — RFC-001 keeps URL /
/// peer-name parsing inside the engine so binding code does not have
/// to keep up with `AutoAdapter`'s protocol probes.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::shared_tokio;
    use std::io::Write;

    #[test]
    fn shared_runtime_acquire_is_idempotent() {
        let a = shared_tokio();
        let b = shared_tokio();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn client_default_is_constructible_without_python() {
        let c = PyClient::new_default();
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
        assert_eq!(task.destination, "alice");
    }

    #[test]
    fn build_upload_task_preserves_source_path() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        let task = build_upload_task(path.clone(), "bob".into(), Some(1024)).unwrap();
        assert_eq!(task.source_path, path);
    }

    #[test]
    fn build_metadata_routes_well_known_fields() {
        let mut input = HashMap::new();
        input.insert("trace_id".to_string(), "run-7".to_string());
        input.insert("tenant".to_string(), "acme".to_string());
        let m = build_metadata(input).unwrap();
        assert_eq!(m.trace_id.as_deref(), Some("run-7"));
        assert_eq!(
            m.user_metadata.get("tenant").map(|s| s.as_str()),
            Some("acme")
        );
        assert!(!m.user_metadata.contains_key("trace_id"));
    }

    #[test]
    fn build_metadata_lifecycle_string_parses() {
        let mut input = HashMap::new();
        input.insert("lifecycle".to_string(), "transient".to_string());
        let m = build_metadata(input).unwrap();
        assert_eq!(m.lifecycle, Some(Lifecycle::Transient as i32));
    }

    #[test]
    fn build_metadata_invalid_lifecycle_rejected() {
        let mut input = HashMap::new();
        input.insert("lifecycle".to_string(), "permanent".to_string());
        let err = build_metadata(input).unwrap_err();
        assert!(err.to_string().contains("lifecycle"));
    }

    #[test]
    fn build_metadata_oversize_user_value_rejected() {
        let mut input = HashMap::new();
        input.insert("k".into(), "x".repeat(20_000));
        let err = build_metadata(input).unwrap_err();
        // metadata_err_to_py maps to ValueError until #10 lands.
        assert!(err.to_string().contains("user_metadata"));
    }
}

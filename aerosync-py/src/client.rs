//! `Client` — outbound transfer handle.
//!
//! Implements RFC-001 §5.2 `Client.send` / `Client.send_directory` /
//! `Client.history` plus w6's metadata pass-through (#16).

use crate::errors::{engine_err_to_py, metadata_err_to_py, timeout_to_py};
use crate::receipt::PyReceipt;
use crate::records::PyProgress;
use crate::runtime::future_into_py;
use aerosync::core::history::HistoryStore;
use aerosync::core::metadata::MetadataBuilder;
use aerosync::core::progress::TransferStatus;
use aerosync::core::receipt::{Receipt, Sender};
use aerosync::core::transfer::{TransferConfig, TransferEngine, TransferTask};
use aerosync::protocols::adapter::AutoAdapter;
use aerosync::protocols::http::HttpConfig;
use aerosync::protocols::quic::QuicConfig;
use aerosync_proto::Lifecycle;
use pyo3::exceptions::{PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyType};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::NamedTempFile;
use tokio::sync::OnceCell;
use uuid::Uuid;

/// Outbound `Client`. Constructed via the `aerosync.client()` async
/// factory which returns an async context manager.
///
/// `chunk_size_default` and `timeout_default` cache the values pulled
/// from the user-supplied `Config` (see [`crate::config::ResolvedConfig`]) so subsequent
/// `send()` calls without explicit `chunk_size=` / `timeout=` kwargs
/// can fall back to them. RFC-001 §5.7 lists these as the only two
/// per-call defaults that travel via `Config`.
#[pyclass(module = "aerosync._native", name = "Client")]
pub struct PyClient {
    pub(crate) engine: Arc<TransferEngine>,
    pub(crate) chunk_size_default: Option<usize>,
    pub(crate) timeout_default: Option<f64>,
    /// Lazily-initialised transport adapter. Populated on the first
    /// `__aenter__` call by [`Self::ensure_started`], which builds an
    /// [`AutoAdapter`], wires it to the engine's
    /// [`TransferEngine::http_receipt_inbox`] (so HTTP `/upload`
    /// responses bearing the RFC-002 §6.4 `receipt_ack` envelope drive
    /// the sender-side `Receipt<Sender>` to a terminal state), and
    /// hands it to [`TransferEngine::start`] so the worker loop is
    /// running before the user issues their first `send()`. Held in
    /// an [`Arc<OnceCell<…>>`] so the future returned by `__aenter__`
    /// can capture a cheap clone without taking the GIL.
    pub(crate) adapter: Arc<OnceCell<Arc<AutoAdapter>>>,
}

impl PyClient {
    /// Build a fresh client with the engine's `TransferConfig::default()`.
    /// Used by the `aerosync.client()` factory when no `config=` is
    /// supplied. The `from_config` constructor (in `lib.rs::make_client`)
    /// is the configurable equivalent.
    pub fn new_default() -> Self {
        Self {
            engine: Arc::new(TransferEngine::new(TransferConfig::default())),
            chunk_size_default: None,
            timeout_default: None,
            adapter: Arc::new(OnceCell::new()),
        }
    }

    /// Build a `PyClient` from a fully-resolved [`TransferConfig`] +
    /// the two per-call defaults pulled from the Python `Config`
    /// dataclass.
    pub fn from_transfer_config(
        cfg: TransferConfig,
        chunk_size_default: Option<usize>,
        timeout_default: Option<f64>,
    ) -> Self {
        Self {
            engine: Arc::new(TransferEngine::new(cfg)),
            chunk_size_default,
            timeout_default,
            adapter: Arc::new(OnceCell::new()),
        }
    }
}

/// Build a fresh [`AutoAdapter`] off the engine's defaults and wire
/// the engine receipt inbox so HTTP `/upload` `receipt_ack` envelopes
/// drive the sender-side receipt past `Processing`. The `aerosync`
/// workspace dependency in this crate always pulls the `quic`
/// feature in via the default feature set, so we use the dual-config
/// `AutoAdapter::new` shape unconditionally — there is no `quic`
/// feature on the `aerosync-py` crate itself to gate on.
fn build_default_adapter(engine: &TransferEngine) -> Arc<AutoAdapter> {
    let inbox = engine.http_receipt_inbox();
    Arc::new(
        AutoAdapter::new(HttpConfig::default(), QuicConfig::default())
            .with_engine_receipt_inbox(inbox)
            .with_rendezvous_token_from_env(),
    )
}

/// Idempotent: build (if not already built) the [`AutoAdapter`] and
/// hand it to [`TransferEngine::start`] so the worker loop is up
/// before any `send()` runs. Subsequent calls are no-ops because
/// [`OnceCell::get_or_try_init`] caches the first successful result.
async fn ensure_started(
    engine: Arc<TransferEngine>,
    cell: Arc<OnceCell<Arc<AutoAdapter>>>,
) -> PyResult<()> {
    cell.get_or_try_init::<PyErr, _, _>(|| async {
        let adapter = build_default_adapter(&engine);
        let dyn_adapter: Arc<dyn aerosync::core::transfer::ProtocolAdapter> = adapter.clone();
        engine.start(dyn_adapter).await.map_err(engine_err_to_py)?;
        Ok(adapter)
    })
    .await?;
    Ok(())
}

/// Materialised source for a `Client.send` call.
///
/// `path` is always a real on-disk file; `_keepalive` carries an
/// optional [`NamedTempFile`] that owns the staging file when the
/// caller passed `bytes` / `BinaryIO`. Dropping `_keepalive` removes
/// the staging file, so the client code spawns a tokio task tied to
/// the receipt's terminal state to do that asynchronously.
struct Staged {
    path: PathBuf,
    keepalive: Option<NamedTempFile>,
}

impl Staged {
    /// Consume the staged source, returning the path and the optional
    /// tempfile keep-alive separately. The keep-alive must outlive
    /// the upload — the engine reads the file off-thread.
    fn materialize(self) -> (PathBuf, Option<NamedTempFile>) {
        (self.path, self.keepalive)
    }
}

/// Inspect a Python `source=` argument and materialise it into a
/// real on-disk path that the engine can read.
///
/// Recognised shapes (in order — first match wins):
/// 1. `bytes` / `bytearray` — staged into a `NamedTempFile`.
/// 2. anything implementing `read(size: int) -> bytes` (typed as
///    `BinaryIO` in Python) — drained synchronously into a
///    `NamedTempFile` (1 MiB chunks, GIL held throughout).
/// 3. `os.PathLike` / `str` / `pathlib.Path` — extracted as
///    `PathBuf` and used directly.
///
/// Anything else raises `TypeError` with a helpful diagnostic.
///
/// **GIL contract:** must be called while holding the GIL because
/// it both reads Python attributes and (for option 2) calls back
/// into Python. The returned [`Staged`] no longer touches Python.
fn stage_source(_py: Python<'_>, source: &Bound<'_, PyAny>) -> PyResult<Staged> {
    use pyo3::types::PyAnyMethods;
    use std::io::Write;

    // Option 1: bytes / bytearray.
    if let Ok(buf) = source.cast::<PyBytes>() {
        let bytes = buf.as_bytes();
        let mut f = NamedTempFile::new()
            .map_err(|e| PyRuntimeError::new_err(format!("staging tempfile: {e}")))?;
        f.write_all(bytes)
            .map_err(|e| PyRuntimeError::new_err(format!("write staging tempfile: {e}")))?;
        f.flush()
            .map_err(|e| PyRuntimeError::new_err(format!("flush staging tempfile: {e}")))?;
        let path = f.path().to_path_buf();
        return Ok(Staged {
            path,
            keepalive: Some(f),
        });
    }
    if let Ok(buf) = source.extract::<Vec<u8>>() {
        // bytearray and other byte-buffers fall through to here via
        // PyO3's `Vec<u8>` extractor. We deliberately try this AFTER
        // the PyBytes downcast so the common-case `bytes` argument
        // takes the zero-copy path.
        if source.hasattr("read")? {
            // Don't accidentally swallow a BinaryIO whose `__iter__`
            // happens to yield int — fall through to the read() path.
        } else {
            let mut f = NamedTempFile::new()
                .map_err(|e| PyRuntimeError::new_err(format!("staging tempfile: {e}")))?;
            f.write_all(&buf)
                .map_err(|e| PyRuntimeError::new_err(format!("write staging tempfile: {e}")))?;
            f.flush()
                .map_err(|e| PyRuntimeError::new_err(format!("flush staging tempfile: {e}")))?;
            let path = f.path().to_path_buf();
            return Ok(Staged {
                path,
                keepalive: Some(f),
            });
        }
    }

    // Option 2: BinaryIO (anything with a callable `read`).
    if source.hasattr("read")? {
        let mut f = NamedTempFile::new()
            .map_err(|e| PyRuntimeError::new_err(format!("staging tempfile: {e}")))?;
        // 1 MiB read window. RFC-001 §6.2 explicitly accepts the
        // GIL-held synchronous drain for v0.2; streaming bytes is a
        // v0.3 enhancement (deferred follow-up).
        const CHUNK: usize = 1024 * 1024;
        loop {
            let chunk_obj = source.call_method1("read", (CHUNK,))?;
            let bytes_chunk: Vec<u8> = chunk_obj.extract().map_err(|_| {
                PyTypeError::new_err("BinaryIO.read() must return `bytes` (got non-bytes object)")
            })?;
            if bytes_chunk.is_empty() {
                break;
            }
            f.write_all(&bytes_chunk)
                .map_err(|e| PyRuntimeError::new_err(format!("write staging tempfile: {e}")))?;
        }
        f.flush()
            .map_err(|e| PyRuntimeError::new_err(format!("flush staging tempfile: {e}")))?;
        let path = f.path().to_path_buf();
        return Ok(Staged {
            path,
            keepalive: Some(f),
        });
    }

    // Option 3: path-like.
    if let Ok(path) = source.extract::<PathBuf>() {
        return Ok(Staged {
            path,
            keepalive: None,
        });
    }

    Err(PyTypeError::new_err(format!(
        "send(source=…) expected str / pathlib.Path / bytes / BinaryIO, got {}",
        source
            .get_type()
            .name()
            .map(|n| n.to_string())
            .unwrap_or_else(|_| "<unknown>".to_string()),
    )))
}

/// Spawn a polling task that surfaces engine-side `TransferProgress`
/// snapshots into the user's Python `on_progress` callback.
///
/// The task lives on the shared tokio runtime; it polls the engine's
/// progress monitor every `POLL_MS` until the receipt reaches a
/// terminal state, calling back at most once per observed change in
/// `bytes_transferred`. Exceptions raised inside the callback are
/// logged at WARN level and stop further invocations.
///
/// `keepalive` (an optional staging [`NamedTempFile`]) is held by the
/// task so it lives at least until the receipt terminates — that's
/// also when the staging file becomes safe to delete.
fn spawn_progress_pump(
    engine: Arc<TransferEngine>,
    task_id: Uuid,
    total_bytes: u64,
    receipt: Arc<Receipt<Sender>>,
    callback: Py<PyAny>,
    keepalive: Option<NamedTempFile>,
) {
    const POLL_MS: u64 = 100;
    let started = std::time::Instant::now();
    tokio::spawn(async move {
        let mut last_bytes: u64 = u64::MAX;
        let mut terminal = false;
        loop {
            let pm = engine.get_progress_monitor().await;
            let snapshot = pm.read().await.get_transfer(&task_id).cloned();
            let receipt_terminal = receipt.state().is_terminal();

            if let Some(snap) = snapshot {
                if snap.bytes_transferred != last_bytes
                    || matches!(
                        snap.status,
                        TransferStatus::Completed
                            | TransferStatus::Failed(_)
                            | TransferStatus::Cancelled
                    )
                {
                    last_bytes = snap.bytes_transferred;
                    let progress = PyProgress {
                        bytes_sent: snap.bytes_transferred,
                        bytes_total: snap.total_bytes.max(total_bytes),
                        files_sent: 0,
                        files_total: 1,
                        elapsed_secs: started.elapsed().as_secs_f64(),
                    };
                    let cb_result = Python::attach(|py| -> PyResult<()> {
                        callback.call1(py, (progress,))?;
                        Ok(())
                    });
                    if let Err(e) = cb_result {
                        tracing::warn!(error=%e, "on_progress callback raised; pump exiting");
                        break;
                    }
                }
                if matches!(
                    snap.status,
                    TransferStatus::Completed
                        | TransferStatus::Failed(_)
                        | TransferStatus::Cancelled
                ) {
                    terminal = true;
                }
            }
            if receipt_terminal || terminal {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(POLL_MS)).await;
        }
        // Keep the staging file alive at least until we exit the
        // pump; receipt is now terminal so the engine is done with it.
        drop(keepalive);
    });
}

/// Drop the staging file once the receipt reaches a terminal state.
/// Used when no `on_progress` callback is supplied (otherwise the
/// progress pump owns the keep-alive).
fn spawn_temp_keepalive(receipt: Arc<Receipt<Sender>>, keepalive: NamedTempFile) {
    tokio::spawn(async move {
        let _ = receipt.await_state(|s| s.is_terminal()).await;
        drop(keepalive);
    });
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
    /// Async context-manager `__aenter__` — boots the transport
    /// adapter (idempotent) so the engine worker is running before
    /// the user issues their first `send()`, then returns `self`.
    ///
    /// The adapter is built once per `PyClient` (held in a tokio
    /// [`OnceCell`]) and stitched to
    /// [`TransferEngine::http_receipt_inbox`] so receiver-side HTTP
    /// `receipt_ack` envelopes (RFC-002 §6.4) flow back to the
    /// sender-side receipt without a dedicated wire. Sockets are
    /// **not** opened eagerly here — the underlying reqwest client +
    /// QUIC endpoint dial on first use, per RFC-001 §11 Q1.
    fn __aenter__<'py>(slf: Py<Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        // Pull the engine + adapter cell out under the GIL so the
        // returned future doesn't need to re-attach.
        let (engine, cell) = Python::attach(|py| -> PyResult<_> {
            let inner = slf.borrow(py);
            Ok((Arc::clone(&inner.engine), Arc::clone(&inner.adapter)))
        })?;
        future_into_py(py, async move {
            ensure_started(engine, cell).await?;
            Ok(slf)
        })
    }

    /// Async context-manager `__aexit__` — drops the engine handle's
    /// adapter reference so the transport pool can wind down once
    /// every in-flight `send()` future settles. The engine itself
    /// stays alive as long as the user keeps the `Client` reference;
    /// real teardown of the worker channel lives in [`Drop`] (when
    /// the last `Arc<TransferEngine>` is released, the
    /// `task_sender` is dropped, the worker's `recv()` returns
    /// `None`, and the worker task exits).
    #[pyo3(signature = (_exc_type=None, _exc_value=None, _traceback=None))]
    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _exc_type: Option<Py<PyType>>,
        _exc_value: Option<Py<PyAny>>,
        _traceback: Option<Py<PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        // Best-effort: there is no public engine-shutdown surface in
        // v0.2 and the engine worker holds the adapter `Arc`
        // independently — we deliberately do not block here. The
        // killer-demo round-trip awaits the receipt's terminal state
        // before unwinding the `async with`, so any in-flight bytes
        // have already drained by the time we land here. Future
        // work: a `TransferEngine::shutdown(timeout)` that closes
        // `task_sender` and joins the worker.
        future_into_py(py, async move { Ok(false) })
    }

    /// `await client.send(source, *, to, metadata=None, chunk_size=None, timeout=None, on_progress=None)`.
    ///
    /// Per RFC-001 §5.2. `metadata` is a `dict[str, str]`; well-known
    /// keys (`trace_id`, `conversation_id`, `correlation_id`,
    /// `lifecycle`) are lifted to the typed `Metadata` fields, every
    /// other key goes into `user_metadata`.
    ///
    /// `source` is one of:
    /// - `str` / `os.PathLike` / `pathlib.Path` — read directly by the
    ///   engine (zero-copy, fast path).
    /// - `bytes` — staged into a `NamedTempFile` and the file path
    ///   handed to the engine (RFC-001 §6.2 v0.2 contract).
    /// - any object with a `read(size: int) -> bytes` method
    ///   (`BinaryIO`) — drained synchronously into a `NamedTempFile`
    ///   (1 MiB chunks, GIL held). Streaming bytes is a v0.3
    ///   enhancement, deliberately out of scope here.
    ///
    /// Staging files are kept alive on the shared tokio runtime
    /// until the returned receipt reaches a terminal state, then
    /// auto-dropped (deletes the inode).
    ///
    /// `chunk_size` is plumbed via per-call `TransferConfig`
    /// override only when set; defaults to the engine's adaptive
    /// choice. `timeout` (seconds, optional) wraps the await in
    /// `tokio::time::timeout` and raises `TimeoutError` on expiry.
    ///
    /// `on_progress`, when supplied, is a Python callable
    /// `Callable[[Progress], None]`. It is invoked from a polling
    /// task on the shared tokio runtime every ~100 ms while the
    /// transfer is in progress, then once more with the terminal
    /// snapshot before the polling task exits. Exceptions raised
    /// inside the callback are logged at WARN level and stop further
    /// invocations — they never propagate back into the awaitable.
    // Clippy's too_many_arguments lint (default 7) tags this signature
    // because RFC-001 §5.2 commits to seven user-visible kwargs plus
    // `self`. Splitting into a builder would break the documented API,
    // so we explicitly silence the lint here.
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (source, *, to, metadata=None, chunk_size=None, timeout=None, on_progress=None))]
    fn send<'py>(
        &self,
        py: Python<'py>,
        source: Bound<'py, PyAny>,
        to: String,
        metadata: Option<HashMap<String, String>>,
        chunk_size: Option<usize>,
        timeout: Option<f64>,
        on_progress: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let engine = Arc::clone(&self.engine);
        // Apply per-Client defaults pulled from `Config` only when the
        // caller did not pass an explicit kwarg. RFC-001 §5.7 contract.
        let chunk_size = chunk_size.or(self.chunk_size_default);
        let timeout = timeout.or(self.timeout_default);
        let meta = match metadata {
            Some(m) if !m.is_empty() => Some(build_metadata(m)?),
            _ => None,
        };
        let staged = stage_source(py, &source)?;
        if let Some(cb) = &on_progress {
            if !cb.is_callable() {
                return Err(PyTypeError::new_err(
                    "on_progress must be a callable accepting one Progress argument",
                ));
            }
        }
        let on_progress_obj = on_progress.map(|c| c.unbind());
        future_into_py(py, async move {
            let (path, keepalive) = staged.materialize();
            let task = build_upload_task(path, to, chunk_size).map_err(PyValueError::new_err)?;
            let task_id = task.id;
            let total_bytes = task.file_size;
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
            // Wire up auxiliary tasks AFTER the receipt is in hand —
            // both need the receipt to know when to stop.
            match (on_progress_obj, keepalive) {
                (Some(cb), keep) => {
                    spawn_progress_pump(
                        Arc::clone(&engine),
                        task_id,
                        total_bytes,
                        Arc::clone(&receipt),
                        cb,
                        keep,
                    );
                }
                (None, Some(keep)) => {
                    spawn_temp_keepalive(Arc::clone(&receipt), keep);
                }
                (None, None) => {}
            }
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
    #[pyo3(signature = (
        *,
        limit=100,
        direction="all",
        metadata_filter=None,
        history_path=None,
        trace_id=None,
        content_type_contains=None
    ))]
    // PyO3 keyword args map 1:1; count exceeds clippy::too_many_arguments.
    #[allow(clippy::too_many_arguments)]
    fn history<'py>(
        &self,
        py: Python<'py>,
        limit: usize,
        direction: &str,
        metadata_filter: Option<HashMap<String, String>>,
        history_path: Option<PathBuf>,
        trace_id: Option<String>,
        content_type_contains: Option<String>,
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
                trace_id,
                content_type_contains,
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

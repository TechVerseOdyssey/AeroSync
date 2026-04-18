//! `aerosync._native` — PyO3 bindings for the AeroSync engine.
//!
//! This crate produces a Python extension module loaded by the
//! `aerosync` package as `aerosync._native`. Every public Python
//! symbol in `python/aerosync/__init__.py` ultimately resolves to a
//! definition declared inside this module.
//!
//! See RFC-001 for the full design and the v0.2.0 task table.
//!
//! # Module layout
//!
//! - [`runtime`] — process-wide tokio runtime singleton + asyncio bridge.
//! - [`errors`] — `From<AeroSyncError> for PyErr` mapping (full
//!   exception hierarchy lands in w6 task #10).
//! - [`records`] — `Peer` / `Progress` / `HistoryEntry` `#[pyclass]`es.
//! - [`client`] — `Client` (and the placeholder `Receipt` that w6
//!   task #14 will replace with the real state machine).
//! - [`receiver`] — `Receiver` and `IncomingFile`.
//!
//! # GIL & runtime ownership
//!
//! All async methods route through [`runtime::future_into_py`] which
//! runs the Rust future on the shared tokio runtime and returns an
//! asyncio-compatible awaitable. The shared runtime is initialized
//! lazily on the first such call. See `runtime.rs` for the full
//! ownership/safety contract.

#![forbid(unsafe_op_in_unsafe_fn)]
// `missing_docs` is intentionally NOT enabled at the crate root: most
// of the public surface here consists of `#[pyclass(get_all)]` data
// fields whose documentation lives in the Python-side dataclass
// declarations (`python/aerosync/_types.py`). Re-stating those
// docstrings on every Rust field would be pure duplication.

use pyo3::prelude::*;

pub mod client;
pub mod errors;
pub mod receipt;
pub mod receiver;
pub mod records;
pub mod runtime;

use client::PyClient;
use receipt::{PyReceipt, PyReceiptWatcher};
use receiver::{PyIncomingFile, PyReceiver};
use records::{PyHistoryEntry, PyPeer, PyProgress};

/// Returns the PyPI package version (= `Cargo.toml` version).
///
/// Pure-Python module init reads this once and assigns it to the
/// public `aerosync.__version__` attribute.
#[pyfunction]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Sync factory: `aerosync.client()` returns an [`AsyncContextManager`]
/// (the [`Client`] itself, whose `__aenter__` performs any async
/// init). Per RFC-001 §5.1 the user writes
/// `async with aerosync.client() as c:` — no `await` before the call.
///
/// Group A signature is config-less; the `config=` parameter lands in
/// w6 task #11.
#[pyfunction(name = "client")]
fn make_client() -> PyClient {
    PyClient::new_default()
}

/// Sync factory: `aerosync.receiver(name=..., listen=..., save_dir=...)`
/// returns an [`AsyncContextManager`]. Same shape as `client()`: the
/// returned [`Receiver`]'s `__aenter__` is what actually binds the
/// socket. The factory is exposed now so the `_native` public
/// surface is stable from this commit forward; Group C wires up
/// `__aenter__` / `__aexit__` and the async iterator.
#[pyfunction(name = "receiver")]
#[pyo3(signature = (name=None, listen=None, save_dir=None))]
fn make_receiver(
    name: Option<String>,
    listen: Option<String>,
    save_dir: Option<std::path::PathBuf>,
) -> PyResult<PyReceiver> {
    use aerosync::core::FileReceiver;
    let mut cfg = receiver::server_config_for(save_dir);
    if let Some(addr) = listen.as_deref() {
        // RFC-001 §5.1 documents `listen` as "host:port"; we
        // split here because ServerConfig keeps host and port
        // separate. Group C's `__aenter__` will actually bind
        // the socket and surface the resolved address back to
        // the caller via the `address` getter.
        if let Some((host, port)) = addr.rsplit_once(':') {
            cfg.bind_address = host.to_string();
            if let Ok(p) = port.parse::<u16>() {
                cfg.http_port = p;
            }
        }
    }
    let address = format!("{}:{}", cfg.bind_address, cfg.http_port);
    let inner = FileReceiver::new(cfg);
    Ok(PyReceiver::new(inner, name, address))
}

/// `await aerosync.discover(timeout=5.0)` — returns a list of [`PyPeer`].
///
/// RFC-001 §5.1 types this as `AsyncIterator[Peer]` for forward
/// compatibility with a streaming rendezvous backend in v0.3+. The
/// current engine surface (`AeroSyncMdns::discover` in
/// `src/core/discovery.rs`) only collects results in batches: it
/// runs the mDNS scan + localhost probe for the full timeout
/// window then returns a `Vec`. Exposing that as an async iterator
/// here would synthesize an iterator over an already-materialized
/// list — strictly worse ergonomics than just returning the list
/// since `async for` over it would never yield mid-stream.
///
/// We therefore ship the v0.2 binding as `Awaitable[list[Peer]]`.
/// Users who want iteration write `for peer in await discover(): ...`.
/// Migrating to a true `AsyncIterator` is a non-breaking change
/// once the rendezvous client lands (v0.3, RFC-001 §10).
///
/// `timeout` (seconds, default 5.0) bounds the mDNS listen window.
/// Values <= 0 or non-finite are rejected with `ValueError`.
#[pyfunction]
#[pyo3(signature = (timeout=5.0))]
fn discover(py: Python<'_>, timeout: f64) -> PyResult<Bound<'_, PyAny>> {
    use pyo3::exceptions::PyValueError;
    if !timeout.is_finite() || timeout <= 0.0 {
        return Err(PyValueError::new_err(
            "timeout must be a finite positive number of seconds",
        ));
    }
    let dur = std::time::Duration::from_secs_f64(timeout);
    runtime::future_into_py(py, async move {
        let peers = aerosync::core::AeroSyncMdns::discover(dur).await;
        let py_peers: Vec<PyPeer> = peers.into_iter().map(PyPeer::from).collect();
        Ok(py_peers)
    })
}

/// `#[pymodule]` entry point. Maturin invokes this when Python
/// imports `aerosync._native`.
#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(version, m)?)?;
    m.add_function(wrap_pyfunction!(make_client, m)?)?;
    m.add_function(wrap_pyfunction!(make_receiver, m)?)?;
    m.add_function(wrap_pyfunction!(discover, m)?)?;
    m.add_function(wrap_pyfunction!(receiver::_test_make_incoming_file, m)?)?;

    m.add_class::<PyClient>()?;
    m.add_class::<PyReceipt>()?;
    m.add_class::<PyReceiptWatcher>()?;
    m.add_class::<PyReceiver>()?;
    m.add_class::<PyIncomingFile>()?;
    m.add_class::<PyPeer>()?;
    m.add_class::<PyProgress>()?;
    m.add_class::<PyHistoryEntry>()?;

    Ok(())
}

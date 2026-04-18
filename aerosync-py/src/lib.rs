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
pub mod receiver;
pub mod records;
pub mod runtime;

use client::{PyClient, PyReceipt};
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

/// Async factory: `await aerosync.client()` returns an async context
/// manager yielding a [`Client`].
///
/// Group A signature is config-less; the `config=` parameter lands in
/// w6 task #11.
#[pyfunction(name = "client")]
fn make_client(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    runtime::future_into_py(py, async { Ok(PyClient::new_default()) })
}

/// Async factory: `await aerosync.receiver(name=..., listen=..., save_dir=...)`
/// returns an async context manager yielding a [`Receiver`].
///
/// Group A returns a `Receiver` whose `FileReceiver` has been
/// constructed but not started — `start()` lives on the engine and is
/// called by the Group C `__aenter__`. The factory is exposed now so
/// the `_native` module's public surface is stable from the first
/// commit.
#[pyfunction(name = "receiver")]
#[pyo3(signature = (name=None, listen=None, save_dir=None))]
fn make_receiver(
    py: Python<'_>,
    name: Option<String>,
    listen: Option<String>,
    save_dir: Option<std::path::PathBuf>,
) -> PyResult<Bound<'_, PyAny>> {
    use aerosync::core::FileReceiver;
    runtime::future_into_py(py, async move {
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
    })
}

/// Async iterator factory: `aerosync.discover(timeout=5.0)`.
///
/// Group A stub returns an empty list; the real mDNS-backed
/// async-iterator wiring lands in Group C (RFC-001 task #7).
#[pyfunction]
#[pyo3(signature = (timeout=5.0))]
fn discover(py: Python<'_>, timeout: f64) -> PyResult<Bound<'_, PyAny>> {
    runtime::future_into_py(py, async move {
        // Group A stub — Group C replaces this with a real
        // AeroSyncMdns::discover call wrapped in an async iterator.
        let _ = timeout;
        Ok(Vec::<PyPeer>::new())
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

    m.add_class::<PyClient>()?;
    m.add_class::<PyReceipt>()?;
    m.add_class::<PyReceiver>()?;
    m.add_class::<PyIncomingFile>()?;
    m.add_class::<PyPeer>()?;
    m.add_class::<PyProgress>()?;
    m.add_class::<PyHistoryEntry>()?;

    Ok(())
}

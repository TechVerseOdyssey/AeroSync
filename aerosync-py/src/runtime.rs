//! Shared tokio runtime used by every Python-facing async entry point.
//!
//! # Why a single runtime
//!
//! RFC-001 §6.1 + §11 Q3 (accepted): one tokio runtime per process,
//! lazily initialized on first call. Per-`Client` runtimes would
//! double the worker-thread count for users running multiple clients
//! in the same process (a common pattern for fan-out producers and
//! receivers in the same script) and would block the asyncio event
//! loop on shutdown when QUIC connections drain.
//!
//! # Ownership model
//!
//! - The runtime is owned by `static SHARED: OnceCell<Runtime>` and
//!   never dropped during the process lifetime. Python's atexit
//!   handler does not run drop on it; tokio's worker threads are
//!   detached on process exit.
//! - We register the runtime with `pyo3-async-runtimes` at first use
//!   so `future_into_py` and friends pick it up transparently.
//!
//! # GIL rules
//!
//! - `shared_tokio()` does not touch the GIL — safe to call from any
//!   thread (including pyo3 callbacks that already released it).
//! - `future_into_py` (re-exported below as `future_into_py`) holds
//!   `Python<'_>` only long enough to construct the asyncio future
//!   and immediately releases the GIL once the tokio task is
//!   scheduled. The wrapped Rust future itself runs without the GIL.

use once_cell::sync::OnceCell;
use pyo3::prelude::*;
use std::sync::Arc;
use tokio::runtime::{Builder, Runtime};

/// The process-wide tokio runtime. Initialized on first call.
static SHARED: OnceCell<Arc<Runtime>> = OnceCell::new();

/// Worker-thread heuristic: at least 2 (so a slow blocking task can
/// never starve the I/O reactor) and at most `num_cpus / 2` so we
/// leave plenty of headroom for the user's own asyncio loop.
fn worker_threads() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    (cores / 2).max(2)
}

/// Acquire (or create) the shared multi-thread tokio runtime.
///
/// The first call constructs the runtime and registers it with
/// `pyo3-async-runtimes::tokio` so subsequent `future_into_py`
/// invocations route to it. Subsequent calls are a `OnceCell` load
/// (no allocation, no contention beyond the get-or-init memory
/// fence).
pub fn shared_tokio() -> Arc<Runtime> {
    SHARED
        .get_or_init(|| {
            let rt = Builder::new_multi_thread()
                .enable_all()
                .worker_threads(worker_threads())
                .thread_name("aerosync-py")
                .build()
                .expect("failed to build aerosync-py tokio runtime");
            let arc = Arc::new(rt);
            // Register with pyo3-async-runtimes so future_into_py
            // schedules onto our runtime and not a freshly-spawned one.
            pyo3_async_runtimes::tokio::init_with_runtime(unsafe {
                // SAFETY: `init_with_runtime` stores a `&'static Runtime`.
                // Our `Arc<Runtime>` lives in `OnceCell` for the
                // entire process lifetime, so a leaked `&'static`
                // is sound — there is no path that drops it.
                &*(Arc::as_ptr(&arc))
            })
            .expect("pyo3-async-runtimes init failed");
            arc
        })
        .clone()
}

/// Bridge a Rust async fn into a Python awaitable, scheduled on the
/// shared runtime. Equivalent to `pyo3_async_runtimes::tokio::future_into_py`
/// but guarantees the runtime is initialized first.
pub fn future_into_py<'py, F, T>(py: Python<'py>, fut: F) -> PyResult<Bound<'py, PyAny>>
where
    F: std::future::Future<Output = PyResult<T>> + Send + 'static,
    T: for<'p> IntoPyObject<'p> + Send + 'static,
{
    let _ = shared_tokio();
    pyo3_async_runtimes::tokio::future_into_py(py, fut)
}

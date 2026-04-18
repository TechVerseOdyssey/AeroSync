//! `Client` — outbound transfer handle.
//!
//! Group A scaffold only declares the empty `PyClient` shell so the
//! `_native` module can register it. The `send` / `send_directory`
//! methods land in Group B (RFC-001 tasks #3, #4). Optional args
//! (`metadata`, `on_progress`, `Config`, bytes sources) are deferred
//! to w6.

use crate::runtime::future_into_py;
use aerosync::core::transfer::{TransferConfig, TransferEngine};
use pyo3::prelude::*;
use pyo3::types::PyType;
use std::sync::Arc;

/// Outbound `Client`. Constructed via the `aerosync.client()` async
/// factory which returns an async context manager.
#[pyclass(module = "aerosync._native", name = "Client")]
pub struct PyClient {
    #[allow(dead_code)] // populated for real in Group B
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
}

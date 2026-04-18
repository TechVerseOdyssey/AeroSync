//! Minimal error scaffold.
//!
//! The full RFC-001 §5.8 exception hierarchy (PeerNotFoundError,
//! AuthError, TransferFailed, ChecksumMismatch, …) is task #10 and
//! lands in w6 alongside RFC-002 receipts. For w5 we only need a
//! single `From<AeroSyncError> for PyErr` impl so binding methods can
//! `?`-bubble engine errors back into Python as `RuntimeError`s with
//! a stable `code` attribute. w6 will graduate this to a proper
//! exception class hierarchy by replacing the `PyRuntimeError::new_err`
//! call below with a registry of `PyType` handles cached at module
//! init time.

use aerosync::core::AeroSyncError;
use pyo3::exceptions::{
    PyConnectionError, PyOSError, PyRuntimeError, PyTimeoutError, PyValueError,
};
use pyo3::PyErr;

/// Map an `AeroSyncError` to the most fitting builtin Python exception.
///
/// w6 task #10 will replace this with custom AeroSyncError subclasses.
/// The mapping below is intentionally conservative — every variant
/// resolves to a builtin Python exception type so callers can
/// `except OSError`, `except ConnectionError` etc. without depending
/// on the (forthcoming) custom class hierarchy.
pub fn engine_err_to_py(err: AeroSyncError) -> PyErr {
    let msg = err.to_string();
    match err {
        AeroSyncError::FileIo(_) => PyOSError::new_err(msg),
        AeroSyncError::Network(_) => PyConnectionError::new_err(msg),
        AeroSyncError::Storage { .. } => PyOSError::new_err(msg),
        AeroSyncError::Cancelled => PyRuntimeError::new_err(msg),
        AeroSyncError::InvalidConfig(_) | AeroSyncError::Config(_) => PyValueError::new_err(msg),
        AeroSyncError::Protocol(_) => PyConnectionError::new_err(msg),
        AeroSyncError::System(_) => PyRuntimeError::new_err(msg),
        AeroSyncError::Auth(_) => PyConnectionError::new_err(msg),
        AeroSyncError::TomlParse(_) => PyValueError::new_err(msg),
        AeroSyncError::Unknown(_) => PyRuntimeError::new_err(msg),
    }
}

/// Convenience: any error implementing `Display` flattened into a
/// `PyRuntimeError`. Used by binding code that wraps `anyhow::Error`
/// or generic `&str` panics.
pub fn anyhow_to_py(err: anyhow::Error) -> PyErr {
    PyRuntimeError::new_err(err.to_string())
}

/// Map a tokio Elapsed (timeout) into the standard Python TimeoutError
/// so callers can `except TimeoutError` consistently.
pub fn timeout_to_py(_err: tokio::time::error::Elapsed) -> PyErr {
    PyTimeoutError::new_err("operation timed out")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_filio_to_oserror() {
        let e = AeroSyncError::FileIo(std::io::Error::new(std::io::ErrorKind::NotFound, "boom"));
        let py_err = engine_err_to_py(e);
        // Ensure the message round-trips intact (the PyType assertion
        // requires an interpreter, deferred to pytest under w7).
        assert!(py_err.to_string().contains("boom"));
    }

    #[test]
    fn maps_network_to_connection_error() {
        let e = AeroSyncError::Network("dial 127.0.0.1:1: refused".into());
        let py_err = engine_err_to_py(e);
        assert!(py_err.to_string().contains("refused"));
    }

    #[test]
    fn maps_invalid_config_to_value_error() {
        let e = AeroSyncError::InvalidConfig("chunk_size must be > 0".into());
        let py_err = engine_err_to_py(e);
        assert!(py_err.to_string().contains("chunk_size"));
    }

    #[test]
    fn anyhow_passthrough_preserves_message() {
        let e = anyhow::anyhow!("kaboom");
        let py_err = anyhow_to_py(e);
        assert!(py_err.to_string().contains("kaboom"));
    }
}

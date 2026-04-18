//! Rust → Python error mapping (RFC-001 §6.4 + §5.8).
//!
//! Every Rust error that can leak into a `pymethod` is funnelled
//! through one of the helpers in this module so the resulting `PyErr`
//! is an instance of the typed [`crate::exceptions`] hierarchy
//! (RFC-001 §5.8) rather than a builtin Python class.
//!
//! Catch-all rule of thumb when adding a new mapping:
//!
//! | Rust error class                     | Python exception        | code             |
//! |--------------------------------------|-------------------------|------------------|
//! | network / handshake / dial / TLS     | `ConnectionError`       | `network`        |
//! | mDNS / rendezvous lookup miss        | `PeerNotFoundError`     | `peer_not_found` |
//! | auth token / signature failure       | `AuthError`             | `auth`           |
//! | config parse / invalid path / token  | `ConfigError`           | `config`         |
//! | timeout (tokio Elapsed)              | `TimeoutError`          | `timeout`        |
//! | transfer aborted / cancelled / nack  | `TransferFailed`        | `transfer_failed`|
//! | checksum mismatch                    | `ChecksumMismatch`      | `checksum_mismatch` |
//! | anything else from the engine        | `EngineError`           | `engine`         |
//!
//! [`metadata_err_to_py`] is the one deliberate exception: metadata
//! builder validation errors stay on the Python builtin `ValueError`
//! to honour the long-standing "bad argument → ValueError" Python
//! convention. They are still `aerosync` errors logically, but
//! callers writing `dict[str, str]` literals expect ValueError.

use crate::exceptions::{
    new_err, AeroSyncError, AuthError, ConfigError, ConnectionError, EngineError, TimeoutError,
    TransferFailed,
};
use aerosync::core::metadata::MetadataError;
use aerosync::core::AeroSyncError as EngineErr;
use pyo3::exceptions::PyValueError;
use pyo3::PyErr;

/// Map an `aerosync::core::AeroSyncError` to a typed `PyErr` from the
/// RFC-001 §5.8 hierarchy.
///
/// The mapping is conservative — every variant produces a sensible
/// catch class that user code can `except`. The full classification
/// matrix is documented in this module's preamble.
pub fn engine_err_to_py(err: EngineErr) -> PyErr {
    let msg = err.to_string();
    match err {
        // FileIo and Storage are both filesystem-flavoured failures.
        // RFC-001 §5.8 has no dedicated FileError class; bucketing
        // them under EngineError keeps the hierarchy small while
        // still being catchable as `aerosync.AeroSyncError`.
        EngineErr::FileIo(_) => new_err::<EngineError>(msg, "file_io"),
        EngineErr::Storage { .. } => new_err::<EngineError>(msg, "storage"),
        EngineErr::Network(_) => new_err::<ConnectionError>(msg, "network"),
        EngineErr::Cancelled => new_err::<TransferFailed>(msg, "cancelled"),
        EngineErr::InvalidConfig(_) | EngineErr::Config(_) => new_err::<ConfigError>(msg, "config"),
        EngineErr::Protocol(_) => new_err::<ConnectionError>(msg, "protocol"),
        EngineErr::System(_) => new_err::<EngineError>(msg, "system"),
        EngineErr::Auth(_) => new_err::<AuthError>(msg, "auth"),
        EngineErr::TomlParse(_) => new_err::<ConfigError>(msg, "toml_parse"),
        EngineErr::Unknown(_) => new_err::<EngineError>(msg, "unknown"),
    }
}

/// Convenience: any error implementing `Display` flattened into a
/// generic [`EngineError`]. Used by binding code that wraps
/// `anyhow::Error` or generic `&str` panics that don't carry enough
/// structure to pick a tighter class.
pub fn anyhow_to_py(err: anyhow::Error) -> PyErr {
    new_err::<EngineError>(err.to_string(), "anyhow")
}

/// Map a tokio `Elapsed` (timeout) into the SDK [`TimeoutError`]
/// class so callers can `except aerosync.TimeoutError` consistently.
///
/// Note: this is the SDK class, NOT Python's builtin
/// `asyncio.TimeoutError` / `TimeoutError`. RFC-001 §5.8 lists
/// `TimeoutError` as part of the AeroSync hierarchy; we follow the
/// RFC even though it shadows the builtin name.
pub fn timeout_to_py(_err: tokio::time::error::Elapsed) -> PyErr {
    new_err::<TimeoutError>("operation timed out", "timeout")
}

/// Build a [`TransferFailed`] from an arbitrary message + code. Used
/// by [`PyReceipt::processed`](crate::receipt::PyReceipt::processed)
/// in the future when we graduate it to the "raises on failure"
/// contract from RFC-001 §5.5.
#[allow(dead_code)]
pub fn transfer_failed(msg: impl Into<String>, code: &'static str) -> PyErr {
    new_err::<TransferFailed>(msg, code)
}

/// Map a [`MetadataError`] (RFC-003 §6 builder validation) into a
/// Python `ValueError`.
///
/// This is the **one** mapping that intentionally stays on a Python
/// builtin instead of the SDK hierarchy: callers passing a literal
/// `dict[str, str]` to `Client.send(metadata=…)` expect Python's
/// argument-validation convention (`ValueError`), and changing that
/// would break ergonomic patterns like
/// `try: …; except (ValueError, TypeError): …`.
pub fn metadata_err_to_py(err: MetadataError) -> PyErr {
    PyValueError::new_err(err.to_string())
}

/// Re-export so call sites can write `errors::AeroSyncError`. Useful
/// for unit tests that want to assert against the base class without
/// pulling in the [`exceptions`](crate::exceptions) module directly.
pub use crate::exceptions::AeroSyncError as BaseError;
#[allow(dead_code)]
fn _force_use_base() {
    let _ = std::any::TypeId::of::<AeroSyncError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exceptions::{
        AuthError, ChecksumMismatch, ConfigError, ConnectionError, EngineError, PeerNotFoundError,
        TransferFailed,
    };
    use pyo3::prelude::*;
    use pyo3::types::PyTypeMethods;

    fn class_name(err: &PyErr) -> String {
        Python::attach(|py| err.get_type(py).name().unwrap().to_string())
    }

    #[test]
    fn maps_network_to_connection_error() {
        let e = EngineErr::Network("dial 127.0.0.1:1: refused".into());
        let py_err = engine_err_to_py(e);
        assert_eq!(class_name(&py_err), "ConnectionError");
        assert!(py_err.to_string().contains("refused"));
    }

    #[test]
    fn maps_invalid_config_to_config_error() {
        let e = EngineErr::InvalidConfig("chunk_size must be > 0".into());
        let py_err = engine_err_to_py(e);
        assert_eq!(class_name(&py_err), "ConfigError");
    }

    #[test]
    fn maps_auth_to_auth_error() {
        let e = EngineErr::Auth("token mismatch".into());
        let py_err = engine_err_to_py(e);
        assert_eq!(class_name(&py_err), "AuthError");
    }

    #[test]
    fn maps_cancelled_to_transfer_failed() {
        let e = EngineErr::Cancelled;
        let py_err = engine_err_to_py(e);
        assert_eq!(class_name(&py_err), "TransferFailed");
    }

    #[test]
    fn engine_err_carries_code_attribute() {
        let e = EngineErr::Network("boom".into());
        let py_err = engine_err_to_py(e);
        Python::attach(|py| {
            let value = py_err.value(py);
            let code: String = value.getattr("code").unwrap().extract().unwrap();
            assert_eq!(code, "network");
        });
    }

    #[test]
    fn timeout_uses_typed_class() {
        // We can't easily construct a tokio Elapsed value directly;
        // emulate the helper's output by calling a no-op timeout.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(async {
            let pending = std::future::pending::<()>();
            tokio::time::timeout(std::time::Duration::from_millis(1), pending)
                .await
                .unwrap_err()
        });
        let py_err = timeout_to_py(err);
        assert_eq!(class_name(&py_err), "TimeoutError");
    }

    #[test]
    fn anyhow_passthrough_lands_on_engine_error() {
        let py_err = anyhow_to_py(anyhow::anyhow!("kaboom"));
        assert_eq!(class_name(&py_err), "EngineError");
        assert!(py_err.to_string().contains("kaboom"));
    }

    #[test]
    fn metadata_error_stays_on_value_error() {
        // Build a real MetadataError via the public builder.
        use aerosync::core::metadata::MetadataBuilder;
        let huge: String = "x".repeat(64 * 1024 + 1);
        let err = MetadataBuilder::new().user("k", huge).build().unwrap_err();
        let py_err = metadata_err_to_py(err);
        assert_eq!(class_name(&py_err), "ValueError");
    }

    #[test]
    fn checksum_mismatch_class_exists_and_subclasses_transfer_failed() {
        Python::attach(|py| {
            let cm = py.get_type::<ChecksumMismatch>();
            let tf = py.get_type::<TransferFailed>();
            assert!(cm.is_subclass(&tf).unwrap());
        });
    }

    #[test]
    fn all_typed_classes_subclass_aerosync_error() {
        Python::attach(|py| {
            let base = py.get_type::<crate::exceptions::AeroSyncError>();
            for cls in [
                py.get_type::<ConfigError>(),
                py.get_type::<PeerNotFoundError>(),
                py.get_type::<ConnectionError>(),
                py.get_type::<AuthError>(),
                py.get_type::<TransferFailed>(),
                py.get_type::<EngineError>(),
            ] {
                assert!(cls.is_subclass(&base).unwrap());
            }
        });
    }
}

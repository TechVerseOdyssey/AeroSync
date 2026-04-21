//! Typed Python exception hierarchy (RFC-001 §5.8).
//!
//! Every exception subclasses [`AeroSyncError`] so user code can write
//! `except aerosync.AeroSyncError` to catch anything raised by the
//! SDK. Each carries two extra attributes documented in the RFC:
//!
//! - `.code`   — stable snake_case identifier (`"network"`,
//!   `"checksum_mismatch"`, …) suitable for log filtering and
//!   metric tagging.
//! - `.detail` — the original Rust error message (the same string
//!   passed as the exception's first positional argument).
//!
//! The hierarchy is wired up at module init by `register`. Once the
//! `_native` extension has been imported these classes are reachable
//! as `aerosync._native.AeroSyncError`, …; the public Python package
//! re-exports them under `aerosync.AeroSyncError` etc.
//!
//! ## Construction
//!
//! Always go through [`new_err::<Class>(msg, code)`](new_err). It
//! instantiates the Python class once, sets `code` / `detail` as
//! attributes on the value, and returns a [`PyErr`] ready to bubble.
//! Bypassing the helper (e.g. `MyClass::new_err(msg)`) skips the
//! attribute set-up — the resulting exception still inherits the
//! class name, but `e.code` / `e.detail` will be missing.

use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::PyType;
use pyo3::PyTypeInfo;

create_exception!(_native, AeroSyncError, PyException);
create_exception!(_native, ConfigError, AeroSyncError);
create_exception!(_native, PeerNotFoundError, AeroSyncError);
create_exception!(_native, ConnectionError, AeroSyncError);
create_exception!(_native, AuthError, AeroSyncError);
create_exception!(_native, TransferFailed, AeroSyncError);
create_exception!(_native, ChecksumMismatch, TransferFailed);
create_exception!(_native, TimeoutError, AeroSyncError);
create_exception!(_native, EngineError, AeroSyncError);

/// Build a `PyErr` of class `T` carrying `code` + `detail` attributes.
///
/// This is the canonical entry point for binding code that needs to
/// surface a typed exception. The returned `PyErr` is ready to
/// `?`-bubble out of any pyo3 method.
///
/// `code` should be a stable snake_case identifier (`"network"`,
/// `"timeout"`, `"transfer_failed"`, …) — RFC-001 §5.8 documents
/// these as part of the public error contract so user code can
/// branch on them without string-matching the message.
pub fn new_err<T>(msg: impl Into<String>, code: &'static str) -> PyErr
where
    T: PyTypeInfo,
{
    let msg_owned = msg.into();
    Python::attach(|py| {
        let cls = T::type_object(py);
        match build_typed(py, &cls, &msg_owned, code) {
            Ok(err) => err,
            Err(e) => e,
        }
    })
}

/// `Class(msg)` + setattr(`code`, …) + setattr(`detail`, …).
fn build_typed<'py>(
    py: Python<'py>,
    cls: &Bound<'py, PyType>,
    msg: &str,
    code: &'static str,
) -> PyResult<PyErr> {
    let _ = py;
    let instance = cls.call1((msg,))?;
    instance.setattr("code", code)?;
    instance.setattr("detail", msg)?;
    Ok(PyErr::from_value(instance))
}

/// Wire the exception classes into the `_native` module so they are
/// reachable from Python as `aerosync._native.<ClassName>`.
///
/// Called once at module init from [`crate::_native`]. Order does not
/// matter — class objects are already linked into the inheritance
/// graph by the `create_exception!` macro at import-time.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add("AeroSyncError", py.get_type::<AeroSyncError>())?;
    m.add("ConfigError", py.get_type::<ConfigError>())?;
    m.add("PeerNotFoundError", py.get_type::<PeerNotFoundError>())?;
    m.add("ConnectionError", py.get_type::<ConnectionError>())?;
    m.add("AuthError", py.get_type::<AuthError>())?;
    m.add("TransferFailed", py.get_type::<TransferFailed>())?;
    m.add("ChecksumMismatch", py.get_type::<ChecksumMismatch>())?;
    m.add("TimeoutError", py.get_type::<TimeoutError>())?;
    m.add("EngineError", py.get_type::<EngineError>())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::types::PyAnyMethods;
    use pyo3::types::PyTypeMethods;

    #[test]
    fn new_err_sets_code_and_detail() {
        let err = new_err::<EngineError>("kaboom", "engine");
        Python::attach(|py| {
            let value = err.value(py);
            let code: String = value.getattr("code").unwrap().extract().unwrap();
            let detail: String = value.getattr("detail").unwrap().extract().unwrap();
            assert_eq!(code, "engine");
            assert_eq!(detail, "kaboom");
            // Message round-trips as the first positional arg.
            assert!(value.str().unwrap().to_string().contains("kaboom"));
        });
    }

    #[test]
    fn checksum_mismatch_is_transfer_failed() {
        Python::attach(|py| {
            let cm = py.get_type::<ChecksumMismatch>();
            let tf = py.get_type::<TransferFailed>();
            assert!(cm.is_subclass(&tf).unwrap());
        });
    }

    #[test]
    fn every_class_subclasses_aerosync_error() {
        Python::attach(|py| {
            let base = py.get_type::<AeroSyncError>();
            for cls in [
                py.get_type::<ConfigError>(),
                py.get_type::<PeerNotFoundError>(),
                py.get_type::<ConnectionError>(),
                py.get_type::<AuthError>(),
                py.get_type::<TransferFailed>(),
                py.get_type::<ChecksumMismatch>(),
                py.get_type::<TimeoutError>(),
                py.get_type::<EngineError>(),
            ] {
                assert!(
                    cls.is_subclass(&base).unwrap(),
                    "{} does not subclass AeroSyncError",
                    cls.name().unwrap()
                );
            }
        });
    }
}

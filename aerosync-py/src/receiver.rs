//! `Receiver` + `IncomingFile`.
//!
//! Group A scaffold only declares the empty shells so the module can
//! register them. The async-iterator wiring (subscribing to the
//! engine's WS broadcast and yielding `IncomingFile` per completed
//! transfer) lands in Group C (RFC-001 tasks #5, #6).

use aerosync::core::server::{FileReceiver, ServerConfig};
use pyo3::prelude::*;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Inbound `Receiver`. Constructed via the `aerosync.receiver()`
/// async factory.
#[pyclass(module = "aerosync._native", name = "Receiver")]
pub struct PyReceiver {
    #[allow(dead_code)] // wired in Group C
    pub(crate) inner: Arc<Mutex<FileReceiver>>,
    pub(crate) name: Option<String>,
    pub(crate) address: String,
}

impl PyReceiver {
    pub fn new(inner: FileReceiver, name: Option<String>, address: String) -> Self {
        Self {
            inner: Arc::new(Mutex::new(inner)),
            name,
            address,
        }
    }
}

#[pymethods]
impl PyReceiver {
    #[getter]
    fn name(&self) -> Option<String> {
        self.name.clone()
    }

    #[getter]
    fn address(&self) -> String {
        self.address.clone()
    }
}

/// `IncomingFile` — receiver-side handle to a freshly-arrived file.
///
/// Group A holds only the descriptive fields; `ack()` / `nack()` ship
/// in w6 task #15 along with the receipt protocol wiring.
#[pyclass(
    module = "aerosync._native",
    name = "IncomingFile",
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyIncomingFile {
    pub(crate) path: PathBuf,
    pub(crate) file_name: String,
    pub(crate) size_bytes: u64,
    pub(crate) sha256: Option<String>,
    pub(crate) receipt_id: Option<String>,
}

#[pymethods]
impl PyIncomingFile {
    #[getter]
    fn path(&self) -> PathBuf {
        self.path.clone()
    }

    #[getter]
    fn file_name(&self) -> String {
        self.file_name.clone()
    }

    #[getter]
    fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    #[getter]
    fn sha256(&self) -> Option<String> {
        self.sha256.clone()
    }

    #[getter]
    fn receipt_id(&self) -> Option<String> {
        self.receipt_id.clone()
    }

    fn __repr__(&self) -> String {
        format!(
            "IncomingFile(path={:?}, file_name={:?}, size_bytes={})",
            self.path, self.file_name, self.size_bytes
        )
    }
}

/// Build a default `ServerConfig` rooted at `save_dir`. Helper used by
/// the Group C `aerosync.receiver()` factory; lifted out so the test
/// suite can exercise the validation logic without spinning up an
/// actual receiver.
pub(crate) fn server_config_for(save_dir: Option<PathBuf>) -> ServerConfig {
    let mut cfg = ServerConfig::default();
    if let Some(dir) = save_dir {
        cfg.receive_directory = dir;
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_config_default_save_dir() {
        let cfg = server_config_for(None);
        assert_eq!(cfg.receive_directory.as_os_str(), "./received");
    }

    #[test]
    fn server_config_overrides_save_dir() {
        let dir = std::path::PathBuf::from("/tmp/aerosync-test");
        let cfg = server_config_for(Some(dir.clone()));
        assert_eq!(cfg.receive_directory, dir);
    }

    #[test]
    fn incoming_file_repr_includes_filename() {
        let f = PyIncomingFile {
            path: "/tmp/x.bin".into(),
            file_name: "x.bin".into(),
            size_bytes: 42,
            sha256: None,
            receipt_id: None,
        };
        let r = f.__repr__();
        assert!(r.contains("x.bin"));
        assert!(r.contains("42"));
    }
}

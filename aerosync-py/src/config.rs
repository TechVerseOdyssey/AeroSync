//! Bridge between the Python `aerosync.Config` dataclass and the
//! engine's `TransferConfig` / `ServerConfig`.
//!
//! The Python side declares `Config` as a flat frozen dataclass (see
//! `python/aerosync/_config.py`); we deliberately do NOT plumb a
//! `#[pyclass] Config` here because:
//!
//! 1. The dataclass is the documentation surface (RFC-001 §5.7) and
//!    keeping it pure-Python lets us add fields without rebuilding the
//!    cdylib.
//! 2. Field types include `pathlib.Path` / `os.PathLike`, which round-
//!    trip more cleanly through pyo3's `extract::<PathBuf>()` than
//!    through a Rust-owned `#[pyclass]` getter mirror.
//!
//! At call time we reach into the supplied `Bound<PyAny>` via
//! `getattr` for each documented field name. Missing attributes are
//! treated as "use the engine default" — that path is exercised by
//! both `Config()` (everything `None` / default) and by the
//! `aerosync.client()` factory called without `config=` (we synthesize
//! `Config()` in Python before crossing the FFI boundary, but defend
//! in depth here in case a future caller passes something
//! Config-shaped from outside the SDK).

use crate::runtime::install_log_level;
use aerosync::core::server::ServerConfig;
use aerosync::core::transfer::TransferConfig;
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use std::path::PathBuf;
use zeroize::Zeroizing;

/// Resolved view of the Python-side `Config` dataclass.
///
/// Every field is `Option<...>` because the Python dataclass itself
/// allows every field to be `None` (the default constructor's shape).
pub struct ResolvedConfig {
    pub auth_token: Option<String>,
    pub state_dir: Option<PathBuf>,
    pub rendezvous_url: Option<String>,
    pub log_level: Option<String>,
    pub chunk_size_default: Option<usize>,
    pub timeout_default: Option<f64>,
}

impl ResolvedConfig {
    /// Empty (engine-default) `Config()` — what the factories use when
    /// the caller passes no `config=` argument.
    pub fn empty() -> Self {
        Self {
            auth_token: None,
            state_dir: None,
            rendezvous_url: None,
            log_level: Some("info".to_string()),
            chunk_size_default: None,
            timeout_default: None,
        }
    }

    /// Pull fields off the supplied Python object via `getattr`.
    ///
    /// The Python `Config` dataclass is frozen with `slots=True`, so
    /// every documented attribute is guaranteed to exist; we still
    /// tolerate `AttributeError` quietly so future field additions on
    /// the Rust side do not break older Python `Config`s.
    pub fn from_pyobject(cfg: &Bound<'_, PyAny>) -> PyResult<Self> {
        let auth_token: Option<String> = optional_str(cfg, "auth_token")?;
        let state_dir: Option<PathBuf> = optional_path(cfg, "state_dir")?;
        let rendezvous_url: Option<String> = optional_str(cfg, "rendezvous_url")?;
        let log_level: Option<String> = optional_str(cfg, "log_level")?;
        let chunk_size_default: Option<usize> = optional_usize(cfg, "chunk_size_default")?;
        let timeout_default: Option<f64> = optional_f64(cfg, "timeout_default")?;
        Ok(Self {
            auth_token,
            state_dir,
            rendezvous_url,
            log_level,
            chunk_size_default,
            timeout_default,
        })
    }

    /// Apply `auth_token`, `state_dir` to a fresh `TransferConfig` and
    /// trigger the (idempotent) tracing-subscriber init for `log_level`.
    /// `rendezvous_url` is consumed by the receiver factory for WAN
    /// heartbeat participation, not by `TransferConfig`.
    pub fn build_transfer_config(&self) -> TransferConfig {
        let mut tc = TransferConfig::default();
        if let Some(tok) = &self.auth_token {
            tc.auth_token = Some(Zeroizing::new(tok.clone()));
        }
        if let Some(dir) = &self.state_dir {
            tc.resume_state_dir = dir.clone();
        }
        if let Some(level) = &self.log_level {
            install_log_level(level);
        }
        tc
    }

    /// Apply `state_dir` to a fresh `ServerConfig` (history paths,
    /// resume state, etc. are derived from it engine-side via the
    /// `dirs_next` helpers; we only override `receive_directory` if
    /// the user did not also set `save_dir=` on the receiver factory).
    pub fn build_server_config(&self, save_dir_override: Option<PathBuf>) -> ServerConfig {
        let mut sc = ServerConfig::default();
        if let Some(dir) = save_dir_override {
            sc.receive_directory = dir;
        } else if let Some(dir) = &self.state_dir {
            // Receiver users who only set `state_dir` get a parallel
            // `received/` subdirectory rather than the cwd default.
            sc.receive_directory = dir.join("received");
        }
        if let Some(level) = &self.log_level {
            install_log_level(level);
        }
        sc
    }
}

/// `getattr(obj, name) → Option<String>`; tolerates missing attribute
/// or explicit `None` by returning `Ok(None)`.
fn optional_str(obj: &Bound<'_, PyAny>, name: &str) -> PyResult<Option<String>> {
    let v = match obj.getattr(name) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    if v.is_none() {
        return Ok(None);
    }
    let s: String = v.extract().map_err(|_| {
        PyTypeError::new_err(format!("Config.{name} must be a str (got non-string)"))
    })?;
    Ok(Some(s))
}

fn optional_path(obj: &Bound<'_, PyAny>, name: &str) -> PyResult<Option<PathBuf>> {
    let v = match obj.getattr(name) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    if v.is_none() {
        return Ok(None);
    }
    let p: PathBuf = v.extract().map_err(|_| {
        PyTypeError::new_err(format!(
            "Config.{name} must be str / pathlib.Path / os.PathLike"
        ))
    })?;
    Ok(Some(p))
}

fn optional_usize(obj: &Bound<'_, PyAny>, name: &str) -> PyResult<Option<usize>> {
    let v = match obj.getattr(name) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    if v.is_none() {
        return Ok(None);
    }
    let n: usize = v
        .extract()
        .map_err(|_| PyTypeError::new_err(format!("Config.{name} must be a positive int")))?;
    Ok(Some(n))
}

fn optional_f64(obj: &Bound<'_, PyAny>, name: &str) -> PyResult<Option<f64>> {
    let v = match obj.getattr(name) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    if v.is_none() {
        return Ok(None);
    }
    let f: f64 = v
        .extract()
        .map_err(|_| PyTypeError::new_err(format!("Config.{name} must be a float (seconds)")))?;
    Ok(Some(f))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_keeps_engine_defaults() {
        let c = ResolvedConfig::empty();
        let tc = c.build_transfer_config();
        // Engine defaults — assertions intentionally tight so a future
        // engine change that silently shifts a default also breaks this
        // test and forces a conscious update.
        assert!(tc.auth_token.is_none());
        assert_eq!(tc.chunk_size, 4 * 1024 * 1024);
    }

    #[test]
    fn auth_token_round_trips_into_zeroizing() {
        let c = ResolvedConfig {
            auth_token: Some("s3cr3t".into()),
            state_dir: None,
            rendezvous_url: None,
            log_level: None,
            chunk_size_default: None,
            timeout_default: None,
        };
        let tc = c.build_transfer_config();
        // Zeroizing<String> derefs to &str via Deref.
        assert_eq!(tc.auth_token.as_deref().map(|s| s.as_str()), Some("s3cr3t"));
    }

    #[test]
    fn state_dir_overrides_resume_state_dir() {
        let c = ResolvedConfig {
            auth_token: None,
            state_dir: Some(PathBuf::from("/tmp/aerosync-test-state")),
            rendezvous_url: None,
            log_level: None,
            chunk_size_default: None,
            timeout_default: None,
        };
        let tc = c.build_transfer_config();
        assert_eq!(
            tc.resume_state_dir,
            PathBuf::from("/tmp/aerosync-test-state")
        );
        let sc = c.build_server_config(None);
        assert_eq!(
            sc.receive_directory,
            PathBuf::from("/tmp/aerosync-test-state/received")
        );
    }

    #[test]
    fn save_dir_override_wins_over_state_dir() {
        let c = ResolvedConfig {
            auth_token: None,
            state_dir: Some(PathBuf::from("/tmp/state")),
            rendezvous_url: None,
            log_level: None,
            chunk_size_default: None,
            timeout_default: None,
        };
        let sc = c.build_server_config(Some(PathBuf::from("/tmp/explicit")));
        assert_eq!(sc.receive_directory, PathBuf::from("/tmp/explicit"));
    }
}

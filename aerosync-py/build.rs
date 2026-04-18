//! Build script — emits per-target linker flags so `cargo build /
//! cargo test -p aerosync-py` Just Work locally without `maturin`.
//!
//! Two distinct concerns:
//!
//! 1. **Wheel build (`--features extension-module`)**.
//!    `pyo3/extension-module` deliberately does **not** link against
//!    `libpython` so the produced .so can be loaded by any Python
//!    interpreter at import time. On macOS we have to add
//!    `-undefined dynamic_lookup` ourselves; on Linux it's a no-op.
//!
//! 2. **Local `cargo test` (default features → `pyo3/auto-initialize`)**.
//!    pyo3 links against the libpython detected by `pyo3-build-config`
//!    at build time. On systems where Python lives somewhere
//!    non-standard (conda envs, pyenv, homebrew framework Pythons),
//!    the resulting test binary's rpath omits that directory and dyld
//!    fails with `Library not loaded: @rpath/libpython3.X.dylib`. We
//!    paper over that here by emitting `-Wl,-rpath,$LIBDIR` for the
//!    detected interpreter.

fn main() {
    let extension_module = std::env::var_os("CARGO_FEATURE_EXTENSION_MODULE").is_some();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    if extension_module {
        // Wheel build path — keep symbols unresolved at link time so
        // Python can supply them when it dlopens the cdylib.
        if target_os == "macos" {
            // The flag MUST be split across two `-C link-arg`s — the
            // single-string form (`-C link-arg=-undefined dynamic_lookup`)
            // is mishandled by Apple's clang driver and silently drops
            // the second word. See pyo3 issue #2120 for the original
            // diagnosis.
            println!("cargo:rustc-cdylib-link-arg=-undefined");
            println!("cargo:rustc-cdylib-link-arg=dynamic_lookup");
        }
        // Linux's GNU ld defers unresolved symbols by default for
        // shared objects; Windows links against pythonXY.lib via
        // pyo3-build-config. Both are no-op branches here.
        return;
    }

    // Default-features path (`pyo3/auto-initialize` on, libpython
    // linked). Make the test binary's rpath include the libpython
    // directory so dyld can find it without `DYLD_FALLBACK_LIBRARY_PATH`.
    if target_os == "macos" || target_os == "linux" {
        let cfg = pyo3_build_config::get();
        if let Some(libdir) = cfg.lib_dir.as_deref() {
            // -Wl,-rpath,DIR works on both Apple ld64 and GNU ld.
            // We emit it for ALL link targets (cdylib + bins + tests)
            // so a developer's `cargo test` and `cargo run --example`
            // both pick libpython up.
            println!("cargo:rustc-link-arg=-Wl,-rpath,{libdir}");
        }
    }
}

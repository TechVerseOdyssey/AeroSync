"""Group A smoke test: the package imports and exposes its version.

Larger pytest matrix lands in w7 (RFC-001 task #18). This module
exists so a future maturin-equipped CI can sanity-check the build
end-to-end with `pytest aerosync-py/tests/`.
"""

from __future__ import annotations

import sys
from pathlib import Path

if sys.version_info >= (3, 11):
    import tomllib
else:
    import tomli as tomllib


def _cargo_pkg_version() -> str:
    """Read `[package].version` from `aerosync-py/Cargo.toml`.

    The Rust `version()` binding returns `env!("CARGO_PKG_VERSION")`,
    which is sourced from this same field. Reading it dynamically here
    means release bumps only have to touch `Cargo.toml` — the smoke
    test follows automatically instead of needing a paired edit.
    """
    cargo_toml = Path(__file__).resolve().parent.parent / "Cargo.toml"
    with cargo_toml.open("rb") as f:
        data = tomllib.load(f)
    version = data["package"]["version"]
    assert isinstance(version, str)
    return version


def test_module_imports() -> None:
    import aerosync

    v = aerosync.version()
    assert isinstance(v, str)
    assert v == aerosync.__version__
    assert v == _cargo_pkg_version()


def test_public_surface_contains_factories() -> None:
    import aerosync

    for name in ("client", "receiver", "discover", "version"):
        assert hasattr(aerosync, name), f"missing public symbol: {name}"


def test_lifecycle_enum_round_trip() -> None:
    from aerosync import Lifecycle

    assert Lifecycle.TRANSIENT.value == "transient"
    assert Lifecycle("durable") is Lifecycle.DURABLE

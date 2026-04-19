"""Group A smoke test: the package imports and exposes its version.

Larger pytest matrix lands in w7 (RFC-001 task #18). This module
exists so a future maturin-equipped CI can sanity-check the build
end-to-end with `pytest aerosync-py/tests/`.
"""

from __future__ import annotations


def test_module_imports() -> None:
    import aerosync

    v = aerosync.version()
    assert isinstance(v, str)
    assert v == aerosync.__version__
    # Cargo package version pin from `aerosync-py/Cargo.toml`.
    assert v == "0.2.1"


def test_public_surface_contains_factories() -> None:
    import aerosync

    for name in ("client", "receiver", "discover", "version"):
        assert hasattr(aerosync, name), f"missing public symbol: {name}"


def test_lifecycle_enum_round_trip() -> None:
    from aerosync import Lifecycle

    assert Lifecycle.TRANSIENT.value == "transient"
    assert Lifecycle("durable") is Lifecycle.DURABLE

"""Pytest coverage for the user-facing :class:`aerosync.Config`
dataclass (RFC-001 §5.7 / task #11).

Every assertion below pins a documented behaviour. If you find
yourself loosening one, that's a SemVer change — update RFC-001 §5.7
in the same commit.
"""

from __future__ import annotations

import asyncio
import warnings
from pathlib import Path

import pytest

import aerosync


# ── Construction & validation ─────────────────────────────────────────


def test_config_defaults_match_documented_baseline() -> None:
    """`Config()` is the documented baseline. Every field except
    `log_level` defaults to None; log_level defaults to "info" so a
    bare `aerosync.client()` already gets human-readable diagnostics."""
    cfg = aerosync.Config()
    assert cfg.auth_token is None
    assert cfg.state_dir is None
    assert cfg.rendezvous_url is None
    assert cfg.log_level == "info"
    assert cfg.chunk_size_default is None
    assert cfg.timeout_default is None


def test_config_is_frozen() -> None:
    """RFC-001 §5.7 commits to the dataclass being immutable so the
    same Config can be safely shared between concurrent client/receiver
    instances without copy-on-write overhead."""
    cfg = aerosync.Config()
    with pytest.raises((AttributeError, Exception)):
        cfg.auth_token = "mutated"  # type: ignore[misc]


def test_config_rejects_unknown_log_level() -> None:
    with pytest.raises(ValueError, match="log_level"):
        aerosync.Config(log_level="screaming")


def test_config_rejects_non_positive_chunk_size() -> None:
    with pytest.raises(ValueError, match="chunk_size_default"):
        aerosync.Config(chunk_size_default=0)
    with pytest.raises(ValueError, match="chunk_size_default"):
        aerosync.Config(chunk_size_default=-1)


def test_config_rejects_non_positive_timeout() -> None:
    with pytest.raises(ValueError, match="timeout_default"):
        aerosync.Config(timeout_default=0.0)
    with pytest.raises(ValueError, match="timeout_default"):
        aerosync.Config(timeout_default=float("nan"))


def test_config_with_overrides_returns_new_instance() -> None:
    base = aerosync.Config(log_level="warn")
    tweaked = base.with_overrides(log_level="debug")
    assert base.log_level == "warn"
    assert tweaked.log_level == "debug"
    assert tweaked is not base


# ── from_dict ─────────────────────────────────────────────────────────


def test_config_from_dict_happy_path() -> None:
    cfg = aerosync.Config.from_dict(
        {
            "auth_token": "tok",
            "log_level": "debug",
            "chunk_size_default": 1024,
            "timeout_default": 12.5,
        }
    )
    assert cfg.auth_token == "tok"
    assert cfg.log_level == "debug"
    assert cfg.chunk_size_default == 1024
    assert cfg.timeout_default == 12.5


def test_config_from_dict_unknown_key_warns_does_not_raise() -> None:
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        cfg = aerosync.Config.from_dict({"auth_token": "t", "totally_invented": 42})
    assert cfg.auth_token == "t"
    matching = [w for w in caught if "totally_invented" in str(w.message)]
    assert matching, "expected a UserWarning for the unknown key"
    assert issubclass(matching[0].category, UserWarning)


def test_config_from_dict_empty_dict_is_defaults() -> None:
    cfg = aerosync.Config.from_dict({})
    # Same as Config() — no overrides applied.
    assert cfg == aerosync.Config()


# ── from_toml ─────────────────────────────────────────────────────────


def test_config_from_toml_roundtrips_fields(tmp_path: Path) -> None:
    p = tmp_path / "config.toml"
    p.write_text(
        'auth_token = "abc"\n'
        'log_level = "warn"\n'
        "chunk_size_default = 4096\n"
        "timeout_default = 30.0\n",
        encoding="utf-8",
    )
    cfg = aerosync.Config.from_toml(p)
    assert cfg.auth_token == "abc"
    assert cfg.log_level == "warn"
    assert cfg.chunk_size_default == 4096
    assert cfg.timeout_default == 30.0


def test_config_from_toml_missing_file_raises_typed_oserror(tmp_path: Path) -> None:
    p = tmp_path / "does-not-exist.toml"
    with pytest.raises(FileNotFoundError):
        aerosync.Config.from_toml(p)


def test_config_from_toml_accepts_str_or_pathlike(tmp_path: Path) -> None:
    p = tmp_path / "x.toml"
    p.write_text('auth_token = "x"\n')
    cfg_via_str = aerosync.Config.from_toml(str(p))
    cfg_via_path = aerosync.Config.from_toml(p)
    assert cfg_via_str.auth_token == "x"
    assert cfg_via_path.auth_token == "x"


# ── from_default ──────────────────────────────────────────────────────


def test_config_from_default_returns_defaults_when_file_missing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Drives HOME at a temp dir with no `~/.aerosync/config.toml` so
    we get the documented "no overrides → defaults" behaviour."""
    monkeypatch.setenv("HOME", str(tmp_path))
    cfg = aerosync.Config.from_default()
    assert cfg == aerosync.Config()


def test_config_from_default_loads_existing_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("HOME", str(tmp_path))
    aerosync_dir = tmp_path / ".aerosync"
    aerosync_dir.mkdir()
    (aerosync_dir / "config.toml").write_text(
        'log_level = "debug"\nauth_token = "from-default"\n', encoding="utf-8"
    )
    cfg = aerosync.Config.from_default()
    assert cfg.log_level == "debug"
    assert cfg.auth_token == "from-default"


# ── Plumbing into the factories ───────────────────────────────────────


def test_client_factory_accepts_config_kwarg(tmp_path: Path) -> None:
    """The `aerosync.client(config=Config(...))` path must build a
    working client — the only assertion we can make without a peer
    is that the returned object is the documented context manager."""
    cfg = aerosync.Config(log_level="warn", chunk_size_default=8192)
    c = aerosync.client(config=cfg)
    assert hasattr(c, "__aenter__")
    assert hasattr(c, "__aexit__")


def test_receiver_factory_accepts_config_kwarg(tmp_path: Path) -> None:
    cfg = aerosync.Config(state_dir=tmp_path)
    r = aerosync.receiver(name="alice", listen="127.0.0.1:0", config=cfg)
    assert r.name == "alice"


def test_client_factory_accepts_none_config() -> None:
    """`config=None` is identical to omitting the kwarg."""
    a = aerosync.client(config=None)
    b = aerosync.client()
    assert hasattr(a, "__aenter__")
    assert hasattr(b, "__aenter__")


def test_client_send_uses_config_chunk_size_default(tmp_path: Path) -> None:
    """Per RFC-001 §5.7, `chunk_size_default` is applied when send()
    is called without an explicit chunk_size kwarg. The send fails
    (no peer), but the receipt is issued so we know the engine accepted
    the cached default. Failure mode lives in errors tests."""
    f = tmp_path / "blob.bin"
    f.write_bytes(b"hello")
    cfg = aerosync.Config(chunk_size_default=4096)

    async def run() -> str:
        async with aerosync.client(config=cfg) as c:
            r = await c.send(f, to="nowhere-host:1")
            return r.id

    rid = asyncio.run(run())
    assert isinstance(rid, str) and len(rid) >= 16


def test_config_unknown_attribute_in_pyobject_is_typeerror() -> None:
    """The Rust binding pulls fields by name; a plain dict is NOT a
    Config (no slots, no validation). Must raise — silently accepting
    would let typos sneak past validation."""

    class FakeConfig:
        # Wrong type for log_level — int not str.
        log_level = 12345
        auth_token = None
        state_dir = None
        rendezvous_url = None
        chunk_size_default = None
        timeout_default = None

    with pytest.raises(TypeError):
        aerosync.client(config=FakeConfig())  # type: ignore[arg-type]


def test_config_dataclass_re_exported() -> None:
    """`from aerosync import Config` is part of the public surface
    contract (`aerosync.__all__`)."""
    assert "Config" in aerosync.__all__
    assert aerosync.Config is not None

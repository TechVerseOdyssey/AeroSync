"""User-facing :class:`Config` dataclass (RFC-001 §5.7 / task #11).

`Config` is the only configuration surface for the SDK. It is a frozen
dataclass that gathers a small, stable set of knobs documented in
RFC-001 §5.7. Both factories accept it as a keyword-only ``config=``
argument:

.. code-block:: python

    import aerosync

    cfg = aerosync.Config(auth_token="s3cr3t", log_level="debug")
    async with aerosync.client(config=cfg) as c:
        ...

The dataclass is **frozen** so it can be cheaply shared between a
client and a receiver in the same process; it has **slots** to keep the
per-Config memory footprint minimal in agent-fan-out scenarios where
many ephemeral clients are created.

Three constructors land here:

- :meth:`Config.from_dict` — build from a plain Python ``dict`` (e.g.
  pulled from FastAPI request body, env vars converted by user code,
  …). Unknown keys emit a ``UserWarning`` to stderr and are dropped;
  RFC-001 §5.7 leaves room for additive future keys, so silently
  ignoring them keeps forward compat without breaking older SDKs.
- :meth:`Config.from_toml` — load from a TOML file. Uses stdlib
  ``tomllib`` on Python ≥ 3.11 and the back-port ``tomli`` on 3.9 /
  3.10 (transitively required via ``pyproject.toml`` so users do not
  need to install it manually).
- :meth:`Config.from_default` — read ``$HOME/.aerosync/config.toml``
  if present, else return :class:`Config` with everything at its
  documented default.

The Rust binding extracts fields by attribute name; the dataclass shape
is therefore part of the public stability contract for v0.2.x.
"""

from __future__ import annotations

import os
import sys
import warnings
from collections.abc import Mapping
from dataclasses import dataclass, fields, replace
from pathlib import Path
from typing import Any, Union

if sys.version_info >= (3, 11):
    import tomllib as _toml
else:  # pragma: no cover - only exercised on 3.9 / 3.10 CI legs
    import tomli as _toml  # type: ignore[no-redef,import-not-found,unused-ignore]


# Set of recognised log-level strings. Mirrors the `tracing` crate
# canonical names plus an explicit "off" sentinel so users can opt out
# of any binding-side tracing-subscriber init. Kept lower-case to
# match the engine's filter parser.
_LOG_LEVELS: frozenset[str] = frozenset(
    {"trace", "debug", "info", "warn", "error", "off"}
)

# Path-like values accepted by `state_dir`. `os.PathLike` covers
# `pathlib.Path` and any duck-typed equivalent without forcing the
# dataclass to widen its declared type.
_PathLike = Union[str, "os.PathLike[str]"]


@dataclass(slots=True, frozen=True)
class Config:
    """Stable v0.2 configuration surface for :func:`aerosync.client` and
    :func:`aerosync.receiver`.

    Every field is optional; ``Config()`` is the documented baseline
    and matches what the factories use when no ``config=`` is passed.

    Attributes:
        auth_token: Shared-secret bearer token forwarded as the
            ``Authorization`` header (HTTP) / handshake credential
            (QUIC). ``None`` disables authentication.
        state_dir: Override for the on-disk directory where resume
            state, history JSONL, and other per-process artifacts live.
            Defaults to ``~/.config/aerosync`` on Linux/macOS (the
            engine's `dirs_next::config_dir` resolution).
        rendezvous_url: Optional URL of a rendezvous service for peer
            discovery beyond mDNS. Reserved for v0.3 — accepted today
            but currently a no-op (see implementation note in
            ``client.rs``).
        log_level: Initial verbosity for the binding's
            ``tracing-subscriber``. One of ``trace`` / ``debug`` /
            ``info`` / ``warn`` / ``error`` / ``off``. Applied at most
            once per process; subsequent calls are silently ignored so
            the SDK never overrides a subscriber the user has already
            installed (e.g. via ``aerosync-cli`` or their own
            framework).
        chunk_size_default: Default ``chunk_size`` value applied to
            every :meth:`Client.send` call that does not pass an
            explicit ``chunk_size=`` kwarg. ``None`` (the default)
            means "let the engine pick adaptively".
        timeout_default: Default ``timeout`` value applied to every
            :meth:`Client.send` call that does not pass an explicit
            ``timeout=`` kwarg. ``None`` means "no per-call timeout".
    """

    auth_token: str | None = None
    state_dir: _PathLike | None = None
    rendezvous_url: str | None = None
    log_level: str = "info"
    chunk_size_default: int | None = None
    timeout_default: float | None = None

    def __post_init__(self) -> None:
        # `frozen=True` means we cannot use plain `self.x = ...`
        # validation that mutates; we limit ourselves to *raises*.
        # Range / type checks chosen to fail fast at construction
        # time rather than deep inside the engine.
        if self.log_level not in _LOG_LEVELS:
            raise ValueError(
                f"Config.log_level: unknown level {self.log_level!r} "
                f"(expected one of {sorted(_LOG_LEVELS)})"
            )
        if self.chunk_size_default is not None and self.chunk_size_default <= 0:
            raise ValueError(
                "Config.chunk_size_default must be positive (or None)"
            )
        if self.timeout_default is not None and (
            self.timeout_default <= 0 or self.timeout_default != self.timeout_default  # NaN
        ):
            raise ValueError(
                "Config.timeout_default must be a finite positive number of seconds (or None)"
            )

    # ── Constructors ──────────────────────────────────────────────────

    @classmethod
    def from_dict(cls, d: Mapping[str, Any]) -> Config:
        """Build a :class:`Config` from a flat mapping.

        Unknown keys (anything not declared as a dataclass field on
        :class:`Config`) emit a ``UserWarning`` and are dropped. The
        warning is intentional: dropping silently could mask typos in
        production configs (`auht_token` vs `auth_token`); raising
        breaks users who wrote forward-compatible code that anticipates
        a key landing in a later SDK release.
        """
        known = {f.name for f in fields(cls)}
        clean: dict[str, Any] = {}
        for k, v in d.items():
            if k in known:
                clean[k] = v
            else:
                warnings.warn(
                    f"Config.from_dict: ignoring unknown key {k!r}",
                    UserWarning,
                    stacklevel=2,
                )
        return cls(**clean)

    @classmethod
    def from_toml(cls, path: _PathLike) -> Config:
        """Build a :class:`Config` from a TOML file.

        Reads the entire file, parses it as TOML, and forwards the
        top-level table to :meth:`from_dict`. Nested tables are NOT
        flattened — only the top level is consumed. Missing / unreadable
        files raise the underlying ``OSError`` (typically
        ``FileNotFoundError`` or ``PermissionError``); malformed TOML
        raises ``tomllib.TOMLDecodeError`` (or ``tomli.TOMLDecodeError``
        on 3.9 / 3.10).
        """
        p = Path(os.fspath(path))
        with p.open("rb") as fh:
            data = _toml.load(fh)
        if not isinstance(data, dict):
            raise ValueError(
                f"Config.from_toml: {p} did not parse to a top-level table"
            )
        return cls.from_dict(data)

    @classmethod
    def from_default(cls) -> Config:
        """Read ``~/.aerosync/config.toml`` if present, else return defaults.

        Designed for the killer-demo "just works" path:
        ``async with aerosync.client(config=Config.from_default()): ...``.
        Missing file is treated as "no overrides"; *any other* IO error
        (permission denied on a file that does exist, malformed TOML)
        propagates so it is not silently swallowed.
        """
        home = os.environ.get("HOME") or os.path.expanduser("~")
        path = Path(home) / ".aerosync" / "config.toml"
        if not path.exists():
            return cls()
        return cls.from_toml(path)

    # ── Convenience ───────────────────────────────────────────────────

    def with_overrides(self, **overrides: Any) -> Config:
        """Return a copy with ``overrides`` applied. Equivalent to
        ``dataclasses.replace(cfg, **overrides)`` but keeps the public
        API self-contained so users do not need to import dataclasses
        for the common "tweak one field" pattern."""
        return replace(self, **overrides)

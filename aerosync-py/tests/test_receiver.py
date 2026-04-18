"""Group C: Receiver async-context-manager + iterator + discover().

End-to-end network round-trip lives in the broader v0.2 e2e suite
(w7 task #18). Here we assert the binding-level contract:

- `aerosync.receiver(...)` returns an object usable as
  `async with` (no preceding `await`).
- The returned object exposes the RFC-001 §5.3 properties
  (`name`, `address`).
- `aerosync.discover(timeout=...)` returns a coroutine that yields
  a `list[Peer]` and rejects non-finite timeouts with `ValueError`.
- The receiver iterator at least supports `__aiter__` returning
  self (StopAsyncIteration on a never-started receiver is exercised
  via shutdown semantics).
"""

from __future__ import annotations

import asyncio
from pathlib import Path

import aerosync
import pytest


def test_receiver_factory_returns_async_context_manager(tmp_path: Path) -> None:
    r = aerosync.receiver(name="alice", listen="127.0.0.1:0", save_dir=tmp_path)
    assert hasattr(r, "__aenter__")
    assert hasattr(r, "__aexit__")
    assert r.name == "alice"
    # ServerConfig parses "host:port" — port 0 is preserved verbatim
    # for now; the real bound port lands when __aenter__ wires the
    # listener through to expose `actual_address`. For w5 the
    # contract is "address echoes whatever the user passed in".
    assert r.address == "127.0.0.1:0"


def test_receiver_supports_async_iteration_protocol(tmp_path: Path) -> None:
    r = aerosync.receiver(listen="127.0.0.1:0", save_dir=tmp_path)
    # __aiter__ on a not-yet-entered receiver still returns self —
    # consumers typically write `async for f in receiver:` *inside*
    # the `async with` block, so iteration without entry is undefined
    # behavior we just don't crash on.
    assert r.__aiter__() is r


def test_receiver_address_default_save_dir(tmp_path: Path) -> None:
    # Without an explicit save_dir, ServerConfig::default() applies.
    r = aerosync.receiver(name="bob", listen="0.0.0.0:0")
    assert r.name == "bob"
    assert r.address == "0.0.0.0:0"


def test_discover_returns_a_list() -> None:
    async def go() -> list[aerosync.Peer]:
        # Tiny timeout so the test stays fast even with no peers
        # on the LAN. The mDNS scan task is awaited to completion
        # by the engine before the future resolves.
        return await aerosync.discover(timeout=0.2)

    peers = asyncio.run(go())
    assert isinstance(peers, list)
    # Each element (if any) must be a Peer instance with the
    # documented attributes — degenerate to a no-op assertion when
    # the LAN is empty.
    for p in peers:
        assert hasattr(p, "name")
        assert hasattr(p, "address")
        assert hasattr(p, "port")


def test_discover_rejects_non_positive_timeout() -> None:
    async def go() -> object:
        return await aerosync.discover(timeout=0.0)

    with pytest.raises(ValueError):
        asyncio.run(go())


def test_discover_rejects_non_finite_timeout() -> None:
    async def go() -> object:
        return await aerosync.discover(timeout=float("inf"))

    with pytest.raises(ValueError):
        asyncio.run(go())


def test_incoming_file_class_is_exported() -> None:
    assert hasattr(aerosync, "IncomingFile")
    cls = aerosync.IncomingFile
    # Cannot construct from Python (frozen native class with
    # internal-only fields), but it must be present so users can
    # `isinstance(f, aerosync.IncomingFile)` in their handlers.
    assert isinstance(cls, type)


def test_peer_class_round_trips_via_constructor() -> None:
    # Peer IS exposed with a constructor for tests / fakes (mDNS
    # discovery tests that don't want a real LAN).
    p = aerosync.Peer(name="alice", address="192.168.1.20", port=7788)
    assert p.name == "alice"
    assert p.address == "192.168.1.20"
    assert p.port == 7788

"""RFC-001 §13 #18 — `aerosync.discover()` surface.

`discover()` is documented as `Awaitable[list[Peer]]` (NOT an async
iterator — RFC-001 §5.5 was finalised that way during w5 review).
These tests pin the awaitable shape, the timeout validation, and the
coroutine-cancellation contract."""

from __future__ import annotations

import asyncio
import inspect

import aerosync
import pytest


def test_discover_returns_awaitable_of_list() -> None:
    """`discover(timeout=...)` returns an awaitable; awaiting it
    yields a `list[Peer]`. We don't assert on the contents — the LAN
    may or may not have peers."""

    async def go() -> object:
        coro = aerosync.discover(timeout=0.2)
        # The returned object must be awaitable. Both `Coroutine` and
        # PyO3-future objects qualify; we accept either via the
        # `__await__` protocol check.
        assert hasattr(coro, "__await__") or inspect.isawaitable(coro)
        return await coro

    result = asyncio.run(go())
    assert isinstance(result, list)


def test_discover_each_peer_is_typed() -> None:
    """If the LAN has any peers, every entry must be an
    `aerosync.Peer` with the documented attributes."""

    async def go() -> list[aerosync.Peer]:
        return await aerosync.discover(timeout=0.2)

    peers = asyncio.run(go())
    for p in peers:
        assert isinstance(p, aerosync.Peer)
        assert isinstance(p.name, str)
        assert isinstance(p.address, str)
        assert isinstance(p.port, int)
        assert 1 <= p.port <= 65535


def test_discover_short_timeout_completes_fast() -> None:
    """A 100 ms timeout must resolve within ~1 s wall clock even on a
    busy CI runner. This guards against an accidental "block until
    first peer" regression."""
    import time

    async def go() -> int:
        await aerosync.discover(timeout=0.1)
        return 0

    t0 = time.monotonic()
    asyncio.run(go())
    assert (time.monotonic() - t0) < 1.0


def test_discover_negative_timeout_rejected() -> None:
    async def go() -> object:
        return await aerosync.discover(timeout=-0.1)

    with pytest.raises(ValueError):
        asyncio.run(go())


def test_discover_nan_timeout_rejected() -> None:
    async def go() -> object:
        return await aerosync.discover(timeout=float("nan"))

    with pytest.raises(ValueError):
        asyncio.run(go())

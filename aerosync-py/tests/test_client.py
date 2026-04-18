"""Group B sanity checks for `Client.send` / `Client.send_directory`.

These run against a real interpreter (skipped by `cargo test`); they
exercise the Python→Rust call path end-to-end without needing a live
peer. The actual transfer always fails (no receiver listening), which
is the assertion we make.

The full pytest matrix lands in w7 (RFC-001 task #18).
"""

from __future__ import annotations

import asyncio
from pathlib import Path

import aerosync
import pytest


def test_client_class_is_exported() -> None:
    assert aerosync.Client is not None
    assert aerosync.Receipt is not None


def test_send_rejects_missing_source(tmp_path: Path) -> None:
    """`send()` validates the source path before any tokio work runs."""

    async def run() -> None:
        async with aerosync.client() as c:
            with pytest.raises((ValueError, OSError)):
                await c.send(tmp_path / "does-not-exist.bin", to="alice")

    asyncio.run(run())


def test_send_directory_rejects_non_directory(tmp_path: Path) -> None:
    f = tmp_path / "regular-file.txt"
    f.write_text("not a dir")

    async def run() -> None:
        async with aerosync.client() as c:
            with pytest.raises(ValueError):
                await c.send_directory(f, to="alice")

    asyncio.run(run())


def test_send_directory_rejects_empty_directory(tmp_path: Path) -> None:
    async def run() -> None:
        async with aerosync.client() as c:
            with pytest.raises(ValueError):
                await c.send_directory(tmp_path, to="alice")

    asyncio.run(run())


def test_client_send_returns_receipt_with_id(tmp_path: Path) -> None:
    """`send()` of a real file enqueues a task and returns a Receipt.

    The actual network transfer happens off-thread on the engine's
    worker pool; `send()` returns as soon as the receipt is issued.
    We assert that the placeholder Receipt has a non-empty id (it
    will be a UUID in canonical form).
    """
    f = tmp_path / "blob.bin"
    f.write_bytes(b"x" * 32)

    async def run() -> str:
        async with aerosync.client() as c:
            r = await c.send(f, to="nowhere-host:1", chunk_size=4096)
            return r.id

    rid = asyncio.run(run())
    assert isinstance(rid, str) and len(rid) >= 16


def test_client_send_rejects_non_finite_timeout(tmp_path: Path) -> None:
    f = tmp_path / "blob.bin"
    f.write_bytes(b"x")

    async def run() -> None:
        async with aerosync.client() as c:
            with pytest.raises(ValueError):
                await c.send(f, to="alice", timeout=0.0)

    asyncio.run(run())

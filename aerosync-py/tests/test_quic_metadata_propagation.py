"""QUIC metadata propagation — Python SDK end-to-end (RFC-002 §6.3 /
RFC-003 §8.4 — w0.2.1 batch C "P0.2").

Sister test to `tests/test_metadata_propagation.py` (the HTTP path).
This file proves that the QUIC transport now carries the same
RFC-003 `Metadata` envelope onto the receiving `IncomingFile` after
batch C wired the bidi receipt control stream and `TransferStart`
metadata propagation.

## Why the sender is the Rust CLI binary, not `aerosync.client()`

The Python `Client` does **not** drive the `TransferEngine` worker
in v0.2 — see `tests/test_killer_demo.py` (skipped pending
`w3c-quic-receipt-wiring`). To exercise the *real* QUIC sender path
end-to-end we shell out to the bundled `aerosync` CLI binary, which
goes through `TransferEngine + AutoAdapter + QuicTransfer` exactly
the way a real production sender does.

## Scope

1. **Happy path** — the CLI sends a file via `quic://` with
   `--trace-id` and `--meta` flags; the receiving `IncomingFile`
   surfaces `trace_id` + `user_metadata` matching the sender.
2. The test is **skipped** (not failed) when the `aerosync` debug
   binary has not been built yet — so a fresh checkout that runs
   `pytest aerosync-py/tests/` without first running `cargo build
   --bin aerosync` does not produce a confusing failure.
"""

from __future__ import annotations

import asyncio
import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

import aerosync


def _aerosync_cli() -> Path | None:
    """Locate the `aerosync` binary. Prefers the workspace's
    `target/debug/aerosync` (what `cargo build --bin aerosync`
    produces during the standard dev loop) and falls back to
    `target/release/aerosync` and finally to whatever `aerosync`
    is on `PATH`. Returns `None` when nothing is found — the
    caller skips the test in that case."""
    here = Path(__file__).resolve()
    workspace = here.parents[2]
    for candidate in (
        workspace / "target" / "debug" / "aerosync",
        workspace / "target" / "release" / "aerosync",
    ):
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate
    found = shutil.which("aerosync")
    return Path(found) if found else None


async def _drive(tmp_path: Path) -> dict[str, object]:
    """Boot a real Python receiver, shell out to the `aerosync` CLI
    to send a file via QUIC with metadata flags, and return the
    asserter's view of the first received file."""
    cli = _aerosync_cli()
    if cli is None:
        pytest.skip(
            "aerosync CLI not built; run `cargo build --bin aerosync` "
            "before this test (it shells out to the real sender to "
            "exercise the production QUIC path end-to-end)"
        )

    payload = tmp_path / "payload.bin"
    body = b"hello from python QUIC E2E test\n"
    payload.write_bytes(body)

    received: dict[str, object] = {}
    addr_q: asyncio.Queue[str] = asyncio.Queue()

    async def consumer() -> None:
        async with aerosync.receiver(
            name="py-quic-receiver",
            listen="127.0.0.1:0",
            save_dir=str(tmp_path / "inbox"),
        ) as r:
            # The Python receiver always boots HTTP+QUIC by default;
            # we wait for the QUIC port to come up because the test's
            # whole point is the QUIC path. `quic_address` returns
            # `None` until the listener has actually bound the UDP
            # socket — poll briefly so we don't race `__aenter__`.
            for _ in range(50):
                qa = r.quic_address
                if qa is not None:
                    break
                await asyncio.sleep(0.05)
            assert r.quic_address is not None, "QUIC listener never bound"
            await addr_q.put(r.quic_address)
            async for incoming in r:
                received["file_name"] = incoming.file_name
                received["size_bytes"] = incoming.size_bytes
                received["metadata"] = dict(incoming.metadata)
                received["trace_id"] = incoming.trace_id
                received["lifecycle"] = incoming.lifecycle
                await incoming.ack()
                return

    consumer_task = asyncio.create_task(consumer())
    try:
        quic_addr = await asyncio.wait_for(addr_q.get(), timeout=10.0)
        cmd = [
            str(cli),
            "send",
            str(payload),
            f"quic://{quic_addr}",
            "--protocol",
            "quic",
            "--trace-id",
            "py-qt-1",
            "--meta",
            "tenant=acme",
            "--meta",
            "agent_id=qa-1",
            "--lifecycle",
            "durable",
            "--no-verify",
            # Skip the preflight HTTP /health probe — this is a
            # QUIC-only test, the receiver's HTTP listener is on a
            # different ephemeral port we don't share with the CLI.
            "--no-preflight",
            # Receiver bakes a self-signed dev certificate; production
            # would `--pin-cert` it, but for the test loopback path
            # we accept it explicitly.
            "--accept-invalid-certs",
        ]
        # Run the CLI in a worker thread so the receiver loop keeps
        # spinning. A 30 s wall-clock budget is generous for a 30-byte
        # local-loopback transfer; if we blow through it the receiver
        # has likely deadlocked, in which case the asserter below
        # reports the much more useful "no IncomingFile" error.
        proc_result = await asyncio.to_thread(
            subprocess.run,
            cmd,
            capture_output=True,
            timeout=30,
        )
        if proc_result.returncode != 0:
            raise AssertionError(
                f"aerosync CLI exited {proc_result.returncode}: "
                f"stdout={proc_result.stdout!r} stderr={proc_result.stderr!r}"
            )

        await asyncio.wait_for(consumer_task, timeout=15.0)
    finally:
        if not consumer_task.done():
            consumer_task.cancel()
            try:
                await consumer_task
            except (asyncio.CancelledError, StopAsyncIteration):
                pass

    return received


@pytest.mark.skipif(
    sys.platform == "win32", reason="aerosync CLI build matrix excludes Windows in v0.2"
)
def test_quic_upload_propagates_metadata_envelope(tmp_path: Path) -> None:
    """A `aerosync send <file> quic://...` invocation with metadata
    flags must surface those fields verbatim on the Python
    `IncomingFile` once batch C wires the bidi receipt control
    stream — the QUIC path now matches the HTTP path covered by
    `test_metadata_propagation.test_http_metadata_propagation_end_to_end`."""

    received = asyncio.run(_drive(tmp_path))

    assert received, "receiver must surface at least one IncomingFile"
    assert received["file_name"] == "payload.bin"
    assert received["trace_id"] == "py-qt-1"
    metadata = received["metadata"]
    assert isinstance(metadata, dict)
    assert metadata.get("tenant") == "acme"
    assert metadata.get("agent_id") == "qa-1"
    # Per RFC-001 §1 killer-demo contract, well-known fields are
    # also mirrored into the flat `metadata` dict so
    # `incoming.metadata.get("trace_id")` from the README quickstart
    # works without the user having to know which keys are typed.
    # The typed getters remain canonical and keep their semantics;
    # the HTTP test enforces the same shape so the two transports
    # stay symmetric.
    assert metadata.get("trace_id") == "py-qt-1"
    assert metadata.get("lifecycle") == "LIFECYCLE_DURABLE"
    assert received["lifecycle"] == "LIFECYCLE_DURABLE"

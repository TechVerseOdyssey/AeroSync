"""RFC-001 §1 — the "killer demo" round-trip, end-to-end through the
public Python SDK surface.

This test ships **shipped-but-skipped** in v0.2.0. The intent is for it
to flip to passing automatically once `w3c-quic-receipt-wiring` lands
(see `TODO.local.md` v0.2.1 backlog). Until then it documents the exact
shape the SDK promises in `README.md` and RFC-001 §1, and the precise
deferral that blocks it.

# Why skipped (pre-v0.2.1)

The Python SDK demo from RFC-001 §1 requires three pieces working
together cross-process:

1. `aerosync.client().send(...)` building a `Metadata` envelope and
   handing it to the engine — works today (covered by
   `test_receipt.py::test_metadata_pass_through_*`).
2. `aerosync.receiver(...)` accepting the file over QUIC and surfacing
   the metadata on `IncomingFile.metadata` — receiver-side metadata
   surface works today; what is missing is the **wire-side** ferrying
   of the envelope from sender to receiver.
3. `await receipt.processed()` resolving to a non-failed `Outcome`
   once the receiver `await incoming.ack()`s — requires the sender's
   Receipt to observe the receiver-side `Ack` over the QUIC bidi
   receipt stream.

Items (2) and (3) are the **same** missing wiring: the QUIC adapter
does not yet open the bidi receipt stream alongside chunk transfer
(see `tests/cross_rfc_smoke.rs` "Honesty clause" + `CHANGELOG.md`
"Known limitations" → `w3c-quic-receipt-wiring`). The Rust-side cross-
RFC smoke test bridges this manually; the Python-side test cannot,
because the Python binding does not expose the internal Receipt /
ReceiptRegistry handles needed to apply `Event::Ack` on the sender
side directly. Doing so would leak transport plumbing into the public
API surface.

When `w3c-quic-receipt-wiring` lands, remove the `pytest.mark.skip`
decorator and verify the demo from `README.md` works as printed.
"""

from __future__ import annotations

import asyncio
from pathlib import Path

import aerosync
import pytest


@pytest.mark.skip(
    reason=(
        "blocked on w3c-quic-receipt-wiring (v0.2.1 backlog): the QUIC "
        "transport does not yet open the bidi receipt stream automatically, "
        "so a cross-process Python sender/receiver pair cannot complete the "
        "ack round-trip end-to-end. The Rust-side smoke test bridges this "
        "manually in `tests/cross_rfc_smoke.rs`; see also CHANGELOG.md "
        "'Known limitations' for the full deferral statement."
    )
)
def test_killer_demo_round_trip(tmp_path: Path) -> None:
    """The verbatim RFC-001 §1 / README.md Python quickstart demo.

    Exercises:
      - `aerosync.client()` async context manager
      - `Client.send(source, to=..., metadata={...})` returning a
        `Receipt`
      - `Receipt.processed()` resolving to an `Outcome` dict whose
        `status` field is one of the canonical terminal labels
      - `aerosync.receiver(name=..., listen=..., save_dir=...)` async
        context manager + async-iterator
      - `IncomingFile.metadata` exposing the sender's envelope
      - `IncomingFile.ack()` driving the receiver-side terminal

    This is the SDK contract from `README.md` "Send a file" / "Receive
    a file" — if it passes, the README is accurate. If it does not, the
    README is a lie and the skip reason MUST be updated to reflect
    whichever new deferral surfaced.
    """

    src = tmp_path / "report.csv"
    src.write_bytes(b"hello,world\n1,2\n")

    received: list[dict[str, object]] = []

    async def producer(peer_addr: str) -> dict[str, object]:
        async with aerosync.client() as c:
            receipt = await c.send(
                str(src),
                to=peer_addr,
                metadata={"trace_id": "demo-trace", "agent_id": "producer"},
            )
            outcome: dict[str, object] = await asyncio.wait_for(
                receipt.processed(), timeout=10.0
            )
            return outcome

    async def consumer(addr_q: asyncio.Queue[str]) -> None:
        async with aerosync.receiver(
            name="demo-receiver",
            listen="127.0.0.1:0",
            save_dir=str(tmp_path / "inbox"),
        ) as r:
            await addr_q.put(r.address)
            async for incoming in r:
                received.append(
                    {
                        "trace_id": incoming.metadata.get("trace_id"),
                        "agent_id": incoming.metadata.get("agent_id"),
                        "size": incoming.size_bytes,
                        "file_name": incoming.file_name,
                    }
                )
                await incoming.ack()
                break

    async def go() -> dict[str, object]:
        addr_q: asyncio.Queue[str] = asyncio.Queue()
        consumer_task = asyncio.create_task(consumer(addr_q))
        addr = await asyncio.wait_for(addr_q.get(), timeout=5.0)
        outcome = await producer(addr)
        await asyncio.wait_for(consumer_task, timeout=5.0)
        return outcome

    outcome = asyncio.run(go())

    assert outcome["status"] in ("acked", "completed")
    assert len(received) == 1
    assert received[0]["trace_id"] == "demo-trace"
    assert received[0]["agent_id"] == "producer"
    assert received[0]["size"] == src.stat().st_size
    assert received[0]["file_name"] == "report.csv"

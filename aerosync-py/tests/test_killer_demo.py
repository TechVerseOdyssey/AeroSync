"""RFC-001 §1 — the "killer demo" round-trip, end-to-end through the
public Python SDK surface.

Exercises the verbatim `README.md` quickstart: a Python `aerosync.client()`
sends a file with a sealed `Metadata` envelope to a Python
`aerosync.receiver()` listening on `127.0.0.1:0`, and the sender's
`Receipt.processed()` resolves to `Outcome(status="acked")`. Wired by
v0.2.1 batch D.5 (engine lifecycle on `__aenter__` + HTTP wire-level
`receipt_ack` echo per RFC-002 §6.4). When this test fails, the
README is a lie.
"""

from __future__ import annotations

import asyncio
from pathlib import Path

import aerosync


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

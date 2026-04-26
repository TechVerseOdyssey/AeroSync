# AeroSync Python SDK cookbook

Short copy-paste recipes against `aerosync` ≥ `0.3.0`. Authoritative API detail remains in
[`docs/v0.3.0-frozen-api.md`](../v0.3.0-frozen-api.md) and RFC-001.

## Send and wait for terminal receipt

```python
import asyncio
import aerosync

async def main():
    async with aerosync.client() as client:
        receipt = await client.send("/tmp/report.bin", to="http://127.0.0.1:7788/upload")
        outcome = await receipt.processed()
        print(outcome)

asyncio.run(main())
```

## Receive loop with metadata-aware filtering

```python
import asyncio
import aerosync

async def main():
    async with aerosync.receiver(listen="0.0.0.0:7788", save_dir="./inbox") as rx:
        async for incoming in rx:
            meta = incoming.metadata  # flat dict: trace_id, user keys, …
            if meta.get("tenant") == "acme":
                await incoming.ack()
                print("accepted", incoming.file_name, "→", incoming.path)

asyncio.run(main())
```

## History query by trace id or content type

```python
import asyncio
import aerosync

async def main():
    async with aerosync.client() as client:
        rows = await client.history(
            limit=50,
            trace_id="run-123",
            content_type_contains="image/",
        )
        for row in rows:
            print(row.file_name, row.trace_id, row.content_type)

asyncio.run(main())
```

## Recoverable receipts after restart (RFC-002 §8 journal)

Lists rows whose latest journal state is **non-terminal**, using the default SQLite journal next to
history (`~/.config/aerosync/receipts.db` unless overridden).

```python
import asyncio
import aerosync

async def main():
    pending = await aerosync.recover()
    for r in pending:
        print(r.receipt_id, r.state, r.filename)

asyncio.run(main())
```

Pass an explicit path when testing:

```python
pending = await aerosync.recover(journal_path="/tmp/wd/receipts.db")
```

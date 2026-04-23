# aerosync-rendezvous

Standalone **rendezvous control plane** for RFC-004 (WAN peer registry + future
signaling). Ships as its own binary so operators can deploy one SQLite file +
reverse-proxy TLS without pulling the full `aerosync` receiver stack.

**Status:** v0.4 scaffold — schema + HTTP health/status only; `/v1/peers/*` API
lands in follow-up tasks per `docs/rfcs/RFC-004-wan-rendezvous.md`.

## Run (dev)

```bash
cargo run -p aerosync-rendezvous -- --bind 127.0.0.1:8787 --database ./rendezvous.db
curl -s http://127.0.0.1:8787/health
curl -s http://127.0.0.1:8787/v1/status
```

Environment: `RENDEZVOUS_DATABASE` overrides `--database` (path to SQLite file).

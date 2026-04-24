# aerosync-rendezvous

Standalone **rendezvous control plane** for RFC-004 (WAN peer registry + future
signaling). Ships as its own binary so operators can deploy one SQLite file +
reverse-proxy TLS without pulling the full `aerosync` receiver stack.

**Status:** Week-1 scope — SQLite schema (RFC §5.4), RS256 JWT, `POST /v1/peers/register`,
`POST /v1/peers/heartbeat`, `GET /v1/peers/:name`, plus HTTP **501** stubs for sessions/relay.

## Run (dev)

Generate an RSA PKCS#8 PEM (2048-bit or larger), then:

```bash
cargo run -p aerosync-rendezvous -- \
  --bind 127.0.0.1:8787 \
  --database ./rendezvous.db \
  --jwt-rsa-private-key ./rendezvous-rsa.pem

curl -s http://127.0.0.1:8787/health
curl -s http://127.0.0.1:8787/v1/status
```

Environment:

- `RENDEZVOUS_DATABASE` — overrides `--database` (SQLite path).
- `RENDEZVOUS_JWT_RSA_PRIVATE_KEY_PATH` — overrides `--jwt-rsa-private-key`.

Optional: `--jwt-issuer`, `--jwt-ttl-secs` (defaults match RFC-004-style deployment notes).

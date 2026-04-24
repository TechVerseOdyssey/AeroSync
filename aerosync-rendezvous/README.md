# aerosync-rendezvous

Standalone **rendezvous control plane** for RFC-004 (WAN peer registry + future
signaling). Ships as its own binary so operators can deploy one SQLite file +
reverse-proxy TLS without pulling the full `aerosync` receiver stack.

**Status:** Control plane + P2 partial — SQLite schema (RFC §5.4), RS256 JWT, `POST /v1/peers/register` (per-IP rate limit), `POST /v1/peers/heartbeat`, `GET /v1/peers/:name` (optional `X-AeroSync-Namespace`), `POST /v1/sessions/initiate`, `GET /v1/sessions/{id}/ws` (signaling stub). **Relay** routes still return **501** until R3. See [`docs/rfcs/RFC-004-p2-protocol-security.md`](../docs/rfcs/RFC-004-p2-protocol-security.md).

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

## Operations (TLS, reverse proxy, addresses)

Production notes (bilingual **EN + 中文**): [`docs/operations/rendezvous.md`](../docs/operations/rendezvous.md) — TLS termination, trusting `X-Forwarded-For`, and what gets stored as `observed_addr`.

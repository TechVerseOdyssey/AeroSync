# RFC-004 P2 — Signaling, hole punch, relay, and public rendezvous hardening

**Status:** implemented in `aerosync-rendezvous` (partial; see below)  
**Parent:** [RFC-004-wan-rendezvous.md](./RFC-004-wan-rendezvous.md)

## 1. Protocol roles (WebSocket vs QUIC)

| Layer | Transport | Purpose |
|-------|-----------|---------|
| **Control / signaling** | HTTPS + **WebSocket** (`GET /v1/sessions/{id}/ws?token=…`) | Peer metadata, ICE/QUIC candidate exchange (future), short-lived JSON messages. |
| **Data plane** | **QUIC** (or negotiated transfer path) | File transfer, receipts; **not** carried on the rendezvous HTTP port. |

- **WebSocket duration:** clients should treat the signaling socket as **short-lived** (connect after `POST /v1/sessions/initiate`, exchange what is needed, then disconnect). The server stub does not yet enforce **idle timeouts** or **max connection time**; production operators should terminate idle WS at the reverse proxy (e.g. 60–120s) until the server adds explicit ping/pong policy.
- **QUIC vs HTTP ports:** the peer’s `observed_addr` and future **STUN reflexive** / **TURN/R3** candidates refer to **UDP/TCP sockets chosen by endpoints**, independent of the rendezvous `bind` port. There is **no** requirement that QUIC reuse the rendezvous TLS port.

## 2. STUN / TURN / R3 relay

| Mechanism | When | AeroSync server today |
|-----------|------|------------------------|
| **STUN** | Discover server-reflexive candidates (NAT) for hole punch | **Optional**; not deployed by default. `InitiateResponse.stun` is `null` until an operator wires a URL. |
| **TURN** | Relay when UDP is blocked or punch fails | **External** product (coturn, cloud TURN). Not part of this binary. |
| **R3 (AeroSync relay)** | First-party relay with `relay_usage` accounting | **Not implemented**; `POST/GET /v1/relay/...` return **501** with a structured JSON body (see §4). |

Clients may use **STUN** for ICE-like discovery; **TURN** remains the standard relay when self-hosted R3 is absent.

## 3. Compatibility with HTTP 501 (existing routes)

- **`POST /v1/sessions/initiate`** — **implemented** (creates a `sessions` row, returns `session_id` and `signaling.websocket_path`). Hole punch and R3 are still **not** implemented; `implementation_status` describes that.
- **`GET /v1/sessions/{id}/ws`** — **stub**: accepts WebSocket upgrade, validates JWT and session membership, sends one `aerosync.signaling.ready` JSON message. No candidate relay yet.
- **`/v1/relay/...`** — **still 501**; response includes `rfc`, `stun_policy`, and `billing_tenant` keys so agents can branch without parsing free-text errors.

## 4. Public rendezvous hardening

### 4.1 Rate limiting

- **`POST /v1/peers/register`:** per **source IP** (from `ConnectInfo`), **~4 sustained / s** with **burst 12** (GCRA via `governor`). Returns **429** with a JSON `error` when exceeded.
- **Note:** limits are **per process**; behind a reverse proxy, configure `X-Forwarded-For` / `Forwarded` at the proxy and ensure the server sees the real client IP (or the limit applies to the proxy IP only).

### 4.2 Multitenant `name` space

- Peers are keyed by **`(namespace, name)`** (migration adds `namespace`, default `''`).
- HTTP header **`X-AeroSync-Namespace`:** optional; empty = default namespace. Allowed characters: ASCII alnum, `.`, `-`, `_`, max 64 bytes.
- JWT includes claim **`ns`** (must match the peer row; lookup requires header namespace to match token `ns`).

### 4.3 Abuse and operations

- **Abuse:** combine register rate limits with proxy-level **connection limits**, **WAF** rules, and **IP blocklists**; the server does not yet ship automated block / abuse scoring.
- **Billing / accounting:** `relay_usage` is prepared in schema; R3 and billing hooks are **not** wired. The 501 relay body mentions `billing_tenant` for forward compatibility with RFC-004 R3.

## 5. References

- [`aerosync-rendezvous/migrations/`](../../aerosync-rendezvous/migrations/) — `20260424120000_rfc004_p2_peers_namespace.sql`
- [`docs/operations/rendezvous.md`](../operations/rendezvous.md) — deploy and proxy headers

---

## 中文摘要

- **信令**走 **WebSocket**；**QUIC 数据面**与 rendezvous 的 HTTP 端口**解耦**。
- **STUN** 可选；**TURN** 用标准外部服务；**R3 真中继**仍 **501**，与既有路由兼容并返回结构化 JSON。
- **全局限流**落在 **`POST /v1/peers/register` 每 IP**；**多租户**用 **`X-AeroSync-Namespace` + JWT `ns` + 库表 `(namespace, name)`**；滥用处置与计费仍以运维 + 未来 R3 为主。

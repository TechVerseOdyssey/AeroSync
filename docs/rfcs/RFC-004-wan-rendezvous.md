# RFC-004: WAN Rendezvous & NAT Traversal

| Field | Value |
| --- | --- |
| Status | Draft |
| Author | TechVerseOdyssey |
| Created | 2026-04-18 |
| Target version | v0.4.0 |
| Wire version | additive to `aerosync-wire/1`; new `aerosync-rendezvous/1` ALPN |
| Estimated work | ~35 engineer-days |
| Depends on | RFC-002 (Receipt), RFC-003 (Metadata) |
| Supersedes | none |

## Implementation status (2026-04-24)

This RFC remains **Draft**; the target release line is **v0.4.0**. The
blurb below matches the **tree as of 2026-04**; for release notes use
`CHANGELOG.md` [Unreleased] in addition to this section.

### Implemented in the codebase

- **Root crate — feature `wan-rendezvous` (default in current `Cargo.toml`,
  can be disabled with `--no-default-features`):**
  - `aerosync::wan::rendezvous::RendezvousClient` — `GET /v1/peers/{name}` with
    `Authorization: Bearer <JWT>`.
  - `parse_peer_at_rendezvous` — parses `peer@rendezvous-host:port` when the
    string has no `://` scheme; path suffixes after the first `/` are kept for
    multi-file `send` (`peer@host:port/rel/...`).
  - `AutoAdapter::with_rendezvous_token` and
    `with_rendezvous_token_from_env` (env **`AEROSYNC_RENDEZVOUS_TOKEN`**);
    rewrites the transfer destination to `http://{observed}/upload/...` before
    the HTTP/QUIC/S3/FTP router runs.
  - CLI `send`, MCP background sends, and the Python `Client` builder chain the
    env-based helper; `negotiate_protocol` / preflight skip `peer@…` forms so
    they are not probed as raw `host:port`.
- **Workspace crate `aerosync-rendezvous` (control plane, week-1 scope):**
  - RFC §5.4 SQLite schema and embedded sqlx migrations.
  - RS256 JWT (issuer/TTL configurable); **register** and **heartbeat** return
    fresh tokens; **lookup** requires Bearer JWT.
  - `POST /v1/peers/register`, `POST /v1/peers/heartbeat`, `GET /v1/peers/:name`
    (`:name` is the registered peer name; see handler for conflict rules).
  - P2: optional header **`X-AeroSync-Namespace`**, unique **`(namespace, name)`**,
    **`POST /v1/sessions/initiate`**, **`GET /v1/sessions/{id}/ws`** (signaling stub;
    not full ICE/QUIC over WS yet), per-IP **register** rate limit (HTTP 429).
    Protocol notes and 501/relay story: [`RFC-004-p2-protocol-security.md`](./RFC-004-p2-protocol-security.md).
  - **Relay** paths `POST/GET /v1/relay/...` still **HTTP 501** (R3) with
    **structured** JSON.
  - Process must be started with a PKCS#8 RSA private key PEM
    (`--jwt-rsa-private-key` or `RENDEZVOUS_JWT_RSA_PRIVATE_KEY_PATH`); see
    `aerosync-rendezvous/README.md`.
- **Feature `wan-relay`:** still a **placeholder** module in `src/wan` — no
  data-plane relay implementation in-tree yet.
- **Python `Config.rendezvous_url` (and similar hooks):** reserved; week-1
  lookup uses the **`peer@host:port` destination** plus env token, not a
  separate base-URL field on `Config`.

### Not yet implemented (vs this RFC)

- **UDP hole punching**, end-to-end candidate exchange, and a **working**
  byte relay with accounting (R3) — **relay** routes 501; WS is only a
  **stub** (no production ICE/relay yet; see
  [RFC-004-p2-protocol-security.md](./RFC-004-p2-protocol-security.md)).
- **Identity/ACL** beyond “JWT from our RSA key + register-time name/key”;
  **§13.1 MCP tools** (`register_peer` server-side, etc.) as a first-class
  product surface.
- **ALPN `aerosync-rendezvous/1`** and the full control/data split described
  for production; current client talks **HTTP(S)** to the rendezvous HTTP API
  only.
- **End-to-end “it just works” WAN** without running a rendezvous, distributing
  keys, and ensuring `observed_addr` is reachable (NAT asymmetry, TLS to peer,
  etc. are all out of scope for week-1).

### Current source of truth

- **Design** — this RFC.
- **What is actually built** — `CHANGELOG.md` [Unreleased],
  `aerosync-rendezvous/README.md`, and the source paths referenced above;
  `docs/ARCHITECTURE_AND_DESIGN.md` and `docs/v0.3.0-frozen-api.md` for broader
  architecture; the latter reflects the **v0.3.0 API freeze** and may trail
  unreleased v0.4 work on `main`.

## 1. Summary

v0.2 makes AeroSync's transfer + receipt + metadata story complete on
**LAN**. It does not attempt cross-internet peer discovery or NAT
traversal — peers must already know each other's `host:port` and have
a network path between them. This is fine for `LAN + mDNS` and for the
narrow case where one side has a public IP plus a forwarded port, but
it is **not** the use case the README headlines:

> "one agent on machine A asks another agent on machine B 'send me
> that 30 GB dataset', and it just works — over LAN (QUIC,
> mDNS-discovered) or WAN (HTTP fallback), resumable, with a single
> binary on each side."

For two agents both behind home / cloud / k8s NATs to find each other
and complete a direct transfer, three pieces are missing today:

1. **Rendezvous** — a public service that knows where each peer is,
   so agent A can resolve "alice's laptop" to a current network
   address.
2. **NAT traversal** — once both peers' server-reflexive addresses
   are known, both must punch a UDP hole simultaneously so a QUIC
   handshake survives both NATs' state.
3. **Relay** — when symmetric NATs or strict firewalls defeat hole
   punching (~20% of real-world peer pairs), bytes must traverse a
   neutral relay node.

This RFC defines the rendezvous service, the hole-punch coordination
flow, the relay fallback, the identity model that makes any of it
safe, and the SDK / MCP surface that exposes it.

> **Compatibility scope.** RFC-004 is **additive** to `aerosync-wire/1`.
> Existing v0.2 LAN peers do not need to know about rendezvous to
> keep working. v0.4 peers that opt into rendezvous gain a new
> control-plane ALPN `aerosync-rendezvous/1` for talking to the
> rendezvous server, while their data plane keeps `aerosync/1`.

## 2. Goals

- A self-hostable rendezvous server (one binary + one SQLite file)
  that any AeroSync user can run for their own namespace.
- Direct peer-to-peer (P2P) connections in ~80% of real-world NAT
  pairs without relaying any data, via coordinated QUIC hole
  punching.
- A relay fallback for the remaining ~20% with a clear
  bandwidth-accounting model so relay operators know what they're on
  the hook for.
- Mutual peer authentication using per-peer Ed25519 keys; rendezvous
  cannot impersonate peers, peers cannot impersonate other peers.
- Idiomatic SDK surface: `client.send(to="alice@example.com", ...)`
  Just Works once `Config(rendezvous_url=..., peer_name=...)` is set.
- Cheap MVP path: ~35 engineer-days for the full lane (server +
  client + SDK + docs); ~14 d for "rendezvous-only, no relay" if
  the budget is tight.

## 3. Non-goals

- **Replacing Tailscale / Nebula / WireGuard.** AeroSync does not aim
  to be a general-purpose mesh VPN. We solve P2P file transfer; users
  who need a full L3 overlay should layer that on separately.
- **Anonymous peer-to-peer.** Every peer presents a stable public
  identity (their Ed25519 pubkey) to the rendezvous; pseudonyms are
  fine, anonymity is not a goal.
- **End-to-end encryption beyond QUIC TLS.** The rendezvous can see
  *metadata* (peer names, addresses, capability bits) but not file
  bytes (those go through QUIC TLS). Cryptographic file-level E2E
  (where even the rendezvous-operated relay can't see plaintext)
  is deferred to RFC-005 (Identity + Sealed Transfers).
- **WebRTC interop.** Adopting libdatachannel / building an SDP-style
  signaling layer would let browsers be peers but quadruples the
  surface. We stay on raw QUIC + a small custom signaling protocol.
- **Federation across rendezvous servers.** Each peer registers with
  exactly one rendezvous; cross-rendezvous lookup is deferred to a
  hypothetical RFC-006.
- **NAT type detection / classification (RFC 5780).** We always try
  direct first and fall back to relay; we don't probe NAT behavior
  ahead of time (saves complexity, costs ~1s on the failure path).

## 4. Architecture overview

```
┌──────────────────────────────────────────────────────────────────┐
│                         Public internet                          │
│                                                                  │
│              ┌──────────────────────────────┐                    │
│              │  rendezvous.example.com      │                    │
│              │  (axum + sqlx + ws)          │                    │
│              │  - peer registry             │                    │
│              │  - session signaling         │                    │
│              │  - relay (optional)          │                    │
│              └────┬───────────────────┬─────┘                    │
│                   │ HTTPS+WS          │ HTTPS+WS                 │
│                   │                   │                          │
│        ┌──────────┴───────┐  ┌────────┴──────────┐               │
│        │ Agent A (sender) │  │ Agent B (receiver)│               │
│        │   alice@         │  │     bob@          │               │
│        │   public IP via  │  │   public IP via   │               │
│        │   server-reflex  │  │   server-reflex   │               │
│        └────────┬─────────┘  └────────┬──────────┘               │
│                 │       ┌─────────────┘                          │
│                 │       │   coordinated UDP punch                │
│                 ▼       ▼                                        │
│            ╔══════════════════════╗                              │
│            ║ Direct QUIC stream   ║  ← happy path (~80%)         │
│            ║ aerosync/1 ALPN      ║                              │
│            ╚══════════════════════╝                              │
│                                                                  │
│                    OR (fallback ~20%)                            │
│                                                                  │
│            ╔══════════════════════╗                              │
│            ║ Relayed QUIC stream  ║  ← via rendezvous            │
│            ║ aerosync-relay/1     ║                              │
│            ╚══════════════════════╝                              │
└──────────────────────────────────────────────────────────────────┘
```

The rendezvous server is the only piece that needs a public IP. Agents
can be anywhere — laptops on home WiFi, workloads in NAT'd k8s clusters,
ephemeral CI runners — as long as they can reach the rendezvous over
HTTPS.

## 5. R1: Rendezvous service

### 5.1 HTTP API

All endpoints are JSON over HTTPS, JWT-authenticated except where
noted. JWTs are signed by the rendezvous server's RSA-2048 key and
carry `{ sub: peer_id, exp, scope }`. JWT issuance flow described in
§8.

| Method | Path                          | Auth   | Purpose                                                                 |
| ------ | ----------------------------- | ------ | ----------------------------------------------------------------------- |
| POST   | `/v1/peers/register`          | none\* | Initial registration: peer presents `name`, `public_key`, `capabilities` |
| POST   | `/v1/peers/heartbeat`         | JWT    | Periodic (every 30s): refresh observed address + TTL                    |
| GET    | `/v1/peers/{name}`            | JWT    | Lookup: returns last observed addr, capabilities, public_key            |
| POST   | `/v1/sessions/initiate`       | JWT    | Sender asks rendezvous to broker a session with `target_name`           |
| GET    | `/v1/sessions/{id}/ws`        | JWT    | WebSocket: real-time signaling for hole punch (both sides connect)      |
| POST   | `/v1/relay/{session_id}/up`   | JWT    | R3 fallback: stream bytes upstream (sender → relay → receiver)          |
| GET    | `/v1/relay/{session_id}/down` | JWT    | R3 fallback: stream bytes downstream                                    |
| GET    | `/v1/health`                  | none   | Operator health check                                                   |
| GET    | `/v1/version`                 | none   | Server build info, supported protocol versions                          |

\* `/register` is unauthenticated **only on first call** for a given
`(name, public_key)` pair; the response carries the JWT used for all
subsequent calls. Re-registration of the same name with a different
public_key is rejected (404 → 409 conflict).

### 5.2 Registration flow

```
Agent A first run:
  1. Generate Ed25519 keypair, store at ~/.aerosync/identity.key
  2. POST /v1/peers/register
     body: {
       name: "alice",
       public_key: "<base64 ed25519 pubkey>",
       capabilities: 0b0011  // SUPPORTS_RECEIPTS | SUPPORTS_BYTES_RECEIVED
     }
  3. Server records: peer_id = sha256(public_key), name → peer_id,
     allocates JWT, returns:
     {
       peer_id: "sha256:abc...",
       jwt: "eyJ...",
       jwt_expires_at: "2026-04-19T...",
       observed_addr: "203.0.113.4:54321"  // server-reflexive
     }
  4. Agent stores JWT, schedules heartbeat every 30s

Subsequent runs (same identity.key):
  1. POST /v1/peers/register with same pubkey → server returns
     fresh JWT (no conflict because pubkey matches)
```

### 5.3 Session initiation flow

```
Agent A wants to send to Agent B:

  A → rendezvous: POST /v1/sessions/initiate
                  body: { target_name: "bob", requested_caps: 0b0011 }

  Rendezvous → A: { session_id, signaling_url, target_observed_addr,
                    target_public_key, target_capabilities }

  A opens WS to /v1/sessions/{id}/ws
  Rendezvous notifies B (via B's open heartbeat WS or push notification)
  B opens WS to /v1/sessions/{id}/ws

  Both A and B exchange via WS:
    - their server-reflexive addrs (from rendezvous's observation)
    - any local addrs they want to advertise (LAN-side fallback)
    - a synchronized "punch_at" timestamp (NTP-like, via rendezvous)

  At punch_at - 50ms:
    Both sides start firing QUIC client+server handshakes at each other
    on all candidate addrs. First successful handshake wins; the other
    candidates time out gracefully.

  On success:
    A and B have a direct QUIC connection. They proceed with the
    standard aerosync/1 transfer (RFC-002), with the receipt stream
    (RFC-002 §4) flowing in-band as usual.

  On failure (after 3s with no successful handshake):
    Both sides post a "punch_failed" frame to the WS.
    Rendezvous responds with relay_url; both sides reconnect via R3.
```

### 5.4 Storage schema (SQLite)

```sql
CREATE TABLE peers (
  peer_id        TEXT PRIMARY KEY,         -- sha256:base64(pubkey)
  name           TEXT UNIQUE NOT NULL,     -- e.g. "alice"
  public_key     BLOB NOT NULL,            -- 32 bytes
  capabilities   INTEGER NOT NULL,         -- u32 bitmask
  observed_addr  TEXT,                     -- "203.0.113.4:54321"
  last_seen_at   INTEGER NOT NULL,         -- unix epoch seconds
  created_at     INTEGER NOT NULL
);
CREATE INDEX idx_peers_last_seen ON peers(last_seen_at);

CREATE TABLE sessions (
  session_id     TEXT PRIMARY KEY,
  initiator_id   TEXT NOT NULL REFERENCES peers(peer_id),
  target_id      TEXT NOT NULL REFERENCES peers(peer_id),
  state          TEXT NOT NULL,            -- 'pending' | 'punching' | 'direct' | 'relayed' | 'failed' | 'completed'
  created_at     INTEGER NOT NULL,
  closed_at      INTEGER
);
CREATE INDEX idx_sessions_state ON sessions(state, created_at);

-- Optional: relay accounting (only populated when R3 is enabled)
CREATE TABLE relay_usage (
  session_id     TEXT NOT NULL REFERENCES sessions(session_id),
  bytes_relayed  INTEGER NOT NULL,
  recorded_at    INTEGER NOT NULL
);
```

### 5.5 Server stack

- Pure Rust: axum 0.7 + sqlx 0.8 + jsonwebtoken + tower-http
- Single static binary `aerosync-rendezvous` shipped from the same
  cargo workspace as `aerosync`.
- Default deployment: 1 binary + 1 `rendezvous.db` SQLite file +
  reverse proxy (caddy / nginx) terminating TLS.
- Resource floor: 50 MB RAM, < 1% CPU at 1k peers and 100 sessions/min.
  Scales horizontally by sharding by peer_id once a single instance is
  saturated; stateful WS sessions stick to the originating instance.

## 6. R2: NAT traversal (QUIC hole punching)

### 6.1 Why hole punching works for QUIC

QUIC runs over UDP. Most consumer / cloud NATs preserve the
`(src_addr, src_port) → (translated_src_addr, translated_src_port)`
mapping for any outbound packet for ~30-60s. If both peers send
outbound UDP packets to each other's *predicted* translated address
within that window, both NATs will accept the inbound replies as
"part of the existing flow" and forward them through. QUIC's TLS
handshake then completes over the established UDP path.

This is the same technique WireGuard, Tailscale, Syncthing, and
WebRTC use. Success rate on real-world Internet (per Tailscale's
public data) is ~95% for cone NATs, ~0% for symmetric NATs. We
target the cone-NAT 80% of the population and relay the rest.

### 6.2 Coordination protocol (over rendezvous WS)

After both peers have connected to `/v1/sessions/{id}/ws`:

```
A → rendezvous:  { type: "candidates",
                   server_reflexive: "203.0.113.4:54321",
                   local: ["192.168.1.10:54321"] }
B → rendezvous:  { type: "candidates",
                   server_reflexive: "198.51.100.7:60000",
                   local: ["10.0.0.5:60000"] }

Rendezvous fans out each side's candidates to the other side, then
broadcasts:

rendezvous → A,B:  { type: "punch_at", timestamp_ms: <now + 200ms> }

At punch_at (NTP-style synchronization within ~10ms):
  A starts a quinn::Endpoint listening on its local UDP socket and
  initiates handshakes to B's candidate addrs simultaneously.
  B does the same.

  Both sides will see a few candidates fail (LAN addr unreachable
  from internet, e.g.). The first successful handshake on either
  side wins; both peers cancel the others.

A → rendezvous:  { type: "punch_succeeded", chosen_addr: "..." }
   (or)
A → rendezvous:  { type: "punch_failed", tried: [...] }
```

### 6.3 quinn 0.11 socket reuse

Both `quinn::Endpoint::client()` and `Endpoint::server_config()` need
to share the same `UdpSocket` so the same `(src_ip, src_port)` is used
for both inbound (server) and outbound (client) — this is what makes
the NAT mapping match. quinn supports this via
`Endpoint::new_with_abstract_socket()` which we already use indirectly
through `Endpoint::server()`. We need to explicitly bind the socket
ourselves and pass it to *both* the client + server endpoint.

### 6.4 Failure modes & timeouts

| Failure                          | Detection                  | Action                       |
| -------------------------------- | -------------------------- | ---------------------------- |
| One peer doesn't show up at WS   | 10s timeout post-`initiate`| 408, sender retries          |
| `punch_at` skew > 100ms          | NTP gap detected at server | Re-broadcast new `punch_at`  |
| All candidates fail              | 3s after `punch_at`        | Fall back to R3 relay        |
| Direct QUIC handshake hangs      | quinn idle_timeout (15s)   | Fall back to R3              |
| Symmetric NAT (port unpredictable)| Implicit (punch fails)    | Fall back to R3              |

## 7. R3: Relay (TURN-style fallback)

### 7.1 Wire encoding

When direct fails, the rendezvous server (or a separate relay node it
designates) acts as a transparent splice. Each peer opens a QUIC
connection to the relay with ALPN `aerosync-relay/1` and a
`session_id` param; the relay pairs them up and forwards bytes
transparently.

```
Peer A ──QUIC──► Relay ──QUIC──► Peer B
       (ALPN aerosync-relay/1)
```

The relay does **not** decrypt; bytes are TLS-encrypted end-to-end
between A and B (the relay's QUIC just multiplexes streams). Frames
on the relay's QUIC connection look like:

```protobuf
message RelayFrame {
  uint64 stream_id = 1;     // mux key per direction
  bytes  payload   = 2;     // opaque ciphertext from app QUIC
}
```

### 7.2 Bandwidth accounting

Relays are expensive. Each `RelayFrame.payload.len()` is added to
the `relay_usage` table at session close. An operator can cap by:
- Per-session bytes (default: 10 GiB)
- Per-peer monthly bytes (default: 100 GiB free tier; configurable)
- Per-namespace concurrent sessions (default: 100)

### 7.3 Relay placement

- **Single-host MVP**: same process as rendezvous, listens on a
  separate port (default 7891). Adds ~200 lines of code.
- **Multi-host**: `aerosync-relay` binary, separate deployment,
  rendezvous picks the geographically nearest relay from a registered
  set. Out of scope for v0.4.0.

## 8. R4: Identity & auth

### 8.1 Peer identity

Every peer holds an Ed25519 keypair at `~/.aerosync/identity.key`
(0600 perms). The `peer_id` is `sha256(public_key)`, base64-url-encoded.
Two AeroSync installations with the same identity key are the
**same peer** (think `~/.ssh/id_ed25519`).

`aerosync identity show` prints peer_id + public key.
`aerosync identity rotate` generates a fresh key (default v0.4.0:
manual; auto-rotation in v0.4).

### 8.2 Rendezvous JWT

On registration, the rendezvous issues a JWT signed by the server's
RSA key. Claims:

```json
{
  "iss": "rendezvous.example.com",
  "sub": "sha256:abc...",      // peer_id
  "iat": 1745000000,
  "exp": 1745086400,           // 24 h
  "scope": ["lookup", "session", "relay"],
  "name": "alice"
}
```

The peer presents this JWT in the `Authorization: Bearer ...` header
for every subsequent call. JWTs auto-rotate via heartbeat: each
heartbeat response carries a fresh JWT if the current one is within
1h of expiry.

### 8.3 Mutual peer auth (per-session)

When peer A initiates a session with peer B, the rendezvous includes
B's `public_key` in A's response. Once A and B have a direct QUIC
connection, they exchange a handshake frame signed with their
respective Ed25519 keys; each side verifies the other's signature
matches the public_key the rendezvous published. **This means a
malicious rendezvous cannot impersonate B to A** (they'd need B's
private key to forge the signed handshake).

```protobuf
message PeerAuthChallenge {
  bytes  nonce        = 1;     // 32 bytes random
  uint64 timestamp_ms = 2;
}

message PeerAuthResponse {
  bytes signature = 1;          // Ed25519(challenge_bytes || timestamp)
}
```

### 8.4 ACL (per-peer allowlist)

Each peer maintains `~/.aerosync/acl.toml`:

```toml
[[allow]]
peer_id = "sha256:abc..."     # alice
name    = "alice"             # for display only
ops     = ["send", "receive"]
paths   = ["/inbox/**"]       # glob

[[allow]]
peer_id = "sha256:def..."     # bob
ops     = ["receive"]         # bob can ask us to send TO him
                              # but cannot push files TO us
paths   = ["/data/exports/**"]
```

Default ACL is `deny all` — out of the box, no peer can send or
request anything until explicitly allowed. The CLI offers
`aerosync allow <peer_id> --ops send,receive --paths "/inbox/**"`
for convenience.

### 8.5 Threat model

| Adversary                          | Threat                       | Mitigation                                |
| ---------------------------------- | ---------------------------- | ----------------------------------------- |
| Network observer                   | Eavesdrop on file bytes      | QUIC TLS (already in v0.2)                |
| Network observer                   | Eavesdrop on rendezvous calls| HTTPS to rendezvous                       |
| Malicious rendezvous operator      | Impersonate peer B to peer A | Per-session Ed25519 challenge (§8.3)      |
| Malicious rendezvous operator      | Inject MITM on file bytes    | If rendezvous is also relay (R3), it can't decrypt; if peers go direct (R2), rendezvous is out of the path |
| Compromised peer key               | Steal alice's identity       | Manual rotation; future revocation list   |
| Unauthorized peer                  | Push files to me             | ACL default-deny + per-peer allowlist     |
| DoS on rendezvous                  | Take down all WAN sessions   | Rate limit + captcha for free tier (op concern, not protocol) |
| Replay of session_initiate         | Hijack a stale session       | nonce + timestamp in PeerAuthChallenge    |

### 8.6 What we do NOT defend against

- **Compromised rendezvous + compromised relay** — if the same operator
  controls both, they could MITM in the relayed path. Mitigation:
  self-host. Defense-in-depth: RFC-005 will add E2E payload encryption
  with peer-derived keys.
- **Side-channel deanonymization** — rendezvous knows which peers talk
  to which other peers, and when. This is a fundamental property of
  any rendezvous system; users who need traffic-flow anonymity should
  use Tor / similar.

## 9. R5: Naming & discovery

### 9.1 Format

Peer names are `local-part@rendezvous-domain`, mailto-style:

- `alice@rendezvous.example.com`
- `prod-cleaner@aerosync.cloud`
- `ci-runner-42@my-org.internal`

The `local-part` is unique within the rendezvous's namespace
(case-insensitive, ASCII a-z 0-9 hyphen, 1-64 chars). The
`@rendezvous-domain` is the rendezvous's HTTPS host (no port; default
443).

### 9.2 Resolution

```
client.send(to="alice@example.com", ...) flow:

  1. Parse "alice@example.com" → (name="alice", rendezvous="example.com")
  2. Check resolver cache (TTL = 60s)
  3. Cache miss: HTTPS GET https://example.com/v1/peers/alice
  4. Validate JWT, check capabilities, get observed_addr + public_key
  5. Cache result, proceed to §5.3 session initiation flow
```

### 9.3 Default rendezvous

For zero-config users, the SDK defaults to `aerosync.cloud` if no
rendezvous_url is configured AND the destination has the form
`name@aerosync.cloud`. This is the "managed" experience — a free tier
the project operates. Self-hosters pin their own rendezvous via
`Config(rendezvous_url="https://my-rendezvous.example.com")`.

> **Operational commitment**: if we run `aerosync.cloud`, we need to
> commit to ≥ 6 months of free-tier SLA before encouraging adoption.
> This is a product / operations decision, not a protocol decision.
> See open question Q1.

### 9.4 Aliases & vanity names

Out of scope for v0.4.0. Future RFC may add `aerosync alias add foo`
to map `foo@my-rendezvous` to the same peer_id as the canonical name.

## 10. R6: Deployment model

### 10.1 Self-hosted (primary path)

```bash
# Operator
docker run -d \
  -p 443:443 \
  -v rendezvous-data:/data \
  -e DOMAIN=rendezvous.example.com \
  -e TLS_CERT=/data/cert.pem -e TLS_KEY=/data/key.pem \
  aerosync/rendezvous:v0.4.0

# User
aerosync config set rendezvous_url https://rendezvous.example.com
aerosync identity init  # generates keypair, registers
aerosync receive --name alice  # listens for incoming
```

### 10.2 Managed (`aerosync.cloud`)

```bash
# User, no setup beyond:
pip install aerosync
aerosync identity init  # auto-registers with aerosync.cloud
aerosync receive --name alice  # listens
# someone sends: aerosync send file.bin alice@aerosync.cloud
```

The managed offering, **if the project chooses to run it**:
- Free tier: 100 GB/mo relay, 1k sessions/mo, no SLA
- No paid tier in v0.4; revisit after 6 months of usage data
- Source-available rendezvous binary so any user can bring their own

### 10.3 Migration from v0.2 (LAN-only)

v0.2 deployments continue to work without rendezvous. To opt in:

1. Upgrade to v0.4.
2. Set `rendezvous_url` in `~/.aerosync/config.toml` (or env).
3. Run `aerosync identity init` (one-time).
4. Existing LAN flows still use mDNS — rendezvous is only consulted
   when the destination matches the `name@rendezvous` pattern.

## 11. Wire-format additions

### 11.1 `aerosync-wire/1` additions (additive, no version bump)

Extend `proto/aerosync/wire/v1.proto`:

```protobuf
// New top-level wire message for the rendezvous control channel
// (sent over ALPN aerosync-rendezvous/1, NOT the data plane).
message RendezvousFrame {
  oneof body {
    RegisterPeer    register   = 1;
    PeerLookup      lookup     = 2;
    SessionInitiate initiate   = 3;
    SessionSignal   signal     = 4;
  }
}

message RegisterPeer {
  string name         = 1;
  bytes  public_key   = 2;
  uint32 capabilities = 3;
  bytes  signature    = 4;  // Ed25519(name || pubkey || caps)
}

message PeerLookup {
  string target_name = 1;
}

message PeerLookupResponse {
  string peer_id      = 1;
  string observed_addr = 2;
  uint32 capabilities  = 3;
  bytes  public_key    = 4;
}

message SessionInitiate {
  string target_peer_id = 1;
  uint32 requested_caps = 2;
}

message SessionSignal {
  string session_id = 1;
  oneof body {
    CandidateAddrs candidates    = 2;
    PunchAt        punch_at      = 3;
    PunchSucceeded punch_success = 4;
    PunchFailed    punch_failed  = 5;
  }
}

message CandidateAddrs {
  string server_reflexive = 1;
  repeated string local    = 2;
}

message PunchAt        { uint64 timestamp_ms = 1; }
message PunchSucceeded { string chosen_addr  = 1; }
message PunchFailed    { repeated string tried = 1; }
```

These are additive: v0.2 peers don't see them because they don't open
the `aerosync-rendezvous/1` ALPN.

### 11.2 New ALPN: `aerosync-rendezvous/1`

For rendezvous-server ↔ peer control plane. Distinct from the
`aerosync/1` data plane so a single endpoint can run both with no
ambiguity.

### 11.3 New ALPN: `aerosync-relay/1`

For peer ↔ relay-node multiplexed transport. The encapsulated bytes
inside `RelayFrame.payload` are themselves a `aerosync/1` QUIC stream
between the two peers — the relay just splices.

## 12. Python SDK changes

### 12.1 `Config` additions

```python
@dataclass
class Config:
    # ... existing v0.2 fields ...

    # NEW in v0.4 (RFC-004)
    rendezvous_url: str | None = None       # https://rendezvous.example.com
    identity_path: Path | None = None        # default: ~/.aerosync/identity.key
    peer_name: str | None = None             # the local-part for this peer
```

### 12.2 New error types

```python
class RendezvousError(AeroSyncError): ...       # rendezvous unreachable / 5xx
class PeerNotReachableError(AeroSyncError): ... # name resolved but peer offline
class NatTraversalFailedError(AeroSyncError): ...  # punch failed AND relay disabled
class IdentityError(AeroSyncError): ...         # identity.key missing / malformed
class AclDeniedError(AuthError): ...            # remote peer's ACL refused us
```

### 12.3 Send/receive surface

No new methods. The `to=` parameter accepts a name:

```python
async with aerosync.client(
    config=Config(rendezvous_url="https://r.example.com", peer_name="alice")
) as c:
    receipt = await c.send("file.bin", to="bob@r.example.com")
    await receipt.processed()
```

```python
async with aerosync.receiver(name="alice") as r:  # auto-registers
    async for incoming in r:
        await incoming.ack()
```

### 12.4 New CLI sub-commands

```bash
aerosync identity init       # generate keypair + register
aerosync identity show       # print peer_id + public key
aerosync identity rotate     # generate fresh key (manual ack required)

aerosync allow <peer_id> --ops send,receive --paths "/inbox/**"
aerosync deny  <peer_id>
aerosync acl list

aerosync send file.bin bob@r.example.com  # name-form destination
aerosync receive --name alice             # auto-registers if rendezvous configured
```

## 13. MCP changes

### 13.1 New tools

| Tool                        | Purpose                                                          |
| --------------------------- | ---------------------------------------------------------------- |
| `register_peer`             | Initial registration with rendezvous                             |
| `lookup_peer`               | Resolve `name@rendezvous` to current addr + capabilities         |
| `list_acl`                  | Inspect local ACL allowlist                                      |
| `allow_peer` / `deny_peer`  | Edit ACL                                                         |

### 13.2 Existing tools extended

- `send_file` / `send_directory` / `request_file` accept either
  `host:port` or `name@rendezvous` as `destination`.
- `discover_receivers` gains a `rendezvous_url` parameter; if set,
  enumerates peers from rendezvous instead of (or in addition to) mDNS.

## 14. Open questions

| #  | Question                                                                                | Default (ships unless changed)                                                  |
| -- | --------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| Q1 | Do we operate a public `aerosync.cloud`?                                                | **Yes**, with 6-month free-tier SLA commitment in writing                       |
| Q2 | Can peers re-register the same name with a new pubkey (key rotation)?                   | **No** for v0.4.0; manual `aerosync identity rotate` requires a new name        |
| Q3 | Does the rendezvous expose a peer-listing endpoint?                                     | **No** by default (privacy); per-namespace operator can opt-in                  |
| Q4 | What's the default JWT lifetime?                                                        | **24h**, refreshed via heartbeat                                                |
| Q5 | Do we ship the relay in the same binary as rendezvous?                                  | **Yes** for v0.4.0 (single binary); split possible in v0.4.1                   |
| Q6 | What capability bit signals "willing to relay for others"?                              | Defer to v0.4.1; v0.4.0 only the rendezvous operator runs relays                |
| Q7 | Should ACL support `deny` rules, not just allowlist?                                    | **No** for v0.4.0; allowlist-only keeps the model simple                        |
| Q8 | Should we encrypt `~/.aerosync/identity.key` at rest with a passphrase?                 | **No** by default (matches `~/.ssh/id_ed25519` ergonomics); CLI flag for opt-in |
| Q9 | What happens when both peers initiate to each other simultaneously (glare)?             | Rendezvous deduplicates by `(min(peer_id), max(peer_id))`; first wins           |
| Q10| End-to-end payload encryption (so a malicious relay can't see plaintext)?               | **Defer** to RFC-005 (Identity + Sealed Transfers)                              |
| Q11| Federation across multiple rendezvous?                                                  | **No** for v0.4.0; one peer = one rendezvous                                    |
| Q12| WebRTC compat (for browser peers)?                                                      | **No**; raw QUIC only. Browser support waits for QUIC-in-browser maturity       |

## 15. Implementation tasks

| #   | Task                                                                                             | Owner    | Days |
| --- | ------------------------------------------------------------------------------------------------ | -------- | ---- |
|  1  | `aerosync-rendezvous` crate scaffold (axum + sqlx + jsonwebtoken + tower-http)                   | Rust     | 1.0  |
|  2  | Storage schema + migrations (sqlx-migrate)                                                       | Rust     | 0.5  |
|  3  | `/v1/peers/register` + JWT issuance + tests                                                      | Rust     | 1.0  |
|  4  | `/v1/peers/heartbeat` + `/v1/peers/{name}` + tests                                               | Rust     | 1.0  |
|  5  | `/v1/sessions/initiate` + `/v1/sessions/{id}/ws` signaling + tests                               | Rust     | 1.5  |
|  6  | Hole-punch coordination protocol (CandidateAddrs / PunchAt / Punch{Succeeded,Failed})            | Rust     | 1.0  |
|  7  | Relay (R3) endpoint + bandwidth accounting + per-session caps                                    | Rust     | 1.5  |
|  8  | Operator runbook + Dockerfile + fly.io config + caddy reverse-proxy example                      | Rust+Ops | 1.0  |
|  9  | `RendezvousClient` in `aerosync-core` (HTTPS + WS + JWT refresh)                                 | Rust     | 1.5  |
| 10  | quinn 0.11 socket reuse for hole-punch (shared `UdpSocket` between client + server endpoint)     | Rust     | 1.0  |
| 11  | Hole-punch driver: parallel candidate handshakes, first-success-wins                             | Rust     | 1.5  |
| 12  | R3 relay client (open relay QUIC, splice in/out)                                                 | Rust     | 1.0  |
| 13  | Identity module: Ed25519 keypair, `~/.aerosync/identity.key`, sign/verify helpers                | Rust     | 1.0  |
| 14  | Per-session mutual auth (PeerAuthChallenge / Response) + integration with `Receipt`              | Rust     | 1.0  |
| 15  | ACL parser (`~/.aerosync/acl.toml`) + glob matcher + enforcement in `FileReceiver`               | Rust     | 1.5  |
| 16  | CLI: `identity init/show/rotate`, `allow`, `deny`, `acl list`                                    | Rust     | 1.0  |
| 17  | `name@rendezvous` resolver + 60s cache + integration into `TransferEngine`                       | Rust     | 1.0  |
| 18  | Python SDK `Config` extensions + new error types                                                 | Python   | 0.5  |
| 19  | Python SDK `client.send(to="name@...")` integration test (cross-process via real rendezvous)     | Python   | 1.0  |
| 20  | Python SDK `receiver(name=...)` auto-registration + integration test                             | Python   | 1.0  |
| 21  | MCP tools: `register_peer`, `lookup_peer`, `list_acl`, `allow_peer`, `deny_peer`                 | Rust     | 1.0  |
| 22  | MCP `send_file` / `send_directory` / `request_file` accept `name@rendezvous` form                | Rust     | 0.5  |
| 23  | NAT traversal coverage matrix: tested combinations (cone-cone, cone-symmetric, etc.) + report    | Rust+Ops | 2.0  |
| 24  | RFC-004 → docs/protocol/wan-rendezvous-v1.md (user-facing wire spec)                             | Docs     | 1.0  |
| 25  | docs/wan-quickstart.md (end-to-end "set up your own rendezvous + run two peers" walkthrough)     | Docs     | 1.0  |
| 26  | CHANGELOG v0.4.0 + migration notes from v0.2 + launch blog                                       | Docs     | 1.0  |
| 27  | Cross-RFC integration smoke (RFC-002 + RFC-003 + RFC-004 e2e over WAN-style setup)               | Rust     | 1.5  |
| 28  | Hardening, release candidate, real-NAT field testing                                             | All      | 2.0  |
|     | **Total**                                                                                        |          | **~32.5** |

Calendar mapping at 5 focused days/week:
- Single developer, 100% focus: ~7 weeks
- 1.5 developers (1 senior Rust + 0.5 Python): ~5 weeks
- 2 developers (Rust lane + Python lane in parallel after task #18 unblocks): ~4 weeks

Add ~30% for review cycles + design-partner feedback + the inevitable
NAT-zoo bugs that only surface against real ISP hardware: practical
calendar range **~6-10 weeks**.

## 16. Sequencing for v0.4.0

```
week 1   Tasks 1-4 (rendezvous server core: registry, heartbeat, lookup, JWT)
week 2   Tasks 5-7 (signaling WS, relay endpoint) + Task 9 (RendezvousClient)
week 3   Tasks 10-12 (quinn socket reuse, punch driver, relay client)
         Task 13 (identity)
week 4   Tasks 14-17 (mutual auth, ACL, CLI, name resolver)
         Task 18 (Python Config)
week 5   Tasks 19-22 (Python SDK + MCP integration)
         Task 23 starts (NAT matrix testing)
week 6   Task 23 finishes + Task 27 (cross-RFC integration smoke)
week 7   Tasks 8, 24-26 (docs, runbook, CHANGELOG)
         Task 28 (hardening + RC + real-NAT field testing)
```

## 17. Approval

| Role               | Reviewer            | Status          |
| ------------------ | ------------------- | --------------- |
| Author             | TechVerseOdyssey    | Draft submitted |
| Protocol reviewer  | TBD                 | —               |
| Operations reviewer| TBD                 | —               |
| Alpha user (LAN)   | TBD (1 v0.2 user)   | —               |
| Alpha user (WAN)   | TBD (1 NAT'd setup) | —               |

Sign-off requires author + protocol reviewer + at least one alpha
user with a real NAT'd setup before this RFC moves from Draft to
Review.

---

## Appendix A: Why not just use libp2p / IPFS / WebRTC / etc.?

| Alternative            | Why we don't adopt it as-is                                                         |
| ---------------------- | ----------------------------------------------------------------------------------- |
| **libp2p**             | Excellent NAT traversal but pulls in a massive dependency tree (DHT, gossipsub, kademlia) we don't need; loose API stability; not a comfortable Rust embed for a file-bus use case |
| **IPFS / Bitswap**     | Content-addressed, not directed transfer; built around a global DHT; very different mental model than "agent A → agent B"  |
| **WebRTC (libdatachannel)** | Adds SDP signaling, ICE, DTLS-SRTP layer cake; designed for real-time A/V; raw QUIC is a much smaller lift for our use case |
| **Tailscale / Nebula** | They are full L3 overlays; heavy infra; users would need to install + run them; we want one-binary AeroSync to Just Work    |
| **Syncthing's relay protocol** | Syncthing-specific; not a general protocol; would be a lift to extract                                              |
| **Hyperswarm (DAT)**   | Excellent design but Node.js-first; Rust port is incomplete; not a comfortable embed |

What we're really doing in this RFC is implementing **the ~10% of
libp2p/Tailscale that solves directed file-bus transfer**, with a
~500-line custom rendezvous instead of dragging in a 50k-line
overlay-network framework. This is a deliberate trade: less
generality, dramatically less complexity, identity model we fully
control.

## Appendix B: References

- RFC 8489 (STUN) — server-reflexive address discovery
- RFC 8445 (ICE)  — connectivity checks (we use a simplified subset)
- RFC 8656 (TURN) — relay model (we use a stripped-down equivalent)
- Tailscale's "How NAT traversal works" blog (2020) — practical guide
- WireGuard whitepaper — Ed25519 identity model inspiration
- quinn crate docs — for socket reuse + Endpoint construction

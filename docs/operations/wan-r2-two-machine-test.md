# Two-machine WAN R2 test guide

Use this to validate **R2** (bare `peer@rendezvous:port` → initiate → WebSocket →
`punch_at` → QUIC) between **two different hosts**. You need **three network
roles** (they can share physical machines if ports and routing allow it):

| Role | Typical host | Listens / reaches |
|------|----------------|-------------------|
| **Rendezvous** | Small VPS or either peer | TCP **8787** (HTTP API + WS upgrade) reachable from **both** peers |
| **Receiver (target)** | e.g. laptop B | TCP+UDP **7788** (or your port) reachable for **QUIC** from the public Internet where possible |
| **Sender (initiator)** | e.g. laptop A | Outbound-only is enough if rendezvous + target are public |

**Minimum viable layout:** rendezvous on a **third** machine with a public IP is
easiest. You can also run rendezvous on the **sender** or **receiver** if the
other side can open `http://<that-public-ip>:8787`.

**Related:** [field matrix](./wan-r2-field-matrix.md), [RUST_LOG](./wan-r2-release-ops.md),
[RFC-004 §6](../rfcs/RFC-004-wan-rendezvous.md).

---

## 1. Build and rendezvous key (once)

From the repo root:

```bash
cargo build --release -p aerosync -p aerosync-rendezvous
```

Generate an RSA key for JWT signing (PKCS#8 PEM), if you do not have one:

```bash
openssl genrsa -out rendezvous-rsa.pem 2048
openssl pkcs8 -topk8 -nocrypt -in rendezvous-rsa.pem -out rendezvous-pkcs8.pem
```

Start the control plane (replace bind address; **0.0.0.0** if remote peers must connect):

```bash
./target/release/aerosync-rendezvous \
  --bind 0.0.0.0:8787 \
  --database ./rendezvous.db \
  --jwt-rsa-private-key ./rendezvous-pkcs8.pem
```

Smoke test:

```bash
curl -sSf "http://<RENDEZVOUS_HOST>:8787/v1/health"
```

Production: put TLS and correct `X-Forwarded-For` in front — see [rendezvous.md](./rendezvous.md).

---

## 2. Ed25519 “device keys” (32 bytes, base64)

Registration requires **`public_key`**: standard **base64** encoding of **exactly 32 bytes**
(Ed25519 seed/public material as stored by the server — see `aerosync-rendezvous`).

On **each** peer, generate a **different** key and save the base64 string:

```bash
# Linux / macOS
python3 -c "import os,base64; print(base64.b64encode(os.urandom(32)).decode('ascii'))"
```

- **Receiver** uses its value as `AEROSYNC_RENDEZVOUS_PUBLIC_KEY`.
- **Sender** uses its value only in the **`register` JSON** for the initiator peer (below).

---

## 3. Receiver (target) — machine B

Pick:

- `PEER_NAME` = logical name, e.g. `bob` (must match what the sender uses in `peer@…`).
- `LISTEN_PORT` = e.g. `7788` (HTTP + QUIC listener).
- `RV_URL` = `http://RENDEZVOUS_HOST:8787` (no trailing slash).
- Optional but **recommended for WAN**: `OBSERVED` = `YOUR_PUBLIC_IP:LISTEN_PORT` or your DNS:port
  so rendezvous stores a **reachable** `observed_addr` (and HTTP fallback works).

**Auth:** use the same token on send and receive, e.g. `shared-secret`.

```bash
export AEROSYNC_RENDEZVOUS_URL='http://RENDEZVOUS_HOST:8787'
export AEROSYNC_RENDEZVOUS_NAME='bob'
export AEROSYNC_RENDEZVOUS_PUBLIC_KEY='<bob-32b-base64>'
# Optional multitenant: export AEROSYNC_RENDEZVOUS_NAMESPACE='acme'
export AEROSYNC_RENDEZVOUS_OBSERVED_ADDR='203.0.113.50:7788'   # your real public endpoint

./target/release/aerosync receive \
  --port 7788 \
  --save-to ./inbox \
  --auth-token 'shared-secret'
```

The receiver process will **register + heartbeat** and, when a session is initiated,
participate in **signaling WebSockets** (R2).

**Firewall:** allow **UDP + TCP** to `LISTEN_PORT` from the Internet (QUIC uses UDP;
HTTP probe may use TCP).

---

## 4. Sender (initiator) — machine A

The server requires the **initiator** to be a **registered peer** too (JWT `sub` ≠ target).
Register **alice** once (replace host and keys):

```bash
RV='http://RENDEZVOUS_HOST:8787'
ALICE_PK='<alice-32b-base64-different-from-bob>'

ALICE_JWT=$(curl -sSf -X POST "$RV/v1/peers/register" \
  -H 'Content-Type: application/json' \
  -d "{\"name\":\"alice\",\"public_key\":\"$ALICE_PK\",\"capabilities\":3}" \
  | python3 -c "import sys,json; print(json.load(sys.stdin)['jwt'])")

export AEROSYNC_RENDEZVOUS_TOKEN="$ALICE_JWT"
# Must match receiver if you use namespace:
# export AEROSYNC_RENDEZVOUS_NAMESPACE=''
```

Confirm **bob** is visible:

```bash
curl -sSf -H "Authorization: Bearer $AEROSYNC_RENDEZVOUS_TOKEN" \
  "$RV/v1/peers/bob"
```

Send using **bare** `peer@` (this selects the **R2** path — **no** `/path` after port):

```bash
export RUST_LOG='aerosync::wan::r2=info,aerosync=warn'
./target/release/aerosync send ./README.md 'bob@RENDEZVOUS_HOST:8787' \
  --token 'shared-secret'
```

You should see `r2: initiate_session ok` → signaling → `quic upload completed` in logs
on success; on failure, a message containing **`[R2_…]`**.

---

## 5. What counts as success

- File appears under receiver `--save-to`.
- Sender exits 0; optional: receipt / ack path if you enabled it.
- Logs: at least **`r2: quic upload completed on punched path`** on the sender when using `RUST_LOG` above.

Record outcome + error line in your [field matrix](./wan-r2-field-matrix.md) / [report template](./wan-r2-field-report-template.md).

---

## 6. Common failures

| Symptom | Things to check |
|---------|-------------------|
| `[R2_NO_TOKEN]` | `AEROSYNC_RENDEZVOUS_TOKEN` not set on sender. |
| `[R2_PEER_UNSEEN]` | Receiver not running or not heartbeating; `GET /v1/peers/bob` shows null `observed_addr`. |
| `[R2_INITIATE]` | JWT wrong/expired; initiator not registered; `target_namespace` mismatch; cannot POST to rendezvous. |
| `[R2_SIGNALING]` | WS blocked; receiver not joining WS; clock skew; corporate proxy stripping WS. |
| `[R2_CANDIDATE_EMPTY]` / timeouts | NAT too strict; wrong reflexive candidates; UDP blocked. |
| QUIC works on LAN but not WAN | Open **UDP** to receiver; symmetric NAT may block (no R3 auto-fallback). |

---

## 7. Python SDK (same idea)

- Receiver: configure `rendezvous_url`, `rendezvous_name`, `rendezvous_public_key`, etc. (see package `Config` / README).
- Sender: set env `AEROSYNC_RENDEZVOUS_TOKEN` after registering **alice**; destination string **`bob@host:port`** bare form.

---

# 双机 WAN R2 实测指引

**三个角色：** rendezvous 控制面（建议独立公网机或云主机）、**接收端**、**发送端**。两台电脑测试时，rendezvous 常放在第三台小机器上；若只有两台，可把 rendezvous 跑在其中一台，并保证另一台可以访问 `http://<公网或内网IP>:8787`。

**步骤摘要：**

1. 编译 `aerosync` / `aerosync-rendezvous`，准备 RSA 私钥，启动 rendezvous（`--bind 0.0.0.0:8787` 便于外网访问）。  
2. 两端各生成 **32 字节随机数的 base64**，作为 **不同** 的 `public_key`。  
3. **接收端**：设置 `AEROSYNC_RENDEZVOUS_URL`、`AEROSYNC_RENDEZVOUS_NAME`（如 `bob`）、`AEROSYNC_RENDEZVOUS_PUBLIC_KEY`，建议设置 `AEROSYNC_RENDEZVOUS_OBSERVED_ADDR=公网IP:端口`；`aerosync receive --port … --auth-token …`。  
4. **发送端**：用 `curl` 对 `/v1/peers/register` 再注册 **alice**，取返回的 `jwt` 设 `AEROSYNC_RENDEZVOUS_TOKEN`；`aerosync send 文件 'bob@rendezvous主机:8787' --token`（与接收端一致）。  
5. 目标格式必须是 **裸** `peer@host:port`（**不要** `/path`），才会走 R2。  
6. 调试：`RUST_LOG=aerosync::wan::r2=info`；失败看 **`[R2_*]`** 与上表。

更细的 TLS/反代见 [rendezvous.md](./rendezvous.md)；填表见 [实网矩阵](./wan-r2-field-matrix.md)。


# WAN R2 — release operations, rollback, and R3 (M8)

**Audience:** operators, release managers, and support.

## What this release includes

- **R2 (direct) path**: for destinations **`peer@rendezvous-host:port`** with
  *no* path suffix, the main library/CLI can run **rendezvous `initiate` →
  WebSocket signaling → `punch_at` → shared UDP/QUIC** to the remote
  candidate. Success is **best effort** over real NAT topologies; failures are
  **explicit** with stable **`[R2_*]`** error tags and CLI/SDK suggestions.
- **What this release does *not* include:** **no automatic R3 (byte) relay
  fallback** when R2 does not work. R3 remains a **later** milestone; users
  will see a failed transfer and must change topology, use another path, or
  wait for a future version that offers relay. Do **not** document R3 as
  “automatic” in user-facing comms for this line.

## User-visible failure mode (no R3)

If hole punching or QUIC negotiation does not complete, the error message
carries a tag such as **`[R2_SIGNALING]`**, **`[R2_CANDIDATE_EMPTY]`**,
**`[R2_TIMEOUT_PUNCH]`**, etc. (full list: [README.md](../../README.md#wan-troubleshooting-r2-tags)).  
**There is no silent switch** to a relay; operators should set expectations
accordingly in release notes and FAQs.

## Rollback and degradation (practical levers)

These are **operational** levers, not a second “hidden” protocol in the
binary.

1. **Use HTTP-only resolution (skip R2 punch for that transfer)**  
   If the user sends to **`peer@host/path/...`**, the implementation resolves the
   peer for **`http://<observed>/upload/...`** and does **not** run the R2
   WebSocket/QUIC punch path. Use this when a direct path is not required and
   `observed_addr` is reachable. (Documented in RFC-004 and README “path suffix”
   behavior.)

2. **Rendezvous control plane down / misconfigured**  
   Register and heartbeat to **`aerosync-rendezvous`** are required for R2. If
   the service is taken offline or keys rotate without updating clients, users
   will see **HTTP/JWT/lookup** errors (often under **`[R2_INITIATE]`** or
   pre-R2 401/404 depending on the path). See [rendezvous.md](./rendezvous.md)
   for TLS, `X-Forwarded-For`, and key handling.

3. **Embedders building a slimmer library**  
   R2 is gated by the **`wan-rendezvous`** (and for the data path, **`quic`**)
   Cargo features. A custom build with `--no-default-features` and a subset of
   features can omit R2; that is a **build-time** switch, not a runtime env flag
   in the current release.

4. **Communication rollback**  
   If you must “pull” WAN punch from marketing while shipping the same binary,
   clarify that the feature is **beta / best-effort** and point to this doc and
   the field matrix; do not promise relay.

## What to log on the server (optional, product-owned)

- Session initiate rate and failure HTTP status.  
- WebSocket attach failures on `/v1/sessions/.../ws` (separate from app-level
  `[R2_*]` on the client).

**Client-side** tags are the primary support signal for “why R2 did not
connect”.

## Related

- [Field test matrix and SLOs](./wan-r2-field-matrix.md)  
- [Rendezvous ops](./rendezvous.md)  
- [RFC-004](../rfcs/RFC-004-wan-rendezvous.md)  

---

# WAN R2：发布期运维、回退与 R3 说明（M8）

**读者：** 运维、发布与技术支持。

## 本版本能力边界

- **R2 直连路径**：对 **无路径后缀** 的 `peer@rendezvous:port`，主库/CLI 可走
  **initiate → WebSocket 信令 → `punch_at` → 共享 UDP/QUIC**；真实网络下为
  **尽力而为**，失败时带稳定 **`[R2_*]`** 标签与可操作建议。
- **本版本明确不包含：无自动 R3（字节流）中继兜底**。R3 仍为**后续**里程碑；R2
  不成则传输**失败**并提示用户调整网络或采用其它路径。对外宣发**勿**将 R3
  写为“自动回退/自动中继”。

## 无 R3 时用户能感知什么

打洞/QUIC 任一步未成功，错误中会出现
**`[R2_SIGNALING]`**、**`[R2_CANDIDATE_EMPTY]`**、**`[R2_TIMEOUT_PUNCH]`**
等（完整列表见 [README](../../README.md#wan-troubleshooting-r2-tags)）。**不会**在
后台默默切到中继；运维与发版材料需管理预期。

## 回退与降级（实操）

1. **HTTP 解析、不走 R2 打洞**  
   使用 **`peer@host/某路径/...`** 时，会解析对端并改写为
   **`http://<observed>/upload/...`**，**不进入** R2 信令/打洞。适用于不需要
   打洞、且 `observed_addr` 可达时。

2. **控制面不可用 / 配错**  
   R2 依赖 rendezvous 的注册/心跳。服务下线、JWT/密钥未同步等会在发起侧体现为
   **initiate/lookup/鉴权** 等错误（常见于 **`[R2_INITIATE]`** 等）。详见
   [rendezvous.md](./rendezvous.md)（TLS、**`X-Forwarded-For`**、私钥等）。

3. **嵌入方裁剪特性**  
   通过 **`wan-rendezvous` / `quic`** 等 Cargo 特性在**编译时**关断 R2；本版本
   不以单一环境变量作为全局“关闭 R2 打洞”开关（若今后增加，以 CHANGELOG
   为准）。

4. **宣传层面的“回退”**  
   若需弱化 WAN 打洞宣传而二进制不变，在发布说明中明确 **beta/尽力而为** 并
   指向本页与[实网矩阵](./wan-r2-field-matrix.md)；**勿**向用户承诺 R3
   自动恢复。

## 与 R3 的边界

- **当前发版线**：R2 失败＝**失败 + 带标签**；R3 自动中继＝**未提供**。  
- **后续发版**若提供 R3，会单独在 RFC-004/CHANGELOG/ README 中声明，并避免与
  本节的“无自动中继”表述冲突。

## 另见

- [实网矩阵与成功率（M7）](./wan-r2-field-matrix.md)  
- [Rendezvous 运维](./rendezvous.md)（中英对照同目录）  
- [RFC-004](../rfcs/RFC-004-wan-rendezvous.md)


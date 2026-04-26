# WAN R2 — SLO commitment scope (internal)

**Purpose:** Single page that defines what “we promise” before publishing **stable
release notes** with numeric WAN success rates. Product + engineering sign-off
this document (or a copy in your issue tracker) before external comms use the
same numbers as **RFC / README** claims.

## 1. What the SLO covers

- **Path:** **R2 only** — destination is **bare** `peer@rendezvous-host:port`
  (no `/path` suffix). Flow: `initiate` → WebSocket `candidates` / `punch_at` →
  UDP warmup → **QUIC** transfer on a shared `UdpSocket` (see
  [`RFC-004`](../rfcs/RFC-004-wan-rendezvous.md) §6).
- **Out of scope for the same numbers:** R3 byte relay, `peer@host/path/...`
  (HTTP rewrite only), senders that only perform lookup without R2, LAN-only
  `host:port` without rendezvous.

## 2. Success and failure (denominator)

- **Success (numerator):** [fill] e.g. transfer completes and receipt reaches
  **acked** (or your chosen end state — **must be one**).
- **Attempt (denominator):** [fill] e.g. both sides on supported versions, token
  valid, receiver registered, **intentional R2-tagged send** (not aborted before
  `initiate`).
- **Exclude from SLO (report separately):** [fill] e.g. wrong token, peer never
  registered, user cancelled — or decide **include all** and document bias.

## 3. Public targets (edit before release)

| Cohort | Target | Notes |
|--------|--------|--------|
| IPv4, typical home / same-region broadband | **≥ 80%** | Define “typical” in the field report. |
| IPv6 (global, where available) | **≥ 95%** | Single-stack vs dual-stack — specify in report. |

These are **field-validated** targets, not unit-test targets.

## 4. Artifacts before calling the release “stable with SLO”

1. A completed **[field report](wan-r2-field-report-template.md)** (or
   internal equivalent) with sample sizes and failure breakdown by `[R2_*]`.
2. **Release notes** paragraph that restates **scope** (§1) and **limits** (not
   all NAT types; no R3 auto-fallback) — see
   [wan-r2-release-ops.md](wan-r2-release-ops.md).
3. **Checklist** run-down: [wan-r2-stable-release-checklist](wan-r2-stable-release-checklist.md).

## Related

- [Field matrix (raw runs)](./wan-r2-field-matrix.md)
- [Field report template](./wan-r2-field-report-template.md)

---

# WAN R2 — SLO 承诺范围（内部）

**用途：** 在发布说明中写**正式 WAN 比例**前，用一页纸固定「我们承诺什么」；产品与工程在此对齐后再对外发数字。

## 1. 指标覆盖范围

- **路径：仅 R2** — 目标为**裸** `peer@rendezvous:端口`（**无** `/path` 后缀）。流程见
  [`RFC-004`](../rfcs/RFC-004-wan-rendezvous.md) §6。
- **不在同一组数字里：** R3、`peer@…/path`、仅 lookup 的 peer@、无 rendezvous 的纯 LAN。

## 2. 成功 / 失败与分母

- **成功（分子）：** [填写] 如传输完成且回执 **acked**（**只能选一种**终态判据）。
- **尝试（分母）：** [填写] 如双方版本在支持范围、鉴权有效、**有意走 R2** 的发送。
- **是否排除**配置错误等：在报告里**单列**或**计入**并写清偏差。

## 3. 对外目标值（发版前再填死）

| 分档 | 目标 | 说明 |
|------|------|------|
| 典型家宽/同城公网 **IPv4** | **≥ 80%** | 「典型」在实网报告里定义。 |
| **IPv6** 可达 | **≥ 95%** | 单栈/双栈在报告里说明。 |

## 4. 对外称「稳定版且带比例」前必备材料

1. 填好的 **[实网报告](wan-r2-field-report-template.md)**（或内部等价物），含样本量与 `[R2_*]` 分布。  
2. **Release notes** 中重申 **§1 范围**与**限制**（无自动 R3 等）— 见
   [wan-r2-release-ops.md](wan-r2-release-ops.md)。  
3. 过一遍 **[发版检查清单](wan-r2-stable-release-checklist.md)**。

## 另见

- [实网矩阵（原始表）](./wan-r2-field-matrix.md)


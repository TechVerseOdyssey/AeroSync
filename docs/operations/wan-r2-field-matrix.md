# WAN R2 field test matrix and success rates (M7)

**Stable release with published percentages:** also fill
[`wan-r2-slo-goals.md`](./wan-r2-slo-goals.md) and
[`wan-r2-field-report-template.md`](./wan-r2-field-report-template.md), then
[`wan-r2-stable-release-checklist.md`](./wan-r2-stable-release-checklist.md).

This runbook is for **product / QA** to validate the release promise for direct
R2 (rendezvous + signaling + UDP/QUIC punch): explicit `[R2_*]` errors on
failure, **no automatic R3 byte relay** in the current line (see
[`wan-r2-release-ops.md`](./wan-r2-release-ops.md)).

## What to measure

- **Success**: bare destination `peer@rendezvous:port` completes the transfer over
  the R2/QUIC path (or the documented success observable on the wire you choose,
  e.g. receipt `acked`) **without** falling back to HTTP.
- **Failure**: transfer errors; record the full message and the **`[R2_*]`** tag
  (see [README.md](../../README.md#wan-troubleshooting-r2-tags)).

## Target (same-environment cohorts, not a guarantee)

| Cohort        | SLO (informal)   | Notes |
|--------------|------------------|--------|
| IPv4, typical home / “same city” broadband | **≥ 80%** success | CGNAT, symmetric NAT, and carrier issues dominate failures. |
| **IPv6** (global, where available)         | **≥ 95%** success | Punches are often easier; still record failures. |

Treat these as **field validation** targets. Engineering owns the
implementation; product owns the measurement window and sample size (e.g. 50+
attempts per cohort before drawing conclusions).

## Scenario matrix (fill in)

Use one row per attempt. Store results in a spreadsheet or CSV; keep raw error
lines for post-mortems.

| # | Date (UTC) | Role A (sender) | Role B (receiver) | A network | B network | IPv4/IPv6 | Rendezvous version / URL | File size | Outcome (ok / fail) | `[R2_*]` or other error | Notes |
|---|------------|-----------------|-------------------|-----------|-----------|-----------|----------------------------|----------|----------------------|-------------------------|--------|
| 1 | | e.g. macOS CLI | e.g. Linux CLI | home FTTH | cloud VM | both v4 + v6 | | 1 MiB | | | |

**Suggested “network” buckets** (pick what matches your user base): home FTTH
behind one NAT, **CGNAT** (carrier grade), **symmetric** NAT, mobile hotspot, two
independent home links, one side **data center / cloud** with public UDP, one
**IPv6-only** path, and **IPv4-only** both sides.

**CLI sketch** (adapt ports and tokens to your environment):

- Receiver: `aerosync receive …` with rendezvous register/heartbeat as in README.
- Sender: `aerosync send <file> peer@<rendezvous-host>:<port>` with
  `AEROSYNC_RENDEZVOUS_TOKEN` set (and `AEROSYNC_RENDEZVOUS_NAMESPACE` if
  multitenant).

**Python**: mirror the same with `aerosync.client()` and `aerosync.receiver()`
using rendezvous config fields from the SDK.

## After a test day

1. **Counts** by outcome and by top-level `[R2_*]` code.
2. **Gap list**: scenarios under SLO — capture NAT hints (if you have them),
  rendezvous logs, and whether failure was pre-punch (signaling) or post-punch
  (QUIC).
3. Open issues with **repro** + error text (tags are enough for triage without
   packet captures).

## Related

- [WAN R2 release / rollback / no R3](./wan-r2-release-ops.md)
- [Rendezvous ops (TLS, proxy, `X-Forwarded-For`)](./rendezvous.md)
- [RFC-004 — WAN / rendezvous](../rfcs/RFC-004-wan-rendezvous.md)

---

# 实网 R2 验证矩阵与成功率（M7）

**若发布稳定版并对外写比例：** 同时填写 [SLO 目标页](./wan-r2-slo-goals.md)、
[实网报告模板](./wan-r2-field-report-template.md)，并过
[稳定版发版清单](./wan-r2-stable-release-checklist.md)。

供 **产品/QA** 在对外承诺「WAN 直连 + 失败带 `[R2_*]` 标签、**不自动 R3 中继**」
前做**统计**使用；目标为同环境下的大样本 SLO 参考，而非单次测试。

## 要统计什么

- **成功**：无路径后缀的 `peer@rendezvous:port` 在 R2/QUIC 路径上完成（或
  你们约定的可观测成功标准，如回执 `acked`）。
- **失败**：记录完整错误文案及 **`[R2_*]`** 标签；对照 [README](../../README.md)。

## 目标值（分群体，非绝对保证）

| 场景 | 非正式 SLO | 说明 |
|------|-------------|------|
| 典型家庭/同城 IPv4 公网/宽带 | **≥ 80%** | CGNAT、对称 NAT 等会拉高失败。 |
| **IPv6** 可达 | **≥ 95%** | 打洞常更容易；仍须记录失败。 |

样本量建议：每个群体 **50 次以上** 再下结论；工程负责实现，产品/QA
负责样本与窗口。

## 场景矩阵

见上表（可复制到电子表格/CSV），按你们用户画像增加「网络侧」行（家宽、
热点、双 NAT、单云主机公网、纯 IPv4/双栈等）。

## 测试日结束后

1. 按 **结果** 与 **[R2_*] 首段标签** 聚合计数。
2. 未达 SLO 的群体列为 **差距清单**（能拿到 NAT/日志则附上）。
3. 提单时附带**复现**与错误原文（有标签即可，未必需要抓包）。

## 另见

- [发布运维 / 回滚 / 无 R3](./wan-r2-release-ops.md)（本目录中英文对照同文件）


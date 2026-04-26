# WAN R2 — field validation report (template)

**Fill in** for the stable release that advertises SLOs. You may copy this to an
internal doc system; keep a **summary** in GitHub Release notes (avoid leaking
sensitive hostnames in public if needed).

**References:** [SLO goals](wan-r2-slo-goals.md), [raw matrix](wan-r2-field-matrix.md).

---

## Metadata

| Field | Value |
|--------|--------|
| Report id | e.g. R2-2026-Q2-01 |
| AeroSync / aerosync version | e.g. git tag, crate version, PyPI version |
| aerosync-rendezvous version / image digest | |
| Date range (UTC) | from — to |
| Authors | |
| Sign-off (product) | name / date |
| Sign-off (engineering) | name / date |

## Executive summary

- **Cohort A (IPv4 / …):** **X / N = P%** — **meets / does not meet** target 80%.
- **Cohort B (IPv6 / …):** **X / N = P%** — **meets / does not meet** target 95%.
- **Top 3 failure tags:** `…`, `…`, `…`
- **Conclusion:** [Approved for public SLO text / not approved / approved with modified wording]

## Scope reminder

- R2 path only: bare `peer@rendezvous:port` (this report’s attempts must be
  identifiable as R2, not path-suffix or HTTP-only rewrite).

## Method

- **Client builds:** (Rust CLI / Python wheel / both)
- **Rendezvous deployment:** (region, TLS, proxy notes)
- **Network buckets tested:** (list: home FTTH, CGNAT, mobile hotspot, cloud VM, …)
- **Success definition used:** (e.g. receipt acked)
- **Exclusion rules:** (what was not counted in SLO denominator, if any)

## Results by cohort

### Cohort A — [e.g. IPv4 dual-stack home, same city]

| Metric | Value |
|--------|--------|
| Valid attempts (N) | |
| Successes | |
| Rate | % |
| vs target 80% | pass / fail |

**Failure breakdown ([R2_*] or other):**

| Tag / class | Count | % of failures |
|-------------|-------|----------------|
| | | |

### Cohort B — [e.g. global IPv6]

| Metric | Value |
|--------|--------|
| Valid attempts (N) | |
| Successes | |
| Rate | % |
| vs target 95% | pass / fail |

**Failure breakdown:**

| Tag / class | Count | % of failures |
|-------------|-------|----------------|
| | | |

## Raw data location

- Spreadsheet / CSV path (if internal, note “on request” for auditors).

## Appendix: example rows (optional paste)

(Short anonymized log lines or error strings if useful.)

---

# WAN R2 — 实网验证报告（模板）

**填写**后用于要对外写**比例承诺**的稳定版。可复制到内部系统；**公开** Release
可只发摘要，避免敏感主机名。

**参照：** [SLO 目标页](wan-r2-slo-goals.md)、[实网矩阵](wan-r2-field-matrix.md)。

---

## 元信息

| 项 | 值 |
|----|-----|
| 报告编号 | 如 R2-2026-Q2-01 |
| AeroSync / 客户端版本 | tag、crate、PyPI |
| rendezvous 版本/镜像 | |
| 统计窗口 (UTC) | 起止 |
| 撰写人 / 产品 sign-off / 工程 sign-off | |

## 摘要

- **分档 A：** X/N=P% — **达/未达** 80%。
- **分档 B：** X/N=P% — **达/未达** 95%。
- **前三失败标签：** …
- **结论：** [同意对外用该表述 / 需改数字或收窄范围]

## 范围说明

- 仅 **R2 裸 peer@** 计入本报告承诺分母（须可区分，非 `…/path` 纯 HTTP 改写）。

## 方法

- 构建形态、控制面部署、网络分档、成功判据、分母排除规则（同上表结构，中文填写即可）。

## 分档结果

（同英文结构：各档 N、成功数、比例、与目标比较、`[R2_*]` 分布表。）

## 原始数据

- 表格/仓库路径（内部可写「备审计索取」）。


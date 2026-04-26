# Stable release + public WAN SLO — checklist

Use this when cutting a version that will **state numeric WAN success rates** in
release notes or marketing. For a normal release without SLO numbers, a subset
suffices (mark **SLO** steps N/A).

## Pre-build

- [ ] `cargo test --workspace --all-targets` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo fmt --all -- --check` passes
- [ ] `pytest` (aerosync-py) passes for the wheel you will ship
- [ ] Run `./scripts/ci-r2-wan-readiness.sh` (R2 feature slice)
- [ ] `cargo deny --all-features check` (or your standard policy) passes

## SLO / product (required if publishing percentages)

- [ ] [SLO goals](wan-r2-slo-goals.md) filled: success rule, denominator, cohorts
- [ ] [Field report](wan-r2-field-report-template.md) complete with N large enough
  per cohort and **pass** vs stated targets (or release notes **narrow** the claim)
- [ ] Legal / support review of **exact** public text (if applicable)

## Versioning and docs

- [ ] `CHANGELOG.md`: move [Unreleased] to **vX.Y.Z** with R2 + SLO summary
- [ ] `README.md` / `README.zh-CN.md`: version badge and WAN paragraph match release
- [ ] [RFC-004 implementation status](../rfcs/RFC-004-wan-rendezvous.md) still accurate
- [ ] [ARCHITECTURE_AND_DESIGN.md](../ARCHITECTURE_AND_DESIGN.md) §12.1 R2 row matches reality

## Publish

- [ ] Git tag `vX.Y.Z` on the released commit
- [ ] `cargo publish` for published crates in dependency order
- [ ] PyPI: wheels + version match tag
- [ ] GitHub Release: notes link field report **summary** and [wan-r2-release-ops](wan-r2-release-ops.md)

## Post-release

- [ ] Support channel: FAQ pointer to `[R2_*]` tags
- [ ] If metrics/logs used in field tests, same filters documented for customers

---

# 稳定版 + 对外 WAN 比例 — 检查清单

在 **Release 说明或对外宣传中写清数字型 WAN 成功率** 时使用。若本版**不写**比例，可将标 **SLO** 的条目标为 N/A。

## 构建前

- [ ] 全量 `cargo test` / clippy / fmt 通过
- [ ] `pytest`、`./scripts/ci-r2-wan-readiness.sh` 通过
- [ ] `cargo deny`（或项目惯例）通过

## SLO / 产品（写比例时必做）

- [ ] [SLO 目标](wan-r2-slo-goals.md) 已填：成功判据、分母、分档
- [ ] [实网报告](wan-r2-field-report-template.md) 已填，样本量足够，**达标**或**已收窄**对外表述
- [ ] 对外文案经法务/支持过目（如需要）

## 版本与文档

- [ ] `CHANGELOG` 为 **vX.Y.Z** 落档，含 R2 与 SLO 摘要
- [ ] 双语 README 版本与 WAN 段一致
- [ ] RFC-004 实现状态、ARCH §12.1 与 R2 现状一致

## 发布动作

- [ ] 打 tag、crates 发布、PyPI wheel 与 tag 一致
- [ ] GitHub Release 附报告**摘要**与 [运维边界](wan-r2-release-ops.md) 链接

## 上线后

- [ ] 支持侧 FAQ 指向 `[R2_*]`
- [ ] 实网用过筛日志的，对用户提供相同取数说明（若适用）


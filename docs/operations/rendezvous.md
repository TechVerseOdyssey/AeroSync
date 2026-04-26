# Operating `aerosync-rendezvous` (RFC-004 control plane)

This document complements [`aerosync-rendezvous/README.md`](../../aerosync-rendezvous/README.md) (build/run flags). It focuses on **TLS**, **reverse proxies**, and **address visibility** for production.

## TLS

The process listens for **plain HTTP** on `--bind` (default `127.0.0.1:8787`). For the public Internet, terminate TLS at a **reverse proxy** (nginx, Caddy, Traefik, cloud LB) and forward to the loopback or private listener.

- **Do not** expose the RSA private key used for JWT signing except on the rendezvous host with strict file permissions (`chmod 600`).

## Reverse proxy and `X-Forwarded-For`

Registration uses the client address from, in order:

1. `observed_addr` in the JSON body (if present),
2. the first IP in `X-Forwarded-For` (if the header is set),
3. the direct TCP remote address (`ConnectInfo`).

**Important:** If you put the service behind a proxy, ensure only your proxy can reach the backend, or that you **strip or validate** `X-Forwarded-For` at the edge. Otherwise a client could spoof a reflexive address. For a first deployment, run the server where the edge sets `X-Forwarded-For` from the real client connection.

## What clients see as `observed_addr`

Downstream AeroSync senders use the stored `observed_addr` to build `http://{observed}/upload/...`. Peers behind NAT should send a reachable **`host:port`** in `observed_addr` at register/heartbeat when possible; otherwise the server may only record a public IP or proxy-visible address, which may not be enough for a direct path.

## Related

- [RFC-004 implementation status](../rfcs/RFC-004-wan-rendezvous.md#implementation-status-2026-04-24)
- **R2 / WAN release operations** (no R3 auto-relay, rollback levers, field SLOs): [wan-r2-release-ops.md](./wan-r2-release-ops.md), [wan-r2-field-matrix.md](./wan-r2-field-matrix.md), [SLO goals](./wan-r2-slo-goals.md), [field report template](./wan-r2-field-report-template.md), [stable release checklist](./wan-r2-stable-release-checklist.md), [two-machine R2 test](./wan-r2-two-machine-test.md)
- Main CLI / env: `AEROSYNC_RENDEZVOUS_TOKEN`, optional **`AEROSYNC_RENDEZVOUS_NAMESPACE`** (P2 multitenant; must match the JWT `ns` claim), destination `name@host:port` — [README.md](../../README.md) and [README.zh-CN.md](../../README.zh-CN.md)

---

# 运维：aerosync-rendezvous（RFC-004 控制面）

本文与仓库内 [`aerosync-rendezvous/README.md`](../../aerosync-rendezvous/README.md)（编译与启动参数）配合阅读，说明 **TLS**、**反代** 与 **地址可见性** 等生产注意事项。

## TLS

进程默认在 `--bind` 上提供 **HTTP**（如 `127.0.0.1:8787`）。对公网提供 HTTPS 时，应在 **反向代理/负载均衡** 上终止 TLS，再回源到本机或内网 HTTP 端口。

- 用于签发 JWT 的 **RSA 私钥** 仅应保存在 rendezvous 主机，权限尽量收紧（如 `chmod 600`），勿打入镜像或日志。

## 反代与 `X-Forwarded-For`

注册时 `observed_addr` 的优先级为：

1. 请求体里的 `observed_addr`（若提供）；
2. 请求头 `X-Forwarded-For` 的**第一个** IP（若存在）；
3. TCP 连接上的远端地址（`ConnectInfo`）。

**注意：** 经反代部署时，应仅允许可信反代访问后端，或在边缘**正确设置/校验** `X-Forwarded-For`；否则客户端可能伪造该头影响记录的地址。简单做法：仅让反代到后端，由反代按真实连接写入 `X-Forwarded-For`。

## 与发送端的关系

AeroSync 主程序会把 rendezvous 返回的 `observed_addr` 拼成 `http://…/upload` 再传文件。若对端在 NAT 后，应在 **register/heartbeat** 中尽量提供对发送端**可路由**的 `host:port`；否则可能只记到公网 IP 或代理侧可见地址，无法保证打洞/直连一定成功（完整 NAT/中继见 RFC-004 后续里程碑）。

## 另见

- [RFC-004 实现状态](../rfcs/RFC-004-wan-rendezvous.md#implementation-status-2026-04-24)
- **R2 / WAN 发布运维**（无 R3 自动中继、实网 SLO、报告模板、发版清单、**双机实测**）：[wan-r2-release-ops.md](./wan-r2-release-ops.md)、[实网矩阵](./wan-r2-field-matrix.md)、[SLO 目标](./wan-r2-slo-goals.md)、[实网报告模板](./wan-r2-field-report-template.md)、[稳定版检查清单](./wan-r2-stable-release-checklist.md)、[双机 R2 验证](./wan-r2-two-machine-test.md)
- 主项目 README [英文](../../README.md) / [中文](../../README.zh-CN.md) 中的 `AEROSYNC_RENDEZVOUS_TOKEN`、可选 **`AEROSYNC_RENDEZVOUS_NAMESPACE`**（P2 多租户，须与 JWT `ns` 一致）与 `peer@host:port` 说明

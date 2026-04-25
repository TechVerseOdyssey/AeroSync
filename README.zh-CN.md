# AeroSync

高性能跨网络文件传输 CLI 工具，专为 Agent 间文件传输场景设计。

支持大文件（GB 级）断点续传和小文件批量高并发传输，两端均安装 AeroSync 时自动协商升级 QUIC 协议，否则降级为 HTTP / S3 / FTP 等标准协议。

> 完整英文说明见 [README.md](README.md)。

## 特性

- **自适应协议**：自动探测对端是否支持 QUIC，支持则升级，否则降级 HTTP
- **断点续传**：32MB 分片上传，状态持久化到本地 JSON，重启后自动恢复
- **批量并发**：小文件（<1MB）16 并发，中等文件（<64MB）8 并发，大文件走分片
- **多协议支持**：HTTP、QUIC、S3（含 MinIO）、FTP
- **目录传输**：`--recursive` 保留完整子目录结构
- **完整性校验**：SHA-256 端到端验证
- **认证**：HMAC-SHA256 Bearer Token
- **配置文件**：TOML 格式，CLI 参数优先覆盖
- **RFC-004 / WAN（分阶段落地）** — 独立工作区成员 **`aerosync-rendezvous`**（自托管控制面：SQLite 注册、JWT、`/v1/peers/*` API，见 [`aerosync-rendezvous/README.md`](aerosync-rendezvous/README.md)）。主程序支持在设置环境变量 **`AEROSYNC_RENDEZVOUS_TOKEN`** 后，将目标写为 **`peer@rendezvous主机:端口`** 以经 rendezvous 查询对端 `observed_addr` 再传输。运维说明（**中英文**）：[`docs/operations/rendezvous.md`](docs/operations/rendezvous.md)。**NAT 打洞、信令、可用中继** 等仍按 [RFC-004](docs/rfcs/RFC-004-wan-rendezvous.md) 规划在 v0.4+，详见 RFC **Implementation status** 与根目录 `CHANGELOG` [Unreleased]。

## 安装

```bash
git clone https://github.com/TechVerseOdyssey/AeroSync.git
cd AeroSync
cargo build --release
# 二进制：target/release/aerosync、aerosync-mcp、aerosync-rendezvous
```

## 快速开始

**接收端**（目标机器）：
```bash
aerosync receive --port 7788 --save-to ./downloads
```

**发送端**（源机器）：
```bash
# 单文件（自动协商协议）
aerosync send ./video.mp4 192.168.1.10:7788

# 发送目录（保留结构）
aerosync send ./project/ 192.168.1.10:7788 --recursive

# 强制 HTTP
aerosync send ./file.zip http://192.168.1.10:7788/upload

# 上传到 S3
aerosync send ./data.tar.gz s3://my-bucket/backups/data.tar.gz

# 上传到 FTP
aerosync send ./report.pdf ftp://ftpserver:21/uploads/report.pdf
```

**RFC-004 rendezvous（可选，week 1）** — 需已部署的 `aerosync-rendezvous`、可 lookup 的 JWT，以及：

```bash
export AEROSYNC_RENDEZVOUS_TOKEN='eyJ...'
# 可选（P2 多租户）：若注册时用了 X-AeroSync-Namespace，须与 JWT 的 ns 一致
# export AEROSYNC_RENDEZVOUS_NAMESPACE='acme'
aerosync send ./video.mp4 'alice@rendezvous.example.com:8787'
```

## CLI 参考

### `aerosync send`

```
aerosync send <SOURCE> <DESTINATION> [OPTIONS]
```

| 参数 | 说明 | 默认值 |
|------|------|--------|
| `<SOURCE>` | 源文件或目录路径 | — |
| `<DESTINATION>` | 目标地址：`host:port`、`http://`、`quic://`、`s3://`、`ftp://`，或 `name@host:port`（RFC-004，需 `AEROSYNC_RENDEZVOUS_TOKEN`） | — |
| `-r, --recursive` | 递归发送目录 | false |
| `--protocol` | 强制协议：`quic` \| `http` | 自动协商 |
| `--token` | 认证 Token | — |
| `--parallel` | 并发流数量 | 4 |
| `--no-verify` | 跳过 SHA-256 校验 | false |
| `--dry-run` | 仅显示传输计划 | false |
| `--no-resume` | 禁用断点续传 | false |

### `aerosync receive`

```
aerosync receive [OPTIONS]
```

| 参数 | 说明 | 默认值 |
|------|------|--------|
| `--port` | HTTP 监听端口 | 7788 |
| `--quic-port` | QUIC 监听端口 | 7789 |
| `--save-to` | 文件保存目录 | ./received |
| `--bind` | 绑定地址 | 0.0.0.0 |
| `--auth-token` | 要求发送方携带此 Token | — |
| `--one-shot` | 接收一个文件后退出 | false |
| `--overwrite` | 允许覆盖同名文件 | false |
| `--max-size` | 最大文件大小（字节） | 100GB |
| `--http-only` | 仅启用 HTTP，禁用 QUIC | false |

### `aerosync token`

```bash
# 生成 Token（有效期 24 小时）
aerosync token generate --hours 24

# 使用自定义密钥
aerosync token generate --secret my-secret-key

# 验证 Token
aerosync token verify <TOKEN> --secret my-secret-key
```

### `aerosync resume`

```bash
# 列出未完成的传输任务
aerosync resume list

# 清除指定任务的续传状态
aerosync resume clear <TASK_ID>

# 清除所有续传状态
aerosync resume clear-all
```

### `aerosync status`

```bash
# 查看接收端状态
aerosync status 192.168.1.10:7788
```

## 配置文件

默认路径：`~/.aerosync/config.toml`（通过 `--config` 指定其他路径）

```toml
[transfer]
max_concurrent = 4       # 最大并发任务数
chunk_size_mb = 32       # 分片大小（MB，用于断点续传）
retry_attempts = 3       # 单分片最大重试次数
timeout_seconds = 60     # 请求超时（秒）

[auth]
token = ""               # 默认认证 Token

[server]
http_port = 7788         # HTTP 监听端口
quic_port = 7789         # QUIC 监听端口
save_to = "./received"   # 文件保存目录
bind = "0.0.0.0"         # 绑定地址
```

CLI 参数优先级高于配置文件。

## 协议说明

### QUIC 自动协商

当目标为 `host:port` 格式时，AeroSync 会先向 `http://host:port/health` 发送探测请求（超时 2s）。若响应头含 `X-AeroSync: true`，自动升级为 QUIC（端口 = HTTP 端口 + 1）；否则降级为 HTTP。

```
host:7788  →  探测  →  有 AeroSync  →  quic://host:7789
                    →  无 AeroSync  →  http://host:7788/upload
```

### S3 兼容存储

支持 AWS S3 和 MinIO 等 S3 兼容存储：

```bash
# AWS S3（使用环境变量或配置）
aerosync send ./file.tar.gz s3://bucket/prefix/file.tar.gz

# MinIO（通过代码配置 endpoint）
# s3_config.endpoint = Some("http://minio:9000".to_string())
```

### FTP

支持标准 FTP（被动模式）：

```bash
aerosync send ./file.csv ftp://ftpserver:21/data/file.csv
```

### RFC-004 rendezvous 查询（可选）

若设置环境变量 **`AEROSYNC_RENDEZVOUS_TOKEN`**，可将目标写为 **`peer@rendezvous主机:端口`**（**不要**加 `http://` 前缀）。客户端会向 rendezvous 发起 `GET /v1/peers/{name}`，再用返回的地址走原有 HTTP/QUIC 发送流程。控制面需单独运行：`cargo run -p aerosync-rendezvous -- --jwt-rsa-private-key ./key.pem`（详见 [`aerosync-rendezvous/README.md`](aerosync-rendezvous/README.md) 与 [RFC-004](docs/rfcs/RFC-004-wan-rendezvous.md)）。

### WAN 故障排查（R2 标签）

当 `peer@rendezvous-host:port` 走 R2 打洞路径时，错误会带稳定标签：

- **`[R2_NO_TOKEN]`**：发送端没有 lookup token。设置 `AEROSYNC_RENDEZVOUS_TOKEN`（多租户还需 `AEROSYNC_RENDEZVOUS_NAMESPACE`）。
- **`[R2_PEER_UNSEEN]`**：目标 peer 还没有 `observed_addr`。先启动接收端并完成 heartbeat/register，再重试。
- **`[R2_INITIATE]`**：`POST /v1/sessions/initiate` 失败（常见是 JWT/namespace/session 权限不匹配）。
- **`[R2_SIGNALING]`**：在 `punch_at` 前信令 WebSocket 失败（对端未参与、WS 被拦截、时序问题）。
- **`[R2_CANDIDATE_EMPTY]`**：信令没有返回可用的远端 socket 地址。
- **`[R2_WARMUP]` / `[R2_SOCKET]`**：本地 UDP 暖包/套接字初始化失败。

当前范围/限制（v0.3.0-rc）：
- 只有目标是裸格式 `peer@rendezvous-host:port` 时才会尝试 R2 信令+打洞。
  若带路径后缀（例如 `peer@host:port/path/file.bin`），客户端会走 lookup +
  目的地址改写（HTTP `/upload/...`），不会进入 R2 信令流程。
- 目前还没有自动 R3 中继回退；R2 失败会直接以带上述标签的传输错误返回。

快速排查：

```bash
# 发送端是否已设置 token
echo "${AEROSYNC_RENDEZVOUS_TOKEN:+set}"

# rendezvous 健康检查
curl -sSf http://<rendezvous-host>:<port>/v1/health

# 查看目标 peer 是否在线（替换 token + peer）
curl -sSf -H "Authorization: Bearer $AEROSYNC_RENDEZVOUS_TOKEN" \
  "http://<rendezvous-host>:<port>/v1/peers/<peer_name>"
```

Python SDK 对应异常映射：
- `r2_no_token` -> `ConfigError`
- `r2_peer_unseen` -> `PeerNotFoundError`
- `r2_negotiation` -> `ConnectionError`

## 断点续传

大文件（默认 >64MB）自动启用分片上传，每片 32MB。状态保存在 `.aerosync/<task_id>.json`。

```bash
# 发送中断后，重新运行相同命令自动恢复
aerosync send ./large_file.bin 192.168.1.10:7788

# 查看未完成任务
aerosync resume list

# 清除续传状态（强制重传）
aerosync send ./large_file.bin 192.168.1.10:7788 --no-resume
```

## 架构

与英文 [`README.md` 架构一节](README.md#architecture) 一致，当前工作区为 **7 个 crate**（含 **`aerosync-rendezvous`** 控制面、**`aerosync-domain` / `aerosync-infra`** 等）。精要图：

```
aerosync (CLI)              aerosync-mcp
       └──────────┬────────────┘
                  ▼
          aerosync（根：引擎 + 协议 + Receipt）
          ├── TransferEngine, FileReceiver, AutoAdapter, …
          ├── HttpTransfer, QuicTransfer, S3Transfer, FtpTransfer
          └── wan::rendezvous（feature wan-rendezvous，RFC-004 客户端解析）
                  │
     ┌────────────┴────────────┐
     ▼                         ▼
aerosync-domain          aerosync-infra
aerosync-proto            （wire protobuf）

aerosync-rendezvous        独立二进制：HTTP 注册/查询 + SQLite（不链入主 `aerosync`）
```

更完整的模块表见 [`docs/ARCHITECTURE_AND_DESIGN.md`](docs/ARCHITECTURE_AND_DESIGN.md)。**v0.3.0** 起 `aerosync-domain` / `aerosync-infra` 为**内部** crate，请通过 `aerosync::core::*` 引用。

### 并发策略

| 文件大小 | 策略 | 并发数 |
|---------|------|--------|
| < 1MB | 小文件高并发 | 16 |
| 1MB – 64MB | 中等文件并发 | 8 |
| > 64MB | 分片上传 + 断点续传 | 1（分片内并发） |

## 开发

```bash
cargo test --workspace
cargo test -p aerosync --lib
cargo test -p aerosync-rendezvous
cargo test -p aerosync --test protocols_pipeline
cargo build --release
```

## 许可证

MIT

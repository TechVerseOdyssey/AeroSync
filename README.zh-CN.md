# AeroSync

高性能跨网络文件传输 CLI 工具，专为 Agent 间文件传输场景设计。

支持大文件（GB 级）断点续传和小文件批量高并发传输，两端均安装 AeroSync 时自动协商升级 QUIC 协议，否则降级为 HTTP / S3 / FTP 等标准协议。

## 特性

- **自适应协议**：自动探测对端是否支持 QUIC，支持则升级，否则降级 HTTP
- **断点续传**：32MB 分片上传，状态持久化到本地 JSON，重启后自动恢复
- **批量并发**：小文件（<1MB）16 并发，中等文件（<64MB）8 并发，大文件走分片
- **多协议支持**：HTTP、QUIC、S3（含 MinIO）、FTP
- **目录传输**：`--recursive` 保留完整子目录结构
- **完整性校验**：SHA-256 端到端验证
- **认证**：HMAC-SHA256 Bearer Token
- **配置文件**：TOML 格式，CLI 参数优先覆盖

## 安装

```bash
git clone https://github.com/TechVerseOdyssey/AeroSync.git
cd AeroSync
cargo build --release
# 二进制位于 target/release/aerosync
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

## CLI 参考

### `aerosync send`

```
aerosync send <SOURCE> <DESTINATION> [OPTIONS]
```

| 参数 | 说明 | 默认值 |
|------|------|--------|
| `<SOURCE>` | 源文件或目录路径 | — |
| `<DESTINATION>` | 目标地址，支持 `host:port`、`http://`、`quic://`、`s3://`、`ftp://` | — |
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

```
aerosync (CLI)
├── aerosync-core
│   ├── TransferEngine      并发 Worker（FuturesUnordered + Semaphore）
│   ├── ProgressMonitor     进度跟踪（MultiProgress）
│   ├── ResumeStore         断点续传状态持久化
│   ├── FileReceiver        HTTP/QUIC 接收端
│   └── AuthManager         HMAC-SHA256 Token 认证
└── aerosync-protocols
    ├── AutoAdapter         协议路由（自动协商）
    ├── HttpTransfer        HTTP 上传/下载（Arc<Client> 复用）
    ├── QuicTransfer        QUIC 传输（quinn + rustls）
    ├── S3Transfer          S3 上传/下载（AWS SigV4）
    └── FtpTransfer         FTP 上传/下载（suppaftp async）
```

### 并发策略

| 文件大小 | 策略 | 并发数 |
|---------|------|--------|
| < 1MB | 小文件高并发 | 16 |
| 1MB – 64MB | 中等文件并发 | 8 |
| > 64MB | 分片上传 + 断点续传 | 1（分片内并发） |

## 开发

```bash
# 运行全量测试（149 个用例）
cargo test --workspace

# 仅测试核心层
cargo test -p aerosync-core

# 仅测试协议层
cargo test -p aerosync-protocols

# 运行流水线集成测试
cargo test -p aerosync-protocols --test pipeline

# 构建发布版本
cargo build --release
```

## 许可证

MIT

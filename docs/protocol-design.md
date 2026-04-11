# AeroSync 协议设计与功能规划

本文档覆盖协议实现细节，以及当前实现状态与后续开发规划。

---

## 目录

1. [功能实现现状](#1-功能实现现状)
2. [开发路线图](#2-开发路线图)
3. [P0 待实现详细设计](#3-p0-待实现详细设计)
4. [P1 待实现详细设计](#4-p1-待实现详细设计)
5. [P2 参考设计](#5-p2-参考设计)
6. [协议交互时序](#6-协议交互时序)

---

## 1. 功能实现现状

| # | 功能点 | 优先级 | 状态 | 说明 |
|---|--------|--------|------|------|
| 1 | SHA-256 文件完整性校验 | P0 | ✅ 已实现 | 发送方预计算、接收方校验，HTTP 单文件 + 分片均覆盖 |
| 2 | 审计日志 | P0 | ⚠️ 部分实现 | 内存记录（含 IP、时间、文件名），**缺磁盘持久化** |
| 3 | TLS 证书管理 | P0 | ✅ 已实现 | `rcgen` 自动生成自签名证书用于 QUIC，**缺外部证书加载** |
| 4 | Auth 与传输流程集成 | P0 | ✅ 已实现 | `/upload`、`/upload/chunk`、`/upload/complete`、QUIC 均覆盖 |
| 5 | QUIC 服务端实现 | P0 | ✅ 已实现 | `start_quic_server` 完整，Quinn + rustls，含认证 |
| 6 | CLI send 命令 | P0 | ✅ 已实现 | 协议协商、SHA-256 预计算、MultiProgress 均完整 |
| 7 | CLI receive 命令 | P0 | ✅ 已实现 | 支持 one-shot 和持续模式 |
| 8 | 协议自动协商 | P1 | ✅ 已实现 | `/health` 探测 + `X-AeroSync` header 触发 QUIC 升级 |
| 9 | 分片上传 | P1 | ✅ 已实现 | 32MB 分片、断点续传、服务端合并校验 |
| 10 | 批量传输流水线 | P1 | ✅ 已实现 | Semaphore + FuturesUnordered，自适应并发 |
| 11 | 带宽限速 | P1 | ❌ 未实现 | 无 rate limiter，无配置字段 |
| 12 | 传输历史持久化 | P1 | ❌ 未实现 | 内存态，进程重启后丢失 |
| 13 | 预检验 | P1 | ⚠️ 部分实现 | 有网络连通性探测，**缺磁盘空间检查** |
| 14 | /health 端点 | P2 | ✅ 已实现 | 含 `X-AeroSync: true` 响应头，已被协商流程使用 |
| 15 | Token 持久化 | P1 | ❌ 未实现 | `generate` 只打印到 stdout，无文件写入 |

**已实现 10 项 / 部分实现 2 项 / 未实现 3 项**

---

## 2. 开发路线图

### Phase 4 — 可靠性与可观测性

优先补齐运维关键短板：

| 任务 | 优先级 | 影响模块 |
|------|--------|---------|
| 审计日志写入磁盘 | P0 | `aerosync-core/src/audit.rs`（新增） |
| Token 持久化到磁盘 | P1 | `aerosync-core/src/auth/` |
| 传输历史持久化 | P1 | `aerosync-core/src/history.rs`（新增） |
| 预检验（磁盘空间） | P1 | `src/main.rs` cmd_send 前置步骤 |
| 外部 TLS 证书加载 | P0 | `aerosync-core/src/server.rs` |

### Phase 5 — 性能与控制

| 任务 | 优先级 | 影响模块 |
|------|--------|---------|
| 带宽限速（Token Bucket） | P1 | `aerosync-protocols/src/ratelimit.rs`（新增） |

---

## 3. P0 待实现详细设计

### 3.1 审计日志持久化

**当前状态**：`ReceivedFile` 含 `received_at / sender_ip / original_name`，只在内存中。

**目标**：所有传输操作（收/发/失败）追加写入 JSONL 格式日志文件，进程重启后不丢失。

**设计**：

```rust
// aerosync-core/src/audit.rs

#[derive(Serialize, Deserialize)]
pub struct AuditEntry {
    pub timestamp: u64,          // Unix timestamp（秒）
    pub event: AuditEvent,
    pub filename: String,
    pub size: u64,
    pub sha256: Option<String>,
    pub remote_ip: Option<String>,
    pub direction: Direction,    // Send / Receive
    pub result: AuditResult,     // Ok / Err(message)
    pub protocol: String,        // "http" / "quic" / "s3" / "ftp"
}

pub enum AuditEvent {
    TransferStarted,
    TransferCompleted,
    TransferFailed,
    ChunkUploaded { index: u32, total: u32 },
    AuthFailed,
}

pub struct AuditLogger {
    file: Arc<Mutex<tokio::fs::File>>,  // 追加写入
}

impl AuditLogger {
    pub async fn new(path: &Path) -> Result<Self>;
    pub async fn log(&self, entry: AuditEntry) -> Result<()>;
    // 每条记录一行 JSON，追加到文件末尾
}
```

**日志文件位置**：`~/.config/aerosync/audit.log`（可通过配置覆盖）

**日志格式（JSONL）**：

```jsonl
{"timestamp":1775903419,"event":"TransferCompleted","filename":"data.bin","size":157286400,"sha256":"204be31...","remote_ip":"192.168.1.5","direction":"Receive","result":"Ok","protocol":"http"}
{"timestamp":1775903500,"event":"AuthFailed","filename":"","size":0,"sha256":null,"remote_ip":"10.0.0.3","direction":"Receive","result":"Err(\"Invalid token\")","protocol":"http"}
```

**集成点**：
- `server.rs handle_file_upload` / `handle_chunk_complete` — 记录接收事件
- `transfer.rs execute_upload` — 记录发送事件
- `server.rs handle_http_request`（auth 失败路径）— 记录 AuthFailed

---

### 3.2 外部 TLS 证书加载

**当前状态**：QUIC 服务端用 `rcgen` 自动生成临时自签名证书，每次重启证书变化，客户端无法做证书固定（pinning）。

**目标**：支持从文件加载 PEM 证书和私钥；自签名作为兜底。

**设计**：

在 `ServerConfig` 中新增：

```rust
pub struct TlsConfig {
    pub cert_path: Option<PathBuf>,   // PEM 证书文件路径
    pub key_path: Option<PathBuf>,    // PEM 私钥文件路径
    // 均为 None 时自动生成自签名证书
}
```

`configure_quic_server` 加载逻辑：

```rust
fn load_tls_config(tls: &TlsConfig) -> Result<rustls::ServerConfig> {
    match (&tls.cert_path, &tls.key_path) {
        (Some(cert), Some(key)) => {
            // 从 PEM 文件加载
            let cert_chain = load_pem_certs(cert)?;
            let private_key = load_pem_key(key)?;
            build_rustls_config(cert_chain, private_key)
        }
        _ => {
            // 自动生成自签名证书
            generate_self_signed()
        }
    }
}
```

CLI 新增参数：

```bash
aerosync receive --tls-cert /path/to/cert.pem --tls-key /path/to/key.pem
```

---

## 4. P1 待实现详细设计

### 4.1 Token 持久化

**当前状态**：`aerosync token generate` 只 `println!` 到 stdout，重启后需重新生成并同步到两端。

**目标**：生成的 Token + Secret 持久化到配置目录，`receive` 自动加载。

**设计**：

```rust
// aerosync-core/src/auth/store.rs

pub struct TokenStore {
    path: PathBuf,   // ~/.config/aerosync/tokens.toml
}

#[derive(Serialize, Deserialize)]
struct StoredToken {
    token: String,
    secret: String,
    created_at: u64,
    expires_at: u64,
    label: Option<String>,   // 用户自定义备注
}

impl TokenStore {
    pub async fn save(&self, entry: StoredToken) -> Result<()>;
    pub async fn load_all(&self) -> Result<Vec<StoredToken>>;
    pub async fn find_valid(&self) -> Result<Option<StoredToken>>;
    pub async fn revoke(&self, token: &str) -> Result<()>;
}
```

`cmd_token generate` 新增 `--save` flag：

```bash
aerosync token generate --save            # 生成并保存到 tokens.toml
aerosync token generate --save --label "prod-server-1"
aerosync token list                       # 列出所有已保存 token
aerosync token revoke <token-prefix>      # 撤销
```

`cmd_receive` 启动时自动从 `TokenStore` 加载最近有效 Token（若未通过 CLI 显式指定）。

---

### 4.2 带宽限速

**当前状态**：无任何限速逻辑，本地回环测试速度达 134–183 MB/s，生产环境可能占满链路。

**目标**：支持全局上传/下载速率上限，精度到 KB/s 级别。

**设计（Token Bucket）**：

```rust
// aerosync-protocols/src/ratelimit.rs

pub struct RateLimiter {
    rate_bytes_per_sec: u64,
    tokens: AtomicU64,           // 当前可用令牌（字节数）
    last_refill: Mutex<Instant>,
}

impl RateLimiter {
    pub fn new(rate_bytes_per_sec: u64) -> Self;

    /// 消耗 n 字节令牌，不足时 sleep 等待补充
    pub async fn consume(&self, bytes: u64);
}
```

集成到 `HttpTransfer` 的流式写入循环：

```rust
// 每写一个 chunk 后限速
while let Some(chunk) = stream.next().await {
    writer.write_all(&chunk).await?;
    if let Some(ref rl) = self.rate_limiter {
        rl.consume(chunk.len() as u64).await;
    }
}
```

配置文件新增字段：

```toml
[transfer]
upload_limit_kbps = 0    # 0 = 不限速
download_limit_kbps = 0
```

CLI 参数：

```bash
aerosync send ... --limit 10MB       # 支持 KB/MB 单位
aerosync send ... --limit 512KB
```

---

### 4.3 传输历史持久化

**当前状态**：`received_files` 只在内存，`/status` 端点返回当次进程数据，重启后清空。

**目标**：接收端每次完成传输后追加记录到磁盘，支持查询历史。

**设计**：

```rust
// aerosync-core/src/history.rs

#[derive(Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: Uuid,
    pub filename: String,
    pub original_name: String,
    pub saved_path: PathBuf,
    pub size: u64,
    pub sha256: Option<String>,
    pub sender_ip: Option<String>,
    pub protocol: String,
    pub completed_at: u64,
    pub duration_ms: u64,
    pub avg_speed_bps: u64,
}

pub struct HistoryStore {
    db_path: PathBuf,    // ~/.config/aerosync/history.jsonl
}

impl HistoryStore {
    pub async fn append(&self, entry: HistoryEntry) -> Result<()>;
    pub async fn query(
        &self,
        limit: usize,
        since: Option<u64>,
    ) -> Result<Vec<HistoryEntry>>;
}
```

CLI 新增子命令：

```bash
aerosync history                    # 列出最近 20 条
aerosync history --limit 50
aerosync history --since 2026-01-01
```

---

### 4.4 预检验（Preflight）

**当前状态**：`negotiate_protocol` 有 2s 网络连通性探测，无磁盘空间检查。

**目标**：`send` 前检查本地读权限 + 远端磁盘剩余空间（需接收端配合）；接收端 `/health` 返回磁盘信息。

**设计**：

接收端 `/health` 响应扩展：

```json
{
  "status": "ok",
  "received_files": 12,
  "free_bytes": 107374182400,
  "total_bytes": 536870912000,
  "version": "0.1.0"
}
```

发送端 preflight 检查：

```rust
async fn preflight_check(
    files: &[(PathBuf, PathBuf, u64)],
    dest_url: &str,
) -> Result<()> {
    let total_size: u64 = files.iter().map(|(_, _, s)| s).sum();

    // 1. 检查源文件可读性
    for (path, _, _) in files {
        if !path.exists() {
            return Err(AeroSyncError::Io(...));
        }
    }

    // 2. 查询接收端磁盘空间
    let health: HealthResponse = reqwest::get(&health_url).await?.json().await?;
    if let Some(free) = health.free_bytes {
        if free < total_size {
            return Err(AeroSyncError::InsufficientSpace {
                required: total_size,
                available: free,
            });
        }
    }

    Ok(())
}
```

---

## 5. P2 参考设计

### 5.1 /health 端点扩展

当前已实现基础 `/health`，后续可扩展为标准监控格式：

```json
{
  "status": "ok",
  "version": "0.1.0",
  "uptime_seconds": 3600,
  "received_files": 42,
  "active_transfers": 3,
  "free_bytes": 107374182400,
  "protocols": ["http", "quic"]
}
```

Prometheus 指标端点（可选）：

```
GET /metrics
aerosync_received_files_total 42
aerosync_received_bytes_total 1073741824
aerosync_active_transfers 3
aerosync_free_disk_bytes 107374182400
```

---

## 6. 协议交互时序

### 6.1 QUIC 自动协商流程

```
Client                           Server (port 7788)
  │                                    │
  │  GET http://{host}:7788/health     │
  │ ─────────────────────────────────► │
  │                                    │
  │  200 OK                            │
  │  X-AeroSync: true                  │
  │ ◄───────────────────────────────── │
  │                                    │
  │  (升级到 QUIC, port = 7788+1)      │
  │                                    │
  │  QUIC connect → {host}:7789        │
  │ ─────────────────────────────────► │
  │                                    │
  │  UPLOAD:{filename}:{size}:{token}  │
  │ ─────────────────────────────────► │
  │                                    │
  │  stream bytes (流式传输)            │
  │ ─────────────────────────────────► │
  │                                    │
  │  OK:{sha256}                       │
  │ ◄───────────────────────────────── │
```

降级条件：
- `/health` 超时（2s）
- 响应头无 `X-AeroSync: true`
- 目标 URL 已有显式协议前缀（`http://`、`s3://`、`ftp://`）

### 6.2 分片上传流程

```
Client                                    Server
  │                                          │
  │  POST /upload/chunk                      │
  │  ?task_id=UUID&chunk_index=0             │
  │  &total_chunks=5&filename=file.bin       │
  │  body: [32MB bytes]                      │
  │ ───────────────────────────────────────► │
  │  200 OK {"received":0}                   │  写入 .aerosync/tmp/{uuid}/00000000
  │ ◄─────────────────────────────────────── │
  │                                          │
  │  POST /upload/chunk?chunk_index=1 ...    │
  │ ───────────────────────────────────────► │
  │  200 OK {"received":1}                   │  写入 .aerosync/tmp/{uuid}/00000001
  │ ◄─────────────────────────────────────── │
  │                                          │
  │  ... (中途可中断，状态持久化) ...          │
  │                                          │
  │  POST /upload/complete                   │
  │  ?task_id=UUID&filename=file.bin         │
  │  &total_chunks=5&sha256=204be31...       │
  │ ───────────────────────────────────────► │
  │                                          │  顺序拼接 00000000~00000004
  │                                          │  校验 SHA-256
  │  200 OK {"saved":"file.bin","size":...}  │  删除 tmp/{uuid}/
  │ ◄─────────────────────────────────────── │
```

断点续传恢复：
- 客户端读取 `.aerosync/{task_id}.json`
- `pending_chunks()` 跳过 `completed_chunks` 中的序号
- 直接从剩余分片继续，不重传已完成分片

### 6.3 Auth 验证流程（HTTP）

```
Client                                    Server
  │                                          │
  │  POST /upload                            │
  │  Authorization: Bearer <token>           │
  │  X-File-Hash: sha256hex                  │
  │ ───────────────────────────────────────► │
  │                                          │  AuthMiddleware::authenticate_http_request()
  │                                          │  ├─ 解析 Bearer token
  │                                          │  ├─ HMAC-SHA256 验签
  │                                          │  ├─ 检查过期时间
  │                                          │  └─ 验证通过 → 处理上传
  │  401 Unauthorized（验证失败）             │
  │  or 200 OK（验证通过）                    │
  │ ◄─────────────────────────────────────── │
```

### 6.4 S3 上传流程

```
Client                           S3 / MinIO
  │                                    │
  │  PUT /{bucket}/{key}               │
  │  Authorization: AWS4-HMAC-SHA256   │  （AWS S3）
  │  or Authorization: Bearer token    │  （MinIO）
  │  x-amz-date: 20260411T000000Z      │
  │  Content-Length: {size}            │
  │  body: file bytes                  │
  │ ─────────────────────────────────► │
  │  200 OK                            │
  │ ◄───────────────────────────────── │
```

URL 格式：`s3://bucket-name/path/to/key`

不支持分片续传（S3 原生 Multipart Upload 未实现），大文件通过单次 PUT 上传。

---

*最后更新：2026-04-11*

# AeroSync 协议设计与功能规划

本文档覆盖协议实现细节，以及当前实现状态与后续开发规划。

---

## 目录

1. [功能实现现状](#1-功能实现现状)
2. [开发路线图](#2-开发路线图)
3. [P0 已实现设计参考](#3-p0-已实现设计参考)
4. [P1 已实现设计参考](#4-p1-已实现设计参考)
5. [P2 参考设计](#5-p2-参考设计)
6. [协议交互时序](#6-协议交互时序)

---

## 1. 功能实现现状

| # | 功能点 | 优先级 | 状态 | 说明 |
|---|--------|--------|------|------|
| 1 | SHA-256 文件完整性校验 | P0 | ✅ 已实现 | 发送方预计算、接收方校验，HTTP 单文件 + 分片均覆盖 |
| 2 | 审计日志 | P0 | ✅ 已实现 | JSONL 格式追加写入磁盘；含 IP、时间、文件名、SHA-256、协议；HTTP + QUIC 均集成 |
| 3 | TLS 证书管理 | P0 | ✅ 已实现 | 支持从 PEM 文件加载外部证书；无证书时自动生成自签名兜底；`--tls-cert`/`--tls-key` CLI 参数 |
| 4 | Auth 与传输流程集成 | P0 | ✅ 已实现 | `/upload`、`/upload/chunk`、`/upload/complete`、QUIC 均覆盖 |
| 5 | QUIC 服务端实现 | P0 | ✅ 已实现 | `start_quic_server` 完整，Quinn + rustls，含认证 |
| 6 | CLI send 命令 | P0 | ✅ 已实现 | 协议协商、SHA-256 预计算、MultiProgress、限速、预检验均完整 |
| 7 | CLI receive 命令 | P0 | ✅ 已实现 | 支持 one-shot 和持续模式；支持外部 TLS 证书 |
| 8 | 协议自动协商 | P1 | ✅ 已实现 | `/health` 探测 + `X-AeroSync` header 触发 QUIC 升级 |
| 9 | 分片上传 | P1 | ✅ 已实现 | 32MB 分片、断点续传、服务端合并校验 |
| 10 | 批量传输流水线 | P1 | ✅ 已实现 | Semaphore + FuturesUnordered，自适应并发 |
| 11 | 带宽限速 | P1 | ✅ 已实现 | Token Bucket RateLimiter；集成到分片上传；`--limit 512KB/10MB/1MB/s` |
| 12 | 传输历史持久化 | P1 | ✅ 已实现 | JSONL 追加写入 `~/.config/aerosync/history.jsonl`；`aerosync history` 支持过滤查询 |
| 13 | 预检验 | P1 | ✅ 已实现 | `/health` 返回 `free_bytes/total_bytes/version`；发送前自动检查磁盘空间；`--no-preflight` 跳过 |
| 14 | /health 端点 | P2 | ✅ 已实现 | 返回 `free_bytes/total_bytes/version`；含 `X-AeroSync: true` 响应头 |
| 15 | Token 持久化 | P1 | ✅ 已实现 | TOML 格式存储至 `~/.config/aerosync/tokens.toml`；`token generate --save/list/revoke` |

**已实现 15 项 / 部分实现 0 项 / 未实现 0 项 — Phase 4 全部完成 🎉**

---

## 2. 开发路线图

### Phase 4 — 可靠性与可观测性 ✅ 已完成

| 任务 | 优先级 | 状态 | 实现文件 |
|------|--------|------|---------|
| 审计日志写入磁盘 | P0 | ✅ | `aerosync-core/src/audit.rs` |
| 外部 TLS 证书加载 | P0 | ✅ | `aerosync-core/src/server.rs` `TlsConfig` |
| Token 持久化到磁盘 | P1 | ✅ | `aerosync-core/src/auth/store.rs` |
| 传输历史持久化 | P1 | ✅ | `aerosync-core/src/history.rs` |
| 预检验（磁盘空间） | P1 | ✅ | `aerosync-core/src/preflight.rs` |
| 带宽限速（Token Bucket） | P1 | ✅ | `aerosync-protocols/src/ratelimit.rs` |

### Phase 5 — 生产增强（规划中）

详细设计见 [docs/phase5-plan.md](phase5-plan.md)。

| # | 任务 | 优先级 | 影响模块 |
|---|------|--------|---------|
| 5.1 | Prometheus 指标端点 `/metrics` | P1 | `aerosync-core/src/metrics.rs`（新增） |
| 5.2 | WebSocket 实时传输进度推送 | P1 | `aerosync-core/src/server.rs` |
| 5.3 | 多接收目录路由（按 sender IP/tag） | P2 | `aerosync-core/src/routing.rs`（新增） |
| 5.4 | S3 Multipart Upload（大文件分片） | P2 | `aerosync-protocols/src/s3.rs` |
| 5.5 | 配置热重载（SIGHUP） | P2 | `src/main.rs`，`aerosync-core/src/server.rs` |

---

## 3. P0 已实现设计参考

### 3.1 审计日志持久化 ✅

**实现**：`aerosync-core/src/audit.rs`，JSONL 格式追加写入，`Arc<Mutex<File>>` 线程安全，支持并发写入。

**核心接口**：

```rust
pub struct AuditLogger { file: Arc<Mutex<tokio::fs::File>>, path: PathBuf }

impl AuditLogger {
    pub async fn new(path: &Path) -> Result<Self>;
    pub async fn log(&self, entry: AuditEntry) -> Result<()>;
    pub async fn log_completed(&self, direction, protocol, filename, size, sha256, remote_ip);
    pub async fn log_failed(&self, direction, protocol, filename, size, remote_ip, error);
    pub async fn log_auth_failed(&self, protocol, remote_ip, reason);
    pub async fn read_all(&self) -> Result<Vec<AuditRecord>>;
    pub async fn read_recent(&self, limit: usize) -> Result<Vec<AuditRecord>>;
}
```

**日志文件位置**：`ServerConfig.audit_log: Option<PathBuf>`（由调用方指定）

**日志格式（JSONL）**：

```jsonl
{"timestamp":1775903419,"event":{"type":"transfer_completed"},"filename":"data.bin","size":157286400,"sha256":"204be31...","remote_ip":"192.168.1.5","direction":"Receive","result":"Ok","protocol":"http"}
{"timestamp":1775903500,"event":{"type":"auth_failed"},"filename":"","size":0,"remote_ip":"10.0.0.3","direction":"Receive","result":{"Err":"Invalid token"},"protocol":"http"}
```

**集成点**：
- `server.rs handle_file_upload` / `handle_chunk_complete` — 记录接收事件
- `transfer.rs transfer_worker` — 记录发送事件
- 所有 auth 失败路径 — 记录 `AuthFailed`

---

### 3.2 外部 TLS 证书加载 ✅

**实现**：`ServerConfig.tls: Option<TlsConfig>`，`rustls-pemfile` 加载 PEM 文件；`--tls-cert`/`--tls-key` CLI 参数；无证书时自动生成自签名兜底。

**核心接口**：

```rust
// aerosync-core/src/server.rs
pub struct TlsConfig {
    pub cert_path: PathBuf,   // PEM 证书文件路径
    pub key_path: PathBuf,    // PEM 私钥文件路径
}

// configure_quic_server(tls: Option<&TlsConfig>) — 有证书时加载，无则自签名
// load_tls_from_pem(cert_path, key_path) — 支持 PKCS8 和 RSA 两种私钥格式
```

**CLI 用法**：

```bash
aerosync receive --tls-cert /path/to/cert.pem --tls-key /path/to/key.pem
```

---

## 4. P1 已实现设计参考

### 4.1 Token 持久化 ✅

**实现**：`aerosync-core/src/auth/store.rs`，TOML 格式存储到 `~/.config/aerosync/tokens.toml`；`token generate --save/list/revoke` 子命令。

**核心接口**：

```rust
pub struct TokenStore { path: PathBuf }

impl TokenStore {
    pub fn new(path: &Path) -> Self;
    pub fn default_path() -> PathBuf;  // ~/.config/aerosync/tokens.toml
    pub fn save(&self, token: &str, label: Option<&str>, expires_at: u64) -> Result<()>;
    pub fn list_all(&self) -> Result<Vec<StoredToken>>;
    pub fn list_valid(&self) -> Result<Vec<StoredToken>>;
    pub fn find_by_prefix(&self, prefix: &str) -> Result<Option<StoredToken>>;
    pub fn revoke(&self, token: &str) -> Result<bool>;
    pub fn prune(&self) -> Result<usize>;  // 清理过期/已撤销
}
```

**CLI 用法**：

```bash
aerosync token generate --save            # 生成并保存到 tokens.toml
aerosync token generate --save --label "prod-server-1"
aerosync token list                       # 列出所有已保存 token
aerosync token revoke <token-prefix>      # 按前缀撤销
```

---

### 4.2 带宽限速 ✅

**实现**：`aerosync-protocols/src/ratelimit.rs`，Token Bucket 算法；集成到 `HttpTransfer.upload_chunked` 分片循环；`--limit 512KB/10MB/1MB/s` CLI 参数；`parse_limit()` 支持多种单位格式。

**核心接口**：

```rust
pub struct RateLimiter { inner: Arc<Mutex<RateLimiterInner>> }

impl RateLimiter {
    pub fn new(rate_bytes_per_sec: u64) -> Self;  // 0 = 不限速
    pub fn unlimited() -> Self;
    pub async fn consume(&self, bytes: u64);      // 令牌不足时异步等待
}

/// 解析限速字符串："512KB" / "10MB" / "1MB/s" / "100"（默认 KB/s）
pub fn parse_limit(s: &str) -> Option<u64>;
```

**集成**：`HttpConfig.upload_limit_bps: u64`（0 = 不限速），每个分片上传前调用 `rate_limiter.consume(chunk_size).await`

**CLI 用法**：

```bash
aerosync send ./large-file.bin 192.168.1.10:7788 --limit 10MB
aerosync send ./dir/ host:7788 --limit 512KB -r
```

---

### 4.3 传输历史持久化 ✅

**实现**：`aerosync-core/src/history.rs`，JSONL 格式追加到 `~/.config/aerosync/history.jsonl`；`HistoryQuery` 支持按方向/协议/成功状态过滤；`aerosync history --sent/--received/--success-only/--limit` 子命令。

**核心接口**：

```rust
pub struct HistoryStore { path: PathBuf, file: Arc<Mutex<tokio::fs::File>> }

impl HistoryStore {
    pub async fn new(path: &Path) -> Result<Self>;
    pub fn default_path() -> PathBuf;  // ~/.config/aerosync/history.jsonl
    pub async fn append(&self, entry: HistoryEntry) -> Result<()>;
    pub async fn append_silent(&self, entry: HistoryEntry);  // fire-and-forget
    pub async fn read_all(&self) -> Result<Vec<HistoryEntry>>;
    pub async fn query(&self, q: &HistoryQuery) -> Result<Vec<HistoryEntry>>;
    pub async fn recent(&self, limit: usize) -> Result<Vec<HistoryEntry>>;
}

pub struct HistoryQuery {
    pub direction: Option<String>,   // "send" / "receive"
    pub protocol: Option<String>,    // "http" / "quic" / ...
    pub success_only: bool,
    pub limit: usize,                // 0 = 不限
}
```

**CLI 用法**：

```bash
aerosync history                    # 最近 20 条（默认）
aerosync history --limit 50
aerosync history --sent             # 只看发送记录
aerosync history --success-only    # 只看成功记录
```

---

### 4.4 预检验（Preflight）✅

**实现**：`aerosync-core/src/preflight.rs`，`preflight_check()` 探测 `/health` 并验证磁盘空间；`free_bytes == 0` 时跳过检查（兼容旧版）；`--no-preflight` 标志完全跳过；`/health` 扩展返回 `free_bytes/total_bytes/version`（`libc::statvfs` 跨平台实现）。

**`/health` 响应格式**：

```json
{
  "status": "ok",
  "received_files": 12,
  "free_bytes": 107374182400,
  "total_bytes": 536870912000,
  "version": "0.2.0"
}
```

**核心接口**：

```rust
/// 探测接收端健康状态（返回磁盘信息和版本号）
pub async fn probe_receiver(http_base: &str) -> Result<PreflightResult>;

/// 验证磁盘空间是否足够；free_bytes==0 时视为跳过
pub async fn preflight_check(
    http_base: &str,
    total_bytes: u64,
) -> Result<PreflightResult, PreflightError>;
```

**CLI 用法**：`aerosync send` 默认自动执行预检验，`--no-preflight` 跳过。

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

*最后更新：2026-04-11（Phase 4 全部完成）*

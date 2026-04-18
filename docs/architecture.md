# AeroSync 架构文档

面向开发者的系统设计与实现细节说明。

---

## 目录

1. [整体架构](#1-整体架构)
2. [Crate 结构与依赖](#2-crate-结构与依赖)
3. [核心数据结构](#3-核心数据结构)
4. [协议层设计](#4-协议层设计)
5. [并发传输模型](#5-并发传输模型)
6. [send 命令主流程](#6-send-命令主流程)
7. [断点续传机制](#7-断点续传机制)
8. [接收端（服务器）](#8-接收端服务器)
9. [认证机制](#9-认证机制)
10. [配置文件](#10-配置文件)
11. [错误处理](#11-错误处理)
12. [测试策略](#12-测试策略)

---

## 1. 整体架构

AeroSync 是一个面向 agent 间跨网络文件传输的 CLI 工具，核心设计目标：

- **高效传输**：GB 级大文件 + 千级小文件批量均有针对性优化
- **协议自适应**：两端均安装 AeroSync 时自动协商 QUIC；否则降级 HTTP/S3/FTP
- **断点续传**：大文件分片，持久化进度，网络中断后可恢复
- **安全**：HMAC-SHA256 Token 认证 + SHA-256 内容校验

### 分层架构图

```
┌─────────────────────────────────────────────────┐
│               aerosync (binary)                 │
│   CLI 解析 / 协议协商 / 进度渲染 / 配置加载      │
│   src/main.rs · src/config.rs                   │
└────────────────────┬────────────────────────────┘
                     │ 使用
        ┌────────────┴────────────┐
        ▼                         ▼
┌───────────────┐       ┌──────────────────────┐
│ aerosync-core │       │ aerosync-protocols   │
│               │       │                      │
│ TransferEngine│──────▶│ AutoAdapter          │
│ FileReceiver  │       │  ├─ HttpTransfer     │
│ ResumeStore   │       │  ├─ QuicTransfer     │
│ AuthManager   │       │  ├─ S3Transfer       │
│ FileManager   │       │  └─ FtpTransfer      │
│ ProgressMonitor│      │                      │
└───────────────┘       └──────────────────────┘
```

---

## 2. Crate 结构与依赖

### Workspace 布局

```
AeroSync/
├── Cargo.toml              # workspace 根
├── src/
│   ├── main.rs             # CLI 入口、cmd_send / cmd_receive / cmd_token
│   └── config.rs           # TOML 配置文件解析
├── aerosync-core/
│   └── src/
│       ├── lib.rs
│       ├── transfer.rs     # 传输引擎（并发调度核心）
│       ├── server.rs       # HTTP 接收端（warp）
│       ├── resume.rs       # 断点续传状态持久化
│       ├── auth/           # 认证（Token 生成 / 验证）
│       ├── file_manager.rs # SHA-256 / 文件 I/O
│       └── progress.rs     # 进度监控
├── aerosync-protocols/
│   └── src/
│       ├── lib.rs
│       ├── traits.rs       # TransferProtocol / ProtocolAdapter trait
│       ├── adapter.rs      # AutoAdapter（协议路由）
│       ├── http.rs         # HTTP 传输（单文件 + 分片）
│       ├── quic.rs         # QUIC 传输（quinn 0.10）
│       ├── s3.rs           # S3 / MinIO 传输
│       └── ftp.rs          # FTP 传输（suppaftp）
└── aerosync-mcp/
    └── src/
        ├── lib.rs
        ├── main.rs         # MCP server stdio 入口
        └── server.rs       # 8 个工具实现（send_file 等）
```

### 关键外部依赖

| 依赖 | 版本 | 用途 |
|------|------|------|
| `tokio` | 1 (full) | 异步运行时 |
| `quinn` | 0.10 | QUIC 协议 |
| `reqwest` | 0.11 | HTTP 客户端 |
| `warp` | 0.3 | HTTP 服务端 |
| `rustls` | 0.21 | TLS |
| `suppaftp` | 8.0.2 (tokio) | FTP 客户端 |
| `clap` | 4 (derive) | CLI 解析 |
| `serde` / `serde_json` | 1.0 | 序列化 |
| `toml` | 0.8 | 配置文件 |
| `indicatif` | 0.17 | 进度条 |
| `sha2` | 0.10 | SHA-256 校验 |
| `uuid` | 1 | 任务 ID |
| `anyhow` / `thiserror` | — | 错误处理 |
| `shellexpand` | 3 | 路径 `~` 展开 |
| `futures` | — | `stream::iter + buffer_unordered` |

---

## 3. 核心数据结构

### 3.1 TransferTask

```rust
pub struct TransferTask {
    pub id: Uuid,               // 任务唯一 ID
    pub source_path: PathBuf,   // 源文件路径（上传）或保存路径（下载）
    pub destination: String,    // 目标 URL（含完整路径，如 http://host/upload/sub/file.txt）
    pub file_size: u64,         // 文件字节数
    pub is_upload: bool,        // true = 上传，false = 下载
    pub sha256: Option<String>, // 预计算的 SHA-256（十六进制）
}
```

> **注意**：`destination` 包含完整文件路径（含子目录），断点续传通过 `source_path + destination` 联合匹配状态。

### 3.2 TransferConfig

```rust
pub struct TransferConfig {
    pub max_concurrent_transfers: usize, // Semaphore permits（默认 4）
    pub chunk_size: usize,               // 分片大小（字节，默认 32MB）
    pub retry_attempts: u32,             // 失败重试次数（默认 3）
    pub timeout_seconds: u64,            // 连接超时（默认 60s）
    pub use_quic: bool,                  // 优先使用 QUIC
    pub auth_token: Option<String>,      // Bearer Token
    pub chunked_threshold: u64,          // 超过此大小触发分片（默认 64MB）
    pub resume_state_dir: PathBuf,       // 续传状态目录（默认 .aerosync/）
    pub enable_resume: bool,             // 启用断点续传（默认 true）
    pub small_file_concurrency: usize,   // 小文件并发数（默认 16，< 1MB）
    pub medium_file_concurrency: usize,  // 中等文件并发数（默认 8，1–64MB）
    pub small_file_threshold: u64,       // 小文件阈值（默认 1MB）
}
```

**自适应并发策略**：

| 文件大小 | 并发数 | 传输方式 |
|---------|--------|---------|
| < 1MB | 16 | 单文件直传 |
| 1MB – 64MB | 8 | 单文件直传 |
| ≥ 64MB | 1（分片内串行） | 分片上传（32MB/片） |

### 3.3 ResumeState

```rust
pub struct ResumeState {
    pub task_id: Uuid,
    pub source_path: PathBuf,
    pub destination: String,         // 完整目标 URL（含文件名路径）
    pub total_size: u64,
    pub chunk_size: u64,             // 默认 32MB（DEFAULT_CHUNK_SIZE）
    pub total_chunks: u32,
    pub completed_chunks: Vec<u32>,  // 已完成分片序号列表（有序）
    pub sha256: Option<String>,      // 整文件 SHA-256
    pub created_at: u64,             // Unix timestamp
    pub updated_at: u64,
}
```

持久化位置：`.aerosync/{task_id}.json`（TOML 配置中可自定义目录）

### 3.4 ServerConfig

```rust
pub struct ServerConfig {
    pub http_port: u16,            // 默认 7788
    pub quic_port: u16,            // 默认 7789
    pub bind_address: String,      // 默认 "0.0.0.0"
    pub receive_directory: PathBuf,
    pub max_file_size: u64,
    pub allow_overwrite: bool,
    pub enable_http: bool,
    pub enable_quic: bool,
    pub auth: Option<AuthConfig>,
}
```

---

## 4. 协议层设计

### 4.1 Trait 体系

```
aerosync-protocols::traits
├── TransferProtocol        # 单文件传输接口（各协议实现）
│   ├── upload_file()
│   ├── download_file()
│   ├── resume_transfer()   # 带 offset 的续传
│   ├── supports_resume()
│   └── protocol_name()
│
aerosync-core::traits
└── ProtocolAdapter         # 传输引擎调用的顶层接口
    ├── upload()
    ├── download()
    ├── upload_chunked()    # 分片上传（断点续传）
    └── protocol_name()
```

`AutoAdapter` 同时实现 `ProtocolAdapter`，内部根据 URL scheme 路由到具体协议。

### 4.2 AutoAdapter 路由逻辑

```
upload(task) / download(task)
    │
    ├─ task.destination.starts_with("s3://")   → S3Transfer
    ├─ task.destination.starts_with("ftp://")  → FtpTransfer
    ├─ task.destination.starts_with("quic://") → QuicTransfer
    └─ 其他（http://, https://）               → HttpTransfer
                                                 （复用 Arc<reqwest::Client>）

upload_chunked(task, state)
    │
    ├─ s3:// / ftp:// / quic://  → fallback 到普通 upload()
    └─ http://                   → HttpTransfer::upload_chunked()
```

**HTTP Client 复用**：`AutoAdapter` 构造时创建一个 `Arc<reqwest::Client>`，所有 HTTP 传输共享同一连接池，避免重复 TLS 握手开销。

### 4.3 各协议实现细节

#### HTTP (`aerosync-protocols/src/http.rs`)

- **单文件上传**：`multipart/form-data`，附带 `X-File-Hash` 和 `Authorization` header
- **分片上传**：
  - `POST /upload/chunk?task_id=&chunk_index=&total_chunks=&filename=` + body 为原始字节
  - 指数退避重试（500ms, 1s, 1.5s...）
  - `POST /upload/complete?task_id=&filename=&total_chunks=&sha256=` 触发合并
- **断点续传**：跳过 `state.completed_chunks` 中已完成的分片

#### QUIC (`aerosync-protocols/src/quic.rs`)

- 基于 `quinn` 0.10，使用 `rustls` TLS
- 消息协议：`UPLOAD:{filename}:{size}:{token}\n` 头 + 流式字节
- 自动协商：客户端探测 `/health` 响应头 `X-AeroSync: true` 时升级

#### S3 (`aerosync-protocols/src/s3.rs`)

- URL 格式：`s3://bucket/key/path`
- AWS S3：SigV4 签名（`x-amz-date` + `AWS4-HMAC-SHA256` header）
- MinIO：Bearer Token 认证
- 不支持分片续传（直接 PUT，fallback 到普通 upload）

#### FTP (`aerosync-protocols/src/ftp.rs`)

- URL 格式：`ftp://host:port/path`
- 使用 `suppaftp::tokio::AsyncFtpStream`
- 支持 passive mode，默认端口 21
- 不支持分片续传

---

## 5. 并发传输模型

### 5.1 架构概览

```
TransferEngine
    │
    ├── task_tx ──────────────────────────────► transfer_worker (tokio::spawn)
    │   (unbounded channel)                         │
    │                                         ┌─────▼──────┐
    └── monitor (Arc<RwLock<ProgressMonitor>>) │ Semaphore  │ max_concurrent permits
                                              └─────┬──────┘
                                                    │ acquire_owned()
                                              ┌─────▼──────────────────┐
                                              │  FuturesUnordered      │
                                              │  ┌────┐ ┌────┐ ┌────┐ │
                                              │  │ T1 │ │ T2 │ │ T3 │ │  ← 并发任务
                                              │  └────┘ └────┘ └────┘ │
                                              └────────────────────────┘
```

### 5.2 transfer_worker 核心循环

```rust
loop {
    tokio::select! {
        // 接收新任务
        maybe_task = task_rx.recv() => {
            if let Some(task) = maybe_task {
                let permit = Arc::clone(&sem).acquire_owned().await?;
                // 动态 max_concurrent：根据文件大小调整 Semaphore permits
                running.push(tokio::spawn(async move {
                    let result = execute_upload(&task, &adapter, &config, progress_tx).await;
                    drop(permit);  // 任务完成自动释放 permit
                    (task.id, result)
                }));
            } else {
                // 发送端关闭，等待所有任务完成后退出
                while let Some(r) = running.next().await { handle(r); }
                break;
            }
        }
        // 处理完成的任务
        Some(result) = running.next() => {
            handle_join_result(result, &monitor).await;
        }
    }
}
```

**关键设计点**：
- `FuturesUnordered`：任意任务完成即触发处理，不按入队顺序等待
- `OwnedSemaphorePermit`：随 task future 一起 drop，无需手动释放
- `select!`：同时监听「新任务」和「完成事件」，实现零空转的事件驱动

### 5.3 SHA-256 并发预计算

在传输前，对所有文件并发计算 SHA-256（最多 8 路并发），不阻塞主流程：

```rust
let sha256_map = stream::iter(files.iter())
    .map(|(path, _, _)| async move {
        let hash = FileManager::compute_sha256(path).await.ok();
        (path.clone(), hash)
    })
    .buffer_unordered(8)
    .collect::<HashMap<_, _>>()
    .await;
```

---

## 6. send 命令主流程

```
aerosync send <source> <destination> [--recursive] [--no-resume]
```

```
collect_files(source, recursive)
    │ 返回 Vec<(abs_path, rel_path, size)>
    ▼
negotiate_protocol(destination)
    │ 探测 http://{dest}/health
    │  └─ X-AeroSync: true → quic://host:(port+1)/upload
    │  └─ 其他 / 超时    → http://host/upload
    ▼
build TransferConfig + AutoAdapter + TransferEngine
    │ engine.start(adapter).await
    ▼
并发计算 SHA-256 (buffer_unordered(8))
    ▼
for each file:
    task.destination = "{dest_url}/{rel_path}"   ← 保留子目录结构
    engine.add_transfer(task).await
    创建 MultiProgress 进度条
    ▼
轮询 ProgressMonitor（100ms 间隔）
    │ 更新每文件进度条 + 汇总条
    ▼
engine.wait().await  ← 等待所有任务完成
    ▼
打印汇总：Completed N/M files, Avg speed
```

### 目录传输路径保留

发送方使用三元组 `(absolute, relative, size)` 收集文件：

```
send ./project/ --recursive
    ./project/src/main.rs  →  relative = src/main.rs
    ./project/README.md    →  relative = README.md
```

`task.destination` = `http://host/upload/src/main.rs`

接收端通过 `warp::path::tail()` 捕获 `/upload/` 后的完整路径尾，保留子目录结构落盘。

---

## 7. 断点续传机制

适用条件：文件 ≥ 64MB（`chunked_threshold`），且 `--no-resume` 未指定。

### 7.1 状态生命周期

```
首次传输开始
    │ find_by_file(src, dest) → None
    │ 创建 ResumeState，save() → .aerosync/{uuid}.json
    ▼
逐片上传：
    for chunk in state.pending_chunks():
        upload chunk
        state.mark_chunk_done(chunk)
        store.save(&state)          ← 每片完成后持久化
    ▼
全部完成 → store.delete(task_id)   ← 清理状态文件

中途中断（网络断开 / Ctrl+C）
    └─ 状态文件保留，记录已完成分片
    ▼
再次执行相同 send 命令
    │ find_by_file(src, dest) → Some(existing_state)
    │ pending_chunks() 自动跳过已完成分片
    ▼
仅传输剩余分片 → 完成后删除状态文件
```

### 7.2 状态文件示例

```json
{
  "task_id": "b1234567-dead-beef-cafe-123456789abc",
  "source_path": "/data/large_file.bin",
  "destination": "http://192.168.1.10:7788/upload/large_file.bin",
  "total_size": 157286400,
  "chunk_size": 33554432,
  "total_chunks": 5,
  "completed_chunks": [0, 1, 2],
  "sha256": "204be3116bc23276538c7252aa8cb058b9149a757b4098e06357dd0b0f55029b",
  "created_at": 1775903419,
  "updated_at": 1775903668
}
```

### 7.3 服务端分片存储

服务端将每个分片存入临时目录，`complete` 请求时合并：

```
{receive_dir}/.aerosync/tmp/{task_id}/
    ├── 00000000   (chunk 0, 32MB)
    ├── 00000001   (chunk 1, 32MB)
    ├── 00000002   (chunk 2, 32MB)
    ├── 00000003   (chunk 3, 32MB)
    └── 00000004   (chunk 4, 22MB)
    ↓
合并 → {receive_dir}/large_file.bin
校验 SHA-256，不匹配则返回 400
```

### 7.4 resume 子命令

```bash
aerosync resume list          # 列出所有未完成传输及进度百分比
aerosync resume clear <id>    # 删除指定任务的续传状态
aerosync resume clear-all     # 清空所有续传状态
```

---

## 8. 接收端（服务器）

### 8.1 启动方式

```bash
aerosync receive [--port 7788] [--save-to ./downloads] [--token <token>] [--http-only]
```

接收端基于 `warp` 框架，默认同时启动 HTTP（7788）和 QUIC（7789）监听。

### 8.2 HTTP 路由表

| 路由 | 方法 | 说明 |
|------|------|------|
| `/health` | GET | 健康检查，响应头含 `X-AeroSync: true`（用于客户端协议协商） |
| `/status` | GET | 返回已接收文件列表和统计信息 |
| `/upload/{path...}` | POST | 单文件上传（`multipart/form-data`），支持任意子路径 |
| `/upload/chunk` | POST | 上传单个分片，Query: `task_id, chunk_index, total_chunks, filename` |
| `/upload/complete` | POST | 合并分片，Query: `task_id, filename, total_chunks, sha256` |

### 8.3 文件名安全处理

```rust
fn sanitize_filename(name: &str) -> String {
    // 1. 按 '/' 分段，逐段清理
    // 2. 每段：仅保留 alphanumeric, '-', '_', '.', ' '；其他替换为 '_'
    // 3. 拒绝 '..' 段（路径穿越攻击防护）
    // 4. 拒绝以 '/' 开头的绝对路径
    // 5. 重新用 '/' 拼接（保留子目录结构）
}
```

### 8.4 文件名冲突处理

接收端默认不覆盖同名文件，自动追加计数器：

```
file.txt → file_1.txt → file_2.txt → ...
sub/file.txt → sub/file_1.txt → ...
```

---

## 9. 认证机制

### 9.1 Token 生成

```bash
aerosync token generate [--secret <key>] [--hours 24]
```

Token 基于 HMAC-SHA256：`HMAC(secret_key, payload + timestamp)`

生成的 Token 通过 Bearer Token 方式传递：
```
Authorization: Bearer <token>
```

### 9.2 发送方配置 Token

三种方式（优先级从高到低）：

1. CLI 参数：`aerosync send ... --token <token>`
2. 环境变量：`AEROSYNC_TOKEN=<token>`
3. 配置文件：`aerosync.toml` 的 `[auth]` 段

### 9.3 服务端验证流程

```
接收请求
    │ 提取 Authorization: Bearer <token>
    ▼
AuthMiddleware::authenticate_http_request()
    ├─ auth 未启用 → 放行
    ├─ Token 有效  → 放行
    └─ Token 无效 / 缺失 → 返回 401 Unauthorized
```

QUIC 端：Token 嵌入消息头 `UPLOAD:{filename}:{size}:{token}`。

### 9.4 验证 Token

```bash
aerosync token verify --token <token> --secret <secret>
```

---

## 10. 配置文件

默认路径：`~/.config/aerosync/aerosync.toml`（`shellexpand` 展开 `~`）

```toml
[transfer]
max_concurrent = 4        # 最大并发传输数
chunk_size_mb = 32        # 分片大小（MB）
retry_attempts = 3        # 失败重试次数
timeout_seconds = 60      # 连接超时（秒）

[auth]
token = "your-token-here" # 可选，发送时自动附带

[server]
http_port = 7788          # 接收端 HTTP 端口
quic_port = 7789          # 接收端 QUIC 端口
save_to = "~/downloads"   # 文件保存目录
bind = "0.0.0.0"          # 监听地址
```

配置文件不存在时自动使用默认值，CLI 参数优先级高于配置文件。

---

## 11. 错误处理

### 错误类型层次

```rust
// aerosync-core
pub enum AeroSyncError {
    Io(#[from] std::io::Error),
    Network(String),
    InvalidConfig(String),
    AuthError(String),
    ChecksumMismatch { expected: String, actual: String },
    Timeout,
    PermissionDenied,
}

// aerosync-protocols（通过 From 转换到 AeroSyncError）
```

### 重试策略

HTTP 分片上传采用指数退避重试：

```
第 1 次重试：等待 500ms
第 2 次重试：等待 1000ms
第 3 次重试：等待 1500ms
超出 max_retries → 返回错误，保存续传状态
```

---

## 12. 测试策略

### 测试分布（共 149 个）

| Crate | 测试数 | 类型 |
|-------|--------|------|
| `aerosync` | 6 | 配置解析单元测试 |
| `aerosync-core` | 106 | 单元测试（含 warp 集成） |
| `aerosync-protocols` | 34 | 单元测试 |
| `aerosync-protocols/tests` | 3 | 集成测试（pipeline） |

### 关键测试场景

- **并发模型**：`test_engine_100_tasks_all_complete`（100 任务并发验证）
- **限流验证**：`test_semaphore_limits_concurrency`（SlowAdapter 验证 Semaphore 上限）
- **断点续传**：`test_save_and_load`、`test_find_by_file_*`、`test_mark_chunk_done*`
- **目录安全**：`test_sanitize_filename_path_traversal_stripped`、`test_sanitize_filename_preserves_subpath`
- **分片完整性**：`test_chunk_complete_sha256_mismatch_returns_error`
- **端到端**：`test_pipeline_100_small_files`（100×4KB，16 并发，warp 服务）

### 运行测试

```bash
cargo test --workspace                    # 全量测试
cargo test -p aerosync-core               # 仅 core
cargo test -p aerosync-protocols          # 仅 protocols
cargo test test_pipeline -- --nocapture   # 查看集成测试输出
```

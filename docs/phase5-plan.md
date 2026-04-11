# AeroSync Phase 5 — 生产增强开发计划

本文档描述 Phase 5 的功能规划、优先级、详细设计与验收标准。

Phase 4 已完成 15/15 项功能（189 个测试全部通过）。Phase 5 聚焦于将 AeroSync 从"功能完整"推向"生产可用"，目标是可观测性、弹性路由和协议完善。

---

## 目录

1. [功能概览](#1-功能概览)
2. [任务详细设计](#2-任务详细设计)
   - [5.1 Prometheus 指标端点](#51-prometheus-指标端点-p1)
   - [5.2 WebSocket 实时进度推送](#52-websocket-实时进度推送-p1)
   - [5.3 多接收目录路由](#53-多接收目录路由-p2)
   - [5.4 S3 Multipart Upload](#54-s3-multipart-upload-p2)
   - [5.5 配置热重载](#55-配置热重载-p2)
3. [实现顺序与依赖关系](#3-实现顺序与依赖关系)
4. [验收标准](#4-验收标准)

---

## 1. 功能概览

| # | 功能 | 优先级 | 影响模块 | 状态 |
|---|------|--------|---------|------|
| 5.1 | Prometheus 指标端点 `/metrics` | P1 | `aerosync-core/src/metrics.rs`（新增） | ❌ 未实现 |
| 5.2 | WebSocket 实时传输进度推送 | P1 | `aerosync-core/src/server.rs` | ❌ 未实现 |
| 5.3 | 多接收目录路由（按 sender IP / tag） | P2 | `aerosync-core/src/server.rs` | ❌ 未实现 |
| 5.4 | S3 Multipart Upload（大文件分片） | P2 | `aerosync-protocols/src/s3.rs` | ❌ 未实现 |
| 5.5 | 配置热重载（SIGHUP） | P2 | `src/main.rs`，`aerosync-core/src/server.rs` | ❌ 未实现 |

**P1**：对生产监控/运维有直接价值，建议优先实现。  
**P2**：增强功能，可延后或按需实现。

---

## 2. 任务详细设计

---

### 5.1 Prometheus 指标端点 (P1)

#### 背景

当前 `/health` 只返回简单的 JSON 状态，无法接入 Prometheus/Grafana 监控体系。生产环境需要实时观测传输速率、错误率、磁盘用量等指标。

#### 目标

- 新增 `GET /metrics` 端点，返回 Prometheus 文本格式（exposition format）
- 指标覆盖：传输吞吐、文件计数、错误计数、磁盘空间、活跃连接数
- 零外部依赖（手动拼接 exposition format），避免引入重量级 crate

#### 设计

**新建 `aerosync-core/src/metrics.rs`**：

```rust
/// 全局指标计数器（AtomicU64，低开销）
pub struct Metrics {
    pub received_files_total: AtomicU64,
    pub received_bytes_total: AtomicU64,
    pub sent_files_total: AtomicU64,
    pub sent_bytes_total: AtomicU64,
    pub transfer_errors_total: AtomicU64,
    pub auth_failures_total: AtomicU64,
    pub active_transfers: AtomicU64,
}

impl Metrics {
    pub fn new() -> Arc<Self>;

    /// 渲染为 Prometheus exposition format（纯文本）
    pub fn render(&self, free_bytes: u64, total_bytes: u64) -> String;
}
```

**`/metrics` 端点输出示例**：

```
# HELP aerosync_received_files_total Total files received
# TYPE aerosync_received_files_total counter
aerosync_received_files_total 42

# HELP aerosync_received_bytes_total Total bytes received
# TYPE aerosync_received_bytes_total counter
aerosync_received_bytes_total 1073741824

# HELP aerosync_active_transfers Current active transfers
# TYPE aerosync_active_transfers gauge
aerosync_active_transfers 3

# HELP aerosync_free_disk_bytes Free disk bytes on receiver
# TYPE aerosync_free_disk_bytes gauge
aerosync_free_disk_bytes 107374182400

# HELP aerosync_transfer_errors_total Total transfer errors
# TYPE aerosync_transfer_errors_total counter
aerosync_transfer_errors_total 2

# HELP aerosync_auth_failures_total Total authentication failures
# TYPE aerosync_auth_failures_total counter
aerosync_auth_failures_total 0
```

**集成点**：
- `ServerConfig` 新增 `enable_metrics: bool`（默认 `true`）
- `FileReceiver::start()` 创建 `Arc<Metrics>` 并注入所有 handler
- `handle_file_upload` 上传成功/失败时更新计数
- `handle_health` 复用 `get_disk_space()` 结果传给 `metrics.render()`

**CLI 用法**：
```bash
# 查看指标（可直接 curl，也可接入 Prometheus scrape）
curl http://192.168.1.10:7788/metrics
```

#### 新增依赖

无（手动生成 exposition format 文本，不引入 `prometheus` crate）。

#### 测试要求

- `test_metrics_render_format`：输出包含所有必要 `# HELP`/`# TYPE` 行
- `test_metrics_counter_increments`：并发递增后值正确
- `test_http_metrics_endpoint_returns_200`：warp 路由测试
- `test_metrics_active_transfers_gauge`：活跃数随任务开始/结束正确变化

---

### 5.2 WebSocket 实时进度推送 (P1)

#### 背景

当前 CLI 轮询 `ProgressMonitor`（100ms 间隔）驱动进度条，远端 UI 工具（如 Web 控制台）无法实时获取传输进度。WebSocket 推送可将进度变化实时推送到任意订阅方。

#### 目标

- 接收端新增 `GET /ws` WebSocket 端点
- 每次传输状态变化时推送 JSON 事件到所有已连接客户端
- 支持多客户端并发订阅（broadcast）
- 连接断开时自动清理，不影响正在进行的传输

#### 设计

**事件格式**：

```json
{
  "event": "transfer_progress",
  "task_id": "uuid",
  "filename": "data.bin",
  "bytes_transferred": 52428800,
  "total_bytes": 157286400,
  "speed_bps": 67108864,
  "status": "in_progress"
}
```

事件类型：
- `transfer_started`：新传输开始
- `transfer_progress`：进度更新（每 500ms 或每 5% 变化推送一次）
- `transfer_completed`：传输完成（含 sha256、duration_ms）
- `transfer_failed`：传输失败（含 error）

**核心实现**（使用 `warp::ws`）：

```rust
// aerosync-core/src/server.rs

// 订阅者广播通道
type WsBroadcast = tokio::sync::broadcast::Sender<WsEvent>;

// 路由
let ws_route = warp::path("ws")
    .and(warp::ws())
    .and(warp::any().map(move || broadcast_tx.clone()))
    .map(|ws: warp::ws::Ws, tx| {
        ws.on_upgrade(move |socket| handle_ws_client(socket, tx))
    });
```

**`ServerConfig` 新增**：
```rust
pub enable_ws: bool,          // 默认 true
pub ws_event_buffer: usize,   // broadcast channel 容量，默认 256
```

**集成点**：
- `FileReceiver` 持有 `broadcast::Sender<WsEvent>`
- `handle_file_upload` 上传进度更新时 `let _ = ws_tx.send(event)`（失败不影响传输）
- `handle_chunk_upload` 每完成一片推送一次进度

**新增依赖**：`warp` 已有 `ws` feature，无需新增。

#### 测试要求

- `test_ws_connect_and_receive_event`：客户端连接后发起上传，收到 `transfer_completed` 事件
- `test_ws_multiple_clients_all_receive`：2 个客户端均收到同一事件
- `test_ws_disconnect_does_not_affect_transfer`：断开 WS 连接后上传继续完成

---

### 5.3 多接收目录路由 (P2)

#### 背景

当前所有文件统一保存到 `ServerConfig.receive_directory`。生产环境中，不同来源的文件（按 sender IP、自定义 tag、文件类型）需要分目录存储，避免混淆。

#### 目标

- 支持按 sender IP 段、请求 header tag 分发到不同子目录
- 规则在配置文件中静态定义（TOML）
- 无匹配规则时回退到默认目录

#### 设计

**配置文件（`~/.aerosync/config.toml`）**：

```toml
[[routing.rules]]
tag = "agent-north"          # 匹配 X-AeroSync-Tag: agent-north
dest = "./received/north"

[[routing.rules]]
ip_prefix = "10.0.1."        # 匹配发送方 IP 前缀
dest = "./received/cluster-1"

[[routing.rules]]
ext = ".log"                 # 匹配文件扩展名
dest = "./received/logs"
```

**核心结构**：

```rust
// aerosync-core/src/routing.rs（新增）

pub struct RoutingRule {
    pub tag: Option<String>,       // X-AeroSync-Tag header 值
    pub ip_prefix: Option<String>, // sender IP 前缀
    pub ext: Option<String>,       // 文件扩展名
    pub dest: PathBuf,
}

pub struct Router {
    rules: Vec<RoutingRule>,
    default_dir: PathBuf,
}

impl Router {
    /// 根据 sender_ip、tag、filename 计算目标目录
    pub fn resolve(&self, sender_ip: &str, tag: Option<&str>, filename: &str) -> PathBuf;
}
```

**集成点**：
- `ServerConfig` 新增 `routing: Option<RouterConfig>`
- `handle_file_upload` 中调用 `router.resolve()` 替换固定的 `receive_directory`
- `handle_chunk_complete` 中同样路由

**CLI 用法**：
```bash
# 发送方附加 tag
aerosync send ./data.bin 192.168.1.10:7788 --tag "agent-north"
# 接收方按 tag 路由到子目录
```

#### 测试要求

- `test_router_match_by_tag`
- `test_router_match_by_ip_prefix`
- `test_router_match_by_ext`
- `test_router_fallback_to_default`
- `test_router_first_rule_wins`（多规则时取第一个匹配）

---

### 5.4 S3 Multipart Upload (P2)

#### 背景

当前 `S3Transfer` 使用单次 `PUT` 上传，AWS S3 对单次 PUT 限制 5GB，大文件传输需要原生 Multipart Upload API。

#### 目标

- 超过阈值（默认 100MB）时自动切换为 S3 Multipart Upload
- 支持断点续传：保存 `UploadId` 到 `ResumeState`，重启后继续上传
- 兼容 AWS S3 和 MinIO

#### 设计

S3 Multipart Upload 三步流程：

```
1. POST   /{bucket}/{key}?uploads           → UploadId
2. PUT    /{bucket}/{key}?partNumber=N&uploadId=X  (× N parts)
3. POST   /{bucket}/{key}?uploadId=X        (CompleteMultipartUpload)
```

**`S3Transfer` 扩展**：

```rust
// aerosync-protocols/src/s3.rs

impl S3Transfer {
    /// 分片上传入口（自动判断是否使用 Multipart）
    pub async fn upload_auto(
        &self,
        file_path: &Path,
        bucket: &str,
        key: &str,
        progress_tx: UnboundedSender<TransferProgress>,
    ) -> Result<()>;

    async fn initiate_multipart(&self, bucket: &str, key: &str) -> Result<String>;  // → UploadId

    async fn upload_part(
        &self, bucket: &str, key: &str, upload_id: &str,
        part_number: u32, data: &[u8],
    ) -> Result<String>;  // → ETag

    async fn complete_multipart(
        &self, bucket: &str, key: &str, upload_id: &str,
        parts: Vec<(u32, String)>,  // (part_number, etag)
    ) -> Result<()>;
}
```

**`S3Config` 新增字段**：
```rust
pub multipart_threshold: u64,  // 默认 100MB
pub part_size: usize,          // 默认 16MB（AWS 最小 5MB）
```

**断点续传**：将 `UploadId` 和已完成的 parts 序列化到 `ResumeState.metadata`。

#### 测试要求（需 MinIO 或 mock）

- `test_s3_multipart_initiate_returns_upload_id`
- `test_s3_upload_part_returns_etag`
- `test_s3_complete_multipart`
- `test_s3_multipart_threshold_triggers_multipart`
- `test_s3_single_put_below_threshold`

---

### 5.5 配置热重载 (P2)

#### 背景

当前修改 `~/.aerosync/config.toml` 需要重启进程才能生效，对长期运行的接收端不友好。

#### 目标

- 接收 `SIGHUP` 信号后重新加载配置文件（Unix 标准热重载信号）
- 仅重载可安全变更的字段：`auth`、`audit_log`、`max_file_size`、`allow_overwrite`、`routing`
- 不可热重载字段（端口、bind 地址）记录警告并忽略变更
- 打印重载日志，方便运维确认

#### 设计

**`FileReceiver` 扩展**：

```rust
impl FileReceiver {
    /// 启动配置热重载监听（后台 task）
    pub fn watch_config_reload(&self, config_path: PathBuf) -> tokio::task::JoinHandle<()>;
}
```

**热重载流程**：

```
SIGHUP
  └─ tokio::signal::unix::signal(SignalKind::hangup())
       └─ AeroSyncConfig::load(config_path)
            └─ 对比新旧配置
                 ├─ 可热重载字段 → self.config.write().await 更新
                 └─ 不可热重载字段变更 → tracing::warn!("port change requires restart")
```

**`main.rs` 集成**：
```rust
// cmd_receive 中
if let Some(ref cfg_path) = config_path {
    let _reload_handle = receiver.watch_config_reload(cfg_path.clone());
}
```

**新增依赖**：`tokio` 已有 `signal` feature（`features = ["full"]`），无需新增。

#### 测试要求

- `test_config_reload_updates_auth`：写入新配置后发送 SIGHUP，验证 auth 变更生效
- `test_config_reload_ignores_port_change`：端口变更不实际生效，有 warn 日志
- `test_config_reload_invalid_toml_keeps_old`：新配置 TOML 解析失败时保留旧配置

---

## 3. 实现顺序与依赖关系

```
Phase 5 实现顺序（建议）

┌─────────────────────────────────────┐
│  并行实现（互不依赖）                  │
│                                     │
│  5.1 Prometheus /metrics  (P1)      │
│  5.2 WebSocket 进度推送   (P1)      │
└───────────────┬─────────────────────┘
                │ 完成后
                ▼
┌─────────────────────────────────────┐
│  并行实现                            │
│                                     │
│  5.3 多目录路由           (P2)      │
│  5.5 配置热重载           (P2)      │
└───────────────┬─────────────────────┘
                │ 完成后
                ▼
┌─────────────────────────────────────┐
│  独立实现                            │
│  5.4 S3 Multipart Upload  (P2)      │
└─────────────────────────────────────┘
```

**依赖说明**：
- 5.1 和 5.2 完全独立，可并行开发
- 5.3 需要 `routing.rs` 新模块，与 5.5 热重载自然结合（路由规则热重载）
- 5.4 完全独立于其他任务，仅改动 `s3.rs`

---

## 4. 验收标准

### 整体标准

- 全量测试继续通过（新增测试后总数 ≥ 220）
- 零编译 warning
- 新功能均有对应单元测试，覆盖正常路径 + 边界条件

### 各任务验收

| 任务 | 验收条件 |
|------|---------|
| 5.1 Prometheus | `curl /metrics` 返回合法 exposition format；Prometheus 可成功 scrape |
| 5.2 WebSocket | `websocat ws://host:7788/ws` 可接收传输事件；多客户端同时订阅不互斥 |
| 5.3 多目录路由 | 按 tag 分发到正确子目录；无匹配时回退默认目录；规则变更热重载生效 |
| 5.4 S3 Multipart | 200MB 文件成功上传 MinIO；与 Phase 2 断点续传兼容 |
| 5.5 配置热重载 | `kill -HUP <pid>` 后 auth token 变更立即生效；端口变更被忽略并打印 warn |

---

*创建时间：2026-04-11*

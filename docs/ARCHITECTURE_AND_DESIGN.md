# AeroSync 架构与功能设计文档

> **版本**：v0.2 设计基准  
> **用途**：DDD 领域设计参考、功能补充规划  
> **目标**：打造跨网络、高效、可靠的 Agent 间文件传输 CLI 工具

---

## 一、项目定位与核心目标

### 1.1 产品定位

AeroSync 是一个**跨网络文件传输 CLI 工具**，专为 Agent-to-Agent 场景设计：

- **主路径**：两端均安装 AeroSync CLI，使用 QUIC 协议高速传输
- **降级路径**：对端无 AeroSync，降级为标准 HTTP / FTP / S3 协议
- **场景特征**：
  - 大文件（GB 级）：分片上传、断点续传、多流并发
  - 小文件批量（千级）：低延迟、高并发、流水线化

### 1.2 核心目标

| 目标 | 指标 |
|------|------|
| 传输速度 | QUIC 模式 ≥ 3x HTTP；千兆网络 ≥ 90% 带宽利用 |
| 可靠性 | 传输成功率 ≥ 99%，支持断点续传 |
| 安全性 | 全程加密（TLS 1.3 / QUIC），Token 认证，SHA-256 完整性校验 |
| 易用性 | 单命令发送/接收，零配置可用，自动协议降级 |
| 可观测 | 实时进度，操作审计日志，健康检查接口 |

---

## 二、现有功能盘点

### 2.1 已实现（可用）

| 功能 | 模块 | 质量 |
|------|------|------|
| 文件元数据读取（大小、权限、修改时间） | `file_manager.rs` | ✅ 完整 |
| 目录扫描与递归列举 | `file_manager.rs` | ✅ 完整 |
| 传输任务抽象（上传/下载任务结构） | `transfer.rs` | ✅ 结构完整 |
| 实时传输进度追踪（速度、ETA、状态） | `progress.rs` | ✅ 完整 |
| 统一错误类型（AeroSyncError） | `error.rs` | ✅ 完整 |
| HTTP 多部分上传（multipart/form-data） | `http.rs` | 🟡 可用但有 OOM 风险 |
| Token 生成/验证（HMAC-SHA256） | `auth/token.rs` | ✅ 完整，18 个测试通过 |
| Token 认证配置（TOML 加载/导出） | `auth/config.rs` | ✅ 完整 |
| HTTP 认证中间件（提取/验证 Bearer Token） | `auth/middleware.rs` | ✅ 已实现，未集成 |
| egui 桌面 GUI（基础界面） | `egui_app.rs` | 🟡 可用，不完整 |
| CLI 交互式菜单 | `cli.rs` | 🟡 基础框架 |

### 2.2 部分实现（有缺陷）

| 功能 | 缺陷 | 影响 |
|------|------|------|
| HTTP 断点续传 | Range 请求未实现 | 大文件中断后从头开始 |
| 文件流式传输 | 整个文件读入内存 | >1GB 文件 OOM 崩溃 |
| QUIC 传输 | 只有客户端，无服务端 | QUIC 无法端到端传输 |
| 服务端文件接收 | server.rs 是骨架，无真实 handler | 接收方无法工作 |
| 传输引擎调度 | transfer.rs 有结构但实际 perform 函数未完成 | 任务提交后无实际传输 |

### 2.3 完全缺失（需新建）

| 功能 | 优先级 | 说明 |
|------|--------|------|
| SHA-256 文件完整性校验 | P0 | 防静默损坏 |
| 审计日志 | P0 | 记录所有操作 |
| TLS 证书管理（自动生成/配置） | P0 | HTTP 明文不可用 |
| Auth 与传输流程集成 | P0 | 现有 Auth 模块孤立 |
| QUIC 服务端实现 | P0 | 高性能模式核心 |
| CLI 发送命令（`aerosync send`） | P0 | CLI 工具的核心入口 |
| CLI 接收命令（`aerosync receive`） | P0 | 接收方入口 |
| 协议自动协商（AeroSync → HTTP 降级） | P1 | 对端无 AeroSync 时降级 |
| 分片上传（文件分块 + 并发） | P1 | 大文件高效传输 |
| 批量传输（目录递归 + 流水线） | P1 | 小文件高并发 |
| 带宽限速 | P1 | 不占满网络 |
| 传输历史持久化 | P1 | 可查记录 |
| 预检验（磁盘空间、网络连通性） | P1 | 避免无效传输 |
| 健康检查端点（`/health`） | P2 | 监控集成 |
| Token 持久化（存储到磁盘） | P1 | 重启后 Token 不丢失 |

---

## 三、现有架构痛点分析

### 3.1 关键安全漏洞

```
// quic.rs - 证书验证完全跳过，生产环境不可用
struct SkipServerVerification;
impl ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(...) -> Result<ServerCertVerified> {
        Ok(ServerCertVerified::assertion())  // 接受任何证书！
    }
}
```

**问题**：任何中间人都可以拦截 QUIC 传输。

### 3.2 内存 OOM 风险

```
// http.rs - 整个文件读入内存
let mut file_contents = Vec::new();
file.read_to_end(&mut file_contents).await?;  // 1GB 文件 = 1GB 内存
```

**问题**：无法传输大文件，是对"高效"目标的根本阻碍。

### 3.3 Auth 模块孤立

- `AuthManager`、`TokenManager`、`AuthMiddleware` 全部实现完毕
- **但没有任何地方调用它们**
- `server.rs` 接收文件时无鉴权，任何人都可以上传

### 3.4 架构耦合问题

```
// ui/lib.rs - AppState 混杂 UI 与业务逻辑
pub struct AppState {
    pub transfer_engine: Arc<TransferEngine>,
    pub config: Arc<RwLock<TransferConfig>>,
    pub file_receiver: Arc<RwLock<FileReceiver>>,    // 业务
    pub server_config: Arc<RwLock<ServerConfig>>,   // 业务
}
```

**问题**：
- CLI/GUI/Headless 模式共用一个 AppState，无法做无头服务
- Config 分散（TransferConfig + ServerConfig），存在同步混乱风险
- UI 层直接持有 FileReceiver，无服务生命周期管理

### 3.5 传输引擎不完整

`transfer.rs` 中 `perform_http_transfer()` / `perform_quic_transfer()` 是骨架函数，未真正连接到 `aerosync-protocols` 的协议实现，任务队列调度存在但实际传输逻辑缺失。

### 3.6 进度监控 O(n) 查找

```
// progress.rs - 每次更新都线性搜索
self.active_transfers.iter_mut().find(|t| t.task_id == task_id)
```

高并发传输时（1000+ 文件）性能会显著下降，应改为 `HashMap<Uuid, TransferProgress>`。

---

## 四、目标架构：DDD 领域设计

### 4.1 领域划分

```
┌─────────────────────────────────────────────────────────────────┐
│                        CLI 接口层                                │
│   aerosync send  /  aerosync receive  /  aerosync status        │
└───────────────────────┬──────────────────────┬──────────────────┘
                        │                      │
           ┌────────────▼────────────┐  ┌──────▼───────────────┐
           │    发送端领域 (Sender)    │  │  接收端领域 (Receiver) │
           │                        │  │                       │
           │ • 文件发现与打包         │  │ • 监听与鉴权           │
           │ • 传输任务规划           │  │ • 文件接收与存储        │
           │ • 协议协商              │  │ • 完整性校验           │
           │ • 分片与并发调度         │  │ • 进度反馈             │
           └────────────┬────────────┘  └──────────────────────┘
                        │
           ┌────────────▼────────────────────────────────────────┐
           │              传输协议领域 (Protocol)                   │
           │                                                      │
           │  ┌──────────────┐  ┌─────────────┐  ┌───────────┐  │
           │  │  QUIC 协议    │  │  HTTP 协议   │  │  S3/FTP   │  │
           │  │ (两端均装)    │  │ (通用降级)   │  │ (对象存储) │  │
           │  └──────────────┘  └─────────────┘  └───────────┘  │
           └─────────────────────────────────────────────────────┘
                        │
           ┌────────────▼────────────────────────────────────────┐
           │              基础设施领域 (Infrastructure)             │
           │                                                      │
           │  • 认证（Auth）     • 文件系统（FileSystem）           │
           │  • 审计日志         • 配置管理                        │
           │  • 证书管理         • 健康检查                        │
           └─────────────────────────────────────────────────────┘
```

### 4.2 DDD 聚合根设计

#### 聚合 1：TransferSession（传输会话）

```
TransferSession [聚合根]
├── id: SessionId
├── source: Endpoint          // 发送方地址
├── destination: Endpoint     // 接收方地址
├── manifest: FileManifest    // 要传输的文件清单
├── protocol: ProtocolKind    // QUIC | HTTP | S3 | FTP
├── status: SessionStatus     // Pending → Active → Completed | Failed
├── tasks: Vec<TransferTask>  // 子任务列表
└── audit_log: Vec<AuditEntry>
```

#### 聚合 2：FileManifest（文件清单）

```
FileManifest [实体]
├── files: Vec<FileEntry>
│   ├── relative_path: PathBuf
│   ├── size: u64
│   ├── sha256: Hash          // 预计算 SHA-256
│   └── chunk_plan: ChunkPlan // 分片方案
└── total_size: u64
```

#### 聚合 3：ReceiverSession（接收会话）

```
ReceiverSession [聚合根]
├── id: SessionId
├── listener: ProtocolListener
├── auth_policy: AuthPolicy
├── storage: StorageBackend   // 本地磁盘 | S3 | ...
└── received_files: Vec<ReceivedFile>
```

#### 值对象

| 值对象 | 字段 |
|--------|------|
| `Endpoint` | host, port, protocol |
| `Hash` | algorithm, hex_value |
| `ChunkPlan` | chunk_size, total_chunks, parallel_count |
| `AuthToken` | value, expires_at, scope |
| `TransferSpeed` | bytes_per_sec, instant, average |

### 4.3 领域事件

```
TransferSessionCreated     // 会话创建
FileManifestBuilt          // 文件清单就绪
ProtocolNegotiated         // 协议协商完成（QUIC / HTTP / 降级）
ChunkTransferStarted       // 分片传输开始
ChunkTransferCompleted     // 分片传输完成
FileIntegrityVerified      // SHA-256 校验通过
FileIntegrityFailed        // SHA-256 校验失败（重传触发器）
TransferSessionCompleted   // 整个会话完成
TransferSessionFailed      // 会话失败（含重试策略）
AuditLogEntry              // 审计事件（所有操作均发布）
```

### 4.4 Crate 重组方案

```
aerosync/
├── src/
│   └── main.rs              # CLI 入口，命令解析
│
├── aerosync-domain/         # 【新建】领域层（纯业务逻辑，无 IO）
│   └── src/
│       ├── session.rs       # TransferSession 聚合
│       ├── manifest.rs      # FileManifest 实体
│       ├── events.rs        # 领域事件定义
│       ├── policy.rs        # 协议选择策略、重试策略
│       └── error.rs         # 领域错误
│
├── aerosync-core/           # 【重构】应用服务层
│   └── src/
│       ├── sender.rs        # 发送端应用服务
│       ├── receiver.rs      # 接收端应用服务
│       ├── progress.rs      # 进度追踪（改 HashMap）
│       ├── file_manager.rs  # 文件扫描、SHA-256 计算
│       ├── auth/            # Auth 模块（集成到流程中）
│       └── audit.rs         # 【新建】审计日志服务
│
├── aerosync-protocols/      # 【重构】协议适配层
│   └── src/
│       ├── traits.rs        # TransferProtocol trait（保留）
│       ├── quic/
│       │   ├── client.rs    # QUIC 客户端（修复证书验证）
│       │   └── server.rs    # 【新建】QUIC 服务端
│       ├── http/
│       │   ├── client.rs    # HTTP 客户端（改流式传输）
│       │   └── server.rs    # HTTP 服务端（集成 Auth）
│       └── s3.rs            # 【新建】S3 兼容适配
│
└── aerosync-infra/          # 【新建】基础设施层
    └── src/
        ├── tls.rs           # TLS 证书自动生成/管理
        ├── config.rs        # 统一配置管理（合并 TransferConfig + ServerConfig）
        ├── storage.rs       # 本地文件存储后端
        └── health.rs        # 健康检查端点
```

---

## 五、CLI 接口设计

### 5.1 命令结构

```
aerosync [全局选项] <子命令> [子命令选项]

全局选项：
  -v, --verbose          详细日志
  --config <path>        指定配置文件（默认 ~/.aerosync/config.toml）
  --token <token>        认证 Token
  --no-color             禁用颜色输出

子命令：
  send       发送文件或目录
  receive    启动接收端监听
  token      Token 管理
  status     查看传输状态
  history    查看传输历史
  config     配置管理
```

### 5.2 核心命令详情

#### `aerosync send`

```
aerosync send <源路径> <目标地址> [选项]

示例：
  aerosync send ./data agent2.local:8080
  aerosync send ./data s3://bucket/path --protocol s3
  aerosync send ./bigfile.tar host:8080 --chunk-size 64M --parallel 8
  aerosync send ./dir/ host:8080 --recursive --compress

参数：
  <源路径>               文件或目录路径
  <目标地址>             host:port 或 protocol://... 格式

选项：
  --protocol <p>         强制指定协议：quic | http | s3 | ftp（默认：自动）
  -r, --recursive        递归传输目录
  --chunk-size <size>    分片大小（默认：4MB，大文件自动调整）
  --parallel <n>         并发分片数（默认：4）
  --bandwidth <rate>     限速，如 10MB/s
  --compress             传输前压缩
  --verify               传输后校验 SHA-256（默认：开启）
  --resume               断点续传（默认：开启）
  --timeout <secs>       超时时间
  --dry-run              预检：不实际传输，只显示计划
```

#### `aerosync receive`

```
aerosync receive [选项]

示例：
  aerosync receive
  aerosync receive --port 8080 --save-to ./downloads
  aerosync receive --enable-quic --one-shot

选项：
  --port <port>          HTTP 监听端口（默认：7788）
  --quic-port <port>     QUIC 监听端口（默认：7789）
  --save-to <dir>        文件保存目录（默认：./received）
  --enable-quic          启用 QUIC 接收（默认：自动）
  --auth-token <token>   要求发送方携带此 Token
  --max-size <size>      最大单文件大小
  --one-shot             接收一次后自动退出
  --overwrite            允许覆盖同名文件
```

#### `aerosync token`

```
aerosync token <子命令>

子命令：
  generate               生成新 Token
  list                   列出所有有效 Token
  revoke <token-id>      吊销指定 Token
  verify <token>         验证 Token 是否有效
```

### 5.3 协议自动协商流程

```
发送方启动
    │
    ▼
尝试连接 <host>:7789 (QUIC)
    │
    ├── 成功 ──► 使用 QUIC 协议（高性能模式）
    │
    └── 失败 ──► 尝试 HTTP <host>:7788
                    │
                    ├── 成功 ──► 使用 HTTP 协议（降级模式）
                    │
                    └── 失败 ──► 使用用户指定的 --protocol
                                （S3 / FTP / 报错）
```

---

## 六、关键技术方案

### 6.1 大文件传输：分片 + 并发

```
文件 (10GB)
    │
    ▼ 切片
┌──────┬──────┬──────┬──────┐
│ 0-4M │ 4-8M │ 8-12M│ ...  │  每片 4MB（可配置）
└──┬───┴──┬───┴──┬───┴──────┘
   │      │      │
   ▼      ▼      ▼
 Worker  Worker  Worker      并发 4 个流（QUIC 多路复用）
   │      │      │
   └──────┴──────┘
           │
           ▼
      接收方合并 + SHA-256 校验
```

**实现要点**：
- 使用 `tokio::fs::File` + `seek()` 实现无拷贝分片读取
- QUIC 天然支持多路复用，每个分片一个 stream
- HTTP 使用 chunked transfer encoding + Content-Range
- 每片有独立 SHA-256，最终全文件再校验一次

### 6.2 小文件批量：流水线化

```
文件列表 [f1, f2, f3, ... f1000]
    │
    ▼ 生产者
┌─────────────────┐
│   FileScanner   │  扫描目录，生成文件清单
└────────┬────────┘
         │ channel
         ▼ 消费者池
┌──┐ ┌──┐ ├──┐ ┌──┐
│W1│ │W2│ │W3│ │W4│  16 个并发 Worker
└──┘ └──┘ └──┘ └──┘
         │
         ▼
   进度聚合器（单个 Receiver）
         │
         ▼
   实时进度输出到 CLI
```

**实现要点**：
- `tokio::sync::mpsc` 流水线，避免等待
- 连接池复用（QUIC 连接复用多个 stream，避免握手开销）
- 批量传输用 `--parallel 16` 控制并发度

### 6.3 安全传输链路

```
发送方                               接收方
  │                                    │
  │  1. aerosync receive --auth-token  │
  │                                    │◄── 生成 TLS 证书，监听
  │                                    │
  │  2. aerosync send ./file host:port │
  │     --token <token>                │
  │                                    │
  ├──── TLS 握手 / QUIC 握手 ─────────►│
  │                                    │
  ├──── Token 验证请求 ────────────────►│
  │                                    │◄── IP 白名单检查
  │                                    │◄── Token HMAC 验证
  │                                    │
  ├──── 文件清单 + 预计算 SHA-256 ──────►│
  │                                    │◄── 磁盘空间预检
  │                                    │
  ├──── 分片数据流 ─────────────────────►│
  │                                    │◄── 写入临时文件
  │                                    │
  ├──── 传输完成信号 ───────────────────►│
  │                                    │◄── SHA-256 校验
  │◄─── 校验结果（OK / RETRY_CHUNKS） ──┤
  │                                    │◄── 写入最终路径
  │                                    │◄── 审计日志记录
```

### 6.4 断点续传机制

```
传输中断
    │
    ▼
本地保存 resume.json
{
  "session_id": "uuid",
  "destination": "host:port",
  "file_path": "./bigfile.tar",
  "sha256": "abc123...",
  "chunks_completed": [0,1,2,5],  // 已完成的分片索引
  "timestamp": 1234567890
}
    │
    ▼ 重新运行 aerosync send（自动检测）
    │
    ▼
读取 resume.json，跳过已完成分片
只传输剩余分片
```

---

## 七、重构优先级路线图

### Phase 1：修复核心缺陷（使 CLI 工具可用）

| 任务 | 模块 | 说明 |
|------|------|------|
| 实现 `aerosync send` 命令 | `src/main.rs` | clap 命令解析 |
| 实现 `aerosync receive` 命令 | `src/main.rs` | 启动监听服务 |
| 修复 HTTP 流式传输（去除整文件读取） | `protocols/http.rs` | 改用 `reqwest::Body::wrap_stream` |
| 完成 HTTP 服务端接收逻辑 | `core/server.rs` | warp 路由 + 文件写入 |
| 集成 Auth 到 HTTP 服务端 | `core/server.rs` | 调用已有 AuthMiddleware |
| 实现 SHA-256 校验 | `core/file_manager.rs` | 计算 + 对比 |
| 修复 QUIC 证书验证 | `protocols/quic.rs` | 移除 SkipServerVerification |
| 进度监控改 HashMap | `core/progress.rs` | O(1) 查找 |

### Phase 2：高性能功能

| 任务 | 模块 | 说明 |
|------|------|------|
| 实现 QUIC 服务端 | `protocols/quic/server.rs` | quinn ServerConfig |
| 分片上传（大文件） | `protocols/http.rs` + `quic/client.rs` | 并发多流 |
| 批量小文件流水线 | `core/sender.rs` | mpsc + worker pool |
| 协议自动协商 | `core/sender.rs` | 探测 QUIC → 降级 HTTP |
| 断点续传 | `core/session.rs` | 本地 resume.json |
| 带宽限速 | `protocols/` | token bucket 算法 |

### Phase 3：可靠性与可观测

| 任务 | 模块 | 说明 |
|------|------|------|
| 审计日志 | `core/audit.rs` | tracing + 文件追加 |
| 传输历史持久化 | `core/history.rs` | SQLite 或 TOML 文件 |
| 预检验（磁盘空间/网络） | `core/sender.rs` | 传输前 pre-flight |
| Token 持久化 | `core/auth/token.rs` | 写入 ~/.aerosync/tokens |
| 健康检查端点 | `core/server.rs` | GET /health |
| 统一配置管理 | `aerosync-infra/config.rs` | 合并两个 Config 结构 |

---

## 八、数据流全景图

```
CLI 用户 / Agent
      │
      │ aerosync send ./files remote:7789
      ▼
┌─────────────────────────────────────────┐
│              CLI 层 (main.rs)            │
│  • 解析命令行参数 (clap)                  │
│  • 加载配置文件                           │
│  • 构建 SendContext                      │
└──────────────────┬──────────────────────┘
                   │
                   ▼
┌─────────────────────────────────────────┐
│         应用服务层 (core/sender.rs)       │
│  1. 文件扫描 → FileManifest              │
│  2. SHA-256 预计算                       │
│  3. 预检验（磁盘/网络）                   │
│  4. 协议协商                             │
│  5. 创建 TransferSession                 │
│  6. 分片规划                             │
└──────────────────┬──────────────────────┘
                   │
                   ▼
┌─────────────────────────────────────────┐
│         协议层 (protocols/)              │
│                                         │
│  ┌─────────────┐   ┌──────────────────┐ │
│  │ QuicTransfer │   │  HttpTransfer    │ │
│  │ • 多路复用   │   │  • 流式分块传输  │ │
│  │ • 内置加密   │   │  • Range 续传    │ │
│  └─────────────┘   └──────────────────┘ │
└──────────────────┬──────────────────────┘
                   │ 网络
                   ▼
┌─────────────────────────────────────────┐
│         应用服务层 (core/receiver.rs)     │
│  1. Token 鉴权                          │
│  2. 接收分片 → 临时文件                  │
│  3. SHA-256 校验                        │
│  4. 合并 → 最终路径                     │
│  5. 审计日志                            │
└─────────────────────────────────────────┘
```

---

## 九、配置文件设计

```toml
# ~/.aerosync/config.toml

[transfer]
chunk_size = "4MB"           # 分片大小
parallel_streams = 4         # 并发流数量
retry_attempts = 3           # 失败重试次数
timeout_seconds = 60         # 单片超时
bandwidth_limit = ""         # 限速，如 "10MB/s"，空表示不限

[server]
http_port = 7788
quic_port = 7789
bind_address = "0.0.0.0"
receive_directory = "./received"
max_file_size = "100GB"
allow_overwrite = false

[auth]
enable_auth = true
secret_key = ""              # 留空则自动生成
token_lifetime_hours = 24
allowed_ips = []             # 空表示不限制 IP

[tls]
auto_generate = true         # 自动生成自签名证书
cert_path = ""               # 自定义证书路径
key_path = ""

[logging]
audit_log = "~/.aerosync/audit.log"
level = "info"               # debug | info | warn | error
```

---

## 十、与 Agent 集成的使用场景

### 场景 1：Agent 发送文件给另一个 Agent

```bash
# 接收方 Agent 启动监听（一次性）
aerosync receive --one-shot --save-to /tmp/data --auth-token $(aerosync token generate)

# 发送方 Agent 发送
aerosync send ./output/ receiver-host:7789 --token <token> --recursive
```

### 场景 2：Agent 发送到 S3（对端无 AeroSync）

```bash
aerosync send ./output/ s3://my-bucket/agent-data/ \
  --protocol s3 \
  --s3-region us-east-1 \
  --s3-access-key $AWS_KEY \
  --s3-secret-key $AWS_SECRET
```

### 场景 3：脚本化批量传输（机器可读输出）

```bash
aerosync send ./data/ host:7789 --json-output | jq '.transfer_id'
# 输出：{"transfer_id":"uuid","status":"completed","bytes":1048576,"sha256":"abc..."}
```

### 场景 4：大文件传输（断点续传）

```bash
# 首次传输中断后，再次运行同一命令自动续传
aerosync send ./bigfile.tar.gz host:7789 --chunk-size 64M --parallel 8
# 自动检测 ~/.aerosync/resume/<session-id>.json 并从断点继续
```

---

## 十一、现有代码可复用清单

| 现有代码 | 位置 | 复用策略 |
|---------|------|---------|
| `AeroSyncError` 错误类型 | `core/error.rs` | 直接复用，补充新变体 |
| `TransferProgress` / `TransferStats` | `core/progress.rs` | 复用结构，改 HashMap 存储 |
| `FileInfo` / `FileManager` | `core/file_manager.rs` | 复用，增加 SHA-256 计算方法 |
| `TokenManager` + HMAC-SHA256 | `core/auth/token.rs` | 完整复用，增加磁盘持久化 |
| `AuthConfig` + TOML 加载 | `core/auth/config.rs` | 完整复用，合并进统一 Config |
| `AuthMiddleware` | `core/auth/middleware.rs` | 复用，真正集成到 server 路由 |
| `TransferProtocol` trait | `protocols/traits.rs` | 完整复用 |
| `HttpTransfer` 上传逻辑 | `protocols/http.rs` | 改造为流式传输后复用 |
| `ServerConfig` 结构 | `core/server.rs` | 复用字段，合并到统一 Config |

---

## 十二、需要新建的模块

| 模块 | 路径 | 核心职责 |
|------|------|---------|
| CLI 命令解析 | `src/main.rs` | clap v4，`send` / `receive` / `token` / `status` |
| 发送端应用服务 | `aerosync-core/src/sender.rs` | 文件扫描、协议协商、任务调度 |
| 接收端应用服务 | `aerosync-core/src/receiver.rs` | 监听、鉴权、文件写入、校验 |
| 审计日志 | `aerosync-core/src/audit.rs` | 结构化日志追加到文件 |
| 传输历史 | `aerosync-core/src/history.rs` | 持久化传输记录（SQLite / TOML） |
| QUIC 服务端 | `aerosync-protocols/src/quic/server.rs` | quinn 服务端监听 |
| HTTP 流式客户端 | `aerosync-protocols/src/http/client.rs` | reqwest streaming body |
| S3 适配器 | `aerosync-protocols/src/s3.rs` | aws-sdk-s3 封装 |
| TLS 管理 | `aerosync-infra/src/tls.rs` | rcgen 自动证书生成 |
| 统一配置 | `aerosync-infra/src/config.rs` | 合并所有配置，TOML 读写 |
| 领域聚合 | `aerosync-domain/src/session.rs` | TransferSession 聚合根 |
| 文件清单 | `aerosync-domain/src/manifest.rs` | FileManifest + ChunkPlan |
| 领域事件 | `aerosync-domain/src/events.rs` | 传输生命周期事件定义 |

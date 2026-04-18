# AeroSync MCP Server 集成指南

AeroSync 提供一个符合 [Model Context Protocol (MCP)](https://modelcontextprotocol.io) 规范的服务器，让 Claude、Cursor、Continue 等 AI 工具可以直接调用 AeroSync 的文件传输能力。

## 安装

```bash
# 从源码构建
git clone https://github.com/your-org/aerosync
cd aerosync
cargo build --release -p aerosync-mcp

# 二进制位于
./target/release/aerosync-mcp
```

## 配置

### Claude Desktop

编辑 `~/Library/Application Support/Claude/claude_desktop_config.json`：

```json
{
  "mcpServers": {
    "aerosync": {
      "command": "/path/to/aerosync-mcp",
      "args": []
    }
  }
}
```

重启 Claude Desktop 后，在对话中即可使用 AeroSync 工具。

### Cursor

在项目根目录创建 `.cursor/mcp.json`：

```json
{
  "mcpServers": {
    "aerosync": {
      "command": "/path/to/aerosync-mcp",
      "args": []
    }
  }
}
```

### Continue (VS Code)

在 `~/.continue/config.json` 中添加：

```json
{
  "mcpServers": [
    {
      "name": "aerosync",
      "command": "/path/to/aerosync-mcp",
      "args": []
    }
  ]
}
```

### 其他支持 MCP stdio 的客户端

AeroSync MCP Server 使用标准 stdio 传输，符合 MCP 2024-11-05 规范。任何支持该规范的客户端均可直接集成：

```
command: aerosync-mcp
transport: stdio
```

## 可用工具

| 工具名 | 说明 |
|--------|------|
| `send_file` | 发送单个文件到远端（自动协商 QUIC/HTTP） |
| `send_directory` | 递归发送整个目录，保留目录结构 |
| `start_receiver` | 启动文件接收服务器（后台运行） |
| `stop_receiver` | 停止接收服务器 |
| `get_receiver_status` | 查看接收服务器状态及已接收文件 |
| `list_history` | 查询传输历史记录 |
| `discover_receivers` | 通过 mDNS 扫描局域网内的 AeroSync 节点 |

## 使用示例

以下是 Claude 对话中的典型用法：

**发送文件：**
> "帮我把 /tmp/report.pdf 发送到 192.168.1.100:7788"

Claude 将调用 `send_file`，自动探测对端是否为 AeroSync（优先 QUIC），完成传输后返回速度和状态。

**启动接收端：**
> "在本机 8080 端口启动一个接收服务器，保存到 ~/downloads"

Claude 将调用 `start_receiver`，服务器在后台运行，直到你请求停止。

**发现局域网节点：**
> "帮我找一下局域网里有哪些 AeroSync 节点"

Claude 将调用 `discover_receivers`，通过 mDNS 扫描返回节点列表（含地址、端口、是否需要认证）。

## 工具参数详情

### send_file

```json
{
  "source": "/path/to/file.zip",
  "destination": "192.168.1.100:7788",
  "token": "optional-auth-token",
  "no_verify": false,
  "limit": "10MB"
}
```

- `destination` 支持多种格式：
  - `host:port` — 自动协商（优先 QUIC）
  - `http://host:port/upload` — 强制 HTTP
  - `quic://host:port/upload` — 强制 QUIC
  - `s3://bucket/key` — 上传到 S3/MinIO
  - `ftp://host:port/path` — FTP 上传
- `limit` 格式：`"10MB"`、`"512KB"`、`"1GB"`

### send_directory

```json
{
  "source": "/path/to/dir",
  "destination": "192.168.1.100:7788",
  "token": "optional-auth-token",
  "no_verify": false
}
```

### start_receiver

```json
{
  "port": 7788,
  "quic_port": 7789,
  "save_to": "./received",
  "auth_token": "secret",
  "overwrite": false
}
```

### list_history

```json
{
  "limit": 20,
  "success_only": false,
  "sent": false,
  "received": false
}
```

### discover_receivers

```json
{
  "timeout": 3
}
```

## 传输协议协商

`send_file` 和 `send_directory` 在目标地址不含协议前缀时，会自动探测：

1. 发送 HTTP 探针到 `http://<dest>/health`
2. 检查响应头 `x-aerosync: true`
3. 若确认为 AeroSync 节点 → 升级为 QUIC（速度更快，内置 TLS）
4. 否则 → 降级为普通 HTTP

## 日志调试

MCP Server 的日志写入 **stderr**（不污染 stdout 的 JSON-RPC 通道）。
调试时可以重定向查看：

```bash
aerosync-mcp 2>mcp.log
```

或设置日志级别：

```bash
RUST_LOG=debug aerosync-mcp
```

## 安全注意事项

### 两种 Token 的区别（务必区分）

AeroSync MCP 涉及**两种语义完全不同的 Token**，不要混淆：

| 字段名 | 作用层 | 谁来校验 | 何时必须 |
|--------|--------|----------|----------|
| `token` / `auth_token` | **远端接收端** | 接收端服务器（HMAC-SHA256） | 当对端启用认证时 |
| `mcp_auth_token` | **本地 MCP 通道** | MCP Server 自身 | 当本机设置 `AEROSYNC_MCP_SECRET` 时 |

- `token`（在 `send_file` / `send_directory` 中）和 `auth_token`（在 `start_receiver` 中）走网络发到对端
- `mcp_auth_token` **不出本机**，仅用于阻止恶意进程通过 stdio 调用 MCP Server

### 启用 MCP 本地认证

在启动 MCP Server 前设置环境变量：

```bash
export AEROSYNC_MCP_SECRET="$(openssl rand -hex 32)"
aerosync-mcp
```

之后每个工具调用必须带 `mcp_auth_token`：

```json
{
  "source": "/tmp/x.zip",
  "destination": "host:7788",
  "mcp_auth_token": "<上面生成的密钥>"
}
```

> **向后兼容**：旧客户端使用的字段名 `_auth_token` 通过 `serde(alias)` 仍可识别，但新代码请使用 `mcp_auth_token`。

### 其他

- `auth_token` 参数使用 HMAC-SHA256 认证，建议生产环境始终启用
- MCP Server 本身运行在本地（stdio），不暴露网络端口
- 接收端（`start_receiver`）会监听 `0.0.0.0`，生产部署时建议配置防火墙或使用 `auth_token`

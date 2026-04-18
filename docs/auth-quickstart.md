# Token 认证快速开始指南

> AeroSync v0.2 新功能：Token 认证系统

## 🎯 概述

Token 认证是 AeroSync 的核心安全功能，可以防止未授权访问文件传输服务。

**核心特性：**
- ✅ 安全的 Token 生成和验证
- ✅ 自动过期机制
- ✅ Token 撤销支持
- ✅ HTTP 和 QUIC 协议集成
- ✅ 零配置快速启动

---

## 🚀 快速开始

### 1. 启用认证

在配置文件 `~/.aerosync/config.toml` 中启用认证：

```toml
[security]
enable_auth = true
secret_key = "your-secret-key-at-least-16-characters"
token_lifetime_hours = 24
```

### 2. 生成 Token

使用 CLI 生成 Token：

```bash
# 生成新 Token
aerosync auth generate-token

# 输出示例：
# Token: a1b2c3d4-e5f6-7890-1234-567890abcdef.1703001234.1703087634.f1a2b3c4d5e6f7g8
# 有效期: 24 小时
# 过期时间: 2024-12-28 10:30:00 UTC
```

### 3. 使用 Token 上传文件

**方式 1: HTTP 请求**

```bash
# 使用 Bearer Token
curl -X POST \
  -H "Authorization: Bearer YOUR_TOKEN_HERE" \
  -F "file=@myfile.txt" \
  http://localhost:8080/upload
```

**方式 2: AeroSync CLI**

```bash
# 设置 Token 环境变量
export AEROSYNC_TOKEN="your-token-here"

# 上传文件
aerosync send myfile.txt http://server:8080/upload
```

**方式 3: AeroSync GUI**

1. 打开设置面板
2. 输入 Token
3. 开始传输

---

## 📖 完整文档

### 生成强随机密钥

```bash
# Linux/macOS
openssl rand -hex 32

# 或使用 UUID
python3 -c "import uuid; print(f'{uuid.uuid4()}-{uuid.uuid4()}')"
```

### Token 管理命令

```bash
# 生成 Token
aerosync auth generate-token

# 列出所有活跃 Token
aerosync auth list-tokens

# 撤销 Token
aerosync auth revoke-token <TOKEN>

# 清理过期 Token
aerosync auth cleanup
```

### 验证 Token

```bash
# 验证 Token 是否有效
aerosync auth verify-token <TOKEN>

# 输出示例：
# ✓ Token 有效
# 创建时间: 2024-12-27 10:00:00 UTC
# 过期时间: 2024-12-28 10:00:00 UTC
# 剩余时间: 23 小时 45 分钟
```

---

## 💻 代码示例

### Rust 代码

```rust
use aerosync::core::auth::{AuthConfig, AuthManager};

// 创建认证管理器
let config = AuthConfig::new()
    .enable_auth()
    .with_secret_key("my-secret-key".to_string())
    .with_token_lifetime(24);

let auth_manager = AuthManager::new(config)?;

// 生成 Token
let token = auth_manager.generate_token()?;
println!("Token: {}", token);

// 验证请求
let is_authenticated = auth_manager.authenticate(
    Some(&token),
    "192.168.1.100"
)?;
```

### Python 客户端示例

```python
import requests

# 配置
SERVER_URL = "http://localhost:8080/upload"
TOKEN = "your-token-here"

# 上传文件
with open("myfile.txt", "rb") as f:
    response = requests.post(
        SERVER_URL,
        files={"file": f},
        headers={"Authorization": f"Bearer {TOKEN}"}
    )

if response.status_code == 200:
    print("✓ 文件上传成功")
else:
    print(f"✗ 上传失败: {response.status_code}")
```

### JavaScript/Node.js 客户端示例

```javascript
const axios = require('axios');
const FormData = require('form-data');
const fs = require('fs');

const SERVER_URL = 'http://localhost:8080/upload';
const TOKEN = 'your-token-here';

const form = new FormData();
form.append('file', fs.createReadStream('myfile.txt'));

axios.post(SERVER_URL, form, {
  headers: {
    ...form.getHeaders(),
    'Authorization': `Bearer ${TOKEN}`
  }
})
.then(response => {
  console.log('✓ 文件上传成功');
})
.catch(error => {
  console.error('✗ 上传失败:', error.response.status);
});
```

---

## 🔒 安全最佳实践

### 1. 密钥管理

**✅ 推荐做法：**
- 使用至少 32 个字符的强随机密钥
- 不要硬编码在代码中
- 使用环境变量或密钥管理服务
- 定期轮换密钥（建议每 3-6 个月）

**❌ 不要这样做：**
```rust
// 不要直接硬编码
let secret = "123456"; // 太弱
let secret = "my-password"; // 可预测
```

**✅ 推荐这样做：**
```rust
// 从环境变量读取
let secret = std::env::var("AEROSYNC_SECRET_KEY")?;

// 或从配置文件读取
let secret = config.security.secret_key;
```

### 2. Token 生命周期

根据安全需求选择合适的生命周期：

| 场景 | 推荐生命周期 | 说明 |
|------|-------------|------|
| 开发环境 | 24 小时 | 方便调试 |
| 测试环境 | 12 小时 | 平衡安全和便利 |
| 生产环境 | 1-8 小时 | 根据实际需求 |
| 高安全场景 | 1 小时 | 最小化风险窗口 |
| 临时访问 | 30 分钟 | 一次性操作 |

### 3. 传输安全

**必须使用 HTTPS/TLS**：

```toml
[server]
# 强制使用 HTTPS
force_https = true
tls_cert = "/path/to/cert.pem"
tls_key = "/path/to/key.pem"
```

**为什么？**
- Token 在 HTTP 明文传输会被窃听
- HTTPS 提供端到端加密
- 防止中间人攻击

### 4. IP 限制

结合 IP 白名单提升安全性：

```toml
[security]
enable_auth = true
allowed_ips = [
    "192.168.1.0/24",  # 内网
    "10.0.0.0/8",      # VPN
    "203.0.113.42"     # 特定公网 IP
]
```

---

## ⚠️ 常见问题

### Q1: Token 验证失败怎么办？

**可能原因：**
1. Token 已过期
2. Token 被撤销
3. 密钥不匹配（服务端重启后更换了密钥）
4. Token 格式错误

**解决方法：**
```bash
# 1. 检查 Token 是否有效
aerosync auth verify-token <YOUR_TOKEN>

# 2. 生成新的 Token
aerosync auth generate-token

# 3. 检查服务端日志
tail -f /var/log/aerosync.log
```

### Q2: 如何在团队中分发 Token？

**推荐方式：**
1. **为每个用户生成独立 Token**
   ```bash
   aerosync auth generate-token --user alice
   aerosync auth generate-token --user bob
   ```

2. **通过安全渠道分发**
   - 使用密码管理器（1Password、LastPass）
   - 通过加密消息发送
   - 避免通过邮件或明文聊天工具

3. **定期轮换**
   ```bash
   # 撤销旧 Token
   aerosync auth revoke-token <OLD_TOKEN>
   
   # 生成新 Token
   aerosync auth generate-token --user alice
   ```

### Q3: Token 泄露了怎么办？

**立即行动：**
```bash
# 1. 立即撤销泄露的 Token
aerosync auth revoke-token <LEAKED_TOKEN>

# 2. 检查访问日志
aerosync auth audit-log --token <LEAKED_TOKEN>

# 3. 生成新 Token
aerosync auth generate-token

# 4. 如果怀疑密钥泄露，轮换密钥
aerosync auth rotate-secret-key
```

---

## 📊 监控和审计

### 启用审计日志

```toml
[security]
enable_audit_log = true
audit_log_file = "/var/log/aerosync-audit.log"
```

### 查看认证统计

```bash
# 认证成功率
aerosync auth stats --metric success-rate

# 最近的认证失败
aerosync auth logs --failed --last 24h

# Token 使用情况
aerosync auth stats --metric token-usage
```

### 告警配置

```toml
[security.alerts]
# 认证失败超过阈值时告警
failed_auth_threshold = 10
failed_auth_window_minutes = 5

# Webhook 通知
alert_webhook = "https://your-webhook-url.com"
```

---

## 🔄 升级到 v0.2

如果您从 v0.1 升级，认证默认是**禁用**的，不会影响现有功能。

**启用认证的步骤：**

1. **备份配置**
   ```bash
   cp ~/.aerosync/config.toml ~/.aerosync/config.toml.backup
   ```

2. **生成密钥**
   ```bash
   openssl rand -hex 32 > ~/.aerosync/secret.key
   ```

3. **更新配置**
   ```toml
   [security]
   enable_auth = true
   secret_key_file = "~/.aerosync/secret.key"
   token_lifetime_hours = 24
   ```

4. **重启服务**
   ```bash
   sudo systemctl restart aerosync
   ```

5. **生成 Token 给客户端**
   ```bash
   aerosync auth generate-token
   ```

---

## 📚 更多资源

- [完整技术文档](docs/auth-token-design.md)
- [API 参考](docs/api.md#authentication)
- [安全指南](docs/security.md)
- [示例代码](examples/auth_token_example.rs)

---

**需要帮助？**
- 📧 邮件：security@aerosync.dev
- 💬 Discord：[加入社区]
- 🐛 问题反馈：[GitHub Issues]

---

*最后更新：2025-12-27 | AeroSync v0.2*


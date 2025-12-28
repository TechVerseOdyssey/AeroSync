# Token 认证模块技术设计文档

## 1. 概述

本文档描述 AeroSync 项目 Token 认证机制的详细技术设计。

### 1.1 目标

- 实现安全的 Token 生成和验证机制
- 防止未授权访问文件传输服务
- 支持 Token 过期和撤销
- 易于集成到 HTTP 和 QUIC 协议中

### 1.2 非目标

- 用户名/密码认证（v0.2 Phase 2）
- IP 访问控制（v0.2 Phase 2）
- OAuth2/OIDC 集成（v0.5）

## 2. 架构设计

### 2.1 模块结构

```
aerosync-core/src/auth/
├── mod.rs          # 认证管理器（AuthManager）
├── token.rs        # Token 生成和验证（TokenManager）
├── config.rs       # 认证配置（AuthConfig）
└── middleware.rs   # HTTP/QUIC 中间件（AuthMiddleware）
```

### 2.2 核心组件

#### Auth Manager
- 统一认证入口
- 协调 Token 验证和 IP 验证
- 提供简单的 API 给上层使用

#### Token Manager
- 生成加密安全的 Token
- 验证 Token 的有效性
- 管理 Token 生命周期
- 支持 Token 撤销

#### Auth Config
- 管理认证配置参数
- 支持从 TOML 文件加载
- 配置验证

#### Auth Middleware
- HTTP/QUIC 请求拦截
- Token 提取和验证
- 返回 401 Unauthorized 响应

## 3. Token 设计

### 3.1 Token 格式

```
Token = {uuid}.{timestamp}.{expires}.{signature}
```

**组成部分：**
- `uuid`: 唯一标识符（UUID v4）
- `timestamp`: 创建时间（Unix 时间戳）
- `expires`: 过期时间（Unix 时间戳）
- `signature`: HMAC-SHA256 签名（前 16 个字符）

**示例：**
```
a1b2c3d4-e5f6-7890-1234-567890abcdef.1703001234.1703087634.f1a2b3c4d5e6f7g8
```

### 3.2 签名算法

使用 HMAC-SHA256 对 Token 进行签名：

```
data = "{uuid}.{timestamp}.{expires}.{secret_key}"
signature = SHA256(data)[0:16]
```

### 3.3 安全性考虑

1. **不可伪造**：没有密钥无法生成有效签名
2. **防重放**：每个 Token 包含唯一 UUID
3. **有时效性**：自动过期机制
4. **可撤销**：服务端可主动撤销 Token
5. **不泄露信息**：Token 不包含敏感用户信息

## 4. API 设计

### 4.1 认证管理器 API

```rust
// 创建认证管理器
let config = AuthConfig::new()
    .enable_auth()
    .with_secret_key("my-secret-key".to_string());
let auth_manager = AuthManager::new(config)?;

// 生成 Token
let token = auth_manager.generate_token()?;

// 验证请求
let is_auth = auth_manager.authenticate(
    Some(&token),
    "192.168.1.100"
)?;
```

### 4.2 Token 管理器 API

```rust
// 创建 Token 管理器
let manager = TokenManager::new("secret-key".to_string())?
    .with_lifetime(24); // 24 小时

// 生成 Token
let token = manager.generate_token()?;

// 验证 Token
let is_valid = manager.verify_token(&token)?;

// 撤销 Token
manager.revoke_token(&token)?;

// 清理过期 Token
manager.cleanup_expired_tokens();
```

### 4.3 中间件 API

```rust
// 创建中间件
let middleware = AuthMiddleware::new(Arc::new(auth_manager));

// HTTP 请求认证
let is_auth = middleware.authenticate_http_request(
    Some("Bearer abc123xyz"),
    "192.168.1.100"
)?;

// QUIC 连接认证
let is_auth = middleware.authenticate_quic_connection(
    Some(token_bytes),
    "192.168.1.100"
)?;

// 生成 401 响应
let response = middleware.unauthorized_response();
```

## 5. 配置格式

### 5.1 TOML 配置

```toml
[security]
enable_auth = true
secret_key = "your-secret-key-at-least-16-chars"
token_lifetime_hours = 24
allowed_ips = ["192.168.1.0/24", "10.0.0.0/8"]
```

### 5.2 配置验证规则

- `enable_auth = true` 时，`secret_key` 不能为空
- `secret_key` 长度至少 16 个字符
- `token_lifetime_hours` 必须大于 0
- `allowed_ips` 必须是有效的 CIDR 格式（待实现）

## 6. HTTP 集成

### 6.1 请求流程

```
客户端请求
    ↓
提取 Authorization Header
    ↓
验证 Token
    ├─ 有效 → 继续处理请求
    └─ 无效 → 返回 401 Unauthorized
```

### 6.2 Header 格式

支持两种格式：

1. **Bearer Token**（推荐）：
   ```
   Authorization: Bearer a1b2c3d4.1703001234.1703087634.f1a2b3c4
   ```

2. **直接 Token**：
   ```
   X-Auth-Token: a1b2c3d4.1703001234.1703087634.f1a2b3c4
   ```

### 6.3 错误响应

```http
HTTP/1.1 401 Unauthorized
WWW-Authenticate: Bearer
Content-Type: application/json

{
  "error": "unauthorized",
  "message": "Invalid or missing authentication token"
}
```

## 7. QUIC 集成

### 7.1 认证流程

```
客户端连接
    ↓
在握手数据中携带 Token
    ↓
服务端验证 Token
    ├─ 有效 → 接受连接
    └─ 无效 → 拒绝连接
```

### 7.2 Token 传递

在 QUIC 连接的 `transport_parameters` 中携带 Token：

```rust
connection.set_transport_parameters(token.as_bytes());
```

## 8. 性能考虑

### 8.1 Token 存储

- 使用 `HashMap` 存储活跃 Token
- 使用 `RwLock` 保证线程安全
- 读多写少，适合 `RwLock`

### 8.2 签名性能

- SHA-256 哈希速度：~500 MB/s
- 单次签名耗时：< 1μs
- 对性能影响可忽略不计

### 8.3 清理策略

- 定期清理过期 Token（每小时）
- 懒惰清理：验证时发现过期自动删除
- 避免内存泄漏

## 9. 测试策略

### 9.1 单元测试

- Token 生成格式正确
- 有效 Token 验证通过
- 无效 Token 验证失败
- 篡改 Token 验证失败
- Token 过期后验证失败
- Token 撤销后验证失败
- 清理功能正常工作

### 9.2 集成测试

- HTTP 请求带有效 Token 成功
- HTTP 请求带无效 Token 失败返回 401
- HTTP 请求不带 Token 失败返回 401
- QUIC 连接带有效 Token 成功
- QUIC 连接带无效 Token 失败

### 9.3 安全测试

- 无法伪造有效 Token
- 无法通过修改时间戳延长有效期
- 撤销后的 Token 无法继续使用
- 不同密钥生成的 Token 无法互通

## 10. 部署建议

### 10.1 密钥管理

**生产环境：**
- 使用强随机密钥（至少 32 个字符）
- 定期轮换密钥
- 不要硬编码在代码中
- 使用环境变量或密钥管理服务

```bash
# 生成随机密钥
export AEROSYNC_SECRET_KEY=$(openssl rand -hex 32)
```

### 10.2 Token 生命周期

**推荐设置：**
- 开发环境：24 小时
- 测试环境：12 小时
- 生产环境：1-8 小时（根据安全需求）
- 高安全场景：1 小时

### 10.3 监控

需要监控的指标：
- 认证失败率
- Token 生成数量
- 活跃 Token 数量
- 认证延迟

## 11. 后续演进

### 11.1 Phase 2 增强（v0.2）

- 用户名/密码认证
- IP 白名单/黑名单
- 多用户支持
- 角色权限（RBAC）

### 11.2 Phase 3 高级特性（v0.5）

- OAuth2/OIDC 集成
- JWT Token 支持
- API Key 管理
- 双因素认证（2FA）

## 12. 参考资料

- [RFC 6750: OAuth 2.0 Bearer Token Usage](https://datatracker.ietf.org/doc/html/rfc6750)
- [HMAC-SHA256 Specification](https://en.wikipedia.org/wiki/HMAC)
- [Rust Cryptography Guidelines](https://rust-lang.github.io/api-guidelines/cryptography.html)

---

**文档版本**: v1.0  
**最后更新**: 2025-12-27  
**作者**: AeroSync Team


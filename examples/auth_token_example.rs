//! Token 认证使用示例
//!
//! 演示如何在 AeroSync 中使用 Token 认证功能

use aerosync_core::auth::{AuthConfig, AuthManager, AuthMiddleware, TokenManager};
use aerosync_core::Result;
use std::sync::Arc;

/// 示例 1: 基础 Token 生成和验证
fn example_basic_token() -> Result<()> {
    println!("=== 示例 1: 基础 Token 生成和验证 ===\n");

    // 创建 Token 管理器
    let token_manager = TokenManager::new("my-secret-key-12345".to_string())?.with_lifetime(24); // 24 小时有效期

    // 生成 Token
    let token = token_manager.generate_token()?;
    println!("生成的 Token: {}", token);

    // 验证 Token
    let is_valid = token_manager.verify_token(&token)?;
    println!("Token 有效性: {}", is_valid);

    // 撤销 Token
    token_manager.revoke_token(&token)?;
    println!("Token 已撤销");

    // 再次验证（应该失败）
    let is_valid_after_revoke = token_manager.verify_token(&token)?;
    println!("撤销后的 Token 有效性: {}\n", is_valid_after_revoke);

    Ok(())
}

/// 示例 2: 使用认证管理器
fn example_auth_manager() -> Result<()> {
    println!("=== 示例 2: 使用认证管理器 ===\n");

    // 创建认证配置
    let config = AuthConfig::new()
        .enable_auth()
        .with_secret_key("production-secret-key".to_string())
        .with_token_lifetime(12); // 12 小时

    // 创建认证管理器
    let auth_manager = AuthManager::new(config)?;

    // 生成 Token
    let token = auth_manager.generate_token()?;
    println!("生成的 Token: {}", token);

    // 模拟客户端请求认证
    let client_ip = "192.168.1.100";
    let is_authenticated = auth_manager.authenticate(Some(&token), client_ip)?;
    println!("客户端 {} 认证结果: {}", client_ip, is_authenticated);

    // 测试无效 Token
    let is_authenticated_invalid = auth_manager.authenticate(Some("invalid-token"), client_ip)?;
    println!("无效 Token 认证结果: {}\n", is_authenticated_invalid);

    Ok(())
}

/// 示例 3: HTTP 中间件集成
fn example_http_middleware() -> Result<()> {
    println!("=== 示例 3: HTTP 中间件集成 ===\n");

    // 设置认证
    let config = AuthConfig::new()
        .enable_auth()
        .with_secret_key("http-secret-key-12345".to_string());
    let auth_manager = Arc::new(AuthManager::new(config)?);
    let middleware = AuthMiddleware::new(auth_manager.clone());

    // 生成有效 Token
    let token = auth_manager.generate_token()?;
    let auth_header = format!("Bearer {}", token);

    // 模拟 HTTP 请求验证
    println!("模拟 HTTP 请求...");
    let is_authenticated =
        middleware.authenticate_http_request(Some(&auth_header), "192.168.1.50")?;
    println!("认证结果: {}", is_authenticated);

    // 模拟无 Token 的请求
    println!("\n模拟无 Token 的 HTTP 请求...");
    let is_authenticated_no_token = middleware.authenticate_http_request(None, "192.168.1.50")?;
    println!("认证结果: {}", is_authenticated_no_token);

    // 生成 401 响应
    if !is_authenticated_no_token {
        let response = middleware.unauthorized_response();
        println!("\n返回 {} 响应:", response.status_code);
        println!("消息: {}", response.message);
        for (key, value) in response.headers {
            println!("Header: {}: {}", key, value);
        }
    }

    println!();
    Ok(())
}

/// 示例 4: 配置文件加载
fn example_config_file() -> Result<()> {
    println!("=== 示例 4: 配置文件加载 ===\n");

    // TOML 配置示例
    let config_toml = r#"
enable_auth = true
secret_key = "config-file-secret-key-1234567890"
token_lifetime_hours = 48
allowed_ips = ["192.168.1.0/24", "10.0.0.0/8"]
"#;

    // 从 TOML 加载配置
    let config = AuthConfig::from_toml(config_toml)?;
    println!("加载的配置:");
    println!("  启用认证: {}", config.enable_auth);
    println!("  Token 生命周期: {} 小时", config.token_lifetime_hours);
    println!("  允许的 IP: {:?}", config.allowed_ips);

    // 验证配置
    match config.validate() {
        Ok(_) => println!("\n配置验证通过 ✓"),
        Err(e) => println!("\n配置验证失败 ✗: {}", e),
    }

    // 导出为 TOML
    let exported_toml = config.to_toml()?;
    println!("\n导出的 TOML 配置:\n{}", exported_toml);

    Ok(())
}

/// 示例 5: 实际使用场景 - 文件上传服务
fn example_file_upload_service() -> Result<()> {
    println!("=== 示例 5: 文件上传服务场景 ===\n");

    // 1. 服务端启动时初始化认证
    println!("1. 服务端启动，初始化认证系统...");
    let config = AuthConfig::new()
        .enable_auth()
        .with_secret_key("file-upload-secret-key".to_string())
        .with_token_lifetime(8); // 8 小时

    let auth_manager = Arc::new(AuthManager::new(config)?);
    println!("   ✓ 认证系统初始化完成\n");

    // 2. 管理员生成 Token 给客户端
    println!("2. 管理员为客户端生成 Token...");
    let client_token = auth_manager.generate_token()?;
    println!("   Token: {}", client_token);
    println!("   ✓ 已发送给客户端\n");

    // 3. 客户端使用 Token 上传文件
    println!("3. 客户端尝试上传文件...");
    let middleware = AuthMiddleware::new(auth_manager.clone());
    let auth_header = format!("Bearer {}", client_token);

    let can_upload = middleware.authenticate_http_request(Some(&auth_header), "203.0.113.42")?;

    if can_upload {
        println!("   ✓ 认证通过，允许上传");
        println!("   正在上传文件: large_dataset.zip (2.5 GB)");
        println!("   ✓ 上传成功");
    } else {
        println!("   ✗ 认证失败，拒绝上传");
    }

    // 4. 未授权用户尝试访问
    println!("\n4. 未授权用户尝试上传文件...");
    let unauthorized_upload = middleware.authenticate_http_request(None, "198.51.100.1")?;

    if !unauthorized_upload {
        println!("   ✗ 未提供 Token，拒绝访问");
        let response = middleware.unauthorized_response();
        println!("   返回 HTTP {} {}", response.status_code, response.message);
    }

    println!();
    Ok(())
}

fn main() -> Result<()> {
    println!("AeroSync Token 认证示例\n");
    println!("===============================================\n");

    // 运行所有示例
    example_basic_token()?;
    example_auth_manager()?;
    example_http_middleware()?;
    example_config_file()?;
    example_file_upload_service()?;

    println!("===============================================");
    println!("\n所有示例运行完成！\n");
    println!("💡 提示：");
    println!("  - 生产环境请使用强随机密钥（至少 32 个字符）");
    println!("  - 定期轮换密钥以提高安全性");
    println!("  - 根据安全需求调整 Token 生命周期");
    println!("  - 启用 HTTPS 保护 Token 在传输中的安全");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_examples() {
        assert!(example_basic_token().is_ok());
        assert!(example_auth_manager().is_ok());
        assert!(example_http_middleware().is_ok());
        assert!(example_config_file().is_ok());
        assert!(example_file_upload_service().is_ok());
    }
}

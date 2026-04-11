//! 认证和授权模块
//!
//! 提供 Token 认证、用户管理、IP 访问控制等安全功能。

mod token;
mod config;
mod middleware;
pub mod store;

pub use token::{TokenManager, TokenInfo, TokenError};
pub use config::AuthConfig;
pub use middleware::AuthMiddleware;
pub use store::{TokenStore, StoredToken};

use crate::error::Result;

/// 认证管理器
/// 
/// 统一管理所有认证相关功能，包括 Token 验证、用户验证、IP 验证等。
#[derive(Clone)]
pub struct AuthManager {
    token_manager: TokenManager,
    config: AuthConfig,
}

impl AuthManager {
    /// 创建新的认证管理器
    pub fn new(config: AuthConfig) -> Result<Self> {
        let token_manager = TokenManager::new(config.secret_key.clone())?;
        Ok(Self {
            token_manager,
            config,
        })
    }

    /// 验证请求是否通过认证
    pub fn authenticate(&self, token: Option<&str>, client_ip: &str) -> Result<bool> {
        // 如果未启用认证，直接通过
        if !self.config.enable_auth {
            return Ok(true);
        }

        // Token 验证
        if let Some(token) = token {
            if !self.token_manager.verify_token(token)? {
                return Ok(false);
            }
        } else {
            // 没有提供 Token
            return Ok(false);
        }

        // IP 访问控制验证
        if !self.verify_ip_access(client_ip)? {
            return Ok(false);
        }

        Ok(true)
    }

    /// 验证 IP 是否允许访问
    fn verify_ip_access(&self, _client_ip: &str) -> Result<bool> {
        // TODO: 实现 IP 白名单/黑名单检查
        // 当前版本简单允许所有 IP
        Ok(true)
    }

    /// 生成新的 Token
    pub fn generate_token(&self) -> Result<String> {
        self.token_manager.generate_token()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_manager_creation() {
        let config = AuthConfig {
            enable_auth: true,
            secret_key: "test-secret-key".to_string(),
            token_lifetime_hours: 24,
            allowed_ips: vec![],
        };
        let manager = AuthManager::new(config);
        assert!(manager.is_ok());
    }

    #[test]
    fn test_auth_disabled() {
        let config = AuthConfig {
            enable_auth: false,
            secret_key: "test-secret-key".to_string(),
            token_lifetime_hours: 24,
            allowed_ips: vec![],
        };
        let manager = AuthManager::new(config).unwrap();
        // 认证禁用时，应该允许任何请求
        assert!(manager.authenticate(None, "127.0.0.1").unwrap());
    }
}


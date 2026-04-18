//! 认证和授权模块
//!
//! 提供 Token 认证、用户管理、IP 访问控制等安全功能。

mod config;
mod middleware;
pub mod store;
mod token;

pub use config::AuthConfig;
pub use middleware::AuthMiddleware;
pub use store::{StoredToken, TokenStore};
pub use token::{TokenError, TokenInfo, TokenManager};

use crate::core::error::Result;

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
    fn verify_ip_access(&self, client_ip: &str) -> Result<bool> {
        // 空列表表示允许所有 IP
        if self.config.allowed_ips.is_empty() {
            return Ok(true);
        }

        let client_addr: std::net::IpAddr = client_ip.parse().map_err(|_| {
            crate::core::error::AeroSyncError::Auth(format!("Invalid client IP: {}", client_ip))
        })?;

        for cidr in &self.config.allowed_ips {
            if ip_in_cidr(client_addr, cidr) {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// 生成新的 Token
    pub fn generate_token(&self) -> Result<String> {
        self.token_manager.generate_token()
    }
}

/// 检查 IP 地址是否在 CIDR 范围内（IPv4 和 IPv6）
fn ip_in_cidr(ip: std::net::IpAddr, cidr: &str) -> bool {
    let Some((net_str, prefix_str)) = cidr.split_once('/') else {
        return false;
    };
    let Ok(prefix_len) = prefix_str.parse::<u32>() else {
        return false;
    };

    match ip {
        std::net::IpAddr::V4(ipv4) => {
            let Ok(net_ip) = net_str.parse::<std::net::Ipv4Addr>() else {
                return false;
            };
            if prefix_len > 32 {
                return false;
            }
            if prefix_len == 0 {
                return true;
            }
            let shift = 32 - prefix_len;
            u32::from(ipv4) >> shift == u32::from(net_ip) >> shift
        }
        std::net::IpAddr::V6(ipv6) => {
            let Ok(net_ip) = net_str.parse::<std::net::Ipv6Addr>() else {
                return false;
            };
            if prefix_len > 128 {
                return false;
            }
            if prefix_len == 0 {
                return true;
            }
            let ip_bits = u128::from(ipv6);
            let net_bits = u128::from(net_ip);
            let shift = 128 - prefix_len;
            ip_bits >> shift == net_bits >> shift
        }
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

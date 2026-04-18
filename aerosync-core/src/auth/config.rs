//! 认证配置
//!
//! 管理认证相关的配置参数。

use serde::{Deserialize, Serialize};

/// 认证配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    /// 是否启用认证
    pub enable_auth: bool,

    /// 密钥，用于签名 Token
    pub secret_key: String,

    /// Token 生命周期（小时）
    pub token_lifetime_hours: u64,

    /// 允许访问的 IP 列表（CIDR 格式）
    /// 例如：["192.168.1.0/24", "10.0.0.0/8"]
    /// 空列表表示允许所有 IP
    pub allowed_ips: Vec<String>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enable_auth: false,
            secret_key: generate_random_key(),
            token_lifetime_hours: 24,
            allowed_ips: vec![],
        }
    }
}

impl AuthConfig {
    /// 创建新的认证配置
    pub fn new() -> Self {
        Self::default()
    }

    /// 启用认证
    pub fn enable_auth(mut self) -> Self {
        self.enable_auth = true;
        self
    }

    /// 设置密钥
    pub fn with_secret_key(mut self, key: String) -> Self {
        self.secret_key = key;
        self
    }

    /// 设置 Token 生命周期
    pub fn with_token_lifetime(mut self, hours: u64) -> Self {
        self.token_lifetime_hours = hours;
        self
    }

    /// 添加允许的 IP
    pub fn add_allowed_ip(mut self, ip: String) -> Self {
        self.allowed_ips.push(ip);
        self
    }

    /// 从 TOML 字符串加载配置
    pub fn from_toml(toml_str: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(toml_str)
    }

    /// 转换为 TOML 字符串
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// 验证配置的有效性
    pub fn validate(&self) -> Result<(), String> {
        if self.enable_auth && self.secret_key.is_empty() {
            return Err("Secret key cannot be empty when auth is enabled".to_string());
        }

        if self.secret_key.len() < 16 {
            return Err("Secret key should be at least 16 characters".to_string());
        }

        if self.token_lifetime_hours == 0 {
            return Err("Token lifetime must be greater than 0".to_string());
        }

        for cidr in &self.allowed_ips {
            validate_cidr(cidr)?;
        }

        Ok(())
    }
}

/// 生成随机密钥
fn generate_random_key() -> String {
    use uuid::Uuid;
    format!("{}-{}", Uuid::new_v4(), Uuid::new_v4())
}

/// 验证单条 CIDR 字符串格式（IPv4 和 IPv6）
fn validate_cidr(cidr: &str) -> Result<(), String> {
    let (ip_str, prefix_str) = cidr
        .split_once('/')
        .ok_or_else(|| format!("Invalid CIDR '{}': missing prefix length (e.g. /24)", cidr))?;

    let prefix_len: u8 = prefix_str
        .parse()
        .map_err(|_| format!("Invalid CIDR '{}': prefix length is not a number", cidr))?;

    // 判断是 IPv4 还是 IPv6
    if ip_str.contains(':') {
        // IPv6
        ip_str
            .parse::<std::net::Ipv6Addr>()
            .map_err(|_| format!("Invalid CIDR '{}': malformed IPv6 address", cidr))?;
        if prefix_len > 128 {
            return Err(format!(
                "Invalid CIDR '{}': IPv6 prefix length must be 0-128",
                cidr
            ));
        }
    } else {
        // IPv4
        ip_str
            .parse::<std::net::Ipv4Addr>()
            .map_err(|_| format!("Invalid CIDR '{}': malformed IPv4 address", cidr))?;
        if prefix_len > 32 {
            return Err(format!(
                "Invalid CIDR '{}': IPv4 prefix length must be 0-32",
                cidr
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AuthConfig::default();
        assert!(!config.enable_auth);
        assert!(!config.secret_key.is_empty());
        assert_eq!(config.token_lifetime_hours, 24);
    }

    #[test]
    fn test_config_builder() {
        let config = AuthConfig::new()
            .enable_auth()
            .with_secret_key("my-secret-key-12345".to_string())
            .with_token_lifetime(48)
            .add_allowed_ip("192.168.1.0/24".to_string());

        assert!(config.enable_auth);
        assert_eq!(config.secret_key, "my-secret-key-12345");
        assert_eq!(config.token_lifetime_hours, 48);
        assert_eq!(config.allowed_ips.len(), 1);
    }

    #[test]
    fn test_config_validation() {
        let config = AuthConfig::new()
            .enable_auth()
            .with_secret_key("short".to_string());

        assert!(config.validate().is_err());

        let config = AuthConfig::new()
            .enable_auth()
            .with_secret_key("valid-secret-key-16-chars".to_string());

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_toml_serialization() {
        let config = AuthConfig::new()
            .enable_auth()
            .with_secret_key("test-secret-key-12345".to_string())
            .with_token_lifetime(48);

        let toml_str = config.to_toml().unwrap();
        assert!(toml_str.contains("enable_auth = true"));
        assert!(toml_str.contains("test-secret-key-12345"));

        let loaded_config = AuthConfig::from_toml(&toml_str).unwrap();
        assert_eq!(loaded_config.enable_auth, config.enable_auth);
        assert_eq!(loaded_config.secret_key, config.secret_key);
    }
}

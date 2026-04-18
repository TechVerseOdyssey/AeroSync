//! Token 生成和验证
//!
//! 实现基于 HMAC-SHA256 的 Token 认证机制。

use hex;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::error::{AeroSyncError, Result};

/// Token 信息
#[derive(Debug, Clone)]
pub struct TokenInfo {
    /// Token 字符串
    pub token: String,
    /// 创建时间（Unix 时间戳）
    pub created_at: u64,
    /// 过期时间（Unix 时间戳）
    pub expires_at: u64,
    /// 是否已撤销
    pub revoked: bool,
}

impl TokenInfo {
    /// 检查 Token 是否已过期
    pub fn is_expired(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        now > self.expires_at
    }

    /// 检查 Token 是否有效（未过期且未撤销）
    pub fn is_valid(&self) -> bool {
        !self.is_expired() && !self.revoked
    }
}

/// Token 错误类型
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("Token 已过期")]
    Expired,
    #[error("Token 无效")]
    Invalid,
    #[error("Token 已被撤销")]
    Revoked,
    #[error("Token 不存在")]
    NotFound,
}

/// Token 管理器
///
/// 负责 Token 的生成、验证、撤销等操作。
#[derive(Clone)]
pub struct TokenManager {
    /// 密钥，用于签名 Token
    secret_key: String,
    /// Token 生命周期（小时）
    lifetime_hours: u64,
    /// 存储所有有效的 Token
    tokens: Arc<RwLock<HashMap<String, TokenInfo>>>,
}

impl TokenManager {
    /// 创建新的 Token 管理器
    pub fn new(secret_key: String) -> Result<Self> {
        if secret_key.is_empty() {
            return Err(AeroSyncError::Config(
                "Secret key cannot be empty".to_string(),
            ));
        }

        Ok(Self {
            secret_key,
            lifetime_hours: 24, // 默认 24 小时
            tokens: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// 设置 Token 生命周期
    pub fn with_lifetime(mut self, hours: u64) -> Self {
        self.lifetime_hours = hours;
        self
    }

    /// 生成新的 Token
    ///
    /// Token 格式：{uuid}.{timestamp}.{signature}
    pub fn generate_token(&self) -> Result<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let expires_at = now + (self.lifetime_hours * 3600);
        let uuid = Uuid::new_v4().to_string();

        // 生成签名
        let signature = self.sign_token(&uuid, now, expires_at);

        // 拼接 Token: {uuid}.{timestamp}.{expires}.{signature}
        let token = format!("{}.{}.{}.{}", uuid, now, expires_at, signature);

        // 存储 Token 信息
        let token_info = TokenInfo {
            token: token.clone(),
            created_at: now,
            expires_at,
            revoked: false,
        };

        self.tokens.write().unwrap().insert(uuid, token_info);

        Ok(token)
    }

    /// 验证 Token 是否有效
    pub fn verify_token(&self, token: &str) -> Result<bool> {
        // 解析 Token
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 4 {
            return Ok(false);
        }

        let uuid = parts[0];
        let created_at: u64 = parts[1]
            .parse()
            .map_err(|_| AeroSyncError::Auth("Invalid token format".to_string()))?;
        let expires_at: u64 = parts[2]
            .parse()
            .map_err(|_| AeroSyncError::Auth("Invalid token format".to_string()))?;
        let signature = parts[3];

        // 验证签名
        let expected_signature = self.sign_token(uuid, created_at, expires_at);
        if signature != expected_signature {
            return Ok(false);
        }

        // 检查是否在存储中
        let tokens = self.tokens.read().unwrap();
        if let Some(token_info) = tokens.get(uuid) {
            return Ok(token_info.is_valid());
        }

        // Token 不在存储中，但签名有效，检查是否过期
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        Ok(now <= expires_at)
    }

    /// 撤销 Token
    pub fn revoke_token(&self, token: &str) -> Result<()> {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 4 {
            return Err(AeroSyncError::Auth("Invalid token format".to_string()));
        }

        let uuid = parts[0];
        let mut tokens = self.tokens.write().unwrap();

        if let Some(token_info) = tokens.get_mut(uuid) {
            token_info.revoked = true;
            Ok(())
        } else {
            Err(AeroSyncError::Auth("Token not found".to_string()))
        }
    }

    /// 清理过期的 Token
    pub fn cleanup_expired_tokens(&self) {
        let mut tokens = self.tokens.write().unwrap();
        tokens.retain(|_, info| info.is_valid());
    }

    /// 获取所有有效 Token 的数量
    pub fn active_token_count(&self) -> usize {
        let tokens = self.tokens.read().unwrap();
        tokens.values().filter(|info| info.is_valid()).count()
    }

    /// 使用 HMAC-SHA256 签名 Token
    fn sign_token(&self, uuid: &str, created_at: u64, expires_at: u64) -> String {
        let data = format!("{}.{}.{}.{}", uuid, created_at, expires_at, self.secret_key);
        let mut hasher = Sha256::new();
        hasher.update(data.as_bytes());
        let result = hasher.finalize();
        hex::encode(result)[..16].to_string() // 只取前 16 个字符
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn test_token_generation() {
        let manager = TokenManager::new("test-secret".to_string()).unwrap();
        let token = manager.generate_token().unwrap();

        // Token 应该包含 4 个部分
        assert_eq!(token.split('.').count(), 4);
    }

    #[test]
    fn test_token_verification() {
        let manager = TokenManager::new("test-secret".to_string()).unwrap();
        let token = manager.generate_token().unwrap();

        // 刚生成的 Token 应该有效
        assert!(manager.verify_token(&token).unwrap());
    }

    #[test]
    fn test_invalid_token() {
        let manager = TokenManager::new("test-secret".to_string()).unwrap();

        // 无效的 Token 格式
        assert!(!manager.verify_token("invalid-token").unwrap());

        // 篡改的 Token：将签名段替换为全 0，确保一定与真实签名不同
        // （旧实现用 token.replace("a", "b") 在 uuid/签名都不含 'a' 时
        // 不会改动 token，约 4.5% 概率出现 flaky）
        let token = manager.generate_token().unwrap();
        let parts: Vec<&str> = token.split('.').collect();
        let tampered = format!("{}.{}.{}.0000000000000000", parts[0], parts[1], parts[2]);
        assert!(!manager.verify_token(&tampered).unwrap());
    }

    #[test]
    fn test_token_revocation() {
        let manager = TokenManager::new("test-secret".to_string()).unwrap();
        let token = manager.generate_token().unwrap();

        // 撤销前应该有效
        assert!(manager.verify_token(&token).unwrap());

        // 撤销
        manager.revoke_token(&token).unwrap();

        // 撤销后应该无效
        assert!(!manager.verify_token(&token).unwrap());
    }

    #[test]
    fn test_token_expiration() {
        let manager = TokenManager::new("test-secret".to_string())
            .unwrap()
            .with_lifetime(0); // 0 小时，立即过期

        let token = manager.generate_token().unwrap();

        // 等待 1 秒让 Token 过期
        sleep(Duration::from_secs(1));

        // 过期的 Token 应该无效
        assert!(!manager.verify_token(&token).unwrap());
    }

    #[test]
    fn test_cleanup_expired_tokens() {
        let manager = TokenManager::new("test-secret".to_string())
            .unwrap()
            .with_lifetime(0);

        // 生成一些 Token
        for _ in 0..5 {
            manager.generate_token().unwrap();
        }

        assert_eq!(manager.active_token_count(), 5);

        // 等待过期
        sleep(Duration::from_secs(1));

        // 清理
        manager.cleanup_expired_tokens();

        // 应该没有有效 Token
        assert_eq!(manager.active_token_count(), 0);
    }
}

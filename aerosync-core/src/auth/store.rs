/// Token 持久化存储
///
/// 将生成的 Token 持久化到 `~/.config/aerosync/tokens.toml`，
/// 支持跨进程复用 Token（接收端重启后无需重新生成）。
use crate::error::{AeroSyncError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 持久化存储中的单个 Token 记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToken {
    /// JWT-style token 字符串
    pub token: String,
    /// 备注（可选）
    pub label: Option<String>,
    /// 创建时间（Unix 时间戳秒）
    pub created_at: u64,
    /// 过期时间（Unix 时间戳秒）
    pub expires_at: u64,
    /// 是否已撤销
    pub revoked: bool,
}

impl StoredToken {
    pub fn is_valid(&self) -> bool {
        !self.revoked && self.expires_at > now_secs()
    }

    pub fn is_expired(&self) -> bool {
        self.expires_at <= now_secs()
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TokenFile {
    tokens: Vec<StoredToken>,
}

/// TOML 格式的 Token 持久化存储
pub struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    /// 打开（或自动创建）Token 文件
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
        }
    }

    /// 默认存储路径：`~/.config/aerosync/tokens.toml`
    pub fn default_path() -> PathBuf {
        dirs_next::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("aerosync")
            .join("tokens.toml")
    }

    // ── 私有 helpers ─────────────────────────────────────────────────────────

    fn load_file(&self) -> Result<TokenFile> {
        if !self.path.exists() {
            return Ok(TokenFile::default());
        }
        let content = std::fs::read_to_string(&self.path)
            .map_err(AeroSyncError::FileIo)?;
        toml::from_str(&content)
            .map_err(|e| AeroSyncError::System(format!("tokens.toml parse error: {}", e)))
    }

    fn save_file(&self, data: &TokenFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(data)
            .map_err(|e| AeroSyncError::System(format!("tokens.toml serialize error: {}", e)))?;
        std::fs::write(&self.path, content)?;
        Ok(())
    }

    // ── 公开 API ──────────────────────────────────────────────────────────────

    /// 保存一个新 Token
    pub fn save(&self, token: &str, label: Option<&str>, expires_at: u64) -> Result<()> {
        let mut data = self.load_file()?;
        data.tokens.push(StoredToken {
            token: token.to_string(),
            label: label.map(|s| s.to_string()),
            created_at: now_secs(),
            expires_at,
            revoked: false,
        });
        self.save_file(&data)
    }

    /// 列出所有 Token（包含已过期/已撤销）
    pub fn list_all(&self) -> Result<Vec<StoredToken>> {
        Ok(self.load_file()?.tokens)
    }

    /// 列出所有有效 Token
    pub fn list_valid(&self) -> Result<Vec<StoredToken>> {
        Ok(self.load_file()?.tokens.into_iter().filter(|t| t.is_valid()).collect())
    }

    /// 按前缀查找有效 Token（前 8 字符匹配即可，避免贴完整 token）
    pub fn find_by_prefix(&self, prefix: &str) -> Result<Option<StoredToken>> {
        Ok(self
            .list_valid()?
            .into_iter()
            .find(|t| t.token.starts_with(prefix)))
    }

    /// 撤销 Token（按完整 token 字符串匹配）
    pub fn revoke(&self, token: &str) -> Result<bool> {
        let mut data = self.load_file()?;
        let mut found = false;
        for t in &mut data.tokens {
            if t.token == token {
                t.revoked = true;
                found = true;
            }
        }
        if found {
            self.save_file(&data)?;
        }
        Ok(found)
    }

    /// 清理过期 + 已撤销 Token（减小文件大小）
    pub fn prune(&self) -> Result<usize> {
        let mut data = self.load_file()?;
        let before = data.tokens.len();
        data.tokens.retain(|t| !t.is_expired() && !t.revoked);
        let removed = before - data.tokens.len();
        if removed > 0 {
            self.save_file(&data)?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp_store() -> (TokenStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tokens.toml");
        (TokenStore::new(&path), dir)
    }

    fn future(secs: u64) -> u64 {
        now_secs() + secs
    }

    #[test]
    fn test_save_and_list_all() {
        let (store, _dir) = tmp_store();
        store.save("tok1", Some("label1"), future(3600)).unwrap();
        store.save("tok2", None, future(3600)).unwrap();
        let all = store.list_all().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_list_valid_excludes_expired() {
        let (store, _dir) = tmp_store();
        store.save("valid_tok", None, future(3600)).unwrap();
        store.save("expired_tok", None, now_secs() - 1).unwrap(); // already expired
        let valid = store.list_valid().unwrap();
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].token, "valid_tok");
    }

    #[test]
    fn test_revoke() {
        let (store, _dir) = tmp_store();
        store.save("rev_tok", None, future(3600)).unwrap();
        assert_eq!(store.list_valid().unwrap().len(), 1);
        let found = store.revoke("rev_tok").unwrap();
        assert!(found);
        assert_eq!(store.list_valid().unwrap().len(), 0);
    }

    #[test]
    fn test_revoke_nonexistent_returns_false() {
        let (store, _dir) = tmp_store();
        assert!(!store.revoke("nonexistent").unwrap());
    }

    #[test]
    fn test_find_by_prefix() {
        let (store, _dir) = tmp_store();
        store.save("abcdef1234567890", None, future(3600)).unwrap();
        let found = store.find_by_prefix("abcdef").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().token, "abcdef1234567890");
    }

    #[test]
    fn test_find_by_prefix_not_found() {
        let (store, _dir) = tmp_store();
        let found = store.find_by_prefix("xxxxxx").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn test_prune_removes_expired_and_revoked() {
        let (store, _dir) = tmp_store();
        store.save("active", None, future(3600)).unwrap();
        store.save("expired", None, now_secs() - 1).unwrap();
        store.save("to_revoke", None, future(3600)).unwrap();
        store.revoke("to_revoke").unwrap();

        let removed = store.prune().unwrap();
        assert_eq!(removed, 2);
        let all = store.list_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].token, "active");
    }

    #[test]
    fn test_empty_store() {
        let (store, _dir) = tmp_store();
        assert!(store.list_all().unwrap().is_empty());
        assert!(store.list_valid().unwrap().is_empty());
    }

    #[test]
    fn test_persists_across_instances() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tokens.toml");

        let store1 = TokenStore::new(&path);
        store1.save("persist_me", Some("test"), future(3600)).unwrap();

        // 新实例读取同一个文件
        let store2 = TokenStore::new(&path);
        let all = store2.list_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].token, "persist_me");
        assert_eq!(all[0].label.as_deref(), Some("test"));
    }
}

pub use aerosync_core::routing::RouterConfig;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// AeroSync 配置文件结构（TOML 格式）
///
/// 示例配置文件 (~/.aerosync/config.toml):
/// ```toml
/// [transfer]
/// max_concurrent = 8
/// chunk_size_mb = 32
/// retry_attempts = 3
/// timeout_seconds = 60
///
/// [auth]
/// token = "my-token"
///
/// [server]
/// http_port = 7788
/// quic_port = 7789
/// save_to = "./received"
/// bind = "0.0.0.0"
///
/// [metrics]
/// enabled = true
///
/// [ws]
/// enabled = true
/// event_buffer = 256
///
/// [[routing.rules]]
/// name = "images"
/// destination = "./received/images"
/// extension = "jpg"
///
/// [[routing.rules]]
/// name = "trusted-agent"
/// destination = "./received/agent"
/// sender_ip = "10.0.0.5"
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AeroSyncConfig {
    #[serde(default)]
    pub transfer: TransferSection,
    #[serde(default)]
    pub auth: AuthSection,
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub metrics: MetricsSection,
    #[serde(default)]
    pub ws: WebSocketSection,
    #[serde(default)]
    pub routing: Option<RouterConfig>,
}

/// Prometheus 指标导出配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetricsSection {
    /// 是否启用 /metrics 端点（默认 true）
    pub enabled: bool,
}

impl Default for MetricsSection {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// WebSocket 事件推送配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebSocketSection {
    /// 是否启用 /ws 端点（默认 true）
    pub enabled: bool,
    /// 事件广播缓冲区大小（默认 256）
    pub event_buffer: usize,
}

impl Default for WebSocketSection {
    fn default() -> Self {
        Self {
            enabled: true,
            event_buffer: 256,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TransferSection {
    /// 最大并发传输任务数
    pub max_concurrent: usize,
    /// 分片大小（MB），用于大文件分片上传
    pub chunk_size_mb: u64,
    /// 失败重试次数
    pub retry_attempts: u32,
    /// 连接超时（秒）
    pub timeout_seconds: u64,
}

impl Default for TransferSection {
    fn default() -> Self {
        Self {
            max_concurrent: 4,
            chunk_size_mb: 32,
            retry_attempts: 3,
            timeout_seconds: 60,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AuthSection {
    /// 认证 Token（发送方使用）
    pub token: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerSection {
    /// HTTP 监听端口
    pub http_port: u16,
    /// QUIC 监听端口
    pub quic_port: u16,
    /// 文件保存目录
    pub save_to: String,
    /// 绑定地址
    pub bind: String,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            http_port: 7788,
            quic_port: 7789,
            save_to: "./received".to_string(),
            bind: "0.0.0.0".to_string(),
        }
    }
}

impl AeroSyncConfig {
    /// 从 TOML 文件加载配置；文件不存在时返回默认值。
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            tracing::debug!(
                "Config file not found at {}, using defaults",
                path.display()
            );
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)?;
        let cfg: Self = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("Failed to parse config {}: {}", path.display(), e))?;
        tracing::info!("Loaded config from {}", path.display());
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_default_config() {
        let cfg = AeroSyncConfig::default();
        assert_eq!(cfg.transfer.max_concurrent, 4);
        assert_eq!(cfg.transfer.chunk_size_mb, 32);
        assert_eq!(cfg.transfer.retry_attempts, 3);
        assert_eq!(cfg.transfer.timeout_seconds, 60);
        assert_eq!(cfg.server.http_port, 7788);
        assert_eq!(cfg.server.quic_port, 7789);
        assert_eq!(cfg.server.save_to, "./received");
        assert_eq!(cfg.server.bind, "0.0.0.0");
        assert!(cfg.auth.token.is_none());
    }

    #[test]
    fn test_load_nonexistent_returns_default() {
        let cfg = AeroSyncConfig::load(Path::new("/nonexistent/path.toml")).unwrap();
        assert_eq!(cfg.transfer.max_concurrent, 4);
        assert_eq!(cfg.server.http_port, 7788);
    }

    #[test]
    fn test_load_valid_toml() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[transfer]\nmax_concurrent = 8\nchunk_size_mb = 64\nretry_attempts = 5\ntimeout_seconds = 120\n",
        )
        .unwrap();
        let cfg = AeroSyncConfig::load(&path).unwrap();
        assert_eq!(cfg.transfer.max_concurrent, 8);
        assert_eq!(cfg.transfer.chunk_size_mb, 64);
        assert_eq!(cfg.transfer.retry_attempts, 5);
        assert_eq!(cfg.transfer.timeout_seconds, 120);
        // server section uses defaults when not specified
        assert_eq!(cfg.server.http_port, 7788);
    }

    #[test]
    fn test_load_auth_section() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[auth]\ntoken = \"my-secret-token\"\n").unwrap();
        let cfg = AeroSyncConfig::load(&path).unwrap();
        assert_eq!(cfg.auth.token, Some("my-secret-token".to_string()));
    }

    #[test]
    fn test_load_server_section() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[server]\nhttp_port = 8080\nquic_port = 8081\nsave_to = \"/tmp/received\"\nbind = \"127.0.0.1\"\n",
        )
        .unwrap();
        let cfg = AeroSyncConfig::load(&path).unwrap();
        assert_eq!(cfg.server.http_port, 8080);
        assert_eq!(cfg.server.quic_port, 8081);
        assert_eq!(cfg.server.save_to, "/tmp/received");
        assert_eq!(cfg.server.bind, "127.0.0.1");
    }

    #[test]
    fn test_load_invalid_toml_returns_err() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "not valid toml [[[").unwrap();
        let result = AeroSyncConfig::load(&path);
        assert!(result.is_err());
    }

    #[test]
    fn test_default_metrics_and_ws() {
        let cfg = AeroSyncConfig::default();
        assert!(cfg.metrics.enabled);
        assert!(cfg.ws.enabled);
        assert_eq!(cfg.ws.event_buffer, 256);
        assert!(cfg.routing.is_none());
    }

    #[test]
    fn test_load_metrics_section() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[metrics]\nenabled = false\n").unwrap();
        let cfg = AeroSyncConfig::load(&path).unwrap();
        assert!(!cfg.metrics.enabled);
        // ws still defaults to true
        assert!(cfg.ws.enabled);
    }

    #[test]
    fn test_load_ws_section() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[ws]\nenabled = false\nevent_buffer = 512\n").unwrap();
        let cfg = AeroSyncConfig::load(&path).unwrap();
        assert!(!cfg.ws.enabled);
        assert_eq!(cfg.ws.event_buffer, 512);
    }

    #[test]
    fn test_load_routing_rules() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[[routing.rules]]
name = "images"
destination = "./received/images"
extension = "jpg"

[[routing.rules]]
name = "agent"
destination = "./received/agent"
sender_ip = "10.0.0.5"
"#,
        )
        .unwrap();
        let cfg = AeroSyncConfig::load(&path).unwrap();
        let rules = &cfg.routing.as_ref().unwrap().rules;
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].name, "images");
        assert_eq!(rules[0].extension.as_deref(), Some("jpg"));
        assert_eq!(rules[1].name, "agent");
        assert_eq!(rules[1].sender_ip.as_deref(), Some("10.0.0.5"));
    }
}

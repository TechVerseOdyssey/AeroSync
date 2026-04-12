//! mDNS 局域网服务发现模块
//!
//! AeroSync receiver 启动时通过 mDNS 广播自身地址，sender 或其他 agent
//! 可以扫描局域网内所有可用的 AeroSync receiver，无需手动配置 IP。
//!
//! ## 服务类型
//! `_aerosync._tcp.local.`
//!
//! ## TXT 记录字段
//! - `version` — AeroSync 版本号
//! - `ws`      — WebSocket 是否启用 (`true` / `false`)
//! - `auth`    — 是否需要认证 (`true` / `false`)
//!
//! ## 用法
//!
//! ### receiver 端：注册服务广播
//! ```rust,ignore
//! let handle = AeroSyncMdns::register("my-receiver", 7788, "0.2.0", true, false).await?;
//! // 保持 handle 存活即持续广播；drop 时自动注销
//! ```
//!
//! ### sender / discover 端：发现服务
//! ```rust,ignore
//! let peers = AeroSyncMdns::discover(Duration::from_secs(3)).await?;
//! for peer in peers {
//!     println!("{} → {}:{}", peer.name, peer.host, peer.port);
//! }
//! ```

use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::collections::HashMap;
use std::time::Duration;
use tracing::{debug, info, warn};

/// mDNS 服务类型
pub const MDNS_SERVICE_TYPE: &str = "_aerosync._tcp.local.";

/// 发现到的 AeroSync peer 信息
#[derive(Debug, Clone)]
pub struct AeroSyncPeer {
    /// mDNS 实例名（通常是主机名）
    pub name: String,
    /// 解析出的 IP 地址（IPv4 优先）
    pub host: String,
    /// HTTP 监听端口
    pub port: u16,
    /// AeroSync 版本号（TXT record）
    pub version: Option<String>,
    /// 是否启用 WebSocket（TXT record）
    pub ws_enabled: bool,
    /// 是否需要认证（TXT record）
    pub auth_required: bool,
}

impl AeroSyncPeer {
    /// 返回 `host:port` 字符串，可直接传给 `aerosync send`
    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// mDNS 广播句柄 — 保持存活则持续广播，drop 时自动注销
pub struct MdnsHandle {
    daemon: ServiceDaemon,
    service_fullname: String,
}

impl Drop for MdnsHandle {
    fn drop(&mut self) {
        if let Err(e) = self.daemon.unregister(&self.service_fullname) {
            warn!("mDNS unregister failed: {}", e);
        }
    }
}

/// AeroSync mDNS 操作集
pub struct AeroSyncMdns;

impl AeroSyncMdns {
    /// 在局域网广播 AeroSync receiver 服务。
    ///
    /// # 参数
    /// - `instance_name`  — 实例名（建议用主机名）
    /// - `port`           — HTTP 监听端口
    /// - `version`        — AeroSync 版本字符串
    /// - `ws_enabled`     — 是否启用 WebSocket
    /// - `auth_required`  — 是否需要认证 token
    ///
    /// 返回 `MdnsHandle`，保持存活即持续广播；drop 时自动注销。
    pub fn register(
        instance_name: &str,
        port: u16,
        version: &str,
        ws_enabled: bool,
        auth_required: bool,
    ) -> Result<MdnsHandle, mdns_sd::Error> {
        let daemon = ServiceDaemon::new()?;

        let host = hostname_or_localhost();
        let mut properties = HashMap::new();
        properties.insert("version".to_string(), version.to_string());
        properties.insert("ws".to_string(), ws_enabled.to_string());
        properties.insert("auth".to_string(), auth_required.to_string());

        let service = ServiceInfo::new(
            MDNS_SERVICE_TYPE,
            instance_name,
            &host,
            (),          // IP 由 mdns-sd 自动从网卡获取
            port,
            properties,
        )?;

        let fullname = service.get_fullname().to_string();
        daemon.register(service)?;

        info!(
            "mDNS: broadcasting AeroSync receiver as '{}' on port {}",
            instance_name, port
        );

        Ok(MdnsHandle { daemon, service_fullname: fullname })
    }

    /// 扫描局域网内的 AeroSync receiver，等待 `timeout` 后返回结果。
    ///
    /// 结果按发现顺序排列，去重（同 fullname 只取最新）。
    pub async fn discover(timeout: Duration) -> Vec<AeroSyncPeer> {
        let daemon = match ServiceDaemon::new() {
            Ok(d) => d,
            Err(e) => {
                warn!("mDNS: failed to create daemon for discovery: {}", e);
                return vec![];
            }
        };

        let receiver = match daemon.browse(MDNS_SERVICE_TYPE) {
            Ok(r) => r,
            Err(e) => {
                warn!("mDNS: failed to browse {}: {}", MDNS_SERVICE_TYPE, e);
                return vec![];
            }
        };

        let mut peers: HashMap<String, AeroSyncPeer> = HashMap::new();
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let remaining = match deadline.checked_duration_since(tokio::time::Instant::now()) {
                Some(d) => d,
                None => break,
            };

            // 非阻塞读取所有已到达的事件
            let event = tokio::time::timeout(
                remaining.min(Duration::from_millis(100)),
                tokio::task::spawn_blocking({
                    let recv = receiver.clone();
                    move || recv.recv_timeout(Duration::from_millis(80))
                }),
            )
            .await;

            match event {
                Ok(Ok(Ok(mdns_sd::ServiceEvent::ServiceResolved(info)))) => {
                    let fullname = info.get_fullname().to_string();
                    let name = info.get_hostname().trim_end_matches('.').to_string();
                    let port = info.get_port();

                    // IPv4 优先
                    let host = info
                        .get_addresses()
                        .iter()
                        .find(|a| a.is_ipv4())
                        .or_else(|| info.get_addresses().iter().next())
                        .map(|a| a.to_string())
                        .unwrap_or_else(|| name.clone());

                    let props = info.get_properties();
                    let version = props.get("version").map(|v| v.val_str().to_string());
                    let ws_enabled = props
                        .get("ws")
                        .map(|v| v.val_str() == "true")
                        .unwrap_or(true);
                    let auth_required = props
                        .get("auth")
                        .map(|v| v.val_str() == "true")
                        .unwrap_or(false);

                    debug!(
                        "mDNS resolved: {} → {}:{} (version={:?} ws={} auth={})",
                        name, host, port, version, ws_enabled, auth_required
                    );

                    peers.insert(
                        fullname,
                        AeroSyncPeer { name, host, port, version, ws_enabled, auth_required },
                    );
                }
                Ok(Ok(Ok(mdns_sd::ServiceEvent::SearchStopped(_)))) => break,
                _ => {
                    // timeout 或其他事件，继续等待直到 deadline
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                }
            }
        }

        let _ = daemon.stop_browse(MDNS_SERVICE_TYPE);
        peers.into_values().collect()
    }
}

/// 获取本机主机名，失败时返回 "localhost"
fn hostname_or_localhost() -> String {
    hostname::get()
        .ok()
        .and_then(|s| s.into_string().ok())
        .unwrap_or_else(|| "localhost".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_addr_format() {
        let peer = AeroSyncPeer {
            name: "machine-a".to_string(),
            host: "192.168.1.10".to_string(),
            port: 7788,
            version: Some("0.2.0".to_string()),
            ws_enabled: true,
            auth_required: false,
        };
        assert_eq!(peer.addr(), "192.168.1.10:7788");
    }

    #[test]
    fn test_peer_addr_ipv6() {
        let peer = AeroSyncPeer {
            name: "machine-b".to_string(),
            host: "::1".to_string(),
            port: 7788,
            version: None,
            ws_enabled: false,
            auth_required: true,
        };
        assert_eq!(peer.addr(), "::1:7788");
    }

    #[test]
    fn test_mdns_service_type_constant() {
        assert!(MDNS_SERVICE_TYPE.contains("_aerosync"));
        assert!(MDNS_SERVICE_TYPE.ends_with(".local."));
    }

    #[test]
    fn test_peer_fields() {
        let peer = AeroSyncPeer {
            name: "recv-1".to_string(),
            host: "10.0.0.5".to_string(),
            port: 8080,
            version: Some("0.2.0".to_string()),
            ws_enabled: true,
            auth_required: true,
        };
        assert!(peer.ws_enabled);
        assert!(peer.auth_required);
        assert_eq!(peer.version.as_deref(), Some("0.2.0"));
        assert_eq!(peer.port, 8080);
    }
}

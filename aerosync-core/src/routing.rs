//! Multi-destination routing module
//!
//! Allows the server to route incoming files to different directories based on
//! configurable rules. Rules are matched in order; the first match wins.
//! If no rule matches, the default `receive_directory` is used.
//!
//! Rule matching criteria (all optional, each defaults to "match any"):
//! - `tag`       — string sent by the client via the `X-AeroSync-Tag` header
//! - `sender_ip` — exact IP match or CIDR prefix (prefix match on string)
//! - `extension` — file extension without the leading dot (case-insensitive)

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A single routing rule entry
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoutingRule {
    /// Human-readable name (used in logs)
    pub name: String,
    /// Destination directory for files matching this rule
    pub destination: PathBuf,
    /// Match only files from senders carrying this tag header value
    pub tag: Option<String>,
    /// Match only files from this sender IP (exact match)
    pub sender_ip: Option<String>,
    /// Match only files with this extension (without dot, case-insensitive)
    pub extension: Option<String>,
}

impl RoutingRule {
    /// Returns true if all non-None criteria match the provided inputs.
    pub fn matches(&self, sender_ip: &str, tag: Option<&str>, filename: &str) -> bool {
        // Check sender_ip criterion
        if let Some(ref rule_ip) = self.sender_ip {
            if rule_ip != sender_ip {
                return false;
            }
        }
        // Check tag criterion
        if let Some(ref rule_tag) = self.tag {
            match tag {
                Some(t) if t == rule_tag.as_str() => {}
                _ => return false,
            }
        }
        // Check extension criterion
        if let Some(ref rule_ext) = self.extension {
            let file_ext = std::path::Path::new(filename)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            if !file_ext.eq_ignore_ascii_case(rule_ext.as_str()) {
                return false;
            }
        }
        true
    }
}

/// Collection of routing rules and the fallback directory
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RouterConfig {
    pub rules: Vec<RoutingRule>,
}

/// Router: resolves the destination directory for an incoming file
#[derive(Debug, Clone)]
pub struct Router {
    config: RouterConfig,
    default_dir: PathBuf,
}

impl Router {
    pub fn new(config: RouterConfig, default_dir: PathBuf) -> Self {
        Self {
            config,
            default_dir,
        }
    }

    /// Resolve destination directory.
    ///
    /// Iterates rules in order; returns the first matching rule's destination.
    /// Falls back to `default_dir` if no rule matches.
    pub fn resolve(&self, sender_ip: &str, tag: Option<&str>, filename: &str) -> PathBuf {
        for rule in &self.config.rules {
            if rule.matches(sender_ip, tag, filename) {
                tracing::debug!(
                    "Router: '{}' matched rule '{}' → {}",
                    filename,
                    rule.name,
                    rule.destination.display()
                );
                return rule.destination.clone();
            }
        }
        self.default_dir.clone()
    }

    pub fn default_dir(&self) -> &PathBuf {
        &self.default_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_router(rules: Vec<RoutingRule>, default_dir: &str) -> Router {
        Router::new(RouterConfig { rules }, PathBuf::from(default_dir))
    }

    // ── 1. No rules → fallback to default ────────────────────────────────────
    #[test]
    fn test_no_rules_returns_default() {
        let r = make_router(vec![], "/default");
        assert_eq!(
            r.resolve("1.2.3.4", None, "file.bin"),
            PathBuf::from("/default")
        );
    }

    // ── 2. IP rule matches ─────────────────────────────────────────────────────
    #[test]
    fn test_ip_rule_matches() {
        let rule = RoutingRule {
            name: "internal".into(),
            destination: PathBuf::from("/internal"),
            tag: None,
            sender_ip: Some("192.168.1.100".into()),
            extension: None,
        };
        let r = make_router(vec![rule], "/default");
        assert_eq!(
            r.resolve("192.168.1.100", None, "data.bin"),
            PathBuf::from("/internal")
        );
        // Wrong IP → default
        assert_eq!(
            r.resolve("10.0.0.1", None, "data.bin"),
            PathBuf::from("/default")
        );
    }

    // ── 3. Extension rule matches (case-insensitive) ───────────────────────────
    #[test]
    fn test_extension_rule_case_insensitive() {
        let rule = RoutingRule {
            name: "images".into(),
            destination: PathBuf::from("/images"),
            tag: None,
            sender_ip: None,
            extension: Some("PNG".into()),
        };
        let r = make_router(vec![rule], "/default");
        assert_eq!(
            r.resolve("1.2.3.4", None, "photo.png"),
            PathBuf::from("/images")
        );
        assert_eq!(
            r.resolve("1.2.3.4", None, "photo.PNG"),
            PathBuf::from("/images")
        );
        // different extension → default
        assert_eq!(
            r.resolve("1.2.3.4", None, "photo.jpg"),
            PathBuf::from("/default")
        );
    }

    // ── 4. Tag rule matches ────────────────────────────────────────────────────
    #[test]
    fn test_tag_rule_matches() {
        let rule = RoutingRule {
            name: "agent-a".into(),
            destination: PathBuf::from("/agent-a"),
            tag: Some("agent-a".into()),
            sender_ip: None,
            extension: None,
        };
        let r = make_router(vec![rule], "/default");
        assert_eq!(
            r.resolve("1.2.3.4", Some("agent-a"), "file.bin"),
            PathBuf::from("/agent-a")
        );
        assert_eq!(
            r.resolve("1.2.3.4", Some("agent-b"), "file.bin"),
            PathBuf::from("/default")
        );
        assert_eq!(
            r.resolve("1.2.3.4", None, "file.bin"),
            PathBuf::from("/default")
        );
    }

    // ── 5. First rule wins ────────────────────────────────────────────────────
    #[test]
    fn test_first_rule_wins() {
        let rules = vec![
            RoutingRule {
                name: "first".into(),
                destination: PathBuf::from("/first"),
                tag: None,
                sender_ip: None,
                extension: Some("bin".into()),
            },
            RoutingRule {
                name: "second".into(),
                destination: PathBuf::from("/second"),
                tag: None,
                sender_ip: None,
                extension: Some("bin".into()),
            },
        ];
        let r = make_router(rules, "/default");
        assert_eq!(
            r.resolve("1.2.3.4", None, "file.bin"),
            PathBuf::from("/first")
        );
    }

    // ── 6. Multi-criteria rule: all must match ─────────────────────────────────
    #[test]
    fn test_multi_criteria_all_must_match() {
        let rule = RoutingRule {
            name: "strict".into(),
            destination: PathBuf::from("/strict"),
            tag: Some("prod".into()),
            sender_ip: Some("10.0.0.1".into()),
            extension: Some("log".into()),
        };
        let r = make_router(vec![rule], "/default");
        // All match
        assert_eq!(
            r.resolve("10.0.0.1", Some("prod"), "app.log"),
            PathBuf::from("/strict")
        );
        // Wrong tag
        assert_eq!(
            r.resolve("10.0.0.1", Some("dev"), "app.log"),
            PathBuf::from("/default")
        );
        // Wrong IP
        assert_eq!(
            r.resolve("10.0.0.2", Some("prod"), "app.log"),
            PathBuf::from("/default")
        );
    }
}

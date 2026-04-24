//! RFC-004 rendezvous HTTP client (`/v1/peers/*` lookup).

use crate::core::{AeroSyncError, Result};
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;

/// Successful `GET /v1/peers/{name}` JSON body.
#[derive(Debug, Deserialize)]
pub struct PeerLookupBody {
    pub peer_id: String,
    pub name: String,
    pub public_key: String,
    pub capabilities: u64,
    pub observed_addr: Option<String>,
    pub last_seen_at: i64,
}

/// Minimal client: shared [`reqwest::Client`] + Bearer JWT (lookup scope).
#[derive(Clone)]
pub struct RendezvousClient {
    client: Arc<Client>,
    bearer_token: String,
}

impl RendezvousClient {
    pub fn new(client: Arc<Client>, bearer_token: String) -> Self {
        Self {
            client,
            bearer_token,
        }
    }

    /// `rendezvous_base` must include scheme, e.g. `http://rv.example.com:8787`.
    pub async fn lookup_peer(&self, rendezvous_base: &str, name: &str) -> Result<PeerLookupBody> {
        let url = format!(
            "{}/v1/peers/{}",
            rendezvous_base.trim_end_matches('/'),
            urlencoding::encode(name)
        );
        let resp = self
            .client
            .get(url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.bearer_token),
            )
            .send()
            .await
            .map_err(|e| {
                AeroSyncError::InvalidConfig(format!("rendezvous lookup request failed: {e}"))
            })?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| AeroSyncError::InvalidConfig(format!("rendezvous lookup body: {e}")))?;
        if !status.is_success() {
            let snippet = truncate_error_body(&text, 512);
            return Err(AeroSyncError::InvalidConfig(format!(
                "rendezvous lookup HTTP {status}: {snippet}"
            )));
        }
        serde_json::from_str(&text).map_err(|e| {
            let snippet = truncate_error_body(&text, 512);
            AeroSyncError::InvalidConfig(format!("rendezvous lookup JSON: {e}; body={snippet}"))
        })
    }
}

fn truncate_error_body(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}… ({} bytes total)", s.len())
}

/// Parse `peer@rendezvous-authority` when there is no `://` scheme.
/// Authority is `host[:port]` as passed to `http://` later. Returns `(peer_name, authority)`.
pub fn parse_peer_at_rendezvous(dest: &str) -> Option<(String, String)> {
    if dest.contains("://") || !dest.contains('@') {
        return None;
    }
    let (name, authority) = dest.rsplit_once('@')?;
    if name.is_empty() || authority.is_empty() || name.contains('@') {
        return None;
    }
    Some((name.to_string(), authority.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple() {
        let (n, a) = parse_peer_at_rendezvous("alice@127.0.0.1:8787").unwrap();
        assert_eq!(n, "alice");
        assert_eq!(a, "127.0.0.1:8787");
    }

    #[test]
    fn parse_rejects_url_scheme() {
        assert!(parse_peer_at_rendezvous("http://x@y").is_none());
    }
}

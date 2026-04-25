//! RFC-004 rendezvous HTTP client (`/v1/peers/*` lookup).

use crate::core::{AeroSyncError, Result};
use reqwest::header::HeaderName;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Must match `aerosync-rendezvous` `peers::NAMESPACE_HEADER` (RFC-004 P2 multitenant registry).
static RENDEZVOUS_NAMESPACE_HEADER: HeaderName = HeaderName::from_static("x-aerosync-namespace");

/// Successful `GET /v1/peers/{name}` JSON body.
#[derive(Debug, Deserialize)]
pub struct PeerLookupBody {
    pub peer_id: String,
    /// Empty when the peer is in the default namespace (P2).
    #[serde(default)]
    pub namespace: String,
    pub name: String,
    pub public_key: String,
    pub capabilities: u64,
    pub observed_addr: Option<String>,
    pub last_seen_at: i64,
}

/// `POST /v1/sessions/initiate` request body (RFC-004 P2).
#[derive(Debug, Serialize)]
struct InitiateRequest {
    target_name: String,
    #[serde(default)]
    target_namespace: String,
}

/// `POST /v1/sessions/initiate` success JSON — R2 signaling hop before hole punch.
#[derive(Debug, Deserialize)]
pub struct InitiateSessionResponse {
    pub session_id: String,
    #[serde(default)]
    pub implementation_status: serde_json::Value,
    pub signaling: InitiateSessionSignaling,
    #[serde(default)]
    pub stun: Option<serde_json::Value>,
}

/// WebSocket path and data-plane note from the rendezvous server.
#[derive(Debug, Deserialize)]
pub struct InitiateSessionSignaling {
    /// Relative path, e.g. `/v1/sessions/{id}/ws`.
    pub websocket_path: String,
    pub quic_data_plane: String,
}

/// Minimal client: shared [`reqwest::Client`] + Bearer JWT (lookup scope).
#[derive(Clone)]
pub struct RendezvousClient {
    client: Arc<Client>,
    bearer_token: String,
    /// Sent as `X-AeroSync-Namespace` on lookup; must match the JWT `ns` claim.
    namespace: String,
}

impl RendezvousClient {
    /// P2 registry partition; empty means default namespace.
    #[inline]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn new(client: Arc<Client>, bearer_token: String) -> Self {
        Self::new_with_namespace(client, bearer_token, String::new())
    }

    /// `namespace` is the tenant partition; use `""` for the default (single-tenant) registry.
    pub fn new_with_namespace(
        client: Arc<Client>,
        bearer_token: String,
        namespace: impl Into<String>,
    ) -> Self {
        Self {
            client,
            bearer_token,
            namespace: namespace.into(),
        }
    }

    /// `rendezvous_base` must include scheme, e.g. `http://rv.example.com:8787`.
    pub async fn lookup_peer(&self, rendezvous_base: &str, name: &str) -> Result<PeerLookupBody> {
        let url = format!(
            "{}/v1/peers/{}",
            rendezvous_base.trim_end_matches('/'),
            urlencoding::encode(name)
        );
        let mut req = self.client.get(url).header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", self.bearer_token),
        );
        if !self.namespace.is_empty() {
            req = req.header(&RENDEZVOUS_NAMESPACE_HEADER, self.namespace.as_str());
        }
        let resp = req.send().await.map_err(|e| {
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

    /// `POST /v1/sessions/initiate` — broker a P2P session and return
    /// [`InitiateSessionResponse`] (WebSocket path for R2 signaling;
    /// relay remains HTTP 501 on separate routes until R3). Uses the
    /// same Bearer token and `X-AeroSync-Namespace` as [`Self::lookup_peer`].
    pub async fn initiate_session(
        &self,
        rendezvous_base: &str,
        target_name: &str,
    ) -> Result<InitiateSessionResponse> {
        self.initiate_session_with_ns(rendezvous_base, target_name, None)
            .await
    }

    /// Like [`Self::initiate_session`] with optional P2
    /// `target_namespace` (default empty = JWT namespace only).
    pub async fn initiate_session_with_ns(
        &self,
        rendezvous_base: &str,
        target_name: &str,
        target_namespace: Option<&str>,
    ) -> Result<InitiateSessionResponse> {
        let url = format!(
            "{}/v1/sessions/initiate",
            rendezvous_base.trim_end_matches('/')
        );
        let body = InitiateRequest {
            target_name: target_name.to_string(),
            target_namespace: target_namespace.unwrap_or("").to_string(),
        };
        let mut req = self.client.post(&url).json(&body).header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", self.bearer_token),
        );
        if !self.namespace.is_empty() {
            req = req.header(&RENDEZVOUS_NAMESPACE_HEADER, self.namespace.as_str());
        }
        let resp = req
            .send()
            .await
            .map_err(|e| AeroSyncError::InvalidConfig(format!("rendezvous initiate: {e}")))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| AeroSyncError::InvalidConfig(format!("initiate body: {e}")))?;
        if !status.is_success() {
            let snippet = truncate_error_body(&text, 512);
            return Err(AeroSyncError::InvalidConfig(format!(
                "initiate HTTP {status}: {snippet}"
            )));
        }
        serde_json::from_str(&text).map_err(|e| {
            let snippet = truncate_error_body(&text, 512);
            AeroSyncError::InvalidConfig(format!("initiate JSON: {e}; body={snippet}"))
        })
    }

    /// Build a `ws://` / `wss://` URL for [`InitiateSessionSignaling::websocket_path`] with
    /// `?token=<JWT>` (same token as `Authorization: Bearer` on the HTTP API).
    pub fn signaling_websocket_url(
        &self,
        rendezvous_base: &str,
        websocket_path: &str,
    ) -> Result<String> {
        super::punch_signaling::rendezvous_signaling_websocket_url(
            rendezvous_base,
            websocket_path,
            &self.bearer_token,
        )
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

    #[test]
    fn client_stores_tenant_namespace() {
        let c = Client::new();
        let r = RendezvousClient::new_with_namespace(Arc::new(c), "t".into(), "acme");
        assert_eq!(r.namespace(), "acme");
    }
}

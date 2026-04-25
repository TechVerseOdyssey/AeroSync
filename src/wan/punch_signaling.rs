//! R2 client: WebSocket signaling toward [`super::super::RendezvousClient`].
//!
//! Pairs with `aerosync-rendezvous`’s `GET /v1/sessions/{id}/ws?token=…` to relay
//! `candidates`, receive `remote.candidates`, and obtain a server-synchronized
//! `punch_at` (RFC-004 §6.2) before a UDP/QUIC data path.

use crate::core::{AeroSyncError, Result};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use url::Url;

/// `GET …/ws?token=` uses the same JWT as `Authorization: Bearer` on HTTP APIs.
pub fn rendezvous_signaling_websocket_url(
    rendezvous_http_base: &str,
    websocket_path: &str,
    query_token: &str,
) -> Result<String> {
    let b = rendezvous_http_base.trim().trim_end_matches('/');
    if b.is_empty() {
        return Err(AeroSyncError::InvalidConfig(
            "empty rendezvous base URL".to_string(),
        ));
    }
    // Join base + path into one absolute URL, then switch scheme to ws(s).
    let p = if websocket_path.starts_with('/') {
        websocket_path
    } else {
        // rare; avoid double-slash
        return Err(AeroSyncError::InvalidConfig(
            "signaling `websocket_path` should start with `/` (RFC-004 P2 response)".to_string(),
        ));
    };
    let joined = if b.contains("://") {
        format!("{b}{p}")
    } else {
        format!("http://{b}{p}")
    };
    let u = Url::parse(&joined).map_err(|e| {
        AeroSyncError::InvalidConfig(format!("invalid rendezvous or WS path URL: {e}"))
    })?;
    let scheme = match u.scheme() {
        "https" => "wss",
        "http" => "ws",
        other => {
            return Err(AeroSyncError::InvalidConfig(format!(
                "expected http(s) rendezvous base, got scheme {other}"
            )));
        }
    };
    let enc = urlencoding::encode(query_token);
    let mut wu = u;
    wu.set_scheme(scheme)
        .map_err(|()| AeroSyncError::InvalidConfig("cannot set ws(s) scheme".to_string()))?;
    wu.set_query(None);
    wu.set_fragment(None);
    let out = format!("{wu}?token={enc}");
    Ok(out)
}

/// Parsed `remote.candidates` from the peer.
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteCandidates {
    /// `"initiator"` or `"target"` (R2 relay).
    pub from: String,
    pub server_reflexive: String,
    pub local: Vec<String>,
}

/// Synchronized `punch_at` from the rendezvous (Unix epoch ms, wall clock).
#[derive(Debug, Clone, Deserialize)]
pub struct PunchAt {
    pub timestamp_ms: u64,
}

/// Send this peer’s `candidates`, collect `remote.candidates` lines, and block until `punch_at`
/// (or the WebSocket closes or returns an error frame).
pub async fn exchange_candidates_and_wait_punch(
    ws_url: &str,
    server_reflexive: &str,
    local_addrs: Vec<String>,
) -> Result<(PunchAt, Vec<RemoteCandidates>)> {
    exchange_candidates_and_wait_punch_with_timeouts(
        ws_url,
        server_reflexive,
        local_addrs,
        std::time::Duration::from_secs(5),
        std::time::Duration::from_secs(8),
    )
    .await
}

/// Like [`exchange_candidates_and_wait_punch`] but with explicit per-stage timeouts.
///
/// Timeout/failure codes are deterministic so caller-side mapping can stay stable:
/// - `[R2_TIMEOUT_WS]` for initial WebSocket connect/send budget
/// - `[R2_TIMEOUT_PUNCH]` for waiting `remote.candidates`/`punch_at`
pub async fn exchange_candidates_and_wait_punch_with_timeouts(
    ws_url: &str,
    server_reflexive: &str,
    local_addrs: Vec<String>,
    ws_connect_timeout: std::time::Duration,
    punch_wait_timeout: std::time::Duration,
) -> Result<(PunchAt, Vec<RemoteCandidates>)> {
    let (mut ws, _) =
        tokio::time::timeout(ws_connect_timeout, tokio_tungstenite::connect_async(ws_url))
            .await
            .map_err(|_| {
                AeroSyncError::Network(format!(
                    "[R2_TIMEOUT_WS] signaling websocket connect timed out after {} ms",
                    ws_connect_timeout.as_millis()
                ))
            })?
            .map_err(|e| AeroSyncError::Network(format!("R2 signaling WebSocket connect: {e}")))?;

    let hello = json!({
        "type": "candidates",
        "server_reflexive": server_reflexive,
        "local": local_addrs
    });
    tokio::time::timeout(
        ws_connect_timeout,
        ws.send(WsMessage::Text(hello.to_string().into())),
    )
    .await
    .map_err(|_| {
        AeroSyncError::Network(format!(
            "[R2_TIMEOUT_WS] signaling candidates send timed out after {} ms",
            ws_connect_timeout.as_millis()
        ))
    })?
    .map_err(|e| AeroSyncError::Network(format!("R2 signaling send candidates: {e}")))?;

    let mut remotes: Vec<RemoteCandidates> = Vec::new();
    let deadline = tokio::time::Instant::now() + punch_wait_timeout;

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(AeroSyncError::Network(format!(
                "[R2_TIMEOUT_PUNCH] signaling did not deliver punch_at within {} ms",
                punch_wait_timeout.as_millis()
            )));
        }
        let wait = deadline.saturating_duration_since(now);
        let frm = tokio::time::timeout(wait, ws.next()).await.map_err(|_| {
            AeroSyncError::Network(format!(
                "[R2_TIMEOUT_PUNCH] signaling did not deliver punch_at within {} ms",
                punch_wait_timeout.as_millis()
            ))
        })?;
        let Some(frm) = frm else {
            break;
        };
        let m = match frm {
            Ok(m) => m,
            Err(e) => {
                return Err(AeroSyncError::Network(format!(
                    "R2 signaling WebSocket read: {e}"
                )))
            }
        };
        if let WsMessage::Text(s) = m {
            if let Some(ev) = parse_incoming(&s)? {
                match ev {
                    Incoming::Remote(r) => remotes.push(r),
                    Incoming::Punch(p) => {
                        return Ok((p, remotes));
                    }
                }
            }
        }
    }
    Err(AeroSyncError::Network(
        "R2 signaling closed before punch_at".to_string(),
    ))
}

enum Incoming {
    Remote(RemoteCandidates),
    Punch(PunchAt),
}

fn parse_incoming(line: &str) -> Result<Option<Incoming>> {
    let v: serde_json::Value = serde_json::from_str(line).map_err(|e| {
        AeroSyncError::InvalidConfig(format!("R2 signaling bad JSON: {e}; line={line:?}"))
    })?;
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if ty == "aerosync.signaling.ready" || ty.is_empty() {
        return Ok(None);
    }
    if ty == "aerosync.signaling.error" {
        let s = v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown");
        return Err(AeroSyncError::InvalidConfig(format!(
            "R2 signaling error: {s}"
        )));
    }
    if ty == "remote.candidates" {
        let r: RemoteCandidates = serde_json::from_value(v)
            .map_err(|e| AeroSyncError::InvalidConfig(format!("R2 remote.candidates: {e}")))?;
        return Ok(Some(Incoming::Remote(r)));
    }
    if ty == "punch_at" {
        let t = v
            .get("timestamp_ms")
            .and_then(|x| x.as_u64())
            .ok_or_else(|| {
                AeroSyncError::InvalidConfig("punch_at missing timestamp_ms".to_string())
            })?;
        return Ok(Some(Incoming::Punch(PunchAt { timestamp_ms: t })));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    #[test]
    fn ws_url_from_http_base() {
        let s = rendezvous_signaling_websocket_url(
            "http://127.0.0.1:8787",
            "/v1/sessions/x/ws",
            "tok%20en",
        )
        .unwrap();
        assert!(s.starts_with("ws://127.0.0.1:8787/v1/sessions/x/ws?token="));
    }

    /// Server accepts TCP but never completes the WebSocket HTTP upgrade, so
    /// `connect_async` blocks until the client budget elapses.
    #[tokio::test]
    async fn exchange_fails_r2_timeout_ws_when_handshake_stalls() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_addr = listener.local_addr().unwrap();
        let _stalled = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.expect("accept");
            std::future::pending::<()>().await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let err = exchange_candidates_and_wait_punch_with_timeouts(
            &format!("ws://{server_addr}/"),
            "192.0.2.1:1",
            vec![],
            Duration::from_millis(500),
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();
        let s = err.to_string();
        assert!(s.contains("[R2_TIMEOUT_WS]"), "got: {s}");
    }

    /// WebSocket is up and candidates are sent, but the peer never supplies
    /// `punch_at` before `punch_wait_timeout` elapses.
    #[tokio::test]
    async fn exchange_fails_r2_timeout_punch_when_punch_at_missing() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_addr = listener.local_addr().unwrap();
        let _srv = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut ws = accept_async(stream).await.expect("ws accept");
            if let Some(Ok(WsMessage::Text(_))) = ws.next().await {
                // no remote.candidates, no punch_at
            }
            std::future::pending::<()>().await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let err = exchange_candidates_and_wait_punch_with_timeouts(
            &format!("ws://{server_addr}/"),
            "192.0.2.1:1",
            vec!["10.0.0.1:1".to_string()],
            Duration::from_secs(3),
            Duration::from_millis(200),
        )
        .await
        .unwrap_err();
        let s = err.to_string();
        assert!(s.contains("[R2_TIMEOUT_PUNCH]"), "got: {s}");
    }
}

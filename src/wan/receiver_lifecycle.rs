//! Receiver-side WAN rendezvous lifecycle (R2):
//! - optional register + heartbeat loop (env-gated)
//! - optional pending-session polling + automatic signaling WS participation
//!
//! This module is intentionally best-effort and non-fatal for receiver startup.

use crate::wan::punch_signaling::{
    exchange_candidates_and_wait_punch, rendezvous_signaling_websocket_url,
};
use reqwest::{header::HeaderName, Client};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

static RENDEZVOUS_NAMESPACE_HEADER: HeaderName = HeaderName::from_static("x-aerosync-namespace");

const ENV_URL: &str = "AEROSYNC_RENDEZVOUS_URL";
const ENV_NAME: &str = "AEROSYNC_RENDEZVOUS_NAME";
const ENV_PUBLIC_KEY_B64: &str = "AEROSYNC_RENDEZVOUS_PUBLIC_KEY";
const ENV_NAMESPACE: &str = "AEROSYNC_RENDEZVOUS_NAMESPACE";
const ENV_CAPABILITIES: &str = "AEROSYNC_RENDEZVOUS_CAPABILITIES";
const ENV_OBSERVED_ADDR: &str = "AEROSYNC_RENDEZVOUS_OBSERVED_ADDR";
const ENV_HEARTBEAT_SECS: &str = "AEROSYNC_RENDEZVOUS_HEARTBEAT_SECS";
const ENV_POLL_SECS: &str = "AEROSYNC_RENDEZVOUS_SESSION_POLL_SECS";

#[derive(Clone, Debug)]
struct ReceiverRendezvousConfig {
    base_url: String,
    name: String,
    public_key_b64: String,
    namespace: String,
    capabilities: u32,
    observed_addr: Option<String>,
    heartbeat_secs: u64,
    session_poll_secs: u64,
}

#[derive(Debug, Serialize)]
struct RegisterRequest {
    name: String,
    public_key: String,
    capabilities: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    observed_addr: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RegisterResponse {
    jwt: String,
}

#[derive(Debug, Serialize)]
struct HeartbeatRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    observed_addr: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HeartbeatResponse {
    jwt: String,
}

#[derive(Debug, Deserialize)]
struct PendingSessionsResponse {
    #[serde(default)]
    sessions: Vec<PendingSession>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PendingSession {
    session_id: String,
    websocket_path: String,
}

impl ReceiverRendezvousConfig {
    fn from_env() -> Option<Self> {
        let base_url = std::env::var(ENV_URL).ok()?.trim().to_string();
        let name = std::env::var(ENV_NAME).ok()?.trim().to_string();
        let public_key_b64 = std::env::var(ENV_PUBLIC_KEY_B64).ok()?.trim().to_string();
        if base_url.is_empty() || name.is_empty() || public_key_b64.is_empty() {
            return None;
        }
        let namespace = std::env::var(ENV_NAMESPACE).unwrap_or_default();
        let observed_addr = std::env::var(ENV_OBSERVED_ADDR)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let capabilities = std::env::var(ENV_CAPABILITIES)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(3);
        let heartbeat_secs = std::env::var(ENV_HEARTBEAT_SECS)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(30);
        let session_poll_secs = std::env::var(ENV_POLL_SECS)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(2);
        Some(Self {
            base_url,
            name,
            public_key_b64,
            namespace,
            capabilities,
            observed_addr,
            heartbeat_secs,
            session_poll_secs,
        })
    }
}

pub fn spawn_from_env(
    local_http_addr: Option<SocketAddr>,
    local_quic_addr: Option<SocketAddr>,
) -> Option<JoinHandle<()>> {
    let cfg = ReceiverRendezvousConfig::from_env()?;
    Some(tokio::spawn(async move {
        run_lifecycle(cfg, local_http_addr, local_quic_addr).await;
    }))
}

async fn run_lifecycle(
    cfg: ReceiverRendezvousConfig,
    local_http_addr: Option<SocketAddr>,
    local_quic_addr: Option<SocketAddr>,
) {
    let client = Client::new();
    let default_observed = cfg
        .observed_addr
        .clone()
        .or_else(|| local_quic_addr.map(|a| a.to_string()))
        .or_else(|| local_http_addr.map(|a| a.to_string()));
    let mut token = register_with_retry(&client, &cfg, default_observed.clone()).await;
    let active_sessions: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let mut hb_ticker = tokio::time::interval(Duration::from_secs(cfg.heartbeat_secs));
    let mut poll_ticker = tokio::time::interval(Duration::from_secs(cfg.session_poll_secs));
    loop {
        tokio::select! {
            _ = hb_ticker.tick() => {
                match heartbeat(&client, &cfg, &token, default_observed.clone()).await {
                    Ok(next) => token = next,
                    Err(e) => {
                        tracing::warn!(error=%e, "rendezvous heartbeat failed; re-registering");
                        token = register_with_retry(&client, &cfg, default_observed.clone()).await;
                    }
                }
            }
            _ = poll_ticker.tick() => {
                let pending = match pending_sessions(&client, &cfg, &token).await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::debug!(error=%e, "rendezvous pending-session poll failed");
                        continue;
                    }
                };
                for session in pending {
                    let mut guard = active_sessions.lock().await;
                    if !guard.insert(session.session_id.clone()) {
                        continue;
                    }
                    drop(guard);
                    let cfg_clone = cfg.clone();
                    let token_snapshot = token.clone();
                    let client_clone = client.clone();
                    let active = Arc::clone(&active_sessions);
                    let local_http = local_http_addr;
                    let local_quic = local_quic_addr;
                    tokio::spawn(async move {
                        if let Err(e) = join_session_signaling(
                            &client_clone,
                            &cfg_clone,
                            &token_snapshot,
                            &session,
                            local_http,
                            local_quic,
                        )
                        .await
                        {
                            tracing::warn!(session_id=%session.session_id, error=%e, "receiver signaling participation failed");
                        }
                        active.lock().await.remove(&session.session_id);
                    });
                }
            }
        }
    }
}

async fn register_with_retry(
    client: &Client,
    cfg: &ReceiverRendezvousConfig,
    observed_addr: Option<String>,
) -> String {
    loop {
        match register(client, cfg, observed_addr.clone()).await {
            Ok(jwt) => return jwt,
            Err(e) => {
                tracing::warn!(error=%e, "rendezvous register failed; retrying in 2s");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

fn maybe_ns_header(
    req: reqwest::RequestBuilder,
    cfg: &ReceiverRendezvousConfig,
) -> reqwest::RequestBuilder {
    if cfg.namespace.is_empty() {
        req
    } else {
        req.header(&RENDEZVOUS_NAMESPACE_HEADER, cfg.namespace.as_str())
    }
}

async fn register(
    client: &Client,
    cfg: &ReceiverRendezvousConfig,
    observed_addr: Option<String>,
) -> anyhow::Result<String> {
    let url = format!("{}/v1/peers/register", cfg.base_url.trim_end_matches('/'));
    let body = RegisterRequest {
        name: cfg.name.clone(),
        public_key: cfg.public_key_b64.clone(),
        capabilities: cfg.capabilities,
        observed_addr,
    };
    let resp = maybe_ns_header(client.post(url), cfg)
        .json(&body)
        .send()
        .await?;
    let resp = resp.error_for_status()?;
    let body: RegisterResponse = resp.json().await?;
    Ok(body.jwt)
}

async fn heartbeat(
    client: &Client,
    cfg: &ReceiverRendezvousConfig,
    token: &str,
    observed_addr: Option<String>,
) -> anyhow::Result<String> {
    let url = format!("{}/v1/peers/heartbeat", cfg.base_url.trim_end_matches('/'));
    let body = HeartbeatRequest { observed_addr };
    let resp = maybe_ns_header(client.post(url), cfg)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await?;
    let resp = resp.error_for_status()?;
    let body: HeartbeatResponse = resp.json().await?;
    Ok(body.jwt)
}

async fn pending_sessions(
    client: &Client,
    cfg: &ReceiverRendezvousConfig,
    token: &str,
) -> anyhow::Result<Vec<PendingSession>> {
    let url = format!("{}/v1/sessions/pending", cfg.base_url.trim_end_matches('/'));
    let resp = maybe_ns_header(client.get(url), cfg)
        .bearer_auth(token)
        .send()
        .await?;
    let resp = resp.error_for_status()?;
    let body: PendingSessionsResponse = resp.json().await?;
    Ok(body.sessions)
}

async fn join_session_signaling(
    _client: &Client,
    cfg: &ReceiverRendezvousConfig,
    token: &str,
    pending: &PendingSession,
    local_http_addr: Option<SocketAddr>,
    local_quic_addr: Option<SocketAddr>,
) -> anyhow::Result<()> {
    let ws_url = rendezvous_signaling_websocket_url(&cfg.base_url, &pending.websocket_path, token)?;
    let mut local_candidates = Vec::new();
    if let Some(addr) = local_quic_addr {
        local_candidates.push(addr.to_string());
    }
    if let Some(addr) = local_http_addr {
        local_candidates.push(addr.to_string());
    }
    let srflx = cfg
        .observed_addr
        .clone()
        .or_else(|| local_quic_addr.map(|a| a.to_string()))
        .or_else(|| local_http_addr.map(|a| a.to_string()))
        .unwrap_or_else(|| "0.0.0.0:0".to_string());
    let _ = exchange_candidates_and_wait_punch(&ws_url, &srflx, local_candidates).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::ws::{Message, WebSocketUpgrade};
    use axum::extract::{Path, Query, State};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use base64::Engine as _;
    use futures::StreamExt;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct TestState {
        register_calls: Arc<AtomicUsize>,
        heartbeat_calls: Arc<AtomicUsize>,
        ws_calls: Arc<AtomicUsize>,
        ws_candidates: Arc<AtomicUsize>,
        pending: Arc<Mutex<VecDeque<Vec<PendingSession>>>>,
    }

    #[derive(Deserialize)]
    struct WsTokenQuery {
        token: String,
    }

    #[tokio::test]
    async fn lifecycle_registers_and_heartbeats() {
        let (base_url, state, _server) = start_test_server(Vec::new()).await;
        let cfg = ReceiverRendezvousConfig {
            base_url,
            name: "bob".into(),
            public_key_b64: base64::engine::general_purpose::STANDARD.encode([7u8; 32]),
            namespace: String::new(),
            capabilities: 3,
            observed_addr: Some("127.0.0.1:7789".into()),
            heartbeat_secs: 1,
            session_poll_secs: 1,
        };
        let handle = tokio::spawn(run_lifecycle(
            cfg,
            Some("127.0.0.1:7788".parse().unwrap()),
            None,
        ));
        tokio::time::sleep(Duration::from_millis(2200)).await;
        handle.abort();
        assert!(state.register_calls.load(Ordering::SeqCst) >= 1);
        assert!(state.heartbeat_calls.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn lifecycle_auto_participates_in_pending_session_signaling() {
        let pending_once = vec![PendingSession {
            session_id: "s-auto-1".to_string(),
            websocket_path: "/v1/sessions/s-auto-1/ws".to_string(),
        }];
        let (base_url, state, _server) = start_test_server(vec![pending_once]).await;
        let cfg = ReceiverRendezvousConfig {
            base_url,
            name: "bob".into(),
            public_key_b64: base64::engine::general_purpose::STANDARD.encode([9u8; 32]),
            namespace: String::new(),
            capabilities: 3,
            observed_addr: Some("127.0.0.1:7789".into()),
            heartbeat_secs: 2,
            session_poll_secs: 1,
        };
        let handle = tokio::spawn(run_lifecycle(
            cfg,
            Some("127.0.0.1:7788".parse().unwrap()),
            None,
        ));
        tokio::time::sleep(Duration::from_millis(2300)).await;
        handle.abort();
        assert!(state.ws_calls.load(Ordering::SeqCst) >= 1);
        assert!(state.ws_candidates.load(Ordering::SeqCst) >= 1);
    }

    async fn start_test_server(
        pending_sequences: Vec<Vec<PendingSession>>,
    ) -> (String, TestState, tokio::task::JoinHandle<()>) {
        let state = TestState {
            register_calls: Arc::new(AtomicUsize::new(0)),
            heartbeat_calls: Arc::new(AtomicUsize::new(0)),
            ws_calls: Arc::new(AtomicUsize::new(0)),
            ws_candidates: Arc::new(AtomicUsize::new(0)),
            pending: Arc::new(Mutex::new(VecDeque::from(pending_sequences))),
        };
        let app = Router::new()
            .route("/v1/peers/register", post(test_register))
            .route("/v1/peers/heartbeat", post(test_heartbeat))
            .route("/v1/sessions/pending", get(test_pending))
            .route("/v1/sessions/:id/ws", get(test_ws))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });
        (format!("http://{addr}"), state, server)
    }

    async fn test_register(State(state): State<TestState>) -> Json<serde_json::Value> {
        state.register_calls.fetch_add(1, Ordering::SeqCst);
        Json(json!({"jwt":"jwt-register"}))
    }

    async fn test_heartbeat(State(state): State<TestState>) -> Json<serde_json::Value> {
        state.heartbeat_calls.fetch_add(1, Ordering::SeqCst);
        Json(json!({"jwt":"jwt-heartbeat"}))
    }

    async fn test_pending(State(state): State<TestState>) -> Json<serde_json::Value> {
        let mut pending = state.pending.lock().await;
        let sessions = pending.pop_front().unwrap_or_default();
        Json(json!({"sessions": sessions}))
    }

    async fn test_ws(
        ws: WebSocketUpgrade,
        Path(id): Path<String>,
        Query(q): Query<WsTokenQuery>,
        State(state): State<TestState>,
    ) -> impl axum::response::IntoResponse {
        state.ws_calls.fetch_add(1, Ordering::SeqCst);
        ws.on_upgrade(move |mut socket| async move {
            if q.token.is_empty() {
                return;
            }
            let _ = socket
                .send(Message::Text(
                    json!({"type":"aerosync.signaling.ready","session_id":id}).to_string(),
                ))
                .await;
            while let Some(Ok(msg)) = socket.next().await {
                if let Message::Text(text) = msg {
                    if text.contains("\"type\":\"candidates\"") {
                        state.ws_candidates.fetch_add(1, Ordering::SeqCst);
                        let _ = socket
                            .send(Message::Text(
                                json!({
                                    "type":"remote.candidates",
                                    "from":"initiator",
                                    "server_reflexive":"127.0.0.1:9001",
                                    "local":[]
                                })
                                .to_string(),
                            ))
                            .await;
                        let _ = socket
                            .send(Message::Text(
                                json!({"type":"punch_at","timestamp_ms":0}).to_string(),
                            ))
                            .await;
                        break;
                    }
                }
            }
        })
    }
}

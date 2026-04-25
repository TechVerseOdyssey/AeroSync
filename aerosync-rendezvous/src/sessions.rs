//! Session signaling: `POST /v1/sessions/initiate`, `GET /v1/sessions/:id/ws`, relay stubs (RFC-004 P2).

use crate::jwt::verify_bearer;
use crate::peers;
use crate::peers::authorization_bearer;
use crate::signaling::PeerRole;
use crate::AppState;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::Query;
use axum::{extract::Path, extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct InitiateRequest {
    pub target_name: String,
    #[serde(default)]
    pub target_namespace: String,
}

#[derive(Debug, Serialize)]
pub struct InitiateResponse {
    pub session_id: String,
    pub implementation_status: P2Status,
    pub signaling: SignalingInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stun: Option<StunInfo>,
}

#[derive(Debug, Serialize)]
pub struct P2Status {
    pub ice_lite: &'static str,
    pub r3_relay: &'static str,
}

#[derive(Debug, Serialize)]
pub struct SignalingInfo {
    /// WebSocket path (relative) for the session; clients prepend rendezvous base URL and append their JWT: `?token=`.
    pub websocket_path: String,
    /// Data-plane QUIC uses a separate local socket and port from this HTTP/WS control plane; see `docs/rfcs/RFC-004-p2-protocol-security.md`.
    pub quic_data_plane: &'static str,
}

// Reserved for `InitiateResponse.stun` when a STUN URL is operator-configured.
#[allow(dead_code)]
#[derive(Debug, Serialize)]
pub struct StunInfo {
    pub url: String,
    pub policy: &'static str,
}

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct PendingSessionsResponse {
    pub sessions: Vec<PendingSession>,
}

#[derive(Debug, Serialize)]
pub struct PendingSession {
    pub session_id: String,
    pub websocket_path: String,
}

fn validate_target_namespace(s: &str) -> Result<String, (StatusCode, Json<Value>)> {
    if s.is_empty() {
        return Ok(String::new());
    }
    if s.len() > 64 {
        return Err(peers::bad_req("target_namespace must be at most 64 bytes"));
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        return Err(peers::bad_req(
            "target_namespace may only use ASCII alnum, '.', '-', and '_'",
        ));
    }
    Ok(s.to_string())
}

pub async fn not_implemented_relay() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "rfc": "RFC-004-R3",
            "error": "relay endpoints are not implemented yet; use TURN or deploy AeroSync R3 relay in a future release",
            "stun_policy": "optional_for_nat_reflexive; not_required_for_relay_501",
            "billing_tenant": "per-session + relay_usage table (prepared; accounting not wired in this build)",
        })),
    )
}

/// Create a `sessions` row and return signaling metadata.
pub async fn initiate_session(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<InitiateRequest>,
) -> Result<Json<InitiateResponse>, (StatusCode, Json<Value>)> {
    let token = authorization_bearer(&headers).ok_or_else(peers::unauthorized)?;
    let claims = verify_bearer(token, state.decoding_key.as_ref(), &state.jwt_issuer)
        .map_err(|_| peers::unauthorized())?;

    let target_namespace = validate_target_namespace(&body.target_namespace)?;
    if claims.ns != target_namespace {
        return Err(peers::forbidden());
    }
    if body.target_name.is_empty() || body.target_name.len() > 128 {
        return Err(peers::bad_req("target_name is invalid"));
    }

    let initiator_id = claims.sub;
    let target =
        sqlx::query("SELECT peer_id FROM peers WHERE namespace = ?1 AND name = ?2 COLLATE NOCASE")
            .bind(&target_namespace)
            .bind(&body.target_name)
            .fetch_optional(&state.pool)
            .await
            .map_err(peers::internal)?;

    let target_row = target.ok_or_else(|| peers::not_found("target peer"))?;
    let target_id: String = target_row.try_get("peer_id").map_err(peers::internal)?;

    if target_id == initiator_id {
        return Err(peers::bad_req("cannot start a session to yourself"));
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let session_id = Uuid::new_v4().to_string();

    sqlx::query(
        "INSERT INTO sessions (session_id, initiator_id, target_id, state, created_at) VALUES (?1, ?2, ?3, 'pending', ?4)",
    )
    .bind(&session_id)
    .bind(&initiator_id)
    .bind(&target_id)
    .bind(now)
    .execute(&state.pool)
    .await
    .map_err(peers::internal)?;

    let ws_path = format!("/v1/sessions/{}/ws", session_id);
    Ok(Json(InitiateResponse {
        session_id: session_id.clone(),
        implementation_status: P2Status {
            ice_lite: "ws_relay_of_candidates_and_punch_at; client_udp_quic_data_plane_ongoing",
            r3_relay: "not_implemented;_relay_routes_still_501",
        },
        signaling: SignalingInfo {
            websocket_path: ws_path,
            quic_data_plane: "independent_sockets; QUIC/DTLS data plane is not the HTTP/WS port",
        },
        stun: None,
    }))
}

#[derive(Debug, Deserialize)]
pub struct SessionIdPath {
    pub id: String,
}

pub async fn session_websocket(
    ws: WebSocketUpgrade,
    Path(path): Path<SessionIdPath>,
    Query(q): Query<WsQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, (StatusCode, Json<Value>)> {
    let session_id = path.id;
    let claims = verify_bearer(&q.token, state.decoding_key.as_ref(), &state.jwt_issuer)
        .map_err(|_| peers::unauthorized())?;
    let peer = claims.sub;
    let row = sqlx::query("SELECT initiator_id, target_id FROM sessions WHERE session_id = ?1")
        .bind(&session_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(peers::internal)?;
    let row = row.ok_or_else(|| peers::not_found("session"))?;
    let a: String = row.try_get("initiator_id").map_err(peers::internal)?;
    let b: String = row.try_get("target_id").map_err(peers::internal)?;
    if peer != a && peer != b {
        return Err(peers::forbidden());
    }

    let role = if peer == a {
        PeerRole::Initiator
    } else {
        PeerRole::Target
    };
    let signaling = Arc::clone(&state.signaling);
    let sid = session_id;

    Ok(ws.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::Message;
        use tokio::sync::mpsc;

        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let init = match signaling.register_peer(&sid, role, out_tx) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(%e, session_id = %sid, "signaling register");
                let _ = socket
                    .send(Message::Text(
                        json!({"type":"aerosync.signaling.error","error":e}).to_string(),
                    ))
                    .await;
                return;
            }
        };
        for line in init {
            if socket.send(Message::Text(line)).await.is_err() {
                signaling.disconnect(&sid, role);
                return;
            }
        }

        let reg = Arc::clone(&signaling);
        let id = sid.clone();
        loop {
            tokio::select! {
                t = out_rx.recv() => {
                    let Some(text) = t else { break };
                    if socket.send(Message::Text(text)).await.is_err() { break; }
                }
                msg = socket.recv() => {
                    let Some(m) = msg else { break };
                    match m {
                        Ok(Message::Text(t)) if reg.on_client_text(&id, role, &t).is_err() => {
                            break;
                        }
                        Ok(Message::Close(_)) | Err(_) => break,
                        _ => {}
                    }
                }
            }
        }
        reg.disconnect(&id, role);
    }))
}

/// Poll pending sessions for the authenticated target peer.
/// Used by receiver-side agents to auto-join R2 signaling without manual scripts.
pub async fn pending_sessions(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<PendingSessionsResponse>, (StatusCode, Json<Value>)> {
    let token = authorization_bearer(&headers).ok_or_else(peers::unauthorized)?;
    let claims = verify_bearer(token, state.decoding_key.as_ref(), &state.jwt_issuer)
        .map_err(|_| peers::unauthorized())?;
    let namespace = peers::parse_namespace_from_headers(&headers)?;
    if namespace != claims.ns {
        return Err(peers::forbidden());
    }
    let rows = sqlx::query(
        "SELECT session_id FROM sessions
         WHERE target_id = ?1 AND state IN ('pending','punching')
         ORDER BY created_at ASC LIMIT 32",
    )
    .bind(&claims.sub)
    .fetch_all(&state.pool)
    .await
    .map_err(peers::internal)?;
    let sessions = rows
        .into_iter()
        .filter_map(|row| row.try_get::<String, _>("session_id").ok())
        .map(|session_id| PendingSession {
            websocket_path: format!("/v1/sessions/{session_id}/ws"),
            session_id,
        })
        .collect();
    Ok(Json(PendingSessionsResponse { sessions }))
}

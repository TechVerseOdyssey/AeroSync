//! `/v1/peers/*` HTTP handlers (RFC-004 §5.1–5.3).

use crate::jwt::{issue_token, verify_bearer};
use crate::AppState;
use axum::{
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub name: String,
    /// Base64-encoded 32-byte Ed25519 public key.
    pub public_key: String,
    pub capabilities: u32,
    #[serde(default)]
    pub observed_addr: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub peer_id: String,
    pub jwt: String,
    /// RFC-3339 UTC
    pub jwt_expires_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_addr: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct HeartbeatRequest {
    #[serde(default)]
    pub observed_addr: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct HeartbeatResponse {
    pub jwt: String,
    pub jwt_expires_at: String,
}

#[derive(Debug, Serialize)]
pub struct PeerLookupResponse {
    pub peer_id: String,
    pub name: String,
    /// Base64-encoded raw public key bytes.
    pub public_key: String,
    pub capabilities: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_addr: Option<String>,
    pub last_seen_at: i64,
}

fn peer_id_from_pubkey(pubkey: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(pubkey);
    format!("sha256:{}", hex::encode(d))
}

fn authorization_bearer(headers: &HeaderMap) -> Option<&str> {
    let hv = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    hv.strip_prefix("Bearer ").or_else(|| hv.strip_prefix("bearer "))
}

fn observed_from_connect(addr: SocketAddr) -> String {
    format!("{}:{}", addr.ip(), addr.port())
}

pub async fn register_peer(
    State(state): State<Arc<AppState>>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>, (StatusCode, Json<Value>)> {
    let pubkey_bytes = B64
        .decode(body.public_key.trim())
        .map_err(|_| bad_req("public_key must be standard base64"))?;
    if pubkey_bytes.len() != 32 {
        return Err(bad_req("public_key must decode to 32 bytes (Ed25519)"));
    }

    let peer_id = peer_id_from_pubkey(&pubkey_bytes);
    let observed = body
        .observed_addr
        .clone()
        .or_else(|| headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).map(|s| {
            s.split(',')
                .next()
                .unwrap_or("")
                .trim()
                .to_string()
        }))
        .filter(|s| !s.is_empty())
        .or_else(|| Some(observed_from_connect(remote)));

    let now = unix_now();

    let existing = sqlx::query(
        "SELECT peer_id, public_key FROM peers WHERE name = ?1 COLLATE NOCASE",
    )
    .bind(&body.name)
    .fetch_optional(&state.pool)
    .await
    .map_err(internal)?;

    match existing {
        Some(row) => {
            let stored_id: String = row.try_get("peer_id").map_err(internal)?;
            let stored_pk: Vec<u8> = row.try_get("public_key").map_err(internal)?;
            if stored_pk != pubkey_bytes {
                return Err(conflict(
                    "name already registered with a different public_key",
                ));
            }
            debug_assert_eq!(stored_id, peer_id);
            sqlx::query(
                "UPDATE peers SET capabilities = ?1, observed_addr = ?2, last_seen_at = ?3 WHERE peer_id = ?4",
            )
            .bind(body.capabilities as i64)
            .bind(observed.clone())
            .bind(now as i64)
            .bind(&peer_id)
            .execute(&state.pool)
            .await
            .map_err(internal)?;
        }
        None => {
            sqlx::query(
                "INSERT INTO peers (peer_id, name, public_key, capabilities, observed_addr, last_seen_at, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .bind(&peer_id)
            .bind(&body.name)
            .bind(&pubkey_bytes[..])
            .bind(body.capabilities as i64)
            .bind(observed.clone())
            .bind(now as i64)
            .bind(now as i64)
            .execute(&state.pool)
            .await
            .map_err(internal)?;
        }
    }

    let (jwt, exp) = issue_token(
        state.encoding_key.as_ref(),
        &state.jwt_issuer,
        &peer_id,
        &body.name,
        state.jwt_ttl_secs,
    )
    .map_err(internal)?;

    let jwt_expires_at = chrono::DateTime::<chrono::Utc>::from_timestamp(exp as i64, 0)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339();

    Ok(Json(RegisterResponse {
        peer_id,
        jwt,
        jwt_expires_at,
        observed_addr: observed,
    }))
}

pub async fn heartbeat(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<HeartbeatRequest>,
) -> Result<Json<HeartbeatResponse>, (StatusCode, Json<Value>)> {
    let token = authorization_bearer(&headers).ok_or_else(|| unauthorized())?;
    let claims =
        verify_bearer(token, state.decoding_key.as_ref(), &state.jwt_issuer).map_err(|_| unauthorized())?;

    let observed = body.observed_addr.clone();
    let now = unix_now();

    let row = sqlx::query("SELECT peer_id FROM peers WHERE peer_id = ?1")
        .bind(&claims.sub)
        .fetch_optional(&state.pool)
        .await
        .map_err(internal)?;
    if row.is_none() {
        return Err(unauthorized());
    }

    sqlx::query(
        "UPDATE peers SET observed_addr = COALESCE(?1, observed_addr), last_seen_at = ?2 WHERE peer_id = ?3",
    )
    .bind(observed.clone())
    .bind(now as i64)
    .bind(&claims.sub)
    .execute(&state.pool)
    .await
    .map_err(internal)?;

    let (jwt, exp) = issue_token(
        state.encoding_key.as_ref(),
        &state.jwt_issuer,
        &claims.sub,
        &claims.name,
        state.jwt_ttl_secs,
    )
    .map_err(internal)?;

    let jwt_expires_at = chrono::DateTime::<chrono::Utc>::from_timestamp(exp as i64, 0)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339();

    Ok(Json(HeartbeatResponse {
        jwt,
        jwt_expires_at,
    }))
}

pub async fn lookup_peer(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<PeerLookupResponse>, (StatusCode, Json<Value>)> {
    let _ = verify_bearer(
        authorization_bearer(&headers).ok_or_else(|| unauthorized())?,
        state.decoding_key.as_ref(),
        &state.jwt_issuer,
    )
    .map_err(|_| unauthorized())?;

    let row = sqlx::query(
        "SELECT peer_id, name, public_key, capabilities, observed_addr, last_seen_at FROM peers WHERE name = ?1 COLLATE NOCASE",
    )
    .bind(&name)
    .fetch_optional(&state.pool)
    .await
    .map_err(internal)?;

    let row = row.ok_or_else(|| not_found("peer"))?;

    let peer_id: String = row.try_get("peer_id").map_err(internal)?;
    let nm: String = row.try_get("name").map_err(internal)?;
    let pk: Vec<u8> = row.try_get("public_key").map_err(internal)?;
    let caps: i64 = row.try_get("capabilities").map_err(internal)?;
    let addr: Option<String> = row.try_get("observed_addr").map_err(internal)?;
    let seen: i64 = row.try_get("last_seen_at").map_err(internal)?;

    Ok(Json(PeerLookupResponse {
        peer_id,
        name: nm,
        public_key: B64.encode(&pk),
        capabilities: caps as u64,
        observed_addr: addr,
        last_seen_at: seen,
    }))
}

pub async fn not_implemented_sessions() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "sessions/initiate not implemented yet (RFC-004 week 2+)"
        })),
    )
}

pub async fn not_implemented_relay() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "relay endpoints not implemented yet (RFC-004 R3)"
        })),
    )
}

fn unix_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn bad_req(msg: &'static str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": msg })),
    )
}

fn conflict(msg: &'static str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::CONFLICT,
        Json(json!({ "error": msg })),
    )
}

fn unauthorized() -> (StatusCode, Json<Value>) {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "missing or invalid Bearer JWT" })),
    )
}

fn not_found(kind: &'static str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": format!("unknown {kind}") })),
    )
}

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, Json<Value>) {
    tracing::error!("internal error: {e}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal server error" })),
    )
}

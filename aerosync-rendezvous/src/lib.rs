//! WAN rendezvous server library (RFC-004).
//!
//! Week-1 scope: SQLite schema, RS256 JWT, `/v1/peers/register`, `/heartbeat`,
//! `/v1/peers/{name}`. P2: `POST /v1/sessions/initiate` + `GET /v1/sessions/{id}/ws` (signaling stub);
//! relay remains HTTP 501 with structured body until R3.

pub mod jwt;
pub mod peers;
pub mod sessions;

use axum::routing::post;
use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
use governor::Quota;
use jsonwebtoken::{DecodingKey, EncodingKey};
use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Arc;

/// Per-IP register rate: ~4/s sustained, burst 12 (abuse + tenant-squatting pressure release).
pub type RegisterRatelimit = governor::DefaultKeyedRateLimiter<IpAddr>;

/// Shared server state.
pub struct AppState {
    pub pool: SqlitePool,
    pub jwt_issuer: String,
    pub jwt_ttl_secs: u64,
    pub encoding_key: Arc<EncodingKey>,
    pub decoding_key: Arc<DecodingKey>,
    pub register_ratelimit: Arc<RegisterRatelimit>,
}

/// Open or create the SQLite database and apply embedded migrations.
pub async fn connect_database(database_path: &Path) -> anyhow::Result<SqlitePool> {
    if let Some(parent) = database_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let opts = SqliteConnectOptions::new()
        .filename(database_path)
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(10)
        .connect_with(opts)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

/// Build the default per-IP register limiter (GCRA / token bucket; see `governor`).
pub fn default_register_ratelimit() -> Arc<RegisterRatelimit> {
    let q = Quota::per_second(NonZeroU32::new(4).expect("4"))
        .allow_burst(NonZeroU32::new(12).expect("12"));
    Arc::new(governor::RateLimiter::keyed(q))
}

async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "aerosync-rendezvous",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn version_info() -> Json<Value> {
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "protocols": [
            "aerosync-wire/1",
            "rfc004-rendezvous-v1",
            "rfc004-p2-sessions-partial"
        ],
    }))
}

async fn status(State(state): State<Arc<AppState>>) -> Result<Json<Value>, (StatusCode, String)> {
    let peers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM peers")
        .fetch_one(&state.pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
        .fetch_one(&state.pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({
        "schema": "rfc004_v1",
        "counts": {
            "peers": peers,
            "sessions": sessions
        }
    })))
}

/// Build the axum router.
pub fn app_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/health", get(health))
        .route("/v1/version", get(version_info))
        .route("/v1/status", get(status))
        .route("/v1/peers/register", post(peers::register_peer))
        .route("/v1/peers/heartbeat", post(peers::heartbeat))
        .route("/v1/peers/:name", get(peers::lookup_peer))
        .route("/v1/sessions/initiate", post(sessions::initiate_session))
        .route("/v1/sessions/:id/ws", get(sessions::session_websocket))
        .route(
            "/v1/relay/:session_id/up",
            post(sessions::not_implemented_relay),
        )
        .route(
            "/v1/relay/:session_id/down",
            get(sessions::not_implemented_relay),
        )
        .with_state(state)
}

/// Bind `addr` and serve until interrupted.
pub async fn serve(addr: SocketAddr, state: Arc<AppState>) -> anyhow::Result<()> {
    let app = app_router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("aerosync-rendezvous listening on http://{addr}");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jwt::{decoding_key_from_private_pkcs8_pem, encoding_key_from_rsa_pem};
    use rsa::pkcs8::{EncodePrivateKey, LineEnding};
    use rsa::RsaPrivateKey;
    use tempfile::tempdir;

    #[tokio::test]
    async fn migrations_apply_empty_tables() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("rv.db");
        let pool = connect_database(&db).await.unwrap();
        let peers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM peers")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(peers, 0);
    }

    fn rsa_pem() -> Vec<u8> {
        let mut rng = rand::thread_rng();
        let key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
        key.to_pkcs8_pem(LineEnding::LF)
            .unwrap()
            .as_bytes()
            .to_vec()
    }

    #[tokio::test]
    async fn register_then_lookup() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("rv.db");
        let pem = rsa_pem();
        let enc = Arc::new(encoding_key_from_rsa_pem(&pem).unwrap());
        let dec = Arc::new(decoding_key_from_private_pkcs8_pem(&pem).unwrap());

        let pool = connect_database(&db).await.unwrap();
        let state = Arc::new(AppState {
            pool,
            jwt_issuer: "test-iss".into(),
            jwt_ttl_secs: 3600,
            encoding_key: enc,
            decoding_key: dec,
            register_ratelimit: default_register_ratelimit(),
        });

        use axum::body::Body;
        use axum::extract::connect_info::MockConnectInfo;
        use axum::http::Request;
        use base64::Engine;
        use serde_json::json;
        use std::net::SocketAddr;
        use tower::ServiceExt;

        let pubkey = base64::engine::general_purpose::STANDARD.encode([0u8; 32]);
        let body = json!({
            "name": "alice",
            "public_key": pubkey,
            "capabilities": 3,
            "observed_addr": "10.0.0.1:9999"
        })
        .to_string();

        let req = Request::builder()
            .method("POST")
            .uri("/v1/peers/register")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();

        let app = app_router(state.clone())
            .layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 50_321))));

        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let reg: Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(reg["namespace"], "");
        let jwt = reg["jwt"].as_str().unwrap().to_string();

        let req2 = Request::builder()
            .method("GET")
            .uri("/v1/peers/alice")
            .header("authorization", format!("Bearer {}", jwt))
            .body(Body::empty())
            .unwrap();
        let app2 = app_router(state.clone())
            .layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 50_321))));
        let res2 = app2.oneshot(req2).await.unwrap();
        assert_eq!(res2.status(), StatusCode::OK);
    }
}

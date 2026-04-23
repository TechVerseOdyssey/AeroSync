//! WAN rendezvous server library (RFC-004).
//!
//! v0.4 **week-1 scaffold**: SQLite schema from RFC §5.4, HTTP `/health` and
//! `/v1/status`. Registration / JWT / WebSocket signaling follow in later tasks.

use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::net::SocketAddr;
use std::path::Path;

/// Shared server state (connection pool).
#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
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

/// Build the axum router (for tests and [`serve`].
pub fn app_router(pool: SqlitePool) -> Router {
    let state = AppState { pool };
    Router::new()
        .route("/health", get(health))
        .route("/v1/status", get(status))
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "aerosync-rendezvous",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn status(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, String)> {
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

/// Bind `addr` and serve until the process is interrupted.
pub async fn serve(addr: SocketAddr, pool: SqlitePool) -> anyhow::Result<()> {
    let app = app_router(pool);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("aerosync-rendezvous listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(peers, 0);
        assert_eq!(sessions, 0);
    }
}

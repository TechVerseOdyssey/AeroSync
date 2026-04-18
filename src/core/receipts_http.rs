//! HTTP control surface for the [`Receipt`](super::receipt::Receipt)
//! state machine — RFC-002 §6.4 + §9 (Week 2 task #5).
//!
//! This module wires the four HTTP endpoints called out in
//! RFC-002 §6.4 onto an `axum::Router` that the existing
//! `build_axum_router` (in `src/core/server.rs`) merges in:
//!
//! | Method | Path                                      | Purpose                              |
//! | ------ | ----------------------------------------- | ------------------------------------ |
//! | GET    | `/v1/receipts/:id/events`                 | SSE stream of state transitions      |
//! | POST   | `/v1/receipts/:id/ack`                    | Receiver-side application ack        |
//! | POST   | `/v1/receipts/:id/nack`                   | Receiver-side application nack       |
//! | POST   | `/v1/receipts/:id/cancel`                 | Sender-side cancel                   |
//!
//! All four take a path parameter `:id` that parses to a UUID; an
//! invalid UUID returns `400 Bad Request`. ack/nack route to the
//! receiver-side [`ReceiptRegistry`], cancel routes to the sender-side
//! [`ReceiptRegistry`].
//!
//! # Idempotency (RFC-002 §9)
//!
//! Every POST body carries an `idempotency_key`. We keep a small
//! in-memory LRU keyed by `(receipt_id, idempotency_key, intent)` with
//! a 5-minute TTL. Re-issuing the same `(receipt_id, key, intent)`
//! triple within the TTL replays the **stored response status** —
//! `200 OK` becomes `200 OK` again even if the receipt is now
//! terminal. This matches the §9 contract: "Receiver acking the same
//! `receipt_id` twice is a no-op." Persistence is deliberately
//! deferred — survives across handler calls within one process, lost
//! on restart. Disk-backed idempotency lands in a future RFC.
//!
//! # SSE wire format
//!
//! Each `Receipt` state transition produces one SSE event:
//!
//! ```text
//! event: receipt-state
//! id: <monotonic per-receipt counter, starting at 1>
//! data: {"receipt_id":"<uuid>","state":"...","terminal":<bool>,"reason":...}
//!
//! ```
//!
//! The body is a serde-serialised mirror of the protobuf
//! `ReceiptFrame` (we deliberately do NOT serialise prost-encoded
//! bytes over SSE — JSON is the wire format here, not bytes inside
//! base64). The stream closes after delivering the terminal event.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path as AxPath, State as AxState};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::core::receipt::{
    CompletedTerminal, Event, FailedTerminal, Receipt, Receiver as RxSide, Sender as TxSide, State,
};
use crate::core::receipt_registry::ReceiptRegistry;

// ─────────────────────────────────────────────────────────────────────
// Public surface
// ─────────────────────────────────────────────────────────────────────

/// TTL of the in-memory idempotency cache. Per the task brief.
pub const IDEMPOTENCY_TTL: Duration = Duration::from_secs(5 * 60);

/// Cap on the idempotency cache size to prevent unbounded memory
/// growth from a hostile client. Eviction is opportunistic on insert.
const IDEMPOTENCY_CACHE_CAP: usize = 4096;

/// Routing intent — the third leg of the idempotency key. Different
/// intents on the same `(receipt_id, idempotency_key)` MUST be
/// treated as distinct entries so a stale `ack` doesn't replay as a
/// `nack` and vice versa.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
enum Intent {
    Ack,
    Nack,
    Cancel,
}

impl Intent {
    fn as_str(self) -> &'static str {
        match self {
            Intent::Ack => "ack",
            Intent::Nack => "nack",
            Intent::Cancel => "cancel",
        }
    }
}

/// Cached idempotency outcome — just the status code so a replay
/// returns exactly the same status as the first call. Body is
/// regenerated to keep the cache small.
#[derive(Debug, Clone, Copy)]
struct Cached {
    status: StatusCode,
    inserted_at: Instant,
}

/// Map key for the idempotency cache —
/// `(receipt_id, idempotency_key, intent_str)`. Hoisted to a type
/// alias to satisfy clippy's `type_complexity` lint and keep the
/// `IdempotencyCache` field readable.
type IdempotencyKey = (Uuid, String, &'static str);

/// In-memory idempotency cache with TTL-based expiry. Cheap to clone
/// (it's an `Arc` under the hood); embed in `AppState`.
#[derive(Debug, Clone, Default)]
pub struct IdempotencyCache {
    inner: Arc<Mutex<HashMap<IdempotencyKey, Cached>>>,
}

impl IdempotencyCache {
    /// Construct an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    fn check(&self, id: Uuid, key: &str, intent: Intent) -> Option<StatusCode> {
        let mut guard = self.inner.lock().ok()?;
        let map_key = (id, key.to_string(), intent.as_str());
        if let Some(c) = guard.get(&map_key).copied() {
            if c.inserted_at.elapsed() < IDEMPOTENCY_TTL {
                return Some(c.status);
            }
            guard.remove(&map_key);
        }
        None
    }

    fn record(&self, id: Uuid, key: &str, intent: Intent, status: StatusCode) {
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        if guard.len() >= IDEMPOTENCY_CACHE_CAP {
            // Cheap LRU-ish eviction: drop expired entries first; if
            // that doesn't free anything, drop one arbitrary entry
            // (HashMap iteration order is fine for this).
            let now = Instant::now();
            guard.retain(|_, c| now.duration_since(c.inserted_at) < IDEMPOTENCY_TTL);
            if guard.len() >= IDEMPOTENCY_CACHE_CAP {
                if let Some(victim) = guard.keys().next().cloned() {
                    guard.remove(&victim);
                }
            }
        }
        guard.insert(
            (id, key.to_string(), intent.as_str()),
            Cached {
                status,
                inserted_at: Instant::now(),
            },
        );
    }

    /// Test-only: number of entries currently cached.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }

    /// Test-only: true if the cache is empty.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Bundle of state the receipt HTTP handlers need. Embed in the
/// outer axum `AppState` (or pass standalone for tests).
#[derive(Clone)]
pub struct ReceiptHttpState {
    /// Sender-side registry — Cancel routes here.
    pub sender_receipts: ReceiptRegistry<TxSide>,
    /// Receiver-side registry — Ack and Nack route here.
    pub receiver_receipts: ReceiptRegistry<RxSide>,
    /// In-memory idempotency cache.
    pub idempotency: IdempotencyCache,
}

impl ReceiptHttpState {
    /// Construct a fresh state with empty registries and cache.
    pub fn new() -> Self {
        Self {
            sender_receipts: ReceiptRegistry::new(),
            receiver_receipts: ReceiptRegistry::new(),
            idempotency: IdempotencyCache::new(),
        }
    }
}

impl Default for ReceiptHttpState {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the `/v1/receipts/...` sub-router. Merge the result into the
/// main router via `axum::Router::merge`.
pub fn router(state: ReceiptHttpState) -> axum::Router {
    axum::Router::new()
        .route("/v1/receipts/:id/events", get(handle_events_sse))
        .route("/v1/receipts/:id/ack", post(handle_ack))
        .route("/v1/receipts/:id/nack", post(handle_nack))
        .route("/v1/receipts/:id/cancel", post(handle_cancel))
        .with_state(state)
}

// ─────────────────────────────────────────────────────────────────────
// JSON wire types
// ─────────────────────────────────────────────────────────────────────

/// Body of `POST /v1/receipts/:id/ack`. `metadata` is RFC-003 and is
/// accepted but ignored in v0.2 (Week 4 wires it into `Receipt`).
#[derive(Debug, Deserialize)]
pub struct AckBody {
    /// Optional ack-time metadata (RFC-003). Currently parsed and
    /// discarded; `Acked` carries it once RFC-003 lands.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    /// Replay-protection key. Re-using the same key + intent on the
    /// same receipt within 5 minutes returns the original status code
    /// without mutating state.
    pub idempotency_key: String,
}

/// Body of `POST /v1/receipts/:id/nack`.
#[derive(Debug, Deserialize)]
pub struct NackBody {
    /// Free-form rejection reason. Surfaced as
    /// `Failed(Nacked{reason})`.
    pub reason: String,
    /// Replay-protection key (see `AckBody`).
    pub idempotency_key: String,
}

/// Body of `POST /v1/receipts/:id/cancel`.
#[derive(Debug, Deserialize)]
pub struct CancelBody {
    /// Free-form cancellation reason. Surfaced as
    /// `Failed(Cancelled{reason})`.
    pub reason: String,
    /// Replay-protection key (see `AckBody`).
    pub idempotency_key: String,
}

/// Serde mirror of [`State`] used in JSON responses and SSE bodies.
/// Carries the terminal payload inline rather than the protobuf
/// `oneof` shape so the JSON is direct without a body-variant
/// indirection.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StateView {
    /// `State::Initiated`.
    Initiated,
    /// `State::StreamOpened`.
    StreamOpened,
    /// `State::DataTransferred`.
    DataTransferred,
    /// `State::StreamClosed`.
    StreamClosed,
    /// `State::Processing`.
    Processing,
    /// `State::Completed(_)`. Carries the terminal sub-reason.
    Completed { outcome: &'static str },
    /// `State::Failed(_)`. Carries the failure sub-reason.
    Failed {
        kind: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

impl From<&State> for StateView {
    fn from(s: &State) -> Self {
        match s {
            State::Initiated => StateView::Initiated,
            State::StreamOpened => StateView::StreamOpened,
            State::DataTransferred => StateView::DataTransferred,
            State::StreamClosed => StateView::StreamClosed,
            State::Processing => StateView::Processing,
            State::Completed(CompletedTerminal::Acked) => StateView::Completed { outcome: "acked" },
            State::Failed(FailedTerminal::Cancelled { reason }) => StateView::Failed {
                kind: "cancelled",
                reason: Some(reason.clone()),
                code: None,
                detail: None,
            },
            State::Failed(FailedTerminal::Nacked { reason }) => StateView::Failed {
                kind: "nacked",
                reason: Some(reason.clone()),
                code: None,
                detail: None,
            },
            State::Failed(FailedTerminal::Errored { code, detail }) => StateView::Failed {
                kind: "errored",
                reason: None,
                code: Some(*code),
                detail: Some(detail.clone()),
            },
        }
    }
}

/// SSE / GET `/v1/receipts/:id/events` body — the JSON dispatched
/// for each transition.
#[derive(Debug, Clone, Serialize)]
pub struct StateEventBody {
    /// String form of the receipt UUID.
    pub receipt_id: String,
    /// Current state and any terminal payload.
    #[serde(flatten)]
    pub state: StateView,
    /// True iff `state` is terminal (Completed / Failed). Convenience
    /// for clients that don't want to enumerate state names.
    pub terminal: bool,
}

// ─────────────────────────────────────────────────────────────────────
// Handlers
// ─────────────────────────────────────────────────────────────────────

fn parse_uuid(raw: &str) -> Result<Uuid, Box<Response>> {
    Uuid::parse_str(raw).map_err(|_| {
        Box::new(
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid_receipt_id"})),
            )
                .into_response(),
        )
    })
}

async fn handle_events_sse(
    AxPath(id_str): AxPath<String>,
    AxState(state): AxState<ReceiptHttpState>,
) -> Response {
    let id = match parse_uuid(&id_str) {
        Ok(u) => u,
        Err(resp) => return *resp,
    };

    // Routes both sides — we look up the receipt in either registry
    // so a single endpoint serves sender-side and receiver-side
    // observers. If both registries hold an entry with the same
    // receipt_id (e.g. during loopback testing) we prefer the
    // sender-side view; the protocol contract is that the two state
    // machines run in lockstep so either gives the same observable
    // sequence.
    // Both branches need the same opaque type; build the stream from
    // the receipt's `watch::Receiver<State>` so the two sides share
    // one concrete `sse_state_stream` instantiation.
    let watch_rx = if let Some(r) = state.sender_receipts.get(id) {
        r.watch()
    } else if let Some(r) = state.receiver_receipts.get(id) {
        r.watch()
    } else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "receipt_not_found"})),
        )
            .into_response();
    };
    let stream = sse_state_stream(id, watch_rx);

    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

fn sse_state_stream(
    id: Uuid,
    watch_rx: tokio::sync::watch::Receiver<State>,
) -> impl Stream<Item = Result<SseEvent, Infallible>> + Send + 'static {
    // Hand-rolled `unfold` rather than `async_stream::stream!` so we
    // don't pull in the `async-stream` proc-macro crate just for one
    // generator. State machine:
    //   Init    -> emit current snapshot, transition to Streaming or
    //              Done depending on terminality.
    //   Streaming -> await watch.changed(); on change, emit; on
    //                terminal, transition to Done; on watch close,
    //                transition to Done.
    //   Done    -> end of stream.
    enum Phase {
        Init,
        Streaming,
        Done,
    }
    struct St {
        phase: Phase,
        counter: u64,
        watch_rx: tokio::sync::watch::Receiver<State>,
        id: Uuid,
    }
    let init = St {
        phase: Phase::Init,
        counter: 0,
        watch_rx,
        id,
    };
    futures::stream::unfold(init, |mut st| async move {
        loop {
            match st.phase {
                Phase::Init => {
                    let snapshot = st.watch_rx.borrow().clone();
                    st.counter += 1;
                    let ev = build_sse_event(st.id, &snapshot, st.counter);
                    st.phase = if snapshot.is_terminal() {
                        Phase::Done
                    } else {
                        Phase::Streaming
                    };
                    return Some((ev, st));
                }
                Phase::Streaming => {
                    if st.watch_rx.changed().await.is_err() {
                        st.phase = Phase::Done;
                        continue;
                    }
                    let s = st.watch_rx.borrow().clone();
                    st.counter += 1;
                    let is_terminal = s.is_terminal();
                    let ev = build_sse_event(st.id, &s, st.counter);
                    if is_terminal {
                        st.phase = Phase::Done;
                    }
                    return Some((ev, st));
                }
                Phase::Done => return None,
            }
        }
    })
}

fn build_sse_event(id: Uuid, state: &State, counter: u64) -> Result<SseEvent, Infallible> {
    let body = StateEventBody {
        receipt_id: id.to_string(),
        state: state.into(),
        terminal: state.is_terminal(),
    };
    let json = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
    Ok(SseEvent::default()
        .event("receipt-state")
        .id(counter.to_string())
        .data(json))
}

async fn handle_ack(
    AxPath(id_str): AxPath<String>,
    AxState(state): AxState<ReceiptHttpState>,
    Json(body): Json<AckBody>,
) -> Response {
    let id = match parse_uuid(&id_str) {
        Ok(u) => u,
        Err(resp) => return *resp,
    };
    apply_to_receiver(&state, id, Intent::Ack, &body.idempotency_key, Event::Ack)
}

async fn handle_nack(
    AxPath(id_str): AxPath<String>,
    AxState(state): AxState<ReceiptHttpState>,
    Json(body): Json<NackBody>,
) -> Response {
    let id = match parse_uuid(&id_str) {
        Ok(u) => u,
        Err(resp) => return *resp,
    };
    apply_to_receiver(
        &state,
        id,
        Intent::Nack,
        &body.idempotency_key,
        Event::Nack {
            reason: body.reason,
        },
    )
}

async fn handle_cancel(
    AxPath(id_str): AxPath<String>,
    AxState(state): AxState<ReceiptHttpState>,
    Json(body): Json<CancelBody>,
) -> Response {
    let id = match parse_uuid(&id_str) {
        Ok(u) => u,
        Err(resp) => return *resp,
    };
    apply_to_sender(
        &state,
        id,
        Intent::Cancel,
        &body.idempotency_key,
        Event::Cancel {
            reason: body.reason,
        },
    )
}

fn apply_to_receiver(
    state: &ReceiptHttpState,
    id: Uuid,
    intent: Intent,
    idempotency_key: &str,
    event: Event,
) -> Response {
    if let Some(status) = state.idempotency.check(id, idempotency_key, intent) {
        return replay_response(state, id, status, &state.receiver_receipts);
    }
    let Some(r) = state.receiver_receipts.get(id) else {
        return not_found(id);
    };
    finalise(state, id, intent, idempotency_key, &r, event)
}

fn apply_to_sender(
    state: &ReceiptHttpState,
    id: Uuid,
    intent: Intent,
    idempotency_key: &str,
    event: Event,
) -> Response {
    if let Some(status) = state.idempotency.check(id, idempotency_key, intent) {
        return replay_response(state, id, status, &state.sender_receipts);
    }
    let Some(r) = state.sender_receipts.get(id) else {
        return not_found(id);
    };
    finalise(state, id, intent, idempotency_key, &r, event)
}

fn finalise<S>(
    state: &ReceiptHttpState,
    id: Uuid,
    intent: Intent,
    idempotency_key: &str,
    r: &Arc<Receipt<S>>,
    event: Event,
) -> Response {
    let snapshot_before = r.state();
    if snapshot_before.is_terminal() {
        let status = StatusCode::CONFLICT;
        state
            .idempotency
            .record(id, idempotency_key, intent, status);
        return (
            status,
            Json(serde_json::json!({
                "error": "receipt_terminal",
                "receipt_id": id.to_string(),
                "state": StateView::from(&snapshot_before),
            })),
        )
            .into_response();
    }
    match r.apply_event(event) {
        Ok(()) => {
            let status = StatusCode::OK;
            state
                .idempotency
                .record(id, idempotency_key, intent, status);
            (
                status,
                Json(serde_json::json!({
                    "ok": true,
                    "receipt_id": id.to_string(),
                    "state": StateView::from(&r.state()),
                })),
            )
                .into_response()
        }
        Err(e) => {
            // Illegal transition (e.g. ack from Initiated). 409 with
            // current state in body, but DO NOT cache so the client
            // can correct the call.
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "invalid_transition",
                    "detail": e.to_string(),
                    "receipt_id": id.to_string(),
                    "state": StateView::from(&r.state()),
                })),
            )
                .into_response()
        }
    }
}

fn replay_response<S>(
    _state: &ReceiptHttpState,
    id: Uuid,
    status: StatusCode,
    registry: &ReceiptRegistry<S>,
) -> Response {
    let view = registry
        .get(id)
        .map(|r| StateView::from(&r.state()))
        .unwrap_or(StateView::Initiated);
    (
        status,
        Json(serde_json::json!({
            "ok": status.is_success(),
            "receipt_id": id.to_string(),
            "replayed": true,
            "state": view,
        })),
    )
        .into_response()
}

fn not_found(id: Uuid) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": "receipt_not_found",
            "receipt_id": id.to_string(),
        })),
    )
        .into_response()
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;

    fn fresh_state() -> ReceiptHttpState {
        ReceiptHttpState::new()
    }

    async fn body_to_json(body: Body) -> serde_json::Value {
        let bytes = to_bytes(body, 64 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    async fn collect_sse_text(body: Body) -> String {
        // SSE bodies stream until the inner stream ends (we end on
        // terminal). `to_bytes` polls the body to completion, which
        // matches what we want for these tests.
        let bytes = to_bytes(body, 1024 * 1024).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn drive_receiver_to_processing(r: &Receipt<RxSide>) {
        r.apply_event(Event::Open).unwrap();
        r.apply_event(Event::Close).unwrap();
        r.apply_event(Event::Close).unwrap();
        r.apply_event(Event::Process).unwrap();
    }

    #[tokio::test]
    async fn ack_happy_path_returns_200_and_transitions_to_completed() {
        let state = fresh_state();
        let r = Arc::new(Receipt::<RxSide>::new(Uuid::new_v4()));
        drive_receiver_to_processing(&r);
        let id = r.id();
        state.receiver_receipts.insert(Arc::clone(&r));

        let app = router(state.clone());
        let req = Request::builder()
            .method("POST")
            .uri(format!("/v1/receipts/{id}/ack"))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"idempotency_key":"k1"}"#.to_string()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_to_json(resp.into_body()).await;
        assert_eq!(json["ok"], true);
        assert_eq!(json["state"]["state"], "completed");
        assert!(matches!(
            r.state(),
            State::Completed(CompletedTerminal::Acked)
        ));
    }

    #[tokio::test]
    async fn ack_unknown_id_returns_404() {
        let state = fresh_state();
        let app = router(state);
        let req = Request::builder()
            .method("POST")
            .uri(format!("/v1/receipts/{}/ack", Uuid::new_v4()))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"idempotency_key":"k"}"#.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn ack_on_terminal_returns_409() {
        let state = fresh_state();
        let r = Arc::new(Receipt::<RxSide>::new(Uuid::new_v4()));
        // Drive directly to terminal via cancel.
        r.apply_event(Event::Cancel {
            reason: "test".into(),
        })
        .unwrap();
        state.receiver_receipts.insert(Arc::clone(&r));

        let app = router(state);
        let req = Request::builder()
            .method("POST")
            .uri(format!("/v1/receipts/{}/ack", r.id()))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"idempotency_key":"k"}"#.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn idempotent_ack_replays_status_no_double_apply() {
        let state = fresh_state();
        let r = Arc::new(Receipt::<RxSide>::new(Uuid::new_v4()));
        drive_receiver_to_processing(&r);
        let id = r.id();
        state.receiver_receipts.insert(Arc::clone(&r));

        let app1 = router(state.clone());
        let req1 = Request::builder()
            .method("POST")
            .uri(format!("/v1/receipts/{id}/ack"))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"idempotency_key":"same"}"#.to_string()))
            .unwrap();
        let r1 = app1.oneshot(req1).await.unwrap();
        assert_eq!(r1.status(), StatusCode::OK);

        // Second call with same key + intent.
        let app2 = router(state.clone());
        let req2 = Request::builder()
            .method("POST")
            .uri(format!("/v1/receipts/{id}/ack"))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"idempotency_key":"same"}"#.to_string()))
            .unwrap();
        let r2 = app2.oneshot(req2).await.unwrap();
        assert_eq!(r2.status(), StatusCode::OK, "replay must yield same status");
        let json = body_to_json(r2.into_body()).await;
        assert_eq!(json["replayed"], true);
        // The receipt is still terminal (not double-applied).
        assert!(matches!(
            r.state(),
            State::Completed(CompletedTerminal::Acked)
        ));
    }

    #[tokio::test]
    async fn nack_happy_path_transitions_to_failed_nacked() {
        let state = fresh_state();
        let r = Arc::new(Receipt::<RxSide>::new(Uuid::new_v4()));
        drive_receiver_to_processing(&r);
        let id = r.id();
        state.receiver_receipts.insert(Arc::clone(&r));

        let app = router(state);
        let req = Request::builder()
            .method("POST")
            .uri(format!("/v1/receipts/{id}/nack"))
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"reason":"schema-mismatch","idempotency_key":"k1"}"#.to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        match r.state() {
            State::Failed(FailedTerminal::Nacked { reason }) => {
                assert_eq!(reason, "schema-mismatch");
            }
            other => panic!("expected nacked, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_happy_path_transitions_sender_to_failed_cancelled() {
        let state = fresh_state();
        let r = Arc::new(Receipt::<TxSide>::new(Uuid::new_v4()));
        let id = r.id();
        state.sender_receipts.insert(Arc::clone(&r));

        let app = router(state);
        let req = Request::builder()
            .method("POST")
            .uri(format!("/v1/receipts/{id}/cancel"))
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"reason":"user-stop","idempotency_key":"k1"}"#.to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        match r.state() {
            State::Failed(FailedTerminal::Cancelled { reason }) => {
                assert_eq!(reason, "user-stop");
            }
            other => panic!("expected cancelled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sse_unknown_id_returns_404() {
        let state = fresh_state();
        let app = router(state);
        let req = Request::builder()
            .method("GET")
            .uri(format!("/v1/receipts/{}/events", Uuid::new_v4()))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn sse_emits_initial_snapshot_then_terminal() {
        let state = fresh_state();
        let r = Arc::new(Receipt::<TxSide>::new(Uuid::new_v4()));
        let id = r.id();
        state.sender_receipts.insert(Arc::clone(&r));

        let app = router(state);
        let req = Request::builder()
            .method("GET")
            .uri(format!("/v1/receipts/{id}/events"))
            .body(Body::empty())
            .unwrap();

        // Poke the receipt into a terminal state in another task, then
        // collect everything the SSE stream emitted.
        let r_clone = Arc::clone(&r);
        let pump = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            r_clone
                .apply_event(Event::Cancel {
                    reason: "done".into(),
                })
                .unwrap();
        });

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body();
        let text = tokio::time::timeout(Duration::from_secs(3), collect_sse_text(body))
            .await
            .expect("sse body should complete after terminal event");
        assert!(
            text.contains("event: receipt-state"),
            "missing event header: {text}"
        );
        assert!(
            text.contains("\"terminal\":true"),
            "missing terminal frame: {text}"
        );
        pump.await.unwrap();
    }

    #[tokio::test]
    async fn sse_on_terminal_already_emits_one_event_then_closes() {
        let state = fresh_state();
        let r = Arc::new(Receipt::<TxSide>::new(Uuid::new_v4()));
        // Pre-cancel before the SSE subscribes.
        r.apply_event(Event::Cancel {
            reason: "pre-cancelled".into(),
        })
        .unwrap();
        let id = r.id();
        state.sender_receipts.insert(Arc::clone(&r));

        let app = router(state);
        let req = Request::builder()
            .method("GET")
            .uri(format!("/v1/receipts/{id}/events"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body();
        let text = tokio::time::timeout(Duration::from_secs(3), collect_sse_text(body))
            .await
            .expect("terminal sse should close immediately");
        assert!(
            text.contains("\"terminal\":true"),
            "expected terminal frame, got: {text}"
        );
        // Exactly one `id:` field — the single terminal event.
        let count = text.matches("\nid:").count();
        assert_eq!(count, 1, "expected exactly one SSE id, got {count}: {text}");
    }

    #[tokio::test]
    async fn invalid_uuid_in_path_returns_400() {
        let state = fresh_state();
        let app = router(state);
        let req = Request::builder()
            .method("POST")
            .uri("/v1/receipts/not-a-uuid/ack")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"idempotency_key":"k"}"#.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn idempotency_cache_distinguishes_intents() {
        // Same receipt_id + same key but different intents → different
        // cache entries; the second call is NOT a replay.
        let state = fresh_state();
        let r = Arc::new(Receipt::<RxSide>::new(Uuid::new_v4()));
        drive_receiver_to_processing(&r);
        let id = r.id();
        state.receiver_receipts.insert(Arc::clone(&r));

        // First, ack with key "K".
        let req1 = Request::builder()
            .method("POST")
            .uri(format!("/v1/receipts/{id}/ack"))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"idempotency_key":"K"}"#.to_string()))
            .unwrap();
        assert_eq!(
            router(state.clone()).oneshot(req1).await.unwrap().status(),
            StatusCode::OK
        );
        // Now nack with same key "K". The receipt is already terminal
        // (Completed/Acked), so it should be 409 — but importantly
        // NOT a 200 OK replay of the prior ack.
        let req2 = Request::builder()
            .method("POST")
            .uri(format!("/v1/receipts/{id}/nack"))
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"reason":"x","idempotency_key":"K"}"#.to_string(),
            ))
            .unwrap();
        let resp2 = router(state.clone()).oneshot(req2).await.unwrap();
        assert_eq!(resp2.status(), StatusCode::CONFLICT);
    }
}

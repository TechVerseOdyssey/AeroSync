//! In-memory R2 session signaling: relay `candidates` between the two members of a
//! `sessions` row and issue a synchronized `punch_at` (see RFC-004 §6.2–6.3).

use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

const PUNCH_OFFSET_MS: u64 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PeerRole {
    Initiator,
    Target,
}

impl fmt::Display for PeerRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PeerRole::Initiator => f.write_str("initiator"),
            PeerRole::Target => f.write_str("target"),
        }
    }
}

struct Room {
    initiator_out: Option<mpsc::UnboundedSender<String>>,
    target_out: Option<mpsc::UnboundedSender<String>>,
    initiator_got_candidates: bool,
    target_got_candidates: bool,
    /// `candidates` value from the initiator when the target was not connected yet
    /// (replayed as `remote.candidates` when the target’s WS opens).
    pending_init_body: Option<Value>,
    /// Same, target → initiator.
    pending_tgt_body: Option<Value>,
    punch_issued: bool,
}

impl Room {
    fn new() -> Self {
        Self {
            initiator_out: None,
            target_out: None,
            initiator_got_candidates: false,
            target_got_candidates: false,
            pending_init_body: None,
            pending_tgt_body: None,
            punch_issued: false,
        }
    }
}

/// Registry keyed by `session_id` (UUID string from `initiate_session`).
pub struct SignalingRegistry {
    rooms: Mutex<HashMap<String, Room>>,
}

impl Default for SignalingRegistry {
    fn default() -> Self {
        Self {
            rooms: Mutex::new(HashMap::new()),
        }
    }
}

impl SignalingRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a peer’s writer and return lines to write immediately
    /// (`aerosync.signaling.ready`, and buffered `remote.candidates` if any).
    pub fn register_peer(
        &self,
        session_id: &str,
        role: PeerRole,
        to_peer: mpsc::UnboundedSender<String>,
    ) -> Result<Vec<String>, String> {
        let mut g = self.rooms.lock().map_err(|e| e.to_string())?;
        let room = g.entry(session_id.to_string()).or_insert_with(Room::new);
        let mut out = vec![self.ready_line(session_id)];

        match role {
            PeerRole::Initiator => {
                if room.initiator_out.is_some() {
                    return Err("initiator already connected to this session".to_string());
                }
                room.initiator_out = Some(to_peer);
                if let Some(b) = room.pending_tgt_body.take() {
                    out.push(remote_candidates_line(PeerRole::Target, b)?);
                }
            }
            PeerRole::Target => {
                if room.target_out.is_some() {
                    return Err("target already connected to this session".to_string());
                }
                room.target_out = Some(to_peer);
                if let Some(b) = room.pending_init_body.take() {
                    out.push(remote_candidates_line(PeerRole::Initiator, b)?);
                }
            }
        }

        Ok(out)
    }

    fn ready_line(&self, session_id: &str) -> String {
        json!({
            "type": "aerosync.signaling.ready",
            "session_id": session_id,
            "p2": "candidates_relay_punch_at"
        })
        .to_string()
    }

    /// Handle a text line from a WebSocket.
    pub fn on_client_text(
        &self,
        session_id: &str,
        from: PeerRole,
        line: &str,
    ) -> Result<(), String> {
        let v: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if ty == "candidates" {
            return self.apply_candidates(session_id, from, v);
        }
        if ty == "aerosync.signaling.close" {
            return Ok(());
        }
        Ok(())
    }

    fn apply_candidates(
        &self,
        session_id: &str,
        from: PeerRole,
        body: Value,
    ) -> Result<(), String> {
        let mut g = self.rooms.lock().map_err(|e| e.to_string())?;
        let room = g
            .get_mut(session_id)
            .ok_or_else(|| "no signaling room; open WS first".to_string())?;

        let to_remote = remote_candidates_line(from, body.clone())?;

        match from {
            PeerRole::Initiator => {
                room.initiator_got_candidates = true;
                if let Some(t) = room.target_out.as_ref() {
                    t.send(to_remote)
                        .map_err(|_| "target write closed".to_string())?;
                } else {
                    room.pending_init_body = Some(candidates_data(&body));
                }
            }
            PeerRole::Target => {
                room.target_got_candidates = true;
                if let Some(t) = room.initiator_out.as_ref() {
                    t.send(to_remote)
                        .map_err(|_| "initiator write closed".to_string())?;
                } else {
                    room.pending_tgt_body = Some(candidates_data(&body));
                }
            }
        }

        if room.punch_issued {
            return Ok(());
        }
        if !(room.initiator_got_candidates && room.target_got_candidates) {
            return Ok(());
        }

        let t_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
            .saturating_add(PUNCH_OFFSET_MS);
        let punch = json!({
            "type": "punch_at",
            "timestamp_ms": t_ms
        })
        .to_string();

        room.punch_issued = true;
        if let Some(o) = room.initiator_out.as_ref() {
            o.send(punch.clone())
                .map_err(|_| "initiator write closed (punch)".to_string())?;
        }
        if let Some(o) = room.target_out.as_ref() {
            o.send(punch)
                .map_err(|_| "target write closed (punch)".to_string())?;
        }

        Ok(())
    }

    /// Remove a role’s writer; drop the room if both are gone.
    pub fn disconnect(&self, session_id: &str, role: PeerRole) {
        if let Ok(mut g) = self.rooms.lock() {
            if let Some(room) = g.get_mut(session_id) {
                match role {
                    PeerRole::Initiator => room.initiator_out = None,
                    PeerRole::Target => room.target_out = None,
                }
                if room.initiator_out.is_none() && room.target_out.is_none() {
                    g.remove(session_id);
                }
            }
        }
    }
}

/// Strip `type` and keep the candidate fields for replay when the other side connects.
fn candidates_data(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut o = m.clone();
            o.remove("type");
            Value::Object(o)
        }
        _ => v.clone(),
    }
}

/// Build `remote.candidates` (flattened) for the peer.
fn remote_candidates_line(from: PeerRole, body: Value) -> Result<String, String> {
    let server_ref = body
        .get("server_reflexive")
        .and_then(|x| x.as_str())
        .unwrap_or("");
    let local = body.get("local").cloned().unwrap_or_else(|| json!([]));
    let line = json!({
        "type": "remote.candidates",
        "from": from.to_string(),
        "server_reflexive": server_ref,
        "local": local
    });
    Ok(line.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_peers_relay_and_punch_at() {
        let r = SignalingRegistry::new();
        let (tx1, mut rx1) = mpsc::unbounded_channel::<String>();
        let (tx2, mut rx2) = mpsc::unbounded_channel::<String>();
        r.register_peer("s1", PeerRole::Initiator, tx1).unwrap();
        r.register_peer("s1", PeerRole::Target, tx2).unwrap();

        r.on_client_text(
            "s1",
            PeerRole::Initiator,
            r#"{"type":"candidates","server_reflexive":"203.0.113.1:1","local":[]}"#,
        )
        .unwrap();
        let to2 = rx2
            .try_recv()
            .expect("target should get remote from initiator");
        assert!(to2.contains("remote.candidates") && to2.contains("initiator"));

        r.on_client_text(
            "s1",
            PeerRole::Target,
            r#"{"type":"candidates","server_reflexive":"198.51.100.2:2","local":[]}"#,
        )
        .unwrap();
        let to1 = rx1
            .try_recv()
            .expect("initiator should get target candidates");
        assert!(to1.contains("remote.candidates") && to1.contains("target"));

        let p1 = rx1.try_recv().expect("punch to initiator");
        let p2 = rx2.try_recv().expect("punch to target");
        assert!(p1.contains("punch_at") && p2.contains("punch_at"));
        let v1: Value = serde_json::from_str(&p1).unwrap();
        let v2: Value = serde_json::from_str(&p2).unwrap();
        assert_eq!(v1["timestamp_ms"], v2["timestamp_ms"]);
    }
}

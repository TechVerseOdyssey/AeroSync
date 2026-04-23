-- RFC-004 §5.4 — baseline registry + sessions (+ relay accounting hooks).
-- Foreign keys enforced at connection open via `SqliteConnectOptions::foreign_keys`.

CREATE TABLE peers (
  peer_id        TEXT PRIMARY KEY,
  name           TEXT UNIQUE NOT NULL,
  public_key     BLOB NOT NULL,
  capabilities   INTEGER NOT NULL,
  observed_addr  TEXT,
  last_seen_at   INTEGER NOT NULL,
  created_at     INTEGER NOT NULL
);

CREATE INDEX idx_peers_last_seen ON peers(last_seen_at);

CREATE TABLE sessions (
  session_id     TEXT PRIMARY KEY,
  initiator_id   TEXT NOT NULL REFERENCES peers(peer_id),
  target_id      TEXT NOT NULL REFERENCES peers(peer_id),
  state          TEXT NOT NULL,
  created_at     INTEGER NOT NULL,
  closed_at      INTEGER
);

CREATE INDEX idx_sessions_state ON sessions(state, created_at);

CREATE TABLE relay_usage (
  session_id     TEXT NOT NULL REFERENCES sessions(session_id),
  bytes_relayed  INTEGER NOT NULL,
  recorded_at    INTEGER NOT NULL
);

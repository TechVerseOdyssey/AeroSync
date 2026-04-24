-- RFC-004 P2: multi-tenant (namespace, name) uniqueness; peers.name no longer unique alone.

PRAGMA foreign_keys = OFF;

CREATE TABLE peers_new (
  peer_id        TEXT PRIMARY KEY,
  namespace      TEXT NOT NULL DEFAULT '',
  name           TEXT NOT NULL COLLATE NOCASE,
  public_key     BLOB NOT NULL,
  capabilities   INTEGER NOT NULL,
  observed_addr  TEXT,
  last_seen_at   INTEGER NOT NULL,
  created_at     INTEGER NOT NULL,
  UNIQUE (namespace, name)
);

INSERT INTO peers_new (peer_id, namespace, name, public_key, capabilities, observed_addr, last_seen_at, created_at)
SELECT
  peer_id,
  '',
  name,
  public_key,
  capabilities,
  observed_addr,
  last_seen_at,
  created_at
FROM peers;

DROP TABLE peers;
ALTER TABLE peers_new RENAME TO peers;

CREATE INDEX idx_peers_last_seen ON peers(last_seen_at);
CREATE INDEX idx_peers_namespace_name ON peers(namespace, name);

PRAGMA foreign_keys = ON;

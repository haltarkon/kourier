-- Signal backend.db, step S6 (docs/architecture.md §5.5, §6.4, §6.12): the backend job queue,
-- the remote config of `GET /v2/config`, when we last asked our phone for the account keys, and
-- the Kourier version that last ran the account (a new version re-sends the sync requests).

-- Backend jobs: network work that must survive restarts (§6.12). One row per (kind, key); a
-- second enqueue of the same job is a no-op. One-shot jobs are deleted when they succeed or are
-- dropped; periodic jobs (remote config) stay and are rescheduled.
CREATE TABLE jobs(
  id INTEGER PRIMARY KEY,
  kind TEXT NOT NULL,                     -- 'remote_config', 'capabilities', 'sync_request', …
  key TEXT NOT NULL DEFAULT '',           -- dedup key within the kind (sync request type, …)
  priority INTEGER NOT NULL,              -- lower runs first among due jobs
  cmd INTEGER UNIQUE,                     -- the core's durable command id, for command jobs (S8)
  attempts INTEGER NOT NULL DEFAULT 0,    -- failed attempts since the last success
  next_at INTEGER NOT NULL,               -- ms since the epoch
  expires_at INTEGER,                     -- dropped when a retry would start after this
  last_error TEXT,                        -- log-safe
  created_at INTEGER NOT NULL,
  UNIQUE (kind, key)
) STRICT;
CREATE INDEX jobs_due ON jobs(next_at);

-- `GET /v2/config` entries we keep (prefixes `desktop.`, `global.`, `cds.`, `client.`).
CREATE TABLE remote_config(
  name TEXT PRIMARY KEY,
  value TEXT NOT NULL
) STRICT;

ALTER TABLE account ADD COLUMN remote_config_etag TEXT;      -- ETag of the stored config
ALTER TABLE account ADD COLUMN remote_config_at INTEGER;     -- last successful fetch (200 or 304)
ALTER TABLE account ADD COLUMN keys_requested_at INTEGER;    -- last SyncMessage.Request{KEYS}
ALTER TABLE account ADD COLUMN app_version TEXT;             -- Kourier version of the last start

-- backend.db of the scripted fake backend (kourier-fake). Mirrors the shape of a real backend:
-- a stage-1 inbox, mirrored state for core.db rebuilds, a durable job table, and the script
-- cursor, all changed in the same transactions that append to core_outbox (§4.1 principle 6).
CREATE TABLE fake_state(key TEXT PRIMARY KEY, value INTEGER NOT NULL);
CREATE TABLE fake_identity(
  id INTEGER PRIMARY KEY CHECK (id = 1),
  own_ids TEXT NOT NULL, device INTEGER, display_name TEXT);
-- stage 1 → stage 2 (§5.6): one row per scripted server delivery
CREATE TABLE inbox(seq INTEGER PRIMARY KEY, step INTEGER NOT NULL, events BLOB NOT NULL);
-- mirrored state (contacts, conversations, settings), re-emitted after a core.db rebuild
CREATE TABLE mirror(seq INTEGER PRIMARY KEY, event TEXT NOT NULL);
-- durable commands from the core (unique on cmd, §4.8)
CREATE TABLE jobs(cmd INTEGER PRIMARY KEY, kind TEXT NOT NULL, created_at INTEGER NOT NULL);

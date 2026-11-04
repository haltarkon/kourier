-- WhatsApp backend.db, second schema (docs/architecture.md §5.6 and §7.1, step W3a): the receive path's
-- durable buffer and ledger. FROZEN once it ran on a kept profile (lane decision 6): a change is a new
-- number, never an edit of this file.
--
-- `inbox` is stage 1's output: one row per received message (or newsletter/PDO item), written in the
-- zone the durability hook commits, BEFORE the server is told anything. Stage 2 turns a `new` row into
-- core events in the zone that sets it `done` and wipes its content; a `done` row stays as the ledger
-- that makes a redelivery a no-op (the UNIQUE key is the one upstream's hook contract asks for:
-- (chat, sender, id)) and is deleted only after a later connection's drain completed durably (D7).
-- `message` is the encoded `wa::Message`, `meta` OUR serde struct (`InboxMeta`), never upstream's
-- Serialize output. Both are NULL once the row is `done`.
-- A row of a kind the build that read it cannot map (a revoke, an edit, a reaction, a poll vote, a
-- channel's item, a sender or chat of a server kind it does not know) is `deferred`: it keeps its
-- content, the poison rule never sees it, and a LATER build whose map version is higher takes it up
-- again (`map_version` is the version of the map that deferred it). W3a fix F3.
CREATE TABLE inbox(
  arrival INTEGER PRIMARY KEY AUTOINCREMENT,   -- never reused: ties in the core's sort key
  chat TEXT NOT NULL,
  sender TEXT NOT NULL,
  id TEXT NOT NULL,
  origin TEXT NOT NULL CHECK (origin IN ('e2e', 'newsletter', 'pdo')),
  received_at INTEGER NOT NULL,                -- wall clock (ms) of the commit
  catch_up INTEGER NOT NULL CHECK (catch_up IN (0, 1)),
  message BLOB,
  meta BLOB,
  state TEXT NOT NULL CHECK (state IN ('new', 'deferred', 'done')),
  attempts INTEGER NOT NULL DEFAULT 0,         -- content failures of stage 2 (the poison rule)
  parked_until INTEGER NOT NULL DEFAULT 0,     -- wall clock (ms) before which stage 2 leaves the row
  map_version INTEGER NOT NULL DEFAULT 0,      -- the map that deferred the row (state 'deferred')
  UNIQUE (chat, sender, id),
  CHECK (state = 'done' OR (message IS NOT NULL AND meta IS NOT NULL))
) STRICT;
CREATE INDEX inbox_by_state ON inbox(state, arrival);

-- The catch-up boundary of the CURRENT connection (§4.5): the arrival number of the last row the
-- offline drain committed, NULL while the drain is not known to be over (every new connection starts
-- there). One row.
CREATE TABLE catch_up(
  id INTEGER PRIMARY KEY CHECK (id = 1),
  boundary INTEGER
) STRICT;
INSERT INTO catch_up(id, boundary) VALUES (1, NULL);

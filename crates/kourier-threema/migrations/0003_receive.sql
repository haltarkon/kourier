-- The Threema backend.db, schema 3 (component `threema`, TH5): what the receive path keeps for a
-- later build. FROZEN once committed: a change is a new number. STRICT tables; no table is WITHOUT
-- ROWID (S10: rows with BLOBs), so NOT NULL is written out on the text and blob columns (a CHECK
-- passes NULL).

-- DEFERRED, NOT LOST (WhatsApp's F3; TH5 decision C3): an envelope this build received, opened and
-- acked but cannot map yet (a kind it does not know, a modifier it does not apply yet). After the ack
-- this row is the only copy. A later build maps it again at a start when `map_version` is BELOW its
-- own MAP_VERSION (a row AT the current version is not mapped again): paged by `id`, at most 30 rows
-- per zone, each row deleted (mapped) or given the new `map_version` and `kind` (deferred again) IN the
-- zone that writes its events; a mapped MESSAGE is stated as a HISTORY row (one `HistoryBatch` per
-- zone and conversation: never a live notification, and the core's cut of a chat cleared meanwhile
-- refuses it), so no catch-up flag is kept.
-- PRIVACY (critic L8): `envelope` is the PLAINTEXT (an edit's text, a file's key), inside this
-- SQLCipher file only; it outlives a Clear Chat or a delete of its chat by up to 45 days (the age
-- bound below).
-- BOUNDS (decision C14), in code (`store::DeferredBounds::REAL`): at most 20,000 rows and 256 MiB of
-- `envelope` (the kept sum in `deferred_total`); over, rows go in the zone of the insert in THE
-- EVICTION ORDER (TH5 round B, M-2): the oldest (lowest `id`) of the LOW-VALUE kinds first (delivery
-- receipts, read / sent updates, calls: 'receipt', 'read', 'sent', 'call', 'group-call'), then the
-- oldest of the rest. Rows whose `received_at` is more than 45 days old go in the zone of every insert
-- and once per start, after the first `ServerInfo` whose clock check passed (`store::expire_deferred`).
-- The SYNC kinds ('settings', 'profile', 'list-sync': deltas of a stored BLOB, few, and their loss
-- leaves the BLOB wrong for good) are never evicted and never expire; they count toward the bounds.
-- The kind lists are the code's (`reflect::classify::LOW_VALUE_KINDS`, `EXEMPT_KINDS`), not this file's.
-- Never INSERT OR REPLACE into this table: the rows REPLACE deletes fire no delete trigger (SQLite's
-- recursive_triggers is off), and `deferred_total` would drift.
CREATE TABLE deferred (
  -- Arrival order, never reused (AUTOINCREMENT, as `core_outbox.seq`; without it an emptied table
  -- starts again at 1): a lower id is an older row, and a page cursor stays valid whatever is written
  -- between two pages.
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  -- Where it came from: 'reflected' (TH5: a D2M `Reflected` of another device) or 'csp' (TH6: the
  -- leader's receive from the chat server).
  source      TEXT    NOT NULL CHECK (source IN ('reflected', 'csp')),
  -- The rule that deferred it, a short name ('envelope', 'unknown-type', ...).
  kind        TEXT    NOT NULL CHECK (length(kind) BETWEEN 1 AND 32 AND kind NOT GLOB '*[^a-z0-9-]*'),
  -- The MAP_VERSION of the build that deferred it (last); TH5's is 1.
  map_version INTEGER NOT NULL CHECK (map_version >= 1),
  -- What the transport said with it that a later mapping needs (the envelope holds the rest). For
  -- 'reflected': `Reflected.timestamp` (ms, the mediator's, a u64 stored as the i64 of the same 64 bits:
  -- a message's `server_at`, a delete's `at`) and `Reflected.flags` (u16; an EPHEMERAL one is never
  -- deferred). For 'csp': the message's `created-at` (as ms) and its flags (u8). The Reflected ID is
  -- not kept: it names the message for its ack only (sent before this row is read again) and wraps.
  server_at   INTEGER NOT NULL,
  flags       INTEGER NOT NULL CHECK (flags BETWEEN 0 AND 65535),
  -- When the zone that wrote the row ran (ms, the local clock): the age bound.
  received_at INTEGER NOT NULL,
  -- The plaintext AS RECEIVED: for 'reflected' the decrypted `d2d.Envelope` with its padding and every
  -- field a newer phone added (a prost re-encode would drop them); at most a 64 KiB reflection. For 'csp'
  -- (TH6): the `d2d.Envelope` the leader reflects for that message (its sender, message id, nonce and
  -- body), so one mapper serves both sources; the sender and the id are in no column of their own.
  envelope    BLOB    NOT NULL CHECK (length(envelope) BETWEEN 1 AND 65536)
) STRICT;

-- The re-map's look-up of the rows below a version (a start with nothing to map again is one seek).
CREATE INDEX deferred_by_version ON deferred(map_version);
-- The age bound's delete (one seek when nothing is due).
CREATE INDEX deferred_by_age ON deferred(received_at);
-- The eviction order's look-up (TH5 round B, M-2): the oldest row of ONE kind is one seek
-- (`SELECT id ... WHERE kind = ? ORDER BY id LIMIT 1`, once per low-value kind), whatever the table
-- holds; without it every insert into a full table would scan up to 20,000 rows. An index on the kind
-- rather than a priority column: which kinds are low-value stays the code's, a later build may change
-- the list without a migration.
CREATE INDEX deferred_by_kind ON deferred(kind, id);

-- THE KEPT RUNNING SUM (critic L9): one row, the count of `deferred`'s rows and the sum of their
-- `length(envelope)`, so the bounds are checked without a scan of 256 MiB. Kept by the three triggers
-- below, which run inside the statement that changes `deferred`: the sum is exact after every zone
-- that commits and rolls back with every zone that does not, whoever writes the row (the batch's
-- insert, the bounds, the age bound, the re-map's delete and update; a later step's own statement
-- cannot forget it, as store code could).
CREATE TABLE deferred_total (
  id        INTEGER PRIMARY KEY CHECK (id = 1),
  row_count INTEGER NOT NULL CHECK (row_count >= 0),
  byte_sum  INTEGER NOT NULL CHECK (byte_sum >= 0)
) STRICT;

INSERT INTO deferred_total (id, row_count, byte_sum) VALUES (1, 0, 0);

CREATE TRIGGER deferred_total_insert AFTER INSERT ON deferred BEGIN
  UPDATE deferred_total SET row_count = row_count + 1, byte_sum = byte_sum + length(NEW.envelope)
  WHERE id = 1;
END;

CREATE TRIGGER deferred_total_delete AFTER DELETE ON deferred BEGIN
  UPDATE deferred_total SET row_count = row_count - 1, byte_sum = byte_sum - length(OLD.envelope)
  WHERE id = 1;
END;

CREATE TRIGGER deferred_total_update AFTER UPDATE OF envelope ON deferred BEGIN
  UPDATE deferred_total SET byte_sum = byte_sum - length(OLD.envelope) + length(NEW.envelope)
  WHERE id = 1;
END;

-- The Threema backend.db, schema 4 (component `threema`, TH6a): what is being sent. FROZEN once committed: a
-- change is a new number. STRICT tables; no table is WITHOUT ROWID (S10: rows with BLOBs), so NOT NULL is written
-- out on the text and blob columns (a CHECK passes NULL).
--
-- SEALED, AND THE KEY IS IN THE SAME FILE. Every body is sealed before its row is written (`reflect_env`,
-- `update_env`: under DGRK, the nonce ahead; `payload`: the whole message-with-metadata-box, its nonce inside),
-- because a resend must be byte for byte what went before. That is no privacy boundary: CK and DGK rest in this
-- SQLCipher file too, so whoever reads a row can open it. In the clear inside the file: the receiver IDs, the
-- conversation, the message id, the times.
--
-- The budgets, the ages and the zones are the code's (`src/send/queue.rs`), not this file's. Never INSERT OR REPLACE
-- into these tables: a PREPARE is a plain INSERT after the ledger's read.
CREATE TABLE send_task (
  -- Queue order, never reused (AUTOINCREMENT, as `core_outbox.seq` and `deferred.id`).
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  -- 'transmit': the core's outbox owns the row. 'job': the backend's own (a delivery receipt, an FS reject, a
  -- creator's answer), deleted at its end. When each may run is the code's (`next_send`).
  origin       TEXT    NOT NULL CHECK (origin IN ('transmit', 'job')),
  -- A short name for logs and later rules: the code's, not this file's.
  kind         TEXT    NOT NULL CHECK (length(kind) BETWEEN 1 AND 32 AND kind NOT GLOB '*[^a-z0-9-]*'),
  -- `req.local`, `msg_id`'s text form: the ledger's key.
  local        TEXT    CHECK (local IS NULL OR (length(local) = 16 AND local NOT GLOB '*[^0-9a-f]*')),
  -- `MessageId.0` as the i64 of its 64 bits.
  message_id   INTEGER NOT NULL,
  -- The prost bytes of a `d2d.ConversationId` (contact, group or list): what the Sent update names, and what the
  -- user's clear looks the row up by.
  conversation BLOB    NOT NULL CHECK (length(conversation) BETWEEN 1 AND 256),
  -- The message's own time (ms): `req.sent_at`, or the clock of a job.
  created_at   INTEGER NOT NULL CHECK (created_at >= 0),
  state        TEXT    NOT NULL CHECK (state IN ('prepared', 'reflected', 'acked', 'done')),
  -- Whether the end of the boxes owes the device group a Sent update (a type that has none goes on to its end).
  has_update   INTEGER NOT NULL CHECK (has_update IN (0, 1)),
  -- The update's reflect-ack time (ms): `TransmitOutcome::Sent.server_at`.
  sent_at      INTEGER,
  -- What the row HOLDS: `length(reflect_env) + length(update_env) + sum(length(payload))` of its receivers, a NULL
  -- counting 0: the open bytes' budget. Written at PREPARE, then recomputed from the stored lengths by every zone that
  -- nulls a BLOB (REFLECTED, ACKS, DONE) or writes the update (ACKS), so an ack lowers it. It reads no BLOB.
  bytes        INTEGER NOT NULL CHECK (bytes >= 0),
  -- The FAILED RUNS of this row (not its starts): a socket that ended by a missing ack or by the server's close with
  -- frames of the row unanswered. Durable, so a restart loses a row's cooling and not its count.
  attempts     INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
  -- When PREPARE ran (ms, the local clock): the age of an open row.
  queued_at    INTEGER NOT NULL,
  -- When the row reached `done` (ms, the local clock): the age of the ledger's row.
  finished_at  INTEGER,
  -- THE TWO LAST COLUMNS ARE THE BLOBS (critic L2): SQLite walks a row's overflow pages to reach a column behind a
  -- large BLOB, and the budgets, `next_send` and the sweep read the columns above on every row they pass.
  -- The sealed `OutgoingMessage`; NULL once its reflect-ack is durable, or for a type that does not reflect.
  reflect_env  BLOB    CHECK (reflect_env IS NULL OR length(reflect_env) BETWEEN 40 AND 65516),
  -- The sealed `OutgoingMessageUpdate`, written by the last ACKS zone; NULL again once its reflect-ack is durable.
  update_env   BLOB    CHECK (update_env IS NULL OR length(update_env) BETWEEN 40 AND 65516),
  CHECK ((origin = 'transmit') = (local IS NOT NULL)),
  CHECK (state <> 'prepared' OR reflect_env IS NOT NULL),
  CHECK (state <> 'acked' OR update_env IS NOT NULL),
  CHECK (state <> 'done' OR (reflect_env IS NULL AND update_env IS NULL AND finished_at IS NOT NULL))
) STRICT;

-- The ledger's one search: a `local` has one row, whatever its state.
CREATE UNIQUE INDEX send_by_local ON send_task (local) WHERE local IS NOT NULL;
-- The open rows in queue order, COVERING for the walks: `next_send` (id, origin, state), the budgets (origin, bytes) and
-- the eviction's search (origin, state, bytes, queued_at) read index pages only, never a row of the table, and no `done`
-- row. `id` stays first: `next_send` walks in queue order. A walk of the whole partial index, not a single search.
CREATE INDEX send_task_open ON send_task (id, origin, state, bytes, queued_at) WHERE state <> 'done';
-- The `done` rows by age: the sweep runs at every socket, and it is one search when nothing is due.
CREATE INDEX send_task_done ON send_task (finished_at) WHERE state = 'done';

CREATE TABLE send_receiver (
  task     INTEGER NOT NULL REFERENCES send_task (id) ON DELETE CASCADE,
  identity TEXT    NOT NULL CHECK (length(identity) = 8),
  -- 'pending': its box is owed. 'acked': the chat server's message-ack is durable. 'blocked', 'invalid', 'no-key': no
  -- box, for the reason named. 'omitted': no box, left out for this type by the receiver's features (critic L3: a
  -- reaction or a poll update to a receiver that cannot take it, from TH7; it says nothing false to the core).
  state    TEXT    NOT NULL CHECK (state IN ('pending', 'acked', 'blocked', 'invalid', 'no-key', 'omitted')),
  -- The whole box as the chat server gets it; NULL once acked. It is at least the 64-byte head and a 24-byte nonce.
  payload  BLOB    CHECK (payload IS NULL OR length(payload) BETWEEN 88 AND 8170),
  CHECK ((state = 'pending') = (payload IS NOT NULL)),
  PRIMARY KEY (task, identity)
) STRICT;

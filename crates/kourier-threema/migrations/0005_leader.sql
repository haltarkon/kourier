-- The Threema backend.db, schema 5 (component `threema`, TH6): what the LEADER took from the chat server. FROZEN once
-- committed: a change is a new number. STRICT; no table is WITHOUT ROWID (S10: rows with BLOBs), so NOT NULL is written
-- out on the text and blob columns (a CHECK passes NULL).
--
-- ONE ROW PER MESSAGE THE LEADER KEPT (sender, message id of the clear head; a spec discard, a held message and a typing
-- indicator write none). It is written by ZONE RECEIVE together with the message's events, its CSP nonce and the D2D
-- nonce of its reflect, as 'pending' with the SEALED reflect; ZONE REFLECTED makes the reflect-ack durable ('reflected':
-- the sealed bytes and the nonce's hash dropped) before the chat server's message-ack goes. STEP 0: every incoming message
-- is looked up here by (sender, message id) BEFORE the nonce check, so a redelivery after any kill resumes ('pending':
-- the stored reflect again; 'reflected': the ack again). A 'reflected' row is the spec's step-14 memory.
--
-- SEALED, AND THE KEY IS IN THE SAME FILE (as 0004): `reflect_env` is the `d2d.Envelope { incoming_message }` sealed under
-- DGRK, because a resend must be byte for byte what went before. In the clear: the sender ID and the message id, as in
-- `send_receiver`. No text, nickname or body in a column.
--
-- THE SWEEP and its ages are the code's (`src/leader/zone.rs`): 'reflected' rows past 14 days; 'pending' rows past 30
-- days WITH THEIR CSP NONCE (critic T-L5 (b): `nonce_hash` is the hash ZONE RECEIVE wrote into `csp_nonce`), so a message
-- the chat server still keeps comes again as new, is reflected and acked, instead of meeting its own nonce and being
-- acked unreflected; the nonce of a 'reflected' row stays for good. Never INSERT OR REPLACE: RECEIVE inserts after the
-- step-0 read.
CREATE TABLE csp_inbound (
  -- Arrival order, never reused (AUTOINCREMENT, as `send_task.id`): the reflect-ack's owner on the socket.
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  -- The sender's Threema ID, from the clear head.
  sender       TEXT    NOT NULL CHECK (length(sender) = 8),
  -- The message id, the u64 as the i64 of its 64 bits.
  message_id   INTEGER NOT NULL,
  state        TEXT    NOT NULL CHECK (state IN ('pending', 'reflected')),
  -- 1: a CSP message-ack is owed (the message has no flag 0x04).
  ack          INTEGER NOT NULL CHECK (ack IN (0, 1)),
  -- 1: ZONE REFLECTED enqueues a delivery receipt for it (a type that has one, no flag 0x80).
  receipt      INTEGER NOT NULL CHECK (receipt IN (0, 1)),
  -- When ZONE RECEIVE ran (ms, the local clock): the age of a 'pending' row.
  received_at  INTEGER NOT NULL,
  -- When ZONE REFLECTED ran (ms, the local clock): the age of a 'reflected' row.
  reflected_at INTEGER,
  -- THE TWO LAST COLUMNS ARE THE BLOBS (TH6a's critic L2: the sweep and step 0 read the columns above).
  -- The CSP nonce as `csp_nonce` holds it (hashed with the own ID); only while 'pending', for the sweep.
  nonce_hash   BLOB    CHECK (nonce_hash IS NULL OR length(nonce_hash) = 32),
  -- The sealed reflect; only while 'pending'.
  reflect_env  BLOB    CHECK (reflect_env IS NULL OR length(reflect_env) BETWEEN 40 AND 65516),
  CHECK ((state = 'pending') = (reflect_env IS NOT NULL)),
  CHECK ((state = 'pending') = (nonce_hash IS NOT NULL)),
  CHECK ((state = 'reflected') = (reflected_at IS NOT NULL)),
  -- Step 0's one search.
  UNIQUE (sender, message_id)
) STRICT;

-- The sweep's two searches: each is one search when nothing is due.
CREATE INDEX csp_inbound_pending ON csp_inbound (received_at) WHERE state = 'pending';
CREATE INDEX csp_inbound_done ON csp_inbound (reflected_at) WHERE state = 'reflected';

-- WhatsApp backend.db, fifth schema (W5a, chunk 6): what the send path keeps.
-- FROZEN once it ran on a kept profile: a change is a new number, never an edit of this file. (No kept
-- WhatsApp profile has run W5a yet, so the chunks of W5a that find a column missing may still amend this
-- file until then.)
--
-- Nothing here holds a message body: ids the core knows (encoded conversation keys, the core's local ids
-- of its own messages), outcomes, times, and the targets of a receipt job.

-- THE OUTCOME LEDGER (decision D4): what the server answered to a message Kourier sent, committed in ONE
-- zone (kill points `wa_inside_send_outcome` / `wa_after_send_outcome`) BEFORE the core is answered, and
-- read first by every Transmit: `acked` is answered `Sent` and `refused` (the server's 463, the reach-out
-- refusal) `Failed(ReachOutRefused)`, both with NO frame, so a resend whose outcome the core lost never
-- reaches the server a second time. Failures are NOT recorded (critic H2): Send Again must work for every
-- failure but the 463.
--   conv      the encoded conversation key of the request
--   local     the core's id of the message (`Transmit.local`), which is also its WhatsApp id
--   kind      'message', 'edit', 'revoke' or 'react'
--   target    the core's id of the message an edit or a revoke is about (NULL for the others): an acked
--             revoke takes upstream's `sent_messages` rows of the original AND of its acked edits in the
--             same zone, so a late retry receipt never sends again a text the person deleted (critic L1)
--   server_t  the server's time of the ack (seconds), when it said one; the edit window judges the
--             original's age by it (D9)
--   at        when the row was written (wall clock, ms): rows go at a start after 14 days
CREATE TABLE send_outcomes(
  conv TEXT NOT NULL,
  local TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('message', 'edit', 'revoke', 'react')),
  outcome TEXT NOT NULL CHECK (outcome IN ('acked', 'refused')),
  target TEXT,
  server_t INTEGER,
  at INTEGER NOT NULL,
  PRIMARY KEY (conv, local)
) STRICT, WITHOUT ROWID;
CREATE INDEX send_outcomes_by_target ON send_outcomes(conv, target) WHERE target IS NOT NULL;
CREATE INDEX send_outcomes_by_age ON send_outcomes(at);
-- The core's locals are unique per account, so a message is found by its local alone (the conversation form it was
-- sent under may not be the one a later look asks with: fixes F9 and F13).
CREATE INDEX send_outcomes_by_local ON send_outcomes(local);

-- THE STARTED FACT (decision D7, D6 in the backend): a DIRECT chat counts as started on the phone once one
-- of these was seen, written in the zone that sees it: 0 a message from the peer (stage 2's zone), 1 an own
-- message from another of our devices (stage 2's zone), 2 a history chat of which the chunk carries at least
-- one REAL message of either side (W4's delivery zone; a chats row alone is no start, fix F2), 3 an own message the
-- server acked (the ledger's zone). A call line or call record, a system line, typing, a receipt, a presence or a
-- merge of two ids is no start. Until a chat has a row the backend states `cannot_send:
-- NotAccepted` for it and refuses to send anything into it. Groups have no row: they are always started.
-- `how` keeps the first reason only.
CREATE TABLE chat_started(
  conv TEXT PRIMARY KEY,
  how INTEGER NOT NULL CHECK (how BETWEEN 0 AND 3)
) STRICT, WITHOUT ROWID;

-- The features each chat was last stated with (brief §2.8, decision D9): a chat is stated once per
-- (`version` of the features, `started` fact), in the zone that states it. A `Rebuild` start empties the
-- table in the zone that restates `AccountReady` (critic M4), and the start sweep states every chat again.
CREATE TABLE features_stated(
  conv TEXT PRIMARY KEY,
  version INTEGER NOT NULL,
  started INTEGER NOT NULL CHECK (started IN (0, 1))
) STRICT, WITHOUT ROWID;

-- The receipts the core asked for (`Command::SendReceipts`, decision D10): ONE zone writes the job and the
-- `CommandPersisted`; a runner sends it when connected and deletes the row after the write returned Ok (a
-- kill after the write sends the receipt again: harmless).
--   cmd      the command's id (`CmdId`)
--   conv     the encoded conversation key
--   kind     'delivered', 'read' or 'viewed'
--   targets  the messages, encoded (JSON of the core's targets)
--   at       when the job was written (wall clock, ms)
CREATE TABLE receipt_jobs(
  cmd INTEGER PRIMARY KEY,
  conv TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('delivered', 'read', 'viewed')),
  targets BLOB NOT NULL,
  at INTEGER NOT NULL
) STRICT;

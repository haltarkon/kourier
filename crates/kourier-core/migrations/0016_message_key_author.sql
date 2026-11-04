-- K-B1 "ids, edits, clears" (2026-10-16): a third message-id scope and a clear that has a time.
--
-- 1. `messages.net_scope` 2 = `MsgIdScope::ConversationAuthor` (WhatsApp: ids are unique per
-- SENDER in a chat, so two members of a group may use one id). The key is `(conversation,
-- author, local)`; `net_author` is the author after identity resolution, '' = own. The index is
-- PARTIAL on `net_scope = 2`: SQLite uses a partial index only for a statement whose WHERE
-- clause implies the condition, so no statement that does not say `net_scope = 2` can pick it,
-- and it can tie with none of the hot statements of the other scopes (core.db has no planner
-- statistics: an index that ties with an older key is taken when it is newer, the trap of
-- migration 14). No row is converted and no table is rebuilt: no core.db holds a WhatsApp
-- message row yet (WhatsApp is registered in debug builds only, no real link has happened), and
-- Signal and the call lines keep scopes 0 and 1. The rule "K-B1 lands before the first real
-- WhatsApp run" is the lead's; rows of scope 1 that a WhatsApp account made before this schema
-- stay scope 1 and are not found by the new key.
--
-- 2. `conversation_cuts`: a clear BY TIME (`BackendEvent::ConversationCleared.before_ms`, and
-- the user's own clear). `before_ms` is the newest `sent_at` the clear covers, in the clock of
-- the network's `sent_at`; a HISTORY row of the conversation at or below it is dropped from
-- then on (`ingest::message`, once per `HistoryBatch`), so a chat cleared on the phone, or
-- here, is not filled up again by the history that keeps coming. Keyed by the canonical
-- conversation KEY, not the row id: a clear can come before the conversation exists here (an
-- app-state patch before the history chunk that makes the chat). It only rises
-- (`before_ms = max(...)`). `claimed_ms` is the CLAIM the cut came from, before the clamp to the
-- core's clock (+ the tolerance, `CLEAR_CLOCK_TOLERANCE`): a delivery raises the cut only when its
-- claim is larger than the stored one, so a clear that is delivered again (an app-state resync, a
-- stage-2 redelivery) is a no-op, not a second clear at a later "now" (fix round F2). Merges move
-- it with the alias (`ingest::merge`, `CUTS_REKEY`: the larger of both); the account's removal
-- takes it (ON DELETE CASCADE); a removed chat keeps it (a re-made chat's older history stays
-- dropped, as on the phone).
CREATE UNIQUE INDEX messages_key_conv_author ON messages(conversation_id, net_author, net_local)
  WHERE net_scope = 2;
CREATE TABLE conversation_cuts(
  account_id INTEGER NOT NULL REFERENCES accounts ON DELETE CASCADE,
  conv_key TEXT NOT NULL,
  before_ms INTEGER NOT NULL,
  claimed_ms INTEGER NOT NULL,
  PRIMARY KEY(account_id, conv_key)) WITHOUT ROWID;

-- Cloud ids and read positions, step KT-2 (2026-11-01): a network whose SERVER names messages,
-- orders them and counts what is unread (Telegram) can say so through the contract, and the core
-- keeps its promises under those rules. Every new column stays NULL, and the new table empty, on a
-- network that states none of it (Signal, WhatsApp, Threema).
--
-- `messages.net_seq` is the message's position in its conversation: higher is newer, the backend
-- states it and the core never derives it from `net_local`. `conversations.read_seq` and
-- `peer_read_seq` are the two read positions, which only rise. `unread_server` and
-- `mentions_server` are the server's counts (`NetworkDescriptor.server`), NULL while the rows
-- count. `message_aliases` keeps the temporary id of an own message the server gave an id of its
-- own, so a late copy or a quote under the old id still finds the row and the old id stays taken.
--
-- No row is converted and no table is rebuilt (`messages.quiet`'s definition is untouched).
--
-- `messages_seq` is PARTIAL: SQLite uses a partial index only for a statement whose WHERE clause
-- implies the condition, so no statement that does not name `net_seq` can pick it, and it can tie
-- with none of the hot statements (core.db has no planner statistics: an index that ties with an
-- older key is taken when it is newer, the trap of migration 14; `tests/cloud_ids.rs` holds the
-- plans).
--
-- `messages_unread_seq` is the unread rows that have a position, in position order: the own read
-- position reads what is unread AT OR BELOW it (`READ_BY_POSITION`, pinned to it) and walks no other
-- unread row. A server that restates its read position with every change of its count (TDLib's
-- `updateChatReadInbox`) sends a position that does not rise with each message; through
-- `messages_unread` each one walked every unread row of the chat (a muted group: thousands). PARTIAL
-- for the same reason as `messages_seq`, and empty on a network that numbers nothing.
--
-- An alias is NOT a `message_revisions` row: revisions are read as a message's edit history (its
-- versions, the edit mark, the outbox's edits, the clears, the restore, `reindex_revisions`), and
-- a fake revision would show or count in each.
--
-- The next test archive's note: core.db 22 -> 23 (back up first; five columns, one table, three
-- indexes, no row converted). The number is settled at the landing, and the runner records only
-- the number: until then no lane build runs on a kept profile with real data but a throwaway copy.
ALTER TABLE messages ADD COLUMN net_seq INTEGER;
CREATE INDEX messages_seq ON messages(conversation_id, net_seq) WHERE net_seq IS NOT NULL;
CREATE INDEX messages_unread_seq ON messages(conversation_id, net_seq)
  WHERE read_state = 'Unread' AND net_seq IS NOT NULL;
ALTER TABLE conversations ADD COLUMN read_seq INTEGER;        -- own read position, only rises
ALTER TABLE conversations ADD COLUMN peer_read_seq INTEGER;   -- our messages read up to here, only rises
ALTER TABLE conversations ADD COLUMN unread_server INTEGER;   -- NULL: the rows count
ALTER TABLE conversations ADD COLUMN mentions_server INTEGER; -- NULL: the rows count
CREATE TABLE message_aliases(
  message_id INTEGER NOT NULL REFERENCES messages ON DELETE CASCADE,
  account_id INTEGER NOT NULL,
  net_local TEXT NOT NULL,
  PRIMARY KEY(message_id, net_local)) WITHOUT ROWID;
CREATE INDEX message_aliases_by_local ON message_aliases(account_id, net_local);

-- Phase G (architecture §13).
--
-- The keys of messages the disappearing sweeper deleted. The sweeper removes the row, so without
-- this a late redelivery of the same message (an envelope the server sends again, a second copy
-- under another identity, an echo of our own send, a history import) would bring it back as a
-- new message, with a notification. Ingest drops a message whose key is here. Rows are kept for
-- a bounded time (`expiry::REMEMBER`), counted from `expired_at`, and purged by the sweeper.
--
-- `net_scope` 0: the key is (account, author, local); 1: (conversation key, local), as for
-- `messages` (§3.1). `conv_key` is the canonical key at the time of the sweep; an identity merge
-- rewrites it and `net_author` like the keys of parked modifiers.
CREATE TABLE expired_messages(
  account_id INTEGER NOT NULL REFERENCES accounts ON DELETE CASCADE,
  net_scope INTEGER NOT NULL,
  conv_key TEXT NOT NULL,
  net_author TEXT NOT NULL,                 -- '' = own
  net_local TEXT NOT NULL,
  expired_at INTEGER NOT NULL);
CREATE INDEX expired_messages_by_key ON expired_messages(account_id, net_local);
CREATE INDEX expired_messages_by_time ON expired_messages(expired_at);

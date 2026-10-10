-- Signal backend.db, step S13 (docs/architecture.md §6.7, §6.13): modifiers and timers.

-- The disappearing-message timer of a 1:1 chat in seconds, 0 = off (Desktop's
-- `conversation.expireTimer`): reactions, edits and the timer-update message carry it together
-- with `expire_timer_version`, and an incoming timer is compared with it.
ALTER TABLE peers ADD COLUMN expire_timer INTEGER NOT NULL DEFAULT 0;

-- The quote each of our replies went out with (the encoded `DataMessage.Quote`, thumbnail
-- pointer included), by the reply's network id and by the id of each of its edits: an edit
-- must carry the quote again, or the receivers drop it from the edited message
-- (`SD/ts/util/handleEditMessage.preload.ts`), and the core's edit request does not name it.
-- Replies sent from this device, and replies (and their edits) another device of ours sent,
-- taken from its sync transcripts. Personal data (quoted text). Purged with the Message Send
-- Log.
CREATE TABLE sent_quotes(
  local TEXT PRIMARY KEY,                 -- the network id of our reply or of one of its edits
  quote BLOB NOT NULL,                    -- encoded DataMessage.Quote
  created_at INTEGER NOT NULL             -- ms
) STRICT, WITHOUT ROWID;
CREATE INDEX sent_quotes_created ON sent_quotes(created_at);

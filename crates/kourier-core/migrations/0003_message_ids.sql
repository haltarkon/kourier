-- Message ids are never reused (architecture §13 phase E "Still open"): `messages.id` travels to
-- backends as `LocalMsgId`, and with a plain INTEGER PRIMARY KEY SQLite handed out max(id) + 1, so
-- the id of a cancelled newest message, or of every message of a removed conversation, came back
-- for the next one. The table is rebuilt with AUTOINCREMENT, keeping every row and id; the
-- tables that reference it (revisions, reactions, receipts, send_state, attachments, outbox, the
-- self-reference of broadcast copies) keep their rows and point at the new table after the rename.
-- Runs as a rebuild step: foreign keys are off for the run and checked before the commit.

CREATE TABLE messages_v2(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id INTEGER NOT NULL REFERENCES accounts ON DELETE CASCADE,
  conversation_id INTEGER NOT NULL REFERENCES conversations ON DELETE CASCADE,
  net_scope INTEGER NOT NULL,               -- 0 AccountAuthor, 1 Conversation
  net_author TEXT NOT NULL,                 -- '' = own
  net_local TEXT NOT NULL,
  origin TEXT NOT NULL,                     -- Remote | OwnThisDevice | OwnOtherDevice
  device INTEGER,
  linked_message_id INTEGER REFERENCES messages ON DELETE SET NULL,
  sort_key INTEGER NOT NULL,
  sent_at INTEGER NOT NULL, server_at INTEGER, received_at INTEGER NOT NULL, arrival INTEGER,
  body_kind TEXT NOT NULL, body TEXT NOT NULL, -- JSON Body (latest revision)
  preview TEXT,                             -- plain text for the conversation list
  reply_to TEXT,                            -- JSON ReplyTo
  revision INTEGER NOT NULL DEFAULT 0, edited_at INTEGER,
  deleted TEXT, deleted_by TEXT,            -- ForEveryone | ForMe; '' = own
  out_floor TEXT,                           -- JSON OutgoingState from explicit sources (max)
  out_state TEXT,                           -- JSON OutgoingState: floor merged with send_state + receipts
  read_state TEXT,                          -- Unread | Read | Viewed (incoming only)
  expiry_kind TEXT, expiry_timer_ms INTEGER, expiry_started_at INTEGER, expires_at INTEGER,
  view_once INTEGER NOT NULL DEFAULT 0, history INTEGER NOT NULL DEFAULT 0,
  silent INTEGER NOT NULL DEFAULT 0, mentions_me INTEGER NOT NULL DEFAULT 0,
  meta BLOB, raw BLOB);
INSERT INTO messages_v2(id, account_id, conversation_id, net_scope, net_author, net_local, origin,
    device, linked_message_id, sort_key, sent_at, server_at, received_at, arrival, body_kind, body,
    preview, reply_to, revision, edited_at, deleted, deleted_by, out_floor, out_state, read_state,
    expiry_kind, expiry_timer_ms, expiry_started_at, expires_at, view_once, history, silent,
    mentions_me, meta, raw)
  SELECT id, account_id, conversation_id, net_scope, net_author, net_local, origin, device,
    linked_message_id, sort_key, sent_at, server_at, received_at, arrival, body_kind, body,
    preview, reply_to, revision, edited_at, deleted, deleted_by, out_floor, out_state, read_state,
    expiry_kind, expiry_timer_ms, expiry_started_at, expires_at, view_once, history, silent,
    mentions_me, meta, raw
  FROM messages ORDER BY id;
DROP TABLE messages;
ALTER TABLE messages_v2 RENAME TO messages;
CREATE UNIQUE INDEX messages_key_account ON messages(account_id, net_author, net_local) WHERE net_scope = 0;
CREATE UNIQUE INDEX messages_key_conv ON messages(conversation_id, net_local) WHERE net_scope = 1;
CREATE INDEX messages_timeline ON messages(conversation_id, sort_key DESC, id DESC);
CREATE INDEX messages_local ON messages(account_id, net_local);
CREATE INDEX messages_unread ON messages(conversation_id)
  WHERE read_state = 'Unread' AND history = 0 AND deleted IS NULL;
CREATE INDEX messages_expiring ON messages(expires_at) WHERE expires_at IS NOT NULL;
CREATE INDEX messages_linked ON messages(linked_message_id) WHERE linked_message_id IS NOT NULL;

-- One row per download request; `id` is the request id echoed in `AttachmentDone` (§4.2), never
-- reused, so a late answer to an earlier request cannot finish or fail a newer one. A resend
-- after a restart keeps the row and its id.
CREATE TABLE attachment_downloads_v2(
  id INTEGER PRIMARY KEY AUTOINCREMENT,     -- = DownloadId
  attachment_id INTEGER NOT NULL UNIQUE REFERENCES attachments ON DELETE CASCADE,
  account_id INTEGER NOT NULL REFERENCES accounts ON DELETE CASCADE,
  path TEXT NOT NULL,                       -- relative to accounts/<uuid>/blobs
  key BLOB NOT NULL,
  requested_at INTEGER NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0);    -- actor starts that resent it
INSERT INTO attachment_downloads_v2(attachment_id, account_id, path, key, requested_at, attempts)
  SELECT attachment_id, account_id, path, key, requested_at, attempts FROM attachment_downloads
  ORDER BY requested_at, attachment_id;
DROP TABLE attachment_downloads;
ALTER TABLE attachment_downloads_v2 RENAME TO attachment_downloads;
CREATE INDEX attachment_downloads_by_account ON attachment_downloads(account_id);

-- Outgoing edits, deletes and reactions (§4.8) are outbox rows of their target message
-- (`kind` = Edit | Delete | React). `undo` is the JSON snapshot that rolls the optimistic local
-- state back when the network refuses the change for good.
ALTER TABLE outbox ADD COLUMN undo TEXT;
CREATE INDEX outbox_by_conversation ON outbox(conversation_id, id);

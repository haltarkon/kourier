-- Outbox row ids are never reused either: the dispatcher keys the attempts in flight by
-- `outbox.id`, and a row can go while its transmit is in flight (an own copy from another device
-- finished it, its message expired, its conversation was removed). With a plain INTEGER PRIMARY
-- KEY the next row could get the same id, and the late answer to the old attempt would be
-- recorded for the new row. The table is rebuilt with AUTOINCREMENT, keeping every row and id
-- (the per-conversation FIFO is `min(id)`); new ids continue after the highest one present.
-- No table references `outbox`, so this is a plain step: dropping the old table runs no
-- foreign-key action, with or without enforcement.

CREATE TABLE outbox_v2(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  message_id INTEGER NOT NULL REFERENCES messages ON DELETE CASCADE,
  conversation_id INTEGER NOT NULL REFERENCES conversations ON DELETE CASCADE,
  kind TEXT NOT NULL, request TEXT NOT NULL, -- Message | Edit | Delete | React; JSON OutgoingRequest
  state TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, next_attempt_at INTEGER,
  deadline_at INTEGER NOT NULL, last_error TEXT,
  undo TEXT);                               -- JSON rollback of an edit, delete or reaction
INSERT INTO outbox_v2(id, message_id, conversation_id, kind, request, state, attempts,
    next_attempt_at, deadline_at, last_error, undo)
  SELECT id, message_id, conversation_id, kind, request, state, attempts, next_attempt_at,
    deadline_at, last_error, undo
  FROM outbox ORDER BY id;
DROP TABLE outbox;
ALTER TABLE outbox_v2 RENAME TO outbox;
CREATE INDEX outbox_by_message ON outbox(message_id);
CREATE INDEX outbox_by_conversation ON outbox(conversation_id, id);

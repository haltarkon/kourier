-- History import, step S3a (2026-10-09): the rows are the truth about what is unread.
--
-- A row of a history batch that says "not read" (`IncomingMessage.read = false`) is known to
-- be unread and counts for `conversations.unread` and `mentions` like a live row
-- (`kourier-core/src/ingest/mod.rs` `recount`); `unread_hint` is only for unread messages the
-- backend has no rows for. Until now a history row never counted, and the partial index of
-- the unread rows left such rows out (`history = 0`): a statement that counts them could not
-- use it.
--
-- The index now holds every row whose read state is `Unread`, and nothing else is asked of
-- it: not `history`, and not `deleted IS NULL` either. A tombstone keeps its read state (a
-- delete and a read marker of another device must end in the same row in any order), so the
-- statements that mark rows read by a `sent_at` watermark (a read marker of another device,
-- a merge of two chats: `ingest::READ_UP_TO`) cannot say `deleted IS NULL`; with an index
-- that asks for it they walked the whole conversation for EVERY marker, also when nothing
-- was unread any more (16 ms per marker on a chat of 20,000 rows, debug build). The few
-- unread tombstones in the index cost nothing; every statement that counts or lists unread
-- rows still implies it (plan test `ingest::tests`).
DROP INDEX messages_unread;
CREATE INDEX messages_unread ON messages(conversation_id)
  WHERE read_state = 'Unread';
-- A delivery of an earlier build (step S2) gave a chat's unread count as `unread_hint`
-- BESIDE the rows that were those very messages. Rows now count themselves, so the same
-- messages would show twice: where a chat has unread history rows its hint is zero. (That
-- delivery is the only source known to have put history rows beside a hint; its hint
-- counted the phone's unread messages, which are those rows.)
UPDATE conversations SET unread_hint = 0
  WHERE unread_hint > 0 AND EXISTS (SELECT 1 FROM messages m
    WHERE m.conversation_id = conversations.id AND m.history = 1
      AND m.read_state = 'Unread' AND m.deleted IS NULL AND m.expiry_hidden = 0);
-- The counters of every conversation under the new rule (a profile of an earlier build may
-- hold unread rows of a history import that were not counted). What no row and no hint
-- changed stays as it was. The statement is `recount`'s, over all conversations, once.
UPDATE conversations SET
  unread = unread_hint + (SELECT count(*) FROM messages m
    WHERE m.conversation_id = conversations.id AND m.read_state = 'Unread'
      AND m.deleted IS NULL AND m.expiry_hidden = 0),
  mentions = (SELECT count(*) FROM messages m
    WHERE m.conversation_id = conversations.id AND m.read_state = 'Unread'
      AND m.deleted IS NULL AND m.mentions_me = 1 AND m.expiry_hidden = 0);

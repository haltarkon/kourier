-- Quiet notices stay out of the conversation list (2026-10-15; the user: "'Your safety number
-- with ...' messages should have toggle to disable showing in All chat as latest message in
-- chat. It looks like digital garbage.").
--
-- `messages.quiet` is 1 for the notices `SystemEvent::is_quiet` names: a safety number that
-- changed, a refreshed session, a changed number, an accepted message request. The list takes
-- a conversation's preview, its time and so its place from the newest row that is LISTED
-- (`crate::notices::listed!`): a quiet notice only when the user asked for them
-- (`settings` scope `core`, key `quiet_notices`, value `show`; no row = hidden).
--
-- A GENERATED VIRTUAL column and not a column Rust writes: seven statements rewrite a row's
-- `body` and `body_kind` (ingest, merge, modifiers, edits, the long-text join), and a
-- generated column follows every one of them; ADD COLUMN of a VIRTUAL column rewrites no
-- page. The expression runs on EVERY insert and update of a `messages` row, so it must never
-- fail: it is a TOTAL rule on the first bytes of the serialised body (serde's compact,
-- externally tagged form, which puts the variant's name first), no `json_extract` /
-- `json_type`, which raise on a body that is not JSON and would then fail every write. The
-- test `kourier-core/tests/quiet_notices.rs` `the_column_and_is_quiet_agree_for_every_kind`
-- pins the rule to `SystemEvent::is_quiet` for every variant (and so to serde's spelling).
--
-- NO index on it: the newest-listed-row walks (`ingest::recount`, `query::preview`) go along
-- `messages_timeline` and read each row anyway, and a handful of quiet rows at the top of a
-- chat is all they pass. NEVER write a statement that looks for `quiet = 1` across a whole
-- chat: nothing serves it.
--
-- A LATER MIGRATION THAT REBUILDS `messages` (`INSERT ... SELECT *` into a new table) must
-- declare this column again and leave it out of the copy; a generated column cannot be
-- written.
ALTER TABLE messages ADD COLUMN quiet INTEGER GENERATED ALWAYS AS (
  CASE WHEN body_kind = 'System' THEN coalesce(
      body = '{"System":"MessageRequestAccepted"}'
      OR substr(body, 1, 32) = '{"System":{"IdentityKeyChanged":'
      OR substr(body, 1, 30) = '{"System":{"SessionRefreshed":'
      OR substr(body, 1, 27) = '{"System":{"NumberChanged":', 0)
  ELSE 0 END) VIRTUAL;

-- No settings row exists yet, so the default applies (quiet notices hidden): every
-- conversation gets the sort time of its newest LISTED row. This is `ingest::recount`'s
-- third subquery, once, for all conversations; what has no such row stays NULL.
UPDATE conversations SET last_activity = (SELECT max(m.sort_key) FROM messages m
  WHERE m.conversation_id = conversations.id
    AND (m.deleted IS NULL OR m.deleted <> 'ForMe') AND m.expiry_hidden = 0
    AND m.quiet = 0);

-- The chats that exist now and hold NOTHING but quiet notices (no sort time after the
-- UPDATE above, at least one visible row: so every visible row is quiet) are garbage chats
-- that quiet notices made before the ingest guard (a stranger's refreshed session opened an
-- empty chat): they are HIDDEN once, in the state of a chat deleted on this device
-- (`conversations.hidden`). NO row is deleted and nothing is queued for any network; a loud
-- message lists such a chat again as it lists any deleted chat, a quiet notice never does
-- (the ingest guard). Not hidden: a pinned chat, a chat with a draft, a message request, a
-- chat with unread messages or an unread mark, and a chat without a visible row (an empty
-- chat is listed as before). The list itself never leaves a chat out for what it holds, so a
-- chat that is listed today cannot vanish when a notice lands in it later (decision F1).
UPDATE conversations SET hidden = 1
WHERE hidden = 0 AND last_activity IS NULL
  AND pinned_order IS NULL AND draft IS NULL AND request_pending = 0
  AND unread = 0 AND marked_unread = 0
  AND EXISTS (SELECT 1 FROM messages x WHERE x.conversation_id = conversations.id
    AND (x.deleted IS NULL OR x.deleted <> 'ForMe') AND x.expiry_hidden = 0);

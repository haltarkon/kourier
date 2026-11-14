-- Settings batch: a chat that is left with a draft moves up in the list (Signal Desktop's rule,
-- `maybeUpdateDraftPreview`: the draft time is stamped when the chat is CLOSED with a changed
-- draft, cleared when the draft is gone).
--
-- `draft_at` is in the unit of `messages.sort_key` and `conversations.last_activity`
-- (arrival milliseconds * 1024), so the two compare directly. NULL: no stamp (no draft, or a
-- draft stored before this step: such a chat stays where its messages put it). The list's own
-- order in core.db stays `last_activity`; a front-end that offers "Move chats with a draft to
-- the top" orders by `ConversationRow::draft_at` when it is newer (a pref, not a core setting).
ALTER TABLE conversations ADD COLUMN draft_at INTEGER;

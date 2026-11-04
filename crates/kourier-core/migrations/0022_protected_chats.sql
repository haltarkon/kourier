-- Protected chats, step S-P (2026-10-29; the number 0022 is final, the next step takes 0023).
-- `protected` is 1 when the user protects the chat on another device
-- (`ConversationPatch::protected`): its messages show nowhere outside the
-- open chat (no preview in the list, no text in a notification, not found by a search). 0 = not
-- protected, also every chat stored before this step (a network that has no such state never sets
-- it). No index, no table rebuilt, no row converted.
ALTER TABLE conversations ADD COLUMN protected INTEGER NOT NULL DEFAULT 0
  CHECK (protected IN (0, 1));

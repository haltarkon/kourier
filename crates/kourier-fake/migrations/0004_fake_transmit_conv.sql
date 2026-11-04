-- The conversation of an accepted copy (K-B1 B6). A network whose message ids are unique per
-- conversation (Threema-like) lets two chats use one `local`, and a person can be a member of
-- both: the copy for that person in the second chat is a new message, not a resend of the
-- first. The server log and the send progress are keyed by `(local, recipient, conv)` from now
-- on. A row an earlier build wrote has no conversation ('') and counts as the copy of any chat,
-- as before.
ALTER TABLE fake_transmits ADD COLUMN conv TEXT NOT NULL DEFAULT '';

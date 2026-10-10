-- Call lines are found by (conversation, id) on every network (review of 2026-10-09).
--
-- The id of a call is chosen by the other side. Under the account-wide key of a Signal-like
-- network (`net_scope` 0, index `messages_key_account`) the first chat that named a call id
-- owned it: the real call in another chat got no line, and a cleared line blocked its id.
-- A call line is therefore stored under the per-conversation key (`net_scope` 1, index
-- `messages_key_conv`) on every network; `kourier-core/src/ingest/call.rs` writes and finds it
-- so. This moves the lines (and what is left of deleted ones) that schema 10 stored, and the
-- memory of removed ones. Rows with one account-wide id were unique per conversation too.
UPDATE messages SET net_scope = 1
 WHERE net_scope = 0 AND net_author = ''
   AND (net_local LIKE 'call:%' OR net_local LIKE 'gcall:%')
   AND (body_kind = 'System' OR deleted = 'ForMe');
UPDATE expired_messages SET net_scope = 1
 WHERE net_scope = 0 AND net_author = ''
   AND (net_local LIKE 'call:%' OR net_local LIKE 'gcall:%');

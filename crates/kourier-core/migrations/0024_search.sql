-- G1b: message search (architecture §5.x "Message search").
--
-- The index itself is NOT in core.db: it lives in its own SQLCipher file, `search.db`, beside
-- this one, with its own writer (`search::file`), so saving a message never waits for search.
-- core.db keeps only what that file is built from: an append-only LOG of the rows whose
-- searchable state may have changed, and an EPOCH that names this core.db for the file.
--
-- The log: one row per change, filled by the triggers below in the same transaction as the
-- row's change, so no writer of `messages` (19 statements and the FK cascades of an account's
-- or a chat's removal, which have no statement of their own) has to know that search exists.
-- AUTOINCREMENT, so a seq is NEVER used twice, even after the log was emptied: search.db
-- remembers the last seq it applied, and a reused seq at or below it would be skipped for
-- good. The search task deletes what it applied (`search::log::prune`, a Low job).
CREATE TABLE search_queue(seq INTEGER PRIMARY KEY AUTOINCREMENT, message_id INTEGER NOT NULL);

CREATE TRIGGER messages_search_insert AFTER INSERT ON messages BEGIN
  INSERT INTO search_queue(message_id) VALUES (NEW.id);
END;
-- Only a change of what the index holds: the text, the kind, the delete, the view-once flag,
-- the chat (a merge moves rows) and the sort key (the rowid). The WHEN keeps a rewrite of the
-- same body (an import's restore, the long text's join with the same text) out of the log.
CREATE TRIGGER messages_search_update
  AFTER UPDATE OF body, body_kind, deleted, view_once, conversation_id, sort_key ON messages
  WHEN OLD.body IS NOT NEW.body OR OLD.body_kind IS NOT NEW.body_kind
    OR OLD.deleted IS NOT NEW.deleted OR OLD.view_once IS NOT NEW.view_once
    OR OLD.conversation_id IS NOT NEW.conversation_id OR OLD.sort_key IS NOT NEW.sort_key
BEGIN
  INSERT INTO search_queue(message_id) VALUES (OLD.id);
END;
CREATE TRIGGER messages_search_delete AFTER DELETE ON messages BEGIN
  INSERT INTO search_queue(message_id) VALUES (OLD.id);
END;

-- Which core.db this is, for search.db: a file made for another core.db (a restore, a rebuilt
-- or replaced one) is built again. Written once here, and again by anything that rewrites
-- message ids or replaces the rows wholesale (none in this build).
INSERT OR REPLACE INTO settings(scope, key, value)
  VALUES ('search', 'epoch', lower(hex(randomblob(16))));

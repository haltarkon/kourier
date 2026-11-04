-- People pages, step P4 (2026-10-23): the gallery of a chat ("Media and Files", `Query::ChatMedia`).
--
-- The gallery lists the attachments of a chat's media messages of ONE kind, newest message first:
-- the pictures, videos and GIFs ("Media") or the files and audio files ("Files"). A chat is mostly
-- one of them (20,000 pictures and one file, a chat of documents), so an index over "the media
-- messages" alone walks every picture to find the file (19 ms for 20,000 pictures, measured). The
-- kind is therefore a property of the MESSAGE the index can hold: two flags, kept by triggers on
-- `attachments` (every writer of an attachment, the core's ingest, the import, a merge, a
-- deletion, a sniffed kind, is covered whatever its place in the code; the triggers fire on the
-- row's own statement, so the flags are right inside the transaction that wrote the attachment):
--   media_visual = the message has an attachment of kind Image, Video or Gif,
--   media_files  = the message has an attachment of kind File or Audio,
-- the rest of a long text (`long_text`) and a voice note being neither. A message can have both.
-- The flags say nothing of the attachment's state or of the message being deleted, view-once or
-- hidden by an unconfirmed clock: the statement of the gallery states those (`query::GALLERY_SHOWN`).
--
-- Two PARTIAL indexes, newest first, the order of `messages_timeline`, over the media rows of
-- each kind. The gallery's statements say `body_kind = 'Media' AND media_<kind> = 1` and name the
-- index (`INDEXED BY`), so they never walk a chat's texts or the other kind's messages; no other
-- statement can pick an index, its WHERE would have to imply the index's, and none does
-- (`core.db` has no planner statistics: an index that ties with an older key for a hot
-- statement would be taken, `.claude/rules/core.md`). A sticker is a `Sticker` row: not in them.
ALTER TABLE messages ADD COLUMN media_visual INTEGER NOT NULL DEFAULT 0;
ALTER TABLE messages ADD COLUMN media_files INTEGER NOT NULL DEFAULT 0;

-- The rows that exist: every message that has an attachment.
UPDATE messages SET
  media_visual = EXISTS (SELECT 1 FROM attachments a WHERE a.message_id = messages.id
                           AND a.long_text = 0 AND a.kind IN ('Image', 'Video', 'Gif')),
  media_files = EXISTS (SELECT 1 FROM attachments a WHERE a.message_id = messages.id
                          AND a.long_text = 0 AND a.kind IN ('File', 'Audio'))
WHERE id IN (SELECT message_id FROM attachments);

CREATE INDEX messages_media_visual ON messages(conversation_id, sort_key DESC, id DESC)
  WHERE body_kind = 'Media' AND media_visual = 1;
CREATE INDEX messages_media_files ON messages(conversation_id, sort_key DESC, id DESC)
  WHERE body_kind = 'Media' AND media_files = 1;

-- The flags follow the attachments (the unique key `(message_id, idx)` makes each look-up one seek).
CREATE TRIGGER attachments_gallery_insert AFTER INSERT ON attachments BEGIN
  UPDATE messages SET
    media_visual = EXISTS (SELECT 1 FROM attachments a WHERE a.message_id = NEW.message_id
                             AND a.long_text = 0 AND a.kind IN ('Image', 'Video', 'Gif')),
    media_files = EXISTS (SELECT 1 FROM attachments a WHERE a.message_id = NEW.message_id
                            AND a.long_text = 0 AND a.kind IN ('File', 'Audio'))
  WHERE id = NEW.message_id;
END;

CREATE TRIGGER attachments_gallery_delete AFTER DELETE ON attachments BEGIN
  UPDATE messages SET
    media_visual = EXISTS (SELECT 1 FROM attachments a WHERE a.message_id = OLD.message_id
                             AND a.long_text = 0 AND a.kind IN ('Image', 'Video', 'Gif')),
    media_files = EXISTS (SELECT 1 FROM attachments a WHERE a.message_id = OLD.message_id
                            AND a.long_text = 0 AND a.kind IN ('File', 'Audio'))
  WHERE id = OLD.message_id;
END;

-- A kind or a `long_text` that changes, and an attachment that moves to another message (a merge
-- of two conversations): both messages are looked at.
CREATE TRIGGER attachments_gallery_update AFTER UPDATE OF kind, long_text, message_id ON attachments BEGIN
  UPDATE messages SET
    media_visual = EXISTS (SELECT 1 FROM attachments a WHERE a.message_id = OLD.message_id
                             AND a.long_text = 0 AND a.kind IN ('Image', 'Video', 'Gif')),
    media_files = EXISTS (SELECT 1 FROM attachments a WHERE a.message_id = OLD.message_id
                            AND a.long_text = 0 AND a.kind IN ('File', 'Audio'))
  WHERE id = OLD.message_id;
  UPDATE messages SET
    media_visual = EXISTS (SELECT 1 FROM attachments a WHERE a.message_id = NEW.message_id
                             AND a.long_text = 0 AND a.kind IN ('Image', 'Video', 'Gif')),
    media_files = EXISTS (SELECT 1 FROM attachments a WHERE a.message_id = NEW.message_id
                            AND a.long_text = 0 AND a.kind IN ('File', 'Audio'))
  WHERE id = NEW.message_id;
END;

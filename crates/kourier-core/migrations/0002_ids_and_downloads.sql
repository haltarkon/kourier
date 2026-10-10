-- Ids that travel to a backend are never reused (architecture §13 phase D "Open for the next
-- phase"). With a plain INTEGER PRIMARY KEY SQLite hands out max(id) + 1, so once the newest row
-- was deleted its id came back: a CmdId the backend still remembered, or an AttachmentId a late
-- AttachmentDone still named. Both tables are rebuilt with AUTOINCREMENT, keeping every row.

CREATE TABLE pending_commands_v2(
  id INTEGER PRIMARY KEY AUTOINCREMENT,     -- = CmdId
  account_id INTEGER NOT NULL REFERENCES accounts ON DELETE CASCADE,
  command TEXT NOT NULL,                    -- JSON DurableCommand
  created_at INTEGER NOT NULL);
INSERT INTO pending_commands_v2(id, account_id, command, created_at)
  SELECT id, account_id, command, created_at FROM pending_commands ORDER BY id;
DROP TABLE pending_commands;
ALTER TABLE pending_commands_v2 RENAME TO pending_commands;

CREATE TABLE attachments_v2(
  id INTEGER PRIMARY KEY AUTOINCREMENT,     -- = AttachmentId
  message_id INTEGER NOT NULL REFERENCES messages ON DELETE CASCADE,
  idx INTEGER NOT NULL, kind TEXT NOT NULL, mime TEXT, size INTEGER, file_name TEXT,
  width INTEGER, height INTEGER, duration_ms INTEGER, blurhash TEXT, waveform BLOB, caption TEXT,
  state TEXT NOT NULL,                      -- JSON AttachmentState
  remote BLOB,
  file_id INTEGER REFERENCES files ON DELETE SET NULL,
  thumb_file_id INTEGER REFERENCES files ON DELETE SET NULL,
  -- when the thumbnail, blurhash and dimensions were derived from the local file (or it was
  -- decided there is nothing to derive); NULL while that is still to do (§5.8)
  derived_at INTEGER,
  UNIQUE(message_id, idx));
INSERT INTO attachments_v2(id, message_id, idx, kind, mime, size, file_name, width, height,
    duration_ms, blurhash, waveform, caption, state, remote, file_id, thumb_file_id)
  SELECT id, message_id, idx, kind, mime, size, file_name, width, height, duration_ms, blurhash,
    waveform, caption, state, remote, file_id, thumb_file_id FROM attachments ORDER BY id;
DROP TABLE attachments;
ALTER TABLE attachments_v2 RENAME TO attachments;
CREATE INDEX attachments_to_derive ON attachments(file_id) WHERE derived_at IS NULL;

-- Downloads issued to a backend and not answered yet (§4.2 DownloadAttachment, §5.8): the fresh
-- BlobTarget the core chose. Resent at every actor start; deleted with the AttachmentDone that
-- answers it (the target becomes a `files` row) or with its attachment (the target is unlinked).
CREATE TABLE attachment_downloads(
  attachment_id INTEGER PRIMARY KEY REFERENCES attachments ON DELETE CASCADE,
  account_id INTEGER NOT NULL REFERENCES accounts ON DELETE CASCADE,
  path TEXT NOT NULL,                       -- relative to accounts/<uuid>/blobs
  key BLOB NOT NULL,
  requested_at INTEGER NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0);    -- actor starts that resent it
CREATE INDEX attachment_downloads_by_account ON attachment_downloads(account_id);
CREATE INDEX files_by_account ON files(account_id);

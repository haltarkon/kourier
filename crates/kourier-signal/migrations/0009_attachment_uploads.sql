-- Signal backend.db, step S12 (docs/architecture.md §5.5, §6.7 step 2): pointers of recent
-- attachment uploads, reused for three days so a retried or repeated send does not upload the
-- same file again (`SD/ts/util/uploadAttachment.preload.ts`
-- MAX_DURATION_TO_REUSE_ATTACHMENT_CDN_POINTER). Purged hourly with the Message Send Log.
CREATE TABLE attachment_uploads(
  source BLOB PRIMARY KEY,                -- SHA-256 of what was uploaded and how (upload::source_key)
  pointer BLOB NOT NULL,                  -- encoded AttachmentPointer, upload fields only (secret: holds the key)
  uploaded_at INTEGER NOT NULL            -- ms
) STRICT, WITHOUT ROWID;

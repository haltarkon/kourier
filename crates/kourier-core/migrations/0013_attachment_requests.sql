-- Attachment backfill, step S6 (2026-10-11): asking the account's other device (Signal: the
-- primary phone) to upload again the files of a message the network no longer has.
--
-- One row per message whose files were asked for and whose answer is awaited
-- (`ingest::restore`). The attachments the user asked for are those in state `Asking`; this
-- row is the wait and its durable deadline (on this computer's clock, like
-- `messages.call_deadline`): the deadline is set at the click to the longest the request may
-- take to go out (the backend's request lifetime and a margin); `sent_at` stays NULL until the
-- backend reports that the request went out (`AttachmentRestore{Sent}`), and then the deadline
-- is 30 s after it and 2 min after each PENDING answer. A deadline that passed while Kourier
-- was closed ends the wait at the first look after a start ("did not answer"), so nothing
-- says "Asking your phone..." for ever, whatever the backend does.
CREATE TABLE attachment_requests(
  message_id INTEGER PRIMARY KEY REFERENCES messages ON DELETE CASCADE,
  account_id INTEGER NOT NULL REFERENCES accounts ON DELETE CASCADE,
  created_at INTEGER NOT NULL,
  sent_at INTEGER,
  deadline INTEGER,
  -- The `pending_commands` row of the request (`DurableCommand::RequestAttachments`). The
  -- backend's `Sent` and `NotSent` name it and apply only to the request that has it, so a
  -- report about an earlier request never touches a newer wait of the same message; ending
  -- the wait deletes the command too, so a request nobody waits for any more is not sent.
  cmd INTEGER);
CREATE INDEX attachment_requests_deadline ON attachment_requests(deadline)
  WHERE deadline IS NOT NULL;
-- The attachments being asked for, to find those without a request at a start
-- (`restore::end_orphan_asks`) without reading the whole table.
CREATE INDEX attachments_asking ON attachments(message_id) WHERE state = '"Asking"';
-- Who started a download: 'user' (a click), 'auto' (the policy) or 'restore' (an answer of
-- the other device). Only a user download that ends "no longer available" asks the other
-- device at once; rows of an earlier build count as automatic (they ask nothing).
ALTER TABLE attachment_downloads ADD COLUMN source TEXT NOT NULL DEFAULT 'auto';
-- The pointer came from the other device's answer: a download of it that ends "no longer
-- available" does not ask again on its own (a click still does).
ALTER TABLE attachments ADD COLUMN restored INTEGER NOT NULL DEFAULT 0;

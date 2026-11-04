-- WhatsApp backend.db, sixth schema (W5b, chunk B9; decision G4): the upload record of a file Kourier sent.
-- FROZEN once it ran on a kept profile: a change is a new number, never an edit of this file.
--
-- THE UPLOAD RECORD: what WhatsApp's media server answered for the file of one own message, with the media key
-- the file was encrypted under, written in ONE zone (kill points `wa_inside_media_upload` /
-- `wa_after_media_upload`) after the upload returned and BEFORE the message frame is built. A resend of the same
-- local within 6 hours of the upload sends the SAME file again from it (same key, same path, same hashes) with no
-- second upload; later it uploads again (whether the server keeps an upload no message names longer is unknown).
-- The caption edit of an own file reads its pointer here. Rows go at a start after 30 days (the media key and the
-- pointer of an own file are what a later re-upload request needs; WhatsApp's media live about that long).
-- The media key is a secret inside this SQLCipher file like every other key of it; nothing of a row is logged.
--   local    the core's id of the message (`Transmit.local`), which is also its WhatsApp id
--   conv     the encoded conversation key of the request
--   pointer  the JSON of the file's pointer (`media::MediaPointer`): type, direct path, media key, hashes,
--            length, the server's time of the key, and the facts the message is built from
--   at       when the upload was answered (wall clock, ms)
CREATE TABLE media_uploads(
  local TEXT PRIMARY KEY,
  conv TEXT NOT NULL,
  pointer BLOB NOT NULL,
  at INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
CREATE INDEX media_uploads_by_age ON media_uploads(at);

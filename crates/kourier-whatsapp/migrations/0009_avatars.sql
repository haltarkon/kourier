-- WhatsApp backend.db, ninth schema: the profile pictures Kourier looked up (`avatars`, W8). One row per chat that was
-- asked about. `picture_id` is the server's id of the picture (asked with the next lookup, so an unchanged picture costs
-- no download); `path` the file, relative to the account's blob directory. Both NULL: the chat has no picture, or its
-- owner's privacy hides it from this account. `checked_at` (ms) is when the server was last asked: the walk goes for rows
-- it has not asked about in a while, and counts the rows of the last day against its daily limit.
CREATE TABLE avatars(
  conv TEXT PRIMARY KEY,
  picture_id TEXT,
  path TEXT,
  checked_at INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
CREATE INDEX avatars_by_check ON avatars(checked_at);

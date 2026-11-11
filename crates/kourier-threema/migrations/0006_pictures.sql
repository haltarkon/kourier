-- PROFILE PICTURES (TH8): per person or group, the blob behind the picture the core was told of.
--
-- `picture::due` compares the synced `contact` and `grp` rows with this table: a row whose `blob_id` differs from the one
-- the sync names is fetched again. A row with a `blob_id` and no `path` is a picture the servers no longer had, or that
-- was no JPEG: it is not asked for again until the sync names another.
CREATE TABLE picture (
  -- The conversation (`ConvKey::encode`).
  conv    TEXT    PRIMARY KEY,
  blob_id BLOB    CHECK (blob_id IS NULL OR length(blob_id) = 16),
  -- The sealed file, relative to the account's blob directory.
  path    TEXT,
  at      INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

-- What the chats were last told they can do (`store::state_features_again`). Empty: an older build stated them.
CREATE TABLE features_stated (
  id      INTEGER PRIMARY KEY CHECK (id = 1),
  version INTEGER NOT NULL
) STRICT;

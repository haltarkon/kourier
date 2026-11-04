-- The Threema backend.db, schema 2 (component `threema`, TH4): what a device join imports.
-- FROZEN once committed: a change is a new number. STRICT tables. Each synced object is kept WHOLE,
-- as the protobuf bytes of its `d2d_sync.*` message; later steps apply their deltas to these bytes.
-- Only the keys the later steps look up are columns; nothing else repeats a field of a BLOB. Sizes
-- and counts are bounded in code (`src/join/essential.rs`), not here. No table is WITHOUT ROWID: a
-- row with a BLOB of any size (a picture, a group with its members) is not what that layout is for
-- (sqlite.org/withoutrowid.html, S10); the primary keys are unique indexes over the rowid tables, and
-- NOT NULL is written out on the text and blob keys (a CHECK passes NULL).

-- The contacts (`d2d_sync.Contact`), by Threema ID (8 characters, as `identity.threema_id`).
CREATE TABLE contact (
  identity       TEXT    NOT NULL PRIMARY KEY CHECK (length(identity) = 8),
  sync           BLOB    NOT NULL,
  -- When the conversation with the contact was last updated (ms); NULL: no conversation.
  last_update_at INTEGER
) STRICT;

-- The groups (`d2d_sync.Group`, its members inside), by `common.GroupIdentity`: the creator's
-- Threema ID and the group id, a `fixed64` stored as the i64 of the same 64 bits.
CREATE TABLE grp (
  creator        TEXT    NOT NULL CHECK (length(creator) = 8),
  group_id       INTEGER NOT NULL,
  sync           BLOB    NOT NULL,
  -- When the group's conversation was last updated (ms).
  last_update_at INTEGER NOT NULL,
  PRIMARY KEY (creator, group_id)
) STRICT;

-- The distribution lists (`d2d_sync.DistributionList`), by `distribution_list_id`, a `fixed64`
-- stored as the i64 of the same 64 bits.
CREATE TABLE dist_list (
  id             INTEGER PRIMARY KEY,
  sync           BLOB    NOT NULL,
  -- When the list's conversation was last updated (ms).
  last_update_at INTEGER NOT NULL
) STRICT;

-- The user's own profile (`d2d_sync.UserProfile`): one row.
CREATE TABLE own_profile (
  id   INTEGER PRIMARY KEY CHECK (id = 1),
  sync BLOB    NOT NULL
) STRICT;

-- The shared settings (`d2d_sync.Settings`): one row.
CREATE TABLE settings (
  id   INTEGER PRIMARY KEY CHECK (id = 1),
  sync BLOB    NOT NULL
) STRICT;

-- The blobs of the join (`common.BlobData`) that the imported objects reference, as received (still
-- under each blob's own key), by Blob ID (16 bytes: libthreema's `BlobId::LENGTH`, Desktop's
-- `BLOB_ID_LENGTH`); a later step turns them into pictures and deletes the row.
CREATE TABLE held_blob (
  id          BLOB    NOT NULL PRIMARY KEY CHECK (length(id) = 16),
  data        BLOB    NOT NULL,
  -- When the join received it (ms).
  received_at INTEGER NOT NULL
) STRICT;

-- ZONE 2's MARKER (S4): one row, written by the join's zone 2 (`store::ready_join`) in the same commit
-- as the first `core_outbox` row. A file with an identity and NO marker is a join that stopped between
-- its two zones (or a file whose core.db was lost then): a `Rebuild` or a `Resume` must not state such a
-- file to the core as a linked account. `at` is when zone 2 committed (ms).
CREATE TABLE join_ready (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  at INTEGER NOT NULL
) STRICT;

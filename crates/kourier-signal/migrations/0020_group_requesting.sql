-- Signal backend.db, step S4b of the history import (docs/architecture.md §14.2 item 14):
-- `groups.membership` learns 'requesting' (we asked to join and wait for an admin: the phone
-- exports such a group with us only in `membersPendingAdminApproval`). SQLite cannot change a
-- CHECK in place, so the table is made anew with the same columns and rows.
CREATE TABLE groups_new(
  master_key BLOB PRIMARY KEY,
  group_id BLOB NOT NULL UNIQUE,
  blocked INTEGER NOT NULL DEFAULT 0 CHECK (blocked IN (0, 1)),
  whitelisted INTEGER NOT NULL DEFAULT 0 CHECK (whitelisted IN (0, 1)),
  archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
  marked_unread INTEGER NOT NULL DEFAULT 0 CHECK (marked_unread IN (0, 1)),
  muted_until INTEGER,
  updated_at INTEGER NOT NULL,
  revision INTEGER,
  state BLOB,
  -- 'member', 'pending' (invited), 'requesting' (asked to join), 'left', 'gone' (404).
  membership TEXT NOT NULL DEFAULT 'unknown'
    CHECK (membership IN ('unknown', 'member', 'pending', 'requesting', 'left', 'gone')),
  endorsements BLOB,
  endorsements_expire_at INTEGER,
  fetched_at INTEGER,
  dirty INTEGER NOT NULL DEFAULT 0 CHECK (dirty IN (0, 1)),
  avatar_key TEXT,
  avatar_blob TEXT,
  features_hash INTEGER
) STRICT;
INSERT INTO groups_new(master_key, group_id, blocked, whitelisted, archived, marked_unread,
    muted_until, updated_at, revision, state, membership, endorsements,
    endorsements_expire_at, fetched_at, dirty, avatar_key, avatar_blob, features_hash)
  SELECT master_key, group_id, blocked, whitelisted, archived, marked_unread, muted_until,
    updated_at, revision, state, membership, endorsements, endorsements_expire_at,
    fetched_at, dirty, avatar_key, avatar_blob, features_hash
  FROM groups;
DROP TABLE groups;
ALTER TABLE groups_new RENAME TO groups;

-- Dead since step S3a (no unread count is delivered): never read or written.
ALTER TABLE history_transfer DROP COLUMN deliver_hinted;

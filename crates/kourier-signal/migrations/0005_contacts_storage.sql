-- Signal backend.db, step S9 (docs/architecture.md §5.5, §6.9-§6.11): contact sync, the Storage
-- Service (read only in M1), profiles and avatars.

-- What contact sync, the Storage Service and profile fetches taught us about each peer.
ALTER TABLE peers ADD COLUMN blocked INTEGER NOT NULL DEFAULT 0 CHECK (blocked IN (0, 1));
-- In the phone's address book (a contact-sync name or a Storage Service system name): Desktop's
-- `isInSystemContacts`, which makes a 1:1 chat accepted (§6.7).
ALTER TABLE peers ADD COLUMN system_contact INTEGER NOT NULL DEFAULT 0
  CHECK (system_contact IN (0, 1));
-- Named by the last full contact sync, which clears the names of the others: 1 named, 2 named
-- by the sync being applied (its last zone turns 1 into 0 and 2 into 1).
ALTER TABLE peers ADD COLUMN in_contact_sync INTEGER NOT NULL DEFAULT 0
  CHECK (in_contact_sync IN (0, 1, 2));
-- Last successful profile fetch (ms); NULL until the first one.
ALTER TABLE peers ADD COLUMN profile_fetched_at INTEGER;
-- CDN0 path of the profile avatar whose file the core has (not log-safe).
ALTER TABLE peers ADD COLUMN profile_avatar TEXT;
-- The avatar file the core has, relative to the account's blobs/ directory, and where it came
-- from; replaced files are deleted after the zone that replaced them committed.
ALTER TABLE peers ADD COLUMN avatar_blob TEXT;
ALTER TABLE peers ADD COLUMN avatar_source TEXT CHECK (avatar_source IN ('profile', 'contact_sync'));
-- SHA-256 of the contact-sync avatar last stored (an unchanged avatar is not rewritten).
ALTER TABLE peers ADD COLUMN avatar_hash BLOB;
CREATE INDEX peers_profile_refresh ON peers(profile_fetched_at) WHERE profile_key IS NOT NULL;

-- The ACIs of the last `SyncMessage.Blocked` (a full list): a later list unblocks only the ones
-- it dropped, never contacts the Storage Service blocks.
CREATE TABLE blocked_sync(
  service_id TEXT PRIMARY KEY
) STRICT, WITHOUT ROWID;

-- Raw decrypted Storage Service records by storage id (kept for the M2 write path, which must
-- re-encode them with their unknown fields). A sync diffs the manifest against these keys.
CREATE TABLE storage_records(
  key BLOB PRIMARY KEY,                   -- raw storage id
  kind INTEGER NOT NULL,                  -- ManifestRecord.Identifier.Type
  peer TEXT,                              -- contact records: service id string (ACI, else PNI)
  record BLOB NOT NULL,                   -- the decrypted StorageRecord
  updated_at INTEGER NOT NULL
) STRICT;
CREATE INDEX storage_records_peer ON storage_records(peer) WHERE peer IS NOT NULL;

-- Groups v2 by master key (S9: from Storage Service `GroupV2Record`s; S10 adds their state).
CREATE TABLE groups(
  master_key BLOB PRIMARY KEY,
  group_id BLOB NOT NULL UNIQUE,          -- GroupSecretParams.get_group_identifier()
  blocked INTEGER NOT NULL DEFAULT 0 CHECK (blocked IN (0, 1)),
  whitelisted INTEGER NOT NULL DEFAULT 0 CHECK (whitelisted IN (0, 1)),
  archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
  marked_unread INTEGER NOT NULL DEFAULT 0 CHECK (marked_unread IN (0, 1)),
  muted_until INTEGER,
  updated_at INTEGER NOT NULL
) STRICT;

-- The link waits for a contact sync (§6.5 step 8, `InitialSync::ContactsAndStorage`): set when
-- one was applied, or when the link stopped waiting for it.
ALTER TABLE account ADD COLUMN contact_sync_done INTEGER NOT NULL DEFAULT 0
  CHECK (contact_sync_done IN (0, 1));
-- Last Storage Service sync that finished (ms), including "no manifest" (404).
ALTER TABLE account ADD COLUMN storage_synced_at INTEGER;

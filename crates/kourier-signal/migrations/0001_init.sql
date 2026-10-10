-- Signal backend.db, first schema (docs/architecture.md §5.5, step S2). `core_outbox` comes from
-- kourier-db's own migration component. Our identities are named by kind: 'aci' or 'pni'.
-- Every table is STRICT; secrets are BLOBs inside the SQLCipher file only.

-- The linked account: exactly one row once linking committed (§6.5 step 6).
CREATE TABLE account(
  id INTEGER PRIMARY KEY CHECK (id = 1),
  aci TEXT NOT NULL,                      -- lowercase hyphenated uuid
  pni TEXT NOT NULL,                      -- lowercase hyphenated uuid (untagged)
  e164 TEXT NOT NULL,
  device_id INTEGER NOT NULL CHECK (device_id BETWEEN 1 AND 127),
  password TEXT NOT NULL,
  aci_registration_id INTEGER NOT NULL,
  pni_registration_id INTEGER NOT NULL,
  profile_key BLOB NOT NULL,
  master_key BLOB NOT NULL,
  account_entropy_pool TEXT,
  storage_key BLOB NOT NULL,
  media_root_backup_key BLOB,
  read_receipts INTEGER NOT NULL DEFAULT 0,
  manifest_version INTEGER,
  post_link_sync TEXT NOT NULL DEFAULT 'pending' CHECK (post_link_sync IN ('pending', 'done')),
  arrival_counter INTEGER NOT NULL DEFAULT 0,  -- last stage-1 arrival number (§3.9)
  catch_up_boundary INTEGER,                   -- arrival counter at the last QueueEmpty (§4.5)
  linked_at INTEGER NOT NULL
) STRICT;

-- Our ACI and PNI identity key pairs, as provisioned (never generated here), and the next key
-- ids per identity (Desktop: random 24-bit start, wrapping at 2^24).
CREATE TABLE own_identity(
  kind TEXT PRIMARY KEY CHECK (kind IN ('aci', 'pni')),
  public_key BLOB NOT NULL,
  private_key BLOB NOT NULL,
  next_prekey_id INTEGER NOT NULL,
  next_signed_prekey_id INTEGER NOT NULL,
  next_kyber_prekey_id INTEGER NOT NULL
) STRICT;

-- Remote identity keys by service id string (`<uuid>` or `PNI:<uuid>`); ours are verified.
CREATE TABLE identities(
  service_id TEXT PRIMARY KEY,
  identity_key BLOB NOT NULL,
  verified INTEGER NOT NULL DEFAULT 0 CHECK (verified IN (0, 1, 2)),  -- default, verified, unverified
  first_use INTEGER NOT NULL DEFAULT 1,
  updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE sessions(
  our_kind TEXT NOT NULL CHECK (our_kind IN ('aci', 'pni')),
  their_service_id TEXT NOT NULL,
  device_id INTEGER NOT NULL,
  record BLOB NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (our_kind, their_service_id, device_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE prekeys(
  identity TEXT NOT NULL CHECK (identity IN ('aci', 'pni')),
  key_id INTEGER NOT NULL,
  record BLOB NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (identity, key_id)
) STRICT;

CREATE TABLE signed_prekeys(
  identity TEXT NOT NULL CHECK (identity IN ('aci', 'pni')),
  key_id INTEGER NOT NULL,
  record BLOB NOT NULL,
  created_at INTEGER NOT NULL,
  confirmed INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (identity, key_id)
) STRICT;

CREATE TABLE kyber_prekeys(
  identity TEXT NOT NULL CHECK (identity IN ('aci', 'pni')),
  key_id INTEGER NOT NULL,
  record BLOB NOT NULL,
  last_resort INTEGER NOT NULL CHECK (last_resort IN (0, 1)),
  created_at INTEGER NOT NULL,
  confirmed INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (identity, key_id)
) STRICT;

-- (Kyber last-resort key, signed EC prekey, base key) triples already used: a repeat is a
-- replayed PreKeySignalMessage and is rejected (LS/rust/protocol/src/storage/traits.rs:129-139).
CREATE TABLE kyber_triples(
  identity TEXT NOT NULL CHECK (identity IN ('aci', 'pni')),
  kyber_id INTEGER NOT NULL,
  signed_id INTEGER NOT NULL,
  base_key BLOB NOT NULL,
  used_at INTEGER NOT NULL,
  PRIMARY KEY (identity, kyber_id, signed_id, base_key)
) STRICT, WITHOUT ROWID;

-- Decrypted envelopes between stage 1 and stage 2 (§5.6). Deleted in the stage-2 transaction
-- that writes their core events into core_outbox.
CREATE TABLE inbox(
  arrival INTEGER PRIMARY KEY,
  received_at INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('content', 'delivery_receipt')),
  lane TEXT NOT NULL,
  our_kind TEXT NOT NULL CHECK (our_kind IN ('aci', 'pni')),
  sender TEXT NOT NULL,                   -- service id string of the sender
  sender_device INTEGER NOT NULL,
  timestamp INTEGER NOT NULL,             -- envelope client timestamp (ms)
  server_timestamp INTEGER,
  server_guid TEXT,
  sealed INTEGER NOT NULL CHECK (sealed IN (0, 1)),
  urgent INTEGER NOT NULL DEFAULT 1,
  content BLOB,                           -- unpadded plaintext Content; NULL for receipts
  attempts INTEGER NOT NULL DEFAULT 0,
  parked_until INTEGER
) STRICT;
CREATE INDEX inbox_lane ON inbox(lane, arrival);

-- WhatsApp backend.db, first schema (docs/architecture.md §5.5 and §7, step W1). `core_outbox` comes
-- from kourier-db's own migration component. ONE account per file: no `device_id` column anywhere
-- (whatsapp-rust's SQLite store keeps many accounts in one file and keys every row by it). The table
-- names and shapes follow that store (storages/sqlite-storage/src/schema.rs at the pinned revision)
-- so a later reader can compare them; the SQL is written for this crate.
-- Every table is STRICT; every secret is a BLOB inside the SQLCipher file only.

-- The device (noise key, identity key, signed prekey, ADV secret, push name, ...): ONE row, the JSON
-- of `wacore::store::Device`, which is upstream's own persisted shape with additive
-- `#[serde(default)]` fields. `format` lets a later change of the encoding migrate.
CREATE TABLE device(
  id INTEGER PRIMARY KEY CHECK (id = 1),
  format INTEGER NOT NULL,
  record BLOB NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;

-- Signal protocol state (SignalStore). Addresses are `user@server` or `user:device@server`.
CREATE TABLE identities(
  address TEXT PRIMARY KEY,
  key BLOB NOT NULL CHECK (length(key) = 32)
) STRICT, WITHOUT ROWID;

CREATE TABLE sessions(
  address TEXT PRIMARY KEY,
  record BLOB NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE prekeys(
  id INTEGER PRIMARY KEY,
  record BLOB NOT NULL,
  uploaded INTEGER NOT NULL CHECK (uploaded IN (0, 1))
) STRICT;

CREATE TABLE signed_prekeys(
  id INTEGER PRIMARY KEY,
  record BLOB NOT NULL
) STRICT;

CREATE TABLE sender_keys(
  address TEXT PRIMARY KEY,
  record BLOB NOT NULL
) STRICT, WITHOUT ROWID;

-- App state (AppSyncStore).
CREATE TABLE app_state_keys(
  key_id BLOB PRIMARY KEY,
  key_data BLOB NOT NULL,
  fingerprint BLOB NOT NULL,
  timestamp INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
-- `get_latest_sync_key_id`: the key with the newest OWN time (a phone that shares an older key again
-- after a newer one does not make the old one the latest), ties by key id.
CREATE INDEX app_state_keys_latest ON app_state_keys(timestamp DESC, key_id DESC);

-- One row per collection that ever synced: the JSON of `HashState` (version, the 128-byte hash,
-- `index_value_map`, `mac_mismatch_fatal`, `bootstrapped`). No row = never synced = bootstrap.
CREATE TABLE app_state_versions(
  name TEXT PRIMARY KEY,
  state BLOB NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE app_state_mutation_macs(
  name TEXT NOT NULL,
  index_mac BLOB NOT NULL,
  version INTEGER NOT NULL,
  value_mac BLOB NOT NULL,
  PRIMARY KEY (name, index_mac)
) STRICT, WITHOUT ROWID;

-- Kourier's own rule (architecture §7.1 rule (b)): a collection whose version moved (a patch or a
-- snapshot page) and whose events did not yet reach `core_outbox` is dirty. Written in the
-- transaction of the version; cleared (`DELETE ... WHERE name = ? AND version <= ?`) in the
-- transaction that writes that collection's events into `core_outbox` (W6).
CREATE TABLE appstate_dirty(
  name TEXT PRIMARY KEY,
  version INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

-- Protocol alignment (ProtocolStore).
CREATE TABLE sender_key_devices(
  group_jid TEXT NOT NULL,
  device_jid TEXT NOT NULL,
  has_key INTEGER NOT NULL CHECK (has_key IN (0, 1)),
  PRIMARY KEY (group_jid, device_jid)
) STRICT, WITHOUT ROWID;
CREATE INDEX sender_key_devices_by_device ON sender_key_devices(device_jid);

-- One row per LID. A phone number can have SEVERAL (it gets a new LID; the old LID still answers
-- `get_lid_mapping`), so the index on `phone` is NOT unique: `get_pn_mapping` takes the newest by
-- `updated_at`, then by rowid.
CREATE TABLE lid_pn_mapping(
  lid TEXT PRIMARY KEY,
  phone TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  learning_source TEXT NOT NULL
) STRICT;
CREATE INDEX lid_pn_mapping_by_phone ON lid_pn_mapping(phone, updated_at DESC);

CREATE TABLE base_keys(
  address TEXT NOT NULL,
  message_id TEXT NOT NULL,
  base_key BLOB NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (address, message_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX base_keys_by_age ON base_keys(created_at);

CREATE TABLE device_registry(
  user TEXT PRIMARY KEY,
  record BLOB NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE group_metadata(
  group_jid TEXT PRIMARY KEY,
  blob BLOB NOT NULL
) STRICT, WITHOUT ROWID;

-- Trusted-contact tokens. `token` may be empty (a placeholder made by the sender side);
-- `sender_timestamp` NULL = none.
CREATE TABLE tc_tokens(
  jid TEXT PRIMARY KEY,
  token BLOB NOT NULL,
  token_timestamp INTEGER NOT NULL,
  sender_timestamp INTEGER
) STRICT, WITHOUT ROWID;

-- The serialized protobuf of every sent message, for retry receipts.
CREATE TABLE sent_messages(
  chat_jid TEXT NOT NULL,
  message_id TEXT NOT NULL,
  payload BLOB NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (chat_jid, message_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX sent_messages_by_age ON sent_messages(created_at);

-- The inbound durability buffer: a decrypted message waits here until the hook committed it.
-- `message` is ANY on purpose: the bounded reads must tell a row that is no BLOB (damaged, or
-- written by something else) from an absent one, as upstream's do, and a test must be able to
-- plant such a row.
CREATE TABLE pending_inbound_messages(
  chat TEXT NOT NULL,
  sender TEXT NOT NULL,
  id TEXT NOT NULL,
  message ANY NOT NULL,
  inserted_at INTEGER NOT NULL,
  PRIMARY KEY (chat, sender, id)
) STRICT, WITHOUT ROWID;
-- The group read looks at every sender of one (chat, id).
CREATE INDEX pending_inbound_by_chat_id ON pending_inbound_messages(chat, id);
CREATE INDEX pending_inbound_by_age ON pending_inbound_messages(inserted_at);

-- Per-message secrets (MsgSecretStore). `expires_at` 0 = never; `message_ts` 0 = unknown.
CREATE TABLE msg_secrets(
  chat TEXT NOT NULL,
  sender TEXT NOT NULL,
  msg_id TEXT NOT NULL,
  secret BLOB NOT NULL,
  expires_at INTEGER NOT NULL DEFAULT 0,
  message_ts INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (chat, sender, msg_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX msg_secrets_expiring ON msg_secrets(expires_at) WHERE expires_at > 0;

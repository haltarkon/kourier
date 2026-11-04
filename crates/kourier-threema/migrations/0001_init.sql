-- The Threema backend.db, schema 1 (component `threema`; docs/architecture.md §7.4).
-- FROZEN once committed: a change is a new number. STRICT tables; secrets (CK, DGK, the device
-- cookie) only as BLOBs inside this SQLCipher file, never anywhere else.

-- This device of the Threema ID: one row, written once by the join (TH4).
CREATE TABLE identity (
  id               INTEGER PRIMARY KEY CHECK (id = 1),
  threema_id       TEXT    NOT NULL CHECK (length(threema_id) = 8),
  -- CK, the client key (secret).
  client_key       BLOB    NOT NULL CHECK (length(client_key) = 32),
  -- The chat server group: one or more of [A-Za-z0-9-] (`csp_server_group`, md-d2d-join.proto:246-247 of
  -- the pinned copy; libthreema's `ChatServerGroup::try_from` accepts the same).
  server_group     TEXT    NOT NULL CHECK (length(server_group) >= 1 AND server_group NOT GLOB '*[^A-Za-z0-9-]*'),
  -- DGK, the device group key (secret).
  device_group_key BLOB    NOT NULL CHECK (length(device_group_key) = 32),
  device_cookie    BLOB    NOT NULL CHECK (length(device_cookie) = 16),
  -- D2xDeviceId(u64) and CspDeviceId(u64), each stored as the i64 of the same 64 bits.
  d2m_device_id    INTEGER NOT NULL,
  csp_device_id    INTEGER NOT NULL,
  slot_state       TEXT    NOT NULL CHECK (slot_state IN ('new', 'existing')),
  device_label     TEXT    NOT NULL,
  linked_at_ms     INTEGER NOT NULL
) STRICT;

-- The two permanent nonce stores: HMAC-SHA256 of the nonce with the identity as key (HashedNonce).
CREATE TABLE csp_nonce (
  hash BLOB PRIMARY KEY CHECK (length(hash) = 32)
) STRICT, WITHOUT ROWID;

CREATE TABLE d2d_nonce (
  hash BLOB PRIMARY KEY CHECK (length(hash) = 32)
) STRICT, WITHOUT ROWID;

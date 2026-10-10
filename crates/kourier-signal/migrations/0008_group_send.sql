-- Signal backend.db, step S11 (docs/architecture.md §5.5, §6.7 step 5): group send with sender
-- keys. Our own sender-key chain lives in `sender_keys` under our ACI and device id.

-- Per group (`SD/ts/util/sendToGroup.preload.ts` `senderKeyInfo`): the distribution id of our
-- chain and when the chain was started (reset after `desktop.senderKeyMaxAge` or a member
-- removal; the distribution id is kept, Desktop's `resetSenderKey`).
CREATE TABLE sender_key_info(
  group_id BLOB PRIMARY KEY,              -- GroupSecretParams.get_group_identifier()
  distribution_id BLOB NOT NULL,          -- 16-byte uuid
  created_at INTEGER NOT NULL             -- ms
) STRICT;

-- The member devices that have our distribution message for the current chain
-- (`senderKeyInfo.memberDevices`): compared by device and registration id before every send.
CREATE TABLE sender_key_devices(
  group_id BLOB NOT NULL REFERENCES sender_key_info(group_id) ON DELETE CASCADE,
  recipient TEXT NOT NULL,                -- service id string
  device INTEGER NOT NULL,
  registration_id INTEGER NOT NULL,
  PRIMARY KEY (group_id, recipient, device)
) STRICT, WITHOUT ROWID;

-- The group a logged payload was sent in: a resend for a retry request carries our sender-key
-- distribution message for it (`SD/ts/util/handleRetry.preload.ts`
-- `maybeAddSenderKeyDistributionMessage`).
ALTER TABLE msl_payloads ADD COLUMN group_id BLOB;

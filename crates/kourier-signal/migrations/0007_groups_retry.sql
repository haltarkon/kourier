-- Signal backend.db, step S10 (docs/architecture.md §5.5, §6.6, §6.8): groups v2 receive (state,
-- credentials, endorsements, sender keys) and 1:1 retry requests (decryption errors both ways).

-- What we know of each group: the decrypted state at `revision`, as a JSON snapshot (member ACIs
-- and roles, title, description, timer, avatar key, access rules; not log-safe), the send
-- endorsements of the last fetch, and the conversation state the core has.
ALTER TABLE groups ADD COLUMN revision INTEGER;            -- NULL until the first fetch
ALTER TABLE groups ADD COLUMN state BLOB;                   -- JSON snapshot (groups::Snapshot)
-- 'member' (we are a full member), 'pending' (invited), 'left' (removed or left), 'gone' (404).
ALTER TABLE groups ADD COLUMN membership TEXT NOT NULL DEFAULT 'unknown'
  CHECK (membership IN ('unknown', 'member', 'pending', 'left', 'gone'));
ALTER TABLE groups ADD COLUMN endorsements BLOB;            -- GroupSendEndorsementsResponse
ALTER TABLE groups ADD COLUMN endorsements_expire_at INTEGER;  -- ms; validated on receipt
ALTER TABLE groups ADD COLUMN fetched_at INTEGER;           -- last successful fetch (ms)
-- A fetch failed while messages were applied with the last known state (§6.6, §6.8); the
-- `group_refresh` job clears it.
ALTER TABLE groups ADD COLUMN dirty INTEGER NOT NULL DEFAULT 0 CHECK (dirty IN (0, 1));
-- CDN0 key of the avatar whose file the core has, and that file (relative to blobs/).
ALTER TABLE groups ADD COLUMN avatar_key TEXT;
ALTER TABLE groups ADD COLUMN avatar_blob TEXT;
-- Hash of the conversation features last sent to the core; NULL = the core lacks the group.
ALTER TABLE groups ADD COLUMN features_hash INTEGER;

-- Group auth credentials for the 7-day window (`SD/ts/services/groupCredentialFetcher`):
-- one `AuthCredentialWithPni` per redemption day.
CREATE TABLE group_credentials(
  redemption_time INTEGER PRIMARY KEY,    -- day start, seconds since the epoch
  credential BLOB NOT NULL,               -- serialized AuthCredentialWithPni (secret)
  fetched_at INTEGER NOT NULL
) STRICT;

-- Sender keys of other members (`SenderKeyStore`): (sender address, distribution id).
CREATE TABLE sender_keys(
  sender TEXT NOT NULL,                   -- service id string
  device INTEGER NOT NULL,
  distribution_id BLOB NOT NULL,          -- 16-byte uuid
  record BLOB NOT NULL,                   -- SenderKeyRecord
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (sender, device, distribution_id)
) STRICT, WITHOUT ROWID;

-- Decryption failures (§6.6 `retry.rs`): stage 1 stores a row of kind 'content' without
-- content and with the serialized DecryptionErrorMessage for the sender's device, the content
-- hint of the sealed envelope and its group id, so stage 2 queues the retry request and emits
-- the placeholder in the conversation's lane.
ALTER TABLE inbox ADD COLUMN decrypt_error BLOB;
ALTER TABLE inbox ADD COLUMN content_hint INTEGER;
ALTER TABLE inbox ADD COLUMN group_id BLOB;

-- Dedup and rate limits of retries (`SD/ts/util/handleRetry.preload.ts`): 'request' counts the
-- retry requests sent per failed message (≤ 5), 'resend' the retry requests answered per
-- requester device and message (≤ 5), 'reset' the automatic session resets per device (≤ 1 per
-- hour; timestamp 0). Purged with the Message Send Log.
CREATE TABLE retry_state(
  kind TEXT NOT NULL CHECK (kind IN ('request', 'resend', 'reset')),
  sender TEXT NOT NULL,                   -- service id string
  device INTEGER NOT NULL,
  timestamp INTEGER NOT NULL,
  attempts INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (kind, sender, device, timestamp)
) STRICT, WITHOUT ROWID;

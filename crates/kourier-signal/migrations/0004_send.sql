-- Signal backend.db, step S8 (docs/architecture.md §5.5, §6.7): the 1:1 send path. What other
-- users let us do (sealed sender, profile sharing), the Message Send Log, per-recipient send
-- progress, the two sender certificates, delivery receipts waiting for their batch, and payloads
-- of command jobs (read receipts, read syncs, sync transcripts).

-- Per contact: the access key derived from their profile key, the sealed-sender state
-- (`SD/ts/types/SealedSender.std.ts`: 0 unknown, 1 enabled, 2 disabled, 3 unrestricted), whether
-- the chat is accepted (profile sharing, Desktop's `profileSharing`), when the server said they
-- are unregistered, and the hash of the conversation features last sent to the core.
ALTER TABLE peers ADD COLUMN access_key BLOB;
ALTER TABLE peers ADD COLUMN sealed_sender INTEGER NOT NULL DEFAULT 0
  CHECK (sealed_sender IN (0, 1, 2, 3));
ALTER TABLE peers ADD COLUMN profile_sharing INTEGER NOT NULL DEFAULT 0
  CHECK (profile_sharing IN (0, 1));
ALTER TABLE peers ADD COLUMN unregistered_at INTEGER;
ALTER TABLE peers ADD COLUMN features_hash INTEGER;

-- Synced settings the send path needs (`SyncMessage.Configuration`; the phone-number sharing
-- mode comes from the Storage Service in S9: NULL means "nobody", Desktop's default).
ALTER TABLE account ADD COLUMN typing_indicators INTEGER;
ALTER TABLE account ADD COLUMN phone_number_sharing TEXT;

-- Message Send Log (`SD/ts/sql/Server.node.ts` sendLogPayloads/Recipients/MessageIds): every
-- sent Content except typing and null messages, kept 14 days (`desktop.retryRespondMaxAge`) to
-- answer retry requests; a delivery receipt removes the recipient device's row.
CREATE TABLE msl_payloads(
  id INTEGER PRIMARY KEY,
  timestamp INTEGER NOT NULL,             -- the sent timestamp (envelope and message)
  content BLOB NOT NULL,                  -- the unpadded Content (not log-safe)
  content_hint INTEGER NOT NULL,          -- libsignal ContentHint (0 default, 1 resendable, 2 implicit)
  urgent INTEGER NOT NULL CHECK (urgent IN (0, 1)),
  has_pni_signature INTEGER NOT NULL CHECK (has_pni_signature IN (0, 1)),
  created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX msl_payloads_timestamp ON msl_payloads(timestamp);
CREATE INDEX msl_payloads_created ON msl_payloads(created_at);

CREATE TABLE msl_recipients(
  payload_id INTEGER NOT NULL REFERENCES msl_payloads(id) ON DELETE CASCADE,
  recipient TEXT NOT NULL,                -- service id string
  device INTEGER NOT NULL,
  PRIMARY KEY (payload_id, recipient, device)
) STRICT, WITHOUT ROWID;
CREATE INDEX msl_recipients_by ON msl_recipients(recipient, device);

CREATE TABLE msl_message_ids(
  payload_id INTEGER NOT NULL REFERENCES msl_payloads(id) ON DELETE CASCADE,
  local TEXT NOT NULL,                    -- the core's message id (Signal: the sent timestamp)
  PRIMARY KEY (payload_id, local)
) STRICT, WITHOUT ROWID;

-- Recipients the server accepted an outgoing message for (§4.8 step 4, §6.7): a repeated
-- Transmit of the same message skips them. Our own ACI stands for the sync transcript.
CREATE TABLE send_progress(
  local TEXT NOT NULL,
  recipient TEXT NOT NULL,
  sealed INTEGER NOT NULL CHECK (sealed IN (0, 1)),
  accepted_at INTEGER NOT NULL,
  PRIMARY KEY (local, recipient)
) STRICT, WITHOUT ROWID;
CREATE INDEX send_progress_accepted ON send_progress(accepted_at);

-- The two sender certificates (`SD/ts/services/senderCertificate.preload.ts`).
CREATE TABLE sender_certs(
  kind TEXT PRIMARY KEY CHECK (kind IN ('with_e164', 'without_e164')),
  certificate BLOB NOT NULL,
  expires_at INTEGER NOT NULL,            -- ms since the epoch
  fetched_at INTEGER NOT NULL
) STRICT;

-- Delivery receipts owed for sealed messages in accepted chats, sent in batches of ≤ 100 per
-- sender after 500 ms (`SD/ts/util/deliveryReceipt.preload.ts`) by the `delivery_receipts` job.
CREATE TABLE delivery_receipts(
  peer TEXT NOT NULL,                     -- ACI service id string of the author
  timestamp INTEGER NOT NULL,             -- the message's sent timestamp
  created_at INTEGER NOT NULL,
  PRIMARY KEY (peer, timestamp)
) STRICT, WITHOUT ROWID;

-- Job payloads: what a read receipt, read sync or sync transcript job sends.
ALTER TABLE jobs ADD COLUMN payload BLOB;

-- Command jobs that finished: the core resends a command until it has committed its
-- CommandPersisted, which can be after the job ran; such a resend must not run it again. Keyed
-- like the job (`<cmd>:<payload digest>`), because the core reuses command ids once a command
-- is confirmed (`pending_commands.id` has no AUTOINCREMENT). Kept 7 days (the purge job).
CREATE TABLE done_commands(
  kind TEXT NOT NULL,
  key TEXT NOT NULL,
  done_at INTEGER NOT NULL,
  PRIMARY KEY (kind, key)
) STRICT, WITHOUT ROWID;

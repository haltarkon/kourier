-- core.db, initial schema (architecture §5.4). `schema_version` is created by the migrator.
-- Differences from the abridged §5.4 listing are noted inline; every `account_id` cascades from
-- `accounts`, so removing an account (§4.3 step 7) deletes all of its rows in one statement.

CREATE TABLE accounts(
  id INTEGER PRIMARY KEY,
  uuid TEXT NOT NULL UNIQUE,
  network TEXT NOT NULL,
  state TEXT NOT NULL,                      -- Linking | Active | LoggedOut | Removing
  state_reason TEXT,                        -- LoggedOut reason (log-safe)
  own_ids TEXT NOT NULL DEFAULT '[]',       -- JSON [UserId]
  own_device INTEGER,
  display_name TEXT,
  settings TEXT NOT NULL DEFAULT '{}',      -- JSON AccountSettings (synced)
  local_prefs TEXT NOT NULL DEFAULT '{}',   -- JSON AccountLocalPrefs (Kourier-only)
  caps TEXT,                                -- JSON AccountCaps
  sort_order INTEGER,
  created_at INTEGER NOT NULL);             -- no keys here: key.wrap (§5.2)

CREATE TABLE files(
  id INTEGER PRIMARY KEY,
  account_id INTEGER NOT NULL REFERENCES accounts ON DELETE CASCADE,
  path TEXT NOT NULL,                       -- relative to accounts/<uuid>/blobs
  key BLOB NOT NULL,
  size INTEGER, mime TEXT,
  purpose TEXT NOT NULL);                   -- attachment | thumb | avatar

CREATE TABLE contacts(
  id INTEGER PRIMARY KEY,
  account_id INTEGER NOT NULL REFERENCES accounts ON DELETE CASCADE,
  user_id TEXT NOT NULL,
  profile_name TEXT, system_name TEXT, nickname TEXT,
  avatar_file INTEGER REFERENCES files ON DELETE SET NULL,
  trust TEXT NOT NULL DEFAULT 'Unverified', fingerprint BLOB,
  blocked INTEGER NOT NULL DEFAULT 0, hidden INTEGER NOT NULL DEFAULT 0,
  unregistered_at INTEGER, identifiers TEXT NOT NULL DEFAULT '[]', meta BLOB, updated_at INTEGER,
  UNIQUE(account_id, user_id));

-- alias user id → canonical user id (PNI→ACI, E164→ACI, PN→LID, §3.3). Rows stay after a merge.
CREATE TABLE identity_alias(
  account_id INTEGER NOT NULL REFERENCES accounts ON DELETE CASCADE,
  alias TEXT NOT NULL, user_id TEXT NOT NULL,
  PRIMARY KEY(account_id, alias));

CREATE TABLE conversations(
  id INTEGER PRIMARY KEY,
  account_id INTEGER NOT NULL REFERENCES accounts ON DELETE CASCADE,
  conv_key TEXT NOT NULL, kind TEXT NOT NULL, is_self INTEGER NOT NULL DEFAULT 0,
  -- roaming (changed only by backend events)
  title TEXT, avatar_file INTEGER REFERENCES files ON DELETE SET NULL,
  muted_until INTEGER, archived INTEGER NOT NULL DEFAULT 0, pinned_order INTEGER,
  blocked INTEGER NOT NULL DEFAULT 0, request_pending INTEGER NOT NULL DEFAULT 0,
  marked_unread INTEGER NOT NULL DEFAULT 0,
  parent_id INTEGER REFERENCES conversations ON DELETE SET NULL,
  timer_secs INTEGER,
  features TEXT,                            -- JSON ConversationFeatures; NULL = not reported yet
  features_hash INTEGER,
  meta BLOB,
  -- core-local
  local_muted_until INTEGER, draft TEXT,
  unread INTEGER NOT NULL DEFAULT 0,        -- live unread rows + unread_hint
  unread_hint INTEGER NOT NULL DEFAULT 0,   -- from history imports (ConversationPatch.unread_hint)
  mentions INTEGER NOT NULL DEFAULT 0,
  read_up_to INTEGER,                       -- sort_key
  read_sent_at INTEGER,                     -- sent_at watermark of read markers from other devices
  last_activity INTEGER,                    -- sort_key of the newest visible message
  UNIQUE(account_id, conv_key));
CREATE INDEX conversations_list ON conversations(account_id, archived, pinned_order, last_activity DESC);

CREATE TABLE members(
  conversation_id INTEGER NOT NULL REFERENCES conversations ON DELETE CASCADE,
  user_id TEXT NOT NULL, role TEXT NOT NULL,
  PRIMARY KEY(conversation_id, user_id));
CREATE INDEX members_by_user ON members(user_id);

CREATE TABLE messages(
  id INTEGER PRIMARY KEY,
  account_id INTEGER NOT NULL REFERENCES accounts ON DELETE CASCADE,
  conversation_id INTEGER NOT NULL REFERENCES conversations ON DELETE CASCADE,
  net_scope INTEGER NOT NULL,               -- 0 AccountAuthor, 1 Conversation
  net_author TEXT NOT NULL,                 -- '' = own
  net_local TEXT NOT NULL,
  origin TEXT NOT NULL,                     -- Remote | OwnThisDevice | OwnOtherDevice
  device INTEGER,
  linked_message_id INTEGER REFERENCES messages ON DELETE SET NULL,
  sort_key INTEGER NOT NULL,
  sent_at INTEGER NOT NULL, server_at INTEGER, received_at INTEGER NOT NULL, arrival INTEGER,
  body_kind TEXT NOT NULL, body TEXT NOT NULL, -- JSON Body (latest revision)
  preview TEXT,                             -- plain text for the conversation list
  reply_to TEXT,                            -- JSON ReplyTo
  revision INTEGER NOT NULL DEFAULT 0, edited_at INTEGER,
  deleted TEXT, deleted_by TEXT,            -- ForEveryone | ForMe; '' = own
  out_floor TEXT,                           -- JSON OutgoingState from explicit sources (max)
  out_state TEXT,                           -- JSON OutgoingState: floor merged with send_state + receipts
  read_state TEXT,                          -- Unread | Read | Viewed (incoming only)
  expiry_kind TEXT, expiry_timer_ms INTEGER, expiry_started_at INTEGER, expires_at INTEGER,
  view_once INTEGER NOT NULL DEFAULT 0, history INTEGER NOT NULL DEFAULT 0,
  silent INTEGER NOT NULL DEFAULT 0, mentions_me INTEGER NOT NULL DEFAULT 0,
  meta BLOB, raw BLOB);
-- the message key (§3.1): account-wide for Signal, per conversation for WhatsApp and Threema
CREATE UNIQUE INDEX messages_key_account ON messages(account_id, net_author, net_local) WHERE net_scope = 0;
CREATE UNIQUE INDEX messages_key_conv ON messages(conversation_id, net_local) WHERE net_scope = 1;
CREATE INDEX messages_timeline ON messages(conversation_id, sort_key DESC, id DESC);
CREATE INDEX messages_local ON messages(account_id, net_local);
CREATE INDEX messages_unread ON messages(conversation_id)
  WHERE read_state = 'Unread' AND history = 0 AND deleted IS NULL;
CREATE INDEX messages_expiring ON messages(expires_at) WHERE expires_at IS NOT NULL;
CREATE INDEX messages_linked ON messages(linked_message_id) WHERE linked_message_id IS NOT NULL;

-- every revision's key resolves to the same message (§3.5.3); revision 0 is the original
CREATE TABLE message_revisions(
  message_id INTEGER NOT NULL REFERENCES messages ON DELETE CASCADE,
  account_id INTEGER NOT NULL,
  net_author TEXT NOT NULL, net_local TEXT NOT NULL,
  revision INTEGER NOT NULL DEFAULT 0,      -- rank by edited_at, recomputed on insert
  body TEXT,                                -- NULL once the message is deleted
  edited_at INTEGER,                        -- NULL for the original
  UNIQUE(message_id, net_local));
CREATE INDEX revisions_by_key ON message_revisions(account_id, net_local, net_author);

-- last-writer-wins per (user, emoji) by `at`; removals are kept as tombstones (removed = 1), so
-- any arrival order gives the same state; the per-user limit is applied when reading (§3.5.1)
CREATE TABLE reactions(
  message_id INTEGER NOT NULL REFERENCES messages ON DELETE CASCADE,
  user_id TEXT NOT NULL,                    -- '' = own
  emoji TEXT NOT NULL, at INTEGER NOT NULL, removed INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(message_id, user_id, emoji));

CREATE TABLE receipts(
  message_id INTEGER NOT NULL REFERENCES messages ON DELETE CASCADE,
  user_id TEXT NOT NULL, kind TEXT NOT NULL, at INTEGER NOT NULL,   -- earliest `at` wins
  PRIMARY KEY(message_id, user_id, kind));

CREATE TABLE send_state(
  message_id INTEGER NOT NULL REFERENCES messages ON DELETE CASCADE,
  user_id TEXT NOT NULL,
  state TEXT NOT NULL,                      -- JSON RecipientState: Pending < Failed < Sent
  error TEXT, at INTEGER NOT NULL,
  PRIMARY KEY(message_id, user_id));

CREATE TABLE attachments(
  id INTEGER PRIMARY KEY,
  message_id INTEGER NOT NULL REFERENCES messages ON DELETE CASCADE,
  idx INTEGER NOT NULL, kind TEXT NOT NULL, mime TEXT, size INTEGER, file_name TEXT,
  width INTEGER, height INTEGER, duration_ms INTEGER, blurhash TEXT, waveform BLOB, caption TEXT,
  state TEXT NOT NULL,                      -- JSON AttachmentState
  remote BLOB,
  file_id INTEGER REFERENCES files ON DELETE SET NULL,
  thumb_file_id INTEGER REFERENCES files ON DELETE SET NULL,
  UNIQUE(message_id, idx));

-- modifiers that arrived before their target (§4.9); `dedup` makes a replayed one a no-op
CREATE TABLE pending_modifiers(
  id INTEGER PRIMARY KEY,
  account_id INTEGER NOT NULL REFERENCES accounts ON DELETE CASCADE,
  conv_key TEXT,                            -- canonical; NULL = account-wide
  target_author TEXT,                       -- NULL unknown, '' own, else canonical user id
  target_local TEXT NOT NULL,
  kind TEXT NOT NULL,                       -- BackendEvent variant name
  payload TEXT NOT NULL,                    -- JSON BackendEvent narrowed to this target
  dedup TEXT NOT NULL,
  created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL,
  UNIQUE(account_id, dedup));
CREATE INDEX pending_by_target ON pending_modifiers(account_id, target_local);
CREATE INDEX pending_expiry ON pending_modifiers(account_id, expires_at);

CREATE TABLE outbox(
  id INTEGER PRIMARY KEY,
  message_id INTEGER NOT NULL REFERENCES messages ON DELETE CASCADE,
  conversation_id INTEGER NOT NULL REFERENCES conversations ON DELETE CASCADE,
  kind TEXT NOT NULL, request TEXT NOT NULL, -- JSON OutgoingRequest
  state TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, next_attempt_at INTEGER,
  deadline_at INTEGER NOT NULL, last_error TEXT);
CREATE INDEX outbox_by_message ON outbox(message_id);

-- durable commands until CommandPersisted (§4.8); id = CmdId
CREATE TABLE pending_commands(
  id INTEGER PRIMARY KEY,
  account_id INTEGER NOT NULL REFERENCES accounts ON DELETE CASCADE,
  command TEXT NOT NULL,                    -- JSON DurableCommand
  created_at INTEGER NOT NULL);

CREATE TABLE settings(scope TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
  PRIMARY KEY(scope, key));

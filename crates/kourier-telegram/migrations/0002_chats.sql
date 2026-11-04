-- Telegram backend.db, schema 2 (TG-3, plan step T3 + TD-3: chats and live receive, the reconcile).
-- What Kourier must know about TDLib's chats and messages to deliver each message once and to find
-- what a crash or a too-long difference lost. Message ids are TDLib's full ids (server id = id >> 20).
-- Every cursor, delivered row, gap and parked fact moves in the SAME transaction as the core_outbox
-- rows that carry the messages (src/store.rs). FROZEN once it ran on a kept run profile: a change
-- is a new number.

-- One row per TDLib chat Kourier has seen. `delivered_max_id` is THE CURSOR (the newest message id
-- durably handed to the core; it never moves down); `verified_max_id` is where continuity is known
-- (critic B1); `low_id` the oldest delivered id (KT-3's anchor); the read positions are parked for
-- KT-2; `held` names why a chat is not mapped yet (decision H5); `stated_hash` gates restating an
-- unchanged ConversationPatch; `updated_ms` is when the cursor last moved.
CREATE TABLE chat_state(
  chat_id INTEGER PRIMARY KEY,
  kind TEXT NOT NULL CHECK (kind IN ('direct', 'group', 'supergroup', 'channel', 'forum', 'unknown')),
  listed INTEGER NOT NULL DEFAULT 0 CHECK (listed IN (0, 1)),
  held TEXT CHECK (held IS NULL OR held IN ('channel', 'bot', 'public', 'direct-messages', 'restricted')),
  stated_hash BLOB,
  delivered_max_id INTEGER NOT NULL DEFAULT 0,
  verified_max_id INTEGER NOT NULL DEFAULT 0,
  low_id INTEGER NOT NULL DEFAULT 0,
  read_inbox_max_id INTEGER NOT NULL DEFAULT 0,
  read_outbox_max_id INTEGER NOT NULL DEFAULT 0,
  photo_small TEXT,
  photo_big TEXT,
  moved_to INTEGER,
  updated_ms INTEGER NOT NULL
) STRICT;

-- The hash of the last ContactPatch stated for a TDLib user (the gate).
CREATE TABLE user_state(
  user_id INTEGER PRIMARY KEY,
  stated_hash BLOB NOT NULL,
  updated_ms INTEGER NOT NULL
) STRICT;

-- What Kourier delivered recently: authors for edits, own ids, read targets. Swept by code: older
-- than 14 days AND not among a chat's newest 200 AND not an unread incoming row (critic M4).
CREATE TABLE delivered(
  chat_id INTEGER NOT NULL,
  msg_id INTEGER NOT NULL,
  author TEXT NOT NULL,
  outgoing INTEGER NOT NULL CHECK (outgoing IN (0, 1)),
  at_ms INTEGER NOT NULL,
  PRIMARY KEY (chat_id, msg_id)
) STRICT, WITHOUT ROWID;

-- A range of a chat whose messages may not all have reached the core: (above, below) exclusive of
-- `above`. `open` until the reconcile delivered it (rows and the removal in one transaction);
-- `lost` at the reconcile's cap, kept for KT-3 (no notice, decision G-5).
CREATE TABLE gap(
  chat_id INTEGER NOT NULL,
  above INTEGER NOT NULL,
  below INTEGER NOT NULL,
  cause TEXT NOT NULL CHECK (cause IN ('id-jump', 'unclean-end', 'start', 'snapshot')),
  state TEXT NOT NULL CHECK (state IN ('open', 'lost')),
  pages INTEGER NOT NULL DEFAULT 0,
  opened_ms INTEGER NOT NULL,
  PRIMARY KEY (chat_id, above)
) STRICT;

-- Facts the core has no shape for yet, kept for the step that maps them ("deferred, not lost"):
-- permanent deletes until KT-2. `kind` has no CHECK: a later kind needs no migration.
CREATE TABLE deferred(
  seq INTEGER PRIMARY KEY,
  chat_id INTEGER NOT NULL,
  kind TEXT NOT NULL,
  payload TEXT NOT NULL,
  map_version INTEGER NOT NULL,
  at_ms INTEGER NOT NULL
) STRICT;

-- Whether the last helper run ended cleanly (every frame before TDLib's `Closed` committed).
CREATE TABLE run_state(
  id INTEGER PRIMARY KEY CHECK (id = 1),
  clean_end INTEGER NOT NULL CHECK (clean_end IN (0, 1)),
  ended_ms INTEGER
) STRICT;

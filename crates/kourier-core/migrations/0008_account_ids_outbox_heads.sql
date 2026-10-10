-- Phase I (architecture §13).
--
-- Account ids are never reused, like message, outbox and download ids since 0003 and 0004:
-- rows without a foreign key are kept per account id (`settings` scopes `account:<id>`: the id
-- allocator, the clock verdict, a challenge that waits), and so are sets in memory. With a
-- plain INTEGER PRIMARY KEY an account added after the newest one was removed got that id
-- again, and with it whatever such bookkeeping was left. The table is rebuilt with
-- AUTOINCREMENT, keeping every row and id; new ids continue after the highest one present.
-- Every other table of an account references `accounts`, so this is a rebuild step: it runs
-- with foreign keys off, and nothing cascades when the old table is dropped.

CREATE TABLE accounts_v2(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
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
  created_at INTEGER NOT NULL,              -- no keys here: key.wrap (§5.2)
  number TEXT);                             -- the account's own number (0007); personal data
INSERT INTO accounts_v2(id, uuid, network, state, state_reason, own_ids, own_device,
    display_name, settings, local_prefs, caps, sort_order, created_at, number)
  SELECT id, uuid, network, state, state_reason, own_ids, own_device, display_name, settings,
    local_prefs, caps, sort_order, created_at, number
  FROM accounts ORDER BY id;
DROP TABLE accounts;
ALTER TABLE accounts_v2 RENAME TO accounts;

-- The dispatcher asks for the head of each conversation's queue after every answer. With the
-- waiting rows in an index of their own it reads one entry per conversation that has any,
-- not every waiting row of the account: a send of thousands of files is thousands of rows
-- of one conversation (§4.8 step 2). `Staged` rows (a large send whose messages are still
-- being written, `outbox::enqueue_files`) and `Failed` ones are not in it.
CREATE INDEX outbox_waiting ON outbox(conversation_id, id)
  WHERE state IN ('Queued', 'Transmitting');

-- Download targets whose attachment row went while the backend was still writing the file
-- (the message was deleted, expired or cleared, or the one attachment was deleted). The row
-- is gone, the backend's answer still comes: it names the request, and the file it left at
-- `path` is removed then instead of at the next start's sweep (§5.8). Rows are dropped with
-- the answer and at every start (the sweep has removed their files by then). A download
-- that is given up at an actor start because it was never answered is not recorded: the
-- actor that was asked is gone, and with it whatever would have written the file.
CREATE TABLE download_orphans(
  request INTEGER PRIMARY KEY,              -- = DownloadId (`attachment_downloads.id`, never reused)
  account_id INTEGER NOT NULL REFERENCES accounts ON DELETE CASCADE,
  attachment_id INTEGER NOT NULL,           -- the row that is gone (answers without a request id)
  path TEXT NOT NULL,                       -- relative to accounts/<uuid>/blobs
  dropped_at INTEGER NOT NULL);
CREATE INDEX download_orphans_by_attachment ON download_orphans(attachment_id);

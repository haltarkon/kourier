-- WhatsApp backend.db, fourth schema (W4, step C6): the history the phone pushes after a link.
-- FROZEN once it ran on a kept profile: a change is a new number, never an edit of this file. (No kept
-- WhatsApp profile exists before the first real run, so the chunks of W4 that find a column missing
-- may still amend this file until then.)
--
-- The chunks themselves live in the stage file (`history/stage.db`, its own component): this file holds
-- what the delivery of a chunk to the core needs to be exactly-once and what the person was told.
-- Everything here is keyed by text ids the core knows (conversation ids as the live map writes them) and
-- numbers; no message body is stored in any of these tables except `history_deferred.item`, the part of
-- a chunk this build's map cannot show yet (the same reason `inbox` keeps a deferred row).

-- Every chunk the phone's notice announced: the notice's message id, recorded in the zone that finishes
-- the notice's inbox row (only a notice from our own phone counts; upstream honours no other). The stage
-- holds the chunk once it is captured; an id announced and not in the stage at the next start was lost
-- (the connection or the process ended between the notice and the capture): `lost` is set once, when it
-- is counted, so a later start does not count it again.
CREATE TABLE history_announced(
  msg_id TEXT PRIMARY KEY,
  announced_at INTEGER NOT NULL,                       -- wall clock (ms)
  lost INTEGER NOT NULL DEFAULT 0 CHECK (lost IN (0, 1))
) STRICT, WITHOUT ROWID;

-- THE CURSOR: how far the stage has been appended to `core_outbox`. It moves in the zone that appends a
-- delivery row, so every piece of the stage is appended exactly once whatever ends the process. One row.
--   chunk  the stage's `chunks.seq` being delivered (0: none yet)
--   phase  0 people (push names), 1 chats, 2 messages, 3 the end rows of the chunk
--   conv   the chat being filled: its place `n` in the chunk's plan
--   msg    how many of that chat's messages are appended
--   idle_ms  the time the delivery RAN (never the wall clock, never the time the program was closed) with
--          the oldest unsettled row at the front of `core_outbox` and no answer of the core since: at the
--          bound the row is taken back
--   stage  the incarnation of the stage file these books point into (`meta.incarnation` of `history/stage.db`,
--          W4 fix HIGH-1); NULL while no stage was adopted. A stage with another id means the books describe a
--          file that is gone: they start over in the zone that writes the new id, before anything is delivered.
--   owner  the stage's owner (the account's own id), decided ONCE from the stored device and kept (LOW-2): never
--          recomputed at an open, so learning the LID later cannot make a stage "foreign"
--   live_floor  when the oldest chunk of the stage that is not delivered yet was captured (wall clock, ms), NULL when
--          none is (W4 fix F5; the driver keeps it): the notes of `history_live` and `history_named` are kept for a day
--          BEFORE it, because a hint or a name is decided when the chunk is delivered, possibly days after its capture
CREATE TABLE history_cursor(
  id INTEGER PRIMARY KEY CHECK (id = 1),
  chunk INTEGER NOT NULL DEFAULT 0,
  phase INTEGER NOT NULL DEFAULT 0,
  conv INTEGER NOT NULL DEFAULT 0,
  msg INTEGER NOT NULL DEFAULT 0,
  idle_ms INTEGER NOT NULL DEFAULT 0,
  stage BLOB,
  owner TEXT,
  live_floor INTEGER
) STRICT;
INSERT INTO history_cursor(id) VALUES (1);

-- The delivery rows that are not settled: appended to `core_outbox` and not acknowledged yet (`seq` is
-- their row there; at most two at a time), or taken back to be appended again as single messages (`seq`
-- NULL). What a row holds is named by its place in the chunk, never copied: the cursor it began at and the
-- cursor it left (a taken-back row is read again from the first up to the second, one unit at a time).
--   kind    0 people, 1 chats, 2 messages of the chat at plan place `place`, 3 the end rows of a chunk
--   place   the chat's place in the chunk's plan (kind 2)
--   conv    the encoded conversation key the row is about ('' for people and chats)
--   first   where in its phase the row began (the cursor's `msg`; it moves while a taken-back row is redone)
--   end_phase, end_place, end_first   the cursor after the row
--   single  one unit: it cannot be cut further
--   messages  the messages the row handed to the core (what a take-back gives back from the counts)
--   starts  starts of the application that found the row not acknowledged at the front of `core_outbox`
CREATE TABLE history_delivery(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  seq INTEGER UNIQUE,
  chunk INTEGER NOT NULL,
  kind INTEGER NOT NULL,
  place INTEGER NOT NULL DEFAULT 0,
  conv TEXT NOT NULL DEFAULT '',
  first INTEGER NOT NULL DEFAULT 0,
  end_phase INTEGER NOT NULL DEFAULT 0,
  end_place INTEGER NOT NULL DEFAULT 0,
  end_first INTEGER NOT NULL DEFAULT 0,
  single INTEGER NOT NULL DEFAULT 0 CHECK (single IN (0, 1)),
  messages INTEGER NOT NULL DEFAULT 0,
  starts INTEGER NOT NULL DEFAULT 0
) STRICT;

-- The conversations the push touched.
--   more         the push has not ended for this chat: an end row (`has_more: false`) is owed
--   hint_stated  the phone's unread count was stated for this chat once (never again, whichever chunk
--                brings the chat next: the core REPLACES a hint with the next one)
--   read_here    the chat was read on this computer since the link: no hint is stated after that
--   last_chunk   the stage's chunk that touched it last; sync_type that chunk's type
--   ended        that chunk said the phone has nothing more for this chat (`endOfHistoryTransfer`)
-- `more` is set only by a sync type that pushes chats (`progress::pushes_chats`); a chat is ended (an
-- empty batch with `has_more: false`, `more` cleared) when `ended` names the chunk just delivered or when
-- the push of its sync type is over (`progress::Facts::over`).
CREATE TABLE history_chats(
  conv TEXT PRIMARY KEY,
  more INTEGER NOT NULL DEFAULT 1 CHECK (more IN (0, 1)),
  hint_stated INTEGER NOT NULL DEFAULT 0 CHECK (hint_stated IN (0, 1)),
  read_here INTEGER NOT NULL DEFAULT 0 CHECK (read_here IN (0, 1)),
  last_chunk INTEGER NOT NULL DEFAULT 0,
  sync_type INTEGER,
  ended INTEGER NOT NULL DEFAULT 0 CHECK (ended IN (0, 1))
) STRICT, WITHOUT ROWID;

-- The chunks whose content the cursor has passed (every row of it is appended; the end rows may still
-- follow), with what the end of a push needs of each: its sync type, its `chunkOrder` and its `progress`
-- (0 to 100, the phone's own). Written in the zone that moves the cursor to the end of the chunk. A chunk
-- of the stage that is not here is still waiting to be delivered.
--   retired  the stage's copy of the chunk was set `done` (its bytes dropped) after every delivery row of the
--            chunk settled: written AFTER the stage job, so a kill between the two redoes the (idempotent) job
CREATE TABLE history_chunks(
  chunk INTEGER PRIMARY KEY,                            -- the stage's `chunks.seq`
  sync_type INTEGER,
  chunk_order INTEGER,
  progress INTEGER,
  retired INTEGER NOT NULL DEFAULT 0 CHECK (retired IN (0, 1))
) STRICT;

-- The parts of a chunk this build's map does not show yet (a kind that waits for a later step, a group
-- notice stub, and the chunk's parts that are no message: call-log records, past participants, the phone's
-- address-book names and settings): kept with the map version that deferred them. `chunk_msg_id` is the
-- notice id (the stage's key); `n` the part's place in the chunk. `kind` says what `item` is (the encoded
-- protobuf of that kind): 0 a `WebMessageInfo`, delivered by a build whose version is higher as a silent
-- history row; the others (1 `CallLogRecord`, 2 `PastParticipants`, 3 `InlineContact`, 4 `GlobalSettings`)
-- wait for the step that shows them and are never a message. `conv` is the chat the part belongs to,
-- or '' when it belongs to none.
CREATE TABLE history_deferred(
  chunk_msg_id TEXT NOT NULL,
  n INTEGER NOT NULL,
  conv TEXT NOT NULL,
  item BLOB NOT NULL,
  map_version INTEGER NOT NULL,
  kind INTEGER NOT NULL DEFAULT 0 CHECK (kind BETWEEN 0 AND 4),
  PRIMARY KEY (chunk_msg_id, n)
) STRICT, WITHOUT ROWID;
CREATE INDEX history_deferred_by_version ON history_deferred(map_version);

-- Counts of the history, over the whole life of the account: messages handed to the core, messages the
-- core refused (dropped), parts deferred, parts of the push that did not read (bad: a chunk, or a chat of
-- one that did not decode), chunks announced and never
-- captured (lost), and how many of the lost the person has been told about (`lost_told <= lost`: the
-- notice is sent once per start until told, and never when nothing was lost). One row.
CREATE TABLE history_counts(
  id INTEGER PRIMARY KEY CHECK (id = 1),
  delivered INTEGER NOT NULL DEFAULT 0,
  dropped INTEGER NOT NULL DEFAULT 0,
  deferred INTEGER NOT NULL DEFAULT 0,
  bad INTEGER NOT NULL DEFAULT 0,
  lost INTEGER NOT NULL DEFAULT 0,
  lost_told INTEGER NOT NULL DEFAULT 0
) STRICT;
INSERT INTO history_counts(id) VALUES (1);

-- The messages of OTHER people the live path delivered while a push can still state a hint, by chat and
-- server time (ms): the phone's unread count of a chat includes them, so the hint the history states is the
-- phone's count minus the live ones older than the chat's last message (stage 2 writes a row only while
-- `history::live::wanted` says a push can still state a hint, and prunes in its own zone with
-- `history_live_by_time`: notes older than a day before `history_cursor.live_floor`, or before now when no chunk waits).
CREATE TABLE history_live(
  id INTEGER PRIMARY KEY,
  conv TEXT NOT NULL,
  sent_at INTEGER NOT NULL
) STRICT;
CREATE INDEX history_live_by_conv ON history_live(conv, sent_at);
CREATE INDEX history_live_by_time ON history_live(sent_at);

-- What the LIVE path named since the link (W4 fix F3): a group's title (kind 0, `key` the encoded conversation)
-- or a person's push name (kind 1, `key` the user id) that a live event stated. A chunk is a snapshot taken
-- earlier than its delivery, so the history delivery leaves `title` / `profile_name` alone for these: the
-- live value is newer. Written in the zone that states the name (stage 2's, or the handler's group commit),
-- only while `history::live::wanted` is true; pruned with `history_live`.
CREATE TABLE history_named(
  kind INTEGER NOT NULL CHECK (kind IN (0, 1)),
  key TEXT NOT NULL,
  at INTEGER NOT NULL,
  PRIMARY KEY (kind, key)
) STRICT, WITHOUT ROWID;
CREATE INDEX history_named_by_time ON history_named(at);

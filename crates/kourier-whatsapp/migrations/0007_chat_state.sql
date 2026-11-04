-- WhatsApp backend.db, seventh schema (W6a1, chunk A2; decisions D2-D6, D12 of `target/wa/w6-decisions.md`): what the phone's
-- app state says about each chat (the mirror), the books of the start-up resync, and the call lines.
-- FROZEN once it ran on a kept profile: a change is a new number, never an edit of this file. (No kept WhatsApp profile has
-- run W6a1 yet, so the chunks of W6a1 that find a column missing may still amend this file until then.)
--
-- Nothing here holds a message body, a name or a number: encoded conversation keys, small numbers, times and call ids.
-- A call id and a conversation key are identifiers the person never sees; nothing of a row is logged.

-- THE MIRROR (D5, H5): one row per (chat, field) the phone's app state ever said something about, written in the SAME
-- zone as the core events it causes, so a core event is stated only when a value really changes and the full pin list
-- can be rebuilt from one pin event. `chat` is the encoded conversation key (`ConvKey::encode`, the form `map::conv_of`
-- gives), the same text as `conv` elsewhere in this file.
--   field       'archived' (value 1 = archived), 'pinned' (1 = pinned; `action_ms` is the order, newest first),
--               'muted' (0 = not muted, 9223372036854775807 = for ever (the phone's -1), else the end in ms),
--               'marked_unread' (1 = marked unread on the phone). The default of every field is 0.
--   action_ms   the mutation's own time (ms): the pin order. The latest ARRIVAL wins, times of different devices are
--               not compared (upstream applies a patch only at the stored version + 1). For `archived` it is the time of
--               the archive as the phone's setting sees it (F1): the last message of the action's range (seconds x 1000;
--               the action's own time when the range names none), and, after a message brought the chat back here, that
--               message's sent time (see `chat_counted`).
--   gen         the generation of the collection the row was last touched in (`appstate_resyncs.gen`): a resync Kourier
--               asks for opens a new one, every row its replay touches takes it, and a row left behind is what the
--               replay did not mention (H1).
--   collection  the app-state collection the verb lives in (`regular_low`, `regular_high`, ...).
--   stated      1 once the core was told the value (the chat is known to it). 0 while the core does not know the chat
--               (H5): the value waits here and is stated in the zone that first states the chat; a row at 0 holding the
--               default needs no statement.
CREATE TABLE chat_state(
  chat TEXT NOT NULL,
  field TEXT NOT NULL CHECK (field IN ('archived', 'pinned', 'muted', 'marked_unread')),
  value INTEGER NOT NULL,
  action_ms INTEGER NOT NULL,
  gen INTEGER NOT NULL DEFAULT 0,
  collection TEXT NOT NULL,
  stated INTEGER NOT NULL DEFAULT 0 CHECK (stated IN (0, 1)),
  PRIMARY KEY (chat, field)
) STRICT, WITHOUT ROWID;
CREATE INDEX chat_state_by_collection ON chat_state(collection, gen);

-- Account-level settings of the phone's app state (H6): `unarchive_chats` = 1 when the phone brings an archived chat
-- back on a new message ("Keep chats archived" is OFF). Never written to the phone. One row per key.
CREATE TABLE account_appstate(
  key TEXT PRIMARY KEY,
  value INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

-- THE NEWEST MESSAGE THAT COUNTS (F1, INTEROP-1, DUR-3): per chat, the sent time (ms) of the newest message Kourier handed
-- to the core that would bring an archived chat back (`zone::message_counts`; the history's rows count by the same rule
-- with their blanket `read: true` left out), written in the zone that delivers the message. With the phone's "Keep chats
-- archived" OFF (`account_appstate.unarchive_chats`) an `archive` record of the phone is only as new as its range's last
-- message: the phone writes NOTHING when its own setting moves a chat out of the archive, so a record older than a
-- message that counts is a stale one and the chat is not archived (`appstate::zone`).
CREATE TABLE chat_counted(
  chat TEXT PRIMARY KEY,
  newest_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

-- THE CUT OF A CHAT (F3, INTEROP-4): per chat, the time (ms) up to which the user's clear or delete took the chat's history
-- away, as far as this backend knows it: Kourier's own confirmed `ClearConversation` (the larger of the command's two
-- times, else the time of the confirmation) and the phone's mapped `clearChat` / `deleteChat` (the range's time, else the
-- action's own). Only ever raised. The core keeps the same cut for its history rows; here it tells a call line of the
-- phone's log that happened AFTER the cut (a live row, which brings a deleted chat back) from one at or below it (a
-- history row, which the core drops again). No row = the backend knows of no cut.
CREATE TABLE chat_cut(
  chat TEXT PRIMARY KEY,
  cut_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

-- THE LINK'S TIME (F13, INTEROP-3, his answer "Only calls after link"): when this store was paired, in ms: the time of the
-- scan, written once by `appstate::startup::link_in` in the zone that appends the link's `AccountReady` (a new link of the
-- same file rewrites it). ONE row. A call of the phone's log from BEFORE it adds no chat (`calls::log::place`): it lands
-- only in a chat that already exists. No row = this store saw no link (nothing counts as older than it). The resync books
-- below cannot serve: a later snapshot of ours re-stamps `asked_at`.
CREATE TABLE account_link(
  id INTEGER PRIMARY KEY CHECK (id = 1),
  linked_ms INTEGER NOT NULL
) STRICT;

-- THE BOOKS OF THE START-UP RESYNC (D2, B1, M7): per collection, when Kourier last asked the server for a snapshot
-- (`asked_at`, ms; also stamped for every collection at the link, which needs no snapshot of ours), the generation
-- that snapshot opened (`gen`, see `chat_state.gen`), and when the server answered it fatally (`fatal_at`, 400/404:
-- asking again gets the same answer; a new map version or a new link clears it). No row = never asked.
CREATE TABLE appstate_resyncs(
  collection TEXT PRIMARY KEY,
  asked_at INTEGER NOT NULL DEFAULT 0,
  gen INTEGER NOT NULL DEFAULT 0,
  fatal_at INTEGER
) STRICT, WITHOUT ROWID;

-- The app-state map version this store was last read with (D4): a build that maps more verbs than the stored version
-- did marks the collections whose verbs grew dirty once (one resync each). ONE row, written when the map is first
-- used; no row = 0.
CREATE TABLE appstate_map(
  id INTEGER PRIMARY KEY CHECK (id = 1),
  version INTEGER NOT NULL
) STRICT;

-- THE CALL LINES (D12, H2): which conversation a call's line was filed under, written by the call interceptor in the
-- zone that writes the line, so the call log's outcome lands in the SAME conversation and `call:<id>` (the log may name
-- the other namespace of the same person). `at` is the line's time (ms); `created_at` is when the row was written
-- (wall clock, ms): rows go at a start after 30 days.
CREATE TABLE call_lines(
  call_id TEXT PRIMARY KEY,
  conv TEXT NOT NULL,
  at INTEGER NOT NULL,
  created_at INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
CREATE INDEX call_lines_by_age ON call_lines(created_at);

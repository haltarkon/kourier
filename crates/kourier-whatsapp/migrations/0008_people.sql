-- WhatsApp backend.db, eighth schema (W6a2, chunk B3; decisions D7-D11, D22 of `target/wa/w6-decisions.md`): what the core
-- was told about groups and people, so that a statement is made when something changed and not every time it could be.
-- 0007 is frozen: these tables are 0008's alone. No kept WhatsApp profile has run W6a2 yet, so a later chunk of W6a2 that
-- finds a column missing may still amend THIS file until then; after the first kept run a change is a new number.
--
-- Rows of `merges_stated`, `names_stated`, `blocked_stated` and `persons_known` mean "the core was told": a
-- `Rebuild` (the core was made again) forgets every one of them in the zone that states `AccountReady` again (`store::people::forget_statements`),
-- except the push names (`names_stated.kind = 'profile'`), which stay as rows marked not stated and are stated again, once.
-- `group_state` is both: its server facts (`not_member`, `rules`, `truncated`, `parent`, the times, the marks) stay through a
-- `Rebuild`, only `members_stated` and `parent_stated` go back to 0. A name is the one thing here that is
-- text a person typed: `names_stated.value` (backend.db is the encrypted file; nothing of a row is ever logged).

-- WHAT THE CORE KNOWS OF EACH GROUP (D9, D10, D11, M1, M2, M5). One row per group once anything is known; the key is the
-- encoded conversation key (`ConvKey::encode`, the same text as `conv` elsewhere in this file). The column the lead's
-- list calls `group` is `conv` here (`GROUP` is an SQL keyword).
--   members_stated  1 once a zone told the core the group's WHOLE member list (a `Found` answer, never a notice or a
--                   `Truncated` one): an admin's delete of this group is mapped only from then on (D11) and the zone
--                   that sets it un-parks the group's parked revokes.
--   subject_time    the title's time in seconds (`s_t`): a notice or metadata older than this never sets the title (M5).
--                   NULL = no title was stated with a time.
--   timer           the group's disappearing timer in seconds the core was last told (0 = told: none); NULL = never.
--   timer_version   the contract's version of that statement, in seconds (`ConversationPatch::timer_version`): a notice
--                   orders by its own time, a metadata by the time of the fetch (the metadata carries no time).
--   rules           the group's rules and our place in it as the last answered fetch said them (B4, `groups::Rules`, a bit
--                   set: 1 announcements only, 2 only admins change the group's data, 4 only admins add members, 8 suspended,
--                   16 we are an admin); NULL = never answered. The group's FEATURES are made of this row, so a restate
--                   (a new `features::VERSION`, a `Rebuild`) states the same `group_info` and `cannot_send` again and does
--                   not clobber what the fetch said.
--   truncated       1 while the last answer for this group was `Truncated` (no member list): it gets ONE single query
--                   per start under the fetch bucket (M1), its stored list stays.
--   not_member      1 after `Forbidden` / `NotFound` (we left, were removed, or the group is gone): the group is
--                   `NotMember`, its parked revokes are finished with no event (M3).
--   parent          the community the last answered fetch named as this group's parent (an encoded group key); NULL = none
--                   (not a subgroup, or never answered). A subgroup's `parent` is stated to the core only when the core already
--                   knows the parent chat (`features_stated`, version > 0) or the same zone states the parent first: the core
--                   makes a bare, title-less chat for a parent it does not know (the lead's call after part 1, Q3).
--   parent_stated   1 once the core was told `parent` for this group; 0 = the parent is named and waits: the zone that first
--                   states the parent group states it for every subgroup that names it.
--   dirty_at        the time (ms) of the FIRST notice the event cannot carry, or `groups_dirty`, that asked for a fresh fetch of
--                   this group and was not answered yet (the order the marks are served in); NULL = no mark. It outlives a
--                   kill (the next start fetches), and it is cleared by the zone of the answer that covers it.
--   dirty_last      the time (ms) of the NEWEST ask while the mark stands (fix F16, DUR-2; NULL exactly when `dirty_at` is):
--                   an answer computed before it does not cover it, so the mark is cleared only when `dirty_last` is not newer
--                   than the answer's ask, and a mark newer than the group's last single query of the start is served again
--                   (fix F17, DUR-3).
--   fetched_at      the time (ms) of the last answered fetch of this group.
CREATE TABLE group_state(
  conv TEXT PRIMARY KEY,
  members_stated INTEGER NOT NULL DEFAULT 0 CHECK (members_stated IN (0, 1)),
  subject_time INTEGER,
  timer INTEGER,
  timer_version INTEGER,
  rules INTEGER,
  truncated INTEGER NOT NULL DEFAULT 0 CHECK (truncated IN (0, 1)),
  not_member INTEGER NOT NULL DEFAULT 0 CHECK (not_member IN (0, 1)),
  parent TEXT,
  parent_stated INTEGER NOT NULL DEFAULT 0 CHECK (parent_stated IN (0, 1)),
  dirty_at INTEGER,
  dirty_last INTEGER,
  fetched_at INTEGER
) STRICT, WITHOUT ROWID;
CREATE INDEX group_state_dirty ON group_state(dirty_at) WHERE dirty_at IS NOT NULL;
CREATE INDEX group_state_parent_waits ON group_state(parent) WHERE parent IS NOT NULL AND parent_stated = 0;

-- THE PAIRS OF ONE NUMBER AND ONE LID THE CORE WAS TOLD ARE ONE PERSON (D7, H4): `IdentityMerged` is final, so each pair
-- is stated once; `pn` and `lid` are the encoded user ids (`pn:<digits>`, `lid:<digits>`).
-- `by_seq` (G2, NEW-1) is the history row (`core_outbox.seq`) whose zone NEWLY recorded the pair, NULL for a pair any other zone
-- recorded (the live path, the sweep, a pair that a row repeats): a refused history row that is taken back forgets only the pairs
-- it recorded itself, so a pair an earlier row told stays told. The table keeps its `rowid` for that: a zone reads the newest one
-- before it writes and claims every row above it (`store::merges::mark`, `claim_since`).
CREATE TABLE merges_stated(
  pn TEXT NOT NULL,
  lid TEXT NOT NULL,
  by_seq INTEGER,
  PRIMARY KEY (pn, lid)
) STRICT;
-- `store::merges::crosses_a_stated_pair` asks for a pair around a LID too (fix F20, DUR-11): without this index its second term
-- scans the table on the writer thread, per pair, in the sweep and in the zone.
CREATE INDEX merges_stated_lid ON merges_stated(lid);

-- THE NAMES THE CORE WAS TOLD (D8): per person and kind the value last stated, so that a name is stated on a change only.
-- `person` is the encoded user id; `kind` is 'profile' (the self-chosen push name), 'system' (the address book's name) or
-- 'number' (the phone number the core was told with the address book's name, as stated: `+<digits>`; stated beside 'system'
-- and gone with it). The address book's names wait in `address_book` below for the person to appear.
-- `stated` is 0 for a 'profile' row a `Rebuild` kept (W6a2 F21, DUR-12): the new core was told none of it, but backend.db still
-- holds exactly the push names the old one was told, and the people who never write again (silent members of a group) would
-- stay nameless. The zone that makes such a person known to the core again states his name once and sets `stated` to 1
-- (`crate::book::states`). A 'system' or 'number' row is deleted by a `Rebuild`: the mirror below states them again.
CREATE TABLE names_stated(
  person TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('profile', 'system', 'number')),
  value TEXT NOT NULL,
  stated INTEGER NOT NULL DEFAULT 1 CHECK (stated IN (0, 1)),
  PRIMARY KEY (person, kind)
) STRICT, WITHOUT ROWID;

-- THE PHONE'S ADDRESS BOOK (D8, B8b): a MIRROR of what the phone said (the `contact` mutation of `critical_unblock_low`, history
-- kind 3), not a statement: it is not forgotten by a `Rebuild`, and it goes with the account's file. One row per contact the
-- phone named. Only a person Kourier has a chat with or who is in one of its groups (`persons_known`) gets his name stated
-- to the core; the rest stay here and are stated when the person first appears. `contact` is the encoded id of the
-- mutation's index; `pn` and `lid` are the encoded `pn:` / `lid:` ids the row names (the index itself, or the action's
-- `pnJid` / `lidJid`), so a person met in either form finds his name; `name` is the contact's full name, else the first.
-- `origin` is who said it: 0 the app state's live `contact` mutation, 1 the history's kind 3 (older, taken up long after).
-- One person is one entry: a history entry is not taken while any row names its number or its LID, and a live word
-- (a put or a remove) also takes the history rows that name the same forms (W6a2 PRIV-4).
-- `gen` is the generation of the collection `critical_unblock_low` when a LIVE row (origin 0) was last said, stamped like
-- `chat_state.gen` (W6a2 F19, DUR-6): a replay that says the same thing again is a touch. After a snapshot Kourier asked for
-- came back synced and had replayed at least one row of the collection, the live rows still on an older generation are
-- contacts the phone no longer has and are taken back (`appstate::zone::reconcile`, through `book::apply(Change::Remove)`).
-- A history row (origin 1) has no generation to be left behind: it is the history's word, not the collection's (0).
CREATE TABLE address_book(
  contact TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  pn TEXT,
  lid TEXT,
  origin INTEGER NOT NULL DEFAULT 0 CHECK (origin IN (0, 1)),
  gen INTEGER NOT NULL DEFAULT 0
) STRICT, WITHOUT ROWID;
CREATE INDEX address_book_pn ON address_book(pn) WHERE pn IS NOT NULL;
CREATE INDEX address_book_lid ON address_book(lid) WHERE lid IS NOT NULL;

-- WHAT A HISTORY CHUNK NAMED OF PEOPLE THE CORE DOES NOT KNOW (W6a2 F11, PRIV-5; D8's scope and N25 everywhere): the pairs of
-- a chunk's `phoneNumberToLidMappings` and its `pushnames` are stated in the chunk's people row only for a person in
-- `persons_known`; the rest wait HERE, in the encoded ids and the cleaned name the row would have stated, and are stated once
-- by the zone where the person first becomes known (`crate::strangers::release`, from `book::states`), then forgotten. Like
-- `address_book`, these rows are what the PHONE said, not what the core was told: a `Rebuild` keeps them, and they go with the
-- account's file. A number of a stranger is an alias row, and a stranger's self-chosen name is text of a person Kourier has
-- no chat or group with: neither has a place in core.db.
CREATE TABLE kept_pairs(
  pn TEXT NOT NULL,
  lid TEXT NOT NULL,
  PRIMARY KEY (pn, lid)
) STRICT, WITHOUT ROWID;
CREATE INDEX kept_pairs_lid ON kept_pairs(lid);
CREATE TABLE kept_names(
  person TEXT PRIMARY KEY,
  name TEXT NOT NULL
) STRICT, WITHOUT ROWID;

-- THE PEOPLE THE CORE WAS TOLD OF AS A CHAT'S PEER OR A GROUP'S MEMBER (D8's scope rule): a person is written here in the
-- zone that states him in a direct chat, a member list or as the author of a message, and from then on his address-book
-- name is stated. A person only the blocklist named is NOT here. Like the other statements, a `Rebuild` forgets it.
CREATE TABLE persons_known(
  person TEXT PRIMARY KEY
) STRICT, WITHOUT ROWID;

-- THE PEOPLE THE CORE WAS TOLD ARE BLOCKED (D22): the phone's blocklist is read once per start; a person who is in this
-- table and no longer on the list is stated unblocked, and one on the list and not here is stated blocked. `person` is
-- the encoded user id as the blocklist wrote it (a number or a LID).
CREATE TABLE blocked_stated(
  person TEXT PRIMARY KEY
) STRICT, WITHOUT ROWID;

-- WHEN THE PHONE'S BLOCKLIST WAS LAST STATED (D22, the lead's call after part 1: Q1): the list is asked at most once per 24 hours
-- per account, because a second identical IQ at every start is traffic the phone's own clients do not make and the list
-- changes rarely. ONE row (`id` 1): the time (ms) of the zone that stated an ANSWER; a read that got no answer stamps nothing.
-- A `Rebuild` (the core lost what it was told) and a link (a new account's first start) delete it, so the next `Online` asks.
CREATE TABLE blocklist_read(
  id INTEGER PRIMARY KEY CHECK (id = 1),
  read_at INTEGER NOT NULL
) STRICT;

-- A GROUP'S ADMIN DELETE WAITS FOR THE GROUP'S MEMBER LIST (D11, M2): the core judges an admin's delete-for-everyone by the
-- member list it was told, and drops the delete for good when the list is not there yet. So stage 2 parks such an inbox row
-- (`state = 'new'`, `parked_until` = its arrival + 45 days: the poison rule's age, counted from `received_at`, never from a
-- take-up) and writes the group's encoded key here, with NO poison attempt counted. The zone that states the group's members
-- clears the mark and the park in the same transaction; an answer that says we are not in the group finishes the rows with
-- no event (M3). NULL = the row waits for nothing. `attempts` and `parked_until` keep their meaning for the failure backoff.
ALTER TABLE inbox ADD COLUMN members_wait TEXT;
CREATE INDEX inbox_members_wait ON inbox(members_wait) WHERE members_wait IS NOT NULL;

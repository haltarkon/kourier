-- Signal backend.db: the delivery of a committed history import to the core
-- (docs/architecture.md §14.2 item 14; `transfer/deliver.rs`).

-- THE CURSOR: how far the committed stage has been appended to `core_outbox`. It moves in
-- the zone that appends a delivery row, so every piece of the stage is appended exactly
-- once whatever ends the process. Numbers only.
--   deliver_phase 0: people; `deliver_pos` of them are appended (the stage's `contacts.n`
--                    below it);
--   deliver_phase 1: the chats that are shown, the least recent first; `deliver_pos` of
--                    them are upserted;
--   deliver_phase 2: messages; `deliver_pos` chats (the most recent first) are filled, and
--                    of the chat at that place the rows up to `deliver_n` (the stage's
--                    `items.n`, -1: none yet).
ALTER TABLE history_transfer ADD COLUMN deliver_phase INTEGER NOT NULL DEFAULT 0;
ALTER TABLE history_transfer ADD COLUMN deliver_pos INTEGER NOT NULL DEFAULT 0;
ALTER TABLE history_transfer ADD COLUMN deliver_n INTEGER NOT NULL DEFAULT -1;

-- Counts of the delivery: messages handed on (the progress a UI shows), delivery rows
-- appended, messages the core refused or never acknowledged (dropped), rows that were due
-- under a clock the server confirmed (not delivered).
ALTER TABLE history_transfer ADD COLUMN delivered_messages INTEGER NOT NULL DEFAULT 0;
ALTER TABLE history_transfer ADD COLUMN deliver_rows INTEGER NOT NULL DEFAULT 0;
ALTER TABLE history_transfer ADD COLUMN deliver_dropped INTEGER NOT NULL DEFAULT 0;
ALTER TABLE history_transfer ADD COLUMN deliver_expired INTEGER NOT NULL DEFAULT 0;

-- How many chats (in the order of delivery) have been given the phone's unread count. Kept
-- when a lost stage is read again, so a chat that was filled, and maybe read here since,
-- does not get its count a second time.
ALTER TABLE history_transfer ADD COLUMN deliver_hinted INTEGER NOT NULL DEFAULT 0;

-- 1 from the first delivery row on: the core holds something of this import, so a failure
-- from here on ends the transfer as "incomplete", never as if nothing had been transferred.
ALTER TABLE history_transfer ADD COLUMN deliver_began INTEGER NOT NULL DEFAULT 0;

-- The bounds of the delivery: the time it has RUN without any acknowledgement from the
-- core (over every run of the application, never the wall clock), and how many times in a
-- row its rows were taken back without one.
ALTER TABLE history_transfer ADD COLUMN deliver_idle_ms INTEGER NOT NULL DEFAULT 0;
ALTER TABLE history_transfer ADD COLUMN deliver_strikes INTEGER NOT NULL DEFAULT 0;

-- Two more bounds that nothing resets while the delivery goes on: how many single units the
-- core refused in a row without a row that holds messages (or people, or chats) being
-- acknowledged in between, and the total of RUNNING time spent waiting for an acknowledgement
-- that ended in a row being taken back (a message the core cannot store holds receiving back
-- for the bound each time, and an acknowledgement in between must not make the next one
-- free). Either one ends the transfer as incomplete.
ALTER TABLE history_transfer ADD COLUMN deliver_refused INTEGER NOT NULL DEFAULT 0;
ALTER TABLE history_transfer ADD COLUMN deliver_stuck_ms INTEGER NOT NULL DEFAULT 0;

-- The delivery rows that are not settled: appended to `core_outbox` and not acknowledged
-- yet (`seq` is their row there; at most two at a time), or taken back to be appended again
-- as single messages (`seq` NULL: the core refused the row, or did not acknowledge it within
-- the bounds). What a row holds is named by its place in the stage, never copied: `kind`
-- 0 people, 1 chats, 2 messages of chat `chat`, 3 a chat's unread count; the stage's rows
-- after `after_n` up to `through_n`. Emptied by every end of the transfer.
CREATE TABLE history_delivery(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  seq INTEGER,
  kind INTEGER NOT NULL,
  chat INTEGER NOT NULL DEFAULT 0,
  after_n INTEGER NOT NULL,
  through_n INTEGER NOT NULL,
  ends_chat INTEGER NOT NULL DEFAULT 0,   -- the row is the last of its chat
  single INTEGER NOT NULL DEFAULT 0,      -- one message (person, chat): cannot be cut further
  messages INTEGER NOT NULL DEFAULT 0,
  starts INTEGER NOT NULL DEFAULT 0       -- delivery starts that found it not acknowledged
);

-- WhatsApp backend.db, third schema (W3b fix F12): where the age of an inbox row is counted from.
-- FROZEN once it ran on a kept profile: a change is a new number, never an edit of this file.
--
-- Stage 2's poison rule gives up on a row that fails for 45 days (`stage2::MAX_AGE_MS`), counted from
-- `received_at`. A row a lower map version DEFERRED can sit for longer than that (a kind that waits for
-- a later step), and when a build whose map can try it takes it up, its first failure would find it
-- "45 days old" and end it with a placeholder at once. `aged_from` is the wall clock (ms) of the
-- take-up (0: never taken up): the rule counts from the later of the two. `received_at` stays what it
-- was, the local time the message arrived, because it is the event's too.
ALTER TABLE inbox ADD COLUMN aged_from INTEGER NOT NULL DEFAULT 0;

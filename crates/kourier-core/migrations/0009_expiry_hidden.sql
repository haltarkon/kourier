-- Phase J (user decision of 2026-10-07; architecture §9.6).
--
-- `messages.expiry_hidden`: the message's disappearing timer has run out by this computer's
-- clock, but no backend has confirmed that clock (a start without a network, or a clock a
-- backend found wrong), and the setting says "hide now, erase once the time is confirmed"
-- (`settings` row `core` / `expiry_policy`). The row, its attachments and its files stay; every
-- read that shows message content leaves the row out, and the conversation's counters do not
-- count it. The sweeper (`kourier-core/src/expiry.rs`) is the only writer: it sets the flag,
-- clears it when the message turns out not to be due (or the setting changes), and deletes the
-- row once a backend took the clock. A column, not a view over the clock: it holds across a
-- kill, and the counters in `conversations` are recomputed in the transaction that flips it.
ALTER TABLE messages ADD COLUMN expiry_hidden INTEGER NOT NULL DEFAULT 0;
-- The sweeper asks "is anything hidden that should show again" at every look.
CREATE INDEX messages_expiry_hidden ON messages(account_id) WHERE expiry_hidden = 1;

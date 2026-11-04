-- Upload progress (2026-10-17): the X on a file of a message that is uploading stops it. The
-- stop is a handshake with the backend (`Command::AbortTransmit`), and what the user asked has
-- to survive a quit or a crash between the two ends of it, so it is a durable mark of the
-- message's `Message` outbox row (`outbox.cancel_state`):
--   0  none (every row that exists, and every row at rest),
--   1  ASKED: the X was pressed, the backend's answer is not recorded yet. A restart, an
--      outcome and a lost reply clear it: nothing was promised to the user yet and the message
--      may have gone (the window is the handshake itself, a few milliseconds),
--   2  STOPPING: the backend said `Stopping`, i.e. nothing of the message goes out any more.
--      Only this value takes the message back when the attempt ends `Failed`, when its reply
--      never comes, and at the next start (`outbox::reset_transmitting`): a stopped attempt
--      reached nobody, whatever it ends with.
-- No index (the column is read with the row, by its message), no row converted.
ALTER TABLE outbox ADD COLUMN cancel_state INTEGER NOT NULL DEFAULT 0
    CHECK (cancel_state IN (0, 1, 2));

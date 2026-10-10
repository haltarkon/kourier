-- Call lines (architecture §3.5; notes: a call is one row that later signals change).
--
-- `messages.call_deadline`: set while the row is a call line that rings
-- (`SystemEvent::Call` with `CallState::Ringing`), to the instant (ms) the ring ends by
-- itself; NULL for every other row. The body holds the same instant; the column is what the
-- deadline sweep (`kourier-core/src/calls.rs`) finds its work by, so a ringing line survives a
-- restart or a suspend and never rings forever. Ingest sets it with the body, and clears it
-- when the line moves on or is deleted.
ALTER TABLE messages ADD COLUMN call_deadline INTEGER;
CREATE INDEX messages_ringing ON messages(call_deadline) WHERE call_deadline IS NOT NULL;

-- Poison policy (architecture §5.6, §13 phase C): a core_outbox row the core refused for good
-- (a contract violation, an undecodable row) moves here, so it never blocks the account. Rows stay
-- for diagnosis inside the encrypted backend.db and go away with the account. `reason` is
-- log-safe (it never quotes the payload).
CREATE TABLE core_outbox_poison(
  seq INTEGER PRIMARY KEY,                  -- its former core_outbox seq
  envelope BLOB NOT NULL,
  created_at INTEGER NOT NULL,
  quarantined_at INTEGER NOT NULL,
  reason TEXT NOT NULL
);

-- The generic transactional outbox of every backend (architecture §4.1 principle 6, §5.6).
-- `envelope` is opaque here: a versioned serde Vec<BackendEvent> written by the backend in the
-- same transaction as the state change that implies it. Rows are deleted only after the
-- core's Ack(Ok), in `seq` order.
CREATE TABLE core_outbox(
  seq INTEGER PRIMARY KEY,
  envelope BLOB NOT NULL,
  created_at INTEGER NOT NULL
);

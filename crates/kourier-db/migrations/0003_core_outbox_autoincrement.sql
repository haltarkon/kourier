-- Ids are never reused (architecture §13 phase D "Open for the next phase"): with a plain INTEGER
-- PRIMARY KEY, SQLite hands out max(seq) + 1, so after the table emptied a new row got the seq of
-- a row the core had already acked (and a quarantine of that seq overwrote the earlier poison
-- row). Both tables are rebuilt with AUTOINCREMENT, keeping every row and its seq; the sequence
-- starts above every seq still on record in either table.
CREATE TABLE core_outbox_v3(
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  envelope BLOB NOT NULL,
  created_at INTEGER NOT NULL
);
INSERT INTO core_outbox_v3(seq, envelope, created_at)
  SELECT seq, envelope, created_at FROM core_outbox ORDER BY seq;
DELETE FROM sqlite_sequence WHERE name = 'core_outbox_v3';
INSERT INTO sqlite_sequence(name, seq)
  SELECT 'core_outbox_v3', max(coalesce((SELECT max(seq) FROM core_outbox), 0),
                               coalesce((SELECT max(seq) FROM core_outbox_poison), 0));
DROP TABLE core_outbox;
ALTER TABLE core_outbox_v3 RENAME TO core_outbox;

-- Poison rows get their own key: quarantining a seq twice keeps both rows.
CREATE TABLE core_outbox_poison_v3(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  seq INTEGER NOT NULL,                     -- its former core_outbox seq
  envelope BLOB NOT NULL,
  created_at INTEGER NOT NULL,
  quarantined_at INTEGER NOT NULL,
  reason TEXT NOT NULL
);
INSERT INTO core_outbox_poison_v3(seq, envelope, created_at, quarantined_at, reason)
  SELECT seq, envelope, created_at, quarantined_at, reason FROM core_outbox_poison ORDER BY seq;
DROP TABLE core_outbox_poison;
ALTER TABLE core_outbox_poison_v3 RENAME TO core_outbox_poison;
CREATE INDEX core_outbox_poison_by_seq ON core_outbox_poison(seq);

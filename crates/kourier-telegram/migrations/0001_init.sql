-- Telegram backend.db, first schema (TG-1 plan §2.8, step TG-1a). `core_outbox` comes from
-- kourier-db's own migration component. ONE account per file. TDLib keeps its own database in the
-- account's `tdlib/` directory; this file holds only what Kourier itself needs to know.

-- The account: ONE row. `env` binds at the first login and never changes afterwards; `user_id` is
-- NULL until TDLib reports the account ready; `pin` is the TDLib commit that created `tdlib/`.
CREATE TABLE account(
  id INTEGER PRIMARY KEY CHECK (id = 1),
  env TEXT NOT NULL CHECK (env IN ('production', 'test')),
  user_id INTEGER,
  pin TEXT NOT NULL,
  created_ms INTEGER NOT NULL
) STRICT;

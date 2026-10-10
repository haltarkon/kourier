-- The fake "server" of own sends (§10.2): one row per message copy accepted for a recipient.
-- It doubles as the backend's send_progress (§6.7): a resend skips recipients already here, so a
-- crash between the server's acceptance and the core's commit causes no second delivery. Tests
-- read it with `kourier_fake::server_log`.
CREATE TABLE fake_transmits(
  id INTEGER PRIMARY KEY,
  local TEXT NOT NULL,
  sent_at INTEGER NOT NULL,
  recipient TEXT NOT NULL,
  recipient_update INTEGER NOT NULL,
  at INTEGER NOT NULL);
CREATE INDEX fake_transmits_by_local ON fake_transmits(local, recipient);

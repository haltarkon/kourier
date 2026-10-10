-- Signal backend.db: the history transfer of a fresh link (docs/architecture.md §14.2 item 14).

-- One row, written in the transaction that stores the linked account when the phone put an
-- ephemeral backup key into its provisioning message (it does so only when Kourier asked for
-- the transfer with `backup5` in the linking URL and the phone's user chose to transfer). The
-- row is what lets the transfer go on after a crash or a restart at any point, and what keeps
-- live receiving and the Storage Service sync held back until the transfer is over
-- (`transfer.rs`). No row: an ordinary link, nothing is held.
CREATE TABLE history_transfer(
  id INTEGER PRIMARY KEY CHECK (id = 1),
  -- 'waiting' (for the phone's upload), 'downloading', 'reading' (verifying and reading the
  -- file), and the three ends 'done', 'skipped', 'failed'.
  state TEXT NOT NULL,
  -- Why it was skipped ('phone_declined', 'user', 'deadline', 'not_found') or failed
  -- ('link_again', 'update_app', 'corrupt', 'too_large', 'download', 'internal'); log-safe.
  reason TEXT,
  -- The ephemeral backup key (32 bytes): a single-use secret that opens the archive. NULL
  -- from the moment the transfer is over.
  key BLOB,
  -- Where the archive is (`GET /v1/devices/transfer_archive`): the CDN number and the object
  -- key. Set from 'downloading' on, NULL once the transfer is over.
  cdn INTEGER,
  cdn_key TEXT,
  -- The archive's file name inside the account's `transfer` directory; the bytes that are
  -- in the file are the offset a download goes on from. NULL once the transfer is over.
  file TEXT,
  -- Bytes downloaded when this was last written (what a UI shows after a restart; the file's
  -- length decides), and the size the CDN stated.
  bytes_done INTEGER NOT NULL DEFAULT 0,
  bytes_total INTEGER,
  -- Until when (ms since the epoch) the phone's upload is waited for: one hour from the link.
  deadline_at INTEGER NOT NULL,
  started_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  -- How many times a read of the archive was started. A read that is cut off (a restart, a
  -- crash) starts over; one that never ends, whatever the reason, must not hold receiving
  -- back at every start for good: after a few starts the transfer fails (`transfer.rs`).
  read_starts INTEGER NOT NULL DEFAULT 0,
  -- How many frames the archive held, once it was read to its end.
  frames INTEGER
) STRICT;

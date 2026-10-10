-- Signal backend.db: bounds of the history transfer (docs/architecture.md §14.2 item 14).

-- How long the download of the archive has run so far (ms), over every run of the
-- application: zero at the write that stores the archive's place, then written with the
-- download's progress. The download as a whole has a deadline in this time, and its average
-- rate is judged over it, so a restart does not start either anew, and the time the
-- application was closed does not count (`transfer.rs`).
ALTER TABLE history_transfer ADD COLUMN download_ran_ms INTEGER NOT NULL DEFAULT 0;

-- 1 while a read of the archive is open: set when a read starts, cleared when a read is
-- stopped in an orderly way (the account stops or the application quits, and the importer was
-- told to drop what it had). A start that finds it set knows the read before it ended with
-- the process (a crash, a kill), and only such a start counts in `read_starts`.
ALTER TABLE history_transfer ADD COLUMN read_open INTEGER NOT NULL DEFAULT 0;

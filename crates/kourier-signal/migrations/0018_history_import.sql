-- Signal backend.db: the import of a history transfer's archive, up to its commit
-- (docs/architecture.md §14.2 item 14; `transfer.rs`, "The import").

-- While the archive is read, its frames are mapped into a private, encrypted file of the
-- transfer directory, the stage (`transfer/stage.rs`). Nothing of the archive is in this
-- database before the reader has checked the archive to its last byte. The write that sets
-- `import_staged = 1` is the commit: it says that the stage in the transfer directory is
-- whole, came from an authentic archive, and holds what the three totals say. A start that
-- finds a stage file without this mark removes the file and reads the archive again.
-- 0 again from the write that ends the transfer (the stage goes with the directory).
ALTER TABLE history_transfer ADD COLUMN import_staged INTEGER NOT NULL DEFAULT 0;

-- What the recorded stage holds: messages (rows of every staged chat) and staged chats. A
-- stage is used only when its own totals equal these. Counts, no secret.
ALTER TABLE history_transfer ADD COLUMN stage_messages INTEGER NOT NULL DEFAULT 0;
ALTER TABLE history_transfer ADD COLUMN stage_chats INTEGER NOT NULL DEFAULT 0;

-- The frames the archive held (what the end of the transfer writes into `frames`): stored
-- with the commit, because a start that finds the stage recorded does not read the archive
-- again and still has to say how many frames it had.
ALTER TABLE history_transfer ADD COLUMN stage_frames INTEGER NOT NULL DEFAULT 0;

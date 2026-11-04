-- The history stage of one WhatsApp account (W4 C3), its own file `history/stage.db` and its own schema
-- component `whatsapp-history-stage`. FROZEN once it ran on a kept profile: a change is a new number,
-- never an edit of this file, and a stage is MIGRATED, never removed, for a version (D5).
--
-- The stage is the ONLY copy of a captured chunk: the phone was told the chunk arrived the moment the
-- capture committed, and upstream keeps no replay buffer. So a newer build that needs another layout
-- adds a numbered step here; a file this build cannot read is left alone and reported.

-- Who the stage belongs to: the account's own id, set when the file is made. A stage of another owner
-- is replaced and its chunks that had not been delivered are counted lost, never silently dropped.
-- `incarnation` (W4 fix HIGH-1) is 16 random bytes made WITH THE FILE (SQLite's `randomblob`): backend.db's
-- books record which incarnation they point into, and any open of a file with another id (a replaced stage,
-- a file that vanished and was made again, a stage a capture made over books that are not empty) starts the
-- books over before anything is delivered, whoever opened the file and however the run ended.
CREATE TABLE meta (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    owner       TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    incarnation BLOB NOT NULL CHECK (length(incarnation) = 16)
);

-- One row per announced chunk that reached the capture: the message id of its notice is the key, so a
-- redelivery of the same notice is one chunk (`INSERT ... ON CONFLICT DO NOTHING`). The zlib
-- `HistorySync` exactly as the phone uploaded it is NOT in this row but in `chunk_bytes` (W4 fix
-- MEDIUM-1): a 1-20 MB blob stored beside the small columns would be walked, page by page and decrypted,
-- by every look at a chunk's state, and rewritten by every state change. `sync_type`, `chunk_order` and
-- `progress` are the chunk's own: the capture knows the type, the planning pass reads the other two
-- from the blob.
--   captured  the bytes are durable, nothing of the chunk was read yet
--   planned   the first pass read the chunk and wrote its `plan`
--   bad       the blob did not inflate or decode: kept 14 days (counted from `bad_at`) for a later build, then
--             forgotten; never delivered
--   done      every row of the chunk settled; the bytes and the plan are gone and the row stays, so a second notice
--             of the same chunk (its message id) is still one chunk
CREATE TABLE chunks (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    msg_id      TEXT NOT NULL UNIQUE,
    sync_type   INTEGER,
    chunk_order INTEGER,
    progress    INTEGER,
    -- Chats of the chunk that did not decode (the first pass skips them and counts them here).
    skipped     INTEGER NOT NULL DEFAULT 0,
    -- When the chunk was set aside as `bad` (wall clock, ms); NULL for every other state.
    bad_at      INTEGER,
    captured_at INTEGER NOT NULL,
    size        INTEGER NOT NULL,
    state       TEXT NOT NULL CHECK (state IN ('captured', 'planned', 'bad', 'done'))
);

-- The bytes of a chunk, written in the zone of its capture and deleted when every row of the chunk settled
-- (`done`; a `bad` chunk keeps its bytes for 14 days). A chunk row without a bytes row is a chunk that
-- reads as `bad` (missing).
CREATE TABLE chunk_bytes (
    seq   INTEGER PRIMARY KEY REFERENCES chunks (seq) ON DELETE CASCADE,
    bytes BLOB NOT NULL
);

-- The first pass over a chunk: its chats in the ORDER OF DELIVERY (`n`: 0 is the most recent chat; equal
-- times by `conv`, then `pos`), with where each is in the chunk's stream (`pos`, counting an entry that did
-- not decode), the id it is placed by, the time of its last message (seconds) and the number of messages
-- it holds. The second pass delivers by it, and a kill between the passes finds the same plan (the
-- chunk's bytes are the same, so is the plan).
CREATE TABLE plan (
    chunk INTEGER NOT NULL REFERENCES chunks (seq) ON DELETE CASCADE,
    n     INTEGER NOT NULL,
    pos   INTEGER NOT NULL,
    conv  TEXT NOT NULL,
    ts    INTEGER NOT NULL,
    count INTEGER NOT NULL,
    PRIMARY KEY (chunk, n)
) WITHOUT ROWID;

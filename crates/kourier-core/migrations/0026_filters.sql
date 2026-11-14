-- RegExp filters (saved patterns that notify and highlight; notes/kourier/regexp-filters.md).
--
-- One row per filter, in the user's order (`position`, unique and dense from 0). The patterns
-- can name people, so they live here (SQLCipher) and never in kourier.toml. NULL = the field is
-- not used; the core refuses a row with no field. Every switch is its own INTEGER 0/1 column,
-- so a later consumer adds one more `... NOT NULL DEFAULT 0` column and nothing else changes.
-- `colour` is 0xRRGGBB. `created` and `updated` are milliseconds on the core's clock.
CREATE TABLE filters (
    id             INTEGER PRIMARY KEY,
    position       INTEGER NOT NULL UNIQUE,
    name           TEXT NOT NULL,
    text_pattern   TEXT,
    sender_pattern TEXT,
    chat_pattern   TEXT,
    match_case     INTEGER NOT NULL DEFAULT 0,
    notify         INTEGER NOT NULL DEFAULT 0,
    notify_muted   INTEGER NOT NULL DEFAULT 0,
    highlight      INTEGER NOT NULL DEFAULT 0,
    colour         INTEGER NOT NULL,
    created        INTEGER NOT NULL,
    updated        INTEGER NOT NULL
);

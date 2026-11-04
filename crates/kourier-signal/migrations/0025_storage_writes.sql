-- Signal backend.db, A10 (docs/architecture.md §5.5, the Storage Service WRITE engine): everything A10
-- needs in this file, so that A10b (the engine, offline), A10c (the wire and the switch) and A10d
-- (the UI) add no migration. Nothing here writes to the Storage Service by itself: with no intent
-- stored, no write in flight and the switch off (the default, for EVERY account, old or new) the
-- backend behaves as before.

-- The per-account switch "Sync chat changes with my phone". 0 = off for every account, existing
-- and new (his answer 2026-10-28): nothing roaming is written until he turns it on himself
-- (`Command::SetRoamingWrites`, the only writer).
ALTER TABLE account ADD COLUMN roaming_writes INTEGER NOT NULL DEFAULT 0
  CHECK (roaming_writes IN (0, 1));
-- Bumped when the storage key changes or the stored records are reset (`storage::reset`): a write
-- that planned against an older generation drops its result at the landing zone (critic M4).
ALTER TABLE account ADD COLUMN storage_generation INTEGER NOT NULL DEFAULT 0;
-- The source of `storage_intents.seq`: bumped in the intent's own zone, so a seq is never reused
-- after intents were deleted (a WITHOUT ROWID table has no autoincrement; critic Y5).
ALTER TABLE account ADD COLUMN storage_intent_seq INTEGER NOT NULL DEFAULT 0;

-- What a stored record belongs to: `contact:<service id string>`, `group:<master key, lower-case
-- hex>` or `account`, and '' (empty) for a record nobody can ask to change (a kind the engine never
-- edits, a contact record that names nobody or only ourselves, a group record without a 32-byte
-- master key). Written by every merge zone; a row from before this migration has NULL until
-- `storage::fill_targets` decodes it, once, offline (NULL = not filled yet, '' = filled, no target).
ALTER TABLE storage_records ADD COLUMN target TEXT;
CREATE INDEX storage_records_target ON storage_records(target) WHERE target IS NOT NULL;
-- 1 = a record OUR write put there (review N6): stored by the 200 zone (`write::store_written`)
-- and by a merge of a key of the write the sync just landed (its `new_keys`); every other record
-- is the phone's (a phone rotation gives a new key: 0). The automatic clear's rule N-C does not
-- count such a record as the phone's newer mark, and the merge of it states no flag (the landing
-- already stated what it should: review L1).
ALTER TABLE storage_records ADD COLUMN ours INTEGER NOT NULL DEFAULT 0 CHECK (ours IN (0, 1));

-- A change made here that the phone has not been told yet (one row per target and field; the newest
-- click wins). `field`: archived | marked_unread | muted_until | identity | pin:<target of the pinned
-- chat> (the pin intents' own `target` is 'account'). `value`: the wanted value (0/1; the mute end in
-- ms, i64::MAX = always; NULL = unmuted; for `identity` the wanted state). `base`: the phone's value
-- when the intent was made: the newest stored record's, for `archived` the base the core was told
-- (A1 and X1 move it: review H2), for `identity` the record's state of the SAME key, else the state
-- the key had here before the click (reviews M1, N3); the value a write in flight sets when it
-- carries the field (X3); moved to the written value after a landing, back to the write's
-- `prior` when it did not land (review N1); `shown`: what
-- the core showed before the first click of the field (a click back deletes the intent while no write
-- carries it; a write that did not land puts `shown` back to `prior` too, review R2-1). `key`: an `identity` intent's identity key (NULL for the others); it is written only
-- onto a record that holds the same key (critic M2). `seq`: from `account.storage_intent_seq`.
-- `automatic` = 1: the clear of "marked unread" made by reading the chat, not a click.
CREATE TABLE storage_intents(
  target TEXT NOT NULL,
  field TEXT NOT NULL,
  value INTEGER,
  base INTEGER,
  shown INTEGER,
  key BLOB,
  seq INTEGER NOT NULL,
  automatic INTEGER NOT NULL DEFAULT 0 CHECK (automatic IN (0, 1)),
  created_at INTEGER NOT NULL,
  PRIMARY KEY(target, field)
) STRICT, WITHOUT ROWID;

-- The write that was planned and may have reached the server: at most one row. It decides after a
-- kill (the landing rule, `fetched.version` against `version` and the ids in `new_keys`) and carries
-- what the landing zone must close, because the plaintext of the records is NOT kept (critic X4):
-- `planned` is JSON, one object per (target, field) the write carries, `{"target": .., "field": ..,
-- "value": <the written value, null = unmuted>, "seq": <the carried seq>}`, plus when set
-- `"rides": true` (X1's told `archived` written along, `seq` 0: it closes nothing, review H1),
-- `"overtaken": true` (a message brought the chat back after this `archived` entry was planned,
-- whatever it sets: a landing with the switch off does not state it, review L1, R2-2 (1)) and `"prior": <the phone's value
-- before the write>` (review N1); the landing zone deletes exactly the intents whose seq still
-- equals the carried one (never for a ride-along) and moves `base` to the written value for the
-- rest (X3, Y5); a write that did not land moves those bases (and `shown`) back to `prior`. This row has no `seq`
-- column of its own: `planned` carries the seqs.
-- `version`: the manifest version the write sets (remote + 1). `new_keys`: the new raw storage ids,
-- 16 bytes each, back to back. `generation`: `account.storage_generation` when it was planned.
CREATE TABLE storage_write_inflight(
  id INTEGER PRIMARY KEY CHECK (id = 1),
  version INTEGER NOT NULL,
  new_keys BLOB NOT NULL,
  planned TEXT NOT NULL CHECK (json_valid(planned)),
  generation INTEGER NOT NULL,
  planned_at INTEGER NOT NULL
) STRICT;

-- When the writes that reached the server were made (ms, one row per PUT): the rate cap (at most 6
-- per 10 minutes and 30 per hour per account, BQ4) survives a restart. Rows older than an hour are
-- deleted by the job that counts them.
CREATE TABLE storage_write_times(
  at INTEGER NOT NULL
) STRICT;

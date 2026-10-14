-- Signal backend.db, link feedback L2 (docs/architecture.md §3.3, §6.13): which ACI a phone
-- number belongs to, as the Storage Service told this device. The core is told
-- `IdentityMerged{number → aci}` when a pair is new or the number moved to another ACI
-- (`links::learn_number`), never again for a record that only came back (a record removed and
-- added, `storage::reset`); a rebuilt core.db is told every pair again (`links::all`).
-- Each merge costs the core a write of what the number's user owns.
CREATE TABLE number_links(
  e164 TEXT PRIMARY KEY,                  -- the number, E.164
  aci TEXT NOT NULL,                      -- service id string of the ACI
  learned_at INTEGER NOT NULL             -- ms since the epoch
) STRICT, WITHOUT ROWID;

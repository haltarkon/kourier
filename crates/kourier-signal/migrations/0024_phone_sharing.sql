-- People pages P1 (docs/architecture.md §6.10, "who may see a number"): whether a person shares
-- their phone number, as their profile said at its last fetch (`phoneNumberSharing`, a profile
-- field the person encrypts for those who hold their profile key). NULL = the profile was never
-- fetched, which counts as "not sharing" (Desktop: `sharingPhoneNumber` undefined). A fetch
-- writes 1 = shares (the decrypted field is exactly one byte 0x01) or 0 = does not (any other
-- value, or no field at all). Signal's rule for showing a number
-- (`getE164`, `peers::number_shown`): shared, or a system contact, or no profile key at all.
-- No index, no row converted: the next fetch of a profile fills it.
ALTER TABLE peers ADD COLUMN phone_sharing INTEGER CHECK (phone_sharing IN (0, 1));

-- A rate limit of a kind of job (a 429 of the profile service) is REMEMBERED: until the hold ends no
-- job of the kind is made due, whoever asks (a person's page, the routine refresh, a new profile
-- key). `until` is a wall-clock time in ms.
CREATE TABLE job_holds(
  kind TEXT PRIMARY KEY,
  until INTEGER NOT NULL
) STRICT;

-- When a profile fetch of this person last ended WITHOUT a profile (a 404, an error); a success
-- clears it (`profile_fetched_at` is the last success). A page open does not try again within an
-- hour of a failed attempt; our own fetch is not retried every hour while it fails.
ALTER TABLE peers ADD COLUMN profile_tried_at INTEGER;

-- Set once the first start on this schema has told the core the number verdict of every person
-- with a number (`maintenance::restate_number_verdicts`): contacts of an older profile have none
-- in core.db 18 (NULL = hidden), and a person with no profile key is never fetched, so nothing
-- else would ever state theirs. A new profile runs the pass once, over little.
ALTER TABLE account ADD COLUMN number_verdicts_stated INTEGER NOT NULL DEFAULT 0
  CHECK (number_verdicts_stated IN (0, 1));

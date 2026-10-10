-- Signal backend.db, after the first run against a real server (docs/architecture.md §6.5
-- step 8).

-- `account.app_version` says under which Kourier version CONTACTS, CONFIGURATION and BLOCKED
-- were asked of the phone; a start under another version, or without one, asks again. Until
-- now it was written when the three requests were queued and stayed when one was dropped: on
-- the first real link the server's unpadded base64 made every prekey bundle unreadable, the
-- three requests were dropped as permanent failures and nothing ever asked again. From this
-- version on a dropped request clears the mark (`maintenance::job_dropped`). A profile made
-- before cannot tell whether its requests went out: ask once more. The phone answers a
-- repeated request with the same sync messages, which are applied like any other.
UPDATE account SET app_version = NULL;

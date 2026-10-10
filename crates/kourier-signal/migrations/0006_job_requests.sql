-- Signal backend.db, step S9 review (docs/architecture.md §5.6, §6.12): a job asked for again
-- while it runs must run again. `jobs::request` (a `FetchLatest`, a new storage key, a new
-- profile key) and a new contact-sync payload bump this counter on a queued row; the runner
-- remembers the value it picked the job with and deletes the row after the run only if nobody
-- asked again meanwhile (otherwise the row stays, due now). Without it such a request that
-- arrived during the run was lost until the next start (or the 3-day profile refresh).
ALTER TABLE jobs ADD COLUMN requests INTEGER NOT NULL DEFAULT 0;

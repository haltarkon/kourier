-- Signal backend.db, before the first run against a real server (docs/architecture.md §6.5
-- step 6, §9.2).

-- The server the account was linked on: 'production', 'staging' or 'local:<host>:<port>'
-- (`net::env::binding`). A start that names another server is refused before anything
-- connects: that server would answer this device's credentials with 401 or 403, which reads
-- as "unlinked" and ends in a wipe of the login. NULL for an account linked before this
-- column existed; no real server had been used then, so it counts as a local test server's.
ALTER TABLE account ADD COLUMN env TEXT;

-- Signal backend.db: the server's delivery time of an envelope (docs/architecture.md §6.6).

-- `x-signal-timestamp` of the request that delivered the envelope (ms, the server's clock):
-- stage 1 threw it away. A call's ring has an age, the time its Offer waited on the server,
-- and Signal measures it on the server's clock at both ends: this delivery time minus the
-- envelope's `server_timestamp` (Desktop's `messageAgeSec`). Stage 2 reads it for call
-- messages (`receive/calls.rs`). NULL: the server stated none, or the row was stored before
-- this version (then the age counts as unknown, see `calls::age_ms`).
ALTER TABLE inbox ADD COLUMN delivered_at INTEGER;

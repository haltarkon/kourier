-- Signal backend.db, step S7 (docs/architecture.md §5.5, §6.6, §6.13): what the receive
-- pipeline learns about other users. One row per remote service id string (`<uuid>` for an ACI);
-- later steps add the access key, sealed-sender state and profile fetch columns (S8, S9).

CREATE TABLE peers(
  service_id TEXT PRIMARY KEY,
  e164 TEXT,                                -- from a sealed-sender certificate (personal data)
  profile_key BLOB,                         -- 32 bytes from their DataMessages
  expire_timer_version INTEGER NOT NULL DEFAULT 0,  -- 1:1 timer version rules (§6.13)
  share_my_number INTEGER NOT NULL DEFAULT 0 CHECK (share_my_number IN (0, 1)),  -- they wrote to our PNI
  updated_at INTEGER NOT NULL
) STRICT;

-- Stage 2 picks rows per lane in arrival order and skips rows parked after a content failure.
CREATE INDEX inbox_parked ON inbox(parked_until) WHERE parked_until IS NOT NULL;

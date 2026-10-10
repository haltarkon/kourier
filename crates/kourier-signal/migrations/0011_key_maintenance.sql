-- Signal backend.db, step S15 (docs/architecture.md §6.12 key maintenance).
--
-- `signed_prekeys.confirmed` and `kyber_prekeys.confirmed` (there since 0001, never set) now mean
-- what they mean in Desktop: the server has the key. The rotation looks at the newest
-- *confirmed* signed prekey and Kyber last-resort key (older than 1.5 days: rotate), so a key
-- whose upload never arrived does not postpone the next one.
--
-- Every signed prekey and last-resort key stored so far went out with the link request, which
-- the server accepted (the account row exists only after it): they are confirmed.
UPDATE signed_prekeys SET confirmed = 1;
UPDATE kyber_prekeys SET confirmed = 1 WHERE last_resort = 1;

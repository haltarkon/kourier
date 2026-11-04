-- P5a (Verify): the quiet column learns the verification notice, `SystemEvent::VerifiedChanged`
-- ("You marked your safety number with ... as verified"), quiet like the safety-number notice
-- (`SystemEvent::is_quiet`; the rule exists twice, see `0015_quiet_notices.sql`).
--
-- SQLite cannot change a generated column's expression, so the column is dropped and added
-- again with the 0015 expression plus ONE prefix: `{"System":{"VerifiedChanged":` is 29
-- characters (`{"System":` 10 + `{"VerifiedChanged":` 19). `quiet` is VIRTUAL and referenced
-- by no index, trigger or view (0015 is its only author): DROP COLUMN of a VIRTUAL column and
-- ADD COLUMN of one rewrite no row. The column now stands after `media_files` (0020): compare
-- dumps by column name, never by position.
--
-- No recount and no hide step (0015 had both): no row of this kind exists before this build,
-- so no conversation's listed newest row changes.
--
-- The 0015 rule holds: a TOTAL prefix rule on the serialised body, never `json_extract` /
-- `json_type`. A LATER MIGRATION THAT REBUILDS `messages` must declare this column again (this
-- expression) and leave it out of the copy.
ALTER TABLE messages DROP COLUMN quiet;
ALTER TABLE messages ADD COLUMN quiet INTEGER GENERATED ALWAYS AS (
  CASE WHEN body_kind = 'System' THEN coalesce(
      body = '{"System":"MessageRequestAccepted"}'
      OR substr(body, 1, 32) = '{"System":{"IdentityKeyChanged":'
      OR substr(body, 1, 30) = '{"System":{"SessionRefreshed":'
      OR substr(body, 1, 27) = '{"System":{"NumberChanged":'
      OR substr(body, 1, 29) = '{"System":{"VerifiedChanged":', 0)
  ELSE 0 END) VIRTUAL;

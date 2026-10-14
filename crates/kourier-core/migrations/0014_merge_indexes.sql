-- A short link tail, step L2 (2026-10-12): a merge of two identities costs what the alias owns.
--
-- The first Storage Service sync after a link tells the core one `IdentityMerged` (a phone
-- number's user into the account's ACI) for every contact with a number, and
-- `ingest::merge::identity_merged` looked for the rows of the number-user with statements
-- that walked ALL of the account's messages about nine times per merge (the partial key index
-- `messages_key_account` cannot serve `account_id = ? AND net_author = ?` without its
-- `net_scope = 0`, and reactions, receipts and send states had no index on the user): ~2 µs
-- per contact and message, minutes in ONE write transaction for a big account (the link page
-- waited for it; `target/link-feedback/tail.md` F2). With these indexes every statement of a
-- merge starts from the alias's own rows; the plan test is
-- `ingest::merge::tests::a_merge_looks_only_at_the_rows_of_the_alias`.
--
-- `expired_messages` (the keys of swept messages, kept 45 days) and `pending_modifiers` (parked
-- until their target arrives) are looked at by author AND by conversation key (`net_author = ?
-- OR conv_key = ?`, and the four UPDATEs that follow the canonical id): the keys they had
-- (`account_id, net_local` and `account_id, target_local`) served neither, so every merge
-- walked all of the account's rows of both (tens of thousands of keys in a busy chat with
-- disappearing messages: seconds per sync, with the writer held). The two of `expired_messages`
-- end in `net_local`: `expiry::remembered`, run for every new message, names the author (or the
-- conversation) AND the local id, and a two-column index ties with `expired_messages_by_key` on
-- two equal terms; the planner took the newer one and walked all of that author's expired keys
-- per message (40 ms a message at 45,000 keys). With the third column the new index is the
-- narrower one (`expiry::tests::a_new_message_looks_up_its_own_expired_key`).
--
-- `identity_alias` is repointed by the user it points at on every merge (aliases of `from` now
-- point at the canonical id): without an index that walked the account's aliases per merge,
-- quadratic over a sync of thousands of numbers.
CREATE INDEX messages_by_author ON messages(account_id, net_author);
CREATE INDEX messages_deleted_by ON messages(account_id, deleted_by) WHERE deleted_by IS NOT NULL;
CREATE INDEX revisions_by_author ON message_revisions(account_id, net_author);
CREATE INDEX reactions_by_user ON reactions(user_id);
CREATE INDEX receipts_by_user ON receipts(user_id);
CREATE INDEX send_state_by_user ON send_state(user_id);
CREATE INDEX expired_messages_by_author ON expired_messages(account_id, net_author, net_local);
CREATE INDEX expired_messages_by_conv ON expired_messages(account_id, conv_key, net_local);
CREATE INDEX pending_by_author ON pending_modifiers(account_id, target_author);
CREATE INDEX pending_by_conv ON pending_modifiers(account_id, conv_key);
CREATE INDEX identity_alias_by_user ON identity_alias(account_id, user_id);

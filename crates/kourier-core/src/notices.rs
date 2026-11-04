//! Quiet notices and the conversation list (2026-10-15).
//!
//! A notice is QUIET when [`kourier_model::SystemEvent::is_quiet`] says so: a safety number
//! that changed, a refreshed session, a changed number, an accepted message request. It is a
//! row of the timeline like any other; what it does not do, unless the user asks for it
//! (Settings > General, scope `core`, key `quiet_notices`, value `show`; NO row = hidden), is
//! stand for its conversation in the list: the preview, the time label and the place in the
//! list (`conversations.last_activity`) come from the newest LISTED row, and the user's words
//! were "[these] messages should have toggle to disable showing in All chat as latest
//! message in chat. It looks like digital garbage."
//!
//! The rule is written twice, in Rust (`SystemEvent::is_quiet`) and as the generated column
//! `messages.quiet` (a TOTAL prefix rule on the serialised body; its newest definition is
//! `migrations/0021_verified_notice.sql`, which redefines the 0015 column: a later
//! redefinition copies the newest one). `tests/quiet_notices.rs` holds them together.
//!
//! The list never leaves a chat out for what it holds (the lead's decision F1, 2026-10-15): a
//! chat that has no listed row is listed as an empty chat, whatever quiet notices it has, so
//! a listed chat (an empty group after a link, a chat he cleared, one whose messages all
//! expired) cannot vanish when a notice lands in it. The chats that WERE all quiet when core.db
//! became schema 15 (the garbage chats quiet notices made before the ingest guard) are hidden
//! once by the migration, in the state of a chat deleted on this device (`conversations.hidden`,
//! no row deleted, nothing queued for any network); a loud message lists such a chat again, a
//! quiet notice never does.

use kourier_db::rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use kourier_model::ConversationId;

use crate::error::Result;

/// SQL: the rows of alias `m` (a row of `messages`) that the list may use for a
/// conversation's preview, time and place: a loud row, or any row when the user asked to
/// see quiet notices.
///
/// The `EXISTS` is uncorrelated, so SQLite looks at the settings table once per statement,
/// one primary-key seek in a table of a few rows. Reading the setting in SQL keeps every
/// caller of `ingest::recount` as it was (nothing to thread through ingest, expiry, outbox,
/// modify or clear) and makes the read consistent with the writer by the snapshot. A macro
/// and not a `const` because the statements that use it are `&'static str` literals for
/// `prepare_cached` (`concat!` takes it).
macro_rules! listed {
    () => {
        "(m.quiet = 0 OR EXISTS (SELECT 1 FROM settings q WHERE q.scope = 'core' \
         AND q.key = 'quiet_notices' AND q.value = 'show'))"
    };
}
pub(crate) use listed;

/// SQL: the sort key of the newest LISTED row of the conversation `$conv` (an expression
/// as a literal: `"?1"` in [`crate::ingest::RECOUNT`], `"conversations.id"` in the flip
/// [`set`] runs): the one rule of what stands for a conversation in the list, written once.
/// Along `messages_timeline` from the newest row down; it stops at the first row that
/// passes, so a quiet notice or two at the top costs a step each.
macro_rules! newest_listed {
    ($conv:literal) => {
        concat!(
            "(SELECT max(m.sort_key) FROM messages m WHERE m.conversation_id = ",
            $conv,
            " AND (m.deleted IS NULL OR m.deleted <> 'ForMe') AND m.expiry_hidden = 0 AND ",
            $crate::notices::listed!(),
            ")"
        )
    };
}
pub(crate) use newest_listed;

/// The `settings` row of the option: one for the whole core (`scope` is not an account's).
const SCOPE: &str = "core";
const KEY: &str = "quiet_notices";
/// The value that shows quiet notices; no row, and any other value, hides them (the default).
const SHOW: &str = "show";
const HIDE: &str = "hide";

/// Whether the user asked to see quiet notices in the list (no row: no, the default). The
/// same look as `listed!`'s, for the front-end's question and for a start.
pub(crate) fn shown(conn: &Connection) -> Result<bool> {
    let value: Option<String> = conn
        .prepare_cached("SELECT value FROM settings WHERE scope = ?1 AND key = ?2")?
        .query_row((SCOPE, KEY), |r| r.get(0))
        .optional()?;
    Ok(value.as_deref() == Some(SHOW))
}

/// The conversations whose sort time the new setting changes, set in the same statement:
/// every conversation whose stored `last_activity` is not the newest LISTED row's now (the
/// setting row is already written in the transaction, so `listed!` reads the new value).
/// `IS NOT` so that a NULL (no listed row) is compared as a value. Two seeks along
/// `messages_timeline` per conversation; `core.db` has no planner statistics, the plan is
/// pinned by `tests`.
const FLIP: &str = concat!(
    "UPDATE conversations SET last_activity = ",
    newest_listed!("conversations.id"),
    " WHERE last_activity IS NOT ",
    newest_listed!("conversations.id"),
    " RETURNING id, hidden, account_id"
);

/// `UiCommand::SetQuietNotices`: stores the choice and, in the same transaction, gives every
/// conversation the sort time its newest listed row says now, so that no read behind the
/// command sees the old order. Returns the conversations whose sort time changed AND that the
/// list shows (not hidden by a delete, not of an account being removed): those are announced
/// in `ConversationsChanged`. Their PREVIEW can change without a change of the sort time
/// (two rows tie, and a pinned chat's order does not move): a front-end reads the whole list
/// again after the command.
pub(crate) fn set(conn: &mut Connection, shown: bool) -> Result<Vec<ConversationId>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.prepare_cached(
        "INSERT INTO settings(scope, key, value) VALUES (?1, ?2, ?3)
         ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
    )?
    .execute((SCOPE, KEY, if shown { SHOW } else { HIDE }))?;
    let moved = tx
        .prepare_cached(FLIP)?
        .query_map([], |r| {
            Ok((
                ConversationId(r.get(0)?),
                r.get::<_, bool>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut changed = Vec::new();
    for (id, hidden, account) in moved {
        if hidden {
            continue;
        }
        let removing: bool = tx
            .prepare_cached("SELECT state = 'Removing' FROM accounts WHERE id = ?1")?
            .query_row([account], |r| r.get(0))
            .optional()?
            .unwrap_or(false);
        if !removing {
            changed.push(id);
        }
    }
    tx.commit()?;
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use kourier_model::{MsgIdScope, NetworkKind, UserId};

    /// The detail lines of `EXPLAIN QUERY PLAN` of `sql` on a migrated `core.db` (the real
    /// SQLCipher build; `core.db` has no planner statistics).
    fn plan(sql: &str) -> Vec<String> {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = crate::testing::IngestHarness::open(
            &dir.path().join("core.db"),
            &kourier_db::DbKey::from([4u8; 32]),
            NetworkKind::Fake,
            MsgIdScope::AccountAuthor,
            &[UserId::parse("aci:me").expect("user id")],
        )
        .expect("harness");
        h.connection()
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("prepare")
            .query_map([], |r| r.get::<_, String>(3))
            .expect("plan")
            .collect::<kourier_db::rusqlite::Result<_>>()
            .expect("rows")
    }

    /// What the newest-listed-row walks have in common: along the chat's timeline index
    /// (`messages_timeline`), the setting looked up by its primary key (once, not per row),
    /// no walk through a table and no sort.
    fn assert_walks_the_timeline(what: &str, steps: &[String]) {
        assert!(
            steps
                .iter()
                .any(|s| s.contains("USING INDEX messages_timeline (conversation_id=?)")),
            "{what} goes along the chat's timeline index: {steps:#?}"
        );
        assert!(
            steps
                .iter()
                .any(|s| s.contains("sqlite_autoindex_settings_1")
                    && s.contains("scope=?")
                    && s.contains("key=?")),
            "{what} reads the setting by its primary key: {steps:#?}"
        );
        assert!(
            steps
                .iter()
                .all(|s| !s.contains("SCAN") && !s.contains("TEMP B-TREE")),
            "{what} walks no table and sorts nothing: {steps:#?}"
        );
    }

    /// The sort time of a conversation (`ingest::recount`, run for every touched chat of
    /// every transaction) and its preview (run for every row of the list page, which is the
    /// start path) both take the newest LISTED row: a quiet notice at the top is stepped
    /// over along the timeline index, never found by a walk through the chat.
    #[test]
    fn the_newest_listed_row_goes_along_the_timeline_index() {
        let steps = plan(&crate::ingest::RECOUNT.replace("?1", "1"));
        // (The two counts above it use the partial index of the unread rows.)
        assert_walks_the_timeline("recount", &steps);
        let steps = plan(&crate::query::PREVIEW.replace("?1", "1"));
        assert_walks_the_timeline("the preview", &steps);
    }

    /// The list statement never looks into a chat's rows to decide whether to list it (the
    /// start path; decision F1, 2026-10-15): no look at `messages` at all, and no SEARCH of
    /// another table than the page's own index (the preview of each row is a separate
    /// statement, `PREVIEW`).
    #[test]
    fn the_list_statement_does_not_look_into_messages() {
        // The inbox (`?2 = 0`), the archive (1) and both (NULL; A1) read the same way.
        for archived in ["0", "1", "NULL"] {
            let mut sql = crate::query::list_sql();
            for (param, value) in [
                ("?1", "NULL"),
                ("?2", archived),
                ("?3", "0"),
                ("?4", "0"),
                ("?5", "0"),
                ("?6", "0"),
                ("?7", "0"),
                ("?8", "10"),
            ] {
                sql = sql.replace(param, value);
            }
            let steps = plan(&sql);
            assert!(
                steps.iter().all(|s| !s.contains("messages")),
                "the list (archived {archived}) decides without a look at the chat's rows: {steps:#?}"
            );
            assert!(
                steps
                    .iter()
                    .any(|s| s.starts_with("SCAN c") || s.starts_with("SEARCH c")),
                "(the plan is of the conversations table, archived {archived}): {steps:#?}"
            );
        }
    }

    /// The flip that `set` runs once per change of the setting reads each conversation's
    /// newest listed row along the timeline index (twice: the new value and the test whether
    /// it changed), the only scan being the conversations themselves.
    #[test]
    fn the_flip_looks_for_the_newest_listed_row_along_the_timeline_index() {
        let steps = plan(super::FLIP);
        assert!(
            steps
                .iter()
                .any(|s| s.contains("SEARCH m USING INDEX messages_timeline (conversation_id=?)")),
            "the flip goes along the chat's timeline index: {steps:#?}"
        );
        assert!(
            steps
                .iter()
                .all(|s| !s.contains("SCAN m") && !s.contains("SCAN messages")),
            "and walks no table of messages: {steps:#?}"
        );
        assert!(
            steps.iter().all(|s| !s.contains("TEMP B-TREE")),
            "and sorts nothing: {steps:#?}"
        );
    }
}

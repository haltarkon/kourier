//! Core-local writes outside ingest: account rows (registry, §4.3) and reading a conversation
//! (durable read sync and receipts, §4.8).

use kourier_backend_api::DurableCommand;
use kourier_db::rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use kourier_model::{
    AccountId, AccountState, AccountUuid, CmdId, ConvKey, ConversationId, MessageId, MsgTarget,
    NetMsgId, NetworkKind, ReceiptKind,
};

use crate::error::{CoreError, Result};
use crate::sql;

/// An account row as the registry sees it.
#[derive(Clone, Debug)]
pub(crate) struct AccountEntry {
    pub id: AccountId,
    pub uuid: AccountUuid,
    pub network: NetworkKind,
    pub state: AccountState,
}

/// Every account row.
pub(crate) fn accounts(conn: &Connection) -> Result<Vec<AccountEntry>> {
    let rows: Vec<(i64, String, String, String, Option<String>)> = conn
        .prepare_cached("SELECT id, uuid, network, state, state_reason FROM accounts ORDER BY id")?
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(|(id, uuid, network, state, reason)| {
            Ok(AccountEntry {
                id: AccountId(id),
                uuid: uuid
                    .parse()
                    .map_err(|_| CoreError::Json("malformed stored uuid".into()))?,
                network: sql::network(&network)?,
                state: sql::account_state(&state, reason)?,
            })
        })
        .collect()
}

/// Inserts an account row.
pub(crate) fn insert_account(
    conn: &Connection,
    uuid: AccountUuid,
    network: NetworkKind,
    state: &AccountState,
    now: i64,
) -> Result<AccountId> {
    let (name, reason) = sql::account_state_name(state);
    conn.prepare_cached(
        "INSERT INTO accounts(uuid, network, state, state_reason, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?
    .execute((uuid.to_string(), network.as_str(), name, reason, now))?;
    Ok(AccountId(conn.last_insert_rowid()))
}

/// Sets an account's lifecycle state.
pub(crate) fn set_state(conn: &Connection, id: AccountId, state: &AccountState) -> Result<()> {
    let (name, reason) = sql::account_state_name(state);
    conn.prepare_cached("UPDATE accounts SET state = ?2, state_reason = ?3 WHERE id = ?1")?
        .execute((id.0, name, reason))?;
    Ok(())
}

/// The unlink bookkeeping (§4.3 step 5): `LoggedOut`, and durable commands can no longer go out.
pub(crate) fn logged_out(conn: &mut Connection, id: AccountId, reason: &str) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    set_state(
        &tx,
        id,
        &AccountState::LoggedOut {
            reason: reason.to_owned(),
        },
    )?;
    tx.prepare_cached("DELETE FROM pending_commands WHERE account_id = ?1")?
        .execute([id.0])?;
    tx.commit()?;
    Ok(())
}

/// Deletes an account row (cascading to every core row of the account) and truncates the WAL so
/// deleted rows leave it too (§4.3 step 7, §5.2).
pub(crate) fn delete_account(conn: &Connection, id: AccountId) -> Result<()> {
    conn.prepare_cached("DELETE FROM accounts WHERE id = ?1")?
        .execute([id.0])?;
    // Account-scoped bookkeeping without a foreign key (the outbox id allocator).
    conn.prepare_cached("DELETE FROM settings WHERE scope = ?1")?
        .execute([crate::outbox::alloc_scope(id)])?;
    kourier_db::checkpoint_truncate(conn)?;
    Ok(())
}

const CLOCK_WRONG_KEY: &str = "clock_wrong";

/// Records (or clears) that `account`'s backend found the local clock wrong (§9.6): the
/// disappearing sweeper leaves the account alone until a server accepted the clock again,
/// also across a restart.
pub(crate) fn set_clock_wrong(conn: &Connection, account: AccountId, wrong: bool) -> Result<()> {
    let scope = crate::outbox::alloc_scope(account);
    if wrong {
        conn.prepare_cached(
            "INSERT INTO settings(scope, key, value) VALUES (?1, ?2, '1')
             ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
        )?
        .execute((scope, CLOCK_WRONG_KEY))?;
    } else {
        conn.prepare_cached("DELETE FROM settings WHERE scope = ?1 AND key = ?2")?
            .execute((scope, CLOCK_WRONG_KEY))?;
    }
    Ok(())
}

/// The accounts whose clock was found wrong and not accepted since ([`set_clock_wrong`]).
pub(crate) fn clock_wrong_accounts(
    conn: &Connection,
) -> Result<std::collections::BTreeSet<AccountId>> {
    let scopes: Vec<String> = conn
        .prepare_cached("SELECT scope FROM settings WHERE key = ?1")?
        .query_map([CLOCK_WRONG_KEY], |r| r.get(0))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    Ok(scopes
        .iter()
        .filter_map(|s| s.strip_prefix("account:")?.parse().ok().map(AccountId))
        .collect())
}

/// The account's own ids (for the relink identity check).
pub(crate) fn own_ids(conn: &Connection, id: AccountId) -> Result<Vec<kourier_model::UserId>> {
    let own: Option<String> = conn
        .prepare_cached("SELECT own_ids FROM accounts WHERE id = ?1")?
        .query_row([id.0], |r| r.get(0))
        .optional()?;
    own.map_or(Ok(Vec::new()), |s| sql::from_json(&s))
}

/// Durable commands still waiting for `CommandPersisted` (resent at every start, §4.8).
pub(crate) fn pending_commands(
    conn: &Connection,
    id: AccountId,
) -> Result<Vec<(CmdId, DurableCommand)>> {
    let rows: Vec<(i64, String)> = conn
        .prepare_cached(
            "SELECT id, command FROM pending_commands WHERE account_id = ?1 ORDER BY id",
        )?
        .query_map([id.0], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(|(id, json)| Ok((CmdId(id), sql::from_json(&json)?)))
        .collect()
}

/// The account's Kourier-local preferences.
pub(crate) fn local_prefs(
    conn: &Connection,
    id: AccountId,
) -> Result<kourier_model::AccountLocalPrefs> {
    let s: Option<String> = conn
        .prepare_cached("SELECT local_prefs FROM accounts WHERE id = ?1")?
        .query_row([id.0], |r| r.get(0))
        .optional()?;
    s.map_or(Ok(Default::default()), |s| sql::from_json(&s))
}

/// What reading a conversation changed and which durable commands it queued.
#[derive(Debug)]
pub(crate) struct MarkedRead {
    pub account: AccountId,
    pub conv: ConversationId,
    pub messages: Vec<MessageId>,
    pub commands: Vec<(CmdId, DurableCommand)>,
    /// The conversation was marked unread by hand (on another device) and is not any more:
    /// the network is told with `ConversationChange::SetMarkedUnread`.
    pub mark_cleared: Option<ConvKey>,
}

/// The unread rows of a conversation, oldest first: what a read of the chat marks. Through
/// the partial index of the unread rows (a read costs what is unread, not the chat's length;
/// the plan is held by `ingest::tests`).
pub(crate) const UNREAD_ROWS: &str =
    "SELECT id, net_author, net_local, history FROM messages INDEXED BY messages_unread
             WHERE conversation_id = ?1 AND read_state = 'Unread' AND deleted IS NULL
               AND expiry_hidden = 0
             ORDER BY sort_key, id";

/// Marks every unread message of `conv` read, clears the history unread hint and the "marked
/// unread" flag another device set and, in the same transaction, queues the read sync (and
/// read receipts when enabled) as `pending_commands` (§4.8). Read receipts never go to a
/// blocked conversation or sender, nor into a message request that was not accepted.
/// Returns `None` when nothing was unread and the conversation was not marked unread.
///
/// Imported rows (`history`): an unread message of a history batch that is read here is told
/// to OUR other devices like a live one (it is in the read sync's list), or the phone would
/// keep showing as unread what was read here. It is NOT told to its sender: no read receipt
/// goes out for a message this device never received, also with read receipts on (the
/// sender would get a receipt, dated today, for a message of months ago).
///
/// `up_to` of the read sync is the newest row of the chat that is a MESSAGE: a row somebody
/// else wrote that is no system notice (a call line has no author). A notice has no id a
/// network could name: a backend that got one as `up_to` could tell nobody (Signal's ids
/// are sent times; the backend drops a target whose id is not a number).
///
/// A conversation that was only marked unread has no message to tell anybody about: its
/// flag is cleared (Signal Desktop does the same when a chat is read) and nothing is queued.
///
/// A message hidden because its time has come on an unconfirmed clock ([`crate::expiry`]) is
/// not read by this: the user cannot see it, so nobody is told they read it. If it shows
/// again it is unread, as it was.
pub(crate) fn mark_read(
    conn: &mut Connection,
    conv: ConversationId,
    now: i64,
) -> Result<Option<MarkedRead>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let row: Option<(i64, String, i64, bool, bool, bool)> = tx
        .prepare_cached(
            "SELECT account_id, conv_key, unread_hint, blocked, request_pending, marked_unread
             FROM conversations WHERE id = ?1",
        )?
        .query_row([conv.0], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .optional()?;
    let Some((account, key, hint, blocked, request_pending, marked)) = row else {
        return Err(CoreError::UnknownConversation(conv));
    };
    // Nobody we blocked, and nobody whose message request we did not accept, learns that we
    // read their message (Signal Desktop sends receipts per sender and skips blocked and
    // unaccepted ones, `SD/ts/jobs/helpers/shouldSendToConversation.preload.ts`). Our own
    // devices are told all the same.
    let blocked_authors: std::collections::BTreeSet<String> = tx
        .prepare_cached("SELECT user_id FROM contacts WHERE account_id = ?1 AND blocked = 1")?
        .query_map([account], |r| r.get(0))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let no_receipts = blocked || request_pending;
    let account = AccountId(account);
    let key = sql::conv_key(&key)?;
    let unread: Vec<(i64, String, String, bool)> = tx
        .prepare_cached(UNREAD_ROWS)?
        .query_map([conv.0], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    if unread.is_empty() && hint == 0 && !marked {
        return Ok(None);
    }
    // Something of the chat itself was unread: only then is there a read state to sync. A
    // row without an author is a call line ([`crate::ingest`] `call`): it is read here and
    // nobody is told, neither its caller (no receipt) nor our other devices (no read sync).
    let had_unread = unread.iter().any(|(_, author, _, _)| !author.is_empty()) || hint != 0;
    // How far the chat is read here (`read_up_to`): the newest row somebody else wrote,
    // a notice included.
    let read_up_to: Option<i64> = tx
        .prepare_cached(
            "SELECT sort_key FROM messages
             WHERE conversation_id = ?1 AND net_author <> '' AND (deleted IS NULL OR deleted <> 'ForMe')
               AND expiry_hidden = 0
             ORDER BY sort_key DESC, id DESC LIMIT 1",
        )?
        .query_row([conv.0], |r| r.get(0))
        .optional()?;
    // What our other devices are told (`up_to`): the newest of them that is a message.
    let newest: Option<(String, String)> = tx
        .prepare_cached(
            "SELECT net_author, net_local FROM messages
             WHERE conversation_id = ?1 AND net_author <> '' AND body_kind <> 'System'
               AND (deleted IS NULL OR deleted <> 'ForMe') AND expiry_hidden = 0
             ORDER BY sort_key DESC, id DESC LIMIT 1",
        )?
        .query_row([conv.0], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let mut ids = Vec::with_capacity(unread.len());
    let mut targets = Vec::new();
    let mut receipts = Vec::new();
    for (id, author, local, history) in &unread {
        // Reading starts an `AfterRead` disappearing timer (the sweeper is woken after commit).
        tx.prepare_cached(
            "UPDATE messages SET read_state = 'Read',
               expiry_started_at = CASE WHEN expiry_kind = 'AfterRead'
                 AND expiry_timer_ms IS NOT NULL AND expiry_started_at IS NULL
                 THEN ?2 ELSE expiry_started_at END,
               expires_at = CASE WHEN expiry_kind = 'AfterRead'
                 AND expiry_timer_ms IS NOT NULL AND expiry_started_at IS NULL
                 THEN ?2 + expiry_timer_ms ELSE expires_at END
             WHERE id = ?1",
        )?
        .execute((id, now))?;
        ids.push(MessageId(*id));
        if !author.is_empty() {
            let id = NetMsgId::remote(sql::user(author)?, local.as_str());
            let target = MsgTarget::to_id(Some(key.clone()), &id);
            // The sender of an imported message gets no receipt for it (see above).
            if !history && !no_receipts && !blocked_authors.contains(author) {
                receipts.push(target.clone());
            }
            targets.push(target);
        }
    }
    tx.prepare_cached(
        "UPDATE conversations SET unread_hint = 0, unread = 0, mentions = 0, marked_unread = 0,
           read_up_to = max(coalesce(read_up_to, ?2), ?2) WHERE id = ?1",
    )?
    .execute((conv.0, read_up_to.unwrap_or(i64::MIN)))?;

    let info = sql::account_info(&tx, account)?.ok_or(CoreError::UnknownAccount(account))?;
    let mut commands = Vec::new();
    let mut queue = |cmd: DurableCommand| -> Result<()> {
        tx.prepare_cached(
            "INSERT INTO pending_commands(account_id, command, created_at) VALUES (?1, ?2, ?3)",
        )?
        .execute((account.0, sql::to_json(&cmd)?, now))?;
        commands.push((CmdId(tx.last_insert_rowid()), cmd));
        Ok(())
    };
    // A logged-out account has no device to tell; its read state stays local.
    let active = info.state == AccountState::Active;
    if let Some((author, local)) = newest.as_ref().filter(|_| active && had_unread) {
        let up_to = MsgTarget::to_id(
            Some(key.clone()),
            &NetMsgId::remote(sql::user(author)?, local.as_str()),
        );
        queue(DurableCommand::SyncReadMarker {
            conv: key.clone(),
            up_to,
            read: targets,
        })?;
    }
    if active && info.read_receipts && !receipts.is_empty() {
        queue(DurableCommand::SendReceipts {
            conv: key.clone(),
            targets: receipts,
            kind: ReceiptKind::Read,
        })?;
    }
    tx.commit()?;
    Ok(Some(MarkedRead {
        account,
        conv,
        messages: ids,
        commands,
        mark_cleared: (marked && active).then_some(key),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kourier_backend_api::BackendEvent;
    use kourier_fake::scripts::remote;
    use kourier_model::{
        AccountSettingsPatch, ContactPatch, ConversationPatch, MsgIdScope, NetworkKind, UserId,
    };

    const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
    const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";
    const BOB: &str = "aci:00000000-0000-4000-8000-000000000b0b";
    const CAROL: &str = "aci:00000000-0000-4000-8000-0000000ca501";

    fn uid(s: &str) -> UserId {
        UserId::parse(s).expect("user id")
    }

    /// The receipts and the read sync `mark_read` queued for `key`: the authors that get a
    /// read receipt, and how many messages our own devices are told about.
    fn read(h: &mut crate::testing::IngestHarness, key: &ConvKey) -> (Vec<String>, usize) {
        let conv = sql::conversation_id(h.connection(), h.account(), key)
            .expect("lookup")
            .expect("conversation");
        let marked = mark_read(h.connection_mut(), conv, 2_000)
            .expect("mark read")
            .expect("something was unread");
        let mut receipts = Vec::new();
        let mut synced = 0;
        for (_, cmd) in marked.commands {
            match cmd {
                DurableCommand::SendReceipts { targets, .. } => {
                    for t in targets {
                        if let kourier_model::TargetAuthor::User(u) = t.author {
                            receipts.push(u.as_str().to_owned());
                        }
                    }
                }
                DurableCommand::SyncReadMarker { read, .. } => synced = read.len(),
                other => panic!("reading queues no {other:?}"),
            }
        }
        receipts.sort();
        (receipts, synced)
    }

    /// Phase E leftover, core side: reading a message tells its sender only when we neither
    /// blocked them (the chat or the contact) nor left their message request unanswered. Our
    /// own devices learn every read. (Dropping what blocked senders send, receipts included,
    /// is the backend's envelope filter.)
    #[test]
    fn read_receipts_skip_blocked_senders_and_unaccepted_requests() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = crate::testing::IngestHarness::open(
            &dir.path().join("core.db"),
            &kourier_db::DbKey::from([9u8; 32]),
            NetworkKind::Fake,
            MsgIdScope::AccountAuthor,
            &[uid(ME)],
        )
        .expect("harness");
        let (alice, bob, carol) = (uid(ALICE), uid(BOB), uid(CAROL));
        let d = |u: &UserId| ConvKey::Direct(u.clone());
        let group = ConvKey::Group("gv2:cmVjZWlwdHM".into());
        let mut blocked_chat = ConversationPatch::new(d(&bob));
        blocked_chat.blocked = Some(true);
        let mut blocked_contact = ContactPatch::new(bob.clone());
        blocked_contact.blocked = Some(true);
        let mut request = ConversationPatch::new(d(&carol));
        request.request_pending = Some(true);
        h.apply(vec![
            BackendEvent::AccountSettings(AccountSettingsPatch {
                read_receipts: Some(true),
                ..Default::default()
            }),
            BackendEvent::Message(remote(&d(&alice), &alice, "100", 100, "hi")),
            BackendEvent::Message(remote(&d(&bob), &bob, "200", 200, "let me in")),
            BackendEvent::ConversationUpsert(blocked_chat),
            BackendEvent::ContactUpsert(blocked_contact),
            BackendEvent::Message(remote(&d(&carol), &carol, "300", 300, "hello?")),
            BackendEvent::ConversationUpsert(request),
            BackendEvent::Message(remote(&group, &alice, "400", 400, "to all")),
            BackendEvent::Message(remote(&group, &bob, "500", 500, "me too")),
        ])
        .expect("apply");

        assert_eq!(read(&mut h, &d(&alice)), (vec![ALICE.to_owned()], 1));
        assert_eq!(read(&mut h, &d(&bob)), (Vec::new(), 1), "a blocked chat");
        assert_eq!(read(&mut h, &d(&carol)), (Vec::new(), 1), "not accepted");
        assert_eq!(
            read(&mut h, &group),
            (vec![ALICE.to_owned()], 2),
            "a blocked member of a group is left out, the others are told"
        );
    }

    /// A chat another device marked unread: reading it clears the mark, with or without
    /// unread messages. A chat that was only marked has no message to tell anybody about,
    /// so nothing is queued for it; the network is told about the mark itself.
    #[test]
    fn reading_clears_the_mark_another_device_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = crate::testing::IngestHarness::open(
            &dir.path().join("core.db"),
            &kourier_db::DbKey::from([9u8; 32]),
            NetworkKind::Fake,
            MsgIdScope::AccountAuthor,
            &[uid(ME)],
        )
        .expect("harness");
        let (alice, bob) = (uid(ALICE), uid(BOB));
        let d = |u: &UserId| ConvKey::Direct(u.clone());
        let marked = |key: ConvKey| {
            let mut p = ConversationPatch::new(key);
            p.marked_unread = Some(true);
            BackendEvent::ConversationUpsert(p)
        };
        h.apply(vec![
            BackendEvent::Message(remote(&d(&alice), &alice, "100", 100, "hi")),
            BackendEvent::Message(remote(&d(&bob), &bob, "200", 200, "hello")),
        ])
        .expect("apply");
        let id = |h: &crate::testing::IngestHarness, key: &ConvKey| {
            sql::conversation_id(h.connection(), h.account(), key)
                .expect("lookup")
                .expect("conversation")
        };
        let is_marked = |h: &crate::testing::IngestHarness, conv: ConversationId| -> bool {
            h.connection()
                .query_row(
                    "SELECT marked_unread FROM conversations WHERE id = ?1",
                    [conv.0],
                    |r| r.get(0),
                )
                .expect("row")
        };
        let (a, b) = (id(&h, &d(&alice)), id(&h, &d(&bob)));

        // Read, then marked unread on the phone: only the mark is there.
        mark_read(h.connection_mut(), a, 1_000)
            .expect("mark read")
            .expect("a message was unread");
        h.apply(vec![marked(d(&alice))]).expect("apply");
        assert!(is_marked(&h, a));
        let done = mark_read(h.connection_mut(), a, 2_000)
            .expect("mark read")
            .expect("the chat was marked unread");
        assert!(!is_marked(&h, a));
        assert!(done.messages.is_empty());
        assert!(
            done.commands.is_empty(),
            "no message was read: nothing to sync"
        );
        assert_eq!(done.mark_cleared, Some(d(&alice)));
        assert!(
            mark_read(h.connection_mut(), a, 3_000)
                .expect("mark read")
                .is_none(),
            "nothing is left to read"
        );

        // Marked unread with an unread message: both go, and the message is synced as before.
        h.apply(vec![marked(d(&bob))]).expect("apply");
        let done = mark_read(h.connection_mut(), b, 4_000)
            .expect("mark read")
            .expect("unread");
        assert!(!is_marked(&h, b));
        assert_eq!(done.messages.len(), 1);
        assert!(done.commands.iter().any(
            |(_, c)| matches!(c, DurableCommand::SyncReadMarker { read, .. } if read.len() == 1)
        ));
        assert_eq!(done.mark_cleared, Some(d(&bob)));

        // A chat that was not marked reports no mark.
        h.apply(vec![BackendEvent::Message(remote(
            &d(&alice),
            &alice,
            "300",
            300,
            "again",
        ))])
        .expect("apply");
        let done = mark_read(h.connection_mut(), a, 5_000)
            .expect("mark read")
            .expect("unread");
        assert_eq!(done.mark_cleared, None);
    }
}

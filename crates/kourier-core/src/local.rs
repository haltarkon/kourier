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

/// One account's lifecycle state (`None`: no such row).
pub(crate) fn state(conn: &Connection, id: AccountId) -> Result<Option<AccountState>> {
    let row: Option<(String, Option<String>)> = conn
        .prepare_cached("SELECT state, state_reason FROM accounts WHERE id = ?1")?
        .query_row([id.0], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    row.map(|(state, reason)| sql::account_state(&state, reason))
        .transpose()
}

/// Who `id` is, for the notice about its removal or unlink: read before the row goes. `twin`:
/// another account of its network, not being removed, has its primary own id or looks the same
/// (same name and number). `None`: no such row.
pub(crate) fn account_label(
    conn: &Connection,
    id: AccountId,
) -> Result<Option<crate::facade::AccountLabel>> {
    type Raw = (
        String,
        Option<String>,
        Option<String>,
        Option<u32>,
        i64,
        String,
    );
    let row: Option<Raw> = conn
        .prepare_cached(
            "SELECT network, display_name, number, own_device, created_at, own_ids
             FROM accounts WHERE id = ?1",
        )?
        .query_row([id.0], |r| {
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
    let Some((network, display_name, number, own_device, linked_at, own_ids)) = row else {
        return Ok(None);
    };
    let primary = sql::from_json::<Vec<kourier_model::UserId>>(&own_ids)?
        .into_iter()
        .next();
    // The PRIMARY own ids only, as the core's check at `AccountReady` does: `other_identities`
    // lists every own id of the other accounts (W2b B4), which is not what a twin is.
    let same_identity = crate::ingest::same_identity(conn, id, primary.as_ref())?.is_some();
    let looks_the_same: bool = conn
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM accounts o, accounts me
               WHERE me.id = ?1 AND o.id <> me.id AND o.state <> 'Removing'
                 AND o.network = me.network
                 AND o.display_name IS me.display_name AND o.number IS me.number)",
        )?
        .query_row([id.0], |r| r.get(0))?;
    Ok(Some(crate::facade::AccountLabel {
        network: sql::network(&network)?,
        display_name,
        number,
        own_device,
        linked_at,
        twin: same_identity || looks_the_same,
    }))
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

/// Sets a lifecycle state in ONE statement and puts the report a cut-off removal or unlink kept
/// (link feedback L5, [`removal_report`]) back with it: [`set_state`] writes no reason for
/// `Removing` and `Unlinking`, so a start that finishes the work and is killed itself between
/// two statements would lose what the first run told. `kept` is what [`removal_report`]
/// answered at this start (`None`: nothing was kept, the reason is cleared as `set_state` does).
pub(crate) fn set_state_keeping_report(
    conn: &Connection,
    id: AccountId,
    state: &AccountState,
    kept: Option<&Option<kourier_model::LogoutReport>>,
) -> Result<()> {
    let (name, _) = sql::account_state_name(state);
    let json = kept
        .map(|r| serde_json::to_string(r).map_err(|e| CoreError::Json(e.to_string())))
        .transpose()?;
    conn.prepare_cached("UPDATE accounts SET state = ?2, state_reason = ?3 WHERE id = ?1")?
        .execute((id.0, name, json))?;
    Ok(())
}

/// What the logout of a removal or an unlink told (`None`: nothing to tell, a link that never
/// finished), kept on the `Removing` or `Unlinking` row before the shred (link feedback L5): a
/// kill after the shred leaves no keys for another logout, and the next start that finishes
/// the work tells this.
pub(crate) fn set_removal_report(
    conn: &Connection,
    id: AccountId,
    report: Option<&kourier_model::LogoutReport>,
) -> Result<()> {
    let json = serde_json::to_string(&report).map_err(|e| CoreError::Json(e.to_string()))?;
    conn.prepare_cached(
        "UPDATE accounts SET state_reason = ?2 WHERE id = ?1 AND state IN ('Removing', 'Unlinking')",
    )?
    .execute((id.0, json))?;
    Ok(())
}

/// The report [`set_removal_report`] kept: `None` when the logout was not tried yet (or the row
/// is neither `Removing` nor `Unlinking`), `Some(None)` when it was and there is nothing to
/// tell.
pub(crate) fn removal_report(
    conn: &Connection,
    id: AccountId,
) -> Result<Option<Option<kourier_model::LogoutReport>>> {
    let kept: Option<Option<String>> = conn
        .prepare_cached(
            "SELECT state_reason FROM accounts WHERE id = ?1 AND state IN ('Removing', 'Unlinking')",
        )?
        .query_row([id.0], |r| r.get(0))
        .optional()?;
    match kept.flatten() {
        None => Ok(None),
        Some(json) => serde_json::from_str(&json)
            .map(Some)
            .map_err(|e| CoreError::Json(e.to_string())),
    }
}

/// The unlink bookkeeping (§4.3 step 5): `LoggedOut`, and durable commands can no longer go out.
/// The waits for the other device's answer end too ("did not answer", [`crate::restore`]): the
/// requests are among the commands that can no longer go out, and no backend will report them
/// unsent. Returns what ending them changed.
pub(crate) fn logged_out(
    conn: &mut Connection,
    id: AccountId,
    reason: &str,
    now: i64,
) -> Result<crate::ingest::Changes> {
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
    let ended = crate::restore::end_account_waits(&tx, id, now)?;
    tx.commit()?;
    Ok(ended)
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

/// The account's stored caps text: `Some(Some(caps))` read, `Some(None)` nothing stored yet
/// (or no such account), `None` stored but this build cannot read it (damaged, or written by a
/// later build). ONE read of `accounts.caps`.
fn stored_caps(
    conn: &Connection,
    id: AccountId,
) -> Result<Option<Option<kourier_model::AccountCaps>>> {
    let caps: Option<String> = conn
        .prepare_cached("SELECT caps FROM accounts WHERE id = ?1")?
        .query_row([id.0], |r| r.get(0))
        .optional()?
        .flatten();
    Ok(sql::from_json_opt::<kourier_model::AccountCaps>(caps).ok())
}

/// How the account's caps state `roaming_writes` (a chat setting the network keeps for the
/// account's other devices): `None` = the network states no such thing (or nothing is known of
/// the account yet), `Some(false)` = it can and the account's switch is off, `Some(true)` = on.
/// ONE read of `accounts.caps`; the backend alone states it, the core never writes it. Caps this
/// build cannot read state nothing (one warn line with the account's id, no content): the
/// reading of the account's chats goes on with the plain send of the clear.
pub(crate) fn roaming_writes(conn: &Connection, id: AccountId) -> Result<Option<bool>> {
    match stored_caps(conn, id)? {
        Some(caps) => Ok(caps.and_then(|c| c.roaming_writes())),
        None => {
            tracing::warn!(
                account = id.0,
                "the account's stored caps cannot be read; it states no roaming settings"
            );
            Ok(None)
        }
    }
}

/// What a start sends again of the account's durable commands ([`pending_commands`]), and how
/// many it dropped instead. A durable roaming change (`DurableCommand::ChangeConversation`,
/// queued by [`mark_read`] only for an account whose caps state `roaming_writes`) of an account
/// that does not state it any more is DELETED here, in one transaction, and not sent: the
/// network cannot take it, and a row nobody confirms would come back at every start for ever.
/// The other commands are never touched.
///
/// An account whose stored caps this build CANNOT READ is another case: nobody knows whether it
/// states the flag, so its roaming rows are KEPT, neither sent at this start nor deleted (a
/// later start that can read the caps sends or drops them), and every other command of the
/// account is sent as always. One warn line (the account's id and how many rows).
pub(crate) fn resendable_commands(
    conn: &mut Connection,
    id: AccountId,
) -> Result<(Vec<(CmdId, DurableCommand)>, usize)> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut cmds = pending_commands(&tx, id)?;
    let roaming = |c: &DurableCommand| matches!(c, DurableCommand::ChangeConversation { .. });
    let mut dropped = 0;
    let held = cmds.iter().filter(|(_, c)| roaming(c)).count();
    if held > 0 {
        match stored_caps(&tx, id)? {
            // Unreadable: kept, not sent now.
            None => {
                tracing::warn!(
                    account = id.0,
                    held,
                    "the account's stored caps cannot be read; its roaming changes wait"
                );
                cmds.retain(|(_, c)| !roaming(c));
            }
            Some(caps) if caps.as_ref().and_then(|c| c.roaming_writes()).is_none() => {
                for (cmd, _) in cmds.iter().filter(|(_, c)| roaming(c)) {
                    tx.prepare_cached(
                        "DELETE FROM pending_commands WHERE id = ?1 AND account_id = ?2",
                    )?
                    .execute((cmd.0, id.0))?;
                    dropped += 1;
                }
                cmds.retain(|(_, c)| !roaming(c));
            }
            Some(_) => {}
        }
    }
    tx.commit()?;
    Ok((cmds, dropped))
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
    /// The conversation was marked unread by hand (on another device) and is not any more, on
    /// an account whose caps do NOT state `roaming_writes`: the network is told with a plain,
    /// best-effort `ConversationChange::SetMarkedUnread` that nothing stored. For an account that
    /// states it the clearing is a durable command among `commands` instead (A10a) and this is
    /// `None`.
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

/// The position a read here reaches ([`mark_read`]): the newest of a row somebody else wrote, among
/// the rows `read_up_to` looks at. A row deleted for me and one the sweeper hides are not seen, so
/// they are not read and do not raise the position past what the person saw. Written out, so it
/// is one probe of `messages_seq`, empty on every network that numbers nothing, and never a walk of
/// the timeline (`max(net_seq)` implies nothing under SQLite's rule, so it could not use the partial
/// index; `tests/cloud_ids.rs` holds the plan and the cost).
pub(crate) const NEWEST_READ_POSITION: &str = "SELECT net_seq FROM messages
             WHERE conversation_id = ?1 AND net_seq IS NOT NULL AND net_author <> ''
               AND (deleted IS NULL OR deleted <> 'ForMe') AND expiry_hidden = 0
             ORDER BY net_seq DESC LIMIT 1";

/// Marks every unread message of `conv` read, clears the history unread hint and the "marked
/// unread" flag another device set and, in the same transaction, queues the read sync (and
/// read receipts when enabled) as `pending_commands` (§4.8). Read receipts never go to a
/// blocked conversation or sender, nor into a message request that was not accepted.
/// Returns `None` when nothing was unread and the conversation was not marked unread (nor
/// `told_mark`: the UI set the mark a moment ago and the backend's echo has not been
/// ingested yet).
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
/// flag is cleared (Signal Desktop does the same when a chat is read) and no read sync is queued.
///
/// The mark itself is told to the account's other devices in one of two ways (A10a, the
/// automatic clear of architecture §4.1 principle 4). For an account whose caps state
/// `roaming_writes` (`Some(_)`, the switch on or off; ONE read in this transaction) the clearing
/// is a DURABLE `DurableCommand::ChangeConversation { SetMarkedUnread { false }, asked_at }`
/// stored in this very transaction, so a kill after the commit cannot lose it (it is sent from
/// `pending_commands` at every start until the backend answers `CommandPersisted`);
/// `asked_at` is the moment the user read the chat on the core's clock (`Shared::now`, the clock
/// the backend reads too), taken by the runtime when the command came in, BEFORE the reading
/// waited for its turn behind an earlier change of the chat; `now` is the time the reading ran
/// (the read state, the `AfterRead` timers). A phone's mark merged after `asked_at` is newer than
/// the reading. For an account that does not state the flag (or whose caps cannot be read) the
/// clearing stays what it was: a plain best-effort command the runtime sends after the commit
/// (`MarkedRead::mark_cleared`).
///
/// A message hidden because its time has come on an unconfirmed clock ([`crate::expiry`]) is
/// not read by this: the user cannot see it, so nobody is told they read it. If it shows
/// again it is unread, as it was.
///
/// On a network that numbers its messages (KT-2) the own read position rises to the newest row
/// somebody else wrote, so a late copy of something older does not come back unread, and a count
/// the server stated is 0 now: until the network's next statement this is what is unread here. A
/// stated count that is not 0 counts like the hint: with no unread row a chat stated at 7 is
/// still read here, and the network is told.
pub(crate) fn mark_read(
    conn: &mut Connection,
    conv: ConversationId,
    now: i64,
    asked_at: i64,
    told_mark: bool,
) -> Result<Option<MarkedRead>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let row: Option<(i64, String, i64, bool, bool, bool)> = tx
        .prepare_cached(
            "SELECT account_id, conv_key,
               unread_hint + coalesce(unread_server, 0) + coalesce(mentions_server, 0),
               blocked, request_pending, marked_unread
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
    // (What a backend stated is unread: the hint, and a count the server owns.)
    let Some((account, key, stated, blocked, request_pending, marked)) = row else {
        return Err(CoreError::UnknownConversation(conv));
    };
    // The mark the UI itself set right before this reading counts as a mark to clear, though
    // the row turns marked only when the backend's echo is ingested (R1 fix: the core's
    // `NetOrder` remembers it).
    let marked = marked || told_mark;
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
    if unread.is_empty() && stated == 0 && !marked {
        return Ok(None);
    }
    // Something of the chat itself was unread: only then is there a read state to sync. A
    // row without an author is a call line ([`crate::ingest`] `call`): it is read here and
    // nobody is told, neither its caller (no receipt) nor our other devices (no read sync).
    let had_unread = unread.iter().any(|(_, author, _, _)| !author.is_empty()) || stated != 0;
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
    let read_seq: Option<i64> = tx
        .prepare_cached(NEWEST_READ_POSITION)?
        .query_row([conv.0], |r| r.get(0))
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
           unread_server = CASE WHEN unread_server IS NULL THEN NULL ELSE 0 END,
           mentions_server = CASE WHEN mentions_server IS NULL THEN NULL ELSE 0 END,
           read_up_to = max(coalesce(read_up_to, ?2), ?2),
           read_seq = CASE WHEN ?3 IS NULL THEN read_seq ELSE max(coalesce(read_seq, ?3), ?3) END
         WHERE id = ?1",
    )?
    .execute((conv.0, read_up_to.unwrap_or(i64::MIN), read_seq))?;

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
    // The mark is gone here and the account's other devices follow (a logged-out account has
    // none to tell). Durable where the network keeps roaming settings, else the plain send.
    let mut mark_cleared = None;
    if marked && active {
        if roaming_writes(&tx, account)?.is_some() {
            queue(DurableCommand::ChangeConversation {
                conv: key.clone(),
                change: kourier_model::ConversationChange::SetMarkedUnread {
                    marked_unread: false,
                },
                asked_at,
            })?;
        } else {
            mark_cleared = Some(key);
        }
    }
    tx.commit()?;
    Ok(Some(MarkedRead {
        account,
        conv,
        messages: ids,
        commands,
        mark_cleared,
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
        let marked = mark_read(h.connection_mut(), conv, 2_000, 2_000, false)
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
        mark_read(h.connection_mut(), a, 1_000, 1_000, false)
            .expect("mark read")
            .expect("a message was unread");
        h.apply(vec![marked(d(&alice))]).expect("apply");
        assert!(is_marked(&h, a));
        let done = mark_read(h.connection_mut(), a, 2_000, 2_000, false)
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
            mark_read(h.connection_mut(), a, 3_000, 3_000, false)
                .expect("mark read")
                .is_none(),
            "nothing is left to read"
        );

        // Marked unread with an unread message: both go, and the message is synced as before.
        h.apply(vec![marked(d(&bob))]).expect("apply");
        let done = mark_read(h.connection_mut(), b, 4_000, 4_000, false)
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
        let done = mark_read(h.connection_mut(), a, 5_000, 5_000, false)
            .expect("mark read")
            .expect("unread");
        assert_eq!(done.mark_cleared, None);

        // R1 fix: a mark the UI set a moment ago, whose echo is not ingested yet, is a mark to
        // clear: the network is told of the clearing though the row does not show the mark,
        // and with nothing else to read the reading is still not "nothing to do".
        assert!(!is_marked(&h, a));
        let done = mark_read(h.connection_mut(), a, 6_000, 6_000, true)
            .expect("mark read")
            .expect("a told mark is a mark");
        assert_eq!(done.mark_cleared, Some(d(&alice)));
        assert!(done.messages.is_empty() && done.commands.is_empty());
        assert!(
            mark_read(h.connection_mut(), a, 7_000, 7_000, false)
                .expect("mark read")
                .is_none(),
            "without the told mark there is nothing to do"
        );
    }

    /// A harness whose chat with alice has an unread message and is marked unread, on an account
    /// whose caps say `flag` about `roaming_writes` (`None`: the flag is not stated at all; the
    /// caps always state another flag, which is no statement about this one).
    fn marked_account(
        dir: &std::path::Path,
        flag: Option<bool>,
    ) -> (crate::testing::IngestHarness, ConversationId, ConvKey) {
        let mut h = crate::testing::IngestHarness::open(
            &dir.join("core.db"),
            &kourier_db::DbKey::from([9u8; 32]),
            NetworkKind::Fake,
            MsgIdScope::AccountAuthor,
            &[uid(ME)],
        )
        .expect("harness");
        let alice = uid(ALICE);
        let key = ConvKey::Direct(alice.clone());
        let mut caps = kourier_model::AccountCaps::default();
        caps.flags.insert(
            kourier_model::AccountCaps::FLAG_ATTACHMENT_RESTORE.into(),
            true,
        );
        if let Some(on) = flag {
            caps.flags
                .insert(kourier_model::AccountCaps::FLAG_ROAMING_WRITES.into(), on);
        }
        let mut marked = ConversationPatch::new(key.clone());
        marked.marked_unread = Some(true);
        h.apply(vec![
            BackendEvent::AccountCaps(caps),
            BackendEvent::Message(remote(&key, &alice, "100", 100, "hi")),
            BackendEvent::ConversationUpsert(marked),
        ])
        .expect("apply");
        let conv = sql::conversation_id(h.connection(), h.account(), &key)
            .expect("lookup")
            .expect("conversation");
        (h, conv, key)
    }

    fn clearing(at: i64) -> DurableCommand {
        DurableCommand::ChangeConversation {
            conv: ConvKey::Direct(uid(ALICE)),
            change: kourier_model::ConversationChange::SetMarkedUnread {
                marked_unread: false,
            },
            asked_at: at,
        }
    }

    fn stored(h: &crate::testing::IngestHarness) -> Vec<DurableCommand> {
        pending_commands(h.connection(), h.account())
            .expect("pending")
            .into_iter()
            .map(|(_, c)| c)
            .collect()
    }

    /// A10a: for an account whose caps state `roaming_writes` (the switch on OR off) reading a
    /// marked chat stores the clearing as a DURABLE command in the reading's own transaction
    /// (`asked_at` is the reading's time) and sends no plain one; an account that states no such
    /// flag (or only another one) keeps the plain send and stores no row for it. A told mark
    /// the row does not show yet counts the same. Mutants: the durable clear is not stored;
    /// the flag is not read (always plain).
    #[test]
    fn reading_stores_the_clearing_durably_only_for_an_account_that_states_the_flag() {
        for flag in [Some(true), Some(false)] {
            let dir = tempfile::tempdir().expect("tempdir");
            let (mut h, conv, _) = marked_account(dir.path(), flag);
            let done = mark_read(h.connection_mut(), conv, 7_654, 7_654, false)
                .expect("mark read")
                .expect("unread and marked");
            assert_eq!(
                done.mark_cleared, None,
                "{flag:?}: no plain send beside the durable one"
            );
            assert!(
                done.commands
                    .iter()
                    .any(|(_, c)| matches!(c, DurableCommand::SyncReadMarker { .. })),
                "{flag:?}: the read sync is still queued"
            );
            let clears: Vec<_> = done
                .commands
                .iter()
                .filter(|(_, c)| matches!(c, DurableCommand::ChangeConversation { .. }))
                .collect();
            assert_eq!(clears.len(), 1, "{flag:?}: one clearing");
            assert_eq!(
                clears[0].1,
                clearing(7_654),
                "{flag:?}: at the reading's time"
            );
            // Stored, not only returned: the row is what a restart sends again.
            assert!(stored(&h).contains(&clearing(7_654)), "{flag:?}: stored");
            assert!(!is_marked(&h, conv));
        }

        // The mark the UI set a moment ago (its echo is not ingested): the same.
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut h, conv, _) = marked_account(dir.path(), Some(true));
        mark_read(h.connection_mut(), conv, 1_000, 1_000, false)
            .expect("read")
            .expect("unread");
        assert!(!is_marked(&h, conv));
        let done = mark_read(h.connection_mut(), conv, 2_000, 2_000, true)
            .expect("read")
            .expect("a told mark is a mark");
        assert_eq!(done.mark_cleared, None);
        assert_eq!(
            done.commands.iter().map(|(_, c)| c).collect::<Vec<_>>(),
            vec![&clearing(2_000)]
        );

        // No such flag, or another one only: the plain send, no durable row.
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut h, conv, key) = marked_account(dir.path(), None);
        let before = stored(&h).len();
        let done = mark_read(h.connection_mut(), conv, 3_000, 3_000, false)
            .expect("mark read")
            .expect("unread and marked");
        assert_eq!(done.mark_cleared, Some(key), "the plain, best-effort send");
        assert!(
            !done
                .commands
                .iter()
                .any(|(_, c)| matches!(c, DurableCommand::ChangeConversation { .. })),
            "no durable clearing without the flag"
        );
        assert!(
            !stored(&h)
                .iter()
                .any(|c| matches!(c, DurableCommand::ChangeConversation { .. }))
                && stored(&h).len() > before,
            "only the read sync is stored"
        );
    }

    fn is_marked(h: &crate::testing::IngestHarness, conv: ConversationId) -> bool {
        h.connection()
            .query_row(
                "SELECT marked_unread FROM conversations WHERE id = ?1",
                [conv.0],
                |r| r.get(0),
            )
            .expect("row")
    }

    /// A logged-out account has no device to tell: nothing is queued for it, durable or plain.
    #[test]
    fn a_logged_out_account_is_told_nothing_of_the_clearing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut h, conv, _) = marked_account(dir.path(), Some(true));
        let account = h.account();
        logged_out(h.connection_mut(), account, "unlinked", 500).expect("logged out");
        let done = mark_read(h.connection_mut(), conv, 600, 600, false)
            .expect("mark read")
            .expect("it was marked");
        assert_eq!(done.mark_cleared, None);
        assert!(done.commands.is_empty(), "{:?}", done.commands);
        assert!(stored(&h).is_empty());
    }

    /// A10a: a start sends the stored durable commands again, except a roaming change of an
    /// account that does not state `roaming_writes` any more: that row is deleted (once, with a
    /// count) and not sent, and every other command stays. An account that still states it (the
    /// switch off too) keeps the row. Mutant: the filter drops nothing / drops every command.
    #[test]
    fn a_start_drops_a_roaming_change_of_an_account_that_no_longer_states_the_flag() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut h, conv, _) = marked_account(dir.path(), Some(true));
        mark_read(h.connection_mut(), conv, 4_000, 4_000, false)
            .expect("mark read")
            .expect("unread and marked");
        let account = h.account();
        let all = stored(&h);
        assert!(all.contains(&clearing(4_000)) && all.len() >= 2, "{all:?}");

        // Still stated: everything is sent again, nothing dropped.
        let (cmds, dropped) = resendable_commands(h.connection_mut(), account).expect("resend");
        assert_eq!((cmds.len(), dropped), (all.len(), 0));
        // Stated with the switch off: still a statement.
        let mut caps = kourier_model::AccountCaps::default();
        caps.flags.insert(
            kourier_model::AccountCaps::FLAG_ROAMING_WRITES.into(),
            false,
        );
        h.apply(vec![BackendEvent::AccountCaps(caps)])
            .expect("caps");
        let (cmds, dropped) = resendable_commands(h.connection_mut(), account).expect("resend");
        assert_eq!((cmds.len(), dropped), (all.len(), 0));

        // The flag went away: the roaming change is dropped, the rest is sent.
        h.apply(vec![BackendEvent::AccountCaps(Default::default())])
            .expect("caps");
        let (cmds, dropped) = resendable_commands(h.connection_mut(), account).expect("resend");
        assert_eq!(dropped, 1);
        assert_eq!(cmds.len(), all.len() - 1);
        assert!(
            !cmds
                .iter()
                .any(|(_, c)| matches!(c, DurableCommand::ChangeConversation { .. }))
        );
        assert!(
            cmds.iter()
                .any(|(_, c)| matches!(c, DurableCommand::SyncReadMarker { .. })),
            "the other commands are left alone"
        );
        // Deleted, not only left out: the next start has nothing to drop.
        assert!(!stored(&h).contains(&clearing(4_000)));
        let (cmds, dropped) = resendable_commands(h.connection_mut(), account).expect("resend");
        assert_eq!((cmds.len(), dropped), (all.len() - 1, 0));
    }

    /// F6: the clearing carries `asked_at`, the moment the user read the chat, and not the time
    /// the reading ran (`now`, later when the reading waited for its turn): the read state and
    /// the stored clearing take their own times. Mutant: `asked_at: now`.
    #[test]
    fn the_clearing_carries_the_moment_the_user_asked_not_the_time_the_reading_ran() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut h, conv, _) = marked_account(dir.path(), Some(true));
        let done = mark_read(h.connection_mut(), conv, 9_000, 8_000, false)
            .expect("mark read")
            .expect("unread and marked");
        assert!(
            done.commands.iter().any(|(_, c)| *c == clearing(8_000)),
            "{:?}",
            done.commands
        );
        assert!(stored(&h).contains(&clearing(8_000)));
        assert!(!stored(&h).contains(&clearing(9_000)));
    }

    /// F7: caps this build cannot read state nothing for the roaming STATE of a reading: the
    /// chat is read, the clear is the plain send (no durable row) and the reading does not fail.
    /// Mutant: the unreadable caps fail the reading (`?`).
    #[test]
    fn reading_a_chat_of_an_account_with_unreadable_caps_still_works_and_sends_the_clear_plainly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut h, conv, key) = marked_account(dir.path(), Some(true));
        let account = h.account();
        h.connection()
            .execute(
                "UPDATE accounts SET caps = 'not json at all' WHERE id = ?1",
                [account.0],
            )
            .expect("damage");
        assert_eq!(roaming_writes(h.connection(), account).expect("read"), None);
        let done = mark_read(h.connection_mut(), conv, 5_000, 5_000, false)
            .expect("the reading works")
            .expect("unread and marked");
        assert_eq!(done.mark_cleared, Some(key), "the plain send of the clear");
        assert!(
            !done
                .commands
                .iter()
                .any(|(_, c)| matches!(c, DurableCommand::ChangeConversation { .. })),
            "no durable row"
        );
        assert!(!is_marked(&h, conv), "the chat is read");
    }

    /// F7 addendum: an account whose caps this build cannot read KEEPS its roaming changes at a
    /// start (not sent, not deleted) and every other durable command of the account is sent as
    /// ever; a later start that can read the caps sends the kept row. Mutants: the start fails as
    /// a whole; the unreadable caps drop the roaming row; they send it.
    #[test]
    fn an_account_with_unreadable_caps_keeps_its_roaming_changes_and_sends_the_rest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut h, conv, _) = marked_account(dir.path(), Some(true));
        mark_read(h.connection_mut(), conv, 4_000, 4_000, false)
            .expect("mark read")
            .expect("unread and marked");
        let account = h.account();
        let all = stored(&h);
        assert!(all.contains(&clearing(4_000)) && all.len() >= 2, "{all:?}");
        h.connection()
            .execute(
                "UPDATE accounts SET caps = 'not json at all' WHERE id = ?1",
                [account.0],
            )
            .expect("damage");

        let (cmds, dropped) = resendable_commands(h.connection_mut(), account)
            .expect("a start does not fail for the account's caps");
        assert_eq!((cmds.len(), dropped), (all.len() - 1, 0), "nothing deleted");
        assert!(
            !cmds
                .iter()
                .any(|(_, c)| matches!(c, DurableCommand::ChangeConversation { .. })),
            "the roaming change is not sent this start"
        );
        assert!(
            cmds.iter()
                .any(|(_, c)| matches!(c, DurableCommand::SyncReadMarker { .. })),
            "the others are sent as before"
        );
        assert_eq!(stored(&h), all, "every row is still there");

        // A later start that can read the caps sends the row it kept.
        let mut caps = kourier_model::AccountCaps::default();
        caps.flags
            .insert(kourier_model::AccountCaps::FLAG_ROAMING_WRITES.into(), true);
        h.apply(vec![BackendEvent::AccountCaps(caps)])
            .expect("caps");
        let (cmds, dropped) = resendable_commands(h.connection_mut(), account).expect("resend");
        assert_eq!((cmds.len(), dropped), (all.len(), 0));
    }
}

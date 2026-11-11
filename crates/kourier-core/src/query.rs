//! Read-side queries for the facade (§4.10, §8.4), run on the read-only connections.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use kourier_db::rusqlite::{Connection, OptionalExtension, Row};
use kourier_model::{
    AccountId, AccountState, AccountUuid, Attachment, AttachmentId, AttachmentKind,
    AttachmentState, BlobKey, BlobRef, Body, ConnState, Contact, ConversationFeatures,
    ConversationId, Deletion, DisappearKind, Expiry, Member, MemberRole, MessageId, ReplyTo,
    TargetAuthor, Text, UserId,
};

use crate::actions::{self, Facts, Sending};
use crate::error::{CoreError, Result};
use crate::facade::{
    AccountRow, ChatMediaItem, ChatMediaPage, ChatMediaQuery, CommonGroup, ConversationDetail,
    ConversationListQuery, ConversationPage, ConversationRow, GroupDetail, GroupMember,
    MAX_MEDIA_PAGE, MediaKind, MessageRevisions, MessageRow, OriginKind, OutboxWait, PersonChat,
    PersonDetail, PersonRef, Preview, QuoteState, ReactionGroup, ReplyPreview, Revision,
    TimelineAnchor, TimelinePage, TimelineQuery,
};
use crate::roaming;
use crate::sql;

/// Largest page any query returns.
pub const MAX_PAGE: u32 = 500;

/// Every account, in display order.
pub(crate) fn accounts(
    conn: &Connection,
    conn_states: &HashMap<AccountId, ConnState>,
    holds: &HashMap<AccountId, kourier_model::AccountHold>,
) -> Result<Vec<AccountRow>> {
    type Raw = (
        i64,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<u32>,
        String,
        Option<String>,
        i64,
        Option<String>,
    );
    let rows: Vec<Raw> = conn
        .prepare_cached(
            "SELECT id, uuid, network, state, state_reason, display_name, own_device, own_ids,
               number, created_at, caps
             FROM accounts ORDER BY coalesce(sort_order, id), id",
        )?
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
            ))
        })?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(
            |(id, uuid, network, state, reason, name, device, own_ids, number, linked_at, caps)| {
                let state = sql::account_state(&state, reason)?;
                // Without a running actor (after a restart) a logged-out account still says so.
                let shown =
                    conn_states
                        .get(&AccountId(id))
                        .cloned()
                        .unwrap_or_else(|| match &state {
                            AccountState::LoggedOut { reason } => ConnState::LoggedOut {
                                reason: reason.clone(),
                                cause: Default::default(),
                                note: Default::default(),
                            },
                            _ => ConnState::default(),
                        });
                // The account's own picture is its first own id's contact avatar.
                let own_ids: Vec<UserId> = sql::from_json(&own_ids)?;
                let avatar = match own_ids.first() {
                    Some(own) => contact_avatar(conn, id, own.as_str())?,
                    None => None,
                };
                let roaming = roaming::AccountRoaming::read(id, caps);
                Ok(AccountRow {
                    id: AccountId(id),
                    uuid: uuid
                        .parse::<AccountUuid>()
                        .map_err(|_| CoreError::Json("malformed stored uuid".into()))?,
                    network: sql::network(&network)?,
                    state,
                    display_name: name,
                    conn: shown,
                    own_device: device,
                    avatar,
                    number,
                    linked_at,
                    hold: holds.get(&AccountId(id)).copied(),
                    roaming_writes: roaming.writes,
                    pin_limit: roaming.pin_limit,
                })
            },
        )
        .collect()
}

/// The avatar file of a contact, when the backend supplied one.
fn contact_avatar(conn: &Connection, account: i64, user: &str) -> Result<Option<BlobRef>> {
    let file: Option<i64> = conn
        .prepare_cached("SELECT avatar_file FROM contacts WHERE account_id = ?1 AND user_id = ?2")?
        .query_row((account, user), |r| r.get(0))
        .optional()?
        .flatten();
    blob_ref(conn, file)
}

fn blob_ref(conn: &Connection, file: Option<i64>) -> Result<Option<BlobRef>> {
    let Some(file) = file else { return Ok(None) };
    let row: Option<(String, Vec<u8>)> = conn
        .prepare_cached("SELECT path, key FROM files WHERE id = ?1")?
        .query_row([file], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    Ok(row.and_then(|(path, key)| blob_of(path, key)))
}

/// A file row's path and key as a blob handle; `None` for a key of the wrong length.
fn blob_of(path: String, key: Vec<u8>) -> Option<BlobRef> {
    Some(BlobRef {
        path: PathBuf::from(path),
        key: BlobKey::new(key.try_into().ok()?),
    })
}

/// The profile name of the contact aliased `$ct` as it is SHOWN (SQL): the name the person chose
/// for themself, which WhatsApp's own apps show with a "~" in front wherever the viewer has no
/// address-book name of their own for the person (his answer "~Name, as the phone", 2026-10-28).
/// `$network` is the SQL expression for the account's stored network; only WhatsApp's people carry
/// the mark (the test `the_mark_names_the_stored_network_of_whatsapp` pins the literal), every other
/// network's profile name is shown as it is. The nickname and the address-book name come before it
/// in every `coalesce`, so they never carry the mark. An empty name stays empty (no lone "~").
macro_rules! shown_profile_name {
    ($ct:literal, $network:literal) => {
        concat!(
            "CASE WHEN ",
            $network,
            " = 'WhatsApp' AND ",
            $ct,
            ".profile_name <> '' THEN '~' || ",
            $ct,
            ".profile_name ELSE ",
            $ct,
            ".profile_name END"
        )
    };
}

/// Display name of a user: nickname, address-book name, then profile name (marked for WhatsApp's
/// people, see [`shown_profile_name`]).
fn display_name(conn: &Connection, account: i64, user: &str) -> Result<Option<String>> {
    Ok(conn
        .prepare_cached(concat!(
            "SELECT coalesce(ct.nickname, ct.system_name, ",
            shown_profile_name!("ct", "a.network"),
            ") FROM contacts ct JOIN accounts a ON a.id = ct.account_id
             WHERE ct.account_id = ?1 AND ct.user_id = ?2"
        ))?
        .query_row((account, user), |r| r.get::<_, Option<String>>(0))
        .optional()?
        .flatten())
}

/// (The last column, `ConversationFeatures::lists_for_roaming`: absent and `true` list the chat,
/// only a stated `false` unlists it.)
const CONV_SELECT: &str = concat!(
    "SELECT c.id, c.account_id, c.kind, c.is_self, c.conv_key,
      coalesce(c.title, ct.nickname, ct.system_name, ",
    shown_profile_name!("ct", "a.network"),
    "),
      coalesce(c.avatar_file, ct.avatar_file), coalesce(c.last_activity, 0), c.unread, c.mentions,
      c.marked_unread, c.muted_until, c.local_muted_until, c.archived, c.pinned_order, c.blocked,
      c.request_pending, coalesce(c.features_hash, 0), c.draft IS NOT NULL, c.hidden,
      json_extract(c.features, '$.cannot_send') IS NULL,
      coalesce(json_extract(c.features, '$.roaming_listed'), 1) <> 0, c.protected
    FROM conversations c
    JOIN accounts a ON a.id = c.account_id
    LEFT JOIN contacts ct ON c.kind = 'Direct' AND ct.account_id = c.account_id
      AND ct.user_id = substr(c.conv_key, 8)"
);

struct ConvRaw {
    id: i64,
    account: i64,
    kind: String,
    is_self: bool,
    key: String,
    title: Option<String>,
    avatar: Option<i64>,
    last_activity: i64,
    unread: i64,
    mentions: i64,
    marked_unread: bool,
    muted_until: Option<i64>,
    local_muted_until: Option<i64>,
    archived: bool,
    pinned_order: Option<i64>,
    blocked: bool,
    request_pending: bool,
    features_hash: i64,
    has_draft: bool,
    hidden: bool,
    can_send: bool,
    /// The network lists the chat for roaming settings (`roaming_listed` is not `false`).
    roaming_listed: bool,
    /// The user protects the chat on another device (core.db `protected`).
    protected: bool,
}

fn conv_raw(r: &Row<'_>) -> kourier_db::rusqlite::Result<ConvRaw> {
    Ok(ConvRaw {
        id: r.get(0)?,
        account: r.get(1)?,
        kind: r.get(2)?,
        is_self: r.get(3)?,
        key: r.get(4)?,
        title: r.get(5)?,
        avatar: r.get(6)?,
        last_activity: r.get(7)?,
        unread: r.get(8)?,
        mentions: r.get(9)?,
        marked_unread: r.get(10)?,
        muted_until: r.get(11)?,
        local_muted_until: r.get(12)?,
        archived: r.get(13)?,
        pinned_order: r.get(14)?,
        blocked: r.get(15)?,
        request_pending: r.get(16)?,
        features_hash: r.get(17)?,
        has_draft: r.get(18)?,
        hidden: r.get(19)?,
        can_send: r.get(20)?,
        roaming_listed: r.get(21)?,
        protected: r.get(22)?,
    })
}

/// A conversation row. `accounts` is every account's roaming statement, read ONCE by the query
/// ([`roaming::accounts`]); an account it does not know (a row deleted meanwhile) states none.
fn conv_row(
    conn: &Connection,
    raw: ConvRaw,
    accounts: &HashMap<i64, roaming::AccountRoaming>,
) -> Result<ConversationRow> {
    let key = sql::conv_key(&raw.key)?;
    let roaming = accounts
        .get(&raw.account)
        .copied()
        .unwrap_or_else(|| roaming::AccountRoaming::of(None))
        .state(raw.roaming_listed);
    // A protected chat's messages show nowhere outside the open chat: its row carries no
    // preview at all (no text, author, time or kind), so no front-end can show one.
    let preview = if raw.protected {
        None
    } else {
        preview(conn, raw.account, raw.id)?
    };
    Ok(ConversationRow {
        id: ConversationId(raw.id),
        account: AccountId(raw.account),
        kind: sql::from_enum_name(&raw.kind)?,
        is_self: raw.is_self,
        title: raw.title,
        peer: key.direct_peer().cloned(),
        avatar: blob_ref(conn, raw.avatar)?,
        preview,
        last_activity: raw.last_activity,
        unread: sql::u32_of(raw.unread),
        mentions: sql::u32_of(raw.mentions),
        marked_unread: raw.marked_unread,
        muted_until: raw.muted_until,
        local_muted_until: raw.local_muted_until,
        archived: raw.archived,
        pinned_order: raw.pinned_order.map(sql::u32_of),
        blocked: raw.blocked,
        request_pending: raw.request_pending,
        features_hash: u64::from_ne_bytes(raw.features_hash.to_ne_bytes()),
        has_draft: raw.has_draft,
        hidden: raw.hidden,
        can_send: raw.can_send,
        roaming,
        protected: raw.protected,
    })
}

/// The newest LISTED row of a conversation ([`crate::notices::listed`]): the same rows
/// that give `conversations.last_activity` its value, so the preview and the place in the
/// list always speak of one message.
pub(crate) const PREVIEW: &str = concat!(
    "SELECT m.id, m.net_author, m.body_kind, m.preview, m.sent_at, m.deleted, m.out_state,
               (SELECT x.kind FROM attachments x WHERE x.message_id = m.id AND x.long_text = 0
                ORDER BY x.idx LIMIT 1),
               (SELECT count(*) FROM attachments x WHERE x.message_id = m.id AND x.long_text = 0),
               CASE WHEN m.body_kind IN ('System', 'Unsupported') THEN m.body END,
               (SELECT x.file_name FROM attachments x WHERE x.message_id = m.id AND x.long_text = 0
                ORDER BY x.idx LIMIT 1),
               m.deleted_by
             FROM messages m
             WHERE m.conversation_id = ?1 AND (m.deleted IS NULL OR m.deleted <> 'ForMe')
               AND m.expiry_hidden = 0 AND ",
    crate::notices::listed!(),
    "
             ORDER BY m.sort_key DESC, m.id DESC LIMIT 1"
);

/// How a row was deleted, from its `deleted` and `deleted_by` columns: the timeline and the list's
/// preview read it alike, so a delete that names nobody (KT-2) is `Unattributed` in both and no
/// list says "You deleted this message" of a delete the peer may have made.
fn deletion_of(deleted: Option<&str>, deleted_by: Option<String>) -> Result<Option<Deletion>> {
    Ok(match deleted {
        None => None,
        Some("ForMe") => Some(Deletion::ForMe),
        Some(_) => Some(match deleted_by {
            // Nobody was named: a server that checks deletes (KT-2) need not say who.
            None => Deletion::Unattributed,
            Some(b) => Deletion::ForEveryone {
                by: (!b.is_empty()).then(|| sql::user(&b)).transpose()?,
            },
        }),
    })
}

fn preview(conn: &Connection, account: i64, conv: i64) -> Result<Option<Preview>> {
    type Raw = (
        i64,
        String,
        String,
        Option<String>,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    // What the list says instead of a text: the first attachment's kind and how many there
    // are (long texts are text, not files), and the body of a notice or an unsupported row.
    let row: Option<Raw> = conn
        .prepare_cached(PREVIEW)?
        .query_row([conv], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
            ))
        })
        .optional()?;
    let Some((
        id,
        author,
        kind,
        text,
        sent_at,
        deleted,
        out_state,
        first,
        count,
        body,
        file_name,
        deleted_by,
    )) = row
    else {
        return Ok(None);
    };
    let (system, kind_hint) = match sql::from_json_opt::<Body>(body)? {
        Some(Body::System(event)) => (Some(event), None),
        Some(Body::Unsupported { kind_hint, .. }) => (None, Some(kind_hint)),
        _ => (None, None),
    };
    let (author, author_name) = if author.is_empty() {
        (None, None)
    } else {
        (
            Some(sql::user(&author)?),
            display_name(conn, account, &author)?,
        )
    };
    Ok(Some(Preview {
        message: MessageId(id),
        author,
        author_name,
        body_kind: kind,
        text,
        sent_at,
        deleted: deletion_of(deleted.as_deref(), deleted_by)?,
        out_state: sql::from_json_opt(out_state)?,
        attachment: first
            .as_deref()
            .map(sql::from_enum_name::<AttachmentKind>)
            .transpose()?,
        attachments: sql::u32_of(count),
        file_name: file_name.filter(|n| !n.trim().is_empty()),
        system,
        kind_hint,
    }))
}

/// The statement of the conversation list (`?1` account or NULL, `?2` archived: 0 the inbox, 1
/// the archive, NULL both, `?3..?7` the keyset position, `?8` the limit). `c.last_activity` is
/// the sort time of the newest LISTED row (none: the chat is listed after the chats that have
/// one, as an empty chat). No chat is left out for what its rows are ([`crate::notices`],
/// decision F1): only `hidden` does.
pub(crate) fn list_sql() -> String {
    format!(
        "{CONV_SELECT}
         WHERE (?1 IS NULL OR c.account_id = ?1) AND (?2 IS NULL OR c.archived = ?2)
           AND a.state <> 'Removing'
           AND c.hidden = 0
           AND (?3 = 0 OR ((c.pinned_order IS NULL), coalesce(c.pinned_order, 0),
                -coalesce(c.last_activity, 0), -c.id) > (?4, ?5, ?6, ?7))
         ORDER BY (c.pinned_order IS NULL), coalesce(c.pinned_order, 0),
           -coalesce(c.last_activity, 0), -c.id
         LIMIT ?8"
    )
}

/// A page of the conversation list: pinned first (pin order), then newest activity.
pub(crate) fn conversations(
    conn: &Connection,
    q: &ConversationListQuery,
) -> Result<ConversationPage> {
    let limit = q.limit.clamp(1, MAX_PAGE);
    let sql = list_sql();
    let cur = q.after;
    let raws: Vec<ConvRaw> = conn
        .prepare_cached(&sql)?
        .query_map(
            kourier_db::rusqlite::params![
                q.account.map(|a| a.0),
                q.archived,
                cur.is_some(),
                cur.map_or(0, |c| i64::from(c.pinned.is_none())),
                cur.and_then(|c| c.pinned).map_or(0, i64::from),
                cur.map_or(0, |c| -c.last_activity),
                cur.map_or(0, |c| -c.id.0),
                i64::from(limit) + 1,
            ],
            conv_raw,
        )?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let more = raws.len() > usize::try_from(limit).unwrap_or(usize::MAX);
    let accounts = roaming::accounts(conn)?;
    let mut rows = raws
        .into_iter()
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
        .map(|r| conv_row(conn, r, &accounts))
        .collect::<Result<Vec<_>>>()?;
    let next = if more {
        rows.last().map(ConversationRow::cursor)
    } else {
        None
    };
    rows.shrink_to_fit();
    Ok(ConversationPage { rows, next })
}

/// One conversation with members and features.
pub(crate) fn conversation(
    conn: &Connection,
    id: ConversationId,
) -> Result<Option<ConversationDetail>> {
    let sql = format!("{CONV_SELECT} WHERE c.id = ?1");
    let raw = conn
        .prepare_cached(&sql)?
        .query_row([id.0], conv_raw)
        .optional()?;
    let Some(raw) = raw else { return Ok(None) };
    let (features, timer, draft): (Option<String>, Option<i64>, Option<String>) = conn
        .prepare_cached("SELECT features, timer_secs, draft FROM conversations WHERE id = ?1")?
        .query_row([id.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    let members = conn
        .prepare_cached(
            "SELECT user_id, role FROM members WHERE conversation_id = ?1 ORDER BY rowid",
        )?
        .query_map([id.0], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<kourier_db::rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(u, role)| {
            Ok(Member {
                user: sql::user(&u)?,
                role: sql::from_enum_name(&role)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let accounts = roaming::accounts(conn)?;
    Ok(Some(ConversationDetail {
        row: conv_row(conn, raw, &accounts)?,
        members,
        features: sql::from_json_opt::<ConversationFeatures>(features)?.unwrap_or_default(),
        timer: timer.map(|s| Duration::from_secs(u64::try_from(s.max(0)).unwrap_or(0))),
        draft: sql::from_json_opt(draft)?,
    }))
}

/// The groups of `account` in which `user` AND one of the account's own ids are full members
/// (`Member` or `Admin`: never one we left or are only invited to, never one `user` only asked
/// to join), deleted chats left out, archived ones in, and a group whose stored features say
/// `NotMember` left out (a group we were removed from before the backend could send the member list:
/// P1 F4). Starts at `members_by_user`, then the
/// group's own row and our own membership by the primary key of `members`
/// (`person::tests::the_groups_in_common_start_at_the_members_index` pins the plan).
pub(crate) const GROUPS_IN_COMMON: &str = "SELECT c.id, c.title, c.avatar_file
    FROM members mb
    CROSS JOIN conversations c ON c.id = mb.conversation_id
    WHERE mb.user_id = ?1 AND mb.role IN ('Member', 'Admin')
      AND c.account_id = ?2 AND c.kind = 'Group' AND c.hidden = 0
      AND coalesce(json_extract(c.features, '$.cannot_send'), '') != 'NotMember'
      AND EXISTS (SELECT 1 FROM members me
                  WHERE me.conversation_id = c.id AND me.role IN ('Member', 'Admin')
                    AND me.user_id IN (SELECT value FROM json_each(?3)))
    ORDER BY c.id";

/// One person for their page ([`crate::facade::Query::Person`]); `None` when the account does
/// not exist. `user` may be an alias (a number, a PNI): the contact, the chat and the groups
/// are the canonical person's.
pub(crate) fn person(
    conn: &Connection,
    account: AccountId,
    user: &UserId,
) -> Result<Option<PersonDetail>> {
    let acc: Option<(String, Option<String>, Option<String>)> = conn
        .prepare_cached("SELECT own_ids, display_name, number FROM accounts WHERE id = ?1")?
        .query_row([account.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    let Some((own_ids_json, own_name, own_number)) = acc else {
        return Ok(None);
    };
    let own_ids: Vec<UserId> = sql::from_json(&own_ids_json)?;
    let canonical = sql::user(&resolve_alias(conn, account.0, user)?)?;
    let is_self = own_ids.contains(&canonical) || own_ids.contains(user);
    let contact = contact_of(conn, account, &canonical)?;
    let chat = match sql::conversation_id(
        conn,
        account,
        &kourier_model::ConvKey::Direct(canonical.clone()),
    )? {
        Some(id) => {
            let (features, timer, blocked, hidden): (Option<String>, Option<i64>, bool, bool) =
                conn.prepare_cached(
                    "SELECT features, timer_secs, blocked, hidden FROM conversations WHERE id = ?1",
                )?
                .query_row([id.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            Some(PersonChat {
                id,
                timer: timer.map(|s| Duration::from_secs(u64::try_from(s.max(0)).unwrap_or(0))),
                features: sql::from_json_opt::<ConversationFeatures>(features)?.unwrap_or_default(),
                blocked,
                hidden,
            })
        }
        None => None,
    };
    // The own page has no groups in common (the account is in all of its own groups, so the
    // query would list every one of them; Signal's own page has no such section).
    let groups: Vec<(i64, Option<String>, Option<i64>)> = if is_self {
        Vec::new()
    } else {
        let own_json = sql::to_json(&own_ids)?;
        conn.prepare_cached(GROUPS_IN_COMMON)?
            .query_map((canonical.as_str(), account.0, own_json), |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<kourier_db::rusqlite::Result<_>>()?
    };
    let groups = groups
        .into_iter()
        .map(|(id, title, avatar)| {
            Ok(CommonGroup {
                id: ConversationId(id),
                title,
                avatar: blob_ref(conn, avatar)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(PersonDetail {
        account,
        user: canonical,
        is_self,
        contact,
        own_name: is_self.then_some(own_name).flatten(),
        own_number: is_self.then_some(own_number).flatten(),
        chat,
        groups,
        // The descriptor's, filled in by the runtime ([`account_network`]).
        safety_numbers: false,
        // Likewise (the paused queues are the runtime's).
        safety_waits: false,
        // Likewise (the network makes the link).
        link: crate::facade::LinkState::Hidden,
        share: crate::facade::ShareState::Hidden,
    }))
}

/// The network of `account` (`None`: no such account).
pub(crate) fn account_network(
    conn: &Connection,
    account: AccountId,
) -> Result<Option<kourier_model::NetworkKind>> {
    let network: Option<String> = conn
        .prepare_cached("SELECT network FROM accounts WHERE id = ?1")?
        .query_row([account.0], |r| r.get(0))
        .optional()?;
    network.as_deref().map(sql::network).transpose()
}

/// The member rows of a group for its page, each with the core's name for the person
/// (`display_name`'s rule: nickname, address book, profile) and the picture's file in the same
/// statement: ONE statement for the whole list, not one per member (a group can hold 1,000).
/// Starts at the group's members by the primary key of `members`; the contact by the unique key
/// of `contacts` (`account_id, user_id`) and its picture by the primary key of `files`.
/// `core.db` has no planner statistics: the LEFT JOINs fix the order (members outer), and
/// `query::tests::the_group_members_start_at_the_group` pins the plan.
pub(crate) const GROUP_MEMBERS: &str = concat!(
    "SELECT m.user_id, m.role, coalesce(ct.nickname, ct.system_name, ",
    shown_profile_name!("ct", "(SELECT network FROM accounts WHERE id = ?2)"),
    "), f.path, f.key
    FROM members m
    LEFT JOIN contacts ct ON ct.account_id = ?2 AND ct.user_id = m.user_id
    LEFT JOIN files f ON f.id = ct.avatar_file
    WHERE m.conversation_id = ?1"
);

/// One group for its page ([`crate::facade::Query::Group`]); `None` when the conversation does not
/// exist or is not a group (a 1:1 chat, a channel, a list: they have no member list to show).
pub(crate) fn group(conn: &Connection, id: ConversationId) -> Result<Option<GroupDetail>> {
    type Raw = (
        i64,
        String,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<i64>,
        Option<String>,
        bool,
    );
    let raw: Option<Raw> = conn
        .prepare_cached(
            "SELECT account_id, kind, title, avatar_file, description, timer_secs, features, blocked
             FROM conversations WHERE id = ?1",
        )?
        .query_row([id.0], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })
        .optional()?;
    let Some((account, kind, title, avatar, description, timer, features, blocked)) = raw else {
        return Ok(None);
    };
    if sql::from_enum_name::<kourier_model::ConvKind>(&kind)? != kourier_model::ConvKind::Group {
        return Ok(None);
    }
    let own_json: Option<String> = conn
        .prepare_cached("SELECT own_ids FROM accounts WHERE id = ?1")?
        .query_row([account], |r| r.get(0))
        .optional()?;
    let own_ids: Vec<UserId> = match own_json {
        Some(json) => sql::from_json(&json)?,
        None => Vec::new(),
    };
    type Row = (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<Vec<u8>>,
    );
    let rows: Vec<Row> = conn
        .prepare_cached(GROUP_MEMBERS)?
        .query_map((id.0, account), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let members = rows
        .into_iter()
        .map(|(user, role, name, path, key)| {
            let user = sql::user(&user)?;
            Ok(GroupMember {
                is_self: own_ids.contains(&user),
                user,
                role: sql::from_enum_name(&role)?,
                name,
                avatar: path.zip(key).and_then(|(p, k)| blob_of(p, k)),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(GroupDetail {
        account: AccountId(account),
        id,
        title,
        avatar: blob_ref(conn, avatar)?,
        description,
        timer: timer.map(|s| Duration::from_secs(u64::try_from(s.max(0)).unwrap_or(0))),
        features: sql::from_json_opt::<ConversationFeatures>(features)?.unwrap_or_default(),
        blocked,
        members,
        invites: kourier_model::GroupInvites::None,
    }))
}

/// What a gallery shows of a message and of an attachment ([`chat_media`]), once: the message is
/// not deleted (for me or for everyone), is not view-once and is not hidden because its time has
/// come on an unconfirmed clock; the attachment is not the rest of a long text and is not one
/// that cannot be had ("Not available on this device", "No longer available", a phone that
/// said no): such a thing has nothing to show or open. The state is JSON: a unit variant is its
/// name in quotes, `AskFailed` an object, matched by its TOTAL prefix (never `json_extract`, as
/// the quiet notices' rule: the statement runs over every attachment of a chat).
const GALLERY_SHOWN: &str = "m.deleted IS NULL AND m.view_once = 0 AND m.expiry_hidden = 0
      AND a.long_text = 0 AND a.state NOT IN ('\"Unavailable\"', '\"Expired\"')
      AND substr(a.state, 1, 12) <> '{\"AskFailed\"'";

/// The attachment kinds of a gallery kind, as `attachments.kind` stores them. Voice notes are in
/// neither list (and a sticker is a message of body kind `Sticker`, not `Media`).
const VISUAL_KINDS: &str = "('Image', 'Video', 'Gif')";
const FILE_KINDS: &str = "('File', 'Audio')";

fn gallery_kinds(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Visual => VISUAL_KINDS,
        MediaKind::Files => FILE_KINDS,
    }
}

/// The index and the flag of a gallery kind (`0020_chat_media.sql`): the partial index of the
/// media messages that have an attachment of that kind, and the flag that says so. A statement
/// states BOTH terms of the index's WHERE (`body_kind = 'Media'` and the flag) and names the
/// index (`INDEXED BY`), so it is an error and not a slow scan if the index ever stops applying
/// (`core.db` has no planner statistics), and it never walks a message of the other kind.
fn gallery_index(kind: MediaKind) -> (&'static str, &'static str) {
    match kind {
        MediaKind::Visual => ("messages_media_visual", "media_visual"),
        MediaKind::Files => ("messages_media_files", "media_files"),
    }
}

/// The statement of a gallery page: `?1` the chat, `?2` the limit, `?3 ?4 ?5` the position
/// (`sort_key`, message, `idx`) the page goes on after. It starts at the PARTIAL index of its
/// kind ([`gallery_index`]), by the row-value range `(sort_key, id) <= (?3, ?4)` of the
/// timeline's own statement; the attachments of the message the position is in that were
/// delivered already are cut by a plain filter (an `OR` there would invite the planner to a
/// multi-index plan), and the attachments of one message come in `idx` order from the unique key
/// of `attachments` (the join is nested, the outer rows are already in order: no sort).
fn gallery_sql(kind: MediaKind) -> String {
    let (index, flag) = gallery_index(kind);
    format!(
        "SELECT m.id, m.sort_key, a.idx, a.id, a.kind, a.mime, a.size, a.file_name, a.width,
                a.height, a.duration_ms, a.blurhash, a.state
         FROM messages m INDEXED BY {index}
         CROSS JOIN attachments a ON a.message_id = m.id
         WHERE m.conversation_id = ?1 AND m.body_kind = 'Media' AND m.{flag} = 1
           AND (m.sort_key, m.id) <= (?3, ?4)
           AND NOT (m.sort_key = ?3 AND m.id = ?4 AND a.idx <= ?5)
           AND a.kind IN {} AND {GALLERY_SHOWN}
         ORDER BY m.sort_key DESC, m.id DESC, a.idx
         LIMIT ?2",
        gallery_kinds(kind)
    )
}

/// Whether the chat has a gallery item of this kind: one row, by the same index and rules.
fn gallery_has_any(kind: MediaKind) -> String {
    let (index, flag) = gallery_index(kind);
    format!(
        "SELECT EXISTS (SELECT 1 FROM messages m INDEXED BY {index}
           CROSS JOIN attachments a ON a.message_id = m.id
           WHERE m.conversation_id = ?1 AND m.body_kind = 'Media' AND m.{flag} = 1
             AND a.kind IN {} AND {GALLERY_SHOWN})",
        gallery_kinds(kind)
    )
}

/// A page of one chat's gallery ([`crate::facade::Query::ChatMedia`]): the attachments of the
/// shown messages of one kind, newest message first. `more` is whether any follow the last item
/// (one row more than asked is read); the first page also says whether the other kind has any
/// (`other`).
pub(crate) fn chat_media(conn: &Connection, q: &ChatMediaQuery) -> Result<ChatMediaPage> {
    let limit = i64::from(q.limit.clamp(1, MAX_MEDIA_PAGE));
    let (key, message, idx) = match q.before {
        Some(c) => (c.sort_key, c.message.0, c.idx.map_or(-1, i64::from)),
        None => (i64::MAX, i64::MAX, -1),
    };
    type Raw = (
        i64,
        i64,
        i64,
        i64,
        String,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        String,
    );
    let raws: Vec<Raw> = conn
        .prepare_cached(&gallery_sql(q.kind))?
        .query_map((q.conv.0, limit + 1, key, message, idx), |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
                r.get(12)?,
            ))
        })?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let more = raws.len() > usize::try_from(limit).unwrap_or(usize::MAX);
    let items = raws
        .into_iter()
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
        .map(
            |(msg, sort_key, idx, id, kind, mime, size, name, w, h, dur, blurhash, state)| {
                let state = sql::from_json::<AttachmentState>(&state)?;
                Ok(ChatMediaItem {
                    message: MessageId(msg),
                    sort_key,
                    idx: sql::u32_of(idx),
                    attachment: Attachment {
                        id: Some(AttachmentId(id)),
                        kind: sql::from_enum_name(&kind)?,
                        mime: mime.unwrap_or_default(),
                        size: size.and_then(|s| u64::try_from(s).ok()).unwrap_or(0),
                        file_name: name,
                        dims: w.zip(h).map(|(w, h)| (sql::u32_of(w), sql::u32_of(h))),
                        duration_ms: dur.map(sql::u32_of),
                        blurhash,
                        state,
                        ..Attachment::default()
                    },
                    // (`GALLERY_SHOWN` leaves out the messages and the long texts the rule refuses.)
                    save: crate::actions::may_save_file(state, false).is_ok(),
                })
            },
        )
        .collect::<Result<Vec<_>>>()?;
    let other = q.before.is_none()
        && conn
            .prepare_cached(&gallery_has_any(match q.kind {
                MediaKind::Visual => MediaKind::Files,
                MediaKind::Files => MediaKind::Visual,
            }))?
            .query_row([q.conv.0], |r| r.get::<_, bool>(0))?;
    Ok(ChatMediaPage {
        conv: q.conv,
        kind: q.kind,
        items,
        more,
        other,
    })
}

/// The media handles of the attachments of a gallery page (registered when the page is answered,
/// like a timeline page's, §8.6): `image://kourier/thumb/<id>` draws nothing without one.
pub(crate) fn chat_media_handles(conn: &Connection, page: &ChatMediaPage) -> Result<Vec<RawMedia>> {
    let mut out = Vec::new();
    for item in &page.items {
        if let Some(id) = item.attachment.id
            && let Some(raw) = media_entry(conn, id)?
        {
            out.push(raw);
        }
    }
    Ok(out)
}

/// The contact row of `user` for a page: every column, as the ingest side loads it.
fn contact_of(conn: &Connection, account: AccountId, user: &UserId) -> Result<Option<Contact>> {
    type Row = (
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        String,
        Option<Vec<u8>>,
        bool,
        bool,
        Option<i64>,
        String,
        Option<Vec<u8>>,
        Option<String>,
        Option<String>,
        Option<bool>,
    );
    let row: Option<Row> = conn
        .prepare_cached(
            "SELECT id, profile_name, system_name, nickname, avatar_file, trust, fingerprint,
               blocked, hidden, unregistered_at, identifiers, meta, about, about_emoji,
               number_shown
             FROM contacts WHERE account_id = ?1 AND user_id = ?2",
        )?
        .query_row((account.0, user.as_str()), |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
                r.get(12)?,
                r.get(13)?,
                r.get(14)?,
            ))
        })
        .optional()?;
    let Some(r) = row else { return Ok(None) };
    Ok(Some(Contact {
        id: kourier_model::ContactId(r.0),
        account,
        user: user.clone(),
        profile_name: r.1,
        system_name: r.2,
        nickname: r.3,
        avatar: blob_ref(conn, r.4)?,
        trust: sql::from_enum_name(&r.5)?,
        identity_fingerprint: r.6,
        blocked: r.7,
        hidden: r.8,
        unregistered_at: r.9,
        identifiers: sql::from_json(&r.10)?,
        meta: sql::blob_from_sql(r.11),
        about: r.12,
        about_emoji: r.13,
        number_shown: r.14,
    }))
}

const MSG_SELECT: &str = "SELECT id, sort_key, net_author, origin, device, body, reply_to, sent_at,
      server_at, received_at, revision, edited_at, deleted, deleted_by, out_state, read_state,
      expiry_kind, expiry_timer_ms, expiry_started_at, view_once, history, mentions_me, account_id,
      net_local <> ''
    FROM messages";

struct MsgRaw {
    id: i64,
    sort_key: i64,
    author: String,
    origin: String,
    device: Option<u32>,
    body: String,
    reply_to: Option<String>,
    sent_at: i64,
    server_at: Option<i64>,
    received_at: i64,
    revision: i64,
    edited_at: Option<i64>,
    deleted: Option<String>,
    deleted_by: Option<String>,
    out_state: Option<String>,
    read_state: Option<String>,
    expiry_kind: Option<String>,
    expiry_ms: Option<i64>,
    expiry_started: Option<i64>,
    view_once: bool,
    history: bool,
    mentions_me: bool,
    account: i64,
    /// The message has a network id the other device can be asked about.
    named: bool,
}

fn msg_raw(r: &Row<'_>) -> kourier_db::rusqlite::Result<MsgRaw> {
    Ok(MsgRaw {
        id: r.get(0)?,
        sort_key: r.get(1)?,
        author: r.get(2)?,
        origin: r.get(3)?,
        device: r.get(4)?,
        body: r.get(5)?,
        reply_to: r.get(6)?,
        sent_at: r.get(7)?,
        server_at: r.get(8)?,
        received_at: r.get(9)?,
        revision: r.get(10)?,
        edited_at: r.get(11)?,
        deleted: r.get(12)?,
        deleted_by: r.get(13)?,
        out_state: r.get(14)?,
        read_state: r.get(15)?,
        expiry_kind: r.get(16)?,
        expiry_ms: r.get(17)?,
        expiry_started: r.get(18)?,
        view_once: r.get(19)?,
        history: r.get(20)?,
        mentions_me: r.get(21)?,
        account: r.get(22)?,
        named: r.get(23)?,
    })
}

/// What every row of one timeline page shares: the conversation's facts for the action flags
/// ([`actions`]) and the lookups cached across rows.
struct PageCtx {
    conv: ConversationId,
    /// The core clock the flags are computed for.
    now: i64,
    /// The account is linked.
    active: bool,
    /// The account's network has a backend in this run (a paused account's messages offer
    /// nothing the core would refuse, [`Facts::registered`]).
    registered: bool,
    /// The account's network can ask its other device for lost files
    /// (`AccountCaps::restores_attachments`).
    restores: bool,
    /// The network's receive limit (`AccountCaps::max_receive_attachment_bytes`).
    receive_limit: Option<u64>,
    /// The chat is with a person known by phone number alone: read-only, and the other device
    /// cannot be asked about its messages (the answer could not be placed).
    number_only: bool,
    blocked: bool,
    /// The chat with ourselves.
    note_to_self: bool,
    /// The conversation is a group (`ConvKey::Group`): its messages name their sender.
    group: bool,
    features: Option<ConversationFeatures>,
    /// One of our ids is an admin of the conversation.
    admin: bool,
    own_ids: Vec<UserId>,
    /// Reactions per user (`None` = a set).
    per_user: Option<u8>,
    /// Own messages that still have their send row in the outbox → that row's state
    /// (`waits` false: it failed and is only kept for "Retry").
    queued: HashMap<i64, QueuedRaw>,
    /// The messages of this conversation whose backend reports an upload that is still moving
    /// ([`crate::runtime::Shared::uploading`]; empty for a paused account, which has no
    /// backend): one term of [`actions::stoppable`].
    uploading: HashSet<i64>,
    /// Whose safety number the conversation's queue waits for (set by [`timeline`]; `None`
    /// for a paused account, which has no backend to confirm with).
    paused: Option<UserId>,
    /// Display name and avatar per author.
    people: HashMap<String, (Option<String>, Option<BlobRef>)>,
}

impl PageCtx {
    fn load(
        conn: &Connection,
        conv: ConversationId,
        now: i64,
        registered: bool,
        uploading: &HashSet<MessageId>,
    ) -> Result<Self> {
        type Raw = (
            bool,
            Option<String>,
            String,
            Option<String>,
            String,
            bool,
            Option<String>,
            String,
        );
        let row: Option<Raw> = conn
            .prepare_cached(
                "SELECT c.blocked, c.features, a.state, a.state_reason, a.own_ids, c.is_self,
                   a.caps, c.conv_key
                 FROM conversations c JOIN accounts a ON a.id = c.account_id WHERE c.id = ?1",
            )?
            .query_row([conv.0], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            })
            .optional()?;
        let (blocked, features, state, reason, own_ids, note_to_self, caps, conv_key) = row
            .unwrap_or_else(|| {
                (
                    false,
                    None,
                    "Removing".into(),
                    None,
                    "[]".into(),
                    false,
                    None,
                    String::new(),
                )
            });
        let caps: Option<kourier_model::AccountCaps> = sql::from_json_opt(caps)?;
        let features: Option<ConversationFeatures> = sql::from_json_opt(features)?;
        let own_ids: Vec<UserId> = sql::from_json(&own_ids)?;
        let admin_role = sql::enum_name(&MemberRole::Admin)?;
        let mut admin = false;
        for own in &own_ids {
            let is: bool = conn
                .prepare_cached(
                    "SELECT EXISTS(SELECT 1 FROM members WHERE conversation_id = ?1
                       AND user_id = ?2 AND role = ?3)",
                )?
                .query_row((conv.0, own.as_str(), &admin_role), |r| r.get(0))?;
            admin |= is;
        }
        // The same condition as `actions::send_waits`, for the whole page at once.
        let queued: HashMap<i64, QueuedRaw> = conn
            .prepare_cached(
                "SELECT message_id, state <> 'Failed', state = ?2, attempts, next_attempt_at,
                   last_error
                 FROM outbox WHERE conversation_id = ?1 AND kind = 'Message'",
            )?
            .query_map((conv.0, crate::outbox::TRANSMITTING), |r| {
                Ok((
                    r.get(0)?,
                    QueuedRaw {
                        waits: r.get(1)?,
                        in_flight: r.get(2)?,
                        attempts: r.get(3)?,
                        next_attempt_at: r.get(4)?,
                        last_error: r.get(5)?,
                    },
                ))
            })?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        Ok(Self {
            conv,
            now,
            active: sql::account_state(&state, reason)? == AccountState::Active,
            registered,
            uploading: if registered {
                uploading.iter().map(|m| m.0).collect()
            } else {
                HashSet::new()
            },
            paused: None,
            restores: caps
                .as_ref()
                .is_some_and(kourier_model::AccountCaps::restores_attachments),
            receive_limit: caps.and_then(|c| c.max_receive_attachment_bytes),
            number_only: sql::conv_key(&conv_key).is_ok_and(|k| k.is_number_only()),
            group: sql::conv_key(&conv_key)
                .is_ok_and(|k| matches!(k, kourier_model::ConvKey::Group(_))),
            blocked,
            note_to_self,
            per_user: match &features {
                Some(f) => f.reactions_per_user,
                // Not reported yet: Signal and WhatsApp semantics (one reaction per user).
                None => Some(1),
            },
            features,
            admin,
            own_ids,
            queued,
            people: HashMap::new(),
        })
    }

    /// Display name and avatar of a canonical author (cached per page).
    fn person(
        &mut self,
        conn: &Connection,
        account: i64,
        user: &str,
    ) -> Result<(Option<String>, Option<BlobRef>)> {
        if let Some(found) = self.people.get(user) {
            return Ok(found.clone());
        }
        let found = (
            display_name(conn, account, user)?,
            contact_avatar(conn, account, user)?,
        );
        self.people.insert(user.to_owned(), found.clone());
        Ok(found)
    }
}

/// Not what a delete for me hid, and not what is hidden because its time has come on a clock
/// nobody confirmed (`expiry_hidden`, [`crate::expiry`]).
const TIMELINE_VISIBLE: &str =
    "conversation_id = ?1 AND (deleted IS NULL OR deleted <> 'ForMe') AND expiry_hidden = 0";

/// The statement of the timeline's older side (`op` is `<` or `<=`): its own text, so a plan
/// test can name the index it uses (`messages_timeline`, never one of the gallery's `messages_media_*`).
pub(crate) fn timeline_older_sql(op: &str) -> String {
    format!(
        "{MSG_SELECT} WHERE {TIMELINE_VISIBLE} AND (sort_key, id) {op} (?2, ?3)
             ORDER BY sort_key DESC, id DESC LIMIT ?4"
    )
}

/// A timeline page, oldest first, keyset-paged by `(sort_key, id)`. `now` is the core clock:
/// the rows' action flags are computed for it. `registered`: the conversation's network has a
/// backend in this run (a paused account's rows offer only what needs no network).
/// `uploading`: the messages of the conversation whose upload is reported moving (the stop
/// rule's second term, [`actions::stoppable`]). `paused`: the conversation's queue waits for
/// the user to confirm this person's safety number ([`crate::runtime::Shared::paused`]): every
/// own message of the queue that still waits (the paused head and the rows behind it) offers
/// "Show <name>'s Safety Number" ([`MessageActions::safety_number`], P5a BQ-3, U4).
pub(crate) fn timeline(
    conn: &Connection,
    q: &TimelineQuery,
    now: i64,
    registered: bool,
    uploading: &HashSet<MessageId>,
    paused: Option<UserId>,
) -> Result<TimelinePage> {
    let limit = i64::from(q.limit.clamp(1, MAX_PAGE));
    let mut ctx = PageCtx::load(conn, q.conv, now, registered, uploading)?;
    ctx.paused = paused.filter(|_| registered);
    let visible = TIMELINE_VISIBLE;
    let older = |key: i64, id: i64, n: i64, inclusive: bool| -> Result<Vec<MsgRaw>> {
        let sql = timeline_older_sql(if inclusive { "<=" } else { "<" });
        Ok(conn
            .prepare_cached(&sql)?
            .query_map((q.conv.0, key, id, n), msg_raw)?
            .collect::<kourier_db::rusqlite::Result<_>>()?)
    };
    let newer = |key: i64, id: i64, n: i64, inclusive: bool| -> Result<Vec<MsgRaw>> {
        let op = if inclusive { ">=" } else { ">" };
        let sql = format!(
            "{MSG_SELECT} WHERE {visible} AND (sort_key, id) {op} (?2, ?3)
             ORDER BY sort_key ASC, id ASC LIMIT ?4"
        );
        Ok(conn
            .prepare_cached(&sql)?
            .query_map((q.conv.0, key, id, n), msg_raw)?
            .collect::<kourier_db::rusqlite::Result<_>>()?)
    };
    let exists = |key: i64, id: i64, older_side: bool| -> Result<bool> {
        let op = if older_side { "<" } else { ">" };
        let sql = format!(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE {visible} AND (sort_key, id) {op} (?2, ?3))"
        );
        Ok(conn
            .prepare_cached(&sql)?
            .query_row((q.conv.0, key, id), |r| r.get(0))?)
    };
    let (mut raws, has_older, has_newer) = match q.anchor {
        TimelineAnchor::Newest => {
            let mut v = older(i64::MAX, i64::MAX, limit + 1, true)?;
            let more = v.len() > usize::try_from(limit).unwrap_or(usize::MAX);
            v.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
            v.reverse();
            (v, more, false)
        }
        TimelineAnchor::Before(c) => {
            let mut v = older(c.sort_key, c.id.0, limit + 1, false)?;
            let more = v.len() > usize::try_from(limit).unwrap_or(usize::MAX);
            v.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
            v.reverse();
            (v, more, exists(c.sort_key, c.id.0 - 1, false)?)
        }
        TimelineAnchor::After(c) => {
            let mut v = newer(c.sort_key, c.id.0, limit + 1, false)?;
            let more = v.len() > usize::try_from(limit).unwrap_or(usize::MAX);
            v.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
            (v, exists(c.sort_key, c.id.0 + 1, true)?, more)
        }
        TimelineAnchor::Around(id) => {
            let at: Option<i64> = conn
                .prepare_cached(
                    "SELECT sort_key FROM messages WHERE id = ?1 AND conversation_id = ?2
                       AND expiry_hidden = 0",
                )?
                .query_row((id.0, q.conv.0), |r| r.get(0))
                .optional()?;
            match at {
                None => (Vec::new(), false, false),
                Some(key) => {
                    let half = (limit / 2).max(1);
                    let mut before = older(key, id.0, half + 1, false)?;
                    let more_old = before.len() > usize::try_from(half).unwrap_or(usize::MAX);
                    before.truncate(usize::try_from(half).unwrap_or(usize::MAX));
                    before.reverse();
                    let rest = (limit - half).max(1);
                    let mut after = newer(key, id.0, rest + 1, true)?;
                    let more_new = after.len() > usize::try_from(rest).unwrap_or(usize::MAX);
                    after.truncate(usize::try_from(rest).unwrap_or(usize::MAX));
                    before.extend(after);
                    (before, more_old, more_new)
                }
            }
        }
    };
    let rows = raws
        .drain(..)
        .map(|raw| message_row(conn, raw, &mut ctx))
        .collect::<Result<Vec<_>>>()?;
    Ok(TimelinePage {
        conv: q.conv,
        rows,
        has_older,
        has_newer,
        now,
    })
}

/// The outbox columns of an own message's send row (§4.5), for [`MessageRow::outbox`].
#[derive(Clone)]
struct QueuedRaw {
    /// Queued, being stored or in flight (not `Failed`).
    waits: bool,
    in_flight: bool,
    attempts: i64,
    next_attempt_at: Option<i64>,
    last_error: Option<String>,
}

impl QueuedRaw {
    fn wait(&self) -> Option<OutboxWait> {
        self.waits.then(|| OutboxWait {
            in_flight: self.in_flight,
            attempts: u32::try_from(self.attempts.max(0)).unwrap_or(u32::MAX),
            // Only a row that waits for its next try has a time that means something.
            next_attempt_at: if self.in_flight {
                None
            } else {
                self.next_attempt_at
            },
            last_error: self.last_error.clone(),
        })
    }
}

/// The attachment columns a timeline row overlays on the stored body.
struct AttRaw {
    id: i64,
    idx: i64,
    state: String,
    mime: Option<String>,
    size: Option<i64>,
    width: Option<i64>,
    height: Option<i64>,
    duration_ms: Option<i64>,
    blurhash: Option<String>,
}

fn message_row(conn: &Connection, raw: MsgRaw, ctx: &mut PageCtx) -> Result<MessageRow> {
    let mut body: Body = sql::from_json(&raw.body)?;
    // Attachment ids, download states and what the core learned since (size, type, dimensions,
    // blurhash) live in the attachment rows. Keys and network pointers never reach the UI: the
    // media handle table has the files (§8.6).
    let atts: Vec<AttRaw> = conn
        .prepare_cached(
            "SELECT id, idx, state, mime, size, width, height, duration_ms, blurhash
             FROM attachments WHERE message_id = ?1",
        )?
        .query_map([raw.id], |r| {
            Ok(AttRaw {
                id: r.get(0)?,
                idx: r.get(1)?,
                state: r.get(2)?,
                mime: r.get(3)?,
                size: r.get(4)?,
                width: r.get(5)?,
                height: r.get(6)?,
                duration_ms: r.get(7)?,
                blurhash: r.get(8)?,
            })
        })?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let mut by_idx: BTreeMap<i64, AttRaw> = BTreeMap::new();
    for a in atts {
        by_idx.insert(a.idx, a);
    }
    let list: Vec<&mut kourier_model::Attachment> = match &mut body {
        Body::Media { attachments, .. } => attachments.iter_mut().collect(),
        Body::Sticker(a) => vec![a.as_mut()],
        _ => Vec::new(),
    };
    for (idx, a) in list.into_iter().enumerate() {
        a.local = None;
        a.thumb = None;
        a.remote = None;
        a.net_ids.clear();
        if let Some(row) = by_idx.get(&i64::try_from(idx).unwrap_or(i64::MAX)) {
            a.id = Some(AttachmentId(row.id));
            a.state = sql::from_json::<AttachmentState>(&row.state)?;
            if let Some(m) = &row.mime {
                a.mime.clone_from(m);
            }
            if let Some(size) = row.size.and_then(|s| u64::try_from(s).ok()) {
                a.size = size;
            }
            if let (Some(w), Some(h)) = (row.width, row.height) {
                a.dims = Some((sql::u32_of(w), sql::u32_of(h)));
            }
            if let Some(d) = row.duration_ms {
                a.duration_ms = Some(sql::u32_of(d));
            }
            if row.blurhash.is_some() {
                a.blurhash.clone_from(&row.blurhash);
            }
        }
    }
    let author = (!raw.author.is_empty())
        .then(|| sql::user(&raw.author))
        .transpose()?;
    let (author_name, author_avatar) = if raw.author.is_empty() {
        // A call line has no author: who rang or started the call is in the line, and their
        // name is what a notification of the line says.
        match body.call_line().and_then(|line| line.by.as_ref()) {
            Some(by) => (ctx.person(conn, raw.account, by.as_str())?.0, None),
            None => (None, None),
        }
    } else {
        ctx.person(conn, raw.account, &raw.author)?
    };
    let reply = match raw.reply_to {
        Some(json) => Some(reply_preview(
            conn,
            raw.account,
            sql::from_json(&json)?,
            ctx,
        )?),
        None => None,
    };
    // The same facts and the same rules the commands check (`actions`).
    let own = raw.author.is_empty();
    let sending = if own && ctx.queued.contains_key(&raw.id) {
        actions::sending_of(conn, MessageId(raw.id), ctx.uploading.contains(&raw.id))?
    } else {
        Sending::Settled
    };
    let flags = actions::all(
        &Facts {
            active: ctx.active,
            registered: ctx.registered,
            blocked: ctx.blocked,
            features: ctx.features.as_ref(),
            admin: ctx.admin,
            own,
            body_kind: body.kind_name(),
            deleted: raw.deleted.as_deref(),
            sent_at: raw.sent_at,
            revision: u16::try_from(raw.revision.max(0)).unwrap_or(u16::MAX),
            media: actions::media_of(&body, ctx.features.as_ref()),
            has_text: actions::has_text(&body),
            sending,
            note_to_self: ctx.note_to_self,
            queued: own && ctx.queued.get(&raw.id).is_some_and(|q| q.waits),
        },
        ctx.now,
    );
    let mut flags = flags;
    // "Send Again" is accepted for an own message whose send row is in the outbox and not in
    // flight, unless the network refused it as a reach-out or the chat lost the right to send
    // (`actions::retry`, the ONE rule `RetryMessage` checks too). A row that waits has no
    // refusal of the network to read, but a chat that cannot send refuses it all the same.
    flags.retry = own
        && ctx.registered
        && match ctx.queued.get(&raw.id) {
            Some(q) if !q.in_flight => {
                let floor = if q.waits {
                    None
                } else {
                    crate::outbox::floor(conn, MessageId(raw.id))?
                };
                actions::retry(floor.as_ref(), ctx.features.as_ref()).is_ok()
            }
            _ => false,
        };
    // "Show Sender's Page" and one "Show <name>'s Page" per person the text mentions: from the
    // body's own ranges and the conversation's kind, never from the UI (`actions::show_people`).
    flags.show_sender = actions::show_sender(own, ctx.group);
    flags.show_people = actions::show_people(raw.deleted.is_some(), &body, &ctx.own_ids);
    // Every own message of a queue that waits for a safety number names the person (BQ-3, U4):
    // the paused head (shown as failed) and the rows behind it (waiting); a row that failed for
    // good (`waits` false) is not part of the wait.
    flags.safety_number = match (&ctx.paused, ctx.queued.get(&raw.id)) {
        (Some(user), Some(q)) if own && q.waits => Some(user.clone()),
        _ => None,
    };
    flags.ask_attachments = actions::ask_attachments(
        ctx.active
            && ctx.registered
            && ctx.restores
            && raw.named
            && !raw.view_once
            && !ctx.number_only
            && !sql::user(&raw.author).is_ok_and(|u| u.is_phone_number()),
        raw.deleted.as_deref(),
        &body,
        ctx.receive_limit,
    );
    // The timeline's SELECT leaves out the rows hidden on an unconfirmed clock
    // (`TIMELINE_VISIBLE`), so none reaches here; the rule asks all the same.
    flags.save_attachments =
        actions::save_attachments(raw.deleted.as_deref(), raw.view_once, false, &body);
    let deleted = deletion_of(raw.deleted.as_deref(), raw.deleted_by)?;
    let expiry = match (raw.expiry_kind, raw.expiry_ms) {
        (Some(kind), Some(ms)) => Some(Expiry {
            kind: sql::from_enum_name::<DisappearKind>(&kind)?,
            timer: Duration::from_millis(u64::try_from(ms.max(0)).unwrap_or(0)),
            started_at: raw.expiry_started,
        }),
        _ => None,
    };
    Ok(MessageRow {
        id: MessageId(raw.id),
        sort_key: raw.sort_key,
        author,
        author_name: author_name.map(Into::into),
        author_avatar,
        origin: match raw.origin.as_str() {
            "Remote" => OriginKind::Remote,
            "OwnThisDevice" => OriginKind::OwnThisDevice,
            _ => OriginKind::OwnOtherDevice,
        },
        device: raw.device,
        body,
        reply,
        sent_at: raw.sent_at,
        server_at: raw.server_at,
        received_at: raw.received_at,
        revision: u16::try_from(raw.revision.max(0)).unwrap_or(u16::MAX),
        edited_at: raw.edited_at,
        deleted,
        out_state: sql::from_json_opt(raw.out_state)?,
        read_state: raw
            .read_state
            .as_deref()
            .map(sql::from_enum_name)
            .transpose()?,
        reactions: reactions(conn, raw.id, ctx.per_user)?,
        expiry,
        view_once: raw.view_once,
        history: raw.history,
        mentions_me: raw.mentions_me,
        outbox: if own {
            ctx.queued.get(&raw.id).and_then(QueuedRaw::wait)
        } else {
            None
        },
        actions: flags,
    })
}

/// The text a version of a message shows: a text, or the caption of a `Media` body.
fn revision_text(body: Body) -> Option<Text> {
    match body {
        Body::Text(t) => Some(t),
        Body::Media { caption, .. } => caption,
        _ => None,
    }
}

/// [`Query::Revisions`](crate::facade::Query::Revisions): the versions of a message, oldest
/// first. A message that was never edited has one (its row); an edited one has its stored
/// revisions, in the order `reindex_revisions` numbers them; a deleted one has none.
pub(crate) fn revisions(conn: &Connection, id: MessageId) -> Result<Option<MessageRevisions>> {
    type Row = (i64, String, Option<String>, i64);
    let row: Option<Row> = conn
        .prepare_cached(
            "SELECT conversation_id, body, deleted, sent_at FROM messages
             WHERE id = ?1 AND expiry_hidden = 0",
        )?
        .query_row([id.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .optional()?;
    let Some((conv, body, deleted, sent_at)) = row else {
        return Ok(None);
    };
    let mut out = MessageRevisions {
        message: id,
        conv: ConversationId(conv),
        revisions: Vec::new(),
    };
    if deleted.is_some() {
        return Ok(Some(out));
    }
    let stored: Vec<(i64, Option<String>, Option<i64>)> = conn
        .prepare_cached(
            "SELECT revision, body, edited_at FROM message_revisions WHERE message_id = ?1
             ORDER BY revision, edited_at IS NOT NULL, edited_at, net_local",
        )?
        .query_map([id.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    if stored.is_empty() {
        out.revisions.push(Revision {
            revision: 0,
            text: revision_text(sql::from_json(&body)?),
            at: sent_at,
        });
        return Ok(Some(out));
    }
    for (revision, body, edited_at) in stored {
        // A version whose body is gone has nothing to show.
        let Some(body) = body else { continue };
        out.revisions.push(Revision {
            revision: u16::try_from(revision.max(0)).unwrap_or(u16::MAX),
            text: revision_text(sql::from_json(&body)?),
            at: edited_at.unwrap_or(sent_at),
        });
    }
    Ok(Some(out))
}

/// The visible reactions of a message: per user the newest `per_user` rows (all when `None`),
/// tombstones hidden (§3.5.1).
fn reactions(conn: &Connection, message: i64, per_user: Option<u8>) -> Result<Vec<ReactionGroup>> {
    let rows: Vec<(String, String, i64, bool)> = conn
        .prepare_cached(
            "SELECT user_id, emoji, at, removed FROM reactions WHERE message_id = ?1
             ORDER BY user_id, at DESC, removed DESC, emoji",
        )?
        .query_map([message], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let mut taken: HashMap<String, usize> = HashMap::new();
    let mut groups: Vec<(i64, ReactionGroup)> = Vec::new();
    for (user, emoji, at, removed) in rows {
        let n = taken.entry(user.clone()).or_default();
        if per_user.is_some_and(|limit| *n >= usize::from(limit)) {
            continue;
        }
        if removed {
            // With one reaction per user the newest entry is the state, a removal included
            // (a replaced reaction never comes back); in a set, a removal hides only itself.
            if per_user == Some(1) {
                *n += 1;
            }
            continue;
        }
        *n += 1;
        let who = (!user.is_empty()).then(|| sql::user(&user)).transpose()?;
        match groups.iter_mut().find(|(_, g)| g.emoji == emoji) {
            Some((first, g)) => {
                *first = (*first).min(at);
                g.users.push(who);
            }
            None => groups.push((
                at,
                ReactionGroup {
                    emoji,
                    users: vec![who],
                },
            )),
        }
    }
    groups.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.emoji.cmp(&b.1.emoji)));
    Ok(groups.into_iter().map(|(_, g)| g).collect())
}

/// SQL: the original of a quote of the conversation `?4`: the rows with the id `?2`, of the author
/// `?3` when one is bound: by their key, a revision's key, or an earlier name (KT-2: a quote made
/// before the server named the message).
pub(crate) const QUOTE_ORIGINAL: &str =
    "SELECT m.id, m.deleted, m.body, m.net_author FROM messages m
 WHERE m.account_id = ?1 AND m.conversation_id = ?4 AND m.net_local = ?2
   AND (?3 IS NULL OR m.net_author = ?3) AND m.expiry_hidden = 0
 UNION ALL
 SELECT m.id, m.deleted, m.body, m.net_author FROM message_revisions v
   JOIN messages m ON m.id = v.message_id
 WHERE v.account_id = ?1 AND m.conversation_id = ?4 AND v.net_local = ?2
   AND (?3 IS NULL OR v.net_author = ?3) AND m.expiry_hidden = 0
 UNION ALL
 SELECT m.id, m.deleted, m.body, m.net_author FROM message_aliases a
   CROSS JOIN messages m ON m.id = a.message_id
 WHERE a.account_id = ?1 AND a.net_local = ?2 AND m.conversation_id = ?4
   AND (?3 IS NULL OR m.net_author = ?3) AND m.expiry_hidden = 0
 LIMIT 4";

/// Resolves the quote of a row for display: the original (when it is a message of this
/// conversation, by its current or a revision key), who wrote it, and what the quote block
/// shows, from the sender's snapshot first and the original second.
fn reply_preview(
    conn: &Connection,
    account: i64,
    r: ReplyTo,
    ctx: &mut PageCtx,
) -> Result<ReplyPreview> {
    let canonical: Option<String> = match &r.target.author {
        TargetAuthor::Own => Some(String::new()),
        TargetAuthor::User(u) => {
            let c = resolve_alias(conn, account, u)?;
            // A peer quoting us names one of our ids.
            Some(if ctx.own_ids.iter().any(|own| own.as_str() == c) {
                String::new()
            } else {
                c
            })
        }
        TargetAuthor::Unknown => None,
    };
    // (id, deleted, body, author). An original that is hidden because its time has come
    // on an unconfirmed clock is not found, like one the sweeper erased: the quote keeps
    // only what its sender's snapshot says. A quote WITHOUT an author names its original by the
    // id alone, and under per-sender ids two members may share an id: it shows the original only
    // when exactly one message of the conversation has the id (a message and its own revisions
    // are one), else the sender's snapshot, as for a missing original. With an author, the row
    // of that author.
    type Found = (i64, Option<String>, String, String);
    let rows: Vec<Found> = conn
        .prepare_cached(QUOTE_ORIGINAL)?
        .query_map(
            (account, r.target.local.as_ref(), &canonical, ctx.conv.0),
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let ambiguous = canonical.is_none() && rows.iter().any(|f| f.0 != rows[0].0);
    let found: Option<Found> = if ambiguous {
        None
    } else {
        rows.into_iter().next()
    };
    let author_col = canonical.or_else(|| found.as_ref().map(|f| f.3.clone()));
    let own = author_col.as_deref() == Some("");
    let author_name = match author_col.as_deref() {
        Some(user) if !user.is_empty() => ctx.person(conn, account, user)?.0,
        _ => None,
    };
    let (state, target) = match &found {
        None => (QuoteState::Missing, None),
        Some((id, None, _, _)) => (QuoteState::Known, Some(MessageId(*id))),
        // The tombstone of a delete for everyone is a row to jump to; a delete for me hid it.
        Some((id, Some(kind), _, _)) => (
            QuoteState::Deleted,
            (kind != "ForMe").then_some(MessageId(*id)),
        ),
    };
    // What the original has now, for a quote that came without a snapshot of it.
    let mut text = None;
    let mut attachment = None;
    let mut file_name = None;
    let mut thumb = None;
    if let (QuoteState::Known, Some((id, _, body, _))) = (state, &found) {
        let (caption, first, sticker) = match sql::from_json::<Body>(body)? {
            Body::Text(t) => (Some(t), None, false),
            Body::Media {
                attachments,
                caption,
            } => (
                caption,
                attachments.into_iter().find(|a| !a.long_text),
                false,
            ),
            Body::Sticker(a) => (None, Some(*a), true),
            _ => (None, None, false),
        };
        text = caption;
        if let Some(a) = first {
            attachment = Some(a.kind);
            file_name = a.file_name;
            let picture = sticker
                || matches!(
                    a.kind,
                    AttachmentKind::Image | AttachmentKind::Gif | AttachmentKind::Video
                );
            if picture {
                thumb = conn
                    .prepare_cached(
                        "SELECT id FROM attachments WHERE message_id = ?1 AND long_text = 0
                         ORDER BY idx LIMIT 1",
                    )?
                    .query_row([*id], |row| row.get(0))
                    .optional()?
                    .map(AttachmentId);
            }
        }
    }
    let mut snapshot = r.snapshot;
    if let Some(s) = snapshot.as_mut() {
        // Only ever part of an outgoing request.
        s.thumb = None;
    }
    let from_snapshot = snapshot.as_ref();
    let filled = |t: &kourier_model::Text| !t.body.trim().is_empty();
    Ok(ReplyPreview {
        target,
        state,
        author: r.author,
        own,
        author_name: author_name.map(Into::into),
        text: from_snapshot
            .and_then(|s| s.text.clone())
            .filter(filled)
            .or(text.filter(filled)),
        attachment: from_snapshot.and_then(|s| s.attachment).or(attachment),
        file_name: from_snapshot
            .and_then(|s| s.file_name.clone())
            .or(file_name)
            .map(Into::into),
        thumb,
        snapshot,
    })
}

fn resolve_alias(conn: &Connection, account: i64, u: &UserId) -> Result<String> {
    let c: Option<String> = conn
        .prepare_cached("SELECT user_id FROM identity_alias WHERE account_id = ?1 AND alias = ?2")?
        .query_row((account, u.as_str()), |r| r.get(0))
        .optional()?;
    Ok(c.unwrap_or_else(|| u.as_str().to_owned()))
}

/// The canonical conversation id and key of a direct chat or any conversation (for typing and
/// presence).
pub(crate) fn conversation_by_key(
    conn: &Connection,
    account: AccountId,
    key: &kourier_model::ConvKey,
) -> Result<Option<ConversationId>> {
    let key = match key {
        kourier_model::ConvKey::Direct(u) => {
            kourier_model::ConvKey::Direct(sql::user(&resolve_alias(conn, account.0, u)?)?)
        }
        other => other.clone(),
    };
    sql::conversation_id(conn, account, &key)
}

/// `(account, key)` of a conversation.
pub(crate) fn conversation_key(
    conn: &Connection,
    id: ConversationId,
) -> Result<Option<(AccountId, kourier_model::ConvKey)>> {
    let row: Option<(i64, String)> = conn
        .prepare_cached("SELECT account_id, conv_key FROM conversations WHERE id = ?1")?
        .query_row([id.0], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    row.map(|(a, k)| Ok((AccountId(a), sql::conv_key(&k)?)))
        .transpose()
}

/// Who `target` stands for: the peer of a direct chat, the account's first own id. `None` when
/// there is nobody.
pub(crate) fn person_of(
    conn: &Connection,
    target: PersonRef,
) -> Result<Option<(AccountId, UserId)>> {
    match target {
        PersonRef::Chat(id) => Ok(conversation_key(conn, id)?
            .and_then(|(account, key)| key.direct_peer().cloned().map(|peer| (account, peer)))),
        PersonRef::Own(account) => {
            let own: Option<String> = conn
                .prepare_cached("SELECT own_ids FROM accounts WHERE id = ?1")?
                .query_row([account.0], |r| r.get(0))
                .optional()?;
            let Some(own) = own else {
                return Ok(None);
            };
            let own: Vec<UserId> = sql::from_json(&own)?;
            Ok(own.into_iter().next().map(|user| (account, user)))
        }
    }
}

/// The conversation of a message (the compose lane a command about it runs in).
pub(crate) fn conversation_of(conn: &Connection, id: MessageId) -> Result<Option<ConversationId>> {
    Ok(conn
        .prepare_cached("SELECT conversation_id FROM messages WHERE id = ?1")?
        .query_row([id.0], |r| r.get(0))
        .optional()?
        .map(ConversationId))
}

/// The conversation of an attachment's message.
pub(crate) fn conversation_of_attachment(
    conn: &Connection,
    id: AttachmentId,
) -> Result<Option<ConversationId>> {
    Ok(conn
        .prepare_cached(
            "SELECT m.conversation_id FROM attachments a JOIN messages m ON m.id = a.message_id
             WHERE a.id = ?1",
        )?
        .query_row([id.0], |r| r.get(0))
        .optional()?
        .map(ConversationId))
}

/// [`crate::facade::Query::SaveFacts`], without the sniffed type (the runtime reads the first
/// bytes of the plaintext): who sent the attachment's message, when, and where the file stands
/// among the message's files. `None` for an unknown attachment and for a message hidden on an
/// unconfirmed clock (gone for the user).
pub(crate) fn save_facts(
    conn: &Connection,
    id: AttachmentId,
) -> Result<Option<crate::facade::SaveFacts>> {
    type Raw = (i64, String, i64, i64, Option<String>, Option<String>);
    let raw: Option<Raw> = conn
        .prepare_cached(
            "SELECT m.account_id, m.net_author, m.sent_at,
                    (SELECT COUNT(*) FROM attachments b WHERE b.message_id = a.message_id AND b.idx <= a.idx),
                    a.file_name, a.mime
             FROM attachments a JOIN messages m ON m.id = a.message_id
             WHERE a.id = ?1 AND m.expiry_hidden = 0 AND m.view_once = 0 AND m.deleted IS NULL",
        )?
        .query_row([id.0], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })
        .optional()?;
    let Some((account, author, sent_at, position, file_name, mime)) = raw else {
        return Ok(None);
    };
    Ok(Some(crate::facade::SaveFacts {
        account: AccountId(account),
        author: if author.is_empty() {
            None
        } else {
            Some(sql::user(&author)?)
        },
        sent_at,
        position: sql::u32_of(position),
        file_name,
        mime,
        sniffed: None,
    }))
}

/// The files of one attachment (media-handle misses, §8.6).
pub(crate) fn attachment(
    conn: &Connection,
    id: AttachmentId,
) -> Result<Option<crate::facade::AttachmentFile>> {
    type Raw = (
        i64,
        i64,
        Option<String>,
        Option<i64>,
        String,
        Option<i64>,
        Option<i64>,
    );
    let raw: Option<Raw> = conn
        .prepare_cached(
            "SELECT m.account_id, a.message_id, a.mime, a.size, a.state, a.file_id, a.thumb_file_id
             FROM attachments a JOIN messages m ON m.id = a.message_id
             WHERE a.id = ?1 AND m.expiry_hidden = 0",
        )?
        .query_row([id.0], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })
        .optional()?;
    let Some((account, message, mime, size, state, file, thumb)) = raw else {
        return Ok(None);
    };
    Ok(Some(crate::facade::AttachmentFile {
        id,
        account: AccountId(account),
        message: MessageId(message),
        mime,
        size: size.and_then(|s| u64::try_from(s).ok()),
        state: sql::from_json(&state)?,
        file: blob_ref(conn, file)?,
        thumb: blob_ref(conn, thumb)?,
    }))
}

/// One media handle as stored: blob paths relative to the account's blob directory, which the
/// caller resolves (`AccountVault::blob_dir(uuid)`).
pub(crate) struct RawMedia {
    pub uuid: AccountUuid,
    pub entry: crate::media::MediaEntry,
}

const MEDIA_SELECT: &str = "SELECT a.id, m.account_id, acc.uuid, a.kind, a.mime, a.state,
    a.blurhash, a.width, a.height, f.path, f.key, f.mime, t.path, t.key, t.mime
    FROM attachments a JOIN messages m ON m.id = a.message_id
    JOIN accounts acc ON acc.id = m.account_id
    LEFT JOIN files f ON f.id = a.file_id LEFT JOIN files t ON t.id = a.thumb_file_id";

/// No media handle for an attachment of a message that is hidden because its time has come
/// on an unconfirmed clock ([`crate::expiry`]): its picture is as gone as its row.
const MEDIA_SHOWN: &str = "m.expiry_hidden = 0";

type MediaCols = (
    i64,
    i64,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    (Option<String>, Option<Vec<u8>>, Option<String>),
    (Option<String>, Option<Vec<u8>>, Option<String>),
);

fn media_cols(r: &Row<'_>) -> kourier_db::rusqlite::Result<MediaCols> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        (r.get(9)?, r.get(10)?, r.get(11)?),
        (r.get(12)?, r.get(13)?, r.get(14)?),
    ))
}

fn media_blob(
    (path, key, mime): (Option<String>, Option<Vec<u8>>, Option<String>),
) -> Option<crate::media::MediaBlob> {
    let key: [u8; 32] = key?.try_into().ok()?;
    Some(crate::media::MediaBlob {
        path: PathBuf::from(path?),
        key: BlobKey::new(key),
        mime,
    })
}

fn raw_media(c: MediaCols) -> Result<RawMedia> {
    let (id, account, uuid, kind, mime, state, blurhash, w, h, file, thumb) = c;
    Ok(RawMedia {
        uuid: uuid
            .parse()
            .map_err(|_| CoreError::Json("malformed stored uuid".into()))?,
        entry: crate::media::MediaEntry {
            attachment: AttachmentId(id),
            account: AccountId(account),
            kind: sql::from_enum_name(&kind)?,
            mime,
            state: sql::from_json(&state)?,
            file: media_blob(file),
            thumb: media_blob(thumb),
            blurhash,
            dims: w.zip(h).map(|(w, h)| (sql::u32_of(w), sql::u32_of(h))),
        },
    })
}

/// The media handle of one attachment (a miss of the table, on the media read connection).
pub(crate) fn media_entry(conn: &Connection, id: AttachmentId) -> Result<Option<RawMedia>> {
    conn.prepare_cached(&format!("{MEDIA_SELECT} WHERE a.id = ?1 AND {MEDIA_SHOWN}"))?
        .query_row([id.0], media_cols)
        .optional()?
        .map(raw_media)
        .transpose()
}

/// The media handles of every attachment on a timeline page (registered when the page is
/// answered, §8.6).
pub(crate) fn page_media(conn: &Connection, page: &TimelinePage) -> Result<Vec<RawMedia>> {
    let mut out = Vec::new();
    let mut stmt = conn.prepare_cached(&format!(
        "{MEDIA_SELECT} WHERE a.message_id = ?1 AND {MEDIA_SHOWN}"
    ))?;
    for row in &page.rows {
        let has = match &row.body {
            Body::Media { attachments, .. } => !attachments.is_empty(),
            Body::Sticker(_) => true,
            _ => false,
        };
        if !has {
            continue;
        }
        let cols: Vec<MediaCols> = stmt
            .query_map([row.id.0], media_cols)?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for c in cols {
            out.push(raw_media(c)?);
        }
    }
    // The thumbnails quotes show belong to other messages, maybe outside the page.
    let mut one =
        conn.prepare_cached(&format!("{MEDIA_SELECT} WHERE a.id = ?1 AND {MEDIA_SHOWN}"))?;
    for thumb in page.rows.iter().filter_map(|r| r.reply.as_ref()?.thumb) {
        if out.iter().any(|m| m.entry.attachment == thumb) {
            continue;
        }
        if let Some(c) = one.query_row([thumb.0], media_cols).optional()? {
            out.push(raw_media(c)?);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use kourier_model::{MsgIdScope, NetworkKind, UserId};

    /// The groups in common start at `members_by_user` (the person), then reach the group's row
    /// and our own membership by primary keys: no walk through `members` (every member of every
    /// group of every account) and no sort of a temporary tree. `core.db` has no planner
    /// statistics, so the plan is pinned.
    #[test]
    fn the_groups_in_common_start_at_the_members_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = crate::testing::IngestHarness::open(
            &dir.path().join("core.db"),
            &kourier_db::DbKey::from([4u8; 32]),
            NetworkKind::Fake,
            MsgIdScope::AccountAuthor,
            &[UserId::parse("aci:me").expect("user id")],
        )
        .expect("harness");
        let steps: Vec<String> = h
            .connection()
            .prepare(&format!(
                "EXPLAIN QUERY PLAN {}",
                super::GROUPS_IN_COMMON
                    .replace("?1", "'aci:a'")
                    .replace("?2", "1")
                    .replace("?3", "'[\"aci:me\"]'")
            ))
            .expect("prepare")
            .query_map([], |r| r.get::<_, String>(3))
            .expect("plan")
            .collect::<kourier_db::rusqlite::Result<_>>()
            .expect("rows");
        assert!(
            steps
                .iter()
                .any(|s| s.contains("members_by_user (user_id=?)")),
            "starts at the person's memberships: {steps:#?}"
        );
        assert!(
            steps
                .iter()
                .any(|s| s.contains("sqlite_autoindex_members_1")
                    && s.contains("conversation_id=?")
                    && s.contains("user_id=?")
                    || s.contains("sqlite_autoindex_members_1") && s.contains("conversation_id=?")),
            "our own membership by the primary key of members: {steps:#?}"
        );
        assert!(
            steps
                .iter()
                .all(|s| !s.contains("SCAN members") && !s.contains("SCAN mb")),
            "no walk through members: {steps:#?}"
        );
    }

    /// The members of a group are read in ONE statement that starts at the group's rows of
    /// `members` (its primary key) and reaches each name by the unique key of `contacts` and each
    /// picture by the primary key of `files`: no walk through `members`, `contacts` or `files`
    /// (a group of 1,000 would cost three scans per page) and no sort of a temporary tree.
    /// `core.db` has no planner statistics, so the plan is pinned.
    #[test]
    fn the_group_members_start_at_the_group() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = crate::testing::IngestHarness::open(
            &dir.path().join("core.db"),
            &kourier_db::DbKey::from([4u8; 32]),
            NetworkKind::Fake,
            MsgIdScope::AccountAuthor,
            &[UserId::parse("aci:me").expect("user id")],
        )
        .expect("harness");
        let steps: Vec<String> = h
            .connection()
            .prepare(&format!(
                "EXPLAIN QUERY PLAN {}",
                super::GROUP_MEMBERS.replace("?1", "1").replace("?2", "1")
            ))
            .expect("prepare")
            .query_map([], |r| r.get::<_, String>(3))
            .expect("plan")
            .collect::<kourier_db::rusqlite::Result<_>>()
            .expect("rows");
        let has = |needles: &[&str]| steps.iter().any(|s| needles.iter().all(|n| s.contains(n)));
        assert!(
            has(&["m", "sqlite_autoindex_members_1", "conversation_id=?"]),
            "starts at the group's members by the primary key: {steps:#?}"
        );
        assert!(
            has(&[
                "ct",
                "sqlite_autoindex_contacts_1",
                "account_id=?",
                "user_id=?"
            ]),
            "each name by the unique key of contacts: {steps:#?}"
        );
        assert!(
            has(&["f", "INTEGER PRIMARY KEY"]),
            "each picture by the primary key of files: {steps:#?}"
        );
        assert!(
            steps
                .iter()
                .all(|s| !s.contains("SCAN") && !s.contains("TEMP B-TREE")),
            "no walk through a table and no temporary sort: {steps:#?}"
        );
    }
    /// The detail lines of `EXPLAIN QUERY PLAN` of `sql` on a migrated `core.db`, with the
    /// parameters bound as a first page would bind them.
    fn plan_of(sql: &str, params: &[i64]) -> Vec<String> {
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
            .query_map(kourier_db::rusqlite::params_from_iter(params), |r| {
                r.get::<_, String>(3)
            })
            .expect("plan")
            .collect::<kourier_db::rusqlite::Result<_>>()
            .expect("rows")
    }

    /// The gallery of a chat (both kinds, the page and the "is there any" statement) starts at
    /// the PARTIAL index of ITS kind (`messages_media_visual` for the pictures,
    /// `messages_media_files` for the files: a chat of 20,000 pictures and one file must not walk
    /// 20,000 messages for the file) by the chat, goes along it in the order of the page (no
    /// temporary sort), reaches the attachments of a message by the unique key of `attachments`,
    /// and never touches the other kind's index or the timeline's: no walk through the chat's
    /// messages, whatever their number (`core.db` has no planner statistics, so the plan is
    /// pinned, and `INDEXED BY` turns a plan that ever stops applying into an error).
    #[test]
    fn the_gallery_starts_at_the_partial_index_of_its_kind() {
        for (kind, mine, other) in [
            (
                super::MediaKind::Visual,
                "messages_media_visual",
                "messages_media_files",
            ),
            (
                super::MediaKind::Files,
                "messages_media_files",
                "messages_media_visual",
            ),
        ] {
            let page = plan_of(&super::gallery_sql(kind), &[1, 8, i64::MAX, i64::MAX, -1]);
            let any = plan_of(&super::gallery_has_any(kind), &[1]);
            for (what, steps) in [("page", page), ("any", any)] {
                assert!(
                    steps
                        .iter()
                        .any(|s| s.contains(mine) && s.contains("conversation_id=?")),
                    "{kind:?} {what} starts at {mine} by the chat: {steps:#?}"
                );
                assert!(
                    steps
                        .iter()
                        .any(|s| s.contains("sqlite_autoindex_attachments_1")
                            && s.contains("message_id=?")),
                    "{kind:?} {what} reaches the attachments by their unique key: {steps:#?}"
                );
                // ("SCAN CONSTANT ROW" is the one row of `SELECT EXISTS (...)`, no table.)
                assert!(
                    steps
                        .iter()
                        .all(|s| (!s.contains("SCAN") || s.contains("CONSTANT ROW"))
                            && !s.contains("TEMP B-TREE")),
                    "{kind:?} {what}: no walk through a table, no temporary sort: {steps:#?}"
                );
                assert!(
                    steps
                        .iter()
                        .all(|s| !s.contains("messages_timeline") && !s.contains(other)),
                    "{kind:?} {what} uses its own gallery index alone: {steps:#?}"
                );
            }
        }
    }

    /// The gallery's partial index takes nothing from the statements that were there: the
    /// timeline's page (its older side) still goes along `messages_timeline`, never along
    /// `messages_media_*` (its WHERE does not say `body_kind = 'Media'`, so SQLite cannot use a
    /// partial index for it; `notices::tests` pin the list's own statements the same way).
    #[test]
    fn the_timeline_page_still_uses_the_timeline_index() {
        for op in ["<", "<="] {
            let steps = plan_of(&super::timeline_older_sql(op), &[1, i64::MAX, i64::MAX, 51]);
            assert!(
                steps.iter().any(|s| s.contains("messages_timeline")),
                "the timeline page goes along messages_timeline: {steps:#?}"
            );
            assert!(
                steps.iter().all(|s| !s.contains("messages_media")),
                "and not along a gallery index: {steps:#?}"
            );
        }
    }
}

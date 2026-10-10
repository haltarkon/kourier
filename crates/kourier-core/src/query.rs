//! Read-side queries for the facade (§4.10, §8.4), run on the read-only connections.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Duration;

use kourier_db::rusqlite::{Connection, OptionalExtension, Row};
use kourier_model::{
    AccountId, AccountState, AccountUuid, AttachmentId, AttachmentKind, AttachmentState, BlobKey,
    BlobRef, Body, ConnState, ConversationFeatures, ConversationId, Deletion, DisappearKind,
    Expiry, Member, MemberRole, MessageId, ReplyTo, TargetAuthor, Text, UserId,
};

use crate::actions::{self, Facts, Sending};
use crate::error::{CoreError, Result};
use crate::facade::{
    AccountRow, ConversationDetail, ConversationListQuery, ConversationPage, ConversationRow,
    MessageRevisions, MessageRow, OriginKind, Preview, QuoteState, ReactionGroup, ReplyPreview,
    Revision, TimelineAnchor, TimelinePage, TimelineQuery,
};
use crate::sql;

/// Largest page any query returns.
pub const MAX_PAGE: u32 = 500;

/// Every account, in display order.
pub(crate) fn accounts(
    conn: &Connection,
    conn_states: &HashMap<AccountId, ConnState>,
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
    );
    let rows: Vec<Raw> = conn
        .prepare_cached(
            "SELECT id, uuid, network, state, state_reason, display_name, own_device, own_ids,
               number
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
            ))
        })?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(
            |(id, uuid, network, state, reason, name, device, own_ids, number)| {
                let state = sql::account_state(&state, reason)?;
                // Without a running actor (after a restart) a logged-out account still says so.
                let shown =
                    conn_states
                        .get(&AccountId(id))
                        .cloned()
                        .unwrap_or_else(|| match &state {
                            AccountState::LoggedOut { reason } => ConnState::LoggedOut {
                                reason: reason.clone(),
                            },
                            _ => ConnState::default(),
                        });
                // The account's own picture is its first own id's contact avatar.
                let own_ids: Vec<UserId> = sql::from_json(&own_ids)?;
                let avatar = match own_ids.first() {
                    Some(own) => contact_avatar(conn, id, own.as_str())?,
                    None => None,
                };
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
    Ok(row.and_then(|(path, key)| {
        Some(BlobRef {
            path: PathBuf::from(path),
            key: BlobKey::new(key.try_into().ok()?),
        })
    }))
}

/// Display name of a user: nickname, address-book name, then profile name.
fn display_name(conn: &Connection, account: i64, user: &str) -> Result<Option<String>> {
    Ok(conn
        .prepare_cached(
            "SELECT coalesce(nickname, system_name, profile_name) FROM contacts
             WHERE account_id = ?1 AND user_id = ?2",
        )?
        .query_row((account, user), |r| r.get::<_, Option<String>>(0))
        .optional()?
        .flatten())
}

const CONV_SELECT: &str = "SELECT c.id, c.account_id, c.kind, c.is_self, c.conv_key,
      coalesce(c.title, ct.nickname, ct.system_name, ct.profile_name),
      coalesce(c.avatar_file, ct.avatar_file), coalesce(c.last_activity, 0), c.unread, c.mentions,
      c.marked_unread, c.muted_until, c.local_muted_until, c.archived, c.pinned_order, c.blocked,
      c.request_pending, coalesce(c.features_hash, 0), c.draft IS NOT NULL, c.hidden
    FROM conversations c
    JOIN accounts a ON a.id = c.account_id
    LEFT JOIN contacts ct ON c.kind = 'Direct' AND ct.account_id = c.account_id
      AND ct.user_id = substr(c.conv_key, 8)";

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
    })
}

fn conv_row(conn: &Connection, raw: ConvRaw) -> Result<ConversationRow> {
    let key = sql::conv_key(&raw.key)?;
    let preview = preview(conn, raw.account, raw.id)?;
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
    );
    // What the list says instead of a text: the first attachment's kind and how many there
    // are (long texts are text, not files), and the body of a notice or an unsupported row.
    let row: Option<Raw> = conn
        .prepare_cached(
            "SELECT m.id, m.net_author, m.body_kind, m.preview, m.sent_at, m.deleted, m.out_state,
               (SELECT x.kind FROM attachments x WHERE x.message_id = m.id AND x.long_text = 0
                ORDER BY x.idx LIMIT 1),
               (SELECT count(*) FROM attachments x WHERE x.message_id = m.id AND x.long_text = 0),
               CASE WHEN m.body_kind IN ('System', 'Unsupported') THEN m.body END,
               (SELECT x.file_name FROM attachments x WHERE x.message_id = m.id AND x.long_text = 0
                ORDER BY x.idx LIMIT 1)
             FROM messages m
             WHERE m.conversation_id = ?1 AND (m.deleted IS NULL OR m.deleted <> 'ForMe')
               AND m.expiry_hidden = 0
             ORDER BY m.sort_key DESC, m.id DESC LIMIT 1",
        )?
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
            ))
        })
        .optional()?;
    let Some((id, author, kind, text, sent_at, deleted, out_state, first, count, body, file_name)) =
        row
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
        deleted: deleted.is_some(),
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

/// A page of the conversation list: pinned first (pin order), then newest activity.
pub(crate) fn conversations(
    conn: &Connection,
    q: &ConversationListQuery,
) -> Result<ConversationPage> {
    let limit = q.limit.clamp(1, MAX_PAGE);
    let sql = format!(
        "{CONV_SELECT}
         WHERE (?1 IS NULL OR c.account_id = ?1) AND c.archived = ?2 AND a.state <> 'Removing'
           AND c.hidden = 0
           AND (?3 = 0 OR ((c.pinned_order IS NULL), coalesce(c.pinned_order, 0),
                -coalesce(c.last_activity, 0), -c.id) > (?4, ?5, ?6, ?7))
         ORDER BY (c.pinned_order IS NULL), coalesce(c.pinned_order, 0),
           -coalesce(c.last_activity, 0), -c.id
         LIMIT ?8"
    );
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
    let mut rows = raws
        .into_iter()
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
        .map(|r| conv_row(conn, r))
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
    Ok(Some(ConversationDetail {
        row: conv_row(conn, raw)?,
        members,
        features: sql::from_json_opt::<ConversationFeatures>(features)?.unwrap_or_default(),
        timer: timer.map(|s| Duration::from_secs(u64::try_from(s.max(0)).unwrap_or(0))),
        draft: sql::from_json_opt(draft)?,
    }))
}

const MSG_SELECT: &str = "SELECT id, sort_key, net_author, origin, device, body, reply_to, sent_at,
      server_at, received_at, revision, edited_at, deleted, deleted_by, out_state, read_state,
      expiry_kind, expiry_timer_ms, expiry_started_at, view_once, history, mentions_me, account_id
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
    blocked: bool,
    /// The chat with ourselves.
    note_to_self: bool,
    features: Option<ConversationFeatures>,
    /// One of our ids is an admin of the conversation.
    admin: bool,
    own_ids: Vec<UserId>,
    /// Reactions per user (`None` = a set).
    per_user: Option<u8>,
    /// Own messages that still have their send row in the outbox → whether that row waits
    /// or is in flight (`false`: it failed and is only kept for "Retry").
    queued: HashMap<i64, bool>,
    /// Display name and avatar per author.
    people: HashMap<String, (Option<String>, Option<BlobRef>)>,
}

impl PageCtx {
    fn load(conn: &Connection, conv: ConversationId, now: i64) -> Result<Self> {
        type Raw = (bool, Option<String>, String, Option<String>, String, bool);
        let row: Option<Raw> = conn
            .prepare_cached(
                "SELECT c.blocked, c.features, a.state, a.state_reason, a.own_ids, c.is_self
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
                ))
            })
            .optional()?;
        let (blocked, features, state, reason, own_ids, note_to_self) =
            row.unwrap_or_else(|| (false, None, "Removing".into(), None, "[]".into(), false));
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
        let queued: HashMap<i64, bool> = conn
            .prepare_cached(
                "SELECT message_id, state <> 'Failed' FROM outbox
                 WHERE conversation_id = ?1 AND kind = 'Message'",
            )?
            .query_map([conv.0], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        Ok(Self {
            conv,
            now,
            active: sql::account_state(&state, reason)? == AccountState::Active,
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

/// A timeline page, oldest first, keyset-paged by `(sort_key, id)`. `now` is the core clock:
/// the rows' action flags are computed for it.
pub(crate) fn timeline(conn: &Connection, q: &TimelineQuery, now: i64) -> Result<TimelinePage> {
    let limit = i64::from(q.limit.clamp(1, MAX_PAGE));
    let mut ctx = PageCtx::load(conn, q.conv, now)?;
    // Not what a delete for me hid, and not what is hidden because its time has come on a
    // clock nobody confirmed (`expiry_hidden`, [`crate::expiry`]).
    let visible =
        "conversation_id = ?1 AND (deleted IS NULL OR deleted <> 'ForMe') AND expiry_hidden = 0";
    let older = |key: i64, id: i64, n: i64, inclusive: bool| -> Result<Vec<MsgRaw>> {
        let op = if inclusive { "<=" } else { "<" };
        let sql = format!(
            "{MSG_SELECT} WHERE {visible} AND (sort_key, id) {op} (?2, ?3)
             ORDER BY sort_key DESC, id DESC LIMIT ?4"
        );
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
        actions::sending_of(conn, MessageId(raw.id))?
    } else {
        Sending::Settled
    };
    let flags = actions::all(
        &Facts {
            active: ctx.active,
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
            queued: own && ctx.queued.get(&raw.id).copied().unwrap_or(false),
        },
        ctx.now,
    );
    let deleted = match raw.deleted.as_deref() {
        None => None,
        Some("ForMe") => Some(Deletion::ForMe),
        Some(_) => Some(Deletion::ForEveryone {
            by: raw
                .deleted_by
                .filter(|b| !b.is_empty())
                .map(|b| sql::user(&b))
                .transpose()?,
        }),
    };
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
    // only what its sender's snapshot says.
    type Found = (i64, Option<String>, String, String);
    let found: Option<Found> = conn
        .prepare_cached(
            "SELECT m.id, m.deleted, m.body, m.net_author FROM messages m
             WHERE m.account_id = ?1 AND m.conversation_id = ?4 AND m.net_local = ?2
               AND (?3 IS NULL OR m.net_author = ?3) AND m.expiry_hidden = 0
             UNION ALL
             SELECT m.id, m.deleted, m.body, m.net_author FROM message_revisions v
               JOIN messages m ON m.id = v.message_id
             WHERE v.account_id = ?1 AND m.conversation_id = ?4 AND v.net_local = ?2
               AND (?3 IS NULL OR v.net_author = ?3) AND m.expiry_hidden = 0
             LIMIT 1",
        )?
        .query_row(
            (account, r.target.local.as_ref(), &canonical, ctx.conv.0),
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
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

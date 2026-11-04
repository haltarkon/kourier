//! What the core was told about people and groups (W6a2, chunk B3; decisions D7-D11 and D22 of
//! `target/wa/w6-decisions.md`): the tables of migration 8, `group_state`, `merges_stated`, `names_stated` and
//! `blocked_stated`.
//!
//! A row of any of them means "the core was told", so that a statement is made when something changed and not every time
//! it could be. They are not history: a `Rebuild` (the core was made again) forgets what the core was told in the zone that
//! states `AccountReady` again ([`forget_statements`]), and the start's reads state everything anew. The push names are
//! kept as rows marked not stated and told again, once, where the person is named to the core ([`unstated_profile`], fix F21). The one exception is
//! `group_state`, which is also the server's word about a group (we left it, it was deleted, its rules and our place):
//! a `Rebuild` takes back only its two "the core was told" flags, and the sweep that follows states the group's features
//! from the facts that stayed (fix F15, DUR-1).
//!
//! B3 owns [`blocked`] (the blocklist's statements) and [`forget_statements`]; B4 adds the group's row ([`group`],
//! [`put_group`]); the later chunks of W6a2 add the merge and name functions here, each keeping `forget_statements`
//! true.
//!
//! The functions run inside the caller's zone (`&Transaction`) or read (`&Connection`); none logs, and none takes or
//! returns anything but encoded ids (`UserId::as_str`), times and flags.

use kourier_db::rusqlite::{Connection, Transaction, params};
use kourier_model::{ConvKey, UserId};
use wacore::store::error::StoreError;

use super::{exec, first, rows};
use crate::groups::{Rules, State};
use crate::map::GroupFacts;

/// Forgets everything the core was told about people and groups: a `Rebuild` made the core again, so none of it is true.
/// How many rows went or changed.
///
/// A group's row is NOT deleted (fix F15, DUR-1): `not_member` (a refusal, our removal), `rules` (the last fetch's rules and
/// our place), the `Deleted` act's `suspended` bit, `truncated`, `parent`, the times and the fetch mark are what the server
/// said, not what the core was told. The restated features (`features::sweep`) are made of them, so a group we left stays
/// `NotMember` and an announcements-only one stays closed in the rebuilt core. Only `members_stated` (the new core holds no
/// list: the start's batch states it again and re-releases the parked deletes, which wait meanwhile), `parent_stated` (the
/// new core was told no parent) and `subject_time` (the title's time, G3 / NEW-2: the new core was told no title, and a time
/// that a notice dated by its OWN clock set later than the server's would hold the next answer's title back) go back.
pub(crate) fn forget_statements(tx: &Transaction<'_>) -> Result<usize, StoreError> {
    let mut forgotten = 0;
    for statement in [
        "UPDATE group_state SET members_stated = 0, parent_stated = 0, subject_time = NULL
         WHERE members_stated <> 0 OR parent_stated <> 0 OR subject_time IS NOT NULL",
        "DELETE FROM merges_stated",
        // The push names stay, marked not stated (fix F21, DUR-12): backend.db holds exactly what the old core was told, and
        // the zone that makes a person known again states his name once ([`unstated_profile`]). The address book's names
        // and numbers go: the mirror states them again.
        "UPDATE names_stated SET stated = 0 WHERE kind = 'profile' AND stated <> 0",
        "DELETE FROM names_stated WHERE kind <> 'profile'",
        "DELETE FROM blocked_stated",
        // The people the core was told of (D8's scope): the new core knows none, and the sweep and the fetches state them anew.
        "DELETE FROM persons_known",
        // The stamp of the blocklist's last answer goes with the statements it covers (Q1): the new core is told anew.
        "DELETE FROM blocklist_read",
    ] {
        forgotten += exec(tx, "forget what the core was told of people", statement, [])?;
    }
    Ok(forgotten)
}

/// The people the core was told are blocked (encoded user ids), in order.
pub(crate) fn blocked(conn: &Connection) -> Result<Vec<String>, StoreError> {
    rows(
        conn,
        "read who is stated blocked",
        "SELECT person FROM blocked_stated ORDER BY person",
        [],
        |r| r.get(0),
    )
}

/// When the phone's blocklist was last STATED, in ms (the zone of an answer wrote it; Q1: asked at most once per 24 hours);
/// `None` when no answer was ever stated since the link or the last `Rebuild`.
pub(crate) fn blocklist_read_at(conn: &Connection) -> Result<Option<i64>, StoreError> {
    first(
        conn,
        "read when the blocklist was last stated",
        "SELECT read_at FROM blocklist_read WHERE id = 1",
        [],
        |r| r.get(0),
    )
}

/// The blocklist's answer was stated at `now_ms` (the zone that states it writes this; a read with no answer does not).
pub(crate) fn stamp_blocklist(tx: &Transaction<'_>, now_ms: i64) -> Result<(), StoreError> {
    exec(
        tx,
        "record when the blocklist was stated",
        "INSERT INTO blocklist_read(id, read_at) VALUES (1, ?1)
         ON CONFLICT (id) DO UPDATE SET read_at = excluded.read_at",
        params![now_ms],
    )
    .map(drop)
}

/// The link: a fresh account's first `Online` asks the blocklist whatever an earlier account of this file stamped.
pub(crate) fn forget_blocklist_stamp(tx: &Transaction<'_>) -> Result<(), StoreError> {
    exec(
        tx,
        "forget when the blocklist was stated",
        "DELETE FROM blocklist_read",
        [],
    )
    .map(drop)
}

/// The core was told `person` is blocked. `true` when the row is new.
pub(crate) fn put_blocked(tx: &Transaction<'_>, person: &str) -> Result<bool, StoreError> {
    exec(
        tx,
        "record a blocked person",
        "INSERT INTO blocked_stated(person) VALUES (?1) ON CONFLICT (person) DO NOTHING",
        params![person],
    )
    .map(|changed| changed > 0)
}

/// The core was told `person` is no longer blocked. `true` when a row went.
pub(crate) fn drop_blocked(tx: &Transaction<'_>, person: &str) -> Result<bool, StoreError> {
    exec(
        tx,
        "record an unblocked person",
        "DELETE FROM blocked_stated WHERE person = ?1",
        params![person],
    )
    .map(|changed| changed > 0)
}

/// The kinds of value the core is told about a person's names (`names_stated.kind`): his own choice, the phone's address
/// book's name and the number that rides with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NameKind {
    /// The push name (`profile_name`).
    Profile,
    /// The name from the phone's address book (`system_name`).
    System,
    /// The phone number stated with the address book's name (`identifiers` and `number_shown`), as `+<digits>`.
    Number,
}

impl NameKind {
    fn column(self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::System => "system",
            Self::Number => "number",
        }
    }
}

/// The core is no longer told `kind` of `person` (the address book dropped him). `true` when a row went.
pub(crate) fn drop_name(
    tx: &Transaction<'_>,
    person: &str,
    kind: NameKind,
) -> Result<bool, StoreError> {
    exec(
        tx,
        "forget a name the core was told",
        "DELETE FROM names_stated WHERE person = ?1 AND kind = ?2",
        params![person, kind.column()],
    )
    .map(|changed| changed > 0)
}

/// The value the core was last told as `kind` of `person`.
pub(crate) fn name_of(
    conn: &Connection,
    person: &str,
    kind: NameKind,
) -> Result<Option<String>, StoreError> {
    first(
        conn,
        "read a name the core was told",
        "SELECT value FROM names_stated WHERE person = ?1 AND kind = ?2",
        params![person, kind.column()],
        |r| r.get(0),
    )
}

/// The core is told `value` as `person`'s name of `kind`. `true` when that is a CHANGE (no row, another value, or a row a
/// `Rebuild` kept and the new core was not told yet): the caller states the name; `false` when the core already has exactly
/// this (nothing is stated, nothing is written).
pub(crate) fn put_name(
    tx: &Transaction<'_>,
    person: &str,
    kind: NameKind,
    value: &str,
) -> Result<bool, StoreError> {
    exec(
        tx,
        "record a name the core was told",
        "INSERT INTO names_stated(person, kind, value) VALUES (?1, ?2, ?3)
         ON CONFLICT (person, kind) DO UPDATE SET value = excluded.value, stated = 1
           WHERE names_stated.value <> excluded.value OR names_stated.stated = 0",
        params![person, kind.column(), value],
    )
    .map(|changed| changed > 0)
}

/// The push name a `Rebuild` kept of `person` that the new core was not told (fix F21, DUR-12), marked told: the caller states
/// it in this zone. `None` when there is none (the usual case: the name was told, or never known).
pub(crate) fn unstated_profile(
    tx: &Transaction<'_>,
    person: &str,
) -> Result<Option<String>, StoreError> {
    let name = first(
        tx,
        "read a push name the new core was not told",
        "SELECT value FROM names_stated WHERE person = ?1 AND kind = 'profile' AND stated = 0",
        params![person],
        |r| r.get::<_, String>(0),
    )?;
    if let Some(name) = &name {
        put_name(tx, person, NameKind::Profile, name)?;
    }
    Ok(name)
}

/// What the core was told of `conv` (a group): the row, or the default when no row exists yet. `dirty_at` is the fetch
/// policy's and is not read here.
pub(crate) fn group(conn: &Connection, conv: &ConvKey) -> Result<State, StoreError> {
    let row = first(
        conn,
        "read what the core was told of a group",
        "SELECT members_stated, subject_time, timer, timer_version, rules, truncated, not_member, fetched_at,
                parent, parent_stated
         FROM group_state WHERE conv = ?1",
        params![conv.encode()],
        |r| {
            Ok(State {
                members_stated: r.get::<_, i64>(0)? != 0,
                subject_time: r.get::<_, Option<i64>>(1)?.and_then(|t| u64::try_from(t).ok()),
                timer: r.get::<_, Option<i64>>(2)?.and_then(|t| u32::try_from(t).ok()),
                timer_version: r.get::<_, Option<i64>>(3)?.and_then(|t| u32::try_from(t).ok()),
                rules: r.get::<_, Option<i64>>(4)?.map(Rules::from_bits),
                truncated: r.get::<_, i64>(5)? != 0,
                not_member: r.get::<_, i64>(6)? != 0,
                fetched_at: r.get(7)?,
                parent: r
                    .get::<_, Option<String>>(8)?
                    .and_then(|key| ConvKey::parse(&key).ok()),
                parent_stated: r.get::<_, i64>(9)? != 0,
            })
        },
    )?;
    Ok(row.unwrap_or_default())
}

/// The core was told `state` of `conv` (a group): the row is made or replaced. `dirty_at`, the mark that a fresh fetch
/// is owed, is not touched.
pub(crate) fn put_group(
    tx: &Transaction<'_>,
    conv: &ConvKey,
    state: &State,
) -> Result<(), StoreError> {
    exec(
        tx,
        "record what the core was told of a group",
        "INSERT INTO group_state(conv, members_stated, subject_time, timer, timer_version, rules, truncated, not_member,
                                fetched_at, parent, parent_stated)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT (conv) DO UPDATE SET
           members_stated = excluded.members_stated, subject_time = excluded.subject_time,
           timer = excluded.timer, timer_version = excluded.timer_version, rules = excluded.rules,
           truncated = excluded.truncated, not_member = excluded.not_member,
           fetched_at = excluded.fetched_at, parent = excluded.parent, parent_stated = excluded.parent_stated",
        params![
            conv.encode(),
            i64::from(state.members_stated),
            state.subject_time.and_then(|t| i64::try_from(t).ok()),
            state.timer.map(i64::from),
            state.timer_version.map(i64::from),
            state.rules.map(Rules::bits),
            i64::from(state.truncated),
            i64::from(state.not_member),
            state.fetched_at,
            state.parent.as_ref().map(ConvKey::encode),
            i64::from(state.parent_stated),
        ],
    )?;
    Ok(())
}

/// The subgroups (other than `parent` itself) whose last answer named `parent` as their community and whose parent the core
/// was NOT told yet (the core did not know the parent chat then), in key order, excluding the groups we are not in. The zone
/// that states `parent` states it for each of them ([`mark_parent_stated`]).
pub(crate) fn subgroups_waiting_for(
    conn: &Connection,
    parent: &ConvKey,
) -> Result<Vec<ConvKey>, StoreError> {
    let rows = rows(
        conn,
        "read the subgroups that wait for their community",
        "SELECT conv FROM group_state
         WHERE parent = ?1 AND parent_stated = 0 AND not_member = 0 AND conv <> ?1 ORDER BY conv",
        params![parent.encode()],
        |r| r.get::<_, String>(0),
    )?;
    Ok(rows
        .into_iter()
        .filter_map(|conv| ConvKey::parse(&conv).ok())
        .collect())
}

/// The core was told the parent of `conv` (a subgroup): it no longer waits.
pub(crate) fn mark_parent_stated(tx: &Transaction<'_>, conv: &ConvKey) -> Result<(), StoreError> {
    exec(
        tx,
        "record that a subgroup's parent was stated",
        "UPDATE group_state SET parent_stated = 1 WHERE conv = ?1",
        params![conv.encode()],
    )
    .map(drop)
}

/// Whether the group's row says we are an admin there and still in it: the last answered fetch (or a notice about our own
/// place) said so. `false` for a group never answered, one we left, a suspended one, and anything else. The admin revoke of another member's
/// message (W6a2, B7b) is built only on this.
pub(crate) fn is_admin(conn: &Connection, conv: &ConvKey) -> Result<bool, StoreError> {
    let state = group(conn, conv)?;
    // A suspended group is ended for everybody: no frame goes into it (fix F4).
    Ok(!state.not_member
        && state
            .rules
            .is_some_and(|rules| rules.own_admin && !rules.suspended))
}

/// The form `author` was named in by the key of the received group message `local` (fix F3, PROTOCOL-6): the person of the
/// `sender` of its inbox ledger row (`chat`, `sender`, `id`), when that person is `author` or his other id by a stored
/// `lid_pn_mapping` pair. The core's author key may be the other id of a merged person, and the group may have changed its
/// addressing mode since; an admin's revoke names the author exactly as the original key did. `None` when the ledger has
/// no such row (a message from the history, a row swept after 14 days) or none of the rows with this id was sent by him
/// (another member may use the same id): the caller then writes the author in the group's current mode.
pub(crate) fn received_as(
    conn: &Connection,
    conv: &ConvKey,
    local: &str,
    author: &UserId,
) -> Result<Option<UserId>, StoreError> {
    let Ok(chat) = crate::send::address::jid_of(conv) else {
        return Ok(None);
    };
    let senders: Vec<String> = rows(
        conn,
        "read who sent a received message",
        "SELECT DISTINCT sender FROM inbox WHERE chat = ?1 AND id = ?2",
        params![chat.to_string(), local],
        |r| r.get(0),
    )?;
    for sender in senders {
        let Ok(jid) = sender.parse::<wacore_binary::Jid>() else {
            continue;
        };
        let Some(person) = crate::map::person_of(&jid.to_non_ad()) else {
            continue;
        };
        if person == *author || paired(conn, &person, author)? {
            return Ok(Some(person));
        }
    }
    Ok(None)
}

/// Whether the store pairs the number and the LID `a` and `b` name (in either order).
fn paired(conn: &Connection, a: &UserId, b: &UserId) -> Result<bool, StoreError> {
    let (pn, lid) = match (a.scheme(), b.scheme()) {
        ("pn", "lid") => (a, b),
        ("lid", "pn") => (b, a),
        _ => return Ok(false),
    };
    Ok(first(
        conn,
        "read whether a number and a LID are paired",
        "SELECT 1 FROM lid_pn_mapping WHERE phone = ?1 AND lid = ?2",
        params![pn.value(), lid.value()],
        |r| r.get::<_, i64>(0),
    )?
    .is_some())
}

/// A notice the event cannot carry, or a `groups_dirty`, asks for a fresh fetch of `conv` (a group): `dirty_at` is the time
/// (ms) of the FIRST ask still unanswered (the order the marks are served in) and `dirty_last` the time of the NEWEST (fix
/// F16, DUR-2: an answer computed before a later notice must not clear it). The row is made when the group has none. The fetch
/// policy (B6) reads the marks ([`dirty`], [`dirty_last`]) and the zone of the answer that covers one clears it.
pub(crate) fn mark_dirty(
    tx: &Transaction<'_>,
    conv: &ConvKey,
    at_ms: i64,
) -> Result<(), StoreError> {
    exec(
        tx,
        "mark a group for a fetch",
        "INSERT INTO group_state(conv, dirty_at, dirty_last) VALUES (?1, ?2, ?2)
         ON CONFLICT (conv) DO UPDATE SET
           dirty_last = CASE WHEN dirty_at IS NULL THEN excluded.dirty_last
                             ELSE MAX(COALESCE(dirty_last, dirty_at), excluded.dirty_last) END,
           dirty_at = COALESCE(dirty_at, excluded.dirty_at)",
        params![conv.encode(), at_ms],
    )?;
    Ok(())
}

/// The groups marked for a fetch with the time of the first ask, oldest first (the tests' reader of the marks; the policy
/// reads [`dirty_last`]).
#[cfg(test)]
pub(crate) fn dirty(conn: &Connection) -> Result<Vec<(ConvKey, i64)>, StoreError> {
    let rows = rows(
        conn,
        "read the groups marked for a fetch",
        "SELECT conv, dirty_at FROM group_state WHERE dirty_at IS NOT NULL ORDER BY dirty_at, conv",
        [],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
    )?;
    Ok(rows
        .into_iter()
        .filter_map(|(conv, at)| ConvKey::parse(&conv).ok().map(|conv| (conv, at)))
        .collect())
}

/// The groups marked for a fetch with the time of the NEWEST ask, in the order of [`dirty`]: the fetch policy asks a group again
/// in the same start when this is later than its last single query (fix F17).
pub(crate) fn dirty_last(conn: &Connection) -> Result<Vec<(ConvKey, i64)>, StoreError> {
    let rows = rows(
        conn,
        "read when the marked groups were last asked for",
        "SELECT conv, COALESCE(dirty_last, dirty_at) FROM group_state WHERE dirty_at IS NOT NULL ORDER BY dirty_at, conv",
        [],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
    )?;
    Ok(rows
        .into_iter()
        .filter_map(|(conv, at)| ConvKey::parse(&conv).ok().map(|conv| (conv, at)))
        .collect())
}

/// The answer of a fetch asked at `asked_at_ms` covers the mark of `conv`: it is cleared, unless the newest ask of the mark
/// is LATER than the ask (a notice that came after the server computed its answer; fix F16: the first ask's time alone would
/// call it covered). `true` when a mark went. A group a fetch did not answer keeps its mark (scout T3).
pub(crate) fn clear_dirty(
    tx: &Transaction<'_>,
    conv: &ConvKey,
    asked_at_ms: i64,
) -> Result<bool, StoreError> {
    exec(
        tx,
        "clear a group's fetch mark",
        "UPDATE group_state SET dirty_at = NULL, dirty_last = NULL
         WHERE conv = ?1 AND dirty_at IS NOT NULL AND COALESCE(dirty_last, dirty_at) <= ?2",
        params![conv.encode(), asked_at_ms],
    )
    .map(|changed| changed > 0)
}

/// The groups whose last answer was `Truncated` (the server named them and gave nothing else), in order: each gets ONE single
/// query per start (M1).
pub(crate) fn truncated(conn: &Connection) -> Result<Vec<ConvKey>, StoreError> {
    let rows = rows(
        conn,
        "read the groups whose answer was truncated",
        "SELECT conv FROM group_state WHERE truncated = 1 ORDER BY conv",
        [],
        |r| r.get::<_, String>(0),
    )?;
    Ok(rows
        .into_iter()
        .filter_map(|conv| ConvKey::parse(&conv).ok())
        .collect())
}

/// The store's facts a step's group revokes are judged by (D11, M3; [`GroupFacts`]): for each `(group, sender)` whether the
/// group's member list was stated, and the pairs of `lid_pn_mapping` the sender has (every LID of a number, the number of a LID).
pub(crate) fn group_facts(
    conn: &Connection,
    wants: &[(ConvKey, UserId)],
) -> Result<GroupFacts, StoreError> {
    let mut facts = GroupFacts::default();
    for (group, sender) in wants {
        if group_stated(conn, group)? {
            facts = facts.with_members_of(group);
        }
        match sender.scheme() {
            "pn" => {
                for lid in rows(
                    conn,
                    "read the LIDs of a number",
                    "SELECT lid FROM lid_pn_mapping WHERE phone = ?1",
                    [sender.value()],
                    |r| r.get::<_, String>(0),
                )? {
                    if let Ok(lid) = UserId::from_parts("lid", &lid) {
                        facts = facts.with_pair(sender, &lid);
                    }
                }
            }
            "lid" => {
                if let Some(number) = first(
                    conn,
                    "read the number of a LID",
                    "SELECT phone FROM lid_pn_mapping WHERE lid = ?1",
                    [sender.value()],
                    |r| r.get::<_, String>(0),
                )? && let Ok(number) = UserId::from_parts("pn", &number)
                {
                    facts = facts.with_pair(&number, sender);
                }
            }
            _ => {}
        }
    }
    Ok(facts)
}

/// Whether the core was told the whole member list of `group`.
fn group_stated(conn: &Connection, group: &ConvKey) -> Result<bool, StoreError> {
    Ok(first(
        conn,
        "read whether a group's members are stated",
        "SELECT members_stated FROM group_state WHERE conv = ?1",
        params![group.encode()],
        |r| r.get::<_, i64>(0),
    )?
    .is_some_and(|stated| stated != 0))
}

/// An admin's delete-for-everyone of `group` waits for the group's member list (D11, M2): the inbox row `arrival` is parked
/// until `until_ms` (its arrival + 45 days) and marked with the group, and NO poison attempt is counted. `true` when the row
/// was still `new` and is parked.
pub(crate) fn wait_for_members(
    tx: &Transaction<'_>,
    arrival: i64,
    group: &ConvKey,
    until_ms: i64,
) -> Result<bool, StoreError> {
    exec(
        tx,
        "park an admin's delete for the member list",
        "UPDATE inbox SET parked_until = ?2, members_wait = ?3 WHERE arrival = ?1 AND state = 'new'",
        params![arrival, until_ms, group.encode()],
    )
    .map(|changed| changed > 0)
}

/// The zone that stated the member list of `group` lets its waiting rows go: they are due at once (the caller wakes stage 2
/// after the commit). How many.
pub(crate) fn release_waiting(tx: &Transaction<'_>, group: &ConvKey) -> Result<usize, StoreError> {
    exec(
        tx,
        "let a group's waiting deletes go",
        "UPDATE inbox SET parked_until = 0, members_wait = NULL WHERE state = 'new' AND members_wait = ?1",
        params![group.encode()],
    )
}

/// The answer says we are not in `group` (or it is gone): its waiting rows can never be judged and are finished with no event
/// (M3), their content wiped. How many.
pub(crate) fn end_waiting(tx: &Transaction<'_>, group: &ConvKey) -> Result<usize, StoreError> {
    exec(
        tx,
        "finish a group's waiting deletes",
        "UPDATE inbox SET state = 'done', message = NULL, meta = NULL, parked_until = 0, members_wait = NULL \
         WHERE state = 'new' AND members_wait = ?1",
        params![group.encode()],
    )
}

#[cfg(test)]
mod tests;

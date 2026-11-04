//! The token gate (W5a, decision D12; critic M5): the first sends after a link wait for the chat's trusted-contact
//! token while the phone's history may still bring it.
//!
//! WHY. Upstream attaches a stored, unexpired tctoken to a one-to-one message by itself (`maybe_include_tc_token`);
//! without one the message goes out BARE and the server may answer 463, which fails the chat's waiting messages for
//! good (no Send Again, D6). Right after a link the chat's token is usually still in the history: a chunk
//! (`Conversation.tcToken*`) captured and not delivered yet, or not pushed yet. The delivery zone writes it
//! (`history::deliver`, kill-safe); until then a send would be bare.
//!
//! THE RULE ([`waits`]): a Transmit into a DIRECT chat is answered `Transient{retry_after: Some(`[`TOKEN_RETRY`]`)}`
//! when ALL hold: no form of the chat (its `pn:` and `lid:` ids by the store's mapping) has a stored token with bytes;
//! the link is less than [`TOKEN_WAIT_BOUND`] ago; and the link's history push is not over: the initial bootstrap has
//! not been delivered up to the phone's 100 % (`history_chunks`), or a chunk of the stage still waits for its delivery
//! (`history_cursor.live_floor` set). After the bound the send goes out bare whatever the history does. A group is
//! never held (sender keys; no token).
//!
//! THE LINK'S TIME: the earliest recorded history notice (`history_announced.announced_at`, our phone announces its
//! push right after the pairing), or this run's own link (`AccountReady` recorded by the login) when that is earlier or
//! no notice is recorded yet. Neither known (an account linked in an earlier run whose notices are gone): no wait.
//! Both are the wall clock in production (the notice is stamped by stage 2's clock).
//!
//! NOT CHECKED: whether a stored token has EXPIRED (upstream's rule decides that at the send; the history would at
//! best bring the same token again). Nothing here logs an id, a token or a time.

use std::time::Duration;

use kourier_db::rusqlite::{Connection, OptionalExtension as _};
use kourier_model::ConvKey;
use wacore::store::error::StoreError;

use super::started;
use crate::error::sql;
use crate::history::progress::sync;

/// What a send answers while it waits for the chat's token: the core tries again after this.
pub(crate) const TOKEN_RETRY: Duration = Duration::from_secs(60);

/// How long after the link a send waits for the history's token at most; then it is sent bare.
pub(crate) const TOKEN_WAIT_BOUND: Duration = Duration::from_secs(30 * 60);

/// What the store says for the gate (all read in the caller's job).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    /// A form of the chat has a stored token with bytes.
    pub(crate) has_token: bool,
    /// The earliest recorded history notice (ms).
    pub(crate) first_notice_ms: Option<i64>,
    /// The link's initial history push is not over (module docs).
    pub(crate) push_running: bool,
}

/// The rule (module docs), pure: `direct` is whether the chat is a one-to-one chat, `linked_ms` this run's link.
pub(crate) fn decide(direct: bool, facts: Facts, linked_ms: Option<i64>, now_ms: i64) -> bool {
    if !direct || facts.has_token || !facts.push_running {
        return false;
    }
    let link = match (facts.first_notice_ms, linked_ms) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) | (None, Some(a)) => a,
        (None, None) => return false,
    };
    let bound = i64::try_from(TOKEN_WAIT_BOUND.as_millis()).unwrap_or(i64::MAX);
    now_ms < link.saturating_add(bound)
}

/// Reads the facts of `conv` (a group reads nothing: it never waits).
pub(crate) fn facts(conn: &Connection, conv: &ConvKey) -> Result<Facts, StoreError> {
    if !matches!(conv, ConvKey::Direct(_)) {
        return Ok(Facts::default());
    }
    let mut has_token = false;
    for form in started::forms(conn, conv)? {
        let ConvKey::Direct(user) = form else {
            continue;
        };
        if conn
            .prepare_cached("SELECT 1 FROM tc_tokens WHERE jid = ?1 AND length(token) > 0")
            .and_then(|mut s| s.query_row([user.value()], |_| Ok(())).optional())
            .map_err(sql("read a chat's tc token"))?
            .is_some()
        {
            has_token = true;
            break;
        }
    }
    let first_notice_ms: Option<i64> = conn
        .prepare_cached("SELECT min(announced_at) FROM history_announced")
        .and_then(|mut s| s.query_row([], |r| r.get(0)))
        .map_err(sql("read the first history notice"))?;
    let waiting: bool = conn
        .prepare_cached("SELECT live_floor IS NOT NULL FROM history_cursor WHERE id = 1")
        .and_then(|mut s| s.query_row([], |r| r.get(0)).optional())
        .map_err(sql("read whether history waits"))?
        .unwrap_or(false);
    let bootstrap_done = conn
        .prepare_cached(
            "SELECT 1 FROM history_chunks WHERE sync_type = ?1 AND progress >= 100 LIMIT 1",
        )
        .and_then(|mut s| {
            s.query_row([sync::INITIAL_BOOTSTRAP], |_| Ok(()))
                .optional()
        })
        .map_err(sql("read whether the bootstrap was delivered"))?
        .is_some();
    Ok(Facts {
        has_token,
        first_notice_ms,
        push_running: waiting || !bootstrap_done,
    })
}

#[cfg(test)]
mod tests;

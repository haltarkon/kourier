//! The core's side of the chat settings a network keeps for the account's other devices
//! ("roaming": archive, pin, mute, marked unread; architecture §4.1 principle 4, §8.3).
//!
//! Three facts decide what the UI may offer for a chat, and the core states them itself so that
//! no front-end re-implements them: the ACCOUNT's statement ([`AccountCaps::roaming_writes`]:
//! absent, off, on; stated by the backend, read from the stored caps ONCE per query), the CHAT's
//! ([`ConversationFeatures::lists_for_roaming`]: whether the network lists the chat at all) and,
//! for a pin, the account's pin count against its limit, when the network states one. The same
//! facts are the core's second line when such a change comes in ([`check`]): the backend is the
//! gate, the core refuses what it can see to be refused without asking. The words of every
//! refusal carry no content.
//!
//! A caps value this build cannot read (damaged, or written by a later build) makes ONE account
//! state nothing for the STATE the queries show ([`AccountRoaming::read`]: `Hidden`, one warn
//! line with the account's id), so the other accounts' chats still list; [`check`], the second
//! line, stays strict (a refusal of the click is the safe side).

use std::collections::HashMap;

use kourier_db::rusqlite::{Connection, OptionalExtension};
use kourier_model::{AccountCaps, ConversationChange, ConversationId};

use crate::error::{CoreError, Refusal, Result};
use crate::facade::Roaming;
use crate::sql;

/// What one account's stored caps say about roaming writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AccountRoaming {
    /// [`AccountCaps::roaming_writes`]: `None` = the network cannot, `Some(false)` = it can and
    /// the account's switch is off, `Some(true)` = on.
    pub writes: Option<bool>,
    /// How many chats the account may pin: [`AccountCaps::max_pinned_chats`] while the flag is
    /// stated; `None` = the network states no limit (the core refuses no pin for a count) or the
    /// flag is not stated at all. A network that has a limit states it.
    pub pin_limit: Option<u32>,
}

impl AccountRoaming {
    /// From an account's caps (`None`: nothing stated yet).
    pub(crate) fn of(caps: Option<&AccountCaps>) -> Self {
        let writes = caps.and_then(AccountCaps::roaming_writes);
        let pin_limit = writes.and(caps.and_then(|c| c.max_pinned_chats));
        Self { writes, pin_limit }
    }

    /// From the stored `accounts.caps` text; a text this build cannot read is an error (the
    /// strict form, for [`check`]).
    pub(crate) fn of_stored(caps: Option<String>) -> Result<Self> {
        let caps: Option<AccountCaps> = sql::from_json_opt(caps)?;
        Ok(Self::of(caps.as_ref()))
    }

    /// From the stored `accounts.caps` text of account `account`, for the STATE a query shows:
    /// a text this build cannot read states nothing (`writes: None`: the chats are `Hidden`,
    /// the reading's clear is the plain send) and leaves ONE warn line with the account's id
    /// (never the text or the parse error: a damaged value may hold content).
    pub(crate) fn read(account: i64, caps: Option<String>) -> Self {
        Self::of_stored(caps).unwrap_or_else(|_| {
            tracing::warn!(
                account,
                "the account's stored caps cannot be read; it states no roaming settings"
            );
            Self::of(None)
        })
    }

    /// The state of a chat of this account: the account's statement first, then whether the
    /// network lists the chat. A chat the network does not list offers nothing, whatever the
    /// account's switch says (turning it on would not help).
    pub(crate) fn state(self, listed: bool) -> Roaming {
        match self.writes {
            None => Roaming::Hidden,
            Some(_) if !listed => Roaming::NotListed,
            Some(false) => Roaming::Off,
            Some(true) => Roaming::On,
        }
    }
}

/// Every account's statement, from ONE read of `accounts.caps` (parsed once per account, here,
/// never per row and never in SQL): what a query that builds rows of several accounts holds. An
/// account whose caps cannot be read states nothing ([`AccountRoaming::read`]); the others are
/// not affected.
pub(crate) fn accounts(conn: &Connection) -> Result<HashMap<i64, AccountRoaming>> {
    let rows: Vec<(i64, Option<String>)> = conn
        .prepare_cached("SELECT id, caps FROM accounts")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(|(id, caps)| Ok((id, AccountRoaming::read(id, caps))))
        .collect()
}

/// The second line for a roaming change the UI asks for (the first is that the UI offers it only
/// where [`AccountRoaming::state`] is [`Roaming::On`]; the backend is the gate): `SetArchived`,
/// `SetPinned`, `SetMuted` and `SetMarkedUnread` of a chat are refused when its account does not
/// state `roaming_writes == Some(true)` or the network does not list the chat
/// (`Refusal::NotAllowedHere`), and, when the network states a pin limit, a pin that would be the
/// account's one too many is refused (`Refusal::TooMany`; no stated limit: no refusal for a
/// count). Every other change (a timer, accepting a request, leaving, blocking) is not roaming
/// ([`ConversationChange::is_roaming`]) and is not looked at. The pins are counted PER ACCOUNT: a
/// blended list holds the chats of several accounts, and the change goes to the chat's own
/// account. The caps are read strictly: a value this build cannot read is an error and the click
/// is refused.
///
/// The count is the pinned chats the core LISTS (a chat deleted here is hidden and is not
/// counted, as no front-end can count it); the backend's gate counts what the network lists
/// (entries this device does not show too) and is the real limit.
pub(crate) fn check(
    conn: &Connection,
    conv: ConversationId,
    change: &ConversationChange,
) -> Result<()> {
    let pin = match change {
        ConversationChange::SetArchived { .. }
        | ConversationChange::SetMuted { .. }
        | ConversationChange::SetMarkedUnread { .. } => false,
        ConversationChange::SetPinned { pinned } => *pinned,
        ConversationChange::SetBlocked { .. }
        | ConversationChange::SetTimer { .. }
        | ConversationChange::AcceptRequest
        | ConversationChange::Leave => return Ok(()),
    };
    let row: Option<(i64, Option<String>, Option<String>, bool)> = conn
        .prepare_cached(
            "SELECT c.account_id, a.caps, c.features, c.pinned_order IS NOT NULL
             FROM conversations c JOIN accounts a ON a.id = c.account_id WHERE c.id = ?1",
        )?
        .query_row([conv.0], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .optional()?;
    let Some((account, caps, features, pinned_now)) = row else {
        return Err(CoreError::UnknownConversation(conv));
    };
    let account_state = AccountRoaming::of_stored(caps)?;
    match account_state.writes {
        None => {
            return Err(CoreError::refused(
                Refusal::NotAllowedHere,
                "the account's network keeps no such setting for the account's other devices",
            ));
        }
        Some(false) => {
            return Err(CoreError::refused(
                Refusal::NotAllowedHere,
                "this account does not change such settings on its other devices",
            ));
        }
        Some(true) => {}
    }
    let listed = sql::from_json_opt::<kourier_model::ConversationFeatures>(features)?
        .is_none_or(|f| f.lists_for_roaming());
    if !listed {
        return Err(CoreError::refused(
            Refusal::NotAllowedHere,
            "the network does not list this chat yet",
        ));
    }
    // A pin of a chat that is pinned already adds none; a network that states no limit refuses
    // none for a count.
    if pin
        && !pinned_now
        && let Some(limit) = account_state.pin_limit
    {
        let pinned: i64 = conn
            .prepare_cached(
                "SELECT count(*) FROM conversations WHERE account_id = ?1
                   AND pinned_order IS NOT NULL AND hidden = 0",
            )?
            .query_row([account], |r| r.get(0))?;
        if pinned >= i64::from(limit) {
            return Err(CoreError::refused(
                Refusal::TooMany,
                "the account has pinned as many chats as it may",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kourier_backend_api::BackendEvent;
    use kourier_model::{
        ConvKey, ConversationFeatures, ConversationPatch, MsgIdScope, NetworkKind, Patch, UserId,
    };

    const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";

    fn person(n: u32) -> ConvKey {
        ConvKey::Direct(
            UserId::parse(&format!("aci:00000000-0000-4000-8000-{:012x}", 0x2000 + n))
                .expect("user id"),
        )
    }

    fn caps(limit: Option<u32>) -> AccountCaps {
        let mut caps = AccountCaps {
            max_pinned_chats: limit,
            ..AccountCaps::default()
        };
        caps.flags
            .insert(AccountCaps::FLAG_ROAMING_WRITES.to_owned(), true);
        caps
    }

    /// An account that states the flag on and `limit`, with chats `1..=chats` of which the first
    /// `pinned` are pinned (places 0, 1, ...).
    fn harness(
        dir: &std::path::Path,
        limit: Option<u32>,
        chats: u32,
        pinned: u32,
    ) -> (crate::testing::IngestHarness, Vec<ConversationId>) {
        let mut h = crate::testing::IngestHarness::open(
            &dir.join("core.db"),
            &kourier_db::DbKey::from([4u8; 32]),
            NetworkKind::Fake,
            MsgIdScope::AccountAuthor,
            &[UserId::parse(ME).expect("user id")],
        )
        .expect("harness");
        let mut events = vec![BackendEvent::AccountCaps(caps(limit))];
        for n in 1..=chats {
            let mut patch = ConversationPatch::new(person(n));
            patch.features = Some(ConversationFeatures::default());
            if n <= pinned {
                patch.pinned_order = Patch::Set(n - 1);
            }
            events.push(BackendEvent::ConversationUpsert(patch));
        }
        h.apply(events).expect("apply");
        let ids = (1..=chats)
            .map(|n| {
                crate::sql::conversation_id(h.connection(), h.account(), &person(n))
                    .expect("lookup")
                    .expect("the chat")
            })
            .collect();
        (h, ids)
    }

    fn pin() -> ConversationChange {
        ConversationChange::SetPinned { pinned: true }
    }

    fn refusal(r: Result<()>) -> Option<Refusal> {
        match r {
            Ok(()) => None,
            Err(e) => Some(
                e.refusal()
                    .unwrap_or_else(|| panic!("not a refusal: {e:?}")),
            ),
        }
    }

    /// F9: the pins the core counts against the limit are the pinned chats it LISTS: a pinned
    /// chat that was deleted here (hidden, it keeps its place in the network's list) is not
    /// counted, as no front-end can count it. Mutant: the count includes hidden chats.
    #[test]
    fn a_deleted_pinned_chat_does_not_count_against_the_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut h, ids) = harness(dir.path(), Some(2), 3, 2);
        assert_eq!(
            refusal(check(h.connection(), ids[2], &pin())),
            Some(Refusal::TooMany),
            "two pinned of two"
        );
        h.clear_conversation(ids[0], true).expect("delete");
        let hidden: bool = h
            .connection()
            .query_row(
                "SELECT hidden FROM conversations WHERE id = ?1",
                [ids[0].0],
                |r| r.get(0),
            )
            .expect("row");
        assert!(hidden, "the deleted chat is hidden");
        assert_eq!(
            refusal(check(h.connection(), ids[2], &pin())),
            None,
            "one listed pin of two: a place is free"
        );
    }

    /// F1: no stated limit is no limit: the count is not looked at (the network's own gate is the
    /// limit). With a stated limit the same chats are refused. Mutant: `None` counts as 4.
    #[test]
    fn a_pin_is_refused_for_a_count_only_at_a_stated_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut h, ids) = harness(dir.path(), None, 6, 5);
        assert_eq!(
            AccountRoaming::of(Some(&caps(None))),
            AccountRoaming {
                writes: Some(true),
                pin_limit: None
            }
        );
        assert_eq!(
            refusal(check(h.connection(), ids[5], &pin())),
            None,
            "a sixth pin, no limit stated"
        );
        h.apply(vec![BackendEvent::AccountCaps(caps(Some(5)))])
            .expect("the network states a limit");
        assert_eq!(
            refusal(check(h.connection(), ids[5], &pin())),
            Some(Refusal::TooMany),
            "five of five"
        );
        assert_eq!(
            AccountRoaming::of(None),
            AccountRoaming {
                writes: None,
                pin_limit: None
            },
            "a limit without the flag states nothing"
        );
        let mut limit_only = caps(Some(3));
        limit_only.flags.clear();
        assert_eq!(AccountRoaming::of(Some(&limit_only)).pin_limit, None);
    }

    /// F7: caps this build cannot read state nothing for the STATE (`read`, `accounts`): the
    /// account is `Hidden`, the other accounts keep their statement; `check` stays strict (the
    /// click is refused as a failure, not as a refusal of the core's words). Mutant: `read`
    /// fails instead of stating nothing.
    #[test]
    fn unreadable_caps_state_nothing_and_check_stays_strict() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (h, ids) = harness(dir.path(), None, 1, 0);
        let own = h.account().0;
        let other = crate::local::insert_account(
            h.connection(),
            kourier_model::AccountUuid::new_random(),
            NetworkKind::Fake,
            &kourier_model::AccountState::Active,
            0,
        )
        .expect("a second account")
        .0;
        h.connection()
            .execute(
                "UPDATE accounts SET caps = 'not json at all' WHERE id = ?1",
                [other],
            )
            .expect("damage");
        let damaged: Option<String> = Some("not json at all".into());
        assert!(
            AccountRoaming::of_stored(damaged.clone()).is_err(),
            "strict"
        );
        assert_eq!(
            AccountRoaming::read(other, damaged),
            AccountRoaming::of(None),
            "states nothing"
        );
        let all = accounts(h.connection()).expect("every account");
        assert_eq!(all[&other], AccountRoaming::of(None));
        assert_eq!(
            all[&own].writes,
            Some(true),
            "the other account is as it was"
        );
        // (the chat of the readable account is checked as ever)
        assert_eq!(
            refusal(check(
                h.connection(),
                ids[0],
                &ConversationChange::SetArchived { archived: true }
            )),
            None
        );
    }
}

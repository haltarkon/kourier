//! `CoreSource`: the filter of shown accounts (blended chats) against the simulated core of
//! [`super::tests`]: the hidden uuids resolve once per call, nothing hidden-dependent is listed
//! before the accounts answer, a search finds the hidden accounts' chats, the archive's count
//! follows the shown accounts and the unread total counts them all.

use kourier_core::facade::{ConversationPage, Query, QueryData, QueryToken};
use kourier_model::AccountUuid;

use super::tests::{Sent, Store, account, conv_row, result, source, take_queries};
use super::*;

pub(super) fn uuid(n: u128) -> AccountUuid {
    AccountUuid(uuid::Uuid::from_u128(n))
}

/// Two accounts, ids 1 and 2, uuids 1 and 2.
pub(super) fn two_accounts() -> Vec<CoreAccount> {
    (1..=2)
        .map(|n| CoreAccount {
            id: AccountId(n),
            uuid: uuid(n as u128),
            ..account()
        })
        .collect()
}

/// A chat of `account`.
pub(super) fn chat(
    id: i64,
    account: i64,
    title: &str,
    last: i64,
    archived: bool,
    unread: u32,
) -> CoreConversation {
    CoreConversation {
        account: AccountId(account),
        archived,
        unread,
        ..conv_row(id, title, last)
    }
}

/// Answers what was asked: the list with `rows` at once, the accounts only when `accounts` is
/// given. The held accounts query is handed back.
pub(super) fn answer(
    src: &mut CoreSource,
    sent: &Sent,
    rows: &[CoreConversation],
    accounts: Option<&[CoreAccount]>,
) -> Vec<(QueryToken, Query)> {
    let store = Store::default();
    let mut held = Vec::new();
    for _ in 0..100 {
        let queries = take_queries(sent);
        if queries.is_empty() {
            return held;
        }
        for (token, query) in queries {
            let data = match (&query, accounts) {
                (Query::Conversations(_), _) => QueryData::Conversations(ConversationPage {
                    rows: rows.to_vec(),
                    next: None,
                }),
                (Query::Accounts, Some(a)) => QueryData::Accounts(a.to_vec()),
                (Query::Accounts, None) => {
                    held.push((token, query));
                    continue;
                }
                (other, _) => store.answer(other),
            };
            src.on_event(&result(token, data));
        }
    }
    panic!("queries do not settle");
}

/// A source with two accounts and `rows` listed.
fn two_account_source(rows: &[CoreConversation]) -> CoreSource {
    let (mut src, sent) = source();
    src.reload();
    answer(&mut src, &sent, rows, Some(&two_accounts()));
    src
}

pub(super) fn ids(rows: &[ConversationRow]) -> Vec<i64> {
    rows.iter().map(|r| r.id.0).collect()
}

pub(super) fn hiding(n: u128) -> ConversationFilter {
    ConversationFilter {
        hidden: [uuid(n)].into(),
        ..ConversationFilter::default()
    }
}

fn four_chats() -> Vec<CoreConversation> {
    vec![
        chat(1, 1, "Dogs club", 40, false, 2),
        chat(2, 2, "Dogs walk", 30, false, 3),
        chat(3, 1, "Cats", 20, false, 0),
        chat(4, 2, "Birds", 10, false, 0),
    ]
}

#[test]
fn the_account_rows_carry_their_uuid() {
    let src = two_account_source(&[]);
    let rows = src.accounts();
    assert_eq!(
        rows.iter().map(|r| r.uuid).collect::<Vec<_>>(),
        [uuid(1), uuid(2)]
    );
}

#[test]
fn a_hidden_accounts_chats_are_left_out() {
    let src = two_account_source(&four_chats());
    assert_eq!(
        ids(&src.conversations(&ConversationFilter::default())),
        [1, 2, 3, 4]
    );
    assert_eq!(ids(&src.conversations(&hiding(2))), [1, 3]);
    assert_eq!(ids(&src.conversations(&hiding(1))), [2, 4]);
    // A uuid no account has hides nothing.
    assert_eq!(ids(&src.conversations(&hiding(77))), [1, 2, 3, 4]);
}

#[test]
fn a_search_finds_hidden_accounts_chats_mixed_in_newest_first() {
    let src = two_account_source(&four_chats());
    let found = src.conversations(&ConversationFilter {
        query: "dogs".into(),
        ..hiding(2)
    });
    assert_eq!(
        ids(&found),
        [1, 2],
        "the hidden account's match is found, between the shown ones by time"
    );
    // Also when the match of the hidden account is the newest.
    let mut rows = four_chats();
    rows[1].last_activity = 99;
    let src = two_account_source(&rows);
    let found = src.conversations(&ConversationFilter {
        query: "dogs".into(),
        ..hiding(2)
    });
    assert_eq!(ids(&found), [2, 1]);
}

#[test]
fn nothing_hidden_is_listed_before_the_accounts_answer() {
    // The conversations answer first; the accounts have not.
    let (mut src, sent) = source();
    src.reload();
    let held = answer(&mut src, &sent, &four_chats(), None);
    assert!(!src.accounts_known());
    assert!(!held.is_empty(), "the accounts query waits");
    // A hidden set lists nothing: no flash of the hidden account's chats ...
    assert!(src.conversations(&hiding(2)).is_empty());
    // ... but an empty set depends on nothing, and a search ignores the set.
    assert_eq!(
        ids(&src.conversations(&ConversationFilter::default())),
        [1, 2, 3, 4]
    );
    let found = src.conversations(&ConversationFilter {
        query: "dogs".into(),
        ..hiding(2)
    });
    assert_eq!(ids(&found), [1, 2]);
    // The archive's count waits too.
    assert_eq!(src.archived_count(&hiding(2)), 0);
    // The accounts answer: the set resolves.
    for (token, query) in held {
        assert!(matches!(query, Query::Accounts));
        src.on_event(&result(token, QueryData::Accounts(two_accounts())));
    }
    answer(&mut src, &sent, &four_chats(), Some(&two_accounts()));
    assert!(src.accounts_known());
    assert_eq!(ids(&src.conversations(&hiding(2))), [1, 3]);
}

#[test]
fn the_archive_count_counts_shown_accounts_only() {
    let mut rows = four_chats();
    rows.push(chat(5, 1, "Old one", 5, true, 0));
    rows.push(chat(6, 2, "Old two", 4, true, 0));
    rows.push(chat(7, 2, "Old three", 3, true, 0));
    let src = two_account_source(&rows);
    assert_eq!(src.archived_count(&ConversationFilter::default()), 3);
    assert_eq!(src.archived_count(&hiding(2)), 1);
    assert_eq!(src.archived_count(&hiding(1)), 2);
    // Not by the query.
    let q = ConversationFilter {
        query: "zzz".into(),
        ..hiding(2)
    };
    assert_eq!(src.archived_count(&q), 1);
}

#[test]
fn the_unread_total_counts_hidden_accounts() {
    let src = two_account_source(&four_chats());
    // The list without account 2 lacks its unread chat; the taskbar's total counts it.
    let shown: u32 = src.conversations(&hiding(2)).iter().map(|r| r.unread).sum();
    assert_eq!(shown, 2);
    assert_eq!(
        src.unread_total(),
        5,
        "the hidden account's messages count too"
    );
}

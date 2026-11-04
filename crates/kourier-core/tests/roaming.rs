//! A10a (chunk N3): what the core says and refuses about the chat settings a network keeps for
//! the account's other devices ("roaming": archive, pin, mute, marked unread; architecture §4.1
//! principle 4, §8.3).
//!
//! The ACCOUNT states whether its network writes them (`AccountCaps::roaming_writes`: absent =
//! cannot, `Some(false)` = the account's switch is off, `Some(true)` = on) and how many chats it
//! may pin; the NETWORK states per chat whether it lists the chat (`roaming_listed`). The core
//! reads both into `AccountRow::roaming_writes` / `pin_limit` (`None` = no limit stated) and
//! `ConversationRow::roaming`, and
//! refuses a change that its second line can see to be refused (`ChangeConversation` of
//! `SetArchived`, `SetPinned`, `SetMuted`, `SetMarkedUnread`): nothing then reaches the backend.
//! A timer, a block, accepting a request and leaving are not roaming settings and are not
//! looked at. The pins are counted PER ACCOUNT (a blended list holds several; a change goes to
//! the chat's own account).

use crate::support;

use kourier_backend_api::BackendEvent;
use kourier_core::facade::{
    ActionSubject, ConversationListQuery, ConversationRow, Query, QueryData, Refusal, Roaming,
    UiCommand, UiEvent,
};
use kourier_core::testing::IngestHarness;
use kourier_fake::scripts::signal_features;
use kourier_fake::{Flavor, Script};
use kourier_model::{
    AccountCaps, AccountId, ConvKey, ConversationChange, ConversationId, ConversationPatch,
    MsgIdScope, NetworkKind, UserId,
};
use support::{Env, WAIT, add_linked, eventually};

const ME_A: &str = "aci:00000000-0000-4000-8000-0000000000a1";
const ME_B: &str = "aci:00000000-0000-4000-8000-0000000000b1";
const ME_C: &str = "aci:00000000-0000-4000-8000-0000000000c1";

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

/// The n-th synthetic person (a chat with them is `chat(n, ..)`).
fn friend(n: u32) -> UserId {
    uid(&format!("aci:00000000-0000-4000-8000-{:012x}", 0x1000 + n))
}

/// The account's statement of roaming writes (`flag`: absent, off, on) and of its pin limit.
fn caps(flag: Option<bool>, max_pinned_chats: Option<u32>) -> AccountCaps {
    let mut caps = AccountCaps {
        max_pinned_chats,
        ..AccountCaps::default()
    };
    if let Some(on) = flag {
        caps.flags
            .insert(AccountCaps::FLAG_ROAMING_WRITES.to_owned(), on);
    }
    caps
}

/// A direct chat with person `n`; `listed` is what the network says about it
/// (`ConversationFeatures::roaming_listed`).
fn chat(n: u32, listed: Option<bool>) -> BackendEvent {
    let mut patch = ConversationPatch::new(ConvKey::Direct(friend(n)));
    let mut features = signal_features(false);
    features.roaming_listed = listed;
    patch.features = Some(features);
    BackendEvent::ConversationUpsert(patch)
}

/// An account of `own` that states `flag` and `max_pins` first and has the chats `chats`
/// (person, listed).
fn script(
    name: &str,
    own: &str,
    flag: Option<bool>,
    max_pins: Option<u32>,
    chats: &[(u32, Option<bool>)],
) -> Script {
    let mut events = vec![BackendEvent::AccountCaps(caps(flag, max_pins))];
    events.extend(chats.iter().map(|(n, listed)| chat(*n, *listed)));
    // The script states the account's caps itself, also "no flag": the fake adds none (N4).
    Script::new(name, vec![uid(own)])
        .with_roaming_writes(None)
        .deliver(events)
}

/// A started core with one linked account (the script's) that has said everything.
async fn linked(script: Script) -> (Env, AccountId) {
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    (env, account)
}

/// A further linked account over the next pushed script.
async fn also(env: &mut Env, script: Script) -> AccountId {
    env.control.push_script(script);
    let account = add_linked(env).await;
    env.wait_finished(account).await;
    account
}

async fn row(env: &Env, account: AccountId, n: u32) -> ConversationRow {
    env.conversations()
        .await
        .into_iter()
        .find(|r| r.account == account && r.peer.as_ref() == Some(&friend(n)))
        .unwrap_or_else(|| panic!("a chat with person {n}"))
}

async fn conv_of(env: &Env, account: AccountId, n: u32) -> ConversationId {
    row(env, account, n).await.id
}

fn change(env: &Env, conv: ConversationId, change: ConversationChange) {
    env.handle
        .send(UiCommand::ChangeConversation { conv, change })
        .expect("send");
}

/// The four roaming changes, each with a value that changes a fresh chat.
fn roaming_changes() -> [ConversationChange; 4] {
    [
        ConversationChange::SetArchived { archived: true },
        ConversationChange::SetPinned { pinned: true },
        ConversationChange::SetMuted {
            until: Some(5_000_000_000_000),
        },
        ConversationChange::SetMarkedUnread {
            marked_unread: true,
        },
    ]
}

/// The next refused `ChangeConversation`: the chat it was about and the core's reason.
async fn refusal(env: &mut Env) -> (ConversationId, Option<Refusal>) {
    env.event("a refused ChangeConversation", |e| match e {
        UiEvent::ActionFailed {
            action: "ChangeConversation",
            subject: ActionSubject::Conversation(conv),
            error,
            ..
        } => Some((*conv, error.refusal())),
        _ => None,
    })
    .await
}

/// What the account's network has heard of `ChangeConversation`, in order.
fn heard(env: &Env, uuid: kourier_model::AccountUuid) -> Vec<ConversationChange> {
    env.control.changes(uuid)
}

/// An account whose statement is `flag` (absent or off): every roaming click on its chats is
/// refused, none reaches the network, and a timer and a block, which are no roaming settings,
/// still do.
async fn refused_clicks(flag: Option<bool>) {
    let (mut env, account) = linked(script("refused", ME_A, flag, None, &[(1, None)])).await;
    let uuid = env.account(account).await.expect("account").uuid;
    let conv = conv_of(&env, account, 1).await;
    for c in roaming_changes() {
        change(&env, conv, c);
        let (about, why) = refusal(&mut env).await;
        assert_eq!(
            (about, why),
            (conv, Some(Refusal::NotAllowedHere)),
            "{flag:?}"
        );
    }
    // (every click above had its refusal, so none was left to reach the network)
    let timer = ConversationChange::SetTimer {
        timer: Some(std::time::Duration::from_secs(360)),
    };
    let block = ConversationChange::SetBlocked { blocked: true };
    change(&env, conv, timer.clone());
    change(&env, conv, block.clone());
    let env_ref = &env;
    eventually(
        "the timer and the block reach the network",
        WAIT,
        || async move { heard(env_ref, uuid).len() >= 2 },
    )
    .await;
    assert_eq!(
        heard(&env, uuid),
        vec![timer, block],
        "the network heard nothing of the refused clicks"
    );
    let r = row(&env, account, 1).await;
    assert!(
        !r.archived && r.pinned_order.is_none() && r.muted_until.is_none() && !r.marked_unread,
        "nothing changed in the chat"
    );
    env.stop().await;
}

/// A chat of an account that states no `roaming_writes` (a network that cannot) refuses all
/// four roaming changes itself and nothing reaches the backend; the changes that are no roaming
/// settings go on. Mutant: the check is skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_click_on_a_chat_of_an_account_without_the_flag_is_refused_and_nothing_reaches_the_backend()
 {
    refused_clicks(None).await;
}

/// The same for an account that states the flag OFF (its network can, the switch is off): the
/// backend is the gate, the core is the second line. Mutant: `Some(false)` counts as on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_click_on_a_chat_of_an_account_whose_switch_is_off_is_refused() {
    refused_clicks(Some(false)).await;
}

/// With the flag ON the four changes go through, in the order of the clicks, and the chat takes
/// them from the backend's echo; no click is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_the_flag_on_the_changes_go_through() {
    let (mut env, account) = linked(script("on", ME_A, Some(true), None, &[(1, None)])).await;
    let uuid = env.account(account).await.expect("account").uuid;
    let conv = conv_of(&env, account, 1).await;
    for c in roaming_changes() {
        change(&env, conv, c);
    }
    let env_ref = &env;
    // (The network's own rule: the pin, behind the archive, took the chat out of the archive.)
    eventually("the chat took all four", WAIT, || async move {
        let r = row(env_ref, account, 1).await;
        !r.archived && r.pinned_order.is_some() && r.muted_until.is_some() && r.marked_unread
    })
    .await;
    assert_eq!(heard(&env, uuid), roaming_changes());
    while let Ok(e) = env.events.try_recv() {
        assert!(
            !matches!(e, UiEvent::ActionFailed { .. }),
            "a click was refused: {e:?}"
        );
    }
    env.stop().await;
}

/// A chat the network does not list (`roaming_listed: Some(false)`) is refused even when the
/// account's switch is on; a chat that states nothing or `Some(true)` is listed. The row says
/// so. Mutant: the chat's statement is not looked at.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chat_the_network_does_not_list_is_refused() {
    let chats = [(1, None), (2, Some(true)), (3, Some(false))];
    let (mut env, account) = linked(script("listed", ME_A, Some(true), None, &chats)).await;
    let uuid = env.account(account).await.expect("account").uuid;
    for (n, want) in [(1, Roaming::On), (2, Roaming::On), (3, Roaming::NotListed)] {
        assert_eq!(row(&env, account, n).await.roaming, want, "person {n}");
    }
    let unlisted = conv_of(&env, account, 3).await;
    for c in roaming_changes() {
        change(&env, unlisted, c);
        let (about, why) = refusal(&mut env).await;
        assert_eq!((about, why), (unlisted, Some(Refusal::NotAllowedHere)));
    }
    assert!(heard(&env, uuid).is_empty());
    // The listed chats, one with the statement and one without, take a click.
    for n in [1, 2] {
        change(
            &env,
            conv_of(&env, account, n).await,
            roaming_changes()[0].clone(),
        );
    }
    let env_ref = &env;
    eventually("the listed chats are archived", WAIT, || async move {
        row(env_ref, account, 1).await.archived && row(env_ref, account, 2).await.archived
    })
    .await;
    assert_eq!(heard(&env, uuid).len(), 2);
    env.stop().await;
}

/// The 5th pin of ONE account is refused (`Refusal::TooMany`, the account's network states a
/// limit of 4) while ANOTHER account pins: four pins in account A
/// do not refuse a pin in account B (a blended list holds both; a change goes to the chat's own
/// account). A pin of a chat that is pinned already adds none; an unpin frees a place.
/// Mutant: the pin count is skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fifth_pin_of_one_account_is_refused() {
    let a_chats: Vec<(u32, Option<bool>)> = (1..=6).map(|n| (n, None)).collect();
    let b_chats: Vec<(u32, Option<bool>)> = (11..=13).map(|n| (n, None)).collect();
    let (mut env, a) = linked(script("pins-a", ME_A, Some(true), Some(4), &a_chats)).await;
    let b = also(
        &mut env,
        script("pins-b", ME_B, Some(true), Some(4), &b_chats),
    )
    .await;
    let (uuid_a, uuid_b) = (
        env.account(a).await.expect("a").uuid,
        env.account(b).await.expect("b").uuid,
    );
    let pin = ConversationChange::SetPinned { pinned: true };
    let unpin = ConversationChange::SetPinned { pinned: false };

    // Four pins in A, each taken before the next is asked (the core counts what it shows).
    for n in 1..=4 {
        change(&env, conv_of(&env, a, n).await, pin.clone());
        let env_ref = &env;
        eventually("a pin is taken", WAIT, || async move {
            row(env_ref, a, n).await.pinned_order.is_some()
        })
        .await;
    }
    // The 5th of A is refused, and nothing of it reaches A's network.
    let fifth = conv_of(&env, a, 5).await;
    change(&env, fifth, pin.clone());
    assert_eq!(
        refusal(&mut env).await,
        (fifth, Some(Refusal::TooMany)),
        "the fifth pin of one account"
    );
    assert_eq!(heard(&env, uuid_a), vec![pin.clone(); 4]);
    assert!(row(&env, a, 5).await.pinned_order.is_none());

    // B has pinned nothing: its first pin goes through although A is full.
    let first_of_b = conv_of(&env, b, 11).await;
    change(&env, first_of_b, pin.clone());
    let env_ref = &env;
    eventually("B pins", WAIT, || async move {
        row(env_ref, b, 11).await.pinned_order.is_some()
    })
    .await;
    assert_eq!(heard(&env, uuid_b), vec![pin.clone()]);

    // A pin of a chat that is pinned already adds none: A at its limit still hears it.
    change(&env, conv_of(&env, a, 1).await, pin.clone());
    eventually(
        "the repeated pin reaches A's network",
        WAIT,
        || async move { heard(env_ref, uuid_a).len() == 5 },
    )
    .await;

    // An unpin frees a place for the fifth.
    change(&env, conv_of(&env, a, 1).await, unpin.clone());
    eventually("the unpin is taken", WAIT, || async move {
        row(env_ref, a, 1).await.pinned_order.is_none()
    })
    .await;
    change(&env, fifth, pin.clone());
    eventually(
        "the fifth chat is pinned in its turn",
        WAIT,
        || async move { row(env_ref, a, 5).await.pinned_order.is_some() },
    )
    .await;
    env.stop().await;
}

/// The limit is what the account's network states (`max_pinned_chats`), not a number of the
/// core's: with 2 stated, the third pin is refused. The account row says the limit it works
/// with. Mutant: the stated limit is ignored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pin_limit_is_what_the_network_states() {
    let chats: Vec<(u32, Option<bool>)> = (1..=3).map(|n| (n, None)).collect();
    let (mut env, account) = linked(script("limit", ME_A, Some(true), Some(2), &chats)).await;
    assert_eq!(env.account(account).await.expect("a").pin_limit, Some(2));
    let pin = ConversationChange::SetPinned { pinned: true };
    for n in 1..=2 {
        change(&env, conv_of(&env, account, n).await, pin.clone());
        let env_ref = &env;
        eventually("a pin is taken", WAIT, || async move {
            row(env_ref, account, n).await.pinned_order.is_some()
        })
        .await;
    }
    let third = conv_of(&env, account, 3).await;
    change(&env, third, pin);
    assert_eq!(refusal(&mut env).await, (third, Some(Refusal::TooMany)));
    env.stop().await;
}

/// `max_pinned_chats: None` means the network states NO limit (the model's words), in the core
/// too: an account that states the flag and no limit pins a fifth, a sixth and a seventh chat (the
/// core refuses nothing for a count, the fake's network has no limit to state), and its account
/// row says no limit. Mutant: `None` counts as the common 4 again (`TooMany` on the fifth).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_network_that_states_no_limit_takes_a_fifth_pin() {
    let chats: Vec<(u32, Option<bool>)> = (1..=7).map(|n| (n, None)).collect();
    let (mut env, account) = linked(script("no-limit", ME_A, Some(true), None, &chats)).await;
    let row_of = env.account(account).await.expect("a");
    assert_eq!(
        (row_of.roaming_writes, row_of.pin_limit),
        (Some(true), None)
    );
    let pin = ConversationChange::SetPinned { pinned: true };
    for n in 1..=7 {
        change(&env, conv_of(&env, account, n).await, pin.clone());
        let env_ref = &env;
        eventually("a pin is taken", WAIT, || async move {
            row(env_ref, account, n).await.pinned_order.is_some()
        })
        .await;
    }
    while let Ok(e) = env.events.try_recv() {
        assert!(
            !matches!(e, UiEvent::ActionFailed { .. }),
            "a pin was refused: {e:?}"
        );
    }
    env.stop().await;
}

/// The rows say what each account and chat offers, from the account's OWN statement: in one list
/// the chat of an account that states the flag on is `On`, one of an account whose switch is off
/// is `Off`, one of a network that states none is `Hidden`, and a chat the network does not list
/// is `NotListed`; the single-chat query says the same. The account rows carry the flag and the
/// pin limit (the stated one; none when the network states no limit or not the flag).
/// Mutant: every row takes the first account's statement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_rows_say_what_each_account_and_chat_offers() {
    let (mut env, on) = linked(script(
        "rows-on",
        ME_A,
        Some(true),
        None,
        &[(1, None), (2, Some(false))],
    ))
    .await;
    let off = also(
        &mut env,
        script("rows-off", ME_B, Some(false), Some(2), &[(1, None)]),
    )
    .await;
    let none = also(
        &mut env,
        script("rows-none", ME_C, None, None, &[(1, None)]),
    )
    .await;

    let accounts = env.accounts().await;
    let of = |id: AccountId| {
        let a = accounts.iter().find(|a| a.id == id).expect("account");
        (a.roaming_writes, a.pin_limit)
    };
    assert_eq!(of(on), (Some(true), None), "the flag without a limit");
    assert_eq!(of(off), (Some(false), Some(2)));
    assert_eq!(of(none), (None, None));

    let want = [
        (on, 1, Roaming::On),
        (on, 2, Roaming::NotListed),
        (off, 1, Roaming::Off),
        (none, 1, Roaming::Hidden),
    ];
    for (account, n, state) in want {
        let listed = row(&env, account, n).await;
        assert_eq!(listed.roaming, state, "account {account:?}, person {n}");
        let QueryData::Conversation(Some(one)) = env
            .handle
            .query(Query::Conversation(listed.id))
            .await
            .expect("the chat")
        else {
            panic!("the chat is there")
        };
        assert_eq!(one.row.roaming, state, "the single-chat query agrees");
    }
    env.stop().await;
}

/// Caps this build cannot read (damaged, or written by a later build) of ONE account state
/// nothing for it: its chats are `Hidden` and its row says no flag, the OTHER account's chats
/// and row are untouched (the blended list, the account list and the single-chat query all
/// answer), and the click on the damaged account's chat is still refused (the second line stays
/// strict; nothing reaches its network) while the other account's click goes through.
/// Mutant: the unreadable value fails the whole query again (`of_stored(caps)?`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_accounts_unreadable_caps_break_nothing_else() {
    let (mut env, damaged) = linked(script("dmg", ME_A, Some(true), None, &[(1, None)])).await;
    let fine = also(
        &mut env,
        script("fine", ME_B, Some(true), None, &[(1, None)]),
    )
    .await;
    let uuid_damaged = env.account(damaged).await.expect("a").uuid;
    // The stored value of the first account cannot be read by this build.
    {
        let conn = kourier_db::open(&env.core_db(), &kourier_db::DbKey::from(support::CORE_KEY))
            .expect("open core.db");
        let n = conn
            .execute(
                "UPDATE accounts SET caps = '{\"max_pinned_chats\":\"many\"}' WHERE id = ?1",
                [damaged.0],
            )
            .expect("damage");
        assert_eq!(n, 1);
    }
    let accounts = env.accounts().await;
    let of = |id: AccountId| {
        let a = accounts.iter().find(|a| a.id == id).expect("account");
        (a.roaming_writes, a.pin_limit)
    };
    assert_eq!(of(damaged), (None, None), "states nothing");
    assert_eq!(of(fine), (Some(true), None), "the other is as it was");
    let hidden = row(&env, damaged, 1).await;
    assert_eq!(hidden.roaming, Roaming::Hidden);
    assert_eq!(row(&env, fine, 1).await.roaming, Roaming::On);
    let QueryData::Conversation(Some(one)) = env
        .handle
        .query(Query::Conversation(hidden.id))
        .await
        .expect("the chat")
    else {
        panic!("the chat is there")
    };
    assert_eq!(one.row.roaming, Roaming::Hidden);

    // The second line is strict: the click on the damaged account's chat is refused as a
    // failure (not a `Refusal`: the core cannot tell what the account states) and nothing
    // reaches its network; the other account's chat takes the click.
    change(&env, hidden.id, roaming_changes()[0].clone());
    let (about, why) = refusal(&mut env).await;
    assert_eq!((about, why), (hidden.id, None));
    change(
        &env,
        conv_of(&env, fine, 1).await,
        roaming_changes()[0].clone(),
    );
    let env_ref = &env;
    eventually("the other account archives", WAIT, || async move {
        row(env_ref, fine, 1).await.archived
    })
    .await;
    assert!(heard(&env, uuid_damaged).is_empty());
    env.stop().await;
}

/// The statement of an account moves what its chats offer, so the core names the account's chats
/// as changed when the statement changes (a front-end re-reads rows it is told changed), and only
/// then: stated for the first time, switched on and off, withdrawn; not for the same statement
/// again or for another cap. The rows follow the statement. Mutant: the chats are not named.
#[test]
fn a_change_of_the_accounts_statement_names_its_chats() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([6; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME_A)],
    )
    .expect("harness");
    h.apply(vec![chat(1, None), chat(2, None)])
        .expect("two chats");
    let rows = |h: &IngestHarness| {
        h.conversations(&ConversationListQuery::default())
            .expect("list")
            .rows
    };
    let mut ids: Vec<ConversationId> = rows(&h).iter().map(|r| r.id).collect();
    ids.sort();
    assert_eq!(ids.len(), 2);
    let states = |h: &IngestHarness| rows(h).iter().map(|r| r.roaming).collect::<Vec<_>>();
    let stated = |h: &mut IngestHarness, c: AccountCaps| {
        let mut named = h
            .apply_announcing(vec![BackendEvent::AccountCaps(c)])
            .expect("caps")
            .conversations;
        named.sort();
        named
    };

    assert_eq!(states(&h), vec![Roaming::Hidden; 2]);
    assert_eq!(
        stated(&mut h, caps(Some(false), None)),
        ids,
        "stated for the first time"
    );
    assert_eq!(states(&h), vec![Roaming::Off; 2]);
    assert!(
        stated(&mut h, caps(Some(false), None)).is_empty(),
        "the same again"
    );
    assert!(
        stated(&mut h, caps(Some(false), Some(3))).is_empty(),
        "another cap moves no row"
    );
    assert_eq!(
        stated(&mut h, caps(Some(true), Some(3))),
        ids,
        "switched on"
    );
    assert_eq!(states(&h), vec![Roaming::On; 2]);
    assert_eq!(stated(&mut h, caps(None, None)), ids, "withdrawn");
    assert_eq!(states(&h), vec![Roaming::Hidden; 2]);
}

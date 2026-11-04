//! P5a: the safety number of a person through the account's backend (`Query::SafetyNumber`,
//! the one query a backend answers) and `UiCommand::SetVerified` (Mark, Clear and Confirm),
//! on the fake ([`kourier_fake::SafetyScript`]): the answer and its refusals, the order of the
//! clicks, and the paused sends a verification releases (critic F2, H1, M4/F19).

use crate::support;

use std::time::Duration;

use kourier_backend_api::{BackendEvent, BackendNotice};
use kourier_core::facade::{ActionSubject, Query, QueryData, UiCommand, UiEvent};
use kourier_fake::scripts::{T0, own, remote};
use kourier_fake::{Flavor, Script, Step, TransmitRule};
use kourier_model::{
    AccountId, AccountUuid, ConvKey, ConversationId, IdentityFingerprint, Member, MemberRole,
    MessageId, OutgoingState, SafetyAnswer, SafetyUnavailable, Text, UserId, VerifyState,
};
use support::send::*;
use support::{Env, WAIT, add_linked, eventually};

fn key(b: u8) -> IdentityFingerprint {
    IdentityFingerprint([5].into_iter().chain([b; 32]).collect())
}

async fn linked(script: kourier_fake::Script) -> (Env, AccountId, AccountUuid) {
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("account").uuid;
    (env, account, uuid)
}

async fn conv(env: &Env, key: &ConvKey) -> ConversationId {
    let db = env.core_db();
    let key2 = key.clone();
    eventually("the conversation", WAIT, || {
        let (db, key) = (db.clone(), key2.clone());
        async move { conv_id(&db, &key).is_some() }
    })
    .await;
    conv_id(&db, key).expect("conversation")
}

async fn ask(env: &Env, account: AccountId, user: UserId) -> SafetyAnswer {
    match env
        .handle
        .query(Query::SafetyNumber { account, user })
        .await
        .expect("the query answers")
    {
        QueryData::SafetyNumber { answer, .. } => answer,
        _ => panic!("the safety-number query answered another kind"),
    }
}

fn set_verified(
    env: &Env,
    account: AccountId,
    user: UserId,
    verified: bool,
    key: IdentityFingerprint,
) {
    env.handle
        .send(UiCommand::SetVerified {
            account,
            user,
            verified,
            key,
        })
        .expect("send");
}

fn send(env: &Env, conv: ConversationId, text: &str) {
    env.handle
        .send(UiCommand::SendText {
            conv,
            text: Text::plain(text),
            reply_to: None,
        })
        .expect("send");
}

async fn queued(env: &Env, conv: ConversationId, n: usize) -> Own {
    let db = env.core_db();
    eventually("the queued message", WAIT, || {
        let db = db.clone();
        async move { own_messages(&db, conv).len() >= n }
    })
    .await;
    own_messages(&db, conv)[n - 1].clone()
}

async fn is_sent(env: &Env, conv: ConversationId, message: MessageId, what: &str) {
    let db = env.core_db();
    eventually(what, WAIT, || {
        let db = db.clone();
        async move {
            own_messages(&db, conv)
                .iter()
                .any(|m| m.id == message && matches!(m.out_state, Some(OutgoingState::Sent { .. })))
        }
    })
    .await;
}

/// Waits for the core's `IdentityChanged` notice (the pause) and returns whom it names.
async fn paused_on(env: &mut Env) -> UserId {
    env.event("IdentityChanged notice", |e| match e {
        UiEvent::Notice {
            notice: BackendNotice::IdentityChanged { user },
            ..
        } => Some(user.clone()),
        _ => None,
    })
    .await
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(150)).await;
}

/// The scripts' `Step::Sleep` before a release arrives (the fake's actor sleeps it from T0, the
/// fake clock's start).
const HOUR: Duration = Duration::from_secs(3600);

/// One poll of a wait for what the script delivers after its [`HOUR`]: moves the fake clock to
/// the next sleep only up to the hour's end, never past it. A wait that moved it to whatever
/// sleeps next jumped, once the hour was over and before the script's delivery landed, on to
/// the outbox's retry budget end (one day after the paused sends were queued): the paused sends
/// gave up there (`Dispatcher::kick`, a row of a paused conversation out of budget) and the
/// confirm that follows had nothing to send (Fix T: it failed whenever the delivery took longer
/// than one 5 ms poll).
fn through_the_hour(env: &Env) {
    let end = u64::try_from(T0).expect("T0 is after the epoch")
        + kourier_backend_api::clock::duration_ms(HOUR);
    if env.clock.next_deadline().is_some_and(|d| d <= end) {
        env.clock.advance_to_next();
    }
}

/// The core asks the account's backend: the held key's number, `NoKey` for a person without
/// one, `NotRunning` (its own answer, critic M7) for an account that does not run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn answers_through_the_backend() {
    let s = script("safety").with_safety(alice(), vec![(key(1), VerifyState::Verified)]);
    let (env, account, uuid) = linked(s).await;
    let n = ask(&env, account, alice())
        .await
        .expect("alice has a number");
    assert!(n.key == key(1), "the number is of the held key");
    assert_eq!(n.state, VerifyState::Verified);
    assert_eq!(n.digits.expose().len(), 60, "60 digits");
    assert!(env.control.commands(uuid).contains(&"SafetyNumber"));
    assert_eq!(
        ask(&env, account, bob()).await,
        Err(SafetyUnavailable::NoKey)
    );
    assert_eq!(
        ask(&env, account, uid(ME)).await,
        Err(SafetyUnavailable::OwnId)
    );
}

/// A person whose key the fake learns late (Y1, `Script::with_late_safety`): the first ask is "no
/// key yet" (`NoKey`, an answer WITHOUT a number), the next one has the number.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_key_is_no_key_yet_first() {
    let s =
        script("safety-late").with_late_safety(alice(), vec![(key(1), VerifyState::Default)], 1);
    let (env, account, _uuid) = linked(s).await;
    assert_eq!(
        ask(&env, account, alice()).await,
        Err(SafetyUnavailable::NoKey),
        "the key is not known yet"
    );
    let n = ask(&env, account, alice())
        .await
        .expect("the second ask has the number");
    assert!(n.key == key(1), "the number is of the key learnt");
}

/// An account the core does not run has no number: `NotRunning`, never "no key yet".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_running() {
    let s = script("safety-none").with_safety(alice(), vec![(key(1), VerifyState::Default)]);
    let (env, _account, _uuid) = linked(s).await;
    assert_eq!(
        ask(&env, AccountId(9_999), alice()).await,
        Err(SafetyUnavailable::NotRunning)
    );
}

/// A Mark and a Clear clicked back to back reach the backend in that order (critic H3: the
/// command goes to the supervisor from `ui_command` itself), so the state ends cleared; each
/// change writes a quiet notice into her chat that adds no unread.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mark_then_clear_in_order() {
    let s = script("safety-order").with_safety(alice(), vec![(key(1), VerifyState::Default)]);
    let (env, account, uuid) = linked(s).await;
    let ca = conv(&env, &d_alice()).await;
    let unread_before = unread(&env, ca);
    for _ in 0..3 {
        set_verified(&env, account, alice(), true, key(1));
        set_verified(&env, account, alice(), false, key(1));
    }
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| c.verified(uuid).len() >= 6))
        .await
        .expect("six SetVerified");
    let heard = env.control.verified(uuid);
    let want: Vec<(UserId, bool)> = (0..6).map(|i| (alice(), i % 2 == 0)).collect();
    assert!(heard == want, "the backend heard the clicks out of order");
    let n = ask(&env, account, alice()).await.expect("a number");
    assert_eq!(n.state, VerifyState::Default, "the last click was a Clear");
    let db = env.core_db();
    eventually("six quiet notices", WAIT, || {
        let db = db.clone();
        async move { verified_notices(&db, ca) == 6 }
    })
    .await;
    assert_eq!(
        unread(&env, ca),
        unread_before,
        "a quiet notice adds no unread"
    );
}

/// A Mark of a key that is no longer the held one is refused, and the refusal names the
/// person (the key changed after the page asked).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_key_is_refused() {
    let s = script("safety-stale").with_safety(
        alice(),
        vec![
            (key(1), VerifyState::Default),
            (key(2), VerifyState::Default),
        ],
    );
    let (mut env, account, _uuid) = linked(s).await;
    let shown = ask(&env, account, alice()).await.expect("a number");
    set_verified(&env, account, alice(), true, shown.key);
    let subject = env
        .event("SetVerified refused", |e| match e {
            UiEvent::ActionFailed {
                action: "SetVerified",
                subject,
                ..
            } => Some(subject.clone()),
            _ => None,
        })
        .await;
    assert_eq!(subject, ActionSubject::Person(alice()));
    let again = ask(&env, account, alice()).await.expect("a number");
    assert!(
        again.key == key(2),
        "the page asks again and gets the new key"
    );
    assert_eq!(again.state, VerifyState::Default, "nothing was marked");
}

/// Critic F2: a `SetVerified` the backend accepts (here a Clear of a DEFAULT contact, which
/// leaves the state as it was and sends no `IdentityTrusted`) releases that contact's paused
/// send: the core releases after the backend's answer all the same. This is NOT "Confirm New
/// Safety Number" (that is `UiCommand::ConfirmIdentity`, BQ-2b: its release after the
/// backend's answer is `a_stale_confirm_leaves_the_head_paused`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accepted_mark_or_clear_releases_a_default_contacts_pause() {
    let s = script("safety-default")
        .with_safety(alice(), vec![(key(1), VerifyState::Default)])
        .fail_transmit(TransmitRule::attempts(untrusted(alice()), 1).in_conv(d_alice()));
    let (mut env, account, uuid) = linked(s).await;
    let ca = conv(&env, &d_alice()).await;
    send(&env, ca, "one");
    let one = queued(&env, ca, 1).await;
    assert_eq!(paused_on(&mut env).await, alice());
    settle().await;
    assert_eq!(env.control.transmits(uuid).len(), 1, "paused");
    let shown = ask(&env, account, alice()).await.expect("a number");
    assert_eq!(shown.state, VerifyState::Default);
    set_verified(&env, account, alice(), false, shown.key);
    is_sent(
        &env,
        ca,
        one.id,
        "the paused message sent after the Confirm",
    )
    .await;
}

/// The whole answer: the number and whether sends to the person wait (BQ-3).
async fn ask_waits(env: &Env, account: AccountId, user: UserId) -> (SafetyAnswer, bool) {
    match env
        .handle
        .query(Query::SafetyNumber { account, user })
        .await
        .expect("the query answers")
    {
        QueryData::SafetyNumber { answer, waits } => (answer, waits),
        _ => panic!("the safety-number query answered another kind"),
    }
}

/// The person's page as the core answers it (P5a: what it says of safety numbers).
async fn person(env: &Env, account: AccountId, user: UserId) -> kourier_core::facade::PersonDetail {
    match env
        .handle
        .query(Query::Person { account, user })
        .await
        .expect("the query answers")
    {
        QueryData::Person(Some(d)) => *d,
        _ => panic!("the person query answered nothing"),
    }
}

/// Waits for the core's `SafetyWaitChanged` and returns whom it names.
async fn wait_changed(env: &mut Env) -> UserId {
    env.event("SafetyWaitChanged", |e| match e {
        UiEvent::SafetyWaitChanged { user, .. } => Some(user.clone()),
        _ => None,
    })
    .await
}

/// The lead's BQ-3 / critic F2: the core publishes whom a paused queue waits for. A DEFAULT
/// peer's send fails `UntrustedIdentity` and pauses: `SafetyWaitChanged` names her, the
/// safety-number answer says `waits`, and the paused head AND the row behind it (every own row
/// that still waits, U4) name her in their actions (`safety_number`: the menu's "Show Alice's
/// Safety Number"). Her Confirm of the shown
/// key ends the wait: `SafetyWaitChanged` again, `waits` false, the head goes and no row names
/// her any more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paused_queue_names_the_person_until_the_confirm() {
    let s = script("safety-paused-named")
        .with_safety(alice(), vec![(key(1), VerifyState::Default)])
        .fail_transmit(TransmitRule::attempts(untrusted(alice()), 1).in_conv(d_alice()));
    let (mut env, account, _uuid) = linked(s).await;
    let ca = conv(&env, &d_alice()).await;
    let (_, waits) = ask_waits(&env, account, alice()).await;
    assert!(!waits, "nothing waits before the send");
    send(&env, ca, "one");
    let one = queued(&env, ca, 1).await;
    send(&env, ca, "two");
    let two = queued(&env, ca, 2).await;
    assert_eq!(paused_on(&mut env).await, alice());
    assert_eq!(wait_changed(&mut env).await, alice(), "the wait is told");
    let (shown, waits) = ask_waits(&env, account, alice()).await;
    let shown = shown.expect("a number");
    assert_eq!(shown.state, VerifyState::Default, "never verified");
    assert!(waits, "the answer says that sends wait");
    let (_, other) = ask_waits(&env, account, bob()).await;
    assert!(!other, "nothing waits for bob");
    // The person's page says the same (U7: its line under "View Safety Number").
    assert!(
        person(&env, account, alice()).await.safety_waits,
        "her page says that sends wait"
    );
    assert!(
        !person(&env, account, bob()).await.safety_waits,
        "bob's page does not"
    );
    let rows = env.timeline(ca).await;
    let named = |id: MessageId| {
        rows.iter()
            .find(|r| r.id == id)
            .and_then(|r| r.actions.safety_number.clone())
    };
    assert_eq!(named(one.id), Some(alice()), "the paused head names her");
    assert_eq!(
        named(two.id),
        Some(alice()),
        "so does a row behind the head (U4)"
    );
    assert!(
        rows.iter()
            .filter(|r| r.id != one.id && r.id != two.id)
            .all(|r| r.actions.safety_number.is_none()),
        "no other row does"
    );
    confirm_key(&env, account, alice(), shown.key);
    assert_eq!(wait_changed(&mut env).await, alice(), "the end is told");
    is_sent(&env, ca, one.id, "the head went after the confirm").await;
    let (_, waits) = ask_waits(&env, account, alice()).await;
    assert!(!waits, "nothing waits after the confirm");
    assert!(
        !person(&env, account, alice()).await.safety_waits,
        "her page says nothing waits"
    );
    assert!(
        env.timeline(ca)
            .await
            .iter()
            .all(|r| r.actions.safety_number.is_none()),
        "no row names her any more"
    );
}

/// Waits until the own message `id` shows a failed send.
async fn failed_for_good(env: &Env, conv: ConversationId, id: MessageId) {
    let db = env.core_db();
    eventually("the send failed for good", WAIT, || {
        let db = db.clone();
        async move {
            own_messages(&db, conv)
                .iter()
                .any(|m| m.id == id && matches!(m.out_state, Some(OutgoingState::Failed { .. })))
        }
    })
    .await;
}

/// V5 (the negative side of U4): while the queue is paused only the rows that still WAIT name
/// the person. An own text that was delivered BEFORE the pause (here one from another device)
/// and a queued row that failed for good (`waits` false: kept for "Send Again", not part of
/// the wait) carry no `safety_number`, while the head and the row behind it do. Kills a rule
/// that names every own row of the paused chat.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rows_outside_the_wait_are_not_named_while_the_queue_is_paused() {
    let at = T0 - 30_000;
    let before = BackendEvent::Message(own(&d_alice(), &at.to_string(), at, "sent before"));
    let s = script("safety-paused-outside")
        .deliver(vec![before])
        .with_safety(alice(), vec![(key(1), VerifyState::Default)])
        // The first send fails for good (the peer is not registered); the next one meets the
        // changed key and pauses the queue.
        .fail_transmit(
            TransmitRule::attempts(unregistered(), 1)
                .of_kind("Message")
                .in_conv(d_alice()),
        )
        .fail_transmit(TransmitRule::attempts(untrusted(alice()), 1).in_conv(d_alice()));
    let (mut env, _account, _uuid) = linked(s).await;
    let ca = conv(&env, &d_alice()).await;
    let delivered = queued(&env, ca, 1).await;
    send(&env, ca, "bad");
    let bad = queued(&env, ca, 2).await;
    failed_for_good(&env, ca, bad.id).await;
    send(&env, ca, "one");
    let one = queued(&env, ca, 3).await;
    send(&env, ca, "two");
    let two = queued(&env, ca, 4).await;
    assert_eq!(paused_on(&mut env).await, alice());
    assert_eq!(wait_changed(&mut env).await, alice(), "the wait is told");
    let rows = env.timeline(ca).await;
    let named = |id: MessageId| {
        rows.iter()
            .find(|r| r.id == id)
            .unwrap_or_else(|| panic!("row {id:?} is in the timeline"))
            .actions
            .safety_number
            .clone()
    };
    assert_eq!(named(one.id), Some(alice()), "the paused head");
    assert_eq!(named(two.id), Some(alice()), "the row behind it");
    assert_eq!(
        named(delivered.id),
        None,
        "an own text delivered before the pause is no part of the wait"
    );
    assert_eq!(
        named(bad.id),
        None,
        "a row that failed for good is no part of the wait"
    );
    assert!(
        rows.iter()
            .filter(|r| r.id != one.id && r.id != two.id)
            .all(|r| r.actions.safety_number.is_none()),
        "no other row does"
    );
}

/// U10 (critic F5): a person's page says whether the account's NETWORK has safety numbers, from
/// the network's descriptor: the Signal-like fake has them, the WhatsApp-like one (a network
/// whose descriptor says `safety_numbers: false`) has none, so its people get no row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_person_page_follows_the_networks_safety_number_capability() {
    let (env, account, _) = linked(script("safety-capability-signal")).await;
    assert!(
        person(&env, account, alice()).await.safety_numbers,
        "a Signal-like network has safety numbers"
    );
    let mut other = Env::new(Flavor::WhatsAppLike, script("safety-capability-wa")).await;
    other.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut other).await;
    assert!(
        !person(&other, account, alice()).await.safety_numbers,
        "a network whose descriptor has no safety numbers offers none"
    );
}

fn confirm_key(env: &Env, account: AccountId, user: UserId, key: IdentityFingerprint) {
    env.handle
        .send(UiCommand::ConfirmIdentity { account, user, key })
        .expect("send");
}

/// The lead's RC6: "Confirm New Safety Number" with a key that is no longer the held one
/// (the number changed after the page asked) is refused by the backend, and the core does
/// NOT requeue the paused head: nothing is transmitted, the refusal names the person. The
/// new number's key then confirms (Changed -> Default) and the head goes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_confirm_leaves_the_head_paused() {
    let s = script("safety-stale-confirm")
        .with_safety(
            alice(),
            vec![
                (key(1), VerifyState::Changed),
                (key(2), VerifyState::Changed),
            ],
        )
        .fail_transmit(TransmitRule::attempts(untrusted(alice()), 1).in_conv(d_alice()));
    let (mut env, account, uuid) = linked(s).await;
    let ca = conv(&env, &d_alice()).await;
    send(&env, ca, "one");
    let one = queued(&env, ca, 1).await;
    assert_eq!(paused_on(&mut env).await, alice());
    settle().await;
    assert_eq!(env.control.transmits(uuid).len(), 1, "paused");
    let shown = ask(&env, account, alice()).await.expect("a number");
    assert!(shown.key == key(1));
    // The fake holds key(2) now: the shown key is stale.
    confirm_key(&env, account, alice(), shown.key);
    let subject = env
        .event("ConfirmIdentity refused", |e| match e {
            UiEvent::ActionFailed {
                action: "ConfirmIdentity",
                subject,
                ..
            } => Some(subject.clone()),
            _ => None,
        })
        .await;
    assert_eq!(subject, ActionSubject::Person(alice()));
    // A requeued head is retried at once (as after an accepted Mark): no clock to move (moving
    // it could reach the message's send budget).
    for _ in 0..4 {
        settle().await;
    }
    assert_eq!(
        env.control.transmits(uuid).len(),
        1,
        "a refused confirm requeues nothing: no attempt went out"
    );
    assert!(
        own_messages(&env.core_db(), ca)
            .iter()
            .any(|m| m.id == one.id && !matches!(m.out_state, Some(OutgoingState::Sent { .. }))),
        "the head is still waiting"
    );
    let again = ask(&env, account, alice()).await.expect("a number");
    assert!(again.key == key(2), "the page shows the new number");
    assert_eq!(again.state, VerifyState::Changed, "nothing was confirmed");
    confirm_key(&env, account, alice(), again.key);
    is_sent(
        &env,
        ca,
        one.id,
        "the head went after the confirm of the shown key",
    )
    .await;
    assert_eq!(
        ask(&env, account, alice()).await.expect("a number").state,
        VerifyState::Default,
        "Confirm of a Changed person is Default"
    );
}

/// Critic H1: a release that did not start at this device's page (another device's Verified
/// sync, a Storage record) comes as `BackendNotice::IdentityTrusted`; the paused chat resumes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_resumes_a_paused_chat() {
    let s = script("safety-released")
        .fail_transmit(TransmitRule::attempts(untrusted(alice()), 1).in_conv(d_alice()))
        .step(Step::Sleep(HOUR))
        .deliver(vec![BackendEvent::Notice(BackendNotice::IdentityTrusted {
            user: alice(),
        })]);
    let (mut env, _account, uuid) = linked(s).await;
    let ca = conv(&env, &d_alice()).await;
    send(&env, ca, "one");
    let one = queued(&env, ca, 1).await;
    assert_eq!(paused_on(&mut env).await, alice());
    settle().await;
    assert_eq!(env.control.transmits(uuid).len(), 1, "paused");
    // The script's hour passes: the notice arrives.
    let db = env.core_db();
    let deadline = tokio::time::Instant::now() + WAIT;
    while !own_messages(&db, ca)
        .iter()
        .any(|m| m.id == one.id && matches!(m.out_state, Some(OutgoingState::Sent { .. })))
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the paused message never went after IdentityTrusted"
        );
        through_the_hour(&env);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        !env.control.commands(uuid).contains(&"ConfirmIdentity"),
        "the backend released it; nothing was confirmed from here"
    );
}

/// Critic M4/F19: a group paused on alice's key resumes when BOB is confirmed (he does not
/// know whom the pause names); its head retries and pauses again on bob, whose key is still
/// untrusted, and a second confirm of bob sends it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_confirm_resumes_the_group() {
    let s = script("safety-group")
        .fail_transmit(TransmitRule::member(alice(), untrusted(alice()), 1).in_conv(group()))
        .fail_transmit(TransmitRule::member(bob(), untrusted(bob()), 2).in_conv(group()));
    let (mut env, account, uuid) = linked(s).await;
    let cg = conv(&env, &group()).await;
    send(&env, cg, "to the group");
    let msg = queued(&env, cg, 1).await;
    assert_eq!(
        paused_on(&mut env).await,
        alice(),
        "the first untrusted member"
    );
    settle().await;
    assert_eq!(env.control.transmits(uuid).len(), 1, "paused");
    confirm(&env, account, bob());
    assert_eq!(
        paused_on(&mut env).await,
        bob(),
        "the head retried and paused on bob"
    );
    assert_eq!(env.control.transmits(uuid).len(), 2, "one retry");
    confirm(&env, account, bob());
    is_sent(
        &env,
        cg,
        msg.id,
        "the group message after both keys were confirmed",
    )
    .await;
}

const CAROL: &str = "aci:00000000-0000-4000-8000-00000000ca01";
const GROUP_NO_BOB: &str = "gv2:b3V0Ym94LXRlc3QtZ3JvdXAy";

fn carol() -> UserId {
    uid(CAROL)
}

fn d_carol() -> ConvKey {
    ConvKey::Direct(carol())
}

/// A group of alice, carol and us: bob is NOT a member.
fn group_no_bob() -> ConvKey {
    ConvKey::Group(GROUP_NO_BOB.into())
}

async fn sent_now(env: &Env, conv: ConversationId, message: MessageId) -> bool {
    own_messages(&env.core_db(), conv)
        .iter()
        .any(|m| m.id == message && matches!(m.out_state, Some(OutgoingState::Sent { .. })))
}

/// The lead's R2 (protocol review HIGH-1): a confirm releases the paused sends of THAT person
/// only (and the paused groups that person is a member of). Three chats are paused: alice's
/// 1:1 chat, carol's 1:1 chat and a group WITHOUT bob whose pause names alice. A
/// `ConfirmIdentity`, an accepted `SetVerified` and an `IdentityTrusted` notice, all for BOB,
/// leave all three paused (no further attempt: the transmit count does not move); confirming
/// alice resumes alice's chat and the group, not carol's; confirming carol sends the last.
/// Kills a partition that resumes every paused chat (`true ||`) and a group lookup that drops
/// its member term.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_confirm_releases_only_that_persons_pauses() {
    let member = |user: UserId, role| Member { user, role };
    let setup = vec![
        conversation(d_alice(), None),
        conversation(d_carol(), None),
        conversation(
            group_no_bob(),
            Some(vec![
                member(alice(), MemberRole::Admin),
                member(carol(), MemberRole::Member),
                member(uid(ME), MemberRole::Member),
            ]),
        ),
        BackendEvent::Message(remote(&d_alice(), &alice(), "first", T0 - 60_000, "hello")),
    ];
    let marker = remote(&d_alice(), &alice(), "marker", T0 + 60_000, "marker");
    let s = Script::new("safety-only-that-person", vec![uid(ME), uid(ME_PNI)])
        .deliver(setup)
        .with_safety(bob(), vec![(key(1), VerifyState::Default)])
        .fail_transmit(TransmitRule::attempts(untrusted(alice()), 1).in_conv(d_alice()))
        .fail_transmit(TransmitRule::attempts(untrusted(carol()), 1).in_conv(d_carol()))
        .fail_transmit(TransmitRule::member(alice(), untrusted(alice()), 1).in_conv(group_no_bob()))
        .step(Step::Sleep(HOUR))
        .deliver(vec![BackendEvent::Notice(BackendNotice::IdentityTrusted {
            user: bob(),
        })])
        .deliver(vec![BackendEvent::Message(marker)]);
    let (mut env, account, uuid) = linked(s).await;
    let ca = conv(&env, &d_alice()).await;
    let cc = conv(&env, &d_carol()).await;
    let cg = conv(&env, &group_no_bob()).await;
    send(&env, ca, "to alice");
    let to_alice = queued(&env, ca, 1).await;
    assert_eq!(paused_on(&mut env).await, alice());
    send(&env, cc, "to carol");
    let to_carol = queued(&env, cc, 1).await;
    assert_eq!(paused_on(&mut env).await, carol());
    send(&env, cg, "to the group");
    let to_group = queued(&env, cg, 1).await;
    assert_eq!(
        paused_on(&mut env).await,
        alice(),
        "the group pauses on alice"
    );
    settle().await;
    assert_eq!(env.control.transmits(uuid).len(), 3, "all three paused");

    // Bob's three ways of being confirmed: none of the three pauses is his.
    confirm(&env, account, bob());
    set_verified(&env, account, bob(), true, key(1));
    let bobs = ask(&env, account, bob()).await.expect("a number");
    assert_eq!(bobs.state, VerifyState::Verified, "bob's Mark was taken");
    settle().await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        3,
        "a confirm of bob resumed a chat that is not paused on him"
    );
    // The script's hour passes: `IdentityTrusted { bob }`, then the marker message.
    let db = env.core_db();
    let deadline = tokio::time::Instant::now() + WAIT;
    while own_and_remote_count(&db, ca) < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the script's marker message never arrived"
        );
        through_the_hour(&env);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    settle().await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        3,
        "an IdentityTrusted of bob resumed a chat that is not paused on him"
    );
    for (conv, m) in [(ca, &to_alice), (cc, &to_carol), (cg, &to_group)] {
        assert!(!sent_now(&env, conv, m.id).await, "still paused");
    }

    // Alice: her chat and the group her pause names go, carol's stays.
    confirm(&env, account, alice());
    is_sent(&env, ca, to_alice.id, "alice's chat after her confirm").await;
    is_sent(&env, cg, to_group.id, "the group paused on alice").await;
    settle().await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        5,
        "two retries: alice's chat and the group"
    );
    assert!(
        !sent_now(&env, cc, to_carol.id).await,
        "alice's confirm resumed carol's chat"
    );

    confirm(&env, account, carol());
    is_sent(&env, cc, to_carol.id, "carol's chat after her confirm").await;
}

fn own_and_remote_count(db: &std::path::Path, conv: ConversationId) -> i64 {
    core_db(db, |c| {
        c.query_row(
            "SELECT count(*) FROM messages WHERE conversation_id = ?1 AND net_author != ''",
            [conv.0],
            |r| r.get(0),
        )
        .expect("count")
    })
}

fn confirm(env: &Env, account: AccountId, user: UserId) {
    env.handle
        .send(UiCommand::ConfirmIdentity {
            account,
            user,
            key: key(9),
        })
        .expect("send");
}

fn unread(env: &Env, conv: ConversationId) -> i64 {
    core_db(&env.core_db(), |c| {
        c.query_row(
            "SELECT unread FROM conversations WHERE id = ?1",
            [conv.0],
            |r| r.get(0),
        )
        .expect("unread")
    })
}

fn verified_notices(db: &std::path::Path, conv: ConversationId) -> i64 {
    core_db(db, |c| {
        c.query_row(
            "SELECT count(*) FROM messages WHERE conversation_id = ?1
               AND substr(body, 1, 29) = '{\"System\":{\"VerifiedChanged\":' AND quiet = 1",
            [conv.0],
            |r| r.get(0),
        )
        .expect("count")
    })
}

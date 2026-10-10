//! Phase G contract groundwork, account side (the core part of S15; architecture §4.5, §4.6,
//! §4.8): a challenge for one member of a group send pauses the account and the send is
//! completed after the user's answer; the facade shows the challenge and takes the answer;
//! connection, clock-skew and logged-out states reach the UI, also after a restart, and a
//! relink keeps the history; the fixture's showcase chats and account states.

mod support;

use std::time::Duration;

use kourier_backend_api::{BackendEvent, BackendNotice};
use kourier_core::facade::{
    ActionSubject, Query, QueryData, QuoteState, TimelineAnchor, TimelineQuery, UiCommand, UiEvent,
};
use kourier_fake::fixture::{
    CHALLENGE_URL, FixtureSpec, FixtureState, fixture, showcase_quotes, showcase_timer,
};
use kourier_fake::scripts::T0;
use kourier_fake::{Flavor, Script, Step, TransmitRule, challenge_error_after};
use kourier_model::{
    AccountId, AccountState, AccountUuid, AttachmentKind, Body, ConnState, ConvKey, ConversationId,
    MessageId, OutgoingState, RecipientState, SystemEvent, Text, UserAction,
};
use support::send::*;
use support::{Env, WAIT, add_account, add_linked, eventually};

const URL: &str = "https://challenge.invalid/solve";

async fn linked(script: Script) -> (Env, AccountId, AccountUuid) {
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

async fn sent(env: &Env, conv: ConversationId, message: MessageId) {
    let db = env.core_db();
    eventually("the message is sent", WAIT, || {
        let db = db.clone();
        async move {
            own_messages(&db, conv)
                .iter()
                .any(|m| m.id == message && matches!(m.out_state, Some(OutgoingState::Sent { .. })))
        }
    })
    .await;
}

/// Gives the core time to do something it must not do.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(150)).await;
}

fn submit(env: &Env, account: AccountId, answer: &str) {
    env.handle
        .send(UiCommand::SubmitChallenge {
            account,
            answer: answer.to_owned().into(),
        })
        .expect("send");
}

/// The URL of the next "solve a challenge" state of the account.
async fn challenge_shown(env: &mut Env) -> String {
    env.event("SolveChallenge", |e| match e {
        UiEvent::ConnState {
            state: ConnState::NeedsUserAction(UserAction::SolveChallenge { url }),
            ..
        } => Some(url.clone()),
        _ => None,
    })
    .await
}

async fn online_again(env: &mut Env) {
    env.event("Online again", |e| match e {
        UiEvent::ConnState {
            state: ConnState::Online { .. },
            ..
        } => Some(()),
        _ => None,
    })
    .await;
}

async fn failed(env: &mut Env, action: &'static str) -> (String, ActionSubject) {
    env.event(action, |e| match e {
        UiEvent::ActionFailed {
            action: a,
            error,
            subject,
            ..
        } if *a == action => Some((error.to_string(), subject.clone())),
        _ => None,
    })
    .await
}

/// Item 8a. One member of a group send is refused with a challenge: the others have the
/// message, the account's outbox pauses with the challenge shown, a wrong answer changes
/// nothing, and after the right one the send goes to the member that was left out, and only to
/// them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_challenge_for_one_member_pauses_and_is_completed_after_the_answer() {
    let s = script("partial-challenge")
        .fail_transmit(TransmitRule::member_challenge(bob(), URL))
        .accept_challenge_answer("solved-it");
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let (g, ca) = (conv(&env, &group()).await, conv(&env, &d_alice()).await);
    let db = env.core_db();

    send(&env, g, "to the group");
    let one = queued(&env, g, 1).await;
    assert_eq!(challenge_shown(&mut env).await, URL);
    let shown = env.account(account).await.expect("account").conn;
    assert_eq!(
        shown,
        ConnState::NeedsUserAction(UserAction::SolveChallenge { url: URL.into() }),
        "the account row has what the banner needs"
    );
    // Alice has it; Bob waits with the row, which stays the head for him alone.
    let states = send_state(&db, one.id);
    let json = |s: RecipientState| serde_json::to_string(&s).expect("json");
    assert_eq!(
        states,
        [
            (
                BOB.to_owned(),
                json(RecipientState::Failed { retryable: true })
            ),
            (
                ALICE.to_owned(),
                json(RecipientState::Sent { sealed: true })
            ),
        ]
    );
    assert_eq!(outbox_len(&db), 1);
    assert!(
        !matches!(
            own_messages(&db, g)[0].out_state,
            Some(OutgoingState::Failed { .. } | OutgoingState::PartiallyFailed { .. })
        ),
        "not a failure: it waits"
    );

    // The whole account waits: nothing goes out in another chat either.
    send(&env, ca, "to alice");
    let to_alice = queued(&env, ca, 1).await;
    settle().await;
    assert_eq!(env.control.transmits(uuid).len(), 1);

    // A wrong answer is refused and the challenge stands.
    submit(&env, account, "wrong");
    let (error, _) = failed(&mut env, "SubmitChallenge").await;
    assert!(error.contains("rejected"), "{error}");
    settle().await;
    assert_eq!(env.control.transmits(uuid).len(), 1);
    assert!(matches!(
        env.account(account).await.expect("account").conn,
        ConnState::NeedsUserAction(UserAction::SolveChallenge { .. })
    ));

    // The right one: the pause is lifted and Bob gets the message, as a follow-up send.
    submit(&env, account, "solved-it");
    online_again(&mut env).await;
    sent(&env, g, one.id).await;
    sent(&env, ca, to_alice.id).await;
    let t = env.control.transmits(uuid);
    assert_eq!(t.len(), 3);
    let again: Vec<_> = t.iter().filter(|r| r.message == one.id).collect();
    assert_eq!(again.len(), 2);
    assert_eq!(*again[1].req.local, *one.local, "the same message id");
    assert_eq!(
        again[1].req.only,
        Some(vec![bob()]),
        "only the member left out"
    );
    assert!(again[1].req.is_recipient_update);
    assert_eq!(outbox_len(&db), 0);
    assert!(
        env.account(account)
            .await
            .expect("account")
            .conn
            .is_online()
    );
    assert_eq!(
        env.control
            .commands(uuid)
            .iter()
            .filter(|c| **c == "SubmitChallenge")
            .count(),
        2
    );
}

/// Item 8b. A challenge for a whole send: the facade shows it with its URL, takes the answer
/// (`SubmitChallenge`) and hands it to the backend with the token of the refused attempt (the
/// fake refuses an answer without it); an empty answer never reaches the backend. A queued
/// edit waits with the account and goes out afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_challenge_is_shown_and_answered_through_the_facade() {
    let s = script("challenge").fail_transmit(TransmitRule::challenge(URL));
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let c = conv(&env, &d_alice()).await;
    send(&env, c, "one");
    let one = queued(&env, c, 1).await;
    assert_eq!(challenge_shown(&mut env).await, URL);

    submit(&env, account, "  ");
    let (error, subject) = failed(&mut env, "SubmitChallenge").await;
    assert!(error.contains("empty"), "{error}");
    assert_eq!(subject, ActionSubject::None);
    assert!(!env.control.commands(uuid).contains(&"SubmitChallenge"));

    submit(&env, account, "signalcaptcha://any-token");
    online_again(&mut env).await;
    sent(&env, c, one.id).await;
    let t = env.control.transmits(uuid);
    assert_eq!(t.len(), 2, "refused once, then sent");
    assert!(t.iter().all(|r| *r.req.local == *one.local));
    // Nothing is left of the challenge: the next message goes straight out.
    send(&env, c, "two");
    let two = queued(&env, c, 2).await;
    sent(&env, c, two.id).await;
    assert_eq!(env.control.transmits(uuid).len(), 3);
}

/// Item 8a for a change: an edit that one member is challenged for waits like a message and
/// reaches that member after the answer; it is neither rolled back nor reported.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_challenged_edit_waits_for_the_answer() {
    let s = script("challenged-edit")
        .fail_transmit(TransmitRule::member_challenge(bob(), URL).of_kind("Edit"));
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let g = conv(&env, &group()).await;
    send(&env, g, "first");
    let one = queued(&env, g, 1).await;
    sent(&env, g, one.id).await;
    env.handle
        .send(UiCommand::EditMessage {
            message: one.id,
            text: Text::plain("second"),
        })
        .expect("send");
    assert_eq!(challenge_shown(&mut env).await, URL);
    settle().await;
    assert_eq!(outbox_len(&env.core_db()), 1, "the edit waits");
    submit(&env, account, "ok");
    online_again(&mut env).await;
    let db = env.core_db();
    eventually("the edit is done", WAIT, || {
        let db = db.clone();
        async move { outbox_len(&db) == 0 }
    })
    .await;
    let edits: Vec<_> = env
        .control
        .transmits(uuid)
        .into_iter()
        .filter(|t| t.req.kind.name() == "Edit")
        .collect();
    assert_eq!(edits.len(), 2);
    assert_eq!(edits[1].req.only, Some(vec![bob()]));
    let rows = env.timeline(g).await;
    let row = rows.iter().find(|r| r.id == one.id).expect("row");
    assert_eq!(row.body, Body::text("second"), "kept, not rolled back");
    assert_eq!(row.revision, 1);
}

/// Item 8a, nobody reached: the challenge of one member comes with a temporary failure of
/// the only other one, so no member has a first copy. The row waits for the answer like any
/// challenged send, and the send after it is still the message's first one, to both members,
/// not a follow-up to an earlier delivery (a backend tells our other devices about a follow-up
/// in a way they drop when they never saw the first copy).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_challenge_before_anyone_has_the_message_retries_it_as_a_first_send() {
    let s = script("nobody-yet")
        .fail_transmit(TransmitRule::member_challenge(bob(), URL))
        .fail_transmit(TransmitRule::member(alice(), transient(), 1).in_conv(group()));
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let g = conv(&env, &group()).await;
    send(&env, g, "to the group");
    let one = queued(&env, g, 1).await;
    assert_eq!(challenge_shown(&mut env).await, URL);
    settle().await;
    assert_eq!(env.control.transmits(uuid).len(), 1, "it waits");
    assert_eq!(outbox_len(&env.core_db()), 1);

    submit(&env, account, "ok");
    online_again(&mut env).await;
    sent(&env, g, one.id).await;
    let all = env.control.transmits(uuid);
    assert_eq!(all.len(), 2);
    assert!(!all[0].req.is_recipient_update);
    assert!(
        !all[1].req.is_recipient_update,
        "nobody had a first copy: {:?}",
        all[1].req
    );
    let mut to = all[1].req.only.clone().unwrap_or_default();
    to.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let mut both = vec![alice(), bob()];
    both.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    assert!(to.is_empty() || to == both, "to both members: {to:?}");
    assert_eq!(outbox_len(&env.core_db()), 0);
    env.stop().await;
}

/// Item 8c. Offline (with its retry time), connecting, a wrong clock and logged out reach the
/// UI as events and in the account row; nothing is sent while the clock is off; a logged-out
/// account still says so after a restart, and linking it again keeps its history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_clock_and_logged_out_states_reach_the_ui() {
    let hold = Step::Sleep(Duration::from_secs(10));
    let retry_at = u64::try_from(T0).expect("time") + 30_000;
    let states = [
        ConnState::Offline {
            retry_at_ms: Some(retry_at),
        },
        ConnState::Connecting,
        ConnState::NeedsUserAction(UserAction::FixClock { skew_ms: 7_200_000 }),
    ];
    let mut s = script("states").step(Step::WaitAcked);
    for state in &states {
        s = s.step(Step::Conn(state.clone())).step(hold.clone());
    }
    let s = s.step(Step::Unlink {
        reason: "unlinked on the phone".into(),
    });
    let mut env = Env::new(Flavor::SignalLike, s).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_account(&mut env).await;
    let uuid = env.account(account).await.expect("row").uuid;
    let c = conv(&env, &d_alice()).await;

    // `ConnState` is a watch value: each state is held on the clock until the test saw it.
    for expected in &states {
        env.event(expected.name(), |e| match e {
            UiEvent::ConnState { account: a, state } if *a == account && state == expected => {
                Some(())
            }
            _ => None,
        })
        .await;
        assert_eq!(
            &env.account(account).await.expect("account").conn,
            expected,
            "the account row shows it too"
        );
        if matches!(expected, ConnState::NeedsUserAction(_)) {
            // With the clock off the account is not online: a message waits.
            send(&env, c, "held back");
            queued(&env, c, 1).await;
            settle().await;
            assert!(env.control.transmits(uuid).is_empty());
        }
        let clock = env.clock.clone();
        eventually("the held state", WAIT, || {
            let c = clock.clone();
            async move { c.sleepers() > 0 }
        })
        .await;
        env.clock.advance(Duration::from_secs(10));
    }

    // Unlinked: logged out, with the reason, and the history stays.
    let logged_out = ConnState::LoggedOut {
        reason: "unlinked on the phone".into(),
    };
    let h = env.handle.clone();
    eventually("logged out", WAIT, || {
        let h = h.clone();
        async move {
            matches!(h.query(Query::Accounts).await, Ok(QueryData::Accounts(a))
                if a.iter().any(|r| matches!(r.state, AccountState::LoggedOut { .. })))
        }
    })
    .await;
    let row = env.account(account).await.expect("account");
    assert_eq!(row.conn, logged_out);
    let before = env.timeline(c).await.len();
    assert_eq!(before, 2, "alice's message and ours");
    let flags = env.timeline(c).await[0].actions;
    assert!(!flags.reply && !flags.react && flags.delete_for_me && flags.copy);

    // After a restart no actor runs, and the row still says why.
    env.restart().await;
    let row = env.account(account).await.expect("account");
    assert_eq!(row.conn, logged_out, "also without a running backend");
    assert!(matches!(row.state, AccountState::LoggedOut { .. }));

    // "Link again" continues the same account: same id, same conversation, same messages, and
    // the message that waited goes out. The fake does not play the account's script again
    // (which would end in the unlink once more): no other script has to be pushed for it.
    env.handle
        .send(UiCommand::Relink {
            account,
            flow: "qr".into(),
        })
        .expect("send");
    let h = env.handle.clone();
    eventually("active again", WAIT, || {
        let h = h.clone();
        async move {
            matches!(h.query(Query::Accounts).await, Ok(QueryData::Accounts(a))
                if a.iter().any(|r| r.id == account && r.state == AccountState::Active
                    && r.conn.is_online()))
        }
    })
    .await;
    assert_eq!(env.accounts().await.len(), 1, "the same account");
    assert_eq!(conv_id(&env.core_db(), &d_alice()), Some(c));
    assert_eq!(env.timeline(c).await.len(), before);
    let held = own_messages(&env.core_db(), c)[0].clone();
    sent(&env, c, held.id).await;
    env.stop().await;
}

async fn page_rows(env: &Env, conv: ConversationId) -> Vec<kourier_core::facade::MessageRow> {
    let q = TimelineQuery {
        conv,
        anchor: TimelineAnchor::Newest,
        limit: 500,
    };
    match env
        .handle
        .query(Query::Timeline(q))
        .await
        .expect("timeline")
    {
        QueryData::Timeline(p) => p.rows,
        other => panic!("unexpected {other:?}"),
    }
}

fn text_of(body: &Body) -> &str {
    match body {
        Body::Text(t) => &t.body,
        Body::Media { caption, .. } => caption.as_ref().map_or("", |t| t.body.as_str()),
        _ => "",
    }
}

/// Item 9. The fixture's showcase chats through the real core: quotes of a known, a missing
/// and a deleted message and of a photo, a photo whose caption was edited, and a chat with a
/// timer, its notices and messages that carry it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fixture_showcase_has_quotes_an_edited_caption_and_a_timer_chat() {
    let spec = FixtureSpec {
        contacts: 2,
        groups: 1,
        messages_per_conversation: 6,
        showcase: true,
        ..FixtureSpec::default()
    };
    let (env, account, _) = linked(fixture(&spec).expect("fixture")).await;
    env.wait_finished(account).await;

    // ---- quotes ----
    let c = conv(&env, &showcase_quotes().expect("key")).await;
    let rows = page_rows(&env, c).await;
    let by_text = |needle: &str| {
        rows.iter()
            .find(|r| text_of(&r.body).contains(needle))
            .unwrap_or_else(|| panic!("no row with {needle:?}"))
    };
    let known = by_text("ten at the usual place")
        .reply
        .clone()
        .expect("quote");
    assert_eq!(known.state, QuoteState::Known);
    assert_eq!(known.target, Some(by_text("still on for Saturday").id));
    assert_eq!(
        known.author_name.as_ref().map(|n| n.expose().as_str()),
        Some("Pim Quill")
    );
    assert!(!known.own);

    let photo = by_text("before the fog came in");
    assert_eq!(photo.revision, 1, "its caption was edited");
    assert!(photo.edited_at.is_some());
    let Body::Media { attachments, .. } = &photo.body else {
        panic!("the photo lost its attachment: {:?}", photo.body);
    };
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].kind, AttachmentKind::Image);
    let of_photo = by_text("Lovely light").reply.clone().expect("quote");
    assert_eq!(of_photo.state, QuoteState::Known);
    assert_eq!(of_photo.target, Some(photo.id));
    assert_eq!(of_photo.attachment, Some(AttachmentKind::Image));
    assert_eq!(of_photo.thumb, attachments[0].id, "a thumbnail handle");

    let missing = by_text("Found them").reply.clone().expect("quote");
    assert_eq!(missing.state, QuoteState::Missing);
    assert_eq!(missing.target, None);
    assert!(missing.own);
    assert_eq!(
        missing.text.as_ref().map(|t| t.body.as_str()),
        Some("Can you bring the tent poles?")
    );

    let deleted = by_text("No worries").reply.clone().expect("quote");
    assert_eq!(deleted.state, QuoteState::Deleted);
    let tombstone = rows
        .iter()
        .find(|r| r.deleted.is_some())
        .expect("the tombstone");
    assert_eq!(
        deleted.target,
        Some(tombstone.id),
        "the tombstone can be jumped to"
    );

    // ---- the timer chat ----
    let key = showcase_timer().expect("key");
    let t = conv(&env, &key).await;
    let detail = match env
        .handle
        .query(Query::Conversation(t))
        .await
        .expect("detail")
    {
        QueryData::Conversation(Some(d)) => d,
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(detail.timer, Some(Duration::from_secs(86_400)));
    let rows = page_rows(&env, t).await;
    let notices: Vec<Option<Duration>> = rows
        .iter()
        .filter_map(|r| match &r.body {
            Body::System(SystemEvent::TimerChanged { by: Some(_), timer }) => Some(*timer),
            _ => None,
        })
        .collect();
    assert_eq!(
        notices,
        [
            Some(Duration::from_secs(604_800)),
            Some(Duration::from_secs(86_400))
        ]
    );
    let timed: Vec<_> = rows.iter().filter(|r| r.expiry.is_some()).collect();
    assert_eq!(timed.len(), 2);
    assert!(
        timed
            .iter()
            .any(|r| r.expiry.is_some_and(|e| e.started_at.is_some()))
            && timed
                .iter()
                .any(|r| r.expiry.is_some_and(|e| e.started_at.is_none())),
        "one read and counting, one unread and waiting"
    );
    // The list describes the newest row: a timer notice.
    let row = env
        .conversations()
        .await
        .into_iter()
        .find(|r| r.id == t)
        .expect("row");
    assert!(matches!(
        row.preview.and_then(|p| p.system),
        Some(SystemEvent::TimerChanged { timer: Some(_), .. })
    ));
    assert_eq!(row.unread, 1);
}

/// Item 9. The fixture can end in each account state the banners show; its challenge takes any
/// answer and the account goes online.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fixture_ends_in_the_asked_account_state() {
    let spec = |state| FixtureSpec {
        contacts: 1,
        groups: 0,
        messages_per_conversation: 2,
        state,
        ..FixtureSpec::default()
    };
    let conn_of = |env: &Env, account: AccountId| {
        let h = env.handle.clone();
        async move {
            match h.query(Query::Accounts).await.expect("accounts") {
                QueryData::Accounts(a) => a.into_iter().find(|r| r.id == account).map(|r| r.conn),
                other => panic!("unexpected {other:?}"),
            }
        }
    };
    let cases = [
        (FixtureState::Online, "Online"),
        (FixtureState::Offline, "Offline"),
        (FixtureState::Connecting, "Connecting"),
        (FixtureState::Syncing, "Syncing"),
        (FixtureState::ClockSkew, "NeedsUserAction"),
        (FixtureState::Challenge, "NeedsUserAction"),
        (FixtureState::LoggedOut, "LoggedOut"),
    ];
    for (state, name) in cases {
        let mut env = Env::new(Flavor::SignalLike, fixture(&spec(state)).expect("fixture")).await;
        env.handle.send(UiCommand::UiReady).expect("send");
        let account = add_account(&mut env).await;
        // The fixture counts as delivered in every state (the app waits for exactly this).
        tokio::time::timeout(WAIT, env.control.wait_for(|c| !c.finished().is_empty()))
            .await
            .unwrap_or_else(|_| panic!("{state:?}: the fixture never finished"));
        eventually(name, WAIT, || {
            let conn = conn_of(&env, account);
            async move { conn.await.is_some_and(|c| c.name() == name) }
        })
        .await;
        assert_eq!(
            env.conversations().await.len(),
            1,
            "{state:?}: the data is there"
        );
        match state {
            FixtureState::Challenge => {
                assert_eq!(
                    conn_of(&env, account).await,
                    Some(ConnState::NeedsUserAction(UserAction::SolveChallenge {
                        url: CHALLENGE_URL.into()
                    }))
                );
                submit(&env, account, "anything");
                eventually("online after the answer", WAIT, || {
                    let conn = conn_of(&env, account);
                    async move { conn.await.is_some_and(|c| c.is_online()) }
                })
                .await;
            }
            FixtureState::ClockSkew => assert!(matches!(
                conn_of(&env, account).await,
                Some(ConnState::NeedsUserAction(UserAction::FixClock { skew_ms })) if skew_ms > 0
            )),
            FixtureState::LoggedOut => {
                // The backend publishes "logged out" before it exits, and the core records
                // the logout once it has exited: the row follows the connection state by a
                // moment (asserting it at once failed about one run in forty).
                eventually("the logout is recorded", WAIT, || {
                    let row = env.account(account);
                    async move {
                        row.await
                            .is_some_and(|r| matches!(r.state, AccountState::LoggedOut { .. }))
                    }
                })
                .await;
            }
            _ => {}
        }
        env.stop().await;
    }
}

/// How many times the fake server was asked to take a message of `conv`.
fn attempts(env: &Env, uuid: AccountUuid, key: &ConvKey) -> usize {
    env.control
        .transmits(uuid)
        .iter()
        .filter(|t| t.req.conv == *key)
        .count()
}

async fn shows_challenge(env: &Env, account: AccountId) -> bool {
    matches!(
        env.account(account).await.expect("account").conn,
        ConnState::NeedsUserAction(UserAction::SolveChallenge { .. })
    )
}

/// `ErrorKind::Challenge.retry_after` (a 428 with a Retry-After): the pause ends by itself at
/// that time, as Desktop's `scheduleRetry` restarts its queues. Not a moment earlier; and when
/// the server still wants its challenge then, the account pauses again and waits again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_challenge_with_a_retry_time_ends_by_itself() {
    let wait = Duration::from_secs(90);
    let script = script("challenge-retry-time")
        .fail_transmit(TransmitRule::attempts(challenge_error_after(URL, wait), 2));
    let (mut env, account, uuid) = linked(script).await;
    env.wait_finished(account).await;
    let a = conv(&env, &d_alice()).await;
    send(&env, a, "held for ninety seconds, twice");
    assert_eq!(challenge_shown(&mut env).await, URL);
    let held = queued(&env, a, 1).await;
    assert!(shows_challenge(&env, account).await);
    assert_eq!(attempts(&env, uuid, &d_alice()), 1);

    // One second short of the server's time: nothing is tried.
    env.clock.advance(wait - Duration::from_secs(1));
    settle().await;
    assert_eq!(attempts(&env, uuid, &d_alice()), 1, "not before the time");
    assert!(shows_challenge(&env, account).await);

    // At the time the pause is lifted and the send is tried once: the server still wants its
    // challenge, so the account pauses anew, with the challenge on show again.
    env.clock.advance(Duration::from_secs(1));
    online_again(&mut env).await;
    assert_eq!(challenge_shown(&mut env).await, URL);
    assert_eq!(attempts(&env, uuid, &d_alice()), 2, "exactly one try");
    env.clock.advance(wait - Duration::from_secs(1));
    settle().await;
    assert_eq!(
        attempts(&env, uuid, &d_alice()),
        2,
        "the new wait counts from the new answer"
    );

    // The second time the server takes it.
    env.clock.advance(Duration::from_secs(1));
    online_again(&mut env).await;
    sent(&env, a, held.id).await;
    let tries = env.control.transmits(uuid);
    assert_eq!(tries.len(), 3);
    assert!(
        tries.iter().all(|t| t.req.local.as_ref() == held.local),
        "every try carries the id from the enqueue"
    );
    assert!(
        env.account(account)
            .await
            .expect("account")
            .conn
            .is_online()
    );
    env.stop().await;
}

/// A retry time does not take the answer away: the user's accepted answer lifts the pause at
/// once, and the time that was armed changes nothing when it comes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_lifts_a_timed_challenge_at_once() {
    let wait = Duration::from_secs(3600);
    let script = script("challenge-answer-first").fail_transmit(TransmitRule::attempts(
        challenge_error_after(URL, wait),
        u32::MAX,
    ));
    let (mut env, account, uuid) = linked(script).await;
    env.wait_finished(account).await;
    let a = conv(&env, &d_alice()).await;
    send(&env, a, "held until the answer");
    challenge_shown(&mut env).await;
    let held = queued(&env, a, 1).await;
    submit(&env, account, "solved");
    online_again(&mut env).await;
    sent(&env, a, held.id).await;
    assert_eq!(attempts(&env, uuid, &d_alice()), 2);
    // The hour passes: nothing is on show, nothing is sent again.
    env.clock.advance(wait);
    settle().await;
    assert!(
        env.account(account)
            .await
            .expect("account")
            .conn
            .is_online()
    );
    assert_eq!(attempts(&env, uuid, &d_alice()), 2);
    env.stop().await;
}

/// `BackendNotice::ChallengeRequired`: a challenge a backend met in a background job (a
/// receipt, a read sync) reaches the account state the UI already shows for a challenged send,
/// and is answered the same way. It holds no send. With a retry time it ends by itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_challenge_met_by_a_background_job_shows_the_same_account_state() {
    let notice = |retry_after| {
        vec![BackendEvent::Notice(BackendNotice::ChallengeRequired {
            url: URL.into(),
            retry_after,
        })]
    };
    let script = script("challenge-notice")
        .step(Step::WaitAcked)
        .deliver(notice(None))
        .step(Step::WaitAcked)
        .step(Step::Sleep(Duration::from_secs(10)))
        .deliver(notice(Some(Duration::from_secs(60))));
    let (mut env, account, uuid) = linked(script).await;
    assert_eq!(challenge_shown(&mut env).await, URL);
    assert!(
        shows_challenge(&env, account).await,
        "also in the account row"
    );

    // Nothing is held for it: a message goes out while the challenge shows.
    let a = conv(&env, &d_alice()).await;
    send(&env, a, "not held");
    let m = queued(&env, a, 1).await;
    sent(&env, a, m.id).await;
    assert!(
        shows_challenge(&env, account).await,
        "a send does not end it"
    );

    // The same command answers it; there is no token of a paused send to hand back.
    submit(&env, account, "solved");
    online_again(&mut env).await;
    assert!(
        env.account(account)
            .await
            .expect("account")
            .conn
            .is_online()
    );
    assert!(env.control.commands(uuid).contains(&"SubmitChallenge"));

    // The second notice names how long the server refuses: the state ends at that time.
    let clock = env.clock.clone();
    eventually("the script sleeps", WAIT, || {
        let c = clock.clone();
        async move { !c.deadlines().is_empty() }
    })
    .await;
    env.clock.advance(Duration::from_secs(10));
    assert_eq!(challenge_shown(&mut env).await, URL);
    env.clock.advance(Duration::from_secs(59));
    settle().await;
    assert!(shows_challenge(&env, account).await, "not before the time");
    env.clock.advance(Duration::from_secs(1));
    online_again(&mut env).await;
    assert!(
        env.account(account)
            .await
            .expect("account")
            .conn
            .is_online()
    );
    env.stop().await;
}

/// A script can make a relink fail (`Script::fail_relink`): the account stays logged out with
/// its history, "Link again" can be tried again, and the relink that then succeeds does not
/// play the account's script again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_relink_leaves_the_account_logged_out_and_can_be_repeated() {
    let script = script("relink-fails-once")
        .with_number("+15550100")
        .fail_relink(1)
        .step(Step::WaitAcked)
        .step(Step::Unlink {
            reason: "unlinked on the phone".into(),
        });
    // The script unlinks right after its setup: the account may be `Active` for a moment only.
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_account(&mut env).await;
    let logged_out = |env: &Env| {
        let h = env.handle.clone();
        async move {
            matches!(h.query(Query::Accounts).await, Ok(QueryData::Accounts(a))
                if a.iter().any(|r| matches!(r.state, AccountState::LoggedOut { .. })))
        }
    };
    eventually("logged out", WAIT, || logged_out(&env)).await;
    let c = conv(&env, &d_alice()).await;
    let before = env.timeline(c).await.len();
    assert_eq!(before, 1, "alice's message");
    let relink = |env: &Env| {
        env.handle
            .send(UiCommand::Relink {
                account,
                flow: "qr".into(),
            })
            .expect("send");
    };

    relink(&env);
    env.event("the failed login step", |e| match e {
        UiEvent::LoginStep { step, .. }
            if matches!(step.kind, kourier_model::LoginStepKind::Failed { .. }) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    eventually("still logged out", WAIT, || logged_out(&env)).await;
    assert_eq!(env.accounts().await.len(), 1);
    assert_eq!(env.timeline(c).await.len(), before, "the history stays");

    relink(&env);
    let h = env.handle.clone();
    eventually("active again", WAIT, || {
        let h = h.clone();
        async move {
            matches!(h.query(Query::Accounts).await, Ok(QueryData::Accounts(a))
                if a.iter().any(|r| r.id == account && r.state == AccountState::Active
                    && r.conn.is_online()))
        }
    })
    .await;
    // The script was not played again: no second copy of anything, no second unlink.
    settle().await;
    let row = env.account(account).await.expect("account");
    assert!(row.state == AccountState::Active && row.conn.is_online());
    assert_eq!(row.number.as_deref(), Some("+15550100"));
    assert_eq!(env.timeline(c).await.len(), before);
    send(&env, c, "back again");
    let m = queued(&env, c, 1).await;
    sent(&env, c, m.id).await;
    env.stop().await;
}

/// The fixture's timer chat got its timer from the peer, with a version. A timer set in the
/// app must count on from it: the fake used to answer with version 1, which the core dropped
/// as older than what it had, so the first timer change in a fixture chat did nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_own_timer_change_in_a_fixture_chat_takes_effect() {
    let spec = FixtureSpec {
        showcase: true,
        images: false,
        ..FixtureSpec::default()
    };
    let (env, account, _) = linked(fixture(&spec).expect("fixture")).await;
    env.wait_finished(account).await;
    let c = conv(&env, &showcase_timer().expect("key")).await;
    let timer = |env: &Env| {
        let h = env.handle.clone();
        async move {
            match h.query(Query::Conversation(c)).await {
                Ok(QueryData::Conversation(Some(d))) => d.timer,
                other => panic!("unexpected {other:?}"),
            }
        }
    };
    let before = timer(&env).await.expect("the scripted timer");
    let versions = |env: &Env| -> Option<i64> {
        env.peek(|db| {
            db.query_row(
                "SELECT timer_version FROM conversations WHERE id = ?1",
                [c.0],
                |r| r.get(0),
            )
            .expect("row")
        })
    };
    let scripted = versions(&env).expect("the version the script delivered");
    assert!(scripted >= 2, "{scripted}");
    let new = Duration::from_secs(3600);
    assert_ne!(before, new);
    env.handle
        .send(UiCommand::SetConversationTimer {
            conv: c,
            timer: Some(new),
        })
        .expect("send");
    eventually("the timer changed", WAIT, || {
        let t = timer(&env);
        async move { t.await == Some(new) }
    })
    .await;
    assert_eq!(
        versions(&env),
        Some(scripted + 1),
        "counted on from the scripted version"
    );
    let rows = env.timeline(c).await;
    assert!(
        rows.iter().any(|r| matches!(
            &r.body,
            Body::System(SystemEvent::TimerChanged { by: None, timer: Some(t) }) if *t == new
        )),
        "our change has its timeline row"
    );
}

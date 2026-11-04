//! Contract conformance (§10.2): every scripted stream passes `Envelope::validate` and the
//! own-id rules; the checks reject what they must; the actor ships durable events only through
//! `core_outbox` with an ack, rotates QR codes on the clock and resumes its script after a
//! restart without losing an unacked event.

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, BackendEvent, Command, ContextParams, ContractViolation, DbKey, Envelope,
    FakeClock, IncomingMessage, MAX_HISTORY_BATCH, Network, StartMode,
};
use kourier_fake::conformance::{Violation, check_envelope};
use kourier_fake::fixture::{FixtureSpec, fixture};
use kourier_fake::{FakeNetwork, Flavor, LoginScript, Script, Step, scripts};
use kourier_model::{
    AccountId, AccountUuid, Body, ConvKey, ConversationChange, LoginStepKind, Origin, UserId,
};
use tokio_util::sync::CancellationToken;

#[test]
fn every_scripted_stream_conforms() {
    let mut all = scripts::all().expect("scripts");
    all.push((
        Flavor::SignalLike,
        fixture(&FixtureSpec::default()).expect("fixture"),
    ));
    all.push((
        Flavor::SignalLike,
        fixture(&FixtureSpec {
            showcase: true,
            ..FixtureSpec::default()
        })
        .expect("fixture"),
    ));
    all.push((
        Flavor::SignalLike,
        scripts::delete_for_me().expect("script"),
    ));
    all.push((
        Flavor::SignalLike,
        kourier_fake::calls::script().expect("script"),
    ));
    for (_, script) in all {
        let envs = script.envelopes();
        assert!(!envs.is_empty(), "{}", script.name);
        for env in envs {
            check_envelope(&env, &script.own_ids)
                .unwrap_or_else(|v| panic!("{}: {v}", script.name));
        }
    }
}

#[test]
fn fixture_is_deterministic_and_synthetic() {
    let a = fixture(&FixtureSpec::default()).expect("fixture");
    let b = fixture(&FixtureSpec::default()).expect("fixture");
    assert_eq!(a, b, "same seed, same data");
    let c = fixture(&FixtureSpec {
        seed: 2,
        ..Default::default()
    })
    .expect("fixture");
    assert_ne!(a, c);
    let messages: usize = a
        .deliveries()
        .iter()
        .flatten()
        .filter(|e| matches!(e, BackendEvent::Message(_)))
        .count();
    let spec = FixtureSpec::default();
    // (A few chats end in a quiet notice: `the_fixture_ends_a_direct_chat_and_a_group_in_a_quiet_notice`.)
    assert_eq!(
        messages,
        (spec.contacts + spec.groups) * spec.messages_per_conversation
            + kourier_fake::fixture::quiet_notice_chats(spec.contacts, spec.groups).len()
    );
    assert!(a.deliveries().iter().all(|d| d.len() <= 50));
}

/// A direct chat and a group end in a safety number notice (the quiet kind the conversation
/// list leaves out): the newest row of its chat, dated after the NEXT chat's newest message
/// and before the newest message of the whole fixture. So a list that counts the notice
/// puts the chat one place higher, above that next chat, and the default list does not move
/// it; and the fixture has the same messages as before otherwise.
#[test]
fn the_fixture_ends_a_direct_chat_and_a_group_in_a_quiet_notice() {
    use kourier_fake::fixture::quiet_notice_chats;
    let spec = FixtureSpec::default();
    let script = fixture(&spec).expect("fixture");
    let messages: Vec<IncomingMessage> = script
        .deliveries()
        .into_iter()
        .flatten()
        .filter_map(|e| match e {
            BackendEvent::Message(m) => Some(m),
            _ => None,
        })
        .collect();
    let newest_of = |key: &ConvKey, quiet: bool| {
        messages
            .iter()
            .filter(|m| {
                m.conv == *key && matches!(&m.body, Body::System(e) if e.is_quiet()) == quiet
            })
            .map(|m| m.sent_at)
            .max()
    };
    let key_of = |ci: usize| -> ConvKey {
        if ci < spec.contacts {
            ConvKey::Direct(uid(&format!(
                "aci:00000000-0000-4000-8000-{:012x}",
                0x00f1_0000 + ci as u64 + 1
            )))
        } else {
            ConvKey::Group(format!("gv2:Zml4dHVyZS1ncm91cC0{:03}", ci - spec.contacts).into())
        }
    };
    let chats = quiet_notice_chats(spec.contacts, spec.groups);
    assert_eq!(chats.len(), 2, "one direct chat and one group");
    assert!(
        chats[0] < spec.contacts && chats[1] >= spec.contacts,
        "{chats:?}"
    );
    let newest_message = messages.iter().map(|m| m.sent_at).max().expect("messages");
    for ci in chats {
        assert!(ci >= 4, "neither pinned, muted nor archived");
        let (key, next) = (key_of(ci), key_of(ci + 1));
        let notice = newest_of(&key, true).expect("the chat ends in a quiet notice");
        let own_last = newest_of(&key, false).expect("the chat has messages");
        let next_last = newest_of(&next, false).expect("the next chat has messages");
        assert!(notice > next_last, "above the next chat when it counts");
        assert!(notice > own_last, "the newest row of its own chat");
        assert!(notice < newest_message, "never the newest of the fixture");
        // Only one: and no other chat is touched.
        assert_eq!(
            messages
                .iter()
                .filter(|m| m.conv == key && matches!(&m.body, Body::System(e) if e.is_quiet()))
                .count(),
            1
        );
    }
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(&m.body, Body::System(e) if e.is_quiet()))
            .count(),
        2,
        "no other chat has one"
    );
}

/// The showcase chats and the end states are extras: without them the fixture is what it was,
/// with them everything before them is unchanged.
#[test]
fn fixture_extras_only_add() {
    use kourier_fake::fixture::{FixtureState, showcase_quotes, showcase_timer};
    let plain = fixture(&FixtureSpec::default()).expect("fixture");
    let events =
        |s: &Script| -> Vec<BackendEvent> { s.deliveries().into_iter().flatten().collect() };
    let showcase = fixture(&FixtureSpec {
        showcase: true,
        ..FixtureSpec::default()
    })
    .expect("fixture");
    let (base, more) = (events(&plain), events(&showcase));
    assert!(more.len() > base.len());
    assert_eq!(
        more[..base.len()],
        base[..],
        "nothing before the showcase moved"
    );
    let keys = [
        showcase_quotes().expect("key"),
        showcase_timer().expect("key"),
    ];
    for key in &keys {
        assert!(
            !base
                .iter()
                .any(|e| matches!(e, BackendEvent::Message(m) if m.conv == *key)),
            "the default fixture has no showcase chat"
        );
        assert!(
            more.iter()
                .any(|e| matches!(e, BackendEvent::Message(m) if m.conv == *key)),
            "the showcase adds it"
        );
    }
    // The showcase is newer than everything else, so its chats lead the list.
    let newest = |events: &[BackendEvent], showcase: bool| {
        events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::Message(m) if keys.contains(&m.conv) == showcase => Some(m.sent_at),
                _ => None,
            })
            .max()
            .expect("messages")
    };
    assert!(newest(&more, true) > newest(&more, false));

    let logged_out = fixture(&FixtureSpec {
        state: FixtureState::LoggedOut,
        ..FixtureSpec::default()
    })
    .expect("fixture");
    assert_eq!(events(&logged_out), base);
    assert!(matches!(logged_out.steps.last(), Some(Step::Unlink { .. })));
    assert_eq!(logged_out.steps.len(), plain.steps.len() + 1);
}

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("id")
}

#[test]
fn conformance_rejects_contract_breaks() {
    let own = [uid("aci:00000000-0000-4000-8000-0000000000aa")];
    let conv = ConvKey::Group("gv2:AAAA".into());
    let msg =
        |origin: Origin| IncomingMessage::new(conv.clone(), "1", origin, Body::text("x"), 1, 2);
    // Durable events must come with an ack (through core_outbox).
    let env = Envelope {
        events: vec![BackendEvent::Message(msg(Origin::OwnThisDevice))],
        ack: None,
    };
    assert!(matches!(
        check_envelope(&env, &own),
        Err(Violation::Contract(ContractViolation::DurableWithoutAck(
            "Message"
        )))
    ));
    // An own message never carries an author.
    let env = Envelope::durable(vec![BackendEvent::Message(msg(Origin::Remote {
        author: own[0].clone(),
        device: None,
    }))])
    .0;
    assert!(matches!(
        check_envelope(&env, &own),
        Err(Violation::OwnMessageWithAuthor("Message"))
    ));
    // History batches are bounded.
    let big: Vec<_> = (0..=MAX_HISTORY_BATCH)
        .map(|i| {
            let mut m = msg(Origin::OwnOtherDevice { device: None });
            m.local = i.to_string().into();
            m
        })
        .collect();
    let env = Envelope::durable(vec![BackendEvent::HistoryBatch {
        conv: conv.clone(),
        messages: big,
        has_more: false,
        progress: None,
    }])
    .0;
    assert!(matches!(
        check_envelope(&env, &own),
        Err(Violation::Contract(ContractViolation::HistoryBatchTooLarge(n))) if n == MAX_HISTORY_BATCH + 1
    ));
    // Own modifiers say `by: None`.
    let env = Envelope::durable(vec![BackendEvent::Reaction {
        target: scripts::target(Some(&conv), None, "1"),
        by: Some(own[0].clone()),
        emoji: "👍".into(),
        remove: false,
        at: 3,
    }])
    .0;
    assert!(matches!(
        check_envelope(&env, &own),
        Err(Violation::OwnModifierWithAuthor("Reaction"))
    ));
}

/// The versions of an edited history message (`IncomingMessage::edits`): only on a message
/// of a history batch that is no notice, oldest first, each under its own id, at most as
/// many as the core reads. And the script that carries one says so in a history batch.
#[test]
fn conformance_holds_the_rules_of_an_edited_history_message() {
    use kourier_backend_api::{HistoryEdit, MAX_HISTORY_EDITS};
    let own = [uid("aci:00000000-0000-4000-8000-0000000000aa")];
    let conv = ConvKey::Group("gv2:AAAA".into());
    let alice = uid("aci:00000000-0000-4000-8000-0000000a11ce");
    let edit = |local: &str, at: i64| HistoryEdit {
        local: local.into(),
        at,
    };
    let edited = |edits: Vec<HistoryEdit>| {
        let mut m = scripts::remote(&conv, &alice, "100", 100, "the newest text");
        m.edits = edits;
        m
    };
    let batch = |m: IncomingMessage| {
        Envelope::durable(vec![BackendEvent::HistoryBatch {
            conv: conv.clone(),
            messages: vec![m],
            has_more: false,
            progress: None,
        }])
        .0
    };
    let good = edited(vec![edit("150", 150), edit("200", 200)]);
    assert_eq!(check_envelope(&batch(good.clone()), &own), Ok(()));
    let why = |env: &Envelope| match check_envelope(env, &own) {
        Err(Violation::HistoryEdits(why)) => why,
        other => panic!("{other:?}"),
    };
    // Live, the field is not read: an edit of a live message is `Edited`.
    let live = Envelope::durable(vec![BackendEvent::Message(good)]).0;
    assert_eq!(why(&live), "on a live message");
    assert_eq!(
        why(&batch(edited(vec![edit("200", 200), edit("150", 150)]))),
        "not oldest first"
    );
    for twice in [
        vec![edit("100", 150)],
        vec![edit("150", 150), edit("150", 200)],
    ] {
        assert_eq!(
            why(&batch(edited(twice))),
            "a version under the message's own id, or two under one id"
        );
    }
    let many = (0..=MAX_HISTORY_EDITS)
        .map(|i| edit(&format!("e{i}"), 200 + i64::try_from(i).expect("small")))
        .collect();
    assert_eq!(why(&batch(edited(many))), "more than the core reads");
    let mut notice = edited(vec![edit("150", 150)]);
    notice.body = Body::System(kourier_model::SystemEvent::TitleChanged {
        by: None,
        title: "x".into(),
    });
    assert_eq!(why(&batch(notice)), "on a notice or a call line");
    // A malformed id of a version is `Envelope::validate`'s.
    assert!(matches!(
        check_envelope(&batch(edited(vec![edit("", 150)])), &own),
        Err(Violation::Contract(ContractViolation::InvalidId { .. }))
    ));

    // The WhatsApp-like history has one edited message, in a history batch.
    let script = scripts::whatsapp_like().expect("script");
    let edited: Vec<IncomingMessage> = script
        .deliveries()
        .into_iter()
        .flatten()
        .filter_map(|e| match e {
            BackendEvent::HistoryBatch { messages, .. } => Some(messages),
            _ => None,
        })
        .flatten()
        .filter(|m| !m.edits.is_empty())
        .collect();
    assert_eq!(edited.len(), 1);
    assert_eq!(
        edited[0].edits.last().map(|e| e.local.as_ref()),
        Some(scripts::WHATSAPP_EDITED_HISTORY.1)
    );
    // Its rows are of unknown read state: all say read, and the count is the hint.
    let unknown = script
        .deliveries()
        .into_iter()
        .flatten()
        .filter_map(|e| match e {
            BackendEvent::HistoryBatch { messages, .. } => Some(messages),
            _ => None,
        })
        .flatten()
        .filter(|m| !m.origin.is_own())
        .all(|m| m.read);
    assert!(unknown, "no history row of this network says unread");
}

struct Running {
    ends: kourier_backend_api::CoreEnds,
    handle: kourier_backend_api::AccountHandle,
    /// The actor's `backend.db`, for the tests that look at what its forwarder still holds.
    backend_db: std::path::PathBuf,
}

fn spawn(
    fake: &FakeNetwork,
    dir: &std::path::Path,
    uuid: AccountUuid,
    start: StartMode,
    clock: &FakeClock,
    rt: &tokio::runtime::Runtime,
) -> Running {
    let backend_db = dir.join("backend.db");
    let (ctx, ends) = AccountContext::new(ContextParams {
        account: AccountId(1),
        uuid,
        start,
        backend_db: backend_db.clone(),
        db_key: DbKey::new([3; 32]),
        blob_dir: dir.join("blobs"),
        net_rt: rt.handle().clone(),
        cancel: CancellationToken::new(),
        clock: Arc::new(clock.clone()),
        config: Default::default(),
        ui: Default::default(),
        local_prefs: Default::default(),
    });
    let handle = fake.spawn(ctx).expect("spawn");
    Running {
        ends,
        handle,
        backend_db,
    }
}

/// A minimal core: acks every durable envelope (after "commit") and records the events.
async fn next(run: &mut Running, ack: bool) -> Envelope {
    let env = tokio::time::timeout(Duration::from_secs(10), run.ends.events.recv())
        .await
        .expect("timely")
        .expect("open");
    if !ack {
        return env;
    }
    if let Some(a) = env.ack {
        a.complete(Ok(()));
    }
    Envelope {
        events: env.events,
        ack: None,
    }
}

#[test]
fn actor_rotates_qr_codes_and_resumes_after_a_restart() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = FakeClock::new(1_000_000);
    let me = uid("aci:00000000-0000-4000-8000-0000000000aa");
    let alice = uid("aci:00000000-0000-4000-8000-0000000a11ce");
    let d = ConvKey::Direct(alice.clone());
    let mut script = Script::new("qr", vec![me.clone()])
        .deliver(vec![BackendEvent::Message(scripts::remote(
            &d, &alice, "1", 1, "one",
        ))])
        .deliver(vec![BackendEvent::Message(scripts::remote(
            &d, &alice, "2", 2, "two",
        ))]);
    script.login = LoginScript::Qr {
        ask_device_name: false,
        codes: 3,
        valid_for: Duration::from_secs(45),
        scanned_at: Some(1),
        linked_as: None,
    };
    script.steps.push(Step::WaitAcked);
    let fake = FakeNetwork::new(Flavor::SignalLike, script);
    let uuid = AccountUuid::new_random();
    rt.block_on(async {
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
        run.ends
            .commands
            .send(Command::StartLogin {
                flow: "qr".into(),
                relink: None,
                taken: Vec::new(),
            })
            .await
            .expect("send");
        let first = next(&mut run, false).await;
        assert!(first.ack.is_none(), "login steps are ephemeral");
        assert!(matches!(&first.events[0], BackendEvent::Login(s) if matches!(s.kind, LoginStepKind::ShowQr { .. })));
        while clock.sleepers() == 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        clock.advance(Duration::from_secs(45));
        let second = next(&mut run, false).await;
        assert!(matches!(&second.events[0], BackendEvent::Login(s) if matches!(s.kind, LoginStepKind::ShowQr { .. })));
        assert_ne!(first.events, second.events, "a new code");
        // Progress, then the durable AccountReady and the ephemeral Done in either order; the
        // forwarder may batch the first deliveries with AccountReady.
        let mut ready = false;
        let mut done = false;
        let mut acked: Vec<String> = Vec::new();
        let messages = |env: &Envelope| -> Vec<String> {
            env.events
                .iter()
                .filter_map(|e| match e {
                    BackendEvent::Message(m) => Some(m.local.to_string()),
                    _ => None,
                })
                .collect()
        };
        while !(ready && done) {
            let env = next(&mut run, true).await;
            acked.extend(messages(&env));
            for e in &env.events {
                match e {
                    BackendEvent::AccountReady { own_ids, .. } => {
                        assert_eq!(own_ids, &vec![me.clone()]);
                        ready = true;
                    }
                    BackendEvent::Login(s) if s.kind == LoginStepKind::Done => done = true,
                    _ => {}
                }
            }
        }
        // The next delivery is not acked; the actor stops (a crash of the core side).
        let mut unacked = Vec::new();
        if acked.len() < 2 {
            let env = next(&mut run, false).await;
            assert!(env.ack.is_some(), "durable events always carry an ack");
            unacked = messages(&env);
            assert!(!unacked.is_empty());
        }
        run.ends.commands.send(Command::Shutdown).await.expect("send");
        let exit = kourier_backend_api::Exit::from_channel(run.handle.exited.await);
        assert_eq!(exit, kourier_backend_api::Exit::Shutdown);
        if let Some(t) = run.handle.thread.take() {
            t.join().expect("join");
        }

        // Resumed: what was not acked comes again, then the rest. Delivery is at least once:
        // a row acked just before the stop may come again too (its DELETE had not run), which
        // the core's idempotent ingest absorbs.
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Resume, &clock, &rt);
        run.ends.commands.send(Command::Connect).await.expect("send");
        let control = fake.control();
        let finished = control.wait_for(|c| c.is_finished(uuid));
        tokio::pin!(finished);
        let mut again = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            tokio::select! {
                _ = &mut finished => break,
                env = run.ends.events.recv() => {
                    let env = env.expect("open");
                    again.extend(messages(&env));
                    if let Some(a) = env.ack {
                        a.complete(Ok(()));
                    }
                }
                _ = tokio::time::sleep_until(deadline) => panic!("not finished"),
            }
        }
        assert!(
            unacked.iter().all(|l| again.contains(l)),
            "unacked events are redelivered"
        );
        let mut all: Vec<String> = acked.iter().chain(again.iter()).cloned().collect();
        all.sort();
        all.dedup();
        assert_eq!(all, ["1", "2"], "nothing is lost");
        let mut order = again.clone();
        order.dedup();
        assert!(order.windows(2).all(|w| w[0] <= w[1]), "seq order: {again:?}");
        tokio::time::timeout(
            Duration::from_secs(10),
            control.wait_for(|c| c.spawns(uuid).len() == 2),
        )
        .await
        .expect("second spawn recorded");
        assert_eq!(control.spawns(uuid), [StartMode::Fresh, StartMode::Resume]);
        assert!(control.violations().is_empty());
        run.ends.commands.send(Command::Shutdown).await.expect("send");
        let _ = run.handle.exited.await;
    });
}

/// Links a fresh actor (or relinks one) and acks everything until `f` holds for an envelope;
/// returns every durable event seen on the way.
async fn link_until(
    run: &mut Running,
    relink: Option<kourier_backend_api::Relink>,
    mut f: impl FnMut(&BackendEvent) -> bool,
) -> Vec<BackendEvent> {
    run.ends
        .commands
        .send(Command::StartLogin {
            flow: "qr".into(),
            relink,
            taken: Vec::new(),
        })
        .await
        .expect("send");
    let mut seen = Vec::new();
    loop {
        let env = next(run, true).await;
        let hit = env.events.iter().any(&mut f);
        seen.extend(env.events.into_iter().filter(|e| !e.is_ephemeral()));
        if hit {
            return seen;
        }
    }
}

/// Changes the timer of `conv` through the actor and returns the version its patch states.
async fn set_timer(run: &mut Running, conv: &ConvKey, secs: u64, id: &str) -> u32 {
    let (reply, answer) = tokio::sync::oneshot::channel();
    run.ends
        .commands
        .send(Command::ChangeConversation {
            conv: conv.clone(),
            change: kourier_model::ConversationChange::SetTimer {
                timer: Some(Duration::from_secs(secs)),
            },
            id: Some(kourier_backend_api::AllocatedId {
                local: id.into(),
                sent_at: 7,
            }),
            reply,
        })
        .await
        .expect("send");
    answer.await.expect("answered").expect("accepted");
    // The patch right before this change's own timeline row (a row acked just before a
    // restart may be delivered again, also in the same envelope; that is an earlier change).
    loop {
        let env = next(run, true).await;
        let mut version = None;
        for e in &env.events {
            match e {
                BackendEvent::ConversationUpsert(p) if p.key == *conv => {
                    version = p.timer_version;
                }
                BackendEvent::Message(m) if m.local.as_ref() == id => {
                    return version.expect("the patch comes with its row");
                }
                _ => {}
            }
        }
    }
}

async fn stop(mut run: Running) {
    // The actor may have exited by itself (an unlink): a closed queue is fine.
    let _ = run.ends.commands.send(Command::Shutdown).await;
    let _ = run.handle.exited.await;
    if let Some(t) = run.handle.thread.take() {
        t.join().expect("join");
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

/// A scripted chat whose timer the network set at version 5.
fn timer_script(name: &str) -> (Script, ConvKey, UserId) {
    let me = uid("aci:00000000-0000-4000-8000-0000000000aa");
    let alice = uid("aci:00000000-0000-4000-8000-0000000a11ce");
    let d = ConvKey::Direct(alice.clone());
    let mut patch = kourier_model::ConversationPatch::new(d.clone());
    patch.features = Some(scripts::signal_features(false));
    patch.timer = kourier_model::Patch::Set(Duration::from_secs(3600));
    patch.timer_version = Some(5);
    let script = Script::new(name, vec![me.clone()])
        .deliver(vec![BackendEvent::ConversationUpsert(patch)])
        .deliver(vec![BackendEvent::Message(scripts::remote(
            &d, &alice, "1", 1, "one",
        ))])
        .step(Step::WaitAcked);
    (script, d, me)
}

/// The fake's own timer changes count on from the version its script delivered (Signal's
/// `expireTimerVersion` is one counter per chat, whoever raised it last). Counting from 1, the
/// core dropped the first timer set in the app as older than the scripted one.
#[test]
fn own_timer_changes_continue_from_the_scripted_version() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = FakeClock::new(1_000_000);
    let (script, d, _) = timer_script("timer-versions");
    let fake = FakeNetwork::new(Flavor::SignalLike, script);
    let uuid = AccountUuid::new_random();
    rt.block_on(async {
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
        link_until(&mut run, None, |e| matches!(e, BackendEvent::Message(_))).await;
        assert_eq!(set_timer(&mut run, &d, 30, "100").await, 6);
        assert_eq!(set_timer(&mut run, &d, 60, "101").await, 7);
        // The counter is durable: a restarted actor goes on from it.
        stop(run).await;
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Resume, &clock, &rt);
        run.ends
            .commands
            .send(Command::Connect)
            .await
            .expect("send");
        assert_eq!(set_timer(&mut run, &d, 90, "102").await, 8);
        // A chat no patch ever versioned starts at 1.
        let other = ConvKey::Direct(uid("aci:00000000-0000-4000-8000-000000000b0b"));
        assert_eq!(set_timer(&mut run, &other, 30, "103").await, 1);
        stop(run).await;
    });
}

/// A relink of an account that ran its script before does not play it again: the unlink wiped
/// `backend.db` (and the script's cursor with it), but what the script delivered is still in
/// the core. Only `AccountReady` comes, the script counts as finished, and timer versions go
/// on from what the script's patches stated. A script pushed for the relink is played.
#[test]
fn a_relink_does_not_replay_the_script() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = FakeClock::new(1_000_000);
    let (script, d, me) = timer_script("relinked");
    let script = script.step(Step::Unlink {
        reason: "unlinked by the phone".into(),
    });
    let fake = FakeNetwork::new(Flavor::SignalLike, script);
    let control = fake.control();
    let uuid = AccountUuid::new_random();
    let relink = || {
        Some(kourier_backend_api::Relink {
            account: AccountId(1),
            expect_own: vec![me.clone()],
        })
    };
    rt.block_on(async {
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
        let first = link_until(&mut run, None, |e| matches!(e, BackendEvent::Message(_))).await;
        assert!(first.iter().any(|e| matches!(e, BackendEvent::Message(_))));
        // The script ends with the unlink: the actor exits by itself.
        let exit = kourier_backend_api::Exit::from_channel((&mut run.handle.exited).await);
        assert!(
            matches!(exit, kourier_backend_api::Exit::LoggedOut { .. }),
            "{exit:?}"
        );
        if let Some(t) = run.handle.thread.take() {
            t.join().expect("join");
        }
        drop(run);
        // What the core does on an unlink: the backend's database is gone.
        for f in std::fs::read_dir(dir.path()).expect("dir").flatten() {
            if f.file_name().to_string_lossy().starts_with("backend.db") {
                std::fs::remove_file(f.path()).expect("wipe");
            }
        }

        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
        let again = link_until(&mut run, relink(), |e| {
            matches!(e, BackendEvent::AccountReady { .. })
        })
        .await;
        assert_eq!(again.len(), 1, "only AccountReady: {again:?}");
        tokio::time::timeout(
            Duration::from_secs(10),
            control.wait_for(|c| c.is_finished(uuid)),
        )
        .await
        .expect("the replay is skipped, so the script is finished");
        // Still linked and online: the script's last step (the unlink) did not run again, and
        // the timer goes on from the version the script's patch stated.
        assert_eq!(set_timer(&mut run, &d, 30, "200").await, 6);
        assert!(
            run.ends.events.try_recv().is_err(),
            "nothing of the script was delivered again"
        );
        stop(run).await;

        // A script pushed for a relink is a new one and is played.
        for f in std::fs::read_dir(dir.path()).expect("dir").flatten() {
            if f.file_name().to_string_lossy().starts_with("backend.db") {
                std::fs::remove_file(f.path()).expect("wipe");
            }
        }
        let alice = uid("aci:00000000-0000-4000-8000-0000000a11ce");
        control.push_script(Script::new("pushed", vec![me.clone()]).deliver(vec![
            BackendEvent::Message(scripts::remote(&d, &alice, "9", 9, "after the relink")),
        ]));
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
        let pushed = link_until(
            &mut run,
            relink(),
            |e| matches!(e, BackendEvent::Message(m) if m.local.as_ref() == "9"),
        )
        .await;
        assert!(
            pushed
                .iter()
                .any(|e| matches!(e, BackendEvent::AccountReady { .. }))
        );
        stop(run).await;
    });
}

/// An account that was unlinked in the middle of its script goes on with the rest after a
/// relink: what it delivered before is not played again (it is still in the core), what it
/// had not reached still comes. The cursor went with `backend.db`; the control remembers it.
#[test]
fn a_relink_plays_the_rest_of_an_unfinished_script() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = FakeClock::new(1_000_000);
    let me = uid("aci:00000000-0000-4000-8000-0000000000aa");
    let alice = uid("aci:00000000-0000-4000-8000-0000000a11ce");
    let d = ConvKey::Direct(alice.clone());
    let message = |local: &str, text: &str| {
        BackendEvent::Message(scripts::remote(&d, &alice, local, 5, text))
    };
    let script = Script::new("unfinished", vec![me.clone()])
        .deliver(vec![message("1", "before the unlink")])
        .step(Step::Unlink {
            reason: "unlinked by the phone".into(),
        })
        .deliver(vec![message("2", "after the relink")])
        .deliver(vec![message("3", "and one more")]);
    let fake = FakeNetwork::new(Flavor::SignalLike, script);
    let control = fake.control();
    let uuid = AccountUuid::new_random();
    let wipe = || {
        for f in std::fs::read_dir(dir.path()).expect("dir").flatten() {
            if f.file_name().to_string_lossy().starts_with("backend.db") {
                std::fs::remove_file(f.path()).expect("wipe");
            }
        }
    };
    let locals = |events: &[BackendEvent]| -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::Message(m) => Some(m.local.to_string()),
                _ => None,
            })
            .collect()
    };
    rt.block_on(async {
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
        let first = link_until(&mut run, None, |e| matches!(e, BackendEvent::Message(_))).await;
        assert_eq!(locals(&first), ["1"]);
        // The core goes on reading and acknowledging while the actor leaves: an actor that
        // exits after an unlink first hands over what its outbox still holds and waits for
        // the core's acknowledgement (up to its 30-second drain bound), so a test that only
        // waits for the exit sits out that whole bound.
        let exit = loop {
            tokio::select! {
                exited = &mut run.handle.exited => {
                    break kourier_backend_api::Exit::from_channel(exited);
                }
                env = run.ends.events.recv() => {
                    if let Some(ack) = env.and_then(|env| env.ack) {
                        ack.complete(Ok(()));
                    }
                }
            }
        };
        assert!(
            matches!(exit, kourier_backend_api::Exit::LoggedOut { .. }),
            "{exit:?}"
        );
        if let Some(t) = run.handle.thread.take() {
            t.join().expect("join");
        }
        drop(run);
        assert!(
            !control.is_finished(uuid),
            "two deliveries are still to come"
        );
        assert_eq!(control.script_progress(uuid), Some(2), "behind the unlink");
        // What the core does on an unlink: the backend's database is gone.
        wipe();

        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
        let relink = Some(kourier_backend_api::Relink {
            account: AccountId(1),
            expect_own: vec![me.clone()],
        });
        let rest = link_until(
            &mut run,
            relink,
            |e| matches!(e, BackendEvent::Message(m) if m.local.as_ref() == "3"),
        )
        .await;
        assert!(matches!(
            rest.first(),
            Some(BackendEvent::AccountReady { .. })
        ));
        assert_eq!(
            locals(&rest),
            ["2", "3"],
            "the rest of the script, and nothing of what was played"
        );
        // The roaming statement was made again with the link (in the fake's own memory); the core keeps
        // the account's stored caps, so no bare statement comes in front of the rest (A10a fix round, F8).
        assert!(
            stated_caps(&rest).is_none(),
            "the caps are not stated again in front of the rest: {rest:?}"
        );
        tokio::time::timeout(
            Duration::from_secs(10),
            control.wait_for(|c| c.is_finished(uuid)),
        )
        .await
        .expect("the script ran to its end");
        stop(run).await;

        // A script pushed for the next relink starts at its first step, whatever the old
        // one's progress was.
        wipe();
        control.push_script(
            Script::new("pushed", vec![me.clone()]).deliver(vec![message("9", "a new script")]),
        );
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
        let relink = Some(kourier_backend_api::Relink {
            account: AccountId(1),
            expect_own: vec![me.clone()],
        });
        let pushed = link_until(&mut run, relink, |e| matches!(e, BackendEvent::Message(_))).await;
        assert_eq!(locals(&pushed), ["9"]);
        stop(run).await;
    });
}

/// The fake takes the deletes for me made on this device like every durable command: one job
/// per command id with what was asked, confirmed with `CommandPersisted`, readable from the
/// control and from `backend.db`; and it names the attachments of own messages.
#[test]
fn deletes_made_here_are_durable_jobs_and_own_attachments_have_names() {
    use kourier_backend_api::{DurableCommand, Network as _};
    use kourier_model::{CmdId, MsgTarget, NetMsgId};
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = FakeClock::new(1_000_000);
    let me = uid("aci:00000000-0000-4000-8000-0000000000aa");
    let alice = uid("aci:00000000-0000-4000-8000-0000000a11ce");
    let d = ConvKey::Direct(alice.clone());
    let fake = FakeNetwork::new(Flavor::SignalLike, Script::new("jobs", vec![me]));
    let control = fake.control();
    let uuid = AccountUuid::new_random();
    assert_eq!(
        fake.attachment_net_ids("1700000000000", 2),
        [scripts::attachment_name("1700000000000", 2)]
    );
    let target = MsgTarget::to_id(Some(d.clone()), &NetMsgId::remote(alice, "7"));
    let clear = DurableCommand::ClearConversation {
        conv: d.clone(),
        newest: vec![target.clone()],
        newest_non_expiring: Vec::new(),
        remove: true,
        before_ms: None,
        system_before_ms: None,
    };
    let gone = DurableCommand::DeleteAttachment {
        conv: d,
        target,
        index: 1,
        net_ids: vec!["fake-att:7:1".to_owned()].into(),
    };
    rt.block_on(async {
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
        link_until(&mut run, None, |e| {
            matches!(e, BackendEvent::AccountReady { .. })
        })
        .await;
        // The clear is sent twice (the core resends what was not confirmed): one job.
        for (id, command) in [(4, &clear), (4, &clear), (5, &gone)] {
            run.ends
                .commands
                .send(command.clone().into_command(CmdId(id)))
                .await
                .expect("send");
        }
        let mut confirmed = Vec::new();
        while confirmed.len() < 3 {
            let env = next(&mut run, true).await;
            for e in &env.events {
                if let BackendEvent::CommandPersisted { cmd } = e {
                    confirmed.push(cmd.0);
                }
            }
        }
        assert_eq!(confirmed, [4, 4, 5], "every send is confirmed");
        assert_eq!(
            control.durable_commands(uuid),
            [clear.clone(), gone.clone()]
        );
        stop(run).await;
    });
    let jobs = kourier_fake::job_log(
        &dir.path().join("backend.db"),
        &kourier_db::DbKey::from([3u8; 32]),
    );
    // (The key is the one `spawn` gives every test actor.)
    let jobs = jobs.expect("job log");
    assert_eq!(
        jobs.iter()
            .map(|j| (j.cmd, j.kind.as_str(), j.command.clone()))
            .collect::<Vec<_>>(),
        [(4, "clear", Some(clear)), (5, "attachment", Some(gone))]
    );
}

/// A10a: the core's own durable change of a chat (the clearing of a mark when it is read) is a durable
/// job like the others: one per command id with what was asked, confirmed with `CommandPersisted`, readable
/// from the control and from `backend.db`. A fake that states no roaming flag (`roaming_writes: None`)
/// refuses the roaming switch, like a network that cannot write the settings.
#[test]
fn a_durable_conversation_change_is_a_job_and_the_roaming_switch_is_refused() {
    use kourier_backend_api::{Command, DurableCommand};
    use kourier_model::{CmdId, ConversationChange, ErrorKind};
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = FakeClock::new(1_000_000);
    let me = uid("aci:00000000-0000-4000-8000-0000000000aa");
    let alice = uid("aci:00000000-0000-4000-8000-0000000a11ce");
    let script = Script::new("change-jobs", vec![me]).with_roaming_writes(None);
    let fake = FakeNetwork::new(Flavor::SignalLike, script);
    let control = fake.control();
    let uuid = AccountUuid::new_random();
    let change = DurableCommand::ChangeConversation {
        conv: ConvKey::Direct(alice),
        change: ConversationChange::SetMarkedUnread {
            marked_unread: false,
        },
        asked_at: 1_700_000_000_000,
    };
    rt.block_on(async {
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
        link_until(&mut run, None, |e| {
            matches!(e, BackendEvent::AccountReady { .. })
        })
        .await;
        let (reply, answer) = tokio::sync::oneshot::channel();
        run.ends
            .commands
            .send(Command::SetRoamingWrites { on: true, reply })
            .await
            .expect("send");
        let refused = answer.await.expect("answered").expect_err("refused");
        assert_eq!(refused.kind, ErrorKind::Unsupported);
        // Sent twice (the core resends what was not confirmed): one job, two confirmations.
        for _ in 0..2 {
            run.ends
                .commands
                .send(change.clone().into_command(CmdId(6)))
                .await
                .expect("send");
        }
        let mut confirmed = Vec::new();
        while confirmed.len() < 2 {
            let env = next(&mut run, true).await;
            for e in &env.events {
                if let BackendEvent::CommandPersisted { cmd } = e {
                    confirmed.push(cmd.0);
                }
            }
        }
        assert_eq!(confirmed, [6, 6], "every send is confirmed");
        assert_eq!(
            control.durable_commands(uuid),
            std::slice::from_ref(&change)
        );
        stop(run).await;
    });
    let jobs = kourier_fake::job_log(
        &dir.path().join("backend.db"),
        &kourier_db::DbKey::from([3u8; 32]),
    )
    .expect("job log");
    assert_eq!(
        jobs.iter()
            .map(|j| (j.cmd, j.kind.as_str(), j.command.clone()))
            .collect::<Vec<_>>(),
        [(6, "change", Some(change))]
    );
}

// ---------- the roaming settings the fake plays (A10a, N4) ----------

/// The `n`-th chat of the roaming scripts: a direct chat with a synthetic person.
fn roaming_chat(n: u8) -> ConvKey {
    ConvKey::Direct(uid(&format!(
        "aci:00000000-0000-4000-8000-{:012x}",
        0xc000 + u64::from(n)
    )))
}

/// An account whose network lists six chats in its first delivery (together with `extra`), with
/// the flag `roaming` (`Script::roaming_writes`).
fn roaming_script(name: &str, roaming: Option<bool>, extra: Vec<BackendEvent>) -> Script {
    let me = uid("aci:00000000-0000-4000-8000-0000000000aa");
    let mut events = extra;
    events.extend((1..=6).map(|n| {
        BackendEvent::ConversationUpsert(kourier_model::ConversationPatch::new(roaming_chat(n)))
    }));
    Script::new(name, vec![me])
        .with_roaming_writes(roaming)
        .deliver(events)
}

/// Links a fresh actor and returns the events of its first delivery.
async fn roaming_link(run: &mut Running) -> Vec<BackendEvent> {
    let last = roaming_chat(6);
    link_until(
        run,
        None,
        |e| matches!(e, BackendEvent::ConversationUpsert(p) if p.key == last),
    )
    .await
}

/// Sends `change` for `conv`. The network's refusal is the `Err`; when it accepted, the `Ok` holds
/// the events it stated, read (and acked) up to the first one `last` accepts: one click is one
/// row of events, so the last event of the row ends the read.
async fn click(
    run: &mut Running,
    conv: &ConvKey,
    change: ConversationChange,
    last: impl Fn(&BackendEvent) -> bool,
) -> Result<Vec<BackendEvent>, kourier_model::BackendError> {
    let (reply, answer) = tokio::sync::oneshot::channel();
    run.ends
        .commands
        .send(Command::ChangeConversation {
            conv: conv.clone(),
            change,
            id: None,
            reply,
        })
        .await
        .expect("send");
    answer.await.expect("answered")?;
    Ok(read_until(run, last).await)
}

/// Acks and returns the durable events up to the first one `last` accepts (that envelope's
/// included).
async fn read_until(run: &mut Running, last: impl Fn(&BackendEvent) -> bool) -> Vec<BackendEvent> {
    let mut seen = Vec::new();
    loop {
        let env = next(run, true).await;
        let hit = env.events.iter().any(&last);
        seen.extend(env.events.into_iter().filter(|e| !e.is_ephemeral()));
        if hit {
            return seen;
        }
    }
}

fn is_pin_list(e: &BackendEvent) -> bool {
    matches!(e, BackendEvent::PinnedConversations(_))
}

fn pin_list(events: &[BackendEvent]) -> Vec<ConvKey> {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            BackendEvent::PinnedConversations(list) => Some(list.clone()),
            _ => None,
        })
        .expect("a pin list")
}

fn archived_patch(events: &[BackendEvent], conv: &ConvKey) -> Option<bool> {
    events.iter().find_map(|e| match e {
        BackendEvent::ConversationUpsert(p) if p.key == *conv => p.archived,
        _ => None,
    })
}

fn pin(on: bool) -> ConversationChange {
    ConversationChange::SetPinned { pinned: on }
}

fn archive(on: bool) -> ConversationChange {
    ConversationChange::SetArchived { archived: on }
}

/// The newest caps among `events`.
fn stated_caps(events: &[BackendEvent]) -> Option<kourier_model::AccountCaps> {
    events.iter().rev().find_map(|e| match e {
        BackendEvent::AccountCaps(caps) => Some(caps.clone()),
        _ => None,
    })
}

/// The account's pins are a list with an order: the network states the whole list, and a chat
/// pinned now goes to the END (the pin list a network keeps; the core shows the chats in the list's order).
/// A chat that is pinned already keeps its place, an unpinned chat that is pinned again goes last.
/// Mutant: a new pin goes first (`list.insert(0, ..)`).
#[test]
fn a_new_pin_goes_to_the_end() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = FakeClock::new(1_000_000);
    let fake = FakeNetwork::new(
        Flavor::SignalLike,
        roaming_script("pin-order", Some(true), Vec::new()),
    );
    let uuid = AccountUuid::new_random();
    let (a, b, c) = (roaming_chat(1), roaming_chat(2), roaming_chat(3));
    rt.block_on(async {
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
        roaming_link(&mut run).await;
        for (chat, expected) in [
            (&b, vec![b.clone()]),
            (&a, vec![b.clone(), a.clone()]),
            (&c, vec![b.clone(), a.clone(), c.clone()]),
            // Pinned already: it keeps its place.
            (&a, vec![b.clone(), a.clone(), c.clone()]),
        ] {
            let events = click(&mut run, chat, pin(true), is_pin_list)
                .await
                .expect("accepted");
            assert_eq!(pin_list(&events), expected);
        }
        // Unpinned, then pinned again: last.
        let events = click(&mut run, &a, pin(false), is_pin_list)
            .await
            .expect("accepted");
        assert_eq!(pin_list(&events), [b.clone(), c.clone()]);
        let events = click(&mut run, &a, pin(true), is_pin_list)
            .await
            .expect("accepted");
        assert_eq!(pin_list(&events), [b, c, a]);
        stop(run).await;
    });
}

/// The list is the network's, so it outlives the actor: a restart goes on from it. And a pin the
/// SCRIPT stated with a `pinned_order` (the fixture's) is in it, in its place.
#[test]
fn the_pin_list_survives_a_restart_and_holds_what_the_script_pinned() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = FakeClock::new(1_000_000);
    let (a, b, d) = (roaming_chat(1), roaming_chat(2), roaming_chat(4));
    let mut scripted = kourier_model::ConversationPatch::new(d.clone());
    scripted.pinned_order = kourier_model::Patch::Set(7);
    let fake = FakeNetwork::new(
        Flavor::SignalLike,
        roaming_script(
            "pin-restart",
            Some(true),
            vec![BackendEvent::ConversationUpsert(scripted)],
        ),
    );
    let uuid = AccountUuid::new_random();
    rt.block_on(async {
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
        roaming_link(&mut run).await;
        let events = click(&mut run, &a, pin(true), is_pin_list)
            .await
            .expect("accepted");
        assert_eq!(pin_list(&events), [d.clone(), a.clone()]);
        stop(run).await;
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Resume, &clock, &rt);
        run.ends
            .commands
            .send(Command::Connect)
            .await
            .expect("send");
        // The first run's last envelope may come again after the Resume (its ack can race the
        // Shutdown: at least once), so the list this click states is the one that holds b.
        let holds_b = |e: &BackendEvent| {
            matches!(e, BackendEvent::PinnedConversations(list) if list.contains(&b))
        };
        let events = click(&mut run, &b, pin(true), holds_b)
            .await
            .expect("accepted");
        assert_eq!(pin_list(&events), [d, a, b.clone()]);
        stop(run).await;
    });
}

/// Archiving a pinned chat unpins it (the list is stated again without it, in the row of the
/// archive); archiving a chat that is not pinned states no list. Mutant: the archive leaves the
/// pin.
#[test]
fn archiving_a_pinned_chat_unpins_it() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = FakeClock::new(1_000_000);
    let fake = FakeNetwork::new(
        Flavor::SignalLike,
        roaming_script("archive-unpins", Some(true), Vec::new()),
    );
    let uuid = AccountUuid::new_random();
    let (a, b, c) = (roaming_chat(1), roaming_chat(2), roaming_chat(3));
    rt.block_on(async {
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
        roaming_link(&mut run).await;
        for chat in [&a, &b] {
            click(&mut run, chat, pin(true), is_pin_list)
                .await
                .expect("accepted");
        }
        let events = click(&mut run, &a, archive(true), is_pin_list)
            .await
            .expect("accepted");
        assert_eq!(archived_patch(&events, &a), Some(true));
        assert_eq!(
            pin_list(&events),
            std::slice::from_ref(&b),
            "a is no longer pinned"
        );
        // Not pinned: the archive alone, one patch and no list.
        let events = click(
            &mut run,
            &c,
            archive(true),
            |e| matches!(e, BackendEvent::ConversationUpsert(p) if p.key == c),
        )
        .await
        .expect("accepted");
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(archived_patch(&events, &c), Some(true));
        // Unarchiving does not pin it back.
        let events = click(
            &mut run,
            &a,
            archive(false),
            |e| matches!(e, BackendEvent::ConversationUpsert(p) if p.key == a),
        )
        .await
        .expect("accepted");
        assert_eq!(events.len(), 1, "{events:?}");
        stop(run).await;
    });
}

/// Pinning an archived chat unarchives it, in the row that states the list. Mutant: the pin
/// leaves the archive.
#[test]
fn pinning_an_archived_chat_unarchives_it() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = FakeClock::new(1_000_000);
    let fake = FakeNetwork::new(
        Flavor::SignalLike,
        roaming_script("pin-unarchives", Some(true), Vec::new()),
    );
    let uuid = AccountUuid::new_random();
    let a = roaming_chat(1);
    rt.block_on(async {
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
        roaming_link(&mut run).await;
        click(
            &mut run,
            &a,
            archive(true),
            |e| matches!(e, BackendEvent::ConversationUpsert(p) if p.archived == Some(true)),
        )
        .await
        .expect("accepted");
        let events = click(
            &mut run,
            &a,
            pin(true),
            |e| matches!(e, BackendEvent::ConversationUpsert(p) if p.archived == Some(false)),
        )
        .await
        .expect("accepted");
        assert_eq!(pin_list(&events), std::slice::from_ref(&a));
        assert_eq!(archived_patch(&events, &a), Some(false));
        stop(run).await;
    });
}

/// The fake lets an account pin as many chats as it stated (`max_pinned_chats`, 4): a fifth pin
/// is refused and states nothing; a chat that is pinned already may be pinned again.
#[test]
fn a_pin_over_the_stated_limit_is_refused() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = FakeClock::new(1_000_000);
    let fake = FakeNetwork::new(
        Flavor::SignalLike,
        roaming_script("pin-limit", Some(true), Vec::new()),
    );
    let uuid = AccountUuid::new_random();
    rt.block_on(async {
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
        let first = roaming_link(&mut run).await;
        let caps = stated_caps(&first).expect("the fake states its caps");
        assert_eq!(caps.max_pinned_chats, Some(kourier_fake::PIN_LIMIT));
        assert_eq!(kourier_fake::PIN_LIMIT, 4);
        for n in 1..=4 {
            click(&mut run, &roaming_chat(n), pin(true), is_pin_list)
                .await
                .expect("accepted");
        }
        let refused = click(&mut run, &roaming_chat(5), pin(true), is_pin_list)
            .await
            .expect_err("refused");
        assert_eq!(refused.kind, kourier_model::ErrorKind::InvalidInput);
        // Nothing was stated for it: the next list is the one of the chat pinned again.
        let events = click(&mut run, &roaming_chat(2), pin(true), is_pin_list)
            .await
            .expect("accepted");
        assert_eq!(pin_list(&events).len(), 4, "{events:?}");
        assert!(!pin_list(&events).contains(&roaming_chat(5)));
        stop(run).await;
    });
}

/// A script plays another limit than the common 4 by stating it in an `AccountCaps` of its own
/// (A10a fix round, F4: the fake's couplings and limit are every such network's, not one network's):
/// the fake keeps the number the caps state and adds the flag; a script whose own caps state the flag
/// and no limit plays a network without one (the sixth pin goes through).
/// Mutant: the fake overwrites the limit with `PIN_LIMIT`.
#[test]
fn a_script_states_its_own_pin_limit() {
    use kourier_model::AccountCaps;
    let rt = runtime();
    let own_limit = AccountCaps {
        max_pinned_chats: Some(2),
        ..AccountCaps::default()
    };
    let mut no_limit = AccountCaps::default();
    no_limit
        .flags
        .insert(AccountCaps::FLAG_ROAMING_WRITES.to_owned(), true);
    // (script's roaming default, its own caps, the pins that go through)
    let cases = [
        (Some(true), own_limit, 2_u8, "limit-of-two"),
        (None, no_limit, 6, "no-limit"),
    ];
    for (roaming, caps, goes_through, name) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock = FakeClock::new(1_000_000);
        let fake = FakeNetwork::new(
            Flavor::SignalLike,
            roaming_script(name, roaming, vec![BackendEvent::AccountCaps(caps.clone())]),
        );
        let uuid = AccountUuid::new_random();
        rt.block_on(async {
            let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
            let first = roaming_link(&mut run).await;
            let stated = stated_caps(&first).expect("caps");
            assert_eq!(stated.max_pinned_chats, caps.max_pinned_chats, "{name}");
            assert_eq!(stated.roaming_writes(), Some(true), "{name}");
            for n in 1..=goes_through {
                click(&mut run, &roaming_chat(n), pin(true), is_pin_list)
                    .await
                    .unwrap_or_else(|e| panic!("pin {n} ({name}): {e}"));
            }
            if goes_through < 6 {
                let refused = click(
                    &mut run,
                    &roaming_chat(goes_through + 1),
                    pin(true),
                    is_pin_list,
                )
                .await
                .expect_err("over the script's own limit");
                assert_eq!(
                    refused.kind,
                    kourier_model::ErrorKind::InvalidInput,
                    "{name}"
                );
            }
            stop(run).await;
        });
    }
}

/// A relink that resumes a script played to its end states the account's roaming settings again
/// (A10a fix round, F8): the unlink wiped `backend.db`, and with it what the account had stated, and
/// no later delivery will state it. The core keeps the account's stored statement and lets a click
/// through, so the fake must apply it (before the fix it answered `Unsupported`). What is restated is
/// the newest `AccountCaps` the script played, with the flag and the limit as a first delivery would
/// have stated them, so a script's own numbers survive; a script that states the switch off is
/// restated off.
/// Mutants: nothing is restated after the relink; the restatement forgets the played caps.
#[test]
fn a_relink_of_a_played_script_states_the_roaming_settings_again() {
    use kourier_model::{AccountCaps, ErrorKind};
    let rt = runtime();
    let me = uid("aci:00000000-0000-4000-8000-0000000000aa");
    let own_caps = AccountCaps {
        max_attachment_bytes: Some(1234),
        ..AccountCaps::default()
    };
    // (the script's switch, the caps it plays, the name)
    let cases = [
        (Some(true), Vec::new(), "relinked-on"),
        (
            Some(false),
            vec![BackendEvent::AccountCaps(own_caps)],
            "relinked-off",
        ),
    ];
    for (roaming, extra, name) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock = FakeClock::new(1_000_000);
        let fake = FakeNetwork::new(Flavor::SignalLike, roaming_script(name, roaming, extra));
        let control = fake.control();
        let uuid = AccountUuid::new_random();
        let a = roaming_chat(1);
        rt.block_on(async {
            let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
            roaming_link(&mut run).await;
            tokio::time::timeout(
                Duration::from_secs(10),
                control.wait_for(|c| c.is_finished(uuid)),
            )
            .await
            .expect("the script ran to its end");
            stop(run).await;
            // What the core does on an unlink: the backend's database is gone.
            for f in std::fs::read_dir(dir.path()).expect("dir").flatten() {
                if f.file_name().to_string_lossy().starts_with("backend.db") {
                    std::fs::remove_file(f.path()).expect("wipe");
                }
            }
            let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
            let relink = Some(kourier_backend_api::Relink {
                account: AccountId(1),
                expect_own: vec![me.clone()],
            });
            let again = link_until(&mut run, relink, |e| {
                matches!(e, BackendEvent::AccountReady { .. })
            })
            .await;
            assert_eq!(
                again.len(),
                1,
                "only AccountReady, nothing is played again ({name})"
            );
            if roaming == Some(true) {
                click(
                    &mut run,
                    &a,
                    archive(true),
                    |e| matches!(e, BackendEvent::ConversationUpsert(p) if p.key == a),
                )
                .await
                .unwrap_or_else(|e| panic!("applied, not refused ({name}): {e}"));
                // The limit is restated too: four pins go through, the fifth does not.
                for n in 2..=5 {
                    click(&mut run, &roaming_chat(n), pin(true), is_pin_list)
                        .await
                        .unwrap_or_else(|e| panic!("pin {n} ({name}): {e}"));
                }
                let refused = click(&mut run, &roaming_chat(6), pin(true), is_pin_list)
                    .await
                    .expect_err("over the limit");
                assert_eq!(refused.kind, ErrorKind::InvalidInput, "{name}");
            } else {
                let refused = click(&mut run, &a, archive(true), |_| true)
                    .await
                    .expect_err("the switch is off, as the script states it");
                assert_eq!(refused.kind, ErrorKind::Unsupported, "{name}");
                // Turning it on states the caps again: the script's own numbers are still there.
                let (reply, answer) = tokio::sync::oneshot::channel();
                run.ends
                    .commands
                    .send(Command::SetRoamingWrites { on: true, reply })
                    .await
                    .expect("send");
                answer.await.expect("answered").expect("accepted");
                let events =
                    read_until(&mut run, |e| matches!(e, BackendEvent::AccountCaps(_))).await;
                let caps = stated_caps(&events).expect("caps again");
                assert_eq!(caps.roaming_writes(), Some(true), "{name}");
                assert_eq!(
                    caps.max_attachment_bytes,
                    Some(1234),
                    "the played caps ({name})"
                );
                assert_eq!(
                    caps.max_pinned_chats,
                    Some(kourier_fake::PIN_LIMIT),
                    "{name}"
                );
            }
            stop(run).await;
        });
    }
}

/// The backend is the gate: with the flag off (`Some(false)`) or not stated, the four roaming
/// changes are refused, nothing is stated for them, and a change that is no roaming setting (a
/// block) goes through.
/// Mutant: the gate lets a click pass.
#[test]
fn with_the_flag_off_a_click_is_refused() {
    use kourier_model::ErrorKind;
    let rt = runtime();
    for (roaming, name) in [(Some(false), "switch-off"), (None, "states-nothing")] {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock = FakeClock::new(1_000_000);
        let fake = FakeNetwork::new(
            Flavor::SignalLike,
            roaming_script(name, roaming, Vec::new()),
        );
        let uuid = AccountUuid::new_random();
        let a = roaming_chat(1);
        rt.block_on(async {
            let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
            let first = roaming_link(&mut run).await;
            assert_eq!(
                stated_caps(&first).and_then(|caps| caps.roaming_writes()),
                roaming,
                "the first delivery states the flag as the script says ({name})"
            );
            for change in [
                archive(true),
                pin(true),
                ConversationChange::SetMuted { until: Some(5) },
                ConversationChange::SetMarkedUnread {
                    marked_unread: true,
                },
            ] {
                let refused = click(&mut run, &a, change.clone(), |_| true)
                    .await
                    .expect_err("refused");
                assert_eq!(refused.kind, ErrorKind::Unsupported, "{change:?} ({name})");
            }
            // The network states nothing for them: the first row after them is the block's own.
            let events = click(
                &mut run,
                &a,
                ConversationChange::SetBlocked { blocked: true },
                |e| matches!(e, BackendEvent::ConversationUpsert(p) if p.blocked == Some(true)),
            )
            .await
            .expect("a block is no roaming setting");
            assert_eq!(events.len(), 1, "{events:?} ({name})");
            stop(run).await;
        });
        // (The network heard all five: a refusal is an answer.)
        assert_eq!(fake.control().changes(uuid).len(), 5, "{name}");
    }
}

/// `Command::SetRoamingWrites` turns the flag the fake states and states the account's caps
/// again, every other cap as it was (a script's own caps keep their numbers); the changes it
/// gates follow. A fake that states no flag refuses the switch.
/// Mutant: the caps are not stated again.
#[test]
fn the_roaming_switch_flips_the_stated_caps() {
    use kourier_model::{AccountCaps, ErrorKind};
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = FakeClock::new(1_000_000);
    let own_caps = AccountCaps {
        max_attachment_bytes: Some(1234),
        ..AccountCaps::default()
    };
    let fake = FakeNetwork::new(
        Flavor::SignalLike,
        roaming_script(
            "switch",
            Some(false),
            vec![BackendEvent::AccountCaps(own_caps)],
        ),
    );
    let uuid = AccountUuid::new_random();
    let a = roaming_chat(1);
    async fn switch(run: &mut Running, on: bool) -> Result<(), kourier_model::BackendError> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        run.ends
            .commands
            .send(Command::SetRoamingWrites { on, reply })
            .await
            .expect("send");
        answer.await.expect("answered")
    }
    rt.block_on(async {
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
        let first = roaming_link(&mut run).await;
        let caps = stated_caps(&first).expect("caps");
        assert_eq!(caps.roaming_writes(), Some(false));
        assert_eq!(caps.max_attachment_bytes, Some(1234), "the script's own");
        assert_eq!(caps.max_pinned_chats, Some(4));
        click(&mut run, &a, archive(true), |_| true)
            .await
            .expect_err("the switch is off");

        switch(&mut run, true).await.expect("accepted");
        let events = read_until(&mut run, |e| matches!(e, BackendEvent::AccountCaps(_))).await;
        let on = stated_caps(&events).expect("caps again");
        assert_eq!(on.roaming_writes(), Some(true));
        let mut expected = caps.clone();
        expected
            .flags
            .insert(AccountCaps::FLAG_ROAMING_WRITES.to_owned(), true);
        assert_eq!(on, expected, "the other caps are as they were");
        click(&mut run, &a, archive(true), |_| true)
            .await
            .expect("the switch is on");

        switch(&mut run, false).await.expect("accepted");
        let events = read_until(&mut run, |e| matches!(e, BackendEvent::AccountCaps(_))).await;
        assert_eq!(
            stated_caps(&events).and_then(|c| c.roaming_writes()),
            Some(false)
        );
        let refused = click(&mut run, &a, archive(false), |_| true)
            .await
            .expect_err("off again");
        assert_eq!(refused.kind, ErrorKind::Unsupported);
        stop(run).await;
    });

    // A fake that states nothing cannot be switched.
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeNetwork::new(
        Flavor::SignalLike,
        roaming_script("no-switch", None, Vec::new()),
    );
    rt.block_on(async {
        let mut run = spawn(
            &fake,
            dir.path(),
            AccountUuid::new_random(),
            StartMode::Fresh,
            &clock,
            &rt,
        );
        roaming_link(&mut run).await;
        let refused = switch(&mut run, true).await.expect_err("refused");
        assert_eq!(refused.kind, ErrorKind::Unsupported);
        stop(run).await;
    });
}

/// The durable change the core sends (the clearing of a mark when the chat is read) is applied
/// like the plain command while the switch is on, in the row that acknowledges it, and once however
/// often the core sends it; with the switch off it is acknowledged and dropped.
/// Mutant: the fake acknowledges and applies nothing.
#[test]
fn the_durable_clear_is_applied_once_and_acknowledged() {
    use kourier_model::CmdId;
    let rt = runtime();
    let a = roaming_chat(1);
    let clear = || {
        kourier_backend_api::DurableCommand::ChangeConversation {
            conv: roaming_chat(1),
            change: ConversationChange::SetMarkedUnread {
                marked_unread: false,
            },
            asked_at: 1_700_000_000_000,
        }
        .into_command(CmdId(7))
    };
    for (roaming, applied) in [(Some(true), true), (Some(false), false)] {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock = FakeClock::new(1_000_000);
        let fake = FakeNetwork::new(
            Flavor::SignalLike,
            roaming_script("durable-clear", roaming, Vec::new()),
        );
        let uuid = AccountUuid::new_random();
        rt.block_on(async {
            let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
            roaming_link(&mut run).await;
            // Sent twice: the core sends again what was not confirmed.
            for _ in 0..2 {
                run.ends.commands.send(clear()).await.expect("send");
            }
            let mut persisted = 0;
            let mut seen = Vec::new();
            while persisted < 2 {
                let env = next(&mut run, true).await;
                for e in env.events {
                    if matches!(e, BackendEvent::CommandPersisted { cmd } if cmd.0 == 7) {
                        persisted += 1;
                    }
                    seen.push(e);
                }
            }
            if applied {
                // The patch is in the row that acknowledges the command, ahead of it, and once.
                assert!(
                    matches!(
                        seen.as_slice(),
                        [
                            BackendEvent::ConversationUpsert(p),
                            BackendEvent::CommandPersisted { .. },
                            BackendEvent::CommandPersisted { .. }
                        ] if p.key == a && p.marked_unread == Some(false)
                    ),
                    "{seen:?}"
                );
            } else {
                assert!(
                    matches!(
                        seen.as_slice(),
                        [
                            BackendEvent::CommandPersisted { .. },
                            BackendEvent::CommandPersisted { .. }
                        ]
                    ),
                    "dropped with the switch off: {seen:?}"
                );
            }
            stop(run).await;
        });
        assert_eq!(
            fake.control().durable_commands(uuid).len(),
            1,
            "one job either way ({roaming:?})"
        );
    }
}

/// A script can make relinks fail (`Script::fail_relink`): the login ends with a `Failed` step
/// and nothing durable, as often as scripted; the next relink links. The first link of the
/// account is not a relink and is not affected.
#[test]
fn a_script_can_fail_relinks() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = FakeClock::new(1_000_000);
    let me = uid("aci:00000000-0000-4000-8000-0000000000aa");
    let script = Script::new("failing-relink", vec![me.clone()])
        .with_number("+15550100")
        .fail_relink(2);
    let fake = FakeNetwork::new(Flavor::SignalLike, script);
    let uuid = AccountUuid::new_random();
    let relink = || {
        Some(kourier_backend_api::Relink {
            account: AccountId(1),
            expect_own: vec![me.clone()],
        })
    };
    rt.block_on(async {
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
        let first = link_until(&mut run, None, |e| {
            matches!(e, BackendEvent::AccountReady { .. })
        })
        .await;
        // The script's number travels with `AccountReady`.
        assert!(matches!(
            first.as_slice(),
            [BackendEvent::AccountReady { number: Some(n), .. }] if n.expose() == "+15550100"
        ));
        stop(run).await;
        for attempt in 1..=3 {
            for f in std::fs::read_dir(dir.path()).expect("dir").flatten() {
                if f.file_name().to_string_lossy().starts_with("backend.db") {
                    std::fs::remove_file(f.path()).expect("wipe");
                }
            }
            let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
            run.ends
                .commands
                .send(Command::StartLogin {
                    flow: "qr".into(),
                    relink: relink(),
                    taken: Vec::new(),
                })
                .await
                .expect("send");
            let mut failed = false;
            let mut ready = false;
            while !(failed || ready) {
                let env = next(&mut run, true).await;
                for e in &env.events {
                    match e {
                        BackendEvent::Login(s) => {
                            failed |= matches!(s.kind, LoginStepKind::Failed { .. });
                        }
                        BackendEvent::AccountReady { .. } => ready = true,
                        _ => {}
                    }
                }
            }
            assert_eq!(
                (failed, ready),
                (attempt <= 2, attempt == 3),
                "relink {attempt}"
            );
            stop(run).await;
        }
    });
}

// ---------- the resend promise (K-B1 B6) ----------

/// Alice's direct chat and a group of alice, bob and us, delivered to the account once it is
/// linked: the chats a transmit test sends into.
fn chats_script(name: &str) -> (Script, ConvKey, ConvKey, UserId, UserId) {
    use kourier_model::{ConversationPatch, Member, MemberList, MemberRole, Patch};
    let me = uid("aci:00000000-0000-4000-8000-0000000000aa");
    let alice = uid("aci:00000000-0000-4000-8000-0000000a11ce");
    let bob = uid("aci:00000000-0000-4000-8000-000000000b0b");
    let direct = ConvKey::Direct(alice.clone());
    let group = ConvKey::Group("gv2:b3V0Ym94LXRlc3QtZ3JvdXA".into());
    let member = |user: &UserId, role| Member {
        user: user.clone(),
        role,
    };
    let mut d = ConversationPatch::new(direct.clone());
    d.features = Some(scripts::signal_features(false));
    let mut g = ConversationPatch::new(group.clone());
    g.members = Some(MemberList::snapshot(vec![
        member(&alice, MemberRole::Admin),
        member(&bob, MemberRole::Member),
        member(&me, MemberRole::Member),
    ]));
    g.features = Some(scripts::signal_features(true));
    g.title = Patch::Set("Resend Test".into());
    let script = Script::new(name, vec![me])
        .deliver(vec![
            BackendEvent::ConversationUpsert(d),
            BackendEvent::ConversationUpsert(g),
        ])
        .step(Step::WaitAcked);
    (script, direct, group, alice, bob)
}

/// One transmit of the plain text `local` into `conv` (to `only` when given), answered.
async fn transmit(
    run: &mut Running,
    conv: &ConvKey,
    local: &str,
    only: Option<Vec<UserId>>,
) -> kourier_backend_api::TransmitOutcome {
    use kourier_backend_api::{OutgoingKind, OutgoingRequest};
    let req = OutgoingRequest {
        conv: conv.clone(),
        local: local.into(),
        sent_at: 5,
        kind: OutgoingKind::Message {
            body: Body::Text(kourier_model::Text::plain("hello")),
            reply_to: None,
            expiry: None,
        },
        only,
        is_recipient_update: false,
    };
    let (reply, answer) = tokio::sync::oneshot::channel();
    run.ends
        .commands
        .send(Command::Transmit {
            local: kourier_model::MessageId(1),
            req: Box::new(req),
            reply,
        })
        .await
        .expect("send");
    tokio::time::timeout(Duration::from_secs(10), answer)
        .await
        .expect("timely")
        .expect("answered")
}

/// Who a `Sent` outcome says has the message.
fn sent_to(outcome: &kourier_backend_api::TransmitOutcome) -> Vec<UserId> {
    match outcome {
        kourier_backend_api::TransmitOutcome::Sent { per_recipient, .. } => {
            per_recipient.iter().map(|(u, _)| u.clone()).collect()
        }
        other => panic!("not sent: {other:?}"),
    }
}

/// Links a fake of `flavor` that has the chats of [`chats_script`] and runs `f` with it; returns
/// what its server logged, from `backend.db`.
fn with_chats(
    flavor: Flavor,
    name: &str,
    f: impl AsyncFnOnce(&mut Running, &ConvKey, &ConvKey, &UserId, &UserId),
) -> Vec<kourier_fake::ServerDelivery> {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = FakeClock::new(1_000_000);
    let (script, direct, group, alice, bob) = chats_script(name);
    let fake = FakeNetwork::new(flavor, script);
    let uuid = AccountUuid::new_random();
    rt.block_on(async {
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
        let g = group.clone();
        link_until(
            &mut run,
            None,
            |e| matches!(e, BackendEvent::ConversationUpsert(p) if p.key == g),
        )
        .await;
        f(&mut run, &direct, &group, &alice, &bob).await;
        stop(run).await;
    });
    // (The key is the one `spawn` gives every test actor.)
    kourier_fake::server_log(
        &dir.path().join("backend.db"),
        &kourier_db::DbKey::from([3u8; 32]),
    )
    .expect("server log")
}

/// The promise of `Command::Transmit`: a transmit of a `local` the network already has makes no
/// second message. The same request twice (the resend after a kill before the outcome commit),
/// a retry that names one member, and a follow-up to the rest: the server holds each recipient
/// ONCE and every answer is `Sent`. A PIN on today's fake; the look-up before a recipient is
/// counted (`have == 0`) is what it guards.
#[test]
fn a_resend_of_the_same_local_is_one_message_per_recipient() {
    let log = with_chats(
        Flavor::WhatsAppLike,
        "resend",
        async |run, _direct, group, alice, bob| {
            // The first attempt reached alice only (a partial send), and is sent again as it was.
            let first = transmit(run, group, "3EB0RESEND", Some(vec![alice.clone()])).await;
            assert_eq!(sent_to(&first), std::slice::from_ref(alice));
            let again = transmit(run, group, "3EB0RESEND", Some(vec![alice.clone()])).await;
            assert_eq!(
                sent_to(&again),
                std::slice::from_ref(alice),
                "answered as sent"
            );
            // The rest of the members are sent to: alice has it, bob does not.
            let rest = transmit(run, group, "3EB0RESEND", None).await;
            assert_eq!(sent_to(&rest), [alice.clone(), bob.clone()]);
            // A retry naming one member who has it already.
            let one = transmit(run, group, "3EB0RESEND", Some(vec![bob.clone()])).await;
            assert_eq!(sent_to(&one), std::slice::from_ref(bob));
        },
    );
    let who: Vec<&str> = log.iter().map(|d| d.recipient.as_str()).collect();
    assert_eq!(
        who,
        [
            "aci:00000000-0000-4000-8000-0000000a11ce",
            "aci:00000000-0000-4000-8000-000000000b0b"
        ],
        "each recipient once: {log:?}"
    );
    assert!(log.iter().all(|d| d.local == "3EB0RESEND"));
}

/// A network whose ids are unique per conversation (Threema) lets two chats use one `local`
/// (hand-made requests: the core's allocator never repeats an id of an account). A person who
/// is in both chats gets two messages, one per chat; each of them is still sent once.
#[test]
fn a_local_in_two_chats_is_two_messages() {
    let log = with_chats(
        Flavor::ThreemaLike,
        "two-chats",
        async |run, direct, group, alice, bob| {
            let in_direct = transmit(run, direct, "0000000000000001", None).await;
            assert_eq!(sent_to(&in_direct), std::slice::from_ref(alice));
            let in_group = transmit(run, group, "0000000000000001", None).await;
            assert_eq!(sent_to(&in_group), [alice.clone(), bob.clone()]);
            // Each chat's copy is sent once, however often it is asked for.
            transmit(run, direct, "0000000000000001", None).await;
            transmit(run, group, "0000000000000001", None).await;
        },
    );
    let copies: Vec<(String, &str)> = log
        .iter()
        .map(|d| (d.conv.clone(), d.recipient.as_str()))
        .collect();
    assert_eq!(
        copies,
        [
            (
                "direct:aci:00000000-0000-4000-8000-0000000a11ce".to_owned(),
                "aci:00000000-0000-4000-8000-0000000a11ce"
            ),
            (
                "group:gv2:b3V0Ym94LXRlc3QtZ3JvdXA".to_owned(),
                "aci:00000000-0000-4000-8000-0000000a11ce"
            ),
            (
                "group:gv2:b3V0Ym94LXRlc3QtZ3JvdXA".to_owned(),
                "aci:00000000-0000-4000-8000-000000000b0b"
            ),
        ],
        "alice has the message in both chats, bob in the group: {log:?}"
    );
}

// ---------- the upload phase and the abort (upload progress, `Script::uploads`) ----------

/// Links a fake with alice's direct chat whose messages with files have an upload phase
/// (`uploads`), and runs `f` with it, the blob directory and the control; returns what its
/// server logged.
fn with_uploads(
    uploads: kourier_fake::UploadScript,
    f: impl AsyncFnOnce(
        &mut Running,
        &ConvKey,
        &std::path::Path,
        &kourier_fake::FakeControl,
        AccountUuid,
        &FakeClock,
    ),
) -> Vec<kourier_fake::ServerDelivery> {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = FakeClock::new(1_000_000);
    let (script, direct, group, _, _) = chats_script("uploads");
    let fake = FakeNetwork::new(
        Flavor::SignalLike,
        script.with_uploads(direct.clone(), uploads),
    );
    let control = fake.control();
    let uuid = AccountUuid::new_random();
    rt.block_on(async {
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
        let g = group.clone();
        link_until(
            &mut run,
            None,
            |e| matches!(e, BackendEvent::ConversationUpsert(p) if p.key == g),
        )
        .await;
        f(
            &mut run,
            &direct,
            &dir.path().join("blobs"),
            &control,
            uuid,
            &clock,
        )
        .await;
        stop(run).await;
    });
    kourier_fake::server_log(
        &dir.path().join("backend.db"),
        &kourier_db::DbKey::from([3u8; 32]),
    )
    .expect("server log")
}

/// A message of `files` (sizes) in `conv`, with a real blob for each in `blobs`, handed to the
/// actor; returns the reply that waits for the outcome.
async fn transmit_files(
    run: &mut Running,
    conv: &ConvKey,
    blobs: &std::path::Path,
    local: i64,
    files: &[usize],
) -> tokio::sync::oneshot::Receiver<kourier_backend_api::TransmitOutcome> {
    use kourier_backend_api::{OutgoingKind, OutgoingRequest};
    std::fs::create_dir_all(blobs).expect("blob dir");
    let attachments: Vec<kourier_model::Attachment> = files
        .iter()
        .enumerate()
        .map(|(i, size)| {
            let key = kourier_blob::FileKey::generate();
            let rel = std::path::PathBuf::from(format!("{local}-{i}"));
            kourier_blob::write_all(&blobs.join(&rel), &key, &vec![i as u8; *size]).expect("blob");
            kourier_model::Attachment {
                kind: kourier_model::AttachmentKind::File,
                size: *size as u64,
                local: Some(kourier_model::BlobRef {
                    path: rel,
                    key: kourier_model::BlobKey::new(*key.as_bytes()),
                }),
                ..kourier_model::Attachment::default()
            }
        })
        .collect();
    let req = OutgoingRequest {
        conv: conv.clone(),
        local: local.to_string().into(),
        sent_at: 5,
        kind: OutgoingKind::Message {
            body: Body::Media {
                attachments,
                caption: None,
            },
            reply_to: None,
            expiry: None,
        },
        only: None,
        is_recipient_update: false,
    };
    let (reply, answer) = tokio::sync::oneshot::channel();
    run.ends
        .commands
        .send(Command::Transmit {
            local: kourier_model::MessageId(local),
            req: Box::new(req),
            reply,
        })
        .await
        .expect("send");
    answer
}

/// The first report of every file of an upload phase: (index, done, total).
async fn first_reports(run: &mut Running, files: usize) -> Vec<(u16, u64, u64)> {
    let mut found: Vec<Option<(u16, u64, u64)>> = vec![None; files];
    while found.iter().any(Option::is_none) {
        let env = next(run, true).await;
        for e in env.events {
            if let BackendEvent::AttachmentProgress {
                index, done, total, ..
            } = e
            {
                found[usize::from(index)].get_or_insert((index, done, total));
            }
        }
    }
    found.into_iter().flatten().collect()
}

async fn abort(run: &mut Running, local: i64) -> kourier_backend_api::AbortAnswer {
    let (reply, answer) = tokio::sync::oneshot::channel();
    run.ends
        .commands
        .send(Command::AbortTransmit {
            local: kourier_model::MessageId(local),
            reply,
        })
        .await
        .expect("send");
    tokio::time::timeout(Duration::from_secs(10), answer)
        .await
        .expect("an answer at once")
        .expect("answered")
}

/// The promise of `Command::AbortTransmit` for a backend that reports uploads, kept by the
/// fake's held upload phase: every file is reported at its share (the first report names all of
/// them), the abort is answered `Stopping` at once, the attempt ends `Failed` and the fake
/// server never got the message (no copy in its log, no transmit recorded). A second abort
/// finds no attempt: `TooLate`.
#[test]
fn a_held_upload_reports_each_file_and_an_abort_stops_it() {
    let log = with_uploads(
        kourier_fake::UploadScript::Held {
            shares: vec![1.0, 0.45],
            hold: Duration::from_secs(3600),
        },
        async |run, conv, blobs, control, uuid, _clock| {
            let outcome = transmit_files(run, conv, blobs, 31, &[1000, 2000]).await;
            let first = first_reports(run, 2).await;
            assert_eq!(
                first,
                [(0, 1000, 1000), (1, 900, 2000)],
                "the shares of the script"
            );
            assert_eq!(
                abort(run, 31).await,
                kourier_backend_api::AbortAnswer::Stopping
            );
            let outcome = tokio::time::timeout(Duration::from_secs(10), outcome)
                .await
                .expect("the attempt ends")
                .expect("answered");
            assert!(
                matches!(&outcome, kourier_backend_api::TransmitOutcome::Failed(e)
                    if matches!(e.kind, kourier_model::ErrorKind::Internal)),
                "{outcome:?}"
            );
            assert!(
                control.transmits(uuid).is_empty(),
                "the fake server was never asked"
            );
            assert_eq!(
                abort(run, 31).await,
                kourier_backend_api::AbortAnswer::TooLate,
                "nothing left to stop"
            );
            let answers: Vec<_> = control.aborts(uuid).iter().map(|a| a.answer).collect();
            assert_eq!(
                answers,
                [
                    kourier_backend_api::AbortAnswer::Stopping,
                    kourier_backend_api::AbortAnswer::TooLate
                ]
            );
        },
    );
    assert!(log.is_empty(), "the server got nothing: {log:?}");
}

/// The script whose abort comes too late: it is answered `TooLate`, the held phase ends at once
/// and the message goes out, to the server and to the record of transmits.
#[test]
fn a_too_late_upload_answers_the_abort_too_late_and_sends() {
    let log = with_uploads(
        kourier_fake::UploadScript::TooLate {
            shares: vec![0.5],
            hold: Duration::from_secs(3600),
        },
        async |run, conv, blobs, control, uuid, _clock| {
            let outcome = transmit_files(run, conv, blobs, 32, &[4000]).await;
            assert_eq!(first_reports(run, 1).await, [(0, 2000, 4000)]);
            assert_eq!(
                abort(run, 32).await,
                kourier_backend_api::AbortAnswer::TooLate
            );
            let outcome = tokio::time::timeout(Duration::from_secs(10), outcome)
                .await
                .expect("the attempt ends")
                .expect("answered");
            assert!(
                matches!(outcome, kourier_backend_api::TransmitOutcome::Sent { .. }),
                "{outcome:?}"
            );
            assert_eq!(control.transmits(uuid).len(), 1);
        },
    );
    assert_eq!(log.len(), 1, "one copy for alice: {log:?}");
}

/// The moving script: the first report names the file at 0, every 250 ms of the ACCOUNT clock
/// (the test's, which only moves when it is told to) it gains its step, it ends full, and the
/// uploads are reported over before the fake server takes the message.
#[test]
fn a_moving_upload_rises_by_its_step_and_ends_full() {
    let log = with_uploads(
        kourier_fake::UploadScript::Moving { step: 0.5 },
        async |run, conv, blobs, control, uuid, clock| {
            let mut outcome = transmit_files(run, conv, blobs, 33, &[1000]).await;
            let (mut seen, mut over) = (Vec::new(), false);
            let mut answer = None;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                while let Ok(env) = run.ends.events.try_recv() {
                    if let Some(ack) = env.ack {
                        ack.complete(Ok(()));
                    }
                    for e in env.events {
                        match e {
                            BackendEvent::AttachmentProgress { done, total, .. } => {
                                assert_eq!(total, 1000);
                                seen.push(done);
                            }
                            BackendEvent::TransmitProgress {
                                progress, phase, ..
                            } if progress >= 1.0 => {
                                assert_eq!(phase, None);
                                over = true;
                            }
                            _ => {}
                        }
                    }
                }
                // (Once more after the answer: the events went out before it.)
                if answer.is_some() {
                    break;
                }
                answer = outcome.try_recv().ok();
                if clock.sleepers() > 0 {
                    clock.advance_to_next();
                }
                assert!(tokio::time::Instant::now() < deadline, "{seen:?}");
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            let outcome = answer.expect("the answer");
            assert_eq!(seen, [0, 500, 1000], "one tick at a time, ending full");
            assert!(over, "the uploads were reported over before the answer");
            assert!(
                matches!(outcome, kourier_backend_api::TransmitOutcome::Sent { .. }),
                "{outcome:?}"
            );
            assert_eq!(control.transmits(uuid).len(), 1);
            assert!(control.aborts(uuid).is_empty());
        },
    );
    assert_eq!(log.len(), 1, "one copy for alice: {log:?}");
}

// ---------- the server that names the messages (KT-2, `Flavor::TelegramLike`) ----------

/// One of the requests of a Telegram-like test: `kind` into `conv` under the id `local`, sent at
/// `sent_at`.
fn request(
    conv: &ConvKey,
    local: &str,
    sent_at: i64,
    kind: kourier_backend_api::OutgoingKind,
) -> kourier_backend_api::OutgoingRequest {
    kourier_backend_api::OutgoingRequest {
        conv: conv.clone(),
        local: local.into(),
        sent_at,
        kind,
        only: None,
        is_recipient_update: false,
    }
}

/// A plain text message, optionally quoting `reply_to`.
fn text_message(
    conv: &ConvKey,
    local: &str,
    sent_at: i64,
    reply_to: Option<kourier_model::ReplyTo>,
) -> kourier_backend_api::OutgoingRequest {
    request(
        conv,
        local,
        sent_at,
        kourier_backend_api::OutgoingKind::Message {
            body: Body::text("hello"),
            reply_to,
            expiry: None,
        },
    )
}

/// A transmit handed to the actor, and what the minimal core has seen of it since: the durable
/// events it acked (in order) and the outcome, once it is in.
struct Sending {
    answer: tokio::sync::oneshot::Receiver<kourier_backend_api::TransmitOutcome>,
    seen: Vec<BackendEvent>,
    outcome: Option<kourier_backend_api::TransmitOutcome>,
}

/// Hands `req` to the actor.
async fn send(run: &mut Running, req: kourier_backend_api::OutgoingRequest) -> Sending {
    let (reply, answer) = tokio::sync::oneshot::channel();
    run.ends
        .commands
        .send(Command::Transmit {
            local: kourier_model::MessageId(1),
            req: Box::new(req),
            reply,
        })
        .await
        .expect("send");
    Sending {
        answer,
        seen: Vec::new(),
        outcome: None,
    }
}

impl Sending {
    /// Acks what comes and notes it, and notes the outcome, until `done` holds (10 s at most).
    /// `drive` moves the account clock on whenever something waits for it (a held forwarder, a
    /// late answer); without it the clock stands still.
    async fn pump(
        &mut self,
        run: &mut Running,
        clock: &FakeClock,
        drive: bool,
        done: impl Fn(&Self) -> bool,
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            while let Ok(env) = run.ends.events.try_recv() {
                if let Some(ack) = env.ack {
                    ack.complete(Ok(()));
                }
                self.seen
                    .extend(env.events.into_iter().filter(|e| !e.is_ephemeral()));
            }
            // (After the events: an outcome that is in came after every ack the actor waited for.)
            if self.outcome.is_none() {
                self.outcome = self.answer.try_recv().ok();
            }
            if done(self) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "nothing happened; seen {:?}, outcome {:?}, the fake's forwarder holds {} rows",
                self.seen,
                self.outcome,
                forwarder_len(&run.backend_db)
            );
            if drive && clock.sleepers() > 0 {
                clock.advance_to_next();
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    /// Waits for the outcome.
    async fn told(&mut self, run: &mut Running, clock: &FakeClock) {
        self.pump(run, clock, false, |s| s.outcome.is_some()).await;
    }

    /// Waits until the outcome is in AND the fake's forwarder holds nothing: every durable event the
    /// actor committed with the accept (the echo is one, committed in the transaction of the accept,
    /// before the answer) has been shipped and acked, so `seen` is what the transmit told the account
    /// and "nothing more comes" is a state, not a stretch of time (R2-L5).
    async fn settled(&mut self, run: &mut Running, clock: &FakeClock) {
        let db = run.backend_db.clone();
        self.pump(run, clock, false, |s| {
            s.outcome.is_some() && forwarder_len(&db) == 0
        })
        .await;
    }
}

/// How many rows of the fake's `core_outbox` are not shipped and acked yet.
fn forwarder_len(db: &std::path::Path) -> u64 {
    let conn = kourier_db::open_read_only(db, &kourier_db::DbKey::from([3u8; 32]))
        .expect("the fake's backend.db");
    kourier_db::outbox::len(&conn).expect("the forwarder's length")
}

/// Links a fake of `flavor` that has the chats of [`chats_script`] (changed by `tweak`, which
/// gets the direct chat's key) and runs `f` with it, the account clock and the direct chat. The
/// clock is driven while the account links, for a script that holds the forwarder back. Returns
/// what the fake's server logged; the fake must have kept the contract.
fn with_flavour(
    flavor: Flavor,
    name: &str,
    tweak: impl FnOnce(Script, &ConvKey) -> Script,
    f: impl AsyncFnOnce(&mut Running, &FakeClock, &ConvKey, &UserId),
) -> Vec<kourier_fake::ServerDelivery> {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = FakeClock::new(1_000_000);
    let (script, direct, group, alice, _) = chats_script(name);
    let fake = FakeNetwork::new(flavor, tweak(script, &direct));
    let control = fake.control();
    let uuid = AccountUuid::new_random();
    rt.block_on(async {
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
        let g = group.clone();
        let driver = async {
            loop {
                if clock.sleepers() > 0 {
                    clock.advance_to_next();
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        };
        tokio::pin!(driver);
        tokio::select! {
            _ = &mut driver => unreachable!("the driver does not end"),
            _ = link_until(
                &mut run,
                None,
                |e| matches!(e, BackendEvent::ConversationUpsert(p) if p.key == g),
            ) => {}
        }
        f(&mut run, &clock, &direct, &alice).await;
        stop(run).await;
    });
    assert_eq!(control.violations(), Vec::<String>::new());
    kourier_fake::server_log(
        &dir.path().join("backend.db"),
        &kourier_db::DbKey::from([3u8; 32]),
    )
    .expect("server log")
}

/// The pieces of a `SentAs` answer.
fn sent_as(
    outcome: Option<kourier_backend_api::TransmitOutcome>,
) -> (kourier_backend_api::AssignedId, Option<i64>, Vec<UserId>) {
    match outcome {
        Some(kourier_backend_api::TransmitOutcome::SentAs {
            assigned,
            server_at,
            per_recipient,
        }) => (
            assigned,
            server_at,
            per_recipient.into_iter().map(|(u, _)| u).collect(),
        ),
        other => panic!("not SentAs: {other:?}"),
    }
}

/// The one own message among `seen`, which must hold nothing else.
fn the_echo(seen: &[BackendEvent]) -> &IncomingMessage {
    match seen {
        [BackendEvent::Message(m)] => m,
        other => panic!("not exactly one message: {other:?}"),
    }
}

/// A message answers `SentAs` with its `sent_at` in decimal as id and position, derived: a resend
/// under the same allocated id gets the SAME one, a later message a larger one, and the server
/// holds each message once. Mutant: the assigned id taken from the allocated one.
#[test]
fn a_telegram_like_message_is_answered_sent_as_and_a_resend_gets_the_same_id() {
    let log = with_flavour(
        Flavor::TelegramLike,
        "sent-as",
        |s, _| s,
        async |run, clock, direct, alice| {
            let req = text_message(direct, "k0000018f00000001", 1_760_000_000_123, None);
            let mut first = send(run, req.clone()).await;
            first.told(run, clock).await;
            let (assigned, server_at, to) = sent_as(first.outcome);
            assert_eq!(&*assigned.local, "1760000000123");
            assert_eq!(assigned.seq, 1_760_000_000_123);
            assert_eq!(&to, std::slice::from_ref(alice));
            assert!(server_at.is_some());

            // The same request again (the resend after a kill before the outcome): the same answer.
            let mut again = send(run, req).await;
            again.told(run, clock).await;
            let (second, _, to) = sent_as(again.outcome);
            assert_eq!(second, assigned, "a resend is the same message");
            assert_eq!(&to, std::slice::from_ref(alice));

            // The next message is later, so it has the larger position.
            let mut next = send(
                run,
                text_message(direct, "k0000018f00000002", 1_760_000_000_124, None),
            )
            .await;
            next.told(run, clock).await;
            let (third, _, _) = sent_as(next.outcome);
            assert_eq!(&*third.local, "1760000000124");
            assert!(third.seq > assigned.seq);
        },
    );
    let held: Vec<&str> = log.iter().map(|d| d.local.as_str()).collect();
    assert_eq!(
        held,
        ["k0000018f00000001", "k0000018f00000002"],
        "the server's log keeps the allocated id, once each: {log:?}"
    );
}

/// An edit, a delete and a reaction name a message that has its id already, under the allocated or
/// the assigned id, and are answered with a plain `Sent` and no echo.
#[test]
fn a_telegram_like_edit_delete_and_reaction_are_answered_sent_under_either_id() {
    use kourier_backend_api::OutgoingKind;
    with_flavour(
        Flavor::TelegramLike,
        "modifiers",
        |s, _| s,
        async |run, clock, direct, _| {
            let mut sent = send(
                run,
                text_message(direct, "k0000018f00000001", 1_760_000_000_123, None),
            )
            .await;
            sent.told(run, clock).await;
            let (assigned, _, _) = sent_as(sent.outcome);
            let mut n = 1_760_000_000_200;
            for target in ["k0000018f00000001", &*assigned.local] {
                let target = scripts::target(Some(direct), None, target);
                let kinds = [
                    OutgoingKind::Edit {
                        target: target.clone(),
                        body: Body::text("edited"),
                    },
                    OutgoingKind::Delete {
                        target: target.clone(),
                        for_everyone: true,
                    },
                    OutgoingKind::React {
                        target: target.clone(),
                        emoji: "👍".into(),
                        remove: false,
                    },
                ];
                for kind in kinds {
                    n += 1;
                    let name = kind.name();
                    let mut modifier =
                        send(run, request(direct, &format!("k{n:016x}"), n, kind)).await;
                    modifier.told(run, clock).await;
                    assert!(
                        matches!(
                            modifier.outcome,
                            Some(kourier_backend_api::TransmitOutcome::Sent { .. })
                        ),
                        "{name}: {:?}",
                        modifier.outcome
                    );
                    modifier.settled(run, clock).await;
                    assert_eq!(modifier.seen, [], "{name}: an echo of a modifier");
                }
            }
        },
    );
}

/// The echo is one durable own message under the assigned id, committed and acked BEFORE the
/// answer goes out: its position, the `server_at` of the answer, the request's content and quote,
/// and the id it was allocated under. A resend commits none.
/// Mutants: `allocated_as` left out; the echo of a resend.
#[test]
fn a_telegram_like_echo_comes_before_the_answer_and_names_the_binding() {
    with_flavour(
        Flavor::TelegramLike,
        "echo",
        |s, _| s,
        async |run, clock, direct, alice| {
            let quote = kourier_model::ReplyTo {
                target: scripts::target(Some(direct), Some(alice), "77"),
                author: Some(alice.clone()),
                snapshot: None,
            };
            let req = text_message(
                direct,
                "k0000018f00000001",
                1_760_000_000_123,
                Some(quote.clone()),
            );
            let mut first = send(run, req.clone()).await;
            first.told(run, clock).await;
            let (assigned, server_at, _) = sent_as(first.outcome.clone());
            let echo = the_echo(&first.seen);
            assert_eq!(echo.conv, *direct);
            assert_eq!(echo.local, assigned.local);
            assert_eq!(echo.origin, Origin::OwnThisDevice);
            assert_eq!(echo.net_seq, Some(assigned.seq));
            assert_eq!(echo.allocated_as.as_deref(), Some("k0000018f00000001"));
            assert_eq!(echo.server_at, server_at, "one server_at in both");
            assert_eq!(echo.sent_at, 1_760_000_000_123);
            assert_eq!(echo.body, Body::text("hello"));
            assert_eq!(echo.reply_to, Some(quote));

            // A resend is the same message: nothing is told again.
            let mut again = send(run, req).await;
            again.told(run, clock).await;
            again.settled(run, clock).await;
            assert_eq!(again.seen, [], "a resend commits no echo");
            assert!(matches!(
                again.outcome,
                Some(kourier_backend_api::TransmitOutcome::SentAs { .. })
            ));
        },
    );
}

/// "Echo first" is an ORDER the fake keeps, not a race it usually wins: the answer of a named
/// message goes out only after the core ACKED its echo. The core here holds the echo's ack: at
/// that state no outcome is in (the fake answers in the same task right after the accept's commit,
/// so an answer that did not wait would be in before the forwarder could ship the echo), and the
/// ack lets it out.
/// Mutant: the `wait_acked` of `answer()` skipped.
#[test]
fn a_telegram_like_answer_waits_until_the_core_acked_the_echo() {
    with_flavour(
        Flavor::TelegramLike,
        "echo-acked",
        |s, _| s,
        async |run, _, direct, _| {
            let mut sending = send(
                run,
                text_message(direct, "k0000018f00000001", 1_760_000_000_123, None),
            )
            .await;
            let held = loop {
                let env = next(run, false).await;
                if env
                    .events
                    .iter()
                    .any(|e| matches!(e, BackendEvent::Message(_)))
                {
                    break env;
                }
                if let Some(ack) = env.ack {
                    ack.complete(Ok(()));
                }
            };
            assert!(
                sending.answer.try_recv().is_err(),
                "the answer came while the echo was not acked"
            );
            held.ack.expect("the echo is durable").complete(Ok(()));
            let outcome = tokio::time::timeout(Duration::from_secs(10), &mut sending.answer)
                .await
                .expect("the answer once the echo is acked")
                .expect("an answer");
            assert!(matches!(
                outcome,
                kourier_backend_api::TransmitOutcome::SentAs { .. }
            ));
        },
    );
}

/// The knob: the echo of a script with `echo_without_binding` states no `allocated_as`, and
/// states the rest as it does otherwise.
#[test]
fn a_telegram_like_echo_can_know_no_binding() {
    with_flavour(
        Flavor::TelegramLike,
        "echo-unbound",
        |s, _| s.with_echo_without_binding(),
        async |run, clock, direct, _| {
            let mut sending = send(
                run,
                text_message(direct, "k0000018f00000001", 1_760_000_000_123, None),
            )
            .await;
            sending.told(run, clock).await;
            let (assigned, server_at, _) = sent_as(sending.outcome.clone());
            let echo = the_echo(&sending.seen);
            assert_eq!(echo.allocated_as, None);
            assert_eq!(echo.local, assigned.local);
            assert_eq!(echo.net_seq, Some(assigned.seq));
            assert_eq!(echo.server_at, server_at);
            assert_eq!(echo.origin, Origin::OwnThisDevice);
        },
    );
}

/// "Outcome first": a script that holds the forwarder back (`forward_delay`) gets the answer at
/// once and the echo behind it, when the clock lets the batch through.
#[test]
fn a_held_forwarder_puts_the_outcome_before_the_echo() {
    with_flavour(
        Flavor::TelegramLike,
        "outcome-first",
        |mut s, _| {
            s.forward_delay = Some(Duration::from_secs(5));
            s
        },
        async |run, clock, direct, _| {
            let mut sending = send(
                run,
                text_message(direct, "k0000018f00000001", 1_760_000_000_123, None),
            )
            .await;
            sending.told(run, clock).await;
            assert_eq!(sending.seen, [], "the echo waits for the clock");
            let (assigned, server_at, _) = sent_as(sending.outcome.clone());
            // Let it through.
            sending.pump(run, clock, true, |s| !s.seen.is_empty()).await;
            let echo = the_echo(&sending.seen);
            assert_eq!(echo.local, assigned.local);
            assert_eq!(echo.allocated_as.as_deref(), Some("k0000018f00000001"));
            assert_eq!(echo.server_at, server_at);
        },
    );
}

/// A slow answer (`reply_delays`) keeps the echo first: the server has the message and told the
/// account, the outcome reaches the core after the delay.
#[test]
fn a_slow_answer_comes_after_the_echo() {
    with_flavour(
        Flavor::TelegramLike,
        "slow-answer",
        |s, direct| s.delay_reply(direct.clone(), Duration::from_secs(30)),
        async |run, clock, direct, _| {
            let mut sending = send(
                run,
                text_message(direct, "k0000018f00000001", 1_760_000_000_123, None),
            )
            .await;
            sending
                .pump(run, clock, false, |s| !s.seen.is_empty())
                .await;
            assert_eq!(sending.outcome, None, "the answer is held for 30 s");
            sending
                .pump(run, clock, true, |s| s.outcome.is_some())
                .await;
            let (assigned, _, _) = sent_as(sending.outcome.clone());
            assert_eq!(the_echo(&sending.seen).local, assigned.local);
        },
    );
}

/// A chat with nobody else in it has no copy for the server to remember, and still gets one echo,
/// once; and the answer is the same for a message sent again.
#[test]
fn a_telegram_like_chat_with_nobody_else_is_echoed_once() {
    with_flavour(
        Flavor::TelegramLike,
        "alone",
        |s, _| s,
        async |run, clock, _, _| {
            let me = ConvKey::Direct(uid("aci:00000000-0000-4000-8000-0000000000aa"));
            let req = text_message(&me, "k0000018f00000001", 1_760_000_000_123, None);
            let mut first = send(run, req.clone()).await;
            first.told(run, clock).await;
            let (assigned, _, to) = sent_as(first.outcome.clone());
            assert_eq!(to, []);
            assert_eq!(the_echo(&first.seen).local, assigned.local);
            let mut again = send(run, req).await;
            again.told(run, clock).await;
            again.settled(run, clock).await;
            assert_eq!(again.seen, []);
            assert_eq!(sent_as(again.outcome).0, assigned);
        },
    );
}

/// A scripted failure answers `Failed` first and commits no echo; the retry that the rule lets
/// through is the first accept, so it answers `SentAs` and tells the account once. A scripted
/// failure of one member is a `Partial` with no echo either, and its retry to that member tells
/// the account once, at the first full accept.
#[test]
fn a_telegram_like_failure_answers_failed_and_echoes_nothing() {
    use kourier_backend_api::TransmitOutcome;
    with_flavour(
        Flavor::TelegramLike,
        "failing",
        |mut s, direct| {
            s.transmit.push(
                kourier_fake::TransmitRule::attempts(
                    kourier_model::BackendError::transient("scripted"),
                    1,
                )
                .in_conv(direct.clone()),
            );
            s
        },
        async |run, clock, direct, alice| {
            let req = text_message(direct, "k0000018f00000001", 1_760_000_000_123, None);
            let mut failed = send(run, req.clone()).await;
            failed.told(run, clock).await;
            failed.settled(run, clock).await;
            assert!(
                matches!(failed.outcome, Some(TransmitOutcome::Failed(_))),
                "{:?}",
                failed.outcome
            );
            assert_eq!(failed.seen, []);

            let mut retry = send(run, req).await;
            retry.told(run, clock).await;
            let (assigned, _, to) = sent_as(retry.outcome.clone());
            assert_eq!(&to, std::slice::from_ref(alice));
            assert_eq!(the_echo(&retry.seen).local, assigned.local);
        },
    );
    // (a member of a group fails)
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = FakeClock::new(1_000_000);
    let (script, _, group, alice, bob) = chats_script("partial");
    let script = {
        let mut s = script;
        s.transmit.push(kourier_fake::TransmitRule::member(
            bob.clone(),
            kourier_model::BackendError::transient("scripted"),
            1,
        ));
        s
    };
    let fake = FakeNetwork::new(Flavor::TelegramLike, script);
    let uuid = AccountUuid::new_random();
    rt.block_on(async {
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &clock, &rt);
        let g = group.clone();
        link_until(
            &mut run,
            None,
            |e| matches!(e, BackendEvent::ConversationUpsert(p) if p.key == g),
        )
        .await;
        let req = text_message(&group, "k0000018f00000001", 1_760_000_000_123, None);
        let mut partial = send(&mut run, req.clone()).await;
        partial.told(&mut run, &clock).await;
        partial.settled(&mut run, &clock).await;
        match &partial.outcome {
            Some(TransmitOutcome::Partial { delivered, failed }) => {
                assert_eq!(delivered.len(), 1);
                assert_eq!(delivered[0].0, alice);
                assert_eq!(failed.len(), 1);
                assert_eq!(failed[0].0, bob);
            }
            other => panic!("not Partial: {other:?}"),
        }
        assert_eq!(partial.seen, [], "no echo for a message not taken whole");

        let mut rest = request(&group, "k0000018f00000001", 1_760_000_000_123, req.kind);
        rest.only = Some(vec![bob.clone()]);
        let mut retry = send(&mut run, rest).await;
        retry.told(&mut run, &clock).await;
        let (assigned, _, to) = sent_as(retry.outcome.clone());
        assert_eq!(&to, std::slice::from_ref(&bob));
        assert_eq!(the_echo(&retry.seen).local, assigned.local);
        stop(run).await;
    });
}

/// A flavour whose server names nothing answers a message with a plain `Sent` and tells the
/// account nothing more (the other flavours' backends have their own ways to echo).
/// It waits for the fake's forwarder to be empty, not for a stretch of time (R2-L5): a stray echo
/// committed beside a plain send but never told to the core holds the forwarder, which the old 150 ms
/// of real time did not see.
/// Mutants: the `SentAs` branch taken whatever the flavour; a stray echo committed after a plain send.
#[test]
fn a_plain_flavour_answers_sent_and_sends_no_echo() {
    for flavor in [
        Flavor::SignalLike,
        Flavor::WhatsAppLike,
        Flavor::ThreemaLike,
    ] {
        with_flavour(
            flavor,
            "plain",
            |s, _| s,
            async |run, clock, direct, _| {
                let mut sending = send(
                    run,
                    text_message(direct, "1760000000123", 1_760_000_000_123, None),
                )
                .await;
                sending.told(run, clock).await;
                sending.settled(run, clock).await;
                assert!(
                    matches!(
                        sending.outcome,
                        Some(kourier_backend_api::TransmitOutcome::Sent { .. })
                    ),
                    "{flavor:?}: {:?}",
                    sending.outcome
                );
                assert_eq!(sending.seen, [], "{flavor:?}");
            },
        );
    }
}

/// The Telegram-like flavour names the attachments of an own message before the send as nothing (its
/// id changes at the answer), the others as they did; a fake that PLAYS Telegram is the Signal-like
/// flavour and keeps the name.
#[test]
fn a_telegram_like_network_names_no_attachment_before_the_send() {
    let script = || Script::new("names", Vec::new());
    let telegram = FakeNetwork::new(Flavor::TelegramLike, script());
    assert_eq!(
        telegram.attachment_net_ids("k0000018f00000001", 0),
        Vec::<String>::new()
    );
    for flavor in [
        Flavor::SignalLike,
        Flavor::WhatsAppLike,
        Flavor::ThreemaLike,
    ] {
        let net = FakeNetwork::new(flavor, script());
        assert_eq!(
            net.attachment_net_ids("l", 2),
            [scripts::attachment_name("l", 2)],
            "{flavor:?}"
        );
    }
    let played = FakeNetwork::playing(kourier_model::NetworkKind::Telegram, script());
    assert_eq!(
        played.attachment_net_ids("l", 2),
        [scripts::attachment_name("l", 2)],
        "the rehearsals do not move"
    );
}

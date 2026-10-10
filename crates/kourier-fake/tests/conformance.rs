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
use kourier_model::{AccountId, AccountUuid, Body, ConvKey, LoginStepKind, Origin, UserId};
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
    assert_eq!(
        messages,
        (spec.contacts + spec.groups) * spec.messages_per_conversation
    );
    assert!(a.deliveries().iter().all(|d| d.len() <= 50));
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
}

fn spawn(
    fake: &FakeNetwork,
    dir: &std::path::Path,
    uuid: AccountUuid,
    start: StartMode,
    clock: &FakeClock,
    rt: &tokio::runtime::Runtime,
) -> Running {
    let (ctx, ends) = AccountContext::new(ContextParams {
        account: AccountId(1),
        uuid,
        start,
        backend_db: dir.join("backend.db"),
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
    Running { ends, handle }
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
        let exit = kourier_backend_api::Exit::from_channel((&mut run.handle.exited).await);
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

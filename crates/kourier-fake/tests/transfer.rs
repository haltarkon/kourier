//! The fake network's history transfer (architecture §14.2 item 14): a fresh link walks it
//! before the login's `Done` step and delivers nothing meanwhile; the state is reported at
//! every start and every connect, also an end, until it is dismissed; a skip ends a wait or
//! a download and the next event is `Skipped`; a read cannot be skipped; a restart goes on
//! from the stage it stopped at.

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, BackendEvent, Command, ContextParams, DbKey, Envelope, Network, StartMode,
    SystemClock,
};
use kourier_fake::{FakeNetwork, Flavor, Script, Step, TransferScript, scripts};
use kourier_model::{
    AccountId, AccountProgress, AccountUuid, ConnState, ConvKey, HistorySource, HistoryTransfer,
    HistoryTransferFailure, LoginStepKind, UserId,
};
use tokio_util::sync::CancellationToken;

const HOLD: Duration = Duration::from_millis(40);

struct Running {
    ends: kourier_backend_api::CoreEnds,
    handle: kourier_backend_api::AccountHandle,
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

fn spawn(
    fake: &FakeNetwork,
    dir: &std::path::Path,
    uuid: AccountUuid,
    start: StartMode,
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
        clock: Arc::new(SystemClock),
        config: Default::default(),
        ui: Default::default(),
        local_prefs: Default::default(),
    });
    let handle = fake.spawn(ctx).expect("spawn");
    Running { ends, handle }
}

async fn stop(mut run: Running) {
    let _ = run.ends.commands.send(Command::Shutdown).await;
    let _ = run.handle.exited.await;
    if let Some(t) = run.handle.thread.take() {
        t.join().expect("join");
    }
}

/// What a UI and a core see of the actor, in the order it came: transfer states, the
/// login's `Done`, messages (by their id). Durable envelopes are acked.
#[derive(Clone, Debug, PartialEq)]
enum Seen {
    Transfer(HistoryTransfer),
    Progress(AccountProgress),
    LoginDone,
    Message(String),
}

async fn next(run: &mut Running) -> Vec<Seen> {
    let env: Envelope = tokio::time::timeout(Duration::from_secs(10), run.ends.events.recv())
        .await
        .expect("timely")
        .expect("open");
    if let Some(a) = env.ack {
        a.complete(Ok(()));
    }
    env.events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::HistoryTransfer(s) => Some(Seen::Transfer(*s)),
            BackendEvent::AccountProgress(p) => Some(Seen::Progress(*p)),
            BackendEvent::Login(s) if s.kind == LoginStepKind::Done => Some(Seen::LoginDone),
            BackendEvent::Message(m) => Some(Seen::Message(m.local.to_string())),
            _ => None,
        })
        .collect()
}

/// Reads until `f` holds for something seen; returns everything seen on the way.
async fn until(run: &mut Running, mut f: impl FnMut(&Seen) -> bool) -> Vec<Seen> {
    let mut all = Vec::new();
    loop {
        let seen = next(run).await;
        let hit = seen.iter().any(&mut f);
        all.extend(seen);
        if hit {
            return all;
        }
    }
}

async fn send(run: &Running, command: Command) {
    run.ends.commands.send(command).await.expect("send");
}

async fn link(run: &Running) {
    send(
        run,
        Command::StartLogin {
            flow: "qr".into(),
            relink: None,
            taken: Vec::new(),
        },
    )
    .await;
}

/// Everything the actor has sent by the time it answered a command sent after `commands`:
/// the answer is the barrier, so what is not here was not said.
async fn settled(run: &mut Running) -> Vec<Seen> {
    let (reply, answer) = tokio::sync::oneshot::channel();
    send(run, Command::ListOwnDevices { reply }).await;
    tokio::time::timeout(Duration::from_secs(10), answer)
        .await
        .expect("timely")
        .expect("answered")
        .expect("devices");
    let mut all = Vec::new();
    while let Ok(env) = run.ends.events.try_recv() {
        if let Some(a) = env.ack {
            a.complete(Ok(()));
        }
        all.extend(env.events.iter().filter_map(|e| match e {
            BackendEvent::HistoryTransfer(s) => Some(Seen::Transfer(*s)),
            _ => None,
        }));
    }
    all
}

/// Waits until the account is online: its start, with what a start says, is over.
async fn online(run: &mut Running) {
    tokio::time::timeout(
        Duration::from_secs(10),
        run.ends
            .conn
            .wait_for(|c| matches!(c, ConnState::Online { .. })),
    )
    .await
    .expect("timely")
    .expect("online");
}

fn me() -> UserId {
    UserId::parse("aci:00000000-0000-4000-8000-0000000000aa").expect("id")
}

/// A script with one message to deliver, and `transfer` before it.
fn script(transfer: TransferScript) -> Script {
    let alice = UserId::parse("aci:00000000-0000-4000-8000-0000000a11ce").expect("id");
    let chat = ConvKey::Direct(alice.clone());
    Script::new("transfer", vec![me()])
        .deliver(vec![BackendEvent::Message(scripts::remote(
            &chat, &alice, "1", 1, "one",
        ))])
        .step(Step::WaitAcked)
        .with_transfer(transfer)
}

fn transfers(seen: &[Seen]) -> Vec<HistoryTransfer> {
    seen.iter()
        .filter_map(|s| match s {
            Seen::Transfer(t) => Some(*t),
            _ => None,
        })
        .collect()
}

#[test]
fn a_fresh_link_walks_the_transfer_before_done_and_delivers_nothing_meanwhile() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let walk = TransferScript::walk(HOLD, HistoryTransfer::Done);
    let stages: Vec<HistoryTransfer> = walk.stages.iter().map(|s| s.0).collect();
    let fake = FakeNetwork::new(Flavor::SignalLike, script(walk));
    let uuid = AccountUuid::new_random();
    rt.block_on(async {
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &rt);
        link(&run).await;
        let seen = until(&mut run, |s| matches!(s, Seen::Message(_))).await;
        let mut expected: Vec<Seen> = stages.iter().map(|s| Seen::Transfer(*s)).collect();
        // The read counts the messages as the delivery does, a quarter at a time.
        let total = TransferScript::MESSAGES;
        expected.extend((0..4).map(|k| {
            Seen::Progress(AccountProgress {
                messages_done: total * k / 4,
                messages_total: total,
                ..AccountProgress::default()
            })
        }));
        expected.push(Seen::Transfer(HistoryTransfer::Done));
        expected.push(Seen::Progress(AccountProgress::finished()));
        expected.push(Seen::LoginDone);
        expected.push(Seen::Message("1".into()));
        assert_eq!(
            seen, expected,
            "the states in order, then the login's Done, then the first delivery"
        );
        assert!(matches!(stages[0], HistoryTransfer::Waiting));
        assert!(
            matches!(
                stages[1],
                HistoryTransfer::Downloading {
                    done: 0,
                    total: None
                }
            ),
            "a download starts before its size is known"
        );
        assert!(matches!(stages.last(), Some(HistoryTransfer::Reading)));
        assert!(fake.control().violations().is_empty());
        stop(run).await;
    });
}

#[test]
fn the_account_reports_syncing_while_its_transfer_runs() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeNetwork::new(
        Flavor::SignalLike,
        script(TransferScript::holding(HistoryTransfer::Waiting)),
    );
    rt.block_on(async {
        let mut run = spawn(
            &fake,
            dir.path(),
            AccountUuid::new_random(),
            StartMode::Fresh,
            &rt,
        );
        link(&run).await;
        until(&mut run, |s| *s == Seen::Transfer(HistoryTransfer::Waiting)).await;
        assert_eq!(
            *run.ends.conn.borrow(),
            ConnState::Syncing { fraction: None }
        );
        send(&run, Command::SkipHistoryTransfer).await;
        until(&mut run, |s| matches!(s, Seen::Message(_))).await;
        tokio::time::timeout(
            Duration::from_secs(10),
            run.ends
                .conn
                .wait_for(|c| matches!(c, ConnState::Online { .. })),
        )
        .await
        .expect("timely")
        .expect("online once the transfer is over");
        stop(run).await;
    });
}

#[test]
fn a_skip_ends_a_wait_and_a_download_as_skipped_and_the_link_finishes() {
    for state in [
        HistoryTransfer::Waiting,
        HistoryTransfer::Downloading {
            done: 12_300_000,
            total: Some(TransferScript::TOTAL),
        },
    ] {
        let rt = runtime();
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = FakeNetwork::new(Flavor::SignalLike, script(TransferScript::holding(state)));
        let uuid = AccountUuid::new_random();
        rt.block_on(async {
            let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &rt);
            link(&run).await;
            let seen = until(&mut run, |s| *s == Seen::Transfer(state)).await;
            assert_eq!(seen, [Seen::Transfer(state)], "nothing before it");
            // It stays there: a dismissal does nothing while it runs, a connect repeats it.
            send(&run, Command::DismissHistoryTransfer).await;
            send(&run, Command::Connect).await;
            assert_eq!(transfers(&settled(&mut run).await), [state]);
            // The skip: the next event is the end, then the link finishes.
            send(&run, Command::SkipHistoryTransfer).await;
            let seen = until(&mut run, |s| matches!(s, Seen::Message(_))).await;
            assert_eq!(
                seen,
                [
                    Seen::Transfer(HistoryTransfer::Skipped),
                    Seen::LoginDone,
                    Seen::Message("1".into())
                ]
            );
            // A second skip has nothing to end.
            send(&run, Command::SkipHistoryTransfer).await;
            assert!(settled(&mut run).await.is_empty());
            assert_eq!(
                fake.control()
                    .commands(uuid)
                    .iter()
                    .filter(|c| **c == "SkipHistoryTransfer")
                    .count(),
                2
            );
            stop(run).await;
        });
    }
}

#[test]
fn a_read_cannot_be_skipped() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeNetwork::new(
        Flavor::SignalLike,
        script(TransferScript::holding(HistoryTransfer::Reading)),
    );
    rt.block_on(async {
        let mut run = spawn(
            &fake,
            dir.path(),
            AccountUuid::new_random(),
            StartMode::Fresh,
            &rt,
        );
        link(&run).await;
        until(&mut run, |s| *s == Seen::Transfer(HistoryTransfer::Reading)).await;
        send(&run, Command::SkipHistoryTransfer).await;
        assert!(settled(&mut run).await.is_empty(), "no end follows");
        send(&run, Command::Connect).await;
        assert_eq!(
            transfers(&settled(&mut run).await),
            [HistoryTransfer::Reading],
            "it still reads"
        );
        stop(run).await;
    });
}

#[test]
fn a_skip_during_a_download_is_not_lost_to_the_read() {
    // The download is over a moment after the skip was sent: the transfer ends as skipped
    // and never starts to read.
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let downloading = HistoryTransfer::Downloading {
        done: 1,
        total: Some(2),
    };
    let transfer = TransferScript {
        stages: vec![
            (downloading, Some(Duration::from_millis(150))),
            (HistoryTransfer::Reading, None),
        ],
        end: HistoryTransfer::Done,
        import: None,
    };
    let fake = FakeNetwork::new(Flavor::SignalLike, script(transfer));
    rt.block_on(async {
        let mut run = spawn(
            &fake,
            dir.path(),
            AccountUuid::new_random(),
            StartMode::Fresh,
            &rt,
        );
        link(&run).await;
        until(&mut run, |s| *s == Seen::Transfer(downloading)).await;
        send(&run, Command::SkipHistoryTransfer).await;
        let seen = until(&mut run, |s| *s == Seen::LoginDone).await;
        assert_eq!(
            seen,
            [Seen::Transfer(HistoryTransfer::Skipped), Seen::LoginDone]
        );
        stop(run).await;
    });
}

#[test]
fn every_end_is_reported_at_each_connect_and_start_until_it_is_dismissed() {
    use HistoryTransferFailure::{Corrupt, Download, Internal, LinkAgain, TooLarge, UpdateApp};
    let mut ends = vec![HistoryTransfer::Done, HistoryTransfer::Skipped];
    ends.extend(
        [LinkAgain, UpdateApp, Corrupt, TooLarge, Download, Internal].map(HistoryTransfer::Failed),
    );
    for end in ends {
        let rt = runtime();
        let dir = tempfile::tempdir().expect("tempdir");
        let transfer = match end {
            HistoryTransfer::Failed(kind) => TransferScript::failing(HOLD / 4, kind),
            end => TransferScript::ending(HOLD / 4, end),
        };
        let fake = FakeNetwork::new(Flavor::SignalLike, script(transfer));
        let uuid = AccountUuid::new_random();
        rt.block_on(async {
            let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &rt);
            link(&run).await;
            // Whatever the end, the link finishes and the account receives.
            let seen = until(&mut run, |s| matches!(s, Seen::Message(_))).await;
            assert_eq!(transfers(&seen).last(), Some(&end), "{end:?}");
            assert!(seen.contains(&Seen::LoginDone), "{end:?}");
            // Said again at a connect, and a skip changes nothing about an end.
            send(&run, Command::SkipHistoryTransfer).await;
            send(&run, Command::Connect).await;
            send(&run, Command::Connect).await;
            assert_eq!(transfers(&settled(&mut run).await), [end, end], "{end:?}");
            stop(run).await;

            // Said again at the next start, and at its connect.
            let mut run = spawn(&fake, dir.path(), uuid, StartMode::Resume, &rt);
            // (Only the transfer's states are compared: a message whose acknowledgement
            // the stop overtook is delivered again at this start, before or after.)
            assert_eq!(
                transfers(&until(&mut run, |s| matches!(s, Seen::Transfer(_))).await),
                [end],
                "{end:?}: at the start"
            );
            send(&run, Command::Connect).await;
            online(&mut run).await;
            assert_eq!(transfers(&settled(&mut run).await), [end], "{end:?}");
            // Until the user has seen it.
            send(&run, Command::DismissHistoryTransfer).await;
            send(&run, Command::Connect).await;
            assert!(settled(&mut run).await.is_empty(), "{end:?}: dismissed");
            stop(run).await;

            let mut run = spawn(&fake, dir.path(), uuid, StartMode::Resume, &rt);
            send(&run, Command::Connect).await;
            // (Online: the start is behind it, with whatever it had to say.)
            online(&mut run).await;
            assert!(settled(&mut run).await.is_empty(), "{end:?}: for good");
            stop(run).await;
        });
    }
}

#[test]
fn a_restart_in_the_middle_goes_on_from_its_stage() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let part = |done| HistoryTransfer::Downloading {
        done,
        total: Some(40),
    };
    let transfer = TransferScript {
        stages: vec![
            (HistoryTransfer::Waiting, Some(HOLD)),
            // It stops here: the actor is shut down while this stage is held.
            (part(30), Some(Duration::from_millis(400))),
            (part(40), Some(HOLD)),
            (HistoryTransfer::Reading, Some(HOLD)),
        ],
        end: HistoryTransfer::Done,
        import: None,
    };
    let fake = FakeNetwork::new(Flavor::SignalLike, script(transfer));
    let uuid = AccountUuid::new_random();
    rt.block_on(async {
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &rt);
        link(&run).await;
        until(&mut run, |s| *s == Seen::Transfer(part(30))).await;
        stop(run).await;

        // The start says where it stands; the walk goes on once the actor may connect, and
        // only then the script delivers.
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Resume, &rt);
        assert_eq!(
            until(&mut run, |s| matches!(s, Seen::Transfer(_))).await,
            [Seen::Transfer(part(30))]
        );
        send(&run, Command::Connect).await;
        let seen = until(&mut run, |s| matches!(s, Seen::Message(_))).await;
        let mut states = transfers(&seen);
        // (The connect and the walk each say the stage they found.)
        states.dedup();
        assert_eq!(
            states,
            [
                part(30),
                part(40),
                HistoryTransfer::Reading,
                HistoryTransfer::Done
            ]
        );
        assert_eq!(seen.last(), Some(&Seen::Message("1".into())));
        assert!(
            !seen.contains(&Seen::LoginDone),
            "no login runs after a restart"
        );
        stop(run).await;
    });
}

#[test]
fn a_script_can_leave_an_account_in_the_middle_of_a_transfer() {
    // What fixture mode shows as "the application was restarted during the download".
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let state = HistoryTransfer::Downloading {
        done: 12_300_000,
        total: Some(45_600_000),
    };
    let script = Script::new("left", vec![me()]).step(Step::Transfer(state));
    let fake = FakeNetwork::new(Flavor::SignalLike, script);
    let uuid = AccountUuid::new_random();
    rt.block_on(async {
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, &rt);
        link(&run).await;
        let seen = until(&mut run, |s| matches!(s, Seen::Transfer(_))).await;
        assert_eq!(seen, [Seen::LoginDone, Seen::Transfer(state)]);
        // The script ends there (once the core has taken everything).
        for _ in 0..500 {
            if fake.control().is_finished(uuid) {
                break;
            }
            settled(&mut run).await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(fake.control().is_finished(uuid));
        send(&run, Command::Connect).await;
        assert_eq!(transfers(&settled(&mut run).await), [state]);
        send(&run, Command::SkipHistoryTransfer).await;
        assert_eq!(
            transfers(&settled(&mut run).await),
            [HistoryTransfer::Skipped]
        );
        send(&run, Command::DismissHistoryTransfer).await;
        send(&run, Command::Connect).await;
        assert!(settled(&mut run).await.is_empty());
        stop(run).await;
    });
}

#[test]
fn a_relink_has_no_transfer_and_the_descriptor_can_promise_history() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeNetwork::new(
        Flavor::SignalLike,
        script(TransferScript::holding(HistoryTransfer::Waiting)),
    );
    assert_eq!(fake.descriptor().history, HistorySource::None);
    let promised = fake.clone().with_history(HistorySource::PushedOnLink);
    assert_eq!(promised.descriptor().history, HistorySource::PushedOnLink);
    rt.block_on(async {
        let mut run = spawn(
            &fake,
            dir.path(),
            AccountUuid::new_random(),
            StartMode::Fresh,
            &rt,
        );
        // "Link Again" of an account that kept its chats asks no phone for its history.
        send(
            &run,
            Command::StartLogin {
                flow: "qr".into(),
                relink: Some(kourier_backend_api::Relink {
                    account: AccountId(1),
                    expect_own: vec![me()],
                }),
                taken: Vec::new(),
            },
        )
        .await;
        let seen = until(&mut run, |s| *s == Seen::LoginDone).await;
        assert!(transfers(&seen).is_empty());
        send(&run, Command::Connect).await;
        assert!(settled(&mut run).await.is_empty());
        stop(run).await;
    });
}

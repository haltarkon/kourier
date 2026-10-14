//! The start-up wiring of the waits for the other device's answer (S6): what the core does
//! with a wait that ran out while Kourier was closed, before and while the account's actor
//! starts. Each test pins one line of wiring that no test of the functions behind it sees
//! (`restore::run` calls `end_orphans`; `Core::start` marks the accounts as starting before the
//! first sweep; the supervisor tells the gate when an actor is spawned and when it is online),
//! and fails when that line is taken out.
//!
//! All of them run a real core on `core.db` files left by an earlier run of the same core:
//! an account whose request for a lost file went out and got no answer, closed, and started
//! again an hour later.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_core::facade::UiCommand;
use kourier_fake::attachments::expired_attachment;
use kourier_fake::scripts::{T0, remote};
use kourier_fake::{BackfillScript, Flavor, Script};
use kourier_model::{AskFailure, AttachmentId, AttachmentKind, AttachmentState, Body, ConnState};
use support::judge::ClockJudge;
use support::send::{ME, ME_PNI, alice, d_alice, setup, uid};
use support::{CORE_KEY, Env, WAIT, add_linked, eventually};
use tokio::sync::watch;

/// Alice's message with one file the CDN no longer has, on an account whose phone never
/// answers.
fn script() -> Script {
    let mut m = remote(&d_alice(), &alice(), &(T0 + 1).to_string(), T0 + 1, "");
    m.body = Body::Media {
        attachments: vec![expired_attachment(AttachmentKind::File)],
        caption: None,
    };
    Script::new("restore", vec![uid(ME), uid(ME_PNI)])
        .deliver(setup())
        .deliver(vec![BackendEvent::Message(m)])
        .with_backfill(BackfillScript::Silent)
}

fn state(env: &Env) -> (AttachmentId, AttachmentState) {
    env.peek(|c| {
        c.query_row(
            "SELECT id, state FROM attachments ORDER BY id LIMIT 1",
            [],
            |r| {
                let state: String = r.get(1)?;
                Ok((
                    AttachmentId(r.get(0)?),
                    serde_json::from_str(&state).expect("state json"),
                ))
            },
        )
        .expect("the attachment")
    })
}

fn no_answer() -> AttachmentState {
    AttachmentState::AskFailed {
        why: AskFailure::NoAnswer,
    }
}

/// A linked account, its lost file asked for and the request reported sent (its wait runs
/// 30 s on the core clock), the core stopped. The clock then moves an hour: the wait is due
/// at the next start.
async fn closed_with_a_wait() -> Env {
    let mut env = Env::new(Flavor::SignalLike, script()).await;
    let account = add_linked(&mut env).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    env.wait_finished(account).await;
    eventually("the automatic download finds the file gone", WAIT, || {
        let gone = state(&env).1 == AttachmentState::Expired;
        async move { gone }
    })
    .await;
    env.handle
        .send(UiCommand::DownloadAttachment {
            attachment: state(&env).0,
        })
        .expect("send");
    eventually("the wait starts", WAIT, || {
        let armed: bool = env.peek(|c| {
            c.query_row(
                "SELECT count(*) FROM attachment_requests WHERE sent_at IS NOT NULL",
                [],
                |r| r.get::<_, i64>(0),
            )
            .expect("count")
                == 1
        });
        async move { armed }
    })
    .await;
    assert_eq!(state(&env).1, AttachmentState::Asking);
    env.stop().await;
    env.clock.advance(Duration::from_secs(3600));
    env
}

/// Starts the core again with a backend of the test's: it shows `first` once it is told to
/// connect (`UiReady`), then what is sent through the returned sender.
async fn start_judged(env: &mut Env, first: ConnState) -> watch::Sender<ConnState> {
    let judge = ClockJudge::new(env.fake.clone());
    let verdict = judge.judge_next(first);
    let core = judge
        .start(env.dir.path(), &env.vault, Arc::new(env.clock.clone()))
        .await;
    env.handle = core.handle();
    env.events = env.handle.subscribe();
    env.core = Some(core);
    verdict
}

/// Real time the tests give the core to do what it must NOT do (the task that judges the
/// waits looks at once at a start and answers a moved clock within a few milliseconds).
const QUIET: Duration = Duration::from_millis(600);

async fn waits_for_the_end(env: &Env, what: &str) {
    eventually(what, Duration::from_secs(5), || {
        let ended = state(env).1 == no_answer();
        async move { ended }
    })
    .await;
}

/// `restore::run` ends what says it is being asked for without a request before anything
/// else, at every start: an attachment left `Asking` with no request row (an earlier build, a
/// backend that delivered it so) is clickable again after the next start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_ends_an_ask_that_has_no_request() {
    let mut env = closed_with_a_wait().await;
    {
        // The request row is gone, the attachment says it is being asked for.
        let conn = kourier_db::open(&env.core_db(), &kourier_db::DbKey::from(CORE_KEY))
            .expect("open core.db");
        conn.execute("DELETE FROM attachment_requests", [])
            .expect("delete");
        conn.execute("DELETE FROM pending_commands", [])
            .expect("delete");
    }
    assert_eq!(state(&env).1, AttachmentState::Asking);
    env.restart().await;
    waits_for_the_end(&env, "the orphan ask ends at the start").await;
    env.stop().await;
}

/// The first look of the task comes at once, before the supervisor of the account has so
/// much as unwrapped its key: `Core::start` marks the accounts that will get an actor as
/// starting before the task is spawned, or the wait that ran out while Kourier was closed
/// would end as "did not answer" before the account's backend could replay the answer it kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_look_leaves_the_waits_of_an_account_whose_actor_is_not_there_yet() {
    let mut env = closed_with_a_wait().await;
    env.vault.hold_unwraps();
    env.restart().await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        state(&env).1,
        AttachmentState::Asking,
        "the wait ended before the account's actor started"
    );
    // The actor starts, the fake goes online: the wait is judged then, not held for good.
    env.vault.release_unwraps();
    env.handle.send(UiCommand::UiReady).expect("send");
    waits_for_the_end(&env, "the wait is judged once the account is online").await;
    env.stop().await;
}

/// The supervisor tells the gate when it spawns the actor: the hold lasts from THAT moment,
/// not from the core's start (the key may take a while). The core clock is 9 s past the
/// start (the grace is 10 s) when the actor is spawned: 5 s later the wait is still held, and
/// it ends 10 s after the spawn with the account never online.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_hold_lasts_from_the_spawn_of_the_actor() {
    let mut env = closed_with_a_wait().await;
    env.vault.hold_unwraps();
    let _offline = start_judged(&mut env, ConnState::Offline { retry_at_ms: None }).await;
    env.clock.advance(Duration::from_secs(9));
    tokio::time::sleep(QUIET).await;
    assert_eq!(state(&env).1, AttachmentState::Asking, "inside the grace");
    // The actor is spawned now, nine seconds after the start.
    env.vault.release_unwraps();
    tokio::time::sleep(QUIET).await;
    env.clock.advance(Duration::from_secs(5));
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        state(&env).1,
        AttachmentState::Asking,
        "five seconds after the spawn the wait is still held (ten after the start it would not be)"
    );
    env.clock.advance(Duration::from_secs(6));
    waits_for_the_end(&env, "the wait ends ten seconds after the spawn").await;
    env.stop().await;
}

/// The supervisor tells the gate when the account is online: its backend caught up, so the
/// replay is over and the wait is judged without waiting out the grace (the clock does not
/// move here: only the connection state ends the hold).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_account_that_is_online_is_judged_at_once() {
    let mut env = closed_with_a_wait().await;
    let verdict = start_judged(&mut env, ConnState::Connecting).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        state(&env).1,
        AttachmentState::Asking,
        "connecting: still held"
    );
    verdict
        .send(ConnState::Online { detail: None })
        .expect("online");
    waits_for_the_end(&env, "the wait ends when the account is online").await;
    env.stop().await;
}

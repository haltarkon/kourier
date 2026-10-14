//! A removed account's background work stops with it (link feedback L4, `buttons.md` F6):
//! thumbnails of pictures downloaded just before "Remove Account…" are not derived from files
//! that are being deleted, not recorded for rows that are gone, and nothing is written into the
//! account's directory after it was removed.
//!
//! One test in a binary of its own: the warnings are collected by a `tracing` subscriber that is
//! installed for the whole process.

mod support;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_core::facade::UiCommand;
use kourier_fake::attachments::synthetic_image;
use kourier_fake::scripts::{T0, remote};
use kourier_fake::{Flavor, Script};
use kourier_model::Body;
use support::send::{ME, ME_PNI, alice, d_alice, setup, uid};
use support::{Env, WAIT, add_linked, eventually};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

/// `(level, message)` of every event of the process.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<(tracing::Level, String)>>>);

struct Message<'a>(&'a mut String);

impl Visit for Message<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            *self.0 = format!("{value:?}");
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for Log {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        let mut text = String::new();
        event.record(&mut Message(&mut text));
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((*event.metadata().level(), text));
    }
}

impl Log {
    fn lines(&self) -> Vec<(tracing::Level, String)> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn count(&self, needle: &str) -> usize {
        self.lines()
            .iter()
            .filter(|(_, m)| m.contains(needle))
            .count()
    }
}

/// Pictures that take a while to thumbnail: two decode at a time, the rest wait for a permit.
const PICTURES: i64 = 30;

fn script() -> Script {
    let pictures = (1..=PICTURES)
        .map(|i| {
            let mut m = remote(&d_alice(), &alice(), &(T0 + i).to_string(), T0 + i, "");
            m.body = Body::Media {
                attachments: vec![synthetic_image(1200, 800, u64::try_from(i).unwrap_or(1))],
                caption: None,
            };
            BackendEvent::Message(m)
        })
        .collect();
    Script::new("remove-derive", vec![uid(ME), uid(ME_PNI)])
        .deliver(setup())
        .deliver(pictures)
}

fn count(env: &Env, filter: &str) -> i64 {
    env.peek(|c| {
        c.query_row(
            &format!("SELECT count(*) FROM attachments WHERE {filter}"),
            [],
            |r| r.get(0),
        )
        .unwrap_or(0)
    })
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

fn derived(env: &Env) -> i64 {
    count(env, "derived_at IS NOT NULL")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removed_account_leaves_no_attachment_work_running() {
    let log = Log::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(log.clone()))
        .expect("the only subscriber of this process");

    let mut env = Env::new(Flavor::SignalLike, script()).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("row").uuid;
    let dir = env.account_dir(uuid);
    // Every picture is downloaded (the network has delivered all it had, so its actor answers
    // the logout at once) and the first thumbnail is recorded: the rest are being derived.
    let deadline = tokio::time::Instant::now() + WAIT;
    while count(&env, "file_id IS NOT NULL") < PICTURES || derived(&env) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "rows {}, files {}, thumbnails {}",
            count(&env, "1"),
            count(&env, "file_id IS NOT NULL"),
            derived(&env)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    env.handle
        .send(UiCommand::RemoveAccount { account })
        .expect("send");
    // Polled, not awaited as an event: 30 pictures' events can overrun the test's receiver.
    let deadline = tokio::time::Instant::now() + WAIT;
    while !env.accounts().await.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the account was not removed: {:?}",
            log.lines().iter().rev().take(40).collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        derived(&env) < PICTURES,
        "the removal came after every thumbnail: the test proves nothing"
    );
    // The derive tasks see the removal at their next permit or commit.
    let l = log.clone();
    eventually("the thumbnails stop", WAIT, || {
        let l = l.clone();
        async move { l.count("its thumbnails stop") > 0 }
    })
    .await;
    // Long enough for every picture left to have been decoded twice over.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !dir.exists(),
        "something wrote into the removed account's directory: {:?}",
        walk(&dir)
    );
    let warned: Vec<_> = log
        .lines()
        .into_iter()
        .filter(|(level, _)| *level <= tracing::Level::WARN)
        .collect();
    assert!(warned.is_empty(), "warnings after the removal: {warned:?}");
    assert_eq!(derived(&env), 0, "no attachment row is left");
    // The tasks that stopped early let go of their jobs: nothing is counted as deriving.
    assert_eq!(
        env.handle.media().deriving(),
        0,
        "stopped derive tasks still count"
    );
    env.stop().await;
}

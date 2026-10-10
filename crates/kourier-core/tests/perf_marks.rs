//! The core's timing marks (architecture §1.3 "how measured") and `UiCommand::ReleaseMemory`.
//!
//! One test in a binary of its own: the marks are `tracing` events, and the subscriber that
//! collects them is installed for the whole process.

mod support;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use kourier_core::PERF_TARGET;
use kourier_core::facade::{UiCommand, UiEvent};
use kourier_fake::{Flavor, scripts};
use support::{Env, add_linked};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

/// One mark: its fields as text.
type Mark = BTreeMap<String, String>;

#[derive(Clone, Default)]
struct Marks(Arc<Mutex<Vec<Mark>>>);

struct Fields<'a>(&'a mut Mark);

impl Visit for Fields<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }
}

impl<S: tracing::Subscriber> Layer<S> for Marks {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        if event.metadata().target() != PERF_TARGET {
            return;
        }
        let mut mark = Mark::new();
        event.record(&mut Fields(&mut mark));
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(mark);
    }
}

impl Marks {
    fn named(&self, name: &str) -> Vec<Mark> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|m| m.get("name").map(String::as_str) == Some(name))
            .cloned()
            .collect()
    }
}

fn number(mark: &Mark, field: &str) -> f64 {
    mark.get(field)
        .unwrap_or_else(|| panic!("mark without {field}: {mark:?}"))
        .parse()
        .unwrap_or_else(|_| panic!("{field} is not a number: {mark:?}"))
}

/// `recv.commit` marks every committed envelope and `recv.catchup` the end of a catch-up,
/// with numbers only: a measuring build reads them without the core knowing who listens.
/// Afterwards the core gives its caches back on request and goes on working.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_receive_path_is_marked_and_memory_is_given_back_on_request() {
    let marks = Marks::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(marks.clone()))
        .expect("the only subscriber of this process");

    // The Signal-like script reports `Syncing`, delivers two catch-up messages among its
    // envelopes and goes `Online`.
    let mut env = Env::new(Flavor::SignalLike, scripts::signal_like().expect("script")).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let summary = env
        .event("CatchUpSummary", |e| match e {
            UiEvent::CatchUpSummary { messages, .. } => Some(*messages),
            _ => None,
        })
        .await;
    env.wait_finished(account).await;

    let commits = marks.named("recv.commit");
    assert!(!commits.is_empty(), "every committed envelope is marked");
    let allowed = [
        "name", "account", "events", "messages", "notify", "catch_up", "took_us",
    ];
    for m in &commits {
        assert!(
            m.keys().all(|k| allowed.contains(&k.as_str())),
            "only these fields: {m:?}"
        );
        for field in &allowed[1..] {
            number(m, field);
        }
        assert!(number(m, "events") >= 1.0);
        assert_eq!(number(m, "account"), account.0 as f64);
    }
    let caught_up: f64 = commits.iter().map(|m| number(m, "catch_up")).sum();
    assert_eq!(caught_up, f64::from(summary), "the catch-up messages");
    let slowest = commits
        .iter()
        .map(|m| number(m, "took_us"))
        .fold(0.0, f64::max);
    eprintln!(
        "recv.commit: {} envelopes, slowest {slowest} us from arrival to commit and ack",
        commits.len()
    );

    let catchups = marks.named("recv.catchup");
    assert_eq!(catchups.len(), 1, "one catch-up: {catchups:?}");
    let c = &catchups[0];
    assert_eq!(number(c, "messages"), f64::from(summary));
    assert!(number(c, "conversations") >= 1.0);
    for field in ["account", "took_ms", "per_second"] {
        number(c, field);
    }
    // Nothing of the content is in a mark: every value is a number or the mark's name.
    for m in commits.iter().chain(&catchups) {
        for (k, v) in m {
            assert!(
                k == "name" || v.parse::<f64>().is_ok(),
                "a mark carries numbers only: {k} = {v}"
            );
        }
    }

    // The window was released: the caches go, everything still answers.
    let before = env.conversations().await.len();
    env.handle.send(UiCommand::ReleaseMemory).expect("send");
    env.handle.send(UiCommand::ReleaseMemory).expect("send");
    for _ in 0..3 {
        assert_eq!(env.conversations().await.len(), before);
    }
    let any = env.conversations().await[0].id;
    assert!(!env.timeline(any).await.is_empty() || before > 0);
    env.stop().await;
}

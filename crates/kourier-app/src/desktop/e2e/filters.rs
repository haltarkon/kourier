//! RegExp filters on the notification path (R4), end to end: the real core over a scripted
//! `FakeNetwork`, the desktop task and a mock notification server on a private bus. Filters are
//! saved through the core, so the desktop task reads the core's one compiled set.
//!
//! The names and patterns are unique markers (`qzv…`): the log of the run is scanned for them.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_core::facade::{FilterSpec, Query, QueryData, UiCommand};
use kourier_fake::scripts::{remote, signal_features, target};
use kourier_fake::{Script, Step};
use kourier_model::{Body, ConvKey, ConversationPatch, Patch, Redacted, UserId};

use super::{Env, WAIT, contact, direct, msg, sleep, start_with, ts, uid};
use crate::prefs::NotificationContent;

const INVOICES: &str = "Qzv Invoices Filter";
const WATCH: &str = "Qzv Muted Watch";

fn spec(name: &str, text: &str, notify: bool, muted: bool) -> FilterSpec {
    FilterSpec {
        name: Redacted::new(name.to_owned()),
        text_pattern: Some(Redacted::new(text.to_owned())),
        notify,
        notify_muted: muted,
        highlight: true,
        colour: 0xFFD400,
        ..Default::default()
    }
}

/// Saves the filters and waits until the core lists them and the desktop task has read the set
/// (it does so on `FiltersChanged`; no event says when).
async fn save(env: &Env, specs: Vec<FilterSpec>) {
    let n = specs.len();
    for s in specs {
        env.handle
            .send(UiCommand::SaveFilter { id: None, spec: s })
            .expect("send");
    }
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Ok(QueryData::Filters(l)) = env.handle.query(Query::Filters).await
            && l.filters.len() == n
        {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "filters not saved");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
}

fn carol() -> UserId {
    uid("aci:00000000-0000-4000-8000-00000000c0ca")
}

fn dave() -> UserId {
    uid("aci:00000000-0000-4000-8000-00000000da7e")
}

fn group() -> ConvKey {
    ConvKey::Group("gv2:ZmlsdGVycy1ncm91cA".into())
}

fn muted_direct(key: &ConvKey) -> BackendEvent {
    let mut p = ConversationPatch::new(key.clone());
    p.features = Some(signal_features(false));
    p.muted_until = Patch::Set(i64::MAX);
    BackendEvent::ConversationUpsert(p)
}

fn live_script(me: &UserId, alice: &UserId, bob: &UserId) -> Script {
    let (d_alice, d_bob) = (ConvKey::Direct(alice.clone()), ConvKey::Direct(bob.clone()));
    let (d_carol, d_dave) = (ConvKey::Direct(carol()), ConvKey::Direct(dave()));
    let mut protect = ConversationPatch::new(d_carol.clone());
    protect.features = Some(signal_features(false));
    protect.protected = Some(true);
    let mut block = ConversationPatch::new(d_dave.clone());
    block.features = Some(signal_features(false));
    block.blocked = Some(true);
    let mut g = ConversationPatch::new(group());
    g.features = Some(signal_features(true));
    g.title = Patch::Set("Filter Group".into());
    g.muted_until = Patch::Set(i64::MAX);
    let mut mention = remote(&group(), bob, &ts(31).to_string(), ts(31), "hey you");
    mention.mentions_me = true;
    Script::new("desktop-filters", vec![me.clone()])
        .step(Step::Conn(kourier_model::ConnState::Syncing {
            fraction: None,
        }))
        .deliver(vec![
            contact(alice, "Alice Example"),
            contact(bob, "Bob Example"),
            contact(&carol(), "Carol Example"),
            contact(&dave(), "Dave Example"),
            direct(&d_alice),
            muted_direct(&d_bob),
            BackendEvent::ConversationUpsert(g),
            BackendEvent::ConversationUpsert(protect),
            BackendEvent::ConversationUpsert(block),
        ])
        .step(Step::CatchUpDone)
        .step(sleep()) // 0: the filters are saved
        .deliver(vec![msg(&d_alice, alice, 10, "Your qzvinvoice is ready")])
        .step(sleep()) // 1
        .deliver(vec![msg(&d_alice, alice, 11, "plain hello")])
        .step(sleep()) // 2
        .deliver(vec![
            msg(&d_bob, bob, 20, "a qzvinvoice please"),
            msg(&d_bob, bob, 21, "qzvwire done"),
        ])
        .step(sleep()) // 3
        .deliver(vec![
            msg(&group(), bob, 30, "a qzvinvoice in the group"),
            BackendEvent::Message(mention),
        ])
        .step(sleep()) // 4
        .deliver(vec![
            msg(&d_carol, &carol(), 40, "qzvinvoice qzvwire"),
            msg(&d_dave, &dave(), 50, "qzvinvoice qzvwire"),
        ])
        .step(sleep()) // 5: the chat on screen
        .deliver(vec![msg(&d_alice, alice, 55, "on screen qzvinvoice")])
        .step(sleep()) // 6: Name only
        .deliver(vec![msg(&d_alice, alice, 60, "another qzvinvoice")])
        .step(sleep()) // 7: an edit
        .deliver(vec![BackendEvent::Edited {
            target: target(Some(&d_bob), Some(bob), &ts(21).to_string()),
            by: Some(bob.clone()),
            new_local: Some(ts(22).to_string().into()),
            body: Body::text("edited"),
            edited_at: ts(22),
            keeps_quote: None,
        }])
        .step(sleep()) // 8: read on the phone
        .deliver(vec![BackendEvent::ReadMarker {
            conv: None,
            up_to: target(None, Some(alice), &ts(60).to_string()),
        }])
}

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).extend(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
    type Writer = Sink;
    fn make_writer(&'a self) -> Sink {
        self.clone()
    }
}

/// Unmuted hit (with the name, beside the chat's usual notification), muted chat with and
/// without "Also in muted chats", a muted group's mention (normal), a protected chat (no name,
/// no muted hit), a blocked one (nothing), "Name only" (no name), an edit (nothing new, the
/// hit's notification goes), reading (both go), and a log without a pattern, a name or a text.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn filters_name_their_notifications_and_follow_the_rules() {
    let sink = Sink::default();
    let _ = tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_env_filter(tracing_subscriber::EnvFilter::new("kourier=trace"))
            .with_ansi(false)
            .finish(),
    );
    let mut env = start_with(false, live_script).await;
    save(
        &env,
        vec![
            spec(INVOICES, "qzvinvoice", true, false),
            spec(WATCH, "qzvwire", true, true),
        ],
    )
    .await;

    env.advance().await; // 0 -> a message that matches Invoices
    let hit = env.call("an unmuted hit", 1).await;
    assert_eq!(hit.summary, format!("Alice Example \u{b7} {INVOICES}"));
    assert_eq!(hit.body, "Your qzvinvoice is ready");
    assert!(hit.has_action("default") && hit.has_action("inline-reply"));
    env.advance().await; // 1 -> an ordinary message beside it
    let usual = env.call("the chat's usual notification", 2).await;
    assert_eq!(usual.summary, "Alice Example", "no filter name");
    assert_eq!(usual.replaces_id, 0, "the filter's notification is its own");
    assert_ne!(usual.id, hit.id);

    // A click on the hit opens the chat at the message.
    env.mock
        .invoke(hit.id, "default", Some("tok"))
        .await
        .expect("click");
    let mut event = tokio::time::timeout(WAIT, env.events.recv())
        .await
        .expect("activation in time");
    while let Some(super::DesktopEvent::TrayAvailable(_)) = event {
        event = tokio::time::timeout(WAIT, env.events.recv())
            .await
            .expect("activation in time");
    }
    match event {
        Some(super::DesktopEvent::Activate {
            conversation: Some(_),
            message: Some(_),
            ..
        }) => {}
        other => panic!("a click on a filter's notification: {other:?}"),
    }

    env.advance().await; // 2 -> muted Bob: the first message matches a filter that is not for muted chats
    let watch = env.call("a muted hit", 3).await;
    assert_eq!(
        watch.summary,
        format!("Bob Example \u{b7} {WATCH}"),
        "the first muted message (Invoices only) made no notification"
    );
    env.advance().await; // 3 -> a muted group: a message, then a mention
    let mention = env.call("a mention in a muted group", 4).await;
    assert_eq!(mention.summary, "Filter Group");
    assert_eq!(mention.body, "hey you");
    env.advance().await; // 4 -> a protected chat and a blocked one
    let protected = env.call("a protected chat", 5).await;
    assert_eq!(protected.summary, "Carol Example", "no filter name");
    assert_eq!(protected.body, "New message");
    // The chat on screen never notifies, not even for a filter.
    let alice = uid("aci:00000000-0000-4000-8000-0000000a11ce");
    let alice_conv = env.conversation_of(&alice).await;
    env.handle
        .send(UiCommand::SetPresence {
            focused: true,
            visible: Some(alice_conv),
            beside: None,
        })
        .expect("send");
    env.advance().await; // 5 -> a hit in the focused chat
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        env.mock.with_log(|l| l.calls.len()),
        5,
        "the focused chat notifies nothing"
    );
    env.handle
        .send(UiCommand::SetPresence {
            focused: false,
            visible: None,
            beside: None,
        })
        .expect("send");
    // "Name only" mode: no filter name either.
    let mut prefs = env.desktop.prefs();
    prefs.notification_content = NotificationContent::NameOnly;
    env.desktop.set_prefs(prefs);
    env.advance().await; // 6 -> Name only
    let named = env.call("name only", 6).await;
    assert_eq!(named.summary, "Alice Example");
    assert_eq!(
        named.body, "2 new messages",
        "the usual notification counts the two ordinary messages"
    );
    assert_eq!(
        named.replaces_id, usual.id,
        "the chat's usual notification is updated, the filter's stays"
    );
    let calls_before = env.mock.with_log(|l| l.calls.len());
    assert_eq!(calls_before, 6, "blocked and muted-only messages made none");

    env.advance().await; // 7 -> an edit of the muted hit
    env.closed("the hit's message was edited", watch.id).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        env.mock.with_log(|l| l.calls.len()),
        calls_before,
        "an edit notifies nothing"
    );
    env.advance().await; // 8 -> read on the phone: both of Alice's go
    env.closed("read: the filter's notification", hit.id).await;
    env.closed("read: the usual notification", usual.id).await;

    // The log holds no pattern, no filter name, no contact name and no text.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let log =
        String::from_utf8_lossy(&sink.0.lock().unwrap_or_else(|e| e.into_inner())).to_string();
    assert!(!log.is_empty(), "the test logged nothing: no proof");
    for secret in [
        "qzv",
        "Qzv",
        "Alice Example",
        "Bob Example",
        "hey you",
        "plain hello",
    ] {
        assert!(!log.contains(secret), "the log holds {secret:?}");
    }
    env.stop.cancel();
    tokio::time::timeout(WAIT, &mut env.task)
        .await
        .expect("desktop task stops")
        .expect("no panic");
    if let Some(core) = env.core.take() {
        core.shutdown().await;
    }
}

fn catch_up_script(me: &UserId, alice: &UserId, bob: &UserId) -> Script {
    let (d_alice, d_bob) = (ConvKey::Direct(alice.clone()), ConvKey::Direct(bob.clone()));
    let d_carol = ConvKey::Direct(carol());
    let mut protect = ConversationPatch::new(d_carol.clone());
    protect.features = Some(signal_features(false));
    protect.protected = Some(true);
    let caught = |conv: &ConvKey, who: &UserId, n: i64, text: &str| {
        let mut m = remote(conv, who, &ts(n).to_string(), ts(n), text);
        m.catch_up = true;
        BackendEvent::Message(m)
    };
    Script::new("desktop-filters-catchup", vec![me.clone()])
        .step(Step::Conn(kourier_model::ConnState::Syncing {
            fraction: None,
        }))
        .deliver(vec![
            contact(alice, "Alice Example"),
            contact(bob, "Bob Example"),
            contact(&carol(), "Carol Example"),
            direct(&d_alice),
            muted_direct(&d_bob),
            BackendEvent::ConversationUpsert(protect),
        ])
        .step(sleep()) // 0: the filters are saved
        .deliver(vec![
            caught(&d_alice, alice, 1, "an qzvinvoice"),
            caught(&d_alice, alice, 2, "qzvinvoice two"),
            caught(&d_bob, bob, 3, "a muted qzvinvoice"),
            caught(&d_bob, bob, 4, "a muted qzvwire"),
            caught(&d_carol, &carol(), 5, "qzvinvoice qzvwire"),
        ])
        .step(Step::CatchUpDone)
}

/// The catch-up summary names the matching filters ("2 match <name>"): muted chats count only
/// for "Also in muted chats" filters, a protected chat never; a click opens the newest match.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn the_catch_up_summary_names_the_filters_that_match() {
    let mut env = start_with(false, catch_up_script).await;
    save(
        &env,
        vec![
            spec(INVOICES, "qzvinvoice", true, false),
            spec(WATCH, "qzvwire", true, true),
        ],
    )
    .await;
    env.advance().await;
    let summary = env.call("the summary", 1).await;
    assert!(
        summary.body.starts_with("5 new messages in 3 chats"),
        "{}",
        summary.body
    );
    assert!(
        summary.body.contains(&format!("2 match {INVOICES}")),
        "{}",
        summary.body
    );
    assert!(
        summary.body.contains(&format!("1 match {WATCH}")),
        "{}",
        summary.body
    );
    // A click opens the chat of the newest match (Bob's muted "wire") at that message.
    env.mock
        .invoke(summary.id, "default", None)
        .await
        .expect("click");
    let mut event = tokio::time::timeout(WAIT, env.events.recv())
        .await
        .expect("activation in time");
    while let Some(super::DesktopEvent::TrayAvailable(_)) = event {
        event = tokio::time::timeout(WAIT, env.events.recv())
            .await
            .expect("activation in time");
    }
    let bob = uid("aci:00000000-0000-4000-8000-000000000b0b");
    let bob_conv = env.conversation_of(&bob).await;
    match event {
        Some(super::DesktopEvent::Activate {
            conversation,
            message: Some(_),
            ..
        }) => assert_eq!(conversation, Some(bob_conv)),
        other => panic!("a click on the summary: {other:?}"),
    }
    env.stop.cancel();
    tokio::time::timeout(WAIT, &mut env.task)
        .await
        .expect("desktop task stops")
        .expect("no panic");
    if let Some(core) = env.core.take() {
        core.shutdown().await;
    }
}

/// A filter without Notify adds nothing to a summary.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn without_a_notify_filter_nothing_changes() {
    let mut env = start_with(false, catch_up_script).await;
    save(&env, vec![spec(INVOICES, "qzvinvoice", false, false)]).await;
    env.advance().await;
    let summary = env.call("the summary", 1).await;
    assert_eq!(summary.body, "5 new messages in 3 chats");
    env.stop.cancel();
    tokio::time::timeout(WAIT, &mut env.task)
        .await
        .expect("desktop task stops")
        .expect("no panic");
    if let Some(core) = env.core.take() {
        core.shutdown().await;
    }
}

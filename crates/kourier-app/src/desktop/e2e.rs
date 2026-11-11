//! The desktop task end to end: the real core over a scripted `FakeNetwork`, the desktop task,
//! and a mock notification server on a private `dbus-daemon` (never the user's session bus).
//!
//! The script sleeps on a fake clock between deliveries, so the test checks each step before
//! the next message arrives: the catch-up summary, one notification per conversation with
//! `replaces_id`, an inline reply (sent and marked read), a click with its activation token,
//! closing on delete, on edit, on a read marker from the phone and when a disappearing message
//! expires, and the content modes.
//! A second test starts the notification server only after Kourier (plasmashell after an
//! autostarted Kourier) and checks that it is used once it appears, and that a restarted
//! server gets no stale `replaces_id`.

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, FakeClock, NetworkRegistry};
use kourier_core::facade::{
    ConversationListQuery, Query, QueryData, TimelineAnchor, TimelineQuery, UiCommand,
};
use kourier_core::testing::DirVault;
use kourier_core::{Core, CoreConfig};
use kourier_desktop::testing::{MockNotifications, NotifyCall, PLASMA_CAPS, PrivateBus};
use kourier_fake::scripts::{T0, remote, signal_features, target};
use kourier_fake::{FakeNetwork, Flavor, Script, Step};
use kourier_model::{
    AccountState, Body, CannotSend, ContactPatch, ConvKey, ConversationId, ConversationPatch,
    DisappearKind, Expiry, NetworkKind, Patch, UserId,
};
use tokio::sync::mpsc;

use super::{Connection, DesktopEvent, DesktopOptions, PlainPresenter, Presenter, Texts};
use crate::paths::{HOME_ENV, Paths, Profile};
use crate::prefs::NotificationContent;

const WAIT: Duration = Duration::from_secs(20);

async fn eventually(what: &str, cond: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !cond() {
        assert!(tokio::time::Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn ts(n: i64) -> i64 {
    T0 + n * 1000
}

fn contact(user: &UserId, name: &str) -> BackendEvent {
    let mut p = ContactPatch::new(user.clone());
    p.profile_name = Patch::Set(name.into());
    BackendEvent::ContactUpsert(p)
}

fn direct(key: &ConvKey) -> BackendEvent {
    let mut p = ConversationPatch::new(key.clone());
    p.features = Some(signal_features(false));
    BackendEvent::ConversationUpsert(p)
}

fn msg(conv: &ConvKey, author: &UserId, n: i64, text: &str) -> BackendEvent {
    BackendEvent::Message(remote(conv, author, &ts(n).to_string(), ts(n), text))
}

fn sleep() -> Step {
    Step::Sleep(Duration::from_secs(10))
}

fn script(me: &UserId, alice: &UserId, bob: &UserId) -> Script {
    let (d_alice, d_bob) = (ConvKey::Direct(alice.clone()), ConvKey::Direct(bob.clone()));
    let mut c1 = remote(&d_alice, alice, &ts(1).to_string(), ts(1), "while away 1");
    c1.catch_up = true;
    let mut c2 = remote(&d_bob, bob, &ts(2).to_string(), ts(2), "while away 2");
    c2.catch_up = true;
    let mut disappearing = remote(&d_bob, bob, &ts(50).to_string(), ts(50), "gone in an hour");
    disappearing.expiry = Some(Expiry {
        kind: DisappearKind::AfterSend,
        timer: Duration::from_secs(3600),
        started_at: None,
    });
    Script::new("desktop", vec![me.clone()])
        .step(Step::Conn(kourier_model::ConnState::Syncing {
            fraction: None,
        }))
        .deliver(vec![
            contact(alice, "Alice Example"),
            contact(bob, "Bob Example"),
            direct(&d_alice),
            direct(&d_bob),
        ])
        .deliver(vec![BackendEvent::Message(c1)])
        .deliver(vec![BackendEvent::Message(c2)])
        .step(Step::CatchUpDone)
        .step(sleep()) // 1: summary checked
        .deliver(vec![msg(&d_alice, alice, 10, "hello <b>there</b>")])
        .step(sleep()) // 2
        .deliver(vec![msg(&d_alice, alice, 11, "second")])
        .step(sleep()) // 3: inline reply
        .deliver(vec![msg(&d_bob, bob, 20, "to be deleted")])
        .step(sleep()) // 4: click
        .deliver(vec![BackendEvent::Deleted {
            target: target(Some(&d_bob), Some(bob), &ts(20).to_string()),
            by: Some(bob.clone()),
            for_everyone: true,
            at: ts(21),
        }])
        .step(sleep()) // 5: hidden mode
        .deliver(vec![msg(&d_bob, bob, 30, "secret")])
        .step(sleep()) // 6
        .deliver(vec![BackendEvent::ReadMarker {
            conv: None,
            up_to: target(None, Some(bob), &ts(30).to_string()),
        }])
        .step(sleep()) // 7: back to full content
        .deliver(vec![msg(&d_alice, alice, 40, "will be edited")])
        .step(sleep()) // 8
        .deliver(vec![BackendEvent::Edited {
            target: target(Some(&d_alice), Some(alice), &ts(40).to_string()),
            by: Some(alice.clone()),
            new_local: Some(ts(41).to_string().into()),
            body: Body::text("edited"),
            edited_at: ts(41),
            keeps_quote: None,
        }])
        .step(sleep()) // 9: a message that disappears an hour after it was sent
        .deliver(vec![BackendEvent::Message(disappearing)])
}

struct Env {
    clock: FakeClock,
    mock: MockNotifications,
    core: Option<Core>,
    handle: kourier_core::facade::CoreHandle,
    desktop: super::DesktopHandle,
    events: mpsc::UnboundedReceiver<DesktopEvent>,
    task: tokio::task::JoinHandle<()>,
    stop: tokio_util::sync::CancellationToken,
    /// The profile's config directory (`kourier.toml`, the inert autostart entry).
    config: std::path::PathBuf,
    _bus: PrivateBus,
    _dir: tempfile::TempDir,
}

impl Env {
    /// Lets the script continue past its next sleep.
    async fn advance(&self) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while self.clock.sleepers() == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "script not sleeping"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Commands sent just before (prefs, focus) reach the desktop task first.
        tokio::time::sleep(Duration::from_millis(50)).await;
        self.clock.advance_to_next();
    }

    async fn call(&self, what: &str, n: usize) -> NotifyCall {
        assert!(
            self.mock.wait_for(WAIT, |l| l.calls.len() >= n).await,
            "{what}: {} notify calls",
            self.mock.with_log(|l| l.calls.len())
        );
        self.mock.with_log(|l| l.calls[n - 1].clone())
    }

    async fn closed(&self, what: &str, id: u32) {
        assert!(
            self.mock.wait_for(WAIT, |l| l.closed.contains(&id)).await,
            "{what}: {id} not closed"
        );
    }

    async fn conversation_of(&self, peer: &UserId) -> ConversationId {
        let page = match self
            .handle
            .query(Query::Conversations(ConversationListQuery::default()))
            .await
            .expect("query")
        {
            QueryData::Conversations(p) => p,
            _ => panic!("unexpected answer"),
        };
        page.rows
            .iter()
            .find(|r| r.peer.as_ref() == Some(peer))
            .map(|r| r.id)
            .expect("conversation")
    }
}

/// Starts everything; with `late_server` the notification server appears only after the
/// desktop task started and the account is linked.
async fn start(late_server: bool) -> Env {
    start_with(late_server, script).await
}

/// [`start`] with the account's script built by `script` from our id and two contacts.
async fn start_with(late_server: bool, script: fn(&UserId, &UserId, &UserId) -> Script) -> Env {
    let bus = PrivateBus::start().expect("dbus-daemon");
    let early = if late_server {
        None
    } else {
        Some(
            MockNotifications::start(bus.address(), &PLASMA_CAPS)
                .await
                .expect("mock server"),
        )
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let me = uid("aci:00000000-0000-4000-8000-0000000000aa");
    let alice = uid("aci:00000000-0000-4000-8000-0000000a11ce");
    let bob = uid("aci:00000000-0000-4000-8000-000000000b0b");
    let fake = FakeNetwork::new(Flavor::SignalLike, script(&me, &alice, &bob));
    let clock = FakeClock::new(u64::try_from(T0).unwrap_or(0));
    let registry = NetworkRegistry::new()
        .with(Arc::new(fake.clone()))
        .expect("registry");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).expect("data dir");
    let cfg = CoreConfig::new(
        data.join("core.db"),
        kourier_db::DbKey::from([0xc0; 32]),
        registry,
        Arc::new(DirVault::new(data.clone())),
        Arc::new(clock.clone()),
        tokio::runtime::Handle::current(),
    );
    let core = Core::start(cfg).await.expect("core");
    let handle = core.handle();
    let home = dir.path().join("home");
    let paths = Paths::from_env(Profile::default(), |k| {
        (k == HOME_ENV).then(|| home.clone().into_os_string())
    })
    .expect("paths");
    paths.create_dirs().expect("dirs");
    let connection = Connection {
        bus: Some(mock_client_bus(&bus).await),
        instance: None,
    };
    let stop = tokio_util::sync::CancellationToken::new();
    let (desktop, task) = super::spawn(
        &tokio::runtime::Handle::current(),
        handle.clone(),
        connection,
        &DesktopOptions::default(),
        false,
        &paths,
        stop.clone(),
    );
    let events = desktop.take_events().expect("events");
    // No tray in this test (ksni would use DBUS_SESSION_BUS_ADDRESS).
    let mut prefs = desktop.prefs();
    prefs.tray = false;
    desktop.set_prefs(prefs);
    desktop.attach(Arc::new(PlainPresenter));
    // Link the account and let the script run.
    handle.send(UiCommand::UiReady).expect("ready");
    handle
        .send(UiCommand::AddAccount {
            network: NetworkKind::Fake,
            flow: "qr".into(),
        })
        .expect("add");
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Ok(QueryData::Accounts(rows)) = handle.query(Query::Accounts).await
            && rows.iter().any(|a| a.state == AccountState::Active)
        {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "account not active");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mock = match early {
        Some(mock) => mock,
        None => MockNotifications::start(bus.address(), &PLASMA_CAPS)
            .await
            .expect("mock server"),
    };
    Env {
        clock,
        mock,
        core: Some(core),
        handle,
        desktop,
        events,
        task,
        stop,
        config: paths.config.clone(),
        _bus: bus,
        _dir: dir,
    }
}

async fn mock_client_bus(bus: &PrivateBus) -> kourier_desktop::Bus {
    bus.connect().await.expect("client bus")
}

/// The race behind a flake of the test below (2026-10-23, under load): the server appears while
/// the first connection attempt, made before it existed, is still out. The appearance must not
/// be lost when that attempt then fails: the task connects again.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_server_that_appears_during_a_failing_attempt_is_used() {
    let (held, release) = super::service::connect_gate::arm();
    let env = start(true).await;
    // The first attempt tried before the server existed and waits at the gate.
    assert!(
        tokio::time::timeout(WAIT, held.notified()).await.is_ok(),
        "the first attempt never came"
    );
    // The server's new owner reaches the task while that attempt is still out (no event says
    // when the task has handled it: a pause long against a D-Bus signal on a private bus).
    tokio::time::sleep(Duration::from_millis(500)).await;
    release.notify_one();
    assert!(
        env.mock.wait_for(WAIT, |l| l.capability_calls >= 1).await,
        "the appearance during the failing attempt was lost: never connected"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_server_that_appears_later_is_used() {
    let mut env = start(true).await;
    assert!(
        env.mock.wait_for(WAIT, |l| l.capability_calls >= 1).await,
        "the desktop task never connected to the late server"
    );
    env.advance().await; // 1: a live message
    assert!(
        env.mock
            .wait_for(WAIT, |l| l.calls.iter().any(|c| c.summary
                == "Alice Example"
                && c.body == "hello &lt;b&gt;there&lt;/b&gt;"))
            .await,
        "no notification from the late server"
    );

    // The server restarts: the old ids mean nothing to the new one, so the next message of
    // the same chat starts a new notification instead of replacing an id the new server may
    // have given to another app.
    env.mock.release().await.expect("release");
    env.mock = MockNotifications::start(env._bus.address(), &PLASMA_CAPS)
        .await
        .expect("restarted server");
    assert!(
        env.mock.wait_for(WAIT, |l| l.capability_calls >= 1).await,
        "the desktop task never connected to the restarted server"
    );
    env.advance().await; // 2: the next message of the same chat
    let next = env.call("after the restart", 1).await;
    assert_eq!(next.body, "second");
    assert_eq!(next.replaces_id, 0, "no stale replaces_id");
    env.stop.cancel();
    tokio::time::timeout(WAIT, &mut env.task)
        .await
        .expect("desktop task stops")
        .expect("no panic");
    if let Some(core) = env.core.take() {
        core.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn notifications_end_to_end() {
    let mut env = start(false).await;
    let alice = uid("aci:00000000-0000-4000-8000-0000000a11ce");
    let bob = uid("aci:00000000-0000-4000-8000-000000000b0b");

    // Catch-up: one summary, never one notification per message.
    let summary = env.call("summary", 1).await;
    assert_eq!(summary.hint_str("category").as_deref(), Some("im"));
    assert!(
        summary.body.contains("2 new messages in 2 chats"),
        "{}",
        summary.body
    );
    assert!(!summary.has_action("inline-reply"));
    // The user looks at the window: the summary goes away.
    env.desktop.set_focused(true);
    env.closed("summary on focus", summary.id).await;
    env.desktop.set_focused(false);
    env.advance().await; // 1

    // A live message: full content, escaped for the markup server, inline reply.
    let first = env.call("first message", 2).await;
    assert_eq!(first.summary, "Alice Example");
    assert_eq!(first.body, "hello &lt;b&gt;there&lt;/b&gt;");
    assert_eq!(first.replaces_id, 0);
    assert_eq!(
        first.hint_str("desktop-entry").as_deref(),
        Some(super::APP_ID)
    );
    assert_eq!(first.hint_str("category").as_deref(), Some("im.received"));
    assert!(first.has_action("default") && first.has_action("inline-reply"));
    assert!(
        first.hint_str("x-kde-origin-name").is_none(),
        "one account: no origin"
    );
    env.advance().await; // 2

    // The next message of the same chat replaces it.
    let second = env.call("second message", 3).await;
    assert_eq!(second.replaces_id, first.id);
    assert_eq!(second.id, first.id);
    assert_eq!(second.body, "second");

    // Inline reply: sent as a message, the chat is marked read, the notification closes.
    env.mock.reply(first.id, "on my way").await.expect("reply");
    env.closed("after the inline reply", first.id).await;
    let alice_conv = env.conversation_of(&alice).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let rows = match env
            .handle
            .query(Query::Timeline(TimelineQuery {
                conv: alice_conv,
                anchor: TimelineAnchor::Newest,
                limit: 10,
            }))
            .await
            .expect("timeline")
        {
            QueryData::Timeline(p) => p.rows,
            _ => panic!("unexpected answer"),
        };
        let sent = rows.iter().any(|r| {
            r.author.is_none() && matches!(&r.body, Body::Text(t) if t.body == "on my way")
        });
        let unread = match env
            .handle
            .query(Query::Conversation(alice_conv))
            .await
            .expect("conversation")
        {
            QueryData::Conversation(Some(d)) => d.row.unread,
            _ => panic!("unexpected answer"),
        };
        if sent && unread == 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "reply not sent or not read"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    env.advance().await; // 3

    // A click on another chat's notification: activation token and conversation.
    let third = env.call("bob's message", 4).await;
    assert_eq!(
        third.replaces_id, 0,
        "another conversation, another notification"
    );
    assert_ne!(third.id, first.id);
    env.mock
        .invoke(third.id, "default", Some("xdg-token-1"))
        .await
        .expect("click");
    let bob_conv = env.conversation_of(&bob).await;
    let activated = tokio::time::timeout(WAIT, env.events.recv())
        .await
        .expect("activation in time");
    let mut activated = activated;
    // TrayAvailable(false) is reported once when the UI attaches without a tray.
    while let Some(DesktopEvent::TrayAvailable(_)) = activated {
        activated = tokio::time::timeout(WAIT, env.events.recv())
            .await
            .expect("activation in time");
    }
    assert_eq!(
        activated,
        Some(DesktopEvent::Activate {
            token: Some("xdg-token-1".into()),
            conversation: Some(bob_conv),
        })
    );
    env.advance().await; // 4: the message is deleted for everyone
    env.closed("deleted", third.id).await;

    // Hidden content: no name, no text, no avatar, no reply.
    let mut prefs = env.desktop.prefs();
    prefs.notification_content = NotificationContent::Hidden;
    env.desktop.set_prefs(prefs.clone());
    env.advance().await; // 5
    let hidden = env.call("hidden", 5).await;
    assert_eq!(hidden.summary, "Kourier");
    assert_eq!(hidden.body, "New message");
    assert!(!hidden.has_action("inline-reply"));
    assert!(!hidden.hints.contains_key("image-data"));
    assert!(!format!("{hidden:?}").contains("secret"));
    env.advance().await; // 6: read on the phone
    env.closed("read on another device", hidden.id).await;

    // Name only, then an edit closes it.
    prefs.notification_content = NotificationContent::NameOnly;
    env.desktop.set_prefs(prefs);
    env.advance().await; // 7
    let named = env.call("name only", 6).await;
    assert_eq!(named.summary, "Alice Example");
    assert_eq!(named.body, "New message");
    env.advance().await; // 8: edited
    env.closed("edited", named.id).await;

    // A disappearing message: its notification goes when its timer runs out (the core's
    // sweeper deletes the message, the desktop finds it gone).
    env.advance().await; // 9
    let disappearing = env.call("disappearing", 7).await;
    assert_eq!(disappearing.summary, "Bob Example");
    env.clock.advance(Duration::from_secs(3600));
    env.closed("expired", disappearing.id).await;

    // Preferences are saved; the autostart entry follows the setting (inside the test
    // profile's config directory, never the session's autostart folder).
    let entry = env
        .config
        .join("autostart")
        .join(format!("{}.desktop", super::APP_ID));
    assert!(!entry.exists());
    let mut prefs = env.desktop.prefs();
    prefs.autostart = true;
    prefs.start_hidden = true;
    env.desktop.set_prefs(prefs.clone());
    eventually("autostart entry written", || entry.is_file()).await;
    let text = std::fs::read_to_string(&entry).expect("entry");
    assert!(text.contains("\nExec="), "{text}");
    assert!(
        !text.contains("--background"),
        "no tray in this test: start hidden would hide it for good"
    );
    let toml = env.config.join("kourier.toml");
    eventually("prefs saved", || {
        crate::prefs::Prefs::load(&toml).ok().as_ref() == Some(&prefs)
    })
    .await;
    prefs.autostart = false;
    env.desktop.set_prefs(prefs);
    eventually("autostart entry removed", || !entry.exists()).await;

    // Quit closes whatever is left and stops the task.
    env.stop.cancel();
    tokio::time::timeout(WAIT, &mut env.task)
        .await
        .expect("desktop task stops")
        .expect("no panic");
    if let Some(core) = env.core.take() {
        core.shutdown().await;
    }
}

/// Alice's chat states a reason the account cannot send in (a group where only the admins write,
/// a chat the phone has not accepted); Bob's takes messages.
fn cannot_send_script(me: &UserId, alice: &UserId, bob: &UserId) -> Script {
    let (d_alice, d_bob) = (ConvKey::Direct(alice.clone()), ConvKey::Direct(bob.clone()));
    let mut closed = ConversationPatch::new(d_alice.clone());
    let mut features = signal_features(false);
    features.cannot_send = Some(CannotSend::AdminsOnly);
    closed.features = Some(features);
    Script::new("desktop-cannot-send", vec![me.clone()])
        .step(Step::Conn(kourier_model::ConnState::Syncing {
            fraction: None,
        }))
        .deliver(vec![
            contact(alice, "Alice Example"),
            contact(bob, "Bob Example"),
            BackendEvent::ConversationUpsert(closed),
            direct(&d_bob),
        ])
        .step(Step::CatchUpDone)
        .step(sleep()) // 1
        .deliver(vec![msg(&d_alice, alice, 10, "an announcement")])
        .step(sleep()) // 2
        .deliver(vec![msg(&d_bob, bob, 20, "are you there?")])
}

/// A notification offers a reply field only where the core takes the reply: the core refuses a
/// send into a chat that states why it cannot send, and the inline reply never looks at the
/// answer, so a reply typed there would vanish. The click still opens the chat.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_chat_that_cannot_send_is_announced_without_a_reply_field() {
    let mut env = start_with(false, cannot_send_script).await;
    env.advance().await;
    let closed = env.call("the chat that cannot send", 1).await;
    assert_eq!(closed.body, "an announcement");
    assert!(closed.has_action("default"), "a click still opens it");
    assert!(
        !closed.has_action("inline-reply"),
        "no reply field where the core refuses the send: {:?}",
        closed.actions
    );
    env.advance().await;
    let open = env.call("the chat that takes messages", 2).await;
    assert_eq!(open.body, "are you there?");
    assert!(open.has_action("default") && open.has_action("inline-reply"));
    env.stop.cancel();
    tokio::time::timeout(WAIT, &mut env.task)
        .await
        .expect("desktop task stops")
        .expect("no panic");
    if let Some(core) = env.core.take() {
        core.shutdown().await;
    }
}

/// Alice's chat is protected on another device, then no longer is; Bob's never was.
fn protected_script(me: &UserId, alice: &UserId, bob: &UserId) -> Script {
    let (d_alice, d_bob) = (ConvKey::Direct(alice.clone()), ConvKey::Direct(bob.clone()));
    let mut protect = ConversationPatch::new(d_alice.clone());
    protect.protected = Some(true);
    let mut lift = ConversationPatch::new(d_alice.clone());
    lift.protected = Some(false);
    Script::new("desktop-protected", vec![me.clone()])
        .step(Step::Conn(kourier_model::ConnState::Syncing {
            fraction: None,
        }))
        .deliver(vec![
            contact(alice, "Alice Example"),
            contact(bob, "Bob Example"),
            BackendEvent::ConversationUpsert(protect),
            direct(&d_bob),
        ])
        .step(Step::CatchUpDone)
        .step(sleep()) // 1
        .deliver(vec![msg(&d_alice, alice, 10, "the secret plan")])
        .step(sleep()) // 2
        .deliver(vec![msg(&d_bob, bob, 20, "are you there?")])
        .step(sleep()) // 3
        .deliver(vec![
            BackendEvent::ConversationUpsert(lift),
            msg(&d_alice, alice, 30, "now readable"),
        ])
}

/// A protected chat (S-P, Threema Desktop's private chats) notifies with the chat's name and
/// "New message", never the text: the body, the title and the reply field hold none of it, in
/// the user's "name and message" mode. Another chat shows its text as ever, and once the other
/// device lifts the protection the chat's next message shows its text too.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_protected_chat_is_announced_without_its_text() {
    let mut env = start_with(false, protected_script).await;
    env.advance().await;
    let hidden = env.call("the protected chat", 1).await;
    assert_eq!(hidden.body, Texts::default().new_message);
    assert_eq!(hidden.summary, "Alice Example", "the name stays");
    assert!(
        !hidden.body.contains("secret") && !hidden.summary.contains("secret"),
        "no text of the message"
    );
    assert!(hidden.has_action("default") && hidden.has_action("inline-reply"));
    env.advance().await;
    let open = env.call("the chat that is not protected", 2).await;
    assert_eq!(open.body, "are you there?");
    env.advance().await;
    let lifted = env.call("the chat whose protection was lifted", 3).await;
    assert!(
        lifted.body.contains("now readable") && !lifted.body.contains("secret"),
        "the text of the new message only"
    );
    env.stop.cancel();
    tokio::time::timeout(WAIT, &mut env.task)
        .await
        .expect("desktop task stops")
        .expect("no panic");
    if let Some(core) = env.core.take() {
        core.shutdown().await;
    }
}

/// Alice's chat (two messages) and Bob's (one) show their text; then both are protected on
/// another device.
fn protect_later_script(me: &UserId, alice: &UserId, bob: &UserId) -> Script {
    let (d_alice, d_bob) = (ConvKey::Direct(alice.clone()), ConvKey::Direct(bob.clone()));
    let mut protect_alice = ConversationPatch::new(d_alice.clone());
    protect_alice.protected = Some(true);
    let mut protect_bob = ConversationPatch::new(d_bob.clone());
    protect_bob.protected = Some(true);
    Script::new("desktop-protect-later", vec![me.clone()])
        .step(Step::Conn(kourier_model::ConnState::Syncing {
            fraction: None,
        }))
        .deliver(vec![
            contact(alice, "Alice Example"),
            contact(bob, "Bob Example"),
            direct(&d_alice),
            direct(&d_bob),
        ])
        .step(Step::CatchUpDone)
        .step(sleep()) // 1
        .deliver(vec![msg(&d_alice, alice, 10, "the plan")])
        .step(sleep()) // 2
        .deliver(vec![msg(&d_alice, alice, 11, "the plan goes on")])
        .step(sleep()) // 3
        .deliver(vec![msg(&d_bob, bob, 20, "the other plan")])
        .step(sleep()) // 4
        .deliver(vec![
            BackendEvent::ConversationUpsert(protect_alice),
            BackendEvent::ConversationUpsert(protect_bob),
        ])
}

/// A chat that BECOMES protected (the user protects it on the phone) must not keep the text it
/// showed before (S-P, architecture §8.7): the notification still on screen is shown again under
/// its own id without the text, and a notification of this run that expired with the text
/// (Plasma keeps those in its history) is revoked.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_chat_that_becomes_protected_loses_the_text_it_showed_before() {
    let mut env = start_with(false, protect_later_script).await;
    env.advance().await; // 1
    let first = env.call("alice's first message", 1).await;
    assert_eq!(first.body, "the plan");
    env.advance().await; // 2
    let second = env.call("alice's second message", 2).await;
    assert_eq!((second.replaces_id, second.id), (first.id, first.id));
    assert_eq!(second.body, "the plan goes on");
    env.advance().await; // 3
    let bob = env.call("bob's message", 3).await;
    assert_eq!(bob.body, "the other plan");
    // Bob's notification expires with its text on it (his popup timed out).
    env.mock.expire(bob.id).await.expect("expire");
    // A click on Alice's notification comes behind that signal on the same stream: once the
    // activation is here, the desktop task has handled the expiry.
    env.mock
        .invoke(first.id, "default", Some("xdg-token-1"))
        .await
        .expect("click");
    loop {
        match tokio::time::timeout(WAIT, env.events.recv()).await {
            Ok(Some(DesktopEvent::Activate { .. })) => break,
            Ok(Some(_)) => {}
            other => panic!("no activation: {other:?}"),
        }
    }
    env.advance().await; // 4: both chats are protected now
    // Alice's notification is shown again IN PLACE (the same id), with the name and the count.
    let again = env.call("alice's notification without its text", 4).await;
    assert_eq!((again.replaces_id, again.id), (first.id, first.id));
    assert_eq!(again.summary, "Alice Example", "the name stays");
    assert_eq!(again.body, "2 new messages");
    assert!(again.has_action("default") && again.has_action("inline-reply"));
    assert!(
        !format!("{again:?}").contains("plan"),
        "no text of either message: {again:?}"
    );
    // Bob's notification had expired: the id is revoked (CloseNotification), not shown again.
    env.closed("bob's expired notification", bob.id).await;
    assert_eq!(
        env.mock.with_log(|l| l.calls.len()),
        4,
        "nothing else was shown"
    );
    assert!(
        env.mock.with_log(|l| !l.closed.contains(&first.id)),
        "Alice's was renewed, not closed"
    );
    env.stop.cancel();
    tokio::time::timeout(WAIT, &mut env.task)
        .await
        .expect("desktop task stops")
        .expect("no panic");
    if let Some(core) = env.core.take() {
        core.shutdown().await;
    }
}

/// Alice's and Bob's chats show their text; then the other device restates, for both, that the
/// chats are NOT protected (a network may state the flag with every sync), and Alice writes
/// again.
fn restated_open_script(me: &UserId, alice: &UserId, bob: &UserId) -> Script {
    let (d_alice, d_bob) = (ConvKey::Direct(alice.clone()), ConvKey::Direct(bob.clone()));
    let mut open_alice = ConversationPatch::new(d_alice.clone());
    open_alice.protected = Some(false);
    let mut open_bob = ConversationPatch::new(d_bob.clone());
    open_bob.protected = Some(false);
    Script::new("desktop-restated-open", vec![me.clone()])
        .step(Step::Conn(kourier_model::ConnState::Syncing {
            fraction: None,
        }))
        .deliver(vec![
            contact(alice, "Alice Example"),
            contact(bob, "Bob Example"),
            direct(&d_alice),
            direct(&d_bob),
        ])
        .step(Step::CatchUpDone)
        .step(sleep()) // 1
        .deliver(vec![msg(&d_alice, alice, 10, "the plan")])
        .step(sleep()) // 2
        .deliver(vec![msg(&d_bob, bob, 20, "the other plan")])
        .step(sleep()) // 3
        .deliver(vec![
            BackendEvent::ConversationUpsert(open_alice),
            BackendEvent::ConversationUpsert(open_bob),
        ])
        .step(sleep()) // 4
        .deliver(vec![msg(&d_alice, alice, 11, "the plan goes on")])
}

/// A chat that is NOT protected keeps what it showed when it changes (S-P, the guard of
/// `keep_text_off`): Bob's notification expired with its text and Plasma may keep it in its
/// history, which is no one's business while his chat is open to the world; the change of a
/// chat (here the other device restating "not protected") revokes none of it and shows nothing
/// again. Alice's notification on screen stays as it was, and her next message updates it in
/// place with its text.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_chat_that_is_not_protected_keeps_its_notifications_when_it_changes() {
    let mut env = start_with(false, restated_open_script).await;
    env.advance().await; // 1
    let first = env.call("alice's message", 1).await;
    assert_eq!(first.body, "the plan");
    env.advance().await; // 2
    let bob = env.call("bob's message", 2).await;
    assert_eq!(bob.body, "the other plan");
    // Bob's notification expires with its text on it (his popup timed out).
    env.mock.expire(bob.id).await.expect("expire");
    // A click on Alice's notification comes behind that signal on the same stream: once the
    // activation is here, the desktop task has handled the expiry.
    env.mock
        .invoke(first.id, "default", Some("xdg-token-1"))
        .await
        .expect("click");
    loop {
        match tokio::time::timeout(WAIT, env.events.recv()).await {
            Ok(Some(DesktopEvent::Activate { .. })) => break,
            Ok(Some(_)) => {}
            other => panic!("no activation: {other:?}"),
        }
    }
    env.advance().await; // 3: both chats change, neither is protected
    env.advance().await; // 4: Alice writes again, behind the changes in the core's order
    let again = env.call("alice's next message", 3).await;
    assert_eq!(
        (again.replaces_id, again.id),
        (first.id, first.id),
        "her notification was still hers"
    );
    assert_eq!(again.body, "the plan goes on");
    assert!(
        env.mock.with_log(|l| l.closed.is_empty()),
        "nothing was revoked or closed: {:?}",
        env.mock.with_log(|l| l.closed.clone())
    );
    assert_eq!(
        env.mock.with_log(|l| l.calls.len()),
        3,
        "no notification was shown again"
    );
    env.stop.cancel();
    tokio::time::timeout(WAIT, &mut env.task)
        .await
        .expect("desktop task stops")
        .expect("no panic");
    if let Some(core) = env.core.take() {
        core.shutdown().await;
    }
}

/// A chat with an unread text, then a call the caller gives up, which the user turns out to
/// have answered on the phone (the phone's sync came late).
fn call_script(me: &UserId, alice: &UserId, _bob: &UserId) -> Script {
    let d_alice = ConvKey::Direct(alice.clone());
    let call = kourier_fake::calls::Call::direct(alice, 4711, kourier_model::CallMedia::Voice);
    Script::new("desktop-calls", vec![me.clone()])
        .step(Step::Conn(kourier_model::ConnState::Syncing {
            fraction: None,
        }))
        .deliver(vec![contact(alice, "Alice Example"), direct(&d_alice)])
        .step(Step::CatchUpDone)
        .step(sleep()) // 1
        .deliver(vec![msg(&d_alice, alice, 10, "are you there?")])
        .step(sleep()) // 2: the text is announced
        .deliver(vec![call.caller_gave_up(ts(20))])
        .step(sleep()) // 3: the missed call is announced
        .deliver(vec![call.answered_elsewhere(ts(30))])
}

/// A missed call notifies like a message; when the line turns out answered, its notification
/// is taken back although the chat still has an unread message (`read_now` does not see it:
/// `changed_since_shown` looks at the line).
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_missed_call_is_announced_and_an_answer_takes_it_back() {
    let mut env = start_with(false, call_script).await;
    let alice = uid("aci:00000000-0000-4000-8000-0000000a11ce");
    env.advance().await;
    let text = env.call("the text", 1).await;
    assert_eq!(text.body, "are you there?");
    env.advance().await;
    let missed = env.call("the missed call", 2).await;
    assert_eq!(
        missed.replaces_id, text.id,
        "one notification per conversation"
    );
    assert_eq!(missed.id, text.id);
    // (The plain presenter of this test has no words for a notice; the UI's has.)
    assert_ne!(missed.body, text.body);
    let conv = env.conversation_of(&alice).await;
    let unread = |env: &Env| {
        let handle = env.handle.clone();
        async move {
            match handle.query(Query::Conversation(conv)).await {
                Ok(QueryData::Conversation(Some(d))) => d.row.unread,
                _ => u32::MAX,
            }
        }
    };
    assert_eq!(unread(&env).await, 2, "the text and the missed call");
    env.advance().await;
    env.closed("answered on the phone", text.id).await;
    assert_eq!(unread(&env).await, 1, "the text is still unread");
    assert_eq!(env.mock.with_log(|l| l.calls.len()), 2, "nothing new shown");
    env.stop.cancel();
    tokio::time::timeout(WAIT, &mut env.task)
        .await
        .expect("desktop task stops")
        .expect("no panic");
    if let Some(core) = env.core.take() {
        core.shutdown().await;
    }
}

/// A text, a missed call, a second text, and then the call turns out answered on the phone.
fn older_call_script(me: &UserId, alice: &UserId, _bob: &UserId) -> Script {
    let d_alice = ConvKey::Direct(alice.clone());
    let call = kourier_fake::calls::Call::direct(alice, 4712, kourier_model::CallMedia::Voice);
    Script::new("desktop-older-call", vec![me.clone()])
        .step(Step::Conn(kourier_model::ConnState::Syncing {
            fraction: None,
        }))
        .deliver(vec![contact(alice, "Alice Example"), direct(&d_alice)])
        .step(Step::CatchUpDone)
        .step(sleep()) // 1
        .deliver(vec![call.caller_gave_up(ts(10))])
        .step(sleep()) // 2: the missed call is announced
        .deliver(vec![msg(&d_alice, alice, 20, "call me back")])
        .step(sleep()) // 3: the text is what the notification shows now
        .deliver(vec![call.answered_elsewhere(ts(30))])
        .step(sleep()) // 4: the older line moved on
        .deliver(vec![msg(&d_alice, alice, 40, "thanks")])
}

/// A missed call that turns out answered while the notification of its chat shows a NEWER
/// message: the notification stays (its text is still true), the line is only not counted any
/// more, and the next message replaces the same notification.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn an_older_call_that_moved_on_leaves_the_notification_of_a_newer_message() {
    let mut env = start_with(false, older_call_script).await;
    env.advance().await;
    let missed = env.call("the missed call", 1).await;
    env.advance().await;
    let text = env.call("the text", 2).await;
    assert_eq!(text.body, "call me back");
    assert_eq!((text.id, text.replaces_id), (missed.id, missed.id));
    // The call was answered on the phone after all.
    env.advance().await;
    env.advance().await;
    let thanks = env.call("the next text", 3).await;
    assert_eq!(thanks.body, "thanks");
    assert_eq!(
        thanks.replaces_id, text.id,
        "the notification was still there to be replaced"
    );
    assert!(
        env.mock.with_log(|l| l.closed.is_empty()),
        "nothing was closed"
    );
    env.stop.cancel();
    tokio::time::timeout(WAIT, &mut env.task)
        .await
        .expect("desktop task stops")
        .expect("no panic");
    if let Some(core) = env.core.take() {
        core.shutdown().await;
    }
}

/// §4.7, the desktop side of S15: logind's resume and NetworkManager's changes on a private
/// bus (mock services, never the machine's own) reach the linked backend through the real
/// core as `Resumed` and `NetworkChanged`; a suspend that only begins reaches nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn resume_and_network_hints_reach_the_backend() {
    use kourier_backend_api::SystemClock;
    use kourier_desktop::testing::MockSystem;

    let bus = PrivateBus::start().expect("dbus-daemon");
    let dir = tempfile::tempdir().expect("tempdir");
    let me = uid("aci:00000000-0000-4000-8000-0000000000aa");
    let fake = FakeNetwork::new(Flavor::SignalLike, Script::new("hints", vec![me]));
    let registry = NetworkRegistry::new()
        .with(Arc::new(fake.clone()))
        .expect("registry");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).expect("data dir");
    // The real clock: the core collects the hints of a burst for one second.
    let cfg = CoreConfig::new(
        data.join("core.db"),
        kourier_db::DbKey::from([0xc0; 32]),
        registry,
        Arc::new(DirVault::new(data.clone())),
        Arc::new(SystemClock),
        tokio::runtime::Handle::current(),
    );
    let core = Core::start(cfg).await.expect("core");
    let handle = core.handle();
    handle.send(UiCommand::UiReady).expect("ready");
    handle
        .send(UiCommand::AddAccount {
            network: NetworkKind::Fake,
            flow: "qr".into(),
        })
        .expect("add");
    let deadline = tokio::time::Instant::now() + WAIT;
    let uuid = loop {
        if let Ok(QueryData::Accounts(rows)) = handle.query(Query::Accounts).await
            && let Some(a) = rows.iter().find(|a| a.state == AccountState::Active)
        {
            break a.uuid;
        }
        assert!(tokio::time::Instant::now() < deadline, "account not active");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let control = fake.control();
    let got = |name: &'static str| {
        control
            .commands(uuid)
            .iter()
            .filter(|c| **c == name)
            .count()
    };

    // The services are not there yet when Kourier starts listening (it must not mind).
    let stop = tokio_util::sync::CancellationToken::new();
    let task = super::spawn_hints(
        &tokio::runtime::Handle::current(),
        handle.clone(),
        super::SystemBus::At(bus.address().to_owned()),
        stop.clone(),
    )
    .expect("a task for a named bus");
    let system = MockSystem::start(bus.address()).await.expect("mock");
    system.prepare_for_sleep(true).await.expect("emit");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        got("Resumed") + got("NetworkChanged"),
        0,
        "a suspend is no hint"
    );

    // Waking up: the backend is told to reconnect.
    let deadline = tokio::time::Instant::now() + WAIT;
    while got("Resumed") == 0 {
        system.prepare_for_sleep(false).await.expect("emit");
        assert!(tokio::time::Instant::now() < deadline, "no Resumed");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    // A burst of network signals (state changes and a new primary connection) is one hint
    // for the backend, not one per signal.
    tokio::time::sleep(kourier_core::HINT_SETTLE * 2).await;
    let before = got("NetworkChanged");
    for state in [20, 40, 70] {
        system.network_state(state).await.expect("emit");
    }
    system
        .property_changed("PrimaryConnection")
        .await
        .expect("emit");
    eventually("the network change reaches the backend", || {
        got("NetworkChanged") > before
    })
    .await;
    tokio::time::sleep(kourier_core::HINT_SETTLE * 2).await;
    assert_eq!(
        got("NetworkChanged"),
        before + 1,
        "one command for the burst"
    );

    // No bus named, or services off: no task at all.
    assert!(
        super::spawn_hints(
            &tokio::runtime::Handle::current(),
            handle.clone(),
            super::SystemBus::Off,
            stop.clone(),
        )
        .is_none()
    );
    let nowhere = super::spawn_hints(
        &tokio::runtime::Handle::current(),
        handle.clone(),
        super::SystemBus::At("unix:path=/nonexistent/kourier-no-system-bus".into()),
        stop.clone(),
    )
    .expect("task");
    // An address without a bus is waited for (a bus may still come up there): the task tells
    // the backend nothing meanwhile and ends with the app, like the other.
    let before = got("Resumed") + got("NetworkChanged");
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert!(!nowhere.is_finished(), "it goes on waiting for a bus");
    assert_eq!(got("Resumed") + got("NetworkChanged"), before);

    stop.cancel();
    tokio::time::timeout(WAIT, nowhere)
        .await
        .expect("the waiting task ends with the app")
        .expect("no panic");
    tokio::time::timeout(WAIT, task)
        .await
        .expect("the task ends with the app")
        .expect("no panic");
    core.shutdown().await;
}

/// A presenter whose `texts()` keeps the desktop task that asks for them busy for a while
/// (the task calls it inside its own poll when the UI attaches).
struct Busy {
    entered: Arc<std::sync::atomic::AtomicBool>,
    hold: Duration,
}

impl Presenter for Busy {
    fn texts(&self) -> Texts {
        self.entered
            .store(true, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(self.hold);
        Texts::default()
    }
}

/// C5 (K-A fix round): preferences that are sent right before the stop are saved, and the
/// newest of several is the one that stays. "Turn On and Restart" changes the preferences
/// (`networks-on`, `pending-link`) and quits at once; with the stop and the queued `Prefs`
/// both ready, `select!` took either, and the restart came up on the old file in about half
/// the runs. Thirty runs of the race, three changes sent each time: in the even runs the task
/// is busy when they and the stop arrive (so they all wait for it), in the odd ones the stop
/// comes after zero to three turns of the task.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn preferences_sent_just_before_the_stop_are_saved() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeNetwork::new(Flavor::SignalLike, Script::new("prefs", Vec::new()));
    let registry = NetworkRegistry::new()
        .with(Arc::new(fake))
        .expect("registry");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).expect("data dir");
    let core = Core::start(CoreConfig::new(
        data.join("core.db"),
        kourier_db::DbKey::from([0xc0; 32]),
        registry,
        Arc::new(DirVault::new(data.clone())),
        Arc::new(FakeClock::new(u64::try_from(T0).unwrap_or(0))),
        tokio::runtime::Handle::current(),
    ))
    .await
    .expect("core");
    let home = dir.path().join("home");
    let paths = Paths::from_env(Profile::default(), |k| {
        (k == HOME_ENV).then(|| home.clone().into_os_string())
    })
    .expect("paths");
    paths.create_dirs().expect("dirs");
    for run in 0..30 {
        let stop = tokio_util::sync::CancellationToken::new();
        let (desktop, task) = super::spawn(
            &tokio::runtime::Handle::current(),
            core.handle(),
            Connection {
                bus: None,
                instance: None,
            },
            &DesktopOptions::default(),
            false,
            &paths,
            stop.clone(),
        );
        if run % 2 == 0 {
            let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
            desktop.attach(Arc::new(Busy {
                entered: entered.clone(),
                hold: Duration::from_millis(40),
            }));
            eventually("the task is busy", || {
                entered.load(std::sync::atomic::Ordering::SeqCst)
            })
            .await;
        }
        let wanted = format!("whatsapp-{run}");
        for step in ["first", "second", "newest"] {
            let w = format!("{wanted}-{step}");
            desktop.update_prefs(move |p| p.pending_link = Some(w));
        }
        // Cancelled at once, or after the task had a few turns of its own.
        for _ in 0..run % 4 {
            tokio::task::yield_now().await;
        }
        stop.cancel();
        tokio::time::timeout(WAIT, task)
            .await
            .expect("desktop task stops")
            .expect("no panic");
        let saved = crate::prefs::Prefs::load_or_default(&paths.config_file());
        assert_eq!(
            saved.pending_link,
            Some(format!("{wanted}-newest")),
            "run {run}: the newest preferences sent before the stop were not the ones saved"
        );
    }
    core.shutdown().await;
}

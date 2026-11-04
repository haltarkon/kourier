//! The network's descriptor, the id of an own message and the parts of the account actor that need
//! no client: it opens the store, answers every request with a typed error and ends cleanly. No
//! client, no socket (the actor over the real client is `tests/actor_real.rs`; its login is
//! there too).
//!
//! Every network here is made from the guard parts (`support::guards`), and the tests that spawn an
//! actor run in ONE offline child under `unshare -rn` (`the_actor_runs_offline`; lane decision L3),
//! the bodies one after the other, so that an actor that builds a client cannot reach a network
//! from this file. The source check in `tests/builder.rs` enforces both.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, Command, ContextParams, CoreEnds, DbKey, Exit, Network, StartMode, SystemClock,
};
use kourier_model::{
    AccountId, AccountUuid, ConvKey, ErrorKind, HistorySource, MsgIdScope, NetworkKind, UserId,
};
use kourier_whatsapp::{OpenError, StoreOwner, WhatsAppNetwork};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use wacore::store::traits::DeviceStore;

use crate::support::guards;
use crate::support::{self, TEST_KEY, db_path, key};

fn conv() -> ConvKey {
    ConvKey::Direct(UserId::parse("lid:100000000000001").expect("a lid"))
}

#[test]
fn the_network_describes_itself_without_triggering_signals_screens() {
    let network = WhatsAppNetwork::new(guards::parts());
    assert_eq!(network.kind(), NetworkKind::WhatsApp);
    let d = network.descriptor();
    assert_eq!(d.display_name, "WhatsApp");
    // The history is pushed after the link; since K-A that no longer makes the screens offer
    // Signal's "Transfer History from Phone…" (a link and unlink cycle, a risk for the account):
    // they key on `history_transfer`, which a pushing network leaves off.
    assert_eq!(d.history, HistorySource::PushedOnLink);
    assert!(!d.history_transfer, "the archive transfer is Signal's");
    // Off until the person turns it on: the app registers it only then (architecture §7.4).
    assert!(d.experimental);
    assert_eq!(d.msg_id_scope, MsgIdScope::ConversationAuthor);
    // A second edit names the original, as WhatsApp's wire format does (K-B1 B5).
    assert_eq!(d.edit_target, kourier_model::EditTarget::Original);
    assert!(d.start_chat.is_empty(), "never a new chat (his decision)");
    // R-Q1: its actor may wait up to 15 s at a quit for a history chunk the phone announced, so
    // the core gives it 20 s (Signal states none: the core's 5 s).
    assert_eq!(d.shutdown_grace, Some(Duration::from_secs(20)));
    // K-A's warning page asks first only for a descriptor that has a notice (the list's
    // `asks_first` is `risk_notice.is_some()`): the real descriptor must, as the fake's WhatsApp does.
    let notice = d.risk_notice.as_ref().expect("the real network asks first");
    assert!(!notice.title.is_empty() && !notice.body.is_empty() && notice.url.is_none());
    let flows: Vec<_> = d.login_flows.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(flows, ["qr", "pair-code"]);
    assert_eq!(format!("{network:?}"), "WhatsAppNetwork");
}

/// W5a, D11: WhatsApp's outbox numbers are its own constant: a day of tries, at most 15 minutes between two, and
/// a day for a challenge it never sends.
#[test]
fn the_outbox_numbers_are_whatsapps() {
    let network = WhatsAppNetwork::new(guards::parts());
    let outbox = network.descriptor().outbox;
    assert_eq!(outbox.budget, Duration::from_secs(24 * 3600));
    assert_eq!(outbox.max_backoff, Duration::from_secs(15 * 60));
    assert_eq!(outbox.challenge_lifetime, Duration::from_secs(24 * 3600));
}

/// W5b G7b (LOC F14): WhatsApp states every type `kourier_media::clean_still` cleans, up to the 64 MiB a picture is read
/// whole (a larger one is streamed untouched as a document, so the core asks for it), and the fake playing WhatsApp
/// (the rehearsal, the fixture) states the same. Mutants: the statement left `None`; its bound dropped.
#[test]
fn it_states_the_pictures_it_cleans_as_the_fake_playing_whatsapp_does() {
    let network = WhatsAppNetwork::new(guards::parts());
    let d = network.descriptor();
    let cleaned = d
        .cleaned_pictures
        .as_ref()
        .expect("W5b states what it cleans");
    let bound = 64 * 1024 * 1024;
    for t in kourier_media::CLEANED_TYPES {
        assert!(cleaned.cleans(t, bound), "{t}");
    }
    assert!(
        !cleaned.cleans("image/jpeg", bound + 1),
        "a larger picture is streamed untouched"
    );
    assert!(!cleaned.cleans("image/gif", 1) && !cleaned.cleans("image/heic", 1));
    assert_eq!(
        kourier_fake::Flavor::SignalLike
            .playing_descriptor(NetworkKind::WhatsApp)
            .cleaned_pictures
            .as_ref(),
        Some(cleaned),
        "the fake playing WhatsApp states what the backend does"
    );
}

fn is_whatsapp_shaped(id: &str) -> bool {
    id.len() == 22
        && id.starts_with("3EB0")
        && id[4..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
}

#[test]
fn an_own_message_id_has_whatsapps_shape_with_and_without_a_phone_number() {
    let network = WhatsAppNetwork::new(guards::parts());
    let now_ms = 1_792_000_000_123;
    let own_pn = [
        UserId::parse("lid:100000000000001").expect("lid"),
        UserId::parse("pn:15550000001").expect("pn"),
    ];
    for own in [&own_pn[..], &own_pn[..1], &[]] {
        let id = network.allocate_message_id(own, &conv(), now_ms);
        assert!(is_whatsapp_shaped(&id.local), "{}", id.local);
        assert_eq!(id.sent_at, i64::try_from(now_ms).unwrap());
    }
    // Random, not deterministic: two ids of one instant differ.
    let a = network.allocate_message_id(&own_pn, &conv(), now_ms);
    let b = network.allocate_message_id(&own_pn, &conv(), now_ms);
    assert_ne!(a.local, b.local);
}

/// A running actor with its channels.
struct Running {
    commands: tokio::sync::mpsc::Sender<Command>,
    /// The core's ends of the channels: kept so that the actor does not see the core go.
    _ends: CoreEnds,
    handle: kourier_backend_api::AccountHandle,
    cancel: CancellationToken,
}

fn start_actor(dir: &Path, start: StartMode, db_key: [u8; 32]) -> Running {
    let cancel = CancellationToken::new();
    let (ctx, ends) = AccountContext::new(ContextParams {
        account: AccountId(1),
        uuid: AccountUuid::new_random(),
        start,
        backend_db: db_path(dir),
        db_key: DbKey::new(db_key),
        blob_dir: dir.join("blobs"),
        net_rt: tokio::runtime::Handle::current(),
        cancel: cancel.clone(),
        clock: Arc::new(SystemClock),
        config: Default::default(),
        ui: Default::default(),
        local_prefs: Default::default(),
    });
    let handle = WhatsAppNetwork::new(guards::parts())
        .spawn(ctx)
        .expect("the actor starts");
    Running {
        commands: ends.commands.clone(),
        _ends: ends,
        handle,
        cancel,
    }
}

impl Running {
    async fn exit(mut self) -> Exit {
        let exited = tokio::time::timeout(Duration::from_secs(10), &mut self.handle.exited)
            .await
            .expect("the actor ends");
        let exit = Exit::from_channel(exited);
        if let Some(thread) = self.handle.thread.take() {
            thread.join().expect("the actor's thread joins");
        }
        exit
    }
}

fn fresh_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

async fn a_request_with_a_reply_gets_an_error_and_a_shutdown_ends_cleanly() {
    let dir = fresh_dir();
    let actor = start_actor(dir.path(), StartMode::Fresh, TEST_KEY);
    let (reply, answer) = oneshot::channel();
    actor
        .commands
        .send(Command::ListOwnDevices { reply })
        .await
        .expect("send");
    let answer = tokio::time::timeout(Duration::from_secs(10), answer)
        .await
        .expect("answered")
        .expect("a reply");
    assert!(matches!(answer, Err(ref e) if matches!(e.kind, ErrorKind::Unsupported)));
    actor.commands.send(Command::Shutdown).await.expect("send");
    let files = dir.path().to_owned();
    assert_eq!(actor.exit().await, Exit::Shutdown);
    // The store was closed before the exit was reported: no writer holds the file, so the WAL is gone.
    assert!(
        !wal_of(&files).exists(),
        "the writer thread outlived the actor"
    );
}

fn wal_of(dir: &Path) -> PathBuf {
    dir.join("backend.db-wal")
}

async fn cancel_ends_the_actor_like_a_shutdown_and_a_logout_ends_logged_out() {
    let dir = fresh_dir();
    let actor = start_actor(dir.path(), StartMode::Fresh, TEST_KEY);
    actor.cancel.cancel();
    assert_eq!(actor.exit().await, Exit::Shutdown);

    let actor = start_actor(dir.path(), StartMode::Resume, TEST_KEY);
    actor.commands.send(Command::Logout).await.expect("send");
    assert!(matches!(actor.exit().await, Exit::LoggedOut { .. }));
}

async fn a_wrong_key_ends_the_actor_failed_and_leaves_the_file_as_it_was() {
    let dir = fresh_dir();
    StoreOwner::open(&db_path(dir.path()), &key(), "wa-test")
        .expect("make the file")
        .close();
    let before = std::fs::read(db_path(dir.path())).expect("read");
    let mut wrong = TEST_KEY;
    wrong[0] ^= 1;
    let actor = start_actor(dir.path(), StartMode::Resume, wrong);
    let exit = actor.exit().await;
    assert!(matches!(exit, Exit::Failed(_)), "{exit:?}");
    assert_eq!(std::fs::read(db_path(dir.path())).expect("read"), before);
    // The right key still opens it.
    let error: Result<StoreOwner, OpenError> =
        StoreOwner::open(&db_path(dir.path()), &key(), "wa-test");
    error.expect("the file is intact").close();
}

async fn a_fresh_start_never_lands_on_an_existing_device_and_a_resume_needs_the_file() {
    let dir = fresh_dir();
    // An account that has a device in its file.
    let owner = StoreOwner::open(&db_path(dir.path()), &key(), "wa-test").expect("open");
    owner.store().create().await.expect("a device");
    owner.close();
    let exit = start_actor(dir.path(), StartMode::Fresh, TEST_KEY)
        .exit()
        .await;
    assert!(
        matches!(exit, Exit::Failed(_)),
        "a fresh start over a device: {exit:?}"
    );
    // A resume goes on.
    let actor = start_actor(dir.path(), StartMode::Resume, TEST_KEY);
    actor.commands.send(Command::Shutdown).await.expect("send");
    assert_eq!(actor.exit().await, Exit::Shutdown);

    // A resume without the file is refused and does not make one.
    let empty = fresh_dir();
    let exit = start_actor(empty.path(), StartMode::Resume, TEST_KEY)
        .exit()
        .await;
    assert!(matches!(exit, Exit::Failed(_)), "{exit:?}");
    assert!(!db_path(empty.path()).exists(), "a resume made a new file");
}

/// T-7: building the network, reading its descriptor and allocating an id make no client part: the
/// maker is for the actor, at its first connect, and not for the application's start.
#[test]
fn a_new_network_builds_no_client_part() {
    let made = Arc::new(AtomicUsize::new(0));
    let network = WhatsAppNetwork::new(guards::counting_parts(&made));
    let _ = network.descriptor();
    let own = [UserId::parse("pn:15550000001").expect("pn")];
    let _ = network.allocate_message_id(&own, &conv(), 1_792_000_000_123);
    assert_eq!(
        made.load(Ordering::SeqCst),
        0,
        "the network made a client part before any actor asked for one"
    );
}

/// An actor that is started and never asked to connect or to link makes no client part either (the
/// client is built at the first `Connect` or `StartLogin`, never at the actor's start).
async fn an_actor_that_never_connects_makes_no_parts() {
    let dir = fresh_dir();
    let made = Arc::new(AtomicUsize::new(0));
    let cancel = CancellationToken::new();
    let (ctx, ends) = AccountContext::new(ContextParams {
        account: AccountId(1),
        uuid: AccountUuid::new_random(),
        start: StartMode::Fresh,
        backend_db: db_path(dir.path()),
        db_key: DbKey::new(TEST_KEY),
        blob_dir: dir.path().join("blobs"),
        net_rt: tokio::runtime::Handle::current(),
        cancel: cancel.clone(),
        clock: Arc::new(SystemClock),
        config: Default::default(),
        ui: Default::default(),
        local_prefs: Default::default(),
    });
    let handle = WhatsAppNetwork::new(guards::counting_parts(&made))
        .spawn(ctx)
        .expect("the actor starts");
    ends.commands.send(Command::Shutdown).await.expect("send");
    let actor = Running {
        commands: ends.commands.clone(),
        _ends: ends,
        handle,
        cancel,
    };
    assert_eq!(actor.exit().await, Exit::Shutdown);
    assert_eq!(
        made.load(Ordering::SeqCst),
        0,
        "an actor that never connected made a client part"
    );
}

/// The parent: every test that spawns an actor runs in the offline child below.
#[test]
fn the_actor_runs_offline() {
    support::offline::run_child(module_path!(), "actors_child");
}

/// The child (a no-op as an ordinary test): the actor tests of this file, one after the other, with
/// the guards untouched at the end.
#[tokio::test]
async fn actors_child() {
    if !support::offline::in_child() {
        return;
    }
    eprintln!("a_request_with_a_reply_gets_an_error_and_a_shutdown_ends_cleanly");
    a_request_with_a_reply_gets_an_error_and_a_shutdown_ends_cleanly().await;
    eprintln!("cancel_ends_the_actor_like_a_shutdown_and_a_logout_ends_logged_out");
    cancel_ends_the_actor_like_a_shutdown_and_a_logout_ends_logged_out().await;
    eprintln!("a_wrong_key_ends_the_actor_failed_and_leaves_the_file_as_it_was");
    a_wrong_key_ends_the_actor_failed_and_leaves_the_file_as_it_was().await;
    eprintln!("a_fresh_start_never_lands_on_an_existing_device_and_a_resume_needs_the_file");
    a_fresh_start_never_lands_on_an_existing_device_and_a_resume_needs_the_file().await;
    eprintln!("an_actor_that_never_connects_makes_no_parts");
    an_actor_that_never_connects_makes_no_parts().await;
    assert_eq!(guards::dials(), 0, "a dial was attempted");
    assert_eq!(guards::http_calls(), 0, "an HTTP request was attempted");
}

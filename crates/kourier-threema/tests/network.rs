//! `ThreemaNetwork`: what the descriptor says, the id of an own message, the device label, that
//! making the network makes no client, and the skeleton actor (TH1): it opens the store, keeps the
//! start rules, answers every request that carries a reply, links nothing, connects nowhere and
//! ends cleanly. The ONE dial of the child is the logout of a stored device (RP-b: it asks for a
//! D2M-only socket to drop the device), which the guard refuses: `NotReached`.
//!
//! Every network here is made from the guard parts (`support::guards`), and the tests that spawn an
//! actor run in ONE offline child under `unshare -rn` (`the_actor_runs_offline`), the bodies one
//! after the other, with the guards checked at the end. The source check in `tests/builder.rs`
//! enforces both.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use kourier_backend_api::{
    AbortAnswer, AccountContext, AccountHandle, BackendConfig, BackendEnv, BackendEvent,
    BlobTarget, Command, ContextParams, CoreEnds, DbKey, Envelope, Exit, Network, OutgoingKind,
    OutgoingRequest, StartMode, SystemClock, TransmitOutcome,
};
use kourier_model::{
    AccountId, AccountSettingsPatch, AccountUuid, AttachmentId, BackendBlob, BackendError, BlobKey,
    Body, ConnState, ContactChange, ConvKey, ConversationChange, DeviceLogout, DownloadId,
    EditTarget, ErrorKind, HistoryReadState, HistorySource, Identifier, IdentifierKind,
    IdentityFingerprint, LoginStepKind, MessageId, MsgIdScope, NetworkKind, OutboxPolicy,
    ProfilePatch, Redacted, UserId,
};
use kourier_threema::ThreemaNetwork;
use kourier_threema::store::{Identity, SlotState, mark_join_ready, save_identity};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::support::guards;
use crate::support::{self, TEST_KEY, db_path, key};

fn conv() -> ConvKey {
    ConvKey::Direct(UserId::parse("threema:TH1QW7KZ").expect("a Threema user id"))
}

#[test]
fn the_descriptor_says_what_threema_is() {
    let network = ThreemaNetwork::new(guards::parts());
    assert_eq!(network.kind(), NetworkKind::Threema);
    let d = network.descriptor();
    assert_eq!(d.display_name, "Threema");
    assert_eq!(d.icon_name, "threema");
    let flows: Vec<&str> = d.login_flows.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(
        flows,
        ["qr"],
        "one login flow: the QR code (the emoji are a step)"
    );
    assert!(
        d.start_chat.is_empty(),
        "a chat from a Threema ID comes later"
    );
    assert_eq!(d.history, HistorySource::None);
    assert_eq!(d.history_read, HistoryReadState::Known);
    assert!(!d.history_transfer);
    assert!(d.experimental, "Threema is off until the user turns it on");
    assert_eq!(d.msg_id_scope, MsgIdScope::Conversation);
    assert_eq!(d.edit_target, EditTarget::Original);
    assert_eq!(d.outbox, OutboxPolicy::default());
    assert_eq!(d.shutdown_grace, None);
    assert_eq!(d.typing.refresh, Duration::from_secs(10));
    assert_eq!(d.typing.stop_after, Duration::from_secs(3));
    let notice = d
        .risk_notice
        .as_ref()
        .expect("a risk notice, so the warning page asks first");
    assert_eq!(notice.title, "Before You Turn On Threema");
    assert!(!notice.body.is_empty() && notice.url.is_none());
}

/// The descriptor twin (TH4, decision L7): `kourier-fake` copies this descriptor for the fake that
/// plays Threema (the fixture pages `thqr` and `themoji`, the link tests), words of the risk
/// notice included, with no import of this crate. Every field but three is equal; the three the
/// fake keeps the Signal-like numbers for on purpose, so the same script gives the same chats and
/// ids as the plain fake, are pinned here and then made equal to compare the rest in one
/// assertion (a field the descriptor gains later is compared too).
#[test]
fn the_fake_playing_threema_copies_this_descriptor() {
    let network = ThreemaNetwork::new(guards::parts());
    let real = network.descriptor().clone();
    let mut fake = kourier_fake::Flavor::SignalLike.playing_descriptor(NetworkKind::Threema);
    assert_eq!(
        (fake.msg_id_scope, fake.edit_target, fake.outbox),
        (
            MsgIdScope::AccountAuthor,
            EditTarget::LatestRevision,
            OutboxPolicy::SIGNAL
        ),
        "the fake keeps the Signal-like numbers on purpose"
    );
    fake.msg_id_scope = real.msg_id_scope;
    fake.edit_target = real.edit_target;
    fake.outbox = real.outbox;
    assert_eq!(fake, real);
    // (The words the warning page falls back to: both sides, by their literals.)
    let notice = real.risk_notice.as_ref().expect("a risk notice");
    assert_eq!(notice.title, "Before You Turn On Threema");
    assert!(
        notice
            .body
            .starts_with("Kourier is not an official Threema app.")
    );
}

#[test]
fn an_own_message_id_is_16_lowercase_hex_digits_and_random() {
    let network = ThreemaNetwork::new(guards::parts());
    let own = [UserId::parse("threema:TH1ZZ9YX").expect("own id")];
    let first = network.allocate_message_id(&own, &conv(), 1_700_000_000_123);
    let second = network.allocate_message_id(&[], &conv(), 1_700_000_000_124);
    for id in [&first, &second] {
        assert!(
            id.local.len() == 16
                && id
                    .local
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "an id is not 16 lowercase hex digits"
        );
    }
    assert!(first.local != second.local, "two ids are the same");
    assert_eq!(first.sent_at, 1_700_000_000_123);
    assert_eq!(second.sent_at, 1_700_000_000_124);
    // A time past i64 is clamped, never wrapped.
    assert_eq!(
        network.allocate_message_id(&[], &conv(), u64::MAX).sent_at,
        i64::MAX
    );
}

#[test]
fn making_and_asking_the_network_makes_no_client() {
    let made = Arc::new(AtomicUsize::new(0));
    let network = ThreemaNetwork::new(guards::counting_parts(&made));
    let _ = network.descriptor();
    let _ = network.allocate_message_id(&[], &conv(), 1);
    let _ = network.device_label();
    let _ = format!("{network:?}");
    drop(network);
    assert_eq!(
        made.load(Ordering::SeqCst),
        0,
        "the parts' maker was called"
    );
    assert_eq!(guards::dials(), 0, "a WebSocket was asked for");
    assert_eq!(guards::http_calls(), 0, "an HTTPS request was made");
}

#[test]
fn the_device_label_is_kourier_and_the_host() {
    let label =
        |host: &str| ThreemaNetwork::new(guards::parts().with_host_name(host)).device_label();
    assert!(
        ThreemaNetwork::new(guards::parts()).device_label() == "Kourier",
        "a network without a host name is not labelled plain Kourier"
    );
    assert!(
        label("workbench") == "Kourier (workbench)",
        "the label is not Kourier and the host name"
    );
    assert!(
        label("  \u{202E}work\nbench\u{2069} ") == "Kourier (workbench)",
        "a bidi mark or a line break stayed in the label"
    );
    assert!(
        label(" \t\u{200F} ") == "Kourier",
        "a blank host name made a label with a host"
    );
    let long = "h".repeat(200);
    let cut = label(&long);
    assert_eq!(
        cut.chars().count(),
        64,
        "the label is cut to libthreema's 64"
    );
    assert!(
        cut == format!("Kourier ({})", "h".repeat(54)),
        "the cut label is not Kourier and the first 54 characters"
    );
    // A multi-byte host is cut on a character border.
    let wide = label(&"ж".repeat(100));
    assert_eq!(wide.chars().count(), 64);
}

#[test]
fn the_network_prints_nothing_of_its_parts() {
    let network = ThreemaNetwork::new(guards::parts().with_host_name("TH1QW7KZ-host"));
    let printed = format!("{network:?}");
    assert!(
        printed == "ThreemaNetwork",
        "the network prints more than its name"
    );
}

// ----- the actor (in the offline child) -----

/// The synthetic ID and label of a stored device.
const LINKED_ID: &str = "TH1QW7KZ";
const LINKED_LABEL: &str = "Kourier (th1-actor-host)";

/// How long a test waits for one answer of the actor. A reply that is dropped is seen at once
/// (`RecvError`); this only bounds an actor that never answers.
const ANSWER: Duration = Duration::from_secs(5);

/// The one counter of the clients every actor of this child made through its parts' maker: each
/// scenario builds its network from `guards::counting_parts` with it, and `actors_child` checks it
/// is 0 at the end (TH1 makes no client, whatever the start mode or the command).
fn made_counter() -> Arc<AtomicUsize> {
    static MADE: OnceLock<Arc<AtomicUsize>> = OnceLock::new();
    MADE.get_or_init(Arc::default).clone()
}

/// A running actor with the core's ends of its channels.
struct Running {
    ends: CoreEnds,
    handle: AccountHandle,
    cancel: CancellationToken,
    /// How many clients the parts' maker has made in this child (see [`made_counter`]).
    made: Arc<AtomicUsize>,
}

impl Running {
    async fn send(&self, command: Command) {
        self.ends
            .commands
            .send(command)
            .await
            .expect("the actor takes commands");
    }

    /// The next envelope the actor sends to the core.
    async fn next_envelope(&mut self) -> Envelope {
        tokio::time::timeout(ANSWER, self.ends.events.recv())
            .await
            .expect("the actor sent nothing in time")
            .expect("the actor's events channel closed")
    }

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

fn start_actor_with(
    dir: &Path,
    start: StartMode,
    db_key: [u8; 32],
    config: BackendConfig,
) -> Running {
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
        config,
        ui: Default::default(),
        local_prefs: Default::default(),
    });
    let made = made_counter();
    let network = ThreemaNetwork::new(guards::counting_parts(&made));
    let handle = network.spawn(ctx).expect("the actor starts");
    Running {
        ends,
        handle,
        cancel,
        made,
    }
}

fn start_actor(dir: &Path, start: StartMode, db_key: [u8; 32]) -> Running {
    start_actor_with(dir, start, db_key, BackendConfig::default())
}

/// A store that holds a linked device (synthetic values), closed again.
fn store_with_a_device(dir: &Path) {
    let db = kourier_threema::store::open(&db_path(dir), &key()).expect("make the file");
    let identity = Identity {
        threema_id: LINKED_ID.to_owned(),
        client_key: Zeroizing::new([0x11; 32]),
        server_group: "a".to_owned(),
        device_group_key: Zeroizing::new([0x22; 32]),
        device_cookie: Zeroizing::new([0x33; 16]),
        d2m_device_id: 7,
        csp_device_id: 8,
        slot_state: SlotState::New,
        device_label: LINKED_LABEL.to_owned(),
        linked_at_ms: 1_761_350_400_000,
    };
    // A linked file: the identity AND zone 2's marker (S4).
    db.zone(|tx| {
        save_identity(tx, &identity)?;
        mark_join_ready(tx, 1)
    })
    .expect("save the device");
}

fn is_unsupported(error: &BackendError) -> bool {
    matches!(error.kind, ErrorKind::Unsupported)
}

/// The answer on `answer`: it must come in time and must not be a dropped reply.
async fn answered<T>(answer: oneshot::Receiver<T>, what: &str) -> T {
    match tokio::time::timeout(ANSWER, answer).await {
        Err(_) => panic!("{what}: no answer in time"),
        Ok(Err(_)) => panic!("{what}: the actor dropped the reply instead of answering it"),
        Ok(Ok(value)) => value,
    }
}

/// A `Reply<T>` command must be answered with the unsupported error, never dropped.
async fn assert_unsupported<T: std::fmt::Debug>(
    answer: oneshot::Receiver<Result<T, BackendError>>,
    what: &str,
) {
    match answered(answer, what).await {
        Err(e) if is_unsupported(&e) => {}
        Ok(_) => panic!("{what}: answered Ok, not with the unsupported error"),
        Err(_) => panic!("{what}: failed with another error than the unsupported one"),
    }
}

fn outgoing() -> OutgoingRequest {
    OutgoingRequest {
        conv: conv(),
        local: Arc::from("0102030405060708"),
        sent_at: 1_761_350_400_000,
        kind: OutgoingKind::Message {
            body: Body::text("synthetic th1 text"),
            reply_to: None,
            expiry: None,
        },
        only: None,
        is_recipient_update: false,
    }
}

/// Lane decision M3: every command that carries a reply is answered, each with what its type says
/// (`Reply<T>`: the unsupported error; `Transmit`: a failed outcome; `AbortTransmit`: `TooLate`);
/// a dropped reply fails the test at once. A login fails at its first step, `Connect` leaves the
/// account offline, and a shutdown ends the actor with the store closed.
async fn every_reply_is_answered_and_a_shutdown_ends_cleanly() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut actor = start_actor(dir.path(), StartMode::Fresh, TEST_KEY);

    let (reply, answer) = oneshot::channel();
    actor
        .send(Command::ChangeConversation {
            conv: conv(),
            change: ConversationChange::SetMarkedUnread {
                marked_unread: true,
            },
            id: None,
            reply,
        })
        .await;
    assert_unsupported(answer, "ChangeConversation").await;
    let (reply, answer) = oneshot::channel();
    actor
        .send(Command::ChangeContact {
            user: UserId::parse("threema:TH1ZZ9YX").expect("id"),
            change: ContactChange::SetNickname { nickname: None },
            reply,
        })
        .await;
    assert_unsupported(answer, "ChangeContact").await;
    let (reply, answer) = oneshot::channel();
    actor
        .send(Command::ChangeAccountSettings {
            patch: AccountSettingsPatch::default(),
            reply,
        })
        .await;
    assert_unsupported(answer, "ChangeAccountSettings").await;
    let (reply, answer) = oneshot::channel();
    actor
        .send(Command::ChangeProfile {
            patch: ProfilePatch::default(),
            reply,
        })
        .await;
    assert_unsupported(answer, "ChangeProfile").await;
    // A10a: Threema states no roaming flag, so the switch is refused like every other request.
    let (reply, answer) = oneshot::channel();
    actor
        .send(Command::SetRoamingWrites { on: true, reply })
        .await;
    assert_unsupported(answer, "SetRoamingWrites").await;
    let (reply, answer) = oneshot::channel();
    actor
        .send(Command::SubmitChallenge {
            token: None,
            answer: Redacted::new("synthetic-answer".to_owned()),
            reply,
        })
        .await;
    assert_unsupported(answer, "SubmitChallenge").await;
    let (reply, answer) = oneshot::channel();
    actor
        .send(Command::Resolve {
            id: Identifier {
                kind: IdentifierKind::NetworkId,
                value: "TH1ZZ9YX".to_owned(),
            },
            reply,
        })
        .await;
    assert_unsupported(answer, "Resolve").await;
    let (reply, answer) = oneshot::channel();
    actor.send(Command::ListOwnDevices { reply }).await;
    assert_unsupported(answer, "ListOwnDevices").await;
    let (reply, answer) = oneshot::channel();
    actor
        .send(Command::SetVerified {
            user: UserId::parse("threema:TH1QW7KZ").expect("id"),
            verified: true,
            key: IdentityFingerprint(vec![7; 32]),
            reply,
        })
        .await;
    assert_unsupported(answer, "SetVerified").await;
    // P5a: no safety number, and a confirmation is an approval with nothing to do.
    let (reply, answer) = oneshot::channel();
    actor
        .send(Command::SafetyNumber {
            user: UserId::parse("threema:TH1QW7KZ").expect("id"),
            reply,
        })
        .await;
    let answer = tokio::time::timeout(Duration::from_secs(5), answer).await;
    assert!(
        matches!(
            answer,
            Ok(Ok(Ok(Err(kourier_model::SafetyUnavailable::Unsupported))))
        ),
        "SafetyNumber is answered: no safety number on Threema"
    );
    let (reply, answer) = oneshot::channel();
    actor
        .send(Command::ConfirmIdentity {
            user: UserId::parse("threema:TH1QW7KZ").expect("id"),
            key: IdentityFingerprint(vec![7; 32]),
            reply,
        })
        .await;
    let answer = tokio::time::timeout(Duration::from_secs(5), answer).await;
    assert!(
        matches!(answer, Ok(Ok(Ok(())))),
        "ConfirmIdentity is answered as an approval with nothing to do"
    );

    let (reply, answer) = oneshot::channel();
    actor
        .send(Command::Transmit {
            local: MessageId(1),
            req: Box::new(outgoing()),
            reply,
        })
        .await;
    match answered(answer, "Transmit").await {
        TransmitOutcome::Failed(e) if is_unsupported(&e) => {}
        TransmitOutcome::Failed(_) => {
            panic!("Transmit: failed with another error than unsupported")
        }
        _ => panic!("Transmit: the outcome is not Failed"),
    }
    let (reply, answer) = oneshot::channel();
    actor
        .send(Command::AbortTransmit {
            local: MessageId(1),
            reply,
        })
        .await;
    assert_eq!(
        answered(answer, "AbortTransmit").await,
        AbortAnswer::TooLate,
        "nothing uploads, so nothing can be stopped"
    );

    // A download is answered too: the core waits for its `AttachmentDone`, here the contract's
    // failure form, durable (an ack comes with it).
    actor
        .send(Command::DownloadAttachment {
            attachment: AttachmentId(7),
            request: DownloadId(9),
            remote: BackendBlob::new(1, vec![1, 2, 3]),
            dest: BlobTarget {
                path: dir.path().join("blobs").join("download"),
                key: BlobKey::new([0x42; 32]),
            },
            net_ids: Redacted::new(Vec::new()),
        })
        .await;
    let envelope = actor.next_envelope().await;
    let ack = envelope.ack.expect("an AttachmentDone is durable");
    match envelope.events.as_slice() {
        [
            BackendEvent::AttachmentDone {
                attachment,
                request,
                result,
            },
        ] => {
            assert!(
                *attachment == AttachmentId(7) && *request == Some(DownloadId(9)),
                "the answer names another download"
            );
            assert!(
                matches!(result, Err(e) if is_unsupported(e)),
                "the download was not refused as unsupported"
            );
        }
        _ => panic!("the download was not answered with one AttachmentDone"),
    }
    ack.complete(Ok(()));

    // A10a: the core's own durable change of a chat has no reply: it is confirmed with
    // `CommandPersisted` in a durable envelope (an ack comes with it) and dropped, or the core would
    // send it again at every start.
    actor
        .send(Command::ChangeConversationDurable {
            cmd: kourier_model::CmdId(31),
            conv: conv(),
            change: ConversationChange::SetMarkedUnread {
                marked_unread: false,
            },
            asked_at: 1_761_350_400_000,
        })
        .await;
    let envelope = actor.next_envelope().await;
    let ack = envelope
        .ack
        .expect("a CommandPersisted is durable: it carries an ack");
    assert!(
        matches!(
            envelope.events.as_slice(),
            [BackendEvent::CommandPersisted { cmd }] if *cmd == kourier_model::CmdId(31)
        ),
        "the durable change was not confirmed with its own command id"
    );
    ack.complete(Ok(()));

    // A login fails at once at its first step, with the typed error; nothing was linked. The guard
    // parts carry no allow-list, so B1's gate refuses it (TH4 J3: `NotTheExpectedAccount` at `qr`,
    // before the parts' maker is called).
    actor
        .send(Command::StartLogin {
            flow: "qr".to_owned(),
            relink: None,
            taken: Vec::new(),
        })
        .await;
    let envelope = actor.next_envelope().await;
    assert!(envelope.ack.is_none(), "a login step is ephemeral");
    match envelope.events.as_slice() {
        [BackendEvent::Login(step)] => {
            assert_eq!(step.step_id, "qr");
            assert!(
                matches!(
                    &step.kind,
                    LoginStepKind::Failed { error } if error.kind == ErrorKind::NotTheExpectedAccount
                ),
                "the login did not fail at B1's gate"
            );
        }
        _ => panic!("the actor sent something other than one login step"),
    }
    // `Connect` keeps the account offline; a request after it shows the actor has taken it.
    actor.send(Command::Connect).await;
    let (reply, answer) = oneshot::channel();
    actor.send(Command::ListOwnDevices { reply }).await;
    assert_unsupported(answer, "ListOwnDevices after Connect").await;
    assert_eq!(
        *actor.ends.conn.borrow(),
        ConnState::Offline { retry_at_ms: None },
        "a connect changed the connection state"
    );

    actor.send(Command::Shutdown).await;
    assert_eq!(actor.exit().await, Exit::Shutdown);
    // The store was closed before the exit was reported: no connection holds the file, so the WAL
    // is gone.
    assert!(
        !dir.path().join("backend.db-wal").exists(),
        "the store outlived the actor"
    );
}

async fn cancel_ends_the_actor_like_a_shutdown_and_a_logout_ends_logged_out() {
    let dir = tempfile::tempdir().expect("temp dir");
    let actor = start_actor(dir.path(), StartMode::Fresh, TEST_KEY);
    actor.cancel.cancel();
    assert_eq!(actor.exit().await, Exit::Shutdown);

    // An account without a device: the logout reports that there was none.
    let mut actor = start_actor(dir.path(), StartMode::Resume, TEST_KEY);
    actor.send(Command::Logout).await;
    match actor.next_envelope().await.events.as_slice() {
        [BackendEvent::Logout(report)] => {
            assert_eq!(report.outcome, DeviceLogout::NoDevice);
            assert!(
                report.device_name.is_none(),
                "a device name without a device"
            );
        }
        _ => panic!("the logout was not reported as one event"),
    }
    assert!(matches!(actor.exit().await, Exit::LoggedOut { .. }));
}

async fn a_wrong_key_ends_the_actor_failed_and_leaves_the_file_as_it_was() {
    let dir = tempfile::tempdir().expect("temp dir");
    drop(kourier_threema::store::open(&db_path(dir.path()), &key()).expect("make the file"));
    let before = std::fs::read(db_path(dir.path())).expect("read");
    let mut wrong = TEST_KEY;
    wrong[0] ^= 1;
    let exit = start_actor(dir.path(), StartMode::Resume, wrong)
        .exit()
        .await;
    assert!(matches!(exit, Exit::Failed(_)), "a wrong key did not fail");
    assert!(
        std::fs::read(db_path(dir.path())).expect("read") == before,
        "a wrong key changed the file"
    );
    drop(kourier_threema::store::open(&db_path(dir.path()), &key()).expect("the file is intact"));
}

async fn a_fresh_start_never_lands_on_a_linked_device_and_a_resume_needs_the_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    store_with_a_device(dir.path());
    let before = std::fs::read(db_path(dir.path())).expect("read");
    let exit = start_actor(dir.path(), StartMode::Fresh, TEST_KEY)
        .exit()
        .await;
    assert!(
        matches!(exit, Exit::Failed(_)),
        "a fresh start over a linked device was not refused"
    );
    assert!(
        std::fs::read(db_path(dir.path())).expect("read") == before,
        "the refused fresh start changed the file"
    );
    // A resume goes on; its logout (RP-b) asks for ONE socket to drop the device, the guard refuses
    // it, and the report says so, with the label.
    let mut actor = start_actor(dir.path(), StartMode::Resume, TEST_KEY);
    let (made, dials) = (actor.made.load(Ordering::SeqCst), guards::dials());
    actor.send(Command::Logout).await;
    match actor.next_envelope().await.events.as_slice() {
        [BackendEvent::Logout(report)] => {
            assert_eq!(report.outcome, DeviceLogout::NotReached);
            assert!(
                report.device_name.as_deref() == Some(LINKED_LABEL),
                "the logout did not name the device's label"
            );
        }
        _ => panic!("the logout was not reported as one event"),
    }
    assert_eq!(
        (actor.made.load(Ordering::SeqCst), guards::dials()),
        (made + 1, dials + 1),
        "the logout did not make one client and ask for one socket"
    );
    assert!(matches!(actor.exit().await, Exit::LoggedOut { .. }));

    // A resume without the file is refused and does not make one.
    let empty = tempfile::tempdir().expect("temp dir");
    let exit = start_actor(empty.path(), StartMode::Resume, TEST_KEY)
        .exit()
        .await;
    assert!(
        matches!(exit, Exit::Failed(_)),
        "a resume without a file went on"
    );
    assert!(!db_path(empty.path()).exists(), "a resume made a new file");
}

/// `Rebuild` states the stored device again, as the first durable envelope, before anything else.
async fn a_rebuild_states_the_device_again_first() {
    let dir = tempfile::tempdir().expect("temp dir");
    store_with_a_device(dir.path());
    let mut actor = start_actor(dir.path(), StartMode::Rebuild, TEST_KEY);
    let envelope = actor.next_envelope().await;
    let ack = envelope.ack.expect("AccountReady is durable");
    match envelope.events.as_slice() {
        [
            BackendEvent::AccountReady {
                own_ids,
                device,
                display_name,
                number,
            },
        ] => {
            assert!(
                own_ids.as_slice() == [UserId::parse("threema:TH1QW7KZ").expect("id")],
                "AccountReady names another identity"
            );
            assert_eq!(*device, None, "no device id is reported before P5 (L10)");
            assert!(display_name.is_none() && number.is_none());
        }
        _ => panic!("the first envelope of a rebuild is not AccountReady alone"),
    }
    ack.complete(Ok(()));
    actor.send(Command::Shutdown).await;
    assert_eq!(actor.exit().await, Exit::Shutdown);
}

/// `Rebuild` over a JOINED file (TH4 J4b) states the account, its settings, its contacts and its
/// groups again BEFORE anything else: the first durable envelopes carry exactly the events the import
/// states (`store::import_events`, `join::events` in memory), `AccountReady` first.
async fn a_rebuild_over_a_joined_file_states_its_contacts_and_groups_first() {
    let dir = tempfile::tempdir().expect("temp dir");
    let fixture = crate::store::join_fixture("J4BNETW1");
    let db = kourier_threema::store::open(&db_path(dir.path()), &key()).expect("make the file");
    kourier_threema::store::import_join(
        &db,
        &fixture.import,
        crate::store::JOIN_LABEL,
        crate::store::JOIN_NOW_MS,
    )
    .expect("zone 1");
    // A Rebuild comes after a finished join (S4): zone 2's marker is there.
    db.zone(|tx| kourier_threema::store::mark_join_ready(tx, crate::store::JOIN_NOW_MS))
        .expect("the marker");
    drop(db);
    let expected: Vec<BackendEvent> = kourier_threema::store::import_events(&fixture.import)
        .expect("the import's events")
        .into_iter()
        .flatten()
        .collect();
    let mut actor = start_actor(dir.path(), StartMode::Rebuild, TEST_KEY);
    let mut got = Vec::new();
    while got.len() < expected.len() {
        let envelope = actor.next_envelope().await;
        let ack = envelope.ack.expect("the rebuild's events are durable");
        got.extend(envelope.events);
        ack.complete(Ok(()));
    }
    assert!(
        matches!(got.first(), Some(BackendEvent::AccountReady { .. })),
        "the rebuild did not start with AccountReady"
    );
    assert!(
        got == expected,
        "the rebuild's first envelopes are not the joined file's account, settings, contacts and groups"
    );
    actor.send(Command::Shutdown).await;
    assert_eq!(actor.exit().await, Exit::Shutdown);
}

/// S4: a file with an identity but WITHOUT zone 2's marker is a join that stopped between its zones.
/// A `Resume` and a `Rebuild` of it do not state the account as linked: the actor ends `LoggedOut` (not
/// linked) at its start, and `core_outbox` stays empty, nothing is connected, the file is as it was. The
/// same file with the marker (a finished join) is a `Rebuild` that states its account (the test above).
async fn an_identity_without_the_marker_is_not_stated_as_a_linked_account() {
    let dir = tempfile::tempdir().expect("temp dir");
    let fixture = crate::store::join_fixture("J4BNOMRK");
    {
        let db = kourier_threema::store::open(&db_path(dir.path()), &key()).expect("make the file");
        kourier_threema::store::import_join(
            &db,
            &fixture.import,
            crate::store::JOIN_LABEL,
            crate::store::JOIN_NOW_MS,
        )
        .expect("zone 1");
        assert!(
            !db.read(kourier_threema::store::is_join_ready)
                .expect("the marker"),
            "zone 1 alone wrote the marker"
        );
    }
    for mode in [StartMode::Rebuild, StartMode::Resume] {
        let exit = start_actor(dir.path(), mode, TEST_KEY).exit().await;
        assert!(
            matches!(&exit, Exit::LoggedOut { reason } if reason == "the link was never finished"),
            "an identity without the marker did not end the actor logged out"
        );
        let db = kourier_threema::store::open(&db_path(dir.path()), &key()).expect("reopen");
        let rows: i64 = db
            .read(|conn: &kourier_db::rusqlite::Connection| {
                conn.query_row("SELECT count(*) FROM core_outbox", [], |r| r.get(0))
                    .map_err(kourier_db::Error::from)
            })
            .expect("count");
        assert_eq!(rows, 0, "the unfinished join was stated to the core");
        assert!(
            !db.read(kourier_threema::store::is_join_ready)
                .expect("the marker"),
            "the start wrote the marker"
        );
    }
}

/// An actor that is asked to link and to connect, under a configuration it cannot honour too, makes
/// no client part: nothing dials in TH1, and the login names the configuration.
async fn an_actor_makes_no_client_part_whatever_it_is_asked() {
    let dir = tempfile::tempdir().expect("temp dir");
    let staging = BackendConfig {
        env: BackendEnv::Staging,
        ..BackendConfig::default()
    };
    let mut actor = start_actor_with(dir.path(), StartMode::Fresh, TEST_KEY, staging);
    let made = actor.made.clone();
    // The counter is the child's: the logout of a stored device before this made one (RP-b).
    let before = made.load(Ordering::SeqCst);
    actor
        .send(Command::StartLogin {
            flow: "qr".to_owned(),
            relink: None,
            taken: Vec::new(),
        })
        .await;
    match actor.next_envelope().await.events.as_slice() {
        [BackendEvent::Login(step)] => assert!(
            matches!(&step.kind, LoginStepKind::Failed { error }
                if is_unsupported(error) && error.detail.contains("real service")),
            "the login did not name the configuration"
        ),
        _ => panic!("the actor sent something other than one login step"),
    }
    actor.send(Command::Connect).await;
    actor.send(Command::NetworkChanged).await;
    actor.send(Command::Shutdown).await;
    assert_eq!(actor.exit().await, Exit::Shutdown);
    assert_eq!(
        made.load(Ordering::SeqCst),
        before,
        "the actor made a client part"
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
    eprintln!("every_reply_is_answered_and_a_shutdown_ends_cleanly");
    every_reply_is_answered_and_a_shutdown_ends_cleanly().await;
    eprintln!("cancel_ends_the_actor_like_a_shutdown_and_a_logout_ends_logged_out");
    cancel_ends_the_actor_like_a_shutdown_and_a_logout_ends_logged_out().await;
    eprintln!("a_wrong_key_ends_the_actor_failed_and_leaves_the_file_as_it_was");
    a_wrong_key_ends_the_actor_failed_and_leaves_the_file_as_it_was().await;
    eprintln!("a_fresh_start_never_lands_on_a_linked_device_and_a_resume_needs_the_file");
    a_fresh_start_never_lands_on_a_linked_device_and_a_resume_needs_the_file().await;
    eprintln!("a_rebuild_states_the_device_again_first");
    a_rebuild_states_the_device_again_first().await;
    eprintln!("a_rebuild_over_a_joined_file_states_its_contacts_and_groups_first");
    a_rebuild_over_a_joined_file_states_its_contacts_and_groups_first().await;
    eprintln!("an_identity_without_the_marker_is_not_stated_as_a_linked_account");
    an_identity_without_the_marker_is_not_stated_as_a_linked_account().await;
    eprintln!("an_actor_makes_no_client_part_whatever_it_is_asked");
    an_actor_makes_no_client_part_whatever_it_is_asked().await;
    // RP-b: the one dial and the one client are the stored device's logout (refused by the guard).
    assert_eq!(guards::dials(), 1, "a dial but the logout's was attempted");
    assert_eq!(guards::http_calls(), 0, "an HTTP request was attempted");
    assert_eq!(
        made_counter().load(Ordering::SeqCst),
        1,
        "an actor but the logout's made a client part"
    );
}

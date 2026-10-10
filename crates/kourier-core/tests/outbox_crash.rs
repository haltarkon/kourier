//! Kill tests of the send path and of durable read syncs (§4.8, §10.4, S5 "done when"). The
//! whole stack (core, writer, supervisor, dispatcher, fake backend with its server log) runs in a
//! child process that aborts at the n-th hit of a failpoint, like a power cut. A clean second run
//! finishes the work; then:
//!
//! - send: the group message went out with the id allocated at enqueue, every recipient got it
//!   exactly once (the fake server's log), and `core.db` shows it sent to both members;
//! - read: the read sync and the read receipts reached the backend's job table and no
//!   `pending_commands` row is left;
//! - held: a message whose send failed for everyone, a reaction made to it meanwhile and the
//!   user's "Retry": the recipient got the message once and the reaction once, the message
//!   first, and the reaction is still there.
//!
//! - split: four files sent in one go, which the fake network shows only as three messages
//!   (an album of two pictures, a document, a picture). After a kill anywhere, in the enqueue
//!   transaction, right after it or on the send path, either none of the three messages
//!   exists or all of them do (never a part of the send); the restart sends each once, to
//!   each recipient once, in the user's order, with the ids from the enqueue; and no blob of a
//!   cut-off import stays on disk.
//!
//! - untrusted: a group message one member of which has a new safety number. The others get
//!   it at once, the member after the user confirmed the number (or after the restart, which
//!   tries the waiting row once). After a kill anywhere on the send path each recipient has
//!   it exactly once, with the id from the enqueue.
//!
//! - clear: the user clears a chat. The delete and the durable command that tells the other
//!   devices are one transaction; after a kill right behind it, or behind any commit of the
//!   backend, the chat is still empty, what went stays remembered, and the backend ends up
//!   with the command exactly once, naming the messages the chat had before.
//!
//! - staged: 150 documents sent in one go, which the core stores in three transactions so
//!   the writer is free in between. After a kill inside any of them, between them or right
//!   behind the last, either none of the 150 messages exists or all of them do; the restart
//!   sends each once, in the user's order, and no blob of the cut-off send stays on disk.
//!
//! - challenge: a send that the network answers with a challenge. After a kill behind the
//!   transaction that recorded the answer, the restart takes the wait up: the account shows
//!   the challenge without sending anything into it first, and the user's answer (with the
//!   token of the send that was challenged) lets the message through, once. After a kill
//!   behind the write that forgot the challenge the network accepted the answer to, the
//!   restart shows no challenge and sends the message.
//!
//! Every failpoint is tried at every hit count until a run finishes without hitting it.

mod support;

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kourier_backend_api::{NetworkRegistry, SystemClock};
use kourier_core::facade::{Query, QueryData, UiCommand};
use kourier_core::testing::DirVault;
use kourier_core::{AccountVault, Core, CoreConfig};
use kourier_fake::{FakeNetwork, Flavor, TransmitRule};
use kourier_model::{AccountState, AccountUuid, NetworkKind, OutgoingState};
use support::send::*;
use support::{CORE_KEY, eventually};

const CHILD_HOME: &str = "KOURIER_TEST_OUTBOX_CRASH_HOME";
const CHILD_MODE: &str = "KOURIER_TEST_OUTBOX_CRASH_MODE";
const SIGABRT: i32 = 6;
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);
const WAIT: Duration = Duration::from_secs(60);

/// Kill points of the send path: after an attempt is recorded, after the fake server accepted
/// (outcome not yet seen by the core), inside the outcome transaction.
const SEND_FAILPOINTS: &[&str] = &[
    kourier_core::FAILPOINT_AFTER_ATTEMPT,
    kourier_fake::FAILPOINT_AFTER_SERVER_ACCEPT,
    kourier_core::FAILPOINT_BEFORE_OUTCOME_COMMIT,
];

/// Kill points of a read sync: after the `pending_commands` row, and after any backend commit
/// (including the one that persists the command and emits `CommandPersisted`).
const READ_FAILPOINTS: &[&str] = &[
    kourier_core::FAILPOINT_AFTER_PENDING_COMMAND,
    kourier_fake::FAILPOINT_AFTER_BACKEND_EFFECTS_COMMIT,
];

/// Kill points of a send that becomes several messages: inside the enqueue transaction (after
/// each message), right after its commit, then the send path of each message.
const SPLIT_FAILPOINTS: &[&str] = &[
    kourier_core::FAILPOINT_IN_ENQUEUE,
    kourier_core::FAILPOINT_AFTER_ENQUEUE,
    kourier_core::FAILPOINT_AFTER_ATTEMPT,
    kourier_fake::FAILPOINT_AFTER_SERVER_ACCEPT,
    kourier_core::FAILPOINT_BEFORE_OUTCOME_COMMIT,
];

/// How many documents the staged send has: more than two transactions of
/// [`kourier_core::ENQUEUE_CHUNK`] messages.
const STAGED_FILES: usize = 150;

/// Where the staged send is killed: inside its first, second and last transaction, behind
/// each of the first two, and right behind the last.
const STAGED_KILLS: &[(&str, usize)] = &[
    (kourier_core::FAILPOINT_IN_ENQUEUE, 1),
    (kourier_core::FAILPOINT_AFTER_STAGED, 1),
    (kourier_core::FAILPOINT_IN_ENQUEUE, 70),
    (kourier_core::FAILPOINT_AFTER_STAGED, 2),
    (kourier_core::FAILPOINT_IN_ENQUEUE, STAGED_FILES),
    (kourier_core::FAILPOINT_AFTER_ENQUEUE, 1),
];

const CHALLENGE_URL: &str = "https://example.invalid/challenge";
/// Where the challenge child notes how many attempts the fake server had seen when the
/// challenge came on show.
const ATTEMPTS_FILE: &str = "attempts-when-shown";

/// The files of the split send, in the user's order, and the messages the fake network makes
/// of them (pictures share a message, a document travels alone).
const SPLIT_FILES: [&str; 4] = ["one.jpg", "two.jpg", "notes.pdf", "three.jpg"];
const SPLIT_MESSAGES: [&[&str]; 3] = [&["one.jpg", "two.jpg"], &["notes.pdf"], &["three.jpg"]];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Send,
    Read,
    Held,
    Split,
    Untrusted,
    Clear,
    Staged,
    Challenge,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::Read => "read",
            Self::Held => "held",
            Self::Split => "split",
            Self::Untrusted => "untrusted",
            Self::Clear => "clear",
            Self::Staged => "staged",
            Self::Challenge => "challenge",
        }
    }
}

/// The child: start (or resume) the core, link if there is no account, wait for the script,
/// then send one group message (send) or read alice's chat (read) unless that already
/// happened, and wait until it is done.
#[test]
fn outbox_crash_child() {
    let (Ok(home), Ok(mode)) = (std::env::var(CHILD_HOME), std::env::var(CHILD_MODE)) else {
        return;
    };
    let mode = match mode.as_str() {
        "send" => Mode::Send,
        "read" => Mode::Read,
        "split" => Mode::Split,
        "untrusted" => Mode::Untrusted,
        "clear" => Mode::Clear,
        "staged" => Mode::Staged,
        "challenge" => Mode::Challenge,
        _ => Mode::Held,
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter("kourier_core=debug,kourier_fake=debug,kourier_db=info")
        .with_writer(std::io::stderr)
        .try_init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(child(PathBuf::from(home), mode));
}

fn child_script(mode: Mode) -> kourier_fake::Script {
    match mode {
        Mode::Send => script("send-crash")
            .fail_transmit(TransmitRule::member(bob(), rate_limited(None), 1).in_conv(group())),
        Mode::Read => script("read-crash"),
        Mode::Split => script("split-crash"),
        Mode::Staged => script("staged-crash"),
        Mode::Clear => script("clear-crash"),
        // Every send is challenged until the answer is accepted (the fake remembers the
        // answer in its `backend.db`).
        Mode::Challenge => script("challenge-crash")
            .fail_transmit(TransmitRule::challenge(CHALLENGE_URL))
            .accept_challenge_answer("solved"),
        // Bob's safety number changed (once: the rule's count is durable).
        Mode::Untrusted => script("untrusted-crash")
            .fail_transmit(TransmitRule::member(bob(), untrusted(bob()), 1).in_conv(group())),
        // The first send to bob is refused for good (once: the rule's count is durable).
        Mode::Held => script("held-crash").fail_transmit(
            TransmitRule::attempts(unregistered(), 1)
                .of_kind("Message")
                .in_conv(d_bob()),
        ),
    }
}

async fn child(home: PathBuf, mode: Mode) {
    let fake = FakeNetwork::new(Flavor::SignalLike, child_script(mode));
    let control = fake.control();
    let data = home.join("data");
    std::fs::create_dir_all(&data).expect("data dir");
    let vault = Arc::new(DirVault::new(&data));
    let networks = NetworkRegistry::new()
        .with(Arc::new(fake))
        .expect("registry");
    let cfg = CoreConfig::new(
        data.join("core.db"),
        kourier_db::DbKey::from(CORE_KEY),
        networks,
        vault,
        Arc::new(SystemClock),
        tokio::runtime::Handle::current(),
    );
    let core = Core::start(cfg).await.expect("core");
    let h = core.handle();
    h.send(UiCommand::UiReady).expect("send");
    let active = |h: kourier_core::facade::CoreHandle| async move {
        match h.query(Query::Accounts).await.expect("accounts") {
            QueryData::Accounts(a) => a
                .into_iter()
                .find(|a| a.state == AccountState::Active)
                .map(|a| a.uuid),
            _ => None,
        }
    };
    if active(h.clone()).await.is_none() {
        h.send(UiCommand::AddAccount {
            network: NetworkKind::Fake,
            flow: "qr".into(),
        })
        .expect("send");
    }
    let h2 = h.clone();
    eventually("an active account", WAIT, || {
        let h = h2.clone();
        async move { active(h).await.is_some() }
    })
    .await;
    let uuid = active(h.clone()).await.expect("active");
    tokio::time::timeout(WAIT, control.wait_for(|c| c.is_finished(uuid)))
        .await
        .expect("script finished");
    let db = data.join("core.db");
    match mode {
        Mode::Send => {
            let g = conv_id(&db, &group()).expect("group");
            if own_messages(&db, g).is_empty() {
                h.send(UiCommand::SendText {
                    conv: g,
                    text: kourier_model::Text::plain("crash-safe hello"),
                    reply_to: None,
                })
                .expect("send");
            }
            eventually("the message sent", WAIT, || {
                let db = db.clone();
                async move {
                    outbox_len(&db) == 0
                        && own_messages(&db, g).first().is_some_and(|m| {
                            matches!(m.out_state, Some(OutgoingState::Sent { .. }))
                        })
                }
            })
            .await;
        }
        Mode::Untrusted => {
            let g = conv_id(&db, &group()).expect("group");
            if own_messages(&db, g).is_empty() {
                h.send(UiCommand::SendText {
                    conv: g,
                    text: kourier_model::Text::plain("crash-safe hello"),
                    reply_to: None,
                })
                .expect("send");
            }
            let account = match h.query(Query::Accounts).await.expect("accounts") {
                QueryData::Accounts(a) => a.first().expect("an account").id,
                _ => panic!("not an account list"),
            };
            eventually("the message sent to everyone", WAIT, || {
                let (db, h) = (db.clone(), h.clone());
                async move {
                    // The user confirms the new safety number whenever the send waits for it.
                    if waits_for_confirmation(&db) {
                        h.send(UiCommand::ConfirmIdentity {
                            account,
                            user: bob(),
                        })
                        .expect("send");
                    }
                    outbox_len(&db) == 0
                        && own_messages(&db, g).first().is_some_and(|m| {
                            matches!(m.out_state, Some(OutgoingState::Sent { .. }))
                        })
                }
            })
            .await;
        }
        Mode::Read => {
            let a = conv_id(&db, &d_alice()).expect("alice");
            if unread(&db, a) > 0 {
                h.send(UiCommand::MarkRead { conv: a }).expect("send");
            }
            eventually("the read sync persisted", WAIT, || {
                let db = db.clone();
                async move { unread(&db, a) == 0 && pending_commands(&db) == 0 }
            })
            .await;
        }
        Mode::Clear => {
            let a = conv_id(&db, &d_alice()).expect("alice");
            // Only a run that still finds the messages clears: after a kill behind the
            // delete's commit, the restart must get the command to the backend by itself.
            if messages_in(&db, a) > 0 {
                h.send(UiCommand::ClearConversation { conv: a })
                    .expect("send");
            }
            eventually("the clear reached the backend", WAIT, || {
                let db = db.clone();
                async move { messages_in(&db, a) == 0 && pending_commands(&db) == 0 }
            })
            .await;
        }
        Mode::Split => {
            let g = conv_id(&db, &group()).expect("group");
            // The user's files, outside the profile (written once; a restart finds them).
            let src = home.join("src");
            std::fs::create_dir_all(&src).expect("sources");
            let paths: Vec<PathBuf> = SPLIT_FILES
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let p = src.join(name);
                    if !p.exists() {
                        let bytes = if name.ends_with(".jpg") {
                            kourier_media::sample_image(
                                64,
                                48,
                                u64::try_from(i).unwrap_or(0) + 1,
                                kourier_media::SampleFormat::Jpeg,
                            )
                            .expect("jpeg")
                        } else {
                            format!("the content of {name}").into_bytes()
                        };
                        std::fs::write(&p, bytes).expect("write");
                    }
                    p
                })
                .collect();
            // All or none: a kill never leaves a part of the send.
            let stored = own_messages(&db, g).len();
            assert!(
                stored == 0 || stored == SPLIT_MESSAGES.len(),
                "a part of the send was stored: {stored} messages"
            );
            if stored == 0 {
                h.send(UiCommand::SendFiles {
                    request: Default::default(),
                    as_file: Vec::new(),
                    conv: g,
                    paths: paths.into(),
                    caption: Some(kourier_model::Text::plain("crash-safe files")),
                    reply_to: None,
                })
                .expect("send");
            }
            eventually("every message of the send is sent", WAIT, || {
                let db = db.clone();
                async move {
                    let own = own_messages(&db, g);
                    outbox_len(&db) == 0
                        && own.len() == SPLIT_MESSAGES.len()
                        && own
                            .iter()
                            .all(|m| matches!(m.out_state, Some(OutgoingState::Sent { .. })))
                }
            })
            .await;
        }
        Mode::Staged => {
            let a = conv_id(&db, &d_alice()).expect("alice");
            let src = home.join("src");
            std::fs::create_dir_all(&src).expect("sources");
            let paths: Vec<PathBuf> = (0..STAGED_FILES)
                .map(|i| {
                    let p = src.join(format!("doc-{i:03}.txt"));
                    if !p.exists() {
                        std::fs::write(&p, format!("document {i}")).expect("write");
                    }
                    p
                })
                .collect();
            // All or none: a kill never leaves a part of the send, not even one that was
            // committed as its first transactions.
            let stored = own_messages(&db, a).len();
            assert!(
                stored == 0 || stored == STAGED_FILES,
                "a part of the send was stored: {stored} messages"
            );
            assert_eq!(
                staged_rows(&db),
                0,
                "a restart leaves no row of a send that was still being stored"
            );
            if stored == 0 {
                h.send(UiCommand::SendFiles {
                    request: Default::default(),
                    as_file: Vec::new(),
                    conv: a,
                    paths: paths.into(),
                    caption: None,
                    reply_to: None,
                })
                .expect("send");
            }
            eventually("every message of the send is sent", WAIT, || {
                let db = db.clone();
                async move {
                    let own = own_messages(&db, a);
                    outbox_len(&db) == 0
                        && own.len() == STAGED_FILES
                        && own
                            .iter()
                            .all(|m| matches!(m.out_state, Some(OutgoingState::Sent { .. })))
                }
            })
            .await;
        }
        Mode::Challenge => {
            let a = conv_id(&db, &d_alice()).expect("alice");
            if own_messages(&db, a).is_empty() {
                h.send(UiCommand::SendText {
                    conv: a,
                    text: kourier_model::Text::plain("crash-safe wait"),
                    reply_to: None,
                })
                .expect("send");
            }
            let account = match h.query(Query::Accounts).await.expect("accounts") {
                QueryData::Accounts(a) => a.first().expect("an account").id,
                _ => panic!("not an account list"),
            };
            let sent = |db: &Path| {
                outbox_len(db) == 0
                    && own_messages(db, a)
                        .first()
                        .is_some_and(|m| matches!(m.out_state, Some(OutgoingState::Sent { .. })))
            };
            // The account shows the challenge (or the message went out, in a run after one
            // whose answer was already accepted).
            eventually("the challenge is on show", WAIT, || {
                let (db, h) = (db.clone(), h.clone());
                async move { sent(&db) || shows_challenge(&h).await }
            })
            .await;
            if !sent(&db) {
                // How many attempts the server had seen from this process by then: none
                // when the wait was taken up from `core.db`.
                let seen = control.transmits(uuid).len();
                std::fs::write(home.join(ATTEMPTS_FILE), seen.to_string()).expect("note");
                // Nothing is sent while the challenge stands.
                tokio::time::sleep(Duration::from_millis(300)).await;
                assert_eq!(control.transmits(uuid).len(), seen, "held");
                h.send(UiCommand::SubmitChallenge {
                    account,
                    answer: "solved".to_owned().into(),
                })
                .expect("send");
            }
            eventually("the message is sent", WAIT, || {
                let db = db.clone();
                async move { sent(&db) }
            })
            .await;
        }
        Mode::Held => {
            // Every step looks at what is stored, so a run after a kill goes on where the
            // killed one stopped (and takes the other course when the kill changed it: a
            // send whose refusal the core never saw is simply sent by the restart).
            let b = conv_id(&db, &d_bob()).expect("bob");
            if own_messages(&db, b).is_empty() {
                h.send(UiCommand::SendText {
                    conv: b,
                    text: kourier_model::Text::plain("crash-safe change"),
                    reply_to: None,
                })
                .expect("send");
            }
            eventually("the first send is answered", WAIT, || {
                let db = db.clone();
                async move {
                    own_messages(&db, b).first().is_some_and(|m| {
                        send_row_state(&db, m.id).is_none_or(|state| state == "Failed")
                    })
                }
            })
            .await;
            let mine = own_messages(&db, b)[0].clone();
            if own_reactions(&db, mine.id) == 0 {
                h.send(UiCommand::React {
                    message: mine.id,
                    emoji: "👍".into(),
                    remove: false,
                })
                .expect("send");
            }
            eventually("the reaction is stored", WAIT, || {
                let db = db.clone();
                async move { own_reactions(&db, mine.id) == 1 }
            })
            .await;
            if send_row_state(&db, mine.id).as_deref() == Some("Failed") {
                h.send(UiCommand::RetryMessage { message: mine.id })
                    .expect("send");
            }
            eventually("the message and its reaction are sent", WAIT, || {
                let db = db.clone();
                async move {
                    outbox_len(&db) == 0
                        && own_messages(&db, b).first().is_some_and(|m| {
                            matches!(m.out_state, Some(OutgoingState::Sent { .. }))
                        })
                }
            })
            .await;
        }
    }
    core.shutdown().await;
}

/// Outbox rows of a send that is still being stored.
fn staged_rows(db: &Path) -> i64 {
    core_db(db, |c| {
        c.query_row(
            "SELECT count(*) FROM outbox WHERE state = 'Staged'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(-1)
    })
}

/// Whether the account shows a challenge to solve.
async fn shows_challenge(h: &kourier_core::facade::CoreHandle) -> bool {
    match h.query(Query::Accounts).await {
        Ok(QueryData::Accounts(a)) => a.iter().any(|a| {
            matches!(
                a.conn,
                kourier_model::ConnState::NeedsUserAction(
                    kourier_model::UserAction::SolveChallenge { .. }
                )
            )
        }),
        _ => false,
    }
}

/// Whether a send row waits for a safety number to be confirmed.
fn waits_for_confirmation(db: &Path) -> bool {
    core_db(db, |c| {
        c.query_row(
            "SELECT count(*) FROM outbox WHERE state = 'Queued'
               AND last_error = 'UntrustedIdentity'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n > 0)
    })
}

/// The state of the message's own send row; `None` once it is gone (sent).
fn send_row_state(db: &Path, message: kourier_model::MessageId) -> Option<String> {
    core_db(db, |c| {
        c.query_row(
            "SELECT state FROM outbox WHERE message_id = ?1 AND kind = 'Message'",
            [message.0],
            |r| r.get(0),
        )
        .ok()
    })
}

/// Our own reactions on `message` that are not removed.
fn own_reactions(db: &Path, message: kourier_model::MessageId) -> i64 {
    core_db(db, |c| {
        c.query_row(
            "SELECT count(*) FROM reactions WHERE message_id = ?1 AND user_id = ''
               AND removed = 0",
            [message.0],
            |r| r.get(0),
        )
        .unwrap_or(0)
    })
}

fn unread(db: &Path, conv: kourier_model::ConversationId) -> i64 {
    core_db(db, |c| {
        c.query_row(
            "SELECT unread FROM conversations WHERE id = ?1",
            [conv.0],
            |r| r.get(0),
        )
        .unwrap_or(1)
    })
}

fn messages_in(db: &Path, conv: kourier_model::ConversationId) -> i64 {
    core_db(db, |c| {
        c.query_row(
            "SELECT count(*) FROM messages WHERE conversation_id = ?1",
            [conv.0],
            |r| r.get(0),
        )
        .unwrap_or(1)
    })
}

fn pending_commands(db: &Path) -> i64 {
    core_db(db, |c| {
        c.query_row("SELECT count(*) FROM pending_commands", [], |r| r.get(0))
            .unwrap_or(1)
    })
}

fn run_child(home: &Path, mode: Mode, failpoint: Option<&str>) -> ExitStatus {
    let exe = std::env::current_exe().expect("test binary");
    let log = home.join(format!("child-{}.log", failpoint.unwrap_or("clean")));
    let mut cmd = Command::new(exe);
    cmd.args([
        "--exact",
        "outbox_crash_child",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(CHILD_HOME, home)
    .env(CHILD_MODE, mode.name())
    .env_remove(kourier_db::failpoint::ENV)
    .stdout(Stdio::null())
    .stderr(Stdio::from(std::fs::File::create(log).expect("log file")));
    if let Some(fp) = failpoint {
        cmd.env(kourier_db::failpoint::ENV, fp);
    }
    let mut child = cmd.spawn().expect("spawn child");
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            return status;
        }
        if start.elapsed() > CHILD_TIMEOUT {
            let _ = child.kill();
            panic!("child hung (failpoint {failpoint:?})");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn log_tail(home: &Path) -> String {
    let log = std::fs::read_to_string(home.join("child-clean.log")).unwrap_or_default();
    let tail: Vec<&str> = log.lines().rev().take(60).collect();
    tail.into_iter().rev().collect::<Vec<_>>().join("\n")
}

/// The account and its `backend.db` key, read like the core does.
fn account(home: &Path) -> (AccountUuid, kourier_backend_api::DbKey, PathBuf) {
    let data = home.join("data");
    let uuid: String = core_db(&data.join("core.db"), |c| {
        c.query_row("SELECT uuid FROM accounts", [], |r| r.get(0))
            .expect("one account")
    });
    let uuid: AccountUuid = uuid.parse().expect("uuid");
    let vault = DirVault::new(&data);
    let key = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
        .block_on(vault.unwrap(uuid))
        .expect("backend.db key");
    (uuid, key, vault.backend_db(uuid))
}

fn verify_send(home: &Path, spec: &str) {
    let db = home.join("data").join("core.db");
    let g = conv_id(&db, &group()).expect("group");
    let own = own_messages(&db, g);
    assert_eq!(own.len(), 1, "{spec}: one message, enqueued once");
    let msg = &own[0];
    assert!(
        matches!(msg.out_state, Some(OutgoingState::Sent { .. })),
        "{spec}: {:?}",
        msg.out_state
    );
    assert_eq!(outbox_len(&db), 0, "{spec}");
    let states = send_state(&db, msg.id);
    let mut both = [ALICE, BOB];
    both.sort_unstable();
    assert_eq!(
        states.iter().map(|(u, _)| u.as_str()).collect::<Vec<_>>(),
        both,
        "{spec}"
    );
    assert!(
        states.iter().all(|(_, s)| s.contains("Sent")),
        "{spec}: {states:?}"
    );
    let (_, key, backend) = account(home);
    let log = server_log(&backend, &key);
    assert!(
        log.iter()
            .all(|d| d.local == msg.local && d.sent_at == msg.sent_at),
        "{spec}: every copy carries the id from the enqueue: {log:?}"
    );
    let mut got: Vec<&str> = log.iter().map(|d| d.recipient.as_str()).collect();
    got.sort_unstable();
    assert_eq!(got, both, "{spec}: each recipient exactly once");
}

fn verify_read(home: &Path, spec: &str) {
    let db = home.join("data").join("core.db");
    let a = conv_id(&db, &d_alice()).expect("alice");
    assert_eq!(unread(&db, a), 0, "{spec}");
    assert_eq!(pending_commands(&db), 0, "{spec}");
    let (_, key, backend) = account(home);
    let jobs: Vec<String> = backend_db(&backend, &key)
        .prepare("SELECT kind FROM jobs ORDER BY cmd")
        .and_then(|mut s| {
            s.query_map([], |r| r.get(0))?
                .collect::<Result<Vec<String>, _>>()
        })
        .expect("jobs");
    assert_eq!(
        jobs.len(),
        2,
        "{spec}: the read sync and the read receipts reached the backend: {jobs:?}"
    );
}

fn verify_clear(home: &Path, spec: &str) {
    use kourier_backend_api::DurableCommand;
    let db = home.join("data").join("core.db");
    let a = conv_id(&db, &d_alice()).expect("alice");
    assert_eq!(messages_in(&db, a), 0, "{spec}: the chat is empty");
    assert_eq!(pending_commands(&db), 0, "{spec}: nothing is left to send");
    let remembered: i64 = core_db(&db, |c| {
        c.query_row("SELECT count(*) FROM expired_messages", [], |r| r.get(0))
            .expect("count")
    });
    assert_eq!(remembered, 1, "{spec}: what went is remembered");
    let (_, key, backend) = account(home);
    let jobs = kourier_fake::job_log(&backend, &kourier_db::DbKey::from_bytes(*key.expose()))
        .expect("jobs");
    let clears: Vec<&DurableCommand> = jobs
        .iter()
        .filter(|j| j.kind == "clear")
        .filter_map(|j| j.command.as_ref())
        .collect();
    match clears.as_slice() {
        [
            DurableCommand::ClearConversation {
                conv,
                newest,
                newest_non_expiring,
                remove,
            },
        ] => {
            assert_eq!(*conv, d_alice(), "{spec}");
            assert!(!remove, "{spec}");
            // The message the chat held before it was cleared: read in the transaction that
            // deleted it, not after a restart, when it was gone.
            let named: Vec<String> = newest.iter().map(|t| t.local.to_string()).collect();
            assert_eq!(
                named,
                [(kourier_fake::scripts::T0 - 60_000).to_string()],
                "{spec}"
            );
            assert_eq!(newest, newest_non_expiring, "{spec}");
        }
        other => panic!("{spec}: the backend has the clear exactly once: {other:?}"),
    }
}

fn verify_held(home: &Path, spec: &str) {
    let db = home.join("data").join("core.db");
    let b = conv_id(&db, &d_bob()).expect("bob");
    let own = own_messages(&db, b);
    assert_eq!(own.len(), 1, "{spec}: one message, enqueued once");
    let msg = &own[0];
    assert!(
        matches!(msg.out_state, Some(OutgoingState::Sent { .. })),
        "{spec}: {:?}",
        msg.out_state
    );
    assert_eq!(outbox_len(&db), 0, "{spec}");
    assert_eq!(
        own_reactions(&db, msg.id),
        1,
        "{spec}: the reaction is kept"
    );
    let (_, key, backend) = account(home);
    let log = server_log(&backend, &key);
    let to_bob: Vec<(&str, i64)> = log
        .iter()
        .filter(|d| d.recipient == bob())
        .map(|d| (d.local.as_str(), d.sent_at))
        .collect();
    assert_eq!(
        to_bob.len(),
        2,
        "{spec}: the message and the reaction, each exactly once: {log:?}"
    );
    assert_eq!(
        to_bob[0],
        (msg.local.as_str(), msg.sent_at),
        "{spec}: the message first, with the id from the enqueue: {log:?}"
    );
    assert_ne!(to_bob[1].0, msg.local, "{spec}: then the reaction: {log:?}");
    assert_eq!(log.len(), 2, "{spec}: nothing else was sent: {log:?}");
}

/// The file names of a stored message's attachments, in order.
fn attachment_names(db: &Path, message: kourier_model::MessageId) -> Vec<String> {
    core_db(db, |c| {
        let mut stmt = c
            .prepare("SELECT file_name FROM attachments WHERE message_id = ?1 ORDER BY idx")
            .expect("prepare");
        stmt.query_map([message.0], |r| r.get(0))
            .expect("query")
            .collect::<Result<Vec<String>, _>>()
            .expect("rows")
    })
}

fn files_on_disk(dir: &Path) -> usize {
    let mut n = 0;
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            if e.path().is_dir() {
                stack.push(e.path());
            } else {
                n += 1;
            }
        }
    }
    n
}

fn verify_split(home: &Path, spec: &str) {
    let db = home.join("data").join("core.db");
    let g = conv_id(&db, &group()).expect("group");
    let own = own_messages(&db, g);
    assert_eq!(
        own.len(),
        SPLIT_MESSAGES.len(),
        "{spec}: the send is stored once, as three messages"
    );
    assert_eq!(outbox_len(&db), 0, "{spec}");
    for (m, want) in own.iter().zip(SPLIT_MESSAGES) {
        assert!(
            matches!(m.out_state, Some(OutgoingState::Sent { .. })),
            "{spec}: {:?}",
            m.out_state
        );
        assert_eq!(
            attachment_names(&db, m.id),
            want,
            "{spec}: the user's order"
        );
    }
    assert!(
        own.windows(2).all(|w| w[0].sent_at < w[1].sent_at),
        "{spec}: later messages have later ids: {own:?}"
    );
    let caption: Vec<bool> = core_db(&db, |c| {
        own.iter()
            .map(|m| {
                c.query_row("SELECT body FROM messages WHERE id = ?1", [m.id.0], |r| {
                    r.get::<_, String>(0)
                })
                .expect("body")
                .contains("crash-safe files")
            })
            .collect()
    });
    assert_eq!(
        caption,
        [true, false, false],
        "{spec}: the caption on the first"
    );
    // Each recipient got each message exactly once, with the id from the enqueue, and the
    // messages left in the user's order.
    let (uuid, key, backend) = account(home);
    let log = server_log(&backend, &key);
    let mut both = [ALICE, BOB];
    both.sort_unstable();
    for m in &own {
        let mut got: Vec<&str> = log
            .iter()
            .filter(|d| d.local == m.local && d.sent_at == m.sent_at)
            .map(|d| d.recipient.as_str())
            .collect();
        got.sort_unstable();
        assert_eq!(got, both, "{spec}: each recipient exactly once: {log:?}");
    }
    assert_eq!(
        log.len(),
        SPLIT_MESSAGES.len() * 2,
        "{spec}: nothing else was sent: {log:?}"
    );
    let order: Vec<usize> = log
        .iter()
        .filter_map(|d| own.iter().position(|m| m.local == d.local))
        .collect();
    assert!(
        order.windows(2).all(|w| w[0] <= w[1]),
        "{spec}: sent in the user's order: {order:?}"
    );
    // The blobs of an import a kill cut off before its transaction are gone: what is on disk
    // is what the stored messages reference (three pictures with thumbnails, one document).
    let files: i64 = core_db(&db, |c| {
        c.query_row("SELECT count(*) FROM files", [], |r| r.get(0))
            .expect("files")
    });
    assert_eq!(files, 7, "{spec}");
    let blobs = DirVault::new(home.join("data")).blob_dir(uuid);
    assert_eq!(
        files_on_disk(&blobs),
        7,
        "{spec}: no blob without a row, no row without a blob"
    );
}

fn verify_staged(home: &Path, spec: &str) {
    let db = home.join("data").join("core.db");
    let a = conv_id(&db, &d_alice()).expect("alice");
    let own = own_messages(&db, a);
    assert_eq!(own.len(), STAGED_FILES, "{spec}: the send is stored once");
    assert_eq!(outbox_len(&db), 0, "{spec}");
    assert!(
        own.iter()
            .all(|m| matches!(m.out_state, Some(OutgoingState::Sent { .. }))),
        "{spec}"
    );
    assert!(
        own.windows(2).all(|w| w[0].sent_at < w[1].sent_at),
        "{spec}: later messages have later ids"
    );
    for (i, m) in own.iter().enumerate() {
        assert_eq!(
            attachment_names(&db, m.id),
            [format!("doc-{i:03}.txt")],
            "{spec}: the user's order"
        );
    }
    // Alice got each message exactly once, in order, with the id from the enqueue.
    let (uuid, key, backend) = account(home);
    let log = server_log(&backend, &key);
    let sent: Vec<&str> = log.iter().map(|d| d.local.as_str()).collect();
    let stored: Vec<&str> = own.iter().map(|m| m.local.as_str()).collect();
    assert_eq!(sent, stored, "{spec}: each once, in the user's order");
    // No blob of a part that was dropped stays: one file per document, each with its row.
    let files: i64 = core_db(&db, |c| {
        c.query_row("SELECT count(*) FROM files", [], |r| r.get(0))
            .expect("files")
    });
    assert_eq!(usize::try_from(files).ok(), Some(STAGED_FILES), "{spec}");
    let blobs = DirVault::new(home.join("data")).blob_dir(uuid);
    assert_eq!(
        files_on_disk(&blobs),
        STAGED_FILES,
        "{spec}: no blob without a row, no row without a blob"
    );
}

fn verify_challenge(home: &Path, spec: &str) {
    let db = home.join("data").join("core.db");
    let a = conv_id(&db, &d_alice()).expect("alice");
    let own = own_messages(&db, a);
    assert_eq!(own.len(), 1, "{spec}: one message, enqueued once");
    assert!(
        matches!(own[0].out_state, Some(OutgoingState::Sent { .. })),
        "{spec}: {:?}",
        own[0].out_state
    );
    assert_eq!(outbox_len(&db), 0, "{spec}");
    let stored: i64 = core_db(&db, |c| {
        c.query_row(
            "SELECT count(*) FROM settings WHERE key = 'challenge'",
            [],
            |r| r.get(0),
        )
        .expect("count")
    });
    assert_eq!(stored, 0, "{spec}: the answered challenge is forgotten");
    let (_, key, backend) = account(home);
    let log = server_log(&backend, &key);
    assert_eq!(log.len(), 1, "{spec}: delivered exactly once: {log:?}");
    assert_eq!(log[0].local, own[0].local, "{spec}: with its first id");
    // What the run that answered the challenge had sent when the challenge came on show.
    let seen: usize = std::fs::read_to_string(home.join(ATTEMPTS_FILE))
        .expect("the challenge was shown")
        .trim()
        .parse()
        .expect("a number");
    if spec.starts_with(kourier_core::FAILPOINT_AFTER_OUTCOME) {
        assert_eq!(
            seen, 0,
            "{spec}: the wait was taken up from core.db, nothing was sent into it"
        );
    } else {
        assert_eq!(
            seen, 1,
            "{spec}: the challenge was met by one attempt of this run"
        );
    }
}

/// After a kill behind the write that forgot an answered challenge: the run after it sent the
/// message without ever showing the challenge again. (The child notes the attempts the server
/// had seen whenever a challenge comes on show; the killed run noted one. A run that took the
/// challenge up from `core.db` would have noted none.)
fn verify_answered(home: &Path, spec: &str) {
    verify_challenge(home, spec);
    let seen = std::fs::read_to_string(home.join(ATTEMPTS_FILE)).expect("the note");
    assert_eq!(
        seen.trim(),
        "1",
        "{spec}: the challenge was on show once, in the run that answered it"
    );
}

/// Kills the child of `mode` at each of `specs` (failpoint, hit), restarts it and verifies.
fn kill_at(mode: Mode, specs: &[(&str, usize)], verify: fn(&Path, &str)) {
    for (name, n) in specs {
        let dir = tempfile::tempdir().expect("tempdir");
        let spec = format!("{name}:{n}");
        let status = run_child(dir.path(), mode, Some(&spec));
        assert_eq!(
            status.signal(),
            Some(SIGABRT),
            "{spec} was not hit: {status:?}"
        );
        assert!(
            run_child(dir.path(), mode, None).success(),
            "{spec}: restart run failed; log tail:\n{}",
            log_tail(dir.path())
        );
        verify(dir.path(), &spec);
        eprintln!("{} {spec}: killed, restarted, verified", mode.name());
    }
}

fn kill_everywhere(mode: Mode, failpoints: &[&str], verify: fn(&Path, &str)) {
    let base = tempfile::tempdir().expect("tempdir");
    assert!(
        run_child(base.path(), mode, None).success(),
        "uncrashed run:\n{}",
        log_tail(base.path())
    );
    verify(base.path(), "uncrashed");
    let mut kills = 0;
    for name in failpoints {
        let mut n = 1;
        loop {
            let dir = tempfile::tempdir().expect("tempdir");
            let spec = format!("{name}:{n}");
            let status = run_child(dir.path(), mode, Some(&spec));
            if status.success() {
                assert!(n > 1, "{name} was never hit");
                break;
            }
            assert_eq!(status.signal(), Some(SIGABRT), "{spec}: {status:?}");
            kills += 1;
            assert!(
                run_child(dir.path(), mode, None).success(),
                "{spec}: restart run failed; log tail:\n{}",
                log_tail(dir.path())
            );
            verify(dir.path(), &spec);
            eprintln!("{} {spec}: killed, restarted, verified", mode.name());
            n += 1;
        }
    }
    assert!(kills >= failpoints.len());
}

/// S5 "done when": a process killed anywhere on the send path resends with unchanged ids and
/// to no recipient twice.
#[test]
fn a_killed_send_resends_with_unchanged_ids_and_to_no_recipient_twice() {
    if std::env::var(CHILD_HOME).is_ok() || !kourier_db::failpoint::ENABLED {
        return;
    }
    kill_everywhere(Mode::Send, SEND_FAILPOINTS, verify_send);
}

/// A process killed anywhere between a send that fails for everyone, a reaction to that
/// message and its "Retry": the recipient never gets the reaction before the message, and
/// gets each exactly once, with unchanged ids; the reaction is not lost.
#[test]
fn a_kill_around_a_held_reaction_sends_the_message_first_and_each_once() {
    if std::env::var(CHILD_HOME).is_ok() || !kourier_db::failpoint::ENABLED {
        return;
    }
    kill_everywhere(Mode::Held, SEND_FAILPOINTS, verify_held);
}

/// One member of the group has a new safety number, so the send reaches the others first and
/// that member after the user's confirmation. A process killed anywhere on that path ends,
/// after a restart, with the message at every member exactly once and under its first id: the
/// row that waited for the member is narrowed to them durably, and the members that were
/// reached are never sent to again.
#[test]
fn a_kill_around_an_unconfirmed_member_sends_to_no_recipient_twice() {
    if std::env::var(CHILD_HOME).is_ok() || !kourier_db::failpoint::ENABLED {
        return;
    }
    kill_everywhere(Mode::Untrusted, SEND_FAILPOINTS, verify_send);
}

/// A send of several files that the network needs as several messages is stored all or none
/// and sent once: a process killed inside the enqueue, right after it or anywhere on the send
/// path ends, after a restart, with every message stored once, sent to each recipient once, in
/// the user's order and with unchanged ids.
#[test]
fn a_killed_split_send_stores_all_or_none_and_sends_each_message_once() {
    if std::env::var(CHILD_HOME).is_ok() || !kourier_db::failpoint::ENABLED {
        return;
    }
    kill_everywhere(Mode::Split, SPLIT_FAILPOINTS, verify_split);
}

/// S5 "done when": a read sync survives a kill (the `pending_commands` row is resent until the
/// backend confirms it with `CommandPersisted`).
#[test]
fn a_read_sync_survives_a_kill() {
    if std::env::var(CHILD_HOME).is_ok() || !kourier_db::failpoint::ENABLED {
        return;
    }
    kill_everywhere(Mode::Read, READ_FAILPOINTS, verify_read);
}

/// Clearing a chat survives a kill: the delete and its durable command are one transaction,
/// so a process killed right behind it (or behind any backend commit) ends, after a restart,
/// with the chat empty and the command at the backend exactly once.
#[test]
fn a_cleared_chat_and_its_command_survive_a_kill() {
    if std::env::var(CHILD_HOME).is_ok() || !kourier_db::failpoint::ENABLED {
        return;
    }
    kill_everywhere(Mode::Clear, READ_FAILPOINTS, verify_clear);
}

/// A send that becomes more messages than one transaction writes is stored in parts, with
/// the writer free in between, and still all or none: a process killed inside any part,
/// between two parts or right behind the last one ends, after a restart, with every message
/// of the send stored and sent once in the user's order, or (before that restart sends
/// again) with none of them; the blobs of a dropped part are gone.
#[test]
fn a_killed_large_send_stores_all_or_none_and_sends_each_message_once() {
    if std::env::var(CHILD_HOME).is_ok() || !kourier_db::failpoint::ENABLED {
        return;
    }
    let base = tempfile::tempdir().expect("tempdir");
    assert!(
        run_child(base.path(), Mode::Staged, None).success(),
        "uncrashed run:\n{}",
        log_tail(base.path())
    );
    verify_staged(base.path(), "uncrashed");
    kill_at(Mode::Staged, STAGED_KILLS, verify_staged);
}

/// The wait for a challenge answer survives a kill. Killed behind the transaction that
/// recorded the challenged attempt, the next run shows the challenge without sending
/// anything into it and holds the message until the user's answer; killed before that
/// transaction, the next run sends the message again (its answer was never recorded), meets
/// the challenge and waits the same way. Either way the message is delivered once.
#[test]
fn a_challenge_wait_survives_a_kill() {
    if std::env::var(CHILD_HOME).is_ok() || !kourier_db::failpoint::ENABLED {
        return;
    }
    kill_at(
        Mode::Challenge,
        &[
            (kourier_core::FAILPOINT_AFTER_OUTCOME, 1),
            (kourier_core::FAILPOINT_BEFORE_OUTCOME_COMMIT, 1),
            (kourier_core::FAILPOINT_AFTER_ATTEMPT, 1),
        ],
        verify_challenge,
    );
}

/// An answer the network accepted ends the challenge for good: it is forgotten by the write
/// that follows the answer, before the pause is lifted. Killed right behind that write, the
/// next run shows no challenge and sends the waiting message, once. (Forgotten only at the
/// dispatcher's next look, a kill here brought a solved challenge back at the next start.)
#[test]
fn an_answered_challenge_does_not_come_back_after_a_kill() {
    if std::env::var(CHILD_HOME).is_ok() || !kourier_db::failpoint::ENABLED {
        return;
    }
    kill_at(
        Mode::Challenge,
        &[(kourier_core::FAILPOINT_AFTER_CHALLENGE_ANSWER, 1)],
        verify_answered,
    );
}

//! The expiry option (user decision of 2026-10-07; architecture §9.6): what happens to
//! disappearing messages whose time has come by this computer's clock while no backend has
//! confirmed that clock. On the fake backend with the fake clock:
//!
//! - each of the three options at a start without a network, with a clock that is ahead and
//!   with a clock that is behind;
//! - "hide" hides everywhere: timeline, list preview, unread and mention counts, quotes, the
//!   versions of a message, attachment lookups, media handles, plaintext playback copies, and
//!   the commands that name the message; nothing of it is deleted, and it shows again exactly
//!   as before when the confirmed time says it is not due;
//! - a change of the option in each of the six directions, at once;
//! - a message that comes due while others are hidden;
//! - a kill between hiding and erasing (a child process that aborts): nothing shows at the
//!   next start, nothing is lost, and it is erased once the time is confirmed;
//! - what closes a notification: the invalidations and the reads the desktop service makes;
//! - "erase" never holds; each account follows its own backend;
//! - the option is stored where a start reads it, with no UI.

mod support;

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kourier_backend_api::{BackendEvent, FakeClock, IncomingMessage};
use kourier_core::AccountVault;
use kourier_core::facade::{
    ConversationRow, CoreHandle, ExpiryPolicy, MessageRow, Query, QueryData, QuoteState, Refusal,
    TimelineAnchor, TimelineQuery, UiCommand, UiEvent,
};
use kourier_core::testing::{DirVault, DumpMode, IngestHarness};
use kourier_fake::attachments::synthetic_image;
use kourier_fake::scripts::{T0, remote, target};
use kourier_fake::{FakeNetwork, Flavor, Script};
use kourier_model::{
    AccountId, AccountState, AttachmentId, Body, ConnState, ConversationId, DisappearKind, Expiry,
    MessageId, MsgIdScope, NetworkKind, QuoteSnapshot, ReplyTo, Text, UserAction,
};
use support::judge::ClockJudge;
use support::send::*;
use support::{CORE_KEY, Env, WAIT, add_linked, eventually};
use tokio::sync::watch;

const MINUTE: u64 = 60_000;
const DAY: u64 = 24 * 60 * MINUTE;
const START: u64 = T0 as u64;

/// Alice's picture with a caption, a mention of us and a timer of one minute.
const PHOTO: i64 = T0 + 1;
/// Alice quotes the picture (no timer).
const QUOTE: i64 = T0 + 2;
/// Alice's text with a timer of one hour.
const HOUR: i64 = T0 + 3;
/// The only message of bob's chat, with a timer of one minute.
const BOBS: i64 = T0 + 5;

fn timed(mut m: IncomingMessage, secs: u64) -> IncomingMessage {
    m.expiry = Some(Expiry {
        kind: DisappearKind::AfterSend,
        timer: Duration::from_secs(secs),
        started_at: None,
    });
    m
}

fn timed_messages() -> Vec<BackendEvent> {
    let mut photo = remote(&d_alice(), &alice(), &PHOTO.to_string(), PHOTO, "");
    photo.body = Body::Media {
        attachments: vec![synthetic_image(900, 600, 11)],
        caption: Some(Text::plain("one minute")),
    };
    photo.mentions_me = true;
    let mut quote = remote(
        &d_alice(),
        &alice(),
        &QUOTE.to_string(),
        QUOTE,
        "about that picture",
    );
    quote.reply_to = Some(ReplyTo {
        target: target(Some(&d_alice()), Some(&alice()), &PHOTO.to_string()),
        author: Some(alice()),
        snapshot: Some(QuoteSnapshot {
            text: Some(Text::plain("one minute")),
            ..Default::default()
        }),
    });
    let hour = remote(&d_alice(), &alice(), &HOUR.to_string(), HOUR, "one hour");
    let bobs = remote(&d_bob(), &bob(), &BOBS.to_string(), BOBS, "bob, one minute");
    vec![
        BackendEvent::Message(timed(photo, 60)),
        BackendEvent::Message(quote),
        BackendEvent::Message(timed(hour, 3600)),
        BackendEvent::Message(timed(bobs, 60)),
    ]
}

fn timers_script() -> Script {
    script("expiry-policy").deliver(timed_messages())
}

/// What became of a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fate {
    /// In its timeline.
    Shown,
    /// Stored, and in no read.
    Hidden,
    /// Its row is gone.
    Erased,
}

fn offline() -> ConnState {
    ConnState::Offline { retry_at_ms: None }
}

fn online() -> ConnState {
    ConnState::Online { detail: None }
}

fn fix_clock(skew_ms: i64) -> ConnState {
    ConnState::NeedsUserAction(UserAction::FixClock { skew_ms })
}

/// Gives the core time to do something it must not do (real time; the fake clock stands still).
async fn settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

fn peek<T>(db: &Path, f: impl FnOnce(&kourier_db::rusqlite::Connection) -> T) -> T {
    let conn = kourier_db::open_read_only(db, &kourier_db::DbKey::from(CORE_KEY)).expect("open");
    f(&conn)
}

fn count(db: &Path, sql: &str) -> i64 {
    peek(db, |c| c.query_row(sql, [], |r| r.get(0)).expect(sql))
}

/// The row id of the message with the network id `local` (`None`: erased).
fn message_id(db: &Path, local: i64) -> Option<MessageId> {
    peek(db, |c| {
        c.query_row(
            "SELECT id FROM messages WHERE net_local = ?1",
            [local.to_string()],
            |r| r.get(0),
        )
        .ok()
        .map(MessageId)
    })
}

async fn timeline_of(h: &CoreHandle, conv: ConversationId) -> Vec<MessageRow> {
    let q = TimelineQuery {
        conv,
        anchor: TimelineAnchor::Newest,
        limit: 500,
    };
    match h.query(Query::Timeline(q)).await.expect("timeline") {
        QueryData::Timeline(p) => p.rows,
        other => panic!("unexpected {other:?}"),
    }
}

async fn fate_of(h: &CoreHandle, db: &Path, conv: ConversationId, local: i64) -> Fate {
    // The timeline first: a row read after the row count could be erased in between.
    let rows = timeline_of(h, conv).await;
    match message_id(db, local) {
        None => Fate::Erased,
        Some(id) if rows.iter().any(|r| r.id == id) => Fate::Shown,
        Some(_) => Fate::Hidden,
    }
}

async fn policy_of(h: &CoreHandle) -> ExpiryPolicy {
    match h.query(Query::ExpiryPolicy).await.expect("policy") {
        QueryData::ExpiryPolicy(p) => p,
        other => panic!("unexpected {other:?}"),
    }
}

/// A linked account whose chats hold the timed messages, on a network whose starts can be
/// judged.
struct Scene {
    env: Env,
    judge: ClockJudge,
    account: AccountId,
    alice: ConversationId,
    bob: ConversationId,
    photo: MessageId,
    quote: MessageId,
    attachment: AttachmentId,
}

impl Scene {
    /// Links, receives the messages and downloads the picture, at `START`. With `policy`,
    /// the option is chosen in this first run (a later start reads it from `core.db`).
    async fn new(policy: Option<ExpiryPolicy>) -> Self {
        let fake = FakeNetwork::new(Flavor::SignalLike, timers_script());
        let judge = ClockJudge::new(fake.clone());
        let dir = tempfile::tempdir().expect("tempdir");
        let clock = FakeClock::new(START);
        let vault = Arc::new(DirVault::new(dir.path().join("data")));
        let core = judge
            .start(dir.path(), &vault, Arc::new(clock.clone()))
            .await;
        let handle = core.handle();
        let mut env = Env {
            dir,
            clock,
            control: fake.control(),
            fake,
            vault,
            events: handle.subscribe(),
            handle,
            core: Some(core),
        };
        env.handle.send(UiCommand::UiReady).expect("send");
        let account = add_linked(&mut env).await;
        env.wait_finished(account).await;
        let db = env.core_db();
        eventually(
            "the picture is downloaded and has its thumbnail",
            WAIT,
            || {
                let db = db.clone();
                async move {
                    count(
                        &db,
                        "SELECT count(*) FROM attachments WHERE state = '\"Local\"'
                       AND thumb_file_id IS NOT NULL AND derived_at IS NOT NULL",
                    ) == 1
                }
            },
        )
        .await;
        if let Some(policy) = policy {
            env.handle
                .send(UiCommand::SetExpiryPolicy { policy })
                .expect("send");
            assert_eq!(policy_of(&env.handle).await, policy, "read right behind");
        }
        let alice = conv_id(&db, &d_alice()).expect("alice's chat");
        let bob = conv_id(&db, &d_bob()).expect("bob's chat");
        let photo = message_id(&db, PHOTO).expect("the picture");
        let quote = message_id(&db, QUOTE).expect("the quote");
        let attachment = AttachmentId(count(&db, "SELECT id FROM attachments"));
        Self {
            env,
            judge,
            account,
            alice,
            bob,
            photo,
            quote,
            attachment,
        }
    }

    /// Stops the core, sets the clock and starts again with a backend that says `first`
    /// once it is told to connect, then what is sent through the returned sender. No
    /// `UiReady` is sent: the backend is not even told to connect yet.
    async fn restart(&mut self, now: u64, first: ConnState) -> watch::Sender<ConnState> {
        self.env.stop().await;
        self.env.clock.set(now);
        let verdict = self.judge.judge_next(first);
        let core = self
            .judge
            .start(
                self.env.dir.path(),
                &self.env.vault,
                Arc::new(self.env.clock.clone()),
            )
            .await;
        self.env.handle = core.handle();
        self.env.events = self.env.handle.subscribe();
        self.env.core = Some(core);
        verdict
    }

    fn db(&self) -> PathBuf {
        self.env.core_db()
    }

    fn conv_of(&self, local: i64) -> ConversationId {
        if local == BOBS { self.bob } else { self.alice }
    }

    async fn fate(&self, local: i64) -> Fate {
        fate_of(&self.env.handle, &self.db(), self.conv_of(local), local).await
    }

    /// The fates of the picture, the one-hour text and bob's message.
    async fn fates(&self) -> [Fate; 3] {
        [
            self.fate(PHOTO).await,
            self.fate(HOUR).await,
            self.fate(BOBS).await,
        ]
    }

    async fn becomes(&self, what: &str, want: [Fate; 3]) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let got = self.fates().await;
            if got == want {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{what}: picture, hour, bob's are {got:?}, not {want:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Still `want` after the core had time to do otherwise.
    async fn stays(&self, what: &str, want: [Fate; 3]) {
        settle().await;
        assert_eq!(self.fates().await, want, "{what}");
    }

    /// The backend is told to connect and has said `state`.
    async fn connects(&mut self, state: &ConnState) {
        self.env.handle.send(UiCommand::UiReady).expect("send");
        let want = state.clone();
        self.env
            .event("the backend's first word", |e| match e {
                UiEvent::ConnState { state, .. } if *state == want => Some(()),
                _ => None,
            })
            .await;
    }

    /// Both chats as the list and the timeline show them.
    async fn view(&self) -> View {
        let convs = self.env.conversations().await;
        let row = |id: ConversationId| convs.iter().find(|c| c.id == id).cloned().expect("listed");
        View {
            alice: row(self.alice),
            bob: row(self.bob),
            alice_rows: self.env.timeline(self.alice).await,
            bob_rows: self.env.timeline(self.bob).await,
        }
    }

    /// The picture's blob file on disk.
    fn blob(&self) -> PathBuf {
        let (uuid, path): (String, String) = peek(&self.db(), |c| {
            c.query_row(
                "SELECT a.uuid, f.path FROM files f JOIN accounts a ON a.id = f.account_id
                 JOIN attachments x ON x.file_id = f.id",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("the picture's file")
        });
        self.env
            .vault
            .blob_dir(uuid.parse().expect("uuid"))
            .join(path)
    }

    /// The next refusal of `action`.
    async fn refused(&mut self, action: &'static str) -> Option<Refusal> {
        self.env
            .event("a refusal", |e| match e {
                UiEvent::ActionFailed {
                    action: a, error, ..
                } if *a == action => Some(error.refusal()),
                _ => None,
            })
            .await
    }
}

/// What the user sees of the two chats.
#[derive(Debug, PartialEq)]
struct View {
    alice: ConversationRow,
    bob: ConversationRow,
    alice_rows: Vec<MessageRow>,
    bob_rows: Vec<MessageRow>,
}

/// Everything "hidden" promises about the picture, bob's message and what refers to them,
/// while the one-hour text shows.
async fn assert_hidden_everywhere(s: &mut Scene, before: &View) {
    let db = s.db();
    let view = s.view().await;
    // Timeline: no row, and the quote of it reads as for an erased original (the sender's
    // snapshot is all that is left; nothing to jump to, no thumbnail).
    assert!(!view.alice_rows.iter().any(|r| r.id == s.photo));
    assert!(view.bob_rows.is_empty(), "{:?}", view.bob_rows);
    let quote = view
        .alice_rows
        .iter()
        .find(|r| r.id == s.quote)
        .expect("the quote stays");
    let reply = quote.reply.as_ref().expect("it quotes");
    assert_eq!(
        (reply.state, reply.target, reply.thumb),
        (QuoteState::Missing, None, None)
    );
    let was = before
        .alice_rows
        .iter()
        .find(|r| r.id == s.quote)
        .and_then(|r| r.reply.clone())
        .expect("it quoted");
    assert_eq!(
        (was.state, was.target, was.thumb),
        (QuoteState::Known, Some(s.photo), Some(s.attachment)),
        "before, the quote led to the picture"
    );
    // List: counters and preview as if the messages were gone.
    assert_eq!(view.alice.unread + 1, before.alice.unread);
    assert_eq!((before.alice.mentions, view.alice.mentions), (1, 0));
    assert_eq!(
        view.alice.preview.as_ref().map(|p| p.text.clone()),
        Some(Some("one hour".to_owned()))
    );
    assert_eq!(
        (view.bob.unread, view.bob.last_activity, &view.bob.preview),
        (0, 0, &None)
    );
    // The reads by id.
    let h = s.env.handle.clone();
    assert!(matches!(
        h.query(Query::Revisions(s.photo)).await,
        Ok(QueryData::Revisions(None))
    ));
    assert!(matches!(
        h.query(Query::Attachment(s.attachment)).await,
        Ok(QueryData::Attachment(None))
    ));
    let around = TimelineQuery {
        conv: s.alice,
        anchor: TimelineAnchor::Around(s.photo),
        limit: 2,
    };
    match h.query(Query::Timeline(around)).await.expect("timeline") {
        QueryData::Timeline(p) => assert!(p.rows.is_empty(), "{:?}", p.rows),
        other => panic!("unexpected {other:?}"),
    }
    // Media: no handle, and none is made.
    let media = h.media();
    assert!(media.get(s.attachment).is_none());
    assert!(matches!(media.resolve(s.attachment).await, Ok(None)));
    // Commands that still name it are refused as for a message that is gone.
    let (photo, alice, attachment) = (s.photo, s.alice, s.attachment);
    h.send(UiCommand::OpenMedia { attachment }).expect("send");
    assert_eq!(s.refused("OpenMedia").await, Some(Refusal::Gone));
    h.send(UiCommand::DownloadAttachment { attachment })
        .expect("send");
    assert_eq!(s.refused("DownloadAttachment").await, Some(Refusal::Gone));
    h.send(UiCommand::DeleteAttachment { attachment })
        .expect("send");
    assert_eq!(s.refused("DeleteAttachment").await, Some(Refusal::Gone));
    h.send(UiCommand::React {
        message: photo,
        emoji: "👍".into(),
        remove: false,
    })
    .expect("send");
    assert_eq!(s.refused("React").await, Some(Refusal::Gone));
    h.send(UiCommand::DeleteMessage {
        message: photo,
        for_everyone: false,
    })
    .expect("send");
    assert_eq!(s.refused("DeleteMessage").await, Some(Refusal::Gone));
    h.send(UiCommand::SendText {
        conv: alice,
        text: Text::plain("re"),
        reply_to: Some(photo),
    })
    .expect("send");
    assert_eq!(s.refused("SendText").await, Some(Refusal::Gone));
    // Nothing of it is deleted, and nothing is remembered as expired.
    assert_eq!(
        count(&db, "SELECT count(*) FROM messages WHERE expiry_hidden = 1"),
        2
    );
    assert_eq!(count(&db, "SELECT count(*) FROM attachments"), 1);
    assert_eq!(count(&db, "SELECT count(*) FROM expired_messages"), 0);
    assert!(s.blob().exists(), "the picture's file");
}

/// What "erased for good" means for the picture and bob's message.
async fn assert_erased_for_good(s: &Scene, blob: &Path) {
    let db = s.db();
    assert_eq!(count(&db, "SELECT count(*) FROM attachments"), 0);
    assert_eq!(
        count(&db, "SELECT count(*) FROM messages WHERE expiry_hidden = 1"),
        0
    );
    let keys =
        format!("SELECT count(*) FROM expired_messages WHERE net_local IN ('{PHOTO}', '{BOBS}')");
    assert_eq!(count(&db, &keys), 2, "their keys are remembered");
    let blob = blob.to_owned();
    eventually("the picture's file is unlinked", WAIT, || {
        let blob = blob.clone();
        async move { !blob.exists() }
    })
    .await;
}

use Fate::{Erased, Hidden, Shown};

/// A start without a network, ten minutes later: the picture and bob's message are due by
/// this computer's clock, and no backend can say whether that clock is right. The default
/// hides them at the first read and erases them at the first connect; "keep" leaves them
/// readable until then; "erase" deletes them at the start, like Signal Desktop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_without_a_network_follows_the_option() {
    // Hide, as the default: no option was ever chosen.
    let mut s = Scene::new(None).await;
    let before = s.view().await;
    let blob = s.blob();
    let verdict = s.restart(START + 10 * MINUTE, offline()).await;
    assert_eq!(policy_of(&s.env.handle).await, ExpiryPolicy::default());
    assert_eq!(
        s.fates().await,
        [Hidden, Shown, Hidden],
        "hidden at the first read of this start, before anything was asked of the backend"
    );
    s.connects(&offline()).await;
    s.stays("no network: hidden, not erased", [Hidden, Shown, Hidden])
        .await;
    assert_hidden_everywhere(&mut s, &before).await;
    // Reading alice's chat reads what shows, not the hidden picture: nobody is told we
    // read a message we cannot see.
    s.env
        .handle
        .send(UiCommand::MarkRead { conv: s.alice })
        .expect("send");
    let db = s.db();
    eventually("the chat is read", WAIT, || {
        let db = db.clone();
        async move {
            count(
                &db,
                "SELECT count(*) FROM messages WHERE read_state = 'Unread' AND expiry_hidden = 0",
            ) == 0
        }
    })
    .await;
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM messages WHERE read_state = 'Unread' AND expiry_hidden = 1"
        ),
        2,
        "the hidden ones are still unread"
    );
    let told = count(
        &db,
        &format!("SELECT count(*) FROM pending_commands WHERE command LIKE '%{PHOTO}%'"),
    );
    assert_eq!(told, 0, "no read sync or receipt names the hidden picture");
    // A server takes the connection: the clock was right, they are erased for good.
    verdict.send(online()).expect("the judge runs");
    s.becomes("confirmed", [Erased, Shown, Erased]).await;
    assert_erased_for_good(&s, &blob).await;
    s.env.stop().await;

    // Keep: chosen in the first run, read from core.db by a start that has no UI.
    let mut s = Scene::new(Some(ExpiryPolicy::KeepUntilConfirmed)).await;
    let before = s.view().await;
    let blob = s.blob();
    let verdict = s.restart(START + 10 * MINUTE, offline()).await;
    assert_eq!(
        policy_of(&s.env.handle).await,
        ExpiryPolicy::KeepUntilConfirmed
    );
    assert_eq!(s.fates().await, [Shown, Shown, Shown]);
    s.connects(&offline()).await;
    s.stays("kept and readable", [Shown, Shown, Shown]).await;
    assert_eq!(s.view().await, before, "nothing changed for the user");
    verdict.send(online()).expect("the judge runs");
    s.becomes("confirmed", [Erased, Shown, Erased]).await;
    assert_erased_for_good(&s, &blob).await;
    s.env.stop().await;

    // Erase: never holds, whatever a backend says or does not say.
    let mut s = Scene::new(Some(ExpiryPolicy::EraseByLocalClock)).await;
    let blob = s.blob();
    let _verdict = s.restart(START + 10 * MINUTE, offline()).await;
    assert_eq!(
        policy_of(&s.env.handle).await,
        ExpiryPolicy::EraseByLocalClock
    );
    s.becomes("erased at the start, nobody asked", [Erased, Shown, Erased])
        .await;
    assert_erased_for_good(&s, &blob).await;
    s.connects(&offline()).await;
    s.stays("and the rest stays", [Erased, Shown, Erased]).await;
    s.env.stop().await;
}

/// What came due while Kourier was off is hidden by the start itself, before `Core::start`
/// hands out a handle, not by the sweeper's first look a moment later (a first query could
/// win that race and show a due message). On a runtime with one thread, nothing the start
/// spawned (the sweeper, the supervisors) has run when it returns, and this test reads the
/// rows straight from `core.db` before it yields.
#[test]
fn a_start_hides_what_is_due_before_it_serves_anything() {
    let first = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let s = first.block_on(async {
        let mut s = Scene::new(None).await;
        s.env.stop().await;
        s
    });
    let db = s.db();
    let hidden = "SELECT count(*) FROM messages WHERE expiry_hidden = 1";
    assert_eq!(count(&db, hidden), 0, "nothing was due in the first run");
    let stored = count(&db, "SELECT count(*) FROM messages");
    s.env.clock.set(START + 10 * MINUTE);
    let _verdict = s.judge.judge_next(offline());
    let single = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    single.block_on(async {
        let core = s
            .judge
            .start(
                s.env.dir.path(),
                &s.env.vault,
                Arc::new(s.env.clock.clone()),
            )
            .await;
        // No await between the start's return and these reads.
        assert_eq!(
            count(&db, hidden),
            2,
            "the picture and bob's message are hidden when the start returns"
        );
        assert_eq!(
            count(&db, "SELECT count(*) FROM messages"),
            stored,
            "and stored"
        );
        assert_eq!(count(&db, "SELECT count(*) FROM expired_messages"), 0);
        core.shutdown().await;
    });
}

/// The clock went wrong while Kourier was off: it reads a day ahead, so every timer looks
/// run out, and the backend says so when it connects. "Hide" hides all three and shows them
/// again, exactly as they were, once the clock is set right and a server took the
/// connection; they go when they are really due. "Keep" never hides. "Erase" deletes them
/// on the wrong clock's word.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clock_that_is_ahead_follows_the_option() {
    let wrong = fix_clock(i64::try_from(DAY).expect("fits"));
    for policy in ExpiryPolicy::ALL {
        let mut s = Scene::new(Some(policy)).await;
        let before = s.view().await;
        let blob = s.blob();
        let verdict = s.restart(START + DAY, wrong.clone()).await;
        let held = match policy {
            ExpiryPolicy::HideUntilConfirmed => [Hidden, Hidden, Hidden],
            ExpiryPolicy::KeepUntilConfirmed => [Shown, Shown, Shown],
            ExpiryPolicy::EraseByLocalClock => [Erased, Erased, Erased],
        };
        s.becomes("at the start", held).await;
        s.connects(&wrong).await;
        let db = s.db();
        eventually("the wrong clock is on record", WAIT, || {
            let db = db.clone();
            async move {
                count(
                    &db,
                    "SELECT count(*) FROM settings WHERE key = 'clock_wrong'",
                ) == 1
            }
        })
        .await;
        s.stays("a backend that objects confirms nothing", held)
            .await;
        if policy == ExpiryPolicy::HideUntilConfirmed {
            let view = s.view().await;
            assert_eq!(
                view.alice.preview.as_ref().map(|p| p.message),
                Some(s.quote),
                "the newest message that shows"
            );
            assert_eq!(
                count(&db, "SELECT count(*) FROM messages WHERE expiry_hidden = 1"),
                3,
                "all three are still stored"
            );
            assert_eq!(count(&db, "SELECT count(*) FROM expired_messages"), 0);
        }
        // The user sets the clock right (half a minute after the messages came) and the
        // server takes the connection.
        s.env.clock.set(START + MINUTE / 2);
        verdict.send(online()).expect("the judge runs");
        if policy == ExpiryPolicy::EraseByLocalClock {
            s.stays("erased is erased", held).await;
            s.env.stop().await;
            continue;
        }
        s.becomes("the confirmed time says: not due", [Shown, Shown, Shown])
            .await;
        assert_eq!(s.view().await, before, "{policy:?}: exactly as before");
        assert!(blob.exists());
        assert!(s.env.handle.media().get(s.attachment).is_some());
        // Their minute ends on the confirmed clock: now they go, the hour stays.
        s.env.clock.set(START + 2 * MINUTE);
        s.becomes("really due", [Erased, Shown, Erased]).await;
        assert_erased_for_good(&s, &blob).await;
        s.env.stop().await;
    }
}

/// Reading a chat reads what shows. With the picture and the one-hour text hidden (a clock a
/// day ahead), "read up to here" names the newest message that shows, the quote, and no
/// read sync or receipt names a hidden one; when they show again they are unread, as they
/// were.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reading_a_chat_does_not_read_what_is_hidden() {
    let wrong = fix_clock(i64::try_from(DAY).expect("fits"));
    let mut s = Scene::new(None).await;
    let verdict = s.restart(START + DAY, wrong.clone()).await;
    s.connects(&wrong).await;
    assert_eq!(s.fates().await, [Hidden, Hidden, Hidden]);
    s.env
        .handle
        .send(UiCommand::MarkRead { conv: s.alice })
        .expect("send");
    let db = s.db();
    eventually("the chat is read", WAIT, || {
        let db = db.clone();
        async move { count(&db, "SELECT count(*) FROM pending_commands") == 2 }
    })
    .await;
    let commands: Vec<String> = peek(&db, |c| {
        c.prepare("SELECT command FROM pending_commands ORDER BY id")
            .expect("prepare")
            .query_map([], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
    });
    assert!(
        commands[0].contains("SyncReadMarker") && commands[1].contains("SendReceipts"),
        "a read sync and the read receipts"
    );
    for c in &commands {
        assert!(c.contains(&format!("\"{QUOTE}\"")), "names the quote");
        assert!(
            !c.contains(&format!("\"{PHOTO}\"")) && !c.contains(&format!("\"{HOUR}\"")),
            "names nothing that is hidden"
        );
    }
    let unread = "SELECT count(*) FROM messages WHERE read_state = 'Unread'";
    assert_eq!(count(&db, unread), 3, "the three hidden ones");
    // The clock is set right and confirmed: they show again, unread.
    s.env.clock.set(START + MINUTE / 2);
    verdict.send(online()).expect("the judge runs");
    s.becomes("not due", [Shown, Shown, Shown]).await;
    let view = s.view().await;
    assert_eq!((view.alice.unread, view.alice.mentions), (2, 1));
    assert_eq!(view.bob.unread, 1);
    s.env.stop().await;
}

/// The clock reads a day behind: by it nothing is due, so nothing is hidden or erased under
/// any option, though the picture's minute is long over. Then the user sets it right while
/// the backend still objects (nobody has confirmed the new time): "hide" hides, "keep"
/// keeps, "erase" erases. Once a server took the connection, all of them have erased.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clock_that_is_behind_follows_the_option() {
    let wrong = fix_clock(-i64::try_from(DAY + 10 * MINUTE).expect("fits"));
    for policy in ExpiryPolicy::ALL {
        let mut s = Scene::new(Some(policy)).await;
        let before = s.view().await;
        let blob = s.blob();
        let verdict = s.restart(START - DAY, wrong.clone()).await;
        assert_eq!(s.fates().await, [Shown, Shown, Shown]);
        s.connects(&wrong).await;
        s.stays("not due by this clock", [Shown, Shown, Shown])
            .await;
        assert_eq!(s.view().await, before);
        // Set right, not confirmed.
        s.env.clock.set(START + 10 * MINUTE);
        let unconfirmed = match policy {
            ExpiryPolicy::HideUntilConfirmed => [Hidden, Shown, Hidden],
            ExpiryPolicy::KeepUntilConfirmed => [Shown, Shown, Shown],
            ExpiryPolicy::EraseByLocalClock => [Erased, Shown, Erased],
        };
        s.becomes("due by the local clock, unconfirmed", unconfirmed)
            .await;
        s.stays("until a backend says so", unconfirmed).await;
        verdict.send(online()).expect("the judge runs");
        s.becomes("confirmed", [Erased, Shown, Erased]).await;
        assert_erased_for_good(&s, &blob).await;
        s.env.stop().await;
    }
}

/// The events of one change, up to the invalidation of both timelines.
async fn invalidations(s: &mut Scene) -> (Vec<ConversationId>, Vec<ConversationId>) {
    let (alice, bob) = (s.alice, s.bob);
    let (mut changed, mut reloaded) = (Vec::new(), Vec::new());
    s.env
        .event("both timelines are invalidated", |e| {
            match e {
                UiEvent::ConversationsChanged { ids } => changed.extend(ids.iter().copied()),
                UiEvent::TimelineInvalidated { conv } => reloaded.push(*conv),
                _ => {}
            }
            (reloaded.contains(&alice) && reloaded.contains(&bob)).then_some(())
        })
        .await;
    (changed, reloaded)
}

/// Chooses `policy`, and reads it back right behind the command: the answer comes once the
/// command is applied, so what the test does next (move the clock) happens under it.
async fn choose(s: &Scene, policy: ExpiryPolicy) {
    s.env
        .handle
        .send(UiCommand::SetExpiryPolicy { policy })
        .expect("send");
    assert_eq!(policy_of(&s.env.handle).await, policy);
}

/// A change of the option takes effect at once, in every direction, for what is due now on
/// an unconfirmed clock: keep → hide hides it (and the invalidations and reads that close
/// its notification follow), hide → keep shows it again as it was, anything → erase deletes
/// it; after erase → hide and erase → keep, what comes due next is hidden or kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_of_the_option_takes_effect_at_once_in_every_direction() {
    let stored = |s: &Scene| {
        peek(&s.db(), |c| {
            c.query_row(
                "SELECT value FROM settings WHERE scope = 'core' AND key = 'expiry_policy'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
        })
    };

    let mut s = Scene::new(Some(ExpiryPolicy::KeepUntilConfirmed)).await;
    let before = s.view().await;
    let blob = s.blob();
    let _verdict = s.restart(START + 10 * MINUTE, offline()).await;
    s.connects(&offline()).await;
    assert_eq!(s.fates().await, [Shown, Shown, Shown]);
    // The picture is open in a viewer: a plaintext copy in the media cache, and a handle.
    let attachment = s.attachment;
    s.env
        .handle
        .send(UiCommand::OpenMedia { attachment })
        .expect("send");
    let copy = s
        .env
        .event("the picture is opened", |e| match e {
            UiEvent::MediaOpened { path, .. } => Some(path.clone()),
            _ => None,
        })
        .await;
    assert!(copy.exists() && copy.starts_with(s.env.media_dir()));
    assert!(s.env.handle.media().get(attachment).is_some());

    // keep → hide: gone from the very next read.
    choose(&s, ExpiryPolicy::HideUntilConfirmed).await;
    assert_eq!(
        s.fates().await,
        [Hidden, Shown, Hidden],
        "the read right behind the command"
    );
    assert_eq!(stored(&s).as_deref(), Some("hide"));
    // What closes a notification of the picture or of bob's message (the desktop service,
    // `kourier-app` `desktop/service.rs`): on `TimelineInvalidated` it reads the message
    // it showed (`check_messages`: no row any more closes it), on `ConversationsChanged`
    // the conversation (`check_read`: nothing unread closes it).
    let (changed, reloaded) = invalidations(&mut s).await;
    assert!(changed.contains(&s.alice) && changed.contains(&s.bob));
    assert!(reloaded.contains(&s.alice) && reloaded.contains(&s.bob));
    assert_hidden_everywhere(&mut s, &before).await;
    let copy_gone = copy.clone();
    eventually("the plaintext copy is removed", WAIT, || {
        let copy = copy_gone.clone();
        async move { !copy.exists() }
    })
    .await;

    // hide → keep: back, exactly as before.
    choose(&s, ExpiryPolicy::KeepUntilConfirmed).await;
    assert_eq!(s.fates().await, [Shown, Shown, Shown]);
    assert_eq!(s.view().await, before);
    let (changed, _) = invalidations(&mut s).await;
    assert!(changed.contains(&s.alice) && changed.contains(&s.bob));
    assert_eq!(stored(&s).as_deref(), Some("keep"));

    // keep → erase: out of the very next read (the command hides what the sweeper is about
    // to erase), and then gone for good.
    choose(&s, ExpiryPolicy::EraseByLocalClock).await;
    let at_once = s.fates().await;
    assert!(
        at_once[0] != Shown && at_once[1] == Shown && at_once[2] != Shown,
        "the read right behind the command shows nothing that is due: {at_once:?}"
    );
    s.becomes("erase erases what is due", [Erased, Shown, Erased])
        .await;
    assert_erased_for_good(&s, &blob).await;

    // erase → hide: what comes due from now on is hidden, not erased.
    choose(&s, ExpiryPolicy::HideUntilConfirmed).await;
    s.env.clock.set(START + 2 * 60 * MINUTE);
    s.becomes("the hour is over: hidden", [Erased, Hidden, Erased])
        .await;
    s.stays("and kept", [Erased, Hidden, Erased]).await;
    // hide → erase.
    choose(&s, ExpiryPolicy::EraseByLocalClock).await;
    s.becomes("erase erases what was hidden", [Erased, Erased, Erased])
        .await;
    assert_eq!(
        count(&s.db(), "SELECT count(*) FROM expired_messages"),
        3,
        "all three keys are remembered"
    );
    s.env.stop().await;

    // erase → keep, on a fresh profile: the picture went at the start; the hour's text,
    // which comes due later, is kept. Then keep → hide → keep.
    let mut s = Scene::new(Some(ExpiryPolicy::EraseByLocalClock)).await;
    let _verdict = s.restart(START + 10 * MINUTE, offline()).await;
    s.connects(&offline()).await;
    s.becomes("erased at the start", [Erased, Shown, Erased])
        .await;
    choose(&s, ExpiryPolicy::KeepUntilConfirmed).await;
    s.env.clock.set(START + 2 * 60 * MINUTE);
    s.stays(
        "the hour is over: kept and readable",
        [Erased, Shown, Erased],
    )
    .await;
    choose(&s, ExpiryPolicy::HideUntilConfirmed).await;
    assert_eq!(s.fates().await, [Erased, Hidden, Erased]);
    choose(&s, ExpiryPolicy::KeepUntilConfirmed).await;
    assert_eq!(s.fates().await, [Erased, Shown, Erased]);
    s.env.stop().await;
}

/// The next failure of `SetExpiryPolicy`: whose account and what it names (`None`: neither),
/// and whether it was a refusal.
async fn option_failed(s: &mut Scene) -> (Option<AccountId>, bool, Option<Refusal>) {
    use kourier_core::facade::ActionSubject;
    s.env
        .event("the change of the option fails", |e| match e {
            UiEvent::ActionFailed {
                account,
                action: "SetExpiryPolicy",
                subject,
                error,
            } => Some((
                *account,
                matches!(subject, ActionSubject::None),
                error.refusal(),
            )),
            _ => None,
        })
        .await
}

/// A change of the option that cannot be stored (a full disk; here a trigger refuses the
/// row) changes nothing: the command fails, the option is what it was, in memory and in
/// `core.db`, and no message was hidden, shown or erased by the option that was not stored.
/// "Erase" is the one that matters, because a delete cannot be taken back: asked for again
/// and again while the sweeper is kept awake, no sweep may ever run by it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_option_that_cannot_be_stored_changes_nothing() {
    let mut s = Scene::new(None).await;
    let blob = s.blob();
    let _verdict = s.restart(START + 10 * MINUTE, offline()).await;
    s.connects(&offline()).await;
    assert_eq!(s.fates().await, [Hidden, Shown, Hidden]);
    let db = s.db();
    {
        let conn = kourier_db::open(&db, &kourier_db::DbKey::from(CORE_KEY)).expect("open");
        conn.execute_batch(
            "CREATE TRIGGER the_option_cannot_be_stored BEFORE INSERT ON settings
               WHEN NEW.key = 'expiry_policy'
             BEGIN SELECT RAISE(ABORT, 'no space left'); END;",
        )
        .expect("trigger");
    }
    let stored = "SELECT count(*) FROM settings WHERE key = 'expiry_policy'";
    for round in 0..200 {
        // `Resumed` wakes the sweeper: it looks while the command is on its way.
        for command in [
            UiCommand::Resumed,
            UiCommand::SetExpiryPolicy {
                policy: ExpiryPolicy::EraseByLocalClock,
            },
            UiCommand::Resumed,
        ] {
            s.env.handle.send(command).expect("send");
        }
        assert_eq!(
            option_failed(&mut s).await,
            (None, true, None),
            "round {round}: a failure of the whole core, naming nothing, and no refusal"
        );
        assert_eq!(
            policy_of(&s.env.handle).await,
            ExpiryPolicy::HideUntilConfirmed,
            "round {round}: the option is what it was"
        );
        assert_eq!(
            count(&db, "SELECT count(*) FROM messages WHERE expiry_hidden = 1"),
            2,
            "round {round}: nothing was erased by an option that is not stored"
        );
    }
    s.stays("hidden and stored", [Hidden, Shown, Hidden]).await;
    assert_eq!(count(&db, "SELECT count(*) FROM expired_messages"), 0);
    assert_eq!(count(&db, stored), 0, "no option was ever stored");
    assert!(blob.exists(), "the picture's file");
    // "Keep" is not stored either, so nothing shows again.
    s.env
        .handle
        .send(UiCommand::SetExpiryPolicy {
            policy: ExpiryPolicy::KeepUntilConfirmed,
        })
        .expect("send");
    assert_eq!(option_failed(&mut s).await, (None, true, None));
    assert_eq!(
        policy_of(&s.env.handle).await,
        ExpiryPolicy::HideUntilConfirmed
    );
    s.stays("still hidden", [Hidden, Shown, Hidden]).await;
    // The disk has room again: the same command is stored and takes effect at once.
    {
        let conn = kourier_db::open(&db, &kourier_db::DbKey::from(CORE_KEY)).expect("open");
        conn.execute_batch("DROP TRIGGER the_option_cannot_be_stored")
            .expect("drop");
    }
    choose(&s, ExpiryPolicy::KeepUntilConfirmed).await;
    assert_eq!(s.fates().await, [Shown, Shown, Shown]);
    assert_eq!(count(&db, stored), 1);
    s.env.stop().await;
}

/// While the picture is hidden, the one-hour text comes due as well: it is hidden then, on
/// time, and nothing is erased; the first connect erases both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_that_comes_due_while_others_are_hidden_is_hidden_too() {
    let mut s = Scene::new(None).await;
    let blob = s.blob();
    let verdict = s.restart(START + 10 * MINUTE, offline()).await;
    s.connects(&offline()).await;
    assert_eq!(s.fates().await, [Hidden, Shown, Hidden]);
    // A second before the hour is over: nothing yet.
    let hour_ends = u64::try_from(HOUR).expect("positive") + 60 * MINUTE;
    s.env.clock.set(hour_ends - 1_000);
    s.stays("not yet", [Hidden, Shown, Hidden]).await;
    s.env.clock.set(hour_ends);
    s.becomes("the hour is over", [Hidden, Hidden, Hidden])
        .await;
    s.stays("hidden, not erased", [Hidden, Hidden, Hidden])
        .await;
    let db = s.db();
    assert_eq!(count(&db, "SELECT count(*) FROM expired_messages"), 0);
    assert!(blob.exists());
    let alice = s.env.timeline(s.alice).await;
    let shown: Vec<MessageId> = alice.iter().map(|r| r.id).collect();
    assert!(shown.contains(&s.quote) && shown.len() == 2, "{shown:?}");
    verdict.send(online()).expect("the judge runs");
    s.becomes("confirmed", [Erased, Erased, Erased]).await;
    assert_eq!(count(&db, "SELECT count(*) FROM expired_messages"), 3);
    s.env.stop().await;
}

/// A clean stop and start between hiding and erasing changes nothing: the stop itself must
/// not release the account (its backend going away is no verdict on the clock), and the next
/// start, still without a network, shows nothing and has lost nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hidden_messages_stay_hidden_and_stored_across_a_restart() {
    let mut s = Scene::new(None).await;
    let before = s.view().await;
    let blob = s.blob();
    let _first = s.restart(START + 10 * MINUTE, offline()).await;
    s.connects(&offline()).await;
    assert_eq!(s.fates().await, [Hidden, Shown, Hidden]);
    for round in 0..3 {
        let verdict = s.restart(START + (11 + round) * MINUTE, offline()).await;
        assert_eq!(
            s.fates().await,
            [Hidden, Shown, Hidden],
            "round {round}: nothing shows, nothing is lost"
        );
        assert_hidden_everywhere(&mut s, &before).await;
        if round == 2 {
            s.connects(&offline()).await;
            verdict.send(online()).expect("the judge runs");
            s.becomes("confirmed", [Erased, Shown, Erased]).await;
            assert_erased_for_good(&s, &blob).await;
        }
    }
    s.env.stop().await;
}

/// An own message can be hidden too: sent to a group with a timer of one minute, it reached
/// alice (its timer runs from then) and failed for bob, so its send row waits for "Retry".
/// Hidden, it takes no command: not a retry, not a cancel, not an edit. Confirmed, it goes
/// with its send row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hidden_own_message_takes_no_command() {
    use kourier_fake::TransmitRule;
    use kourier_model::{ConversationPatch, OutgoingState, Patch};
    let mut timer = ConversationPatch::new(group());
    timer.timer = Patch::Set(Duration::from_secs(60));
    timer.timer_version = Some(2);
    let script = script("hidden-own")
        .deliver(vec![BackendEvent::ConversationUpsert(timer)])
        .fail_transmit(TransmitRule::member(bob(), unregistered(), 1).in_conv(group()));
    let fake = FakeNetwork::new(Flavor::SignalLike, script);
    let judge = ClockJudge::new(fake.clone());
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = FakeClock::new(START);
    let vault = Arc::new(DirVault::new(dir.path().join("data")));
    let core = judge
        .start(dir.path(), &vault, Arc::new(clock.clone()))
        .await;
    let handle = core.handle();
    let mut env = Env {
        dir,
        clock,
        control: fake.control(),
        fake,
        vault,
        events: handle.subscribe(),
        handle,
        core: Some(core),
    };
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let db = env.core_db();
    let g = conv_id(&db, &group()).expect("the group");
    env.handle
        .send(UiCommand::SendText {
            conv: g,
            text: Text::plain("one minute, from me"),
            reply_to: None,
        })
        .expect("send");
    let h = env.handle.clone();
    eventually("alice has it, bob does not", WAIT, || {
        let h = h.clone();
        async move {
            timeline_of(&h, g)
                .await
                .last()
                .is_some_and(|r| matches!(r.out_state, Some(OutgoingState::PartiallyFailed { .. })))
        }
    })
    .await;
    let message = timeline_of(&env.handle, g).await.last().expect("row").id;
    let rows = "SELECT count(*) FROM messages WHERE net_author = '' AND expires_at IS NOT NULL";
    assert_eq!(count(&db, rows), 1, "its timer runs: alice has it");
    assert_eq!(count(&db, "SELECT count(*) FROM outbox"), 1, "kept for bob");

    // Ten minutes later, without a network.
    env.stop().await;
    env.clock.set(START + 10 * MINUTE);
    let verdict = judge.judge_next(offline());
    let core = judge
        .start(env.dir.path(), &env.vault, Arc::new(env.clock.clone()))
        .await;
    env.handle = core.handle();
    env.events = env.handle.subscribe();
    env.core = Some(core);
    assert!(
        !timeline_of(&env.handle, g)
            .await
            .iter()
            .any(|r| r.id == message),
        "hidden"
    );
    assert_eq!(count(&db, rows), 1, "and stored");
    let commands = [
        ("RetryMessage", UiCommand::RetryMessage { message }),
        ("CancelMessage", UiCommand::CancelMessage { message }),
        (
            "EditMessage",
            UiCommand::EditMessage {
                message,
                text: Text::plain("changed"),
            },
        ),
        (
            "DeleteMessage",
            UiCommand::DeleteMessage {
                message,
                for_everyone: true,
            },
        ),
    ];
    for (action, command) in commands {
        env.handle.send(command).expect("send");
        let why = env
            .event("a refusal", |e| match e {
                UiEvent::ActionFailed {
                    action: a, error, ..
                } if *a == action => Some(error.refusal()),
                _ => None,
            })
            .await;
        assert_eq!(why, Some(Refusal::Gone), "{action}");
    }
    assert_eq!(
        count(&db, "SELECT count(*) FROM outbox WHERE state = 'Failed'"),
        1,
        "untouched"
    );
    // Confirmed: erased, and its send row with it.
    env.handle.send(UiCommand::UiReady).expect("send");
    verdict.send(online()).expect("the judge runs");
    let db2 = db.clone();
    eventually("erased once the time is confirmed", WAIT, || {
        let db = db2.clone();
        async move { count(&db, rows) == 0 }
    })
    .await;
    assert_eq!(count(&db, "SELECT count(*) FROM outbox"), 0);
    env.stop().await;
}

const OTHER: &str = "aci:00000000-0000-4000-8000-00000000beef";
/// The second account's message from alice, with a timer of one minute.
const SECOND: i64 = T0 + 7;

/// Clocks are confirmed per account, by each account's own backend: at one start, the
/// account whose server took the connection erases what is due, and the one without a
/// network hides it (or keeps it, or erases it, as the one option says).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_account_follows_its_own_backend() {
    let mut s = Scene::new(None).await;
    let second = timed(
        remote(&d_alice(), &alice(), &SECOND.to_string(), SECOND, "second"),
        60,
    );
    s.env.control.push_script(
        Script::new("second-account", vec![uid(OTHER)])
            .deliver(vec![BackendEvent::Message(second)]),
    );
    let other = add_linked(&mut s.env).await;
    s.env.wait_finished(other).await;
    assert_ne!(other, s.account);
    let db = s.db();
    let conv = ConversationId(count(
        &db,
        &format!(
            "SELECT conversation_id FROM messages WHERE net_local = '{SECOND}' AND account_id = {}",
            other.0
        ),
    ));

    // The first account's backend is online at the next start, the second has no network.
    s.env.stop().await;
    s.env.clock.set(START + 10 * MINUTE);
    let _first = s.judge.judge_account(s.account, online());
    let _second = s.judge.judge_account(other, offline());
    let core = s
        .judge
        .start(
            s.env.dir.path(),
            &s.env.vault,
            Arc::new(s.env.clock.clone()),
        )
        .await;
    s.env.handle = core.handle();
    s.env.events = s.env.handle.subscribe();
    s.env.core = Some(core);
    assert_eq!(
        (
            s.fates().await,
            fate_of(&s.env.handle, &db, conv, SECOND).await
        ),
        ([Hidden, Shown, Hidden], Hidden),
        "nobody has spoken yet"
    );
    s.env.handle.send(UiCommand::UiReady).expect("send");
    s.becomes("the first account is confirmed", [Erased, Shown, Erased])
        .await;
    settle().await;
    assert_eq!(
        fate_of(&s.env.handle, &db, conv, SECOND).await,
        Hidden,
        "the second account waits for its own backend"
    );
    s.env
        .handle
        .send(UiCommand::SetExpiryPolicy {
            policy: ExpiryPolicy::KeepUntilConfirmed,
        })
        .expect("send");
    assert_eq!(fate_of(&s.env.handle, &db, conv, SECOND).await, Shown);
    s.env
        .handle
        .send(UiCommand::SetExpiryPolicy {
            policy: ExpiryPolicy::EraseByLocalClock,
        })
        .expect("send");
    let h = s.env.handle.clone();
    eventually("erase holds no account", WAIT, || {
        let (h, db) = (h.clone(), db.clone());
        async move { fate_of(&h, &db, conv, SECOND).await == Erased }
    })
    .await;
    s.env.stop().await;
}

// ---------- a kill between hiding and erasing ----------

const CHILD_HOME: &str = "KOURIER_TEST_EXPIRY_KILL_HOME";
const CHILD_PHASE: &str = "KOURIER_TEST_EXPIRY_KILL_PHASE";
const SIGABRT: i32 = 6;
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);

/// The child of [`hidden_messages_survive_a_kill_and_are_erased_once_the_time_is_confirmed`].
#[test]
fn expiry_kill_child() {
    let (Ok(home), Ok(phase)) = (std::env::var(CHILD_HOME), std::env::var(CHILD_PHASE)) else {
        return;
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(kill_child(PathBuf::from(home), &phase));
}

/// `hide`: link, receive, stop; start ten minutes later without a network, see the picture
/// hidden, and die (abort: no destructor, no flush, like a power cut). `after`: a new
/// process on the same files, still without a network.
async fn kill_child(home: PathBuf, phase: &str) {
    let db = home.join("data").join("core.db");
    let vault = Arc::new(DirVault::new(home.join("data")));
    let fake = FakeNetwork::new(Flavor::SignalLike, timers_script());
    let judge = ClockJudge::new(fake.clone());
    let (alice, bob) = (d_alice(), d_bob());
    let fates = |h: CoreHandle| {
        let (db, alice, bob) = (db.clone(), alice.clone(), bob.clone());
        async move {
            let a = conv_id(&db, &alice).expect("alice's chat");
            let b = conv_id(&db, &bob).expect("bob's chat");
            [
                fate_of(&h, &db, a, PHOTO).await,
                fate_of(&h, &db, a, HOUR).await,
                fate_of(&h, &db, b, BOBS).await,
            ]
        }
    };
    let stored = |db: &Path| {
        (
            count(db, "SELECT count(*) FROM messages WHERE expiry_hidden = 1"),
            count(db, "SELECT count(*) FROM attachments"),
            count(db, "SELECT count(*) FROM files"),
            count(db, "SELECT count(*) FROM expired_messages"),
        )
    };
    if phase == "hide" {
        let clock = FakeClock::new(START);
        let core = judge.start(&home, &vault, Arc::new(clock.clone())).await;
        let h = core.handle();
        h.send(UiCommand::UiReady).expect("send");
        h.send(UiCommand::AddAccount {
            network: NetworkKind::Fake,
            flow: "qr".into(),
        })
        .expect("send");
        let active = |h: CoreHandle| async move {
            match h.query(Query::Accounts).await.expect("accounts") {
                QueryData::Accounts(a) => a
                    .into_iter()
                    .find(|a| a.state == AccountState::Active)
                    .map(|a| a.uuid),
                _ => None,
            }
        };
        let h2 = h.clone();
        eventually("an active account", WAIT, || {
            let h = h2.clone();
            async move { active(h).await.is_some() }
        })
        .await;
        let uuid = active(h.clone()).await.expect("active");
        let control = fake.control();
        tokio::time::timeout(WAIT, control.wait_for(|c| c.is_finished(uuid)))
            .await
            .expect("script finished");
        let db2 = db.clone();
        eventually("the picture is downloaded", WAIT, || {
            let db = db2.clone();
            async move {
                count(
                    &db,
                    "SELECT count(*) FROM attachments WHERE state = '\"Local\"'
                       AND thumb_file_id IS NOT NULL AND derived_at IS NOT NULL",
                ) == 1
            }
        })
        .await;
        core.shutdown().await;
        clock.set(START + 10 * MINUTE);
        let _verdict = judge.judge_next(offline());
        let core = judge.start(&home, &vault, Arc::new(clock.clone())).await;
        let h = core.handle();
        h.send(UiCommand::UiReady).expect("send");
        assert_eq!(fates(h.clone()).await, [Hidden, Shown, Hidden]);
        assert_eq!(stored(&db), (2, 1, 2, 0));
        eprintln!("hidden; dying");
        std::process::abort();
    }
    // After the kill: a minute later, still no network.
    let clock = FakeClock::new(START + 11 * MINUTE);
    let verdict = judge.judge_next(offline());
    let core = judge.start(&home, &vault, Arc::new(clock.clone())).await;
    let h = core.handle();
    assert_eq!(
        fates(h.clone()).await,
        [Hidden, Shown, Hidden],
        "nothing shows at the first read after the kill"
    );
    assert_eq!(stored(&db), (2, 1, 2, 0), "and nothing is lost");
    h.send(UiCommand::UiReady).expect("send");
    settle().await;
    assert_eq!(fates(h.clone()).await, [Hidden, Shown, Hidden]);
    assert_eq!(stored(&db), (2, 1, 2, 0));
    // The first connect confirms the clock: erased for good.
    verdict.send(online()).expect("the judge runs");
    let h2 = h.clone();
    eventually("erased once the time is confirmed", WAIT, || {
        let h = h2.clone();
        async move { fates(h).await == [Erased, Shown, Erased] }
    })
    .await;
    assert_eq!(stored(&db), (0, 0, 0, 2));
    core.shutdown().await;
}

fn run_kill_child(home: &Path, phase: &str) -> std::process::ExitStatus {
    let exe = std::env::current_exe().expect("test binary");
    let log = home.join(format!("child-{phase}.log"));
    let mut child = Command::new(exe)
        .args([
            "--exact",
            "expiry_kill_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_HOME, home)
        .env(CHILD_PHASE, phase)
        .env_remove(kourier_db::failpoint::ENV)
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(log).expect("log file")))
        .spawn()
        .expect("spawn child");
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            return status;
        }
        if start.elapsed() > CHILD_TIMEOUT {
            let _ = child.kill();
            panic!("child hung in phase {phase}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn child_log(home: &Path, phase: &str) -> String {
    let log = std::fs::read_to_string(home.join(format!("child-{phase}.log"))).unwrap_or_default();
    let tail: Vec<&str> = log.lines().rev().take(40).collect();
    tail.into_iter().rev().collect::<Vec<_>>().join("\n")
}

/// The process is killed between hiding and erasing. The next start, still without a
/// network, shows nothing of the hidden messages at its first read and has lost nothing of
/// them (rows, attachment, files, and no key was remembered as expired); the first connect
/// then erases them for good.
#[test]
fn hidden_messages_survive_a_kill_and_are_erased_once_the_time_is_confirmed() {
    if std::env::var(CHILD_HOME).is_ok() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let killed = run_kill_child(dir.path(), "hide");
    assert_eq!(
        killed.signal(),
        Some(SIGABRT),
        "the first run did not get as far as hiding: {killed:?}\n{}",
        child_log(dir.path(), "hide")
    );
    let after = run_kill_child(dir.path(), "after");
    assert!(
        after.success(),
        "the run after the kill: {after:?}\n{}",
        child_log(dir.path(), "after")
    );
}

// ---------- the sweeper, row by row ----------

/// The sweeper on one database, without a core around it: what each option does to a due
/// message of an account whose clock is unconfirmed, and that hiding and showing again
/// leaves the database exactly as it was.
#[test]
fn the_sweeper_hides_shows_and_erases_by_the_option() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([7u8; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness");
    let mut mention = remote(&d_alice(), &alice(), "101", T0 + 1, "one minute");
    mention.mentions_me = true;
    let events = vec![
        BackendEvent::Message(remote(&d_alice(), &alice(), "100", T0, "stays")),
        BackendEvent::Message(timed(mention, 60)),
        BackendEvent::Message(timed(
            remote(&d_bob(), &bob(), "102", T0 + 2, "bob, one minute"),
            60,
        )),
    ];
    h.set_now(T0 + 5_000);
    h.apply(events.clone()).expect("apply");
    assert_eq!(
        h.expiry_policy().expect("policy"),
        ExpiryPolicy::HideUntilConfirmed,
        "no row: the default"
    );
    let before = h.dump(DumpMode::Exact).expect("dump");
    let hidden = |h: &IngestHarness| -> i64 {
        h.connection()
            .query_row(
                "SELECT count(*) FROM messages WHERE expiry_hidden = 1",
                [],
                |r| r.get(0),
            )
            .expect("count")
    };
    let rows = |h: &IngestHarness, table: &str| -> i64 {
        h.connection()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .expect("count")
    };
    let counters = |h: &IngestHarness| -> Vec<(u32, u32, i64, bool)> {
        h.conversations(&Default::default())
            .expect("list")
            .rows
            .iter()
            .map(|c| (c.unread, c.mentions, c.last_activity, c.preview.is_some()))
            .collect()
    };
    let shown_before = counters(&h);

    // Not due: no option touches anything.
    for policy in ExpiryPolicy::ALL {
        assert_eq!(h.sweep_unconfirmed(policy).expect("sweep"), 0, "{policy:?}");
    }
    assert_eq!(h.dump(DumpMode::Exact).expect("dump"), before);

    // Due by the local clock, unconfirmed.
    h.set_now(T0 + 10 * 60_000);
    assert_eq!(
        h.sweep_unconfirmed(ExpiryPolicy::KeepUntilConfirmed)
            .expect("sweep"),
        0
    );
    assert_eq!(h.dump(DumpMode::Exact).expect("dump"), before, "keep keeps");
    assert_eq!(
        h.sweep_unconfirmed(ExpiryPolicy::HideUntilConfirmed)
            .expect("sweep"),
        2,
        "both chats change"
    );
    assert_eq!((hidden(&h), rows(&h, "messages")), (2, 3));
    assert_eq!(rows(&h, "expired_messages"), 0);
    let mut now = counters(&h);
    now.sort();
    assert!(
        now.contains(&(0, 0, 0, false)),
        "bob's chat reads as empty: {now:?}"
    );
    assert!(
        now.iter().all(|c| c.1 == 0) && now.iter().map(|c| c.0).sum::<u32>() == 1,
        "one unread message shows, and no mention: {now:?}"
    );
    assert_eq!(
        h.sweep_unconfirmed(ExpiryPolicy::HideUntilConfirmed)
            .expect("sweep"),
        0,
        "nothing more to do"
    );
    // A late copy of a hidden message is the duplicate it would be of a shown one.
    h.apply(events.clone()).expect("apply");
    assert_eq!((hidden(&h), rows(&h, "messages")), (2, 3));

    // The clock is set back: by it nothing is due, so it shows again, exactly as it was.
    h.set_now(T0 + 5_000);
    assert_eq!(
        h.sweep_unconfirmed(ExpiryPolicy::HideUntilConfirmed)
            .expect("sweep"),
        2
    );
    assert_eq!(h.dump(DumpMode::Exact).expect("dump"), before);
    assert_eq!(counters(&h), shown_before);

    // The option, changed in each direction, in the transaction that stores it.
    h.set_now(T0 + 10 * 60_000);
    assert_eq!(
        h.set_expiry_policy(ExpiryPolicy::HideUntilConfirmed, true)
            .expect("set"),
        2
    );
    assert_eq!(hidden(&h), 2);
    assert_eq!(
        h.set_expiry_policy(ExpiryPolicy::KeepUntilConfirmed, true)
            .expect("set"),
        2
    );
    assert_eq!(hidden(&h), 0);
    assert_eq!(
        h.expiry_policy().expect("policy"),
        ExpiryPolicy::KeepUntilConfirmed
    );
    // keep → erase: what is due and was readable is out of every read with the command; its
    // rows and files go with the sweep that follows, so the command itself erases nothing.
    assert_eq!(
        h.set_expiry_policy(ExpiryPolicy::EraseByLocalClock, true)
            .expect("set"),
        2
    );
    assert_eq!(
        (
            hidden(&h),
            rows(&h, "messages"),
            rows(&h, "expired_messages")
        ),
        (2, 3, 0)
    );
    let mut now = counters(&h);
    now.sort();
    assert!(
        now.contains(&(0, 0, 0, false)),
        "bob's chat reads as empty: {now:?}"
    );
    // Back to keep before the sweeper had its turn: nothing was lost.
    assert_eq!(
        h.set_expiry_policy(ExpiryPolicy::KeepUntilConfirmed, true)
            .expect("set"),
        2
    );
    assert_eq!(hidden(&h), 0);
    assert_eq!(counters(&h), shown_before);
    // From hide, a change to erase shows nothing in between; the sweep erases.
    h.set_expiry_policy(ExpiryPolicy::HideUntilConfirmed, true)
        .expect("set");
    assert_eq!(hidden(&h), 2);
    assert_eq!(
        h.set_expiry_policy(ExpiryPolicy::EraseByLocalClock, true)
            .expect("set"),
        0,
        "hidden and due: the sweep that follows erases it, nothing shows in between"
    );
    assert_eq!(hidden(&h), 2);
    assert_eq!(
        h.sweep_unconfirmed(ExpiryPolicy::EraseByLocalClock)
            .expect("sweep"),
        2
    );
    assert_eq!((hidden(&h), rows(&h, "messages")), (0, 1));
    assert_eq!(rows(&h, "expired_messages"), 2);
    // Erased is remembered: the late copy does not come back.
    h.apply(events).expect("apply");
    assert_eq!(rows(&h, "messages"), 1);

    // A value a newer build wrote reads as the default, which erases nothing early.
    h.connection()
        .execute(
            "UPDATE settings SET value = 'shred' WHERE key = 'expiry_policy'",
            [],
        )
        .expect("write");
    assert_eq!(
        h.expiry_policy().expect("policy"),
        ExpiryPolicy::HideUntilConfirmed
    );
}

/// A hidden message is still a message: what arrives for it meanwhile is applied, and it
/// shows with it when it shows again. Confirmed, a hidden message is erased like any other.
#[test]
fn a_hidden_message_still_takes_what_arrives_for_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([7u8; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness");
    h.set_now(T0 + 5_000);
    h.apply(vec![BackendEvent::Message(timed(
        remote(&d_alice(), &alice(), "101", T0 + 1, "one minute"),
        60,
    ))])
    .expect("apply");
    let conv = h
        .conversations(&Default::default())
        .expect("list")
        .rows
        .first()
        .expect("alice's chat")
        .id;
    let page = |h: &IngestHarness| {
        h.timeline(&TimelineQuery {
            conv,
            anchor: TimelineAnchor::Newest,
            limit: 10,
        })
        .expect("timeline")
        .rows
    };
    let id = page(&h)[0].id;
    h.set_now(T0 + 10 * 60_000);
    h.sweep_unconfirmed(ExpiryPolicy::HideUntilConfirmed)
        .expect("sweep");
    assert!(page(&h).is_empty());
    assert!(h.revisions(id).expect("revisions").is_none());
    h.apply(vec![BackendEvent::Reaction {
        target: target(Some(&d_alice()), Some(&alice()), "101"),
        by: Some(alice()),
        emoji: "👍".into(),
        remove: false,
        at: T0 + 9_000,
    }])
    .expect("apply");
    assert!(page(&h).is_empty(), "still hidden");
    // The clock was wrong: set back, the message shows with its reaction.
    h.set_now(T0 + 6_000);
    h.sweep_unconfirmed(ExpiryPolicy::HideUntilConfirmed)
        .expect("sweep");
    let rows = page(&h);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].reactions.len(), 1, "{:?}", rows[0].reactions);
    // Hidden again, and then confirmed: erased.
    h.set_now(T0 + 10 * 60_000);
    h.sweep_unconfirmed(ExpiryPolicy::HideUntilConfirmed)
        .expect("sweep");
    assert!(page(&h).is_empty());
    assert_eq!(h.sweep().expect("sweep"), 1);
    let left: i64 = h
        .connection()
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .expect("count");
    assert_eq!(left, 0);
}

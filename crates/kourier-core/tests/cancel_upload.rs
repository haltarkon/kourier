//! Upload progress, the core's half (call 1 of the step): which message in flight may be
//! STOPPED. The fake reports an upload phase ([`UploadScript`]), the core keeps the set of
//! messages whose upload is reported moving, and the row's `cancel_sending` flag is
//! `actions::stoppable` (in flight AND an upload of it is reported AND the first attempt AND
//! no recipient has it) or a take-back. A delete command still refuses a message in flight
//! (call 3). Every term has a test that fails without it.
//!
//! The X itself (C3b, calls 2 and 3 and critic B2): pressing it writes the stop down
//! (`outbox.cancel_state`) and asks the backend, whose answer is final: `Stopping` takes the
//! message back with its files, `TooLate` leaves it alone and says so once it went out. A kill
//! between the answer and the end of the attempt is tried in a child process that aborts at the
//! failpoint right behind the answer's commit.

use crate::support;

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kourier_backend_api::{AbortAnswer, Clock as _};
use kourier_core::AccountVault;
use kourier_core::facade::{
    MessageRow, Progress, Query, QueryData, TimelineAnchor, TimelineQuery, UiCommand, UiEvent,
};
use kourier_fake::{Flavor, Script, TransmitRule, UploadScript};
use kourier_media::{SampleFormat, sample_image};
use kourier_model::{
    AccountId, AccountUuid, ConvKey, ConversationId, MessageId, OutgoingState, SendPhase,
};
use support::send::*;
use support::{Env, WAIT, add_linked, eventually};

/// How long the fake's held upload phase lasts (account clock).
const HOLD: Duration = Duration::from_secs(10);
/// How long the fake server's reply is late after the phase, where a test wants the message
/// to stay in flight past the end of its uploads.
const LATE: Duration = Duration::from_secs(60);

async fn linked(script: Script) -> (Env, AccountId, AccountUuid) {
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("account").uuid;
    (env, account, uuid)
}

async fn conv(env: &Env, key: &ConvKey) -> ConversationId {
    let db = env.core_db();
    let key2 = key.clone();
    eventually("the conversation", WAIT, || {
        let (db, key) = (db.clone(), key2.clone());
        async move { conv_id(&db, &key).is_some() }
    })
    .await;
    conv_id(&db, key).expect("conversation")
}

/// The timeline row of `id`, as the facade answers it now.
async fn row(env: &Env, conv: ConversationId, id: MessageId) -> MessageRow {
    let q = TimelineQuery {
        conv,
        anchor: TimelineAnchor::Newest,
        limit: 500,
    };
    match env
        .handle
        .query(Query::Timeline(q))
        .await
        .expect("timeline")
    {
        QueryData::Timeline(p) => p.rows.into_iter().find(|r| r.id == id).expect("row"),
        other => panic!("unexpected {other:?}"),
    }
}

/// Waits until the row of `id` satisfies `f` and returns it.
async fn row_when(
    env: &Env,
    conv: ConversationId,
    id: MessageId,
    what: &str,
    f: impl Fn(&MessageRow) -> bool,
) -> MessageRow {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let r = row(env, conv, id).await;
        if f(&r) {
            return r;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A JPEG file to send.
fn jpeg_file(dir: &tempfile::TempDir, name: &str, seed: u64) -> std::path::PathBuf {
    let path = dir.path().join(name);
    let bytes = sample_image(320, 240, seed, SampleFormat::Jpeg).expect("jpeg");
    std::fs::write(&path, bytes).expect("write");
    path
}

/// Sends one picture into `conv`; returns the message once its row exists.
async fn send_picture(
    env: &Env,
    conv: ConversationId,
    dir: &tempfile::TempDir,
    n: usize,
) -> MessageId {
    env.handle
        .send(UiCommand::SendFiles {
            request: Default::default(),
            as_file: Vec::new(),
            with_location: Vec::new(),
            conv,
            paths: vec![jpeg_file(dir, &format!("picture{n}.jpg"), 6 + n as u64)].into(),
            caption: None,
            reply_to: None,
        })
        .expect("send");
    let db = env.core_db();
    eventually("the message", WAIT, || {
        let db = db.clone();
        async move { own_messages(&db, conv).len() >= n }
    })
    .await;
    own_messages(&db, conv)[n - 1].id
}

/// The next upload report of the fake that is NOT the end of an upload (`done < total`).
async fn moving(env: &mut Env, message: MessageId) -> (u64, u64) {
    env.event("a moving upload report", |e| match e {
        UiEvent::Progress(Progress::Upload {
            message: m,
            done,
            total,
            ..
        }) if *m == message && done < total => Some((*done, *total)),
        _ => None,
    })
    .await
}

/// The next upload report that ends an upload (`done == total`).
async fn ended(env: &mut Env, message: MessageId) {
    env.event("the end of an upload", |e| match e {
        UiEvent::Progress(Progress::Upload {
            message: m,
            done,
            total,
            ..
        }) if *m == message && done == total => Some(()),
        _ => None,
    })
    .await;
}

/// Moves the clock by `d` once a sleep of `d` from now waits on it: the fake's late reply
/// (`actor.rs`, `delay_reply`) is armed a moment AFTER the upload's last report, and a sleep's
/// deadline is taken from the clock when it is made, so an advance that comes first carries the
/// deadline along and the reply is never due. (A held upload needs none of this: `upload.rs`
/// fixes the hold's end before its first report.)
async fn advance_armed(env: &Env, d: Duration) {
    let want = env.clock.now_ms() + u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    let deadline = tokio::time::Instant::now() + WAIT;
    while !env.clock.deadlines().contains(&want) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no sleep of {d:?} armed on the clock"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    env.clock.advance(d);
}

/// The error text of the next `ActionFailed` of `action`.
async fn refused(env: &mut Env, action: &'static str) -> String {
    env.event(action, |e| match e {
        UiEvent::ActionFailed {
            action: a, error, ..
        } if *a == action => Some(error.to_string()),
        _ => None,
    })
    .await
}

/// The outbox row of `message`: (state, attempts).
fn outbox_row(env: &Env, message: MessageId) -> (String, i64) {
    env.peek(|c| {
        c.query_row(
            "SELECT state, attempts FROM outbox WHERE message_id = ?1 AND kind = 'Message'",
            [message.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("outbox row")
    })
}

async fn sent_state(env: &Env, conv: ConversationId, message: MessageId) {
    let db = env.core_db();
    eventually("the message is sent", WAIT, || {
        let db = db.clone();
        async move {
            own_messages(&db, conv)
                .iter()
                .any(|m| m.id == message && matches!(m.out_state, Some(OutgoingState::Sent { .. })))
        }
    })
    .await;
}

/// The script: the fake reports a held upload of one picture at 45 % in `conv`.
fn uploading(name: &str, conv: ConvKey) -> Script {
    script(name).with_uploads(
        conv,
        UploadScript::Held {
            shares: vec![0.45],
            hold: HOLD,
        },
    )
}

/// Call 1, terms 1 and 2, and call 3. While the fake reports a moving upload of a first
/// attempt the row offers "Cancel Sending" (`cancel_sending`) and nothing that deletes; the
/// delete commands are refused as before and the send still happens. When the uploads are
/// over (the final `TransmitProgress`), the message is in flight still and no longer
/// stoppable: the core emptied its set at that report, not at the answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_is_stoppable_while_its_first_attempt_reports_an_upload() {
    let s = uploading("stop", d_alice()).delay_reply(d_alice(), LATE);
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let c = conv(&env, &d_alice()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let id = send_picture(&env, c, &dir, 1).await;

    // The fake reports 45 % of the picture, with the attachment's id (the core's mapping).
    let (done, total) = moving(&mut env, id).await;
    assert!(done > 0 && done < total, "45 % of {total}: {done}");
    let r = row_when(&env, c, id, "Cancel Sending is offered", |r| {
        r.actions.cancel_sending
    })
    .await;
    // What "Cancel Sending" offers is a STOP; no delete is offered (call 3).
    assert!(!r.actions.take_back && !r.actions.delete_for_me && !r.actions.delete_for_everyone);
    assert_eq!(outbox_row(&env, id), ("Transmitting".to_owned(), 1));

    // H3: a delete of the uploading row is refused, whatever the user asks.
    for for_everyone in [false, true] {
        env.handle
            .send(UiCommand::DeleteMessage {
                message: id,
                for_everyone,
            })
            .expect("send");
        let error = refused(&mut env, "DeleteMessage").await;
        assert!(error.contains("being sent"), "{error}");
    }
    assert!(row(&env, c, id).await.actions.cancel_sending);

    // The uploads end: the fake's final `TransmitProgress { 1.0, None }` takes the message
    // out of the set, while the server's late reply keeps it in flight.
    env.clock.advance(HOLD);
    ended(&mut env, id).await;
    let r = row_when(&env, c, id, "Cancel Sending is gone", |r| {
        !r.actions.cancel_sending
    })
    .await;
    assert!(!r.actions.take_back && !r.actions.delete_for_me);
    assert_eq!(
        outbox_row(&env, id).0,
        "Transmitting",
        "still in flight: the flag went with the uploads, not with the answer"
    );
    advance_armed(&env, LATE).await;
    sent_state(&env, c, id).await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        1,
        "the send still happened"
    );
    assert!(
        env.control.aborts(uuid).is_empty(),
        "nothing asked the fake to stop"
    );
}

/// Call 1, term 2. A message in flight whose backend reports no upload (the fake reports a
/// phase of a group send and nothing per file) cannot be stopped: it never gets an X, an abort
/// could not be answered. The command is refused as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_in_flight_without_a_reported_upload_cannot_be_stopped() {
    let s = script("phase").slow_sends(Some(SendPhase::RefreshingGroup), LATE);
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let c = conv(&env, &d_alice()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let id = send_picture(&env, c, &dir, 1).await;

    // The server has the request; the answer is late: the message is in flight.
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| c.transmits(uuid).len() == 1))
        .await
        .expect("the transmit");
    let r = row(&env, c, id).await;
    assert_eq!(outbox_row(&env, id).0, "Transmitting");
    assert!(!r.actions.cancel_sending && !r.actions.take_back);

    env.handle
        .send(UiCommand::CancelMessage { message: id })
        .expect("send");
    let error = refused(&mut env, "CancelMessage").await;
    assert!(error.contains("being sent"), "{error}");
    assert!(env.control.aborts(uuid).is_empty());

    advance_armed(&env, LATE).await;
    sent_state(&env, c, id).await;
}

/// Call 1, term 3 (critic B1). The attempt after a temporary failure uploads again, but the
/// failed one may have reached the server (a time-out says nothing): the row is not stoppable,
/// the command is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_that_uploads_again_cannot_be_stopped() {
    let s = uploading("retry", d_alice())
        .fail_transmit(TransmitRule::attempts(transient(), 1).in_conv(d_alice()));
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let c = conv(&env, &d_alice()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let id = send_picture(&env, c, &dir, 1).await;

    // The first attempt is stoppable while its upload moves ...
    moving(&mut env, id).await;
    row_when(&env, c, id, "stoppable", |r| r.actions.cancel_sending).await;
    // ... and its server write fails with a temporary error after the uploads ended.
    env.clock.advance(HOLD);
    ended(&mut env, id).await;

    // The second attempt (at once: the first wait is 0 ms) uploads again: reported, not
    // stoppable.
    moving(&mut env, id).await;
    let (state, attempts) = outbox_row(&env, id);
    assert_eq!((state.as_str(), attempts), ("Transmitting", 2));
    let r = row(&env, c, id).await;
    assert!(
        !r.actions.cancel_sending,
        "an earlier attempt may have reached the server"
    );
    env.handle
        .send(UiCommand::CancelMessage { message: id })
        .expect("send");
    let error = refused(&mut env, "CancelMessage").await;
    assert!(error.contains("being sent"), "{error}");
    assert!(env.control.aborts(uuid).is_empty());

    env.clock.advance(HOLD);
    sent_state(&env, c, id).await;
}

/// Call 1, term 4 (critic B1). The send "Retry" makes to the members that failed uploads again
/// as a FIRST attempt of its own, but a member has the message already: it is not stoppable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resend_to_the_members_that_failed_cannot_be_stopped() {
    let s = uploading("partial", group()).fail_transmit(TransmitRule::member(
        alice(),
        unregistered(),
        1,
    ));
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let g = conv(&env, &group()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let id = send_picture(&env, g, &dir, 1).await;

    // The first send is stoppable; bob gets it, alice is refused for good.
    moving(&mut env, id).await;
    row_when(&env, g, id, "stoppable", |r| r.actions.cancel_sending).await;
    env.clock.advance(HOLD);
    ended(&mut env, id).await;
    let db = env.core_db();
    eventually("a part of the group has it", WAIT, || {
        let db = db.clone();
        async move {
            own_messages(&db, g).iter().any(|m| {
                m.id == id && matches!(m.out_state, Some(OutgoingState::PartiallyFailed { .. }))
            })
        }
    })
    .await;

    // "Retry" sends to alice again: the first attempt of the row, uploading again.
    env.handle
        .send(UiCommand::RetryMessage { message: id })
        .expect("send");
    moving(&mut env, id).await;
    let (state, attempts) = outbox_row(&env, id);
    assert_eq!(
        (state.as_str(), attempts),
        ("Transmitting", 1),
        "a first attempt"
    );
    let r = row(&env, g, id).await;
    assert!(!r.actions.cancel_sending, "bob has the message");
    env.handle
        .send(UiCommand::CancelMessage { message: id })
        .expect("send");
    let error = refused(&mut env, "CancelMessage").await;
    assert!(error.contains("being sent"), "{error}");
    assert!(env.control.aborts(uuid).is_empty());

    env.clock.advance(HOLD);
    sent_state(&env, g, id).await;
}

/// The mark of the message's `Message` outbox row (`outbox.cancel_state`).
fn cancel_state(env: &Env, message: MessageId) -> i64 {
    env.peek(|c| {
        c.query_row(
            "SELECT cancel_state FROM outbox WHERE message_id = ?1 AND kind = 'Message'",
            [message.0],
            |r| r.get(0),
        )
        .expect("outbox row")
    })
}

/// The blob files (relative paths) the message's attachments own.
fn blobs_of(env: &Env, message: MessageId) -> Vec<String> {
    env.peek(|c| {
        let mut q = c
            .prepare(
                "SELECT f.path FROM files f JOIN attachments a
                   ON f.id = a.file_id OR f.id = a.thumb_file_id WHERE a.message_id = ?1",
            )
            .expect("prepare");
        q.query_map([message.0], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
    })
}

fn rows_of(env: &Env, table: &str, message: MessageId) -> i64 {
    env.peek(|c| {
        c.query_row(
            &format!("SELECT count(*) FROM {table} WHERE message_id = ?1"),
            [message.0],
            |r| r.get(0),
        )
        .expect("count")
    })
}

/// Call 2 and critic B2. The X on a message that uploads: the core writes the stop down, asks
/// the backend and the backend (the fake, which holds the uploads) says `Stopping`. The message
/// goes with its attachment rows and its files, nothing reached the fake server, and the chat's
/// next message, which waited behind it, is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_takes_the_message_back_with_its_files_and_the_chat_goes_on() {
    let (mut env, account, uuid) = linked(uploading("stop-b", d_alice())).await;
    env.wait_finished(account).await;
    let c = conv(&env, &d_alice()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let first = send_picture(&env, c, &dir, 1).await;
    let second = send_picture(&env, c, &dir, 2).await;
    moving(&mut env, first).await;
    row_when(&env, c, first, "stoppable", |r| r.actions.cancel_sending).await;
    let owned = blobs_of(&env, first);
    assert!(!owned.is_empty(), "the picture's files");
    let blobs = env.vault.blob_dir(uuid);
    for path in &owned {
        assert!(blobs.join(path).exists(), "{path} is there before the stop");
    }

    env.handle
        .send(UiCommand::CancelMessage { message: first })
        .expect("send");
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| !c.aborts(uuid).is_empty()))
        .await
        .expect("the fake was asked to stop");
    let asked = env.control.aborts(uuid);
    assert_eq!(asked.len(), 1, "one AbortTransmit");
    assert_eq!(asked[0].answer, AbortAnswer::Stopping);

    // The attempt ends `Failed` having sent nothing; the message is taken back whole.
    let db = env.core_db();
    eventually("the message is gone", WAIT, || {
        let db = db.clone();
        async move { own_messages(&db, c).iter().all(|m| m.id != first) }
    })
    .await;
    assert_eq!(rows_of(&env, "attachments", first), 0);
    assert_eq!(rows_of(&env, "outbox", first), 0);
    // The files go AFTER the commit that took the rows away: `supervisor::publish` hands them to
    // `attachments::unlink` (a blocking task) a few statements after the outcome's changes were
    // sent, so a look at the disk the moment the rows are gone can still find them (it did, once in
    // a few runs at a load of 60+: "went with the message ... left: (0, true)"). Wait for them,
    // bounded; a take-back that never unlinks still fails here, after `WAIT`.
    let still_there = || owned.iter().any(|p| blobs.join(p).exists());
    eventually("the files of the message are unlinked", WAIT, || async {
        !still_there()
    })
    .await;
    for path in &owned {
        let (rows, on_disk) = (
            env.peek(|c| {
                c.query_row("SELECT count(*) FROM files WHERE path = ?1", [path], |r| {
                    r.get::<_, i64>(0)
                })
                .expect("count")
            }),
            blobs.join(path).exists(),
        );
        assert_eq!((rows, on_disk), (0, false), "{path} went with the message");
    }
    assert!(
        env.control.transmits(uuid).is_empty(),
        "the server never had it"
    );

    // The chat is not blocked: the message behind it uploads and is sent.
    moving(&mut env, second).await;
    env.clock.advance(HOLD);
    sent_state(&env, c, second).await;
    assert_eq!(
        env.control.transmits(uuid).len(),
        1,
        "only the second message reached the server"
    );
    assert_eq!(env.control.aborts(uuid).len(), 1);
}

/// Call 2 and critic B2, the other answer. The fake says `TooLate` (the message is going out
/// anyway): the message stays, the X is gone at once, the mark is cleared durably, the message
/// ends sent, and only THEN does the core say the X came too late (so the message's Delete for
/// Everyone works when the note shows).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_that_comes_too_late_changes_nothing_and_is_told_after_the_send() {
    // The fake's `TooLate` ends its held phase at the abort and the send goes out; the server's
    // reply is late, so the message is in flight when the test looks.
    let s = script("stop-c")
        .with_uploads(
            d_alice(),
            UploadScript::TooLate {
                shares: vec![0.45],
                hold: HOLD,
            },
        )
        .delay_reply(d_alice(), LATE);
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let c = conv(&env, &d_alice()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let id = send_picture(&env, c, &dir, 1).await;
    moving(&mut env, id).await;
    row_when(&env, c, id, "stoppable", |r| r.actions.cancel_sending).await;

    env.handle
        .send(UiCommand::CancelMessage { message: id })
        .expect("send");
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| !c.aborts(uuid).is_empty()))
        .await
        .expect("the fake was asked to stop");
    assert_eq!(env.control.aborts(uuid)[0].answer, AbortAnswer::TooLate);

    // Still in flight, no X any more (it cannot be stopped), and the mark is cleared durably.
    let r = row_when(&env, c, id, "the X is gone", |r| !r.actions.cancel_sending).await;
    assert!(
        !r.actions.take_back && !r.actions.delete_for_everyone,
        "{:?}",
        r.actions
    );
    // The X goes as soon as the mark says ASKED, a moment before the dispatcher writes the
    // backend's TooLate as 0: wait for the 0 (the send is held until LATE, so it cannot end first).
    let deadline = tokio::time::Instant::now() + WAIT;
    while cancel_state(&env, id) != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "TooLate clears the mark while the message is in flight"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(outbox_row(&env, id).0, "Transmitting");
    while let Ok(e) = env.events.try_recv() {
        assert!(
            !matches!(e, UiEvent::CancelTooLate { .. }),
            "nothing is said before the message went out: {e:?}"
        );
    }

    // The server answers: the message is sent, THEN the note comes, with the message already sent.
    advance_armed(&env, LATE).await;
    let (said_account, said_conv, said_message) = env
        .event("CancelTooLate", |e| match e {
            UiEvent::CancelTooLate {
                account,
                conv,
                message,
            } => Some((*account, *conv, *message)),
            _ => None,
        })
        .await;
    assert_eq!((said_account, said_conv, said_message), (account, c, id));
    let db = env.core_db();
    assert!(
        own_messages(&db, c)
            .iter()
            .any(|m| m.id == id && matches!(m.out_state, Some(OutgoingState::Sent { .. }))),
        "the message is sent when the note comes"
    );
    assert_eq!(env.control.transmits(uuid).len(), 1);
}

/// Fix U3. A backend that says `SendPhase::Preparing` (it makes the files ready before the first
/// byte) reports an upload of the message as far as the stop is concerned: the row offers
/// "Cancel Sending" while NO file is reported yet, the X is answered `Stopping`, and the
/// message is taken back with nothing sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_whose_files_are_being_prepared_can_be_stopped() {
    let s = script("stop-e").with_uploads(d_alice(), UploadScript::Preparing { hold: HOLD });
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let c = conv(&env, &d_alice()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let id = send_picture(&env, c, &dir, 1).await;
    env.event("the phase Preparing", |e| match e {
        UiEvent::Progress(Progress::Transmit {
            message,
            phase: Some(SendPhase::Preparing),
            ..
        }) if *message == id => Some(()),
        _ => None,
    })
    .await;
    // No file was reported (the phase said it), and the row offers the stop all the same.
    row_when(&env, c, id, "stoppable while preparing", |r| {
        r.actions.cancel_sending
    })
    .await;
    assert_eq!(outbox_row(&env, id), ("Transmitting".to_owned(), 1));

    env.handle
        .send(UiCommand::CancelMessage { message: id })
        .expect("send");
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| !c.aborts(uuid).is_empty()))
        .await
        .expect("the fake was asked to stop");
    assert_eq!(env.control.aborts(uuid)[0].answer, AbortAnswer::Stopping);
    let db = env.core_db();
    eventually("the message is taken back", WAIT, || {
        let db = db.clone();
        async move { own_messages(&db, c).is_empty() }
    })
    .await;
    assert_eq!(rows_of(&env, "outbox", id), 0);
    assert!(env.control.transmits(uuid).is_empty(), "nothing was sent");
}

/// Fix K3 (the cancel review's LOW 3). The backend's command loop is inside an emit on a FULL
/// event queue when the X comes (the fake sends twice the queue's size before it answers): it
/// can only answer after the core read. The core keeps reading events while it waits, the answer
/// is `Stopping` in time, and the message is taken back with nothing sent. A core that waited
/// for the answer without reading would run into its time-out, take the stop for `TooLate`
/// and leave the stopped message to end failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_is_answered_although_the_backend_waits_on_a_full_event_queue() {
    let s = uploading("stop-d", d_alice()).with_abort_backlog(2 * kourier_backend_api::EVENT_QUEUE);
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let c = conv(&env, &d_alice()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let id = send_picture(&env, c, &dir, 1).await;
    moving(&mut env, id).await;
    row_when(&env, c, id, "stoppable", |r| r.actions.cancel_sending).await;

    env.handle
        .send(UiCommand::CancelMessage { message: id })
        .expect("send");
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| !c.aborts(uuid).is_empty()))
        .await
        .expect("the fake was asked to stop");
    assert_eq!(env.control.aborts(uuid)[0].answer, AbortAnswer::Stopping);

    let started = Instant::now();
    let db = env.core_db();
    eventually("the message is taken back", WAIT, || {
        let db = db.clone();
        async move { own_messages(&db, c).is_empty() }
    })
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the answer came in time, not at the core's time-out: {:?}",
        started.elapsed()
    );
    assert_eq!(rows_of(&env, "outbox", id), 0);
    assert!(env.control.transmits(uuid).is_empty(), "nothing was sent");
    while let Ok(e) = env.events.try_recv() {
        assert!(
            !matches!(e, UiEvent::CancelTooLate { .. }),
            "the stop was in time: {e:?}"
        );
    }
}

const CHILD_HOME: &str = "KOURIER_TEST_CANCEL_HOME";
const CHILD_MODE: &str = "KOURIER_TEST_CANCEL_MODE";
const SIGABRT: i32 = 6;

/// The child of the kill tests: link an account, send a picture whose upload the fake holds, press
/// the X and wait. `KOURIER_FAILPOINT` aborts the process right behind the commit of the
/// backend's answer, so the attempt's end is never recorded. Run by itself it does nothing.
#[test]
fn cancel_child() {
    let (Ok(home), Ok(mode)) = (std::env::var(CHILD_HOME), std::env::var(CHILD_MODE)) else {
        return;
    };
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async move {
            let mut env = Env::inside(Path::new(&home), Flavor::SignalLike, held(&mode)).await;
            env.handle.send(UiCommand::UiReady).expect("send");
            let account = add_linked(&mut env).await;
            env.wait_finished(account).await;
            let c = conv(&env, &d_alice()).await;
            let dir = tempfile::tempdir().expect("tempdir");
            let id = send_picture(&env, c, &dir, 1).await;
            moving(&mut env, id).await;
            row_when(&env, c, id, "stoppable", |r| r.actions.cancel_sending).await;
            env.handle
                .send(UiCommand::CancelMessage { message: id })
                .expect("send");
            // The failpoint kills the process behind the answer; getting here is the failure.
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
}

/// The script of the child (`stop`: the backend can stop; `late`: it cannot any more).
fn held(mode: &str) -> Script {
    let upload = if mode == "late" {
        UploadScript::TooLate {
            shares: vec![0.45],
            hold: HOLD,
        }
    } else {
        UploadScript::Held {
            shares: vec![0.45],
            hold: HOLD,
        }
    };
    script("kill").with_uploads(d_alice(), upload)
}

/// Runs the child to its death at the failpoint behind the answer to the stop.
fn kill_child(home: &Path, mode: &str) {
    kill_child_at(home, mode, kourier_core::FAILPOINT_AFTER_CANCEL_ANSWER);
}

/// Runs the child to its death at `failpoint`.
fn kill_child_at(home: &Path, mode: &str, failpoint: &str) {
    let log = home.join("child.log");
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .arg("--exact")
        .arg(support::child_filter(module_path!(), "cancel_child"))
        .args(["--nocapture", "--test-threads=1"])
        .env(CHILD_HOME, home)
        .env(CHILD_MODE, mode)
        .env(kourier_db::failpoint::ENV, failpoint)
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&log).expect("log")))
        .spawn()
        .expect("spawn the child");
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if start.elapsed() > Duration::from_secs(120) {
            let _ = child.kill();
            panic!("the child hung");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(
        status.signal(),
        Some(SIGABRT),
        "the child was killed at the failpoint, not {status:?}:\n{}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
}

/// The conversation of `d_alice` and the one own message in it, in the profile the child left.
fn left_behind(home: &Path) -> (ConversationId, MessageId, PathBuf) {
    let profile = std::fs::read_dir(home)
        .expect("read the home")
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir() && p.join("data").exists())
        .expect("the child's profile");
    let db = profile.join("data").join("core.db");
    let c = support::send::conv_id(&db, &d_alice()).expect("the chat");
    let own = own_messages(&db, c);
    assert_eq!(own.len(), 1, "the message the child sent");
    (c, own[0].id, profile)
}

/// What a start finds of an attempt whose stop the backend accepted (mark 2) and whose end was
/// never recorded, the child having died at `failpoint`: the message is TAKEN BACK, with its
/// files (no orphan blob, no `files` row), and nothing is sent.
async fn a_start_takes_back_what_was_stopped(failpoint: &str) {
    let home = tempfile::tempdir().expect("tempdir");
    kill_child_at(home.path(), "stop", failpoint);
    let (c, id, profile) = left_behind(home.path());
    let probe = |profile: &Path| {
        let db = profile.join("data").join("core.db");
        core_db(&db, |conn| {
            conn.query_row(
                "SELECT state, cancel_state FROM outbox WHERE message_id = ?1",
                [id.0],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .expect("outbox row")
        })
    };
    assert_eq!(
        probe(&profile),
        ("Transmitting".to_owned(), 2),
        "the kill left an attempt in flight whose stop was accepted"
    );

    // A clean start over the same files.
    let env = Env::adopt(home.path(), Flavor::SignalLike, script("kill")).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = env.accounts().await[0].id;
    env.wait_finished(account).await;
    let db = env.core_db();
    eventually("the message is taken back", WAIT, || {
        let db = db.clone();
        async move { own_messages(&db, c).is_empty() }
    })
    .await;
    assert_eq!(rows_of(&env, "attachments", id), 0);
    assert_eq!(outbox_len(&db), 0);
    let uuid = env.account(account).await.expect("account").uuid;
    assert_eq!(
        support::blob_files(&env.vault.blob_dir(uuid)),
        0,
        "its files went"
    );
    assert_eq!(
        env.peek(|c| c
            .query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))
            .expect("count")),
        0,
        "and no row of the file store is left for them"
    );
    assert!(
        env.control.transmits(uuid).is_empty(),
        "the restart sent nothing"
    );
}

/// Critic B2, first half. The backend said `Stopping` (the mark is durable) and the process died
/// before the attempt's end was recorded: the next start TAKES THE MESSAGE BACK, with its files,
/// and sends nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_after_stopping_takes_the_message_back_at_the_next_start() {
    a_start_takes_back_what_was_stopped(kourier_core::FAILPOINT_AFTER_CANCEL_ANSWER).await;
}

/// Fix K4. The process died INSIDE the write of the stopped attempt's end (the failpoint behind
/// the take-back, before the commit): nothing of that write stays, the row is still
/// `Transmitting` with mark 2, and the next start takes the message back with its files and
/// leaves no orphan.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_at_the_outcome_of_a_stopped_attempt_takes_the_message_back_at_the_next_start() {
    a_start_takes_back_what_was_stopped(kourier_core::FAILPOINT_BEFORE_OUTCOME_COMMIT).await;
}

/// Fix K4. The process died right behind the commit of the X's mark (asked, nothing answered
/// yet): nothing was promised, so the next start clears the mark and the message is sent as it
/// would have been, once, and is not taken back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_behind_the_asked_mark_clears_it_and_the_send_goes_on() {
    let home = tempfile::tempdir().expect("tempdir");
    kill_child_at(
        home.path(),
        "stop",
        kourier_core::FAILPOINT_AFTER_CANCEL_ASKED,
    );
    let (c, id, profile) = left_behind(home.path());
    let db = profile.join("data").join("core.db");
    let probe = core_db(&db, |conn| {
        conn.query_row(
            "SELECT state, cancel_state FROM outbox WHERE message_id = ?1",
            [id.0],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .expect("outbox row")
    });
    assert_eq!(
        probe,
        ("Transmitting".to_owned(), 1),
        "the kill left the X asked and not answered"
    );

    // The start sends the message again; the server's reply is late, so the row can be read.
    let s = script("kill").delay_reply(d_alice(), LATE);
    let env = Env::adopt(home.path(), Flavor::SignalLike, s).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = env.accounts().await[0].id;
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("account").uuid;
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| c.transmits(uuid).len() == 1))
        .await
        .expect("the restart sent the message");
    assert_eq!(
        (outbox_row(&env, id), cancel_state(&env, id)),
        (("Transmitting".to_owned(), 2), 0),
        "attempt 2 of the same row, without the mark"
    );
    advance_armed(&env, LATE).await;
    sent_state(&env, c, id).await;
    assert_eq!(env.control.transmits(uuid).len(), 1, "sent once");
    assert!(
        env.control.aborts(uuid).is_empty(),
        "nobody was asked to stop"
    );
}

/// Critic B2, second half. The backend said `TooLate` (the mark is cleared durably) and the
/// process died before the attempt's end was recorded: the next start does NOT take the
/// message back; it goes out as it would have.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_after_too_late_does_not_take_the_message_back() {
    let home = tempfile::tempdir().expect("tempdir");
    kill_child(home.path(), "late");
    let (c, id, _) = left_behind(home.path());

    let env = Env::adopt(home.path(), Flavor::SignalLike, script("kill")).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = env.accounts().await[0].id;
    env.wait_finished(account).await;
    sent_state(&env, c, id).await;
    let uuid = env.account(account).await.expect("account").uuid;
    assert_eq!(
        env.control.transmits(uuid).len(),
        1,
        "the restart sent the message once"
    );
    assert!(env.control.aborts(uuid).is_empty());
}

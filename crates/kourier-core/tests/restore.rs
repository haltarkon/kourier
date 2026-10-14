//! Attachment backfill, core side (S6, `ingest::restore`, `restore`): a click on a file the
//! network lost asks the account's other device once per message; the wait starts when the
//! request went out (30 s, 2 min more after each "still uploading"), is durable and ends as
//! "did not answer"; an answer is matched by names first and by position only where the
//! counts prove the order; only what the user asked for downloads; answers to another
//! device's request only give pointers; a 404 of a download the USER started asks at once,
//! an automatic one and one whose pointer came from an answer do not.

use std::collections::BTreeMap;
use std::path::Path;

use kourier_backend_api::{BackendEvent, DurableCommand, RestoreSlot, RestoreUpdate};
use kourier_core::testing::{IngestHarness, Started};
use kourier_fake::attachments::Pointer;
use kourier_fake::scripts::{remote, target};
use kourier_model::{
    AccountCaps, AskFailure, Attachment, AttachmentId, AttachmentKind, AttachmentState,
    BackendBlob, BackendError, Body, CmdId, ConvKey, ErrorKind, MsgIdScope, MsgTarget, NetworkKind,
    Redacted, UserId,
};

const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";
const LIMIT: u64 = 1 << 20;
/// How long a request may take to go out before the core ends the wait: the backend's hour
/// and five minutes (`ingest::restore::UNSENT_BOUND_MS`).
const UNSENT_BOUND: i64 = 65 * 60_000;

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn d_alice() -> ConvKey {
    ConvKey::Direct(uid(ALICE))
}

fn open(path: &Path) -> IngestHarness {
    IngestHarness::open(
        path,
        &kourier_db::DbKey::from([6; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness")
}

/// A harness whose account can ask its other device (or not) and accepts files up to
/// [`LIMIT`].
fn harness(restores: bool) -> (tempfile::TempDir, IngestHarness) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = open(&dir.path().join("core.db"));
    let mut flags = BTreeMap::new();
    flags.insert(AccountCaps::FLAG_ATTACHMENT_RESTORE.to_owned(), restores);
    h.apply(vec![BackendEvent::AccountCaps(AccountCaps {
        max_receive_attachment_bytes: Some(LIMIT),
        flags,
        ..Default::default()
    })])
    .expect("caps");
    (dir, h)
}

/// A file the network lost (no pointer, "not available on this device"), named `name`.
fn lost(name: Option<&str>, size: u64) -> Attachment {
    Attachment {
        kind: AttachmentKind::File,
        mime: "application/octet-stream".into(),
        size,
        state: AttachmentState::Unavailable,
        net_ids: name.map(|n| vec![n.to_owned()]).unwrap_or_default(),
        ..Attachment::default()
    }
}

/// A file whose pointer the CDN says it no longer has.
fn expired(name: &str, size: u64) -> Attachment {
    Attachment {
        state: AttachmentState::Expired,
        remote: Some(Pointer::Expired.to_blob()),
        ..lost(Some(name), size)
    }
}

/// A video with a pointer (downloads on a click only).
fn video(seed: u64) -> Attachment {
    Attachment {
        kind: AttachmentKind::Video,
        mime: "video/mp4".into(),
        size: 5000,
        remote: Some(Pointer::Bytes { len: 5000, seed }.to_blob()),
        ..Attachment::default()
    }
}

/// The rest of a long text the network lost.
fn lost_long_text() -> Attachment {
    Attachment {
        mime: "text/x-signal-plain".into(),
        size: 3000,
        long_text: true,
        ..lost(None, 3000)
    }
}

/// Alice's message `local` with `attachments`.
fn message(local: &str, attachments: Vec<Attachment>) -> BackendEvent {
    let at: i64 = local.parse().expect("numeric id");
    let mut m = remote(&d_alice(), &uid(ALICE), local, at, "");
    m.body = Body::Media {
        attachments,
        caption: None,
    };
    BackendEvent::Message(m)
}

fn msg_target(local: &str) -> MsgTarget {
    target(Some(&d_alice()), Some(&uid(ALICE)), local)
}

/// The command of the open request of message `local` (`None`: no request).
fn request_cmd(h: &IngestHarness, local: &str) -> Option<CmdId> {
    h.connection()
        .query_row(
            "SELECT r.cmd FROM attachment_requests r JOIN messages m ON m.id = r.message_id
             WHERE m.net_local = ?1",
            [local],
            |r| r.get::<_, Option<i64>>(0),
        )
        .ok()
        .flatten()
        .map(CmdId)
}

/// The backend's news about the open request of message `local`: a report (`Sent`,
/// `NotSent`) names the request's command, as a backend's does; an answer names none.
fn restore(h: &IngestHarness, local: &str, update: RestoreUpdate) -> BackendEvent {
    let cmd = match update {
        RestoreUpdate::Sent | RestoreUpdate::NotSent => request_cmd(h, local),
        _ => None,
    };
    restore_cmd(h, local, update, cmd)
}

/// [`restore`] naming `cmd`, whichever request that is.
fn restore_cmd(
    h: &IngestHarness,
    local: &str,
    update: RestoreUpdate,
    cmd: Option<CmdId>,
) -> BackendEvent {
    BackendEvent::AttachmentRestore {
        target: msg_target(local),
        update,
        at: h.now(),
        cmd,
    }
}

fn answer(files: Vec<RestoreSlot>) -> RestoreUpdate {
    RestoreUpdate::Answer {
        attachments: files,
        long_text: None,
    }
}

fn new_pointer(seed: u64) -> BackendBlob {
    Pointer::Bytes { len: 100, seed }.to_blob()
}

/// A pointer the phone re-uploaded: it states the digest of its blob (`digest:s<seed>`) beside
/// `names`, so the core can have it checked at the download.
fn slot(seed: u64, names: &[&str], size: u64) -> RestoreSlot {
    let mut names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
    if !names.iter().any(|n| n.starts_with("digest:")) {
        names.push(format!("digest:s{seed}"));
    }
    RestoreSlot::File {
        remote: new_pointer(seed),
        net_ids: Redacted::new(names),
        size,
    }
}

/// A pointer with no digest (a file the phone restored from a backup): it can be checked
/// only against a plaintext hash the attachment has.
fn bare(seed: u64, names: &[&str], size: u64) -> RestoreSlot {
    RestoreSlot::File {
        remote: new_pointer(seed),
        net_ids: Redacted::new(names.iter().map(|n| (*n).to_owned()).collect()),
        size,
    }
}

/// `(id, state, remote)` of the attachments of message `local`, in order.
fn atts(h: &IngestHarness, local: &str) -> Vec<(AttachmentId, AttachmentState, Option<Vec<u8>>)> {
    let c = h.connection();
    let mut stmt = c
        .prepare(
            "SELECT a.id, a.state, a.remote FROM attachments a JOIN messages m
               ON m.id = a.message_id WHERE m.net_local = ?1 ORDER BY a.idx",
        )
        .expect("prepare");
    stmt.query_map([local], |r| {
        let state: String = r.get(1)?;
        Ok((
            AttachmentId(r.get(0)?),
            serde_json::from_str(&state).expect("state json"),
            r.get(2)?,
        ))
    })
    .expect("query")
    .collect::<Result<Vec<_>, _>>()
    .expect("rows")
}

fn ids(h: &IngestHarness, local: &str) -> Vec<AttachmentId> {
    atts(h, local).into_iter().map(|a| a.0).collect()
}

fn states(h: &IngestHarness, local: &str) -> Vec<AttachmentState> {
    atts(h, local).into_iter().map(|a| a.1).collect()
}

fn requests(h: &IngestHarness) -> i64 {
    h.connection()
        .query_row("SELECT count(*) FROM attachment_requests", [], |r| r.get(0))
        .expect("count")
}

fn failed(why: AskFailure) -> AttachmentState {
    AttachmentState::AskFailed { why }
}

fn downloaded(s: &Started) -> Vec<AttachmentId> {
    s.downloads.iter().map(|d| d.0).collect()
}

fn is_request(c: &DurableCommand, local: &str) -> bool {
    matches!(c, DurableCommand::RequestAttachments { conv, target }
        if *conv == d_alice() && target.local.as_ref() == local)
}

#[test]
fn a_click_on_a_lost_file_asks_once_per_message() {
    let (_d, mut h) = harness(true);
    h.apply(vec![message(
        "1000",
        vec![lost(Some("uuid:a"), 10), expired("uuid:b", 20)],
    )])
    .expect("message");
    let [a, b] = ids(&h, "1000")[..] else {
        panic!("two files")
    };
    let first = h.download_attachment(a).expect("click");
    assert_eq!(first.commands.len(), 1, "one request");
    assert!(is_request(&first.commands[0], "1000"), "{first:?}");
    assert!(first.downloads.is_empty(), "nothing to download yet");
    assert_eq!(
        states(&h, "1000"),
        vec![AttachmentState::Asking, AttachmentState::Expired]
    );
    // Another file of the message and a second click join the open request.
    let second = h.download_attachment(b).expect("click b");
    let third = h.download_attachment(a).expect("click a again");
    assert!(second.commands.is_empty() && third.commands.is_empty());
    assert!(second.downloads.is_empty() && third.downloads.is_empty());
    assert_eq!(states(&h, "1000"), vec![AttachmentState::Asking; 2]);
    assert_eq!(requests(&h), 1);
    // Not sent yet: the request has the longest time it may take to go out.
    assert_eq!(
        h.next_restore_deadline().expect("deadline"),
        Some(h.now() + UNSENT_BOUND)
    );
}

#[test]
fn without_the_capability_a_lost_file_is_refused() {
    let (_d, mut h) = harness(false);
    h.apply(vec![message("1000", vec![lost(Some("uuid:a"), 10)])])
        .expect("message");
    let a = ids(&h, "1000")[0];
    assert!(h.download_attachment(a).is_err(), "nothing can fetch it");
    assert_eq!(states(&h, "1000"), vec![AttachmentState::Unavailable]);
    assert_eq!(requests(&h), 0);
}

#[test]
fn the_wait_starts_when_sent_and_ends_as_no_answer() {
    let (_d, mut h) = harness(true);
    h.apply(vec![message("1000", vec![lost(Some("uuid:a"), 10)])])
        .expect("message");
    let a = ids(&h, "1000")[0];
    h.download_attachment(a).expect("click");
    let sent = h.now();
    h.apply(vec![restore(&h, "1000", RestoreUpdate::Sent)])
        .expect("sent");
    assert_eq!(
        h.next_restore_deadline().expect("next"),
        Some(sent + 30_000)
    );
    // A second report does not move it.
    h.set_now(sent + 10_000);
    h.apply(vec![restore(&h, "1000", RestoreUpdate::Sent)])
        .expect("sent again");
    assert_eq!(
        h.next_restore_deadline().expect("next"),
        Some(sent + 30_000)
    );
    h.set_now(sent + 29_999);
    assert_eq!(h.settle_restores().expect("sweep"), 0);
    assert_eq!(states(&h, "1000"), vec![AttachmentState::Asking]);
    h.set_now(sent + 30_000);
    assert_eq!(h.settle_restores().expect("sweep"), 1);
    assert_eq!(states(&h, "1000"), vec![failed(AskFailure::NoAnswer)]);
    assert_eq!(requests(&h), 0);
    assert_eq!(h.next_restore_deadline().expect("next"), None);
    // "Did not answer" can be asked again.
    let again = h.download_attachment(a).expect("click again");
    assert_eq!(again.commands.len(), 1);
    assert_eq!(states(&h, "1000"), vec![AttachmentState::Asking]);
}

#[test]
fn a_request_the_backend_gave_up_ends_as_no_answer() {
    let (_d, mut h) = harness(true);
    h.apply(vec![message("1000", vec![lost(Some("uuid:a"), 10)])])
        .expect("message");
    h.download_attachment(ids(&h, "1000")[0]).expect("click");
    h.apply(vec![restore(&h, "1000", RestoreUpdate::NotSent)])
        .expect("not sent");
    assert_eq!(states(&h, "1000"), vec![failed(AskFailure::NoAnswer)]);
    assert_eq!(requests(&h), 0);
}

#[test]
fn a_wait_whose_deadline_passed_while_closed_ends_at_the_first_look() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("core.db");
    let sent;
    {
        let mut h = open(&path);
        let mut flags = BTreeMap::new();
        flags.insert(AccountCaps::FLAG_ATTACHMENT_RESTORE.to_owned(), true);
        h.apply(vec![
            BackendEvent::AccountCaps(AccountCaps {
                flags,
                ..Default::default()
            }),
            message("1000", vec![lost(Some("uuid:a"), 10)]),
        ])
        .expect("setup");
        h.download_attachment(ids(&h, "1000")[0]).expect("click");
        sent = h.now();
        h.apply(vec![restore(&h, "1000", RestoreUpdate::Sent)])
            .expect("sent");
    }
    let mut h = open(&path);
    assert_eq!(states(&h, "1000"), vec![AttachmentState::Asking]);
    assert_eq!(
        h.next_restore_deadline().expect("next"),
        Some(sent + 30_000)
    );
    h.set_now(sent + 3_600_000);
    assert_eq!(h.settle_restores().expect("sweep"), 1);
    assert_eq!(states(&h, "1000"), vec![failed(AskFailure::NoAnswer)]);
}

#[test]
fn pending_waits_two_minutes_more() {
    let (_d, mut h) = harness(true);
    h.apply(vec![message("1000", vec![lost(Some("uuid:a"), 10)])])
        .expect("message");
    let a = ids(&h, "1000")[0];
    h.download_attachment(a).expect("click");
    h.apply(vec![restore(&h, "1000", RestoreUpdate::Sent)])
        .expect("sent");
    h.set_now(h.now() + 20_000);
    let pending_at = h.now();
    h.apply(vec![restore(
        &h,
        "1000",
        answer(vec![RestoreSlot::Pending]),
    )])
    .expect("pending");
    assert_eq!(
        h.next_restore_deadline().expect("next"),
        Some(pending_at + 120_000)
    );
    h.set_now(pending_at + 60_000);
    assert_eq!(h.settle_restores().expect("sweep"), 0);
    assert_eq!(states(&h, "1000"), vec![AttachmentState::Asking]);
    // Then the file: it downloads and the wait ends.
    let done = h
        .apply_starting(vec![restore(
            &h,
            "1000",
            answer(vec![slot(1, &["uuid:a"], 10)]),
        )])
        .expect("answer");
    assert_eq!(downloaded(&done), vec![a]);
    assert_eq!(requests(&h), 0);
    assert!(matches!(
        states(&h, "1000")[0],
        AttachmentState::Downloading { .. }
    ));
}

#[test]
fn names_match_before_position_and_only_the_asked_file_downloads() {
    let (_d, mut h) = harness(true);
    h.apply(vec![message(
        "1000",
        vec![
            lost(Some("uuid:a"), 10),
            lost(Some("uuid:b"), 20),
            expired("uuid:c", 30),
        ],
    )])
    .expect("message");
    let [_, b, _] = ids(&h, "1000")[..] else {
        panic!("three files")
    };
    h.download_attachment(b).expect("click b");
    h.apply(vec![restore(&h, "1000", RestoreUpdate::Sent)])
        .expect("sent");
    // Same count, other order: the names decide.
    let done = h
        .apply_starting(vec![restore(
            &h,
            "1000",
            answer(vec![
                slot(3, &["uuid:c"], 30),
                slot(1, &["uuid:a"], 10),
                slot(2, &["uuid:b", "digest:bb"], 20),
            ]),
        )])
        .expect("answer");
    assert_eq!(downloaded(&done), vec![b], "only the asked file downloads");
    assert!(
        done.downloads[0].2.contains(&"digest:bb".to_owned()),
        "the new name is merged"
    );
    assert!(done.commands.is_empty());
    let rows = atts(&h, "1000");
    assert_eq!(rows[0].1, AttachmentState::Remote);
    assert!(matches!(rows[1].1, AttachmentState::Downloading { .. }));
    assert_eq!(rows[2].1, AttachmentState::Remote);
    let blob = |seed| {
        // As `core.db` stores a pointer: its version (little-endian u16), then its bytes.
        let b = new_pointer(seed);
        let mut out = b.v.to_le_bytes().to_vec();
        out.extend_from_slice(&b.bytes);
        out
    };
    assert_eq!(rows[0].2, Some(blob(1)), "a got a's pointer");
    assert_eq!(rows[1].2, Some(blob(2)), "b got b's pointer");
    assert_eq!(rows[2].2, Some(blob(3)), "c got c's pointer");
    assert_eq!(requests(&h), 0);
}

#[test]
fn position_counts_only_where_the_counts_and_sizes_agree() {
    let (_d, mut h) = harness(true);
    h.apply(vec![
        message("1000", vec![lost(None, 10), lost(None, 20)]),
        message("2000", vec![lost(None, 10), lost(None, 20)]),
        message("3000", vec![lost(None, 10), lost(None, 20)]),
    ])
    .expect("messages");
    for local in ["1000", "2000", "3000"] {
        let first = ids(&h, local)[0];
        h.download_attachment(first).expect("click");
    }
    // One slot for two files: something was dropped, no position is trusted.
    let short = h
        .apply_starting(vec![restore(
            &h,
            "1000",
            answer(vec![slot(1, &["uuid:x"], 10)]),
        )])
        .expect("short");
    assert!(short.downloads.is_empty());
    assert_eq!(
        states(&h, "1000"),
        vec![failed(AskFailure::NoAnswer), AttachmentState::Unavailable],
        "the phone did not say it lacks the file: it may be asked again"
    );
    // Equal counts but another size: not that file.
    let other = h
        .apply_starting(vec![restore(
            &h,
            "2000",
            answer(vec![slot(1, &["uuid:x"], 99), RestoreSlot::Skip]),
        )])
        .expect("other size");
    assert!(other.downloads.is_empty());
    assert_eq!(states(&h, "2000")[0], failed(AskFailure::NoAnswer));
    // Equal counts and sizes: by position.
    let first = ids(&h, "3000")[0];
    let good = h
        .apply_starting(vec![restore(
            &h,
            "3000",
            answer(vec![slot(1, &["uuid:x"], 10), slot(2, &["uuid:y"], 20)]),
        )])
        .expect("good");
    assert_eq!(downloaded(&good), vec![first]);
    assert_eq!(states(&h, "3000")[1], AttachmentState::Remote);
}

#[test]
fn gone_and_not_found_end_the_wait_with_their_reasons() {
    let (_d, mut h) = harness(true);
    h.apply(vec![
        message(
            "1000",
            vec![lost(Some("uuid:a"), 10), lost(Some("uuid:b"), 20)],
        ),
        message("2000", vec![lost(Some("uuid:c"), 10)]),
        message("3000", vec![lost(Some("uuid:d"), 10)]),
    ])
    .expect("messages");
    let one = ids(&h, "1000");
    h.download_attachment(one[0]).expect("click a");
    h.download_attachment(one[1]).expect("click b");
    h.download_attachment(ids(&h, "2000")[0]).expect("click c");
    // a is gone on the phone, b comes.
    let done = h
        .apply_starting(vec![restore(
            &h,
            "1000",
            answer(vec![RestoreSlot::Gone, slot(2, &["uuid:b"], 20)]),
        )])
        .expect("answer");
    assert_eq!(downloaded(&done), vec![one[1]]);
    let s = states(&h, "1000");
    assert_eq!(s[0], failed(AskFailure::NoFile));
    assert!(matches!(s[1], AttachmentState::Downloading { .. }));
    h.apply(vec![restore(&h, "2000", RestoreUpdate::MessageNotFound)])
        .expect("not found");
    assert_eq!(states(&h, "2000"), vec![failed(AskFailure::NoMessage)]);
    // "Not found" for a message this device did not ask about changes nothing.
    h.apply(vec![restore(&h, "3000", RestoreUpdate::MessageNotFound)])
        .expect("not ours");
    assert_eq!(states(&h, "3000"), vec![AttachmentState::Unavailable]);
    assert_eq!(requests(&h), 0);
}

#[test]
fn answers_to_another_devices_request_give_pointers_and_download_nothing() {
    let (_d, mut h) = harness(true);
    h.apply(vec![message(
        "1000",
        vec![lost(Some("uuid:a"), 10), expired("uuid:b", 20), video(4)],
    )])
    .expect("message");
    let before_video = atts(&h, "1000")[2].2.clone();
    let done = h
        .apply_starting(vec![restore(
            &h,
            "1000",
            answer(vec![
                slot(1, &["uuid:a"], 10),
                slot(2, &["uuid:b"], 20),
                slot(3, &[], 5000),
            ]),
        )])
        .expect("answer");
    assert!(done.downloads.is_empty() && done.commands.is_empty());
    let rows = atts(&h, "1000");
    assert_eq!(rows[0].1, AttachmentState::Remote);
    assert_eq!(rows[1].1, AttachmentState::Remote);
    assert_eq!(rows[2].1, AttachmentState::Remote, "the video is untouched");
    assert_eq!(rows[2].2, before_video, "a file with its pointer keeps it");
    // The click now downloads.
    let click = h.download_attachment(rows[0].0).expect("click");
    assert_eq!(downloaded(&click), vec![rows[0].0]);
    assert!(click.commands.is_empty());
}

#[test]
fn an_answer_for_a_message_that_is_not_here_is_ignored() {
    let (_d, mut h) = harness(true);
    let done = h
        .apply_starting(vec![restore(
            &h,
            "9999",
            answer(vec![slot(1, &["uuid:a"], 10)]),
        )])
        .expect("answer");
    assert_eq!(done, Started::default());
}

#[test]
fn a_slot_above_the_receive_limit_is_ignored() {
    let (_d, mut h) = harness(true);
    h.apply(vec![message("1000", vec![lost(Some("uuid:a"), 10)])])
        .expect("message");
    h.download_attachment(ids(&h, "1000")[0]).expect("click");
    let done = h
        .apply_starting(vec![restore(
            &h,
            "1000",
            answer(vec![slot(1, &["uuid:a"], LIMIT + 1)]),
        )])
        .expect("answer");
    assert!(done.downloads.is_empty());
    let rows = atts(&h, "1000");
    assert_eq!(rows[0].1, failed(AskFailure::NoAnswer));
    assert_eq!(rows[0].2, None, "no pointer taken");
}

#[test]
fn the_long_text_comes_only_with_a_request_of_this_device() {
    let (_d, mut h) = harness(true);
    h.apply(vec![
        message("1000", vec![lost(Some("uuid:a"), 10), lost_long_text()]),
        message("2000", vec![lost(Some("uuid:b"), 10), lost_long_text()]),
    ])
    .expect("messages");
    let one = ids(&h, "1000");
    // The long text itself is never asked on its own.
    assert!(h.download_attachment(one[1]).is_err());
    assert_eq!(requests(&h), 0);
    h.download_attachment(one[0]).expect("click");
    let text = || RestoreSlot::File {
        remote: new_pointer(9),
        net_ids: Redacted::new(vec!["digest:t9".to_owned()]),
        size: 3000,
    };
    let done = h
        .apply_starting(vec![restore(
            &h,
            "1000",
            RestoreUpdate::Answer {
                attachments: vec![slot(1, &["uuid:a"], 10)],
                long_text: Some(text()),
            },
        )])
        .expect("ours");
    assert_eq!(downloaded(&done), vec![one[0], one[1]]);
    // The same answer to another device's request: the file gets a pointer, the long text
    // nothing.
    let others = h
        .apply_starting(vec![restore(
            &h,
            "2000",
            RestoreUpdate::Answer {
                attachments: vec![slot(1, &["uuid:b"], 10)],
                long_text: Some(text()),
            },
        )])
        .expect("others");
    assert!(others.downloads.is_empty());
    assert_eq!(
        states(&h, "2000"),
        vec![AttachmentState::Remote, AttachmentState::Unavailable]
    );
}

fn expired_done(attachment: AttachmentId, request: kourier_model::DownloadId) -> BackendEvent {
    BackendEvent::AttachmentDone {
        attachment,
        request: Some(request),
        result: Err(BackendError::new(ErrorKind::Expired, "gone")),
    }
}

#[test]
fn a_404_of_a_user_download_asks_at_once_and_an_automatic_one_does_not() {
    let (_d, mut h) = harness(true);
    let small = Attachment {
        kind: AttachmentKind::File,
        mime: "application/octet-stream".into(),
        size: 100,
        remote: Some(Pointer::Bytes { len: 100, seed: 5 }.to_blob()),
        ..Attachment::default()
    };
    // The small file downloads on its own (the policy), the video on a click.
    let auto = h
        .apply_starting(vec![message("1000", vec![small, video(4)])])
        .expect("message");
    let [f, v] = ids(&h, "1000")[..] else {
        panic!("two files")
    };
    assert_eq!(downloaded(&auto), vec![f]);
    let none = h
        .apply_starting(vec![expired_done(f, auto.downloads[0].1)])
        .expect("auto 404");
    assert!(
        none.commands.is_empty(),
        "an automatic download asks nothing"
    );
    assert_eq!(states(&h, "1000")[0], AttachmentState::Expired);
    let click = h.download_attachment(v).expect("click");
    assert_eq!(downloaded(&click), vec![v]);
    let asked = h
        .apply_starting(vec![expired_done(v, click.downloads[0].1)])
        .expect("user 404");
    assert_eq!(asked.commands.len(), 1, "the user's download asks at once");
    assert!(is_request(&asked.commands[0], "1000"));
    assert_eq!(states(&h, "1000")[1], AttachmentState::Asking);
    // The answer's pointer is gone too: no second request by itself.
    let restored = h
        .apply_starting(vec![restore(
            &h,
            "1000",
            answer(vec![RestoreSlot::Skip, slot(7, &[], 5000)]),
        )])
        .expect("answer");
    assert_eq!(downloaded(&restored), vec![v]);
    let again = h
        .apply_starting(vec![expired_done(v, restored.downloads[0].1)])
        .expect("restored 404");
    assert!(
        again.commands.is_empty(),
        "a restored pointer does not ask again"
    );
    assert_eq!(states(&h, "1000")[1], AttachmentState::Expired);
}

#[test]
fn the_long_text_of_an_edited_message_is_not_taken_from_the_first_version() {
    let (_d, mut h) = harness(true);
    h.apply(vec![message(
        "1000",
        vec![lost(Some("uuid:a"), 10), lost_long_text()],
    )])
    .expect("message");
    // As an edit leaves the row (the edit's own rules are `revisions.rs`'s): one edit.
    h.connection()
        .execute(
            "UPDATE messages SET revision = 1 WHERE net_local = '1000'",
            [],
        )
        .expect("edited");
    let one = ids(&h, "1000");
    h.download_attachment(one[0]).expect("click");
    let done = h
        .apply_starting(vec![restore(
            &h,
            "1000",
            RestoreUpdate::Answer {
                attachments: vec![slot(1, &["uuid:a"], 10)],
                long_text: Some(RestoreSlot::File {
                    remote: new_pointer(9),
                    net_ids: Redacted::new(vec!["digest:t9".to_owned()]),
                    size: 3000,
                }),
            },
        )])
        .expect("answer");
    assert_eq!(
        downloaded(&done),
        vec![one[0]],
        "the old text is not fetched"
    );
    assert_eq!(states(&h, "1000")[1], AttachmentState::Unavailable);
}

/// A socket that never comes up (or a backend that lost the command): nothing ever reports the
/// request sent or unsent. The core's own deadline, set at the click, ends the wait, and the
/// file can be asked for again.
#[test]
fn a_request_that_never_went_out_ends_at_its_bound_and_can_be_asked_again() {
    let (_d, mut h) = harness(true);
    h.apply(vec![message("1000", vec![lost(Some("uuid:a"), 10)])])
        .expect("message");
    let a = ids(&h, "1000")[0];
    let click = h.now();
    h.download_attachment(a).expect("click");
    assert_eq!(
        h.next_restore_deadline().expect("next"),
        Some(click + UNSENT_BOUND)
    );
    h.set_now(click + UNSENT_BOUND - 1);
    assert_eq!(h.settle_restores().expect("sweep"), 0);
    assert_eq!(states(&h, "1000"), vec![AttachmentState::Asking]);
    h.set_now(click + UNSENT_BOUND);
    assert_eq!(h.settle_restores().expect("sweep"), 1);
    assert_eq!(states(&h, "1000"), vec![failed(AskFailure::NoAnswer)]);
    assert_eq!(requests(&h), 0);
    // A report that comes after the end changes nothing.
    h.apply(vec![restore(&h, "1000", RestoreUpdate::Sent)])
        .expect("late sent");
    assert_eq!(h.next_restore_deadline().expect("next"), None);
    assert_eq!(
        h.download_attachment(a).expect("again").commands.len(),
        1,
        "asked again"
    );
}

/// A logout (or an unlink) while a request is queued: no backend will report it unsent.
#[test]
fn a_logged_out_account_ends_its_open_waits() {
    let (_d, mut h) = harness(true);
    h.apply(vec![
        message("1000", vec![lost(Some("uuid:a"), 10)]),
        message("2000", vec![lost(Some("uuid:b"), 10)]),
        message("3000", vec![lost(Some("uuid:c"), 10)]),
    ])
    .expect("messages");
    h.download_attachment(ids(&h, "1000")[0]).expect("click 1");
    h.apply(vec![restore(&h, "1000", RestoreUpdate::Sent)])
        .expect("sent");
    h.download_attachment(ids(&h, "2000")[0]).expect("click 2");
    assert_eq!(requests(&h), 2);
    assert_eq!(h.log_out().expect("logout"), 2);
    assert_eq!(states(&h, "1000"), vec![failed(AskFailure::NoAnswer)]);
    assert_eq!(states(&h, "2000"), vec![failed(AskFailure::NoAnswer)]);
    assert_eq!(states(&h, "3000"), vec![AttachmentState::Unavailable]);
    assert_eq!(requests(&h), 0);
}

/// The requests' commands still waiting for a backend (`pending_commands`).
fn pending_requests(h: &IngestHarness) -> i64 {
    h.connection()
        .query_row(
            "SELECT count(*) FROM pending_commands WHERE command LIKE '%RequestAttachments%'",
            [],
            |r| r.get(0),
        )
        .expect("count")
}

/// A request nobody waits for any more is not sent later: when the core ends a wait (its
/// deadline, a final answer) it deletes the request's command too, or a command that never
/// reached a backend (no actor, a closed socket) would be sent at a later start, and the
/// phone asked for a wait that is over.
#[test]
fn ending_a_wait_deletes_its_command() {
    let (_d, mut h) = harness(true);
    h.apply(vec![
        message("1000", vec![lost(Some("uuid:a"), 10)]),
        message("2000", vec![lost(Some("uuid:b"), 10)]),
    ])
    .expect("messages");
    h.download_attachment(ids(&h, "1000")[0]).expect("click 1");
    h.download_attachment(ids(&h, "2000")[0]).expect("click 2");
    assert_eq!(pending_requests(&h), 2);
    // A final answer ends the wait of message 2000 ...
    h.apply(vec![restore(&h, "2000", answer(vec![RestoreSlot::Gone]))])
        .expect("answer");
    assert_eq!(requests(&h), 1);
    assert_eq!(
        pending_requests(&h),
        1,
        "the answered request's command went"
    );
    // ... and the deadline the wait of message 1000.
    h.set_now(h.now() + UNSENT_BOUND);
    assert_eq!(h.settle_restores().expect("sweep"), 1);
    assert_eq!(requests(&h), 0);
    assert_eq!(pending_requests(&h), 0, "the swept request's command went");
    // A command a backend confirmed is gone from the table already; ending the wait finds
    // nothing to delete and changes nothing else.
    h.download_attachment(ids(&h, "1000")[0]).expect("again");
    let cmd = request_cmd(&h, "1000").expect("the new request's command");
    h.apply(vec![BackendEvent::CommandPersisted { cmd }])
        .expect("confirmed");
    assert_eq!(pending_requests(&h), 0);
    h.apply(vec![restore(&h, "1000", RestoreUpdate::NotSent)])
        .expect("not sent");
    assert_eq!(states(&h, "1000"), vec![failed(AskFailure::NoAnswer)]);
}

/// The backend's reports name the command of the request they are about: `Sent` and
/// `NotSent` apply only to the request that command made, never to a newer wait of the same
/// message (the first request ended by its deadline, the user clicked again, and then the
/// first one's job reported: expired, or sent after all). An answer names no command on any
/// network: it applies to the message's open request.
#[test]
fn a_report_about_an_earlier_request_does_not_touch_the_newer_wait() {
    let (_d, mut h) = harness(true);
    h.apply(vec![message("1000", vec![lost(Some("uuid:a"), 10)])])
        .expect("message");
    let a = ids(&h, "1000")[0];
    h.download_attachment(a).expect("click");
    let first = request_cmd(&h, "1000").expect("first command");
    h.set_now(h.now() + UNSENT_BOUND);
    assert_eq!(h.settle_restores().expect("sweep"), 1);
    h.download_attachment(a).expect("again");
    let second = request_cmd(&h, "1000").expect("second command");
    assert_ne!(first, second);
    let deadline = h.next_restore_deadline().expect("next");
    // The first request's job reports it sent: the newer wait does not start.
    h.apply(vec![restore_cmd(
        &h,
        "1000",
        RestoreUpdate::Sent,
        Some(first),
    )])
    .expect("stale sent");
    assert_eq!(h.next_restore_deadline().expect("next"), deadline);
    // ... and unsent: the newer wait is not ended.
    h.apply(vec![restore_cmd(
        &h,
        "1000",
        RestoreUpdate::NotSent,
        Some(first),
    )])
    .expect("stale not sent");
    assert_eq!(states(&h, "1000"), vec![AttachmentState::Asking]);
    assert_eq!(requests(&h), 1);
    // A report that names nothing is no report about any request.
    h.apply(vec![restore_cmd(&h, "1000", RestoreUpdate::NotSent, None)])
        .expect("anonymous not sent");
    assert_eq!(states(&h, "1000"), vec![AttachmentState::Asking]);
    // The newer request's own reports apply.
    h.apply(vec![restore_cmd(
        &h,
        "1000",
        RestoreUpdate::Sent,
        Some(second),
    )])
    .expect("sent");
    assert_ne!(h.next_restore_deadline().expect("next"), deadline);
    // An answer names no command: it applies to the open request.
    let done = h
        .apply_starting(vec![restore(
            &h,
            "1000",
            answer(vec![slot(1, &["uuid:a"], 10)]),
        )])
        .expect("answer");
    assert_eq!(downloaded(&done), vec![a]);
    assert_eq!(requests(&h), 0);
}

/// An attachment that says it is being asked for with no request of its message cannot end by
/// itself (a backend delivered it so, or an earlier build left it): the first look after a
/// start ends it, and leaves the asks that have their request.
#[test]
fn an_ask_without_a_request_ends_at_a_start() {
    let (_d, mut h) = harness(true);
    let stuck = Attachment {
        state: AttachmentState::Asking,
        ..lost(Some("uuid:b"), 10)
    };
    h.apply(vec![
        message("1000", vec![lost(Some("uuid:a"), 10)]),
        message("2000", vec![stuck]),
    ])
    .expect("messages");
    h.download_attachment(ids(&h, "1000")[0]).expect("click");
    assert_eq!(states(&h, "2000"), vec![AttachmentState::Asking]);
    assert_eq!(h.end_orphan_asks().expect("start"), 1);
    assert_eq!(states(&h, "1000"), vec![AttachmentState::Asking]);
    assert_eq!(states(&h, "2000"), vec![failed(AskFailure::NoAnswer)]);
    assert_eq!(requests(&h), 1);
    assert_eq!(h.end_orphan_asks().expect("again"), 0);
}

/// A due request whose message is gone (a table rebuilt with foreign keys off could leave
/// one) is deleted by the sweep: kept, it would be due for ever and the task would sweep it
/// again at once.
#[test]
fn a_due_request_whose_message_is_gone_is_deleted() {
    let (_d, mut h) = harness(true);
    h.apply(vec![message("1000", vec![lost(Some("uuid:a"), 10)])])
        .expect("message");
    h.download_attachment(ids(&h, "1000")[0]).expect("click");
    h.connection()
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             DELETE FROM attachments WHERE message_id IN
               (SELECT id FROM messages WHERE net_local = '1000');
             DELETE FROM messages WHERE net_local = '1000';
             PRAGMA foreign_keys = ON;",
        )
        .expect("orphan");
    assert_eq!(requests(&h), 1, "the request outlived its message");
    h.set_now(h.now() + UNSENT_BOUND);
    h.settle_restores().expect("sweep");
    assert_eq!(requests(&h), 0);
    assert_eq!(h.next_restore_deadline().expect("next"), None);
}

/// A final answer ends as "not on your phone" only what a "gone" slot was placed on, and a
/// "gone" slot is placed by position only where a NAME placed a slot too; an asked file whose
/// slot is one this version cannot use, or that no slot named, may be asked again.
#[test]
fn a_final_answer_ends_as_not_on_the_phone_only_what_a_gone_slot_was_placed_on() {
    let (_d, mut h) = harness(true);
    h.apply(vec![message(
        "1000",
        vec![
            lost(Some("uuid:a"), 10),
            lost(Some("uuid:b"), 20),
            lost(Some("uuid:c"), 30),
        ],
    )])
    .expect("message");
    for id in ids(&h, "1000") {
        h.download_attachment(id).expect("click");
    }
    // c's slot is placed by its name on its own index: the order is proven, so a's "gone" is
    // a's; b's slot is one this version cannot use.
    h.apply_starting(vec![restore(
        &h,
        "1000",
        answer(vec![
            RestoreSlot::Gone,
            RestoreSlot::Skip,
            slot(3, &["uuid:c"], 30),
        ]),
    )])
    .expect("answer");
    let now = states(&h, "1000");
    assert_eq!(now[0], failed(AskFailure::NoFile));
    assert_eq!(now[1], failed(AskFailure::NoAnswer));
    assert!(matches!(now[2], AttachmentState::Downloading { .. }));
    assert_eq!(requests(&h), 0);
}

/// A slot that states only a status ("gone", "still uploading") has no name to be placed by: when
/// no name placed any slot of the answer, nothing proves the phone's order is ours, and the
/// slots are not placed by their index. [GONE, PENDING] with no names: the first file is not
/// "not on your phone" (final, no Download), it stays asked for while the phone says it is
/// still uploading and then ends as "did not answer", clickable again; a final [GONE, GONE]
/// with no names ends both the same way.
#[test]
fn a_gone_slot_no_name_proves_is_not_believed() {
    let (_d, mut h) = harness(true);
    h.apply(vec![message(
        "1000",
        vec![lost(Some("uuid:a"), 10), lost(Some("uuid:b"), 20)],
    )])
    .expect("message");
    for id in ids(&h, "1000") {
        h.download_attachment(id).expect("click");
    }
    h.apply(vec![restore(&h, "1000", RestoreUpdate::Sent)])
        .expect("sent");
    let pending = restore(
        &h,
        "1000",
        answer(vec![RestoreSlot::Gone, RestoreSlot::Pending]),
    );
    h.apply(vec![pending]).expect("answer");
    assert_eq!(
        states(&h, "1000"),
        vec![AttachmentState::Asking, AttachmentState::Asking],
        "no 'gone' is placed by index alone"
    );
    assert_eq!(
        requests(&h),
        1,
        "the phone is still uploading: the wait stays"
    );
    h.set_now(h.now() + 120_000);
    assert_eq!(h.settle_restores().expect("sweep"), 2, "both files end");
    assert_eq!(
        states(&h, "1000"),
        vec![failed(AskFailure::NoAnswer), failed(AskFailure::NoAnswer)],
        "ends as 'did not answer', never as the final 'not on your phone'"
    );
    assert_eq!(requests(&h), 0);
    // The same for a final answer that says "gone" twice without a name.
    let (a, b) = (ids(&h, "1000")[0], ids(&h, "1000")[1]);
    h.download_attachment(a).expect("again a");
    h.download_attachment(b).expect("again b");
    h.apply(vec![restore(
        &h,
        "1000",
        answer(vec![RestoreSlot::Gone, RestoreSlot::Gone]),
    )])
    .expect("final");
    assert_eq!(
        states(&h, "1000"),
        vec![failed(AskFailure::NoAnswer), failed(AskFailure::NoAnswer)]
    );
    assert_eq!(requests(&h), 0);
}

/// The phone lists its files in an order of its own. A slot placed by name that is not on its
/// own index proves the orders differ, so no PENDING or "gone" slot is placed by position:
/// B's and A's "gone" are not taken for A's and B's.
#[test]
fn the_phones_other_order_is_not_taken_for_positions() {
    let (_d, mut h) = harness(true);
    h.apply(vec![message(
        "1000",
        vec![
            lost(Some("uuid:a"), 10),
            lost(Some("uuid:b"), 20),
            lost(Some("uuid:c"), 30),
        ],
    )])
    .expect("message");
    let [a, b, _] = ids(&h, "1000")[..] else {
        panic!("three files")
    };
    h.download_attachment(a).expect("click a");
    h.download_attachment(b).expect("click b");
    // The phone's order: b, c, a. Its entry for c is a pointer, the others are "gone".
    let done = h
        .apply_starting(vec![restore(
            &h,
            "1000",
            answer(vec![
                RestoreSlot::Gone,
                slot(3, &["uuid:c"], 30),
                RestoreSlot::Gone,
            ]),
        )])
        .expect("answer");
    assert!(done.downloads.is_empty());
    assert_eq!(
        states(&h, "1000"),
        vec![
            failed(AskFailure::NoAnswer),
            failed(AskFailure::NoAnswer),
            AttachmentState::Remote
        ],
        "neither is told the phone lacks it"
    );
}

/// The long text asked with the files: a pointer for the files with the text still PENDING
/// keeps the wait open, and the text that comes next is fetched.
#[test]
fn a_pending_long_text_keeps_the_wait_open_until_it_comes() {
    let (_d, mut h) = harness(true);
    h.apply(vec![message(
        "1000",
        vec![lost(Some("uuid:a"), 10), lost_long_text()],
    )])
    .expect("message");
    let one = ids(&h, "1000");
    h.download_attachment(one[0]).expect("click");
    h.apply(vec![restore(&h, "1000", RestoreUpdate::Sent)])
        .expect("sent");
    let text = |long_text| RestoreUpdate::Answer {
        attachments: vec![slot(1, &["uuid:a"], 10)],
        long_text: Some(long_text),
    };
    let first = h
        .apply_starting(vec![restore(&h, "1000", text(RestoreSlot::Pending))])
        .expect("first");
    assert_eq!(downloaded(&first), vec![one[0]]);
    assert_eq!(requests(&h), 1, "the text is still on its way");
    assert_eq!(
        h.next_restore_deadline().expect("next"),
        Some(h.now() + 120_000)
    );
    assert_eq!(states(&h, "1000")[1], AttachmentState::Unavailable);
    let second = h
        .apply_starting(vec![restore(
            &h,
            "1000",
            text(RestoreSlot::File {
                remote: new_pointer(9),
                net_ids: Redacted::new(vec!["digest:t9".to_owned()]),
                size: 3000,
            }),
        )])
        .expect("second");
    assert_eq!(downloaded(&second), vec![one[1]], "the text is fetched");
    assert_eq!(requests(&h), 0);
}

/// A pointer without a digest is taken only for an attachment that has the hash of its
/// plaintext to check it against, and never replaces the working pointer of a file that is
/// there to download.
#[test]
fn a_pointer_that_cannot_be_checked_is_not_taken() {
    let (_d, mut h) = harness(true);
    let hashed = Attachment {
        net_ids: vec![
            "uuid:p".to_owned(),
            format!("plaintext:{}", "aa".repeat(32)),
        ],
        ..lost(None, 10)
    };
    h.apply(vec![
        message("1000", vec![lost(Some("uuid:a"), 10)]),
        message("2000", vec![hashed]),
        message("3000", vec![lost(Some("uuid:c"), 10), video(4)]),
    ])
    .expect("messages");
    for local in ["1000", "2000", "3000"] {
        h.download_attachment(ids(&h, local)[0]).expect("click");
    }
    // Nothing to check it against: not taken, the file may be asked for again.
    let none = h
        .apply_starting(vec![restore(
            &h,
            "1000",
            answer(vec![bare(1, &["uuid:a"], 10)]),
        )])
        .expect("1000");
    assert!(none.downloads.is_empty());
    let rows = atts(&h, "1000");
    assert_eq!(
        (rows[0].1, rows[0].2.clone()),
        (failed(AskFailure::NoAnswer), None)
    );
    // The plaintext hash is there: taken.
    let ok = h
        .apply_starting(vec![restore(
            &h,
            "2000",
            answer(vec![bare(2, &["uuid:p"], 10)]),
        )])
        .expect("2000");
    assert_eq!(downloaded(&ok), vec![ids(&h, "2000")[0]]);
    // The video has its pointer; the phone's unverifiable one does not replace it.
    let before = atts(&h, "3000")[1].2.clone();
    let done = h
        .apply_starting(vec![restore(
            &h,
            "3000",
            answer(vec![slot(3, &["uuid:c"], 10), bare(9, &[], 5000)]),
        )])
        .expect("3000");
    assert_eq!(downloaded(&done), vec![ids(&h, "3000")[0]]);
    assert_eq!(atts(&h, "3000")[1].2, before, "the working pointer stays");
}

/// A size the import did not know (0) proves nothing about the file there: a slot placed by
/// position needs the very size.
#[test]
fn an_unknown_size_does_not_place_a_slot_by_position() {
    let (_d, mut h) = harness(true);
    h.apply(vec![message("1000", vec![lost(None, 0)])])
        .expect("message");
    h.download_attachment(ids(&h, "1000")[0]).expect("click");
    let done = h
        .apply_starting(vec![restore(
            &h,
            "1000",
            answer(vec![slot(1, &["uuid:x"], 4242)]),
        )])
        .expect("answer");
    assert!(done.downloads.is_empty());
    assert_eq!(states(&h, "1000"), vec![failed(AskFailure::NoAnswer)]);
}

/// A chat or an author known by phone number alone is read-only and its answer could not be
/// placed: the other device is not asked, and the click is refused.
#[test]
fn a_chat_or_an_author_known_by_number_alone_is_not_asked() {
    let (_d, mut h) = harness(true);
    let number = uid("e164:+15550100");
    let file = || Body::Media {
        attachments: vec![lost(Some("uuid:a"), 10)],
        caption: None,
    };
    // A chat with a number, a message by Alice in it; Alice's chat, a message by a number.
    let mut in_number_chat = remote(
        &ConvKey::Direct(number.clone()),
        &uid(ALICE),
        "1000",
        1000,
        "",
    );
    in_number_chat.body = file();
    let mut by_number = remote(&d_alice(), &number, "2000", 2000, "");
    by_number.body = file();
    h.apply(vec![
        BackendEvent::Message(in_number_chat),
        BackendEvent::Message(by_number),
    ])
    .expect("messages");
    for local in ["1000", "2000"] {
        let id = ids(&h, local)[0];
        assert!(h.download_attachment(id).is_err(), "{local}");
        assert_eq!(states(&h, local), vec![AttachmentState::Unavailable]);
    }
    assert_eq!(requests(&h), 0);
}

/// A wait whose deadline passed while Kourier was closed is not judged before the answer that
/// came in time, which the backend replays from its outbox at the start, was applied: the
/// answer finds its request and the file downloads, instead of "did not answer" first and a
/// pointer nobody downloads.
#[test]
fn a_wait_that_ran_out_while_closed_waits_for_the_replayed_answer() {
    let (_d, mut h) = harness(true);
    h.apply(vec![message("1000", vec![lost(Some("uuid:a"), 10)])])
        .expect("message");
    let a = ids(&h, "1000")[0];
    h.download_attachment(a).expect("click");
    h.apply(vec![restore(&h, "1000", RestoreUpdate::Sent)])
        .expect("sent");
    // An hour later, the actor has just started: the due wait is left alone ...
    h.set_now(h.now() + 3_600_000);
    assert_eq!(h.settle_restores_held(true).expect("held"), (0, true));
    assert_eq!(states(&h, "1000"), vec![AttachmentState::Asking]);
    // ... and the answer that came in time is applied to the request that is still open.
    let done = h
        .apply_starting(vec![restore(
            &h,
            "1000",
            answer(vec![slot(1, &["uuid:a"], 10)]),
        )])
        .expect("answer");
    assert_eq!(downloaded(&done), vec![a], "the answer downloads the file");
    assert_eq!(h.settle_restores_held(false).expect("judged"), (0, false));
    assert!(matches!(
        states(&h, "1000")[0],
        AttachmentState::Downloading { .. }
    ));
}

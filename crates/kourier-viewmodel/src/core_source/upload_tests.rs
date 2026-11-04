//! `CoreSource`: upload progress (the ring on each file of an own message that waits or
//! uploads, the X from the core's flag, the line "Uploading 45% · 12 of 30 MB"), against the
//! simulated core of [`super::tests`].

use kourier_core::facade::{Progress, UiEvent};
use kourier_model::{
    AccountId, Attachment, AttachmentId, AttachmentKind, AttachmentState, ConnState, SendPhase,
};

use super::tests::{Sent, Store, make_pending, settle, source};
use super::*;
use crate::timeline::{RowKey, UploadView};

const CONV: ConversationId = ConversationId(1);
/// The pictures of the album (attachments 21 to 27) and the file after it (28).
const PICTURES: std::ops::RangeInclusive<i64> = 21..=27;
const FILE: i64 = 28;
/// Seven pictures of 1 MB and a file of 23 MB: 30 MB in all.
const PICTURE_BYTES: u64 = 1_000_000;
const FILE_BYTES: u64 = 23_000_000;

fn attachment(id: i64, kind: AttachmentKind, size: u64) -> Attachment {
    Attachment {
        id: Some(AttachmentId(id)),
        kind,
        mime: if kind == AttachmentKind::Image {
            "image/jpeg".into()
        } else {
            "application/pdf".into()
        },
        size,
        dims: (kind == AttachmentKind::Image).then_some((640, 480)),
        file_name: (kind != AttachmentKind::Image).then(|| "plan.pdf".into()),
        state: AttachmentState::Local,
        ..Attachment::default()
    }
}

fn media_body() -> Body {
    let mut attachments: Vec<Attachment> = PICTURES
        .map(|id| attachment(id, AttachmentKind::Image, PICTURE_BYTES))
        .collect();
    attachments.push(attachment(FILE, AttachmentKind::File, FILE_BYTES));
    Body::Media {
        attachments,
        caption: None,
    }
}

/// Conversation 1: the third message is an own message with the album and the file, on its way
/// (`in_flight`), and the core offers "Cancel Sending" for it when `cancel` says so. The
/// second one is somebody else's with the same files.
fn store(in_flight: bool, cancel: bool) -> (Store, MessageId) {
    let mut store = Store::new(&[(1, 4)]);
    let rows = store.convs.get_mut(&1).expect("conv");
    rows[1].body = media_body();
    make_pending(&mut rows[2], in_flight, None);
    rows[2].body = media_body();
    rows[2].actions.cancel_sending = cancel;
    let id = rows[2].id;
    (store, id)
}

fn open(store: &Store) -> (CoreSource, Sent) {
    let (mut src, sent) = source();
    src.reload();
    src.open_conversation(Some(CONV));
    settle(&mut src, &sent, store);
    src.on_event(&UiEvent::ConnState {
        account: AccountId(1),
        state: ConnState::Online { detail: None },
    });
    (src, sent)
}

fn upload(
    src: &mut CoreSource,
    message: MessageId,
    attachment: i64,
    done: u64,
    total: u64,
) -> Changes {
    src.on_event(&UiEvent::Progress(Progress::Upload {
        message,
        attachment: AttachmentId(attachment),
        done,
        total,
    }))
}

fn transmit(
    src: &mut CoreSource,
    message: MessageId,
    progress: f32,
    phase: Option<SendPhase>,
) -> Changes {
    src.on_event(&UiEvent::Progress(Progress::Transmit {
        message,
        progress,
        phase,
    }))
}

/// Every file of the message reported at once, as a backend does at its first tick.
fn first_report(src: &mut CoreSource, message: MessageId) {
    for id in PICTURES {
        upload(src, message, id, 0, PICTURE_BYTES);
    }
    upload(src, message, FILE, 0, FILE_BYTES);
}

fn row_of(src: &CoreSource, message: MessageId) -> MessageRow {
    src.timeline(CONV)
        .into_iter()
        .find(|r| r.key == RowKey::Message(message))
        .expect("row")
}

/// The rings of the album's pictures (in order), of the "+N" tile and of the file.
fn rings(
    row: &MessageRow,
) -> (
    Vec<Option<UploadView>>,
    Option<UploadView>,
    Option<UploadView>,
) {
    let mut pictures = Vec::new();
    let mut more = None;
    let mut file = None;
    for block in &row.blocks {
        match block {
            Block::Album(a) => {
                pictures = a.items.iter().map(|i| i.upload.clone()).collect();
                more = a.more.clone();
            }
            Block::File(f) => file = f.upload.clone(),
            _ => {}
        }
    }
    (pictures, more, file)
}

fn line(src: &CoreSource, message: MessageId) -> String {
    let lines = src.wait_lines(CONV);
    let (_, l) = lines.iter().find(|(m, _)| *m == message).expect("a line");
    l.text()
}

/// The rings move on the per-row path (`Changes::uploads`), never the timeline, and what the
/// source hands that path (`upload_rows`) is what a rebuilt timeline shows. A report that stays
/// inside the same whole percent of its file changes nothing.
#[test]
fn a_report_moves_one_ring_and_rebuilds_nothing() {
    let (store, id) = store(true, true);
    let (mut src, _) = open(&store);
    transmit(&mut src, id, 0.0, Some(SendPhase::Uploading));
    first_report(&mut src, id);

    let shown = src.timeline(CONV);
    let (pictures, more, file) = rings(&row_of(&src, id));
    let zero = UploadView {
        share: 0.0,
        label: "0%".into(),
        cancel: true,
    };
    assert!(pictures.iter().all(|p| p.as_ref() == Some(&zero)));
    assert_eq!(
        more.as_ref(),
        Some(&zero),
        "the pictures behind +2 share a ring"
    );
    assert_eq!(file.as_ref(), Some(&zero));

    // A picture half up: its ring alone moves.
    let ch = upload(&mut src, id, 22, 500_000, PICTURE_BYTES);
    assert_eq!(
        ch,
        Changes {
            uploads: true,
            waits: true,
            ..Changes::default()
        },
        "the ring, and the line (its whole percent moved too)"
    );
    // Inside the same whole percent: nothing at all.
    assert_eq!(
        upload(&mut src, id, 22, 500_001, PICTURE_BYTES),
        Changes::default()
    );
    // The next percent: the ring again; the line's whole percent has not moved.
    let ch = upload(&mut src, id, 22, 510_000, PICTURE_BYTES);
    assert_eq!((ch.uploads, ch.timeline), (true, false));

    // The model's rows, patched, are the rows a rebuild would give.
    let patched = src.upload_rows(CONV, &shown);
    assert_eq!(patched.len(), 1, "one row changed");
    let (at, row) = &patched[0];
    assert_eq!(shown[*at].key, RowKey::Message(id));
    let rebuilt = row_of(&src, id);
    assert!(row.blocks == rebuilt.blocks);
    let (pictures, _, _) = rings(row);
    assert_eq!(
        pictures[1],
        Some(UploadView {
            share: 0.51,
            label: "51%".into(),
            cancel: true
        })
    );
    // The patched row is up to date: nothing more to hand out for it.
    let mut model = shown.clone();
    model[*at] = row.clone();
    assert!(src.upload_rows(CONV, &model).is_empty());

    // A file nobody shows, and a report for a message that is not on its way, change nothing.
    assert_eq!(upload(&mut src, id, 99, 1, 2), Changes::default());
    let other = store.convs[&1][1].id;
    assert_eq!(upload(&mut src, other, 21, 1, 2), Changes::default());
    assert_eq!(upload(&mut src, id, 22, 1, 0), Changes::default());
    // The pump reads the other conversation's rows from nothing.
    assert!(src.upload_rows(ConversationId(2), &shown).is_empty());
}

/// The line under the message: "Preparing files" until a byte is up, then the bytes of all the
/// files together in his form, with the timer; it changes (`Changes::waits`) when the
/// message's whole percent does, and the ticker's rows say the same as a rebuilt timeline.
#[test]
fn the_line_follows_the_bytes_of_all_the_files() {
    let (store, id) = store(true, true);
    let (mut src, _) = open(&store);
    transmit(&mut src, id, 0.0, Some(SendPhase::Uploading));
    assert_eq!(
        line(&src, id),
        "Uploading files · 0:42",
        "nothing reported yet"
    );

    // The first report: the files are known, no byte is up.
    let ch = upload(&mut src, id, 21, 0, PICTURE_BYTES);
    assert!(ch.waits, "the line changes to Preparing files");
    for att in 22..=27 {
        upload(&mut src, id, att, 0, PICTURE_BYTES);
    }
    upload(&mut src, id, FILE, 0, FILE_BYTES);
    assert_eq!(line(&src, id), "Preparing files · 0:42");

    // 1 MB (a picture) + 0.45 MB + 11.55 MB of the file: 13 of 30 MB, 43 %.
    upload(&mut src, id, 21, PICTURE_BYTES, PICTURE_BYTES);
    upload(&mut src, id, 22, 450_000, PICTURE_BYTES);
    let ch = upload(&mut src, id, FILE, 11_550_000, FILE_BYTES);
    assert!(ch.waits);
    assert_eq!(line(&src, id), "Uploading 43% · 13 of 30 MB · 0:42");
    let row = row_of(&src, id);
    assert_eq!(
        (row.wait.as_str(), row.wait_timer.as_str()),
        ("Uploading 43% · 13 of 30 MB", "· 0:42"),
        "a rebuilt row says the same"
    );
    // A report inside the same whole percent of the message moves the ring, not the line.
    let ch = upload(&mut src, id, FILE, 11_551_000, FILE_BYTES);
    assert!(!ch.waits, "{ch:?}");

    // A hold goes first: offline, the connection is what is missing.
    src.on_event(&UiEvent::ConnState {
        account: AccountId(1),
        state: ConnState::Offline { retry_at_ms: None },
    });
    assert_eq!(line(&src, id), "Waiting for the connection · 0:42");
}

/// Call 5: the bytes of a message are let go when its uploads are over (a full share or
/// another step, or every file up), so that no ring stays at 100 % through a group's key setup;
/// a step that is still the upload keeps them, and so does a report for another message.
#[test]
fn the_bytes_are_let_go_when_the_uploads_are_over() {
    let over = |name: &str, end: &dyn Fn(&mut CoreSource, MessageId), line_after: &str| {
        let (store, id) = store(true, true);
        let (mut src, _) = open(&store);
        transmit(&mut src, id, 0.0, Some(SendPhase::Uploading));
        first_report(&mut src, id);
        upload(&mut src, id, 21, 600_000, PICTURE_BYTES);
        let before = row_of(&src, id);
        assert!(rings(&before).0[0].is_some(), "{name}: a ring before");
        end(&mut src, id);
        let (pictures, more, file) = rings(&row_of(&src, id));
        assert!(
            pictures.iter().all(Option::is_none) && more.is_none() && file.is_none(),
            "{name}: no ring is left"
        );
        assert_eq!(line(&src, id), line_after, "{name}");
    };
    over(
        "the share is full",
        &|src, id| {
            let ch = transmit(src, id, 1.0, None);
            assert!(ch.uploads, "the rings are taken off the row");
            assert!(!ch.timeline);
        },
        "Sending… 0:42",
    );
    over(
        "the group's next step",
        &|src, id| {
            transmit(src, id, 0.4, Some(SendPhase::RefreshingGroup));
        },
        "Checking the group · 0:42",
    );
    over(
        "every file is up",
        &|src, id| {
            for att in PICTURES {
                upload(src, id, att, PICTURE_BYTES, PICTURE_BYTES);
            }
            upload(src, id, FILE, FILE_BYTES, FILE_BYTES);
        },
        "Uploading files · 0:42",
    );

    // Still the upload step, or a report of another message: the bytes stay.
    let (store, id) = store(true, true);
    let (mut src, _) = open(&store);
    transmit(&mut src, id, 0.0, Some(SendPhase::Uploading));
    first_report(&mut src, id);
    assert_eq!(
        transmit(&mut src, id, 0.5, Some(SendPhase::Uploading)),
        Changes::default()
    );
    let other = store.convs[&1][1].id;
    assert_eq!(transmit(&mut src, other, 1.0, None), Changes::default());
    assert!(rings(&row_of(&src, id)).0[0].is_some());
}

/// The bytes do not outlive their attempt: when the row stops being in flight (a failure, a
/// retry wait) the next window forgets them, and the next attempt reports its own.
#[test]
fn the_bytes_do_not_outlive_their_attempt() {
    let (mut store, id) = store(true, true);
    let (mut src, sent) = open(&store);
    transmit(&mut src, id, 0.0, Some(SendPhase::Uploading));
    first_report(&mut src, id);
    upload(&mut src, id, 21, 600_000, PICTURE_BYTES);
    // The attempt failed: the row waits to be tried again.
    make_pending(
        &mut store.convs.get_mut(&1).expect("conv")[2],
        false,
        Some(1),
    );
    store.convs.get_mut(&1).expect("conv")[2]
        .actions
        .cancel_sending = false;
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: vec![id],
    });
    settle(&mut src, &sent, &store);
    let row = row_of(&src, id);
    // A message in a retry wait: the empty rings, with the core's answer for the X.
    let (pictures, _, file) = rings(&row);
    let empty = Some(UploadView {
        share: 0.0,
        label: String::new(),
        cancel: false,
    });
    assert!(pictures.iter().all(|p| *p == empty));
    assert_eq!(file, empty);
    assert!(!line(&src, id).starts_with("Uploading"));
}

/// A file that waits for its turn has an empty ring with the core's X: queued behind another
/// message, offline, held or in a retry wait (the row waits and is not in flight). A message in
/// flight has rings only where the backend reports (or says it prepares the files); a failed
/// one, a sent one and somebody else's have none.
#[test]
fn a_file_that_waits_has_an_empty_ring_with_the_cores_x() {
    // Queued, and the core takes it back.
    let (store_q, id) = store(false, true);
    let (src, _) = open(&store_q);
    let row = row_of(&src, id);
    let (pictures, more, file) = rings(&row);
    let empty = UploadView {
        share: 0.0,
        label: String::new(),
        cancel: true,
    };
    assert!(pictures.iter().all(|p| p.as_ref() == Some(&empty)));
    assert_eq!((more.as_ref(), file.as_ref()), (Some(&empty), Some(&empty)));
    assert!(row.actions.cancel_sending);
    // Somebody else's message with the same files has none.
    let theirs = store_q.convs[&1][1].id;
    assert_eq!(rings(&row_of(&src, theirs)), (vec![None; 7], None, None));

    // In flight, and the backend has said nothing of uploads: no ring.
    let (store_f, id) = store(true, false);
    let (mut src, _) = open(&store_f);
    assert_eq!(rings(&row_of(&src, id)), (vec![None; 7], None, None));
    // In flight, uploading, no file reported and no `Preparing` said: still no ring (the
    // backend does not report files; the line says "Uploading files").
    transmit(&mut src, id, 0.0, Some(SendPhase::Uploading));
    assert_eq!(rings(&row_of(&src, id)), (vec![None; 7], None, None));
    // In flight, the backend SAYS it prepares the files: an empty ring on each, without the X
    // (the core does not offer it yet).
    transmit(&mut src, id, 0.0, Some(SendPhase::Preparing));
    let (pictures, _, file) = rings(&row_of(&src, id));
    let preparing = Some(UploadView {
        share: 0.0,
        label: String::new(),
        cancel: false,
    });
    assert!(pictures.iter().all(|p| *p == preparing));
    assert_eq!(file, preparing);
    // The first report: rings with their numbers.
    first_report(&mut src, id);
    assert_eq!(
        rings(&row_of(&src, id)).2.map(|r| r.label),
        Some("0%".into())
    );
    // A step after the uploads: none.
    transmit(&mut src, id, 1.0, None);
    assert_eq!(rings(&row_of(&src, id)), (vec![None; 7], None, None));

    // Failed: the row left the outbox's waiting rows.
    let (mut store_x, id) = store(false, false);
    {
        let m = &mut store_x.convs.get_mut(&1).expect("conv")[2];
        m.outbox = None;
        m.out_state = Some(OutgoingState::Failed {
            reason: kourier_model::FailReason::Network,
            retryable: true,
        });
    }
    let (src, _) = open(&store_x);
    assert_eq!(rings(&row_of(&src, id)), (vec![None; 7], None, None));
}

/// Upload progress, fix U3. "Preparing files" is the re-encode window itself: a backend that
/// says `SendPhase::Preparing` and reports no file yet gets the line AND an empty ring with the
/// core's X on every file at once (the rings are refreshed by the phase alone, never a
/// timeline rebuild); when the phase turns to the uploads the empty rings are replaced by the
/// files' own figures, and when it turns to a step after them they go.
#[test]
fn a_backend_that_prepares_its_files_shows_the_line_and_a_ring_per_file() {
    let (rows, id) = store(true, true);
    let (mut src, _) = open(&rows);
    let ch = transmit(&mut src, id, 0.0, Some(SendPhase::Preparing));
    assert!(
        ch.uploads && ch.waits && !ch.timeline,
        "the rings and the line move on their paths: {ch:?}"
    );
    assert_eq!(line(&src, id), "Preparing files · 0:42");
    let (pictures, more, file) = rings(&row_of(&src, id));
    let empty = UploadView {
        share: 0.0,
        label: String::new(),
        cancel: true,
    };
    assert!(pictures.iter().all(|p| p.as_ref() == Some(&empty)));
    assert_eq!((more.as_ref(), file.as_ref()), (Some(&empty), Some(&empty)));
    // Said again (a ticker repeats it): nothing moves.
    assert_eq!(
        transmit(&mut src, id, 0.0, Some(SendPhase::Preparing)),
        Changes::default()
    );
    // The files are known: figures per file (the first report), then the phase turns to the
    // uploads; the rings keep their figures.
    first_report(&mut src, id);
    let ch = transmit(&mut src, id, 0.0, Some(SendPhase::Uploading));
    assert!(!ch.uploads, "the files' own rings stand: {ch:?}");
    let zero = UploadView {
        share: 0.0,
        label: "0%".into(),
        cancel: true,
    };
    assert_eq!(rings(&row_of(&src, id)).2.as_ref(), Some(&zero));

    // A message whose backend leaves `Preparing` for a step after the uploads without a file
    // ever reported: the empty rings go with the phase.
    let (rows, id) = store(true, true);
    let (mut src, _) = open(&rows);
    transmit(&mut src, id, 0.0, Some(SendPhase::Preparing));
    let ch = transmit(&mut src, id, 0.3, Some(SendPhase::RefreshingGroup));
    assert!(ch.uploads, "the empty rings are taken off: {ch:?}");
    assert_eq!(rings(&row_of(&src, id)), (vec![None; 7], None, None));
}

/// The X came after the message went out (R-Q2): the note "Already sent", with the message its
/// Delete for Everyone acts on, only in the chat that is open and only while it is the notice.
#[test]
fn the_note_for_an_x_that_came_too_late() {
    let (store, id) = store(true, true);
    let (mut src, _) = open(&store);
    assert!(src.notice_message().is_none());
    // Another chat's note is nobody's here.
    let ch = src.on_event(&UiEvent::CancelTooLate {
        account: AccountId(1),
        conv: ConversationId(2),
        message: id,
    });
    assert!(!ch.any() && src.notice().is_empty());
    let ch = src.on_event(&UiEvent::CancelTooLate {
        account: AccountId(1),
        conv: CONV,
        message: id,
    });
    assert!(ch.notice);
    assert_eq!(
        (src.notice().as_str(), src.notice_message()),
        ("Already sent", Some(id))
    );
    // Another notice replaces it: no Delete for Everyone for the old message.
    src.on_action_failed(
        "SendText",
        &CoreError::InvalidInput("x"),
        &ActionSubject::None,
    );
    assert_ne!(src.notice(), "Already sent");
    assert!(src.notice_message().is_none());
    src.on_event(&UiEvent::CancelTooLate {
        account: AccountId(1),
        conv: CONV,
        message: id,
    });
    assert_eq!(src.notice_message(), Some(id));
    src.clear_notice();
    assert!(src.notice_message().is_none());
    let _ = store;
}

/// A measurement, not a test (call 10): one report on a 300-row window with a 31-picture
/// album uploading, the viewmodel's handling and the rows it hands the model. Run by hand:
/// `cargo test -p kourier-viewmodel --lib upload_tests::measure -- --ignored --nocapture`.
#[test]
#[ignore = "a measurement, run by hand"]
fn measure_a_report_on_a_300_row_chat_with_a_31_picture_album() {
    use std::time::Instant;
    let mut store = Store::new(&[(1, 300)]);
    let id = {
        let rows = store.convs.get_mut(&1).expect("conv");
        let m = &mut rows[295];
        make_pending(m, true, None);
        m.actions.cancel_sending = true;
        m.body = Body::Media {
            attachments: (0..31)
                .map(|i| attachment(100 + i, AttachmentKind::Image, PICTURE_BYTES))
                .collect(),
            caption: None,
        };
        m.id
    };
    let (mut src, sent) = open(&store);
    while src.window_len() < 300 {
        assert!(src.fetch_older());
        settle(&mut src, &sent, &store);
    }
    transmit(&mut src, id, 0.0, Some(SendPhase::Uploading));
    for i in 0..31 {
        upload(&mut src, id, 100 + i, 0, PICTURE_BYTES);
    }
    let mut model = src.timeline(CONV);
    assert!(
        upload(&mut src, id, 100, 10_000, PICTURE_BYTES).uploads,
        "the album is in the window"
    );
    let reports: u64 = 2_000;
    let (mut handling, mut rows_for_model) = (0u128, 0u128);
    let mut patched = 0;
    for n in 0..reports {
        let t = Instant::now();
        // Every report moves the picture by a whole percent: the worst case.
        let picture = 100 + i64::try_from(n % 31).expect("small");
        let done = PICTURE_BYTES * (n / 31 + 1).min(100) / 100;
        let ch = upload(&mut src, id, picture, done, PICTURE_BYTES);
        handling += t.elapsed().as_nanos();
        assert!(!ch.timeline);
        let t = Instant::now();
        for (at, row) in src.upload_rows(CONV, &model) {
            model[at] = row;
            patched += 1;
        }
        rows_for_model += t.elapsed().as_nanos();
    }
    let rebuild = Instant::now();
    let _ = src.timeline(CONV);
    println!(
        "upload report, {} rows in the window, 31 pictures: handling {} us, upload_rows {} us (patched {patched} of {reports}); a whole timeline rebuild {} us",
        model.len(),
        handling / u128::from(reports) / 1000,
        rows_for_model / u128::from(reports) / 1000,
        rebuild.elapsed().as_micros(),
    );
}

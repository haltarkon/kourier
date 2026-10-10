//! `CoreSource`: the composer's file plan, files in preparation, album rows and the edit
//! history, against the simulated core of [`super::tests`].

use kourier_core::facade::{
    FilePlanQuery, FilesPreview, MessageRevisions, OriginKind, Progress, Query, QueryData,
    RequestId, Revision,
};
use kourier_model::{
    Attachment, AttachmentId, AttachmentKind, AttachmentState, Body, ConversationId, FilePlan,
    MessageId, Text,
};

use super::tests::{NOW, Sent, Store, message, result, settle, source, take_queries};
use super::*;
use crate::files::{FileStrip, PlanRequest};
use crate::timeline::RowKey;

const CONV: ConversationId = ConversationId(1);

fn opened(store: &Store) -> (CoreSource, Sent) {
    let (mut src, sent) = source();
    src.reload();
    settle(&mut src, &sent, store);
    src.open_conversation(Some(CONV));
    settle(&mut src, &sent, store);
    (src, sent)
}

fn request(serial: u64, names: &[&str]) -> PlanRequest {
    PlanRequest {
        serial,
        paths: names
            .iter()
            .map(|n| std::path::PathBuf::from(format!("/t/{n}")))
            .collect(),
        as_file: Vec::new(),
        caption: false,
        caption_text: None,
        reply: false,
    }
}

/// The `SendFiles` commands handed to the core so far: their request, how many files, and
/// which of them go as the file they are.
fn sends(sent: &Sent) -> Vec<(RequestId, usize, Vec<bool>)> {
    sent.borrow()
        .iter()
        .filter_map(|c| match c {
            UiCommand::SendFiles {
                request,
                paths,
                as_file,
                ..
            } => Some((*request, paths.expose().len(), as_file.clone())),
            _ => None,
        })
        .collect()
}

fn stored(conv: ConversationId, request: RequestId) -> UiEvent {
    UiEvent::FilesStored {
        conv,
        request,
        messages: Vec::new(),
    }
}

fn copied(conv: ConversationId, request: RequestId, files_done: u32, files: u32) -> UiEvent {
    UiEvent::Progress(Progress::Import {
        conv,
        request,
        files_done,
        files,
        bytes_done: u64::from(files_done) * 10,
        bytes: u64::from(files) * 10,
    })
}

/// A preview that puts every file into a message of its own.
fn alone(files: usize) -> QueryData {
    QueryData::FilePlan(FilesPreview {
        kinds: vec![Some(AttachmentKind::File); files],
        refused: Vec::new(),
        plan: FilePlan {
            lead_text: false,
            caption_messages: 0,
            messages: (0..files).map(|i| vec![i]).collect(),
            reply_unsendable: false,
        },
    })
}

fn plan_queries(sent: &Sent) -> Vec<(QueryToken, usize)> {
    take_queries(sent)
        .into_iter()
        .filter_map(|(t, q)| match q {
            Query::FilePlan(q) => {
                assert_eq!(q.conv, CONV);
                Some((t, q.paths.expose().len()))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn the_plan_of_the_newest_selection_wins() {
    let (mut src, sent) = opened(&Store::new(&[(1, 3)]));
    // The user adds files, then more while the core still looks at the first lot.
    assert!(src.plan_files(CONV, request(1, &["a", "b"])));
    let first = plan_queries(&sent);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].1, 2);
    assert!(src.plan_files(CONV, request(2, &["a", "b", "c"])));
    assert!(src.plan_files(CONV, request(3, &["a", "b", "c", "d"])));
    assert!(plan_queries(&sent).is_empty(), "one query in flight");
    // The answer for the first lot is of no use any more; the newest selection is asked for.
    let ch = src.on_event(&result(first[0].0, alone(2)));
    assert!(!ch.plan && src.take_file_plan().is_none());
    let second = plan_queries(&sent);
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].1, 4, "the selection as it is now");
    let ch = src.on_event(&result(second[0].0, alone(4)));
    assert!(ch.plan);
    let answer = src.take_file_plan().expect("the plan");
    assert_eq!(answer.serial, 3);
    assert_eq!(answer.preview.plan.messages.len(), 4);
    assert!(src.take_file_plan().is_none(), "taken once");
    assert!(plan_queries(&sent).is_empty());
    // The strip takes it only for the selection it was made for.
    let mut strip = FileStrip::new();
    strip.add(request(0, &["a", "b", "c", "d"]).paths, false);
    assert!(!strip.apply_plan(&answer), "another serial");
    // A query the core cannot answer leaves the strip without a split (and without waiting
    // for one), and the next request asks again.
    assert!(src.plan_files(CONV, request(4, &["a"])));
    let third = plan_queries(&sent);
    let ch = src.on_event(&UiEvent::QueryResult {
        token: third[0].0,
        result: Err(std::sync::Arc::new(CoreError::UnknownConversation(CONV))),
    });
    assert!(ch.plan && src.take_file_plan().is_none());
    assert_eq!(
        src.take_plan_failure(),
        Some(4),
        "the composer stops waiting"
    );
    assert_eq!(src.take_plan_failure(), None, "told once");
    assert!(src.plan_files(CONV, request(5, &["a"])));
    assert_eq!(plan_queries(&sent).len(), 1);
}

fn picture(id: i64, kind: AttachmentKind) -> Attachment {
    Attachment {
        id: Some(AttachmentId(id)),
        kind,
        mime: match kind {
            AttachmentKind::Video => "video/mp4".into(),
            AttachmentKind::File => "application/pdf".into(),
            _ => "image/jpeg".into(),
        },
        size: 40_000,
        dims: Some((640, 480)),
        file_name: (kind == AttachmentKind::File).then(|| "plan.pdf".into()),
        state: AttachmentState::Local,
        ..Attachment::default()
    }
}

fn own_media(n: i64, attachments: Vec<Attachment>, caption: Option<&str>) -> super::CoreMessage {
    let mut m = message(1, n, false);
    m.author = None;
    m.author_name = None;
    m.origin = OriginKind::OwnThisDevice;
    m.read_state = None;
    m.body = Body::Media {
        attachments,
        caption: caption.map(Text::plain),
    };
    m
}

#[test]
fn files_in_preparation_are_counted_until_the_core_says_they_are_stored() {
    let mut store = Store::new(&[(1, 3)]);
    let (mut src, sent) = opened(&store);
    assert_eq!(src.preparing_files(CONV), 0);
    let paths: Vec<std::path::PathBuf> = (0..5)
        .map(|i| std::path::PathBuf::from(format!("/t/{i}.jpg")))
        .collect();
    let id = src
        .send_files(CONV, paths, Vec::new(), "", None)
        .expect("sent");
    let request = sends(&sent)[0].0;
    assert_eq!(request.0, id, "the id the composer got is the command's");
    assert_eq!(
        (src.preparing_files(CONV), src.prepared_files(CONV)),
        (5, 0)
    );
    assert_eq!(src.preparing_files(ConversationId(2)), 0);
    // An incoming message meanwhile changes nothing: the files are still being prepared.
    store
        .convs
        .get_mut(&1)
        .expect("conv")
        .push(message(1, 4, true));
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: vec![MessageId(100_004)],
    });
    let ch = settle(&mut src, &sent, &store);
    assert!(!ch.files);
    assert_eq!(src.preparing_files(CONV), 5);
    // The core says how far the copy is. Its first report (nothing copied yet) and a report
    // that goes backwards change nothing; neither does one about a request nobody made.
    assert!(!src.on_event(&copied(CONV, request, 0, 5)).files);
    assert!(src.on_event(&copied(CONV, request, 3, 5)).files);
    assert_eq!(src.prepared_files(CONV), 3);
    assert!(!src.on_event(&copied(CONV, request, 2, 5)).files);
    assert!(!src.on_event(&copied(CONV, RequestId(999), 4, 5)).files);
    assert_eq!(
        (src.preparing_files(CONV), src.prepared_files(CONV)),
        (5, 3)
    );
    assert!(src.on_event(&copied(CONV, request, 5, 5)).files);
    // The send became two messages (an album of three and one of two). Their rows alone do
    // not end the count: only the core knows that the whole send is stored.
    {
        use AttachmentKind::Image;
        let rows = store.convs.get_mut(&1).expect("conv");
        rows.push(own_media(
            5,
            vec![picture(1, Image), picture(2, Image), picture(3, Image)],
            None,
        ));
        rows.push(own_media(
            6,
            vec![picture(4, Image), picture(5, Image)],
            None,
        ));
    }
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: vec![MessageId(100_005), MessageId(100_006)],
    });
    let ch = settle(&mut src, &sent, &store);
    assert!(ch.timeline && !ch.files);
    assert_eq!(src.preparing_files(CONV), 5);
    assert!(src.take_finished_sends().is_empty());
    // "Stored" ends it, and tells the composer once that the request is over.
    let ch = src.on_event(&stored(CONV, request));
    assert!(ch.files);
    assert_eq!(
        (src.preparing_files(CONV), src.prepared_files(CONV)),
        (0, 0)
    );
    assert_eq!(
        src.take_finished_sends(),
        vec![FinishedSend {
            request: id,
            conv: CONV,
            outcome: SendOutcome::Stored,
        }]
    );
    assert!(src.take_finished_sends().is_empty(), "told once");
    assert!(!src.on_event(&stored(CONV, request)).files, "a repeat");
    // The two messages are separate rows in sending order (newest first), each an album.
    let rows = src.timeline(CONV);
    assert_eq!(rows[0].key, RowKey::Message(MessageId(100_006)));
    assert_eq!(rows[1].key, RowKey::Message(MessageId(100_005)));
    assert!(matches!(rows[0].blocks.as_slice(), [Block::Album(a)] if a.len() == 2));
    assert!(matches!(rows[1].blocks.as_slice(), [Block::Album(a)] if a.len() == 3));
    assert!(rows[0].own && rows[1].own);
}

/// `count` own messages with one document each, numbered from `first`.
fn documents(first: i64, count: i64) -> Vec<super::CoreMessage> {
    (first..first + count)
        .map(|n| own_media(n, vec![picture(n, AttachmentKind::File)], None))
        .collect()
}

fn files(count: usize) -> Vec<std::path::PathBuf> {
    (0..count)
        .map(|i| std::path::PathBuf::from(format!("/t/{i}.pdf")))
        .collect()
}

#[test]
fn sends_are_told_apart_by_their_request_whatever_chat_is_open() {
    let mut store = Store::new(&[(1, 3), (2, 3)]);
    let (mut src, sent) = opened(&store);
    // Two sends to one chat and one to another: three requests, each with its own id.
    let first = src
        .send_files(CONV, files(3), Vec::new(), "", None)
        .expect("sent");
    let second = src
        .send_files(CONV, files(2), Vec::new(), "", None)
        .expect("sent");
    let other = src
        .send_files(ConversationId(2), files(4), Vec::new(), "", None)
        .expect("sent");
    assert!(first != second && second != other && first != other);
    assert_eq!(src.preparing_files(CONV), 5);
    assert_eq!(src.preparing_files(ConversationId(2)), 4);
    // The user goes on to the other chat while the core still copies. Messages of the
    // first chat that are announced meanwhile (somebody wrote, a phone sent pictures, and
    // the 150 rows of a large send would look the same) do not end anything: in phase H a
    // closed chat's count ended with the first new message, whoever sent it.
    src.open_conversation(Some(ConversationId(2)));
    settle(&mut src, &sent, &store);
    store
        .convs
        .get_mut(&1)
        .expect("conv")
        .extend(documents(4, 3));
    let ch = src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: vec![MessageId(100_004), MessageId(100_005), MessageId(100_006)],
    });
    assert!(!ch.files);
    assert_eq!(src.preparing_files(CONV), 5);
    // The answers come per request, in whatever order.
    assert!(src.on_event(&stored(CONV, RequestId(second))).files);
    assert_eq!(src.preparing_files(CONV), 3);
    assert!(src.on_event(&copied(CONV, RequestId(first), 1, 3)).files);
    assert_eq!(src.prepared_files(CONV), 1);
    assert_eq!(src.prepared_files(ConversationId(2)), 0);
    assert!(src.on_event(&stored(CONV, RequestId(first))).files);
    assert_eq!(src.preparing_files(CONV), 0);
    assert_eq!(src.preparing_files(ConversationId(2)), 4, "still copying");
    let over: Vec<(u64, SendOutcome)> = src
        .take_finished_sends()
        .iter()
        .map(|f| (f.request, f.outcome))
        .collect();
    assert_eq!(
        over,
        [(second, SendOutcome::Stored), (first, SendOutcome::Stored)]
    );
    // Coming back to the first chat changes nothing about the count either way.
    src.open_conversation(Some(CONV));
    settle(&mut src, &sent, &store);
    assert_eq!(src.preparing_files(CONV), 0);
    assert_eq!(src.preparing_files(ConversationId(2)), 4);
    assert!(
        src.on_event(&stored(ConversationId(2), RequestId(other)))
            .files
    );
    assert_eq!(src.preparing_files(ConversationId(2)), 0);
}

#[test]
fn lost_events_end_the_count_without_calling_the_files_stored() {
    // The channel lagged: the answer to a send may be among the lost events. The composer
    // stops saying "preparing", and is told that it is not known whether the core is done,
    // so it keeps the files it made for that send.
    let store = Store::new(&[(1, 3)]);
    let (mut src, sent) = opened(&store);
    let id = src
        .send_files(CONV, files(150), Vec::new(), "", None)
        .expect("sent");
    assert_eq!(src.preparing_files(CONV), 150);
    let ch = src.reload();
    assert!(ch.files);
    assert_eq!(src.preparing_files(CONV), 0, "not stuck");
    assert_eq!(
        src.take_finished_sends(),
        vec![FinishedSend {
            request: id,
            conv: CONV,
            outcome: SendOutcome::Unknown,
        }]
    );
    assert!(!src.reload().files, "nothing changed the second time");
    // The late answer of that send is nothing new.
    assert!(!src.on_event(&stored(CONV, RequestId(id))).files);
    assert!(src.take_finished_sends().is_empty());
    let _ = sent;
}

#[test]
fn the_plan_is_asked_with_the_choices_and_a_long_caption() {
    let (mut src, sent) = opened(&Store::new(&[(1, 3)]));
    let mut wanted = request(1, &["a.jpg", "b.jpg", "c.pdf"]);
    wanted.as_file = vec![false, true, false];
    wanted.caption = true;
    assert!(src.plan_files(CONV, wanted.clone()));
    let asked = |sent: &Sent| -> Vec<FilePlanQuery> {
        take_queries(sent)
            .into_iter()
            .filter_map(|(_, q)| match q {
                Query::FilePlan(q) => Some(q),
                _ => None,
            })
            .collect()
    };
    let first = asked(&sent);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].as_file, [false, true, false]);
    // A caption of ordinary length: only that there is one.
    assert!(first[0].caption && first[0].caption_text.is_none());
    // One longer than a message carries is handed over as the send would hand it over
    // (line ends normalized, trimmed), so the plan counts exactly what will be sent.
    let (mut src, sent) = opened(&Store::new(&[(1, 3)]));
    wanted.serial = 2;
    wanted.caption_text = Some(format!("  {}\r\nend  ", "x".repeat(70_000)));
    assert!(src.plan_files(CONV, wanted));
    let second = asked(&sent);
    let text = second[0].caption_text.as_ref().expect("the caption");
    assert!(text.body.starts_with("  xxx") && text.body.ends_with("\nend"));
    assert!(!text.body.contains('\r'));
}

#[test]
fn every_refused_send_comes_back() {
    // Two sends are refused before the composer looked (the second is the first one sent
    // again without its refused file, refused again): both lots return, none is lost, and
    // what goes out again keeps the choices made for each file.
    use kourier_core::facade::{ActionSubject, FileRefusal, RefusedFile, RefusedFiles};
    let store = Store::new(&[(1, 3)]);
    let (mut src, sent) = opened(&store);
    let paths = files(3);
    let id = src
        .send_files(CONV, paths.clone(), vec![false, true, true], "", None)
        .expect("sent");
    let refusal = |request: RequestId, paths: &[std::path::PathBuf], as_file: &[bool]| {
        UiEvent::ActionFailed {
            account: None,
            action: "SendFiles",
            error: std::sync::Arc::new(CoreError::InvalidInput("x")),
            subject: ActionSubject::Files(std::sync::Arc::new(RefusedFiles {
                conv: CONV,
                request,
                as_file: as_file.to_vec(),
                paths: paths.to_vec().into(),
                caption: None,
                reply_to: None,
                refused: vec![RefusedFile {
                    index: 0,
                    reason: FileRefusal::Unreadable,
                }],
            })),
        }
    };
    let ch = src.on_event(&refusal(RequestId(id), &paths, &[false, true, true]));
    assert!(ch.files);
    assert_eq!(src.preparing_files(CONV), 2, "the other two went out again");
    let again = sends(&sent);
    assert_eq!(again.len(), 2);
    assert_ne!(again[1].0, again[0].0, "a request of its own");
    assert_eq!(again[1].2, [true, true], "each file keeps its choice");
    // That send is refused too: the second file is gone meanwhile.
    let ch = src.on_event(&refusal(again[1].0, &paths[1..], &[true, true]));
    assert!(ch.files);
    assert_eq!(src.preparing_files(CONV), 1, "the last one went out again");
    let last = sends(&sent);
    assert_eq!(last[2].2, [true]);
    let first = src.take_refused_files().expect("the first refused file");
    let second = src.take_refused_files().expect("the second refused file");
    assert_eq!(
        first.files,
        vec![ReturnedFile {
            path: paths[0].clone(),
            as_file: false,
            reason: Some(FileRefusal::Unreadable),
        }]
    );
    assert_eq!(
        (first.request, first.resent, first.resent_request),
        (id, 2, Some(again[1].0.0))
    );
    assert_eq!(
        second.files,
        vec![ReturnedFile {
            path: paths[1].clone(),
            as_file: true,
            reason: Some(FileRefusal::Unreadable),
        }]
    );
    assert_eq!(
        (second.request, second.resent_request),
        (again[1].0.0, Some(last[2].0.0))
    );
    assert!(src.take_refused_files().is_none());
    // Both refused requests are over; the last one is still with the core.
    let over: Vec<(u64, SendOutcome)> = src
        .take_finished_sends()
        .iter()
        .map(|f| (f.request, f.outcome))
        .collect();
    assert_eq!(
        over,
        [
            (id, SendOutcome::Refused),
            (again[1].0.0, SendOutcome::Refused)
        ]
    );
    assert_eq!(src.preparing_files(CONV), 1);
    assert!(src.on_event(&stored(CONV, last[2].0)).files);
    assert_eq!(src.preparing_files(CONV), 0);
    let counts: Vec<usize> = sends(&sent).iter().map(|s| s.1).collect();
    assert_eq!(counts, [3, 2, 1]);
}

#[test]
fn neighbouring_pictures_and_videos_are_one_album() {
    use AttachmentKind::{File, Image, Video};
    let mut store = Store::new(&[(1, 2)]);
    {
        let rows = store.convs.get_mut(&1).expect("conv");
        // What another client may send in one message: a file between pictures.
        rows.push(own_media(
            3,
            vec![
                picture(1, Image),
                picture(2, Image),
                picture(3, Video),
                picture(4, File),
                picture(5, Image),
            ],
            Some("all of it"),
        ));
        // More pictures than the grid has tiles: all of them are in the row.
        rows.push(own_media(
            4,
            (10..50).map(|i| picture(i, Image)).collect(),
            None,
        ));
        rows.push(own_media(5, vec![picture(60, Image)], None));
    }
    let (src, _) = opened(&store);
    let rows = src.timeline(CONV);
    let row = |n: i64| {
        rows.iter()
            .find(|r| r.key == RowKey::Message(MessageId(100_000 + n)))
            .expect("row")
    };
    let mixed = row(3);
    match mixed.blocks.as_slice() {
        [
            Block::Album(album),
            Block::File(file),
            Block::Image(last),
            Block::Text(caption),
        ] => {
            assert_eq!(album.len(), 3);
            assert_eq!(album[2].kind, Video);
            assert_eq!(file.kind, File);
            assert_eq!(last.attachment, Some(AttachmentId(5)));
            assert_eq!(caption.plain, "all of it");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        mixed.attachment_kinds(),
        ["image", "image", "video", "file", "image"]
    );
    assert!(mixed.has_text() && mixed.quote().is_none());
    let many = row(4);
    assert!(matches!(many.blocks.as_slice(), [Block::Album(a)] if a.len() == 40));
    assert_eq!(many.attachment_kinds().len(), 40);
    // One picture stays a picture.
    assert!(matches!(row(5).blocks.as_slice(), [Block::Image(_)]));
    // A reply to an album quotes it as what it is.
    let quote = crate::timeline::quote_of_row(many).expect("quote");
    assert_eq!(quote.text, "40 photos");
    assert_eq!(
        crate::timeline::edit_text_of_row(mixed),
        ("all of it".into(), true)
    );
    assert_eq!(
        crate::timeline::edit_text_of_row(many),
        (String::new(), true)
    );
}

#[test]
fn the_edit_history_lists_versions_newest_first() {
    let (mut src, sent) = opened(&Store::new(&[(1, 3)]));
    let message = MessageId(100_003);
    assert!(src.request_revisions(message));
    let asked: Vec<(QueryToken, Query)> = take_queries(&sent);
    assert!(matches!(asked.as_slice(), [(_, Query::Revisions(m))] if *m == message));
    let day = 24 * 3_600_000;
    let found = MessageRevisions {
        message,
        conv: CONV,
        revisions: vec![
            Revision {
                revision: 0,
                text: Some(Text::plain("See you at 5")),
                at: NOW - day - 60_000,
            },
            Revision {
                revision: 1,
                text: None,
                at: NOW - 120_000,
            },
            Revision {
                revision: 2,
                text: Some(Text::plain("See you at 6 <b>sharp</b>")),
                at: NOW - 60_000,
            },
        ],
    };
    let ch = src.on_event(&result(asked[0].0, QueryData::Revisions(Some(found))));
    assert!(ch.revisions);
    let list = src.take_revisions().expect("the history");
    assert_eq!(list.message, message);
    let rows = &list.rows;
    assert_eq!(rows.len(), 3);
    assert!(rows[0].current && !rows[0].original);
    assert!(
        !rows[1].current && rows[1].text.is_none(),
        "a version without a caption"
    );
    assert!(rows[2].original && !rows[2].current);
    let newest = rows[0].text.as_ref().expect("text");
    assert_eq!(newest.plain, "See you at 6 <b>sharp</b>");
    assert!(
        newest.html.contains("&lt;b&gt;"),
        "text from the network is escaped"
    );
    assert!(rows[0].when.starts_with("Today, "), "{}", rows[0].when);
    assert!(rows[2].when.starts_with("Yesterday, "), "{}", rows[2].when);
    assert!(src.take_revisions().is_none(), "taken once");
    // A message that is gone has no history; asking for another message meanwhile drops the
    // first answer.
    assert!(src.request_revisions(MessageId(1)));
    let first = take_queries(&sent);
    assert!(src.request_revisions(MessageId(2)));
    let ch = src.on_event(&result(first[0].0, QueryData::Revisions(None)));
    assert!(!ch.revisions && src.take_revisions().is_none());
    let second = take_queries(&sent);
    assert!(matches!(second.as_slice(), [(_, Query::Revisions(m))] if *m == MessageId(2)));
    let ch = src.on_event(&result(second[0].0, QueryData::Revisions(None)));
    assert!(ch.revisions);
    let gone = src.take_revisions().expect("answer");
    assert!(gone.rows.is_empty());
    assert_eq!(gone.message, MessageId(2));
    let debug = format!("{list:?}");
    assert!(!debug.contains("sharp"), "{debug}");
}

#[test]
fn asking_twice_for_the_same_thing_does_not_block_later_questions() {
    // A double click on the "edited" mark asks for the same history twice before the first
    // answer is back. That answer settles both; the question after it must still go out.
    let (mut src, sent) = opened(&Store::new(&[(1, 3)]));
    let message = MessageId(100_003);
    assert!(src.request_revisions(message));
    let first = take_queries(&sent);
    assert_eq!(first.len(), 1);
    assert!(src.request_revisions(message));
    assert!(take_queries(&sent).is_empty(), "one query in flight");
    let ch = src.on_event(&result(first[0].0, QueryData::Revisions(None)));
    assert!(ch.revisions);
    assert_eq!(src.take_revisions().map(|l| l.message), Some(message));
    assert!(take_queries(&sent).is_empty(), "nothing more to ask");
    assert!(src.request_revisions(MessageId(100_002)));
    let next = take_queries(&sent);
    assert!(
        matches!(next.as_slice(), [(_, Query::Revisions(m))] if *m == MessageId(100_002)),
        "the next history is asked for"
    );
    let ch = src.on_event(&result(next[0].0, QueryData::Revisions(None)));
    assert!(ch.revisions);
    assert_eq!(
        src.take_revisions().map(|l| l.message),
        Some(MessageId(100_002))
    );

    // The same for the composer's plan: the same selection asked about twice.
    assert!(src.plan_files(CONV, request(1, &["a"])));
    let first = plan_queries(&sent);
    assert_eq!(first.len(), 1);
    assert!(src.plan_files(CONV, request(1, &["a"])));
    let ch = src.on_event(&result(first[0].0, alone(1)));
    assert!(ch.plan);
    assert_eq!(src.take_file_plan().map(|a| a.serial), Some(1));
    assert!(plan_queries(&sent).is_empty());
    assert!(src.plan_files(CONV, request(2, &["a", "b"])));
    let next = plan_queries(&sent);
    assert_eq!(next.len(), 1, "the next selection is asked about");
    let ch = src.on_event(&result(next[0].0, alone(2)));
    assert!(ch.plan);
    assert_eq!(src.take_file_plan().map(|a| a.serial), Some(2));
}

#[test]
fn the_conversation_list_is_read_to_its_end() {
    use super::tests::conv_row;
    use kourier_core::facade::{ConversationCursor, ConversationPage};

    let (mut src, sent) = source();
    src.reload();
    let page = |ids: std::ops::Range<i64>, more: bool| {
        let rows: Vec<_> = ids
            .clone()
            .map(|i| conv_row(i, &format!("c{i}"), 10_000 - i))
            .collect();
        QueryData::Conversations(ConversationPage {
            next: more.then(|| ConversationCursor {
                pinned: None,
                last_activity: 10_000 - (ids.end - 1),
                id: ConversationId(ids.end - 1),
            }),
            rows,
        })
    };
    let list_queries = |sent: &Sent| -> Vec<(QueryToken, Option<ConversationCursor>)> {
        take_queries(sent)
            .into_iter()
            .filter_map(|(t, q)| match q {
                Query::Conversations(q) => {
                    assert_eq!(q.limit, CONVERSATION_PAGE);
                    Some((t, q.after))
                }
                _ => None,
            })
            .collect()
    };
    // More chats than one page holds: the list reads on from where the page ended, and shows
    // nothing half-read.
    let first = list_queries(&sent);
    assert!(matches!(first.as_slice(), [(_, None)]));
    let ch = src.on_event(&result(first[0].0, page(1..501, true)));
    assert!(!ch.conversations);
    assert!(src.conversations(&ConversationFilter::default()).is_empty());
    let second = list_queries(&sent);
    assert_eq!(second.len(), 1);
    assert_eq!(
        second[0].1.as_ref().map(|c| c.id),
        Some(ConversationId(500))
    );
    // (A chat that moved meanwhile shows up on both pages: it is listed once.)
    let ch = src.on_event(&result(second[0].0, page(500..1201, false)));
    assert!(ch.conversations);
    let all = src.conversations(&ConversationFilter::default());
    assert_eq!(all.len(), 1200, "every conversation, not the first 500");
    let mut ids: Vec<i64> = all.iter().map(|c| c.id.0).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 1200);
    assert!(list_queries(&sent).is_empty());

    // A change while the second page is on its way: the read starts over from the top.
    src.on_event(&UiEvent::ConversationsChanged {
        ids: vec![ConversationId(7)],
    });
    let first = list_queries(&sent);
    src.on_event(&result(first[0].0, page(1..501, true)));
    let second = list_queries(&sent);
    src.on_event(&UiEvent::ConversationsChanged {
        ids: vec![ConversationId(8)],
    });
    assert!(list_queries(&sent).is_empty(), "one query in flight");
    let ch = src.on_event(&result(second[0].0, page(500..601, false)));
    assert!(!ch.conversations, "a read that was overtaken is not shown");
    let again = list_queries(&sent);
    assert!(matches!(again.as_slice(), [(_, None)]), "from the top");
    let ch = src.on_event(&result(again[0].0, page(1..41, false)));
    assert!(ch.conversations);
    assert_eq!(src.conversations(&ConversationFilter::default()).len(), 40);
    assert!(list_queries(&sent).is_empty());

    // The whole list from one query is a consistent snapshot: it is shown although a change
    // came while it was read (during a sync changes never stop, and the list must keep up),
    // and the list is read again for that change.
    let changed = |src: &mut CoreSource, id: i64| {
        src.on_event(&UiEvent::ConversationsChanged {
            ids: vec![ConversationId(id)],
        });
    };
    changed(&mut src, 7);
    let one = list_queries(&sent);
    assert!(matches!(one.as_slice(), [(_, None)]));
    changed(&mut src, 8);
    let ch = src.on_event(&result(one[0].0, page(1..31, false)));
    assert!(ch.conversations, "an overtaken one-page read is shown");
    assert_eq!(src.conversations(&ConversationFilter::default()).len(), 30);
    let again = list_queries(&sent);
    assert!(matches!(again.as_slice(), [(_, None)]), "and read again");
    let ch = src.on_event(&result(again[0].0, page(1..32, false)));
    assert!(ch.conversations);
    assert_eq!(src.conversations(&ConversationFilter::default()).len(), 31);
    assert!(list_queries(&sent).is_empty());

    // Changes that never stop do not keep a long list from showing: after `LIST_RESTARTS`
    // passes that started over, one is read to its end and shown, then read again.
    changed(&mut src, 7);
    let mut next = list_queries(&sent);
    for _ in 0..LIST_RESTARTS {
        changed(&mut src, 8);
        let ch = src.on_event(&result(next[0].0, page(1..501, true)));
        assert!(!ch.conversations);
        next = list_queries(&sent);
        assert!(matches!(next.as_slice(), [(_, None)]), "from the top");
    }
    changed(&mut src, 8);
    let ch = src.on_event(&result(next[0].0, page(1..501, true)));
    assert!(!ch.conversations, "nothing half-read");
    next = list_queries(&sent);
    assert_eq!(next.len(), 1);
    assert_eq!(
        next[0].1.as_ref().map(|c| c.id),
        Some(ConversationId(500)),
        "this pass reads on"
    );
    assert_eq!(src.conversations(&ConversationFilter::default()).len(), 31);
    let ch = src.on_event(&result(next[0].0, page(501..701, false)));
    assert!(ch.conversations);
    assert_eq!(src.conversations(&ConversationFilter::default()).len(), 700);
    next = list_queries(&sent);
    assert!(
        matches!(next.as_slice(), [(_, None)]),
        "the overtaken pass is read again from the top"
    );
    let ch = src.on_event(&result(next[0].0, page(1..41, false)));
    assert!(ch.conversations);
    assert_eq!(src.conversations(&ConversationFilter::default()).len(), 40);
    assert!(list_queries(&sent).is_empty());
}

#[test]
fn an_open_edit_history_follows_the_message() {
    let store = Store::new(&[(1, 3)]);
    let (mut src, sent) = opened(&store);
    let message = MessageId(100_003);
    let revisions = |sent: &Sent| -> Vec<QueryToken> {
        take_queries(sent)
            .into_iter()
            .filter_map(|(t, q)| matches!(q, Query::Revisions(m) if m == message).then_some(t))
            .collect()
    };
    let history = |texts: &[&str]| {
        QueryData::Revisions(Some(MessageRevisions {
            message,
            conv: CONV,
            revisions: texts
                .iter()
                .enumerate()
                .map(|(i, t)| Revision {
                    revision: u16::try_from(i).expect("few"),
                    text: Some(Text::plain(*t)),
                    at: NOW,
                })
                .collect(),
        }))
    };
    assert!(src.request_revisions(message));
    let first = revisions(&sent);
    assert_eq!(first.len(), 1);
    assert!(
        src.on_event(&result(first[0], history(&["one", "two"])))
            .revisions
    );
    assert_eq!(src.take_revisions().expect("history").rows.len(), 2);
    // Another edit of the message arrives while its history is on screen: it is read again
    // without anybody asking, and the new answer is announced like the first.
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: vec![message],
    });
    let again = revisions(&sent);
    assert_eq!(again.len(), 1, "asked again by itself");
    let ch = src.on_event(&result(again[0], history(&["one", "two", "three"])));
    assert!(ch.revisions);
    let list = src.take_revisions().expect("the longer history");
    assert_eq!(list.message, message);
    assert_eq!(list.rows.len(), 3);
    assert!(list.rows[0].current && list.rows[2].original);
    // The message is deleted meanwhile: the history says so (no rows).
    src.on_event(&UiEvent::TimelineInvalidated { conv: CONV });
    let gone = revisions(&sent);
    assert_eq!(gone.len(), 1);
    assert!(
        src.on_event(&result(gone[0], QueryData::Revisions(None)))
            .revisions
    );
    assert!(src.take_revisions().expect("answer").rows.is_empty());
    // A change in another chat is not about this message.
    src.on_event(&UiEvent::MessagesChanged {
        conv: ConversationId(2),
        ids: vec![MessageId(200_001)],
    });
    assert!(revisions(&sent).is_empty());
    // The dialog closed: nothing is asked any more.
    src.close_revisions();
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: vec![message],
    });
    assert!(revisions(&sent).is_empty(), "nobody is looking");
    // Nor after the chat was left with the dialog open.
    assert!(src.request_revisions(message));
    let asked = revisions(&sent);
    src.on_event(&result(asked[0], history(&["one"])));
    src.open_conversation(None);
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: vec![message],
    });
    assert!(revisions(&sent).is_empty());
}

/// The commands other than queries sent so far (taken).
fn commands(sent: &Sent) -> Vec<UiCommand> {
    let all = std::mem::take(&mut *sent.borrow_mut());
    let (queries, others): (Vec<UiCommand>, Vec<UiCommand>) = all
        .into_iter()
        .partition(|c| matches!(c, UiCommand::Query { .. }));
    *sent.borrow_mut() = queries;
    others
}

/// The settings queries sent so far (taken), by token.
fn settings_queries(sent: &Sent) -> Vec<QueryToken> {
    take_queries(sent)
        .into_iter()
        .filter(|(_, q)| matches!(q, Query::ExpiryPolicy))
        .map(|(t, _)| t)
        .collect()
}

#[test]
fn the_expiry_option_is_read_from_the_core_and_a_choice_reaches_it() {
    use kourier_core::facade::ExpiryPolicy;
    let (mut src, sent) = source();
    // Not known before the core said: the page offers nothing to choose from a guess.
    assert_eq!(src.expiry_policy(), None);
    // Nothing is read for the settings page at the start; it asks when it is shown.
    src.reload();
    assert!(settings_queries(&sent).is_empty());
    src.load_settings();
    let asked = settings_queries(&sent);
    assert_eq!(asked.len(), 1, "asked when the page is shown");
    let ch = src.on_event(&result(
        asked[0],
        QueryData::ExpiryPolicy(ExpiryPolicy::HideUntilConfirmed),
    ));
    assert!(ch.settings);
    assert_eq!(src.expiry_policy(), Some("hide"));
    commands(&sent);

    // A choice: shown at once, sent to the core, and read back from it.
    assert!(src.set_expiry_policy("keep"));
    assert_eq!(src.expiry_policy(), Some("keep"));
    let sent_now = commands(&sent);
    assert!(
        matches!(
            sent_now.as_slice(),
            [UiCommand::SetExpiryPolicy {
                policy: ExpiryPolicy::KeepUntilConfirmed
            }]
        ),
        "{sent_now:?}"
    );
    let confirm = settings_queries(&sent);
    assert_eq!(confirm.len(), 1, "read back behind the command");
    let ch = src.on_event(&result(
        confirm[0],
        QueryData::ExpiryPolicy(ExpiryPolicy::KeepUntilConfirmed),
    ));
    assert!(!ch.settings, "what the page shows already is no change");
    // The same choice again sends nothing.
    assert!(src.set_expiry_policy("keep"));
    assert!(commands(&sent).is_empty() && settings_queries(&sent).is_empty());
    // A name the core does not know is not sent.
    assert!(!src.set_expiry_policy("sometimes"));
    assert!(commands(&sent).is_empty());
    assert_eq!(src.expiry_policy(), Some("keep"));

    // An answer that was on its way when the user chose is about the old option: dropped.
    src.load_settings();
    let early = settings_queries(&sent);
    assert!(src.set_expiry_policy("erase"));
    let late = settings_queries(&sent);
    assert_eq!((early.len(), late.len()), (1, 1));
    let ch = src.on_event(&result(
        early[0],
        QueryData::ExpiryPolicy(ExpiryPolicy::KeepUntilConfirmed),
    ));
    assert!(!ch.settings);
    assert_eq!(
        src.expiry_policy(),
        Some("erase"),
        "not back to the old one"
    );
    src.on_event(&result(
        late[0],
        QueryData::ExpiryPolicy(ExpiryPolicy::EraseByLocalClock),
    ));
    assert_eq!(src.expiry_policy(), Some("erase"));
    commands(&sent);

    // The core could not store a choice: the option is what it was, the page shows that
    // again, and a notice says so.
    assert!(src.set_expiry_policy("hide"));
    assert_eq!(src.expiry_policy(), Some("hide"));
    let first = settings_queries(&sent);
    let ch = src.on_event(&UiEvent::ActionFailed {
        account: None,
        action: "SetExpiryPolicy",
        subject: ActionSubject::None,
        error: std::sync::Arc::new(CoreError::Stopped),
    });
    assert!(ch.notice);
    assert_eq!(src.notice(), "The setting could not be saved.");
    let again = settings_queries(&sent);
    assert_eq!(again.len(), 1, "asked what the option is now");
    // (The answer to the query sent with the choice no longer counts.)
    src.on_event(&result(
        first[0],
        QueryData::ExpiryPolicy(ExpiryPolicy::HideUntilConfirmed),
    ));
    let ch = src.on_event(&result(
        again[0],
        QueryData::ExpiryPolicy(ExpiryPolicy::EraseByLocalClock),
    ));
    assert!(ch.settings);
    assert_eq!(src.expiry_policy(), Some("erase"));
}

#[test]
fn a_choice_the_core_never_got_changes_nothing() {
    use crate::time::FixedOffset;
    use kourier_core::facade::ExpiryPolicy;
    // A core that takes commands until it stops.
    let sent: Sent = Sent::default();
    let running = std::rc::Rc::new(std::cell::Cell::new(true));
    let (s2, r2) = (sent.clone(), running.clone());
    let mut src = CoreSource::new(
        Box::new(move |c: UiCommand| {
            if r2.get() {
                s2.borrow_mut().push(c);
            }
            r2.get()
        }),
        CoreSourceConfig {
            now: Box::new(|| NOW),
            calendar: Box::new(FixedOffset(0)),
            networks: Vec::new(),
            hostname: "workstation".into(),
            demo: None,
        },
    );
    src.load_settings();
    let asked = settings_queries(&sent);
    src.on_event(&result(
        asked[0],
        QueryData::ExpiryPolicy(ExpiryPolicy::HideUntilConfirmed),
    ));
    assert_eq!(src.expiry_policy(), Some("hide"));
    // The core has stopped (the app is closing): the choice is not taken, and the page goes
    // on showing what holds, not what was clicked.
    running.set(false);
    assert!(!src.set_expiry_policy("erase"));
    assert_eq!(src.expiry_policy(), Some("hide"));
    assert!(commands(&sent).is_empty());
}

#[test]
fn an_archive_goes_where_the_chat_takes_plain_files_and_only_up_to_its_limit() {
    use kourier_model::FileLimits;
    let limits = |max_size| FileLimits {
        max_size,
        caption: true,
    };
    let mut store = Store::new(&[(1, 1), (2, 1)]);
    // Nothing known about a chat that is not open: nothing is held back in the question.
    let (src, _) = opened(&Store::new(&[(1, 1)]));
    assert_eq!(src.archive_rule(ConversationId(2)), ArchiveRule::default());
    // A chat that takes pictures only takes no archive.
    store.features.files = [(AttachmentKind::Image, limits(0))].into();
    let (src, _) = opened(&store);
    assert_eq!(
        src.archive_rule(CONV),
        ArchiveRule {
            accepted: false,
            max: None
        }
    );
    // Plain files up to 100 MiB of upload (Signal's number): that is the archive's limit.
    store.features.files = [
        (AttachmentKind::Image, limits(0)),
        (AttachmentKind::File, limits(102_224_512)),
    ]
    .into();
    let (src, _) = opened(&store);
    assert_eq!(
        src.archive_rule(CONV),
        ArchiveRule {
            accepted: true,
            max: Some(102_224_512)
        }
    );
    // No limit stated for the kind (0): none here either.
    store.features.files = [(AttachmentKind::File, limits(0))].into();
    let (src, _) = opened(&store);
    assert_eq!(
        src.archive_rule(CONV),
        ArchiveRule {
            accepted: true,
            max: None
        }
    );
}

#[test]
fn every_merge_is_told_also_of_a_chat_that_is_not_open() {
    let store = Store::new(&[(1, 1), (2, 1), (3, 1), (4, 1)]);
    let (mut src, sent) = opened(&store);
    assert!(src.take_merges().is_empty());
    // Two chats that are not open become one: nothing switches, and the composer still
    // hears of it (files may wait for the chat that went).
    let ch = src.on_event(&UiEvent::ConversationMerged {
        from: ConversationId(3),
        into: ConversationId(4),
    });
    assert!(ch.merged);
    assert_eq!(ch.switch_to, None);
    // The open chat is merged away: it switches, and that merge is told too.
    let ch = src.on_event(&UiEvent::ConversationMerged {
        from: CONV,
        into: ConversationId(2),
    });
    assert!(ch.merged);
    assert_eq!(ch.switch_to, Some(ConversationId(2)));
    assert_eq!(
        src.take_merges(),
        [
            (ConversationId(3), ConversationId(4)),
            (CONV, ConversationId(2))
        ],
        "in the order they happened"
    );
    assert!(src.take_merges().is_empty(), "told once");
    settle(&mut src, &sent, &store);
}

//! TG-3 C4a: the content map against the pinned list, the recordings' texts and the synthetic
//! `testdata/receive-contents.jsonl` (one message per line, line n = message `n << 20`). C4b: calls
//! and service kinds against `rec-incoming-call.jsonl`, `rec-basic-group-upgrade.jsonl` and the
//! synthetic `testdata/receive-service.jsonl` (the same numbering).

use std::collections::BTreeSet;
use std::time::Duration;

use kourier_model::{
    AttachmentKind, AttachmentState, Body, DisappearKind, IdentifierKind, Placeholder,
};
use serde_json::{Value, json};

use super::*;
use crate::tdapi::{Message, Raw, Receive, Update, parse_update};

const CONTENT_LIST: &str = include_str!("../../testdata/message-contents.txt");
const CONTENTS_FILE: &str = include_str!("../../testdata/receive-contents.jsonl");
const SERVICE_FILE: &str = include_str!("../../testdata/receive-service.jsonl");
const CALL_REC: &str = include_str!("../../testdata/rec-incoming-call.jsonl");
const UPGRADE_REC: &str = include_str!("../../testdata/rec-basic-group-upgrade.jsonl");
/// The receive clock of these tests.
const NOW_MS: i64 = 1_700_000_500_000;

fn me() -> Me<'static> {
    Me {
        id: 100,
        usernames: &[],
    }
}

/// Line `n` (1-based) of the synthetic file as a message.
fn line(n: usize) -> Message {
    line_of(CONTENTS_FILE, n)
}

/// Line `n` (1-based) of a synthetic file as a message.
fn line_of(file: &str, n: usize) -> Message {
    let text = file.lines().nth(n - 1).expect("the line");
    match parse_update(text) {
        Some(Update::Receive(Receive::NewMessage(m))) => {
            assert_eq!(m.id, (n as i64) << 20, "line {n} is message {n}");
            *m
        }
        other => panic!("line {n} is no new message: {other:?}"),
    }
}

fn mapped(n: usize) -> Mapped {
    message(&line(n), me(), NOW_MS)
}

fn raw(v: Value) -> Raw {
    serde_json::from_value(v).expect("a raw object")
}

fn kind_hint(body: &Body) -> (&str, Option<&str>) {
    match body {
        Body::Unsupported { kind_hint, summary } => (kind_hint.as_str(), summary.as_deref()),
        other => panic!("not unsupported: {}", other.kind_name()),
    }
}

/// The one attachment of a media body and its caption's text.
fn one_attachment(body: &Body) -> (&kourier_model::Attachment, Option<&str>) {
    match body {
        Body::Media {
            attachments,
            caption,
        } => {
            assert_eq!(attachments.len(), 1);
            (&attachments[0], caption.as_ref().map(|c| c.body.as_str()))
        }
        other => panic!("not media: {}", other.kind_name()),
    }
}

fn pointer(a: &kourier_model::Attachment) -> FilePointer {
    let blob = a.remote.as_ref().expect("a pointer");
    assert_eq!(blob.v, FILE_BLOB_VERSION);
    serde_json::from_slice(&blob.bytes).expect("tg-file v1")
}

fn meta_of(m: &Mapped) -> MsgMeta {
    let blob = m.meta.as_ref().expect("meta");
    assert_eq!(blob.v, MSG_META_VERSION);
    serde_json::from_slice(&blob.bytes).expect("tg-msg v1")
}

#[test]
fn the_table_names_every_content_of_the_pin() {
    let pinned: Vec<&str> = CONTENT_LIST
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    assert_eq!(pinned.len(), 108, "the pin's MessageContent constructors");
    let table: Vec<&str> = CONTENTS.iter().map(|(n, _)| *n).collect();
    let unique: BTreeSet<&str> = table.iter().copied().collect();
    assert_eq!(unique.len(), table.len(), "a name is listed twice");
    assert_eq!(table, pinned, "the table is the pin's list, in its order");
    for name in &pinned {
        assert!(family(name).is_some(), "{name} has no family");
    }
    assert_eq!(family("messageSyntheticFuture"), None);
    // A content of no known type is TDLib's "unsupported", never a panic.
    assert_eq!(kind_hint(&mapped(28).body), (UNKNOWN_HINT, None));
    assert_eq!(
        kind_hint(&content(&Raw::default(), me()).body),
        (UNKNOWN_HINT, None)
    );
}

/// One media line's expected attachment: (line, kind, mime, size, dims, duration ms, caption).
type MediaCase = (
    usize,
    AttachmentKind,
    &'static str,
    u64,
    Option<(u32, u32)>,
    Option<u32>,
    Option<&'static str>,
);

#[test]
fn media_has_a_pointer_and_no_download() {
    let cases: [MediaCase; 7] = [
        (
            1,
            AttachmentKind::Image,
            "image/jpeg",
            90_000,
            Some((1280, 960)),
            None,
            Some("synthetic text 1"),
        ),
        (
            2,
            AttachmentKind::Gif,
            "video/mp4",
            50_000,
            Some((480, 270)),
            Some(3_000),
            Some("synthetic text 2"),
        ),
        (
            3,
            AttachmentKind::Audio,
            "audio/mpeg",
            3_000_000,
            None,
            Some(180_000),
            Some("synthetic text 3"),
        ),
        (
            4,
            AttachmentKind::File,
            "application/pdf",
            12_345,
            None,
            None,
            None,
        ),
        (
            5,
            AttachmentKind::Video,
            "video/mp4",
            4_000_000,
            Some((1920, 1080)),
            Some(10_000),
            Some("synthetic text 5"),
        ),
        (
            6,
            AttachmentKind::Video,
            "video/mp4",
            400_000,
            Some((384, 384)),
            Some(7_000),
            None,
        ),
        (
            7,
            AttachmentKind::Voice,
            "audio/ogg",
            9_000,
            None,
            Some(4_000),
            Some("synthetic text 7"),
        ),
    ];
    for (n, kind, mime, size, dims, duration, caption) in cases {
        let m = mapped(n);
        let (a, text) = one_attachment(&m.body);
        assert_eq!(a.kind, kind, "line {n}");
        assert_eq!(a.mime, mime, "line {n}");
        assert_eq!(a.size, size, "line {n}");
        assert_eq!(a.dims, dims, "line {n}");
        assert_eq!(a.duration_ms, duration, "line {n}");
        assert_eq!(text, caption, "line {n}");
        assert_eq!(a.state, AttachmentState::Remote, "line {n}: only a pointer");
        assert!(
            a.local.is_none() && a.thumb.is_none(),
            "line {n}: nothing local"
        );
        let p = pointer(a);
        assert!(p.id.starts_with("synthetic-remote-"), "line {n}");
        assert!(p.unique_id.starts_with("synthetic-unique-"), "line {n}");
        assert_eq!(p.size, size, "line {n}");
        assert!(
            !m.view_once && m.expiry.is_none() && m.meta.is_none(),
            "line {n}"
        );
    }
    // The photo's largest size is the one kept (file 2 of line 1).
    assert_eq!(
        pointer(one_attachment(&mapped(1).body).0).id,
        "synthetic-remote-2"
    );
    // File names, never a path of ours.
    assert_eq!(
        one_attachment(&mapped(4).body).0.file_name.as_deref(),
        Some("synthetic-4.pdf")
    );
    // A sticker is a sticker body with the same pointer rule.
    match &mapped(8).body {
        Body::Sticker(a) => {
            assert_eq!(a.state, AttachmentState::Remote);
            assert_eq!(a.mime, "image/webp");
            assert_eq!(a.dims, Some((512, 512)));
            assert_eq!(pointer(a).id, "synthetic-remote-9");
        }
        other => panic!("not a sticker: {}", other.kind_name()),
    }
    // A media file without remote ids keeps no pointer.
    let no_ids = content(
        &raw(
            json!({"@type": "messageDocument", "document": {"@type": "document",
            "mime_type": "application/pdf", "document": {"@type": "file", "id": 1, "size": 5}}}),
        ),
        me(),
    );
    let (a, _) = one_attachment(&no_ids.body);
    assert_eq!(a.state, AttachmentState::Unavailable);
    assert!(a.remote.is_none());
}

/// The service kinds that are a system event or take Kourier's words (C4b); every other service
/// kind is the generic line.
const MAPPED_SERVICE: &[&str] = &[
    "messageBasicGroupChatCreate",
    "messageSupergroupChatCreate",
    "messageChatChangeTitle",
    "messageChatChangePhoto",
    "messageChatDeletePhoto",
    "messageChatAddMembers",
    "messageChatJoinByLink",
    "messageChatJoinByRequest",
    "messageChatDeleteMember",
    "messageChatSetMessageAutoDeleteTime",
    "messagePinMessage",
    "messageInvoice",
    "messagePaymentSuccessful",
    "messagePaymentSuccessfulBot",
    "messagePaymentRefunded",
];

/// A whole message of a group (chat -301) from user 202 with a content.
fn group_message(content: Value) -> Message {
    serde_json::from_value(json!({
        "@type": "message", "id": 1 << 20, "chat_id": -301, "date": 1_700_000_501,
        "sender_id": {"@type": "messageSenderUser", "user_id": 202},
        "is_outgoing": false, "can_be_saved": true, "content": content,
    }))
    .expect("a message")
}

#[test]
fn a_kind_without_kouriers_words_is_a_generic_row() {
    assert_eq!(kind_hint(&mapped(18).body), ("tg:messageDice", None));
    // Every kind of the generic family and every service kind without words of Kourier's, as a
    // whole message.
    let mut generic_rows = 0;
    for (name, fam) in CONTENTS {
        let generic = match fam {
            Family::Generic => true,
            Family::Service => !MAPPED_SERVICE.contains(name),
            _ => false,
        };
        if generic {
            generic_rows += 1;
            let m = message(&group_message(json!({"@type": name})), me(), NOW_MS);
            assert_eq!(
                kind_hint(&m.body),
                (format!("tg:{name}").as_str(), None),
                "{name}: the generic line, no summary of ours"
            );
            assert!(!m.body.is_quiet(), "{name}: a normal row");
            assert!(!m.own, "{name}");
        }
        // A content alone (an edit's) never maps a call or a service kind: the generic line.
        if matches!(fam, Family::Call | Family::GroupCall | Family::Service) {
            let c = content(&raw(json!({"@type": name})), me());
            assert_eq!(kind_hint(&c.body), (format!("tg:{name}").as_str(), None));
        }
    }
    // 66 at 1.8.68; LC-20 added `messageUnpaidMessagesAllowedByLink`, a service kind without words of ours.
    assert_eq!(generic_rows, 67, "the generic kinds of the pin");
    assert_eq!(
        kind_hint(&message(&line_of(SERVICE_FILE, 22), me(), NOW_MS).body),
        ("tg:messageScreenshotTaken", None)
    );
    // The other kinds map to their own body.
    assert_eq!(mapped(19).body, Body::Placeholder(Placeholder::Expired));
    assert_eq!(mapped(20).body, Body::text("\u{1F600}"));
}

#[test]
fn a_sticker_and_a_contact_use_kouriers_hints() {
    assert_eq!(kind_hint(&mapped(9).body), ("sticker", None));
    assert_eq!(kind_hint(&mapped(14).body), ("contact", None));
    assert_eq!(
        kind_hint(&mapped(17).body),
        ("poll", Some("synthetic text 17"))
    );
    assert_eq!(kind_hint(&mapped(21).body), ("newer-protocol", None));
    // Shown as themselves when they can be.
    match &mapped(13).body {
        Body::Contact(card) => {
            assert_eq!(card.name.as_deref(), Some("Cy Sample"));
            assert_eq!(card.identifiers.len(), 1);
            assert_eq!(card.identifiers[0].kind, IdentifierKind::Phone);
            assert_eq!(card.identifiers[0].value, "+999081736254");
        }
        other => panic!("not a contact: {}", other.kind_name()),
    }
    match &mapped(15).body {
        Body::Poll(p) => {
            assert_eq!(p.question, "synthetic text 15");
            assert_eq!(
                p.options,
                [
                    "synthetic text 151",
                    "synthetic text 152",
                    "synthetic text 153"
                ]
            );
            assert!(p.multiple);
        }
        other => panic!("not a poll: {}", other.kind_name()),
    }
    assert!(matches!(&mapped(16).body, Body::Poll(p) if !p.multiple && p.options.len() == 2));
    // Places.
    match (&mapped(10).body, &mapped(12).body) {
        (Body::Location(l), Body::Location(v)) => {
            assert_eq!((l.lat_e7, l.lon_e7), (504_500_000, 305_234_000));
            assert_eq!((l.name.as_deref(), l.address.as_deref()), (None, None));
            assert_eq!((v.lat_e7, v.lon_e7), (15_000_000, -22_500_000));
            assert_eq!(v.name.as_deref(), Some("Group 9"));
            assert_eq!(v.address.as_deref(), Some("synthetic address 1"));
        }
        other => panic!("not locations: {other:?}"),
    }
    let live = mapped(11);
    assert!(
        matches!(&live.body, Body::Location(l) if l.lat_e7 == -338_688_000 && l.lon_e7 == 1_512_093_000)
    );
    assert!(meta_of(&live).live);
    // A position outside the globe is no location.
    let off = content(
        &raw(json!({"@type": "messageLocation", "location": {"latitude": 91.0, "longitude": 0.0}})),
        me(),
    );
    assert_eq!(kind_hint(&off.body), ("tg:messageLocation", None));
}

/// TF13 (review pF4, the plan's TSC M7): a message TDLib marks restricted (`restriction_reason`)
/// is Kourier's generic line, with no text, no caption and no pointer; the fact stays in `meta`.
/// (Mutant: the content kept.)
#[test]
fn a_restricted_message_shows_no_content() {
    let m = mapped(29);
    assert_eq!(
        kind_hint(&m.body),
        ("tg:restricted", None),
        "a restricted message showed content"
    );
    assert!(meta_of(&m).restricted);
    // The same message without the restriction is a photo with a caption (the control).
    let mut open = line(29);
    open.restriction_info = None;
    let shown = message(&open, me(), NOW_MS);
    let (attachment, caption) = one_attachment(&shown.body);
    assert!(attachment.remote.is_some() && caption.is_some());
}

#[test]
fn self_destruct_immediately_is_view_once() {
    let m = mapped(22);
    assert!(m.view_once);
    assert_eq!(
        kind_hint(&m.body),
        ("view-once", None),
        "no attachment, no pointer"
    );
    assert!(m.expiry.is_none());
    // A timer: after read, started 30 - 13 s before now (12.5 s left, rounded up).
    let t = mapped(23);
    assert!(!t.view_once);
    assert!(matches!(t.body, Body::Media { .. }));
    let e = t.expiry.expect("an expiry");
    assert_eq!(e.kind, DisappearKind::AfterRead);
    assert_eq!(e.timer, Duration::from_secs(30));
    assert_eq!(e.started_at, Some(NOW_MS + 13_000 - 30_000));
    assert_eq!(e.expires_at(), Some(NOW_MS + 13_000));
    // Not opened yet: no start.
    let mut unopened = line(23);
    unopened.self_destruct_in = 0;
    assert_eq!(
        message(&unopened, me(), NOW_MS).expiry.unwrap().started_at,
        None
    );
}

#[test]
fn protected_message_carries_meta() {
    let p = mapped(24);
    let meta = meta_of(&p);
    assert!(meta.protected && meta.sensitive && !meta.restricted);
    assert!(matches!(p.body, Body::Text(_)));
    // The chat's auto-delete: from the date to now + 86,400 s (86,399.2 rounded up).
    let a = mapped(25);
    let e = a.expiry.expect("an expiry");
    assert_eq!(e.kind, DisappearKind::AfterSend);
    let date_ms = (1_700_000_400 + 25) * 1000;
    assert_eq!(e.started_at, Some(date_ms));
    assert_eq!(e.expires_at(), Some(NOW_MS + 86_400_000));
    assert!(a.meta.is_none(), "a timer is no meta fact");
    assert_eq!(meta_of(&mapped(26)).album.as_deref(), Some("77"));
    let f = meta_of(&mapped(27)).forward.expect("a forward");
    assert_eq!(
        (f.origin.as_str(), f.date),
        ("messageOriginUser", 1_700_000_100)
    );
    // An ordinary message has none.
    assert!(mapped(1).meta.is_none());
}

#[test]
fn the_recorded_texts_map_to_text_bodies() {
    // Every `message` object of every recording (updates, histories, last messages).
    fn messages<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
        match v {
            Value::Array(items) => items.iter().for_each(|i| messages(i, out)),
            Value::Object(map) => {
                if map.get("@type").and_then(Value::as_str) == Some("message") {
                    out.push(v);
                }
                map.values().for_each(|i| messages(i, out));
            }
            _ => {}
        }
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    let mut texts = 0;
    for entry in std::fs::read_dir(&dir).expect("testdata/") {
        let path = entry.expect("an entry").path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !(name.starts_with("rec-") && name.ends_with(".jsonl")) {
            continue;
        }
        for l in std::fs::read_to_string(&path).expect("read").lines() {
            let Ok(v) = serde_json::from_str::<Value>(l) else {
                continue;
            };
            let mut found = Vec::new();
            messages(&v, &mut found);
            for m in found {
                let msg: Message = serde_json::from_value(m.clone()).expect("a message");
                if msg.content.type_name() != "messageText" {
                    continue;
                }
                texts += 1;
                let mapped = message(&msg, me(), NOW_MS);
                let want = m["content"]["text"]["text"].as_str().unwrap();
                match &mapped.body {
                    Body::Text(t) => assert_eq!(t.body, want, "{name}"),
                    other => panic!("{name}: a text that is {}", other.kind_name()),
                }
                assert!(!mapped.view_once, "{name}");
            }
        }
    }
    assert!(texts >= 200, "the recordings' texts: {texts}");
}

#[test]
fn debug_prints_no_text_name_number_or_pointer() {
    let lines = CONTENTS_FILE.lines().count();
    for n in 1..=lines {
        let printed = format!("{:?}", mapped(n));
        for never in [
            "synthetic",
            "Cy",
            "Sample",
            "999081736254",
            "Group 9",
            "\u{1F600}",
            "50.45",
            "504500000",
        ] {
            assert!(!printed.contains(never), "line {n} prints a value");
        }
    }
    for n in 1..=SERVICE_FILE.lines().count() {
        let printed = format!("{:?}", mapped_service(n));
        assert!(
            !printed.contains("Group 9"),
            "service line {n} prints a title"
        );
    }
}

/// Every `message` object of a recording's phase `step` (the lines after its `kourierStep` up to
/// the next one), with the line's `@type`.
fn recorded_messages(rec: &str, step: &str) -> Vec<(String, Message)> {
    fn walk(v: &Value, out: &mut Vec<Value>) {
        match v {
            Value::Array(items) => items.iter().for_each(|i| walk(i, out)),
            Value::Object(map) => {
                if map.get("@type").and_then(Value::as_str) == Some("message") {
                    out.push(v.clone());
                }
                map.values().for_each(|i| walk(i, out));
            }
            _ => {}
        }
    }
    let mut phase = String::new();
    let mut out = Vec::new();
    for l in rec.lines() {
        let v: Value = serde_json::from_str(l).expect("a recorded line");
        let ty = v["@type"].as_str().unwrap_or_default().to_owned();
        if ty == "kourierStep" {
            phase = v["step"].as_str().unwrap_or_default().to_owned();
            continue;
        }
        if phase != step {
            continue;
        }
        let mut found = Vec::new();
        walk(&v, &mut found);
        for m in found {
            out.push((ty.clone(), serde_json::from_value(m).expect("a message")));
        }
    }
    out
}

fn tg(id: i64) -> UserId {
    UserId::from_parts("tg", &id.to_string()).expect("a tg id")
}

fn line_in(m: &Mapped) -> &CallLine {
    m.body.call_line().expect("a call line")
}

/// The call line of a whole message as the core gets it, through the contract's own validator.
fn validated(msg: &Message, m: &Mapped) -> Result<(), kourier_backend_api::ContractViolation> {
    use kourier_backend_api::{BackendEvent, Envelope, IncomingMessage};
    let conv = kourier_model::ConvKey::Direct(tg(msg.chat_id));
    let origin = origin(msg, m).expect("an origin");
    let at = i64::from(msg.date) * 1000;
    let incoming = IncomingMessage::call(
        conv,
        &msg.id.to_string(),
        origin,
        line_in(m).clone(),
        at,
        NOW_MS,
    );
    Envelope::durable(vec![BackendEvent::Message(incoming)])
        .0
        .validate()
}

#[test]
fn a_missed_incoming_call_is_the_peers_missed_line() {
    // rec-incoming-call.jsonl, step `call-missed`: the `messageCall` of the 0002 library (no
    // `updateCall`), as the new message and as the chat's last message.
    let recorded = recorded_messages(CALL_REC, "call-missed");
    let calls: Vec<_> = recorded
        .iter()
        .filter(|(_, m)| m.content.type_name() == "messageCall")
        .collect();
    assert_eq!(calls.len(), 2, "the recorded call messages used");
    assert_eq!(
        calls.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
        ["updateNewMessage", "updateChatLastMessage"]
    );
    for (_, msg) in calls {
        assert!(!msg.is_outgoing);
        let m = message(msg, me(), NOW_MS);
        let line = line_in(&m);
        assert_eq!(line.direction, CallDirection::Incoming);
        assert!(!line.group);
        assert_eq!(line.state, CallState::Missed);
        assert_eq!(line.media, Some(CallMedia::Voice));
        assert_eq!(line.by, Some(tg(201)));
        assert!(line.may_come_from_peer() && line.counts_unread());
        assert!(!m.own, "the peer says a missed call");
        assert_eq!(
            origin(msg, &m),
            Some(Origin::Remote {
                author: tg(201),
                device: None
            })
        );
        validated(msg, &m).expect("the contract takes the peer's missed line");
        assert!(!m.view_once && m.expiry.is_none());
    }
}

#[test]
fn answered_elsewhere_is_own_other_device() {
    for (n, state, media) in [
        (1, CallState::Answered, CallMedia::Video),
        (2, CallState::Declined, CallMedia::Voice),
    ] {
        let msg = line_of(SERVICE_FILE, n);
        let m = message(&msg, me(), NOW_MS);
        let line = line_in(&m);
        assert_eq!(line.direction, CallDirection::Incoming, "line {n}");
        assert_eq!(line.state, state, "line {n}");
        assert_eq!(line.media, Some(media), "line {n}");
        assert_eq!(line.by, Some(tg(202)), "line {n}: who rang");
        assert!(!line.may_come_from_peer(), "line {n}");
        assert!(m.own, "line {n}: our act on another device");
        assert_eq!(
            origin(&msg, &m),
            Some(Origin::OwnOtherDevice { device: None }),
            "line {n}"
        );
        validated(&msg, &m).expect("the contract takes our own device's line");
    }
}

#[test]
fn an_outgoing_call_from_another_device_is_ours() {
    for (n, state) in [
        (3, CallState::Answered),
        (4, CallState::Unanswered),
        (5, CallState::Declined),
    ] {
        let msg = line_of(SERVICE_FILE, n);
        let m = message(&msg, me(), NOW_MS);
        let line = line_in(&m);
        assert_eq!(line.direction, CallDirection::Outgoing, "line {n}");
        assert_eq!(line.state, state, "line {n}");
        assert_eq!(line.by, None, "line {n}: we called");
        assert!(!line.counts_unread(), "line {n}");
        assert_eq!(
            origin(&msg, &m),
            Some(Origin::OwnOtherDevice { device: None }),
            "line {n}"
        );
        validated(&msg, &m).expect("line {n}");
    }
}

#[test]
fn group_calls_are_shown_never_joined() {
    // A video chat started is a group line nobody rang us for; the other group-call kinds take
    // Kourier's words.
    let msg = line_of(SERVICE_FILE, 16);
    let m = message(&msg, me(), NOW_MS);
    let line = line_in(&m);
    assert!(line.group);
    assert_eq!(line.state, CallState::Started);
    assert_eq!(line.by, Some(tg(202)));
    assert!(line.may_come_from_peer());
    assert_eq!(kind_hint(&mapped_service(17).body), ("group-call", None));
    assert_eq!(kind_hint(&mapped_service(20).body), ("group-call", None));
    for (name, fam) in CONTENTS {
        if *fam == Family::GroupCall && *name != "messageVideoChatStarted" {
            let m = message(&group_message(json!({"@type": name})), me(), NOW_MS);
            assert_eq!(kind_hint(&m.body), ("group-call", None), "{name}");
        }
    }
}

fn mapped_service(n: usize) -> Mapped {
    message(&line_of(SERVICE_FILE, n), me(), NOW_MS)
}

#[test]
fn the_upgrade_contents_are_notices_in_both_chats() {
    // rec-basic-group-upgrade.jsonl, step `upgrade`: the basic group's `messageChatUpgradeTo` and
    // the new supergroup's `messageChatUpgradeFrom`, each as the new and as the last message.
    let recorded = recorded_messages(UPGRADE_REC, "upgrade");
    let mut seen = Vec::new();
    for (ty, msg) in &recorded {
        let name = msg.content.type_name();
        if !name.starts_with("messageChatUpgrade") {
            continue;
        }
        seen.push((ty.as_str(), name.to_owned(), msg.chat_id));
        let m = message(msg, me(), NOW_MS);
        assert_eq!(
            kind_hint(&m.body),
            (format!("tg:{name}").as_str(), None),
            "{name}: a normal row with the generic line"
        );
        assert!(!m.body.is_quiet() && !m.own && !m.view_once);
        assert_eq!(
            origin(msg, &m),
            Some(Origin::OwnOtherDevice { device: None }),
            "we upgraded it, on the phone"
        );
    }
    assert_eq!(
        seen,
        [
            ("updateNewMessage", "messageChatUpgradeTo".to_owned(), -301),
            (
                "updateChatLastMessage",
                "messageChatUpgradeTo".to_owned(),
                -301
            ),
            (
                "updateNewMessage",
                "messageChatUpgradeFrom".to_owned(),
                -1_000_000_000_404
            ),
            (
                "updateChatLastMessage",
                "messageChatUpgradeFrom".to_owned(),
                -1_000_000_000_404
            ),
        ],
        "the recorded upgrade messages used"
    );
}

#[test]
fn a_pin_uses_kouriers_words() {
    assert_eq!(kind_hint(&mapped_service(6).body), ("pin", None));
    assert_eq!(kind_hint(&mapped_service(18).body), ("payment", None));
    for name in [
        "messageInvoice",
        "messagePaymentSuccessfulBot",
        "messagePaymentRefunded",
    ] {
        let m = message(&group_message(json!({"@type": name})), me(), NOW_MS);
        assert_eq!(kind_hint(&m.body), ("payment", None), "{name}");
    }
}

#[test]
fn member_changes_are_system_events() {
    fn event(n: usize) -> SystemEvent {
        match mapped_service(n).body {
            Body::System(e) => e,
            other => panic!("line {n} is {}", other.kind_name()),
        }
    }
    assert_eq!(
        event(7),
        SystemEvent::MembersAdded {
            by: Some(tg(202)),
            users: vec![tg(203), tg(204)]
        }
    );
    assert_eq!(event(8), SystemEvent::Joined, "our own join");
    assert_eq!(
        event(9),
        SystemEvent::MembersAdded {
            by: Some(tg(203)),
            users: vec![tg(203)]
        },
        "a join by link: the model's 'joined' wording"
    );
    assert_eq!(event(10), SystemEvent::MemberLeft { user: tg(203) });
    assert_eq!(
        event(11),
        SystemEvent::MembersRemoved {
            by: Some(tg(202)),
            users: vec![tg(204)]
        }
    );
    assert_eq!(
        event(12),
        SystemEvent::TitleChanged {
            by: Some(tg(202)),
            title: "Group 9".to_owned()
        }
    );
    assert_eq!(
        event(13),
        SystemEvent::TimerChanged {
            by: None,
            timer: Some(Duration::from_secs(86_400))
        },
        "we set it: the model's 'You'"
    );
    assert_eq!(
        event(14),
        SystemEvent::TimerChanged {
            by: Some(tg(202)),
            timer: None
        }
    );
    assert_eq!(event(15), SystemEvent::GroupCreated { by: None });
    assert_eq!(event(19), SystemEvent::AvatarChanged { by: Some(tg(202)) });
    assert_eq!(event(21), SystemEvent::Joined);
    for n in [7, 9, 10, 11, 12, 14, 19] {
        let msg = line_of(SERVICE_FILE, n);
        let m = mapped_service(n);
        assert!(
            !m.body.is_quiet() && !m.own,
            "line {n}: a loud group change"
        );
        assert!(matches!(origin(&msg, &m), Some(Origin::Remote { .. })));
    }
}

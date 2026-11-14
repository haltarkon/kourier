use kourier_core::facade::{
    ExportFileRefusal, ExportPlan, ExportStamp, ExportVerdict, LeftOut, MessageActions, MessageRow,
    OriginKind, PlannedFile, PlannedMessage, QuoteState, ReactionGroup, ReplyPreview, Revision,
    check_entry_name,
};
use kourier_model::{
    Attachment, AttachmentId, AttachmentKind, Body, CallDirection, CallLine, CallMedia, CallState,
    ContactCard, ConvKind, ConversationId, Deletion, Location, MessageId, Placeholder, Poll,
    RangeKind, SystemEvent, Text, TextRange, UserId,
};

use super::*;
use crate::names::NameMap;

const T0: i64 = 1_767_225_600_000; // 2026-01-01 00:00 UTC

fn user(v: &str) -> UserId {
    UserId::parse(&format!("aci:{v}")).unwrap()
}

fn row(id: i64, author: Option<UserId>, body: Body) -> MessageRow {
    MessageRow {
        id: MessageId(id),
        sort_key: id,
        origin: if author.is_some() {
            OriginKind::Remote
        } else {
            OriginKind::OwnThisDevice
        },
        author,
        author_name: None,
        author_avatar: None,
        device: None,
        body,
        reply: None,
        sent_at: T0 + id * 60_000,
        server_at: None,
        received_at: T0,
        revision: 0,
        edited_at: None,
        deleted: None,
        out_state: None,
        read_state: None,
        reactions: Vec::new(),
        expiry: None,
        view_once: false,
        history: false,
        mentions_me: false,
        outbox: None,
        actions: MessageActions::default(),
    }
}

fn planned(row: MessageRow) -> PlannedMessage {
    PlannedMessage {
        stamp: ExportStamp {
            id: row.id,
            deleted: row.deleted.is_some(),
            revision: row.revision,
        },
        row,
        quote: None,
        verdict: ExportVerdict::Included,
        files: Vec::new(),
        revisions: Vec::new(),
    }
}

fn plan(messages: Vec<PlannedMessage>, members: u32) -> ExportPlan {
    ExportPlan {
        conv: ConversationId(1),
        chat: Ok(()),
        members,
        messages,
        now: T0 + 86_400_000,
    }
}

fn file(id: i64, kind: AttachmentKind, name: Option<&str>, sniffed: Option<&str>) -> PlannedFile {
    PlannedFile {
        attachment: AttachmentId(id),
        kind,
        mime: sniffed.unwrap_or("application/octet-stream").to_owned(),
        size: 1000,
        file_name: name.map(str::to_owned),
        sniffed: sniffed.map(str::to_owned),
        accepted: Ok(()),
    }
}

fn att(id: i64, kind: AttachmentKind, name: Option<&str>) -> Attachment {
    Attachment {
        id: Some(AttachmentId(id)),
        kind,
        file_name: name.map(str::to_owned),
        size: 1000,
        ..Attachment::default()
    }
}

fn media(
    id: i64,
    author: Option<UserId>,
    kind: AttachmentKind,
    name: &str,
    sniffed: &str,
) -> PlannedMessage {
    let mut pm = planned(row(
        id,
        author,
        Body::Media {
            attachments: vec![att(id, kind, Some(name))],
            caption: None,
        },
    ));
    pm.files = vec![file(id, kind, Some(name), Some(sniffed))];
    pm
}

fn input(plan: ExportPlan, ticks: Ticks, names: &NameMap, own: &str) -> BuildInput {
    BuildInput {
        names: NameSnapshot::from_plan(&plan, names),
        offsets: OffsetTable::fixed(7200),
        plan,
        ticks,
        own_name: own.to_owned(),
        chat_title: "Club".to_owned(),
        chat_kind: ConvKind::Group,
        peer: None,
        own: Vec::new(),
        network: "Signal".to_owned(),
        time_zone: "+02:00".to_owned(),
        exporter: "Kourier test".to_owned(),
        salt: [7; 32],
        language: "en".to_owned(),
    }
}

fn all() -> Ticks {
    Ticks {
        pictures: true,
        videos: true,
        other_files: true,
        disappearing: false,
    }
}

fn names() -> NameMap {
    let mut n = NameMap::new();
    n.insert(user("ada"), "Ada");
    n.insert(user("bob"), "Bob");
    n
}

fn doc<'a>(b: &'a Built, entry: &str) -> &'a str {
    std::str::from_utf8(&b.documents.iter().find(|d| d.entry == entry).unwrap().bytes).unwrap()
}

fn lines(b: &Built) -> Vec<serde_json::Value> {
    doc(b, "messages.jsonl")
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn a_text_record_is_written_as_expected() {
    let p = plan(
        vec![planned(row(1, Some(user("ada")), Body::text("hello")))],
        3,
    );
    let b = build(input(p, all(), &names(), "Me"));
    assert_eq!(
        doc(&b, "messages.jsonl"),
        format!(
            "{{\"id\":\"m1\",\"time\":\"2026-01-01T02:01:00+02:00\",\"ts\":{},\"author\":\"p1\",\"kind\":\"text\",\"text\":{{\"plain\":\"hello\"}}}}\n",
            T0 + 60_000
        )
    );
    assert_eq!(
        b.messages.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![MessageId(1)]
    );
    let names: Vec<&str> = b.documents.iter().map(|d| d.entry.as_str()).collect();
    assert_eq!(names, ["index.html", "messages.jsonl"]);
}

#[test]
fn every_body_kind_has_its_record() {
    let ada = user("ada");
    let mut deleted = row(9, Some(ada.clone()), Body::text("secret"));
    deleted.deleted = Some(Deletion::ForEveryone {
        by: Some(ada.clone()),
    });
    let call = CallLine {
        direction: CallDirection::Incoming,
        group: false,
        state: CallState::Missed,
        media: Some(CallMedia::Voice),
        media_at: 1,
        by: Some(ada.clone()),
        by_at: 1,
    };
    let msgs = vec![
        planned(row(1, Some(ada.clone()), Body::text("t"))),
        media(2, None, AttachmentKind::File, "a.txt", "text/plain"),
        planned(row(
            3,
            None,
            Body::Sticker(Box::new(att(3, AttachmentKind::Image, None))),
        )),
        planned(row(
            4,
            Some(ada.clone()),
            Body::Contact(ContactCard {
                name: Some("Cy".into()),
                ..ContactCard::default()
            }),
        )),
        planned(row(
            5,
            None,
            Body::Location(Location {
                lat_e7: 500_000_000,
                lon_e7: 300_000_000,
                name: Some("Hill".into()),
                address: None,
            }),
        )),
        planned(row(
            6,
            None,
            Body::Poll(Poll {
                question: "Q?".into(),
                options: vec!["a".into(), "b".into()],
                multiple: false,
            }),
        )),
        planned(row(
            7,
            None,
            Body::System(SystemEvent::MembersAdded {
                by: Some(ada.clone()),
                users: vec![user("bob")],
            }),
        )),
        planned(row(8, None, Body::System(SystemEvent::Call(call)))),
        planned(deleted),
        planned(row(
            10,
            Some(ada.clone()),
            Body::Placeholder(Placeholder::Expired),
        )),
        planned(row(
            11,
            Some(ada),
            Body::Unsupported {
                kind_hint: "payment".into(),
                summary: Some("5 EUR".into()),
            },
        )),
    ];
    let b = build(input(plan(msgs, 3), all(), &names(), "Me"));
    let v = lines(&b);
    let kinds: Vec<&str> = v.iter().map(|r| r["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        [
            "text",
            "media",
            "sticker",
            "contact",
            "location",
            "poll",
            "notice",
            "call",
            "deleted",
            "placeholder",
            "unsupported"
        ]
    );
    assert_eq!(v[1]["files"][0]["state"], "included");
    assert_eq!(v[1]["files"][0]["path"], "files/a.txt");
    assert_eq!(v[1]["author"], "me");
    assert_eq!(v[6]["notice"]["event"], "members_added");
    assert!(v[6].get("author").is_none());
    assert_eq!(v[7]["call"]["state"], "missed");
    assert_eq!(v[8]["deleted"]["for"], "everyone");
    assert!(v[8].get("text").is_none());
    assert_eq!(v[10]["unsupported"]["kind"], "payment");
    assert!(!doc(&b, "messages.jsonl").contains("5 EUR"));
    assert_eq!(b.head.0["format"], "kourier-chat-export");
    assert_eq!(b.head.0["counts"]["messages"], 11);
}

#[test]
fn styles_mentions_links_edits_reactions_and_quotes() {
    let ada = user("ada");
    let body = "a spoiler \u{fffc} see http://x.example/p ok".to_owned();
    let start = body.find("spoiler").unwrap() as u32;
    let ment = body.find('\u{fffc}').unwrap() as u32;
    let text = Text {
        body,
        ranges: vec![
            TextRange {
                start,
                len: 7,
                kind: RangeKind::Spoiler,
            },
            TextRange {
                start,
                len: 7,
                kind: RangeKind::Bold,
            },
            TextRange {
                start: ment,
                len: 3,
                kind: RangeKind::Mention(user("bob")),
            },
        ],
    };
    let mut r = row(1, Some(ada.clone()), Body::Text(text));
    r.edited_at = Some(T0 + 120_000);
    r.reactions = vec![ReactionGroup {
        emoji: "👍".into(),
        users: vec![None, Some(user("bob"))],
    }];
    r.reply = Some(ReplyPreview {
        target: Some(MessageId(0)),
        state: QuoteState::Known,
        author: Some(user("bob")),
        own: false,
        author_name: None,
        snapshot: None,
        text: Some(Text::plain("q".repeat(300))),
        attachment: None,
        file_name: None,
        thumb: None,
    });
    let mut pm = planned(r);
    pm.revisions = vec![
        Revision {
            revision: 0,
            text: Some(Text::plain("first")),
            at: T0 + 60_000,
        },
        Revision {
            revision: 1,
            text: Some(Text::plain("second")),
            at: T0 + 120_000,
        },
    ];
    let b = build(input(plan(vec![pm], 3), all(), &names(), "Me"));
    let v = &lines(&b)[0];
    let runs = v["text"]["runs"].as_array().unwrap();
    assert_eq!(
        v["text"]["plain"],
        "a spoiler @Bob see http://x.example/p ok"
    );
    assert!(
        runs.iter()
            .any(|r| r["t"] == "spoiler" && r["s"] == serde_json::json!(["bold", "spoiler"]))
    );
    assert!(runs.iter().any(|r| r["mention"] == "p2"));
    assert!(runs.iter().any(|r| r["link"] == "http://x.example/p"));
    assert_eq!(v["edits"][0]["plain"], "first");
    assert_eq!(v["edits"].as_array().unwrap().len(), 1);
    assert_eq!(v["reactions"][0]["by"], serde_json::json!(["me", "p2"]));
    assert_eq!(v["reply_to"]["author"], "p2");
    assert_eq!(
        v["reply_to"]["excerpt"].as_str().unwrap().chars().count(),
        201
    );
    // spoilers are written out in the page too
    assert!(doc(&b, "index.html").contains("<span class=\"spoiler\">"));
}

#[test]
fn a_long_text_is_written_whole() {
    let long = "word ".repeat(5000);
    let p = plan(
        vec![planned(row(1, Some(user("ada")), Body::text(long.clone())))],
        3,
    );
    let b = build(input(p, all(), &names(), "Me"));
    assert_eq!(lines(&b)[0]["text"]["plain"], long);
}

#[test]
fn a_name_that_is_a_number_is_never_written() {
    let mut n = NameMap::new();
    n.insert(user("ada"), "+49 151 1234567");
    let phone = UserId::parse("e164:+4915112345678").unwrap();
    let p = plan(
        vec![
            planned(row(1, Some(user("ada")), Body::text("hi"))),
            planned(row(2, Some(phone), Body::text("yo"))),
        ],
        3,
    );
    let b = build(input(p, all(), &n, "Me"));
    let all_text = format!(
        "{}{}{:?}",
        doc(&b, "messages.jsonl"),
        doc(&b, "index.html"),
        serde_json::Value::Object(b.head.0.clone())
    );
    for needle in ["1234567", "+49", "4915112345678", "151"] {
        assert!(!all_text.contains(needle), "{needle}");
    }
    let people = b.head.0["people"].as_array().unwrap();
    assert!(people[1]["pseudonym"] == true && people[2]["pseudonym"] == true);
}

#[test]
fn pseudonyms_start_at_one_hundred_members() {
    let mk = |members| {
        let p = plan(
            vec![planned(row(1, Some(user("ada")), Body::text("hi")))],
            members,
        );
        build(input(p, all(), &names(), "Me"))
    };
    let small = mk(99);
    assert_eq!(small.head.0["people"][1]["name"], "Ada");
    assert_eq!(small.head.0["options"]["people"], "names");
    let big = mk(100);
    let name = big.head.0["people"][1]["name"].as_str().unwrap();
    assert!(name.starts_with("p_") && name.len() == 8, "{name}");
    assert_eq!(big.head.0["options"]["people"], "pseudonyms");
    assert!(!doc(&big, "index.html").contains("Ada"));
    // stable within an export
    assert_eq!(
        big.head.0["people"][1]["name"],
        mk(100).head.0["people"][1]["name"]
    );
}

#[test]
fn entry_names_are_unique_and_accepted() {
    let long = "x".repeat(300);
    let raw: Vec<Option<&str>> = vec![
        Some("a.txt"),
        Some("A.TXT"),
        Some("a.txt"),
        Some("a/b.txt"),
        Some(".."),
        Some("..\\x"),
        Some("e\u{202e}vil.txt"),
        Some(&long),
        Some(""),
        None,
        None,
        Some("CON"),
        Some("x.html"),
        // The lead's round: names the writer refuses even after the cleaning, and the joiners
        // it keeps.
        Some("CON.txt"),
        Some("nul"),
        Some("COM1.png"),
        Some("lpt9"),
        Some("a:b*c?d\"e<f>g|h"),
        Some("trailing dot."),
        Some("trailing space "),
        Some("\u{202E}\u{2066}\u{200B}\u{200E}\u{FEFF}"),
        Some(":::"),
        Some("می\u{200C}خواهم.txt"),
        Some("\u{1F468}\u{200D}\u{1F469}.png"),
    ];
    let mut set = Entries::new();
    let mut seen = std::collections::HashSet::new();
    for n in raw {
        let e = set.next(n, "image/png", Some("image/png"));
        assert!(check_entry_name(&e).is_ok(), "{e}");
        assert!(e.starts_with("files/") && !e["files/".len()..].contains('/'));
        assert!(seen.insert(e.to_lowercase()), "{e}");
    }
}

#[test]
fn the_joiners_stay_in_a_name_and_a_name_the_cleaning_cannot_save_falls_back_to_the_generic_one() {
    let mut set = Entries::new();
    let persian = set.next(Some("می\u{200C}خواهم.txt"), "text/plain", None);
    assert!(persian.contains('\u{200C}'), "ZWNJ kept: {persian}");
    assert!(check_entry_name(&persian).is_ok());
    let family = set.next(Some("\u{1F468}\u{200D}\u{1F469}.png"), "image/png", None);
    assert!(family.contains('\u{200D}'), "ZWJ kept: {family}");
    assert!(check_entry_name(&family).is_ok());
    // Reserved names and names of nothing but refused characters: whatever the cleaning makes of
    // them, an entry comes back that the writer accepts (the generic stem and the content's
    // extension when the cleaning could not save the name).
    for hostile in ["CON.txt", "nul", ":::", "\u{202E}\u{200B}", "lpt1.png"] {
        let e = set.next(Some(hostile), "image/png", Some("image/png"));
        assert!(check_entry_name(&e).is_ok(), "{hostile:?} -> {e}");
    }
    // The numbering has a way out: past the cap the names stay unique and valid.
    let mut many = Entries::new();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..40 {
        let e = many.next(Some("x.png"), "image/png", None);
        assert!(seen.insert(e));
    }
}

#[test]
fn hostile_content_leaves_the_page_inert() {
    let evil =
        "<script>alert(1)</script> \"><img src=x onerror=alert(1)> http://evil.example/p.png";
    let body = "click here".to_owned();
    let linked = |url: &str| Text {
        body: body.clone(),
        ranges: vec![TextRange {
            start: 0,
            len: 10,
            kind: RangeKind::Link(url.into()),
        }],
    };
    let mut svg = media(
        5,
        Some(user("ada")),
        AttachmentKind::Image,
        "x.svg",
        "image/svg+xml",
    );
    svg.files[0].mime = "image/svg+xml".into();
    let mut htm = media(
        6,
        Some(user("ada")),
        AttachmentKind::File,
        "x.html",
        "text/html",
    );
    htm.files[0].mime = "text/html".into();
    let pic = media(
        7,
        Some(user("ada")),
        AttachmentKind::Image,
        "a b\"<.png",
        "image/png",
    );
    let mut n = NameMap::new();
    n.insert(user("ada"), "<b onmouseover=x>Ada</b>");
    let msgs = vec![
        planned(row(1, Some(user("ada")), Body::text(evil))),
        planned(row(
            2,
            Some(user("ada")),
            Body::Text(linked("javascript:alert(1)")),
        )),
        planned(row(
            3,
            Some(user("ada")),
            Body::Text(linked("data:text/html,<script>x</script>")),
        )),
        planned(row(
            4,
            Some(user("ada")),
            Body::Text(linked("https://ok.example/a?b=1&c=\"2\"")),
        )),
        svg,
        htm,
        pic,
    ];
    let b = build(input(plan(msgs, 3), all(), &n, "Me"));
    let page = doc(&b, "index.html");
    let lower = page.to_lowercase();
    assert!(!lower.contains("<script"));
    assert!(!lower.contains("javascript:") && !lower.contains("data:text"));
    assert!(page.contains("http-equiv=\"Content-Security-Policy\""));
    assert!(page.contains("<meta name=\"referrer\" content=\"no-referrer\">"));
    let mut refs = 0;
    for tag in page.split('<').skip(1) {
        let tag = tag.split('>').next().unwrap();
        // no event attribute in any tag
        let words: Vec<&str> = tag.split_whitespace().collect();
        assert!(
            !words
                .iter()
                .skip(1)
                .any(|w| w.to_lowercase().starts_with("on")),
            "{tag}"
        );
        assert!(!tag.contains("style=") || tag.starts_with("style"), "{tag}");
        for key in ["src=\"", "href=\""] {
            if let Some(i) = tag.find(key) {
                refs += 1;
                let v = &tag[i + key.len()..];
                assert!(
                    v.starts_with("files/")
                        || v.starts_with("http://")
                        || v.starts_with("https://"),
                    "{tag}"
                );
                if v.starts_with("http") {
                    assert!(tag.contains("rel=\"noopener noreferrer\""), "{tag}");
                }
            }
        }
    }
    assert!(refs >= 3);
    // the safe link stays a link, the SVG and the HTML file are links, never images
    assert!(page.contains("href=\"https://ok.example/a?b=1&amp;c=&quot;2&quot;\""));
    // the SVG and the HTML file are never images and never linked: no `href` to them
    assert!(!page.contains("<img src=\"files/x.svg") && !page.contains("href=\"files/x.svg"));
    assert!(!page.contains("<img src=\"files/x.html") && !page.contains("href=\"files/x.html"));
    assert!(page.contains("files/x.html") && page.contains("files/x.svg"));
    assert!(
        page.contains("<img src=\"files/a%20b%22%3C.png\"")
            || page.contains("<img src=\"files/a%20b")
    );
    // the remote picture URL in text is text, not an image source
    assert!(!lower.contains("src=\"http"));
}

#[test]
fn the_lists_follow_the_documents_for_every_tick_combination() {
    let ada = Some(user("ada"));
    for bits in 0..16u8 {
        let t = Ticks {
            pictures: bits & 1 != 0,
            videos: bits & 2 != 0,
            other_files: bits & 4 != 0,
            disappearing: bits & 8 != 0,
        };
        let mut tick_msg = media(4, ada.clone(), AttachmentKind::Image, "d.png", "image/png");
        tick_msg.verdict = ExportVerdict::NeedsTick;
        let mut once = media(5, ada.clone(), AttachmentKind::Image, "o.png", "image/png");
        once.verdict = ExportVerdict::LeftOut(LeftOut::ViewOnce);
        let mut here = media(6, ada.clone(), AttachmentKind::File, "n.bin", "image/png");
        here.files[0].accepted = Err(ExportFileRefusal::NotHere);
        let msgs = vec![
            media(1, ada.clone(), AttachmentKind::Image, "p.png", "image/png"),
            media(2, ada.clone(), AttachmentKind::Video, "v.mp4", "video/mp4"),
            media(
                3,
                ada.clone(),
                AttachmentKind::File,
                "f.pdf",
                "application/pdf",
            ),
            tick_msg,
            once,
            here,
        ];
        let b = build(input(plan(msgs, 3), t, &names(), "Me"));
        let rows = lines(&b);
        let ids: Vec<String> = rows
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_owned())
            .collect();
        let want: Vec<String> = b.messages.iter().map(|m| format!("m{}", m.id.0)).collect();
        assert_eq!(ids, want, "{bits}");
        assert_eq!(
            b.messages.iter().any(|m| m.id == MessageId(4)),
            t.disappearing,
            "{bits}"
        );
        assert!(!b.messages.iter().any(|m| m.id == MessageId(5)));
        let mut paths: Vec<String> = rows
            .iter()
            .flat_map(|r| r["files"].as_array().cloned().unwrap_or_default())
            .filter_map(|f| f["path"].as_str().map(str::to_owned))
            .collect();
        let mut entries: Vec<String> = b.files.iter().map(|f| f.entry.clone()).collect();
        paths.sort();
        entries.sort();
        assert_eq!(paths, entries, "{bits}");
        let page = doc(&b, "index.html");
        let json = doc(&b, "messages.jsonl");
        assert!(!page.contains("o.png") && !json.contains("o.png"));
        assert_eq!(entries.iter().any(|e| e.ends_with("p.png")), t.pictures);
        assert_eq!(entries.iter().any(|e| e.ends_with("v.mp4")), t.videos);
        assert_eq!(entries.iter().any(|e| e.ends_with("f.pdf")), t.other_files);
        assert_eq!(b.counts.files_not_on_this_computer, 1);
        assert_eq!(b.counts.left_out_view_once, 1);
        assert_eq!(b.counts.left_out_disappearing, u32::from(!t.disappearing));
    }
}

/// Everything of a message the export leaves out is absent from both documents, and a quote of
/// a disappearing message shows its content only with the tick.
#[test]
fn a_left_out_row_and_a_quote_of_a_disappearing_original_show_nothing_without_the_tick() {
    const MARK: &str = "VIEWONCE-MARKER-4711";
    const QUOTED: &str = "QUOTED-WORDS-0815";
    let ada = Some(user("ada"));
    // The plan strips a left-out row; a document builder must not carry one even if it came
    // with content (the markers sit in the body, the file name and the quote).
    let mut once = media(5, ada.clone(), AttachmentKind::Image, MARK, "image/png");
    once.row.body = Body::Media {
        attachments: vec![att(5, AttachmentKind::Image, Some(MARK))],
        caption: Some(Text::plain(MARK)),
    };
    once.verdict = ExportVerdict::LeftOut(LeftOut::ViewOnce);
    let quoting = |id: i64, verdict: Option<ExportVerdict>| {
        let mut r = row(id, ada.clone(), Body::text("a reply"));
        r.reply = Some(ReplyPreview {
            target: None,
            state: QuoteState::Missing,
            author: Some(user("ada")),
            own: false,
            author_name: None,
            snapshot: None,
            text: Some(Text::plain(QUOTED)),
            attachment: None,
            file_name: None,
            thumb: None,
        });
        let mut pm = planned(r);
        pm.quote = verdict;
        pm
    };
    let msgs = || {
        vec![
            once.clone(),
            quoting(6, Some(ExportVerdict::NeedsTick)),
            quoting(7, Some(ExportVerdict::LeftOut(LeftOut::ViewOnce))),
        ]
    };
    for tick in [false, true] {
        let mut t = all();
        t.disappearing = tick;
        let b = build(input(plan(msgs(), 3), t, &names(), "Me"));
        let (page, json) = (doc(&b, "index.html"), doc(&b, "messages.jsonl"));
        for text in [page, json] {
            assert!(!text.contains(MARK), "a view-once marker is in a document");
        }
        // A left-out quote never shows its text, with or without the tick.
        let rows = lines(&b);
        let excerpt = |id: &str| {
            rows.iter()
                .find(|r| r["id"] == id)
                .map(|r| r["reply_to"]["excerpt"].as_str().unwrap().to_owned())
        };
        assert!(excerpt("m7").is_some_and(|e| !e.contains(QUOTED)));
        // The quote of a disappearing message: its text only with the tick.
        let shown = excerpt("m6").unwrap();
        assert_eq!(shown.contains(QUOTED), tick, "tick {tick}: {shown}");
        assert_eq!(page.contains(QUOTED), tick, "tick {tick}");
        assert_eq!(json.contains(QUOTED), tick, "tick {tick}");
    }
}

#[test]
fn the_dialog_numbers_count_by_kind_and_reason() {
    let ada = Some(user("ada"));
    let mut tick = media(4, ada.clone(), AttachmentKind::Image, "d.png", "image/png");
    tick.verdict = ExportVerdict::NeedsTick;
    let mut gone = planned(row(5, ada.clone(), Body::text("g")));
    gone.verdict = ExportVerdict::LeftOut(LeftOut::Gone);
    let mut missing = media(6, ada.clone(), AttachmentKind::Video, "m.mp4", "video/mp4");
    missing.files[0].accepted = Err(ExportFileRefusal::NotHere);
    let sticker = {
        let mut m = planned(row(
            7,
            ada.clone(),
            Body::Sticker(Box::new(att(7, AttachmentKind::Image, None))),
        ));
        m.files = vec![file(7, AttachmentKind::Image, None, Some("image/webp"))];
        m
    };
    let p = plan(
        vec![
            media(1, ada.clone(), AttachmentKind::Image, "p.png", "image/png"),
            media(2, ada.clone(), AttachmentKind::Video, "v.mp4", "video/mp4"),
            media(3, ada, AttachmentKind::Audio, "a.mp3", "audio/mpeg"),
            tick,
            gone,
            missing,
            sticker,
        ],
        3,
    );
    let n = numbers(&p);
    assert_eq!(
        (
            n.included,
            n.disappearing,
            n.left_out_gone,
            n.left_out_view_once
        ),
        (5, 1, 1, 0)
    );
    assert_eq!(
        n.pictures,
        ClassCount {
            count: 2,
            bytes: 2000
        }
    );
    assert_eq!(n.videos.count, 1);
    assert_eq!(n.other_files.count, 1);
    assert_eq!((n.not_here, n.disappearing_files()), (1, 1));
    assert!(n.has_disappearing());
}

#[test]
fn building_five_thousand_messages_prints_its_time() {
    let n = NameMap::new();
    let msgs: Vec<PlannedMessage> = (1..=5000)
        .map(|i| {
            let a = Some(user(if i % 3 == 0 { "ada" } else { "bob" }));
            if i % 2 == 0 {
                media(i, a, AttachmentKind::Image, "photo.jpg", "image/jpeg")
            } else {
                planned(row(
                    i,
                    a,
                    Body::text(format!("message number {i} with some words in it")),
                ))
            }
        })
        .collect();
    let p = plan(msgs, 5);
    let started = std::time::Instant::now();
    let b = build(input(p, all(), &n, "Me"));
    println!(
        "EXPORT BUILD 5000 messages (2500 files): {:?}",
        started.elapsed()
    );
    assert_eq!(b.messages.len(), 5000);
    assert_eq!(b.files.len(), 2500);
}

#[test]
fn the_build_runs_off_the_qt_thread() {
    let p = plan(
        vec![planned(row(1, Some(user("ada")), Body::text("hi")))],
        3,
    );
    let inp = input(p, all(), &names(), "Me");
    fn assert_send<T: Send>(_: &T) {}
    assert_send(&inp);
    let b = std::thread::spawn(move || build(inp)).join().unwrap();
    assert_eq!(b.messages.len(), 1);
}

#[test]
fn many_equal_names_are_numbered_in_order() {
    let mut set = Entries::new();
    let first = set.next(Some("photo (3).jpg"), "image/jpeg", None);
    assert_eq!(first, "files/photo (3).jpg");
    let mut seen = std::collections::HashSet::new();
    seen.insert(first.to_lowercase());
    let mut got = Vec::new();
    for _ in 0..2500 {
        let e = set.next(Some("photo.jpg"), "image/jpeg", None);
        assert!(check_entry_name(&e).is_ok(), "{e}");
        assert!(seen.insert(e.to_lowercase()), "{e}");
        got.push(e);
    }
    // in order, skipping the number a sender's own file already took
    assert_eq!(got[0], "files/photo.jpg");
    assert_eq!(got[1], "files/photo (1).jpg");
    assert_eq!(got[2], "files/photo (2).jpg");
    assert_eq!(got[3], "files/photo (4).jpg");
    assert_eq!(got[2499], "files/photo (2500).jpg");
    // a different case of the same name goes on counting
    assert_eq!(
        set.next(Some("PHOTO.JPG"), "image/jpeg", None),
        "files/PHOTO (2501).JPG"
    );
}

// ---------------------------------------------------------------------------------------------
// The documents fix round (D1-D16).
// ---------------------------------------------------------------------------------------------

fn everything(b: &Built) -> String {
    format!(
        "{}{}{}",
        doc(b, "messages.jsonl"),
        doc(b, "index.html"),
        serde_json::Value::Object(b.head.0.clone())
    )
}

fn hrefs(page: &str) -> Vec<String> {
    page.split("href=\"")
        .skip(1)
        .map(|r| r.split('"').next().unwrap().to_owned())
        .collect()
}

#[test]
fn a_file_is_linked_only_when_it_cannot_open_as_a_live_page() {
    let ada = Some(user("ada"));
    let mk =
        |id, name: &str, sniffed: &str| media(id, ada.clone(), AttachmentKind::File, name, sniffed);
    let msgs = vec![
        mk(1, "x.html", "text/html"),
        mk(2, "x.svg", "image/svg+xml"),
        mk(3, "noext", "text/html"),
        mk(4, "x.xml", "text/xml"),
        mk(5, "x.mht", "text/html"),
        mk(6, "ok.pdf", "application/pdf"),
        mk(7, "ok.MP4", "video/mp4"),
        mk(8, "x.png", "text/html"),
    ];
    let b = build(input(plan(msgs, 3), all(), &names(), "Me"));
    let page = doc(&b, "index.html");
    let links = hrefs(page);
    for bad in [
        "files/x.html",
        "files/x.svg",
        "files/noext",
        "files/x.xml",
        "files/x.mht",
        "files/x.png",
    ] {
        assert!(!links.iter().any(|l| l == bad), "{bad}");
        // the entry path is shown as text, with the quiet line
        assert!(page.contains(bad), "{bad}");
    }
    assert!(links.iter().any(|l| l == "files/ok.pdf"));
    assert!(links.iter().any(|l| l == "files/ok.MP4"));
    assert!(page.contains("Open it from the folder, not from this page"));
    for f in &b.files {
        assert!(check_entry_name(&f.entry).is_ok(), "{}", f.entry);
    }
}

#[test]
fn a_picture_that_is_also_markup_gets_the_extension_of_its_content() {
    let ada = Some(user("ada"));
    let msgs = vec![
        media(
            1,
            ada.clone(),
            AttachmentKind::Image,
            "p.html",
            "image/jpeg",
        ),
        media(
            2,
            ada.clone(),
            AttachmentKind::Image,
            "q.JPEG",
            "image/jpeg",
        ),
        media(3, ada.clone(), AttachmentKind::Image, "r", "image/webp"),
        media(4, ada.clone(), AttachmentKind::Image, "s.svg", "image/avif"),
        media(5, ada, AttachmentKind::Image, "t.png", "image/jpeg"),
    ];
    let b = build(input(plan(msgs, 3), all(), &names(), "Me"));
    let e: Vec<&str> = b.files.iter().map(|f| f.entry.as_str()).collect();
    assert_eq!(
        e,
        [
            "files/p.html.jpg",
            "files/q.JPEG",
            "files/r.webp",
            "files/s.svg.avif",
            "files/t.png.jpg"
        ]
    );
    let page = doc(&b, "index.html");
    assert!(
        page.contains("<a class=\"file\" href=\"files/p.html.jpg\"><img src=\"files/p.html.jpg\"")
    );
    assert!(page.contains("<img src=\"files/r.webp\""));
    assert!(page.contains("<img src=\"files/s.svg.avif\""));
    assert!(!page.contains("href=\"files/p.html\""));
    for f in &b.files {
        assert!(check_entry_name(&f.entry).is_ok());
    }
    // webp and avif are shown inline
    assert_eq!(page.matches("<img ").count(), 5);
}

#[test]
fn the_page_policy_is_stated_whole() {
    let b = build(input(plan(Vec::new(), 3), all(), &names(), "Me"));
    assert!(doc(&b, "index.html").contains(
        "content=\"default-src 'none'; img-src 'self' file:; style-src 'unsafe-inline'; media-src 'self'; base-uri 'none'; form-action 'none'\""
    ));
}

#[test]
fn a_direct_chat_is_titled_by_its_pseudonymised_peer() {
    let phone = UserId::parse("e164:+4915112345678").unwrap();
    let p = plan(
        vec![planned(row(1, Some(phone.clone()), Body::text("yo")))],
        2,
    );
    let mut i = input(p, all(), &NameMap::new(), "Me");
    i.chat_kind = ConvKind::Direct;
    i.chat_title = "+4915112345678".to_owned();
    i.peer = Some(phone);
    let b = build(i);
    let all_text = everything(&b);
    for needle in ["4915112345678", "+49", "151"] {
        assert!(!all_text.contains(needle), "{needle}");
    }
    let name = b.head.0["people"][1]["name"].as_str().unwrap().to_owned();
    assert!(name.starts_with("p_"));
    assert_eq!(b.head.0["chat"]["title"], name.as_str());
    let page = doc(&b, "index.html");
    assert!(page.contains(&format!("<title>{name}</title>")));
    assert!(page.contains(&format!("<h1>{name}</h1>")));
    // a named peer is the title
    let mut i = input(
        plan(
            vec![planned(row(1, Some(user("ada")), Body::text("hi")))],
            2,
        ),
        all(),
        &names(),
        "Me",
    );
    i.chat_kind = ConvKind::Direct;
    i.peer = Some(user("ada"));
    assert_eq!(build(i).head.0["chat"]["title"], "Ada");
}

#[test]
fn a_group_title_that_hides_an_identity_becomes_a_generic_word() {
    for (title, kind, want) in [
        ("+49 151 1234567", ConvKind::Group, "Group"),
        ("a@b.example", ConvKind::Group, "Group"),
        ("Signal user ⋯ab12", ConvKind::Channel, "Channel"),
        ("", ConvKind::Broadcast, "Broadcast list"),
        ("Club", ConvKind::Group, "Club"),
    ] {
        let mut i = input(plan(Vec::new(), 3), all(), &names(), "Me");
        i.chat_title = title.to_owned();
        i.chat_kind = kind;
        let b = build(i);
        assert_eq!(b.head.0["chat"]["title"], want, "{title}");
        assert!(doc(&b, "index.html").contains(&format!("<h1>{want}</h1>")));
    }
}

#[test]
fn a_group_whose_roster_is_unknown_gets_pseudonyms() {
    let mk = |kind, members| {
        let mut i = input(
            plan(
                vec![planned(row(1, Some(user("ada")), Body::text("hi")))],
                members,
            ),
            all(),
            &names(),
            "Me",
        );
        i.chat_kind = kind;
        build(i)
    };
    for kind in [ConvKind::Group, ConvKind::Channel, ConvKind::Broadcast] {
        let b = mk(kind, 0);
        assert_eq!(b.head.0["options"]["people"], "pseudonyms");
        assert!(!everything(&b).contains("Ada"));
    }
    assert_eq!(mk(ConvKind::Group, 5).head.0["options"]["people"], "names");
    assert_eq!(mk(ConvKind::Direct, 0).head.0["options"]["people"], "names");
}

#[test]
fn one_name_rule_covers_digits_of_every_script_mail_and_fallbacks() {
    use super::people::hides_identity;
    for hides in [
        "",
        "+49 151 1234567",
        "０１２３４５６７８９",
        "٠١٢٣٤٥٦٧٨٩",
        "۰۱۲۳۴۵۶۷۸۹",
        "+49\u{a0}151\u{a0}123\u{a0}4567",
        "ada@example.org",
        "Signal user ⋯ab12",
        "Anyone ⋯1f2e",
        "1234",
        "(12) 34",
        "Ada 1234567",
    ] {
        assert!(hides_identity(hides, None), "{hides:?}");
    }
    for fine in [
        "123456 Ada",
        "Ada",
        "Ada 123",
        "123 Ada",
        "12345 Ada",
        "ada@localhost",
        "a.b@c",
        "Room 101",
    ] {
        assert!(!hides_identity(fine, None), "{fine:?}");
    }
    // 4-, 6- and 7-digit edges
    assert!(!looks_like_number("123"));
    assert!(looks_like_number("1234"));
    assert!(!looks_like_number("123456 x"));
    assert!(looks_like_number("1234567 x"));
    assert!(hides_identity("same", Some("same")) && !hides_identity("same", Some("other")));
}

#[test]
fn people_card_and_own_names_follow_the_rule() {
    let mut n = NameMap::new();
    n.insert(user("ada"), "ａｂ ０１２３４５６７");
    n.insert(user("bob"), "Signal user ⋯ab12");
    let mut contact = row(
        3,
        Some(user("bob")),
        Body::Contact(ContactCard {
            name: Some("+49 151 1234567".into()),
            organization: Some("ada@example.org".into()),
            ..ContactCard::default()
        }),
    );
    contact.author = Some(user("bob"));
    let p = plan(
        vec![
            planned(row(1, Some(user("ada")), Body::text("a"))),
            planned(row(2, Some(user("bob")), Body::text("b"))),
            planned(contact),
            planned(row(4, None, Body::text("mine"))),
        ],
        3,
    );
    let b = build(input(p, all(), &n, "+49 151 1234567"));
    let people = b.head.0["people"].as_array().unwrap();
    assert_eq!(people[0]["name"], "You");
    assert!(people[1]["pseudonym"] == true && people[2]["pseudonym"] == true);
    let text = everything(&b);
    for needle in ["1234567", "ab12", "example.org", "０１２"] {
        assert!(!text.contains(needle), "{needle}");
    }
    let card = lines(&b)[2]["contact"].clone();
    assert!(card.get("name").is_none() && card.get("organization").is_none());
}

#[test]
fn our_own_account_is_never_a_second_person() {
    let mine = user("mine");
    let notice = |id, e| planned(row(id, None, Body::System(e)));
    let mut group_created = notice(1, SystemEvent::GroupCreated { by: None });
    group_created.row.origin = OriginKind::Remote;
    let p = plan(
        vec![
            group_created,
            notice(
                2,
                SystemEvent::MembersAdded {
                    by: Some(mine.clone()),
                    users: vec![mine.clone(), user("ada")],
                },
            ),
            notice(
                3,
                SystemEvent::TitleChanged {
                    by: None,
                    title: "T".into(),
                },
            ),
            notice(4, SystemEvent::MemberLeft { user: user("ada") }),
            planned(row(
                5,
                Some(mine.clone()),
                Body::text("from another device"),
            )),
        ],
        3,
    );
    let mut i = input(p, all(), &names(), "Me");
    i.own = vec![mine];
    let b = build(i);
    let people = b.head.0["people"].as_array().unwrap();
    assert_eq!(people.len(), 2, "{people:?}");
    let l = lines(&b);
    assert_eq!(l[0]["notice"]["by"], "me");
    assert_eq!(l[1]["notice"]["by"], "me");
    assert_eq!(l[1]["notice"]["users"], serde_json::json!(["me", "p1"]));
    assert_eq!(l[2]["notice"]["by"], "me");
    assert!(l[3]["notice"].get("by").is_none());
    assert_eq!(l[4]["author"], "me");
}

#[test]
fn a_link_that_says_another_address_shows_the_address() {
    let body = "visit bank.example now, see example.org/ and http://ok.example/x".to_owned();
    let start = 6u32;
    let text = Text {
        body,
        ranges: vec![TextRange {
            start,
            len: 12,
            kind: RangeKind::Link("https://evil.example/login".into()),
        }],
    };
    let b = build(input(
        plan(
            vec![planned(row(1, Some(user("ada")), Body::Text(text)))],
            3,
        ),
        all(),
        &names(),
        "Me",
    ));
    let page = doc(&b, "index.html");
    assert!(page.contains(
        "bank.example (<a href=\"https://evil.example/login\" rel=\"noopener noreferrer\">https://evil.example/login</a>)"
    ));
    assert_eq!(
        page.matches("href=\"https://evil.example/login\"").count(),
        1
    );
    // an address shown as itself stays one link
    assert!(page.contains(
        "<a href=\"http://ok.example/x\" rel=\"noopener noreferrer\">http://ok.example/x</a>"
    ));
    // the jsonl keeps the href in the run
    let v = &lines(&b)[0];
    assert!(
        v["text"]["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["link"] == "https://evil.example/login")
    );
}

#[test]
fn only_http_links_are_written_in_both_documents() {
    let body = "open it sgnl://signal.me/#p/+15551230000 or https://ok.example/".to_owned();
    let text = Text {
        body: body.clone(),
        ranges: vec![TextRange {
            start: 8,
            len: 31,
            kind: RangeKind::Link("sgnl://signal.me/#p/+15551230000".into()),
        }],
    };
    let b = build(input(
        plan(
            vec![planned(row(1, Some(user("ada")), Body::Text(text)))],
            3,
        ),
        all(),
        &names(),
        "Me",
    ));
    let runs = lines(&b)[0]["text"]["runs"].clone();
    for r in runs.as_array().unwrap() {
        if let Some(l) = r.get("link") {
            assert!(l.as_str().unwrap().starts_with("https://ok.example"), "{l}");
        }
    }
    assert!(!doc(&b, "index.html").contains("href=\"sgnl"));
    assert!(!doc(&b, "messages.jsonl").contains("\"link\":\"sgnl"));
}

#[test]
fn a_record_with_line_separators_is_one_line() {
    let t = "a\u{2028}b\u{2029}c\u{0085}d";
    let b = build(input(
        plan(
            vec![
                planned(row(1, Some(user("ada")), Body::text(t))),
                planned(row(2, Some(user("ada")), Body::text("two"))),
            ],
            3,
        ),
        all(),
        &names(),
        "Me",
    ));
    let j = doc(&b, "messages.jsonl");
    assert_eq!(j.lines().count(), 2);
    assert_eq!(j.split('\n').count(), 3);
    assert!(j.contains("\\u2028") && j.contains("\\u2029") && j.contains("\\u0085"));
    assert!(!j.contains('\u{2028}') && !j.contains('\u{2029}') && !j.contains('\u{0085}'));
    assert_eq!(lines(&b)[0]["text"]["plain"], t);
}

#[test]
fn pseudonyms_depend_on_the_salt_and_never_run_out() {
    let users: Vec<UserId> = (0..30).map(|i| user(&format!("u{i}"))).collect();
    let mk = |salt: u8| {
        let msgs = users
            .iter()
            .enumerate()
            .map(|(i, u)| planned(row(i as i64 + 1, Some(u.clone()), Body::text("x"))))
            .collect();
        let mut i = input(plan(msgs, 100), all(), &NameMap::new(), "Me");
        i.salt = [salt; 32];
        build(i)
    };
    let names_of = |b: &Built| -> Vec<String> {
        b.head.0["people"].as_array().unwrap()[1..]
            .iter()
            .map(|p| p["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let (a, b) = (names_of(&mk(7)), names_of(&mk(8)));
    assert_ne!(a, b);
    assert!(a.iter().zip(&b).all(|(x, y)| x != y));
    assert!(a.iter().all(|n| n.len() == 8 && n.starts_with("p_")));
    // many people: all different
    let names = NameSnapshot::new();
    let mut table = People::new(&names, "Me", &[], ConvKind::Group, 100, [1; 32]);
    let mut seen = std::collections::HashSet::new();
    for i in 0..3000 {
        table.id_of(Some(&user(&format!("many{i}"))));
    }
    for p in &table.list()[1..] {
        assert!(seen.insert(p.name.clone()), "{}", p.name);
    }
    // a person the table never heard of is named by a fixed neutral word, never a pseudonym
    assert!(!table.name_of(Some(&user("nobody"))).starts_with("p_"));
}

#[test]
fn every_person_id_in_the_records_is_in_the_people_table() {
    let (ada, bob, cat, dan, eve, fay) = (
        user("ada"),
        user("bob"),
        user("cat"),
        user("dan"),
        user("eve"),
        user("fay"),
    );
    let ment = Text {
        body: "hi \u{fffc}".to_owned(),
        ranges: vec![TextRange {
            start: 3,
            len: 3,
            kind: RangeKind::Mention(fay.clone()),
        }],
    };
    let mut deleted = row(3, Some(ada.clone()), Body::text("x"));
    deleted.deleted = Some(Deletion::ForEveryone {
        by: Some(dan.clone()),
    });
    let mut reacted = row(4, Some(ada.clone()), Body::text("r"));
    reacted.reactions = vec![ReactionGroup {
        emoji: "👍".into(),
        users: vec![Some(eve.clone())],
    }];
    let p = plan(
        vec![
            planned(row(
                1,
                None,
                Body::System(SystemEvent::MembersAdded {
                    by: Some(bob.clone()),
                    users: vec![cat.clone()],
                }),
            )),
            planned(row(2, Some(ada.clone()), Body::Text(ment))),
            planned(deleted),
            planned(reacted),
        ],
        3,
    );
    let b = build(input(p, all(), &NameMap::new(), "Me"));
    let people: std::collections::HashSet<String> = b.head.0["people"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_owned())
        .collect();
    fn walk(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, x) in m {
                    if matches!(k.as_str(), "author" | "by" | "users" | "mention") {
                        match x {
                            serde_json::Value::String(s) => out.push(s.clone()),
                            serde_json::Value::Array(a) => {
                                out.extend(a.iter().filter_map(|s| s.as_str().map(str::to_owned)))
                            }
                            _ => {}
                        }
                    }
                    walk(x, out);
                }
            }
            serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut ids = Vec::new();
    for l in lines(&b) {
        walk(&l, &mut ids);
    }
    assert!(ids.len() >= 8, "{ids:?}");
    for id in ids {
        assert!(id == "everyone" || people.contains(&id), "{id}");
    }
    assert!(!doc(&b, "messages.jsonl").contains("p?"));
}

/// A calendar that changes its offset at fixed instants.
struct Steps(Vec<(i64, i32)>);

impl crate::time::Calendar for Steps {
    fn utc_offset_s(&self, ts: i64) -> i32 {
        self.0
            .iter()
            .rev()
            .find(|(from, _)| ts >= *from)
            .map_or(0, |(_, o)| *o)
    }
}

#[test]
fn the_offset_table_keeps_the_offset_of_each_instant() {
    let cal = Steps(vec![
        (0, 3600),
        (T0 + 2 * 3_600_000, -(3 * 3600 + 1800)),
        (T0 + 50 * 3_600_000, 7200),
    ]);
    let mut m = planned(row(1, Some(user("ada")), Body::text("v2")));
    m.row.edited_at = Some(T0 + 3 * 3_600_000);
    m.row.sent_at = T0 + 60_000;
    m.revisions = vec![
        Revision {
            revision: 0,
            text: Some(Text::plain("v1")),
            at: T0 + 60_000,
        },
        Revision {
            revision: 1,
            text: Some(Text::plain("v2")),
            at: T0 + 3 * 3_600_000,
        },
    ];
    let p = plan(vec![m], 3);
    let table = OffsetTable::from_plan(&p, &cal);
    let mut i = input(p, all(), &names(), "Me");
    i.offsets = table;
    let b = build(i);
    let v = &lines(&b)[0];
    // sent before the change: +01:00; edited after it: -03:30 (the minutes are written)
    assert_eq!(v["time"], "2026-01-01T01:01:00+01:00");
    assert_eq!(v["edited_at"], "2025-12-31T23:30:00-03:30");
    assert_eq!(v["edits"][0]["at"], "2026-01-01T01:01:00+01:00");
    // the plan's own clock is a day later, still before the third step: -03:30
    assert_eq!(b.head.0["exported_at"], "2026-01-01T20:30:00-03:30");
}

#[test]
fn a_remote_row_without_an_author_is_not_ours_and_a_left_out_kind_is_counted() {
    let mut remote = planned(row(1, None, Body::text("who")));
    remote.row.origin = OriginKind::Remote;
    let own = planned(row(2, None, Body::text("me")));
    let pic = media(
        3,
        Some(user("ada")),
        AttachmentKind::Image,
        "a.png",
        "image/png",
    );
    let mut t = all();
    t.pictures = false;
    let b = build(input(plan(vec![remote, own, pic], 3), t, &names(), "Me"));
    let l = lines(&b);
    assert!(l[0].get("author").is_none());
    assert_eq!(l[1]["author"], "me");
    assert_eq!(b.counts.left_out_kinds, 1);
    assert_eq!(b.head.0["counts"]["left_out"]["kinds"], 1);
    assert_eq!(l[2]["files"][0]["state"], "left_out");
}

#[test]
fn a_deleted_row_lists_no_file() {
    let mut m = media(
        1,
        Some(user("ada")),
        AttachmentKind::Image,
        "a.png",
        "image/png",
    );
    m.row.deleted = Some(Deletion::ForEveryone { by: None });
    let b = build(input(plan(vec![m], 3), all(), &names(), "Me"));
    assert!(b.files.is_empty());
    assert!(!everything(&b).contains("a.png"));
    assert_eq!(lines(&b)[0]["kind"], "deleted");
}

#[test]
fn the_dialog_counts_files_not_here_by_the_tick_they_need() {
    let ada = Some(user("ada"));
    let mut here = media(1, ada.clone(), AttachmentKind::Video, "a.mp4", "video/mp4");
    here.files[0].accepted = Err(ExportFileRefusal::NotHere);
    let mut later = media(2, ada, AttachmentKind::Video, "b.mp4", "video/mp4");
    later.files[0].accepted = Err(ExportFileRefusal::NotHere);
    later.verdict = ExportVerdict::NeedsTick;
    let n = numbers(&plan(vec![here, later], 3));
    assert_eq!((n.not_here, n.not_here_disappearing), (1, 1));
}

type RunTuple = (String, Vec<&'static str>, Option<String>, Option<String>);

/// The cutting of a text as it was before the single sweep, kept as the reference.
fn reference_runs(text: &Text, people: &People) -> Vec<RunTuple> {
    use std::collections::BTreeSet;
    let body = text.body.as_str();
    let valid: Vec<&TextRange> = text
        .ranges
        .iter()
        .filter(|r| kourier_model::text::validate_range(body, r).is_ok())
        .collect();
    let mentions = text.shown_mentions();
    let urls = crate::richtext::find_urls(body);
    let mut cuts: BTreeSet<usize> = BTreeSet::from([0, body.len()]);
    for r in &valid {
        cuts.insert(r.start as usize);
        cuts.insert(r.start as usize + r.len as usize);
    }
    for (s, e) in &urls {
        cuts.insert(*s);
        cuts.insert(*e);
    }
    let cuts: Vec<usize> = cuts.into_iter().collect();
    let order = ["bold", "italic", "strike", "mono", "spoiler"];
    let mut runs: Vec<RunTuple> = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let mention = mentions.iter().find(|r| r.start as usize == a);
        let (piece, mid) = match mention.map(|r| &r.kind) {
            Some(RangeKind::Mention(u)) => (
                format!("@{}", people.name_of(Some(u))),
                Some(people.lookup_id(Some(u))),
            ),
            Some(_) => ("@everyone".to_owned(), Some("everyone".to_owned())),
            None => (
                body[a..b].replace(kourier_model::text::MENTION_PLACEHOLDER, ""),
                None,
            ),
        };
        if piece.is_empty() {
            continue;
        }
        let mut styles: Vec<&'static str> = Vec::new();
        let mut link: Option<(usize, String)> = None;
        for r in valid
            .iter()
            .filter(|r| (r.start as usize) <= a && b <= r.start as usize + r.len as usize)
        {
            let s = match &r.kind {
                RangeKind::Bold => Some("bold"),
                RangeKind::Italic => Some("italic"),
                RangeKind::Strike => Some("strike"),
                RangeKind::Mono => Some("mono"),
                RangeKind::Spoiler => Some("spoiler"),
                _ => None,
            };
            if let Some(s) = s {
                styles.push(s);
            } else if let RangeKind::Link(url) = &r.kind
                && let Some(h) = record::http_href(url.trim())
                && link.as_ref().is_none_or(|(l, _)| (r.len as usize) < *l)
            {
                link = Some((r.len as usize, h.to_owned()));
            }
        }
        styles.sort_by_key(|s| order.iter().position(|x| x == s));
        styles.dedup();
        let mut href = link.map(|(_, h)| h);
        if href.is_none() {
            href = urls
                .iter()
                .find(|(s, e)| *s <= a && b <= *e)
                .and_then(|(s, e)| {
                    record::http_href(&crate::richtext::href_of_match(&body[*s..*e]))
                        .map(str::to_owned)
                });
        }
        if mid.is_some() {
            href = None;
        }
        match runs.last_mut() {
            Some(last)
                if last.2.is_none() && mid.is_none() && last.1 == styles && last.3 == href =>
            {
                last.0.push_str(&piece)
            }
            _ => runs.push((piece, styles, mid, href)),
        }
    }
    runs
}

fn as_tuples(t: &record::TextOut) -> Vec<RunTuple> {
    t.runs
        .iter()
        .map(|r| {
            (
                r.text.clone(),
                r.styles.clone(),
                r.mention.clone(),
                r.href.clone(),
            )
        })
        .collect()
}

#[test]
fn the_sweep_cuts_a_text_like_the_scan_per_window() {
    let names = NameSnapshot::new();
    let mut table = People::new(&names, "Me", &[], ConvKind::Group, 5, [1; 32]);
    table.id_of(Some(&user("bob")));
    let body = "ab \u{fffc} cd http://x.example/a, e https://y.example/b. fg hi jk".to_owned();
    let kinds = [
        RangeKind::Bold,
        RangeKind::Italic,
        RangeKind::Strike,
        RangeKind::Mono,
        RangeKind::Spoiler,
        RangeKind::Link("https://l.example/".into()),
        RangeKind::Link("sgnl://signal.me/#p/x".into()),
    ];
    let mut seed = 12345u64;
    let mut next = |m: u64| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) % m
    };
    for round in 0..200 {
        let mut ranges = Vec::new();
        for _ in 0..(round % 9) {
            let s = next(body.len() as u64) as u32;
            let l = 1 + next(20) as u32;
            let k = kinds[next(kinds.len() as u64) as usize].clone();
            ranges.push(TextRange {
                start: s,
                len: l,
                kind: k,
            });
        }
        ranges.push(TextRange {
            start: 3,
            len: 3,
            kind: RangeKind::Mention(user("bob")),
        });
        let t = Text {
            body: body.clone(),
            ranges,
        };
        let got = record::text_out(&t, &table);
        assert_eq!(as_tuples(&got), reference_runs(&t, &table), "round {round}");
    }
}

#[test]
fn twenty_thousand_style_ranges_in_one_message_cut_in_one_sweep() {
    let n = 20_000usize;
    let body: String = (0..n).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
    let ranges: Vec<TextRange> = (0..n)
        .map(|i| TextRange {
            start: i as u32,
            len: 1,
            kind: if i % 2 == 0 {
                RangeKind::Bold
            } else {
                RangeKind::Italic
            },
        })
        .collect();
    let t = Text {
        body: body.clone(),
        ranges,
    };
    let names = NameSnapshot::new();
    let table = People::new(&names, "Me", &[], ConvKind::Group, 5, [1; 32]);
    let started = std::time::Instant::now();
    let out = record::text_out(&t, &table);
    let took = started.elapsed();
    println!("text_out of {n} one-character ranges: {took:?}");
    assert_eq!(out.plain, body);
    assert_eq!(out.runs.len(), n);
    assert!(out.runs.iter().enumerate().all(|(i, r)| r.styles
        == [if i % 2 == 0 { "bold" } else { "italic" }]
        && r.text.chars().count() == 1));
    // the same cut as the scan per window, on a prefix the scan can still do
    let small = Text {
        body: body[..400].to_owned(),
        ranges: t.ranges[..400].to_vec(),
    };
    assert_eq!(
        as_tuples(&record::text_out(&small, &table)),
        reference_runs(&small, &table)
    );
}

/// What `build` carries, by class: the numbers the dialog must show for the same ticks.
fn carried_by_class(plan: &ExportPlan, b: &Built) -> [ClassCount; 3] {
    let mut out = [ClassCount::default(); 3];
    for f in &b.files {
        let (pm, pf) = plan
            .messages
            .iter()
            .flat_map(|pm| pm.files.iter().map(move |pf| (pm, pf)))
            .find(|(_, pf)| pf.attachment == f.attachment)
            .expect("a planned file");
        let i = match class_of(&pm.row.body, pf.kind) {
            FileClass::Pictures => 0,
            FileClass::Videos => 1,
            FileClass::Other => 2,
        };
        out[i].count += 1;
        out[i].bytes += pf.size;
    }
    out
}

#[test]
fn the_dialog_numbers_equal_what_build_carries_for_all_sixteen_tick_sets() {
    let ada = Some(user("ada"));
    let sized = |mut pm: PlannedMessage, size: u64| {
        for f in &mut pm.files {
            f.size = size;
        }
        pm
    };
    let mut messages = vec![
        sized(
            media(1, ada.clone(), AttachmentKind::Image, "a.png", "image/png"),
            1_000,
        ),
        sized(
            media(2, ada.clone(), AttachmentKind::Video, "b.mp4", "video/mp4"),
            20_000,
        ),
        sized(
            media(
                3,
                ada.clone(),
                AttachmentKind::File,
                "c.bin",
                "application/pdf",
            ),
            300_000,
        ),
    ];
    // A disappearing message with a picture and a file.
    let mut timed = planned(row(
        4,
        ada.clone(),
        Body::Media {
            attachments: vec![
                att(41, AttachmentKind::Image, Some("d.png")),
                att(42, AttachmentKind::File, Some("e.bin")),
            ],
            caption: None,
        },
    ));
    timed.verdict = ExportVerdict::NeedsTick;
    timed.files = vec![
        PlannedFile {
            size: 4_000_000,
            ..file(41, AttachmentKind::Image, Some("d.png"), Some("image/png"))
        },
        PlannedFile {
            size: 50_000_000,
            ..file(42, AttachmentKind::File, Some("e.bin"), None)
        },
    ];
    messages.push(timed);
    // A view-once message and a gone one, both with a file the numbers never count.
    for (id, left) in [(5, LeftOut::ViewOnce), (6, LeftOut::Gone)] {
        let mut m = sized(
            media(id, ada.clone(), AttachmentKind::Image, "x.png", "image/png"),
            600_000_000,
        );
        m.verdict = ExportVerdict::LeftOut(left);
        messages.push(m);
    }
    // A file that is not on this computer.
    let mut away = media(
        7,
        ada.clone(),
        AttachmentKind::Video,
        "far.mp4",
        "video/mp4",
    );
    away.files[0].accepted = Err(ExportFileRefusal::NotHere);
    messages.push(away);
    let p = plan(messages, 3);
    let n = numbers(&p);
    for bits in 0..16u8 {
        let ticks = Ticks {
            pictures: bits & 1 != 0,
            videos: bits & 2 != 0,
            other_files: bits & 4 != 0,
            disappearing: bits & 8 != 0,
        };
        let built = build(input(p.clone(), ticks, &names(), "Me"));
        let shown = n.shown(ticks);
        assert_eq!(shown.messages as usize, built.messages.len(), "{ticks:?}");
        let by_class = carried_by_class(&p, &built);
        assert_eq!(
            [shown.pictures, shown.videos, shown.other_files],
            by_class,
            "{ticks:?}"
        );
        assert_eq!(
            shown.total_bytes,
            by_class.iter().map(|c| c.bytes).sum::<u64>(),
            "{ticks:?}"
        );
        assert_eq!(
            shown.not_here, built.counts.files_not_on_this_computer,
            "{ticks:?}"
        );
    }
}

//! The app without Qt: fixture mode starts the real core over the fake network in a temporary
//! profile, serves queries from `core.db`, links through rotating QR codes, and shuts down in
//! order.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use kourier_app::facade::{
    ConversationListQuery, Query, QueryData, TimelineAnchor, TimelineQuery, UiCommand, UiEvent,
};
use kourier_app::{App, FixtureNetwork, FixtureOptions, Launch, StartOptions};
use kourier_model::{AccountState, ConversationId, LoginStepKind, NetworkKind, Redacted};

const WAIT: Duration = Duration::from_secs(60);

fn fixture(link: bool) -> StartOptions {
    StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            link,
            qr_valid_for: Duration::from_millis(200),
            qr_codes: 3,
            messages_per_conversation: 60,
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    }
}

#[test]
fn seeded_fixture_serves_core_db_and_stops() {
    let started = Instant::now();
    let app = App::start(fixture(false)).expect("start");
    let h = app.handle();
    let home = app.paths().data.clone();
    assert!(home.join("core.db").is_file());
    assert!(h.is_fixture());
    // The bound of the shutdown below follows the registry the start built (R-Q1): the fixture's
    // network states no grace of its own, so it is the core's 5 s plus the margin, as it always was.
    assert_eq!(app.core_shutdown_timeout(), Duration::from_secs(10));
    let info = h.fixture().expect("fixture info");
    let demo = info.demo_conversation.expect("a group");
    assert_eq!(h.networks().len(), 1);
    assert_eq!(h.networks()[0].kind, NetworkKind::Fake);
    // This build holds an experimental network that is turned off: the welcome page, Add
    // Account and Settings show it that way, and no account or choice of the user exists.
    assert_eq!(h.networks_off().len(), 1);
    assert_eq!(h.networks_off()[0].kind, NetworkKind::WhatsApp);
    assert!(h.networks_off()[0].experimental);
    assert!(h.desktop().prefs().networks_on.is_empty());
    assert_eq!(h.pending_link(), None);
    assert!(!h.restart_requested());
    assert!(!h.hostname().is_empty());

    let (accounts, convs, page) = h.runtime().block_on(async {
        let accounts = match h.query(Query::Accounts).await.expect("accounts") {
            QueryData::Accounts(a) => a,
            other => panic!("unexpected {other:?}"),
        };
        let convs = match h
            .query(Query::Conversations(ConversationListQuery::default()))
            .await
            .expect("conversations")
        {
            QueryData::Conversations(p) => p,
            other => panic!("unexpected {other:?}"),
        };
        let page = match h
            .query(Query::Timeline(TimelineQuery {
                conv: demo,
                anchor: TimelineAnchor::Newest,
                limit: 50,
            }))
            .await
            .expect("timeline")
        {
            QueryData::Timeline(p) => p,
            other => panic!("unexpected {other:?}"),
        };
        (accounts, convs, page)
    });
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].state, AccountState::Active);
    assert!(
        accounts[0].avatar.is_some(),
        "the account has its own profile picture"
    );
    // 12 direct chats and 4 groups, one of them archived; and the five showcase chats.
    assert_eq!(convs.rows.len(), 20);
    assert_eq!(page.rows.len(), 50);
    assert!(page.has_older);
    assert!(info.now_ms > page.rows.last().map_or(0, |r| r.sent_at));

    // The showcase chats are found, each a chat of its own.
    let quotes = info.quotes_conversation.expect("the quotes chat");
    let timer = info.timer_conversation.expect("the timer chat");
    let kinds = info.kinds_conversation.expect("the chat with stand-ins");
    assert!(quotes != timer && timer != kinds && quotes != kinds);
    let long = info
        .readmore_conversation
        .expect("the chat with a long text");
    assert!(![quotes, timer, kinds].contains(&long));
    let timeline = |conv| {
        h.runtime().block_on(async {
            match h
                .query(Query::Timeline(TimelineQuery {
                    conv,
                    anchor: TimelineAnchor::Newest,
                    limit: 50,
                }))
                .await
                .expect("timeline")
            {
                QueryData::Timeline(p) => p,
                other => panic!("unexpected {other:?}"),
            }
        })
    };
    // The core's clock runs at the fixture's date: "now" is a little after the newest message,
    // so recent own messages can still be edited and deleted for everyone (with the system
    // clock every fixture message would be months outside those windows).
    let page = timeline(quotes);
    assert!(
        (0..60_000).contains(&(page.now - info.now_ms)),
        "the core's clock is the fixture's: {} vs {}",
        page.now,
        info.now_ms
    );
    let editable = page
        .rows
        .iter()
        .filter(|r| r.author.is_none() && r.actions.edit && r.actions.delete_for_everyone)
        .count();
    assert!(
        editable >= 2,
        "recent own messages can be edited: {editable}"
    );
    assert!(
        page.rows.iter().any(|r| r.reply.is_some()),
        "the quotes chat has quotes"
    );
    // Stand-ins for content that is not shown yet, each with its kind.
    let hints: Vec<String> = timeline(kinds)
        .rows
        .iter()
        .filter_map(|r| match &r.body {
            kourier_model::Body::Unsupported { kind_hint, .. } => Some(kind_hint.clone()),
            _ => None,
        })
        .collect();
    assert!(
        hints.iter().any(|h| h == "view-once") && hints.len() >= 5,
        "{hints:?}"
    );
    // The chat of imported attachments that cannot be had (`--page=unavailable`): one of the
    // generated chats (none is added, so no list changes), with every state the page shows and
    // nothing downloaded by itself.
    let imported = info
        .unavailable_conversation
        .expect("the chat of attachments that cannot be had");
    assert!(![quotes, timer, kinds, long].contains(&imported));
    let mut states = std::collections::BTreeMap::<String, usize>::new();
    // (The chat's own photo, from the generated data, is on its way or here: not counted.)
    for row in timeline(imported).rows {
        let attachments = match &row.body {
            kourier_model::Body::Media { attachments, .. } => attachments.clone(),
            kourier_model::Body::Sticker(a) => vec![a.as_ref().clone()],
            _ => Vec::new(),
        };
        for a in attachments {
            if !matches!(
                a.state,
                kourier_model::AttachmentState::Local
                    | kourier_model::AttachmentState::Downloading { .. }
            ) {
                *states.entry(format!("{:?}", a.state)).or_default() += 1;
            }
        }
    }
    assert_eq!(
        states,
        std::collections::BTreeMap::from([
            ("AskFailed { why: NoAnswer }".to_owned(), 2),
            ("AskFailed { why: NoFile }".to_owned(), 1),
            ("AskFailed { why: NoMessage }".to_owned(), 1),
            ("Asking".to_owned(), 1),
            ("Expired".to_owned(), 2),
            ("Remote".to_owned(), 1),
            ("Unavailable".to_owned(), 7),
        ]),
        "a picture, a file, a sticker and four of an album are not available on this device, a \
         picture and a file are gone, one picture is asked of the phone and four asks ended \
         (two without an answer, no message, no file), one picture waits for a click"
    );
    // Several members reacted to one message of the group `--page=chat` opens.
    let reacted = timeline(demo)
        .rows
        .iter()
        .map(|r| r.reactions.iter().map(|g| g.users.len()).sum::<usize>())
        .max()
        .unwrap_or(0);
    assert!(reacted >= 4, "reactions of several members: {reacted}");

    let stop = Instant::now();
    app.shutdown();
    assert!(
        stop.elapsed() < Duration::from_secs(15),
        "shutdown took too long"
    );
    assert!(!home.exists(), "the temporary profile is removed");
    eprintln!("seeded fixture round trip: {:?}", started.elapsed());
}

#[test]
fn fixture_link_walks_rotating_qr_codes() {
    let app = App::start(fixture(true)).expect("start");
    let h = app.handle();
    assert!(h.fixture().is_none(), "nothing is seeded before the link");
    let mut events = h.subscribe();
    let accounts = h.runtime().block_on(async {
        match h.query(Query::Accounts).await.expect("accounts") {
            QueryData::Accounts(a) => a,
            other => panic!("unexpected {other:?}"),
        }
    });
    assert!(
        accounts.is_empty(),
        "an empty profile starts on the welcome page"
    );

    h.send(UiCommand::AddAccount {
        network: NetworkKind::Fake,
        flow: "qr".into(),
    })
    .expect("add");
    let mut qr_codes = BTreeSet::new();
    let mut done = false;
    let mut conversations = false;
    h.runtime().block_on(async {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !(done && conversations) {
            let e = tokio::time::timeout_at(deadline, events.recv())
                .await
                .expect("timed out")
                .expect("events");
            match e {
                UiEvent::LoginStep { account, step } => match step.kind {
                    LoginStepKind::Input { fields } => {
                        assert_eq!(fields[0].id, "device-name");
                        h.send(UiCommand::SubmitLoginInput {
                            account,
                            txn: step.txn,
                            values: vec![(
                                "device-name".into(),
                                Redacted::new(h.hostname().to_owned()),
                            )],
                        })
                        .expect("submit");
                    }
                    LoginStepKind::ShowQr { data, .. } => {
                        qr_codes.insert(data.expose().clone());
                    }
                    LoginStepKind::Done => done = true,
                    LoginStepKind::Failed { error } => panic!("login failed: {error:?}"),
                    _ => {}
                },
                UiEvent::ConversationsChanged { .. } if done => conversations = true,
                _ => {}
            }
        }
    });
    assert_eq!(qr_codes.len(), 3, "the codes rotate");
    app.shutdown();
}

/// The fixture's chat for `--page=calls`: one line for every state of a 1:1 call, in a direct
/// chat that is below the fold of the conversation list; on that page the first missed call is
/// unread and the last call rings from the fixture's "now" (its chat is then the first of the
/// list below the pinned ones), for no longer than a call may ring.
#[test]
fn the_calls_chat_has_a_line_of_every_state() {
    use kourier_app::facade::OriginKind;
    use kourier_model::{Body, CallDirection, CallMedia, CallState, ReadState, SystemEvent};
    let start = |unread: bool| {
        App::start(StartOptions {
            launch: Launch::Fixture(FixtureOptions {
                messages_per_conversation: 20,
                calls_unread: unread,
                calls_ring_for: unread.then_some(FixtureOptions::RING),
                ..FixtureOptions::default()
            }),
            ..StartOptions::default()
        })
        .expect("start")
    };
    for unread in [true, false] {
        let app = start(unread);
        let h = app.handle();
        let info = h.fixture().expect("fixture info");
        let calls = info.calls_conversation.expect("the chat with calls");
        let (convs, page) = h.runtime().block_on(async {
            let convs = match h
                .query(Query::Conversations(ConversationListQuery {
                    limit: 500,
                    ..ConversationListQuery::default()
                }))
                .await
                .expect("conversations")
            {
                QueryData::Conversations(p) => p,
                other => panic!("unexpected {other:?}"),
            };
            let page = match h
                .query(Query::Timeline(TimelineQuery {
                    conv: calls,
                    anchor: TimelineAnchor::Newest,
                    limit: 50,
                }))
                .await
                .expect("timeline")
            {
                QueryData::Timeline(p) => p,
                other => panic!("unexpected {other:?}"),
            };
            (convs, page)
        });
        // Below the fold of the list in every window (the list shows about eleven rows), so
        // no picture of another page shows its preview or moves its scroll bar.
        let at = convs
            .rows
            .iter()
            .position(|r| r.id == calls)
            .expect("the chat is listed");
        if unread {
            // A call rings now: its chat is the first below the pinned ones.
            assert!(
                at < 14 && convs.rows[..at].iter().all(|r| r.pinned_order.is_some()),
                "the chat is {at} of {} in the list",
                convs.rows.len()
            );
        } else {
            assert!(
                at >= 14,
                "the chat is {at} of {} in the list",
                convs.rows.len()
            );
        }
        let row = &convs.rows[at];
        assert_eq!(row.unread, u32::from(unread), "the missed call is unread");
        // The lines, newest first: every state of a 1:1 call once.
        let lines: Vec<_> = page
            .rows
            .iter()
            .rev()
            .filter_map(|m| match &m.body {
                Body::System(SystemEvent::Call(l)) => Some((l.clone(), m.read_state, m.origin)),
                _ => None,
            })
            .collect();
        assert_eq!(lines.len(), 7, "{lines:?}");
        let states: Vec<_> = lines
            .iter()
            .map(|(l, ..)| (l.direction, l.state, l.media))
            .collect();
        use CallDirection::{Incoming, Outgoing};
        if unread {
            assert!(matches!(
                states[0],
                (Incoming, CallState::Ringing { .. }, Some(CallMedia::Video))
            ));
        } else {
            assert_eq!(
                states[0],
                (Incoming, CallState::Missed, Some(CallMedia::Video))
            );
        }
        assert_eq!(states[1], (Incoming, CallState::Missed, None));
        assert_eq!(
            states[2],
            (Outgoing, CallState::Unanswered, Some(CallMedia::Voice))
        );
        assert_eq!(
            states[3],
            (Outgoing, CallState::Answered, Some(CallMedia::Video))
        );
        assert_eq!(
            states[4],
            (Incoming, CallState::Answered, Some(CallMedia::Voice))
        );
        assert_eq!(
            states[5],
            (Incoming, CallState::Declined, Some(CallMedia::Video))
        );
        assert_eq!(
            states[6],
            (Incoming, CallState::Missed, Some(CallMedia::Voice))
        );
        // Only the first missed call can be unread, and only when asked for.
        let unread_lines = lines
            .iter()
            .filter(|(_, read, _)| *read == Some(ReadState::Unread))
            .count();
        assert_eq!(unread_lines, usize::from(unread));
        assert_eq!(
            lines[6].1 == Some(ReadState::Unread),
            unread,
            "the voice call that was missed"
        );
        // The ring ends on the fixture's clock when a ring may end at the latest, and not
        // before a picture is taken; a call line has no author.
        if let CallState::Ringing { deadline } = lines[0].0.state {
            let ring = i64::try_from(FixtureOptions::RING.as_millis()).expect("ms");
            assert_eq!(ring, kourier_model::MAX_RING_MS);
            assert!(
                (info.now_ms + 60_000..=info.now_ms + ring).contains(&deadline),
                "rings for two minutes from the start: {}",
                deadline - info.now_ms
            );
        }
        assert!(
            lines
                .iter()
                .all(|(.., origin)| *origin != OriginKind::OwnThisDevice)
        );
        app.shutdown();
    }
}

/// A chat's title as the plain fixture states it: a WhatsApp person's self-chosen name carries
/// the phone's "~" in the core's shown title (B8c), which a row-for-row comparison of the chats
/// of a WhatsApp fixture with the plain fixture's leaves off (the mark itself is pinned in
/// `kourier-core/tests/profile_mark.rs`, and every WhatsApp picture shows it).
fn unmarked(title: Option<&str>) -> Option<&str> {
    title.map(|t| t.strip_prefix('~').unwrap_or(t))
}

/// What a fixture shows of its chats, one line per chat in the list's order: the kind, title,
/// preview time, unread count, pin and the number of messages the chat holds.
fn chats_of(h: &kourier_app::AppHandle) -> Vec<String> {
    h.runtime().block_on(async {
        let page = match h
            .query(Query::Conversations(ConversationListQuery::default()))
            .await
            .expect("conversations")
        {
            QueryData::Conversations(p) => p,
            other => panic!("unexpected {other:?}"),
        };
        let mut lines = Vec::new();
        for row in &page.rows {
            let timeline = match h
                .query(Query::Timeline(TimelineQuery {
                    conv: row.id,
                    anchor: TimelineAnchor::Newest,
                    limit: 400,
                }))
                .await
                .expect("timeline")
            {
                QueryData::Timeline(t) => t,
                other => panic!("unexpected {other:?}"),
            };
            lines.push(format!(
                "{:?} {:?} {} unread={} pin={:?} messages={} older={}",
                row.kind,
                unmarked(row.title.as_deref()),
                row.last_activity,
                row.unread,
                row.pinned_order,
                timeline.rows.len(),
                timeline.has_older,
            ));
        }
        lines
    })
}

/// The fixture whose account is a WhatsApp account (played by the fake) is the plain
/// fixture in every chat, row for row: a chat added to it would move the scroll-bar thumb of
/// the list in every wide picture. Its network is on, in the preferences too, and nothing is
/// off.
#[test]
fn the_whatsapp_fixture_has_the_plain_fixtures_chats_row_for_row() {
    let plain = App::start(fixture(false)).expect("start");
    let plain_chats = chats_of(&plain.handle());
    assert_eq!(plain_chats.len(), 20, "{plain_chats:#?}");
    drop(plain);

    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            link: false,
            messages_per_conversation: 60,
            network: FixtureNetwork::WhatsApp,
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start");
    let h = app.handle();
    assert_eq!(chats_of(&h), plain_chats);
    let accounts = h.runtime().block_on(async {
        match h.query(Query::Accounts).await.expect("accounts") {
            QueryData::Accounts(a) => a,
            other => panic!("unexpected {other:?}"),
        }
    });
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].network, NetworkKind::WhatsApp);
    assert_eq!(accounts[0].state, AccountState::Active);
    assert_eq!(h.networks().len(), 1);
    assert_eq!(h.networks()[0].kind, NetworkKind::WhatsApp);
    assert!(h.networks()[0].experimental && !h.networks()[0].history_transfer);
    assert!(h.networks_off().is_empty());
    assert!(h.desktop().prefs().network_on(NetworkKind::WhatsApp));
}

/// The fixture whose account is a Threema account (the fake plays one, TH4 KH1c) is the plain
/// fixture in every chat too, with Threema's network on in the preferences and nothing off.
#[test]
fn the_threema_fixture_has_the_plain_fixtures_chats_and_the_network_on() {
    let plain = App::start(fixture(false)).expect("start");
    let plain_chats = chats_of(&plain.handle());
    assert_eq!(plain_chats.len(), 20, "{plain_chats:#?}");
    drop(plain);

    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            link: false,
            messages_per_conversation: 60,
            network: FixtureNetwork::Threema,
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start");
    let h = app.handle();
    assert_eq!(chats_of(&h), plain_chats);
    let accounts = h.runtime().block_on(async {
        match h.query(Query::Accounts).await.expect("accounts") {
            QueryData::Accounts(a) => a,
            other => panic!("unexpected {other:?}"),
        }
    });
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].network, NetworkKind::Threema);
    assert_eq!(accounts[0].state, AccountState::Active);
    assert_eq!(h.networks().len(), 1);
    assert_eq!(h.networks()[0].kind, NetworkKind::Threema);
    assert!(h.networks()[0].experimental && !h.networks()[0].history_transfer);
    assert!(h.networks_off().is_empty());
    assert!(h.desktop().prefs().network_on(NetworkKind::Threema));
    assert!(!h.desktop().prefs().network_on(NetworkKind::WhatsApp));
}

/// The held links of the pages `thqr` and `themoji` (TH4 KH1c): both start from the welcome page
/// with no account and link as Threema; the first stands at the QR code, the second scans it at
/// once and stands at the three emoji, and neither goes on (the fake's phone confirms in ten
/// minutes).
#[test]
fn a_held_threema_link_stands_at_the_qr_code_or_at_the_emoji() {
    for at_emoji in [false, true] {
        let app = App::start(StartOptions {
            launch: Launch::Fixture(FixtureOptions {
                link: true,
                hold_link: true,
                hold_emoji: at_emoji,
                network: FixtureNetwork::Threema,
                ..FixtureOptions::default()
            }),
            ..StartOptions::default()
        })
        .expect("start");
        let h = app.handle();
        assert!(
            h.networks().iter().all(|n| n.kind == NetworkKind::Threema),
            "the welcome page offers Threema alone"
        );
        let mut events = h.subscribe();
        h.send(UiCommand::AddAccount {
            network: NetworkKind::Threema,
            flow: "qr".into(),
        })
        .expect("add");
        // The steps the page stands after (a device name is never asked for): the QR code, and
        // for the emoji page the emoji too.
        let want = if at_emoji { 2 } else { 1 };
        let mut steps = Vec::new();
        h.runtime().block_on(async {
            let deadline = tokio::time::Instant::now() + WAIT;
            while steps.len() < want {
                let e = tokio::time::timeout_at(deadline, events.recv())
                    .await
                    .expect("timed out")
                    .expect("events");
                if let UiEvent::LoginStep { step, .. } = e {
                    match &step.kind {
                        LoginStepKind::Failed { error } => panic!("login failed: {error:?}"),
                        LoginStepKind::ConfirmEmoji { emoji } => {
                            assert_eq!(emoji, &kourier_fake::FAKE_EMOJI, "the fake's three");
                        }
                        _ => {}
                    }
                    steps.push(step.step_id);
                }
            }
            // And it stays there: a step that came now would be the link going on (the fake's
            // phone confirms the emoji of a plain link after `EMOJI_FOR`, so the wait is longer).
            let more = tokio::time::timeout(kourier_fake::EMOJI_FOR * 2, async {
                loop {
                    if let UiEvent::LoginStep { step, .. } = events.recv().await.expect("events") {
                        return step.step_id;
                    }
                }
            })
            .await;
            assert!(more.is_err(), "the held link went on to {more:?}");
        });
        assert_eq!(
            steps,
            if at_emoji {
                vec!["qr", "emoji"]
            } else {
                vec!["qr"]
            }
        );
        app.shutdown();
    }
}

/// The fixture whose account is a Telegram account (the fake plays one, KT-1b) is the plain
/// fixture in every chat too, with Telegram's network on in the preferences, nothing off, and
/// nothing said about missing parts (the fake's Telegram lacks nothing).
#[test]
fn the_telegram_fixture_has_the_plain_fixtures_chats_row_for_row() {
    let plain = App::start(fixture(false)).expect("start");
    let plain_chats = chats_of(&plain.handle());
    assert_eq!(plain_chats.len(), 20, "{plain_chats:#?}");
    drop(plain);

    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            link: false,
            messages_per_conversation: 60,
            network: FixtureNetwork::Telegram,
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start");
    let h = app.handle();
    assert_eq!(chats_of(&h), plain_chats);
    let account = first_account(&h);
    assert_eq!(account.network, NetworkKind::Telegram);
    assert_eq!(account.state, AccountState::Active);
    assert_eq!(h.networks().len(), 1);
    assert_eq!(h.networks()[0].kind, NetworkKind::Telegram);
    assert!(h.networks()[0].experimental && !h.networks()[0].history_transfer);
    // The flag the descriptor states reaches the UI's network row (Settings' second line).
    assert!(h.networks()[0].receipts_always_sent);
    assert!(h.networks_off().is_empty());
    assert!(h.desktop().prefs().network_on(NetworkKind::Telegram));
    assert!(!h.desktop().prefs().network_on(NetworkKind::WhatsApp));
    assert_eq!(h.telegram_missing(), None);
    assert!(h.networks_missing().is_empty());
}

/// The OFF Telegram is listed for the page `tgwarn` only (LC-KT1b-5): the plain fixture lists the
/// OFF WhatsApp and nothing else, with or without the option; no other network's fixture lists it.
/// Mutants: the off list always gets Telegram; the option is ignored.
#[test]
fn the_off_telegram_is_listed_only_for_its_page() {
    for telegram_off in [false, true] {
        let app = App::start(StartOptions {
            launch: Launch::Fixture(FixtureOptions {
                link: true,
                messages_per_conversation: 20,
                telegram_off,
                ..FixtureOptions::default()
            }),
            ..StartOptions::default()
        })
        .expect("start");
        let h = app.handle();
        let off: Vec<NetworkKind> = h.networks_off().iter().map(|n| n.kind).collect();
        assert_eq!(
            off,
            if telegram_off {
                vec![NetworkKind::WhatsApp, NetworkKind::Telegram]
            } else {
                vec![NetworkKind::WhatsApp]
            },
            "telegram_off={telegram_off}"
        );
        let telegram = h
            .networks_off()
            .iter()
            .find(|n| n.kind == NetworkKind::Telegram);
        if let Some(telegram) = telegram {
            // What the Welcome row and its warning page are made of: the descriptor's words, the
            // flag of Settings' second line, the experimental mark and no history source.
            assert!(telegram.experimental && telegram.receipts_always_sent);
            assert!(telegram.risk_notice.is_some());
            assert_eq!(telegram.flows.len(), 2, "QR and phone number");
        }
        // The fixture's own account is no Telegram's, and no Telegram is missing anything.
        assert_eq!(h.networks()[0].kind, NetworkKind::Fake);
        assert!(h.networks_missing().is_empty());
        app.shutdown();
    }
}

/// A fixture says what the build lacks of Telegram only when it is asked to (the window tests'
/// knob): the handle then names Telegram with the reason, and without the knob nothing is named
/// (no picture has a missing Telegram). Mutant: the knob is not carried into the setup.
#[test]
fn a_fixture_says_what_the_build_lacks_of_telegram_only_when_asked() {
    use kourier_app::TelegramMissing::{ApiKey, Library};
    for (asked, want) in [
        (None, Vec::new()),
        (Some(ApiKey), vec![(NetworkKind::Telegram, ApiKey)]),
        (Some(Library), vec![(NetworkKind::Telegram, Library)]),
    ] {
        let app = App::start(StartOptions {
            launch: Launch::Fixture(FixtureOptions {
                link: true,
                messages_per_conversation: 20,
                telegram_missing: asked,
                ..FixtureOptions::default()
            }),
            ..StartOptions::default()
        })
        .expect("start");
        let h = app.handle();
        assert_eq!(h.telegram_missing(), asked);
        assert_eq!(h.networks_missing(), want);
        app.shutdown();
    }
}

/// The held links of the pages `tgqr`, `tgcode` and `tgpassword` (KT-1b): all start from the
/// welcome page with no account and link as Telegram, asking no device name. The first stands at
/// its QR code. The second takes the phone-number way and stands at the CODE step: the typed
/// number is the fixture's row, the field `code` states it was sent through the Telegram app and
/// that a new one may be asked for at once (wait 0). The third types the code and stands at the
/// PASSWORD step with the owner's hint as the field's note. None goes on.
/// Mutants: the Telegram arm of `ask_device_name` removed (the first step is `device-name`); the
/// delivery's wait 1 s; the row without its password.
#[test]
fn a_held_telegram_link_stands_at_its_qr_code_its_code_or_its_password() {
    use kourier_model::{CodeDelivery, CodeResend, CodeVia};

    for stop in ["qr", "code", "password"] {
        let app = App::start(StartOptions {
            launch: Launch::Fixture(FixtureOptions {
                link: true,
                hold_link: true,
                network: FixtureNetwork::Telegram,
                ..FixtureOptions::default()
            }),
            ..StartOptions::default()
        })
        .expect("start");
        let h = app.handle();
        assert!(
            h.networks().iter().all(|n| n.kind == NetworkKind::Telegram),
            "the welcome page offers Telegram alone"
        );
        let mut events = h.subscribe();
        let flow = if stop == "qr" { "qr" } else { "phone" };
        h.send(UiCommand::AddAccount {
            network: NetworkKind::Telegram,
            flow: flow.into(),
        })
        .expect("add");
        let mut seen: Vec<String> = Vec::new();
        h.runtime().block_on(async {
            let deadline = tokio::time::Instant::now() + WAIT;
            loop {
                let e = tokio::time::timeout_at(deadline, events.recv())
                    .await
                    .expect("timed out")
                    .expect("events");
                let UiEvent::LoginStep { account, step } = e else {
                    continue;
                };
                seen.push(step.step_id.clone());
                match step.kind {
                    LoginStepKind::Failed { error } => panic!("login failed: {error:?}"),
                    LoginStepKind::ShowQr { .. } => {
                        assert_eq!(stop, "qr", "a QR code beyond the page's");
                        break;
                    }
                    LoginStepKind::Input { fields } => {
                        assert_eq!(fields.len(), 1, "{fields:?}");
                        let field = &fields[0];
                        let typed = match field.id.as_str() {
                            "phone" => FixtureOptions::TELEGRAM_NUMBER,
                            "code" => {
                                assert_eq!(
                                    field.code,
                                    Some(CodeDelivery {
                                        via: CodeVia::App,
                                        resend: Some(CodeResend {
                                            after: Duration::ZERO,
                                            via: CodeVia::Sms
                                        }),
                                    }),
                                    "the code was sent through the app, a new one may come at once"
                                );
                                if stop == "code" {
                                    break;
                                }
                                FixtureOptions::TELEGRAM_CODE
                            }
                            "password" => {
                                assert_eq!(stop, "password");
                                assert_eq!(
                                    field.note.as_ref().and_then(|n| n.explanation.as_deref()),
                                    Some(FixtureOptions::TELEGRAM_HINT),
                                    "the owner's hint"
                                );
                                assert_eq!(field.kind, kourier_model::InputKind::Secret);
                                break;
                            }
                            other => panic!("a field {other:?} (a device name is never asked)"),
                        };
                        h.send(UiCommand::SubmitLoginInput {
                            account,
                            txn: step.txn,
                            values: vec![(field.id.clone(), Redacted::new(typed.to_owned()))],
                        })
                        .expect("submit");
                    }
                    _ => {}
                }
            }
            // And it stays there: a step that came now would be the link going on.
            let more = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let UiEvent::LoginStep { step, .. } = events.recv().await.expect("events") {
                        return step.step_id;
                    }
                }
            })
            .await;
            assert!(more.is_err(), "the held link went on to {more:?}");
        });
        let want: &[&str] = match stop {
            "qr" => &["qr"],
            "code" => &["phone", "code"],
            _ => &["phone", "code", "password"],
        };
        assert_eq!(seen, want, "{stop}");
        app.shutdown();
    }
}

/// U5: the fake that plays Threema shows ONE QR code for the whole link, as the real actor does,
/// whatever `qr_codes` asks for (the Signal-like fake rotates its three). The link then goes on
/// to the emoji and is done. Mutant: the `else if threema { (1, f.qr_valid_for) }` arm of `demo_network` removed.
#[test]
fn an_unheld_threema_link_shows_one_qr_code_whatever_qr_codes_says() {
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            link: true,
            qr_valid_for: Duration::from_millis(200),
            qr_codes: 3,
            messages_per_conversation: 20,
            network: FixtureNetwork::Threema,
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start");
    let h = app.handle();
    let mut events = h.subscribe();
    h.send(UiCommand::AddAccount {
        network: NetworkKind::Threema,
        flow: "qr".into(),
    })
    .expect("add");
    let mut codes = Vec::new();
    let mut emoji = false;
    let mut done = false;
    h.runtime().block_on(async {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !done {
            let e = tokio::time::timeout_at(deadline, events.recv())
                .await
                .expect("timed out")
                .expect("events");
            if let UiEvent::LoginStep { step, .. } = e {
                match step.kind {
                    LoginStepKind::ShowQr { data, .. } => codes.push(data.expose().clone()),
                    LoginStepKind::ConfirmEmoji { .. } => emoji = true,
                    LoginStepKind::Done => done = true,
                    LoginStepKind::Failed { error } => panic!("login failed: {error:?}"),
                    _ => {}
                }
            }
        }
    });
    assert_eq!(codes.len(), 1, "one code, not rotating: {codes:?}");
    assert!(emoji, "the link goes on to the emoji");
    app.shutdown();
}

/// The titles of the chats in the list's order: what moves when a chat is added or one gets a
/// newer message.
fn titles_of(h: &kourier_app::AppHandle) -> Vec<String> {
    h.runtime().block_on(async {
        match h
            .query(Query::Conversations(ConversationListQuery::default()))
            .await
            .expect("conversations")
        {
            QueryData::Conversations(p) => p
                .rows
                .into_iter()
                .map(|r| format!("{:?}", unmarked(r.title.as_deref())))
                .collect(),
            other => panic!("unexpected {other:?}"),
        }
    })
}

/// The first account of the profile.
fn first_account(h: &kourier_app::AppHandle) -> kourier_app::facade::AccountRow {
    h.runtime().block_on(async {
        match h.query(Query::Accounts).await.expect("accounts") {
            QueryData::Accounts(a) => a.into_iter().next().expect("an account"),
            other => panic!("unexpected {other:?}"),
        }
    })
}

/// The newest row of a chat's timeline.
fn newest_row(h: &kourier_app::AppHandle, chat: ConversationId) -> kourier_app::facade::MessageRow {
    h.runtime().block_on(async {
        match h
            .query(Query::Timeline(TimelineQuery {
                conv: chat,
                anchor: TimelineAnchor::Newest,
                limit: 5,
            }))
            .await
            .expect("timeline")
        {
            QueryData::Timeline(t) => t
                .rows
                .into_iter()
                .max_by_key(|r| r.sort_key)
                .expect("a row"),
            other => panic!("unexpected {other:?}"),
        }
    })
}

/// The pages of WhatsApp's own states (K-A, `--page=waban`, `walock`, `wahistory`, `wafailed`,
/// `waoff`; W4: `walost`) put their state into the fixture account and add no chat and move none: the list
/// has the plain fixture's chats in its order, so no scroll-bar thumb of a wide picture moves.
/// Each state is what the page is named for: a ban for 23 more hours with the network's own
/// words and an https page to appeal on, the reach-out lock for three days, a history that is
/// 40 % there with more to come, an own message refused as a first contact that offers no
/// "Send Again", and a network that is turned off and not registered.
#[test]
fn the_pages_of_whatsapps_own_states_put_their_state_into_the_plain_fixtures_chats() {
    use kourier_app::FixtureWhatsApp::{Banned, Filling, Locked, Lost, Refused};
    use kourier_backend_api::BackendNotice;
    use kourier_model::{
        AccountHold, BanReason, ConnState, FailReason, HoldReason, OutgoingState, Restriction,
    };

    let plain = App::start(fixture(false)).expect("start");
    let plain_titles = titles_of(&plain.handle());
    assert_eq!(plain_titles.len(), 20, "{plain_titles:#?}");
    drop(plain);

    let start = |network, whatsapp| {
        App::start(StartOptions {
            launch: Launch::Fixture(FixtureOptions {
                link: false,
                messages_per_conversation: 60,
                network,
                whatsapp,
                ..FixtureOptions::default()
            }),
            ..StartOptions::default()
        })
        .expect("start")
    };

    // A temporary ban: the connection is restricted until 23 hours after the fixture's "now".
    let app = start(FixtureNetwork::WhatsApp, Some(Banned));
    let h = app.handle();
    assert_eq!(titles_of(&h), plain_titles, "ban");
    let now = h.fixture().expect("fixture").now_ms;
    let account = first_account(&h);
    let ConnState::Restricted {
        until_ms,
        kind,
        note,
        ..
    } = account.conn
    else {
        panic!("not restricted: {:?}", account.conn);
    };
    assert_eq!(until_ms, u64::try_from(now + 23 * 3_600_000).ok());
    assert_eq!(kind, Restriction::Banned(BanReason::TooManyStrangers));
    assert!(note.explanation.is_some());
    assert!(
        note.appeal_url
            .as_deref()
            .is_some_and(|u| u.starts_with("https://")),
        "{note:?}"
    );
    drop(app);

    // The reach-out lock: the connection stays up, the hold ends three days after "now".
    let app = start(FixtureNetwork::WhatsApp, Some(Locked));
    let h = app.handle();
    // (An own message waits in a chat of the list's lower body, which makes it the newest of
    // the chats that are not pinned: that one chat moves up, the others keep their order.)
    let mut titles = titles_of(&h);
    assert_eq!(titles.len(), plain_titles.len(), "lock");
    let at = titles
        .iter()
        .zip(&plain_titles)
        .position(|(now, plain)| now != plain)
        .expect("the chat of the message moved");
    let moved = titles.remove(at);
    let was = plain_titles
        .iter()
        .position(|t| *t == moved)
        .expect("known");
    assert!(was > at, "{moved} moved up from {was} to {at}");
    let mut others = plain_titles.clone();
    others.remove(was);
    assert_eq!(titles, others, "lock");
    let chat = h
        .fixture()
        .and_then(|f| f.whatsapp_conversation)
        .expect("the chat of the page");
    let newest = newest_row(&h, chat);
    assert!(newest.outbox.is_some(), "the message waits: {newest:?}");
    assert_eq!(newest.out_state, Some(OutgoingState::Queued), "{newest:?}");
    let now = h.fixture().expect("fixture").now_ms;
    let account = first_account(&h);
    assert!(account.conn.can_send(), "{:?}", account.conn);
    assert_eq!(
        account.hold,
        Some(AccountHold {
            until_ms: u64::try_from(now + 3 * 24 * 3_600_000).ok(),
            reason: HoldReason::ReachOutLocked,
        })
    );
    drop(app);

    // A history that is 40 % there: batches that say more follow, one a second.
    let app = start(FixtureNetwork::WhatsApp, Some(Filling));
    let h = app.handle();
    let mut events = h.subscribe();
    assert_eq!(titles_of(&h), plain_titles, "history");
    h.runtime().block_on(async {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let e = tokio::time::timeout_at(deadline, events.recv())
                .await
                .expect("no batch of the history came")
                .expect("events");
            if let UiEvent::HistoryProgress {
                progress, has_more, ..
            } = e
            {
                assert!(has_more);
                assert_eq!(progress, Some(0.4));
                break;
            }
        }
    });
    drop(app);

    // Part of the pushed history lost: the backend's notice reaches the front-end, with its
    // count, and the page adds no chat (the viewmodel tests hold the line that shows it).
    let app = start(FixtureNetwork::WhatsApp, Some(Lost));
    let h = app.handle();
    let mut events = h.subscribe();
    assert_eq!(titles_of(&h), plain_titles, "lost");
    h.runtime().block_on(async {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let e = tokio::time::timeout_at(deadline, events.recv())
                .await
                .expect("no notice of the lost history came")
                .expect("events");
            if let UiEvent::Notice { notice, .. } = e {
                assert_eq!(notice, BackendNotice::HistoryLost { count: 2 });
                break;
            }
        }
    });
    drop(app);

    // An own message refused as a first contact is the newest row of a chat that exists.
    let app = start(FixtureNetwork::WhatsApp, Some(Refused));
    let h = app.handle();
    assert_eq!(titles_of(&h), plain_titles, "refused");
    let chat = h
        .fixture()
        .and_then(|f| f.whatsapp_conversation)
        .expect("the chat of the page");
    let newest = newest_row(&h, chat);
    assert_eq!(
        newest.out_state,
        Some(OutgoingState::Failed {
            reason: FailReason::ReachOutRefused,
            retryable: false,
        })
    );
    assert!(!newest.actions.retry, "no Send Again for a first contact");
    drop(app);

    // Turned off: the account is there, its network is off the list of the registered ones.
    let app = start(FixtureNetwork::WhatsAppOff, None);
    let h = app.handle();
    assert_eq!(titles_of(&h), plain_titles, "off");
    assert_eq!(first_account(&h).network, NetworkKind::WhatsApp);
    assert!(h.networks().is_empty(), "{:?}", h.networks());
    assert_eq!(h.networks_off().len(), 1);
    assert_eq!(h.networks_off()[0].kind, NetworkKind::WhatsApp);
    assert!(!h.desktop().prefs().network_on(NetworkKind::WhatsApp));
}

/// A request to restart is recorded on the handle, for `main` to act on after the app shut
/// down in order; a fixture run never starts another process.
#[test]
fn a_restart_request_is_recorded_on_the_handle() {
    let app = App::start(fixture(false)).expect("start");
    let h = app.handle();
    assert!(!h.restart_requested());
    h.request_restart();
    assert!(h.restart_requested() && app.handle().restart_requested());
}

/// The profile of the pages of blended chats (`FixtureOptions::accounts`, `--page=accounts`,
/// `accountsdrawer`): three accounts that are linked and online (the fixture account, a second
/// account of the test network and a REGISTERED fake that plays WhatsApp, its network on in the
/// preferences and nothing off), each with chats of its own, one pinned in each of the second and
/// the third, the unread messages where the page counts them, and every extra chat older than the
/// fixture account's newest. Nothing here may put an account into a state that has a banner:
/// every connection is online and no history transfer runs (the WhatsApp fake pushes its history
/// at a link: it is a batch of the account's script, not a transfer, and the account must be
/// settled before the page opens), so the pages show no banner. The first account's chats are
/// those of the plain fixture, row for row.
#[test]
fn the_blended_pages_hold_three_linked_accounts_with_chats_and_no_banner() {
    use kourier_model::ConnState;
    let plain = App::start(fixture(false)).expect("start");
    let plain_chats = chats_of(&plain.handle());
    drop(plain);

    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            link: false,
            messages_per_conversation: 60,
            accounts: 3,
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start");
    let h = app.handle();
    let (accounts, rows) = h.runtime().block_on(async {
        let accounts = match h.query(Query::Accounts).await.expect("accounts") {
            QueryData::Accounts(a) => a,
            other => panic!("unexpected {other:?}"),
        };
        let rows = match h
            .query(Query::Conversations(ConversationListQuery {
                limit: 500,
                ..ConversationListQuery::default()
            }))
            .await
            .expect("conversations")
        {
            QueryData::Conversations(p) => p.rows,
            other => panic!("unexpected {other:?}"),
        };
        (accounts, rows)
    });
    // Three accounts, in the order the panel lists them, all linked.
    let kinds: Vec<NetworkKind> = accounts.iter().map(|a| a.network).collect();
    assert_eq!(
        kinds,
        [NetworkKind::Fake, NetworkKind::Fake, NetworkKind::WhatsApp]
    );
    for a in &accounts {
        assert_eq!(a.state, AccountState::Active, "{:?}", a.network);
        // (Online, or settling toward it: none is in a state that has a banner.)
        assert!(
            matches!(
                a.conn,
                ConnState::Online { .. } | ConnState::Connecting | ConnState::Syncing { .. }
            ),
            "{:?}: {:?}",
            a.network,
            a.conn
        );
        assert!(a.hold.is_none(), "{:?}", a.network);
    }
    // The networks: both are registered and on, nothing is off.
    assert_eq!(h.networks().len(), 2);
    assert!(h.networks_off().is_empty());
    assert!(h.desktop().prefs().network_on(NetworkKind::WhatsApp));

    // The chats: the first account's are the plain fixture's (20); the second has five, the
    // third three, one pinned in each, unread where the pages count them.
    let of = |n: usize| -> Vec<&kourier_app::facade::ConversationRow> {
        rows.iter()
            .filter(|r| r.account == accounts[n].id)
            .collect()
    };
    let (first, second, third) = (of(0), of(1), of(2));
    assert_eq!(
        first.len(),
        plain_chats.len(),
        "the fixture account's chats"
    );
    assert_eq!((second.len(), third.len()), (5, 3));
    assert_eq!(
        second.iter().filter(|r| r.pinned_order.is_some()).count(),
        1
    );
    assert_eq!(third.iter().filter(|r| r.pinned_order.is_some()).count(), 1);
    assert_eq!(second.iter().map(|r| r.unread).sum::<u32>(), 3);
    assert_eq!(third.iter().map(|r| r.unread).sum::<u32>(), 1);
    assert!(second.iter().chain(&third).all(|r| !r.archived));
    // Older than the fixture account's newest chat, and spread among its chats (not all newer
    // or older than the lot): the blended list interleaves.
    let newest = first.iter().map(|r| r.last_activity).max().expect("chats");
    let oldest = first.iter().map(|r| r.last_activity).min().expect("chats");
    for r in second.iter().chain(&third) {
        assert!(r.last_activity < newest, "{:?}", r.title);
        assert!(r.last_activity > oldest, "{:?}", r.title);
    }
}

/// S3 (2026-10-28): a fixture page whose window sends a message (`--page=media`: QML presses
/// Send for the photo) is stamped by the core's clock at that moment, some seconds after the
/// start. The clock used to start at the fixture's "now", whose second of the minute is a fixed
/// number, so the minute under the message was 05:23 in a render that sent within a few
/// seconds and 05:24 in one that took longer (the picture `media-narrow-dark` flapped between
/// them). With `window_sends` the clock starts five seconds into the minute of "now": the
/// stamp is in that minute whatever the start took, and the chat list's label with it.
#[test]
fn a_page_whose_window_sends_is_stamped_in_the_minute_of_now() {
    const MINUTE: i64 = 60_000;
    let began = Instant::now();
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 60,
            window_sends: true,
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start");
    let h = app.handle();
    let info = h.fixture().expect("fixture info");
    let now = info.now_ms;
    let chat = info.media_conversation.expect("the media chat");
    let at = h.core_now_ms();
    let took = i64::try_from(began.elapsed().as_millis()).expect("ms");
    // The clock stands five seconds into the minute of "now", plus what has passed since it
    // was made (the start; a slack for the read itself).
    let into_minute = at.rem_euclid(MINUTE);
    assert_eq!(at.div_euclid(MINUTE), now.div_euclid(MINUTE), "{at} {now}");
    assert!(
        (5_000..5_000 + took + 500).contains(&into_minute),
        "the core's clock reads {into_minute} ms into the minute, the start took {took} ms"
    );
    // A message sent now (what the window's Send does) carries a time in that minute.
    h.send(UiCommand::SendText {
        conv: chat,
        text: kourier_model::Text::plain("Sent from the window"),
        reply_to: None,
    })
    .expect("send");
    let deadline = Instant::now() + WAIT;
    let sent = loop {
        let row = newest_row(&h, chat);
        if row.sent_at > now - 2 * MINUTE {
            break row;
        }
        assert!(Instant::now() < deadline, "the message never appeared");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        sent.sent_at.div_euclid(MINUTE),
        now.div_euclid(MINUTE),
        "{} {now}",
        sent.sent_at
    );
    assert!(sent.sent_at.rem_euclid(MINUTE) >= 5_000, "{}", sent.sent_at);
}

//! The app without Qt: fixture mode starts the real core over the fake network in a temporary
//! profile, serves queries from `core.db`, links through rotating QR codes, and shuts down in
//! order.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use kourier_app::facade::{
    ConversationListQuery, Query, QueryData, TimelineAnchor, TimelineQuery, UiCommand, UiEvent,
};
use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_model::{AccountState, LoginStepKind, NetworkKind, Redacted};

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
    let info = h.fixture().expect("fixture info");
    let demo = info.demo_conversation.expect("a group");
    assert_eq!(h.networks().len(), 1);
    assert_eq!(h.networks()[0].kind, NetworkKind::Fake);
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

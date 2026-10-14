//! `CoreSource` against a simulated core: query generations with shuffled completions,
//! coalescing, paging, login, presence and notices.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use kourier_core::facade::{
    AccountRow as CoreAccount, ActionSubject, ConversationDetail, ConversationPage,
    ConversationRow as CoreConversation, FileRefusal, MessageRow as CoreMessage, OriginKind,
    Preview, Query, QueryData, QueryToken, RefusedFile, TimelineAnchor, TimelinePage, UiCommand,
    UiEvent,
};
use kourier_model::{
    AccountId, AccountState, AccountUuid, Body, ConnState, ConvKind, ConversationFeatures,
    ConversationId, InputField, InputKind, LoginStep, LoginStepKind, MessageId, NetworkKind,
    ReadState, Redacted, Text, UserId,
};
use proptest::prelude::*;

use super::*;
use crate::login::network_choices;
use crate::time::FixedOffset;
use crate::timeline::{RowKey, RowKind};

pub(super) const NOW: i64 = 1_760_000_000_000;

pub(super) type Sent = Rc<RefCell<Vec<UiCommand>>>;

pub(super) fn source() -> (CoreSource, Sent) {
    let sent: Sent = Rc::default();
    let s2 = sent.clone();
    let mut networks = network_choices();
    networks.push(NetworkChoice {
        kind: NetworkKind::Fake,
        name: "Test network".into(),
        description: String::new(),
        icon: "fake".into(),
        available: true,
        history_on_link: true,
    });
    let src = CoreSource::new(
        Box::new(move |c: UiCommand| {
            s2.borrow_mut().push(c);
            true
        }),
        CoreSourceConfig {
            now: Box::new(|| NOW),
            calendar: Box::new(FixedOffset(0)),
            networks,
            hostname: "workstation".into(),
            demo: None,
        },
    );
    (src, sent)
}

pub(super) fn user(n: u32) -> UserId {
    UserId::parse(&format!("aci:00000000-0000-4000-8000-{n:012x}")).expect("id")
}

pub(super) fn account() -> CoreAccount {
    CoreAccount {
        id: AccountId(1),
        uuid: AccountUuid::new_random(),
        network: NetworkKind::Fake,
        state: AccountState::Active,
        display_name: Some("Me".into()),
        conn: ConnState::Connecting,
        own_device: Some(2),
        avatar: None,
        number: None,
        linked_at: 0,
    }
}

pub(super) fn conv_row(id: i64, title: &str, last: i64) -> CoreConversation {
    CoreConversation {
        id: ConversationId(id),
        account: AccountId(1),
        kind: ConvKind::Group,
        is_self: false,
        title: Some(title.into()),
        peer: None,
        avatar: None,
        preview: Some(Preview {
            message: MessageId(id * 1000),
            author: Some(user(1)),
            author_name: Some("Ari Arden".into()),
            body_kind: "Text".into(),
            text: Some(format!("last of {title}")),
            sent_at: NOW - 60_000,
            deleted: false,
            out_state: None,
            attachment: None,
            attachments: 0,
            file_name: None,
            system: None,
            kind_hint: None,
        }),
        last_activity: last,
        unread: 0,
        mentions: 0,
        marked_unread: false,
        muted_until: None,
        local_muted_until: None,
        archived: false,
        pinned_order: None,
        blocked: false,
        request_pending: false,
        features_hash: 0,
        has_draft: false,
        hidden: false,
    }
}

pub(super) fn message(conv: i64, n: i64, unread: bool) -> CoreMessage {
    let remote = n % 3 != 0;
    CoreMessage {
        id: MessageId(conv * 100_000 + n),
        sort_key: n * 10,
        author: remote.then(|| user(1)),
        author_name: remote.then(|| Redacted::new("Ari Arden".to_owned())),
        author_avatar: None,
        origin: if remote {
            OriginKind::Remote
        } else {
            OriginKind::OwnOtherDevice
        },
        device: None,
        body: Body::Text(Text {
            body: format!("{conv}:{n}"),
            ranges: Vec::new(),
        }),
        reply: None,
        sent_at: NOW - 3_600_000 + n * 1000,
        server_at: None,
        received_at: NOW,
        revision: 0,
        edited_at: None,
        deleted: None,
        out_state: None,
        read_state: remote.then_some(if unread {
            ReadState::Unread
        } else {
            ReadState::Read
        }),
        reactions: Vec::new(),
        expiry: None,
        view_once: false,
        history: false,
        mentions_me: false,
        outbox: None,
        actions: Default::default(),
    }
}

/// A simulated `core.db`: a versioned conversation list and per-conversation messages.
#[derive(Clone, Default)]
pub(super) struct Store {
    pub(super) version: u32,
    pub(super) convs: BTreeMap<i64, Vec<CoreMessage>>,
    /// The detail's features, timer and the list rows' previews, when a test sets them.
    pub(super) features: ConversationFeatures,
    pub(super) timer: Option<Duration>,
}

impl Store {
    pub(super) fn new(convs: &[(i64, i64)]) -> Self {
        let mut s = Self::default();
        for (id, n) in convs {
            s.convs
                .insert(*id, (1..=*n).map(|i| message(*id, i, false)).collect());
        }
        s
    }

    fn title(&self, id: i64) -> String {
        format!("c{id} v{}", self.version)
    }

    pub(super) fn answer(&self, q: &Query) -> QueryData {
        match q {
            Query::Accounts => QueryData::Accounts(vec![account()]),
            Query::Conversations(_) => QueryData::Conversations(ConversationPage {
                rows: self
                    .convs
                    .iter()
                    .map(|(id, m)| conv_row(*id, &self.title(*id), m.len() as i64))
                    .collect(),
                next: None,
            }),
            Query::Conversation(id) => {
                QueryData::Conversation(Some(Box::new(ConversationDetail {
                    row: conv_row(id.0, &self.title(id.0), 0),
                    members: Vec::new(),
                    features: self.features.clone(),
                    timer: self.timer,
                    draft: None,
                })))
            }
            Query::Timeline(q) => {
                let all = self.convs.get(&q.conv.0).cloned().unwrap_or_default();
                let limit = q.limit as usize;
                let key = |m: &CoreMessage| (m.sort_key, m.id);
                let (rows, has_older, has_newer) = match q.anchor {
                    TimelineAnchor::Newest => {
                        let start = all.len().saturating_sub(limit);
                        (all[start..].to_vec(), start > 0, false)
                    }
                    TimelineAnchor::Before(c) => {
                        let older: Vec<_> = all
                            .iter()
                            .filter(|m| key(m) < (c.sort_key, c.id))
                            .cloned()
                            .collect();
                        let start = older.len().saturating_sub(limit);
                        (
                            older[start..].to_vec(),
                            start > 0,
                            all.iter().any(|m| key(m) >= (c.sort_key, c.id)),
                        )
                    }
                    TimelineAnchor::After(c) => {
                        let newer: Vec<_> = all
                            .iter()
                            .filter(|m| key(m) > (c.sort_key, c.id))
                            .cloned()
                            .collect();
                        let more = newer.len() > limit;
                        (
                            newer.into_iter().take(limit).collect(),
                            all.iter().any(|m| key(m) <= (c.sort_key, c.id)),
                            more,
                        )
                    }
                    TimelineAnchor::Around(id) => match all.iter().position(|m| m.id == id) {
                        Some(at) => {
                            let start = at.saturating_sub(limit / 2);
                            let end = (start + limit).min(all.len());
                            (all[start..end].to_vec(), start > 0, end < all.len())
                        }
                        None => (Vec::new(), false, false),
                    },
                };
                QueryData::Timeline(TimelinePage {
                    conv: q.conv,
                    rows,
                    has_older,
                    has_newer,
                    now: NOW,
                })
            }
            Query::Attachment(_) => QueryData::Attachment(None),
            Query::FilePlan(_) | Query::Revisions(_) => QueryData::Revisions(None),
            Query::ExpiryPolicy => QueryData::ExpiryPolicy(Default::default()),
        }
    }
}

pub(super) fn result(token: QueryToken, data: QueryData) -> UiEvent {
    UiEvent::QueryResult {
        token,
        result: Ok(data),
    }
}

/// Takes the queries sent so far.
pub(super) fn take_queries(sent: &Sent) -> Vec<(QueryToken, Query)> {
    let mut out = Vec::new();
    sent.borrow_mut().retain(|c| match c {
        UiCommand::Query { token, query } => {
            out.push((*token, query.clone()));
            false
        }
        _ => true,
    });
    out
}

/// Answers every query (and the follow-ups they cause) from `store`, in order.
pub(super) fn settle(src: &mut CoreSource, sent: &Sent, store: &Store) -> Changes {
    let mut ch = Changes::default();
    for _ in 0..100 {
        let qs = take_queries(sent);
        if qs.is_empty() {
            return ch;
        }
        for (t, q) in qs {
            ch.merge(src.on_event(&result(t, store.answer(&q))));
        }
    }
    panic!("queries do not settle");
}

pub(super) fn message_texts(rows: &[MessageRow]) -> Vec<String> {
    rows.iter()
        .filter(|r| r.kind == RowKind::Message)
        .filter_map(|r| match r.blocks.first() {
            Some(Block::Text(t)) => Some(t.plain.clone()),
            _ => None,
        })
        .collect()
}

/// The unread marker of the core's timeline and a call line: the core gives a line a read
/// state exactly while it counts as unread (a missed call), so a ringing or an answered line
/// is passed over, and a missed one is where the marker goes.
#[test]
fn the_unread_marker_counts_a_missed_call_and_no_other_call_line() {
    use kourier_model::{CallDirection, CallLine, CallMedia, CallState, SystemEvent};
    let line = |state: CallState, read_state: Option<ReadState>| {
        let mut m = message(1, 2, false);
        // As the core stores a line: no author, the origin says which way the call went.
        m.author = None;
        m.author_name = None;
        m.origin = OriginKind::Remote;
        m.body = Body::System(SystemEvent::Call(
            CallLine::signal(CallDirection::Incoming, false, state, NOW)
                .with_media(CallMedia::Voice, NOW),
        ));
        m.read_state = read_state;
        m
    };
    // Oldest first, as the window holds them: a read text, the line, an unread text.
    let rows = |call: CoreMessage| vec![message(1, 1, false), call, message(1, 4, true)];
    let text = MessageId(100_004);
    let call = MessageId(100_002);
    assert_eq!(
        first_unread(&rows(line(CallState::Ringing { deadline: NOW }, None))),
        Some(text)
    );
    assert_eq!(
        first_unread(&rows(line(CallState::Missed, Some(ReadState::Unread)))),
        Some(call)
    );
    assert_eq!(
        first_unread(&rows(line(CallState::Answered, None))),
        Some(text)
    );
    assert_eq!(
        first_unread(&rows(line(CallState::Missed, Some(ReadState::Read)))),
        Some(text)
    );
    // A missed call alone, newest in the chat.
    let alone = vec![
        message(1, 1, false),
        line(CallState::Missed, Some(ReadState::Unread)),
    ];
    assert_eq!(first_unread(&alone), Some(call));
    // Another notice is never unread, whatever its row says.
    let mut notice = line(CallState::Missed, Some(ReadState::Unread));
    notice.body = Body::System(SystemEvent::Joined);
    assert_eq!(first_unread(&[message(1, 1, false), notice]), None);
}

#[test]
fn tracker_generations_follow_ups_and_floor() {
    let mut t = QueryTracker::new(7);
    let a = t.request().expect("first");
    assert_eq!((a.model, a.generation), (7, 1));
    assert_eq!(t.request(), None, "one query in flight");
    assert_eq!(t.request(), None, "coalesced");
    assert_eq!(t.complete(a), Completion::Apply);
    let b = t.follow_up().expect("one follow-up for both invalidations");
    assert_eq!(t.follow_up(), None);
    assert_eq!(t.complete(b), Completion::Apply);
    assert_eq!(t.applied(), 2);
    // A switch: the old query's answer is dropped whenever it arrives.
    let c = t.request().expect("c");
    t.reset();
    let d = t.request().expect("d");
    assert_eq!(t.complete(d), Completion::Apply);
    assert_eq!(t.complete(c), Completion::Stale);
    // Never older than what is applied.
    assert_eq!(t.complete(b), Completion::Stale);
    assert_eq!(
        t.complete(QueryToken {
            model: 8,
            generation: 99
        }),
        Completion::Stale
    );
}

#[test]
fn first_load_and_coalesced_invalidations() {
    let (mut src, sent) = source();
    let store = Store::new(&[(1, 10), (2, 5)]);
    src.reload();
    let first = take_queries(&sent);
    assert_eq!(first.len(), 2, "accounts and the list");
    for (t, q) in first {
        let ch = src.on_event(&result(t, store.answer(&q)));
        assert!(ch.accounts && ch.conversations);
    }
    assert_eq!(src.accounts().len(), 1);
    assert_eq!(src.conversations(&ConversationFilter::default()).len(), 2);

    // Ten invalidations while the list query is in flight → one follow-up query.
    src.on_event(&UiEvent::ConversationsChanged {
        ids: vec![ConversationId(1)],
    });
    let inflight = take_queries(&sent);
    assert_eq!(inflight.len(), 1);
    for _ in 0..10 {
        src.on_event(&UiEvent::ConversationsChanged {
            ids: vec![ConversationId(2)],
        });
    }
    assert!(take_queries(&sent).is_empty(), "coalesced while in flight");
    let (t, q) = inflight[0].clone();
    src.on_event(&result(t, store.answer(&q)));
    let follow = take_queries(&sent);
    assert_eq!(follow.len(), 1, "exactly one follow-up");
    let (t, q) = follow[0].clone();
    src.on_event(&result(t, store.answer(&q)));
    assert!(take_queries(&sent).is_empty());
}

#[test]
fn a_read_asked_of_the_core_and_not_applied_is_in_flight() {
    let (mut src, sent) = source();
    let store = Store::new(&[(1, 10), (2, 5)]);
    assert_eq!(src.queries_in_flight(), 0);
    src.reload();
    let first = take_queries(&sent);
    assert_eq!(
        src.queries_in_flight(),
        2,
        "the accounts and the list are asked for"
    );
    for (t, q) in first {
        src.on_event(&result(t, store.answer(&q)));
    }
    assert_eq!(src.queries_in_flight(), 0, "everything applied");

    // An invalidation: the list is going to change until its answer is applied.
    src.on_event(&UiEvent::ConversationsChanged {
        ids: vec![ConversationId(1)],
    });
    let inflight = take_queries(&sent);
    assert_eq!(src.queries_in_flight(), 1);
    // A second one while it is in flight waits as a follow-up: still the one list.
    src.on_event(&UiEvent::ConversationsChanged {
        ids: vec![ConversationId(2)],
    });
    assert_eq!(src.queries_in_flight(), 1);
    let (t, q) = inflight[0].clone();
    src.on_event(&result(t, store.answer(&q)));
    // (The answer started the follow-up.)
    let follow = take_queries(&sent);
    assert_eq!(follow.len(), 1);
    assert_eq!(src.queries_in_flight(), 1, "the follow-up is in flight");
    let (t, q) = follow[0].clone();
    src.on_event(&result(t, store.answer(&q)));
    assert_eq!(src.queries_in_flight(), 0);
}

#[test]
fn timeline_pages_older_and_caps_the_window() {
    let (mut src, sent) = source();
    let store = Store::new(&[(1, 620)]);
    src.reload();
    settle(&mut src, &sent, &store);
    let ch = src.open_conversation(Some(ConversationId(1)));
    assert!(ch.timeline && ch.header);
    let qs = take_queries(&sent);
    assert!(
        qs.iter().any(|(_, q)| matches!(
            q,
            Query::Timeline(t) if t.anchor == TimelineAnchor::Newest && t.limit == TIMELINE_PAGE
        )),
        "the newest 50 first"
    );
    for (t, q) in qs {
        src.on_event(&result(t, store.answer(&q)));
    }
    assert_eq!(src.window_len(), 50);
    let rows = src.timeline(ConversationId(1));
    assert_eq!(rows.last().map(|r| r.kind), Some(RowKind::Loading));
    assert_eq!(
        message_texts(&rows).first().map(String::as_str),
        Some("1:620")
    );
    assert_eq!(
        src.chat(ConversationId(1)).map(|h| h.title),
        Some("c1 v0".into())
    );

    // Scrolling up pages older rows; a second request while one is in flight waits.
    assert!(src.fetch_older());
    assert!(src.fetch_older());
    let qs = take_queries(&sent);
    assert_eq!(qs.len(), 1, "one page at a time");
    assert!(
        matches!(&qs[0].1, Query::Timeline(t) if matches!(t.anchor, TimelineAnchor::Before(_)))
    );
    src.on_event(&result(qs[0].0, store.answer(&qs[0].1)));
    assert_eq!(src.window_len(), 100);
    let texts = message_texts(&src.timeline(ConversationId(1)));
    assert_eq!(texts.last().map(String::as_str), Some("1:521"));
    assert_eq!(texts.first().map(String::as_str), Some("1:620"));
    assert!(
        take_queries(&sent).is_empty(),
        "the queued request was the same page"
    );

    // Up to the window limit, then the newest rows are dropped and paged back in later.
    while src.paging().has_older {
        assert!(src.fetch_older());
        settle(&mut src, &sent, &store);
    }
    assert!(src.window_len() <= TIMELINE_WINDOW);
    assert!(src.paging().has_newer, "the newest rows were dropped");
    let texts = message_texts(&src.timeline(ConversationId(1)));
    assert_eq!(texts.last().map(String::as_str), Some("1:1"));
    while src.paging().has_newer {
        assert!(src.fetch_newer());
        settle(&mut src, &sent, &store);
    }
    let texts = message_texts(&src.timeline(ConversationId(1)));
    assert_eq!(texts.first().map(String::as_str), Some("1:620"));
    assert!(src.window_len() <= TIMELINE_WINDOW);
}

#[test]
fn reload_keeps_a_scrolled_window_in_place() {
    let (mut src, sent) = source();
    let mut store = Store::new(&[(1, 700)]);
    src.open_conversation(Some(ConversationId(1)));
    settle(&mut src, &sent, &store);
    while src.window_len() < TIMELINE_WINDOW {
        src.fetch_older();
        settle(&mut src, &sent, &store);
    }
    src.fetch_older();
    settle(&mut src, &sent, &store);
    assert!(src.paging().has_newer);
    let before = message_texts(&src.timeline(ConversationId(1)));
    store
        .convs
        .get_mut(&1)
        .expect("conv")
        .push(message(1, 701, true));
    src.on_event(&UiEvent::MessagesChanged {
        conv: ConversationId(1),
        ids: vec![MessageId(100_701)],
    });
    settle(&mut src, &sent, &store);
    assert_eq!(message_texts(&src.timeline(ConversationId(1))), before);
}

#[test]
fn a_burst_at_the_bottom_keeps_following_the_newest() {
    let (mut src, sent) = source();
    let mut store = Store::new(&[(1, 60)]);
    src.open_conversation(Some(ConversationId(1)));
    settle(&mut src, &sent, &store);
    assert_eq!(src.window_len(), 50);
    let list = store.convs.get_mut(&1).expect("conv");
    for n in 61..=200 {
        list.push(message(1, n, true));
    }
    src.on_event(&UiEvent::MessagesChanged {
        conv: ConversationId(1),
        ids: Vec::new(),
    });
    settle(&mut src, &sent, &store);
    let texts = message_texts(&src.timeline(ConversationId(1)));
    assert_eq!(texts.first().map(String::as_str), Some("1:200"));
    assert!(!src.paging().has_newer);
}

#[derive(Clone, Debug)]
enum Op {
    /// A new message in a conversation, with the event the core would send.
    Message(i64),
    /// Titles change (a new store version).
    Rename,
    /// The core answers one pending query now (it reads the store as it is).
    Answer(usize),
    /// One answered query is delivered (shuffled order).
    Deliver(usize),
    /// The user opens a conversation.
    Open(i64),
    /// The user scrolls up.
    Older,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (1i64..4).prop_map(Op::Message),
        Just(Op::Rename),
        (0usize..8).prop_map(Op::Answer),
        (0usize..8).prop_map(Op::Deliver),
        (1i64..4).prop_map(Op::Open),
        Just(Op::Older),
    ]
}

proptest! {
    /// §8.5: queries complete in any order; the source keeps one query in flight per model,
    /// never shows rows of a conversation it switched away from, never regresses the list,
    /// and ends equal to the store.
    #[test]
    fn shuffled_completions_converge(ops in proptest::collection::vec(op(), 1..120)) {
        let (mut src, sent) = source();
        let mut store = Store::new(&[(1, 80), (2, 30), (3, 3)]);
        let mut issued: Vec<(QueryToken, Query)> = Vec::new();
        let mut answered: Vec<(QueryToken, QueryData)> = Vec::new();
        let mut best_list_version = 0u32;
        src.reload();
        src.set_presence(true, true);
        for op in ops {
            issued.extend(take_queries(&sent));
            match op {
                Op::Message(c) => {
                    let list = store.convs.get_mut(&c).expect("conv");
                    let n = list.len() as i64 + 1;
                    list.push(message(c, n, true));
                    src.on_event(&UiEvent::MessagesChanged {
                        conv: ConversationId(c),
                        ids: vec![MessageId(c * 100_000 + n)],
                    });
                    src.on_event(&UiEvent::ConversationsChanged { ids: vec![ConversationId(c)] });
                }
                Op::Rename => {
                    store.version += 1;
                    src.on_event(&UiEvent::ConversationsChanged {
                        ids: vec![ConversationId(1), ConversationId(2), ConversationId(3)],
                    });
                }
                Op::Answer(i) if !issued.is_empty() => {
                    let (t, q) = issued.remove(i % issued.len());
                    answered.push((t, store.answer(&q)));
                }
                Op::Deliver(i) if !answered.is_empty() => {
                    let (t, data) = answered.remove(i % answered.len());
                    src.on_event(&result(t, data));
                }
                Op::Open(c) => {
                    src.open_conversation(Some(ConversationId(c)));
                }
                Op::Older => {
                    src.fetch_older();
                }
                _ => {}
            }
            issued.extend(take_queries(&sent));
            // One query in flight per model, counting only queries the source still waits
            // for (the accounts and list trackers never switch; the others may have stale
            // ones outstanding after a switch, which are dropped on arrival).
            for model in [MODEL_ACCOUNTS, MODEL_CONVERSATIONS] {
                let n = issued.iter().filter(|(t, _)| t.model == model).count()
                    + answered.iter().filter(|(t, _)| t.model == model).count();
                prop_assert!(n <= 1, "model {model}: {n} queries in flight");
            }
            // Only the open conversation's rows are shown.
            if let Some(open) = src.open() {
                let prefix = format!("{}:", open.0);
                for t in message_texts(&src.timeline(open)) {
                    prop_assert!(t.starts_with(&prefix), "{t} shown in {open:?}");
                }
            }
            // The list never goes back to an older version.
            let titles = src.conversations(&ConversationFilter::default());
            if let Some(v) = titles.first().and_then(|r| r.title.rsplit_once(" v")).and_then(|(_, v)| v.parse::<u32>().ok()) {
                prop_assert!(v >= best_list_version, "list regressed from v{best_list_version} to v{v}");
                best_list_version = v;
            }
        }
        // Drain: answer and deliver everything, follow-ups included; the view pages in rows
        // a burst left below the window.
        for _ in 0..100 {
            if src.paging().has_newer {
                src.fetch_newer();
            }
            issued.extend(take_queries(&sent));
            if issued.is_empty() && answered.is_empty() {
                break;
            }
            for (t, q) in issued.drain(..) {
                answered.push((t, store.answer(&q)));
            }
            for (t, data) in std::mem::take(&mut answered) {
                src.on_event(&result(t, data));
            }
        }
        prop_assert!(issued.is_empty() && answered.is_empty());
        let rows = src.conversations(&ConversationFilter::default());
        prop_assert_eq!(rows.len(), 3);
        for r in &rows {
            prop_assert_eq!(&r.title, &store.title(r.id.0));
        }
        if let Some(open) = src.open() {
            let all = &store.convs[&open.0];
            let texts = message_texts(&src.timeline(open));
            let newest = format!("{}:{}", open.0, all.len());
            prop_assert_eq!(texts.first(), Some(&newest), "the newest message is shown");
            prop_assert_eq!(src.chat(open).map(|h| h.title), Some(store.title(open.0)));
        }
    }
}

#[test]
fn login_walks_input_qr_and_done() {
    let (mut src, sent) = source();
    assert!(
        src.start_login(NetworkKind::Threema).is_err(),
        "not available"
    );
    let view = src.start_login(NetworkKind::Fake).expect("start");
    assert_eq!(view.kind, LoginViewKind::Waiting);
    assert!(matches!(
        sent.borrow().last(),
        Some(UiCommand::AddAccount { network: NetworkKind::Fake, flow }) if flow == "qr"
    ));
    let account = AccountId(5);
    src.on_event(&UiEvent::AccountAdded { account });
    let step = |kind, txn| UiEvent::LoginStep {
        account,
        step: LoginStep {
            step_id: "x".into(),
            txn,
            kind,
        },
    };
    let ch = src.on_event(&step(
        LoginStepKind::Input {
            fields: vec![InputField {
                id: "device-name".into(),
                label: "Device name".into(),
                kind: InputKind::Text,
                default: Some("kourier".into()),
                required: true,
            }],
        },
        1,
    ));
    assert!(ch.login);
    let v = src.login_view().expect("view");
    assert_eq!(v.kind, LoginViewKind::Input);
    assert_eq!(
        v.fields[0].value, "workstation",
        "prefilled with the host name"
    );
    src.submit_login(vec![("device-name".into(), "desk".into())])
        .expect("submit");
    assert!(matches!(
        sent.borrow().last(),
        Some(UiCommand::SubmitLoginInput { account: a, txn: 1, values })
            if *a == account && values[0].1.expose() == "desk"
    ));
    for (i, code) in ["fake-link://a/0", "fake-link://a/1"].iter().enumerate() {
        src.on_event(&step(
            LoginStepKind::ShowQr {
                data: Redacted::new((*code).to_owned()),
                valid_for: Some(Duration::from_secs(5)),
            },
            2 + i as u64,
        ));
        assert_eq!(src.login_view().map(|v| v.qr), Some((*code).to_owned()));
    }
    // Steps of another account are not ours.
    src.on_event(&UiEvent::LoginStep {
        account: AccountId(9),
        step: LoginStep {
            step_id: "done".into(),
            txn: 0,
            kind: LoginStepKind::Done,
        },
    });
    assert_eq!(src.login_view().map(|v| v.kind), Some(LoginViewKind::Qr));
    sent.borrow_mut().clear();
    src.on_event(&step(LoginStepKind::Done, 0));
    assert_eq!(src.login_view().map(|v| v.kind), Some(LoginViewKind::Done));
    let reloads = take_queries(&sent);
    assert!(
        reloads
            .iter()
            .any(|(_, q)| matches!(q, Query::Conversations(_))),
        "the list reloads (the accounts query is still in flight)"
    );
    // Cancelling a finished login sends nothing.
    src.cancel_login();
    assert!(sent.borrow().is_empty());
    assert!(src.login_view().is_none());
}

#[test]
fn cancel_and_failure() {
    let (mut src, sent) = source();
    src.start_login(NetworkKind::Fake).expect("start");
    src.on_event(&UiEvent::AccountAdded {
        account: AccountId(3),
    });
    src.cancel_login();
    assert!(matches!(
        sent.borrow().last(),
        Some(UiCommand::CancelLogin { account }) if *account == AccountId(3)
    ));
    src.start_login(NetworkKind::Fake).expect("again");
    src.on_event(&UiEvent::AccountAdded {
        account: AccountId(4),
    });
    src.on_event(&UiEvent::LoginStep {
        account: AccountId(4),
        step: LoginStep {
            step_id: "failed".into(),
            txn: 9,
            kind: LoginStepKind::Failed {
                error: kourier_model::BackendError::new(
                    kourier_model::ErrorKind::Expired,
                    "expired",
                ),
            },
        },
    });
    let v = src.login_view().expect("view");
    assert_eq!(v.kind, LoginViewKind::Failed);
    assert!(v.retry);
}

#[test]
fn send_notice_drafts_and_typing() {
    let (mut src, sent) = source();
    let conv = ConversationId(1);
    assert_eq!(src.send_text(conv, "  \n", None), Err(SourceError::Empty));
    src.save_draft(conv, "half a thought");
    assert_eq!(src.draft(conv), "half a thought");
    src.save_draft(conv, "half a thought");
    src.set_typing(conv, true);
    src.set_typing(conv, true);
    assert_eq!(src.send_text(conv, "hello", None), Ok(None));
    let cmds = std::mem::take(&mut *sent.borrow_mut());
    let names: Vec<&str> = cmds
        .iter()
        .map(|c| match c {
            UiCommand::SaveDraft { draft: Some(_), .. } => "draft",
            UiCommand::SaveDraft { draft: None, .. } => "clear-draft",
            UiCommand::SetTyping { on: true, .. } => "typing",
            UiCommand::SetTyping { on: false, .. } => "stopped",
            UiCommand::SendText { .. } => "send",
            _ => "other",
        })
        .collect();
    assert_eq!(names, ["draft", "typing", "stopped", "clear-draft", "send"]);
    assert_eq!(src.draft(conv), "");
}

#[test]
fn not_implemented_send_shows_a_notice() {
    let (mut src, _sent) = source();
    src.open_conversation(Some(ConversationId(1)));
    src.send_text(ConversationId(1), "keep me", None)
        .expect("send");
    assert_eq!(src.draft(ConversationId(1)), "");
    let ch = src.on_action_failed(
        "SendText",
        &CoreError::NotImplemented("SendText"),
        &ActionSubject::None,
    );
    assert!(ch.notice);
    assert!(ch.draft, "the refused text comes back");
    assert_eq!(src.draft(ConversationId(1)), "keep me");
    assert_eq!(src.notice(), "Sending messages is not available yet.");
    assert!(
        !src.on_action_failed(
            "SaveDraft",
            &CoreError::NotImplemented("SaveDraft"),
            &ActionSubject::None
        )
        .any()
    );
    assert!(
        !src.on_action_failed(
            "SetTyping",
            &CoreError::NotImplemented("SetTyping"),
            &ActionSubject::None
        )
        .any()
    );
    src.clear_notice();
    assert_eq!(src.notice(), "");
}

#[test]
fn mark_read_when_visible_and_focused() {
    let (mut src, sent) = source();
    let mut store = Store::new(&[(1, 5)]);
    store
        .convs
        .get_mut(&1)
        .expect("conv")
        .push(message(1, 7, true));
    src.reload();
    src.open_conversation(Some(ConversationId(1)));
    settle(&mut src, &sent, &store);
    let marks = |sent: &Sent| {
        sent.borrow()
            .iter()
            .filter(|c| matches!(c, UiCommand::MarkRead { .. }))
            .count()
    };
    assert_eq!(marks(&sent), 0, "not focused yet");
    // The unread marker sits above the unread message.
    let rows = src.timeline(ConversationId(1));
    let marker = rows.iter().position(|r| r.kind == RowKind::UnreadMarker);
    let unread = rows
        .iter()
        .position(|r| r.key == RowKey::Message(MessageId(100_007)));
    assert_eq!(marker, unread.map(|u| u + 1));
    src.set_presence(true, true);
    assert_eq!(marks(&sent), 1);
    assert!(sent.borrow().iter().any(|c| matches!(
        c,
        UiCommand::SetPresence {
            focused: true,
            visible: Some(ConversationId(1))
        }
    )));
    src.set_presence(true, true);
    settle(&mut src, &sent, &store);
    assert_eq!(marks(&sent), 1, "once per newest message");
    src.set_presence(false, true);
    store
        .convs
        .get_mut(&1)
        .expect("conv")
        .push(message(1, 8, true));
    src.on_event(&UiEvent::MessagesChanged {
        conv: ConversationId(1),
        ids: Vec::new(),
    });
    settle(&mut src, &sent, &store);
    assert_eq!(marks(&sent), 1, "not while unfocused");
    src.set_presence(true, true);
    assert_eq!(marks(&sent), 2);
}

/// A call line that turns into a missed call in the chat the user is looking at keeps its
/// id: "the newest incoming row was marked read already" must not keep the chat from being
/// marked read again, or the missed call stays unread with no notification and no marker.
#[test]
fn a_row_that_becomes_unread_in_the_open_chat_is_marked_read_again() {
    use kourier_model::{CallDirection, CallLine, CallMedia, CallState, SystemEvent};
    let (mut src, sent) = source();
    let line = |state: CallState, read_state: Option<ReadState>| {
        // As the core stores a line: no author, the origin says which way the call went.
        let mut m = message(1, 8, false);
        m.author = None;
        m.author_name = None;
        m.origin = OriginKind::Remote;
        m.body = Body::System(SystemEvent::Call(
            CallLine::signal(CallDirection::Incoming, false, state, NOW)
                .with_media(CallMedia::Voice, NOW),
        ));
        m.read_state = read_state;
        m
    };
    let mut store = Store::new(&[(1, 5)]);
    let rows = store.convs.get_mut(&1).expect("conv");
    rows.push(message(1, 7, true));
    rows.push(line(
        CallState::Ringing {
            deadline: NOW + 60_000,
        },
        None,
    ));
    src.reload();
    src.open_conversation(Some(ConversationId(1)));
    settle(&mut src, &sent, &store);
    let marks = |sent: &Sent| {
        sent.borrow()
            .iter()
            .filter(|c| matches!(c, UiCommand::MarkRead { .. }))
            .count()
    };
    src.set_presence(true, true);
    assert_eq!(
        marks(&sent),
        1,
        "the unread text; the ringing line is the newest row"
    );
    // The core marked the text read; the ring goes on. Nothing more to mark.
    let changed = |src: &mut CoreSource, store: &Store| {
        src.on_event(&UiEvent::MessagesChanged {
            conv: ConversationId(1),
            ids: vec![MessageId(100_008)],
        });
        settle(src, &sent, store);
    };
    store.convs.get_mut(&1).expect("conv")[5].read_state = Some(ReadState::Read);
    changed(&mut src, &store);
    assert_eq!(marks(&sent), 1);
    // The ring runs out while the user looks at the chat: the same row is a missed call
    // and unread. The chat is marked read again.
    store.convs.get_mut(&1).expect("conv")[6] = line(CallState::Missed, Some(ReadState::Unread));
    changed(&mut src, &store);
    assert_eq!(
        marks(&sent),
        2,
        "the missed call is read where the user looks"
    );
    // The same page again (the core has not answered yet): not a third time.
    changed(&mut src, &store);
    assert_eq!(marks(&sent), 2, "once per change");
    // The core marked it read; later the phone says it was answered: nothing to mark.
    store.convs.get_mut(&1).expect("conv")[6] = line(CallState::Missed, Some(ReadState::Read));
    changed(&mut src, &store);
    store.convs.get_mut(&1).expect("conv")[6] = line(CallState::Answered, None);
    changed(&mut src, &store);
    assert_eq!(marks(&sent), 2);
    // Not while the window has no focus: the usual rule holds for the second mark too.
    src.set_presence(false, true);
    let mut second = line(CallState::Ringing { deadline: NOW }, None);
    second.id = MessageId(100_009);
    second.sort_key = 90;
    store.convs.get_mut(&1).expect("conv").push(second.clone());
    changed(&mut src, &store);
    second.read_state = Some(ReadState::Unread);
    store.convs.get_mut(&1).expect("conv")[7] = second;
    changed(&mut src, &store);
    assert_eq!(marks(&sent), 2, "not while unfocused");
    src.set_presence(true, true);
    assert_eq!(marks(&sent), 3);
}

#[test]
fn an_asked_for_mark_read_goes_to_the_core_for_any_chat_and_every_time() {
    let (mut src, sent) = source();
    let mut store = Store::new(&[(1, 5), (2, 4), (3, 4)]);
    store
        .convs
        .get_mut(&1)
        .expect("conv")
        .push(message(1, 7, true));
    src.reload();
    src.open_conversation(Some(ConversationId(1)));
    settle(&mut src, &sent, &store);
    let marked = |sent: &Sent| -> Vec<ConversationId> {
        std::mem::take(&mut *sent.borrow_mut())
            .into_iter()
            .filter_map(|c| match c {
                UiCommand::MarkRead { conv } => Some(conv),
                _ => None,
            })
            .collect()
    };
    marked(&sent);
    // A chat that is not open, while the window has no focus: the open chat's own rule would
    // send nothing.
    assert!(src.mark_read(ConversationId(2)));
    assert_eq!(marked(&sent), [ConversationId(2)]);
    // The open chat, unfocused: it too.
    assert!(src.mark_read(ConversationId(1)));
    assert_eq!(marked(&sent), [ConversationId(1)]);
    // The open chat's automatic mark is deduplicated by its newest message; the asked-for
    // one is not, before it or after it.
    src.set_presence(true, true);
    assert_eq!(marked(&sent), [ConversationId(1)], "the automatic mark");
    src.set_presence(true, true);
    settle(&mut src, &sent, &store);
    assert_eq!(marked(&sent), [], "once per newest message");
    assert!(src.mark_read(ConversationId(1)));
    assert!(src.mark_read(ConversationId(1)));
    assert_eq!(marked(&sent), [ConversationId(1), ConversationId(1)]);
    // A chat merged into another is marked under the surviving id.
    src.on_event(&UiEvent::ConversationMerged {
        from: ConversationId(3),
        into: ConversationId(2),
    });
    assert!(src.mark_read(ConversationId(3)));
    assert_eq!(marked(&sent), [ConversationId(2)]);
    // Nothing is sent to a stopped core.
    let mut stopped = CoreSource::new(
        Box::new(|_: UiCommand| false),
        CoreSourceConfig {
            now: Box::new(|| NOW),
            calendar: Box::new(FixedOffset(0)),
            networks: Vec::new(),
            hostname: String::new(),
            demo: None,
        },
    );
    assert!(!stopped.mark_read(ConversationId(1)));
}

#[test]
fn merge_switches_and_typing_expires() {
    let (mut src, sent) = source();
    let store = Store::new(&[(1, 3), (2, 3)]);
    src.reload();
    src.open_conversation(Some(ConversationId(1)));
    settle(&mut src, &sent, &store);
    let ch = src.on_event(&UiEvent::ConversationMerged {
        from: ConversationId(1),
        into: ConversationId(2),
    });
    assert_eq!(ch.switch_to, Some(ConversationId(2)));
    src.open_conversation(Some(ConversationId(2)));
    settle(&mut src, &sent, &store);
    let ch = src.on_event(&UiEvent::Typing {
        account: AccountId(1),
        conv: ConversationId(2),
        user: user(1),
        on: true,
        ttl: Duration::from_millis(1),
    });
    assert!(ch.header && ch.conversations);
    assert!(src.has_timers());
    // The fixed clock never advances, so a 1 ms TTL is still alive "now"; an explicit stop
    // clears it.
    src.on_event(&UiEvent::Typing {
        account: AccountId(1),
        conv: ConversationId(2),
        user: user(1),
        on: false,
        ttl: Duration::ZERO,
    });
    src.tick();
    assert!(!src.has_timers());
    // ConnState events win over the queried state.
    src.on_event(&UiEvent::ConnState {
        account: AccountId(1),
        state: ConnState::Online { detail: None },
    });
    assert_eq!(
        src.accounts().first().map(|a| a.status),
        Some(AccountStatus::Online)
    );
}

#[test]
fn a_login_cancelled_before_the_core_names_its_account_is_cancelled_then() {
    let (mut src, sent) = source();
    src.start_login(NetworkKind::Fake).expect("start");
    // Cancel at once, before `AccountAdded`, and pick the network again.
    src.cancel_login();
    src.start_login(NetworkKind::Fake).expect("again");
    sent.borrow_mut().clear();
    let (first, second) = (AccountId(7), AccountId(8));
    src.on_event(&UiEvent::AccountAdded { account: first });
    assert!(
        matches!(
            sent.borrow().as_slice(),
            [UiCommand::CancelLogin { account }, ..] if *account == first
        ),
        "the abandoned account is cancelled once the core names it"
    );
    let step = |account, kind| UiEvent::LoginStep {
        account,
        step: LoginStep {
            step_id: "qr".into(),
            txn: 1,
            kind,
        },
    };
    let qr = |s: &str| LoginStepKind::ShowQr {
        data: Redacted::new(s.to_owned()),
        valid_for: None,
    };
    // Its steps never reach the page, before or after the second account is known.
    assert!(!src.on_event(&step(first, qr("first/0"))).login);
    src.on_event(&UiEvent::AccountAdded { account: second });
    assert!(!src.on_event(&step(first, qr("first/1"))).login);
    assert!(src.on_event(&step(second, qr("second/0"))).login);
    assert_eq!(
        src.login_view().map(|v| v.qr),
        Some("second/0".to_owned()),
        "the page shows the login the user is in"
    );
    // Cancelling the live login now names its account directly.
    sent.borrow_mut().clear();
    src.cancel_login();
    assert!(matches!(
        sent.borrow().as_slice(),
        [UiCommand::CancelLogin { account }] if *account == second
    ));
}

#[test]
fn a_failed_add_of_a_cancelled_login_does_not_fail_the_next_one() {
    let (mut src, _sent) = source();
    src.start_login(NetworkKind::Fake).expect("start");
    src.cancel_login();
    src.start_login(NetworkKind::Fake).expect("again");
    assert!(
        !src.on_action_failed("AddAccount", &CoreError::Stopped, &ActionSubject::None)
            .any(),
        "the answer to the cancelled AddAccount"
    );
    assert_eq!(
        src.login_view().map(|v| v.kind),
        Some(LoginViewKind::Waiting)
    );
    assert!(
        src.on_action_failed("AddAccount", &CoreError::Stopped, &ActionSubject::None)
            .login
    );
    assert_eq!(
        src.login_view().map(|v| v.kind),
        Some(LoginViewKind::Failed)
    );
}

#[test]
fn a_failing_timeline_query_is_not_retried_in_a_loop() {
    let (mut src, sent) = source();
    let store = Store::new(&[(1, 10)]);
    src.reload();
    settle(&mut src, &sent, &store);
    src.open_conversation(Some(ConversationId(1)));
    let timeline = |qs: &[(QueryToken, Query)]| {
        qs.iter()
            .filter(|(_, q)| matches!(q, Query::Timeline(_)))
            .map(|(t, _)| *t)
            .collect::<Vec<_>>()
    };
    let first = timeline(&take_queries(&sent));
    assert_eq!(first.len(), 1);
    let err: Result<QueryData, std::sync::Arc<&str>> = Err(std::sync::Arc::new("disk I/O"));
    let ch = src.on_query_result(first[0], &err);
    assert!(ch.timeline);
    assert!(
        timeline(&take_queries(&sent)).is_empty(),
        "no immediate re-query of a failing timeline"
    );
    assert!(src.timeline(ConversationId(1)).is_empty());
    assert!(!src.paging().loading);
    // The next invalidation asks again and recovers.
    src.on_event(&UiEvent::MessagesChanged {
        conv: ConversationId(1),
        ids: Vec::new(),
    });
    let again = take_queries(&sent);
    assert_eq!(timeline(&again).len(), 1);
    for (t, q) in again {
        src.on_event(&result(t, store.answer(&q)));
    }
    settle(&mut src, &sent, &store);
    assert_eq!(src.window_len(), 10);
    assert_eq!(message_texts(&src.timeline(ConversationId(1))).len(), 10);
}

#[test]
fn a_merge_keeps_the_composer_text() {
    let (mut src, sent) = source();
    let store = Store::new(&[(1, 3), (2, 3)]);
    src.reload();
    src.open_conversation(Some(ConversationId(1)));
    settle(&mut src, &sent, &store);
    let ch = src.on_event(&UiEvent::ConversationMerged {
        from: ConversationId(1),
        into: ConversationId(2),
    });
    assert_eq!(ch.switch_to, Some(ConversationId(2)));
    // The composer leaves the merged-away chat: its text is saved under the old id …
    sent.borrow_mut().clear();
    src.save_draft(ConversationId(1), "half typed");
    // … and restored in the surviving one.
    assert_eq!(src.draft(ConversationId(2)), "half typed");
    assert!(matches!(
        sent.borrow().as_slice(),
        [UiCommand::SaveDraft { conv, draft: Some(_) }] if *conv == ConversationId(2)
    ));
}

#[test]
fn nameless_direct_chats_show_a_number_or_a_short_id() {
    let (mut src, sent) = source();
    src.reload();
    let mut nameless = conv_row(1, "x", 10);
    nameless.kind = ConvKind::Direct;
    nameless.title = None;
    nameless.peer = Some(user(0x1f2e));
    nameless.preview = None;
    let mut phone = nameless.clone();
    phone.id = ConversationId(2);
    phone.last_activity = 5;
    phone.peer = Some(UserId::parse("e164:+15550001234").expect("id"));
    for (t, q) in take_queries(&sent) {
        let data = match q {
            Query::Accounts => QueryData::Accounts(vec![account()]),
            Query::Conversations(_) => QueryData::Conversations(ConversationPage {
                rows: vec![nameless.clone(), phone.clone()],
                next: None,
            }),
            other => panic!("unexpected query {other:?}"),
        };
        src.on_event(&result(t, data));
    }
    let titles: Vec<String> = src
        .conversations(&ConversationFilter::default())
        .into_iter()
        .map(|r| r.title)
        .collect();
    assert_eq!(titles, ["Signal user ⋯1f2e", "+15550001234"]);
    assert!(titles.iter().all(|t| !t.contains("aci:")));
}

/// Avatar URLs come from the hook; a row built while its avatar is being prepared is rebuilt on
/// a tick once it is ready (S9 avatars in the UI).
#[test]
fn avatars_show_once_ready_and_rows_are_rebuilt_on_a_tick() {
    use std::cell::Cell;

    use kourier_model::{BlobKey, BlobRef};

    struct Urls(Rc<Cell<bool>>);
    impl AvatarUrls for Urls {
        fn url(&self, _account: &AccountUuid, avatar: &BlobRef) -> Option<String> {
            self.0
                .get()
                .then(|| format!("file:///media/{}", avatar.path.display()))
        }
        fn pending(&self) -> bool {
            !self.0.get()
        }
    }

    let (mut src, sent) = source();
    let ready = Rc::new(Cell::new(false));
    src.set_avatar_urls(Box::new(Urls(ready.clone())));
    src.reload();
    for (t, q) in take_queries(&sent) {
        let data = match q {
            Query::Accounts => QueryData::Accounts(vec![account()]),
            Query::Conversations(_) => {
                let mut row = conv_row(1, "Ada", 1);
                row.avatar = Some(BlobRef {
                    path: "ab/cd".into(),
                    key: BlobKey::new([1; 32]),
                });
                QueryData::Conversations(ConversationPage {
                    rows: vec![row, conv_row(2, "Bruno", 2)],
                    next: None,
                })
            }
            other => panic!("unexpected query {other:?}"),
        };
        src.on_event(&result(t, data));
    }
    let urls = |src: &CoreSource| -> Vec<String> {
        src.conversations(&ConversationFilter::default())
            .into_iter()
            .map(|r| r.avatar_url)
            .collect()
    };
    assert!(urls(&src).iter().all(String::is_empty), "not ready yet");
    assert!(src.has_timers(), "a tick looks again");
    assert!(!src.tick().conversations, "still being prepared");
    ready.set(true);
    assert!(src.tick().conversations, "rows rebuilt once ready");
    let mut shown = urls(&src);
    shown.sort();
    assert_eq!(
        shown,
        ["", "file:///media/ab/cd"],
        "only the row with an avatar"
    );
    assert!(!src.has_timers());
}

fn media_store() -> Store {
    use kourier_model::{Attachment, AttachmentId, AttachmentKind, AttachmentState};
    let mut store = Store::new(&[(1, 4)]);
    let rows = store.convs.get_mut(&1).expect("conv");
    rows[1].body = Body::Media {
        attachments: vec![
            Attachment {
                id: Some(AttachmentId(11)),
                kind: AttachmentKind::Image,
                mime: "image/jpeg".into(),
                size: 48_000,
                dims: Some((640, 480)),
                blurhash: Some("LKO2?U%2Tw=w]~RBVZRi};RPxuwH".into()),
                state: AttachmentState::Local,
                ..Attachment::default()
            },
            Attachment {
                id: Some(AttachmentId(12)),
                kind: AttachmentKind::File,
                mime: "application/pdf".into(),
                size: 2_000_000,
                file_name: Some("plan.pdf".into()),
                state: AttachmentState::Downloading {
                    done: 0,
                    total: 2_000_000,
                },
                ..Attachment::default()
            },
        ],
        caption: Some(Text {
            body: "look".into(),
            ranges: Vec::new(),
        }),
    };
    store
}

fn media_blocks(rows: &[MessageRow]) -> (crate::timeline::ImageBlock, crate::timeline::FileBlock) {
    let mut image = None;
    let mut file = None;
    for b in rows.iter().flat_map(|r| r.blocks.iter()) {
        match b {
            Block::Image(i) => image = Some(i.clone()),
            Block::File(f) => file = Some(f.clone()),
            _ => {}
        }
    }
    (image.expect("image block"), file.expect("file block"))
}

#[test]
fn media_rows_carry_provider_urls_and_live_progress() {
    use kourier_core::facade::Progress;
    use kourier_model::AttachmentId;
    let (mut src, sent) = source();
    let store = media_store();
    src.reload();
    settle(&mut src, &sent, &store);
    src.open_conversation(Some(ConversationId(1)));
    settle(&mut src, &sent, &store);
    let (image, file) = media_blocks(&src.timeline(ConversationId(1)));
    assert_eq!(image.attachment, Some(AttachmentId(11)));
    assert!(
        image.url.starts_with("image://kourier/thumb/11?v="),
        "thumb url"
    );
    assert!(
        image.full_url.starts_with("image://kourier/att/11?v="),
        "full url"
    );
    assert!(
        image
            .placeholder_url
            .starts_with("image://kourier/blurhash/11?v=")
    );
    assert!(!image.animated);
    assert!(
        !format!("{image:?}").contains("kourier/"),
        "Debug stays log-safe"
    );
    assert_eq!(file.attachment, Some(AttachmentId(12)));
    assert_eq!(file.state.as_str(), "downloading");
    assert!(!file.media, "a PDF asks before it opens");

    // Live progress of a shown download rebuilds the row; an unknown attachment does not.
    let ch = src.on_event(&UiEvent::Progress(Progress::Download {
        attachment: AttachmentId(12),
        done: 500_000,
        total: 2_000_000,
    }));
    assert!(ch.timeline);
    let (_, file) = media_blocks(&src.timeline(ConversationId(1)));
    assert!((file.progress - 0.25).abs() < 1e-6);
    let ch = src.on_event(&UiEvent::Progress(Progress::Download {
        attachment: AttachmentId(99),
        done: 1,
        total: 2,
    }));
    assert!(!ch.timeline);

    // Downloads, opens and closes go to the core as commands.
    sent.borrow_mut().clear();
    assert!(src.download_attachment(AttachmentId(12)));
    assert!(src.open_media(AttachmentId(11)));
    src.close_media(AttachmentId(11));
    let cmds: Vec<String> = sent.borrow().iter().map(|c| format!("{c:?}")).collect();
    assert_eq!(cmds.len(), 3, "{cmds:?}");
    assert!(cmds[0].starts_with("DownloadAttachment"));
    assert!(cmds[1].starts_with("OpenMedia"));
    assert!(cmds[2].starts_with("CloseMedia"));

    // The core's answers queue up for the UI; a failed open is reported too.
    let path = std::path::PathBuf::from("/run/user/1000/kourier/media/attachments/x.jpg");
    let ch = src.on_event(&UiEvent::MediaOpened {
        attachment: AttachmentId(11),
        path: path.clone(),
    });
    assert!(ch.media);
    // A failed open names its attachment (`ActionSubject::Attachment`): the UI fails that
    // request, whatever else is waiting.
    let ch = src.on_action_failed(
        "OpenMedia",
        &CoreError::InvalidInput("the attachment is not downloaded"),
        &ActionSubject::Attachment(AttachmentId(12)),
    );
    assert!(ch.media);
    assert_eq!(
        src.take_media(),
        vec![
            MediaAnswer::Opened(AttachmentId(11), path.clone()),
            MediaAnswer::Failed(Some(AttachmentId(12)))
        ]
    );
    assert!(src.take_media().is_empty());
    assert!(!format!("{:?}", MediaAnswer::Opened(AttachmentId(11), path)).contains("/run"));

    // A closed conversation forgets its live progress.
    src.open_conversation(None);
    assert!(src.downloads.is_empty());
}

#[test]
fn send_files_takes_the_composer_text_as_caption_and_gives_it_back_on_failure() {
    let (mut src, sent) = source();
    let store = media_store();
    src.reload();
    settle(&mut src, &sent, &store);
    src.open_conversation(Some(ConversationId(1)));
    settle(&mut src, &sent, &store);
    src.save_draft(ConversationId(1), "draft");
    sent.borrow_mut().clear();
    assert_eq!(
        src.send_files(ConversationId(1), Vec::new(), Vec::new(), "x", None),
        Err(SourceError::Empty)
    );
    let paths = vec![std::path::PathBuf::from("/tmp/a.jpg")];
    let quoted = MessageId(100_002);
    let request = src
        .send_files(
            ConversationId(1),
            paths.clone(),
            vec![true],
            "Photos \n",
            Some(quoted),
        )
        .expect("sent");
    let cmds = sent.borrow();
    let files = cmds
        .iter()
        .find_map(|c| match c {
            UiCommand::SendFiles {
                conv,
                paths: p,
                caption,
                reply_to,
                ..
            } => Some((*conv, p.expose().clone(), caption.clone(), *reply_to)),
            _ => None,
        })
        .expect("SendFiles");
    assert_eq!(files.0, ConversationId(1));
    assert_eq!(files.1, paths);
    assert_eq!(files.2.clone().map(|t| t.body).as_deref(), Some("Photos"));
    assert_eq!(files.3, Some(quoted), "files can be a reply");
    assert!(
        cmds.iter()
            .any(|c| matches!(c, UiCommand::SaveDraft { draft: None, .. })),
        "the draft is cleared"
    );
    drop(cmds);
    // The core hands a request it refused for a reason that is not about a file back whole:
    // files, caption and quote return to the composer (nothing of it was queued).
    assert_eq!(src.preparing_files(ConversationId(1)), 1);
    let refused = |refused: Vec<RefusedFile>, paths: &[std::path::PathBuf]| {
        ActionSubject::Files(std::sync::Arc::new(kourier_core::facade::RefusedFiles {
            conv: ConversationId(1),
            request: kourier_core::facade::RequestId(request),
            as_file: vec![true],
            paths: paths.to_vec().into(),
            caption: files.2.clone(),
            reply_to: files.3,
            refused,
        }))
    };
    let ch = src.on_action_failed(
        "SendFiles",
        &CoreError::Stopped,
        &refused(Vec::new(), &paths),
    );
    assert!(ch.notice && ch.draft && ch.files);
    assert_eq!(src.notice(), "The files could not be sent.");
    assert_eq!(src.draft(ConversationId(1)), "Photos");
    assert_eq!(src.preparing_files(ConversationId(1)), 0);
    let back = src.take_refused_files().expect("the refused request");
    assert_eq!(
        (back.conv, back.caption.as_str(), back.reply_to, back.resent),
        (ConversationId(1), "Photos", Some(quoted), 0)
    );
    // The file comes back with the choice that was made for it, and the request is named.
    assert_eq!(
        back.files,
        vec![ReturnedFile {
            path: paths[0].clone(),
            as_file: true,
            reason: None,
        }]
    );
    assert_eq!((back.request, back.resent_request), (request, None));
    assert!(src.take_refused_files().is_none(), "taken once");
    let debug = format!(
        "{:?}",
        RefusedSend {
            conv: ConversationId(1),
            request: 1,
            files: vec![ReturnedFile {
                path: "/home/me/secret.jpg".into(),
                as_file: false,
                reason: None,
            }],
            caption: "private".into(),
            reply_to: None,
            resent: 0,
            resent_request: None,
        }
    );
    assert!(
        !debug.contains("secret") && !debug.contains("private"),
        "{debug}"
    );
    // A failure that names no request says so and hands nothing back.
    src.on_action_failed("SendFiles", &CoreError::Stopped, &ActionSubject::None);
    assert_eq!(src.notice(), "The files could not be sent.");
    assert!(src.take_refused_files().is_none());
}

#[test]
fn files_the_network_cannot_take_do_not_hold_the_others_back() {
    let (mut src, sent) = source();
    let store = media_store();
    src.reload();
    settle(&mut src, &sent, &store);
    src.open_conversation(Some(ConversationId(1)));
    settle(&mut src, &sent, &store);
    let paths: Vec<std::path::PathBuf> = ["a.jpg", "huge.zip", "b.pdf", "folder"]
        .iter()
        .map(|n| std::path::PathBuf::from(format!("/tmp/{n}")))
        .collect();
    let request = src
        .send_files(
            ConversationId(1),
            paths.clone(),
            Vec::new(),
            "Trip",
            Some(MessageId(100_002)),
        )
        .expect("sent");
    assert_eq!(src.preparing_files(ConversationId(1)), 4);
    sent.borrow_mut().clear();
    let too_large = FileRefusal::TooLarge {
        max: 100_000_000,
        chat: false,
    };
    let subject = |request: u64, paths: &[std::path::PathBuf], refused: Vec<RefusedFile>| {
        ActionSubject::Files(std::sync::Arc::new(kourier_core::facade::RefusedFiles {
            conv: ConversationId(1),
            request: kourier_core::facade::RequestId(request),
            as_file: Vec::new(),
            paths: paths.to_vec().into(),
            caption: Some(Text::plain("Trip")),
            reply_to: Some(MessageId(100_002)),
            refused,
        }))
    };
    // The core names the two files it cannot take (whatever words its error has).
    let ch = src.on_action_failed(
        "SendFiles",
        &CoreError::InvalidInput("reworded by the core"),
        &subject(
            request,
            &paths,
            vec![
                RefusedFile {
                    index: 1,
                    reason: too_large,
                },
                RefusedFile {
                    index: 3,
                    reason: FileRefusal::NotAFile,
                },
            ],
        ),
    );
    assert!(ch.notice && ch.files && !ch.draft);
    // The other two go out again at once, with the caption and the quote.
    let resent = sent
        .borrow()
        .iter()
        .find_map(|c| match c {
            UiCommand::SendFiles {
                paths: p,
                caption,
                reply_to,
                ..
            } => Some((p.expose().clone(), caption.clone(), *reply_to)),
            _ => None,
        })
        .expect("the rest is sent again");
    assert_eq!(resent.0, vec![paths[0].clone(), paths[2].clone()]);
    assert_eq!(resent.1.map(|t| t.body).as_deref(), Some("Trip"));
    assert_eq!(resent.2, Some(MessageId(100_002)));
    assert_eq!(src.preparing_files(ConversationId(1)), 2);
    assert_eq!(
        src.notice(),
        "2 files could not be sent and are still in the list, which says why. The others are on their way."
    );
    // Only the refused files return, each with its reason; caption and quote went out.
    let back = src.take_refused_files().expect("the refused files");
    let returned = |path: &std::path::PathBuf, reason| ReturnedFile {
        path: path.clone(),
        as_file: false,
        reason: Some(reason),
    };
    assert_eq!(
        back.files,
        vec![
            returned(&paths[1], too_large),
            returned(&paths[3], FileRefusal::NotAFile),
        ]
    );
    assert_eq!(
        (back.caption.as_str(), back.reply_to, back.resent),
        ("", None, 2)
    );
    assert_eq!(
        src.draft(ConversationId(1)),
        "",
        "the caption was not a draft again"
    );
    // When no file can go, everything returns and nothing is sent.
    sent.borrow_mut().clear();
    let one = vec![paths[3].clone()];
    let request = src
        .send_files(ConversationId(1), one.clone(), Vec::new(), "Trip", None)
        .expect("sent");
    sent.borrow_mut().clear();
    let ch = src.on_action_failed(
        "SendFiles",
        &CoreError::InvalidInput("x"),
        &subject(
            request,
            &one,
            vec![RefusedFile {
                index: 0,
                reason: FileRefusal::NotAFile,
            }],
        ),
    );
    assert!(ch.files && ch.draft);
    assert!(
        !sent
            .borrow()
            .iter()
            .any(|c| matches!(c, UiCommand::SendFiles { .. }))
    );
    assert_eq!(
        src.notice(),
        "This file cannot be sent here. The list says why."
    );
    let back = src.take_refused_files().expect("back");
    assert_eq!(back.files, vec![returned(&one[0], FileRefusal::NotAFile)]);
    assert_eq!(back.caption, "Trip");
    assert_eq!(
        src.preparing_files(ConversationId(1)),
        2,
        "only the resend is left"
    );
}

#[test]
fn the_header_lists_what_can_be_attached() {
    use kourier_model::{AttachmentKind, FileLimits};
    let (mut src, sent) = source();
    let store = media_store();
    src.reload();
    settle(&mut src, &sent, &store);
    src.open_conversation(Some(ConversationId(1)));
    // Answer the detail with features that accept pictures only.
    for (t, q) in take_queries(&sent) {
        let data = match (&q, store.answer(&q)) {
            (Query::Conversation(_), QueryData::Conversation(Some(mut d))) => {
                d.features.files = BTreeMap::from([(
                    AttachmentKind::Image,
                    FileLimits {
                        max_size: 1000,
                        caption: true,
                    },
                )]);
                QueryData::Conversation(Some(d))
            }
            (_, other) => other,
        };
        src.on_event(&result(t, data));
    }
    settle(&mut src, &sent, &store);
    let header = src.chat(ConversationId(1)).expect("header");
    assert!(header.attach.accepts(AttachmentKind::Image));
    assert!(!header.attach.accepts(AttachmentKind::File));
    assert_eq!(header.attach.names(), "image");
}

/// Group timelines show an author's picture: the avatar of the author's direct chat.
#[test]
fn group_authors_get_the_avatar_of_their_direct_chat() {
    use kourier_model::{BlobKey, BlobRef};

    struct Urls;
    impl AvatarUrls for Urls {
        fn url(&self, _account: &AccountUuid, avatar: &BlobRef) -> Option<String> {
            Some(format!("image://kourier/avatar/{}", avatar.path.display()))
        }
    }

    let (mut src, sent) = source();
    src.set_avatar_urls(Box::new(Urls));
    let store = Store::new(&[(1, 6)]);
    let answer = |q: &Query| match q {
        Query::Conversations(_) => {
            let mut direct = conv_row(2, "Ari", 1);
            direct.kind = ConvKind::Direct;
            direct.peer = Some(user(1));
            direct.avatar = Some(BlobRef {
                path: "7".into(),
                key: BlobKey::new([1; 32]),
            });
            QueryData::Conversations(ConversationPage {
                rows: vec![conv_row(1, "Group", 6), direct],
                next: None,
            })
        }
        other => store.answer(other),
    };
    src.reload();
    for _ in 0..10 {
        for (t, q) in take_queries(&sent) {
            src.on_event(&result(t, answer(&q)));
        }
    }
    src.open_conversation(Some(ConversationId(1)));
    for _ in 0..10 {
        for (t, q) in take_queries(&sent) {
            src.on_event(&result(t, answer(&q)));
        }
    }
    let rows = src.timeline(ConversationId(1));
    let with_avatar: Vec<&MessageRow> = rows.iter().filter(|r| r.show_avatar).collect();
    assert!(!with_avatar.is_empty(), "group rows show avatars");
    assert!(
        with_avatar
            .iter()
            .all(|r| r.avatar_url == "image://kourier/avatar/7"),
        "the author's picture"
    );
    assert!(
        rows.iter()
            .filter(|r| !r.show_avatar)
            .all(|r| r.avatar_url.is_empty())
    );
}

#[test]
fn the_line_under_a_message_on_its_way_follows_the_connection_and_the_send() {
    use kourier_core::facade::{OutboxWait, Progress};
    use kourier_model::{OutgoingState, SendPhase};
    let (mut src, sent) = source();
    let mut store = Store::new(&[(1, 3)]);
    let pending = |m: &mut CoreMessage, in_flight: bool, next: Option<i64>| {
        m.origin = OriginKind::OwnThisDevice;
        m.author = None;
        m.read_state = None;
        m.sent_at = NOW - 42_000;
        m.out_state = Some(OutgoingState::Sending { progress: None });
        m.outbox = Some(OutboxWait {
            in_flight,
            attempts: 1,
            next_attempt_at: next,
            last_error: None,
        });
    };
    let msgs = store.convs.get_mut(&1).expect("conv");
    pending(&mut msgs[2], true, None);
    let id = msgs[2].id;
    src.reload();
    src.open_conversation(Some(ConversationId(1)));
    settle(&mut src, &sent, &store);
    let line = |src: &CoreSource| {
        src.timeline(ConversationId(1))
            .iter()
            .find(|r| r.key == RowKey::Message(id))
            .map(|r| format!("{} {}", r.wait, r.wait_timer).trim().to_owned())
            .expect("row")
    };
    // The account is still connecting.
    assert_eq!(line(&src), "Waiting for the connection · 0:42");
    // The seconds count: a timer runs and every tick brings the lines up to date (not the
    // whole timeline: `the_tick_changes_only_the_wait_lines`).
    assert!(src.has_timers());
    assert!(src.tick().waits);
    // The connection comes up (still catching up): the outbox drains.
    let ch = src.on_event(&UiEvent::ConnState {
        account: AccountId(1),
        state: ConnState::Syncing { fraction: None },
    });
    assert!(ch.timeline);
    assert_eq!(line(&src), "Sending… 0:42");
    // The backend's steps of the running attempt.
    let ch = src.on_event(&UiEvent::Progress(Progress::Transmit {
        message: id,
        progress: 0.0,
        phase: Some(SendPhase::SettingUpKeys { done: 3, of: 12 }),
    }));
    assert!(ch.waits && !ch.timeline, "the one line, not the timeline");
    assert_eq!(line(&src), "Setting up encryption with 12 members · 0:42");
    src.on_event(&UiEvent::Progress(Progress::Transmit {
        message: id,
        progress: 0.0,
        phase: Some(SendPhase::Sending { done: 8, of: 20 }),
    }));
    assert_eq!(line(&src), "Sent to 8 of 20 members · 0:42");
    // A step of a message the window does not hold changes nothing.
    let ch = src.on_event(&UiEvent::Progress(Progress::Transmit {
        message: MessageId(9),
        progress: 0.0,
        phase: Some(SendPhase::RepairingDevices),
    }));
    assert!(!ch.any());
    // The attempt failed: the row waits for its next try, and the step is forgotten.
    pending(
        &mut store.convs.get_mut(&1).expect("conv")[2],
        false,
        Some(NOW + 30_000),
    );
    src.on_event(&UiEvent::MessagesChanged {
        conv: ConversationId(1),
        ids: vec![id],
    });
    settle(&mut src, &sent, &store);
    assert_eq!(line(&src), "Next try in 0:30");
    // The next attempt runs without a step reported yet: plain sending, not the old step.
    pending(&mut store.convs.get_mut(&1).expect("conv")[2], true, None);
    src.on_event(&UiEvent::MessagesChanged {
        conv: ConversationId(1),
        ids: vec![id],
    });
    settle(&mut src, &sent, &store);
    assert_eq!(line(&src), "Sending… 0:42");
    // Sent: no line, no timer.
    let m = &mut store.convs.get_mut(&1).expect("conv")[2];
    m.out_state = Some(OutgoingState::Sent { server_at: None });
    m.outbox = None;
    src.on_event(&UiEvent::MessagesChanged {
        conv: ConversationId(1),
        ids: vec![id],
    });
    settle(&mut src, &sent, &store);
    assert_eq!(line(&src), "");
    assert!(!src.has_timers());
    assert!(!src.tick().waits);
}

/// A pending own message (the third of conversation 1) with the given outbox row.
fn make_pending(m: &mut CoreMessage, in_flight: bool, next: Option<i64>) {
    use kourier_core::facade::OutboxWait;
    m.origin = OriginKind::OwnThisDevice;
    m.author = None;
    m.read_state = None;
    m.sent_at = NOW - 42_000;
    m.out_state = Some(OutgoingState::Queued);
    m.outbox = Some(OutboxWait {
        in_flight,
        attempts: 1,
        next_attempt_at: next,
        last_error: None,
    });
}

/// The reason under a message that waits names the REAL hold, never "the connection" for
/// something else (link feedback L6): a challenge, a wrong clock, a logged-out or restricted
/// account, an old version, a connection another session took. Each has its own words, in the
/// terms of the banner above the chat.
#[test]
fn the_line_names_what_holds_the_account() {
    use kourier_model::UserAction;
    let (mut src, sent) = source();
    let mut store = Store::new(&[(1, 3)]);
    make_pending(&mut store.convs.get_mut(&1).expect("conv")[2], false, None);
    let id = store.convs[&1][2].id;
    src.reload();
    src.open_conversation(Some(ConversationId(1)));
    settle(&mut src, &sent, &store);
    let line = |src: &CoreSource| {
        src.timeline(ConversationId(1))
            .iter()
            .find(|r| r.key == RowKey::Message(id))
            .map(|r| format!("{} {}", r.wait, r.wait_timer).trim().to_owned())
            .expect("row")
    };
    let state = |src: &mut CoreSource, state: ConnState| {
        src.on_event(&UiEvent::ConnState {
            account: AccountId(1),
            state,
        });
    };
    let url = || "https://signalcaptchas.example/x".to_owned();
    let cases = [
        (ConnState::Connecting, "Waiting for the connection · 0:42"),
        (
            ConnState::Offline { retry_at_ms: None },
            "Waiting for the connection · 0:42",
        ),
        (
            ConnState::NeedsUserAction(UserAction::SolveChallenge { url: url() }),
            "Waiting for you to verify · 0:42",
        ),
        (
            ConnState::NeedsUserAction(UserAction::FixClock { skew_ms: 7_200_000 }),
            "Waiting for the computer's clock to be set · 0:42",
        ),
        (
            ConnState::NeedsUserAction(UserAction::Relink),
            "Waiting for this device to be linked again · 0:42",
        ),
        (
            ConnState::LoggedOut {
                reason: "unlinked".into(),
            },
            "Waiting for this device to be linked again · 0:42",
        ),
        (
            ConnState::NeedsUserAction(UserAction::UpdateApp),
            "Waiting for Kourier to be updated · 0:42",
        ),
        (
            ConnState::NeedsUserAction(UserAction::Reconnect),
            "Waiting for you to reconnect · 0:42",
        ),
        (
            ConnState::Restricted {
                until_ms: None,
                reason: "later".into(),
            },
            "Waiting: the network restricted this account · 0:42",
        ),
        // Connected again (still catching up, then caught up): its own send.
        (ConnState::Syncing { fraction: None }, "Sending… 0:42"),
        (ConnState::Online { detail: None }, "Sending… 0:42"),
    ];
    for (conn, want) in cases {
        state(&mut src, conn.clone());
        assert_eq!(line(&src), want, "{conn:?}");
    }
}

/// The queue of a conversation waits for the user to confirm a changed safety number: its
/// head shows as failed (no line), the messages behind it say what they wait for, and so
/// does no message of another conversation.
#[test]
fn a_queue_that_waits_for_a_safety_number_says_so() {
    use kourier_core::facade::OutboxWait;
    use kourier_model::{FailReason, OutgoingState};
    let (mut src, sent) = source();
    let mut store = Store::new(&[(1, 4), (2, 3)]);
    let msgs = store.convs.get_mut(&1).expect("conv");
    // The head: paused by the changed safety number, shown as failed, still in the outbox.
    make_pending(&mut msgs[2], false, None);
    msgs[2].out_state = Some(OutgoingState::Failed {
        reason: FailReason::UntrustedIdentity,
        retryable: true,
    });
    msgs[2].outbox = Some(OutboxWait {
        in_flight: false,
        attempts: 1,
        next_attempt_at: None,
        last_error: Some("UntrustedIdentity".into()),
    });
    // Behind it: queued, never tried.
    make_pending(&mut msgs[3], false, None);
    msgs[3].outbox = Some(OutboxWait {
        in_flight: false,
        attempts: 0,
        next_attempt_at: None,
        last_error: None,
    });
    let (head, behind) = (msgs[2].id, msgs[3].id);
    src.reload();
    src.open_conversation(Some(ConversationId(1)));
    settle(&mut src, &sent, &store);
    src.on_event(&UiEvent::ConnState {
        account: AccountId(1),
        state: ConnState::Online { detail: None },
    });
    let rows = src.timeline(ConversationId(1));
    let line = |id: MessageId| {
        let r = rows
            .iter()
            .find(|r| r.key == RowKey::Message(id))
            .expect("row");
        format!("{} {}", r.wait, r.wait_timer).trim().to_owned()
    };
    assert_eq!(line(head), "", "the failed head shows its failure, no line");
    assert_eq!(
        line(behind),
        "Waiting for you to confirm the new safety number · 0:42"
    );
    // The account's own hold goes first: offline, the connection is what is missing.
    src.on_event(&UiEvent::ConnState {
        account: AccountId(1),
        state: ConnState::Offline { retry_at_ms: None },
    });
    let rows = src.timeline(ConversationId(1));
    let r = rows
        .iter()
        .find(|r| r.key == RowKey::Message(behind))
        .expect("row");
    assert_eq!(r.wait, "Waiting for the connection");
}

/// The per-second tick brings up to date the lines under the messages that show one, row by
/// row (`UiSource::wait_lines`), and never rebuilds the timeline (link feedback L6): what it
/// hands out is what a rebuilt timeline would show.
#[test]
fn the_tick_changes_only_the_wait_lines() {
    use std::cell::Cell;
    let clock = Rc::new(Cell::new(NOW));
    let sent: Sent = Rc::default();
    let s2 = sent.clone();
    let c2 = clock.clone();
    let mut src = CoreSource::new(
        Box::new(move |c: UiCommand| {
            s2.borrow_mut().push(c);
            true
        }),
        CoreSourceConfig {
            now: Box::new(move || c2.get()),
            calendar: Box::new(FixedOffset(0)),
            networks: network_choices(),
            hostname: "workstation".into(),
            demo: None,
        },
    );
    let mut store = Store::new(&[(1, 3)]);
    make_pending(&mut store.convs.get_mut(&1).expect("conv")[2], true, None);
    let id = store.convs[&1][2].id;
    src.reload();
    src.open_conversation(Some(ConversationId(1)));
    settle(&mut src, &sent, &store);
    src.on_event(&UiEvent::ConnState {
        account: AccountId(1),
        state: ConnState::Online { detail: None },
    });
    let conv = ConversationId(1);
    let ch = src.tick();
    assert!(ch.waits, "a line shows: the tick asks for the lines");
    assert!(!ch.timeline, "and does not rebuild the timeline");
    assert!(ch.any());
    let lines = src.wait_lines(conv);
    assert_eq!(lines.len(), 1, "only the message on its way has a line");
    let rebuilt = |src: &CoreSource| {
        let rows = src.timeline(conv);
        let r = rows
            .iter()
            .find(|r| r.key == RowKey::Message(id))
            .expect("row");
        (r.wait.clone(), r.wait_timer.clone())
    };
    assert_eq!(lines[0].0, id);
    assert_eq!(
        (lines[0].1.reason.clone(), lines[0].1.timer.clone()),
        rebuilt(&src)
    );
    assert_eq!(lines[0].1.timer, "0:42");
    // A second later the lines count on, the same as a rebuilt timeline says.
    clock.set(NOW + 1_000);
    let lines = src.wait_lines(conv);
    assert_eq!(lines[0].1.timer, "0:43");
    assert_eq!(
        (lines[0].1.reason.clone(), lines[0].1.timer.clone()),
        rebuilt(&src)
    );
    // Another conversation (or none open) has nothing to bring up to date.
    assert!(src.wait_lines(ConversationId(2)).is_empty());
    // A message that left the outbox has no line and no tick.
    let m = &mut store.convs.get_mut(&1).expect("conv")[2];
    m.out_state = Some(OutgoingState::Sent { server_at: None });
    m.outbox = None;
    src.on_event(&UiEvent::MessagesChanged {
        conv,
        ids: vec![id],
    });
    settle(&mut src, &sent, &store);
    assert!(src.wait_lines(conv).is_empty());
    assert!(!src.tick().any());
}

/// A step a send reports (a group send: "Setting up encryption with 12 members", up to four a
/// second) changes the reason under that one message and nothing else (link feedback L6): the
/// report asks for the lines (`Changes::waits`, what the tick asks for), never for the
/// timeline to be rebuilt, and `wait_lines` hands out the new reason for that row only. A
/// repeated step changes nothing; the end of the steps (`None`) falls back to "Sending…".
#[test]
fn a_send_step_changes_one_line_and_rebuilds_nothing() {
    use kourier_core::facade::Progress;
    use kourier_model::SendPhase;
    let (mut src, sent) = source();
    let mut store = Store::new(&[(1, 4)]);
    make_pending(&mut store.convs.get_mut(&1).expect("conv")[2], true, None);
    let id = store.convs[&1][2].id;
    src.reload();
    src.open_conversation(Some(ConversationId(1)));
    settle(&mut src, &sent, &store);
    src.on_event(&UiEvent::ConnState {
        account: AccountId(1),
        state: ConnState::Syncing { fraction: None },
    });
    let conv = ConversationId(1);
    let report = |src: &mut CoreSource, message: MessageId, phase: Option<SendPhase>| {
        src.on_event(&UiEvent::Progress(Progress::Transmit {
            message,
            progress: 0.0,
            phase,
        }))
    };
    let lines = |src: &CoreSource| {
        let lines = src.wait_lines(conv);
        assert_eq!(lines.len(), 1, "only the message on its way has a line");
        assert_eq!(lines[0].0, id);
        format!("{} {}", lines[0].1.reason, lines[0].1.timer)
    };
    assert_eq!(lines(&src), "Sending… 0:42");
    let rows_before = src.timeline(conv);

    // A step: the lines are asked for, nothing else (not the timeline, header, list, ...).
    let ch = report(
        &mut src,
        id,
        Some(SendPhase::SettingUpKeys { done: 3, of: 12 }),
    );
    assert_eq!(
        ch,
        Changes {
            waits: true,
            ..Changes::default()
        }
    );
    assert_eq!(lines(&src), "Setting up encryption with 12 members · 0:42");
    // The rows a rebuild would give differ from before in this one row's line only.
    let rows_after = src.timeline(conv);
    assert_eq!(rows_before.len(), rows_after.len());
    for (before, after) in rows_before.iter().zip(&rows_after) {
        if before.key == RowKey::Message(id) {
            assert_ne!(before.wait, after.wait);
            continue;
        }
        assert!(before == after, "no other row changes");
    }

    // The same step again (the backend repeats it while it lasts) changes nothing.
    let ch = report(
        &mut src,
        id,
        Some(SendPhase::SettingUpKeys { done: 3, of: 12 }),
    );
    assert!(!ch.any());
    // The next step is again the one line.
    let ch = report(&mut src, id, Some(SendPhase::Sending { done: 8, of: 20 }));
    assert_eq!((ch.waits, ch.timeline), (true, false));
    assert_eq!(lines(&src), "Sent to 8 of 20 members · 0:42");
    // The steps end (a 1:1 send after its uploads): back to plain sending.
    let ch = report(&mut src, id, None);
    assert_eq!((ch.waits, ch.timeline), (true, false));
    assert_eq!(lines(&src), "Sending… 0:42");
    assert!(!report(&mut src, id, None).any(), "nothing to forget");
}

/// An account that is logged out holds its queue whatever its last connection state says.
#[test]
fn a_logged_out_account_holds_its_queue_whatever_the_connection_says() {
    let (mut src, sent) = source();
    let mut store = Store::new(&[(1, 3)]);
    make_pending(&mut store.convs.get_mut(&1).expect("conv")[2], false, None);
    let id = store.convs[&1][2].id;
    let answer = |q: &Query| match q {
        Query::Accounts => {
            let mut a = account();
            a.state = AccountState::LoggedOut {
                reason: "unlinked".into(),
            };
            a.conn = ConnState::Online { detail: None };
            QueryData::Accounts(vec![a])
        }
        other => store.answer(other),
    };
    src.reload();
    src.open_conversation(Some(ConversationId(1)));
    for _ in 0..10 {
        for (t, q) in take_queries(&sent) {
            src.on_event(&result(t, answer(&q)));
        }
    }
    let rows = src.timeline(ConversationId(1));
    let r = rows
        .iter()
        .find(|r| r.key == RowKey::Message(id))
        .expect("row");
    assert_eq!(r.wait, "Waiting for this device to be linked again");
}

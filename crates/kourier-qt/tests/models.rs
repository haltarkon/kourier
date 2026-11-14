//! Qt model tests: row counts, roles and `ListOp` application on the real cxx-qt models, and
//! rich-text positions as Qt's document sees them. Runs offscreen on the main thread
//! (`harness = false`): `QT_QPA_PLATFORM=offscreen cargo test -p kourier-qt`.

mod support;

use std::pin::Pin;

use cxx_qt_lib::{QGuiApplication, QModelIndex, QString, QVariant};
use kourier_model::{
    AccountId, ConvKind, ConversationId, MessageId, NetworkKind, RangeKind, Text, TextRange, UserId,
};
use kourier_qt::bridge::qobject::{self, ConversationListModel, TimelineModel};
use kourier_qt::models::{self, ListModel};
use kourier_viewmodel::group::GroupMemberRow;
use kourier_viewmodel::search::{SearchRow, SearchView, SnippetLine};
use kourier_viewmodel::share::ShareTarget;
use kourier_viewmodel::timeline::{AlbumBlock, Block, FileBlock, ImageBlock, UploadView};
use kourier_viewmodel::{ConversationFilter, FixtureSource, NameMap, UiSource, readmore, richtext};

type Check = (&'static str, fn());

fn main() {
    support::answer_list("models");
    if std::env::var_os("QT_QPA_PLATFORM").is_none() {
        // SAFETY: single-threaded test binary, before Qt starts.
        unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    }
    cxx_qt::init_crate!(kourier_qt);
    let _app = QGuiApplication::new();
    let checks: [Check; 23] = [
        (
            "timeline rows, roles and an appended message",
            timeline_rows_and_roles,
        ),
        (
            "stale generations are dropped",
            stale_generations_are_dropped,
        ),
        (
            "conversation list follows sort and filter",
            conversation_list_ops,
        ),
        (
            "random reorders keep the model equal to the rows",
            random_list_ops,
        ),
        ("accounts", accounts_model),
        ("the member rows of a group's page", members_model),
        (
            "the search rows: a view shows, a stale one changes nothing, the snippet is escaped",
            search_rows_model,
        ),
        (
            "the share picker lists only the account's chats that can send",
            share_targets_exclude,
        ),
        (
            "a wait line is patched into its row only",
            a_wait_line_is_patched_into_its_row,
        ),
        (
            "the Qt form of a row is built when QML first asks for it",
            qt_rows_are_built_on_first_data,
        ),
        (
            "a ring is patched into its row only",
            a_ring_is_patched_into_its_row,
        ),
        (
            "rich text positions match Qt's document",
            rich_text_positions,
        ),
        (
            "a row names its author and the people its text mentions",
            a_row_names_its_author_and_its_mentions,
        ),
        (
            "a mention keeps the look of the text, a link its own",
            a_mention_keeps_the_look_of_the_text,
        ),
        (
            "marker backgrounds survive the recoloring of links",
            marks_survive_colorlinks,
        ),
        (
            "a marked name's html escapes the name and holds only the marker spans",
            a_marked_name_is_escaped,
        ),
        (
            "a row carries the pages its menu offers",
            a_row_carries_the_pages_its_menu_offers,
        ),
        (
            "the safety map says when the number swapped under the level",
            the_safety_map_says_when_the_number_swapped,
        ),
        (
            "the safety level takes answers only while it is open",
            the_safety_level_takes_answers_only_while_it_is_open,
        ),
        (
            "the offer map carries every key",
            the_offer_map_carries_every_key,
        ),
        (
            "the switch key says the state",
            the_switch_key_says_the_state,
        ),
        (
            "an Until time that is no date is refused",
            an_until_time_that_is_no_date_is_refused,
        ),
        (
            "the selection lives in the model and survives a reset",
            the_selection_lives_in_the_model,
        ),
    ];
    let mut failed = 0;
    for (name, check) in checks {
        match std::panic::catch_unwind(check) {
            Ok(()) => println!("test {name} ... ok"),
            Err(_) => {
                println!("test {name} ... FAILED");
                failed += 1;
            }
        }
    }
    println!(
        "\ntest result: {}. {} passed; {failed} failed",
        if failed == 0 { "ok" } else { "FAILED" },
        checks.len() - failed
    );
    if failed > 0 {
        std::process::exit(1);
    }
}

fn root() -> QModelIndex {
    QModelIndex::default()
}

/// Role id of a role name in a model's `roleNames()`.
fn role(names: &[&str], name: &str) -> i32 {
    let i = names
        .iter()
        .position(|n| *n == name)
        .unwrap_or_else(|| panic!("no role {name}"));
    models::role_id(i)
}

fn string(v: QVariant) -> String {
    v.value::<QString>()
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn timeline_data(m: &TimelineModel, row: i32, name: &str) -> QVariant {
    let index = m.index(row, 0, &root());
    m.data(&index, role(&models::timeline::ROLES, name))
}

fn conversation_data(m: &ConversationListModel, row: i32, name: &str) -> QVariant {
    let index = m.index(row, 0, &root());
    m.data(&index, role(&models::conversations::ROLES, name))
}

fn timeline_rows_and_roles() {
    let mut src = FixtureSource::new().expect("fixture");
    let conv = src.fixture().long_chat;
    let rows = src.timeline(conv);
    let mut model = qobject::new_timeline_model();
    let mut m: Pin<&mut TimelineModel> = model.pin_mut();
    assert!(models::set_rows(m.as_mut(), 1, rows.clone()));
    assert_eq!(m.row_count(&root()) as usize, rows.len());
    assert_eq!(*m.count() as usize, rows.len());
    let names = m.role_names();
    for name in models::timeline::ROLES {
        assert!(
            names
                .iter()
                .any(|(_, v)| String::from_utf8_lossy(v.as_slice()) == name),
            "roleNames lacks {name}"
        );
    }
    // Row 0 is the newest message, the last row the oldest date header.
    assert_eq!(string(timeline_data(&m, 0, "kind")), "message");
    let last = m.row_count(&root()) - 1;
    assert_eq!(string(timeline_data(&m, last, "kind")), "date");
    let blocks = timeline_data(&m, 0, "blocks");
    assert!(
        blocks
            .value::<cxx_qt_lib::QList<QVariant>>()
            .is_some_and(|l| !l.is_empty())
    );
    // An invalid row or role yields an empty variant.
    let bad = m.index(9999, 0, &root());
    assert!(
        m.data(&bad, models::role_id(0))
            .value::<QString>()
            .is_none()
    );

    // Sending prepends a pending row; the diff inserts exactly one row at 0.
    let id = src
        .send_text(conv, "hello there", None)
        .expect("send")
        .expect("the fixture assigns ids");
    let after = src.timeline(conv);
    assert!(models::set_rows(m.as_mut(), 2, after.clone()));
    assert_eq!(m.row_count(&root()) as usize, rows.len() + 1);
    assert_eq!(timeline_data(&m, 0, "messageId").value::<i64>(), Some(id.0));
    assert_eq!(string(timeline_data(&m, 0, "receipt")), "pending");
    assert_eq!(timeline_data(&m, 0, "own").value::<bool>(), Some(true));
    assert_eq!(m.rows(), after.as_slice());
}

/// A reset and an insert build no `QString` and no `QVariant`: a row's Qt form is made by the
/// first `data()` of the row (structs.md #1: QML reads the visible rows only), a changed row is
/// emptied and built again when read (its announced roles are the same), and a read gives what
/// an eager build gave. Mutant: `apply` builds every row again.
fn qt_rows_are_built_on_first_data() {
    let mut src = FixtureSource::new().expect("fixture");
    let conv = src.fixture().long_chat;
    let rows = src.timeline(conv);
    assert!(rows.len() > 6, "{}", rows.len());
    let mut model = qobject::new_timeline_model();
    let mut m: Pin<&mut TimelineModel> = model.pin_mut();
    assert!(models::set_rows(m.as_mut(), 1, rows.clone()));
    assert_eq!(m.row_count(&root()) as usize, rows.len());
    assert_eq!(m.store().built(), 0, "a reset builds no row");
    let kind0 = string(timeline_data(&m, 0, "kind"));
    let kind1 = string(timeline_data(&m, 1, "kind"));
    assert!(!kind0.is_empty() && !kind1.is_empty());
    assert_eq!(m.store().built(), 2, "only the rows asked for");
    // The same row again is not built again.
    timeline_data(&m, 0, "kind");
    assert_eq!(m.store().built(), 2);
    // An insert at the top (a sent message) builds nothing either.
    let id = src
        .send_text(conv, "lazy", None)
        .expect("send")
        .expect("the fixture assigns ids");
    let after = src.timeline(conv);
    assert!(models::set_rows(m.as_mut(), 2, after.clone()));
    assert_eq!(timeline_data(&m, 0, "messageId").value::<i64>(), Some(id.0));
    assert!(m.store().built() <= 3, "{}", m.store().built());
    let built = m.store().built();
    // A patched row is emptied; it is built again with the new values when it is read.
    let mut row = after[0].clone();
    row.blocks.clear();
    row.edited = !row.edited;
    let before = m.store().built();
    if models::patch_row(m.as_mut(), 0, row.clone()) {
        assert!(m.store().built() < before, "a patched row is emptied");
    }
    let _ = built;
    assert_eq!(m.rows()[0].edited, row.edited);
    // Whatever was never read is built from the rows as they are now.
    let last = m.row_count(&root()) - 1;
    assert_eq!(
        string(timeline_data(&m, last, "kind")),
        "date",
        "the last row builds from its view row"
    );
}

fn stale_generations_are_dropped() {
    let src = FixtureSource::new().expect("fixture");
    let conv = src.fixture().long_chat;
    let rows = src.timeline(conv);
    let mut model = qobject::new_timeline_model();
    let mut m = model.pin_mut();
    assert!(models::set_rows(m.as_mut(), 5, rows.clone()));
    assert!(
        !models::set_rows(m.as_mut(), 4, Vec::new()),
        "older result applied"
    );
    assert_eq!(m.row_count(&root()) as usize, rows.len());
    assert!(
        models::set_rows(m.as_mut(), 5, rows),
        "same generation re-applies"
    );
}

fn conversation_list_ops() {
    let mut src = FixtureSource::new().expect("fixture");
    let all = ConversationFilter::default();
    let mut model = qobject::new_conversation_list_model();
    let mut m = model.pin_mut();
    let rows = src.conversations(&all);
    models::set_rows(m.as_mut(), 1, rows.clone());
    assert_eq!(m.row_count(&root()) as usize, rows.len());
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(string(conversation_data(&m, i as i32, "title")), r.title);
    }
    // A message in the last conversation moves it right below the pinned ones.
    let last = rows.last().expect("rows").id;
    src.send_text(last, "bump", None).expect("send");
    let bumped = src.conversations(&all);
    models::set_rows(m.as_mut(), 2, bumped.clone());
    for (i, r) in bumped.iter().enumerate() {
        assert_eq!(
            conversation_data(&m, i as i32, "conversationId").value::<i64>(),
            Some(r.id.0),
            "row {i}"
        );
    }
    let pinned = bumped.iter().take_while(|r| r.pinned()).count();
    assert_eq!(bumped[pinned].id, last);
    assert_eq!(
        string(conversation_data(&m, pinned as i32, "preview")),
        "bump"
    );
    // Filtering by account removes rows.
    let work = ConversationFilter {
        hidden: src
            .accounts()
            .iter()
            .filter(|a| a.id != AccountId(2))
            .map(|a| a.uuid)
            .collect(),
        query: String::new(),
        archived: false,
        ..Default::default()
    };
    let filtered = src.conversations(&work);
    models::set_rows(m.as_mut(), 3, filtered.clone());
    assert_eq!(m.row_count(&root()) as usize, filtered.len());
    assert!(filtered.len() < bumped.len() && !filtered.is_empty());
    assert_eq!(m.store().resyncs, 0, "a diff fell back to a reset");
}

/// A tiny deterministic generator (no rand dependency in tests).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        ((self.0 >> 33) as usize) % n.max(1)
    }
}

fn random_list_ops() {
    let src = FixtureSource::new().expect("fixture");
    let base = src.conversations(&ConversationFilter::default());
    let mut model = qobject::new_conversation_list_model();
    let mut m = model.pin_mut();
    let mut rng = Lcg(42);
    for generation in 1..200u64 {
        // Random subset in random order, sometimes with an edited title.
        let mut rows = base.clone();
        let keep = 1 + rng.next(rows.len());
        for _ in 0..rows.len() {
            let (a, b) = (rng.next(rows.len()), rng.next(rows.len()));
            rows.swap(a, b);
        }
        rows.truncate(keep);
        if let Some(r) = rows.get_mut(rng.next(keep)) {
            r.title = format!("{} {generation}", r.title);
        }
        // Fields without a role of their own (sort keys, pin position) change too.
        if let Some(r) = rows.get_mut(rng.next(keep)) {
            r.last_activity += 1;
            r.pinned_order = r.pinned_order.map(|p| p + 1);
        }
        models::set_rows(m.as_mut(), generation, rows.clone());
        assert_eq!(m.row_count(&root()) as usize, rows.len());
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(
                conversation_data(&m, i as i32, "conversationId").value::<i64>(),
                Some(r.id.0)
            );
            assert_eq!(string(conversation_data(&m, i as i32, "title")), r.title);
        }
        assert_eq!(m.store().view, rows);
    }
    assert_eq!(m.store().resyncs, 0, "a diff fell back to a reset");
}

fn accounts_model() {
    let src = FixtureSource::new().expect("fixture");
    let rows = src.accounts();
    let mut model = qobject::new_account_list_model();
    let mut m = model.pin_mut();
    models::set_rows(m.as_mut(), 1, rows.clone());
    assert_eq!(m.row_count(&root()) as usize, 3);
    let index = m.index(2, 0, &root());
    let network = m.data(&index, models::role_id(2));
    assert_eq!(string(network), "Threema");
}

fn member(n: u32, name: &str, admin: bool) -> GroupMemberRow {
    GroupMemberRow {
        user: UserId::parse(&format!("aci:{n:08x}-0000-4000-8000-000000000000")).expect("an id"),
        name: name.to_owned(),
        avatar_url: String::new(),
        admin,
        is_self: false,
    }
}

/// The member rows of the group page: the roles, the filter, and what a refresh announces.
fn members_model() {
    use std::sync::{Arc, Mutex};

    let roles = ["user", "name", "avatarUrl", "admin", "isSelf"];
    let people = vec![
        member(1, "Anna", true),
        member(2, "Boris", false),
        member(3, "Clara", false),
        member(4, "Dmytro", false),
    ];
    let mut model = qobject::new_member_list_model();
    let mut m = model.pin_mut();
    /// A `dataChanged`: the row and the role ids.
    type Changed = (i32, Vec<i32>);
    let seen: Arc<Mutex<Vec<Changed>>> = Arc::default();
    let sink = seen.clone();
    let _connection = m.as_mut().on_data_changed(move |_, top, _, changed| {
        let ids = (0..changed.len())
            .filter_map(|i| changed.get(i).copied())
            .collect();
        sink.lock().expect("lock").push((top.row(), ids));
    });
    let name_of = |m: &qobject::MemberListModel, row: i32| {
        string(m.data(&m.index(row, 0, &root()), role(&roles, "name")))
    };
    m.as_mut().set_members(people.clone());
    assert_eq!(m.row_count(&root()), 4);
    assert_eq!(*m.count(), 4);
    assert_eq!(name_of(&m, 2), "Clara");
    assert_eq!(m.user_at(1).to_string(), people[1].user.as_str());
    assert_eq!(m.row_of(&QString::from(people[3].user.as_str())), 3);
    assert_eq!(m.row_of(&QString::from("aci:nobody")), -1);
    assert!(
        m.data(&m.index(0, 0, &root()), role(&roles, "admin"))
            .value::<bool>()
            .expect("a bool")
    );

    // The same rows again: nothing is announced.
    seen.lock().expect("lock").clear();
    m.as_mut().set_members(people.clone());
    assert!(seen.lock().expect("lock").is_empty());

    // A rename: ONE dataChanged, of the name role only; the row stays where it is.
    let mut renamed = people.clone();
    renamed[1].name = "Borys".into();
    m.as_mut().set_members(renamed.clone());
    assert_eq!(
        *seen.lock().expect("lock"),
        vec![(1, vec![role(&roles, "name")])]
    );
    assert_eq!(name_of(&m, 1), "Borys");

    // A join, a leave and a new administrator keep the rows equal to the page.
    let mut next = renamed.clone();
    next.insert(2, member(9, "Bohdan", false));
    next[4].admin = true;
    let last = next.remove(4);
    next.insert(1, last);
    next.remove(0);
    m.as_mut().set_members(next.clone());
    assert_eq!(m.row_count(&root()), 4);
    assert_eq!(m.store().view, next);
    assert_eq!(m.store().resyncs, 0, "a diff fell back to a reset");

    // The filter: the viewmodel's rule, applied in Rust; narrowing, widening, and no match.
    m.as_mut().set_filter_text(&QString::from(" R "));
    let want = next
        .iter()
        .filter(|r| r.name.to_lowercase().contains('r'))
        .count();
    assert!(want > 0 && want < next.len());
    assert_eq!(m.row_count(&root()) as usize, want);
    assert_eq!(*m.count() as usize, want);
    m.as_mut().set_filter_text(&QString::from("zzz"));
    assert_eq!(m.row_count(&root()), 0);
    m.as_mut().set_filter_text(&QString::default());
    assert_eq!(m.row_count(&root()) as usize, next.len());
    // A refresh while a filter is set lists only the matches of the new page.
    m.as_mut().set_filter_text(&QString::from("bo"));
    let mut more = next.clone();
    more.push(member(11, "Bogdan", false));
    m.as_mut().set_members(more);
    assert_eq!(m.row_count(&root()), 3);
    // An empty page empties the list, and the filter stays.
    m.as_mut().set_members(Vec::new());
    assert_eq!(m.row_count(&root()), 0);
    assert_eq!(m.filter().to_string(), "bo");
    assert_eq!(m.store().resyncs, 0);

    // The row's identity is the member, whatever else of the row changes.
    let a = member(1, "Anna", false);
    let mut b = a.clone();
    b.name = "Hanna".into();
    b.admin = true;
    type Members = qobject::MemberListModel;
    assert!(<Members as ListModel>::key(&a) == <Members as ListModel>::key(&b));
    assert!(
        <Members as ListModel>::key(&a) != <Members as ListModel>::key(&member(2, "Anna", false))
    );
}

fn hit(message: i64, chat: i64, html: &str, plain: &str) -> SearchRow {
    SearchRow {
        message: MessageId(message),
        conv: ConversationId(chat),
        account: AccountId(1),
        network: NetworkKind::Fake,
        kind: ConvKind::Direct,
        is_self: false,
        title: format!("Chat {chat}"),
        avatar_url: String::new(),
        author: String::new(),
        snippet: SnippetLine {
            html: html.to_owned(),
            plain: plain.to_owned(),
        },
        icon: String::new(),
        time_label: "10:00".to_owned(),
        sent_at: message,
        sort_key: message,
        archived: false,
        account_badge: String::new(),
        badge_icon: String::new(),
        badge_number: 0,
        tint: String::new(),
        sender: "Sender".to_owned(),
        sender_avatar_url: String::new(),
        own: false,
    }
}

fn view_of(rows: Vec<SearchRow>) -> SearchView {
    SearchView {
        active: true,
        rows,
        ..SearchView::default()
    }
}

/// The message search's rows (G1m): the model shows what a search answer holds, an answer of an
/// older generation changes nothing (rows and properties), a row's snippet role is the escaped
/// markup and its plain twin is the text, and a re-read that drops a hit removes that row only.
fn search_rows_model() {
    let mut model = qobject::new_search_list_model();
    let mut m = model.pin_mut();
    let data = |m: &qobject::SearchListModel, row: i32, name: &str| {
        m.data(
            &m.index(row, 0, &root()),
            role(&models::search::ROLES, name),
        )
    };
    // Nothing is shown before a search.
    assert_eq!(m.row_count(&root()), 0);
    assert!(!*m.active());
    assert!(*m.index_ready());

    let rows = vec![
        hit(30, 7, "a &lt;b&gt; <b>word</b>", "a <b> word"),
        hit(20, 7, "<b>word</b> again", "word again"),
        hit(10, 8, "the <b>word</b>", "the word"),
    ];
    let mut view = view_of(rows.clone());
    view.more = true;
    view.index_ready = false;
    view.index_percent = 40;
    assert!(m.as_mut().show(5, view));
    assert_eq!(m.row_count(&root()), 3);
    assert_eq!(*m.count(), 3);
    assert!(*m.active() && *m.more() && !*m.index_ready());
    assert_eq!(*m.index_percent(), 40);
    assert_eq!(m.message_at(1), 20);
    assert_eq!(m.conversation_at(2), 8);
    assert_eq!(m.message_at(3), 0);
    // The snippet: escaped markup for a StyledText label, the plain text apart.
    assert_eq!(
        string(data(&m, 0, "snippetHtml")),
        "a &lt;b&gt; <b>word</b>"
    );
    assert_eq!(string(data(&m, 0, "snippetPlain")), "a <b> word");
    assert_eq!(string(data(&m, 1, "title")), "Chat 7");

    // An answer of an older generation changes nothing of the rows or the properties.
    let mut stale = view_of(vec![hit(99, 9, "x", "x")]);
    stale.more = false;
    stale.index_ready = true;
    assert!(!m.as_mut().show(4, stale));
    assert_eq!(m.row_count(&root()), 3);
    assert_eq!(m.message_at(0), 30);
    assert!(*m.more() && !*m.index_ready());

    // A re-read without the deleted message removes that row alone.
    let after = vec![rows[0].clone(), rows[2].clone()];
    assert!(m.as_mut().show(6, view_of(after)));
    assert_eq!(m.row_count(&root()), 2);
    assert_eq!(m.message_at(0), 30);
    assert_eq!(m.message_at(1), 10);
    // The search ends: the rows and the flags go.
    assert!(m.as_mut().show(7, SearchView::default()));
    assert_eq!(m.row_count(&root()), 0);
    assert!(!*m.active());
    assert_eq!(m.store().resyncs, 0);
}

fn share_target(id: i64, account: i64, title: &str, can_send: bool) -> ShareTarget {
    ShareTarget {
        id: ConversationId(id),
        account: AccountId(account),
        title: title.to_owned(),
        avatar_url: String::new(),
        is_self: false,
        can_send,
        archived: false,
        pinned_order: None,
        last_activity: id,
    }
}

/// P5c's picker (`App.shareTargets`): only the chats of the person's account where the user can
/// send are listed (Note to Self included, another account's chats and a chat that cannot send
/// not), the search narrows them in Rust, and a refresh announces only what changed.
fn share_targets_exclude() {
    let roles = ["conversation", "title", "avatarUrl", "isSelf"];
    let mut note = share_target(4, 1, "Note to Self", true);
    note.is_self = true;
    let all = vec![
        share_target(1, 1, "Ada", true),
        share_target(2, 2, "Ada at work", true),
        share_target(3, 1, "Announcements", false),
        note,
        share_target(5, 1, "Book club", true),
    ];
    let mut model = qobject::new_share_target_list_model();
    let mut m = model.pin_mut();
    assert_eq!(m.row_count(&root()), 0, "no person, no rows");
    m.as_mut().set_targets(AccountId(1), all.clone());
    let listed: Vec<i64> = (0..m.row_count(&root()))
        .map(|r| m.conversation_at(r))
        .collect();
    assert_eq!(listed, vec![5, 4, 1], "newest first; 2 and 3 not listed");
    assert_eq!(*m.count(), 3);
    assert!(!m.is_listed(3) && !m.is_listed(2) && m.is_listed(4));
    assert_eq!(m.title_of(5).to_string(), "Book club");
    assert_eq!(m.title_of(3).to_string(), "", "not listed, no title");
    assert!(
        m.data(&m.index(1, 0, &root()), role(&roles, "isSelf"))
            .value::<bool>()
            .expect("a bool")
    );
    assert_eq!(m.conversation_at(9), 0);

    m.as_mut().set_filter_text(&QString::from(" ADA "));
    assert_eq!(m.row_count(&root()), 1);
    assert_eq!(m.conversation_at(0), 1);
    m.as_mut().set_filter_text(&QString::from("announce"));
    assert_eq!(
        m.row_count(&root()),
        0,
        "a chat that cannot send is never found"
    );
    m.as_mut().set_filter_text(&QString::default());
    // Another person's account lists that account's chats only.
    m.as_mut().set_targets(AccountId(2), all);
    assert_eq!(m.row_count(&root()), 1);
    assert_eq!(m.conversation_at(0), 2);
    assert_eq!(m.store().resyncs, 0);
}

fn rich_text_positions() {
    let src = FixtureSource::new().expect("fixture");
    let mut texts = Vec::new();
    for c in &src.fixture().conversations {
        for row in src.timeline(c.id) {
            for b in row.blocks {
                if let Block::Text(t) = b {
                    texts.push(t);
                }
            }
        }
    }
    let names = NameMap::new();
    for body in [
        "a  b\n\n  c\td \nx",
        "😀 secret 👨\u{200D}👩\u{200D}👧 end",
        "<b>not bold</b> & \"quotes\"",
        "see https://kde.org/plasma, ok",
        " leading and trailing  ",
    ] {
        texts.push(richtext::render(&Text::plain(body), &names));
    }
    // A text shown in part: its markup shows its plain text and the ellipsis that says it
    // goes on, spoilers included.
    let long = Text {
        body: format!("{} 😀 {}", "word ".repeat(170), "tail ".repeat(100)),
        ranges: vec![
            TextRange {
                start: 700,
                len: 400,
                kind: RangeKind::Bold,
            },
            TextRange {
                start: 780,
                len: 100,
                kind: RangeKind::Spoiler,
            },
        ],
    };
    let cut = readmore::render_limited(&long, &names, readmore::INITIAL);
    assert!(cut.more && !cut.spoilers.is_empty());
    texts.push(cut);
    assert!(texts.len() > 20);
    for t in &texts {
        let shown = qobject::rich_text_plain(&QString::from(t.html.as_str())).to_string();
        let expected = if t.more {
            format!("{}…", t.plain)
        } else {
            t.plain.clone()
        };
        assert_eq!(shown, expected, "html {:?}", t.html);
        let len16 = shown.encode_utf16().count() as u32;
        for s in &t.spoilers {
            assert!(s.start + s.len <= len16);
        }
    }
}

/// People pages P3: the `authorId` role carries the sender's user id (a click on the name or the picture
/// opens that person), and a text block carries the people its `kourier:mention/<i>` links open,
/// each with whether a spoiler hides it. Only the role changes when the id does.
fn a_row_names_its_author_and_its_mentions() {
    use cxx_qt_lib::{QList, QMap, QMapPair_QString_QVariant};
    let src = FixtureSource::new().expect("fixture");
    let conv = src.fixture().long_chat;
    let rows = src.timeline(conv);
    let mut model = qobject::new_timeline_model();
    let mut m: Pin<&mut TimelineModel> = model.pin_mut();
    assert!(models::set_rows(m.as_mut(), 1, rows.clone()));
    // The role is the viewmodel row's author id as text ("" for a row without one).
    for (i, r) in rows.iter().enumerate() {
        let want = r.author_id.as_ref().map_or("", |u| u.as_str());
        assert_eq!(string(timeline_data(&m, i as i32, "authorId")), want);
    }
    assert!(
        rows.iter().any(|r| r.author_id.is_some()) && rows.iter().any(|r| r.author_id.is_none()),
        "the fixture chat has rows with and without an author"
    );

    let olena = UserId::parse("aci:olena").expect("id");
    let ivan = UserId::parse("aci:ivan").expect("id");
    let body = format!("hi {0} and {0}", kourier_model::text::MENTION_PLACEHOLDER);
    let text = Text {
        body: body.clone(),
        ranges: vec![
            TextRange {
                start: 3,
                len: 3,
                kind: RangeKind::Mention(olena.clone()),
            },
            TextRange {
                start: 11,
                len: 3,
                kind: RangeKind::Mention(ivan.clone()),
            },
            TextRange {
                start: 11,
                len: 3,
                kind: RangeKind::Spoiler,
            },
        ],
    };
    let mut row = rows[0].clone();
    row.author_id = Some(olena.clone());
    row.blocks = vec![Block::Text(richtext::render(&text, &NameMap::new()))];
    let roles = <TimelineModel as ListModel>::changed_roles(&rows[0], &row);
    assert_ne!(roles.0 & (1 << 22), 0, "authorId is a role of its own");
    let mut same_text = row.clone();
    same_text.author_id = Some(ivan.clone());
    let roles = <TimelineModel as ListModel>::changed_roles(&row, &same_text);
    assert_eq!(roles.0, 1 << 22, "a new author changes that role only");
    assert!(models::patch_row(m.as_mut(), 0, row.clone()));
    assert_eq!(string(timeline_data(&m, 0, "authorId")), "aci:olena");

    let blocks = timeline_data(&m, 0, "blocks")
        .value::<QList<QVariant>>()
        .expect("blocks");
    let block = blocks
        .get(0)
        .and_then(|b| b.value::<QMap<QMapPair_QString_QVariant>>())
        .expect("a text block");
    let html = string(block.get(&QString::from("html")).expect("html"));
    // (No text in the message: it holds a name.)
    assert!(
        html.contains("href=\"kourier:mention/0\""),
        "the first mention is no link"
    );
    assert!(
        html.contains("href=\"kourier:mention/1\""),
        "the second mention is no link"
    );
    let mentions = block
        .get(&QString::from("mentions"))
        .and_then(|v| v.value::<QList<QVariant>>())
        .expect("mentions");
    assert_eq!(mentions.len(), 2);
    let entry = |i: isize| {
        let e = mentions
            .get(i)
            .and_then(|v| v.value::<QMap<QMapPair_QString_QVariant>>())
            .expect("a mention");
        (
            string(e.get(&QString::from("user")).expect("user")),
            e.get(&QString::from("spoiler"))
                .and_then(|v| v.value::<bool>())
                .expect("spoiler"),
        )
    };
    assert_eq!(entry(0), ("aci:olena".to_owned(), false));
    assert_eq!(entry(1), ("aci:ivan".to_owned(), true));
}

/// People pages P3: a mention is a link for Qt (a click opens the person) but it keeps the look it
/// had as plain bold text, the text's ink and no underline; an ordinary link takes the link color
/// and Qt's underline. Read from the document `colorLinks` works on.
fn a_mention_keeps_the_look_of_the_text() {
    use cxx_qt_lib::{QColor, QColorNameFormat};
    let link = QColor::from_rgb(10, 20, 200);
    let ink = QColor::from_rgb(30, 40, 50);
    let hex = |c: &QColor| c.name(QColorNameFormat::HexRgb).to_string();
    let looks = qobject::rich_text_link_looks(
        &QString::from(
            "<a href=\"kourier:mention/0\"><b>@Olena</b></a> and \
             <a href=\"https://kde.org\">kde.org</a>",
        ),
        &link,
        &ink,
    )
    .to_string();
    let lines: Vec<&str> = looks.lines().collect();
    assert_eq!(lines.len(), 2, "two links, one line each");
    assert_eq!(
        lines[0],
        format!("kourier:mention/0 {} 0 1", hex(&ink)),
        "the mention: the ink, no underline, bold"
    );
    assert_eq!(
        lines[1],
        format!("https://kde.org {} 1 0", hex(&link)),
        "an ordinary link: the link color and the underline"
    );
}

/// RegExp filters R3: the marker backgrounds are set on the document by `applyMarks` and
/// `colorLinks` (which merges a foreground only) leaves them alone; the link keeps its own colour
/// under a mark.
fn marks_survive_colorlinks() {
    use cxx_qt_lib::{QColor, QColorNameFormat, QList, QMap, QMapPair_QString_QVariant};
    let link = QColor::from_rgb(10, 20, 200);
    let ink = QColor::from_rgb(30, 40, 50);
    let hex = |c: &QColor| c.name(QColorNameFormat::HexRgb).to_string();
    let mark = |start: i32, len: i32, color: &str| {
        let mut m = QMap::<QMapPair_QString_QVariant>::default();
        m.insert(QString::from("start"), QVariant::from(&start));
        m.insert(QString::from("len"), QVariant::from(&len));
        m.insert(
            QString::from("color"),
            QVariant::from(&QString::from(color)),
        );
        QVariant::from(&m)
    };
    let mut list = QList::<QVariant>::default();
    list.append(mark(0, 3, "#80ffd60a"));
    let marks = QVariant::from(&list);
    let looks = qobject::rich_text_marked(
        &QString::from("<a href=\"https://kde.org\">kde.org</a> and more"),
        &marks,
        &link,
        &ink,
    )
    .to_string();
    let lines: Vec<&str> = looks.lines().collect();
    assert_eq!(lines.len(), 1, "one marked fragment: {looks}");
    assert_eq!(
        lines[0],
        format!("0 3 #80ffd60a {}", hex(&link)),
        "the background stays and the link keeps its colour"
    );
}

/// RegExp filters R3 (the third form of rich text, `CLAUDE.md`): `markedNameHtml` makes the html of
/// a name from the ESCAPED name and nothing else but `<span style="background-color:#rrggbb">`:
/// a name that carries markup (an `<img>` pointing at a remote address, `&`, a quote) shows as text.
fn a_marked_name_is_escaped() {
    use cxx_qt_lib::{QColor, QList, QMap, QMapPair_QString_QVariant};
    let name = r#"Ann <img src="https://example.invalid/x.png"> & "Co""#;
    let shown = QString::from(name);
    let surface = QColor::from_rgb(255, 255, 255);
    let mark = |start: i32, len: i32| {
        let mut m = QMap::<QMapPair_QString_QVariant>::default();
        m.insert(QString::from("start"), QVariant::from(&start));
        m.insert(QString::from("len"), QVariant::from(&len));
        m.insert(
            QString::from("color"),
            QVariant::from(&QString::from("#80ffd60a")),
        );
        QVariant::from(&m)
    };
    let mut list = QList::<QVariant>::default();
    // Over "Ann ", and over the `&` inside the name.
    list.append(mark(0, 4));
    let amp = i32::try_from(
        name.encode_utf16()
            .position(|u| u == u16::from(b'&'))
            .expect("&"),
    )
    .expect("small");
    list.append(mark(amp, 1));
    let html =
        qobject::marked_name_html(&shown, &shown, &QVariant::from(&list), &surface).to_string();
    assert!(
        !html.contains("<img"),
        "no tag of the name survives: {html}"
    );
    assert!(
        !html.contains('"') || html.matches("style=\"").count() == 2,
        "{html}"
    );
    // Strip the only markup that may be there; what is left holds no raw `<`, `>` or `&`.
    let mut rest = html.clone();
    while let Some(i) = rest.find("<span style=\"background-color:#") {
        let end = rest[i..].find("\">").expect("the tag ends") + i + 2;
        let tag = &rest[i..end];
        assert_eq!(
            tag.len(),
            "<span style=\"background-color:#rrggbb\">".len(),
            "{tag}"
        );
        rest.replace_range(i..end, "");
    }
    rest = rest.replace("</span>", "");
    for entity in ["&lt;", "&gt;", "&amp;", "&quot;"] {
        rest = rest.replace(entity, "");
    }
    assert!(
        !rest.contains(['<', '>', '&', '"']),
        "a raw character is left: {rest}"
    );
    assert!(
        html.contains("&lt;img"),
        "the name's tag is shown as text: {html}"
    );
    assert!(html.contains("&amp;"), "{html}");
    // Without marks the name is escaped the same way.
    let plain = qobject::marked_name_html(
        &shown,
        &shown,
        &QVariant::from(&QList::<QVariant>::default()),
        &surface,
    )
    .to_string();
    assert!(
        !plain.contains("<img") && !plain.contains("<span"),
        "{plain}"
    );
}

/// People pages P3 (F4): the `pages` role is the row's menu entries, `[{user, label}]` in the viewmodel's order,
/// the flags `showSender` / `showPeople` are in `actions` (they make the menu worth opening), and only the
/// `pages` role (and `actions` with it) changes when the entries do.
fn a_row_carries_the_pages_its_menu_offers() {
    use cxx_qt_lib::{QList, QMap, QMapPair_QString_QVariant};
    use kourier_viewmodel::timeline::PageEntry;
    let src = FixtureSource::new().expect("fixture");
    let conv = src.fixture().long_chat;
    let rows = src.timeline(conv);
    let mut model = qobject::new_timeline_model();
    let mut m: Pin<&mut TimelineModel> = model.pin_mut();
    assert!(models::set_rows(m.as_mut(), 1, rows.clone()));
    let pages_of = |m: &TimelineModel, row: i32| {
        timeline_data(m, row, "pages")
            .value::<QList<QVariant>>()
            .expect("a list")
    };
    assert_eq!(
        pages_of(&m, 0).len(),
        0,
        "a row with no page has an empty list"
    );

    let who = UserId::parse("aci:olena").expect("id");
    let mut row = rows[0].clone();
    row.pages = vec![
        PageEntry {
            user: who.clone(),
            label: "Show Sender's Page".into(),
            safety: false,
        },
        PageEntry {
            user: who.clone(),
            label: "Show Your Page".into(),
            safety: true,
        },
    ];
    row.actions.show_sender = true;
    row.actions.show_people = true;
    let roles = <TimelineModel as ListModel>::changed_roles(&rows[0], &row);
    assert_ne!(roles.0 & (1 << 23), 0, "the pages are a role of their own");
    assert_ne!(roles.0 & (1 << 17), 0, "the flags are in `actions`");
    assert_eq!(
        roles.0 & !((1 << 23) | (1 << 17)),
        0,
        "nothing else changes"
    );
    assert!(models::patch_row(m.as_mut(), 0, row));
    let pages = pages_of(&m, 0);
    assert_eq!(pages.len(), 2);
    let entry = |i: isize| {
        let e = pages
            .get(i)
            .and_then(|v| v.value::<QMap<QMapPair_QString_QVariant>>())
            .expect("an entry");
        (
            string(e.get(&QString::from("user")).expect("user")),
            string(e.get(&QString::from("label")).expect("label")),
            e.get(&QString::from("safety"))
                .and_then(|v| v.value::<bool>())
                .expect("safety"),
        )
    };
    assert_eq!(
        entry(0),
        (
            "aci:olena".to_owned(),
            "Show Sender's Page".to_owned(),
            false
        )
    );
    assert_eq!(entry(1).1, "Show Your Page");
    assert!(entry(1).2, "a safety-number entry says so (P5a BQ-3)");
    let actions = timeline_data(&m, 0, "actions")
        .value::<QMap<QMapPair_QString_QVariant>>()
        .expect("actions");
    let flag = |k: &str| {
        actions
            .get(&QString::from(k))
            .and_then(|v| v.value::<bool>())
            .expect("a flag")
    };
    assert!(flag("showSender") && flag("showPeople") && flag("any"));
}

/// A report of an upload (upload progress) moves the rings on the files of one own message
/// without a rebuild: the row is taken over by `patch_row`, its Qt form is built again and the
/// only role announced is `blocks` (and `actions` when the core's X flag flipped), never a
/// reset of the model.
fn a_ring_is_patched_into_its_row() {
    let src = FixtureSource::new().expect("fixture");
    let conv = src.fixture().long_chat;
    let rows = src.timeline(conv);
    let mut model = qobject::new_timeline_model();
    let mut m: Pin<&mut TimelineModel> = model.pin_mut();
    assert!(models::set_rows(m.as_mut(), 1, rows.clone()));
    let ring = |share: f32, label: &str, cancel: bool| {
        Some(UploadView {
            share,
            label: label.into(),
            cancel,
        })
    };
    let picture = |upload| ImageBlock {
        upload,
        ..ImageBlock::default()
    };
    let mut row = rows[0].clone();
    row.blocks = vec![
        Block::Album(AlbumBlock::new(vec![
            picture(ring(0.0, "", true)),
            picture(None),
        ])),
        Block::File(FileBlock {
            upload: ring(0.0, "", true),
            ..FileBlock::default()
        }),
    ];
    assert!(models::patch_row(m.as_mut(), 0, row.clone()));
    let resets = m.store().resyncs;

    let mut moved = row.clone();
    if let Block::File(f) = &mut moved.blocks[1] {
        f.upload = ring(0.45, "45%", true);
    }
    if let Block::Album(a) = &mut moved.blocks[0] {
        a.items[1].upload = ring(0.1, "10%", true);
        a.more = ring(0.3, "30%", true);
    }
    let roles = <TimelineModel as ListModel>::changed_roles(&row, &moved);
    assert_eq!(roles.0, 1 << 10, "blocks only");
    assert!(models::patch_row(m.as_mut(), 0, moved.clone()));
    assert_eq!(m.rows()[0], moved, "the viewmodel row follows");
    assert_eq!(m.store().resyncs, resets, "the model was not reset");
    assert_eq!(m.rows().len(), rows.len());
    // The same ring again changes nothing; the X that goes (the core's flag) flips `actions`
    // with the blocks.
    assert!(!models::patch_row(m.as_mut(), 0, moved.clone()));
    let mut asked = moved.clone();
    asked.actions.cancel_sending = true;
    let roles = <TimelineModel as ListModel>::changed_roles(&moved, &asked);
    assert_eq!(roles.0, 1 << 17, "actions only");
}

/// The per-second tick (link feedback L6) changes the lines under the messages on their way
/// one row at a time: `patch_row` takes over a row with the same identity, rebuilds its Qt
/// form, and the roles it announces are the two of the line, nothing else.
fn a_wait_line_is_patched_into_its_row() {
    let src = FixtureSource::new().expect("fixture");
    let conv = src.fixture().long_chat;
    let rows = src.timeline(conv);
    let mut model = qobject::new_timeline_model();
    let mut m: Pin<&mut TimelineModel> = model.pin_mut();
    assert!(models::set_rows(m.as_mut(), 1, rows.clone()));
    assert_eq!(string(timeline_data(&m, 0, "wait")), "");

    let mut line = rows[0].clone();
    line.wait = "Sending…".into();
    line.wait_timer = "0:42".into();
    // Only the two roles of the line differ, and those are what a change announces.
    let roles = <TimelineModel as ListModel>::changed_roles(&rows[0], &line);
    assert_eq!(roles.0, (1 << 20) | (1 << 21), "wait and waitTimer only");
    assert!(models::patch_row(m.as_mut(), 0, line.clone()));
    assert_eq!(string(timeline_data(&m, 0, "wait")), "Sending…");
    assert_eq!(string(timeline_data(&m, 0, "waitTimer")), "0:42");
    assert_eq!(string(timeline_data(&m, 1, "wait")), "", "no other row");
    assert_eq!(m.rows()[0], line, "the viewmodel row follows");
    assert_eq!(m.rows().len(), rows.len());

    // The next second.
    line.wait_timer = "0:43".into();
    assert!(models::patch_row(m.as_mut(), 0, line.clone()));
    assert_eq!(string(timeline_data(&m, 0, "waitTimer")), "0:43");
    // Nothing new, another row's identity, a row that is not there: nothing happens.
    assert!(!models::patch_row(m.as_mut(), 0, line.clone()));
    assert!(!models::patch_row(m.as_mut(), 0, rows[1].clone()));
    assert!(!models::patch_row(m.as_mut(), rows.len(), line));
    assert_eq!(string(timeline_data(&m, 0, "waitTimer")), "0:43");
}

/// U2: `App.safety` carries `swapped` only for another key of the SAME person than the level on
/// screen held (both known, V3: held through answers without a number); a first answer, a
/// change of state alone, another person and a person without a number are not a swap.
fn the_safety_map_says_when_the_number_swapped() {
    use cxx_qt_lib::{QMap, QMapPair_QString_QVariant};
    use kourier_model::{
        IdentityFingerprint, Redacted, SafetyNumber, SafetyUnavailable, VerifyState,
    };
    use kourier_viewmodel::safety::{SafetyView, safety_view};
    let view = |user: &str, key: u8, state: VerifyState| -> SafetyView {
        safety_view(
            AccountId(1),
            UserId::parse(user).expect("id"),
            "Ada",
            &Ok(SafetyNumber {
                digits: Redacted::new("1".repeat(60)),
                scannable: Redacted::new(vec![8, 2, key]),
                key: IdentityFingerprint(vec![key; 33]),
                state,
            }),
            false,
        )
    };
    let k1 = view("aci:ada", 1, VerifyState::Default);
    let k2 = view("aci:ada", 2, VerifyState::Default);
    assert!(
        !models::timeline::safety_swapped(None, &k1),
        "a first answer"
    );
    assert!(
        !models::timeline::safety_swapped(Some(&k1), &k1),
        "the same number"
    );
    assert!(
        !models::timeline::safety_swapped(Some(&k1), &view("aci:ada", 1, VerifyState::Verified)),
        "the same key, another state"
    );
    assert!(
        models::timeline::safety_swapped(Some(&k1), &k2),
        "another key"
    );
    assert!(
        !models::timeline::safety_swapped(Some(&k1), &view("aci:bob", 2, VerifyState::Default)),
        "another person"
    );
    let none = safety_view(
        AccountId(1),
        UserId::parse("aci:ada").expect("id"),
        "Ada",
        &Err(SafetyUnavailable::NoKey),
        false,
    );
    assert!(
        !models::timeline::safety_swapped(Some(&k1), &none),
        "no number now"
    );
    assert!(
        !models::timeline::safety_swapped(Some(&none), &k2),
        "no number before"
    );
    // V3: the level holds the last number it SHOWED through an answer without one, so K1, then
    // "not running", then K2 is a swap; the next level starts over.
    let mut seen = models::timeline::SafetySeen::default();
    assert!(!seen.observe(&k1), "a first answer");
    assert!(!seen.observe(&none), "no number is no swap");
    assert!(seen.observe(&k2), "K1, no number, K2: the number swapped");
    assert!(!seen.observe(&k2), "K2 again");
    assert!(!seen.observe(&none) && !seen.observe(&k2), "K2, none, K2");
    seen.forget();
    assert!(!seen.observe(&k1), "a new level starts over");
    let flag = |swapped: bool| {
        models::timeline::safety_variant(&k2, swapped)
            .value::<QMap<QMapPair_QString_QVariant>>()
            .expect("a map")
            .get(&QString::from("swapped"))
            .and_then(|v| v.value::<bool>())
            .expect("the flag")
    };
    assert!(flag(true) && !flag(false), "the map carries the flag");
}

/// Y4: the level holds a view and takes an answer only while it is open. A view the source hands over AFTER the
/// level was closed (taken in the same drain that closed it, the source's own close still queued) is dropped: no map
/// is shown, nothing is held, and no button acts (`target` is the lookup `App::safety_action` makes).
fn the_safety_level_takes_answers_only_while_it_is_open() {
    use cxx_qt_lib::{QMap, QMapPair_QString_QVariant};
    use kourier_model::{IdentityFingerprint, Redacted, SafetyNumber, VerifyState};
    use kourier_viewmodel::safety::{SafetyAction, SafetyView, safety_view};
    let view = |key: u8, state: VerifyState| -> SafetyView {
        safety_view(
            AccountId(1),
            UserId::parse("aci:ada").expect("id"),
            "Ada",
            &Ok(SafetyNumber {
                digits: Redacted::new("1".repeat(60)),
                scannable: Redacted::new(vec![8, 2, key]),
                key: IdentityFingerprint(vec![key; 33]),
                state,
            }),
            false,
        )
    };
    let swapped = |map: &cxx_qt_lib::QVariant| {
        map.value::<QMap<QMapPair_QString_QVariant>>()
            .expect("a map")
            .get(&QString::from("swapped"))
            .and_then(|v| v.value::<bool>())
            .expect("the flag")
    };
    let k1 = view(1, VerifyState::Default);
    let k2 = view(2, VerifyState::Default);
    let mut level = models::timeline::SafetyLevel::default();
    // No level was asked for: an answer is dropped and nothing acts.
    assert!(level.receive(k1.clone()).is_none(), "no level, no answer");
    assert!(level.target(SafetyAction::Mark).is_none());
    // A level is asked for: its answer is taken, held, and a button of it acts on that number alone.
    level.begin();
    let map = level
        .receive(k1.clone())
        .expect("an open level takes its answer");
    assert!(!swapped(&map), "a first answer is no swap");
    let (account, user, key) = level.target(SafetyAction::Mark).expect("the held view");
    assert!(
        account == AccountId(1) && user.as_str() == "aci:ada" && Some(key) == k1.key,
        "a button acts on the person and the key on screen"
    );
    assert!(
        level.target(SafetyAction::Clear).is_none()
            && level.target(SafetyAction::Confirm).is_none(),
        "a button the number does not offer acts on nothing"
    );
    // The level closes, and a view of the source (queued before the close) comes right after: dropped.
    level.end();
    assert!(
        level.target(SafetyAction::Mark).is_none(),
        "no level, no held view"
    );
    assert!(
        level.receive(k2.clone()).is_none(),
        "a view that comes after the close is dropped"
    );
    assert!(
        level.target(SafetyAction::Mark).is_none(),
        "and nothing is held for a button to act on"
    );
    // The next level starts over: taken again, and the number it saw before is forgotten (no swap).
    level.begin();
    let map = level.receive(k2).expect("the next level takes its answer");
    assert!(!swapped(&map), "the next level starts over");
    assert!(level.target(SafetyAction::Mark).is_some());
}

/// A10d D2: what a chat's menu offers reaches QML as a map with every key, each from its own
/// field. One offer per flag (only that flag set) tells a key taken from another field (`on` from
/// `shown`) apart: a key must be true for its own offer and for no other. The timeline model holds
/// the same map as `chatOffer`, an empty one while no chat is open.
fn the_offer_map_carries_every_key() {
    use cxx_qt_lib::{QMap, QMapPair_QString_QVariant};
    use kourier_viewmodel::ChatOffer;
    type Offer = QMap<QMapPair_QString_QVariant>;
    let read = |v: &QVariant| v.value::<Offer>().expect("the offer is a map");
    let key = |m: &Offer, name: &str| {
        m.get(&QString::from(name))
            .unwrap_or_else(|| panic!("the offer lacks the key {name}"))
    };
    type Flag = (&'static str, fn(&mut ChatOffer));
    let flags: [Flag; 9] = [
        ("shown", |o| o.shown = true),
        ("on", |o| o.on = true),
        ("archived", |o| o.archived = true),
        ("pinned", |o| o.pinned = true),
        ("pinAtLimit", |o| o.pin_at_limit = true),
        ("markedUnread", |o| o.marked_unread = true),
        ("hasUnread", |o| o.has_unread = true),
        ("muteOffered", |o| o.mute_offered = true),
        ("muted", |o| o.muted = true),
    ];
    for (own, set) in flags {
        let mut offer = ChatOffer::default();
        set(&mut offer);
        let map = read(&models::timeline::offer_variant(&offer));
        for (name, _) in flags {
            assert_eq!(
                key(&map, name).value::<bool>(),
                Some(name == own),
                "the key {name} follows its own flag only (offer with {own} set)"
            );
        }
    }
    let words = ChatOffer {
        pin_why: "pin-why".into(),
        muted_label: "muted-label".into(),
        why: "why".into(),
        ..ChatOffer::default()
    };
    let map = read(&models::timeline::offer_variant(&words));
    for (name, text) in [
        ("pinWhy", "pin-why"),
        ("mutedLabel", "muted-label"),
        ("why", "why"),
    ] {
        assert_eq!(
            string(key(&map, name)),
            text,
            "the key {name} carries its own text"
        );
    }

    // The timeline model: an empty map (never an invalid value) before and while no chat is
    // open, the offer of the open chat, and the empty map again when it closes.
    let src = FixtureSource::new().expect("fixture");
    let conv = src.fixture().long_chat;
    let mut header = src.chat(conv).expect("the fixture's chat has a header");
    header.offer = ChatOffer {
        shown: true,
        on: false,
        why: "greyed".into(),
        ..ChatOffer::default()
    };
    let mut model = qobject::new_timeline_model();
    let mut m: Pin<&mut TimelineModel> = model.pin_mut();
    assert!(
        !m.chat_offer().is_valid(),
        "the property starts unset, so the first header must set it"
    );
    m.as_mut().set_header(0, None);
    let closed = read(m.chat_offer());
    assert!(
        closed.get(&QString::from("shown")).is_none(),
        "no chat open: the empty map says nothing is shown"
    );
    m.as_mut().set_header(conv.0, Some(&header));
    let open = read(m.chat_offer());
    assert_eq!(key(&open, "shown").value::<bool>(), Some(true));
    assert_eq!(
        key(&open, "on").value::<bool>(),
        Some(false),
        "a greyed chat is shown and not on"
    );
    assert_eq!(string(key(&open, "why")), "greyed");
    header.offer.on = true;
    m.as_mut().set_header(conv.0, Some(&header));
    assert_eq!(
        key(&read(m.chat_offer()), "on").value::<bool>(),
        Some(true),
        "a changed offer is shown"
    );
    m.as_mut().set_header(0, None);
    assert!(
        read(m.chat_offer()).get(&QString::from("shown")).is_none(),
        "the chat closed: the empty map again"
    );
}

/// A10d D2: `accountActions` names the sync switch's state by a key, `""` for an account whose
/// network states no switch (the row is not shown), `off` and `on`. The real row's kill is
/// `settings_ui`'s `roaming` (D5).
fn the_switch_key_says_the_state() {
    use kourier_qt::app::roaming_key;
    assert_eq!(
        roaming_key(None),
        "",
        "a network that writes no settings shows no switch row"
    );
    assert_eq!(roaming_key(Some(false)), "off", "a switch that is off");
    assert_eq!(roaming_key(Some(true)), "on", "a switch that is on");
}

/// A10d D2: the "Until…" dialog's fields become a local time only when they are a date the
/// pickers can give; anything else is refused before a time is computed (a year beyond 9999 would
/// overflow the millisecond count).
fn an_until_time_that_is_no_date_is_refused() {
    use kourier_qt::app::local_pick;
    let pick = local_pick(2026, 11, 2, 8, 30).expect("a date and a time");
    assert_eq!(
        (pick.year, pick.month, pick.day, pick.hour, pick.minute),
        (2026, 11, 2, 8, 30),
        "the fields arrive as picked"
    );
    for (year, month, day, hour, minute) in [
        (1969, 11, 2, 8, 0),
        (10_000, 11, 2, 8, 0),
        (i32::MAX, 11, 2, 8, 0),
        (2026, 0, 2, 8, 0),
        (2026, 13, 2, 8, 0),
        (2026, 11, 0, 8, 0),
        (2026, 11, 32, 8, 0),
        (2026, 11, 2, -1, 0),
        (2026, 11, 2, 24, 0),
        (2026, 11, 2, 8, -1),
        (2026, 11, 2, 8, 60),
    ] {
        assert!(
            local_pick(year, month, day, hour, minute).is_none(),
            "a time outside the calendar is refused ({year}-{month}-{day} {hour}:{minute})"
        );
    }
}

/// The selection mode (Export Selected, SEL): the flag is the `selected` role, a toggle changes that
/// role of that row alone, a reset of the rows keeps the flags, a row that went inside the window
/// leaves the count, and another chat ends the mode.
fn the_selection_lives_in_the_model() {
    use kourier_viewmodel::MessageRow;
    let src = FixtureSource::new().expect("fixture");
    let conv = src.fixture().long_chat;
    let rows = src.timeline(conv);
    let ids: Vec<MessageId> = rows.iter().filter_map(MessageRow::message_id).collect();
    assert!(ids.len() > 6, "the fixture chat has messages");
    let selected = |m: &TimelineModel, id: MessageId| -> bool {
        let at = m.row_of(id).expect("the row is there");
        timeline_data(m, at, "selected")
            .value::<bool>()
            .expect("a bool")
    };
    let mut model = qobject::new_timeline_model();
    let mut m: Pin<&mut TimelineModel> = model.pin_mut();
    m.as_mut().set_header(conv.0, None);
    let mut shown = rows.clone();
    m.as_mut().observe_selection(Some(conv), &mut shown);
    assert!(models::set_rows(m.as_mut(), 1, shown));
    assert!(!*m.selecting());
    assert!(ids.iter().all(|id| !selected(&m, *id)));

    // Start, toggle, and the flag of each row.
    let (first, middle) = (ids[0], ids[ids.len() / 2]);
    m.as_mut().select_start(first.0);
    assert!(*m.selecting());
    assert_eq!(*m.selected_count(), 1);
    assert!(selected(&m, first) && !selected(&m, middle));
    m.as_mut().select_toggle(middle.0);
    assert_eq!(*m.selected_count(), 2);
    assert!(selected(&m, middle));
    // A date header is no row to select.
    let header = m
        .rows()
        .iter()
        .find(|r| r.message_id().is_none())
        .expect("a date header");
    assert!(!header.selected);

    // A new row list of the same chat that is a reset (every row in the other order): the flags stay.
    let mut reversed = rows.clone();
    reversed.reverse();
    m.as_mut().observe_selection(Some(conv), &mut reversed);
    assert!(models::set_rows(m.as_mut(), 2, reversed));
    assert_eq!(*m.selected_count(), 2);
    assert!(selected(&m, first) && selected(&m, middle));

    // Shift+click extends from the anchor over the loaded order.
    m.as_mut().select_extend(ids[3].0);
    assert!(*m.selected_count() >= 3);

    // A row that was deleted inside the window leaves the count; one at the edge would stay.
    let mut without: Vec<MessageRow> = rows
        .iter()
        .filter(|r| r.message_id() != Some(middle))
        .cloned()
        .collect();
    let before = *m.selected_count();
    m.as_mut().observe_selection(Some(conv), &mut without);
    assert!(models::set_rows(m.as_mut(), 3, without));
    assert_eq!(*m.selected_count(), before - 1);
    assert!(*m.selecting());

    // Another chat: the mode ends, no flag is left.
    let mut other = rows.clone();
    m.as_mut()
        .observe_selection(Some(ConversationId(conv.0 + 1)), &mut other);
    assert!(models::set_rows(m.as_mut(), 4, other));
    assert!(!*m.selecting());
    assert_eq!(*m.selected_count(), 0);
    assert!(m.rows().iter().all(|r| !r.selected));
}

//! Qt model tests: row counts, roles and `ListOp` application on the real cxx-qt models, and
//! rich-text positions as Qt's document sees them. Runs offscreen on the main thread
//! (`harness = false`): `QT_QPA_PLATFORM=offscreen cargo test -p kourier-qt`.

use std::pin::Pin;

use cxx_qt_lib::{QGuiApplication, QModelIndex, QString, QVariant};
use kourier_model::{AccountId, RangeKind, Text, TextRange};
use kourier_qt::bridge::qobject::{self, ConversationListModel, TimelineModel};
use kourier_qt::models::{self, ListModel};
use kourier_viewmodel::timeline::Block;
use kourier_viewmodel::{ConversationFilter, FixtureSource, NameMap, UiSource, readmore, richtext};

type Check = (&'static str, fn());

fn main() {
    if std::env::var_os("QT_QPA_PLATFORM").is_none() {
        // SAFETY: single-threaded test binary, before Qt starts.
        unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    }
    cxx_qt::init_crate!(kourier_qt);
    let _app = QGuiApplication::new();
    let checks: [Check; 6] = [
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
        (
            "rich text positions match Qt's document",
            rich_text_positions,
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
        account: Some(AccountId(2)),
        query: String::new(),
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

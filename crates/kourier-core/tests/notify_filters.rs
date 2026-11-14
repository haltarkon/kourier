//! RegExp filters, step R4 (architecture §8.7): what ingest hands the notification path for the
//! filters. A muted chat's live message is a candidate with `muted: true` (the supervisor drops
//! it while no filter watches the muted chats, `Shared::notify_muted_watch`, tested end to end
//! in `kourier-app`'s desktop tests); a muted group's mention stays an ordinary candidate; a
//! blocked sender and an edit never are one; a caught-up message is recorded by id for the
//! summary's filter matching.

use kourier_backend_api::BackendEvent;
use kourier_core::testing::{Announced, IngestHarness};
use kourier_fake::scripts::{remote, signal_features, target};
use kourier_model::{Body, ConvKey, ConversationPatch, MsgIdScope, NetworkKind, Patch, UserId};

const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";
const BOB: &str = "aci:00000000-0000-4000-8000-000000000b0b";
const T0: i64 = 1_700_000_000_000;

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn harness(dir: &tempfile::TempDir) -> IngestHarness {
    IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([9; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness")
}

fn group() -> ConvKey {
    ConvKey::Group("gv2:ZmlsdGVyLWdyb3Vw".into())
}

fn upsert(key: &ConvKey, group: bool, muted: bool) -> BackendEvent {
    let mut p = ConversationPatch::new(key.clone());
    p.features = Some(signal_features(group));
    if muted {
        p.muted_until = Patch::Set(i64::MAX);
    }
    BackendEvent::ConversationUpsert(p)
}

fn say(key: &ConvKey, author: &str, n: i64, mention: bool) -> BackendEvent {
    let mut m = remote(key, &uid(author), &n.to_string(), T0 + n * 1000, "text");
    m.mentions_me = mention;
    BackendEvent::Message(m)
}

fn announce(h: &mut IngestHarness, events: Vec<BackendEvent>) -> Announced {
    h.apply_announcing(events).expect("apply")
}

#[test]
fn a_muted_chat_is_a_muted_candidate_and_a_muted_group_mention_is_not() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let direct = ConvKey::Direct(uid(ALICE));
    announce(
        &mut h,
        vec![upsert(&direct, false, false), upsert(&group(), true, false)],
    );

    // Not muted: an ordinary candidate.
    let said = announce(&mut h, vec![say(&direct, ALICE, 1, false)]);
    assert_eq!((said.notify.len(), said.muted.len()), (1, 0));

    // Muted 1:1: still a candidate, flagged.
    announce(&mut h, vec![upsert(&direct, false, true)]);
    let said = announce(&mut h, vec![say(&direct, ALICE, 2, false)]);
    assert_eq!((said.notify.len(), said.muted.len()), (0, 1));

    // Muted group: a message is flagged, a mention stays ordinary.
    announce(&mut h, vec![upsert(&group(), true, true)]);
    let said = announce(&mut h, vec![say(&group(), BOB, 3, false)]);
    assert_eq!((said.notify.len(), said.muted.len()), (0, 1));
    let said = announce(&mut h, vec![say(&group(), BOB, 4, true)]);
    assert_eq!(
        (said.notify.len(), said.muted.len()),
        (1, 0),
        "a mention in a muted group is as it was"
    );
}

#[test]
fn a_blocked_sender_and_an_edit_are_no_candidate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let direct = ConvKey::Direct(uid(ALICE));
    let blocked = ConvKey::Direct(uid(BOB));
    let mut block = ConversationPatch::new(blocked.clone());
    block.features = Some(signal_features(false));
    block.blocked = Some(true);
    announce(
        &mut h,
        vec![
            upsert(&direct, false, true),
            BackendEvent::ConversationUpsert(block),
        ],
    );
    for n in 1..3 {
        let said = announce(&mut h, vec![say(&blocked, BOB, n, false)]);
        assert!(said.notify.is_empty() && said.muted.is_empty(), "blocked");
    }
    // An edit of a muted chat's message makes no candidate.
    announce(&mut h, vec![say(&direct, ALICE, 10, false)]);
    let said = announce(
        &mut h,
        vec![BackendEvent::Edited {
            target: target(Some(&direct), Some(&uid(ALICE)), "10"),
            by: Some(uid(ALICE)),
            new_local: Some("11".to_owned().into()),
            body: Body::text("edited"),
            edited_at: T0 + 11_000,
            keeps_quote: None,
        }],
    );
    assert!(said.notify.is_empty(), "an edit never notifies");
}

#[test]
fn a_caught_up_message_is_recorded_by_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let direct = ConvKey::Direct(uid(ALICE));
    announce(&mut h, vec![upsert(&direct, false, false)]);
    let mut caught = remote(&direct, &uid(ALICE), "5", T0 + 5000, "while away");
    caught.catch_up = true;
    let said = announce(&mut h, vec![BackendEvent::Message(caught)]);
    assert!(
        said.notify.is_empty(),
        "a catch-up message never notifies alone"
    );
    assert_eq!(said.catch_up, 1);
    assert_eq!(said.catch_up_ids.len(), 1);
    // A live message is no catch-up id.
    let said = announce(&mut h, vec![say(&direct, ALICE, 6, false)]);
    assert!(said.catch_up_ids.is_empty());
}

/// R4 item 1: the catch-up summary's read never returns a row of a protected chat (the rule is
/// in the SQL, `core.md`; the desktop policy checks again).
#[test]
fn a_protected_chats_caught_up_row_is_not_returned() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let direct = ConvKey::Direct(uid(ALICE));
    announce(&mut h, vec![upsert(&direct, false, false)]);
    let mut caught = remote(&direct, &uid(ALICE), "5", T0 + 5000, "while away");
    caught.catch_up = true;
    let said = announce(&mut h, vec![BackendEvent::Message(caught)]);
    let ids = said.catch_up_ids.clone();
    assert_eq!(ids.len(), 1);
    assert_eq!(h.caught_up_messages(&ids).expect("read").len(), 1);

    let mut p = ConversationPatch::new(direct.clone());
    p.protected = Some(true);
    announce(&mut h, vec![BackendEvent::ConversationUpsert(p)]);
    assert!(
        h.caught_up_messages(&ids).expect("read").is_empty(),
        "a protected chat's row stays in the chat"
    );
}

/// R4 item 2: the account's notifications switch off (`local_prefs`, written raw) announces
/// nothing, filters or not: an unmuted chat and a muted one (the muted-chat candidate too).
#[test]
fn the_notifications_switch_off_silences_every_candidate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let direct = ConvKey::Direct(uid(ALICE));
    announce(&mut h, vec![upsert(&direct, false, false)]);
    let said = announce(&mut h, vec![say(&direct, ALICE, 1, false)]);
    assert_eq!(said.notify.len(), 1, "on: a candidate");

    h.connection()
        .execute(
            "UPDATE accounts SET local_prefs = '{\"notifications\":false}'",
            [],
        )
        .expect("prefs");
    let said = announce(&mut h, vec![say(&direct, ALICE, 2, false)]);
    assert!(
        said.notify.is_empty() && said.muted.is_empty(),
        "off: unmuted"
    );
    announce(&mut h, vec![upsert(&direct, false, true)]);
    let said = announce(&mut h, vec![say(&direct, ALICE, 3, false)]);
    assert!(
        said.notify.is_empty() && said.muted.is_empty(),
        "off: muted"
    );
}

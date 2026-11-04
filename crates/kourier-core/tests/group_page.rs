//! People pages, step P2 (architecture §3.4, §4.2): what the core keeps of a group for its page,
//! the description (core.db 19), and what `Query::Group` answers: the group's facts and every
//! member row with the core's name for it.

use kourier_backend_api::BackendEvent;
use kourier_core::testing::IngestHarness;
use kourier_db::rusqlite::{Connection, OptionalExtension};
use kourier_model::{
    ContactPatch, ConvKey, ConversationFeatures, ConversationId, ConversationPatch, GroupInfo,
    Member, MemberList, MemberRole, MsgIdScope, NetworkKind, Patch, UserId,
};

const ME: &str = "aci:00000000-0000-4000-8000-00000000000a";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";
const BOB: &str = "aci:00000000-0000-4000-8000-0000000000b0";
const CLEO: &str = "aci:00000000-0000-4000-8000-0000000c1e00";
const DAN: &str = "aci:00000000-0000-4000-8000-000000000da0";
const EVE: &str = "aci:00000000-0000-4000-8000-0000000000e0";

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn harness(dir: &tempfile::TempDir) -> IngestHarness {
    IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([4; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[UserId::parse(ME).expect("user id")],
    )
    .expect("harness")
}

fn description(c: &Connection, key: &ConvKey) -> Option<Option<String>> {
    c.query_row(
        "SELECT description FROM conversations WHERE conv_key = ?1",
        [key.encode()],
        |r| r.get(0),
    )
    .optional()
    .expect("row")
}

fn upsert(h: &mut IngestHarness, p: ConversationPatch) {
    h.apply(vec![BackendEvent::ConversationUpsert(p)])
        .expect("apply");
}

/// A `ConversationUpsert` with a description lands with its line breaks; a later patch that says
/// nothing about it keeps it; `Clear` clears it; a new text replaces it. A group that never had
/// one has NULL.
#[test]
fn a_description_lands_is_kept_by_a_silent_patch_and_cleared() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let key = ConvKey::Group("gv2:abc".into());

    let mut named = ConversationPatch::new(key.clone());
    named.title = Patch::Set("Hiking".into());
    upsert(&mut h, named);
    assert_eq!(
        description(h.connection(), &key),
        Some(None),
        "a group that never had one"
    );

    let mut set = ConversationPatch::new(key.clone());
    set.description = Patch::Set("Meet at six.\nBring water.".into());
    upsert(&mut h, set);
    assert_eq!(
        description(h.connection(), &key),
        Some(Some("Meet at six.\nBring water.".into())),
        "stored whole, with its line break"
    );

    let mut silent = ConversationPatch::new(key.clone());
    silent.title = Patch::Set("Hiking club".into());
    upsert(&mut h, silent);
    assert_eq!(
        description(h.connection(), &key),
        Some(Some("Meet at six.\nBring water.".into())),
        "a patch that says nothing about it keeps it"
    );

    let mut clear = ConversationPatch::new(key.clone());
    clear.description = Patch::Clear;
    upsert(&mut h, clear);
    assert_eq!(description(h.connection(), &key), Some(None));

    let mut again = ConversationPatch::new(key.clone());
    again.description = Patch::Set("New text".into());
    upsert(&mut h, again);
    assert_eq!(
        description(h.connection(), &key),
        Some(Some("New text".into()))
    );
}

// ---- the group's page (Query::Group) -----------------------------------------------------------

fn group_id(h: &IngestHarness, key: &ConvKey) -> ConversationId {
    h.connection()
        .query_row(
            "SELECT id FROM conversations WHERE conv_key = ?1",
            [key.encode()],
            |r| r.get::<_, i64>(0),
        )
        .map(ConversationId)
        .expect("conversation row")
}

fn contact(h: &mut IngestHarness, user: &str, f: impl FnOnce(&mut ContactPatch)) {
    let mut p = ContactPatch::new(uid(user));
    f(&mut p);
    h.apply(vec![BackendEvent::ContactUpsert(p)])
        .expect("contact");
}

/// `Query::Group` answers the group's own facts (name, description whole, timer, features) and
/// EVERY member row: full members, admins, people invited and people who asked to join, with the
/// core's name for each (nickname, then address book, then profile; none without a contact row)
/// and the picture, and says which row is the user's own. Another account's contact of the same
/// id never names a member.
#[test]
fn a_group_page_comes_with_its_facts_and_names_every_member_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    contact(&mut h, ALICE, |p| {
        p.nickname = Patch::Set("Ally".into());
        p.system_name = Patch::Set("Alice Book".into());
        p.profile_name = Patch::Set("Alice Profile".into());
    });
    contact(&mut h, BOB, |p| {
        p.system_name = Patch::Set("Bob Book".into());
        p.profile_name = Patch::Set("Bob Profile".into());
    });
    contact(&mut h, DAN, |p| {
        p.profile_name = Patch::Set("Dan Profile".into())
    });
    // Cleo and Eve have no contact row; Bob has a picture.
    h.connection()
        .execute_batch(&format!(
            "INSERT INTO files(id, account_id, path, key, purpose)
               VALUES (7, 1, 'avatars/bob', zeroblob(32), 'avatar');
             UPDATE contacts SET avatar_file = 7 WHERE user_id = '{BOB}';"
        ))
        .expect("avatar");
    // A second account that knows ALICE under another name.
    h.connection()
        .execute_batch(&format!(
            "INSERT INTO accounts(id, uuid, network, state, created_at, own_ids)
               VALUES (2, '6b3f0c1e-1d2a-4c5b-9e8f-0a1b2c3d4e5f', 'fake', 'Active', 0, '[\"{ME}\"]');
             INSERT INTO contacts(account_id, user_id, nickname) VALUES (2, '{CLEO}', 'Foreign Cleo');"
        ))
        .expect("a second account");

    let key = ConvKey::Group("gv2:page".into());
    let mut p = ConversationPatch::new(key.clone());
    p.title = Patch::Set("Hiking".into());
    p.description = Patch::Set("Meet at six.\nBring water.".into());
    p.timer = Patch::Set(std::time::Duration::from_secs(3600));
    p.features = Some(ConversationFeatures {
        group_info: Some(GroupInfo {
            attributes_admin_only: true,
            ..GroupInfo::default()
        }),
        ..ConversationFeatures::default()
    });
    let member = |u: &str, role| Member { user: uid(u), role };
    p.members = Some(MemberList::snapshot(vec![
        member(ME, MemberRole::Member),
        member(ALICE, MemberRole::Admin),
        member(BOB, MemberRole::Member),
        member(CLEO, MemberRole::Member),
        member(DAN, MemberRole::Pending),
        member(EVE, MemberRole::Requesting),
    ]));
    upsert(&mut h, p);
    let id = group_id(&h, &key);

    let g = h.group(id).expect("query").expect("a group");
    assert_eq!((g.account, g.id), (h.account(), id));
    assert_eq!(g.title.as_deref(), Some("Hiking"));
    assert_eq!(
        g.description.as_deref(),
        Some("Meet at six.\nBring water."),
        "whole, with its line break"
    );
    assert_eq!(g.timer, Some(std::time::Duration::from_secs(3600)));
    assert_eq!(
        g.features.group_info.map(|i| i.attributes_admin_only),
        Some(true)
    );
    assert_eq!(g.members.len(), 6, "every row, invited and asking too");
    let row = |u: &str| {
        g.members
            .iter()
            .find(|m| m.user == uid(u))
            .unwrap_or_else(|| panic!("a row for {u}"))
    };
    assert_eq!(row(ALICE).name.as_deref(), Some("Ally"), "nickname wins");
    assert_eq!(row(ALICE).role, MemberRole::Admin);
    assert_eq!(
        row(BOB).name.as_deref(),
        Some("Bob Book"),
        "then the address book"
    );
    assert!(row(BOB).avatar.is_some(), "the picture comes with the row");
    assert!(row(ALICE).avatar.is_none());
    assert_eq!(
        row(DAN).name.as_deref(),
        Some("Dan Profile"),
        "then the profile"
    );
    assert_eq!(row(DAN).role, MemberRole::Pending);
    assert_eq!(
        row(CLEO).name,
        None,
        "no contact row, no name (the front end labels them)"
    );
    assert_eq!(
        row(CLEO).role,
        MemberRole::Member,
        "the other account's contact does not name them"
    );
    assert_eq!(row(EVE).role, MemberRole::Requesting);
    assert!(row(ME).is_self, "our own row");
    assert!(g.members.iter().filter(|m| m.is_self).count() == 1);
    let shown = format!("{g:?}");
    for leak in ["Hiking", "Bring water", "Ally", "Bob Book", "Dan Profile"] {
        assert!(!shown.contains(leak), "{leak} in the Debug form: {shown}");
    }
}

/// Only a conversation of kind `Group` has a group page: a 1:1 chat, a channel, a list and an id
/// the core does not know answer `None` (the header of those is not a button).
#[test]
fn only_a_group_has_a_group_page() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let mut ids = Vec::new();
    for key in [
        ConvKey::Direct(uid(ALICE)),
        ConvKey::Channel("wa:news".into()),
        ConvKey::Broadcast("tm:list".into()),
        ConvKey::Group("gv2:g".into()),
    ] {
        let mut p = ConversationPatch::new(key.clone());
        p.title = Patch::Set("Named".into());
        upsert(&mut h, p);
        ids.push(group_id(&h, &key));
    }
    let answers: Vec<bool> = ids
        .iter()
        .map(|id| h.group(*id).expect("query").is_some())
        .collect();
    assert_eq!(answers, [false, false, false, true]);
    assert!(
        h.group(ConversationId(9_999)).expect("query").is_none(),
        "an unknown id"
    );
    // A group without members is a group still.
    assert!(
        h.group(ids[3])
            .expect("query")
            .expect("group")
            .members
            .is_empty()
    );
}

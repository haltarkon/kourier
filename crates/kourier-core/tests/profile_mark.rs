//! The "~" mark of a self-chosen name (W6a2 B8c, his answer "~Name, as the phone", 2026-10-28):
//! on a WhatsApp account the name a person chose for themself shows with a "~" in front wherever
//! the core names them and the viewer has no address-book name or nickname of their own for the
//! person: the chat's title, the member rows of a group's page, a message's author and the
//! preview's author. The nickname and the address-book name never carry it, the contact row
//! (the contact card's source) keeps the name as the network stated it, and every other network
//! shows its profile names as they are.

use kourier_backend_api::BackendEvent;
use kourier_core::facade::{
    ConversationListQuery, ConversationRow, MessageRow, TimelineAnchor, TimelineQuery,
};
use kourier_core::testing::IngestHarness;
use kourier_fake::scripts::remote;
use kourier_model::{
    ContactPatch, ConvKey, ConversationId, ConversationPatch, Member, MemberList, MemberRole,
    MsgIdScope, NetworkKind, Patch, UserId,
};

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

/// The people of one scene, in the id form of the network under test.
struct Ids {
    me: UserId,
    /// Chose a name (profile only).
    ann: UserId,
    /// Has an address-book name besides the profile name.
    bea: UserId,
    /// Has a nickname besides both.
    cal: UserId,
    /// A profile name that is the empty string.
    dee: UserId,
}

fn whatsapp_ids() -> Ids {
    Ids {
        me: uid("pn:49151555010001"),
        ann: uid("pn:49151555010002"),
        bea: uid("lid:151555010003"),
        cal: uid("pn:49151555010004"),
        dee: uid("pn:49151555010005"),
    }
}

fn signal_ids() -> Ids {
    Ids {
        me: uid("aci:00000000-0000-4000-8000-0000000000a1"),
        ann: uid("aci:00000000-0000-4000-8000-0000000000a2"),
        bea: uid("aci:00000000-0000-4000-8000-0000000000a3"),
        cal: uid("aci:00000000-0000-4000-8000-0000000000a4"),
        dee: uid("aci:00000000-0000-4000-8000-0000000000a5"),
    }
}

fn contact(h: &mut IngestHarness, user: &UserId, f: impl FnOnce(&mut ContactPatch)) {
    let mut p = ContactPatch::new(user.clone());
    f(&mut p);
    h.apply(vec![BackendEvent::ContactUpsert(p)])
        .expect("contact");
}

struct Scene {
    _dir: tempfile::TempDir,
    h: IngestHarness,
    ids: Ids,
    group: ConversationId,
}

/// An account of `kind` with the four people above, a 1:1 chat with each of the first three, and
/// a group of all four in which Ann wrote the newest message.
fn scene(kind: NetworkKind, ids: Ids) -> Scene {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([6; 32]),
        kind,
        MsgIdScope::AccountAuthor,
        std::slice::from_ref(&ids.me),
    )
    .expect("harness");
    contact(&mut h, &ids.ann, |p| {
        p.profile_name = Patch::Set("Ann Chosen".into());
    });
    contact(&mut h, &ids.bea, |p| {
        p.system_name = Patch::Set("Bea Book".into());
        p.profile_name = Patch::Set("Bea Chosen".into());
    });
    contact(&mut h, &ids.cal, |p| {
        p.nickname = Patch::Set("Cal Nick".into());
        p.system_name = Patch::Set("Cal Book".into());
        p.profile_name = Patch::Set("Cal Chosen".into());
    });
    contact(&mut h, &ids.dee, |p| {
        p.profile_name = Patch::Set(String::new());
    });
    let mut events: Vec<BackendEvent> = [&ids.ann, &ids.bea, &ids.cal]
        .into_iter()
        .map(|u| {
            BackendEvent::ConversationUpsert(ConversationPatch::new(ConvKey::Direct(u.clone())))
        })
        .collect();
    let key = ConvKey::Group("gv2:marks".into());
    let mut g = ConversationPatch::new(key.clone());
    g.title = Patch::Set("Marks".into());
    let member = |u: &UserId| Member {
        user: u.clone(),
        role: MemberRole::Member,
    };
    g.members = Some(MemberList::snapshot(vec![
        member(&ids.me),
        member(&ids.ann),
        member(&ids.bea),
        member(&ids.cal),
        member(&ids.dee),
    ]));
    events.push(BackendEvent::ConversationUpsert(g));
    for (i, by) in [&ids.bea, &ids.cal, &ids.ann].into_iter().enumerate() {
        events.push(BackendEvent::Message(remote(
            &key,
            by,
            &format!("m{i}"),
            1_700_000_000_000 + 1_000 * (i as i64 + 1),
            "hello",
        )));
    }
    h.apply(events).expect("events");
    let group = rows(&h)
        .into_iter()
        .find(|r| r.peer.is_none())
        .expect("the group row")
        .id;
    Scene {
        _dir: dir,
        h,
        ids,
        group,
    }
}

fn rows(h: &IngestHarness) -> Vec<ConversationRow> {
    h.conversations(&ConversationListQuery::default())
        .expect("list")
        .rows
}

impl Scene {
    fn chat_title(&self, who: &UserId) -> Option<String> {
        rows(&self.h)
            .into_iter()
            .find(|r| r.peer.as_ref() == Some(who))
            .expect("the chat")
            .title
    }

    fn member_name(&self, who: &UserId) -> Option<String> {
        self.h
            .group(self.group)
            .expect("query")
            .expect("a group")
            .members
            .into_iter()
            .find(|m| &m.user == who)
            .expect("the member's row")
            .name
    }

    fn timeline(&self) -> Vec<MessageRow> {
        self.h
            .timeline(&TimelineQuery {
                conv: self.group,
                anchor: TimelineAnchor::Newest,
                limit: 50,
            })
            .expect("timeline")
            .rows
    }

    fn author_name(&self, who: &UserId) -> Option<String> {
        self.timeline()
            .into_iter()
            .find(|r| r.author.as_ref() == Some(who))
            .expect("the author's message")
            .author_name
            .map(|n| n.expose().clone())
    }

    fn preview_author(&self) -> Option<String> {
        rows(&self.h)
            .into_iter()
            .find(|r| r.id == self.group)
            .expect("the group row")
            .preview
            .expect("a preview")
            .author_name
    }
}

/// On a WhatsApp account the chosen name carries the mark in all four places the core names a
/// person, an address-book name or a nickname wins and carries none, an empty name stays empty
/// (no lone "~"), and the contact row, which the contact card is built from, is as stated.
#[test]
fn a_whatsapp_persons_chosen_name_carries_the_mark_wherever_the_core_names_them() {
    let s = scene(NetworkKind::WhatsApp, whatsapp_ids());
    let i = &s.ids;

    assert_eq!(
        s.chat_title(&i.ann).as_deref(),
        Some("~Ann Chosen"),
        "the chat's title"
    );
    assert_eq!(
        s.chat_title(&i.bea).as_deref(),
        Some("Bea Book"),
        "the address book wins, unmarked"
    );
    assert_eq!(
        s.chat_title(&i.cal).as_deref(),
        Some("Cal Nick"),
        "the nickname wins, unmarked"
    );

    assert_eq!(
        s.member_name(&i.ann).as_deref(),
        Some("~Ann Chosen"),
        "a member row"
    );
    assert_eq!(s.member_name(&i.bea).as_deref(), Some("Bea Book"));
    assert_eq!(s.member_name(&i.cal).as_deref(), Some("Cal Nick"));
    assert_ne!(
        s.member_name(&i.dee).as_deref(),
        Some("~"),
        "an empty name is no lone mark"
    );

    assert_eq!(
        s.author_name(&i.ann).as_deref(),
        Some("~Ann Chosen"),
        "a message's author"
    );
    assert_eq!(s.author_name(&i.bea).as_deref(), Some("Bea Book"));
    assert_eq!(s.author_name(&i.cal).as_deref(), Some("Cal Nick"));

    assert_eq!(
        s.preview_author().as_deref(),
        Some("~Ann Chosen"),
        "the list's preview names the newest message's author (Ann)"
    );

    let p = s.h.person(&i.ann).expect("query").expect("a person");
    assert_eq!(
        p.contact.as_ref().and_then(|c| c.profile_name.as_deref()),
        Some("Ann Chosen"),
        "the contact row keeps the name as the network stated it"
    );
    assert_eq!(
        s.h.person(&i.bea)
            .expect("query")
            .and_then(|p| p.contact)
            .and_then(|c| c.profile_name),
        Some("Bea Chosen".into())
    );
}

/// Every other network shows its profile names as they are: the same scene on a Signal account
/// has no mark anywhere.
#[test]
fn another_networks_chosen_name_carries_no_mark() {
    let s = scene(NetworkKind::Signal, signal_ids());
    let i = &s.ids;
    assert_eq!(s.chat_title(&i.ann).as_deref(), Some("Ann Chosen"));
    assert_eq!(s.member_name(&i.ann).as_deref(), Some("Ann Chosen"));
    assert_eq!(s.author_name(&i.ann).as_deref(), Some("Ann Chosen"));
    assert_eq!(s.preview_author().as_deref(), Some("Ann Chosen"));
    assert_eq!(s.chat_title(&i.bea).as_deref(), Some("Bea Book"));
    assert_eq!(s.member_name(&i.cal).as_deref(), Some("Cal Nick"));
}

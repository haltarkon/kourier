//! People pages, step P1 (architecture §3.3, §4.2): what the core keeps of a person for the
//! page: the about text and the verdict whether the phone number may be shown (core.db 18), and
//! what a merge does with them.

use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_core::facade::{LinkState, PersonDetail};
use kourier_core::testing::IngestHarness;
use kourier_db::rusqlite::{Connection, OptionalExtension};
use kourier_model::{
    AccountId, Contact, ContactId, ContactPatch, ConvKey, ConversationPatch, Identifier,
    IdentifierKind, Member, MemberList, MemberRole, MsgIdScope, NetworkKind, Patch, Redacted,
    TrustLevel, UserId,
};

const ME: &str = "aci:00000000-0000-4000-8000-00000000000a";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";
const ALICE_PNI: &str = "pni:00000000-0000-4000-8000-0000000a11ce";

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn harness(dir: &tempfile::TempDir) -> IngestHarness {
    IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([4; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness")
}

type Row = (Option<String>, Option<String>, Option<i64>);

fn row(c: &Connection, user: &str) -> Option<Row> {
    c.query_row(
        "SELECT about, about_emoji, number_shown FROM contacts WHERE user_id = ?1",
        [user],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
    .expect("row")
}

fn upsert(h: &mut IngestHarness, p: ContactPatch) {
    h.apply(vec![BackendEvent::ContactUpsert(p)])
        .expect("apply");
}

/// A `ContactUpsert` with an about text, its emoji and a number verdict lands; a later patch that
/// says nothing about them keeps them; a stated `false` is a value, not "never stated"; `Clear`
/// clears the text and leaves the verdict (a `Patch` clears text, the verdict only changes).
#[test]
fn an_upsert_with_about_and_number_shown_lands_and_a_silent_patch_keeps_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let mut p = ContactPatch::new(uid(ALICE));
    p.about = Patch::Set("Out fishing".into());
    p.about_emoji = Patch::Set("x".into());
    p.number_shown = Some(true);
    upsert(&mut h, p);
    assert_eq!(
        row(h.connection(), ALICE),
        Some((Some("Out fishing".into()), Some("x".into()), Some(1)))
    );

    let mut name = ContactPatch::new(uid(ALICE));
    name.profile_name = Patch::Set("Alice".into());
    upsert(&mut h, name);
    assert_eq!(
        row(h.connection(), ALICE),
        Some((Some("Out fishing".into()), Some("x".into()), Some(1))),
        "a patch without them keeps them"
    );

    let mut off = ContactPatch::new(uid(ALICE));
    off.number_shown = Some(false);
    upsert(&mut h, off);
    assert_eq!(row(h.connection(), ALICE).map(|r| r.2), Some(Some(0)));

    let mut clear = ContactPatch::new(uid(ALICE));
    clear.about = Patch::Clear;
    clear.about_emoji = Patch::Clear;
    upsert(&mut h, clear);
    assert_eq!(row(h.connection(), ALICE), Some((None, None, Some(0))));

    // A contact nobody said anything about has NULL: not shown.
    upsert(
        &mut h,
        ContactPatch::new(uid("aci:00000000-0000-4000-8000-00000000b0b0")),
    );
    assert_eq!(
        row(h.connection(), "aci:00000000-0000-4000-8000-00000000b0b0"),
        Some((None, None, None))
    );
}

/// The merge of an alias's contact into the canonical person follows the names' rule: the
/// canonical row's own word wins, an empty one is filled from the alias.
#[test]
fn a_merge_fills_what_the_canonical_person_lacks_and_keeps_what_it_has() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let mut alias = ContactPatch::new(uid(ALICE_PNI));
    alias.about = Patch::Set("From the alias".into());
    alias.about_emoji = Patch::Set("a".into());
    alias.number_shown = Some(true);
    upsert(&mut h, alias);
    let mut canon = ContactPatch::new(uid(ALICE));
    canon.about = Patch::Set("Own words".into());
    upsert(&mut h, canon);

    h.apply(vec![BackendEvent::IdentityMerged {
        from: uid(ALICE_PNI),
        into: uid(ALICE),
    }])
    .expect("merge");
    assert_eq!(
        row(h.connection(), ALICE_PNI),
        None,
        "the alias row is gone"
    );
    assert_eq!(
        row(h.connection(), ALICE),
        Some((Some("Own words".into()), Some("a".into()), None)),
        "the canonical about wins and the emoji is filled; the alias's verdict is NOT taken (P1 F2a)"
    );
}

/// P1 F2a: an alias's "number shown" was stated for another service id (a PNI has no profile key,
/// so the backend said "shown"): no merge carries it to the canonical person, neither the one
/// that fills a canonical row nor the one that renames the alias row; the canonical person's own
/// stated word stays.
#[test]
fn no_merge_carries_an_aliass_number_verdict_to_the_canonical_person() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);

    // A rename: the canonical person has no contact row yet.
    let mut alias = ContactPatch::new(uid(ALICE_PNI));
    alias.number_shown = Some(true);
    alias.identifiers = Some(vec![kourier_model::Identifier {
        kind: kourier_model::IdentifierKind::Phone,
        value: "+15550100123".into(),
    }]);
    upsert(&mut h, alias);
    h.apply(vec![BackendEvent::IdentityMerged {
        from: uid(ALICE_PNI),
        into: uid(ALICE),
    }])
    .expect("merge");
    assert_eq!(row(h.connection(), ALICE_PNI), None);
    assert_eq!(
        row(h.connection(), ALICE).map(|r| r.2),
        Some(None),
        "the renamed row's flag is NULL = not shown until the backend states it"
    );

    // A fill: the canonical row exists, with a stated `false`; the alias says `true`.
    let carol = "aci:00000000-0000-4000-8000-00000000ca01";
    let carol_pni = "pni:00000000-0000-4000-8000-00000000ca01";
    let mut canon = ContactPatch::new(uid(carol));
    canon.number_shown = Some(false);
    upsert(&mut h, canon);
    let mut alias = ContactPatch::new(uid(carol_pni));
    alias.number_shown = Some(true);
    upsert(&mut h, alias);
    h.apply(vec![BackendEvent::IdentityMerged {
        from: uid(carol_pni),
        into: uid(carol),
    }])
    .expect("merge");
    assert_eq!(row(h.connection(), carol).map(|r| r.2), Some(Some(0)));
}

// ---- the person's page (Query::Person) --------------------------------------------------------

const BOB: &str = "aci:00000000-0000-4000-8000-0000000000b0";

fn group(
    h: &mut IngestHarness,
    id: &str,
    title: &str,
    members: &[(&str, MemberRole)],
    archived: bool,
) -> kourier_model::ConversationId {
    let key = ConvKey::Group(format!("gv2:{id}").into());
    let mut p = ConversationPatch::new(key.clone());
    p.title = Patch::Set(title.to_owned());
    p.archived = Some(archived);
    p.members = Some(MemberList::snapshot(
        members
            .iter()
            .map(|(u, role)| Member {
                user: uid(u),
                role: *role,
            })
            .collect(),
    ));
    h.apply(vec![BackendEvent::ConversationUpsert(p)])
        .expect("group");
    h.connection()
        .query_row(
            "SELECT id FROM conversations WHERE conv_key = ?1",
            [key.encode()],
            |r| r.get::<_, i64>(0),
        )
        .map(kourier_model::ConversationId)
        .expect("group row")
}

/// The page asks for a person: the right contact and 1:1 chat, an alias resolves to the canonical
/// person, the user's own id gives the own page with the account's name and number, an id the
/// core knows nothing else of still answers (no contact, no chat, no group).
#[test]
fn a_person_comes_with_their_contact_their_chat_and_the_aliases_resolve() {
    use MemberRole::Member as M;
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    h.connection()
        .execute(
            "UPDATE accounts SET display_name = 'My Own Name', number = '+15550100' WHERE id = ?1",
            [h.account().0],
        )
        .expect("account");
    let mut alice = ContactPatch::new(uid(ALICE));
    alice.profile_name = Patch::Set("Alice".into());
    alice.about = Patch::Set("Out fishing".into());
    alice.number_shown = Some(true);
    upsert(&mut h, alice);
    let mut chat = ConversationPatch::new(ConvKey::Direct(uid(ALICE)));
    chat.timer = Patch::Set(Duration::from_secs(3600));
    h.apply(vec![BackendEvent::ConversationUpsert(chat)])
        .expect("chat");
    group(&mut h, "g1", "Fishers", &[(ME, M), (ALICE, M)], false);

    let p = h.person(&uid(ALICE)).expect("query").expect("account");
    assert!(!p.is_self);
    assert_eq!(p.user, uid(ALICE));
    let c = p.contact.as_ref().expect("contact");
    assert_eq!(c.profile_name.as_deref(), Some("Alice"));
    assert_eq!(c.about.as_deref(), Some("Out fishing"));
    assert_eq!(c.number_shown, Some(true));
    let chat = p.chat.as_ref().expect("their chat");
    assert_eq!(chat.timer, Some(Duration::from_secs(3600)));
    assert_eq!(p.groups.len(), 1);
    assert_eq!(p.groups[0].title.as_deref(), Some("Fishers"));
    assert_eq!(
        (p.own_name.as_deref(), p.own_number.as_deref()),
        (None, None)
    );

    // An alias is the canonical person.
    h.apply(vec![BackendEvent::IdentityMerged {
        from: uid(ALICE_PNI),
        into: uid(ALICE),
    }])
    .expect("merge");
    let via = h.person(&uid(ALICE_PNI)).expect("query").expect("account");
    assert_eq!(via.user, uid(ALICE));
    assert_eq!(via.groups.len(), 1);
    assert!(via.contact.is_some());

    // The user's own page: the account's name and number beside the contact row.
    let me = h.person(&uid(ME)).expect("query").expect("account");
    assert!(me.is_self);
    // The account is a member of "Fishers" too, but its own page has no groups in common.
    assert!(me.groups.is_empty(), "the own page lists groups in common");
    assert_eq!(me.own_name.as_deref(), Some("My Own Name"));
    assert_eq!(me.own_number.as_deref(), Some("+15550100"));

    // Nobody the core has heard of.
    let nobody = h.person(&uid(BOB)).expect("query").expect("account");
    assert!(nobody.contact.is_none() && nobody.chat.is_none() && nobody.groups.is_empty());
    assert!(!nobody.is_self);
}

/// The page of a person says whether their 1:1 chat was deleted from the list (the row stays, as
/// the state of a deleted chat): the timer of a member's page must not bring it back.
#[test]
fn the_persons_page_says_when_their_chat_was_deleted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let key = ConvKey::Direct(uid(ALICE));
    h.apply(vec![BackendEvent::ConversationUpsert(
        ConversationPatch::new(key.clone()),
    )])
    .expect("chat");
    let hidden = |h: &IngestHarness| {
        h.person(&uid(ALICE))
            .expect("query")
            .expect("account")
            .chat
            .expect("their chat")
            .hidden
    };
    assert!(!hidden(&h), "a listed chat");
    h.connection()
        .execute(
            "UPDATE conversations SET hidden = 1 WHERE conv_key = ?1",
            [key.encode()],
        )
        .expect("deleted");
    assert!(hidden(&h), "the row stays, marked deleted");
}

/// Groups in common are the groups BOTH are full members of: never one we left, one we are only
/// invited to, one the person is only invited to or asked to join, one deleted, and never another
/// account's; an archived one is listed.
#[test]
fn groups_in_common_leave_out_left_invited_deleted_and_other_accounts_groups() {
    use MemberRole::{Admin, Member as M, Pending, Requesting};
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let both = group(&mut h, "both", "Both", &[(ME, M), (ALICE, M)], false);
    let admins = group(
        &mut h,
        "admins",
        "Admins",
        &[(ME, Admin), (ALICE, Admin)],
        false,
    );
    let archived = group(
        &mut h,
        "arch",
        "Archived",
        &[(ME, M), (ALICE, M), (BOB, M)],
        true,
    );
    group(
        &mut h,
        "invited",
        "She is invited",
        &[(ME, M), (ALICE, Pending)],
        false,
    );
    group(
        &mut h,
        "asked",
        "She asked",
        &[(ME, M), (ALICE, Requesting)],
        false,
    );
    group(&mut h, "left", "We left", &[(ALICE, M), (BOB, M)], false);
    group(
        &mut h,
        "mine-invited",
        "We are invited",
        &[(ME, Pending), (ALICE, M)],
        false,
    );
    let deleted = group(&mut h, "gone", "Deleted", &[(ME, M), (ALICE, M)], false);
    let other = group(
        &mut h,
        "other",
        "Other account",
        &[(ME, M), (ALICE, M)],
        false,
    );
    group(&mut h, "bobs", "Only Bob", &[(ME, M), (BOB, M)], false);
    // P1 F4: removed from it before the backend could send the member list (an imported group
    // whose first fetch answered 403): our member row still says "Member", the features say no.
    group(
        &mut h,
        "kicked",
        "We were removed",
        &[(ME, M), (ALICE, M)],
        false,
    );
    let features = kourier_model::ConversationFeatures {
        cannot_send: Some(kourier_model::CannotSend::NotMember),
        ..Default::default()
    };
    let mut gone_patch = ConversationPatch::new(ConvKey::Group("gv2:kicked".into()));
    gone_patch.features = Some(features);
    h.apply(vec![BackendEvent::ConversationUpsert(gone_patch)])
        .expect("features");
    h.connection()
        .execute(
            "UPDATE conversations SET hidden = 1 WHERE id = ?1",
            [deleted.0],
        )
        .expect("deleted");
    h.connection()
        .execute(
            "INSERT INTO accounts(id, uuid, network, state, created_at, own_ids)
               VALUES (2, '6b3f0c1e-1d2a-4c5b-9e8f-0a1b2c3d4e5f', 'fake', 'Active', 0, ?1)",
            [format!("[\"{ME}\"]")],
        )
        .expect("a second account");
    h.connection()
        .execute(
            "UPDATE conversations SET account_id = 2 WHERE id = ?1",
            [other.0],
        )
        .expect("the group of the other account");

    let ids = |user: &str| -> Vec<kourier_model::ConversationId> {
        h.person(&uid(user))
            .expect("query")
            .expect("account")
            .groups
            .iter()
            .map(|g| g.id)
            .collect()
    };
    assert_eq!(
        ids(ALICE),
        vec![both, admins, archived],
        "and in the order of their rows"
    );
    assert_eq!(
        ids(BOB).len(),
        2,
        "Bob: the archived group and the one only he and we share"
    );
}

/// People pages P1: the page's "fresh profile please" reaches the running backend as
/// `Command::RefreshContact` (the backend decides whether it is due), and a command for an
/// account with no running backend fails quietly (an action failure, never a panic).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refresh_asked_by_the_page_reaches_the_backend() {
    use crate::support::{Env, WAIT, add_linked, eventually};
    use kourier_core::facade::UiCommand;
    use kourier_fake::{Flavor, scripts};

    let script = scripts::all()
        .expect("scripts")
        .into_iter()
        .find(|(f, _)| *f == Flavor::SignalLike)
        .expect("a signal-like script")
        .1;
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("account").uuid;
    env.handle
        .send(UiCommand::RefreshContact {
            account,
            user: uid(ALICE),
        })
        .expect("send");
    let env_ref = &env;
    eventually("the backend heard the refresh", WAIT, || async move {
        env_ref.control.commands(uuid).contains(&"RefreshContact")
    })
    .await;
}

// ---- Copy Link (P5b): the link is made only of a number the page may show ---------------------

pub(crate) fn link_detail(
    user: &str,
    number_shown: Option<bool>,
    number: Option<&str>,
) -> PersonDetail {
    let user = uid(user);
    PersonDetail {
        account: AccountId(1),
        user: user.clone(),
        is_self: false,
        contact: Some(Contact {
            id: ContactId(1),
            account: AccountId(1),
            user,
            profile_name: None,
            system_name: None,
            nickname: None,
            avatar: None,
            trust: TrustLevel::Unverified,
            identity_fingerprint: None,
            blocked: false,
            hidden: false,
            unregistered_at: None,
            identifiers: number
                .map(|n| {
                    vec![Identifier {
                        kind: IdentifierKind::Phone,
                        value: n.to_owned(),
                    }]
                })
                .unwrap_or_default(),
            meta: None,
            about: None,
            about_emoji: None,
            number_shown,
        }),
        own_name: None,
        own_number: None,
        chat: None,
        groups: Vec::new(),
        safety_numbers: true,
        safety_waits: false,
        link: LinkState::Hidden,
        share: kourier_core::facade::ShareState::Hidden,
    }
}

/// The network's formatter as the Signal backend has it (E.164 only); it records each number it
/// was handed, so a test sees that a hidden number never reaches it.
fn signal_like(seen: &std::cell::RefCell<Vec<String>>) -> impl FnOnce(&str) -> Option<String> + '_ {
    move |n: &str| {
        seen.borrow_mut().push(n.to_owned());
        kourier_backend_api::is_e164(n).then(|| format!("https://signal.me/#p/{n}"))
    }
}

fn link_of(d: &PersonDetail, links: bool) -> (LinkState, usize) {
    let seen = std::cell::RefCell::new(Vec::new());
    let state = LinkState::of(d, links, signal_like(&seen));
    let handed = seen.borrow().len();
    (state, handed)
}

const NUMBER: &str = "+15550001111";

/// Critic F20 / call 1: a number the page may not show makes no link and never reaches the
/// network: "does not share" only when the network said so (`Some(false)`), the "Kourier does
/// not know" state when it never said (`None`, a profile not fetched yet) or when no number is
/// known; a number-only id links its own number; the own page and a network without links hide
/// the button.
#[test]
fn a_hidden_number_gives_no_link() {
    let (s, handed) = link_of(&link_detail(ALICE, Some(false), Some(NUMBER)), true);
    assert_eq!(s, LinkState::NotShared, "she does not share her number");
    assert_eq!(
        handed, 0,
        "a number she does not share never reaches the network"
    );
    let (s, handed) = link_of(&link_detail(ALICE, None, Some(NUMBER)), true);
    assert_eq!(
        s,
        LinkState::NoNumber,
        "never said is not shown, and not 'does not share'"
    );
    assert_eq!(
        handed, 0,
        "a number never said to be shown never reaches the network"
    );
    let (s, _) = link_of(&link_detail(ALICE, Some(true), None), true);
    assert_eq!(s, LinkState::NoNumber, "shared, but no number known");
    let (s, _) = link_of(&link_detail(ALICE, Some(true), Some("15550001111")), true);
    assert_eq!(s, LinkState::NoNumber, "not a number the network links");
    let mut own = link_detail(ALICE, Some(true), Some(NUMBER));
    own.is_self = true;
    assert_eq!(
        link_of(&own, true).0,
        LinkState::Hidden,
        "no Copy Link on the own page (BQ-4)"
    );
    assert_eq!(
        link_of(&link_detail(ALICE, Some(true), Some(NUMBER)), false).0,
        LinkState::Hidden,
        "a network without links shows no button"
    );
}

/// The link of a shown number, and of a person known by the number alone (`e164:` and `pn:`).
#[test]
fn a_shown_number_gives_the_link() {
    let want = LinkState::Link(Redacted::new(format!("https://signal.me/#p/{NUMBER}")));
    assert_eq!(
        link_of(&link_detail(ALICE, Some(true), Some(NUMBER)), true).0,
        want
    );
    let mut by_number = link_detail(&format!("e164:{NUMBER}"), None, None);
    assert_eq!(link_of(&by_number, true).0, want, "the id is the number");
    by_number.user = uid("pn:15550001111");
    by_number.contact = None;
    assert_eq!(
        link_of(&by_number, true).0,
        want,
        "a pn: id without its plus"
    );
    assert!(
        !format!("{want:?}").contains("555"),
        "the link's Debug hides the number"
    );
    // Z2: a shown number of 7 digits (Niue, +683 and 4 digits) links too: the page shows it, so
    // the button must not say that Kourier does not know it.
    assert_eq!(
        link_of(&link_detail(ALICE, Some(true), Some("+6834002")), true).0,
        LinkState::Link(Redacted::new("https://signal.me/#p/+6834002".into())),
        "a 7-digit E.164 number"
    );
}

/// The runtime fills `PersonDetail.link` from the account's network (the Signal-like fake has
/// links): a person who shares her number gets the link, one who does not gets `NotShared`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_page_carries_the_link_of_a_shown_number() {
    use crate::support::{Env, WAIT, add_linked, eventually};
    use kourier_core::facade::{Query, QueryData, UiCommand};
    use kourier_fake::{Flavor, Script};

    const BOB: &str = "aci:00000000-0000-4000-8000-000000000b0b";
    let mut ada = ContactPatch::new(uid(ALICE));
    ada.number_shown = Some(true);
    ada.identifiers = Some(vec![Identifier {
        kind: IdentifierKind::Phone,
        value: NUMBER.into(),
    }]);
    let mut bob = ContactPatch::new(uid(BOB));
    bob.number_shown = Some(false);
    bob.identifiers = Some(vec![Identifier {
        kind: IdentifierKind::Phone,
        value: "+15550002222".into(),
    }]);
    let script = Script::new("people-link", vec![uid(ME)])
        .deliver(vec![
            BackendEvent::ContactUpsert(ada),
            BackendEvent::ContactUpsert(bob),
        ])
        .step(kourier_fake::Step::Sleep(Duration::from_secs(3600)));
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let env_ref = &env;
    let link_of = |user: &'static str| async move {
        match env_ref
            .handle
            .query(Query::Person {
                account,
                user: uid(user),
            })
            .await
        {
            Ok(QueryData::Person(Some(d))) if d.contact.is_some() => Some(d.link),
            _ => None,
        }
    };
    eventually("both contacts landed", WAIT, || async move {
        link_of(ALICE).await.is_some() && link_of(BOB).await == Some(LinkState::NotShared)
    })
    .await;
    assert_eq!(
        link_of(ALICE).await,
        Some(LinkState::Link(Redacted::new(format!(
            "https://signal.me/#p/{NUMBER}"
        )))),
        "she shares her number: the link"
    );
}

/// Z5 (P5b review F5): a network without links to people shows no button, however the person's
/// number stands. The account is on a fake PLAYING WhatsApp (no links) and the person shares her
/// number, so the number rule alone would hand it to the network: the runtime must ask
/// `Network::contact_links` and answer `LinkState::Hidden`, not a disabled "Kourier does not
/// know" button (what the answer would be if the runtime took every network for one with links).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_network_without_links_gives_a_page_without_the_button() {
    use crate::support::{Env, WAIT, add_linked_as, eventually};
    use kourier_core::facade::{Query, QueryData, UiCommand};
    use kourier_fake::Script;

    let mut ada = ContactPatch::new(uid(ALICE));
    ada.number_shown = Some(true);
    ada.identifiers = Some(vec![Identifier {
        kind: IdentifierKind::Phone,
        value: NUMBER.into(),
    }]);
    let script = Script::new("people-no-links", vec![uid(ME)])
        .deliver(vec![BackendEvent::ContactUpsert(ada)])
        .step(kourier_fake::Step::Sleep(Duration::from_secs(3600)));
    let mut env = Env::playing(NetworkKind::WhatsApp, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked_as(&mut env, NetworkKind::WhatsApp).await;
    let env_ref = &env;
    // The page of a person whose number the page may show (she shares it).
    let page = || async move {
        match env_ref
            .handle
            .query(Query::Person {
                account,
                user: uid(ALICE),
            })
            .await
        {
            Ok(QueryData::Person(Some(d)))
                if d.contact
                    .as_ref()
                    .is_some_and(|c| c.number_shown == Some(true)) =>
            {
                Some(d.link)
            }
            _ => None,
        }
    };
    eventually("her contact landed", WAIT, || async move {
        page().await.is_some()
    })
    .await;
    assert_eq!(
        page().await,
        Some(LinkState::Hidden),
        "a network without links has no button, even for a shown number"
    );
}

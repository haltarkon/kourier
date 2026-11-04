//! The phone's address book (W6a2, B8b; D8): the entry the phone's `contact` mutation makes, the scope rule (a name is stated
//! only for a person the core knows), the number that rides with it and the removal. Every name and id is synthetic.

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_db::DbKey;
use kourier_model::{
    Body, ContactPatch, ConvKey, ConversationPatch, Identifier, IdentifierKind, Member, MemberList,
    Origin, Patch, UserId,
};
use wacore::store::Device;
use wacore::store::traits::DeviceStore as _;
use waproto::whatsapp as wa;

use super::{
    Change, apply, apply_older, change_of_action, change_of_inline, change_of_removal, clean_name,
    states, with_names,
};
use crate::names::NAME_CAP;
use crate::store::book as rows;
use crate::store::people::{self, NameKind};
use crate::store::{KourierStore, StoreOwner};

fn open() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([16; 32]),
        "wa-book",
    )
    .expect("open");
    (dir, owner)
}

const ANN_PN: &str = "15550481201";
const ANN_LID: &str = "99900481201";
const BOB_PN: &str = "15550481202";
const CAT_LID: &str = "99900481203";

fn user(scheme: &str, value: &str) -> UserId {
    UserId::from_parts(scheme, value).expect("an id")
}

fn jid(raw: &str) -> wacore_binary::Jid {
    raw.parse().expect("a jid")
}

/// The `contact` mutation of `index`: a full name, a first name and the ids it names.
fn action(
    full: Option<&str>,
    first: Option<&str>,
    pn: Option<&str>,
    lid: Option<&str>,
) -> wa::sync_action_value::ContactAction {
    let mut a = wa::sync_action_value::ContactAction::default();
    a.full_name = full.map(str::to_owned);
    a.first_name = first.map(str::to_owned);
    a.pn_jid = pn.map(str::to_owned);
    a.lid_jid = lid.map(str::to_owned);
    a
}

fn put(index: &str, full: &str, pn: Option<&str>, lid: Option<&str>) -> Change {
    change_of_action(&jid(index), &action(Some(full), None, pn, lid)).expect("a change")
}

fn removal(index: &str) -> Change {
    change_of_removal(&jid(index)).expect("a change")
}

async fn run(store: &KourierStore, change: Change) -> Vec<BackendEvent> {
    store
        .write("test: apply", move |tx| apply(tx, &change, 0))
        .await
        .expect("apply")
}

async fn appear(store: &KourierStore, who: UserId) -> Vec<BackendEvent> {
    store
        .write("test: appear", move |tx| states(tx, &[who]))
        .await
        .expect("states")
}

fn contact(event: &BackendEvent) -> &ContactPatch {
    match event {
        BackendEvent::ContactUpsert(patch) => patch,
        _ => panic!("a contact upsert"),
    }
}

fn phone(number: &str) -> Vec<Identifier> {
    vec![Identifier {
        kind: IdentifierKind::Phone,
        value: number.to_owned(),
    }]
}

#[test]
fn a_contact_is_named_by_its_full_name_else_its_first_and_the_forms_it_names_are_kept() {
    let pn_index = format!("{ANN_PN}@s.whatsapp.net");
    let lid_ids = format!("{ANN_LID}@lid");
    // Full name wins; the number is the index; the LID is the action's.
    let Some(Change::Put(entry)) = change_of_action(
        &jid(&pn_index),
        &action(Some("  Ann Example  "), Some("Ann"), None, Some(&lid_ids)),
    ) else {
        panic!("a name");
    };
    let row = entry.row();
    assert_eq!(row.name, "Ann Example", "full name, trimmed");
    assert_eq!(row.contact, user("pn", ANN_PN).as_str());
    assert_eq!(row.pn.as_deref(), Some(user("pn", ANN_PN).as_str()));
    assert_eq!(row.lid.as_deref(), Some(user("lid", ANN_LID).as_str()));

    // The first name when the full one is blank.
    let Some(Change::Put(entry)) = change_of_action(
        &jid(&pn_index),
        &action(Some("   "), Some("Ann"), None, None),
    ) else {
        panic!("a name");
    };
    assert_eq!(entry.row().name, "Ann");

    // A LID index: the LID is the index; the number is the action's `pnJid`.
    let Some(Change::Put(entry)) = change_of_action(
        &jid(&lid_ids),
        &action(Some("Ann"), None, Some(&pn_index), None),
    ) else {
        panic!("a name");
    };
    let row = entry.row();
    assert_eq!(row.contact, user("lid", ANN_LID).as_str());
    assert_eq!(row.lid.as_deref(), Some(user("lid", ANN_LID).as_str()));
    assert_eq!(row.pn.as_deref(), Some(user("pn", ANN_PN).as_str()));

    // A form in the wrong place names nothing: a LID in `pnJid`, a number in `lidJid`.
    let Some(Change::Put(entry)) = change_of_action(
        &jid(&pn_index),
        &action(Some("Ann"), None, Some(&lid_ids), Some(&pn_index)),
    ) else {
        panic!("a name");
    };
    let row = entry.row();
    assert_eq!(
        row.pn.as_deref(),
        Some(user("pn", ANN_PN).as_str()),
        "the index itself"
    );
    assert_eq!(row.lid, None, "a number is no LID");
}

#[test]
fn a_contact_with_no_name_is_a_removal_and_a_dash_is_a_name_here() {
    let index = jid(&format!("{ANN_PN}@s.whatsapp.net"));
    for none in [None, Some(""), Some("  \n")] {
        let change = change_of_action(&index, &action(none, none, None, None)).expect("a change");
        assert!(matches!(change, Change::Remove(_)), "{none:?} names nobody");
    }
    // The placeholder rule is for push names: the user typed this one into his own address book.
    assert_eq!(clean_name("-").as_deref(), Some("-"));
    let long = "\u{e9}".repeat(NAME_CAP + 9);
    assert_eq!(clean_name(&long).expect("a name").chars().count(), NAME_CAP);
    // Something that is no person (a status broadcast) maps to nothing.
    assert!(change_of_removal(&jid("status@broadcast")).is_none());
    assert!(
        change_of_action(
            &jid("status@broadcast"),
            &action(Some("x"), None, None, None)
        )
        .is_none()
    );
}

#[test]
fn an_inline_contact_of_the_history_is_the_same_entry_and_names_one_or_nothing() {
    let mut inline = wa::InlineContact::default();
    inline.pn_jid = Some(format!("{ANN_PN}@s.whatsapp.net"));
    inline.lid_jid = Some(format!("{ANN_LID}@lid"));
    inline.first_name = Some("Ann".to_owned());
    let Some(Change::Put(entry)) = change_of_inline(&inline) else {
        panic!("a name");
    };
    let row = entry.row();
    assert_eq!(row.name, "Ann");
    assert_eq!(
        row.contact,
        user("pn", ANN_PN).as_str(),
        "indexed under the number"
    );
    assert_eq!(row.lid.as_deref(), Some(user("lid", ANN_LID).as_str()));
    // No name, or no id: nothing (a history record never removes).
    inline.first_name = None;
    assert!(change_of_inline(&inline).is_none());
    let mut nameless_id = wa::InlineContact::default();
    nameless_id.full_name = Some("Ann".to_owned());
    assert!(change_of_inline(&nameless_id).is_none());
}

#[tokio::test]
async fn a_person_the_core_does_not_know_gets_no_name_until_he_appears() {
    let (_dir, owner) = open();
    let store = owner.store();
    let pn_index = format!("{ANN_PN}@s.whatsapp.net");
    // The phone names Ann and Bob; the core knows neither.
    assert!(
        run(&store, put(&pn_index, "Ann Example", None, None))
            .await
            .is_empty()
    );
    let bob = format!("{BOB_PN}@s.whatsapp.net");
    assert!(
        run(&store, put(&bob, "Bob Example", None, None))
            .await
            .is_empty()
    );
    let mirror = store
        .read(|conn| rows::of(conn, user("pn", ANN_PN).as_str()))
        .await
        .expect("read");
    assert!(mirror.is_some(), "the mirror holds what the phone said");

    // Ann appears (a chat with her): her name is stated, Bob's is not.
    let events = appear(&store, user("pn", ANN_PN)).await;
    assert_eq!(events.len(), 1);
    let patch = contact(&events[0]);
    assert_eq!(patch.user, user("pn", ANN_PN));
    assert_eq!(patch.system_name, Patch::Set("Ann Example".to_owned()));
    // The same appearance again says nothing (on change only).
    assert!(appear(&store, user("pn", ANN_PN)).await.is_empty());
    owner.close();
}

#[tokio::test]
async fn a_known_person_is_named_at_once_and_a_rename_is_stated_once() {
    let (_dir, owner) = open();
    let store = owner.store();
    // Ann appears first, the address book has nothing for her yet.
    assert!(appear(&store, user("pn", ANN_PN)).await.is_empty());
    let index = format!("{ANN_PN}@s.whatsapp.net");
    let first = run(&store, put(&index, "Ann Example", None, None)).await;
    assert_eq!(first.len(), 1);
    assert_eq!(
        contact(&first[0]).system_name,
        Patch::Set("Ann Example".to_owned())
    );
    // A repeat of the same mutation (a snapshot's replay) states nothing.
    assert!(
        run(&store, put(&index, "Ann Example", None, None))
            .await
            .is_empty()
    );
    let renamed = run(&store, put(&index, "Ann Renamed", None, None)).await;
    assert_eq!(renamed.len(), 1);
    assert_eq!(
        contact(&renamed[0]).system_name,
        Patch::Set("Ann Renamed".to_owned())
    );
    owner.close();
}

#[tokio::test]
async fn the_number_rides_with_the_name_and_a_contact_without_one_states_none() {
    let (_dir, owner) = open();
    let store = owner.store();
    let lid = user("lid", ANN_LID);
    assert!(appear(&store, lid.clone()).await.is_empty());
    // A LID-indexed contact that names the number: the person is stated under the form the core knows, with the number.
    let index = format!("{ANN_LID}@lid");
    let pn = format!("{ANN_PN}@s.whatsapp.net");
    let events = run(&store, put(&index, "Ann Example", Some(&pn), None)).await;
    assert_eq!(events.len(), 1);
    let patch = contact(&events[0]);
    assert_eq!(patch.user, lid);
    assert_eq!(patch.identifiers, Some(phone(&format!("+{ANN_PN}"))));
    assert_eq!(
        patch.number_shown,
        Some(true),
        "the phone shows the number of a contact it saved"
    );

    // A contact with no number states none: nothing about identifiers or `number_shown`.
    let cat = user("lid", CAT_LID);
    assert!(appear(&store, cat.clone()).await.is_empty());
    let events = run(
        &store,
        put(&format!("{CAT_LID}@lid"), "Cat Example", None, None),
    )
    .await;
    let patch = contact(&events[0]);
    assert_eq!(patch.user, cat);
    assert_eq!(patch.identifiers, None);
    assert_eq!(patch.number_shown, None, "never claimed without a number");
    owner.close();
}

#[tokio::test]
async fn a_person_is_found_by_either_form_the_entry_names() {
    let (_dir, owner) = open();
    let store = owner.store();
    // The core knows Ann by her LID; the phone indexes her by her number and names the LID.
    assert!(appear(&store, user("lid", ANN_LID)).await.is_empty());
    let index = format!("{ANN_PN}@s.whatsapp.net");
    let lid = format!("{ANN_LID}@lid");
    let events = run(&store, put(&index, "Ann Example", None, Some(&lid))).await;
    assert_eq!(events.len(), 1, "stated once, for the form the core knows");
    assert_eq!(contact(&events[0]).user, user("lid", ANN_LID));
    // And a person who appears later in the number's form finds the same entry.
    let later = appear(&store, user("pn", ANN_PN)).await;
    assert_eq!(later.len(), 1);
    assert_eq!(contact(&later[0]).user, user("pn", ANN_PN));
    owner.close();
}

#[tokio::test]
async fn a_removal_clears_the_name_and_hides_the_number() {
    let (_dir, owner) = open();
    let store = owner.store();
    let lid = user("lid", ANN_LID);
    assert!(appear(&store, lid.clone()).await.is_empty());
    let index = format!("{ANN_LID}@lid");
    let pn = format!("{ANN_PN}@s.whatsapp.net");
    run(&store, put(&index, "Ann Example", Some(&pn), None)).await;

    let gone = run(&store, removal(&index)).await;
    assert_eq!(gone.len(), 1);
    let patch = contact(&gone[0]);
    assert_eq!(patch.user, lid);
    assert_eq!(patch.system_name, Patch::Clear);
    assert_eq!(patch.number_shown, Some(false));
    assert_eq!(
        patch.identifiers,
        Some(Vec::new()),
        "the number leaves core.db with the name, not only out of sight (PRIV-8)"
    );
    // The mirror and the statements are gone: a second removal says nothing, and a later appearance states nothing.
    assert!(run(&store, removal(&index)).await.is_empty());
    assert!(appear(&store, lid.clone()).await.is_empty());
    // The contact comes back: stated again (the statement was taken back).
    let back = run(&store, put(&index, "Ann Example", Some(&pn), None)).await;
    assert_eq!(back.len(), 1);
    assert_eq!(
        contact(&back[0]).system_name,
        Patch::Set("Ann Example".to_owned())
    );

    // A contact with NO number: the name statement alone must be taken back, or the contact's return says nothing.
    let cat = user("lid", CAT_LID);
    let cat_index = format!("{CAT_LID}@lid");
    assert!(appear(&store, cat.clone()).await.is_empty());
    assert_eq!(
        run(&store, put(&cat_index, "Cat Example", None, None))
            .await
            .len(),
        1
    );
    let gone = run(&store, removal(&cat_index)).await;
    assert_eq!(gone.len(), 1);
    assert_eq!(contact(&gone[0]).system_name, Patch::Clear);
    assert_eq!(
        contact(&gone[0]).number_shown,
        None,
        "no number was stated, none is hidden"
    );
    let back = run(&store, put(&cat_index, "Cat Example", None, None)).await;
    assert_eq!(back.len(), 1, "the name is stated again after its removal");
    owner.close();
}

/// An entry that loses its number states `identifiers` without it beside `number_shown: false` (PRIV-8): the core replaces
/// `identifiers` only when it is `Some`. Mutant: the empty list left out of the arm.
#[tokio::test]
async fn a_number_that_leaves_an_entry_leaves_core_with_it() {
    let (_dir, owner) = open();
    let store = owner.store();
    let lid = user("lid", ANN_LID);
    assert!(appear(&store, lid.clone()).await.is_empty());
    let index = format!("{ANN_LID}@lid");
    let pn = format!("{ANN_PN}@s.whatsapp.net");
    let first = run(&store, put(&index, "Ann Example", Some(&pn), None)).await;
    assert_eq!(first.len(), 1);
    assert_eq!(
        contact(&first[0]).identifiers,
        Some(phone(&format!("+{ANN_PN}")))
    );
    assert_eq!(contact(&first[0]).number_shown, Some(true));

    let second = run(&store, put(&index, "Ann Example", None, None)).await;
    assert_eq!(second.len(), 1);
    let patch = contact(&second[0]);
    assert_eq!(patch.identifiers, Some(Vec::new()));
    assert_eq!(patch.number_shown, Some(false));
    owner.close();
}

/// One person is one entry (PRIV-4). The app state may index a contact by its LID where the history keeps it by its number:
/// (a) a history entry is not taken while ANY row names its number or its LID, so it states nothing and cannot win over the
/// live word; (b) a live word also takes the history rows that name the same forms, in whichever order they came, so the
/// phone's REMOVE clears the name and no older history name comes back. Mutants: (a) the check by the same index only;
/// (b) the history rows left by the live word.
#[tokio::test]
async fn one_person_is_one_entry_whichever_form_and_order_the_phone_said_him_in() {
    let by_lid = format!("{ANN_LID}@lid");
    let by_pn = format!("{ANN_PN}@s.whatsapp.net");
    let older = |name: &str| {
        change_of_inline(&inline(name, Some(&by_pn), Some(&by_lid))).expect("an entry")
    };
    let older_run = |store: &KourierStore, change: Change| {
        let store = store.clone();
        async move {
            store
                .write("test: older", move |tx| apply_older(tx, &change))
                .await
                .expect("apply_older")
        }
    };
    let named_in_book = |store: &KourierStore| {
        let store = store.clone();
        async move {
            store
                .read(|conn| rows::of(conn, &format!("pn:{ANN_PN}")))
                .await
                .expect("of")
                .is_some()
        }
    };

    // (a) The app state first, indexed by the LID; then the history's entry of the same person indexed by his number.
    let (_dir, owner) = open();
    let store = owner.store();
    let lid = user("lid", ANN_LID);
    assert!(appear(&store, lid.clone()).await.is_empty());
    let live = run(&store, put(&by_lid, "Ann Now", Some(&by_pn), None)).await;
    assert_eq!(live.len(), 1);
    assert!(
        older_run(&store, older("Ann Old")).await.is_empty(),
        "an older entry of the same person, by his other form, states nothing"
    );
    let gone = run(&store, removal(&by_lid)).await;
    assert_eq!(gone.len(), 1);
    assert_eq!(contact(&gone[0]).system_name, Patch::Clear);
    assert!(
        !named_in_book(&store).await,
        "no history row came in beside the live one"
    );
    owner.close();

    // (b) The history first (stated: nothing else is known), then the live word by the LID, then the phone's removal.
    let (_dir, owner) = open();
    let store = owner.store();
    assert!(appear(&store, lid.clone()).await.is_empty());
    let first = older_run(&store, older("Ann Old")).await;
    assert_eq!(first.len(), 1);
    assert_eq!(
        contact(&first[0]).system_name,
        Patch::Set("Ann Old".to_owned())
    );
    let live = run(&store, put(&by_lid, "Ann Now", Some(&by_pn), None)).await;
    assert_eq!(live.len(), 1);
    assert_eq!(
        contact(&live[0]).system_name,
        Patch::Set("Ann Now".to_owned())
    );
    let gone = run(&store, removal(&by_lid)).await;
    assert_eq!(gone.len(), 1);
    assert_eq!(
        contact(&gone[0]).system_name,
        Patch::Clear,
        "the older history name does not come back when the phone removes him"
    );
    assert!(!named_in_book(&store).await);
    owner.close();
}

fn inline(name: &str, pn: Option<&str>, lid: Option<&str>) -> wa::InlineContact {
    let mut contact = wa::InlineContact::default();
    contact.full_name = Some(name.to_owned());
    contact.pn_jid = pn.map(str::to_owned);
    contact.lid_jid = lid.map(str::to_owned);
    contact
}

#[tokio::test]
async fn a_removal_of_a_person_the_core_never_heard_of_says_nothing_and_another_entry_keeps_his_name()
 {
    let (_dir, owner) = open();
    let store = owner.store();
    // Nobody is known: a put then a removal changes the mirror only.
    let index = format!("{BOB_PN}@s.whatsapp.net");
    assert!(
        run(&store, put(&index, "Bob Example", None, None))
            .await
            .is_empty()
    );
    assert!(run(&store, removal(&index)).await.is_empty());

    // Two entries name the same person (his number's index and his LID's): the removal of one restates the other.
    let lid = user("lid", ANN_LID);
    assert!(appear(&store, lid.clone()).await.is_empty());
    let by_pn = format!("{ANN_PN}@s.whatsapp.net");
    let by_lid = format!("{ANN_LID}@lid");
    run(&store, put(&by_pn, "Ann By Number", None, Some(&by_lid))).await;
    run(&store, put(&by_lid, "Ann By Lid", None, None)).await;
    let events = run(&store, removal(&by_lid)).await;
    assert_eq!(events.len(), 1);
    let patch = contact(&events[0]);
    assert_eq!(patch.user, lid);
    assert_eq!(
        patch.system_name,
        Patch::Set("Ann By Number".to_owned()),
        "he keeps a name, from the entry that is left"
    );
    owner.close();
}

fn message_by(conv: ConvKey, author: UserId) -> BackendEvent {
    BackendEvent::Message(IncomingMessage::new(
        conv,
        "M0481",
        Origin::Remote {
            author,
            device: None,
        },
        Body::text("hi"),
        1_000,
        1_000,
    ))
}

#[tokio::test]
async fn the_people_of_the_events_become_known_and_are_named_after_the_merges() {
    let (_dir, owner) = open();
    let store = owner.store();
    // The phone named Ann, Bob and Cat; none is known to the core.
    for (index, name) in [
        (format!("{ANN_PN}@s.whatsapp.net"), "Ann Example"),
        (format!("{BOB_PN}@s.whatsapp.net"), "Bob Example"),
        (format!("{CAT_LID}@lid"), "Cat Example"),
    ] {
        run(&store, put(&index, name, None, None)).await;
    }
    let group = ConvKey::Group("wag:120363900000481".into());
    let mut upsert = ConversationPatch::new(group.clone());
    upsert.members = Some(MemberList::snapshot(vec![
        Member {
            user: user("pn", BOB_PN),
            role: Default::default(),
        },
        Member {
            user: user("lid", CAT_LID),
            role: Default::default(),
        },
    ]));
    let merged = BackendEvent::IdentityMerged {
        from: user("pn", BOB_PN),
        into: user("lid", "99900481209"),
    };
    let events = vec![
        merged.clone(),
        BackendEvent::ConversationUpsert(upsert),
        message_by(group, user("pn", ANN_PN)),
    ];
    let named = store
        .write("test: with names", {
            let events = events.clone();
            move |tx| with_names(tx, &events)
        })
        .await
        .expect("with names")
        .expect("names were added");
    // The merge stays first; the three names follow, in the order the people appear; the original events come after.
    assert_eq!(named[0], merged);
    let who: Vec<&UserId> = named[1..4].iter().map(|e| &contact(e).user).collect();
    assert_eq!(
        who,
        [
            &user("pn", BOB_PN),
            &user("lid", CAT_LID),
            &user("pn", ANN_PN)
        ]
    );
    assert_eq!(&named[4..], &events[1..]);

    // All three are known now; a second zone with the same people says nothing.
    let again = store
        .write("test: with names again", move |tx| with_names(tx, &events))
        .await
        .expect("with names");
    assert!(again.is_none(), "on change only");
    let known = store
        .read(|conn| {
            rows::known_of(
                conn,
                &[
                    user("pn", ANN_PN).as_str(),
                    user("pn", BOB_PN).as_str(),
                    user("lid", CAT_LID).as_str(),
                    user("pn", "15550481299").as_str(),
                ],
            )
        })
        .await
        .expect("known");
    assert_eq!(
        known.len(),
        3,
        "the three people of the events, and nobody else"
    );
    owner.close();
}

#[tokio::test]
async fn a_rebuild_forgets_who_the_core_knows_and_keeps_the_phones_address_book() {
    let (_dir, owner) = open();
    let store = owner.store();
    let index = format!("{ANN_PN}@s.whatsapp.net");
    assert!(appear(&store, user("pn", ANN_PN)).await.is_empty());
    run(&store, put(&index, "Ann Example", None, None)).await;
    store
        .write("test: rebuild", people::forget_statements)
        .await
        .expect("forget");
    // The new core knows nobody and was told no name; the mirror still holds the phone's word.
    let (known, mirror, told) = store
        .read(|conn| {
            Ok((
                rows::known_of(conn, &[user("pn", ANN_PN).as_str()])?,
                rows::of(conn, user("pn", ANN_PN).as_str())?,
                people::name_of(conn, user("pn", ANN_PN).as_str(), NameKind::System)?,
            ))
        })
        .await
        .expect("read");
    assert!(known.is_empty());
    assert!(mirror.is_some());
    assert!(told.is_none());
    // A person the new core meets is named from the mirror again.
    let events = appear(&store, user("pn", ANN_PN)).await;
    assert_eq!(events.len(), 1);
    owner.close();
}

const OWN_PN: &str = "15550481701";
const OWN_LID: &str = "99900481701";

/// F13 (PRIV-7): our own ids are nobody's address-book person. He saved his own number as "Me" (under the number's index and
/// under the LID's), and both ids are named in a group's member list: neither becomes a person the core knows, and nothing is
/// stated for either form, however the phone said it; another person is named as always.
/// Mutant for the re-check: the own-id skip in `states` removed.
#[tokio::test]
async fn our_own_ids_are_no_address_book_person() {
    let (_dir, owner) = open();
    let store = owner.store();
    let mut device = Device::new();
    device.pn = Some(format!("{OWN_PN}:3@s.whatsapp.net").parse().expect("jid"));
    device.lid = Some(format!("{OWN_LID}@lid").parse().expect("jid"));
    store.save(&device).await.expect("save");

    let own_pn = format!("{OWN_PN}@s.whatsapp.net");
    let own_lid = format!("{OWN_LID}@lid");
    run(&store, put(&own_pn, "Me", None, Some(&own_lid))).await;
    run(&store, put(&own_lid, "Me Too", Some(&own_pn), None)).await;
    run(
        &store,
        put(
            &format!("{ANN_PN}@s.whatsapp.net"),
            "Ann Example",
            None,
            None,
        ),
    )
    .await;

    // The group's member list names us (both forms, as the group's mode wants one) and Ann.
    assert!(appear(&store, user("pn", OWN_PN)).await.is_empty());
    assert!(appear(&store, user("lid", OWN_LID)).await.is_empty());
    let named = appear(&store, user("pn", ANN_PN)).await;
    assert_eq!(named.len(), 1, "Ann is named as always");
    assert_eq!(contact(&named[0]).user, user("pn", ANN_PN));

    let (known, told) = store
        .read(|conn| {
            Ok((
                rows::known_of(
                    conn,
                    &[
                        user("pn", OWN_PN).as_str(),
                        user("lid", OWN_LID).as_str(),
                        user("pn", ANN_PN).as_str(),
                    ],
                )?,
                people::name_of(conn, user("pn", OWN_PN).as_str(), NameKind::System)?,
            ))
        })
        .await
        .expect("read");
    assert_eq!(
        known,
        [user("pn", ANN_PN).as_str().to_owned()],
        "only Ann is known"
    );
    assert!(told.is_none(), "no name was told for us");
    // A later change of the phone's word about us says nothing either.
    assert!(
        run(&store, put(&own_pn, "Me Again", None, Some(&own_lid)))
            .await
            .is_empty()
    );
    assert!(run(&store, removal(&own_lid)).await.is_empty());
    owner.close();
}

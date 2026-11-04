//! The features of a WhatsApp chat and where they are stated (W5a, D7, D9). Synthetic ids only: the fictitious
//! 555-01xx range, tails of 8 digits that are substrings of no other value.

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_db::DbKey;
use kourier_model::{
    Body, CannotSend, ConvKey, ConversationFeatures, FormatKind, Origin, Support, SystemEvent,
    UserId,
};

use super::{VERSION, is_statement, of, sweep, with_statements};
use crate::send::started::{forget_statements, mark_stated, note_messages};
use crate::store::{KourierStore, StoreOwner, exec};

const PEER: &str = "15550147302958";
const CALLER: &str = "15550183620471";
const OLD: &str = "15550129475036";
const HISTORY: &str = "15550161837092";
const GROUP: &str = "120363000083914562";

fn direct(value: &str) -> ConvKey {
    ConvKey::Direct(UserId::from_parts("pn", value).expect("a user"))
}

fn group() -> ConvKey {
    ConvKey::Group(format!("wag:{GROUP}").into())
}

fn from_peer(conv: &ConvKey, body: Body) -> BackendEvent {
    let author = conv
        .direct_peer()
        .cloned()
        .unwrap_or_else(|| UserId::from_parts("pn", PEER).expect("a user"));
    BackendEvent::Message(IncomingMessage::new(
        conv.clone(),
        "M1",
        Origin::Remote {
            author,
            device: None,
        },
        body,
        1_000,
        1_000,
    ))
}

fn open() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([7; 32]),
        "wa-features",
    )
    .expect("open");
    (dir, owner)
}

/// Notes the started facts of `events` and states them, in one zone, as stage 2 does.
async fn stage(store: &KourierStore, events: Vec<BackendEvent>) -> Option<Vec<BackendEvent>> {
    store
        .write("stage", move |tx| {
            note_messages(tx, &events)?;
            with_statements(tx, &events)
        })
        .await
        .expect("stage")
}

/// The chat and the features of a statement.
fn stated(event: &BackendEvent) -> Option<(&ConvKey, &ConversationFeatures)> {
    match event {
        BackendEvent::ConversationUpsert(patch) if is_statement(event) => {
            Some((&patch.key, patch.features.as_ref()?))
        }
        _ => None,
    }
}

#[test]
fn a_mention_is_a_group_feature_and_the_limits_are_whatsapps() {
    let one = of(&direct(PEER), true);
    let many = of(&group(), true);
    for kind in [
        FormatKind::Bold,
        FormatKind::Italic,
        FormatKind::Strike,
        FormatKind::Mono,
    ] {
        assert_eq!(one.format_support(kind), Support::Full, "{kind:?} in 1:1");
        assert_eq!(
            many.format_support(kind),
            Support::Full,
            "{kind:?} in a group"
        );
    }
    assert!(
        !one.format_support(FormatKind::Mention).is_usable(),
        "a 1:1 chat offers no mention (critic L4)"
    );
    assert_eq!(many.format_support(FormatKind::Mention), Support::Full);
    for kind in [
        FormatKind::Spoiler,
        FormatKind::MentionAll,
        FormatKind::Link,
    ] {
        assert!(!many.format_support(kind).is_usable(), "{kind:?}");
    }
    assert_eq!(one.edit.and_then(|w| w.max_age_secs), Some(900));
    assert_eq!(
        one.delete_for_everyone.and_then(|w| w.max_age_secs),
        Some(216_000)
    );
    // Fix F16: the receivers drop an edit 1,200 s after its original, so the core judges a received edit by that age;
    // a window that is `None` makes the core drop EVERY received edit or delete (`ingest/modifier.rs`).
    assert_eq!(
        one.edit_receive.and_then(|w| w.max_age_secs),
        Some(1200),
        "the receivers' edit window"
    );
    assert_eq!(
        many.edit_receive.and_then(|w| w.max_age_secs),
        Some(1200),
        "in a group too"
    );
    assert!(
        one.delete_receive.is_some_and(|w| w.max_age_secs.is_none()),
        "a received delete is taken at any age, not refused"
    );
    assert!(!one.delete_for_me, "delete for me is W6's");
    assert_eq!(one.reactions, Support::Full);
    assert_eq!(one.reactions_per_user, Some(1));
    assert_eq!(one.reply, Support::Full);
    assert!(one.typing && one.read_receipts && one.delivery_receipts);
    assert_eq!(
        one.files.len(),
        5,
        "files: W5b (`files_are_stated_per_kind_with_version_4`)"
    );
    assert!(
        many.disappearing.is_empty() && !one.disappearing.is_empty(),
        "a direct chat offers timers (W6a2 B7b, `a_direct_chat_offers_the_three_timers...`), a group none"
    );
    assert_eq!(one.max_text_bytes, Some(65_536));
    assert_eq!(one.cannot_send, None);
    assert_eq!(
        of(&direct(PEER), false).cannot_send,
        Some(CannotSend::NotAccepted)
    );
    assert_eq!(
        of(&group(), false).cannot_send,
        Some(CannotSend::NotAccepted)
    );
}

#[tokio::test]
async fn the_backend_states_whatsapps_features_once_per_version() {
    let (_dir, owner) = open();
    let store = owner.store();
    let peer = direct(PEER);
    let caller = direct(CALLER);
    let first = vec![
        // A call line of a stranger: the chat is in the core, the phone never started it (D7).
        from_peer(&caller, Body::System(SystemEvent::Joined)),
        from_peer(&peer, Body::text("hi")),
        from_peer(&peer, Body::text("again")),
        from_peer(&group(), Body::text("in a group")),
    ];
    let out = stage(&store, first.clone()).await.expect("statements");
    let statements: Vec<_> = out.iter().filter_map(stated).collect();
    assert_eq!(
        statements.len(),
        3,
        "one statement per chat, not per message"
    );
    let rest: Vec<_> = out.iter().filter(|e| !is_statement(e)).cloned().collect();
    assert_eq!(
        rest, first,
        "the events themselves are untouched and in order"
    );
    // Each statement comes right before the first event of its chat.
    for (i, event) in out.iter().enumerate() {
        if let Some((conv, _)) = stated(event) {
            assert!(
                matches!(out.get(i + 1), Some(BackendEvent::Message(m)) if &m.conv == conv),
                "a statement before its chat's first event"
            );
        }
    }
    let of_chat = |conv: &ConvKey| {
        statements
            .iter()
            .find(|(c, _)| *c == conv)
            .map(|(_, f)| (*f).clone())
            .expect("stated")
    };
    assert_eq!(
        of_chat(&caller).cannot_send,
        Some(CannotSend::NotAccepted),
        "a call-only chat cannot send"
    );
    assert_eq!(
        of_chat(&peer).cannot_send,
        None,
        "the peer's first message lifts it in its own row"
    );
    assert_eq!(
        of_chat(&group()).format_support(FormatKind::Mention),
        Support::Full
    );

    assert!(
        stage(&store, first).await.is_none(),
        "stated once per version and fact"
    );
    // The caller writes: the fact changed, the chat is stated again, and only that one.
    let out = stage(&store, vec![from_peer(&caller, Body::text("hello"))])
        .await
        .expect("restated");
    let again: Vec<_> = out.iter().filter_map(stated).collect();
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].0, &caller);
    assert_eq!(again[0].1.cannot_send, None);
    // A chat stated by an older build is stated again with its next event.
    let older = peer.clone();
    store
        .write("older", move |tx| {
            mark_stated(tx, &older, VERSION - 1, true)
        })
        .await
        .expect("mark");
    let out = stage(&store, vec![from_peer(&peer, Body::text("more"))])
        .await
        .expect("a new version");
    assert_eq!(out.iter().filter_map(stated).count(), 1);
    owner.close();
}

#[tokio::test]
async fn the_start_sweep_states_the_historys_chats_and_every_chat_again_after_a_rebuild() {
    let (_dir, owner) = open();
    let store = owner.store();
    let old = direct(OLD);
    let history = direct(HISTORY);
    let (o, h) = (old.clone(), history.clone());
    store
        .write("seed", move |tx| {
            mark_stated(tx, &o, VERSION, false)?;
            exec(
                tx,
                "a chat of the history",
                "INSERT INTO history_chats(conv) VALUES (?1)",
                [h.encode()],
            )
            .map(drop)
        })
        .await
        .expect("seed");
    let swept = |store: &KourierStore| {
        let store = store.clone();
        async move { store.write("sweep", sweep).await.expect("sweep").concat() }
    };
    let first = swept(&store).await;
    let chats: Vec<_> = first
        .iter()
        .filter_map(stated)
        .map(|(c, _)| c.clone())
        .collect();
    assert_eq!(
        chats,
        vec![history.clone()],
        "the history's chat was never stated"
    );
    assert!(swept(&store).await.is_empty(), "nothing is due twice");
    store
        .write("rebuild", forget_statements)
        .await
        .expect("forget");
    let mut chats: Vec<_> = swept(&store)
        .await
        .iter()
        .filter_map(stated)
        .map(|(c, _)| c.encode())
        .collect();
    chats.sort();
    let mut want = vec![old.encode(), history.encode()];
    want.sort();
    assert_eq!(
        chats, want,
        "a rebuild states every chat stated before again"
    );
    owner.close();
}

/// D16 (brief L15): the fake that plays WhatsApp (the rehearsal's chats) states the SAME features as this backend
/// for a chat the phone started, direct and group: one source, compared here so a change of either fails.
#[test]
fn fake_and_backend_state_the_same_features() {
    for (conv, group) in [(direct(HISTORY), false), (group(), true)] {
        assert_eq!(
            of(&conv, true),
            kourier_fake::scripts::whatsapp_features(group),
            "the fake playing WhatsApp states other features than the backend (group: {group})"
        );
    }
}

/// W5b (G6, G9, G11): every chat states files of five kinds (pictures, videos, GIFs, audio, any file) at the one
/// `FILE_LIMIT` (a larger media file is adapted to a document by the route, never refused), a caption on every kind
/// but audio (an `audioMessage` carries none), no voice notes and no stickers, one file per message; the version is 4
/// (W6a2: a group's `admin_delete`, `group_info` and `cannot_send`) so every chat stated before is stated again.
/// Mutant: audio stated with a caption.
#[test]
fn files_are_stated_per_kind_with_version_4() {
    use kourier_model::{AttachmentKind, FileLimits};

    assert_eq!(VERSION, 4);
    for (conv, group) in [(direct(HISTORY), false), (group(), true)] {
        let features = of(&conv, true);
        let stated: Vec<(AttachmentKind, FileLimits)> =
            features.files.iter().map(|(k, l)| (*k, *l)).collect();
        let limits = |caption| FileLimits {
            max_size: 2_000_000_000,
            caption,
        };
        assert_eq!(
            stated,
            vec![
                (AttachmentKind::Image, limits(true)),
                (AttachmentKind::Video, limits(true)),
                (AttachmentKind::Audio, limits(false)),
                (AttachmentKind::Gif, limits(true)),
                (AttachmentKind::File, limits(true)),
            ],
            "group: {group}"
        );
        assert!(!features.files.contains_key(&AttachmentKind::Voice));
        assert_eq!(
            features.file_packing,
            kourier_model::FilePacking::default(),
            "one file per message"
        );
    }
}

/// W6a2 (D10, D11, M4): only a group's admin may delete another's message, and a group's statement is made from the row of
/// what the core was told of it, so a statement made again (a new version, a `Rebuild`) says the same rules and the same
/// reason we cannot send as the fetch said.
/// Mutants: `admin_delete` true for a direct chat; the statement made without the row.
#[tokio::test]
async fn a_groups_statement_is_made_from_its_row_and_only_a_group_has_an_admin_delete() {
    use crate::groups::{Rules, State};
    use crate::store::people;

    assert!(of(&group(), true).admin_delete);
    assert!(!of(&direct(PEER), true).admin_delete);
    let (_dir, owner) = open();
    let store = owner.store();
    let state = State {
        members_stated: true,
        rules: Some(Rules {
            announcements_only: true,
            locked: true,
            ..Rules::default()
        }),
        ..State::default()
    };
    store
        .write("seed a group", {
            let state = state.clone();
            move |tx| people::put_group(tx, &group(), &state)
        })
        .await
        .expect("seed");
    let out = stage(&store, vec![from_peer(&group(), Body::text("hi"))])
        .await
        .expect("a statement");
    let (conv, features) = out.iter().find_map(stated).expect("the statement");
    assert_eq!(conv, &group());
    assert_eq!(features, &crate::groups::features(&group(), &state));
    assert_eq!(features.cannot_send, Some(CannotSend::AdminsOnly));
    assert!(
        features
            .group_info
            .expect("group_info")
            .attributes_admin_only
    );
    assert!(features.admin_delete);
    owner.close();
}

/// Fix F15 (DUR-1): a `Rebuild` makes the core again but keeps what the server said of a group. The sweep that follows states a
/// group we left as `NotMember` and a suspended or announcements-only one closed, and the row says no member list was stated
/// (the batch states it again). Mutant: `forget_statements` deletes the group's row again.
#[tokio::test]
async fn a_rebuild_keeps_the_groups_facts_so_the_sweep_states_a_group_we_left_as_left() {
    use crate::groups::{Rules, State};
    use crate::store::people;

    let (_dir, owner) = open();
    let store = owner.store();
    let left = group();
    let closed = ConvKey::Group("wag:120363000083914563".into());
    store
        .write("seed", {
            let (left, closed) = (left.clone(), closed.clone());
            move |tx| {
                for conv in [&left, &closed] {
                    mark_stated(tx, conv, VERSION, true)?;
                }
                people::put_group(
                    tx,
                    &left,
                    &State {
                        members_stated: true,
                        not_member: true,
                        ..State::default()
                    },
                )?;
                people::put_group(
                    tx,
                    &closed,
                    &State {
                        members_stated: true,
                        rules: Some(Rules {
                            announcements_only: true,
                            ..Rules::default()
                        }),
                        ..State::default()
                    },
                )
            }
        })
        .await
        .expect("seed");
    // The rebuild's zone forgets the same books `outbox` does.
    store
        .write("rebuild", |tx| {
            forget_statements(tx)?;
            people::forget_statements(tx)
        })
        .await
        .expect("forget");
    let swept = store.write("sweep", sweep).await.expect("sweep").concat();
    let features_of = |conv: &ConvKey| {
        swept
            .iter()
            .filter_map(stated)
            .find(|(c, _)| *c == conv)
            .map(|(_, features)| features.clone())
            .expect("the group is stated again")
    };
    assert_eq!(
        features_of(&left).cannot_send,
        Some(CannotSend::NotMember),
        "a group we left is not sendable in the rebuilt core"
    );
    let closed_features = features_of(&closed);
    assert_eq!(closed_features.cannot_send, Some(CannotSend::AdminsOnly));
    assert!(
        closed_features
            .group_info
            .expect("group_info")
            .announcements_only
    );
    let row = store
        .read({
            let left = left.clone();
            move |conn| people::group(conn, &left)
        })
        .await
        .expect("row");
    assert!(!row.members_stated, "the batch states the list again");
    assert!(row.not_member);
    owner.close();
}

/// W6a2 round 2, G3 (NEW-2): the title's time is something the core was told. A notice dated by its own time (later than the
/// server's `subject_time`) holds back an older answer's title, and after a `Rebuild` the new core has no title at all, so the
/// row forgets the time and the next answer states the title.
/// Mutant: `forget_statements` keeps `subject_time`.
#[tokio::test]
async fn a_rebuild_forgets_the_titles_time_so_the_next_answer_states_the_title() {
    use crate::groups::{Fetch, State, map};
    use crate::map::OwnIds;
    use crate::store::people;
    use kourier_model::Patch;
    use whatsapp_rust::{GroupMetadata, GroupMetadataResult};

    let (_dir, owner) = open();
    let store = owner.store();
    let conv = group();
    store
        .write("seed", {
            let conv = conv.clone();
            move |tx| {
                people::put_group(
                    tx,
                    &conv,
                    &State {
                        members_stated: true,
                        subject_time: Some(1_800_000_050),
                        ..State::default()
                    },
                )
            }
        })
        .await
        .expect("seed");
    let mut metadata = GroupMetadata::new(format!("{GROUP}@g.us").parse().expect("a jid"));
    metadata.subject = Some("Synthetic title".into());
    metadata.subject_time = Some(1_800_000_000);
    let answer = GroupMetadataResult::Found(Box::new(metadata));
    let own = OwnIds::default();
    let title_of = |stored: &State| {
        let fetch = Fetch {
            own: &own,
            at_ms: 1_800_000_100_000,
        };
        let outcome = map(&answer, stored, &fetch).expect("a group");
        outcome
            .events
            .iter()
            .find_map(|event| match event {
                BackendEvent::ConversationUpsert(patch) => Some(patch.title.clone()),
                _ => None,
            })
            .expect("an upsert")
    };
    let read = |store: &KourierStore| {
        let conv = conv.clone();
        let store = store.clone();
        async move {
            store
                .read(move |conn| people::group(conn, &conv))
                .await
                .expect("row")
        }
    };
    assert_eq!(
        title_of(&read(&store).await),
        Patch::Unchanged,
        "before the rebuild the newer time holds the older answer's title back"
    );
    store
        .write("rebuild", |tx| {
            forget_statements(tx)?;
            people::forget_statements(tx)
        })
        .await
        .expect("forget");
    let row = read(&store).await;
    assert_eq!(row.subject_time, None, "the new core was told no title");
    assert_eq!(
        title_of(&row),
        Patch::Set("Synthetic title".into()),
        "the rebuilt core gets the title from the next answer"
    );
    owner.close();
}

/// W6a2 (B7b, D22): a DIRECT chat offers the three timers WhatsApp's apps offer, counted from the send, and nothing else
/// (no custom range, and so `takes_timer` is false for any other value); a group offers none (its timer is K8c's).
/// Mutants: a group that states the timers; a custom range; the timers counted from the read.
#[test]
fn a_direct_chat_offers_the_three_timers_from_the_send_and_a_group_none() {
    use kourier_model::DisappearKind;
    use std::time::Duration;

    let direct = of(&direct(PEER), true);
    assert_eq!(
        direct.disappearing.iter().collect::<Vec<_>>(),
        vec![(&DisappearKind::AfterSend, &vec![86_400, 604_800, 7_776_000])]
    );
    assert!(
        direct.disappearing_custom.is_empty(),
        "the apps ignore any other value"
    );
    for (secs, taken) in [
        (86_400, true),
        (604_800, true),
        (7_776_000, true),
        (3_600, false),
        (86_401, false),
    ] {
        assert_eq!(
            direct.takes_timer(Duration::from_secs(secs)),
            taken,
            "{secs} s"
        );
    }
    assert!(direct.has_timers());
    let group = of(&group(), true);
    assert!(group.disappearing.is_empty() && group.disappearing_custom.is_empty());
    assert!(!group.has_timers());
}

/// The phone's address-book name for `person` (a `pn:` id), put in the mirror the way the `contact` mutation's zone does.
async fn phone_names(store: &KourierStore, person: &str, name: &str) {
    let change = crate::book::change_of_action(
        &format!("{person}@s.whatsapp.net").parse().expect("a jid"),
        &{
            let mut action = waproto::whatsapp::sync_action_value::ContactAction::default();
            action.full_name = Some(name.to_owned());
            action
        },
    )
    .expect("a change");
    let events = store
        .write("test: the phone names a contact", move |tx| {
            crate::book::apply(tx, &change, 0)
        })
        .await
        .expect("apply");
    assert!(events.is_empty(), "the core does not know him yet");
}

fn system_names(events: &[BackendEvent]) -> Vec<(UserId, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            BackendEvent::ContactUpsert(patch) => match &patch.system_name {
                kourier_model::Patch::Set(name) => Some((patch.user.clone(), name.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// W6a2, B8b (D8's scope rule): the address-book name the phone gave a person is stated in the zone that first states him,
/// as a chat's peer, a group message's author or a message of his; it is stated once. Mutants: `with_statements` does not
/// ask the address book; `state` does not name a direct chat's peer.
#[tokio::test]
async fn a_person_is_named_from_the_address_book_in_the_zone_that_first_states_him() {
    let (_dir, owner) = open();
    let store = owner.store();
    phone_names(&store, PEER, "Pia Example").await;
    phone_names(&store, OLD, "Old Example").await;

    // His first message in a direct chat: the chat's statement and his name.
    let staged = stage(&store, vec![from_peer(&direct(PEER), Body::text("hi"))])
        .await
        .expect("a statement was due");
    assert_eq!(
        system_names(&staged),
        [(
            UserId::from_parts("pn", PEER).expect("a user"),
            "Pia Example".to_owned()
        )]
    );
    // Stated once: his next message names nobody.
    let again = stage(&store, vec![from_peer(&direct(PEER), Body::text("again"))]).await;
    assert!(again.is_none(), "nothing is due the second time");

    // A group message from a person the phone named: the author is named; the unnamed OLD is not in the zone and gets nothing.
    let mut in_group = from_peer(&group(), Body::text("hello"));
    if let BackendEvent::Message(message) = &mut in_group {
        message.origin = Origin::Remote {
            author: UserId::from_parts("pn", OLD).expect("a user"),
            device: None,
        };
    }
    let staged = stage(&store, vec![in_group])
        .await
        .expect("the group is stated");
    assert_eq!(
        system_names(&staged),
        [(
            UserId::from_parts("pn", OLD).expect("a user"),
            "Old Example".to_owned()
        )],
        "an author of a group message is in the group"
    );

    // The direct chat's statement alone (the call log's `features::state`) names the peer too.
    let (_other_dir, other) = open();
    let other_store = other.store();
    phone_names(&other_store, HISTORY, "Hugo Example").await;
    let stated = other_store
        .write("test: state", |tx| super::state(tx, &direct(HISTORY)))
        .await
        .expect("state");
    assert_eq!(
        system_names(&stated).len(),
        1,
        "named by the statement itself"
    );
    owner.close();
    other.close();
}

/// Fix F21 (DUR-12): a `Rebuild` keeps the push names the old core was told, marked not stated, and the new core hears each
/// ONCE, where its person is named to it again: Ann's with the sweep that states her chat, a silent member's with the zone
/// that lists him, and a name the live path states first (the same words again after the rebuild) is not told a second
/// time. Mutants: the rows deleted as before, the restate left out of `book::states`, a told name never marked told.
#[tokio::test]
async fn a_rebuild_states_the_push_names_it_held_once_where_the_people_are_named_again() {
    use crate::book::states;
    use crate::names::{push_name_event, state_push_name};
    use crate::store::people::{self, NameKind};

    let (_dir, owner) = open();
    let store = owner.store();
    let ann = direct(PEER);
    let ann_id = UserId::from_parts("pn", PEER).expect("a user");
    let bob = UserId::from_parts("pn", CALLER).expect("a user");
    let cat = UserId::from_parts("pn", OLD).expect("a user");
    store
        .write("seed", {
            let (ann, ann_id, bob, cat) = (ann.clone(), ann_id.clone(), bob.clone(), cat.clone());
            move |tx| {
                mark_stated(tx, &ann, VERSION, true)?;
                for (user, name) in [(&ann_id, "Ann P"), (&bob, "Bob Q"), (&cat, "Cat R")] {
                    people::put_name(tx, user.as_str(), NameKind::Profile, name)?;
                }
                Ok(())
            }
        })
        .await
        .expect("seed");
    store
        .write("rebuild", |tx| {
            forget_statements(tx)?;
            people::forget_statements(tx)
        })
        .await
        .expect("forget");

    // The sweep states Ann's chat again and her name with it, once.
    let swept = store.write("sweep", sweep).await.expect("sweep").concat();
    let name_of = |user: &UserId, name: &str| push_name_event(user.clone(), name.to_owned());
    assert_eq!(
        swept
            .iter()
            .filter(|event| **event == name_of(&ann_id, "Ann P"))
            .count(),
        1,
        "Ann is named to the new core"
    );
    assert_eq!(
        swept.len(),
        2,
        "her statement and her name, nothing of Bob or Cat"
    );
    assert!(
        store
            .write("again", sweep)
            .await
            .expect("sweep")
            .concat()
            .is_empty(),
        "stated once"
    );

    // Bob never writes: the zone that lists him as a member names him, once.
    let listed = |who: UserId| {
        let store = store.clone();
        async move {
            store
                .write("list", move |tx| states(tx, std::slice::from_ref(&who)))
                .await
                .expect("states")
        }
    };
    assert_eq!(listed(bob.clone()).await, [name_of(&bob, "Bob Q")]);
    assert!(listed(bob.clone()).await.is_empty(), "once");

    // Cat writes the same words as before: the live path states them (the row was not told), and the listing adds nothing.
    let mut live = Vec::new();
    let said = store
        .write("live", {
            let cat = cat.clone();
            move |tx| {
                let said = state_push_name(tx, &mut live, &cat, "Cat R")?;
                Ok((said, live))
            }
        })
        .await
        .expect("live");
    assert_eq!(said, (true, vec![name_of(&cat, "Cat R")]));
    assert!(
        listed(cat).await.is_empty(),
        "told by the live path already"
    );
    owner.close();
}

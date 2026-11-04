//! TH5 A7b: THE JOIN STATES EVERY 1:1 CHAT AT ONCE (decision E1, his QB "All at once, empty") with the phone's
//! archive, pin and mute (his QC; M8: "only when mentioned" is not muted) and its protected flag (his QA, E2),
//! and Threema's features on every chat (H2 + his QD: `NotYetBuilt`). The phone hands three contacts (one
//! archived and muted until a time, one pinned and protected, one HIDDEN) and a pinned, protected group "only
//! when mentioned"; zone 2's rows reach the core side, read LITERALLY: a 1:1 chat for each direct contact, none
//! for the hidden one, after the contacts and before the group.

use kourier_backend_api::BackendEvent;
use kourier_model::{ConvKey, ConversationPatch, Patch, UserId};

use super::harness::{self, CHATS_MUTED_UNTIL, Setup};
use crate::support;
use crate::support::mock::essential::{Spec, contact_id};

#[test]
fn zone_2_states_a_one_to_one_chat_for_every_direct_contact_with_the_phones_state() {
    support::offline::run_child(
        module_path!(),
        "zone_2_states_a_one_to_one_chat_for_every_direct_contact_with_the_phones_state_child",
    );
}

#[tokio::test]
async fn zone_2_states_a_one_to_one_chat_for_every_direct_contact_with_the_phones_state_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup {
        spec: Spec {
            contacts: 3,
            groups: 1,
            ..Setup::new(0).spec
        },
        chats: true,
        ..Setup::new(731)
    });
    j.nominated().await;
    j.expect_step("linking", 3).await;
    let got = j.ack_until_done(Vec::new(), 4).await;
    assert!(
        got == j.file_states(),
        "the core side got other events than zone 2 wrote"
    );
    let user = |i: usize| UserId::from_parts("threema", &contact_id(i)).expect("a user");
    let is_direct = |e: &BackendEvent| matches!(e, BackendEvent::ConversationUpsert(p) if matches!(p.key, ConvKey::Direct(_)));
    let directs: Vec<&ConversationPatch> = got
        .iter()
        .filter(|e| is_direct(e))
        .filter_map(|e| match e {
            BackendEvent::ConversationUpsert(p) => Some(p),
            _ => None,
        })
        .collect();
    let mut archived = ConversationPatch::new(ConvKey::Direct(user(0)));
    archived.features = Some(support::threema_features());
    archived.archived = Some(true);
    archived.pinned_order = Patch::Clear;
    archived.muted_until = Patch::Set(i64::try_from(CHATS_MUTED_UNTIL).expect("fits"));
    let mut pinned = ConversationPatch::new(ConvKey::Direct(user(1)));
    pinned.features = Some(support::threema_features());
    pinned.protected = Some(true);
    pinned.archived = Some(false);
    pinned.pinned_order = Patch::Set(0);
    pinned.muted_until = Patch::Clear;
    assert!(
        directs == vec![&archived, &pinned],
        "the 1:1 chats are not the direct contacts' with the phone's state (none for the hidden one)"
    );
    // After the contacts, before the group.
    let last_contact = got
        .iter()
        .rposition(|e| matches!(e, BackendEvent::ContactUpsert(_)))
        .expect("the contacts");
    let first_direct = got.iter().position(is_direct).expect("a 1:1 chat");
    let group_at = got
        .iter()
        .position(
            |e| matches!(e, BackendEvent::ConversationUpsert(p) if matches!(p.key, ConvKey::Group(_))),
        )
        .expect("the group");
    assert!(
        last_contact < first_direct && first_direct < group_at,
        "the 1:1 chats are not between the contacts and the groups"
    );
    let BackendEvent::ConversationUpsert(group) = &got[group_at] else {
        unreachable!("found as a ConversationUpsert");
    };
    assert!(
        group.features == Some(support::threema_features()),
        "the group lacks Threema's features"
    );
    assert_eq!(group.protected, Some(true), "the protected group");
    assert_eq!(
        (group.archived, group.pinned_order, group.muted_until),
        (Some(false), Patch::Set(0), Patch::Clear),
        "the pinned group, only when mentioned = not muted"
    );
    j.no_more_steps().await;
    j.finish().await;
}

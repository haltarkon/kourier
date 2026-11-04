//! The history map, the rest of a chunk (W4, C10): the people go first (pairs, then names), never naming
//! us, and the parts that are no message are kept for later. Every number, id and name is synthetic.

use kourier_backend_api::BackendEvent;
use kourier_model::{ContactPatch, Patch, UserId};
use waproto::buffa::{Message as _, MessageField};
use waproto::whatsapp as wa;

use super::{Kind, kept, people};
use crate::history::testing::{Chunk, SyncType};
use crate::map::{Ctx, OwnIds};
use crate::names::NAME_CAP;

const A: &str = "15550100101@s.whatsapp.net";
const A_LID: &str = "99900100101@lid";
const B: &str = "15550100102@s.whatsapp.net";
const B_LID: &str = "99900100102@lid";
const OWN: &str = "15550100999@s.whatsapp.net";
const OWN_LID: &str = "99900100999@lid";
const GROUP: &str = "120363000000000001@g.us";

fn ctx() -> Ctx {
    Ctx {
        own: OwnIds {
            lid: Some(user("lid", "99900100999")),
            pn: Some(user("pn", "15550100999")),
        },
    }
}

fn user(scheme: &str, value: &str) -> UserId {
    UserId::from_parts(scheme, value).expect("user")
}

fn merged(pn: &str, lid: &str) -> BackendEvent {
    BackendEvent::IdentityMerged {
        from: user("pn", pn),
        into: user("lid", lid),
    }
}

fn named(scheme: &str, value: &str, name: &str) -> BackendEvent {
    let mut patch = ContactPatch::new(user(scheme, value));
    patch.profile_name = Patch::Set(name.to_owned());
    BackendEvent::ContactUpsert(patch)
}

#[test]
fn pairs_come_before_the_batches_and_never_name_us() {
    // The names are given first and the pairs last; the people list puts every pair ahead of every name.
    let chunk = Chunk::new(SyncType::PUSH_NAME, 1, 100)
        .push_name(A, "Ann Example")
        .push_name(B_LID, "Bob Example")
        .pair(A, A_LID)
        .pair(B, B_LID)
        .pair(A, A_LID);
    let events = people(&chunk.sync, &ctx());
    assert_eq!(
        events,
        vec![
            merged("15550100101", "99900100101"),
            merged("15550100102", "99900100102"),
            named("pn", "15550100101", "Ann Example"),
            named("lid", "99900100102", "Bob Example"),
        ]
    );

    // A pair that names one of our ids (either of the two), and our own push name, are never stated.
    let chunk = Chunk::new(SyncType::PUSH_NAME, 1, 100)
        .pair(OWN, OWN_LID)
        .pair(OWN, B_LID)
        .pair(A, OWN_LID)
        .push_name(OWN, "My Own Name")
        .push_name(OWN_LID, "My Own Name")
        .push_name(A, "Ann Example");
    assert_eq!(
        people(&chunk.sync, &ctx()),
        vec![named("pn", "15550100101", "Ann Example")]
    );
}

#[test]
fn a_push_name_is_trimmed_cut_and_belongs_to_a_person() {
    let long = "n".repeat(NAME_CAP + 40);
    let chunk = Chunk::new(SyncType::PUSH_NAME, 1, 100)
        .push_name(A, "  Ann Example \n")
        .push_name(B, "   ")
        .push_name(B_LID, &long)
        .push_name(GROUP, "A Group Is No Person")
        .push_name("status@broadcast", "Nobody")
        .push_name("not a jid", "Nobody")
        // The same person twice: the last name given stands, in the place of the first.
        .push_name(A, "Ann Renamed");
    let events = people(&chunk.sync, &ctx());
    assert_eq!(
        events,
        vec![
            named("pn", "15550100101", "Ann Renamed"),
            named("lid", "99900100102", &"n".repeat(NAME_CAP)),
        ]
    );

    // A pair whose halves are missing, broken or of one kind is no pair.
    let mut rest = wa::HistorySync::default();
    for (pn, lid) in [
        (Some(A), None),
        (None, Some(A_LID)),
        (Some("not a jid"), Some(A_LID)),
        (Some(A), Some(B)),
        (Some(A_LID), Some(B_LID)),
    ] {
        let mut mapping = wa::PhoneNumberToLIDMapping::default();
        mapping.pn_jid = pn.map(str::to_owned);
        mapping.lid_jid = lid.map(str::to_owned);
        rest.phone_number_to_lid_mappings.push(mapping);
    }
    assert_eq!(people(&rest, &ctx()), Vec::new());
}

#[test]
fn call_records_are_kept_for_later() {
    let mut rest = Chunk::new(SyncType::RECENT, 2, 60).sync;

    let mut call = wa::CallLogRecord::default();
    call.call_id = Some("CALL-0001".to_owned());
    call.start_time = Some(1_700_000_100);
    call.is_incoming = Some(true);
    call.group_jid = Some(GROUP.to_owned());
    let mut solo = wa::CallLogRecord::default();
    solo.call_id = Some("CALL-0002".to_owned());
    rest.call_log_records = vec![call.clone(), solo.clone()];

    let mut past = wa::PastParticipants::default();
    past.group_jid = Some(GROUP.to_owned());
    let mut gone = wa::PastParticipant::default();
    gone.user_jid = Some(A.to_owned());
    past.past_participants.push(gone);
    rest.past_participants = vec![past.clone()];

    let mut contact = wa::InlineContact::default();
    contact.pn_jid = Some(A.to_owned());
    contact.full_name = Some("Ann Example".to_owned());
    rest.inline_contacts = vec![contact.clone()];

    let mut settings = wa::GlobalSettings::default();
    settings.disappearing_mode_duration = Some(86_400);
    rest.global_settings = MessageField::some(settings.clone());

    // What this build does not keep: statuses, stickers, secrets, keys, the AI wait list.
    rest.status_v3_messages = vec![wa::WebMessageInfo::default()];
    rest.thread_id_user_secret = Some(vec![7; 32]);
    rest.shareable_chat_identifier_encryption_key = Some(vec![8; 32]);
    rest.nct_salt = Some(vec![9; 32]);
    rest.companion_meta_nonce = Some("nonce".to_owned());

    let parts = kept(&rest);
    let kinds: Vec<(Kind, &str)> = parts.iter().map(|p| (p.kind, p.conv.as_str())).collect();
    assert_eq!(
        kinds,
        vec![
            (Kind::Call, GROUP),
            (Kind::Call, ""),
            (Kind::PastParticipants, GROUP),
            (Kind::Contact, ""),
            (Kind::Settings, ""),
        ]
    );
    // Each part is the encoded protobuf of its kind, so the build that shows it can read it back.
    assert_eq!(parts[0].item, call.encode_to_vec());
    assert_eq!(parts[1].item, solo.encode_to_vec());
    assert_eq!(parts[2].item, past.encode_to_vec());
    assert_eq!(parts[3].item, contact.encode_to_vec());
    assert_eq!(parts[4].item, settings.encode_to_vec());
    let back = wa::CallLogRecord::decode_from_slice(&parts[0].item).expect("reads back");
    assert_eq!(back.call_id.as_deref(), Some("CALL-0001"));

    // No secret or key of the chunk is among the bytes kept.
    for part in &parts {
        for secret in [7u8, 8, 9] {
            assert!(
                !part
                    .item
                    .windows(32)
                    .any(|w| w.iter().all(|b| *b == secret))
            );
        }
        assert!(!part.item.windows(5).any(|w| w == b"nonce"));
    }

    // A chunk with none of them keeps nothing, and the kinds are what the table stores.
    assert!(kept(&Chunk::new(SyncType::RECENT, 1, 10).sync).is_empty());
    assert_eq!(
        [
            Kind::Message,
            Kind::Call,
            Kind::PastParticipants,
            Kind::Contact,
            Kind::Settings
        ]
        .map(|k| k as u8),
        [0, 1, 2, 3, 4]
    );
}

/// W6a2 B8a (D8): the placeholder "-" is no name in the history either (it would name a hundred strangers), nor is a name that
/// only a dash and blanks make; a name that holds a dash stays. Mutant: the history path keeps the placeholder.
#[test]
fn the_placeholder_is_no_push_name_in_the_history() {
    let chunk = Chunk::new(SyncType::PUSH_NAME, 1, 100)
        .push_name(A, "-")
        .push_name(B, " - ")
        .push_name(B_LID, "-Bob");
    assert_eq!(
        people(&chunk.sync, &ctx()),
        vec![named("lid", "99900100102", "-Bob")]
    );
}

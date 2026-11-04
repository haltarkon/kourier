//! The phone's builders (`support/mock/{body,envelope}.rs`, TH5 A2) on their own: the CSP bodies
//! against LITERAL byte vectors written from the spec text (each cites its line), the protobuf kinds
//! and the envelopes against libthreema's GENERATED types (rule 9 lets the fake take those), the sync
//! deltas field by field. No server and no client: the builders through the fake mediator are
//! `envelope_reflect.rs`. A failure says what differs and never prints a body, an ID or a key.

use libthreema::protobuf::{common, csp_e2e, d2d, d2d_sync};
use prost::Message as _;

use super::unhex;
use crate::support::mock::body::{
    self, FileMeta, FileSpec, MENTION_ALL, MsgId, Place, TYPE_NAMES, id_hex, id_u64, ty,
};
use crate::support::mock::d2m::EPHEMERAL;
use crate::support::mock::envelope::{
    Author, ContactDelta, Conv, GroupDelta, MemberChange, Msg, PADDING, flags_for, tampered,
    too_short,
};

#[expect(deprecated, reason = "the spec's padding field")]
fn padding_of(envelope: &d2d::Envelope) -> &[u8] {
    &envelope.padding
}

/// Message ids with every byte different, so a wrong byte order shows.
const ID_A: MsgId = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
const ID_B: MsgId = [0xf1, 0xe2, 0xd3, 0xc4, 0xb5, 0xa6, 0x97, 0x88];
const ID_C: MsgId = [0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xee];
const NOW: u64 = 1_760_000_123_456;
const NONCE: [u8; 24] = [0x42; 24];

const PEER: &str = "PEERAAAA";
const MEMBER: &str = "MEMBBBBB";
const CREATOR: &str = "CREACCCC";
const GROUP: u64 = 0x0807_0605_0403_0201;

fn author() -> Author {
    Author::new(0x0123_4567_89ab_cdef)
}

/// Encodes and decodes an envelope through libthreema's generated type (the wire form).
fn wire(envelope: &d2d::Envelope) -> d2d::Envelope {
    d2d::Envelope::decode(envelope.encode_to_vec().as_slice())
        .expect("the builder's envelope decodes with the generated type")
}

// --- The type table -----------------------------------------------------------------------------

/// The numbers of `body::ty` are the pinned `CspE2eMessageType`'s, by name, and the pinned enum has
/// no value the table lacks (a newer spec stops here until the builders know it).
#[test]
fn the_type_numbers_are_the_pinned_enums() {
    for (name, number) in TYPE_NAMES {
        let value = common::CspE2eMessageType::from_str_name(name)
            .unwrap_or_else(|| panic!("the pinned enum has no value {name}"));
        assert!(
            value as i32 == i32::from(*number),
            "{name} is not {number:#04x} in the pinned enum"
        );
    }
    let in_enum: Vec<i32> = (0..=255)
        .filter(|n| common::CspE2eMessageType::try_from(*n).is_ok())
        .collect();
    assert!(
        in_enum.len() == TYPE_NAMES.len(),
        "the pinned enum has {} values, the builders' table {}",
        in_enum.len(),
        TYPE_NAMES.len()
    );
    // The type the builders call unknown is really not in the enum.
    assert!(common::CspE2eMessageType::try_from(i32::from(ty::UNKNOWN)).is_err());
}

/// A group type is the 1:1 type's name behind `GROUP_` (`DEPRECATED_` dropped), and a type without a
/// group form (typing, calls, the controls) has none.
#[test]
fn group_types_are_the_group_names_of_the_pinned_enum() {
    let name_of = |n: u8| {
        TYPE_NAMES
            .iter()
            .find(|(_, t)| *t == n)
            .map(|(name, _)| *name)
            .expect("a named type")
    };
    let mut with_group = 0;
    for (name, number) in TYPE_NAMES {
        match body::group_type(*number) {
            Some(group) => {
                with_group += 1;
                let expected = format!("GROUP_{}", name.trim_start_matches("DEPRECATED_"));
                assert!(
                    name_of(group) == expected,
                    "{name} has the group form {}",
                    name_of(group)
                );
            }
            None => assert!(
                !name.starts_with("GROUP_") || body::group_type(*number).is_none(),
                "{name}"
            ),
        }
    }
    assert!(
        with_group == 12,
        "12 types have a group form, found {with_group}"
    );
    assert!(body::group_type(ty::TYPING_INDICATOR).is_none());
    assert!(body::group_type(ty::CALL_OFFER).is_none());
}

// --- Text, quote, mentions ---------------------------------------------------------------------

/// A text body is the UTF-8 and nothing else (`TP/csp.struct.yml:3012-3016`, the `text` field "UTF-8
/// encoded text"); a quote is `> quote #` + the id's 8 wire bytes as 16 lowercase hex digits + TWO line
/// feeds + the comment. The literal is Desktop's `serializeQuoteText` (`quotes.ts:62`), not the pinned
/// spec's [I].
#[test]
fn a_quote_has_its_prefix_and_separator() {
    assert!(body::text("héllo") == "héllo".as_bytes());
    // "> quote #" 0102030405060708 "\n\n" "ok"
    let expected = unhex(
        "3e2071756f7465202330313032303330343035303630373038 0a0a 6f6b"
            .replace(' ', "")
            .as_str(),
    );
    assert!(
        body::quote(ID_A, "ok").into_bytes() == expected,
        "the quote's prefix, id digits or separator changed"
    );
    // The digits are the wire bytes in order, not the u64's big-endian form.
    assert!(id_hex(ID_B) == "f1e2d3c4b5a69788");
    assert!(id_u64(ID_A) == 0x0807_0605_0403_0201);
}

#[test]
fn mentions_are_at_bracket_id_bracket() {
    // "@[" "PEERAAAA" "]"
    assert!(
        body::mention(PEER).into_bytes()
            == unhex("405b504545524141414 15d".replace(' ', "").as_str())
    );
    assert!(MENTION_ALL == "@[@@@@@@@@]");
}

// --- Group containers ---------------------------------------------------------------------------

/// `group-member-container` (`TP/csp.struct.yml:2886-2903`): creator-identity (8 bytes), group-id
/// (`u64-le`, `:28`), then the inner struct.
#[test]
fn the_member_container_is_creator_then_the_group_id_little_endian() {
    let inner = [0xaa, 0xbb];
    let got = body::group_member_container(CREATOR, GROUP, &inner);
    // "CREACCCC" 01 02 03 04 05 06 07 08 aa bb
    let expected = unhex(
        "435245414343434301020304050607 08aabb"
            .replace(' ', "")
            .as_str(),
    );
    assert!(got == expected, "the member container's layout");
}

/// `group-creator-container` (`TP/csp.struct.yml:2870-2884`): the group-id (`u64-le`) then the inner
/// struct, no creator.
#[test]
fn the_creator_container_is_the_group_id_little_endian() {
    let got = body::group_creator_container(GROUP, &[0xcc]);
    assert!(
        got == unhex("0102030405060708cc"),
        "the creator container's layout"
    );
}

#[test]
fn a_message_sent_into_a_group_gets_the_group_type_and_the_container() {
    let msg = Msg::text(ID_A, NOW, "hi").in_group(CREATOR, GROUP);
    assert!(msg.ty == ty::GROUP_TEXT);
    let mut expected = b"CREACCCC".to_vec();
    expected.extend_from_slice(&GROUP.to_le_bytes());
    expected.extend_from_slice(b"hi");
    assert!(msg.body == expected, "the group text's body");
    // The receipt and the protobuf kinds too.
    assert!(
        Msg::receipt(ID_A, NOW, 3, &[ID_B])
            .in_group(CREATOR, GROUP)
            .ty
            == ty::GROUP_DELIVERY_RECEIPT
    );
    assert!(Msg::edit(ID_A, NOW, ID_B, "x").in_group(CREATOR, GROUP).ty == ty::GROUP_EDIT_MESSAGE);
    assert!(Msg::delete(ID_A, NOW, ID_B).in_group(CREATOR, GROUP).ty == ty::GROUP_DELETE_MESSAGE);
    assert!(
        Msg::reaction(ID_A, NOW, ID_B, "x")
            .in_group(CREATOR, GROUP)
            .ty
            == ty::GROUP_REACTION
    );
}

// --- Location, file, receipt, typing -------------------------------------------------------------

/// `e2e.location` (`TP/csp.struct.yml:3243-3278`): `<lat>,<lon>[,<accuracy>]`, then `\n<address>` or
/// `\n<name>\n<address>`; a line feed in an address is the two characters backslash and `n`.
#[test]
fn a_location_is_comma_and_line_separated() {
    let loc = |acc, place: &Place| {
        String::from_utf8(body::location("47.3769", "8.5417", acc, place)).expect("UTF-8")
    };
    assert!(loc(None, &Place::Nothing) == "47.3769,8.5417");
    assert!(loc(Some("12.5"), &Place::Nothing) == "47.3769,8.5417,12.5");
    assert!(loc(None, &Place::Address("Main 1".into())) == "47.3769,8.5417\nMain 1");
    assert!(
        loc(
            Some("3"),
            &Place::Named {
                name: "Cafe".into(),
                address: "Main 1\nZurich".into()
            }
        ) == "47.3769,8.5417,3\nCafe\nMain 1\\nZurich",
        "name, then the address with its line feed escaped"
    );
}

/// `e2e.file` (`TP/csp.struct.yml:3607-3660`): UTF-8 JSON with the one-letter keys; `'i'` is 1 for
/// rendering 1 and 2 only; absent options are left out.
#[test]
fn a_file_message_is_json_with_the_spec_keys() {
    let spec = FileSpec::picture("pic.jpg", Some("A \"cap\"\nline"));
    let json = String::from_utf8(body::file(&spec)).expect("UTF-8");
    let k = "ab".repeat(32);
    let b = "cd".repeat(16);
    let t = "ef".repeat(16);
    let expected = format!(
        "{{\"j\":1,\"i\":1,\"k\":\"{k}\",\"b\":\"{b}\",\"t\":\"{t}\",\"p\":\"image/jpeg\",\"m\":\"image/jpeg\",\
         \"n\":\"pic.jpg\",\"s\":123456,\"d\":\"A \\\"cap\\\"\\nline\",\"x\":{{\"w\":640,\"h\":480,\"a\":false}}}}"
    );
    assert!(json == expected, "the file JSON's keys, order or escaping");
    // Rendering 0 is a plain file: 'i' is 0, and with no options only the required keys stay.
    let plain = FileSpec {
        rendering: 0,
        thumbnail_hex: None,
        thumbnail_type: None,
        name: None,
        caption: None,
        meta: None,
        ..spec.clone()
    };
    let json = String::from_utf8(body::file(&plain)).expect("UTF-8");
    assert!(
        json == format!(
            "{{\"j\":0,\"i\":0,\"k\":\"{k}\",\"b\":\"{b}\",\"m\":\"image/jpeg\",\"s\":123456}}"
        ),
        "a plain file's JSON"
    );
    // Rendering 2 (a sticker) is 'i' 1 again; audio metadata is the duration.
    let sticker = FileSpec {
        rendering: 2,
        meta: Some(FileMeta {
            duration: Some("12.5".into()),
            ..FileMeta::default()
        }),
        ..plain
    };
    let json = String::from_utf8(body::file(&sticker)).expect("UTF-8");
    assert!(
        json.contains("\"i\":1") && json.ends_with("\"x\":{\"d\":12.5}}"),
        "a sticker with a duration"
    );
}

/// `e2e.delivery-receipt` (`TP/csp.struct.yml:4344-4358`): the status byte, then the ids (`u64-le[]`, so
/// the wire bytes of each id).
#[test]
fn a_delivery_receipt_is_the_status_then_the_ids() {
    let got = body::delivery_receipt(2, &[ID_A, ID_B]);
    assert!(
        got == unhex(
            "0201020304050607 08f1e2d3c4b5a69788"
                .replace(' ', "")
                .as_str()
        )
    );
    assert!(body::delivery_receipt(3, &[ID_C]) == unhex("0311000000000000ee"));
}

/// `e2e.typing-indicator` (`TP/csp.struct.yml:4443-4449`): one byte, 1 typing, 0 stopped, others
/// invalid.
#[test]
fn a_typing_indicator_is_one_byte() {
    assert!(body::typing(1) == [1] && body::typing(0) == [0] && body::typing(2) == [2]);
}

// --- The protobuf kinds -------------------------------------------------------------------------

/// `EditMessage`, `DeleteMessage` and `Reaction` decode with libthreema's generated types; the
/// message id is a `fixed64` (wire tag `0x09` for field 1), the id's wire bytes little-endian; a
/// reaction's `apply` is field 2 (`0x12`), `withdraw` field 3 (`0x1a`).
#[test]
fn the_protobuf_kinds_decode_with_the_generated_types() {
    let edit = csp_e2e::EditMessage::decode(body::edit_message(ID_A, "new").as_slice())
        .expect("EditMessage");
    assert!(edit.message_id == id_u64(ID_A) && edit.text == "new");
    let delete = body::delete_message(ID_B);
    assert!(
        delete == unhex("09f1e2d3c4b5a69788"),
        "DeleteMessage's literal bytes"
    );
    assert!(
        csp_e2e::DeleteMessage::decode(delete.as_slice())
            .expect("DeleteMessage")
            .message_id
            == id_u64(ID_B)
    );
    let apply = body::reaction_apply(ID_A, "👍".as_bytes());
    // 09 <id> 12 04 f0 9f 91 8d
    assert!(
        apply == unhex("090102030405060708 1204f09f918d".replace(' ', "").as_str()),
        "apply's literal bytes"
    );
    let withdraw = body::reaction_withdraw(ID_A, "👍".as_bytes());
    assert!(
        withdraw == unhex("090102030405060708 1a04f09f918d".replace(' ', "").as_str()),
        "withdraw's literal bytes"
    );
    let decoded = csp_e2e::Reaction::decode(apply.as_slice()).expect("Reaction");
    assert!(decoded.message_id == id_u64(ID_A));
    assert!(
        matches!(decoded.action, Some(csp_e2e::reaction::Action::Apply(e)) if e == "👍".as_bytes())
    );
    let none = csp_e2e::Reaction::decode(body::reaction_without_action(ID_A).as_slice())
        .expect("Reaction");
    assert!(none.action.is_none());
    // Invalid bytes pass through untouched: an empty emoji and one of 65 bytes.
    assert!(matches!(
        csp_e2e::Reaction::decode(body::reaction_apply(ID_A, b"").as_slice()).expect("Reaction").action,
        Some(csp_e2e::reaction::Action::Apply(e)) if e.is_empty()
    ));
    assert!(matches!(
        csp_e2e::Reaction::decode(body::reaction_withdraw(ID_A, &[b'x'; 65]).as_slice()).expect("Reaction").action,
        Some(csp_e2e::reaction::Action::Withdraw(e)) if e.len() == 65
    ));
}

// --- Envelopes ----------------------------------------------------------------------------------

/// An incoming message decodes with the generated type, every field where the proto puts it; the id
/// is the wire bytes read as a little-endian `fixed64`; no nonce is none, the nonce is the 24 bytes.
#[test]
fn an_incoming_message_decodes_with_the_generated_types() {
    let a = author();
    let envelope = wire(&a.incoming(PEER, &Msg::text(ID_A, NOW, "hi").nonce(NONCE)));
    assert!(envelope.device_id == 0x0123_4567_89ab_cdef);
    assert!(envelope.protocol_version == d2d::ProtocolVersion::V03 as u32);
    let Some(d2d::envelope::Content::IncomingMessage(m)) = envelope.content else {
        panic!("not an incoming message");
    };
    assert!(m.sender_identity == PEER && m.message_id == id_u64(ID_A) && m.created_at == NOW);
    assert!(m.r#type == 1 && m.body == b"hi" && m.nonce == NONCE);
    // The same without a nonce.
    let envelope = wire(&a.incoming(PEER, &Msg::text(ID_A, NOW, "hi")));
    let Some(d2d::envelope::Content::IncomingMessage(m)) = envelope.content else {
        panic!("not an incoming message");
    };
    assert!(m.nonce.is_empty(), "a message with no nonce carries none");
}

/// An outgoing message names its conversation (a contact, a group, a list), carries one nonce per
/// receiver and the optional thread id.
#[test]
fn an_outgoing_message_names_its_conversation_and_nonces() {
    let a = author();
    let decode = |e: d2d::Envelope| match wire(&e).content {
        Some(d2d::envelope::Content::OutgoingMessage(m)) => m,
        _ => panic!("not an outgoing message"),
    };
    let msg = Msg::text(ID_A, NOW, "out")
        .nonce([1; 24])
        .nonce([2; 24])
        .thread(ID_B);
    let m = decode(a.outgoing(&Conv::contact(PEER), &msg));
    assert!(
        m.conversation.and_then(|c| c.id) == Some(d2d::conversation_id::Id::Contact(PEER.into()))
    );
    assert!(m.nonces == vec![vec![1; 24], vec![2; 24]]);
    assert!(m.thread_message_id == Some(id_u64(ID_B)) && m.message_id == id_u64(ID_A));
    let m = decode(a.outgoing(&Conv::group(CREATOR, GROUP), &msg));
    assert!(
        m.conversation.and_then(|c| c.id)
            == Some(d2d::conversation_id::Id::Group(common::GroupIdentity {
                group_id: GROUP,
                creator_identity: CREATOR.into()
            }))
    );
    let m = decode(a.outgoing(&Conv::List(77), &Msg::text(ID_C, NOW, "x")));
    assert!(
        m.conversation.and_then(|c| c.id) == Some(d2d::conversation_id::Id::DistributionList(77))
    );
    assert!(m.nonces.is_empty() && m.thread_message_id.is_none());
}

#[test]
fn the_sent_and_read_updates_decode() {
    let a = author();
    let Some(d2d::envelope::Content::OutgoingMessageUpdate(u)) = wire(&a.sent(&[
        (Conv::contact(PEER), ID_A),
        (Conv::group(CREATOR, GROUP), ID_B),
    ]))
    .content
    else {
        panic!("not an outgoing update");
    };
    assert!(u.updates.len() == 2);
    assert!(u.updates[0].message_id == id_u64(ID_A));
    assert!(matches!(
        u.updates[1].update,
        Some(d2d::outgoing_message_update::update::Update::Sent(_))
    ));
    let Some(d2d::envelope::Content::IncomingMessageUpdate(u)) =
        wire(&a.read(&[(Conv::contact(PEER), ID_C, NOW + 5)])).content
    else {
        panic!("not an incoming update");
    };
    assert!(u.updates.len() == 1 && u.updates[0].message_id == id_u64(ID_C));
    assert!(matches!(
        u.updates[0].update,
        Some(d2d::incoming_message_update::update::Update::Read(d2d::incoming_message_update::Read { at })) if at == NOW + 5
    ));
}

/// The padding is "a random amount, ignored by the receiver": here a fixed non-zero length by default,
/// settable; the protocol version is V0.3, V0.1 or unset; an envelope can have no content, and the
/// plaintext with a oneof member of a newer build decodes with `content` unset.
#[test]
fn padding_versions_and_empty_content() {
    let a = author();
    assert!(padding_of(&wire(&a.incoming(PEER, &Msg::text(ID_A, NOW, "x")))).len() == PADDING);
    assert!(
        padding_of(&wire(
            &a.padding(0).incoming(PEER, &Msg::text(ID_A, NOW, "x"))
        ))
        .is_empty()
    );
    let padded = wire(&a.padding(300).incoming(PEER, &Msg::text(ID_A, NOW, "x")));
    assert!(padding_of(&padded).len() == 300);
    assert!(
        padding_of(&padded).iter().any(|b| *b != 0),
        "padding of zeros"
    );
    assert!(a.v01().protocol_version == 1 && a.unversioned().protocol_version == 0);
    assert!(wire(&a.empty()).content.is_none());
    let unknown = d2d::Envelope::decode(a.unknown_content_plaintext().as_slice())
        .expect("a newer oneof member decodes");
    assert!(unknown.content.is_none() && unknown.device_id == a.device_id);
}

/// A typing indicator is reflected with the ephemeral flag, nothing else is, and an OUTGOING typing is
/// not (the spec reflects it for incoming only, `TP/csp.struct.yml:4410-4415`).
#[test]
fn only_an_incoming_typing_indicator_is_ephemeral() {
    let a = author();
    assert!(flags_for(&a.incoming(PEER, &Msg::typing(ID_A, NOW, 1))) == EPHEMERAL);
    assert!(flags_for(&a.incoming(PEER, &Msg::text(ID_A, NOW, "x"))) == 0);
    assert!(flags_for(&a.outgoing(&Conv::contact(PEER), &Msg::typing(ID_A, NOW, 1))) == 0);
    assert!(flags_for(&a.sent(&[(Conv::contact(PEER), ID_A)])) == 0);
}

/// A tampered box differs in one byte, a short one has 39 bytes.
#[test]
fn a_tampered_box_and_a_short_one() {
    let sealed = vec![0u8; 80];
    let bad = tampered(&sealed, 30);
    assert!(bad[30] == 1 && bad.iter().filter(|b| **b != 0).count() == 1);
    assert!(too_short().len() == 39);
}

// --- Sync deltas --------------------------------------------------------------------------------

/// A contact create carries what a NEW contact requires; an update carries the identity and what
/// changed; names cleared are PRESENT and empty on the wire (an empty string is a value, not an
/// absence); "removed" is `acquaintance_level = GROUP_OR_DELETED` (the proto has no delete).
#[test]
fn contact_deltas_carry_only_what_changed() {
    let a = author();
    let key = [0x77; 32];
    let content = |e: d2d::Envelope| match wire(&e).content {
        Some(d2d::envelope::Content::ContactSync(s)) => s.action.expect("an action"),
        _ => panic!("not a contact sync"),
    };
    let d2d::contact_sync::Action::Create(create) =
        content(a.contact_create(ContactDelta::create(PEER, key, NOW).names("Ann", "Lee")))
    else {
        panic!("not a create");
    };
    let c = create.contact.expect("a contact");
    assert!(
        c.identity == PEER
            && c.public_key.as_deref() == Some(key.as_slice())
            && c.created_at == Some(NOW)
    );
    assert!(c.first_name.as_deref() == Some("Ann") && c.last_name.as_deref() == Some("Lee"));
    assert!(
        c.verification_level.is_some() && c.acquaintance_level == Some(0) && c.sync_state.is_some()
    );
    assert!(c.nickname.is_none() && c.conversation_visibility.is_none());

    // An update: only what changed.
    let d2d::contact_sync::Action::Update(update) =
        content(a.contact_update(ContactDelta::new(PEER).clear_names().archived()))
    else {
        panic!("not an update");
    };
    let c = update.contact.expect("a contact");
    assert!(c.public_key.is_none() && c.created_at.is_none() && c.acquaintance_level.is_none());
    assert!(
        c.first_name.as_deref() == Some("") && c.last_name.as_deref() == Some(""),
        "cleared names must be present"
    );
    assert!(c.conversation_visibility == Some(d2d_sync::ConversationVisibility::Archived as i32));

    // "Deleted".
    let d2d::contact_sync::Action::Update(update) = content(a.contact_delete(PEER)) else {
        panic!("a delete is an update");
    };
    assert!(
        update.contact.expect("a contact").acquaintance_level
            == Some(d2d_sync::contact::AcquaintanceLevel::GroupOrDeleted as i32)
    );
}

/// The contact's chat settings: category, visibility, the notification override (`NEVER` with or without
/// an expiry, and the default again).
#[test]
fn contact_chat_settings() {
    use d2d_sync::contact::notification_trigger_policy_override as o;
    let c = ContactDelta::new(PEER)
        .protected()
        .pinned()
        .muted_until(NOW + 9)
        .build();
    assert!(c.conversation_category == Some(d2d_sync::ConversationCategory::Protected as i32));
    assert!(c.conversation_visibility == Some(d2d_sync::ConversationVisibility::Pinned as i32));
    let Some(o::Override::Policy(p)) = c
        .notification_trigger_policy_override
        .and_then(|n| n.r#override)
    else {
        panic!("not a policy");
    };
    assert!(
        p.policy == o::policy::NotificationTriggerPolicy::Never as i32
            && p.expires_at == Some(NOW + 9)
    );
    let c = ContactDelta::new(PEER)
        .unprotected()
        .normal()
        .unmuted()
        .build();
    assert!(c.conversation_category == Some(d2d_sync::ConversationCategory::Default as i32));
    assert!(c.conversation_visibility == Some(d2d_sync::ConversationVisibility::Normal as i32));
    assert!(matches!(
        c.notification_trigger_policy_override
            .and_then(|n| n.r#override),
        Some(o::Override::Default(_))
    ));
    let c = ContactDelta::new(PEER).muted().build();
    assert!(matches!(
        c.notification_trigger_policy_override.and_then(|n| n.r#override),
        Some(o::Override::Policy(p)) if p.expires_at.is_none()
    ));
}

/// A group create has the members; an update names the member changes (the map the spec's notices come
/// from) and replaces the member list whole; a delete names the group; names are set and cleared; the
/// group's own notification policy has MENTIONED.
#[test]
fn group_deltas_and_member_changes() {
    use d2d_sync::group::notification_trigger_policy_override as o;
    let a = author();
    let action = |e: d2d::Envelope| match wire(&e).content {
        Some(d2d::envelope::Content::GroupSync(s)) => s.action.expect("an action"),
        _ => panic!("not a group sync"),
    };
    let d2d::group_sync::Action::Create(create) = action(a.group_create(GroupDelta::create(
        CREATOR,
        GROUP,
        "Team",
        &[MEMBER, PEER],
        NOW,
    ))) else {
        panic!("not a create");
    };
    let g = create.group.expect("a group");
    assert!(
        g.group_identity
            == Some(common::GroupIdentity {
                group_id: GROUP,
                creator_identity: CREATOR.into()
            })
    );
    assert!(g.name.as_deref() == Some("Team") && g.created_at == Some(NOW));
    assert!(g.user_state == Some(d2d_sync::group::UserState::Member as i32));
    assert!(g.member_identities.expect("members").identities == vec![MEMBER, PEER]);

    let d2d::group_sync::Action::Update(update) = action(
        a.group_update(
            GroupDelta::new(CREATOR, GROUP)
                .clear_name()
                .members(&[PEER])
                .mentioned_only_until(NOW + 1),
            &[
                (MEMBER, MemberChange::Kicked),
                ("NEWDDDDD", MemberChange::Added),
                ("LEFTEEEE", MemberChange::Left),
            ],
        ),
    ) else {
        panic!("not an update");
    };
    assert!(update.member_state_changes.len() == 3);
    assert!(update.member_state_changes[MEMBER] == MemberChange::Kicked as i32);
    assert!(update.member_state_changes["NEWDDDDD"] == MemberChange::Added as i32);
    assert!(update.member_state_changes["LEFTEEEE"] == MemberChange::Left as i32);
    let g = update.group.expect("a group");
    assert!(
        g.name.as_deref() == Some(""),
        "a cleared name is present and empty"
    );
    assert!(
        g.member_identities.expect("members").identities == vec![PEER],
        "the list is the whole new list"
    );
    assert!(g.created_at.is_none() && g.user_state.is_none());
    let Some(o::Override::Policy(p)) = g
        .notification_trigger_policy_override
        .and_then(|n| n.r#override)
    else {
        panic!("not a policy");
    };
    assert!(
        p.policy == o::policy::NotificationTriggerPolicy::Mentioned as i32
            && p.expires_at == Some(NOW + 1)
    );
    let g = GroupDelta::new(CREATOR, GROUP)
        .muted()
        .archived()
        .protected()
        .user_state(d2d_sync::group::UserState::Left)
        .build();
    assert!(matches!(
        g.notification_trigger_policy_override.and_then(|n| n.r#override),
        Some(o::Override::Policy(p)) if p.policy == o::policy::NotificationTriggerPolicy::Never as i32 && p.expires_at.is_none()
    ));
    assert!(g.conversation_visibility == Some(d2d_sync::ConversationVisibility::Archived as i32));
    assert!(g.user_state == Some(d2d_sync::group::UserState::Left as i32));

    let d2d::group_sync::Action::Delete(delete) = action(a.group_delete(CREATOR, GROUP)) else {
        panic!("not a delete");
    };
    assert!(
        delete.group_identity
            == Some(common::GroupIdentity {
                group_id: GROUP,
                creator_identity: CREATOR.into()
            })
    );
}

/// The other syncs decode: lists, the user profile, the settings, the MDM parameters.
#[test]
fn the_other_syncs_decode() {
    let a = author();
    let list = d2d_sync::DistributionList {
        distribution_list_id: 99,
        name: Some("List".into()),
        member_identities: Some(common::Identities {
            identities: vec![PEER.into()],
        }),
        ..Default::default()
    };
    for (envelope, kind) in [
        (a.list_create(list.clone()), "create"),
        (a.list_update(list), "update"),
        (a.list_delete(99), "delete"),
    ] {
        let Some(d2d::envelope::Content::DistributionListSync(s)) = wire(&envelope).content else {
            panic!("not a list sync");
        };
        let ok = match (kind, s.action) {
            ("create", Some(d2d::distribution_list_sync::Action::Create(c))) => c
                .distribution_list
                .is_some_and(|l| l.distribution_list_id == 99),
            ("update", Some(d2d::distribution_list_sync::Action::Update(c))) => {
                c.distribution_list.is_some()
            }
            ("delete", Some(d2d::distribution_list_sync::Action::Delete(c))) => {
                c.distribution_list_id == 99
            }
            _ => false,
        };
        assert!(ok, "the list {kind}");
    }
    let profile = d2d_sync::UserProfile {
        nickname: Some("nick".into()),
        ..Default::default()
    };
    let Some(d2d::envelope::Content::UserProfileSync(p)) = wire(&a.profile_update(profile)).content
    else {
        panic!("not a profile sync");
    };
    assert!(
        matches!(p.action, Some(d2d::user_profile_sync::Action::Update(u)) if u.user_profile.as_ref().is_some_and(|p| p.nickname.as_deref() == Some("nick")))
    );
    let settings = d2d_sync::Settings {
        blocked_identities: Some(common::Identities {
            identities: vec![PEER.into()],
        }),
        read_receipt_policy: Some(d2d_sync::ReadReceiptPolicy::DontSendReadReceipt as i32),
        ..Default::default()
    };
    let Some(d2d::envelope::Content::SettingsSync(s)) = wire(&a.settings_update(settings)).content
    else {
        panic!("not a settings sync");
    };
    assert!(
        matches!(s.action, Some(d2d::settings_sync::Action::Update(u)) if u.settings.as_ref().is_some_and(|s| s.blocked_identities.is_some()))
    );
    let Some(d2d::envelope::Content::MdmParameterSync(m)) =
        wire(&a.mdm_update(d2d_sync::MdmParameters::default())).content
    else {
        panic!("not an MDM sync");
    };
    assert!(matches!(
        m.action,
        Some(d2d::mdm_parameter_sync::Action::Update(_))
    ));
}

/// A kind Kourier keeps raw, a type from a newer protocol and type 0x00 carry the bytes given.
#[test]
fn raw_and_unknown_types_carry_what_they_are_given() {
    let a = author();
    let filler = body::filler(40, 7);
    assert!(filler.len() == 40 && filler[0] == 7 && filler[39] == 46);
    for t in [
        ty::UNKNOWN,
        ty::INVALID,
        ty::POLL_SETUP,
        ty::DEPRECATED_IMAGE,
    ] {
        let Some(d2d::envelope::Content::IncomingMessage(m)) =
            wire(&a.incoming(PEER, &Msg::raw(ID_A, NOW, t, filler.clone()))).content
        else {
            panic!("not an incoming message");
        };
        assert!(
            m.r#type == i32::from(t) && m.body == filler,
            "type {t:#04x}"
        );
    }
}

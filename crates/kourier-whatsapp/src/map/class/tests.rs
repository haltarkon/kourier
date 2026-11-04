//! The classification (W3b, chunk C2): every top-level field of `wa::Message` has a class.

use std::collections::BTreeSet;

use waproto::buffa::MessageField;
use waproto::whatsapp as wa;

use super::{Class, Content, FIELDS, Modifier, class_of, peel};

/// The message with only the table's field `name` set (to an empty message).
fn only(name: &str) -> wa::Message {
    let mut message = wa::Message::default();
    let field = FIELDS
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("{name} is not in the table"));
    (field.set)(&mut message);
    message
}

fn text(body: &str) -> wa::Message {
    let mut message = wa::Message::default();
    message.conversation = Some(body.to_owned());
    message
}

/// The class each field has when it is the only one set. INDEPENDENT of the table: a literal list per
/// group, so a table entry that moves to the wrong class fails here.
fn expected(name: &str) -> Class {
    const DEFERRED: &[(&str, &str)] = &[
        ("poll_update_message", "K8a"),
        ("enc_reaction_message", "K8b"),
        ("album_message", "K-B3"),
        ("pin_in_chat_message", "A4"),
        ("keep_in_chat_message", "A4"),
        ("poll_add_option_message", "K8a"),
        ("enc_event_response_message", "K8a"),
        ("enc_comment_message", "K8b"),
        ("comment_message", "K8b"),
        ("status_sticker_interaction_message", "Q1"),
        ("poll_result_snapshot_message", "K8a"),
        ("poll_result_snapshot_message_v3", "K8a"),
        // Fix F4: kinds that modify another message or are children of one wait for their owner and
        // are never bubbles of their own.
        ("secret_encrypted_message", "K8a"),
        ("question_response_message", "K8a"),
        ("poll_creation_option_image_message", "K8a"),
        ("event_cover_image", "K-B3"),
        ("message_history_bundle", "W4"),
        ("message_history_notice", "W4"),
    ];
    const CONTENT: &[(&str, Class)] = &[
        ("extended_text_message", Class::Unsupported("message")),
        ("image_message", Class::Content(Content::Image)),
        ("video_message", Class::Content(Content::Video)),
        ("ptv_message", Class::Content(Content::VideoNote)),
        ("audio_message", Class::Content(Content::Audio)),
        ("document_message", Class::Content(Content::Document)),
        ("sticker_message", Class::Content(Content::Sticker)),
        ("contact_message", Class::Content(Content::Contact)),
        ("location_message", Class::Content(Content::Location)),
        (
            "live_location_message",
            Class::Content(Content::LiveLocation),
        ),
        ("poll_creation_message", Class::Content(Content::Poll)),
        ("poll_creation_message_v2", Class::Content(Content::Poll)),
        ("poll_creation_message_v3", Class::Content(Content::Poll)),
        ("poll_creation_message_v5", Class::Content(Content::Poll)),
        ("poll_creation_message_v6", Class::Content(Content::Poll)),
        ("contacts_array_message", Class::Unsupported("contacts")),
        ("group_invite_message", Class::Unsupported("group-invite")),
    ];
    const WRAPPERS: &[&str] = &[
        "device_sent_message",
        "ephemeral_message",
        "view_once_message",
        "view_once_message_v2",
        "view_once_message_v2_extension",
        "document_with_caption_message",
        "edited_message",
        "associated_child_message",
        "lottie_sticker_message",
        "bot_invoke_message",
        "group_mentioned_message",
        "poll_creation_message_v4",
        "spoiler_message",
    ];
    if let Some((_, owner)) = DEFERRED.iter().find(|(n, _)| *n == name) {
        return Class::Deferred(owner);
    }
    if let Some((_, class)) = CONTENT.iter().find(|(n, _)| *n == name) {
        // An extended text message with no text is shown as unsupported.
        return *class;
    }
    if WRAPPERS.contains(&name) {
        return Class::Unsupported("wrapper");
    }
    match name {
        "reaction_message" => Class::Modifier(Modifier::Reaction),
        // An empty protocol message (no type, no payload) is nobody's: kept.
        "protocol_message" => Class::Deferred("protocol"),
        // Keys alone are not a message; metadata alone is nothing at all.
        "sender_key_distribution_message" => Class::Inert,
        // Finished with no event since W6a2 (D13).
        "scheduled_call_edit_message" => Class::Inert,
        "message_context_info" | "fast_ratchet_key_sender_key_distribution_message" => {
            Class::Unsupported("message")
        }
        // Every other kind is a message in the timeline that says what kind it is.
        other => Class::Unsupported(
            FIELDS
                .iter()
                .find(|f| f.name == other)
                .expect("in the table")
                .name,
        ),
    }
}

/// The proto's own list of the fields of `Message`, read from the pinned `waproto` (found through
/// `cargo metadata`, as the log module list is checked).
fn proto_fields() -> BTreeSet<String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let out = std::process::Command::new(cargo)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["metadata", "--format-version", "1", "--locked", "--offline"])
        .output()
        .expect("cargo runs");
    assert!(
        out.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).expect("metadata is JSON");
    let manifest = meta["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .find(|p| p["name"] == "waproto")
        .and_then(|p| p["manifest_path"].as_str())
        .expect("waproto is in the graph")
        .to_owned();
    let proto = std::path::Path::new(&manifest)
        .parent()
        .expect("a directory")
        .join("src/whatsapp.proto");
    let text = std::fs::read_to_string(&proto).expect("the proto of waproto is there");
    let mut lines = text.lines().skip_while(|l| *l != "message Message {");
    assert_eq!(lines.next(), Some("message Message {"));
    let mut names = BTreeSet::new();
    for line in lines {
        let line = line.trim();
        if line.starts_with("message ") || line.starts_with("enum ") || line == "}" {
            break;
        }
        // `optional <Type> <name> = <number>;`
        let mut parts = line.split_whitespace();
        if parts.next() != Some("optional") {
            continue;
        }
        let name = parts.nth(1).expect("a field name");
        let mut snake = String::new();
        for (i, c) in name.chars().enumerate() {
            if c.is_ascii_uppercase() && i > 0 {
                snake.push('_');
            }
            snake.push(c.to_ascii_lowercase());
        }
        names.insert(snake);
    }
    names
}

#[test]
fn the_table_names_every_field_of_the_proto_and_nothing_else() {
    let theirs = proto_fields();
    assert!(theirs.len() > 100, "the proto was read: {}", theirs.len());
    let mut ours: BTreeSet<String> = FIELDS.iter().map(|f| f.name.to_owned()).collect();
    assert_eq!(ours.len(), FIELDS.len(), "no field twice");
    // `conversation` is a string and is read by `class_of` itself.
    ours.insert("conversation".to_owned());
    let missing: Vec<_> = theirs.difference(&ours).collect();
    let extra: Vec<_> = ours.difference(&theirs).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "the proto has {missing:?} that the table lacks; the table has {extra:?} the proto lacks"
    );
}

#[test]
fn the_classification_table() {
    for field in FIELDS {
        let message = only(field.name);
        let class = class_of(peel(&message));
        assert_eq!(class, expected(field.name), "{}", field.name);
    }
    // The text and its edge cases.
    assert_eq!(class_of(&text("hello")), Class::Text);
    assert_eq!(class_of(&text("")), Class::Unsupported("message"));
    assert_eq!(
        class_of(&wa::Message::default()),
        Class::Unsupported("message"),
        "no field at all"
    );
    let mut extended = wa::Message::default();
    let mut ext = wa::message::ExtendedTextMessage::default();
    ext.text = Some("a link".into());
    extended.extended_text_message = MessageField::some(ext);
    assert_eq!(class_of(&extended), Class::Text);
}

#[test]
fn a_message_beside_its_metadata_is_its_content() {
    // The metadata that rides beside content decides nothing.
    let mut with_context = text("hi");
    with_context.message_context_info = MessageField::some(wa::MessageContextInfo::default());
    with_context.sender_key_distribution_message =
        MessageField::some(wa::message::SenderKeyDistributionMessage::default());
    assert_eq!(class_of(&with_context), Class::Text);
    let mut image = only("image_message");
    image.message_context_info = MessageField::some(wa::MessageContextInfo::default());
    assert_eq!(class_of(&image), Class::Content(Content::Image));
}

fn wrap(name: &str, inner: wa::Message) -> wa::Message {
    // Every wrapper holds its inner message in `message`: build the same shape for the named one.
    let mut outer = wa::Message::default();
    match name {
        "associated_child_message" => {
            let mut w = wa::message::FutureProofMessage::default();
            w.message = MessageField::some(inner);
            outer.associated_child_message = MessageField::some(w);
        }
        "ephemeral_message" => {
            let mut w = wa::message::FutureProofMessage::default();
            w.message = MessageField::some(inner);
            outer.ephemeral_message = MessageField::some(w);
        }
        "view_once_message_v2" => {
            let mut w = wa::message::FutureProofMessage::default();
            w.message = MessageField::some(inner);
            outer.view_once_message_v2 = MessageField::some(w);
        }
        "device_sent_message" => {
            let mut w = wa::message::DeviceSentMessage::default();
            w.message = MessageField::some(inner);
            outer.device_sent_message = MessageField::some(w);
        }
        "group_mentioned_message" => {
            let mut w = wa::message::FutureProofMessage::default();
            w.message = MessageField::some(inner);
            outer.group_mentioned_message = MessageField::some(w);
        }
        "poll_creation_message_v4" => {
            let mut w = wa::message::FutureProofMessage::default();
            w.message = MessageField::some(inner);
            outer.poll_creation_message_v4 = MessageField::some(w);
        }
        other => panic!("no wrapper builder for {other}"),
    }
    outer
}

/// Critic H2: the children of an album and the other wrappers are peeled, so they are what they hold.
#[test]
fn the_wrappers_are_peeled_before_the_class_is_read() {
    // An album child is the photo it holds, not an unsupported message (mutant: no peel).
    let child = wrap("associated_child_message", only("image_message"));
    assert_eq!(class_of(peel(&child)), Class::Content(Content::Image));
    // Wrappers nest in any order: device-sent -> ephemeral -> view-once v2 -> a group mention -> text.
    let nested = wrap(
        "device_sent_message",
        wrap(
            "ephemeral_message",
            wrap(
                "view_once_message_v2",
                wrap("group_mentioned_message", text("deep")),
            ),
        ),
    );
    assert_eq!(class_of(peel(&nested)), Class::Text);
    assert_eq!(peel(&nested).conversation.as_deref(), Some("deep"));
    // A poll creation v4 holds a poll creation.
    let v4 = wrap("poll_creation_message_v4", only("poll_creation_message"));
    assert_eq!(class_of(peel(&v4)), Class::Content(Content::Poll));
    // A message with no wrapper is itself.
    let plain = text("plain");
    assert!(std::ptr::eq(peel(&plain), &plain));
    // A nesting past the cap ends as a wrapper, never as a loop or a panic.
    let mut deep = text("bottom");
    for _ in 0..(super::MAX_PEEL + 4) {
        deep = wrap("ephemeral_message", deep);
    }
    assert_eq!(class_of(peel(&deep)), Class::Unsupported("wrapper"));
}

fn protocol_of(fill: impl FnOnce(&mut wa::message::ProtocolMessage)) -> wa::Message {
    let mut inner = wa::message::ProtocolMessage::default();
    fill(&mut inner);
    let mut message = wa::Message::default();
    message.protocol_message = MessageField::some(inner);
    message
}

#[test]
fn a_protocol_message_is_classified_by_its_type_and_payload() {
    use wa::message::protocol_message::Type;
    let typed = |t: Type| protocol_of(|p| p.r#type = Some(t));
    // Mapped from chunk 5 on.
    assert_eq!(
        class_of(&typed(Type::REVOKE)),
        Class::Modifier(Modifier::Revoke)
    );
    assert_eq!(
        class_of(&typed(Type::MESSAGE_EDIT)),
        Class::Modifier(Modifier::Edit)
    );
    assert_eq!(
        class_of(&typed(Type::EPHEMERAL_SETTING)),
        Class::Modifier(Modifier::TimerSetting)
    );
    // Upstream consumed these itself (D13): finished with no event since W6a2. The third is our phone's LID-migration push:
    // its pairs are in the store (`migration_sync_latest`) and the merge sweep reads them there.
    for t in [
        Type::EPHEMERAL_SYNC_RESPONSE,
        Type::SHARE_PHONE_NUMBER,
        Type::LID_MIGRATION_MAPPING_SYNC,
    ] {
        assert_eq!(class_of(&typed(t)), Class::Inert);
    }
    // Plumbing upstream consumes itself: no message, by type or by payload.
    for t in [
        Type::PEER_DATA_OPERATION_REQUEST_MESSAGE,
        Type::PEER_DATA_OPERATION_REQUEST_RESPONSE_MESSAGE,
    ] {
        assert_eq!(class_of(&typed(t)), Class::Inert);
    }
    // The phone's history-sync notice is its own class: by the type alone (upstream takes the field out
    // before the hook and leaves the type) and by the payload alone.
    assert_eq!(
        class_of(&typed(Type::HISTORY_SYNC_NOTIFICATION)),
        Class::HistoryNotice
    );
    assert_eq!(
        class_of(&protocol_of(|p| {
            p.history_sync_notification =
                MessageField::some(wa::message::HistorySyncNotification::default());
        })),
        Class::HistoryNotice
    );
    // Any other type nobody maps is kept, never wiped.
    assert_eq!(
        class_of(&typed(Type::BOT_FEEDBACK_MESSAGE)),
        Class::Deferred("protocol")
    );
    assert_eq!(class_of(&protocol_of(|_| {})), Class::Deferred("protocol"));
}

/// Fix F7: the five association types of a companion upload (the same media again at another quality, a
/// motion photo's video, a sticker's annotation) are told from an album child, and from a message with no
/// association; the association may ride on the outer message or on the peeled one.
#[test]
fn a_companion_upload_is_told_from_an_album_child() {
    use wa::message_association::AssociationType as Type;
    let with = |kind: Option<Type>| {
        let mut message = wa::Message::default();
        let mut context = wa::MessageContextInfo::default();
        let mut association = wa::MessageAssociation::default();
        association.association_type = kind;
        context.message_association = MessageField::some(association);
        message.message_context_info = MessageField::some(context);
        message
    };
    for kind in [
        Type::HD_IMAGE_DUAL_UPLOAD,
        Type::HD_VIDEO_DUAL_UPLOAD,
        Type::HEVC_VIDEO_DUAL_UPLOAD,
        Type::MOTION_PHOTO,
        Type::STICKER_ANNOTATION,
    ] {
        let message = with(Some(kind));
        assert!(super::is_companion(&message, &message), "{kind:?}");
        // On the peeled message only.
        assert!(
            super::is_companion(&wa::Message::default(), &message),
            "{kind:?} on the inner message"
        );
    }
    for kind in [
        None,
        Some(Type::UNKNOWN),
        Some(Type::MEDIA_ALBUM),
        Some(Type::EVENT_COVER_IMAGE),
        Some(Type::POLL_ADD_OPTION),
        Some(Type::STATUS_POLL),
    ] {
        let message = with(kind);
        assert!(!super::is_companion(&message, &message), "{kind:?}");
    }
    let plain = wa::Message::default();
    assert!(!super::is_companion(&plain, &plain));
}

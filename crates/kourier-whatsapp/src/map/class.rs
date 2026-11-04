//! What a stored message IS: ONE table over every top-level field of `wa::Message` (W3b, chunk C2).
//!
//! [`class_of`] reads the table in order and the first field that is set decides the [`Class`]; the
//! order is the priority (a protocol message and a reaction first, then the kinds a later step maps,
//! then content, then everything else). The wrappers upstream and the phone put around a message
//! (device-sent, ephemeral, the three view-once forms, a document with a caption, an edit, an album
//! child, a sticker, a bot invocation, a group mention, a poll creation v4, a spoiler) are peeled by
//! [`peel`] from the same table before the class is read, so an album child of two photos sent at once
//! is a photo, not an "unsupported message" (critic H2).
//!
//! A test reads the proto of the pinned `waproto` and fails when a field is in one and not the other,
//! so a pin bump that adds a field stops here until the field is classified; a field the table does not
//! name never silently becomes content. Pure: no I/O, no clock, no logging.

use waproto::buffa::MessageField;
use waproto::whatsapp as wa;

/// What a stored message is, for the map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// `conversation` or `extendedTextMessage`: text, with formatting and mentions.
    Text,
    /// Content with a body of its own.
    Content(Content),
    /// Changes or reports on another message or on the chat.
    Modifier(Modifier),
    /// A kind a later step maps: the row is kept WITH its content (the step is named). A kind that
    /// modifies another message is deferred, never shown as a bubble (critic M6).
    Deferred(&'static str),
    /// Provably no message: finished and wiped (W3a's list plus the PDO request and response).
    Inert,
    /// The phone's notice that a chunk of the message history is on its way (`historySyncNotification`;
    /// upstream takes the field out and leaves the protocol message's type). No message either, but
    /// the row is what W4 records as an ANNOUNCED chunk when it is our own phone's: the map says
    /// `announced`, not `inert` ([`super::Mapped::announced`]).
    HistoryNotice,
    /// A message in the timeline that says "unsupported", naming the kind (log-safe, static).
    Unsupported(&'static str),
}

/// The content kinds with a body of their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Content {
    Image,
    Video,
    /// A round video note (`ptvMessage`): a video.
    VideoNote,
    /// A voice message (`ptt`) or an audio file.
    Audio,
    Document,
    Sticker,
    Contact,
    Location,
    LiveLocation,
    /// `pollCreationMessage` v1 to v3 and v4 to v6 (the v4 wrapper is peeled).
    Poll,
}

impl Content {
    /// The short name `Body::Unsupported` carries while a kind is not mapped.
    pub(crate) fn hint(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video | Self::VideoNote => "video",
            Self::Audio => "audio",
            Self::Document => "document",
            Self::Sticker => "sticker",
            Self::Contact => "contact",
            Self::Location | Self::LiveLocation => "location",
            Self::Poll => "poll",
        }
    }
}

/// What a modifier does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Modifier {
    Reaction,
    /// A protocol message of type `MESSAGE_EDIT`.
    Edit,
    /// A protocol message of type `REVOKE` (delete for everyone).
    Revoke,
    /// A protocol message of type `EPHEMERAL_SETTING` (the chat's timer).
    TimerSetting,
}

/// What a table entry is.
#[derive(Clone, Copy)]
enum Role {
    /// Not content: metadata that rides beside content.
    Meta,
    /// A wrapper around another message; the function reads the inner one.
    Peel(fn(&wa::Message) -> Option<&wa::Message>),
    /// The protocol message: its class depends on its type and payload ([`protocol_class`]).
    Protocol,
    /// A kind with a fixed class.
    Kind(Class),
}

struct Field {
    /// The proto field in snake case, as the generated struct names it (the tests compare it with the
    /// proto).
    #[cfg(test)]
    name: &'static str,
    present: fn(&wa::Message) -> bool,
    /// Sets the field to an empty message (the table test builds one message per field).
    #[cfg(test)]
    set: fn(&mut wa::Message),
    role: Role,
}

macro_rules! entry {
    ($name:ident, $role:expr) => {
        Field {
            #[cfg(test)]
            name: stringify!($name),
            present: |m| m.$name.as_option().is_some(),
            #[cfg(test)]
            set: |m| m.$name = MessageField::some(Default::default()),
            role: $role,
        }
    };
}
macro_rules! kind {
    ($name:ident, $class:expr) => {
        entry!($name, Role::Kind($class))
    };
}
macro_rules! other {
    ($name:ident) => {
        entry!($name, Role::Kind(Class::Unsupported(stringify!($name))))
    };
}
macro_rules! meta {
    ($name:ident) => {
        entry!($name, Role::Meta)
    };
}
macro_rules! protocol {
    ($name:ident) => {
        entry!($name, Role::Protocol)
    };
}
macro_rules! peel {
    ($name:ident) => {
        Field {
            #[cfg(test)]
            name: stringify!($name),
            present: |m| m.$name.as_option().is_some(),
            #[cfg(test)]
            set: |m| m.$name = MessageField::some(Default::default()),
            role: Role::Peel(|m| m.$name.as_option().and_then(|w| w.message.as_option())),
        }
    };
}

/// Every top-level field of `wa::Message` except `conversation` (a string, handled by [`class_of`]
/// itself), in priority order.
static FIELDS: &[Field] = &[
    protocol!(protocol_message),
    kind!(reaction_message, Class::Modifier(Modifier::Reaction)),
    kind!(poll_update_message, Class::Deferred("K8a")),
    kind!(enc_reaction_message, Class::Deferred("K8b")),
    kind!(album_message, Class::Deferred("K-B3")),
    kind!(pin_in_chat_message, Class::Deferred("A4")),
    kind!(keep_in_chat_message, Class::Deferred("A4")),
    kind!(poll_add_option_message, Class::Deferred("K8a")),
    kind!(enc_event_response_message, Class::Deferred("K8a")),
    kind!(enc_comment_message, Class::Deferred("K8b")),
    kind!(comment_message, Class::Deferred("K8b")),
    kind!(status_sticker_interaction_message, Class::Deferred("Q1")),
    kind!(poll_result_snapshot_message, Class::Deferred("K8a")),
    kind!(poll_result_snapshot_message_v3, Class::Deferred("K8a")),
    kind!(extended_text_message, Class::Text),
    kind!(image_message, Class::Content(Content::Image)),
    kind!(video_message, Class::Content(Content::Video)),
    kind!(ptv_message, Class::Content(Content::VideoNote)),
    kind!(audio_message, Class::Content(Content::Audio)),
    kind!(document_message, Class::Content(Content::Document)),
    kind!(sticker_message, Class::Content(Content::Sticker)),
    kind!(contact_message, Class::Content(Content::Contact)),
    kind!(location_message, Class::Content(Content::Location)),
    kind!(live_location_message, Class::Content(Content::LiveLocation)),
    kind!(poll_creation_message, Class::Content(Content::Poll)),
    kind!(poll_creation_message_v2, Class::Content(Content::Poll)),
    kind!(poll_creation_message_v3, Class::Content(Content::Poll)),
    kind!(poll_creation_message_v5, Class::Content(Content::Poll)),
    kind!(poll_creation_message_v6, Class::Content(Content::Poll)),
    kind!(contacts_array_message, Class::Unsupported("contacts")),
    kind!(group_invite_message, Class::Unsupported("group-invite")),
    other!(call),
    other!(chat),
    other!(highly_structured_message),
    other!(send_payment_message),
    other!(request_payment_message),
    other!(decline_payment_request_message),
    other!(cancel_payment_request_message),
    other!(template_message),
    other!(template_button_reply_message),
    other!(product_message),
    other!(list_message),
    other!(order_message),
    other!(list_response_message),
    other!(invoice_message),
    other!(buttons_message),
    other!(buttons_response_message),
    other!(payment_invite_message),
    other!(interactive_message),
    other!(sticker_sync_rmr_message),
    other!(interactive_response_message),
    other!(request_phone_number_message),
    other!(scheduled_call_creation_message),
    // Cancels or changes a scheduled call (a modifier of another message): finished with no event since W6a2 (D13): the
    // call log of the phone (W6a1) says what became of a call, and a scheduled call has no line of its own.
    kind!(scheduled_call_edit_message, Class::Inert),
    other!(call_log_messsage),
    // The group history a member shares with the people he adds: W4's, never a bubble (fix F4).
    kind!(message_history_bundle, Class::Deferred("W4")),
    other!(bcall_message),
    other!(event_message),
    other!(newsletter_admin_invite_message),
    other!(placeholder_message),
    // The encrypted form of a poll option added, a poll or an event edited, a message edited: it modifies
    // another message and only the step that owns that message can read it (fix F4).
    kind!(secret_encrypted_message, Class::Deferred("K8a")),
    // A child of an event, not a message of its own (fix F4).
    kind!(event_cover_image, Class::Deferred("K-B3")),
    other!(sticker_pack_message),
    other!(status_mention_message),
    // A child of a poll, not a message of its own (fix F4).
    kind!(poll_creation_option_image_message, Class::Deferred("K8a")),
    other!(group_status_mention_message),
    other!(status_add_yours),
    other!(group_status_message),
    other!(rich_response_message),
    other!(status_notification_message),
    other!(limit_sharing_message),
    other!(bot_task_message),
    other!(question_message),
    // The notice that a member shares the group's history with the people he adds: W4's, like the bundle
    // it announces, never a bubble.
    kind!(message_history_notice, Class::Deferred("W4")),
    other!(group_status_message_v2),
    other!(bot_forwarded_message),
    other!(status_question_answer_message),
    other!(question_reply_message),
    // Answers a question message, in the shape of a reaction (fix F4).
    kind!(question_response_message, Class::Deferred("K8a")),
    other!(status_quoted_message),
    other!(newsletter_follower_invite_message_v2),
    other!(newsletter_admin_profile_message),
    other!(conditional_reveal_message),
    other!(event_invite_message),
    other!(group_root_key_share),
    other!(payment_reminder_message),
    other!(split_payment_message),
    other!(newsletter_admin_profile_status_message),
    other!(root_secret_distribute_message),
    other!(split_payment_update_message),
    other!(music_message),
    other!(status_link_preview_metadata),
    other!(bot_platform_registration_success_message),
    other!(newsletter_scheduled_message),
    other!(acp2_setting_message),
    other!(audio_sticker_message),
    peel!(device_sent_message),
    peel!(ephemeral_message),
    peel!(view_once_message),
    peel!(view_once_message_v2),
    peel!(view_once_message_v2_extension),
    peel!(document_with_caption_message),
    peel!(edited_message),
    peel!(associated_child_message),
    peel!(lottie_sticker_message),
    peel!(bot_invoke_message),
    peel!(group_mentioned_message),
    peel!(poll_creation_message_v4),
    peel!(spoiler_message),
    meta!(sender_key_distribution_message),
    meta!(fast_ratchet_key_sender_key_distribution_message),
    meta!(message_context_info),
];

/// The most wrappers one message is peeled through (a hostile nesting ends here, as a wrapper).
const MAX_PEEL: usize = 8;

/// The message inside its wrappers (the message itself when it has none).
pub(crate) fn peel(message: &wa::Message) -> &wa::Message {
    let mut current = message;
    for _ in 0..MAX_PEEL {
        let inner = FIELDS.iter().find_map(|field| match field.role {
            Role::Peel(inner) => inner(current),
            _ => None,
        });
        match inner {
            Some(inner) => current = inner,
            None => break,
        }
    }
    current
}

/// Whether `message` reaches its content through a `documentWithCaptionMessage` (fix F11): the phone sent a document
/// with a caption, and a caption edit must write the shape it sent. Follows exactly the steps [`peel`] takes.
pub(crate) fn through_caption_wrapper(message: &wa::Message) -> bool {
    let mut current = message;
    for _ in 0..MAX_PEEL {
        let step = FIELDS.iter().find_map(|field| match field.role {
            Role::Peel(inner) => inner(current),
            _ => None,
        });
        let Some(inner) = step else {
            return false;
        };
        let wrapped = current
            .document_with_caption_message
            .as_option()
            .and_then(|wrapper| wrapper.message.as_option());
        if wrapped.is_some_and(|wrapped| std::ptr::eq(wrapped, inner)) {
            return true;
        }
        current = inner;
    }
    false
}

/// Classifies a PEELED message.
pub(crate) fn class_of(base: &wa::Message) -> Class {
    // A sender-key distribution and NOTHING else (W3a fix N1) is the keys of a group message that
    // came on its own: not a message. Read off the ENCODED length of the rest, not off `==`: buffa's
    // `MessageField` equality calls a field set to an empty message equal to an unset one, so `==`
    // would take a pin in chat (or any kind) that arrives with no fields of its own for nothing.
    if is_keys_only(base) {
        return Class::Inert;
    }
    match base.conversation.as_deref() {
        Some(text) if !text.is_empty() => return Class::Text,
        _ => {}
    }
    for field in FIELDS {
        if !(field.present)(base) {
            continue;
        }
        match field.role {
            Role::Meta | Role::Peel(_) => {}
            Role::Protocol => {
                if let Some(protocol) = base.protocol_message.as_option() {
                    return protocol_class(protocol);
                }
            }
            Role::Kind(Class::Text) => {
                // A text message with no text (a link preview with nothing in it) shows as unsupported.
                let text = base
                    .extended_text_message
                    .as_option()
                    .and_then(|m| m.text.as_deref());
                if text.is_some_and(|t| !t.is_empty()) {
                    return Class::Text;
                }
            }
            Role::Kind(class) => return class,
        }
    }
    // Only wrappers that hold nothing (or a nesting past `MAX_PEEL`), or no field at all.
    if FIELDS
        .iter()
        .any(|f| matches!(f.role, Role::Peel(_)) && (f.present)(base))
    {
        Class::Unsupported("wrapper")
    } else {
        Class::Unsupported("message")
    }
}

/// Whether the message is a COMPANION upload of another message: the same media sent again at another
/// quality or as another form (`messageAssociation`: HD image or video dual upload, HEVC video, the video
/// of a motion photo, a sticker's annotation). It is a part of its parent, not a message: shown as a second
/// bubble it doubles the picture or the video. Kept for K-B3, which attaches it to its parent (fix F7).
/// The association is read off the outer message's context info, else the peeled one's, as
/// `body::kept_context` does.
pub(crate) fn is_companion(outer: &wa::Message, base: &wa::Message) -> bool {
    use wa::message_association::AssociationType as Type;
    let Some(kind) = outer
        .message_context_info
        .as_option()
        .or_else(|| base.message_context_info.as_option())
        .and_then(|context| context.message_association.as_option())
        .and_then(|association| association.association_type)
    else {
        return false;
    };
    matches!(
        kind,
        Type::HD_IMAGE_DUAL_UPLOAD
            | Type::HD_VIDEO_DUAL_UPLOAD
            | Type::HEVC_VIDEO_DUAL_UPLOAD
            | Type::MOTION_PHOTO
            | Type::STICKER_ANNOTATION
    )
}

/// A sender-key distribution and nothing else (the message's `message_context_info` is metadata, not
/// content, and does not count).
fn is_keys_only(base: &wa::Message) -> bool {
    if base.sender_key_distribution_message.as_option().is_none() {
        return false;
    }
    let mut rest = base.clone();
    rest.sender_key_distribution_message = MessageField::none();
    rest.message_context_info = MessageField::none();
    waproto::codec::message_encoded_len(&rest) == 0
}

/// The class of a protocol message. Inert: the plumbing upstream consumes itself (app-state key shares
/// and requests, the fatal-exception notice, the peer-data-operation request and response: upstream
/// answers them in `receive.rs`, and the messages a recovery brings leave as events of their own,
/// never through the durability hook; chunk 1 scout S3). A history-sync notice is its own class, told
/// by the field OR by the type (upstream takes the field before the hook and the type stays: W4's
/// scout, C1). Mapped: a revoke, an edit and the chat's timer setting. Everything else is deferred,
/// never lost (F3).
fn protocol_class(protocol: &wa::message::ProtocolMessage) -> Class {
    use wa::message::protocol_message::Type;
    if protocol.history_sync_notification.as_option().is_some()
        || protocol.r#type == Some(Type::HISTORY_SYNC_NOTIFICATION)
    {
        return Class::HistoryNotice;
    }
    let plumbing = protocol.app_state_sync_key_share.as_option().is_some()
        || protocol.app_state_sync_key_request.as_option().is_some()
        || protocol
            .app_state_fatal_exception_notification
            .as_option()
            .is_some()
        || protocol
            .peer_data_operation_request_message
            .as_option()
            .is_some()
        || protocol
            .peer_data_operation_request_response_message
            .as_option()
            .is_some()
        || protocol.r#type == Some(Type::PEER_DATA_OPERATION_REQUEST_MESSAGE)
        || protocol.r#type == Some(Type::PEER_DATA_OPERATION_REQUEST_RESPONSE_MESSAGE);
    if plumbing {
        return Class::Inert;
    }
    let kind = protocol.r#type;
    if kind == Some(Type::REVOKE) {
        Class::Modifier(Modifier::Revoke)
    } else if kind == Some(Type::MESSAGE_EDIT) {
        Class::Modifier(Modifier::Edit)
    } else if kind == Some(Type::EPHEMERAL_SETTING) {
        Class::Modifier(Modifier::TimerSetting)
    } else if kind == Some(Type::EPHEMERAL_SYNC_RESPONSE)
        || kind == Some(Type::SHARE_PHONE_NUMBER)
        || kind == Some(Type::LID_MIGRATION_MAPPING_SYNC)
    {
        // Upstream consumed all three itself (D13); nothing of them is Kourier's: finished with no event since W6a2.
        // The third is our phone's LID-migration push, a vouched source of merges (D7): upstream stores its pairs in
        // `lid_pn_mapping` as `migration_sync_latest`, and the merge sweep reads them there (`crate::merges`), never from
        // this row, whose content the finish wipes.
        Class::Inert
    } else {
        Class::Deferred("protocol")
    }
}

#[cfg(test)]
mod tests;

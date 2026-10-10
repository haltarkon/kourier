//! Every enum of the contract round-trips through serde by variant *name* (§3: never by
//! index). Each list is guarded by an exhaustive `match`, so a new variant fails to compile
//! here until it gets a sample.

use std::fmt::Debug;
use std::path::PathBuf;
use std::time::Duration;

use kourier_model::*;
use serde::Serialize;
use serde::de::DeserializeOwned;

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("valid user id")
}

fn target() -> MsgTarget {
    MsgTarget {
        conv: Some(ConvKey::Group("gv2:AbC".into())),
        author: TargetAuthor::User(uid("aci:a")),
        local: "1700000000000".into(),
        alternates: vec!["1700000000001".into()],
    }
}

fn error() -> BackendError {
    BackendError::transient("timeout").with_user_message("Try again")
}

fn blob_ref() -> BlobRef {
    BlobRef {
        path: PathBuf::from("ab/abcdef"),
        key: BlobKey::new([9; 32]),
    }
}

fn text() -> Text {
    Text {
        body: format!("hi {}", text::MENTION_PLACEHOLDER),
        ranges: vec![
            TextRange {
                start: 0,
                len: 2,
                kind: RangeKind::Bold,
            },
            TextRange {
                start: 3,
                len: 3,
                kind: RangeKind::Mention(uid("aci:b")),
            },
        ],
    }
}

fn attachment() -> Attachment {
    Attachment {
        id: Some(AttachmentId(4)),
        kind: AttachmentKind::Voice,
        mime: "audio/aac".into(),
        size: 1234,
        file_name: Some("note.aac".into()),
        dims: Some((10, 20)),
        duration_ms: Some(5000),
        blurhash: Some("LEHV6nWB2yk8".into()),
        waveform: Some(vec![0, 128, 255]),
        caption: Some(text()),
        state: AttachmentState::Downloading {
            done: 10,
            total: 20,
        },
        remote: Some(BackendBlob::new(1, vec![1, 2, 3])),
        local: Some(blob_ref()),
        thumb: Some(blob_ref()),
        long_text: true,
        net_ids: vec!["uuid:5f0c".into(), "digest:q83v".into()],
    }
}

/// Serializes `v`, checks the variant name is the tag, deserializes and compares.
fn check<T: Serialize + DeserializeOwned + PartialEq + Debug>(name: &str, v: &T) {
    let json = serde_json::to_string(v).expect("serialize");
    assert!(
        json == format!("\"{name}\"") || json.starts_with(&format!("{{\"{name}\":")),
        "{name} is not tagged by name: {json}"
    );
    let back: T = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(&back, v, "{json}");
}

macro_rules! roundtrip_all {
    ($ty:ty { $($variant:ident => $value:expr),+ $(,)? }) => {{
        type T = $ty;
        #[allow(dead_code, unreachable_patterns)]
        fn exhaustive(v: &T) {
            match v {
                $(T::$variant { .. } => {})+
            }
        }
        $(check(stringify!($variant), &$value);)+
    }};
}

#[test]
fn id_enums() {
    roundtrip_all!(NetworkKind {
        Signal => NetworkKind::Signal,
        WhatsApp => NetworkKind::WhatsApp,
        Threema => NetworkKind::Threema,
        Fake => NetworkKind::Fake,
    });
    roundtrip_all!(MsgIdScope {
        AccountAuthor => MsgIdScope::AccountAuthor,
        Conversation => MsgIdScope::Conversation,
    });
    roundtrip_all!(TargetAuthor {
        Own => TargetAuthor::Own,
        User => TargetAuthor::User(uid("pni:x")),
        Unknown => TargetAuthor::Unknown,
    });
    roundtrip_all!(IdentifierKind {
        Phone => IdentifierKind::Phone,
        Username => IdentifierKind::Username,
        Email => IdentifierKind::Email,
        NetworkId => IdentifierKind::NetworkId,
    });
    // ConvKey is stored by its string grammar instead of a serde tag.
    for k in [
        ConvKey::Direct(uid("aci:a")),
        ConvKey::Group("gv2:g".into()),
        ConvKey::Broadcast("threema-list:1".into()),
        ConvKey::Channel("newsletter:1".into()),
    ] {
        let json = serde_json::to_string(&k).expect("serialize");
        assert_eq!(json, format!("\"{}\"", k.encode()));
        assert_eq!(serde_json::from_str::<ConvKey>(&json).ok(), Some(k));
    }
}

#[test]
fn text_and_feature_enums() {
    roundtrip_all!(RangeKind {
        Bold => RangeKind::Bold,
        Italic => RangeKind::Italic,
        Strike => RangeKind::Strike,
        Spoiler => RangeKind::Spoiler,
        Mono => RangeKind::Mono,
        Mention => RangeKind::Mention(uid("aci:a")),
        MentionAll => RangeKind::MentionAll,
        Link => RangeKind::Link("https://example.org".into()),
    });
    roundtrip_all!(Support {
        Rejected => Support::Rejected,
        Dropped => Support::Dropped,
        Unsupported => Support::Unsupported,
        Partial => Support::Partial,
        Full => Support::Full,
    });
    roundtrip_all!(FormatKind {
        Bold => FormatKind::Bold,
        Italic => FormatKind::Italic,
        Strike => FormatKind::Strike,
        Spoiler => FormatKind::Spoiler,
        Mono => FormatKind::Mono,
        Mention => FormatKind::Mention,
        MentionAll => FormatKind::MentionAll,
        Link => FormatKind::Link,
    });
    roundtrip_all!(HistorySource {
        None => HistorySource::None,
        PushedOnLink => HistorySource::PushedOnLink,
        OnDemand => HistorySource::OnDemand,
    });
    roundtrip_all!(CannotSend {
        NotMember => CannotSend::NotMember,
        AdminsOnly => CannotSend::AdminsOnly,
        Blocked => CannotSend::Blocked,
        RequestPending => CannotSend::RequestPending,
        Unregistered => CannotSend::Unregistered,
        NotAccepted => CannotSend::NotAccepted,
        Other => CannotSend::Other("Group is full".into()),
    });
}

#[test]
fn content_enums() {
    roundtrip_all!(Origin {
        Remote => Origin::Remote { author: uid("aci:a"), device: Some(2) },
        OwnThisDevice => Origin::OwnThisDevice,
        OwnOtherDevice => Origin::OwnOtherDevice { device: None },
    });
    roundtrip_all!(Body {
        Text => Body::Text(text()),
        Media => Body::Media { attachments: vec![attachment(), Attachment::default()], caption: Some(text()) },
        Sticker => Body::Sticker(Box::new(attachment())),
        Contact => Body::Contact(ContactCard {
            name: Some("Bob".into()),
            organization: None,
            identifiers: vec![Identifier { kind: IdentifierKind::Phone, value: "+491".into() }],
            avatar: Some(blob_ref()),
        }),
        Location => Body::Location(Location { lat_e7: -1, lon_e7: 2, name: None, address: Some("x".into()) }),
        Poll => Body::Poll(Poll { question: "q".into(), options: vec!["a".into()], multiple: true }),
        System => Body::System(SystemEvent::Joined),
        Placeholder => Body::Placeholder(Placeholder::Expired),
        Unsupported => Body::Unsupported { kind_hint: "payment".into(), summary: None },
    });
    roundtrip_all!(AttachmentKind {
        Image => AttachmentKind::Image,
        Video => AttachmentKind::Video,
        Audio => AttachmentKind::Audio,
        Voice => AttachmentKind::Voice,
        Gif => AttachmentKind::Gif,
        File => AttachmentKind::File,
    });
    roundtrip_all!(AttachmentState {
        Remote => AttachmentState::Remote,
        Downloading => AttachmentState::Downloading { done: 1, total: 2 },
        Local => AttachmentState::Local,
        Failed => AttachmentState::Failed { retryable: true },
        Expired => AttachmentState::Expired,
    });
    let by = Some(uid("aci:admin"));
    roundtrip_all!(SystemEvent {
        MembersAdded => SystemEvent::MembersAdded { by: by.clone(), users: vec![uid("aci:a")] },
        MembersRemoved => SystemEvent::MembersRemoved { by: None, users: vec![uid("aci:a")] },
        MemberLeft => SystemEvent::MemberLeft { user: uid("aci:a") },
        TitleChanged => SystemEvent::TitleChanged { by: by.clone(), title: "T".into() },
        AvatarChanged => SystemEvent::AvatarChanged { by: by.clone() },
        DescriptionChanged => SystemEvent::DescriptionChanged { by: None },
        TimerChanged => SystemEvent::TimerChanged { by: by.clone(), timer: Some(Duration::from_secs(60)) },
        IdentityKeyChanged => SystemEvent::IdentityKeyChanged { user: uid("aci:a") },
        SessionRefreshed => SystemEvent::SessionRefreshed { user: uid("aci:a") },
        GroupCreated => SystemEvent::GroupCreated { by },
        MessageRequestAccepted => SystemEvent::MessageRequestAccepted,
        Call => SystemEvent::Call(
            CallLine::signal(CallDirection::Incoming, false, CallState::Ringing { deadline: 9 }, 3)
                .with_media(CallMedia::Video, 3)
                .with_by(uid("aci:a"), 3),
        ),
        Joined => SystemEvent::Joined,
        NumberChanged => SystemEvent::NumberChanged { user: uid("aci:a") },
    });
    roundtrip_all!(Placeholder {
        Undecryptable => Placeholder::Undecryptable { resendable: true },
        ViewOnceOpened => Placeholder::ViewOnceOpened,
        Expired => Placeholder::Expired,
    });
    roundtrip_all!(Deletion {
        ForEveryone => Deletion::ForEveryone { by: Some(uid("aci:a")) },
        ForMe => Deletion::ForMe,
    });
    roundtrip_all!(DisappearKind {
        AfterRead => DisappearKind::AfterRead,
        AfterSend => DisappearKind::AfterSend,
    });
}

#[test]
fn state_and_error_enums() {
    roundtrip_all!(OutgoingState {
        Queued => OutgoingState::Queued,
        Sending => OutgoingState::Sending { progress: Some(0.5) },
        Sent => OutgoingState::Sent { server_at: Some(17) },
        Delivered => OutgoingState::Delivered,
        Read => OutgoingState::Read,
        Viewed => OutgoingState::Viewed,
        Failed => OutgoingState::Failed { reason: FailReason::Network, retryable: true },
        PartiallyFailed => OutgoingState::PartiallyFailed { failed: vec![uid("aci:a")] },
    });
    roundtrip_all!(FailReason {
        Network => FailReason::Network,
        RateLimited => FailReason::RateLimited,
        Challenge => FailReason::Challenge,
        Unregistered => FailReason::Unregistered,
        UntrustedIdentity => FailReason::UntrustedIdentity,
        TooLarge => FailReason::TooLarge,
        Unsupported => FailReason::Unsupported,
        LoggedOut => FailReason::LoggedOut,
        Other => FailReason::Other("x".into()),
    });
    roundtrip_all!(ReadState {
        Unread => ReadState::Unread,
        Read => ReadState::Read,
        Viewed => ReadState::Viewed,
    });
    roundtrip_all!(ReceiptKind {
        Delivered => ReceiptKind::Delivered,
        Read => ReceiptKind::Read,
        Viewed => ReceiptKind::Viewed,
    });
    roundtrip_all!(RecipientState {
        Pending => RecipientState::Pending,
        Sent => RecipientState::Sent { sealed: true },
        Failed => RecipientState::Failed { retryable: false },
    });
    let retry = Some(Duration::from_millis(1500));
    roundtrip_all!(ErrorKind {
        Transient => ErrorKind::Transient { retry_after: retry },
        RateLimited => ErrorKind::RateLimited { retry_after: None },
        Challenge => ErrorKind::Challenge { token: Redacted::new("t".into()), url: "u".into(), retry_after: retry },
        Unauthorized => ErrorKind::Unauthorized,
        LoggedOut => ErrorKind::LoggedOut,
        PeerUnregistered => ErrorKind::PeerUnregistered,
        UntrustedIdentity => ErrorKind::UntrustedIdentity { user: uid("aci:a") },
        TooLarge => ErrorKind::TooLarge,
        InvalidInput => ErrorKind::InvalidInput,
        Unsupported => ErrorKind::Unsupported,
        Expired => ErrorKind::Expired,
        Internal => ErrorKind::Internal,
    });
}

#[test]
fn account_conn_login_enums() {
    roundtrip_all!(ConnState {
        Offline => ConnState::Offline { retry_at_ms: Some(5) },
        Connecting => ConnState::Connecting,
        Syncing => ConnState::Syncing { fraction: Some(0.25) },
        Online => ConnState::Online { detail: Some("leader".into()) },
        NeedsUserAction => ConnState::NeedsUserAction(UserAction::Relink),
        Restricted => ConnState::Restricted { until_ms: None, reason: "temporary ban".into() },
        LoggedOut => ConnState::LoggedOut { reason: "unlinked".into() },
    });
    roundtrip_all!(UserAction {
        Relink => UserAction::Relink,
        UpdateApp => UserAction::UpdateApp,
        SolveChallenge => UserAction::SolveChallenge { url: "signalcaptcha://".into() },
        FixClock => UserAction::FixClock { skew_ms: -3000 },
        Reconnect => UserAction::Reconnect,
    });
    roundtrip_all!(LoginStepKind {
        ShowQr => LoginStepKind::ShowQr { data: Redacted::new("sgnl://x".into()), valid_for: Some(Duration::from_secs(45)) },
        ShowCode => LoginStepKind::ShowCode { code: Redacted::new("ABCD-1234".into()) },
        ConfirmEmoji => LoginStepKind::ConfirmEmoji { emoji: vec!["🐱".into()] },
        Input => LoginStepKind::Input { fields: vec![InputField {
            id: "device-name".into(),
            label: "Device name".into(),
            kind: InputKind::Text,
            default: Some("host".into()),
            required: true,
        }] },
        Progress => LoginStepKind::Progress { label: "Linking".into(), fraction: None },
        Done => LoginStepKind::Done,
        Failed => LoginStepKind::Failed { error: error() },
    });
    roundtrip_all!(InputKind {
        Text => InputKind::Text,
        Secret => InputKind::Secret,
        Phone => InputKind::Phone,
    });
    roundtrip_all!(AccountState {
        Linking => AccountState::Linking,
        Active => AccountState::Active,
        LoggedOut => AccountState::LoggedOut { reason: "unlinked".into() },
        Removing => AccountState::Removing,
    });
    roundtrip_all!(TrustLevel {
        Unverified => TrustLevel::Unverified,
        ServerVerified => TrustLevel::ServerVerified,
        Verified => TrustLevel::Verified,
    });
    roundtrip_all!(MemberRole {
        Member => MemberRole::Member,
        Admin => MemberRole::Admin,
        Pending => MemberRole::Pending,
        Requesting => MemberRole::Requesting,
    });
    roundtrip_all!(ContactChange {
        SetNickname => ContactChange::SetNickname { nickname: Some("Bobby".into()) },
        SetBlocked => ContactChange::SetBlocked { blocked: true },
        SetVerified => ContactChange::SetVerified { verified: false },
    });
    roundtrip_all!(ConvKind {
        Direct => ConvKind::Direct,
        Group => ConvKind::Group,
        Broadcast => ConvKind::Broadcast,
        Channel => ConvKind::Channel,
    });
    roundtrip_all!(ConversationChange {
        SetArchived => ConversationChange::SetArchived { archived: true },
        SetPinned => ConversationChange::SetPinned { pinned: false },
        SetMuted => ConversationChange::SetMuted { until: Some(i64::MAX) },
        SetBlocked => ConversationChange::SetBlocked { blocked: true },
        SetMarkedUnread => ConversationChange::SetMarkedUnread { marked_unread: true },
        SetTimer => ConversationChange::SetTimer { timer: None },
        AcceptRequest => ConversationChange::AcceptRequest,
        Leave => ConversationChange::Leave,
    });
    roundtrip_all!(Patch<String> {
        Unchanged => Patch::<String>::Unchanged,
        Clear => Patch::<String>::Clear,
        Set => Patch::Set("x".to_owned()),
    });
}

/// Rows and descriptors round-trip as a whole (base64 bytes, maps with enum keys, nested
/// patches), and minimal JSON fills defaults.
#[test]
fn structs_roundtrip_and_default_missing_fields() {
    fn same<T: Serialize + DeserializeOwned + PartialEq + Debug>(v: &T) {
        let json = serde_json::to_string(v).expect("serialize");
        assert_eq!(
            &serde_json::from_str::<T>(&json).expect("deserialize"),
            v,
            "{json}"
        );
    }
    let mut features = ConversationFeatures {
        max_text_bytes: Some(2048),
        reply: Support::Full,
        edit: Some(Window {
            max_age_secs: Some(86_400),
            max_count: Some(10),
        }),
        reactions: Support::Full,
        reactions_per_user: Some(1),
        typing: true,
        history: HistorySource::OnDemand,
        cannot_send: Some(CannotSend::NotAccepted),
        ..ConversationFeatures::default()
    };
    features
        .formatting
        .insert(FormatKind::Spoiler, Support::Partial);
    features.files.insert(
        AttachmentKind::Image,
        FileLimits {
            max_size: 100 << 20,
            caption: true,
        },
    );
    features
        .disappearing
        .insert(DisappearKind::AfterRead, vec![30, 3600]);
    same(&features);
    same(&NetworkDescriptor {
        display_name: "Signal".into(),
        icon_name: "signal".into(),
        login_flows: vec![LoginFlow {
            id: "qr".into(),
            label: "Link with QR code".into(),
        }],
        start_chat: vec![IdentifierKind::Phone, IdentifierKind::Username],
        history: HistorySource::None,
        msg_id_scope: MsgIdScope::AccountAuthor,
        typing: TypingPolicy {
            refresh: Duration::from_secs(10),
            stop_after: Duration::from_secs(3),
        },
        risk_notice: Some(RiskNotice {
            title: "t".into(),
            body: "b".into(),
            url: None,
        }),
    });
    let message = Message {
        id: MessageId(1),
        conversation: ConversationId(2),
        net_id: NetMsgId::remote(uid("aci:a"), "17"),
        origin: Origin::Remote {
            author: uid("aci:a"),
            device: Some(1),
        },
        body: Body::Media {
            attachments: vec![attachment()],
            caption: None,
        },
        reply_to: Some(ReplyTo {
            target: target(),
            author: Some(uid("aci:b")),
            snapshot: Some(QuoteSnapshot {
                text: Some(text()),
                attachment: Some(AttachmentKind::Image),
                file_name: None,
                mime: Some("image/png".into()),
                thumb: Some(Box::new(blob_ref())),
            }),
        }),
        sent_at: 17,
        server_at: Some(18),
        received_at: 19,
        arrival: Some(3),
        sort_key: 19 << 10,
        revision: 1,
        edited_at: Some(20),
        deleted: None,
        out_state: None,
        read_state: Some(ReadState::Read),
        expiry: Some(Expiry {
            kind: DisappearKind::AfterRead,
            timer: Duration::from_secs(30),
            started_at: None,
        }),
        view_once: false,
        history: false,
        linked: None,
        meta: Some(BackendBlob::new(2, vec![0xff])),
        raw: None,
    };
    same(&message);
    let minimal: Message = serde_json::from_str(
        r#"{"id":1,"conversation":2,"net_id":"m1:17@aci:a",
            "origin":{"Remote":{"author":"aci:a"}},"body":{"Text":{"body":"x"}},
            "sent_at":17,"received_at":19,"sort_key":5}"#,
    )
    .expect("minimal message");
    assert_eq!(minimal.net_id, NetMsgId::remote(uid("aci:a"), "17"));
    assert!(minimal.reply_to.is_none() && !minimal.history && minimal.revision == 0);
    let attachment: Attachment = serde_json::from_str("{}").expect("empty attachment");
    assert_eq!(attachment, Attachment::default());
    same(&AccountCaps {
        max_attachment_bytes: Some(100 << 20),
        flags: [("polls".to_owned(), true)].into(),
        ..AccountCaps::default()
    });
    same(&LoginStep {
        step_id: "qr".into(),
        txn: 2,
        kind: LoginStepKind::Done,
    });
    same(&error());
}

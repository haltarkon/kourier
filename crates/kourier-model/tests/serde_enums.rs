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
        Telegram => NetworkKind::Telegram,
    });
    roundtrip_all!(MsgIdScope {
        AccountAuthor => MsgIdScope::AccountAuthor,
        Conversation => MsgIdScope::Conversation,
        ConversationAuthor => MsgIdScope::ConversationAuthor,
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
    roundtrip_all!(EditTarget {
        LatestRevision => EditTarget::LatestRevision,
        Original => EditTarget::Original,
    });
    roundtrip_all!(CannotSend {
        NotMember => CannotSend::NotMember,
        AdminsOnly => CannotSend::AdminsOnly,
        Blocked => CannotSend::Blocked,
        RequestPending => CannotSend::RequestPending,
        Unregistered => CannotSend::Unregistered,
        NotAccepted => CannotSend::NotAccepted,
        StillLoading => CannotSend::StillLoading,
        InvitationPending => CannotSend::InvitationPending,
        JoinRequestPending => CannotSend::JoinRequestPending,
        NotYetBuilt => CannotSend::NotYetBuilt,
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
        Unavailable => AttachmentState::Unavailable,
        Asking => AttachmentState::Asking,
        AskFailed => AttachmentState::AskFailed { why: AskFailure::NoFile },
    });
    roundtrip_all!(AskFailure {
        NoAnswer => AskFailure::NoAnswer,
        NoMessage => AskFailure::NoMessage,
        NoFile => AskFailure::NoFile,
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
        VerifiedChanged => SystemEvent::VerifiedChanged {
            user: uid("aci:a"),
            verified: true,
            other_device: true,
        },
    });
    roundtrip_all!(Placeholder {
        Undecryptable => Placeholder::Undecryptable { resendable: true },
        ViewOnceOpened => Placeholder::ViewOnceOpened,
        Expired => Placeholder::Expired,
    });
    roundtrip_all!(Deletion {
        ForEveryone => Deletion::ForEveryone { by: Some(uid("aci:a")) },
        ForMe => Deletion::ForMe,
        Unattributed => Deletion::Unattributed,
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
        ReachOutRefused => FailReason::ReachOutRefused,
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
        AlreadyAdded => ErrorKind::AlreadyAdded { logged_out: true },
        NotTheExpectedAccount => ErrorKind::NotTheExpectedAccount,
        TooManyDevices => ErrorKind::TooManyDevices { max: Some(4) },
        ReachOutRefused => ErrorKind::ReachOutRefused,
        Internal => ErrorKind::Internal,
    });
}

/// A `core.db` row, a `core_outbox` row or a dev-control line written before the typed
/// restrictions and the two descriptor flags has none of the new fields: it must read as the
/// defaults, never fail (the stored JSON outlives the build that wrote it).
#[test]
fn json_written_before_the_typed_states_reads_as_the_defaults() {
    let old = r#"{"Restricted":{"until_ms":1700000000000,"reason":"temporary ban"}}"#;
    assert_eq!(
        serde_json::from_str::<ConnState>(old).expect("an old Restricted"),
        ConnState::Restricted {
            until_ms: Some(1_700_000_000_000),
            reason: "temporary ban".into(),
            kind: Restriction::Unknown,
            note: NetworkNote::default(),
        }
    );
    let old = r#"{"LoggedOut":{"reason":"unlinked"}}"#;
    assert_eq!(
        serde_json::from_str::<ConnState>(old).expect("an old LoggedOut"),
        ConnState::LoggedOut {
            reason: "unlinked".into(),
            cause: LogoutCause::Unknown,
            note: NetworkNote::default(),
        }
    );
    let old = r#"{"display_name":"Signal","icon_name":"signal"}"#;
    let d = serde_json::from_str::<NetworkDescriptor>(old).expect("an old descriptor");
    assert!(!d.history_transfer && !d.experimental);
    // K-B1 (T16): the edit target and the outbox's numbers come after the descriptor was
    // stored too: a stored descriptor reads as Signal's values, and a policy with only some
    // of its numbers keeps Signal's for the rest.
    assert_eq!(d.msg_id_scope, MsgIdScope::AccountAuthor);
    assert_eq!(d.edit_target, EditTarget::LatestRevision);
    assert_eq!(d.outbox, OutboxPolicy::default());
    assert_eq!(d.outbox.budget, Duration::from_secs(24 * 3600));
    assert_eq!(d.outbox.max_backoff, Duration::from_secs(15 * 60));
    assert_eq!(d.outbox.challenge_lifetime, Duration::from_secs(24 * 3600));
    let part = r#"{"display_name":"W","icon_name":"w","outbox":{"budget":{"secs":21600,"nanos":0}},"edit_target":"Original","msg_id_scope":"ConversationAuthor"}"#;
    let d = serde_json::from_str::<NetworkDescriptor>(part).expect("a descriptor with new fields");
    assert_eq!(d.msg_id_scope, MsgIdScope::ConversationAuthor);
    assert_eq!(d.edit_target, EditTarget::Original);
    assert_eq!(d.outbox.budget, Duration::from_secs(6 * 3600));
    assert_eq!(d.outbox.max_backoff, Duration::from_secs(15 * 60));
    // TooManyDevices without its number (a backend that does not know it).
    assert_eq!(
        serde_json::from_str::<ErrorKind>(r#"{"TooManyDevices":{}}"#).ok(),
        Some(ErrorKind::TooManyDevices { max: None })
    );
}

#[test]
fn account_conn_login_enums() {
    roundtrip_all!(ConnState {
        Offline => ConnState::Offline { retry_at_ms: Some(5) },
        Connecting => ConnState::Connecting,
        Syncing => ConnState::Syncing { fraction: Some(0.25) },
        Online => ConnState::Online { detail: Some("leader".into()) },
        NeedsUserAction => ConnState::NeedsUserAction(UserAction::Relink),
        Restricted => ConnState::Restricted {
            until_ms: None,
            reason: "temporary ban".into(),
            kind: Restriction::Banned(BanReason::TooManyStrangers),
            note: NetworkNote {
                explanation: Some("explained".into()),
                appeal_url: Some("https://example.invalid/appeal".into()),
            },
        },
        LoggedOut => ConnState::LoggedOut {
            reason: "unlinked".into(),
            cause: LogoutCause::RemovedOnPhone,
            note: NetworkNote::default(),
        },
    });
    roundtrip_all!(Restriction {
        Unknown => Restriction::Unknown,
        Banned => Restriction::Banned(BanReason::Other(107)),
    });
    roundtrip_all!(BanReason {
        TooManyStrangers => BanReason::TooManyStrangers,
        ManyBlocked => BanReason::ManyBlocked,
        TooManyGroups => BanReason::TooManyGroups,
        SameMessageToMany => BanReason::SameMessageToMany,
        BroadcastList => BanReason::BroadcastList,
        Other => BanReason::Other(-1),
    });
    roundtrip_all!(LogoutCause {
        Unknown => LogoutCause::Unknown,
        RemovedOnPhone => LogoutCause::RemovedOnPhone,
        PhoneInactive => LogoutCause::PhoneInactive,
        AccountGone => LogoutCause::AccountGone,
        Unofficial => LogoutCause::Unofficial,
    });
    roundtrip_all!(HoldReason {
        Unknown => HoldReason::Unknown,
        ReachOutLocked => HoldReason::ReachOutLocked,
        Frozen => HoldReason::Frozen,
    });
    let hold = AccountHold {
        until_ms: Some(1_800_000_000_000),
        reason: HoldReason::ReachOutLocked,
    };
    let json = serde_json::to_string(&hold).expect("serialize");
    assert_eq!(serde_json::from_str::<AccountHold>(&json).ok(), Some(hold));
    // A hold with nothing stated reads as "until the backend lifts it, no reason".
    assert_eq!(
        serde_json::from_str::<AccountHold>("{}").ok(),
        Some(AccountHold {
            until_ms: None,
            reason: HoldReason::Unknown
        })
    );
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
            note: None,
            code: None,
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
    roundtrip_all!(CodeVia {
        App => CodeVia::App,
        Sms => CodeVia::Sms,
        SmsWord => CodeVia::SmsWord,
        SmsPhrase => CodeVia::SmsPhrase,
        Call => CodeVia::Call,
        Fragment => CodeVia::Fragment,
        Other => CodeVia::Other,
    });
    roundtrip_all!(AccountState {
        Linking => AccountState::Linking,
        Active => AccountState::Active,
        LoggedOut => AccountState::LoggedOut { reason: "unlinked".into() },
        Removing => AccountState::Removing,
        Unlinking => AccountState::Unlinking,
    });
    roundtrip_all!(TrustLevel {
        Unverified => TrustLevel::Unverified,
        ServerVerified => TrustLevel::ServerVerified,
        Verified => TrustLevel::Verified,
        Changed => TrustLevel::Changed,
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
        group_info: Some(GroupInfo {
            attributes_admin_only: true,
            members_admin_only: false,
            announcements_only: true,
            terminated: false,
        }),
        ..ConversationFeatures::default()
    };
    // A conversation whose network states no rules keeps the JSON it had before the field
    // existed (P2: its stored hash stays equal), and an older stored row reads as none.
    let none = serde_json::to_string(&ConversationFeatures::default()).expect("serialize");
    assert!(!none.contains("group_info"), "{none}");
    let some = serde_json::to_string(&features).expect("serialize");
    assert!(
        some.contains("\"group_info\":{\"attributes_admin_only\":true"),
        "{some}"
    );
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
        history_read: HistoryReadState::Known,
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
        history_transfer: true,
        experimental: true,
        edit_target: EditTarget::Original,
        outbox: OutboxPolicy {
            budget: Duration::from_secs(6 * 3600),
            max_backoff: Duration::from_secs(60),
            challenge_lifetime: Duration::from_secs(600),
        },
        shutdown_grace: Some(Duration::from_secs(20)),
        safety_numbers: true,
        contact_cards: true,
        cleaned_pictures: Some(CleanedPictures {
            types: ["image/jpeg".to_owned(), "image/png".to_owned()].into(),
            max_bytes: Some(64 << 20),
        }),
        receipts_always_sent: true,
        server: ServerRules {
            owns_unread: true,
            validates_deletes: true,
        },
    });
    // A descriptor that states no grace (an older one, or Signal's) reads back as none.
    let mut plain = serde_json::to_value(NetworkDescriptor {
        display_name: "Signal".into(),
        icon_name: "signal".into(),
        login_flows: vec![],
        start_chat: vec![],
        history: HistorySource::None,
        history_read: HistoryReadState::Known,
        msg_id_scope: MsgIdScope::AccountAuthor,
        typing: TypingPolicy::default(),
        risk_notice: None,
        history_transfer: false,
        experimental: false,
        edit_target: EditTarget::default(),
        outbox: OutboxPolicy::default(),
        shutdown_grace: None,
        safety_numbers: true,
        contact_cards: true,
        cleaned_pictures: Some(CleanedPictures::stills(&["image/jpeg"])),
        receipts_always_sent: true,
        server: ServerRules {
            owns_unread: true,
            validates_deletes: true,
        },
    })
    .expect("to value");
    let fields = plain.as_object_mut().expect("an object");
    fields.remove("shutdown_grace");
    fields.remove("safety_numbers");
    fields.remove("contact_cards");
    fields.remove("cleaned_pictures");
    fields.remove("receipts_always_sent");
    fields.remove("server");
    let read: NetworkDescriptor = serde_json::from_value(plain).expect("reads without the field");
    assert_eq!(read.shutdown_grace, None);
    assert!(
        !read.safety_numbers,
        "an older descriptor has no safety numbers"
    );
    assert!(
        !read.contact_cards,
        "an older descriptor sends no contact cards"
    );
    assert_eq!(
        read.cleaned_pictures, None,
        "an older descriptor cleans no pictures"
    );
    assert!(
        !read.receipts_always_sent,
        "an older descriptor does not say its receipts are always sent (KT-1b)"
    );
    assert_eq!(
        read.server,
        ServerRules::default(),
        "an older descriptor leaves everything to the clients (KT-2)"
    );
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

/// What the network said about a ban or a logout comes from outside: its `Debug` (a connection
/// state is logged) says which parts are there and never what they say.
#[test]
fn a_network_note_never_prints_its_text() {
    let note = NetworkNote {
        explanation: Some("the account +000 sent too much".into()),
        appeal_url: Some("https://example.invalid/appeal?token=secret".into()),
    };
    let shown = format!("{note:?}");
    assert!(
        !shown.contains("+000") && !shown.contains("secret"),
        "{shown}"
    );
    assert!(shown.contains("explanation") && shown.contains("appeal_url"));
    let state = ConnState::LoggedOut {
        reason: "closed".into(),
        cause: LogoutCause::AccountGone,
        note,
    };
    let shown = format!("{state:?}");
    assert!(
        !shown.contains("+000") && !shown.contains("secret"),
        "{shown}"
    );
    assert!(NetworkNote::default().is_empty());
    assert!(!state_note(&state).is_empty());
    // A state written before the note existed reads with an empty one.
    let old = r#"{"LoggedOut":{"reason":"unlinked","cause":"RemovedOnPhone"}}"#;
    assert!(state_note(&serde_json::from_str(old).expect("old")).is_empty());
}

/// A login field's note is the network's own words (Telegram's password hint, the owner's own
/// text): it round-trips, an older field without it reads as `None`, a field without one writes
/// no `note` key, and its `Debug` never shows what it says (S-3, critic M1).
#[test]
fn a_login_fields_note_round_trips_and_never_prints_its_text() {
    let field = InputField {
        id: "password".into(),
        label: "Password".into(),
        kind: InputKind::Secret,
        default: None,
        required: true,
        note: Some(NetworkNote {
            explanation: Some("synthetic hint".into()),
            appeal_url: None,
        }),
        code: None,
    };
    let json = serde_json::to_string(&field).expect("serialize");
    assert_eq!(
        serde_json::from_str::<InputField>(&json).ok(),
        Some(field.clone())
    );
    let shown = format!("{field:?}");
    assert!(!shown.contains("synthetic hint"), "{shown}");
    assert!(shown.contains("note"), "{shown}");
    let old = r#"{"id":"phone","label":"Phone number","kind":"Phone","required":true}"#;
    let read: InputField = serde_json::from_str(old).expect("an older field");
    assert_eq!(read.note, None);
    let plain = InputField {
        note: None,
        ..field
    };
    let json = serde_json::to_string(&plain).expect("serialize");
    assert!(!json.contains("\"note\""), "{json}");
    assert!(!json.contains("\"code\""), "no delivery, no key: {json}");
}

/// KT-1b (TSC M2): the code field states how the network sent the code and whether a new one may be
/// asked for. It round-trips with and without a resend, an older field and an older delivery read as
/// `None` ("the network said nothing"), `Debug` prints the way and never a value, and the resend's
/// value id is one constant.
#[test]
fn a_login_codes_delivery_round_trips_and_an_older_field_reads_without_it() {
    let delivery = CodeDelivery {
        via: CodeVia::Sms,
        resend: Some(CodeResend {
            after: Duration::from_secs(45),
            via: CodeVia::Call,
        }),
    };
    let field = InputField {
        id: "code".into(),
        label: "Code".into(),
        kind: InputKind::Text,
        default: None,
        required: true,
        note: None,
        code: Some(delivery),
    };
    let json = serde_json::to_string(&field).expect("serialize");
    assert!(json.contains("\"code\":{\"via\":\"Sms\""), "{json}");
    assert_eq!(
        serde_json::from_str::<InputField>(&json).ok(),
        Some(field.clone())
    );
    // The delivery with no resend round-trips too, and an older delivery (no `resend` key) is one.
    let last = CodeDelivery {
        via: CodeVia::App,
        resend: None,
    };
    let json = serde_json::to_string(&last).expect("serialize");
    assert_eq!(serde_json::from_str::<CodeDelivery>(&json).ok(), Some(last));
    let older: CodeDelivery = serde_json::from_str(r#"{"via":"Fragment"}"#).expect("no resend");
    assert_eq!(
        older,
        CodeDelivery {
            via: CodeVia::Fragment,
            resend: None
        }
    );
    // A field written before the delivery existed reads as "said nothing".
    let old = r#"{"id":"code","label":"Code","kind":"Text","required":true}"#;
    let read: InputField = serde_json::from_str(old).expect("an older field");
    assert_eq!(read.code, None);
    // `Debug` keeps printing the way (not secret) and still hides the default and the note.
    let shown = format!("{field:?}");
    assert!(shown.contains("Sms") && shown.contains("Call"), "{shown}");
    assert_eq!(RESEND_FIELD, "resend");
}

fn state_note(state: &ConnState) -> &NetworkNote {
    match state {
        ConnState::LoggedOut { note, .. } | ConnState::Restricted { note, .. } => note,
        other => panic!("no note on {other:?}"),
    }
}

/// LOC: a network that cleans nothing writes no `cleaned_pictures` field, one that cleans
/// reads back the same, and `cleans` follows the types and the bound.
#[test]
fn cleaned_pictures_are_left_out_when_none_and_bound_by_type_and_size() {
    let plain = serde_json::to_value(NetworkDescriptor {
        display_name: "Fake".into(),
        icon_name: "fake".into(),
        login_flows: vec![],
        start_chat: vec![],
        history: HistorySource::None,
        history_read: HistoryReadState::Known,
        msg_id_scope: MsgIdScope::AccountAuthor,
        typing: TypingPolicy::default(),
        risk_notice: None,
        history_transfer: false,
        experimental: false,
        edit_target: EditTarget::default(),
        outbox: OutboxPolicy::default(),
        shutdown_grace: None,
        safety_numbers: false,
        contact_cards: false,
        cleaned_pictures: None,
        receipts_always_sent: false,
        server: ServerRules::default(),
    })
    .expect("to value");
    assert!(
        !plain
            .as_object()
            .expect("an object")
            .contains_key("cleaned_pictures"),
        "None is not written"
    );
    let stated = CleanedPictures {
        types: ["image/jpeg".to_owned()].into(),
        max_bytes: Some(100),
    };
    let json = serde_json::to_string(&stated).expect("serialize");
    assert_eq!(
        serde_json::from_str::<CleanedPictures>(&json).expect("deserialize"),
        stated
    );
    assert!(stated.cleans("image/jpeg", 100));
    assert!(!stated.cleans("image/jpeg", 101), "over the bound");
    assert!(!stated.cleans("image/heic", 1), "a type it does not state");
    let unbounded = CleanedPictures::stills(&["image/png", "image/webp"]);
    assert_eq!(unbounded.max_bytes, None);
    assert!(unbounded.cleans("image/webp", u64::MAX));
    assert!(!unbounded.cleans("image/jpeg", 1));
}

/// A10a: the account's roaming flag has three states (absent, off, on) and its pin limit is
/// optional; an `AccountCaps` stored before either existed reads as "the network cannot" and "no
/// limit stated", and the flag and the limit round-trip.
#[test]
fn account_caps_state_roaming_writes_in_three_states_and_older_json_reads_as_absent() {
    // JSON as an earlier build stored it: no pin limit, no roaming flag.
    let older: AccountCaps = serde_json::from_str(
        r#"{"max_attachment_bytes":100,"flags":{"attachment_restore":true,"admin_delete":false}}"#,
    )
    .expect("an older caps value");
    assert_eq!(older.max_pinned_chats, None);
    assert_eq!(older.roaming_writes(), None, "absent: the network cannot");
    assert!(
        older.restores_attachments(),
        "the other flags read as before"
    );
    assert_eq!(
        serde_json::from_str::<AccountCaps>("{}")
            .expect("empty")
            .roaming_writes(),
        None
    );

    for (state, flag) in [(Some(false), false), (Some(true), true)] {
        let caps = AccountCaps {
            max_pinned_chats: Some(4),
            flags: [(AccountCaps::FLAG_ROAMING_WRITES.to_owned(), flag)].into(),
            ..AccountCaps::default()
        };
        assert_eq!(caps.roaming_writes(), state);
        let json = serde_json::to_string(&caps).expect("serialize");
        assert!(json.contains("\"roaming_writes\""), "{json}");
        let back: AccountCaps = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, caps, "{json}");
        assert_eq!(
            (back.roaming_writes(), back.max_pinned_chats),
            (state, Some(4))
        );
    }
    // The flag is one named flag among the others: another flag does not state it.
    let other = AccountCaps {
        flags: [(AccountCaps::FLAG_ATTACHMENT_RESTORE.to_owned(), true)].into(),
        ..AccountCaps::default()
    };
    assert_eq!(other.roaming_writes(), None);
    assert_eq!(AccountCaps::FLAG_ROAMING_WRITES, "roaming_writes");
}

/// S-P: a protected chat (the user protects it on another device) is a field of the patch and
/// of the conversation that a network which states nothing leaves out of the JSON; older JSON
/// reads as unchanged (the patch) and as not protected (the conversation).
#[test]
fn a_protected_chat_is_stated_in_its_patch_and_absent_means_not_said() {
    let key = ConvKey::Direct(uid("aci:a"));
    let plain = ConversationPatch::new(key.clone());
    let json = serde_json::to_string(&plain).expect("serialize");
    assert!(!json.contains("protected"), "{json}");
    let older: ConversationPatch = serde_json::from_str(&json).expect("older patch");
    assert_eq!(older.protected, None);
    for state in [true, false] {
        let p = ConversationPatch {
            protected: Some(state),
            ..ConversationPatch::new(key.clone())
        };
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(json.contains(&format!("\"protected\":{state}")), "{json}");
        assert_eq!(
            serde_json::from_str::<ConversationPatch>(&json).expect("deserialize"),
            p
        );
    }
    // A conversation written before the field existed: the same JSON without it.
    let mut stored = serde_json::to_value(Conversation {
        protected: true,
        ..serde_json::from_value::<Conversation>(serde_json::json!({
            "id": 1, "account": 1, "key": key, "kind": "Direct"
        }))
        .expect("a conversation from its four fields")
    })
    .expect("to value");
    assert_eq!(stored["protected"], serde_json::json!(true));
    stored
        .as_object_mut()
        .expect("an object")
        .remove("protected");
    let older: Conversation = serde_json::from_value(stored).expect("older conversation");
    assert!(
        !older.protected,
        "a conversation stored before the field is not protected"
    );
}

/// A10a (M4): whether the network lists a chat for its roaming settings is a field of the chat's
/// features that a network which states nothing leaves out of the JSON, so every stored
/// features value (and its hash) is what it was; only `Some(false)` unlists a chat.
#[test]
fn a_chat_the_network_does_not_list_is_stated_in_its_features_and_absent_means_listed() {
    let plain = ConversationFeatures::default();
    assert_eq!(plain.roaming_listed, None);
    assert!(plain.lists_for_roaming(), "nothing stated: listed");
    let json = serde_json::to_string(&plain).expect("serialize");
    assert!(!json.contains("roaming_listed"), "{json}");
    // The hash of a features value that states nothing is unchanged: the JSON is the same bytes
    // as before the field existed.
    assert_eq!(plain.stable_hash(), stable_hash(json.as_bytes()));

    // Features stored before the field existed read as listed.
    let older: ConversationFeatures =
        serde_json::from_str(r#"{"max_text_bytes":2048,"typing":true}"#).expect("older features");
    assert!(older.lists_for_roaming() && older.roaming_listed.is_none());

    let unlisted = ConversationFeatures {
        roaming_listed: Some(false),
        ..ConversationFeatures::default()
    };
    assert!(!unlisted.lists_for_roaming());
    let json = serde_json::to_string(&unlisted).expect("serialize");
    assert!(json.contains("\"roaming_listed\":false"), "{json}");
    assert_eq!(
        serde_json::from_str::<ConversationFeatures>(&json).expect("deserialize"),
        unlisted
    );
    assert_ne!(
        unlisted.stable_hash(),
        plain.stable_hash(),
        "unlisting a chat is a change that is stated"
    );

    let listed = ConversationFeatures {
        roaming_listed: Some(true),
        ..ConversationFeatures::default()
    };
    assert!(listed.lists_for_roaming());
    assert_eq!(
        serde_json::from_str::<ConversationFeatures>(
            &serde_json::to_string(&listed).expect("serialize")
        )
        .expect("deserialize"),
        listed
    );
}

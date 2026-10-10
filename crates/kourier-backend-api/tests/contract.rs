//! Contract tests of the core ↔ backend boundary: serde of every enum (by variant name), the
//! `core_outbox` codec, envelope rules, ack-after-commit, the actor lifecycle through a test
//! `Network`, and log-safe `Debug`.

use std::fmt::Debug;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kourier_backend_api::model::*;
use kourier_backend_api::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("valid user id")
}

fn group() -> ConvKey {
    ConvKey::Group("gv2:AbC".into())
}

fn target() -> MsgTarget {
    MsgTarget::to_id(
        Some(group()),
        &NetMsgId::remote(uid("aci:a"), "1700000000000"),
    )
}

fn message(local: &str) -> IncomingMessage {
    IncomingMessage::new(
        group(),
        local,
        Origin::Remote {
            author: uid("aci:a"),
            device: Some(1),
        },
        Body::text("hello"),
        1_700_000_000_000,
        1_700_000_000_500,
    )
}

fn check<T: Serialize + DeserializeOwned + PartialEq + Debug>(name: &str, v: &T) {
    let json = serde_json::to_string(v).expect("serialize");
    assert!(
        json == format!("\"{name}\"") || json.starts_with(&format!("{{\"{name}\":")),
        "{name} is not tagged by name: {json}"
    );
    let back: T = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(&back, v, "{json}");
}

/// Round-trips every sample; the exhaustive `match` fails to compile when a variant has none.
macro_rules! roundtrip_all {
    ($ty:ty { $($variant:ident => $value:expr),+ $(,)? }) => {{
        type T = $ty;
        #[allow(dead_code, unreachable_patterns)]
        fn exhaustive(v: &T) {
            match v {
                $(T::$variant { .. } => {})+
            }
        }
        let samples: Vec<T> = vec![$($value),+];
        let names = [$(stringify!($variant)),+];
        for (name, v) in names.iter().zip(&samples) {
            check(name, v);
        }
        samples
    }};
}

fn all_events() -> Vec<BackendEvent> {
    let mut history = message("1");
    history.initial_out_state = Some(OutgoingState::Read);
    history.read = true;
    history.reactions = vec![HistoryReaction {
        by: None,
        emoji: "👍".into(),
        at: 5,
    }];
    history.edits = vec![HistoryEdit {
        local: "3".into(),
        at: 6,
    }];
    roundtrip_all!(BackendEvent {
        Login => BackendEvent::Login(LoginStep {
            step_id: "qr".into(),
            txn: 1,
            kind: LoginStepKind::ShowQr { data: Redacted::new("sgnl://linkdevice?x".into()), valid_for: None },
        }),
        AccountReady => BackendEvent::AccountReady {
            own_ids: vec![uid("aci:me"), uid("pni:me")],
            device: Some(2),
            display_name: Some(Redacted::new("Me".into())),
            number: Some(Redacted::new("+15550100".into())),
        },
        AccountSettings => BackendEvent::AccountSettings(AccountSettingsPatch {
            read_receipts: Some(false),
            universal_timer: Patch::Clear,
            ..AccountSettingsPatch::default()
        }),
        AccountCaps => BackendEvent::AccountCaps(AccountCaps {
            max_attachment_bytes: Some(100 << 20),
            ..AccountCaps::default()
        }),
        ContactUpsert => BackendEvent::ContactUpsert(ContactPatch {
            profile_name: Patch::Set("Alice".into()),
            nickname: Patch::Clear,
            ..ContactPatch::new(uid("aci:a"))
        }),
        IdentityMerged => BackendEvent::IdentityMerged { from: uid("pni:a"), into: uid("aci:a") },
        ConversationUpsert => BackendEvent::ConversationUpsert(ConversationPatch {
            title: Patch::Set("Group".into()),
            members: Some(MemberList::snapshot(vec![Member { user: uid("aci:a"), role: MemberRole::Admin }])),
            unread_hint: Some(3),
            ..ConversationPatch::new(group())
        }),
        PinnedConversations => BackendEvent::PinnedConversations(vec![group(), ConvKey::Direct(uid("aci:a"))]),
        ConversationRemoved => BackendEvent::ConversationRemoved { conv: group() },
        ConversationCleared => BackendEvent::ConversationCleared {
            conv: group(),
            up_to: vec![target(), MsgTarget::to_id(Some(group()), &NetMsgId::own("7"))],
            remove: true,
            at: 1_700_000_000_100,
        },
        AttachmentDeleted => BackendEvent::AttachmentDeleted {
            target: target(),
            net_ids: Redacted::new(vec!["uuid:1".into(), "digest:2".into()]),
            at: 1_700_000_000_200,
        },
        Message => BackendEvent::Message(message("2")),
        HistoryBatch => BackendEvent::HistoryBatch {
            conv: group(),
            messages: vec![history],
            has_more: true,
            progress: Some(0.5),
        },
        Edited => BackendEvent::Edited {
            target: target(),
            by: Some(uid("aci:a")),
            new_local: Some("1700000000999".into()),
            body: Body::text("edited"),
            edited_at: 7,
            keeps_quote: None,
        },
        Deleted => BackendEvent::Deleted { target: target(), by: None, for_everyone: true, at: 8 },
        Reaction => BackendEvent::Reaction {
            target: target(),
            by: Some(uid("aci:b")),
            emoji: "❤️".into(),
            remove: false,
            at: 9,
        },
        Receipts => BackendEvent::Receipts {
            targets: vec![MsgTarget::to_id(None, &NetMsgId::own("5"))],
            by: uid("aci:b"),
            kind: ReceiptKind::Read,
            at: 10,
        },
        ReadMarker => BackendEvent::ReadMarker { conv: None, up_to: target() },
        OwnSendState => BackendEvent::OwnSendState {
            local: Some(MessageId(4)),
            target: MsgTarget::to_id(Some(group()), &NetMsgId::own("6")),
            per_recipient: vec![(uid("aci:b"), RecipientState::Sent { sealed: true })],
        },
        CommandPersisted => BackendEvent::CommandPersisted { cmd: CmdId(12) },
        Typing => BackendEvent::Typing {
            conv: group(),
            by: uid("aci:a"),
            device: Some(1),
            on: true,
            ttl: Duration::from_secs(15),
        },
        AttachmentProgress => BackendEvent::AttachmentProgress { local: MessageId(4), index: 0, done: 1, total: 2 },
        DownloadProgress => BackendEvent::DownloadProgress { attachment: AttachmentId(3), done: 1, total: 2 },
        AttachmentDone => BackendEvent::AttachmentDone {
            attachment: AttachmentId(3),
            request: Some(DownloadId(9)),
            result: Ok(AttachmentMeta { size: 2, ..AttachmentMeta::default() }),
        },
        AttachmentDone => BackendEvent::AttachmentDone {
            attachment: AttachmentId(3),
            request: None,
            result: Err(BackendError::new(ErrorKind::Expired, "gone from CDN")),
        },
        TransmitProgress => BackendEvent::TransmitProgress { local: MessageId(4), progress: 0.25 },
        Notice => BackendEvent::Notice(BackendNotice::IdentityChanged { user: uid("aci:a") }),
        HistoryTransfer => BackendEvent::HistoryTransfer(HistoryTransfer::Downloading { done: 1, total: Some(2) }),
        HistoryTransfer => BackendEvent::HistoryTransfer(HistoryTransfer::Failed(HistoryTransferFailure::UpdateApp)),
    })
}

#[test]
fn every_enum_roundtrips_by_name() {
    assert!(all_events().len() >= 24);
    roundtrip_all!(BackendNotice {
        IdentityChanged => BackendNotice::IdentityChanged { user: uid("aci:a") },
        ChallengeRequired => BackendNotice::ChallengeRequired { url: "signalcaptcha://".into(), retry_after: Some(Duration::from_secs(60)) },
        Error => BackendNotice::Error(BackendError::unsupported("no").with_user_message("Not here")),
    });
    roundtrip_all!(OutgoingKind {
        Message => OutgoingKind::Message {
            body: Body::text("hi"),
            reply_to: Some(ReplyTo { target: target(), author: Some(uid("aci:a")), snapshot: None }),
            expiry: Some(Duration::from_secs(30)),
        },
        Edit => OutgoingKind::Edit { target: target(), body: Body::text("fixed") },
        Delete => OutgoingKind::Delete { target: target(), for_everyone: true },
        React => OutgoingKind::React { target: target(), emoji: "👍".into(), remove: true },
    });
    roundtrip_all!(TransmitOutcome {
        Sent => TransmitOutcome::Sent {
            server_at: Some(1),
            per_recipient: vec![(uid("aci:a"), RecipientState::Sent { sealed: false })],
        },
        Partial => TransmitOutcome::Partial {
            delivered: vec![(uid("aci:a"), RecipientState::Sent { sealed: true })],
            failed: vec![(uid("aci:b"), BackendError::transient("timeout"))],
        },
        Failed => TransmitOutcome::Failed(BackendError::new(ErrorKind::RateLimited { retry_after: None }, "429")),
    });
    roundtrip_all!(DurableCommand {
        SendReceipts => DurableCommand::SendReceipts { conv: group(), targets: vec![target()], kind: ReceiptKind::Viewed },
        SyncReadMarker => DurableCommand::SyncReadMarker { conv: group(), up_to: target(), read: vec![target()] },
        ClearConversation => DurableCommand::ClearConversation {
            conv: group(),
            newest: vec![target(), target()],
            newest_non_expiring: vec![target()],
            remove: true,
        },
        DeleteAttachment => DurableCommand::DeleteAttachment {
            conv: group(),
            target: target(),
            index: 2,
            net_ids: vec!["uuid:00ff".to_owned()].into(),
        },
    });
    roundtrip_all!(StartMode {
        Fresh => StartMode::Fresh,
        Resume => StartMode::Resume,
        Rebuild => StartMode::Rebuild,
    });
    roundtrip_all!(BackendEnv {
        Production => BackendEnv::Production,
        Staging => BackendEnv::Staging,
        LocalTest => BackendEnv::LocalTest(LocalTestEnv {
            host: "127.0.0.1".into(),
            port: 8443,
            ca_cert: Some(PathBuf::from("/tmp/ca.pem")),
            trust_root: Some("BXu6".into()),
            zk_params: None,
        }),
    });
    roundtrip_all!(InitialSync {
        LinkOnly => InitialSync::LinkOnly,
        ContactsAndStorage => InitialSync::ContactsAndStorage,
    });
    let request = OutgoingRequest {
        conv: group(),
        local: "1700000000000".into(),
        sent_at: 1_700_000_000_000,
        kind: OutgoingKind::Delete {
            target: target(),
            for_everyone: false,
        },
        only: Some(vec![uid("aci:b")]),
        is_recipient_update: true,
    };
    let json = serde_json::to_string(&request).expect("serialize");
    assert_eq!(
        serde_json::from_str::<OutgoingRequest>(&json).ok(),
        Some(request)
    );
    let config: BackendConfig = serde_json::from_str("{}").expect("defaults");
    assert_eq!(config.env, BackendEnv::Production);
    assert_eq!(config.initial_sync, InitialSync::ContactsAndStorage);
    let presence: UiPresence = serde_json::from_str(r#"{"focused":true}"#).expect("defaults");
    assert!(presence.focused && presence.visible.is_none());
}

#[test]
fn outbox_codec_roundtrips_durable_events_only() {
    let events = all_events();
    let (ephemeral, durable): (Vec<_>, Vec<_>) =
        events.into_iter().partition(BackendEvent::is_ephemeral);
    assert_eq!(ephemeral.len(), 7);
    // One row per event, and all durable events of a transaction in one row.
    for e in &durable {
        let row = encode_outbox_row(std::slice::from_ref(e)).expect("durable");
        assert_eq!(row[..2], OUTBOX_FORMAT.to_le_bytes());
        assert_eq!(decode_outbox_row(&row).ok(), Some(vec![e.clone()]));
    }
    let row = encode_outbox_row(&durable).expect("durable");
    assert_eq!(decode_outbox_row(&row).ok(), Some(durable.clone()));
    for e in &ephemeral {
        let mixed = [durable[0].clone(), e.clone()];
        assert_eq!(
            encode_outbox_row(&mixed),
            Err(CodecError::Ephemeral(e.name()))
        );
    }
    assert_eq!(encode_outbox_row(&[]), Err(CodecError::Empty));
    assert_eq!(decode_outbox_row(&[1]), Err(CodecError::Truncated));
    let mut newer = row.clone();
    newer[..2].copy_from_slice(&(OUTBOX_FORMAT + 1).to_le_bytes());
    assert_eq!(
        decode_outbox_row(&newer),
        Err(CodecError::UnknownFormat(OUTBOX_FORMAT + 1))
    );
    let mut corrupt = OUTBOX_FORMAT.to_le_bytes().to_vec();
    corrupt.extend_from_slice(br#"[{"Message":{"conv":"group:gv2:x","body":"SECRET-TEXT"}}]"#);
    let err = decode_outbox_row(&corrupt).expect_err("not an event");
    assert!(!err.to_string().contains("SECRET"), "{err}");
    // Rows that encoding refuses are refused on the way out too (no empty or ephemeral
    // envelopes reach the core from a corrupt or foreign row).
    let raw_row = |json: &str| {
        let mut row = OUTBOX_FORMAT.to_le_bytes().to_vec();
        row.extend_from_slice(json.as_bytes());
        row
    };
    assert_eq!(decode_outbox_row(&raw_row("[]")), Err(CodecError::Empty));
    for e in &ephemeral {
        let json = serde_json::to_string(&[durable[0].clone(), e.clone()]).expect("serialize");
        assert_eq!(
            decode_outbox_row(&raw_row(&json)),
            Err(CodecError::Ephemeral(e.name()))
        );
    }
}

#[test]
fn every_sample_event_satisfies_the_envelope_rules() {
    let (ephemeral, durable): (Vec<_>, Vec<_>) = all_events()
        .into_iter()
        .partition(BackendEvent::is_ephemeral);
    let (env, _w) = Envelope::durable(durable);
    assert_eq!(env.validate(), Ok(()));
    for e in ephemeral {
        assert_eq!(Envelope::ephemeral(e).validate(), Ok(()));
    }
}

#[test]
fn history_batches_and_reply_targets_are_checked() {
    let mut other = message("2");
    other.conv = ConvKey::Direct(uid("aci:b"));
    let (mixed, _w) = Envelope::durable(vec![BackendEvent::HistoryBatch {
        conv: group(),
        messages: vec![message("1"), other],
        has_more: false,
        progress: None,
    }]);
    assert_eq!(
        mixed.validate(),
        Err(ContractViolation::HistoryBatchMixesConversations)
    );
    let mut quoted = message("3");
    let mut bad_target = target();
    bad_target.alternates = vec!["".into()];
    quoted.reply_to = Some(ReplyTo {
        target: bad_target,
        author: None,
        snapshot: None,
    });
    let (bad, _w) = Envelope::durable(vec![BackendEvent::Message(quoted)]);
    assert_eq!(
        bad.validate(),
        Err(ContractViolation::InvalidId {
            event: "Message",
            error: IdError::Empty,
        })
    );
}

#[test]
fn envelope_rules() {
    let durable = BackendEvent::Message(message("1"));
    let typing = BackendEvent::Typing {
        conv: group(),
        by: uid("aci:a"),
        device: None,
        on: true,
        ttl: Duration::from_secs(15),
    };
    let (ok, _waiter) = Envelope::durable(vec![durable.clone()]);
    assert_eq!(ok.validate(), Ok(()));
    assert_eq!(Envelope::ephemeral(typing.clone()).validate(), Ok(()));
    let unacked = Envelope {
        events: vec![durable.clone()],
        ack: None,
    };
    assert_eq!(
        unacked.validate(),
        Err(ContractViolation::DurableWithoutAck("Message"))
    );
    let (acked_typing, _w) = Envelope::durable(vec![durable, typing]);
    assert_eq!(
        acked_typing.validate(),
        Err(ContractViolation::EphemeralWithAck("Typing"))
    );
    let (empty, _w) = Envelope::durable(vec![]);
    assert_eq!(empty.validate(), Err(ContractViolation::Empty));
    let big = BackendEvent::HistoryBatch {
        conv: group(),
        messages: (0..=MAX_HISTORY_BATCH)
            .map(|i| message(&i.to_string()))
            .collect(),
        has_more: false,
        progress: None,
    };
    let (big, _w) = Envelope::durable(vec![big]);
    assert_eq!(
        big.validate(),
        Err(ContractViolation::HistoryBatchTooLarge(
            MAX_HISTORY_BATCH + 1
        ))
    );
    let mut bad = message("1");
    bad.conv = ConvKey::Group("no-scheme".into());
    let (bad, _w) = Envelope::durable(vec![BackendEvent::Message(bad)]);
    assert!(matches!(
        bad.validate(),
        Err(ContractViolation::InvalidId {
            event: "Message",
            ..
        })
    ));
    let mut empty_local = target();
    empty_local.local = "".into();
    let (bad, _w) = Envelope::durable(vec![BackendEvent::ReadMarker {
        conv: None,
        up_to: empty_local,
    }]);
    assert!(bad.validate().is_err());
}

/// The versions of an edited history message (`IncomingMessage::edits`): their ids are
/// checked like every other id of an envelope, a message without them is stored as before
/// (a row written before the field existed reads as "not edited").
#[test]
fn the_ids_of_a_history_messages_edits_are_checked() {
    let batch = |m: IncomingMessage| {
        let (env, _w) = Envelope::durable(vec![BackendEvent::HistoryBatch {
            conv: group(),
            messages: vec![m],
            has_more: false,
            progress: None,
        }]);
        env
    };
    let mut edited = message("1");
    edited.edits = vec![
        HistoryEdit {
            local: "2".into(),
            at: 2,
        },
        HistoryEdit {
            local: "3".into(),
            at: 3,
        },
    ];
    assert_eq!(batch(edited.clone()).validate(), Ok(()));
    let json = serde_json::to_string(&edited).expect("json");
    assert!(
        json.contains("\"edits\":[{\"local\":\"2\",\"at\":2}"),
        "{json}"
    );
    let back: IncomingMessage = serde_json::from_str(&json).expect("back");
    assert_eq!(back, edited);

    let plain = message("1");
    let json = serde_json::to_string(&plain).expect("json");
    assert!(!json.contains("edits"), "skipped when empty: {json}");
    let back: IncomingMessage = serde_json::from_str(&json).expect("back");
    assert!(back.edits.is_empty());

    let long = "9".repeat(kourier_backend_api::model::ids::MAX_ID_LEN + 1);
    for bad in ["", "x\u{0}y", long.as_str()] {
        let mut m = message("1");
        m.edits = vec![
            HistoryEdit {
                local: "2".into(),
                at: 2,
            },
            HistoryEdit {
                local: bad.into(),
                at: 3,
            },
        ];
        assert!(
            matches!(
                batch(m).validate(),
                Err(ContractViolation::InvalidId {
                    event: "HistoryBatch",
                    ..
                })
            ),
            "{bad:?}"
        );
    }
    assert_eq!(MAX_HISTORY_EDITS, 100);
}

/// A call is reported as messages whose body is a call line: every signal of one call under
/// the same id, `call:<id>` in the peer's chat or `gcall:<id>` in the group; live or in a
/// history batch; its deletion as a delete for me of that id.
#[test]
fn call_lines_are_checked() {
    let peer = ConvKey::Direct(uid("aci:a"));
    let caller = || Origin::Remote {
        author: uid("aci:a"),
        device: None,
    };
    let ring = |group: bool| {
        CallLine::signal(
            CallDirection::Incoming,
            group,
            CallState::Ringing { deadline: 61_000 },
            1_000,
        )
        .with_by(uid("aci:a"), 1_000)
    };
    let direct = IncomingMessage::call(peer.clone(), "77", caller(), ring(false), 1_000, 1_500);
    assert_eq!(&*direct.local, "call:77");
    assert_eq!(direct.net_id().author, None, "a call line has no author");
    let in_group = IncomingMessage::call(group(), "77", caller(), ring(true), 1_000, 1_500);
    assert_eq!(&*in_group.local, "gcall:77");
    let deleted = BackendEvent::Deleted {
        target: IncomingMessage::call_target(peer.clone(), false, "77"),
        by: None,
        for_everyone: false,
        at: 2_000,
    };
    let (ok, _w) = Envelope::durable(vec![
        BackendEvent::Message(direct.clone()),
        BackendEvent::Message(in_group.clone()),
        deleted,
        BackendEvent::HistoryBatch {
            conv: peer.clone(),
            messages: vec![direct.clone()],
            has_more: false,
            progress: None,
        },
    ]);
    assert_eq!(ok.validate(), Ok(()));

    let refused = |m: IncomingMessage| {
        let (env, _w) = Envelope::durable(vec![BackendEvent::Message(m)]);
        matches!(env.validate(), Err(ContractViolation::CallLine(_)))
    };
    // The id of the other kind, no call id, or no prefix at all.
    for local in ["gcall:77", "call:", "77"] {
        let mut m = direct.clone();
        m.local = local.into();
        assert!(refused(m), "{local}");
    }
    // A 1:1 line in a group, a group line in a direct chat.
    assert!(refused(IncomingMessage::call(
        group(),
        "77",
        caller(),
        ring(false),
        1,
        2
    )));
    assert!(refused(IncomingMessage::call(
        peer.clone(),
        "77",
        caller(),
        ring(true),
        1,
        2
    )));
    let (batch, _w) = Envelope::durable(vec![BackendEvent::HistoryBatch {
        conv: peer.clone(),
        messages: vec![IncomingMessage::call(
            peer,
            "77",
            caller(),
            ring(true),
            1,
            2,
        )],
        has_more: false,
        progress: None,
    }]);
    assert!(matches!(
        batch.validate(),
        Err(ContractViolation::CallLine(_))
    ));
    // A message that is no call line may have any id.
    let (text, _w) = Envelope::durable(vec![BackendEvent::Message(message("call:77"))]);
    assert_eq!(text.validate(), Ok(()));
}

/// Who may say what of a call: the other side (`Origin::Remote`) only that an incoming call
/// was started, rings or was missed; answered, declined, unanswered and every signal of an
/// outgoing call only one of the user's own devices. Anything else is a violation, live or
/// in a history batch.
#[test]
fn the_other_side_of_a_call_says_only_what_it_may() {
    use CallDirection::{Incoming, Outgoing};
    let peer = ConvKey::Direct(uid("aci:a"));
    let states = [
        CallState::Started,
        CallState::Ringing { deadline: 61_000 },
        CallState::Missed,
        CallState::Unanswered,
        CallState::Declined,
        CallState::Answered,
    ];
    for direction in [Incoming, Outgoing] {
        for state in states {
            for group_call in [false, true] {
                let conv = if group_call { group() } else { peer.clone() };
                let line = CallLine::signal(direction, group_call, state, 1_000);
                // ("Unanswered" of an incoming call is the one form "missed":
                // `CallLine::normalized`.)
                let allowed = direction == Incoming
                    && !matches!(state, CallState::Declined | CallState::Answered);
                assert_eq!(line.may_come_from_peer(), allowed);
                let said_by = |origin: Origin| {
                    let m = IncomingMessage::call(
                        conv.clone(),
                        "5",
                        origin,
                        line.clone(),
                        1_000,
                        1_500,
                    );
                    let (live, _w) = Envelope::durable(vec![BackendEvent::Message(m.clone())]);
                    let (batch, _w) = Envelope::durable(vec![BackendEvent::HistoryBatch {
                        conv: conv.clone(),
                        messages: vec![m],
                        has_more: false,
                        progress: None,
                    }]);
                    assert_eq!(live.validate().is_ok(), batch.validate().is_ok());
                    live.validate()
                };
                let remote = said_by(Origin::Remote {
                    author: uid("aci:a"),
                    device: Some(1),
                });
                assert_eq!(
                    remote.is_ok(),
                    allowed,
                    "{direction:?} {state:?} from the other side"
                );
                if !allowed {
                    assert!(matches!(remote, Err(ContractViolation::CallLine(_))));
                }
                // Our own devices may say everything.
                assert_eq!(said_by(Origin::OwnOtherDevice { device: None }), Ok(()));
            }
        }
    }
}

/// `AccountReady` is the first durable event of a link (§4.3): during a link a durable envelope
/// must start with it; ephemeral login steps may come first; outside a link the rule is off.
#[test]
fn linking_envelopes_start_with_account_ready() {
    let ready = BackendEvent::AccountReady {
        own_ids: vec![uid("aci:me")],
        device: Some(2),
        display_name: None,
        number: None,
    };
    let contact = BackendEvent::ContactUpsert(ContactPatch::new(uid("aci:a")));
    let (first, _w) = Envelope::durable(vec![ready.clone(), contact.clone()]);
    assert_eq!(first.validate_linking(), Ok(()));
    let (early, _w) = Envelope::durable(vec![contact.clone(), ready]);
    assert_eq!(
        early.validate_linking(),
        Err(ContractViolation::DurableBeforeAccountReady(
            "ContactUpsert"
        ))
    );
    assert_eq!(
        early.validate(),
        Ok(()),
        "only a link in progress has the rule"
    );
    assert_eq!(
        ContractViolation::DurableBeforeAccountReady("ContactUpsert").to_string(),
        "durable event ContactUpsert before AccountReady while linking"
    );
    let step = BackendEvent::Login(LoginStep {
        step_id: "qr".into(),
        txn: 1,
        kind: LoginStepKind::Progress {
            label: "Linking".into(),
            fraction: None,
        },
    });
    assert_eq!(Envelope::ephemeral(step).validate_linking(), Ok(()));
    // The general rules still apply first.
    let (empty, _w) = Envelope::durable(vec![]);
    assert_eq!(empty.validate_linking(), Err(ContractViolation::Empty));
}

#[tokio::test]
async fn ack_completes_only_after_the_core_commit() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Envelope>(EVENT_QUEUE);
    let committed = Arc::new(AtomicBool::new(false));
    let core_committed = committed.clone();
    let core = tokio::spawn(async move {
        while let Some(env) = rx.recv().await {
            assert_eq!(env.validate(), Ok(()));
            tokio::task::yield_now().await;
            core_committed.store(true, Ordering::SeqCst); // the core.db COMMIT
            if let Some(ack) = env.ack {
                ack.complete(Ok(()));
            }
        }
    });
    let (env, waiter) = Envelope::durable(vec![BackendEvent::Message(message("1"))]);
    tx.send(env).await.expect("core is running");
    assert_eq!(waiter.wait().await, Ok(()));
    assert!(
        committed.load(Ordering::SeqCst),
        "the backend saw the ack only after the commit"
    );
    // A core that drops an envelope without completing it: the rows stay and are resent.
    let (env, waiter) = Envelope::durable(vec![BackendEvent::Message(message("2"))]);
    drop(env);
    let err = waiter.wait().await.expect_err("dropped ack");
    assert!(err.retryable);
    // A failed transaction is reported as such.
    let (ack, waiter) = Ack::pair();
    ack.complete(Err(PersistError {
        retryable: false,
        detail: "constraint".into(),
    }));
    assert!(!waiter.wait().await.expect_err("failed").retryable);
    drop(tx);
    core.await.expect("core task");
}

#[test]
fn blocking_ack_wait_for_sync_forwarders() {
    let (ack, waiter) = Ack::pair();
    let forwarder = std::thread::spawn(move || waiter.blocking_wait());
    ack.complete(Ok(()));
    assert_eq!(forwarder.join().expect("forwarder thread"), Ok(()));
}

/// A minimal backend: answers `Transmit`, emits one durable message and goes `Online` after
/// its ack, panics on `RefreshContact`, exits on `Shutdown`.
struct EchoNetwork {
    descriptor: NetworkDescriptor,
}

impl EchoNetwork {
    fn new() -> Self {
        Self {
            descriptor: NetworkDescriptor {
                display_name: "Echo".into(),
                icon_name: "network-wireless".into(),
                login_flows: vec![],
                start_chat: vec![],
                history: HistorySource::None,
                msg_id_scope: MsgIdScope::AccountAuthor,
                typing: TypingPolicy::default(),
                risk_notice: None,
            },
        }
    }
}

impl Network for EchoNetwork {
    fn kind(&self) -> NetworkKind {
        NetworkKind::Fake
    }

    fn descriptor(&self) -> &NetworkDescriptor {
        &self.descriptor
    }

    fn allocate_message_id(&self, _own: &[UserId], _conv: &ConvKey, now_ms: u64) -> AllocatedId {
        AllocatedId {
            local: now_ms.to_string().into(),
            sent_at: i64::try_from(now_ms).unwrap_or(i64::MAX),
        }
    }

    fn spawn(&self, mut ctx: AccountContext) -> Result<AccountHandle, BackendError> {
        let name = format!("echo-{}", ctx.uuid.short());
        spawn_account_thread(name, move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async move {
                let (env, waiter) = Envelope::durable(vec![BackendEvent::Message(message("1"))]);
                if ctx.events.send(env).await.is_err() || waiter.wait().await.is_err() {
                    return Exit::Failed(BackendError::internal("core gone"));
                }
                ctx.conn.send_replace(ConnState::Online { detail: None });
                while let Some(cmd) = ctx.commands.recv().await {
                    match cmd {
                        Command::Transmit { req, reply, .. } => {
                            let only = req.only.unwrap_or_default();
                            let _ = reply.send(TransmitOutcome::Sent {
                                server_at: Some(1),
                                per_recipient: only
                                    .into_iter()
                                    .map(|u| (u, RecipientState::Sent { sealed: false }))
                                    .collect(),
                            });
                        }
                        Command::RefreshContact { .. } => panic!("boom"),
                        Command::Shutdown => return Exit::Shutdown,
                        _ => {}
                    }
                }
                Exit::Shutdown
            })
        })
    }
}

fn context(rt: &tokio::runtime::Runtime) -> (AccountContext, CoreEnds) {
    AccountContext::new(ContextParams {
        account: AccountId(1),
        uuid: AccountUuid::new_random(),
        start: StartMode::Resume,
        backend_db: PathBuf::from("/nonexistent/backend.db"),
        db_key: DbKey::new([7; 32]),
        blob_dir: PathBuf::from("/nonexistent/blobs"),
        net_rt: rt.handle().clone(),
        cancel: CancellationToken::new(),
        clock: Arc::new(FakeClock::new(1_000)),
        config: BackendConfig::default(),
        ui: UiPresence::default(),
        local_prefs: LocalPrefsView::default(),
    })
}

#[test]
fn registry_spawn_command_and_exit_lifecycle() {
    let echo: Arc<dyn Network> = Arc::new(EchoNetwork::new());
    let mut registry = NetworkRegistry::new().with(echo.clone()).expect("first");
    assert_eq!(
        registry.register(echo),
        Err(RegistryError::Duplicate(NetworkKind::Fake))
    );
    assert_eq!(registry.len(), 1);
    assert_eq!(format!("{registry:?}"), "[Fake]");
    assert!(registry.get(NetworkKind::Signal).is_none());
    let net = registry.get(NetworkKind::Fake).expect("registered").clone();
    let id = net.allocate_message_id(&[uid("aci:me")], &group(), 42);
    assert_eq!((id.local.as_ref(), id.sent_at), ("42", 42));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let (ctx, mut core) = context(&rt);
    assert!(format!("{ctx:?}").contains("AccountContext"));
    assert!(!format!("{ctx:?}").contains("7, 7"), "no key bytes");
    let mut handle = net.spawn(ctx).expect("spawn");
    rt.block_on(async {
        assert_eq!(*core.conn.borrow(), ConnState::default());
        let env = core.events.recv().await.expect("first envelope");
        assert_eq!(env.validate(), Ok(()));
        env.ack.expect("durable").complete(Ok(()));
        core.conn
            .wait_for(ConnState::is_online)
            .await
            .expect("online after the ack");
        let (reply, outcome) = oneshot::channel();
        let req = OutgoingRequest {
            conv: group(),
            local: "42".into(),
            sent_at: 42,
            kind: OutgoingKind::React {
                target: target(),
                emoji: "👍".into(),
                remove: false,
            },
            only: None,
            is_recipient_update: false,
        };
        let retry = req.resend_to(vec![uid("aci:b")]);
        assert!(retry.is_recipient_update && retry.net_id() == NetMsgId::own("42"));
        core.commands
            .send(Command::Transmit {
                local: MessageId(1),
                req: Box::new(retry),
                reply,
            })
            .await
            .expect("backend alive");
        let outcome = outcome.await.expect("answered");
        assert!(matches!(&outcome, TransmitOutcome::Sent { per_recipient, .. } if per_recipient.len() == 1));
        assert!(outcome.retryable_members().is_empty());
        core.commands
            .send(Command::Shutdown)
            .await
            .expect("backend alive");
        let exit = Exit::from_channel((&mut handle.exited).await);
        assert_eq!(exit, Exit::Shutdown);
        assert!(!exit.should_restart());
    });
    handle
        .thread
        .take()
        .expect("own thread")
        .join()
        .expect("joined");

    // A panicking actor reports Exit::Panicked and is restarted by the supervisor.
    let (ctx, mut core) = context(&rt);
    let handle = net.spawn(ctx).expect("spawn");
    rt.block_on(async {
        let env = core.events.recv().await.expect("first envelope");
        env.ack.expect("durable").complete(Ok(()));
        core.commands
            .send(Command::RefreshContact { user: uid("aci:a") })
            .await
            .expect("backend alive");
        let exit = Exit::from_channel(handle.exited.await);
        assert_eq!(exit, Exit::Panicked("boom".into()));
        assert!(exit.should_restart());
    });
}

#[test]
fn account_thread_tolerates_any_name() {
    let handle = spawn_account_thread("bad\0name".into(), || Exit::Shutdown).expect("spawned");
    let thread = handle.thread.expect("own thread");
    assert_eq!(thread.thread().name(), Some("badname"));
    thread.join().expect("joined");
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    assert_eq!(
        Exit::from_channel(rt.block_on(handle.exited)),
        Exit::Shutdown
    );
}

#[test]
fn dropped_exit_sender_reads_as_panic() {
    let (tx, rx) = oneshot::channel::<Exit>();
    drop(tx);
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    assert!(matches!(
        Exit::from_channel(rt.block_on(rx)),
        Exit::Panicked(_)
    ));
}

#[test]
fn durable_commands_and_outcomes() {
    let d = DurableCommand::SyncReadMarker {
        conv: group(),
        up_to: target(),
        read: vec![],
    };
    let cmd = d.into_command(CmdId(9));
    assert_eq!(
        (cmd.name(), cmd.durable_id()),
        ("SyncReadMarker", Some(CmdId(9)))
    );
    assert_eq!(Command::Connect.durable_id(), None);
    // The deletes made on this device are durable commands too, and their persisted form
    // keeps every field.
    let clear = DurableCommand::ClearConversation {
        conv: group(),
        newest: vec![target()],
        newest_non_expiring: Vec::new(),
        remove: true,
    };
    match clear.clone().into_command(CmdId(10)) {
        Command::ClearConversation {
            cmd,
            conv,
            newest,
            newest_non_expiring,
            remove,
        } => {
            assert_eq!((cmd, conv, remove), (CmdId(10), group(), true));
            assert_eq!((newest, newest_non_expiring), (vec![target()], Vec::new()));
        }
        other => panic!("unexpected {}", other.name()),
    }
    let cmd = clear.into_command(CmdId(10));
    assert_eq!(
        (cmd.name(), cmd.durable_id()),
        ("ClearConversation", Some(CmdId(10)))
    );
    let secret = "uuid:5ec2e7";
    let gone = DurableCommand::DeleteAttachment {
        conv: group(),
        target: target(),
        index: 1,
        net_ids: vec![secret.to_owned()].into(),
    };
    assert!(
        !format!("{gone:?}").contains(secret),
        "the names of an attachment name content"
    );
    let cmd = gone.into_command(CmdId(11));
    assert_eq!(
        (cmd.name(), cmd.durable_id()),
        ("DeleteAttachment", Some(CmdId(11)))
    );
    assert!(!format!("{cmd:?}").contains(secret));
    // Rows written before a field existed still decode.
    let old: DurableCommand = serde_json::from_str(&format!(
        r#"{{"ClearConversation":{{"conv":{}}}}}"#,
        serde_json::to_string(&group()).expect("key")
    ))
    .expect("an old row");
    assert!(matches!(
        old,
        DurableCommand::ClearConversation { remove: false, ref newest, .. } if newest.is_empty()
    ));
    assert_eq!(kourier_backend_api::CLEAR_ANCHORS, 5);
    let partial = TransmitOutcome::Partial {
        delivered: vec![(uid("aci:a"), RecipientState::Sent { sealed: true })],
        failed: vec![
            (uid("aci:b"), BackendError::transient("timeout")),
            (
                uid("aci:c"),
                BackendError::new(ErrorKind::PeerUnregistered, "404"),
            ),
        ],
    };
    assert_eq!(partial.retryable_members(), vec![uid("aci:b")]);
    assert_eq!(partial.permanently_failed(), vec![uid("aci:c")]);
}

#[test]
fn debug_of_events_and_commands_is_log_safe() {
    let secret = "SECRET";
    let mut msg = message("1");
    msg.body = Body::Media {
        attachments: vec![Attachment {
            file_name: Some(format!("{secret}.pdf")),
            caption: Some(Text::plain(secret)),
            ..Attachment::default()
        }],
        caption: Some(Text::plain(secret)),
    };
    let events = [
        BackendEvent::Message(msg),
        BackendEvent::AccountReady {
            own_ids: vec![uid("aci:me")],
            device: None,
            display_name: Some(Redacted::new(secret.into())),
            number: Some(Redacted::new(format!("+4915{secret}"))),
        },
        BackendEvent::AttachmentDeleted {
            target: target(),
            net_ids: Redacted::new(vec![secret.into()]),
            at: 1,
        },
        BackendEvent::ContactUpsert(ContactPatch {
            profile_name: Patch::Set(secret.into()),
            system_name: Patch::Set(secret.into()),
            identifiers: Some(vec![Identifier {
                kind: IdentifierKind::Phone,
                value: format!("+49{secret}"),
            }]),
            ..ContactPatch::new(uid("e164:+4915112345678"))
        }),
        BackendEvent::ConversationUpsert(ConversationPatch {
            title: Patch::Set(secret.into()),
            ..ConversationPatch::new(group())
        }),
        BackendEvent::Login(LoginStep {
            step_id: "qr".into(),
            txn: 1,
            kind: LoginStepKind::ShowQr {
                data: Redacted::new(secret.into()),
                valid_for: None,
            },
        }),
        BackendEvent::Notice(BackendNotice::Error(
            BackendError::internal("x").with_user_message(secret),
        )),
    ];
    for e in &events {
        let s = format!("{e:?} {e:#?}");
        assert!(!s.contains(secret) && !s.contains("4915"), "{s}");
    }
    let (reply, _rx) = oneshot::channel();
    let commands = [
        Command::SubmitLoginInput {
            txn: 1,
            values: vec![("password".into(), Redacted::new(secret.into()))],
        },
        Command::ChangeContact {
            user: uid("aci:a"),
            change: ContactChange::SetNickname {
                nickname: Some(secret.into()),
            },
            reply,
        },
    ];
    for c in &commands {
        assert!(!format!("{c:?}").contains(secret), "{c:?}");
    }
    let config = BackendConfig {
        proxy: Some(ProxyConfig {
            url: Redacted::new(format!("socks5://user:{secret}@host")),
        }),
        ..BackendConfig::default()
    };
    assert!(!format!("{config:?}").contains(secret));
    assert_eq!(format!("{:?}", DbKey::new([1; 32])), "DbKey([redacted])");
}

#[tokio::test(flavor = "current_thread")]
async fn fake_clock_drives_async_timers() {
    let clock = FakeClock::new(0);
    let shared: Arc<dyn Clock> = Arc::new(clock.clone());
    let task = tokio::spawn(async move {
        // A restart backoff: 1 s, then 2 s.
        shared.sleep(Duration::from_secs(1)).await;
        let first = shared.now_ms();
        shared.sleep(Duration::from_secs(2)).await;
        (first, shared.now_ms())
    });
    while clock.sleepers() == 0 {
        tokio::task::yield_now().await;
    }
    assert_eq!(clock.advance_to_next(), Some(1_000));
    while clock.next_deadline() != Some(3_000) {
        tokio::task::yield_now().await;
    }
    clock.advance(Duration::from_secs(2));
    assert_eq!(task.await.expect("task"), (1_000, 3_000));
}

/// `AttachmentDone.request` is additive: a `core_outbox` row written before it existed still
/// decodes (as `None`), and a new row carries it.
#[test]
fn attachment_done_rows_without_a_request_id_still_decode() {
    let mut old = OUTBOX_FORMAT.to_le_bytes().to_vec();
    old.extend_from_slice(br#"[{"AttachmentDone":{"attachment":3,"result":{"Ok":{"size":2}}}}]"#);
    let events = decode_outbox_row(&old).expect("old row decodes");
    assert!(matches!(
        events.as_slice(),
        [BackendEvent::AttachmentDone {
            attachment: AttachmentId(3),
            request: None,
            result: Ok(_),
        }]
    ));
    let new = encode_outbox_row(&[BackendEvent::AttachmentDone {
        attachment: AttachmentId(3),
        request: Some(DownloadId(7)),
        result: Ok(AttachmentMeta::default()),
    }])
    .expect("encode");
    let json = std::str::from_utf8(&new[2..]).expect("utf-8");
    assert!(json.contains(r#""request":7"#), "{json}");
}

/// `AccountReady.number` is additive: a row written before it existed decodes with `None`
/// (which leaves the core's number alone), a row without a number does not write the field, and
/// one with a number carries it.
#[test]
fn account_ready_rows_without_a_number_still_decode() {
    let mut old = OUTBOX_FORMAT.to_le_bytes().to_vec();
    old.extend_from_slice(br#"[{"AccountReady":{"own_ids":["aci:me"],"device":2}}]"#);
    let events = decode_outbox_row(&old).expect("old row decodes");
    assert!(matches!(
        events.as_slice(),
        [BackendEvent::AccountReady {
            number: None,
            device: Some(2),
            ..
        }]
    ));
    let json = |number: Option<&str>| {
        let row = encode_outbox_row(&[BackendEvent::AccountReady {
            own_ids: vec![uid("aci:me")],
            device: None,
            display_name: None,
            number: number.map(|n| Redacted::new(n.to_owned())),
        }])
        .expect("encode");
        String::from_utf8_lossy(&row[2..]).into_owned()
    };
    assert!(!json(None).contains("number"));
    assert!(json(Some("+15550100")).contains(r#""number":"+15550100""#));
}

/// The ids inside the delete-for-me events are checked like those of every other event.
#[test]
fn clear_and_attachment_delete_targets_are_checked() {
    let mut bad = target();
    bad.local = "".into();
    for (name, event) in [
        (
            "ConversationCleared",
            BackendEvent::ConversationCleared {
                conv: group(),
                up_to: vec![target(), bad.clone()],
                remove: false,
                at: 1,
            },
        ),
        (
            "AttachmentDeleted",
            BackendEvent::AttachmentDeleted {
                target: bad.clone(),
                net_ids: Redacted::new(vec!["uuid:1".into()]),
                at: 1,
            },
        ),
    ] {
        assert!(!event.is_ephemeral(), "{name} is durable");
        let (env, _w) = Envelope::durable(vec![event]);
        assert_eq!(
            env.validate(),
            Err(ContractViolation::InvalidId {
                event: name,
                error: IdError::Empty,
            })
        );
    }
    // A clear without anchors is well-formed (Signal's local-only conversation delete).
    let (env, _w) = Envelope::durable(vec![BackendEvent::ConversationCleared {
        conv: group(),
        up_to: Vec::new(),
        remove: true,
        at: 1,
    }]);
    assert_eq!(env.validate(), Ok(()));
}

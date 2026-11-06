//! THE HEAD'S WAIT AND THE CONTACT TRANSACTION (TH6 L5; brief §2.10 with the critic's T-H3 and T-L2;
//! `TP/csp.struct.yml:762-823`, `TP/md.md:75-102`, `TP/md-d2m.proto:236-317`): a message whose sender needs the
//! DIRECTORY or whose steps 22.4-22.6 need a `CONTACT_SYNC` transaction waits at the head of the leader's FIFO (the
//! messages behind it wait too: the spec runs the steps one message at a time). Sans-I/O: the connection writes the
//! frames and feeds the answers in ([`Head::on_event`], [`Head::on_reflect_ack`]).
//!
//! THE MACHINE: `BeginTransaction { scope CONTACT_SYNC sealed with DGTSK, ttl 0 = the server's maximum }` ->
//! `BeginTransactionAck` -> THE PRECONDITION READ AGAIN (critic T-L2: a `ContactSync` the phone reflected meanwhile is
//! in the file by then; the steps are decided again, and when they no longer ask for this change the transaction is
//! ended gracefully by a commit with nothing reflected, `TP/md.md:96-98`) -> the `ContactSync` reflect (its D2D nonce
//! stored first) -> its reflect-ack -> `CommitTransaction` -> `CommitTransactionAck` -> the message joins a RECEIVE batch
//! alone CARRYING the committed delta, which ZONE RECEIVE applies through the follower's `sync::apply_contact` (one
//! mapper for both roles: the phone gets the same delta at the commit). `TransactionRejected` -> wait for
//! `TransactionEnded` (bounded by [`TX_WAIT`]) -> the steps decided again (the phone's own create makes ours a no-op),
//! and Begin again when they still ask. A head that would begin a transaction a 4th time ends the socket (the spec's
//! "4th iteration": exceptionally abort the connection). A `TransactionEnded` while not waiting is ignored.
//!
//! Nothing here logs; `Debug` prints states only (no ID, nickname or key).

use std::fmt;
use std::time::Duration;

use libthreema::protobuf::common::Unit;
use libthreema::protobuf::d2d;
use libthreema::protobuf::d2d::transaction_scope::Scope;
use libthreema::protobuf::d2d_sync::{self, contact};
use prost::Message as _;
use zeroize::Zeroizing;

use super::directory::Looked;
use super::steps::Change;
use crate::send::encode;

/// How long a rejected transaction waits for the holder's `TransactionEnded` (brief §2.9: then close 1000 `NoAck`).
pub(crate) const TX_WAIT: Duration = Duration::from_secs(120);

/// The most transactions one head begins (the spec's loop of 22.6: the 4th ends the connection).
pub(crate) const MOST_BEGINS: u32 = 3;

/// The message at the head of the FIFO that waits, and what for.
pub(crate) struct Head {
    /// The whole `message-with-metadata-box`.
    pub(crate) bytes: Vec<u8>,
    /// The directory's answer for its sender, once it came (the steps' [`super::steps::Input::looked`]).
    pub(crate) looked: Option<Looked>,
    /// The plaintext `d2d.Envelope { contact_sync }` of the committed transaction (ZONE RECEIVE applies it first).
    pub(crate) committed: Option<Zeroizing<Vec<u8>>>,
    /// How many transactions this head began.
    pub(crate) begins: u32,
    pub(crate) wait: Wait,
}

impl fmt::Debug for Head {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Head")
            .field("looked", &self.looked.is_some())
            .field("committed", &self.committed.is_some())
            .field("begins", &self.begins)
            .field("wait", &self.wait)
            .finish_non_exhaustive()
    }
}

/// What the head waits for.
pub(crate) enum Wait {
    /// The directory's answer (the call runs as a future of the socket's loop).
    Lookup,
    /// Nothing: the steps are decided again at the next turn.
    Ready,
    /// `BeginTransaction` went: its ack or `TransactionRejected`.
    Begun(Change),
    /// Rejected: the holder's `TransactionEnded`.
    Rejected,
    /// The `ContactSync` reflect went under `reflect_id`: its reflect-ack.
    Reflected {
        reflect_id: u32,
        plaintext: Zeroizing<Vec<u8>>,
    },
    /// `CommitTransaction` went: its ack. `Some`: the delta it commits; `None`: the graceful end with nothing (T-L2).
    Committing(Option<Zeroizing<Vec<u8>>>),
}

impl fmt::Debug for Wait {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Lookup => "Lookup",
            Self::Ready => "Ready",
            Self::Begun(_) => "Begun",
            Self::Rejected => "Rejected",
            Self::Reflected { .. } => "Reflected",
            Self::Committing(Some(_)) => "Committing",
            Self::Committing(None) => "CommittingNothing",
        })
    }
}

/// A transaction answer of the mediator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TxEvent {
    BeginAck,
    Rejected,
    Ended,
    CommitAck,
}

/// What the connection does after an answer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TxNext {
    /// Not this head's: nothing.
    Ignored,
    /// The lock is ours: decide the steps again (T-L2) and reflect `change` when they still ask for it, else commit
    /// with nothing.
    Precondition(Change),
    /// Rejected: wait for the holder's end ([`TX_WAIT`]).
    WaitEnded,
    /// The holder ended: decide the steps again.
    Recheck,
    /// The commit was acked: decide the steps again (with the delta when one was committed).
    Committed,
}

impl Head {
    /// A head that waits for the directory.
    pub(crate) fn lookup(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            looked: None,
            committed: None,
            begins: 0,
            wait: Wait::Lookup,
        }
    }

    /// A head whose steps ask for `change` before anything else (the caller begins the transaction).
    pub(crate) fn contact(bytes: Vec<u8>, looked: Option<Looked>) -> Self {
        Self {
            bytes,
            looked,
            committed: None,
            begins: 0,
            wait: Wait::Ready,
        }
    }

    /// Whether the head waits for an answer of the mediator under the socket's `ACK_WITHIN` (a rejected head waits for
    /// another device's end under [`TX_WAIT`] instead).
    pub(crate) fn awaits_mediator(&self) -> bool {
        matches!(
            self.wait,
            Wait::Begun(_) | Wait::Reflected { .. } | Wait::Committing(_)
        )
    }

    /// The answer `event` of the mediator.
    pub(crate) fn on_event(&mut self, event: TxEvent) -> TxNext {
        match (std::mem::replace(&mut self.wait, Wait::Ready), event) {
            (Wait::Begun(change), TxEvent::BeginAck) => TxNext::Precondition(change),
            (Wait::Begun(_), TxEvent::Rejected) => {
                self.wait = Wait::Rejected;
                TxNext::WaitEnded
            }
            (Wait::Rejected, TxEvent::Ended) => TxNext::Recheck,
            (Wait::Committing(delta), TxEvent::CommitAck) => {
                if delta.is_some() {
                    self.committed = delta;
                }
                TxNext::Committed
            }
            (wait, _) => {
                self.wait = wait;
                TxNext::Ignored
            }
        }
    }

    /// A reflect-ack: whether it was this head's `ContactSync` reflect (the head then commits).
    pub(crate) fn on_reflect_ack(&mut self, reflect_id: u32) -> bool {
        match std::mem::replace(&mut self.wait, Wait::Ready) {
            Wait::Reflected {
                reflect_id: id,
                plaintext,
            } if id == reflect_id => {
                self.wait = Wait::Committing(Some(plaintext));
                true
            }
            wait => {
                self.wait = wait;
                false
            }
        }
    }
}

/// The encoded `d2d.TransactionScope { CONTACT_SYNC }` (sealed with DGTSK by `reflect::Keys::seal_scope`).
pub(crate) fn contact_scope() -> Vec<u8> {
    d2d::TransactionScope {
        scope: Scope::ContactSync as i32,
    }
    .encode_to_vec()
}

/// The plaintext `d2d.Envelope { contact_sync }` of `change` from `device` at `now_ms`, padded as the send core pads.
pub(crate) fn contact_sync(
    device: u64,
    change: &Change,
    now_ms: u64,
    draw: u16,
) -> Zeroizing<Vec<u8>> {
    let action = match change {
        Change::Create { init, nickname } => {
            d2d::contact_sync::Action::Create(d2d::contact_sync::Create {
                contact: Some(created(init, nickname.clone(), now_ms)),
            })
        }
        Change::Update {
            identity,
            nickname,
            direct,
        } => d2d::contact_sync::Action::Update(d2d::contact_sync::Update {
            contact: Some(d2d_sync::Contact {
                identity: identity.clone(),
                nickname: nickname.clone(),
                acquaintance_level: direct.then_some(contact::AcquaintanceLevel::Direct as i32),
                ..Default::default()
            }),
        }),
    };
    encode::envelope_bytes(
        device,
        d2d::envelope::Content::ContactSync(d2d::ContactSync {
            action: Some(action),
        }),
        draw,
    )
}

/// 22.4.2's `ContactSync.Create`: the directory's init, `created_at` now, the nickname, DIRECT, every policy and
/// category at its default (`TP/csp.struct.yml:779-783`; libthreema's `ContactInit` encoding for the rest). TH6 FIX-B,
/// P-L3: a PREDEFINED contact (the Valid Contacts Lookup Steps 8.4.3.3, `:1320-1330`) is made `FULLY_VERIFIED` with the
/// spec's nickname, whatever the message's own says; any other ID is `UNVERIFIED` with the message's nickname.
#[expect(
    deprecated,
    reason = "the deprecated sound override must be provided until phased out (libthreema's encoding)"
)]
fn created(
    init: &super::directory::ContactInit,
    nickname: Option<String>,
    now_ms: u64,
) -> d2d_sync::Contact {
    use contact::{
        deprecated_notification_sound_policy_override as dns,
        notification_trigger_policy_override as ntp, read_receipt_policy_override as rrp,
        typing_indicator_policy_override as tip,
    };
    let (verification, nickname) = match super::directory::predefined_nickname(&init.identity) {
        Some(predefined) => (
            contact::VerificationLevel::FullyVerified,
            predefined.to_owned(),
        ),
        None => (
            contact::VerificationLevel::Unverified,
            nickname.unwrap_or_default(),
        ),
    };
    d2d_sync::Contact {
        identity: init.identity.clone(),
        public_key: Some(init.public_key.to_vec()),
        created_at: Some(now_ms),
        first_name: Some(String::new()),
        last_name: Some(String::new()),
        nickname: Some(nickname),
        verification_level: Some(verification as i32),
        work_verification_level: Some(contact::WorkVerificationLevel::None as i32),
        identity_type: Some(if init.work {
            contact::IdentityType::Work as i32
        } else {
            contact::IdentityType::Regular as i32
        }),
        acquaintance_level: Some(contact::AcquaintanceLevel::Direct as i32),
        activity_state: Some(if init.inactive {
            contact::ActivityState::Inactive as i32
        } else {
            contact::ActivityState::Active as i32
        }),
        feature_mask: Some(init.feature_mask),
        sync_state: Some(contact::SyncState::Initial as i32),
        work_last_full_sync_at: None,
        work_availability_status: None,
        read_receipt_policy_override: Some(contact::ReadReceiptPolicyOverride {
            r#override: Some(rrp::Override::Default(Unit {})),
        }),
        typing_indicator_policy_override: Some(contact::TypingIndicatorPolicyOverride {
            r#override: Some(tip::Override::Default(Unit {})),
        }),
        notification_trigger_policy_override: Some(contact::NotificationTriggerPolicyOverride {
            r#override: Some(ntp::Override::Default(Unit {})),
        }),
        deprecated_notification_sound_policy_override: Some(
            contact::DeprecatedNotificationSoundPolicyOverride {
                r#override: Some(dns::Override::Default(Unit {})),
            },
        ),
        contact_defined_profile_picture: None,
        user_defined_profile_picture: None,
        conversation_category: Some(d2d_sync::ConversationCategory::Default as i32),
        conversation_visibility: Some(d2d_sync::ConversationVisibility::Normal as i32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::leader::directory::ContactInit;

    fn init() -> ContactInit {
        ContactInit {
            identity: "PEER0001".to_owned(),
            public_key: [5; 32],
            work: true,
            inactive: false,
            feature_mask: 0x0f,
        }
    }

    /// The machine's answers in order: begin -> rejected -> ended -> begin -> ack -> the reflect's ack -> commit-ack;
    /// an answer of another state is ignored and changes nothing.
    #[test]
    fn the_head_takes_each_answer_only_in_its_state() {
        let change = Change::Create {
            init: init(),
            nickname: None,
        };
        let mut head = Head::lookup(vec![1, 2]);
        assert_eq!(head.on_event(TxEvent::Ended), TxNext::Ignored);
        assert!(matches!(head.wait, Wait::Lookup));
        head.wait = Wait::Begun(change.clone());
        assert!(head.awaits_mediator());
        assert_eq!(head.on_event(TxEvent::Rejected), TxNext::WaitEnded);
        assert_eq!(head.on_event(TxEvent::CommitAck), TxNext::Ignored);
        assert_eq!(head.on_event(TxEvent::Ended), TxNext::Recheck);
        head.wait = Wait::Begun(change.clone());
        assert_eq!(
            head.on_event(TxEvent::BeginAck),
            TxNext::Precondition(change)
        );
        head.wait = Wait::Reflected {
            reflect_id: 7,
            plaintext: Zeroizing::new(vec![9]),
        };
        assert!(!head.on_reflect_ack(8));
        assert!(head.on_reflect_ack(7));
        assert!(matches!(head.wait, Wait::Committing(Some(_))));
        assert_eq!(head.on_event(TxEvent::CommitAck), TxNext::Committed);
        assert_eq!(head.committed.as_deref().map(Vec::as_slice), Some(&[9][..]));
        assert!(!head.awaits_mediator());
        // A graceful end with nothing keeps an earlier delta and adds none.
        head.wait = Wait::Committing(None);
        assert_eq!(head.on_event(TxEvent::CommitAck), TxNext::Committed);
        assert!(head.committed.is_some());
        assert!(!format!("{head:?}").contains("PEER"));
    }

    /// The create carries the init, the time, the nickname, DIRECT and the defaults; the update only what changes; the
    /// scope opens as CONTACT_SYNC.
    #[test]
    fn the_contact_sync_carries_the_change_and_nothing_else() {
        let plain = contact_sync(
            3,
            &Change::Create {
                init: init(),
                nickname: Some("Nick".to_owned()),
            },
            1_700,
            0,
        );
        let envelope = d2d::Envelope::decode(plain.as_slice()).expect("an envelope");
        assert_eq!(envelope.device_id, 3);
        let Some(d2d::envelope::Content::ContactSync(d2d::ContactSync {
            action: Some(d2d::contact_sync::Action::Create(create)),
        })) = envelope.content
        else {
            panic!("not a create");
        };
        let c = create.contact.expect("a contact");
        assert_eq!(c.identity, "PEER0001");
        assert_eq!(c.public_key.as_deref(), Some(&[5; 32][..]));
        assert_eq!(c.created_at, Some(1_700));
        assert_eq!(c.nickname.as_deref(), Some("Nick"));
        assert_eq!(
            c.acquaintance_level,
            Some(contact::AcquaintanceLevel::Direct as i32)
        );
        assert_eq!(c.identity_type, Some(contact::IdentityType::Work as i32));
        assert_eq!(c.feature_mask, Some(0x0f));
        let plain = contact_sync(
            3,
            &Change::Update {
                identity: "PEER0002".to_owned(),
                nickname: None,
                direct: true,
            },
            1_700,
            0,
        );
        let envelope = d2d::Envelope::decode(plain.as_slice()).expect("an envelope");
        let Some(d2d::envelope::Content::ContactSync(d2d::ContactSync {
            action: Some(d2d::contact_sync::Action::Update(update)),
        })) = envelope.content
        else {
            panic!("not an update");
        };
        assert_eq!(
            update.contact,
            Some(d2d_sync::Contact {
                identity: "PEER0002".to_owned(),
                acquaintance_level: Some(contact::AcquaintanceLevel::Direct as i32),
                ..Default::default()
            })
        );
        let scope = d2d::TransactionScope::decode(contact_scope().as_slice()).expect("a scope");
        assert_eq!(scope.scope, Scope::ContactSync as i32);
    }

    /// The `Create` of `identity` from the directory for a message with `nickname`.
    fn created_of(identity: &str, nickname: Option<&str>) -> d2d_sync::Contact {
        let init = ContactInit {
            identity: identity.to_owned(),
            ..init()
        };
        let plain = contact_sync(
            3,
            &Change::Create {
                init,
                nickname: nickname.map(str::to_owned),
            },
            1_700,
            0,
        );
        let envelope = d2d::Envelope::decode(plain.as_slice()).expect("an envelope");
        let Some(d2d::envelope::Content::ContactSync(d2d::ContactSync {
            action: Some(d2d::contact_sync::Action::Create(create)),
        })) = envelope.content
        else {
            panic!("not a create");
        };
        create.contact.expect("a contact")
    }

    /// P-L3 (`csp.struct.yml:1320-1330`): a contact made from the directory for a PREDEFINED ID is FULLY_VERIFIED and
    /// carries the spec's nickname, not the message's (also none); any other ID stays UNVERIFIED with the message's own
    /// nickname. Every predefined ID is checked, the key and the rest of the create unchanged.
    #[test]
    fn a_predefined_contact_is_created_fully_verified_with_the_specs_nickname() {
        use crate::leader::directory::{PREDEFINED, PREDEFINED_NICKNAMES};
        for ((id, _, _), (_, nickname)) in PREDEFINED.iter().zip(PREDEFINED_NICKNAMES) {
            for given in [None, Some("Imposter")] {
                let c = created_of(id, given);
                assert_eq!(c.identity, *id);
                assert_eq!(c.nickname.as_deref(), Some(nickname), "{id} {given:?}");
                assert_eq!(
                    c.verification_level,
                    Some(contact::VerificationLevel::FullyVerified as i32),
                    "{id}"
                );
                assert_eq!(
                    c.acquaintance_level,
                    Some(contact::AcquaintanceLevel::Direct as i32),
                    "{id}"
                );
                assert_eq!(c.created_at, Some(1_700));
                assert_eq!(c.first_name.as_deref(), Some(""));
                assert_eq!(c.last_name.as_deref(), Some(""));
            }
        }
        let c = created_of("PEER0001", Some("Nick"));
        assert_eq!(c.nickname.as_deref(), Some("Nick"));
        assert_eq!(
            c.verification_level,
            Some(contact::VerificationLevel::Unverified as i32)
        );
        assert_eq!(created_of("PEER0001", None).nickname.as_deref(), Some(""));
    }
}

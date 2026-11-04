//! The PHONE's builders of `d2d.Envelope`s (TH5 A2; `TP/md-d2d.proto`, `TP/md-d2d-sync.proto`): every kind
//! Kourier's follower receive will classify, written from the spec on the fake's own crypto, and the
//! helper that seals one with the fake's DGRK and reflects it through the fake mediator. The CSP message
//! bodies they carry are [`super::body`]'s.
//!
//! - [`Author`] is the reflecting device: its device id, the protocol version it announces (V0.3 by
//!   default, V0.1 or 0 for the desync cases) and the length of the `padding` ("random amount of
//!   padding, ignored by the receiver", `TP/md-d2d.proto:126`); its methods return the envelope.
//! - [`Msg`] is one CSP message (id, `created_at`, type, body, the CSP nonce(s) the reflection carries).
//! - [`ContactDelta`] / [`GroupDelta`] build the sync deltas the way the spec says to send them: only the
//!   identity and the fields that changed (`TP/md-d2d-sync.proto:7-17`); an empty string is a value
//!   ("An empty string is valid", `:255-275`), not an absence.
//! - [`Reflect`] reflects an [`Author`]'s envelope through a [`Phone`]: flags 0, or `EPHEMERAL` where the
//!   spec reflects a message that way (typing), or whatever the test says.
//!
//! [U: real phone] marks what the pinned spec does not fix: which protocol version a real phone
//! announces, whether it writes the CSP nonce of a reflected message always, whether a contact is ever
//! removed by anything but `acquaintance_level = GROUP_OR_DELETED` (the proto has no `ContactSync.Delete`).
//!
//! Nothing here prints a body, an ID or a key: the types print counts.

use libthreema::protobuf::{common, d2d, d2d_sync};
use prost::Message as _;

use super::body::{self, MsgId, id_u64, ty};
use super::d2m::EPHEMERAL;
use super::phone::Phone;

/// The conversation an envelope names (`d2d.ConversationId`).
#[derive(Clone, PartialEq, Eq)]
pub enum Conv {
    /// A 1:1 conversation: the peer's Threema ID.
    Contact(String),
    /// A group: its creator's Threema ID and its group id.
    Group { creator: String, id: u64 },
    /// A distribution list: its id.
    List(u64),
}

impl std::fmt::Debug for Conv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Contact(_) => "Conv::Contact",
            Self::Group { .. } => "Conv::Group",
            Self::List(_) => "Conv::List",
        })
    }
}

impl Conv {
    pub fn contact(identity: &str) -> Self {
        Self::Contact(identity.to_owned())
    }

    pub fn group(creator: &str, id: u64) -> Self {
        Self::Group {
            creator: creator.to_owned(),
            id,
        }
    }

    fn id(&self) -> d2d::ConversationId {
        use d2d::conversation_id::Id;
        d2d::ConversationId {
            id: Some(match self {
                Self::Contact(identity) => Id::Contact(identity.clone()),
                Self::Group { creator, id } => Id::Group(common::GroupIdentity {
                    group_id: *id,
                    creator_identity: creator.clone(),
                }),
                Self::List(id) => Id::DistributionList(*id),
            }),
        }
    }
}

/// One CSP E2E message as a phone reflects it: the id (8 wire bytes), `created_at` (ms), the type
/// number, the body (the unpadded inner struct, wrapped for a group) and the CSP nonce(s) of the
/// `message-with-metadata-box` that carried it ("Optional for now", `TP/md-d2d.proto:178`: none by
/// default).
#[derive(Clone, PartialEq, Eq)]
pub struct Msg {
    pub id: MsgId,
    pub created_at: u64,
    pub ty: u8,
    pub body: Vec<u8>,
    pub nonces: Vec<[u8; 24]>,
    /// `OutgoingMessage.thread_message_id`.
    pub thread: Option<MsgId>,
}

impl std::fmt::Debug for Msg {
    /// The type and the length of the body only.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Msg(type {:#04x}, {} B)", self.ty, self.body.len())
    }
}

impl Msg {
    /// A message of any type with a ready body (an unknown type, 0x00, a kind Kourier keeps raw).
    pub fn raw(id: MsgId, created_at: u64, ty: u8, body: Vec<u8>) -> Self {
        Self {
            id,
            created_at,
            ty,
            body,
            nonces: Vec::new(),
            thread: None,
        }
    }

    /// `e2e.text`.
    pub fn text(id: MsgId, created_at: u64, text: &str) -> Self {
        Self::raw(id, created_at, ty::TEXT, body::text(text))
    }

    /// `e2e.location`.
    pub fn location(
        id: MsgId,
        created_at: u64,
        lat: &str,
        lon: &str,
        accuracy: Option<&str>,
        place: &body::Place,
    ) -> Self {
        Self::raw(
            id,
            created_at,
            ty::LOCATION,
            body::location(lat, lon, accuracy, place),
        )
    }

    /// `e2e.file`.
    pub fn file(id: MsgId, created_at: u64, spec: &body::FileSpec) -> Self {
        Self::raw(id, created_at, ty::FILE, body::file(spec))
    }

    /// `e2e.delivery-receipt` (status 1 received, 2 read, 3 and 4 the deprecated reactions).
    pub fn receipt(id: MsgId, created_at: u64, status: u8, ids: &[MsgId]) -> Self {
        Self::raw(
            id,
            created_at,
            ty::DELIVERY_RECEIPT,
            body::delivery_receipt(status, ids),
        )
    }

    /// `e2e.typing-indicator` (`is_typing` 1 or 0; other bytes are invalid on purpose).
    pub fn typing(id: MsgId, created_at: u64, is_typing: u8) -> Self {
        Self::raw(
            id,
            created_at,
            ty::TYPING_INDICATOR,
            body::typing(is_typing),
        )
    }

    /// `csp_e2e.EditMessage`.
    pub fn edit(id: MsgId, created_at: u64, target: MsgId, text: &str) -> Self {
        Self::raw(
            id,
            created_at,
            ty::EDIT_MESSAGE,
            body::edit_message(target, text),
        )
    }

    /// `csp_e2e.DeleteMessage`.
    pub fn delete(id: MsgId, created_at: u64, target: MsgId) -> Self {
        Self::raw(
            id,
            created_at,
            ty::DELETE_MESSAGE,
            body::delete_message(target),
        )
    }

    /// `csp_e2e.Reaction` apply.
    pub fn reaction(id: MsgId, created_at: u64, target: MsgId, emoji: &str) -> Self {
        Self::raw(
            id,
            created_at,
            ty::REACTION,
            body::reaction_apply(target, emoji.as_bytes()),
        )
    }

    /// `csp_e2e.Reaction` withdraw.
    pub fn withdraw(id: MsgId, created_at: u64, target: MsgId, emoji: &str) -> Self {
        Self::raw(
            id,
            created_at,
            ty::REACTION,
            body::reaction_withdraw(target, emoji.as_bytes()),
        )
    }

    /// The same message sent into the group of `creator` and `group_id`: the type becomes its group
    /// form ([`body::group_type`]) and the body gets the `group-member-container`.
    #[track_caller]
    pub fn in_group(mut self, creator: &str, group_id: u64) -> Self {
        self.ty = body::group_type(self.ty).expect("this message type has a group form");
        self.body = body::group_member_container(creator, group_id, &self.body);
        self
    }

    /// Adds the CSP nonce a reflection carries.
    pub fn nonce(mut self, nonce: [u8; 24]) -> Self {
        self.nonces.push(nonce);
        self
    }

    /// Sets `thread_message_id` (an outgoing message only).
    pub fn thread(mut self, id: MsgId) -> Self {
        self.thread = Some(id);
        self
    }
}

/// The member changes of `GroupSync.Update.member_state_changes`.
pub use d2d::group_sync::update::MemberStateChange as MemberChange;

/// The device that reflects: its id, the protocol version it announces and the padding length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Author {
    pub device_id: u64,
    /// `Envelope.protocol_version`: V0.3 by default; 1 is V0.1 and 0 means "assume V0.1" (the spec,
    /// `TP/md-d2d.proto:130-133`).
    pub protocol_version: u32,
    /// How many bytes of `Envelope.padding`.
    pub padding: usize,
}

/// The default padding length: not zero, so a reader that trips over padding fails at once.
pub const PADDING: usize = 21;

impl Author {
    /// A V0.3 author with [`PADDING`] bytes of padding.
    pub fn new(device_id: u64) -> Self {
        Self {
            device_id,
            protocol_version: d2d::ProtocolVersion::V03 as u32,
            padding: PADDING,
        }
    }

    /// Announces V0.1 (a group control message is then the group's only carrier).
    pub fn v01(mut self) -> Self {
        self.protocol_version = d2d::ProtocolVersion::V01 as u32;
        self
    }

    /// Announces no version (`0`): a reader assumes V0.1.
    pub fn unversioned(mut self) -> Self {
        self.protocol_version = 0;
        self
    }

    pub fn padding(mut self, len: usize) -> Self {
        self.padding = len;
        self
    }

    /// An envelope around `content`.
    pub fn envelope(&self, content: d2d::envelope::Content) -> d2d::Envelope {
        d2d::Envelope {
            #[expect(deprecated, reason = "the spec's padding field")]
            padding: (0..self.padding)
                .map(|i| 0xa5 ^ u8::try_from(i % 251).expect("below 256"))
                .collect(),
            device_id: self.device_id,
            protocol_version: self.protocol_version,
            content: Some(content),
        }
    }

    /// An envelope whose `content` is not set (what a reader of an older build sees for a oneof
    /// member it does not know).
    pub fn empty(&self) -> d2d::Envelope {
        let mut envelope = self.envelope(d2d::envelope::Content::MdmParameterSync(
            d2d::MdmParameterSync::default(),
        ));
        envelope.content = None;
        envelope
    }

    // --- Messages -------------------------------------------------------------------------

    /// `IncomingMessage`: the phone received `msg` from `sender` and reflects it.
    pub fn incoming(&self, sender: &str, msg: &Msg) -> d2d::Envelope {
        self.envelope(d2d::envelope::Content::IncomingMessage(
            d2d::IncomingMessage {
                sender_identity: sender.to_owned(),
                message_id: id_u64(msg.id),
                created_at: msg.created_at,
                r#type: i32::from(msg.ty),
                body: msg.body.clone(),
                nonce: msg.nonces.first().map(|n| n.to_vec()).unwrap_or_default(),
            },
        ))
    }

    /// `OutgoingMessage`: the phone sent `msg` into `conv` and reflects it; one nonce per receiver.
    pub fn outgoing(&self, conv: &Conv, msg: &Msg) -> d2d::Envelope {
        self.envelope(d2d::envelope::Content::OutgoingMessage(
            d2d::OutgoingMessage {
                conversation: Some(conv.id()),
                message_id: id_u64(msg.id),
                thread_message_id: msg.thread.map(id_u64),
                created_at: msg.created_at,
                r#type: i32::from(msg.ty),
                body: msg.body.clone(),
                nonces: msg.nonces.iter().map(|n| n.to_vec()).collect(),
            },
        ))
    }

    /// `OutgoingMessageUpdate`: each `(conversation, message id)` is marked sent.
    pub fn sent(&self, updates: &[(Conv, MsgId)]) -> d2d::Envelope {
        use d2d::outgoing_message_update::{Sent, Update, update};
        self.envelope(d2d::envelope::Content::OutgoingMessageUpdate(
            d2d::OutgoingMessageUpdate {
                updates: updates
                    .iter()
                    .map(|(conv, id)| Update {
                        conversation: Some(conv.id()),
                        message_id: id_u64(*id),
                        update: Some(update::Update::Sent(Sent {})),
                    })
                    .collect(),
            },
        ))
    }

    /// `IncomingMessageUpdate`: each `(conversation, message id, read at ms)` is marked read.
    pub fn read(&self, updates: &[(Conv, MsgId, u64)]) -> d2d::Envelope {
        use d2d::incoming_message_update::{Read, Update, update};
        self.envelope(d2d::envelope::Content::IncomingMessageUpdate(
            d2d::IncomingMessageUpdate {
                updates: updates
                    .iter()
                    .map(|(conv, id, at)| Update {
                        conversation: Some(conv.id()),
                        message_id: id_u64(*id),
                        update: Some(update::Update::Read(Read { at: *at })),
                    })
                    .collect(),
            },
        ))
    }

    // --- Sync -----------------------------------------------------------------------------

    /// `ContactSync.Create`.
    pub fn contact_create(&self, contact: ContactDelta) -> d2d::Envelope {
        self.envelope(d2d::envelope::Content::ContactSync(d2d::ContactSync {
            action: Some(d2d::contact_sync::Action::Create(
                d2d::contact_sync::Create {
                    contact: Some(contact.0),
                },
            )),
        }))
    }

    /// `ContactSync.Update`.
    pub fn contact_update(&self, contact: ContactDelta) -> d2d::Envelope {
        self.envelope(d2d::envelope::Content::ContactSync(d2d::ContactSync {
            action: Some(d2d::contact_sync::Action::Update(
                d2d::contact_sync::Update {
                    contact: Some(contact.0),
                },
            )),
        }))
    }

    /// A contact that is removed: the proto has NO `ContactSync.Delete`; "the contact has been
    /// explicitly removed by the user" is `acquaintance_level = GROUP_OR_DELETED`
    /// (`TP/md-d2d-sync.proto:336-346`), sent as an update. [U: real phone] that this is all a phone
    /// reflects when a contact is deleted.
    pub fn contact_delete(&self, identity: &str) -> d2d::Envelope {
        self.contact_update(ContactDelta::new(identity).hidden())
    }

    /// `GroupSync.Create`.
    pub fn group_create(&self, group: GroupDelta) -> d2d::Envelope {
        self.envelope(d2d::envelope::Content::GroupSync(d2d::GroupSync {
            action: Some(d2d::group_sync::Action::Create(d2d::group_sync::Create {
                group: Some(group.0),
            })),
        }))
    }

    /// `GroupSync.Update` with the member changes the spec's notices come from.
    pub fn group_update(
        &self,
        group: GroupDelta,
        changes: &[(&str, MemberChange)],
    ) -> d2d::Envelope {
        self.envelope(d2d::envelope::Content::GroupSync(d2d::GroupSync {
            action: Some(d2d::group_sync::Action::Update(d2d::group_sync::Update {
                group: Some(group.0),
                member_state_changes: changes
                    .iter()
                    .map(|(identity, change)| ((*identity).to_owned(), *change as i32))
                    .collect(),
            })),
        }))
    }

    /// `GroupSync.Delete`.
    pub fn group_delete(&self, creator: &str, id: u64) -> d2d::Envelope {
        self.envelope(d2d::envelope::Content::GroupSync(d2d::GroupSync {
            action: Some(d2d::group_sync::Action::Delete(d2d::group_sync::Delete {
                group_identity: Some(common::GroupIdentity {
                    group_id: id,
                    creator_identity: creator.to_owned(),
                }),
            })),
        }))
    }

    /// `DistributionListSync.Create`.
    pub fn list_create(&self, list: d2d_sync::DistributionList) -> d2d::Envelope {
        self.envelope(d2d::envelope::Content::DistributionListSync(
            d2d::DistributionListSync {
                action: Some(d2d::distribution_list_sync::Action::Create(
                    d2d::distribution_list_sync::Create {
                        distribution_list: Some(list),
                    },
                )),
            },
        ))
    }

    /// `DistributionListSync.Update`.
    pub fn list_update(&self, list: d2d_sync::DistributionList) -> d2d::Envelope {
        self.envelope(d2d::envelope::Content::DistributionListSync(
            d2d::DistributionListSync {
                action: Some(d2d::distribution_list_sync::Action::Update(
                    d2d::distribution_list_sync::Update {
                        distribution_list: Some(list),
                    },
                )),
            },
        ))
    }

    /// `DistributionListSync.Delete`.
    pub fn list_delete(&self, id: u64) -> d2d::Envelope {
        self.envelope(d2d::envelope::Content::DistributionListSync(
            d2d::DistributionListSync {
                action: Some(d2d::distribution_list_sync::Action::Delete(
                    d2d::distribution_list_sync::Delete {
                        distribution_list_id: id,
                    },
                )),
            },
        ))
    }

    /// `UserProfileSync.Update`.
    pub fn profile_update(&self, profile: d2d_sync::UserProfile) -> d2d::Envelope {
        self.envelope(d2d::envelope::Content::UserProfileSync(
            d2d::UserProfileSync {
                action: Some(d2d::user_profile_sync::Action::Update(
                    d2d::user_profile_sync::Update {
                        user_profile: Some(profile),
                    },
                )),
            },
        ))
    }

    /// `SettingsSync.Update`.
    pub fn settings_update(&self, settings: d2d_sync::Settings) -> d2d::Envelope {
        self.envelope(d2d::envelope::Content::SettingsSync(d2d::SettingsSync {
            action: Some(d2d::settings_sync::Action::Update(
                d2d::settings_sync::Update {
                    settings: Some(settings),
                },
            )),
        }))
    }

    /// `MdmParameterSync.Update` (Work only: a consumer build drops it).
    pub fn mdm_update(&self, parameters: d2d_sync::MdmParameters) -> d2d::Envelope {
        self.envelope(d2d::envelope::Content::MdmParameterSync(
            d2d::MdmParameterSync {
                action: Some(d2d::mdm_parameter_sync::Action::Update(
                    d2d::mdm_parameter_sync::Update {
                        parameters: Some(parameters),
                    },
                )),
            },
        ))
    }

    /// The PLAINTEXT of an envelope with a oneof member this build does not know (field 99, length
    /// delimited, three bytes) after the padding and device id: it decodes, and its `content` is `None`.
    /// [I] what a newer phone's envelope looks like to an older reader.
    pub fn unknown_content_plaintext(&self) -> Vec<u8> {
        let mut bytes = self.empty().encode_to_vec();
        // tag = (99 << 3) | 2 = 794 as a varint, then the length and the bytes.
        bytes.extend_from_slice(&[0x9a, 0x06, 0x03, 0x01, 0x02, 0x03]);
        bytes
    }
}

/// `sealed` with the byte at `at` flipped: it no longer opens (the tag is wrong).
#[track_caller]
pub fn tampered(sealed: &[u8], at: usize) -> Vec<u8> {
    let mut out = sealed.to_vec();
    *out.get_mut(at).expect("a byte of the sealed envelope") ^= 0x01;
    out
}

/// A sealed envelope that is too short to hold a nonce and a tag: 39 bytes (24 + 16 is the least).
pub fn too_short() -> Vec<u8> {
    vec![0x5a; 39]
}

/// What a reflection left behind (the phone's side).
#[derive(Clone, PartialEq, Eq)]
pub struct Reflection {
    /// The phone's own reflect-id (the ack it got back carries it).
    pub reflect_id: u32,
    /// The flags it reflected with.
    pub flags: u16,
    /// The sealed bytes every other device must get byte for byte.
    pub sealed: Vec<u8>,
}

impl std::fmt::Debug for Reflection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Reflection(id {}, flags {:#06x}, {} B)",
            self.reflect_id,
            self.flags,
            self.sealed.len()
        )
    }
}

/// The reflect flags the spec gives an envelope: `EPHEMERAL` for a typing indicator (the one kind that
/// is "not stored and not acked", `TP/md-d2m.struct.yml:272-276`), 0 for everything else.
/// [U: real phone] that a phone sets the flag on exactly this.
pub fn flags_for(envelope: &d2d::Envelope) -> u16 {
    match &envelope.content {
        Some(d2d::envelope::Content::IncomingMessage(m))
            if m.r#type == i32::from(ty::TYPING_INDICATOR) =>
        {
            EPHEMERAL
        }
        _ => 0,
    }
}

/// Reflecting through a connected [`Phone`] (its mediator connection must be logged in).
pub trait Reflect {
    /// Seals `envelope` with the fake's DGRK (a fresh nonce) and reflects it with the flags the kind
    /// calls for ([`flags_for`]): the other devices of the group get it at once or find it in their queue.
    fn reflect_envelope(&mut self, envelope: &d2d::Envelope) -> Reflection;

    /// The same with the flags the test chooses (0 for a typing indicator that is NOT ephemeral: the
    /// kind decides the event, the flag decides the ack).
    fn reflect_with(&mut self, envelope: &d2d::Envelope, flags: u16) -> Reflection;

    /// Reflects already sealed bytes (a replay of an earlier reflection, a tampered or short box).
    fn reflect_bytes(&mut self, sealed: &[u8], flags: u16) -> Reflection;
}

impl Reflect for Phone {
    fn reflect_envelope(&mut self, envelope: &d2d::Envelope) -> Reflection {
        self.reflect_with(envelope, flags_for(envelope))
    }

    fn reflect_with(&mut self, envelope: &d2d::Envelope, flags: u16) -> Reflection {
        let sealed = self.seal(envelope);
        self.reflect_bytes(&sealed, flags)
    }

    fn reflect_bytes(&mut self, sealed: &[u8], flags: u16) -> Reflection {
        let reflect_id = self.reflect_sealed(sealed, flags);
        Reflection {
            reflect_id,
            flags,
            sealed: sealed.to_vec(),
        }
    }
}

// --- Sync deltas ------------------------------------------------------------------------------

/// A `d2d_sync.Contact` delta: a new one carries what the spec requires of a new contact, an update only
/// its identity and what changed.
#[derive(Clone, PartialEq)]
pub struct ContactDelta(pub d2d_sync::Contact);

impl std::fmt::Debug for ContactDelta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ContactDelta")
    }
}

impl ContactDelta {
    /// An update that changes nothing yet: just the identity.
    pub fn new(identity: &str) -> Self {
        Self(d2d_sync::Contact {
            identity: identity.to_owned(),
            ..Default::default()
        })
    }

    /// What a CREATE carries: the key, the time, and the fields "required for a new contact"
    /// (`TP/md-d2d-sync.proto:269-330`) at the zero values of their enums (UNVERIFIED, NONE, REGULAR,
    /// DIRECT, ACTIVE, INITIAL); the feature mask is [I] the voice and group bits.
    pub fn create(identity: &str, public_key: [u8; 32], created_at: u64) -> Self {
        Self(d2d_sync::Contact {
            identity: identity.to_owned(),
            public_key: Some(public_key.to_vec()),
            created_at: Some(created_at),
            verification_level: Some(0),
            work_verification_level: Some(0),
            identity_type: Some(0),
            acquaintance_level: Some(0),
            activity_state: Some(0),
            feature_mask: Some(0x0003),
            sync_state: Some(0),
            ..Default::default()
        })
    }

    pub fn public_key(mut self, key: [u8; 32]) -> Self {
        self.0.public_key = Some(key.to_vec());
        self
    }

    /// First and last name SET.
    pub fn names(mut self, first: &str, last: &str) -> Self {
        self.0.first_name = Some(first.to_owned());
        self.0.last_name = Some(last.to_owned());
        self
    }

    /// First and last name CLEARED: present and empty (`TP/md-d2d-sync.proto:255-275`).
    pub fn clear_names(self) -> Self {
        self.names("", "")
    }

    pub fn nickname(mut self, nickname: &str) -> Self {
        self.0.nickname = Some(nickname.to_owned());
        self
    }

    pub fn clear_nickname(self) -> Self {
        self.nickname("")
    }

    /// Acquaintance level DIRECT.
    pub fn direct(mut self) -> Self {
        self.0.acquaintance_level = Some(d2d_sync::contact::AcquaintanceLevel::Direct as i32);
        self
    }

    /// Acquaintance level GROUP_OR_DELETED (a contact known only through a group, or removed).
    pub fn hidden(mut self) -> Self {
        self.0.acquaintance_level =
            Some(d2d_sync::contact::AcquaintanceLevel::GroupOrDeleted as i32);
        self
    }

    pub fn activity(mut self, state: d2d_sync::contact::ActivityState) -> Self {
        self.0.activity_state = Some(state as i32);
        self
    }

    /// `conversation_category = PROTECTED` (the phone's PIN-protected chat).
    pub fn protected(mut self) -> Self {
        self.0.conversation_category = Some(d2d_sync::ConversationCategory::Protected as i32);
        self
    }

    /// `conversation_category = DEFAULT` (the chat is not protected any more).
    pub fn unprotected(mut self) -> Self {
        self.0.conversation_category = Some(d2d_sync::ConversationCategory::Default as i32);
        self
    }

    pub fn visibility(mut self, visibility: d2d_sync::ConversationVisibility) -> Self {
        self.0.conversation_visibility = Some(visibility as i32);
        self
    }

    pub fn archived(self) -> Self {
        self.visibility(d2d_sync::ConversationVisibility::Archived)
    }

    pub fn pinned(self) -> Self {
        self.visibility(d2d_sync::ConversationVisibility::Pinned)
    }

    pub fn normal(self) -> Self {
        self.visibility(d2d_sync::ConversationVisibility::Normal)
    }

    /// Muted for good: notification policy NEVER, no expiry (a contact has no MENTIONED).
    pub fn muted(self) -> Self {
        self.mute_policy(None)
    }

    /// Muted until `expires_at` (ms).
    pub fn muted_until(self, expires_at: u64) -> Self {
        self.mute_policy(Some(expires_at))
    }

    fn mute_policy(mut self, expires_at: Option<u64>) -> Self {
        use d2d_sync::contact::notification_trigger_policy_override as o;
        self.0.notification_trigger_policy_override =
            Some(d2d_sync::contact::NotificationTriggerPolicyOverride {
                r#override: Some(o::Override::Policy(o::Policy {
                    policy: o::policy::NotificationTriggerPolicy::Never as i32,
                    expires_at,
                })),
            });
        self
    }

    /// The override removed: the default notification policy again.
    pub fn unmuted(mut self) -> Self {
        use d2d_sync::contact::notification_trigger_policy_override as o;
        self.0.notification_trigger_policy_override =
            Some(d2d_sync::contact::NotificationTriggerPolicyOverride {
                r#override: Some(o::Override::Default(common::Unit {})),
            });
        self
    }

    /// The delta.
    pub fn build(self) -> d2d_sync::Contact {
        self.0
    }
}

/// A `d2d_sync.Group` delta.
#[derive(Clone, PartialEq)]
pub struct GroupDelta(pub d2d_sync::Group);

impl std::fmt::Debug for GroupDelta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GroupDelta")
    }
}

impl GroupDelta {
    /// An update that changes nothing yet: just the group identity.
    pub fn new(creator: &str, id: u64) -> Self {
        Self(d2d_sync::Group {
            group_identity: Some(common::GroupIdentity {
                group_id: id,
                creator_identity: creator.to_owned(),
            }),
            ..Default::default()
        })
    }

    /// A CREATE: name, time, user state MEMBER and the member list.
    pub fn create(creator: &str, id: u64, name: &str, members: &[&str], created_at: u64) -> Self {
        let mut group = Self::new(creator, id).name(name).members(members);
        group.0.created_at = Some(created_at);
        group.0.user_state = Some(d2d_sync::group::UserState::Member as i32);
        group
    }

    pub fn name(mut self, name: &str) -> Self {
        self.0.name = Some(name.to_owned());
        self
    }

    /// The name CLEARED: present and empty.
    pub fn clear_name(self) -> Self {
        self.name("")
    }

    /// The member list REPLACED as a whole (`member_identities` is a sub-message: a present one
    /// replaces the stored list).
    pub fn members(mut self, members: &[&str]) -> Self {
        self.0.member_identities = Some(common::Identities {
            identities: members.iter().map(|m| (*m).to_owned()).collect(),
        });
        self
    }

    pub fn user_state(mut self, state: d2d_sync::group::UserState) -> Self {
        self.0.user_state = Some(state as i32);
        self
    }

    pub fn protected(mut self) -> Self {
        self.0.conversation_category = Some(d2d_sync::ConversationCategory::Protected as i32);
        self
    }

    pub fn unprotected(mut self) -> Self {
        self.0.conversation_category = Some(d2d_sync::ConversationCategory::Default as i32);
        self
    }

    pub fn visibility(mut self, visibility: d2d_sync::ConversationVisibility) -> Self {
        self.0.conversation_visibility = Some(visibility as i32);
        self
    }

    pub fn archived(self) -> Self {
        self.visibility(d2d_sync::ConversationVisibility::Archived)
    }

    pub fn pinned(self) -> Self {
        self.visibility(d2d_sync::ConversationVisibility::Pinned)
    }

    pub fn normal(self) -> Self {
        self.visibility(d2d_sync::ConversationVisibility::Normal)
    }

    /// Notify only when mentioned (policy MENTIONED), for good.
    pub fn mentioned_only(self) -> Self {
        self.policy(
            d2d_sync::group::notification_trigger_policy_override::policy::NotificationTriggerPolicy::Mentioned,
            None,
        )
    }

    /// Notify only when mentioned, until `expires_at` (ms).
    pub fn mentioned_only_until(self, expires_at: u64) -> Self {
        self.policy(
            d2d_sync::group::notification_trigger_policy_override::policy::NotificationTriggerPolicy::Mentioned,
            Some(expires_at),
        )
    }

    /// Muted for good (policy NEVER).
    pub fn muted(self) -> Self {
        self.policy(
            d2d_sync::group::notification_trigger_policy_override::policy::NotificationTriggerPolicy::Never,
            None,
        )
    }

    /// Muted until `expires_at` (ms).
    pub fn muted_until(self, expires_at: u64) -> Self {
        self.policy(
            d2d_sync::group::notification_trigger_policy_override::policy::NotificationTriggerPolicy::Never,
            Some(expires_at),
        )
    }

    fn policy(
        mut self,
        policy: d2d_sync::group::notification_trigger_policy_override::policy::NotificationTriggerPolicy,
        expires_at: Option<u64>,
    ) -> Self {
        use d2d_sync::group::notification_trigger_policy_override as o;
        self.0.notification_trigger_policy_override =
            Some(d2d_sync::group::NotificationTriggerPolicyOverride {
                r#override: Some(o::Override::Policy(o::Policy {
                    policy: policy as i32,
                    expires_at,
                })),
            });
        self
    }

    /// The override removed: the default notification policy again.
    pub fn unmuted(mut self) -> Self {
        use d2d_sync::group::notification_trigger_policy_override as o;
        self.0.notification_trigger_policy_override =
            Some(d2d_sync::group::NotificationTriggerPolicyOverride {
                r#override: Some(o::Override::Default(common::Unit {})),
            });
        self
    }

    /// The delta.
    pub fn build(self) -> d2d_sync::Group {
        self.0
    }
}

impl From<ContactDelta> for d2d_sync::Contact {
    fn from(delta: ContactDelta) -> Self {
        delta.0
    }
}

impl From<GroupDelta> for d2d_sync::Group {
    fn from(delta: GroupDelta) -> Self {
        delta.0
    }
}

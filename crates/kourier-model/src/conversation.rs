//! Conversations, their patches and user-requested roaming changes (architecture §3.4).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::contact::{Member, MemberList};
use crate::content::BlobRef;
use crate::features::ConversationFeatures;
use crate::ids::{AccountId, BackendBlob, ConvKey, ConversationId};
use crate::patch::{Patch, apply_set};
use crate::text::Text;

/// Kind of a conversation (implied by its [`ConvKey`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ConvKind {
    /// 1:1.
    Direct,
    /// Group.
    Group,
    /// Distribution list (Threema).
    Broadcast,
    /// Channel/newsletter (WhatsApp).
    Channel,
}

/// A conversation row (§3.4). Roaming fields change only through backend events. `Debug`
/// hides the title and the description.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    /// Core rowid.
    pub id: ConversationId,
    /// Account.
    pub account: AccountId,
    /// Network key.
    pub key: ConvKey,
    /// Kind.
    pub kind: ConvKind,
    /// Note to self.
    #[serde(default)]
    pub is_self: bool,
    /// Title (groups; direct chats use the contact's name).
    #[serde(default)]
    pub title: Option<String>,
    /// Description (groups; plain text, line breaks kept; `None` = never said or none). The
    /// network's text is stored whole: the page that shows it cuts it (architecture §13).
    #[serde(default)]
    pub description: Option<String>,
    /// Avatar file.
    #[serde(default)]
    pub avatar: Option<BlobRef>,
    /// Members; empty for direct chats.
    #[serde(default)]
    pub members: Vec<Member>,
    /// Roaming: muted until (ms).
    #[serde(default)]
    pub muted_until: Option<i64>,
    /// Roaming: archived.
    #[serde(default)]
    pub archived: bool,
    /// Roaming: position among pinned conversations.
    #[serde(default)]
    pub pinned_order: Option<u32>,
    /// Roaming: blocked.
    #[serde(default)]
    pub blocked: bool,
    /// Roaming: message request not accepted yet.
    #[serde(default)]
    pub request_pending: bool,
    /// Roaming: marked unread.
    #[serde(default)]
    pub marked_unread: bool,
    /// Roaming: disappearing timer.
    #[serde(default)]
    pub timer: Option<Duration>,
    /// Reserved: WhatsApp community.
    #[serde(default)]
    pub parent: Option<ConversationId>,
    /// Features (§3.8).
    #[serde(default)]
    pub features: ConversationFeatures,
    /// Local: muted on this computer until (ms).
    #[serde(default)]
    pub local_muted_until: Option<i64>,
    /// Local: composer draft.
    #[serde(default)]
    pub draft: Option<Text>,
    /// Local: unread count.
    #[serde(default)]
    pub unread: u32,
    /// Local: unread mentions.
    #[serde(default)]
    pub mentions: u32,
    /// Local: `sort_key` read up to.
    #[serde(default)]
    pub read_up_to: Option<i64>,
    /// Local: `sort_key` of the newest visible message.
    #[serde(default)]
    pub last_activity: i64,
    /// Backend metadata (`conversations.meta`).
    #[serde(default)]
    pub meta: Option<BackendBlob>,
    /// Roaming: the user protects this chat on another device: its messages show nowhere
    /// outside the open chat (no preview in the list, no text in a notification, not found by
    /// a search; architecture §3.4 "Protected chats"). `false` when the network never said.
    /// Not Telegram's protected content (no saving or forwarding): that is a network's own
    /// `meta`.
    #[serde(default)]
    pub protected: bool,
}

crate::redacting_debug!(Conversation {
    id, account, key, kind, is_self, avatar, members, muted_until, archived, pinned_order,
    blocked, request_pending, marked_unread, timer, parent, features, local_muted_until, draft,
    unread, mentions, read_up_to, last_activity, meta, protected
} redact { title, description });

/// Field-wise conversation change (`ConversationUpsert`). `Patch` fields: unchanged | clear |
/// set; `Option` fields: unchanged | set. `Debug` hides the title and the description.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationPatch {
    /// The conversation (created if unknown).
    pub key: ConvKey,
    /// Note to self.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_self: Option<bool>,
    /// Title.
    #[serde(default, skip_serializing_if = "Patch::is_unchanged")]
    pub title: Patch<String>,
    /// Description of a group (plain text; the network's whole text).
    #[serde(default, skip_serializing_if = "Patch::is_unchanged")]
    pub description: Patch<String>,
    /// Avatar file written by the backend into the account's blob directory.
    #[serde(default, skip_serializing_if = "Patch::is_unchanged")]
    pub avatar: Patch<BlobRef>,
    /// Members: snapshot or delta.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub members: Option<MemberList>,
    /// Features, replaced as a whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<ConversationFeatures>,
    /// Disappearing timer.
    #[serde(default, skip_serializing_if = "Patch::is_unchanged")]
    pub timer: Patch<Duration>,
    /// The network's version counter of the timer this patch sets or clears (Signal
    /// `expireTimerVersion`), for networks that order timer changes by one. The core keeps the
    /// highest version it saw and ignores the `timer` of a patch with a lower one, so timer
    /// changes that reach it out of order end in the same state. `None`: the last patch wins.
    /// The version stays the backend's to count and to attach to what it sends; the core never
    /// hands it back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timer_version: Option<u32>,
    /// Muted until (ms).
    #[serde(default, skip_serializing_if = "Patch::is_unchanged")]
    pub muted_until: Patch<i64>,
    /// Archived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    /// Pinned position.
    #[serde(default, skip_serializing_if = "Patch::is_unchanged")]
    pub pinned_order: Patch<u32>,
    /// Blocked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<bool>,
    /// Message request pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_pending: Option<bool>,
    /// Marked unread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marked_unread: Option<bool>,
    /// How many unread messages of the conversation the backend has NO ROWS for (a history
    /// import that knows a chat's unread count and not which messages it means, or that
    /// loads only a part of the chat). The core adds it to the unread rows it counts and
    /// clears it when the chat is read here; a later patch replaces it.
    ///
    /// It has this one meaning. An unread message the backend DOES deliver says so itself
    /// (a history row with `read: false`) and is counted as a row: never state the same
    /// messages in both, they would count twice, and a hint is not lowered when another
    /// device reads the chat (a row is). A backend whose history rows are of unknown read
    /// state says `read: true` on them and gives the count here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unread_hint: Option<u32>,
    /// How many unread messages the network's SERVER says the conversation has (KT-2). Read
    /// only from a network whose `NetworkDescriptor::server` has `owns_unread`; any other
    /// ignores it. Until a patch states it the core counts the rows; from then on it shows this
    /// count instead (it wins over `unread_hint`). A read here sets it to 0; a later patch
    /// replaces it.
    ///
    /// A live row does not move it, so such a network STATES the count at every change: a
    /// message that arrived, a read on another device, a delete. A chat it has never stated is
    /// counted by its rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_unread: Option<u32>,
    /// How many of those unread messages mention us, by the same rules as
    /// [`server_unread`](Self::server_unread).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_mentions: Option<u32>,
    /// Parent conversation (WhatsApp community).
    #[serde(default, skip_serializing_if = "Patch::is_unchanged")]
    pub parent: Patch<ConvKey>,
    /// Backend metadata.
    #[serde(default, skip_serializing_if = "Patch::is_unchanged")]
    pub meta: Patch<BackendBlob>,
    /// The user protects this chat on another device (`true`) or no longer does (`false`): its
    /// messages show nowhere outside the open chat ([`Conversation::protected`]). `None`
    /// leaves it as it is; a network that has no such state never sets it. A network that has
    /// the state states it NO LATER than the envelope that carries the chat's first message
    /// after the change (a notification is built when the message arrives, and reads the
    /// flag then), keeps it in its own store, and restates it with the chat on
    /// `StartMode::Rebuild` (the core's flag starts at `false` there). Not Telegram's protected
    /// content (no saving or forwarding): that is a network's own `meta`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protected: Option<bool>,
}

crate::redacting_debug!(ConversationPatch {
    key, is_self, avatar, members, features, timer, timer_version, muted_until, archived,
    pinned_order, blocked, request_pending, marked_unread, unread_hint, server_unread,
    server_mentions, parent, meta, protected
} redact { title, description });

impl ConversationPatch {
    /// A patch that changes nothing (but creates the conversation if it is unknown).
    pub fn new(key: ConvKey) -> Self {
        Self {
            key,
            is_self: None,
            title: Patch::Unchanged,
            description: Patch::Unchanged,
            avatar: Patch::Unchanged,
            members: None,
            features: None,
            timer: Patch::Unchanged,
            timer_version: None,
            muted_until: Patch::Unchanged,
            archived: None,
            pinned_order: Patch::Unchanged,
            blocked: None,
            request_pending: None,
            marked_unread: None,
            unread_hint: None,
            server_unread: None,
            server_mentions: None,
            parent: Patch::Unchanged,
            meta: Patch::Unchanged,
            protected: None,
        }
    }

    /// Applies the roaming fields to `c` (the conversation with the same key); returns whether
    /// any field was touched. `members` replaces (`full`) or merges (delta). Not applied here:
    /// `parent` (the core resolves the key to a rowid), `unread_hint` (the core adds it to
    /// the unread rows it counts), `server_unread` and `server_mentions` (the core stores them
    /// beside the rows' count and recounts, when the network owns the count) and `timer_version`
    /// (the core compares it with the version it stored and drops a stale `timer` before applying
    /// the patch).
    pub fn apply(self, c: &mut Conversation) -> bool {
        let mut t = apply_set(self.is_self, &mut c.is_self);
        t |= self.title.apply(&mut c.title);
        t |= self.description.apply(&mut c.description);
        t |= self.avatar.apply(&mut c.avatar);
        if let Some(members) = self.members {
            members.apply(&mut c.members);
            t = true;
        }
        t |= apply_set(self.features, &mut c.features);
        t |= self.timer.apply(&mut c.timer);
        t |= self.muted_until.apply(&mut c.muted_until);
        t |= apply_set(self.archived, &mut c.archived);
        t |= self.pinned_order.apply(&mut c.pinned_order);
        t |= apply_set(self.blocked, &mut c.blocked);
        t |= apply_set(self.request_pending, &mut c.request_pending);
        t |= apply_set(self.marked_unread, &mut c.marked_unread);
        t |= self.meta.apply(&mut c.meta);
        t |= apply_set(self.protected, &mut c.protected);
        t
    }
}

/// A user-requested change of roaming conversation state (`ChangeConversation`). The backend
/// confirms it with `ConversationUpsert` after its network transaction (§4.1 principle 4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConversationChange {
    /// Archive or unarchive.
    SetArchived {
        /// Archived.
        archived: bool,
    },
    /// Pin or unpin.
    SetPinned {
        /// Pinned.
        pinned: bool,
    },
    /// Mute until (ms) or unmute (`None`).
    SetMuted {
        /// Until.
        #[serde(default)]
        until: Option<i64>,
    },
    /// Block or unblock.
    SetBlocked {
        /// Blocked.
        blocked: bool,
    },
    /// Mark unread or read.
    SetMarkedUnread {
        /// Marked unread.
        marked_unread: bool,
    },
    /// Change the disappearing timer: one of the conversation's offered values or a value of
    /// its range (`ConversationFeatures::takes_timer`), in whole seconds.
    SetTimer {
        /// The timer; `None` = off.
        #[serde(default)]
        timer: Option<Duration>,
    },
    /// Accept the message request.
    AcceptRequest,
    /// Leave the group.
    Leave,
}

impl ConversationChange {
    /// Whether this is one of the four settings a network keeps for the account's other devices
    /// (a "roaming" setting: archive, pin, mute, marked unread; `AccountCaps::roaming_writes`
    /// states whether the account's network writes them). The others (a timer, a block,
    /// accepting a request, leaving) are not roaming: the account's roaming switch does not gate
    /// them and a backend applies them as it always did.
    ///
    /// THE LIST EXISTS ONCE, here, for the core's second line, the fake and every backend; the
    /// match has no `_` arm, so a new variant (a notification level, say) must be judged here.
    pub fn is_roaming(&self) -> bool {
        match self {
            Self::SetArchived { .. }
            | Self::SetPinned { .. }
            | Self::SetMuted { .. }
            | Self::SetMarkedUnread { .. } => true,
            Self::SetBlocked { .. } | Self::SetTimer { .. } | Self::AcceptRequest | Self::Leave => {
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contact::MemberRole;
    use crate::ids::UserId;

    /// The table of [`ConversationChange::is_roaming`]: archive, pin, mute and marked unread
    /// are the roaming settings, the other four changes are not.
    #[test]
    fn exactly_four_changes_are_roaming() {
        let roaming = [
            ConversationChange::SetArchived { archived: true },
            ConversationChange::SetPinned { pinned: true },
            ConversationChange::SetMuted { until: Some(1) },
            ConversationChange::SetMarkedUnread {
                marked_unread: true,
            },
        ];
        let not_roaming = [
            ConversationChange::SetBlocked { blocked: true },
            ConversationChange::SetTimer { timer: None },
            ConversationChange::AcceptRequest,
            ConversationChange::Leave,
        ];
        for change in &roaming {
            assert!(change.is_roaming(), "{change:?}");
        }
        for change in &not_roaming {
            assert!(!change.is_roaming(), "{change:?}");
        }
        // The flip side of each roaming setting is roaming too (the value does not matter).
        assert!(ConversationChange::SetArchived { archived: false }.is_roaming());
        assert!(ConversationChange::SetPinned { pinned: false }.is_roaming());
        assert!(ConversationChange::SetMuted { until: None }.is_roaming());
        assert!(
            ConversationChange::SetMarkedUnread {
                marked_unread: false
            }
            .is_roaming()
        );
    }

    fn conv() -> Conversation {
        Conversation {
            id: ConversationId(1),
            account: AccountId(1),
            key: ConvKey::Group("gv2:abc".into()),
            kind: ConvKind::Group,
            is_self: false,
            title: Some("Old title".into()),
            description: None,
            avatar: None,
            members: Vec::new(),
            muted_until: Some(10),
            archived: false,
            pinned_order: Some(2),
            blocked: false,
            request_pending: true,
            marked_unread: false,
            timer: None,
            parent: None,
            features: ConversationFeatures::default(),
            local_muted_until: None,
            draft: None,
            unread: 3,
            mentions: 0,
            read_up_to: None,
            last_activity: 0,
            meta: None,
            protected: false,
        }
    }

    /// A protected chat is stated by its patch and changes both ways; a patch that says
    /// nothing leaves it as it is.
    #[test]
    fn a_patch_protects_a_chat_and_lifts_it_again() {
        let mut c = conv();
        let protect = ConversationPatch {
            protected: Some(true),
            ..ConversationPatch::new(c.key.clone())
        };
        assert!(protect.apply(&mut c));
        assert!(c.protected);
        assert!(!ConversationPatch::new(c.key.clone()).apply(&mut c));
        assert!(c.protected, "a patch that says nothing keeps it");
        let lift = ConversationPatch {
            protected: Some(false),
            ..ConversationPatch::new(c.key.clone())
        };
        assert!(lift.apply(&mut c));
        assert!(!c.protected);
    }

    #[test]
    fn patch_unchanged_clear_set() {
        let mut c = conv();
        assert!(!ConversationPatch::new(c.key.clone()).apply(&mut c));
        assert_eq!(c, conv());
        let member = Member {
            user: UserId::parse("aci:a").expect("valid"),
            role: MemberRole::Admin,
        };
        let p = ConversationPatch {
            title: Patch::Set("New".into()),
            muted_until: Patch::Clear,
            archived: Some(true),
            request_pending: Some(false),
            members: Some(MemberList::snapshot(vec![member.clone()])),
            unread_hint: Some(9),
            ..ConversationPatch::new(c.key.clone())
        };
        let json = serde_json::to_string(&p).expect("serialize");
        let back: ConversationPatch = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, p);
        assert!(back.apply(&mut c));
        assert_eq!(c.title.as_deref(), Some("New"));
        assert_eq!(c.muted_until, None, "cleared");
        assert_eq!(c.pinned_order, Some(2), "unchanged");
        assert!(c.archived && !c.request_pending);
        assert_eq!(c.members, vec![member]);
        assert_eq!(c.unread, 3, "unread_hint is the core's job");
    }

    #[test]
    fn description_patch_unchanged_clear_set_and_round_trips() {
        let mut c = conv();
        let set = ConversationPatch {
            description: Patch::Set("First line\nSecond line".into()),
            ..ConversationPatch::new(c.key.clone())
        };
        let json = serde_json::to_string(&set).expect("serialize");
        let back: ConversationPatch = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, set);
        assert!(back.apply(&mut c));
        assert_eq!(c.description.as_deref(), Some("First line\nSecond line"));
        // Unchanged keeps it, and says nothing in the JSON.
        let keep = ConversationPatch::new(c.key.clone());
        assert!(
            !serde_json::to_string(&keep)
                .expect("serialize")
                .contains("description")
        );
        assert!(!keep.apply(&mut c));
        assert_eq!(c.description.as_deref(), Some("First line\nSecond line"));
        // Clear removes it.
        let clear = ConversationPatch {
            description: Patch::Clear,
            ..ConversationPatch::new(c.key.clone())
        };
        assert!(clear.apply(&mut c));
        assert_eq!(c.description, None);
        // An old stored conversation without the field reads as none.
        let mut v = serde_json::to_value(conv()).expect("value");
        v.as_object_mut().expect("object").remove("description");
        let old: Conversation = serde_json::from_value(v).expect("old row");
        assert_eq!(old.description, None);
    }

    #[test]
    fn debug_of_conversation_and_patch_shows_no_description() {
        let c = Conversation {
            description: Some("Meet at the Zebra gate".into()),
            ..conv()
        };
        let p = ConversationPatch {
            description: Patch::Set("Meet at the Zebra gate".into()),
            ..ConversationPatch::new(c.key.clone())
        };
        let s = format!("{c:?} {p:?}");
        assert!(!s.contains("Zebra") && !s.contains("gate"), "{s}");
    }

    #[test]
    fn debug_hides_title() {
        let c = conv();
        let p = ConversationPatch {
            title: Patch::Set("Secret plans".into()),
            ..ConversationPatch::new(c.key.clone())
        };
        let s = format!("{c:?} {p:?}");
        assert!(
            !s.contains("Old title") && !s.contains("Secret plans"),
            "{s}"
        );
    }
}

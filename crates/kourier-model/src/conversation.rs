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
/// hides the title.
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
}

crate::redacting_debug!(Conversation {
    id, account, key, kind, is_self, avatar, members, muted_until, archived, pinned_order,
    blocked, request_pending, marked_unread, timer, parent, features, local_muted_until, draft,
    unread, mentions, read_up_to, last_activity, meta
} redact { title });

/// Field-wise conversation change (`ConversationUpsert`). `Patch` fields: unchanged | clear |
/// set; `Option` fields: unchanged | set. `Debug` hides the title.
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
    /// Parent conversation (WhatsApp community).
    #[serde(default, skip_serializing_if = "Patch::is_unchanged")]
    pub parent: Patch<ConvKey>,
    /// Backend metadata.
    #[serde(default, skip_serializing_if = "Patch::is_unchanged")]
    pub meta: Patch<BackendBlob>,
}

crate::redacting_debug!(ConversationPatch {
    key, is_self, avatar, members, features, timer, timer_version, muted_until, archived,
    pinned_order, blocked, request_pending, marked_unread, unread_hint, parent, meta
} redact { title });

impl ConversationPatch {
    /// A patch that changes nothing (but creates the conversation if it is unknown).
    pub fn new(key: ConvKey) -> Self {
        Self {
            key,
            is_self: None,
            title: Patch::Unchanged,
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
            parent: Patch::Unchanged,
            meta: Patch::Unchanged,
        }
    }

    /// Applies the roaming fields to `c` (the conversation with the same key); returns whether
    /// any field was touched. `members` replaces (`full`) or merges (delta). Not applied here:
    /// `parent` (the core resolves the key to a rowid), `unread_hint` (the core adds it to
    /// the unread rows it counts) and `timer_version` (the core compares it with
    /// the version it stored and drops a stale `timer` before applying the patch).
    pub fn apply(self, c: &mut Conversation) -> bool {
        let mut t = apply_set(self.is_self, &mut c.is_self);
        t |= self.title.apply(&mut c.title);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contact::MemberRole;
    use crate::ids::UserId;

    fn conv() -> Conversation {
        Conversation {
            id: ConversationId(1),
            account: AccountId(1),
            key: ConvKey::Group("gv2:abc".into()),
            kind: ConvKind::Group,
            is_self: false,
            title: Some("Old title".into()),
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
        }
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

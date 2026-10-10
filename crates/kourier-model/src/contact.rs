//! Contacts, members and their patches (architecture §3.3).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::content::BlobRef;
use crate::ids::{AccountId, BackendBlob, ContactId, Identifier, UserId};
use crate::patch::Patch;

/// How far a contact's identity key is trusted.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub enum TrustLevel {
    /// Not verified.
    #[default]
    Unverified,
    /// Verified by the server/directory (Threema "server verified").
    ServerVerified,
    /// Verified by the user (safety number / QR).
    Verified,
}

/// A contact row: one per account and [`UserId`]. `Debug` hides names and the fingerprint.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contact {
    /// Core rowid.
    pub id: ContactId,
    /// Account.
    pub account: AccountId,
    /// The user.
    pub user: UserId,
    /// Name from the user's profile.
    #[serde(default)]
    pub profile_name: Option<String>,
    /// Name from the phone's address book.
    #[serde(default)]
    pub system_name: Option<String>,
    /// Nickname set by us.
    #[serde(default)]
    pub nickname: Option<String>,
    /// Avatar file.
    #[serde(default)]
    pub avatar: Option<BlobRef>,
    /// Identity trust.
    #[serde(default)]
    pub trust: TrustLevel,
    /// Identity key fingerprint.
    #[serde(default, with = "crate::serde_util::b64_opt")]
    pub identity_fingerprint: Option<Vec<u8>>,
    /// Blocked.
    #[serde(default)]
    pub blocked: bool,
    /// Hidden (Threema group-only or deleted contacts).
    #[serde(default)]
    pub hidden: bool,
    /// When the network reported the user unregistered (ms).
    #[serde(default)]
    pub unregistered_at: Option<i64>,
    /// Phone, username, … for display and search only.
    #[serde(default)]
    pub identifiers: Vec<Identifier>,
    /// Backend metadata.
    #[serde(default)]
    pub meta: Option<BackendBlob>,
}

crate::redacting_debug!(Contact {
    id, account, user, avatar, trust, blocked, hidden, unregistered_at, identifiers, meta
} redact { profile_name, system_name, nickname, identity_fingerprint });

/// Field-wise contact change (`ContactUpsert`). `Patch` fields: unchanged | clear | set;
/// `Option` fields: unchanged | set. `Debug` hides names and the fingerprint.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactPatch {
    /// The contact.
    pub user: UserId,
    /// Profile name.
    #[serde(default, skip_serializing_if = "Patch::is_unchanged")]
    pub profile_name: Patch<String>,
    /// Address-book name.
    #[serde(default, skip_serializing_if = "Patch::is_unchanged")]
    pub system_name: Patch<String>,
    /// Nickname.
    #[serde(default, skip_serializing_if = "Patch::is_unchanged")]
    pub nickname: Patch<String>,
    /// Avatar file written by the backend into the account's blob directory.
    #[serde(default, skip_serializing_if = "Patch::is_unchanged")]
    pub avatar: Patch<BlobRef>,
    /// Trust level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<TrustLevel>,
    /// Identity fingerprint.
    #[serde(default, skip_serializing_if = "Patch::is_unchanged")]
    pub identity_fingerprint: Patch<IdentityFingerprint>,
    /// Blocked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<bool>,
    /// Hidden.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    /// Unregistered since (ms).
    #[serde(default, skip_serializing_if = "Patch::is_unchanged")]
    pub unregistered_at: Patch<i64>,
    /// Full replacement of the identifiers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifiers: Option<Vec<Identifier>>,
    /// Backend metadata.
    #[serde(default, skip_serializing_if = "Patch::is_unchanged")]
    pub meta: Patch<BackendBlob>,
}

crate::redacting_debug!(ContactPatch {
    user, avatar, trust, blocked, hidden, unregistered_at, identifiers, meta
} redact { profile_name, system_name, nickname, identity_fingerprint });

impl ContactPatch {
    /// A patch that changes nothing.
    pub fn new(user: UserId) -> Self {
        Self {
            user,
            profile_name: Patch::Unchanged,
            system_name: Patch::Unchanged,
            nickname: Patch::Unchanged,
            avatar: Patch::Unchanged,
            trust: None,
            identity_fingerprint: Patch::Unchanged,
            blocked: None,
            hidden: None,
            unregistered_at: Patch::Unchanged,
            identifiers: None,
            meta: Patch::Unchanged,
        }
    }

    /// Applies the patch to `c` (same user); returns whether any field was touched.
    pub fn apply(self, c: &mut Contact) -> bool {
        use crate::patch::apply_set;
        let mut t = self.profile_name.apply(&mut c.profile_name);
        t |= self.system_name.apply(&mut c.system_name);
        t |= self.nickname.apply(&mut c.nickname);
        t |= self.avatar.apply(&mut c.avatar);
        t |= apply_set(self.trust, &mut c.trust);
        t |= self
            .identity_fingerprint
            .map(|f| f.0)
            .apply(&mut c.identity_fingerprint);
        t |= apply_set(self.blocked, &mut c.blocked);
        t |= apply_set(self.hidden, &mut c.hidden);
        t |= self.unregistered_at.apply(&mut c.unregistered_at);
        t |= apply_set(self.identifiers, &mut c.identifiers);
        t |= self.meta.apply(&mut c.meta);
        t
    }
}

/// Identity key fingerprint bytes (base64 in JSON). `Debug` prints only the length (§9.1).
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IdentityFingerprint(#[serde(with = "crate::serde_util::b64")] pub Vec<u8>);

impl fmt::Debug for IdentityFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "IdentityFingerprint([redacted; {} bytes])", self.0.len())
    }
}

/// Role of a conversation member.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub enum MemberRole {
    /// Regular member.
    #[default]
    Member,
    /// Administrator.
    Admin,
    /// Invited, not joined yet.
    Pending,
    /// Asked to join.
    Requesting,
}

/// A conversation member. A contact row is optional.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Member {
    /// The member.
    pub user: UserId,
    /// Role.
    #[serde(default)]
    pub role: MemberRole,
}

/// Member changes: a snapshot (`full`) or a delta (bridgev2 semantics, §4.2 notes).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemberList {
    /// `true`: `upsert` is the complete list and `remove` is ignored.
    pub full: bool,
    /// Members added or whose role changed.
    pub upsert: Vec<Member>,
    /// Members removed (delta only).
    pub remove: Vec<UserId>,
}

impl MemberList {
    /// A full snapshot.
    pub fn snapshot(members: Vec<Member>) -> Self {
        Self {
            full: true,
            upsert: members,
            remove: Vec::new(),
        }
    }

    /// Applies the change to `members` (keeps first-seen order; later upserts replace roles).
    pub fn apply(&self, members: &mut Vec<Member>) {
        if self.full {
            members.clear();
        } else {
            members.retain(|m| !self.remove.contains(&m.user));
        }
        for m in &self.upsert {
            match members.iter_mut().find(|x| x.user == m.user) {
                Some(x) => x.role = m.role,
                None => members.push(m.clone()),
            }
        }
    }
}

/// A user-requested change of a contact's roaming state (`ChangeContact`). `Debug` hides the
/// nickname.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContactChange {
    /// Set or clear our nickname.
    SetNickname {
        /// The nickname; `None` clears it.
        #[serde(default)]
        nickname: Option<String>,
    },
    /// Block or unblock.
    SetBlocked {
        /// Blocked.
        blocked: bool,
    },
    /// Mark the identity verified or not.
    SetVerified {
        /// Verified.
        verified: bool,
    },
}

impl fmt::Debug for ContactChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SetNickname { nickname } => f
                .debug_struct("SetNickname")
                .field("nickname", &crate::log::Shape(nickname))
                .finish(),
            Self::SetBlocked { blocked } => f
                .debug_struct("SetBlocked")
                .field("blocked", blocked)
                .finish(),
            Self::SetVerified { verified } => f
                .debug_struct("SetVerified")
                .field("verified", verified)
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid(s: &str) -> UserId {
        UserId::parse(s).expect("valid")
    }

    fn contact() -> Contact {
        Contact {
            id: ContactId(1),
            account: AccountId(1),
            user: uid("aci:a"),
            profile_name: Some("Old".into()),
            system_name: Some("Sys".into()),
            nickname: None,
            avatar: None,
            trust: TrustLevel::Unverified,
            identity_fingerprint: None,
            blocked: false,
            hidden: false,
            unregistered_at: Some(5),
            identifiers: vec![],
            meta: None,
        }
    }

    #[test]
    fn contact_patch_unchanged_clear_set() {
        let mut c = contact();
        assert!(!ContactPatch::new(c.user.clone()).apply(&mut c));
        assert_eq!(c, contact());
        let p = ContactPatch {
            profile_name: Patch::Set("New".into()),
            system_name: Patch::Clear,
            unregistered_at: Patch::Clear,
            identity_fingerprint: Patch::Set(IdentityFingerprint(vec![1, 2])),
            blocked: Some(true),
            ..ContactPatch::new(c.user.clone())
        };
        let json = serde_json::to_string(&p).expect("serialize");
        let back: ContactPatch = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, p);
        assert!(back.apply(&mut c));
        assert_eq!(c.profile_name.as_deref(), Some("New"));
        assert_eq!(c.system_name, None);
        assert_eq!(c.nickname, None, "unchanged stays None");
        assert_eq!(c.unregistered_at, None);
        assert_eq!(c.identity_fingerprint, Some(vec![1, 2]));
        assert!(c.blocked && !c.hidden);
    }

    #[test]
    fn debug_hides_names_and_fingerprint() {
        let fp = IdentityFingerprint(vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(
            format!("{fp:?}"),
            "IdentityFingerprint([redacted; 4 bytes])"
        );
        let c = Contact {
            identity_fingerprint: Some(fp.0.clone()),
            ..contact()
        };
        let p = ContactPatch {
            nickname: Patch::Set("Nick".into()),
            identity_fingerprint: Patch::Set(fp),
            ..ContactPatch::new(c.user.clone())
        };
        let s = format!("{c:?} {p:?} {:?}", p.identity_fingerprint);
        for leak in ["Old", "Sys", "Nick", "222", "173", "de, ad"] {
            assert!(!s.contains(leak), "{leak} in {s}");
        }
    }

    #[test]
    fn minimal_contact_patch_json() {
        let p: ContactPatch = serde_json::from_str(r#"{"user":"aci:a"}"#).expect("deserialize");
        assert_eq!(p, ContactPatch::new(uid("aci:a")));
        assert_eq!(
            serde_json::to_string(&p).ok().as_deref(),
            Some(r#"{"user":"aci:a"}"#)
        );
    }

    #[test]
    fn member_list_full_and_delta() {
        let m = |s: &str, role| Member { user: uid(s), role };
        let mut members = vec![
            m("aci:a", MemberRole::Admin),
            m("aci:b", MemberRole::Member),
        ];
        MemberList {
            full: false,
            upsert: vec![
                m("aci:c", MemberRole::Member),
                m("aci:b", MemberRole::Admin),
            ],
            remove: vec![uid("aci:a")],
        }
        .apply(&mut members);
        assert_eq!(
            members,
            vec![
                m("aci:b", MemberRole::Admin),
                m("aci:c", MemberRole::Member)
            ]
        );
        MemberList::snapshot(vec![m("aci:d", MemberRole::Pending)]).apply(&mut members);
        assert_eq!(members, vec![m("aci:d", MemberRole::Pending)]);
    }
}

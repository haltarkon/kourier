//! Contacts, members and their patches (architecture §3.3).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::content::BlobRef;
use crate::ids::{AccountId, BackendBlob, ContactId, Identifier, UserId};
use crate::patch::Patch;

/// How far a contact's identity key is trusted.
///
/// NOT ordered by trust (no `Ord`, P5 critic M3): `Changed` is "worse" than `Unverified` and a
/// `max` would also move a trust without the key it was given to. A merge uses
/// [`TrustLevel::merge`], which moves the trust together with its fingerprint. core.db stores
/// the variant's NAME (TEXT, no CHECK), so a new variant needs no migration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TrustLevel {
    /// Not verified.
    #[default]
    Unverified,
    /// Verified by the server/directory (Threema "server verified").
    ServerVerified,
    /// Verified by the user (safety number / QR).
    Verified,
    /// The user verified this person, then the key changed (Signal's identity state 2,
    /// `UNVERIFIED`): the new key is not verified and a send waits for the user.
    Changed,
}

impl TrustLevel {
    /// The trust and fingerprint of a merged contact (`kept` = the canonical row, `alias` = the
    /// row merged into it): the trust MOVES WITH ITS FINGERPRINT (P5 critic M3). The kept row's
    /// fingerprint wins when it has one, and then its own trust stays (an alias's `Verified`
    /// was given to another key); a kept row without a fingerprint takes the alias's pair. With
    /// no fingerprint on either side the kept row's trust stays.
    pub fn merge<F>(
        kept: (TrustLevel, Option<F>),
        alias: (TrustLevel, Option<F>),
    ) -> (TrustLevel, Option<F>) {
        match (kept, alias) {
            (k @ (_, Some(_)), _) => k,
            (_, a @ (_, Some(_))) => a,
            ((t, None), _) => (t, None),
        }
    }
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
    /// The "about" text of the person's profile (P1).
    #[serde(default)]
    pub about: Option<String>,
    /// The emoji that goes with the about text.
    #[serde(default)]
    pub about_emoji: Option<String>,
    /// Whether the person's phone number may be shown (P1): `None` = the backend never said,
    /// which counts as NOT shown; the backend applies its network's privacy rule (Signal: the
    /// person shares it, or a system contact, or no profile key; architecture §6.10).
    #[serde(default)]
    pub number_shown: Option<bool>,
}

crate::redacting_debug!(Contact {
    id, account, user, avatar, trust, blocked, hidden, unregistered_at, identifiers, meta,
    number_shown
} redact { profile_name, system_name, nickname, identity_fingerprint, about, about_emoji });

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
    /// The about text.
    #[serde(default, skip_serializing_if = "Patch::is_unchanged")]
    pub about: Patch<String>,
    /// The emoji of the about text.
    #[serde(default, skip_serializing_if = "Patch::is_unchanged")]
    pub about_emoji: Patch<String>,
    /// Whether the phone number may be shown (see [`Contact::number_shown`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number_shown: Option<bool>,
}

crate::redacting_debug!(ContactPatch {
    user, avatar, trust, blocked, hidden, unregistered_at, identifiers, meta, number_shown
} redact { profile_name, system_name, nickname, identity_fingerprint, about, about_emoji });

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
            about: Patch::Unchanged,
            about_emoji: Patch::Unchanged,
            number_shown: None,
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
        t |= self.about.apply(&mut c.about);
        t |= self.about_emoji.apply(&mut c.about_emoji);
        if let Some(n) = self.number_shown {
            t |= c.number_shown != Some(n);
            c.number_shown = Some(n);
        }
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

/// The verification state of a person's identity key as the backend stores it (P5a; Signal's
/// `identities.verified` 0 / 1 / 2, Desktop's `VerifiedStatus`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VerifyState {
    /// Not verified ("Mark as Verified" is offered).
    #[default]
    Default,
    /// The user compared the safety numbers (here or on another device).
    Verified,
    /// The user had verified this person, then the key changed: sends wait for a confirm.
    Changed,
}

/// A safety number of one person (P5a), computed by the backend from OUR identity key and
/// the person's STORED key. `Debug` hides the digits, the scannable bytes and the key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyNumber {
    /// The 60 digits (Signal: 12 groups of 5, both halves sorted; no spaces).
    pub digits: crate::log::Redacted<String>,
    /// The bytes a QR code of OUR side carries (Signal: the `ScannableFingerprint` protobuf,
    /// 74 bytes for version 2).
    pub scannable: crate::log::Redacted<Vec<u8>>,
    /// The person's identity key the number was computed from: a Mark or a Clear names it,
    /// and the backend refuses when the stored key is no longer this one.
    pub key: IdentityFingerprint,
    /// The stored verification state of that key.
    pub state: VerifyState,
}

/// Why a person has no safety number to show (P5a).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SafetyUnavailable {
    /// The person has no account id the number is made of (Signal: no ACI, a PNI-only or a
    /// number-only person).
    NoAccountId,
    /// No identity key of the person is known yet (no message exchanged).
    NoKey,
    /// The person is the account itself.
    OwnId,
    /// The network has no safety numbers (or the backend cannot compute one).
    Unsupported,
    /// The account is not running (paused, logged out, still linking): the core's own answer
    /// when the backend could not be asked.
    NotRunning,
}

/// A backend's answer to `Command::SafetyNumber`.
pub type SafetyAnswer = Result<SafetyNumber, SafetyUnavailable>;

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
            about: None,
            about_emoji: None,
            number_shown: None,
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
    fn trust_merges_with_its_fingerprint() {
        use TrustLevel::*;
        assert_eq!(
            TrustLevel::merge((Unverified, Some(1)), (Verified, Some(2))),
            (Unverified, Some(1)),
            "the alias's Verified was for another key"
        );
        assert_eq!(
            TrustLevel::merge((Verified, None), (Changed, Some(2))),
            (Changed, Some(2)),
            "a kept row without a key takes the alias's pair"
        );
        assert_eq!(
            TrustLevel::merge::<u8>((Changed, None), (Verified, None)),
            (Changed, None)
        );
    }

    #[test]
    fn debug_hides_a_safety_number() {
        let n = SafetyNumber {
            digits: crate::log::Redacted::new("123451234512345".into()),
            scannable: crate::log::Redacted::new(vec![0xab; 74]),
            key: IdentityFingerprint(vec![0x05, 0xcd, 0xcd]),
            state: VerifyState::Changed,
        };
        let s = format!("{n:?}");
        for leak in ["12345", "171", "205"] {
            assert!(!s.contains(leak), "a safety number's Debug shows {leak}");
        }
        assert!(s.contains("Changed"), "the state is log-safe");
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

    /// P1: the about text and its emoji are as private as the names (architecture §9.1).
    #[test]
    fn debug_of_contact_and_patch_shows_no_about_text() {
        let c = Contact {
            about: Some("Gone fishing at Lake Zebra".into()),
            about_emoji: Some("\u{1F41F}".into()),
            number_shown: Some(true),
            ..contact()
        };
        let p = ContactPatch {
            about: Patch::Set("Gone fishing at Lake Zebra".into()),
            about_emoji: Patch::Set("\u{1F41F}".into()),
            number_shown: Some(true),
            ..ContactPatch::new(c.user.clone())
        };
        let s = format!("{c:?} {p:?}");
        for leak in ["Zebra", "fishing", "\u{1F41F}"] {
            assert!(!s.contains(leak), "{leak} in {s}");
        }
        assert!(
            s.contains("number_shown: Some(true)"),
            "the flag is not a secret: {s}"
        );
    }

    #[test]
    fn about_and_number_shown_patch_unchanged_clear_set() {
        let mut c = contact();
        let set = ContactPatch {
            about: Patch::Set("Hi".into()),
            about_emoji: Patch::Set("x".into()),
            number_shown: Some(false),
            ..ContactPatch::new(c.user.clone())
        };
        let json = serde_json::to_string(&set).expect("serialize");
        let back: ContactPatch = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, set);
        assert!(back.apply(&mut c));
        assert_eq!(c.about.as_deref(), Some("Hi"));
        assert_eq!(
            c.number_shown,
            Some(false),
            "a stated false is not 'never stated'"
        );
        // Unchanged keeps all three.
        assert!(!ContactPatch::new(c.user.clone()).apply(&mut c));
        assert_eq!(c.about.as_deref(), Some("Hi"));
        assert_eq!(c.number_shown, Some(false));
        // The same value again touches nothing; a clear clears.
        let again = ContactPatch {
            number_shown: Some(false),
            ..ContactPatch::new(c.user.clone())
        };
        assert!(!again.apply(&mut c));
        let clear = ContactPatch {
            about: Patch::Clear,
            ..ContactPatch::new(c.user.clone())
        };
        assert!(clear.apply(&mut c));
        assert_eq!(c.about, None);
        assert_eq!(c.about_emoji.as_deref(), Some("x"));
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

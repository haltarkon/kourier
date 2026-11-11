//! [`AllowList`]: the Threema IDs a link may bring (M6, TH4 decision B1). A normal start of the
//! application takes ANY ID ([`AllowList::any`], the user's decision of 2026-11-09: "Any ID, every
//! build"); only a start by the run tool is held to a list. A list holds SHA-256 digests of the
//! bytes `kourier-threema-allow-v1:` + the 8 ID characters, never an ID in plain text (the hash
//! keeps the ID out of plain sight; it is not a secret: brief §2.8).
//!
//! An EMPTY list (the default of [`crate::ThreemaParts`]) refuses every link at once, before any
//! transport is made (B1's gate in the actor's `StartLogin`); the ID check against a non-empty list
//! happens when the phone has sent its data (TH4 chunk J4).

use std::collections::BTreeSet;

use sha2::{Digest, Sha256};

/// The bytes in front of the ID in every digest (the list's format version).
const DOMAIN: &[u8] = b"kourier-threema-allow-v1:";

/// The digests of the IDs a link may bring. `Debug` prints the count only.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct AllowList {
    digests: BTreeSet<[u8; 32]>,
    /// Every ID may link, whatever `digests` holds.
    any: bool,
}

impl AllowList {
    /// The list that takes every ID: what a normal start of the application uses.
    #[must_use]
    pub fn any() -> Self {
        Self {
            digests: BTreeSet::new(),
            any: true,
        }
    }

    /// The list of these IDs (the tests' synthetic IDs; the application reads digests).
    #[must_use]
    pub fn from_ids(ids: &[&str]) -> Self {
        Self::from_digests(ids.iter().map(|id| Self::digest(id)))
    }

    /// The list of these digests (what the application reads from the run profile's file).
    #[must_use]
    pub fn from_digests(digests: impl IntoIterator<Item = [u8; 32]>) -> Self {
        Self {
            digests: digests.into_iter().collect(),
            any: false,
        }
    }

    /// The digest of `id`: SHA-256 of `kourier-threema-allow-v1:` + the ID's characters.
    #[must_use]
    pub fn digest(id: &str) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(DOMAIN);
        hash.update(id.as_bytes());
        hash.finalize().into()
    }

    /// Whether `id` is on the list.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.any || self.digests.contains(&Self::digest(id))
    }

    /// Whether the list takes no ID (every link is refused at once).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.any && self.digests.is_empty()
    }

    /// How many IDs the list holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.digests.len()
    }
}

/// The count only: no digest (a digest of a known ID would name it).
impl std::fmt::Debug for AllowList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.any {
            return f.write_str("AllowList(any id)");
        }
        write!(f, "AllowList({} ids)", self.digests.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `printf 'kourier-threema-allow-v1:TESTID01' | sha256sum` (coreutils, 2026-10-27).
    const TESTID01: &str = "871ad9802d67bec50e0d5a9d2c4113efec962437723daac9d86b8f1e0ab82a87";

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn the_digest_is_sha256_of_the_prefix_and_the_id() {
        assert_eq!(hex(&AllowList::digest("TESTID01")), TESTID01);
    }

    #[test]
    fn the_list_holds_its_ids_and_no_other() {
        let list = AllowList::from_ids(&["TESTID01", "TESTID02"]);
        assert!(list.contains("TESTID01") && list.contains("TESTID02"));
        assert!(
            !list.contains("TESTID03"),
            "an ID not on the list was taken"
        );
        assert!(
            !list.contains("testid01"),
            "the case of an ID is part of it"
        );
        assert_eq!(list.len(), 2);
        assert!(!list.is_empty());
        assert_eq!(
            AllowList::from_digests([AllowList::digest("TESTID01")]),
            AllowList::from_ids(&["TESTID01"])
        );
    }

    #[test]
    fn the_default_list_is_empty_and_takes_nothing() {
        let list = AllowList::default();
        assert!(list.is_empty());
        assert!(!list.contains("TESTID01"));
    }

    #[test]
    fn the_any_list_takes_every_id_and_is_not_empty() {
        let list = AllowList::any();
        assert!(!list.is_empty(), "an any list would refuse every link");
        assert!(list.contains("TESTID01") && list.contains("TESTID02"));
        assert_ne!(list, AllowList::default());
        assert_eq!(format!("{list:?}"), "AllowList(any id)");
    }

    #[test]
    fn debug_prints_the_count_only() {
        let printed = format!("{:?}", AllowList::from_ids(&["TESTID01"]));
        assert_eq!(printed, "AllowList(1 ids)");
        assert!(!printed.contains("TESTID01") && !printed.contains(&TESTID01[..8]));
    }
}

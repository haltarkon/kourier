//! What a group's revoke is judged by besides the row (W6a2, B7a; D11, M2, M3): two facts of the store, read for the rows
//! that need them and handed to the PURE map.
//!
//! - whether the core was told the group's WHOLE member list (`group_state.members_stated`): it judges an admin's
//!   delete-for-everyone by that list and DROPS the delete for good when the list is not there (D11), so the map waits
//!   for it;
//! - which `(number, LID)` pairs the store's `lid_pn_mapping` holds for the revoke's sender: a revoke whose key names
//!   the sender in his OTHER namespace is the author's own delete, not an admin's (M3).
//!
//! Without facts ([`super::map_row`]) the map cannot tell, and says so ([`super::Mapped::needs_facts`]): the caller reads
//! the facts of exactly those rows and maps them again. A deleted group in this struct is simply "not stated".

use std::collections::BTreeSet;
use std::fmt;

use kourier_model::{ConvKey, UserId};

/// The store's facts about a step's group revokes. `Default` knows nothing: no group has its members stated and no
/// number is paired with a LID.
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct GroupFacts {
    /// The groups (`ConvKey::encode`) whose whole member list the core was told.
    stated: BTreeSet<String>,
    /// `(number user, LID user)` pairs of `lid_pn_mapping` (the user parts of `pn:` and `lid:` ids).
    pairs: BTreeSet<(String, String)>,
}

/// `Debug` prints counts only: the facts are group keys and ids.
impl fmt::Debug for GroupFacts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GroupFacts")
            .field("groups", &self.stated.len())
            .field("pairs", &self.pairs.len())
            .finish()
    }
}

impl GroupFacts {
    /// The core was told the member list of `group`.
    pub(crate) fn with_members_of(mut self, group: &ConvKey) -> Self {
        self.stated.insert(group.encode());
        self
    }

    /// The store pairs `number` with `lid`.
    pub(crate) fn with_pair(mut self, number: &UserId, lid: &UserId) -> Self {
        self.pairs
            .insert((number.value().to_owned(), lid.value().to_owned()));
        self
    }

    /// Whether the core was told the whole member list of `group`.
    pub(crate) fn members_stated(&self, group: &ConvKey) -> bool {
        self.stated.contains(&group.encode())
    }

    /// Whether `a` and `b` are one person's two ids by a stored pair (a number and a LID of it, either way round).
    pub(crate) fn one_person(&self, a: &UserId, b: &UserId) -> bool {
        let (number, lid) = match (a.scheme(), b.scheme()) {
            ("pn", "lid") => (a, b),
            ("lid", "pn") => (b, a),
            _ => return false,
        };
        self.pairs
            .contains(&(number.value().to_owned(), lid.value().to_owned()))
    }
}

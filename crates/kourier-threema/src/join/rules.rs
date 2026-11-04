//! What the profile and the run say about the Threema ID a join brings (TH4 J4a, brief §2.7 steps 2-4;
//! WhatsApp's shape, `kourier-whatsapp/src/link.rs` `Rules`): checked on the parsed `EssentialData`, AFTER
//! [`super::essential`] passed it and BEFORE anything is written or the mediator hears of the device (M6:
//! a refused ID never gets a device).
//!
//! THE ORDER IS THE RULE: RELINK (a relink brought another account) -> TAKEN (the ID is another account of
//! the profile, `.claude/rules/core.md` "one identity is one account") -> ALLOW-LIST (M6, B1: the ID is not
//! on the run profile's list). The core's check at `AccountReady` stays the second line for the first two.

use kourier_backend_api::{Relink, TakenIdentity};
use kourier_model::{BackendError, ErrorKind};

use crate::AllowList;

/// The id scheme of a Threema account (`threema:<ID>`).
const SCHEME: &str = "threema";

/// Why a join's ID was turned away. Each ends the login with a `Failed` step at `linking`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// A relink brought another ID than the account's primary own id.
    NotTheRelinkedAccount,
    /// The ID is one of the profile's other accounts.
    AlreadyAdded { logged_out: bool },
    /// The ID is not on the run profile's allow-list (E2: at step `linking`; the viewmodel writes QC's
    /// sentence from the step and the kind).
    NotOnTheList,
}

impl Refusal {
    /// The error of the `Failed` step. The detail is a fixed phrase; nothing names the ID.
    pub(crate) fn error(self) -> BackendError {
        match self {
            Self::NotTheRelinkedAccount => BackendError::new(
                ErrorKind::InvalidInput,
                "the account is not the one being linked again",
            )
            .with_user_message("This is not the account that is being linked again."),
            Self::AlreadyAdded { logged_out } => BackendError::new(
                ErrorKind::AlreadyAdded { logged_out },
                "this account is already in the profile",
            )
            .with_user_message("This account is already in Kourier."),
            Self::NotOnTheList => BackendError::new(
                ErrorKind::NotTheExpectedAccount,
                "the Threema ID is not on the list of this profile",
            ),
        }
    }
}

/// The rules of one login, carried from `StartLogin` (immutable for the login). `Debug` prints counts.
#[derive(Clone)]
pub(crate) struct Rules {
    /// Every own id of the profile's other accounts on this network.
    taken: Vec<TakenIdentity>,
    /// Set for a relink: the ids of the account being linked again (the primary first).
    relink: Option<Relink>,
    /// The run profile's IDs (M6).
    allowed: AllowList,
}

impl std::fmt::Debug for Rules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rules")
            .field("taken", &self.taken.len())
            .field("relink", &self.relink.is_some())
            .field("allowed", &self.allowed)
            .finish()
    }
}

impl Rules {
    pub(crate) fn new(
        taken: Vec<TakenIdentity>,
        relink: Option<Relink>,
        allowed: AllowList,
    ) -> Self {
        Self {
            taken,
            relink,
            allowed,
        }
    }

    /// The checks on the Threema ID the phone sent (already accepted by libthreema's `ThreemaId`), in
    /// the module's order.
    pub(crate) fn check(&self, threema_id: &str) -> Result<(), Refusal> {
        let is_this =
            |user: &kourier_model::UserId| user.scheme() == SCHEME && user.value() == threema_id;
        // RELINK: the account's PRIMARY own id (the core's rule, `supervisor.rs` `relink_mismatch`);
        // a relink of an account whose ids the core does not know is not checked, as the core does.
        if let Some(relink) = &self.relink
            && let Some(primary) = relink.expect_own.first()
            && !is_this(primary)
        {
            return Err(Refusal::NotTheRelinkedAccount);
        }
        // TAKEN.
        if let Some(taken) = self.taken.iter().find(|t| is_this(&t.user)) {
            return Err(Refusal::AlreadyAdded {
                logged_out: taken.logged_out,
            });
        }
        // ALLOW-LIST (an empty list takes nothing: fail closed).
        if !self.allowed.contains(threema_id) {
            return Err(Refusal::NotOnTheList);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use kourier_model::{AccountId, UserId};

    use super::*;

    /// Synthetic IDs, each a substring of no other test value.
    const ID: &str = "J4ARULE1";
    const OTHER: &str = "J4ARULE2";

    fn user(id: &str) -> UserId {
        UserId::from_parts(SCHEME, id).expect("a valid id")
    }

    fn taken(id: &str, logged_out: bool) -> TakenIdentity {
        TakenIdentity {
            user: user(id),
            logged_out,
        }
    }

    fn relink(ids: &[&str]) -> Option<Relink> {
        Some(Relink {
            account: AccountId(7),
            expect_own: ids.iter().map(|id| user(id)).collect(),
        })
    }

    fn listed(ids: &[&str]) -> AllowList {
        AllowList::from_ids(ids)
    }

    #[test]
    fn a_first_link_of_a_listed_id_passes() {
        let rules = Rules::new(vec![taken(OTHER, false)], None, listed(&[ID]));
        assert_eq!(rules.check(ID), Ok(()));
    }

    #[test]
    fn a_relink_must_bring_the_primary_own_id() {
        let same = Rules::new(Vec::new(), relink(&[ID, OTHER]), listed(&[ID, OTHER]));
        assert_eq!(same.check(ID), Ok(()));
        // The secondary id is not enough (the core compares the primary).
        assert_eq!(same.check(OTHER), Err(Refusal::NotTheRelinkedAccount));
        let other = Rules::new(Vec::new(), relink(&[OTHER]), listed(&[ID, OTHER]));
        assert_eq!(other.check(ID), Err(Refusal::NotTheRelinkedAccount));
        // An id of another network's scheme with the same text is not this ID.
        let foreign = Rules::new(
            Vec::new(),
            Some(Relink {
                account: AccountId(7),
                expect_own: vec![UserId::from_parts("aci", ID).expect("id")],
            }),
            listed(&[ID]),
        );
        assert_eq!(foreign.check(ID), Err(Refusal::NotTheRelinkedAccount));
        // A relink of an account whose ids are not known is not checked (the core's rule).
        let unknown = Rules::new(Vec::new(), relink(&[]), listed(&[ID]));
        assert_eq!(unknown.check(ID), Ok(()));
    }

    #[test]
    fn a_taken_id_is_already_added_active_or_logged_out() {
        let active = Rules::new(
            vec![taken(OTHER, true), taken(ID, false)],
            None,
            listed(&[ID]),
        );
        assert_eq!(
            active.check(ID),
            Err(Refusal::AlreadyAdded { logged_out: false })
        );
        let logged_out = Rules::new(vec![taken(ID, true)], None, listed(&[ID]));
        assert_eq!(
            logged_out.check(ID),
            Err(Refusal::AlreadyAdded { logged_out: true })
        );
        // Another scheme's id with the same text is not taken.
        let foreign = Rules::new(
            vec![TakenIdentity {
                user: UserId::from_parts("aci", ID).expect("id"),
                logged_out: false,
            }],
            None,
            listed(&[ID]),
        );
        assert_eq!(foreign.check(ID), Ok(()));
    }

    #[test]
    fn the_allow_list_takes_its_ids_only_and_an_empty_one_none() {
        let hit = Rules::new(Vec::new(), None, listed(&[OTHER, ID]));
        assert_eq!(hit.check(ID), Ok(()));
        let miss = Rules::new(Vec::new(), None, listed(&[OTHER]));
        assert_eq!(miss.check(ID), Err(Refusal::NotOnTheList));
        let empty = Rules::new(Vec::new(), None, AllowList::default());
        assert_eq!(empty.check(ID), Err(Refusal::NotOnTheList));
    }

    /// RELINK before TAKEN before the ALLOW-LIST.
    #[test]
    fn the_order_is_relink_then_taken_then_the_allow_list() {
        // Taken AND not on the list: AlreadyAdded.
        let taken_unlisted = Rules::new(vec![taken(ID, true)], None, listed(&[OTHER]));
        assert_eq!(
            taken_unlisted.check(ID),
            Err(Refusal::AlreadyAdded { logged_out: true })
        );
        // A relink mismatch AND taken AND not on the list: the relink.
        let all_three = Rules::new(vec![taken(ID, false)], relink(&[OTHER]), listed(&[OTHER]));
        assert_eq!(all_three.check(ID), Err(Refusal::NotTheRelinkedAccount));
        // A relink mismatch AND not on the list: the relink.
        let relink_unlisted = Rules::new(Vec::new(), relink(&[OTHER]), AllowList::default());
        assert_eq!(
            relink_unlisted.check(ID),
            Err(Refusal::NotTheRelinkedAccount)
        );
    }

    #[test]
    fn each_refusal_has_its_typed_kind_and_names_no_id() {
        let kinds = [
            (Refusal::NotTheRelinkedAccount, ErrorKind::InvalidInput),
            (
                Refusal::AlreadyAdded { logged_out: false },
                ErrorKind::AlreadyAdded { logged_out: false },
            ),
            (
                Refusal::AlreadyAdded { logged_out: true },
                ErrorKind::AlreadyAdded { logged_out: true },
            ),
            (Refusal::NotOnTheList, ErrorKind::NotTheExpectedAccount),
        ];
        for (refusal, kind) in kinds {
            let error = refusal.error();
            assert_eq!(error.kind, kind);
            assert!(!format!("{error:?}").contains(ID), "a refusal names the ID");
        }
        let rules = Rules::new(vec![taken(ID, false)], relink(&[ID]), listed(&[ID]));
        let printed = format!("{rules:?}");
        assert!(
            !printed.contains(ID) && printed.contains("taken: 1"),
            "Debug of the rules prints an ID or no count"
        );
    }
}

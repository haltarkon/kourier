//! WHO GETS A MESSAGE (TH6a S4): step 2 of the Bundled Messages Send Steps (`csp.struct.yml`) over a read view of the
//! file: each receiver's [`Fate`], and the receivers of a group. Pure over the view: no write, no clock, no log.
//!
//! THE ORDER OF A FATE is the spec's: the user itself; marked invalid; the Identity Blocked Steps unless the type is
//! exempt; then the key. What a receiver with no stored key gets is the lane's (C13): no box, the rest goes.
//!
//! A GROUP'S RECEIVERS are its stored members, the creator added (`md-d2d-sync.proto`: the list holds neither the
//! creator nor the user), under the rule of a Threema Gateway creator: a `*` creator gets a message only when the
//! group's name starts with the cloud or the type says always (`shouldSendGroupMessageToCreator` of Desktop's
//! `task/index.ts`).
//!
//! STEP 9's LOOK AT THE GROUPS (under `BLOCK_UNKNOWN`, for a contact that is not direct) is ONE walk of the stored
//! groups per [`fates`] call, taken lazily by the first receiver that gets as far and kept for the rest: a set of the
//! identities that are the creator or a member of a group the user is in. A walk per receiver decoded every stored
//! group once per member inside the write zone (S-M2).

use std::collections::HashSet;
use std::fmt;

use libthreema::common::ThreemaId;
use libthreema::common::keys::{ClientKey, CspE2eKey, PublicKey};
use libthreema::protobuf::d2d_sync;

use super::props::{Exempt, GatewayCreator};
use crate::error::Result;

/// The contacts the spec marks _special_ (`csp.struct.yml`, "Predefined Contacts"): never blocked.
const SPECIAL_CONTACTS: [&str; 2] = ["*3MAPUSH", "*3MAW0RK"];

/// Every predefined contact of the pinned spec, the special ones too: not blocked by the unknown-contact policy.
/// (Desktop lists four of the eight: its Work contacts are not in its build.)
const PREDEFINED_CONTACTS: [&str; 8] = [
    "*3MAPUSH", "*3MATOKN", "*3MAW0RK", "*3MAWORK", "*BETAFBK", "*MY3DATA", "*SUPPORT", "*THREEMA",
];

/// A group whose name starts with this is captured by its Threema Gateway creator.
const CLOUD: char = '\u{2601}';

/// What the send steps read of the file.
pub(crate) trait View {
    /// The stored contact of `identity`.
    fn contact(&self, identity: &str) -> Result<Option<d2d_sync::Contact>>;
    /// The stored settings (`None`: the file has no row, nobody is blocked).
    fn settings(&self) -> Result<Option<d2d_sync::Settings>>;
    /// Every identity that is the creator or a member of a stored group the user has not left: ONE walk of the groups.
    /// [`fates`] asks at most once per call, and only when a receiver needs it (S-M2).
    fn identities_in_groups(&self) -> Result<HashSet<String>>;
}

/// What becomes of one receiver.
pub(crate) enum Fate {
    /// A box, under the key shared with it.
    Send(CspE2eKey),
    /// The user itself: no box and no row (the spec: log a warning, remove it).
    Own,
    /// Marked invalid (revoked): no box.
    Invalid,
    /// The Identity Blocked Steps blocked it: no box.
    Blocked,
    /// The file holds no usable key of it (no contact, no 32-byte key, or a non-contributory one): no box.
    NoKey,
}

impl fmt::Debug for Fate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Send(_) => "Send",
            Self::Own => "Own",
            Self::Invalid => "Invalid",
            Self::Blocked => "Blocked",
            Self::NoKey => "NoKey",
        })
    }
}

/// The two settings the Identity Blocked Steps read.
struct Policy {
    blocked: Vec<String>,
    block_unknown: bool,
}

impl Policy {
    fn of(settings: Option<d2d_sync::Settings>) -> Self {
        use d2d_sync::settings::UnknownContactPolicy;
        let settings = settings.unwrap_or_default();
        Self {
            block_unknown: settings.unknown_contact_policy
                == Some(UnknownContactPolicy::BlockUnknown as i32),
            blocked: settings
                .blocked_identities
                .map(|b| b.identities)
                .unwrap_or_default(),
        }
    }
}

/// THE IDENTITY BLOCKED STEPS (`csp.struct.yml`), step by step. `contact` is what the file holds of `identity`.
/// `in_groups` is the look at the groups (step 9), made on the first receiver that gets that far and kept for the
/// rest of the call: a group of 256 members is one walk of the groups, not 256 (S-M2).
fn is_blocked(
    view: &dyn View,
    policy: &Policy,
    in_groups: &mut Option<HashSet<String>>,
    identity: &str,
    contact: Option<&d2d_sync::Contact>,
) -> Result<bool> {
    use d2d_sync::contact::AcquaintanceLevel;
    if SPECIAL_CONTACTS.contains(&identity) {
        return Ok(false);
    }
    if policy.blocked.iter().any(|b| b == identity) {
        return Ok(true);
    }
    if !policy.block_unknown || PREDEFINED_CONTACTS.contains(&identity) {
        return Ok(false);
    }
    let Some(contact) = contact else {
        return Ok(true);
    };
    if contact
        .acquaintance_level
        .is_none_or(|level| level == AcquaintanceLevel::Direct as i32)
    {
        return Ok(false);
    }
    if in_groups.is_none() {
        *in_groups = Some(view.identities_in_groups()?);
    }
    Ok(!in_groups
        .as_ref()
        .is_some_and(|identities| identities.contains(identity)))
}

/// The key shared with a stored contact; `None` for a missing or malformed key, and for a non-contributory one.
fn shared_key(ck: &ClientKey, contact: Option<&d2d_sync::Contact>) -> Option<CspE2eKey> {
    let public = PublicKey::try_from(contact?.public_key.as_deref()?).ok()?;
    ck.derive_csp_e2e_key(&public)
}

/// The fate of each of `listed`, in that order. `exempt_of` says what the type of the message FOR THAT RECEIVER asks of
/// blocking: only [`Exempt::Yes`] skips the steps. `DedicatedSteps` does not: the spec's dedicated steps of a
/// `group-setup` are the receiver's, and Desktop's send runs the blocked steps for anything but `true`.
pub(crate) fn fates(
    view: &dyn View,
    ck: &ClientKey,
    own: ThreemaId,
    listed: &[String],
    exempt_of: &dyn Fn(&str) -> Exempt,
) -> Result<Vec<(String, Fate)>> {
    use d2d_sync::contact::ActivityState;
    let policy = Policy::of(view.settings()?);
    let mut in_groups = None;
    listed
        .iter()
        .map(|identity| {
            if identity == own.as_str() {
                return Ok((identity.clone(), Fate::Own));
            }
            let contact = view.contact(identity)?;
            let contact = contact.as_ref();
            let fate =
                if contact.and_then(|c| c.activity_state) == Some(ActivityState::Invalid as i32) {
                    Fate::Invalid
                } else if exempt_of(identity.as_str()) != Exempt::Yes
                    && is_blocked(view, &policy, &mut in_groups, identity, contact)?
                {
                    Fate::Blocked
                } else {
                    shared_key(ck, contact).map_or(Fate::NoKey, Fate::Send)
                };
            Ok((identity.clone(), fate))
        })
        .collect()
}

/// Whether a message goes to the group's creator, who is not the user.
fn creator_gets(creator: ThreemaId, name: &str, rule: GatewayCreator) -> bool {
    !creator.is_gateway_id() || name.starts_with(CLOUD) || rule == GatewayCreator::Always
}

/// The receivers of a message into a stored group: its members and the creator under `rule`, sorted, each once. The
/// user may stand in the list ([`Fate::Own`] removes it); a creator the list names counts as the creator, whatever the
/// rule says of it. EMPTY for a notes group.
pub(crate) fn group_receivers(
    group: &d2d_sync::Group,
    creator: ThreemaId,
    own: ThreemaId,
    rule: GatewayCreator,
) -> Vec<String> {
    let mut ids: Vec<String> = group
        .member_identities
        .iter()
        .flat_map(|m| m.identities.iter())
        .filter(|id| *id != creator.as_str())
        .cloned()
        .collect();
    let name = group.name.as_deref().unwrap_or_default();
    if creator != own && creator_gets(creator, name, rule) {
        ids.push(creator.as_str().to_owned());
    }
    ids.sort_unstable();
    ids.dedup();
    ids
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::BTreeMap;

    use d2d_sync::contact::{AcquaintanceLevel, ActivityState};
    use d2d_sync::settings::UnknownContactPolicy;
    use libthreema::protobuf::common::Identities;

    use super::*;
    use crate::error::Result;

    const OWN: &str = "KOURIER3";
    const ANN: &str = "PEER0001";
    const GATE: &str = "*GATEWAY";

    fn id(text: &str) -> ThreemaId {
        ThreemaId::try_from(text).expect("a synthetic Threema ID")
    }

    /// A key that gives a real shared secret.
    const GOOD_KEY: [u8; 32] = [7; 32];

    fn contact(
        identity: &str,
        level: AcquaintanceLevel,
        state: ActivityState,
    ) -> d2d_sync::Contact {
        d2d_sync::Contact {
            identity: identity.to_owned(),
            public_key: Some(GOOD_KEY.to_vec()),
            acquaintance_level: Some(level as i32),
            activity_state: Some(state as i32),
            ..Default::default()
        }
    }

    fn direct(identity: &str) -> d2d_sync::Contact {
        contact(identity, AcquaintanceLevel::Direct, ActivityState::Active)
    }

    #[derive(Default)]
    struct Fixture {
        contacts: BTreeMap<String, d2d_sync::Contact>,
        settings: Option<d2d_sync::Settings>,
        in_groups: Vec<String>,
        /// How many times the view was asked for the identities of the groups (S-M2).
        walks: Cell<usize>,
    }

    impl View for Fixture {
        fn contact(&self, identity: &str) -> Result<Option<d2d_sync::Contact>> {
            Ok(self.contacts.get(identity).cloned())
        }

        fn settings(&self) -> Result<Option<d2d_sync::Settings>> {
            Ok(self.settings.clone())
        }

        fn identities_in_groups(&self) -> Result<HashSet<String>> {
            self.walks.set(self.walks.get() + 1);
            Ok(self.in_groups.iter().cloned().collect())
        }
    }

    impl Fixture {
        fn with(mut self, contact: d2d_sync::Contact) -> Self {
            self.contacts.insert(contact.identity.clone(), contact);
            self
        }

        fn policy(mut self, blocked: &[&str], policy: UnknownContactPolicy) -> Self {
            self.settings = Some(d2d_sync::Settings {
                blocked_identities: Some(Identities {
                    identities: blocked.iter().map(|b| (*b).to_owned()).collect(),
                }),
                unknown_contact_policy: Some(policy as i32),
                ..Default::default()
            });
            self
        }

        /// The name of `identity`'s fate under `exempt`.
        fn fate(&self, identity: &str, exempt: Exempt) -> String {
            let ck = ClientKey::from([0x11; 32]);
            let listed = [identity.to_owned()];
            let mut found = fates(self, &ck, id(OWN), &listed, &|_| exempt).expect("the fates");
            assert_eq!(found.len(), 1);
            let (who, fate) = found.remove(0);
            assert!(who == identity, "the fate of another receiver");
            format!("{fate:?}")
        }
    }

    /// The user itself is removed first, whatever the file says of it (a contact row of the own ID, a key).
    #[test]
    fn the_user_itself_is_own_whatever_the_file_says() {
        let view = Fixture::default().with(direct(OWN));
        assert_eq!(view.fate(OWN, Exempt::No), "Own");
        assert_eq!(view.fate(OWN, Exempt::Yes), "Own");
    }

    /// `activity_state = INVALID` comes before blocking and takes no box, exempt or not; `INACTIVE` still gets one.
    #[test]
    fn an_invalid_contact_gets_nothing_and_an_inactive_one_does() {
        let view = Fixture::default()
            .with(contact(
                ANN,
                AcquaintanceLevel::Direct,
                ActivityState::Invalid,
            ))
            .policy(&[ANN], UnknownContactPolicy::AllowUnknown);
        assert_eq!(view.fate(ANN, Exempt::No), "Invalid");
        assert_eq!(view.fate(ANN, Exempt::Yes), "Invalid");
        let view = Fixture::default().with(contact(
            ANN,
            AcquaintanceLevel::Direct,
            ActivityState::Inactive,
        ));
        assert_eq!(view.fate(ANN, Exempt::No), "Send");
    }

    /// An explicit block holds unless the type is exempt; `DedicatedSteps` runs the steps as Desktop's send does.
    #[test]
    fn an_explicit_block_holds_unless_the_type_is_exempt() {
        let view = Fixture::default()
            .with(direct(ANN))
            .policy(&[ANN], UnknownContactPolicy::AllowUnknown);
        assert_eq!(view.fate(ANN, Exempt::No), "Blocked");
        assert_eq!(view.fate(ANN, Exempt::DedicatedSteps), "Blocked");
        assert_eq!(view.fate(ANN, Exempt::Yes), "Send");
    }

    /// The steps in the spec's order, every line of them under `BLOCK_UNKNOWN`.
    #[test]
    fn the_identity_blocked_steps_line_by_line() {
        use AcquaintanceLevel::GroupOrDeleted;
        let strict = UnknownContactPolicy::BlockUnknown;
        // 2: a special contact is not blocked, not even explicitly (it has a stored key here).
        for (at, special) in ["*3MAPUSH", "*3MAW0RK"].into_iter().enumerate() {
            let view = Fixture::default()
                .with(direct(special))
                .policy(&[special], strict);
            assert_eq!(
                view.fate(special, Exempt::No),
                "Send",
                "special contact {at}"
            );
        }
        // 3: a predefined contact that is not special can be blocked explicitly.
        let view = Fixture::default()
            .with(direct("*SUPPORT"))
            .policy(&["*SUPPORT"], strict);
        assert_eq!(view.fate("*SUPPORT", Exempt::No), "Blocked");
        // 4: the policy allows: an unknown contact is not blocked and only has no key.
        let view = Fixture::default().policy(&[], UnknownContactPolicy::AllowUnknown);
        assert_eq!(view.fate(ANN, Exempt::No), "NoKey");
        // No settings row at all: the same.
        assert_eq!(Fixture::default().fate(ANN, Exempt::No), "NoKey");
        // 5: every predefined contact passes the policy, stored or not.
        assert_eq!(PREDEFINED_CONTACTS.len(), 8);
        for (at, predefined) in PREDEFINED_CONTACTS.into_iter().enumerate() {
            let view = Fixture::default().policy(&[], strict);
            assert_eq!(
                view.fate(predefined, Exempt::No),
                "NoKey",
                "predefined contact {at}"
            );
        }
        // 7: no contact: blocked. An exempt type goes on to the key.
        let view = Fixture::default().policy(&[], strict);
        assert_eq!(view.fate(ANN, Exempt::No), "Blocked");
        assert_eq!(view.fate(ANN, Exempt::Yes), "NoKey");
        // 8: direct: not blocked.
        let view = Fixture::default().with(direct(ANN)).policy(&[], strict);
        assert_eq!(view.fate(ANN, Exempt::No), "Send");
        // 9: not direct, in a group: not blocked.
        let mut view = Fixture::default()
            .with(contact(ANN, GroupOrDeleted, ActivityState::Active))
            .policy(&[], strict);
        view.in_groups.push(ANN.to_owned());
        assert_eq!(view.fate(ANN, Exempt::No), "Send");
        // 10: not direct and in no group: blocked.
        view.in_groups.clear();
        assert_eq!(view.fate(ANN, Exempt::No), "Blocked");
        // The level is only read under the policy: the same contact goes when the policy allows.
        let view = Fixture::default().with(contact(ANN, GroupOrDeleted, ActivityState::Active));
        assert_eq!(view.fate(ANN, Exempt::No), "Send");
        // An absent level is the proto's default, direct.
        let mut bare = direct(ANN);
        bare.acquaintance_level = None;
        let view = Fixture::default().with(bare).policy(&[], strict);
        assert_eq!(view.fate(ANN, Exempt::No), "Send");
    }

    /// S-M2: the look at the groups is ONE walk per call of `fates` however many receivers need it, and none when no
    /// receiver gets as far as step 9. (The per-receiver walk read every stored group once per member.)
    #[test]
    fn the_groups_are_walked_once_per_call_and_only_when_a_receiver_needs_it() {
        use AcquaintanceLevel::GroupOrDeleted;
        const CAL: &str = "PEER0002";
        const DAN: &str = "PEER0003";
        let strict = UnknownContactPolicy::BlockUnknown;
        let ck = ClientKey::from([0x11; 32]);
        let names = |view: &Fixture, listed: &[&str], exempt: Exempt| -> Vec<String> {
            let listed: Vec<String> = listed.iter().map(|i| (*i).to_owned()).collect();
            let found = fates(view, &ck, id(OWN), &listed, &|_| exempt).expect("the fates");
            found.iter().map(|(_, fate)| format!("{fate:?}")).collect()
        };

        // Three receivers that are not direct, two of them in a group: ONE walk, and each gets its own answer.
        let mut view = Fixture::default()
            .with(contact(ANN, GroupOrDeleted, ActivityState::Active))
            .with(contact(CAL, GroupOrDeleted, ActivityState::Active))
            .with(contact(DAN, GroupOrDeleted, ActivityState::Active))
            .policy(&[], strict);
        view.in_groups = vec![ANN.to_owned(), DAN.to_owned()];
        assert_eq!(
            names(&view, &[ANN, CAL, DAN], Exempt::No),
            ["Send", "Blocked", "Send"],
            "the fates of three receivers that are not direct"
        );
        assert_eq!(
            view.walks.get(),
            1,
            "the walks of the groups for 3 receivers"
        );
        // The cache is the call's own: the next call looks again (the groups may have changed by then).
        names(&view, &[ANN], Exempt::No);
        assert_eq!(view.walks.get(), 2, "the walks after a second call");

        // No receiver gets as far as step 9: no walk. A direct contact, one with no level, one explicitly blocked,
        // one unknown (blocked at step 7), a predefined one, an exempt type, the user itself, an invalid contact, and
        // a policy that allows unknown contacts all end before it.
        let mut bare = direct(DAN);
        bare.acquaintance_level = None;
        let quiet = Fixture::default()
            .with(direct(ANN))
            .with(bare)
            .with(contact(CAL, GroupOrDeleted, ActivityState::Invalid))
            .with(direct(OWN))
            .policy(&["PEER0004"], strict);
        let listed = [ANN, DAN, CAL, OWN, "PEER0005", "*SUPPORT", "PEER0004"];
        assert_eq!(
            names(&quiet, &listed, Exempt::No),
            [
                "Send", "Send", "Invalid", "Own", "Blocked", "NoKey", "Blocked"
            ],
            "the fates of receivers that end before step 9"
        );
        assert_eq!(
            quiet.walks.get(),
            0,
            "the walks when no receiver needs them"
        );
        let exempt = Fixture::default()
            .with(contact(ANN, GroupOrDeleted, ActivityState::Active))
            .policy(&[], strict);
        assert_eq!(
            names(&exempt, &[ANN], Exempt::Yes),
            ["Send"],
            "an exempt type"
        );
        assert_eq!(exempt.walks.get(), 0, "the walks for an exempt type");
        let allowing = Fixture::default()
            .with(contact(ANN, GroupOrDeleted, ActivityState::Active))
            .policy(&[], UnknownContactPolicy::AllowUnknown);
        assert_eq!(
            names(&allowing, &[ANN], Exempt::No),
            ["Send"],
            "a policy that allows"
        );
        assert_eq!(
            allowing.walks.get(),
            0,
            "the walks under a policy that allows"
        );
    }

    /// No key, a key of the wrong length and a non-contributory key (all zeroes) are no box and no failure.
    #[test]
    fn a_contact_without_a_usable_key_is_no_key() {
        let bent = |key: Option<Vec<u8>>| {
            let mut c = direct(ANN);
            c.public_key = key;
            Fixture::default().with(c).fate(ANN, Exempt::No)
        };
        assert_eq!(bent(None), "NoKey");
        assert_eq!(bent(Some(vec![7; 31])), "NoKey");
        assert_eq!(bent(Some(vec![7; 33])), "NoKey");
        assert_eq!(bent(Some(vec![0; 32])), "NoKey");
        assert_eq!(bent(Some(vec![7; 32])), "Send");
    }

    /// Each receiver gets the answer for ITS type, in the list's order.
    #[test]
    fn each_receiver_is_judged_by_the_type_it_gets() {
        let view = Fixture::default()
            .with(direct(ANN))
            .with(direct("PEER0002"))
            .policy(&[ANN, "PEER0002"], UnknownContactPolicy::AllowUnknown);
        let ck = ClientKey::from([0x11; 32]);
        let listed = [ANN.to_owned(), "PEER0002".to_owned()];
        let found = fates(&view, &ck, id(OWN), &listed, &|who| {
            if who == ANN { Exempt::Yes } else { Exempt::No }
        })
        .expect("the fates");
        let names: Vec<String> = found.iter().map(|(_, fate)| format!("{fate:?}")).collect();
        assert_eq!(names, ["Send", "Blocked"]);
        assert!(
            found[0].0 == ANN && found[1].0 == "PEER0002",
            "the list's order"
        );
    }

    fn group(name: Option<&str>, members: Option<&[&str]>) -> d2d_sync::Group {
        d2d_sync::Group {
            name: name.map(str::to_owned),
            member_identities: members.map(|m| Identities {
                identities: m.iter().map(|i| (*i).to_owned()).collect(),
            }),
            ..Default::default()
        }
    }

    /// An ordinary creator is added whatever the rule, sorted in, each once; the creator that is the user is not.
    #[test]
    fn a_group_is_its_members_and_the_creator() {
        let g = group(Some("friends"), Some(&["PEER0003", "PEER0001"]));
        for rule in [
            GatewayCreator::NotApplicable,
            GatewayCreator::IfCaptured,
            GatewayCreator::Always,
        ] {
            assert!(
                group_receivers(&g, id("PEER0002"), id(OWN), rule)
                    == ["PEER0001", "PEER0002", "PEER0003"],
                "{rule:?}"
            );
            assert!(
                group_receivers(&g, id(OWN), id(OWN), rule) == ["PEER0001", "PEER0003"],
                "{rule:?}: the user's own group"
            );
        }
        // Listed twice, or the creator listed as well: once.
        let g = group(None, Some(&["PEER0001", "PEER0001", "PEER0002"]));
        assert!(
            group_receivers(&g, id("PEER0002"), id(OWN), GatewayCreator::IfCaptured)
                == ["PEER0001", "PEER0002"],
            "the receivers differ"
        );
        // The user in the list stays for its fate to remove (the spec's warning).
        let g = group(None, Some(&[OWN, ANN]));
        assert!(
            group_receivers(&g, id("PEER0002"), id(OWN), GatewayCreator::IfCaptured)
                == [OWN, ANN, "PEER0002"],
            "the receivers differ"
        );
        // A notes group: nobody; no member list at all: the same.
        assert!(
            group_receivers(
                &group(None, None),
                id(OWN),
                id(OWN),
                GatewayCreator::IfCaptured
            )
            .is_empty()
        );
        assert!(
            group_receivers(
                &group(None, Some(&[])),
                id(OWN),
                id(OWN),
                GatewayCreator::IfCaptured
            )
            .is_empty()
        );
    }

    /// The Threema Gateway creator (Desktop's `shouldSendGroupMessageToCreator`): the cloud captures every type, without
    /// it only `Always`.
    #[test]
    fn a_gateway_creator_gets_a_message_when_captured_or_when_the_type_says_always() {
        use GatewayCreator::{Always, IfCaptured, NotApplicable};
        let receivers = |name: &str, rule| {
            group_receivers(&group(Some(name), Some(&[ANN])), id(GATE), id(OWN), rule)
        };
        let with = [GATE, ANN];
        let without = [ANN];
        assert!(
            receivers("plain", IfCaptured) == without,
            "the receivers differ"
        );
        assert!(
            receivers("plain", NotApplicable) == without,
            "the receivers differ"
        );
        assert!(receivers("plain", Always) == with, "the receivers differ");
        assert!(
            receivers("\u{2601} captured", IfCaptured) == with,
            "the receivers differ"
        );
        assert!(
            receivers("\u{2601}\u{fe0f} captured", IfCaptured) == with,
            "the receivers differ"
        );
        assert!(
            receivers("\u{2601} captured", NotApplicable) == with,
            "the receivers differ"
        );
        assert!(
            receivers("not \u{2601} first", IfCaptured) == without,
            "the receivers differ"
        );
        // A list that names the Gateway creator does not get round the rule.
        let listed = group(Some("plain"), Some(&[GATE, ANN]));
        assert!(
            group_receivers(&listed, id(GATE), id(OWN), IfCaptured) == without,
            "the receivers differ"
        );
        // No name at all: not captured.
        let unnamed = group(None, Some(&[ANN]));
        assert!(
            group_receivers(&unnamed, id(GATE), id(OWN), IfCaptured) == without,
            "the receivers differ"
        );
    }
}

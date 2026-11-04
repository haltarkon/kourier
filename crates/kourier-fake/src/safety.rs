//! The fake's safety numbers (P5a, [`crate::Script::safety`]): which key it holds for a person,
//! the synthetic number it shows for it, and what a Mark or a Clear does. The numbers are made
//! from the key bytes, not by Signal's algorithm (the fake links no libsignal): different keys
//! give different digits, and the scannable bytes have the length and the frame of a real
//! version-2 `ScannableFingerprint` (74 bytes, critic F20), so a QR code of them has the real
//! module count.

use kourier_model::{IdentityFingerprint, Redacted, SafetyNumber, TrustLevel, VerifyState};

/// One person's keys and which of them the fake holds now.
#[derive(Clone, Debug)]
pub(crate) struct Held {
    keys: Vec<(IdentityFingerprint, VerifyState)>,
    at: usize,
    /// How many more asks are answered "no key yet" before the first key is shown (Y1: a
    /// person whose key the fake learns late).
    keyless: u32,
}

/// What a `SetVerified` the fake took changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Applied {
    /// The state before.
    pub(crate) was: VerifyState,
    /// The state now.
    pub(crate) now: VerifyState,
    /// The key it applies to (the held one).
    pub(crate) key: IdentityFingerprint,
}

impl Held {
    /// `None` for a script entry without keys. The first `keyless` asks ([`Held::ask`]) are
    /// answered "no key yet".
    pub(crate) fn new(keys: &[(IdentityFingerprint, VerifyState)], keyless: u32) -> Option<Self> {
        (!keys.is_empty()).then(|| Self {
            keys: keys.to_vec(),
            at: 0,
            keyless,
        })
    }

    /// One `SafetyNumber` ask of the core: `None` (no key yet) while the script's keyless asks
    /// last, then [`Held::answer`].
    pub(crate) fn ask(&mut self) -> Option<SafetyNumber> {
        if self.keyless > 0 {
            self.keyless -= 1;
            return None;
        }
        Some(self.answer())
    }

    /// The number of the held key; then the fake moves on to the next key, if any.
    pub(crate) fn answer(&mut self) -> SafetyNumber {
        let (key, state) = self.keys[self.at].clone();
        self.at = (self.at + 1).min(self.keys.len() - 1);
        number(key, state)
    }

    /// A Mark (`verified`) or a Clear of `key`: refused (`None`) when the fake holds another
    /// key, so no state is ever applied to a key the page did not show.
    pub(crate) fn set(&mut self, verified: bool, key: &IdentityFingerprint) -> Option<Applied> {
        let held = &mut self.keys[self.at];
        if held.0 != *key {
            return None;
        }
        let was = held.1;
        held.1 = if verified {
            VerifyState::Verified
        } else {
            VerifyState::Default
        };
        Some(Applied {
            was,
            now: held.1,
            key: held.0.clone(),
        })
    }
}

/// What a `ConfirmIdentity` the fake took did (BQ-2b, RC2, RC6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Confirm {
    /// A person who is not in state Changed: approval only, nothing changes.
    Approved,
    /// State Changed and the held key: it becomes Default (as a Clear of that key).
    Applied(Applied),
    /// State Changed and another key than the held one: refused, nothing changes.
    Refused,
}

impl Held {
    /// "Confirm New Safety Number" of `key` (Desktop's "Send anyway"): DEFAULT for the held
    /// key in state Changed, refused for another key there, an approval only otherwise.
    pub(crate) fn confirm(&mut self, key: &IdentityFingerprint) -> Confirm {
        if self.keys[self.at].1 != VerifyState::Changed {
            return Confirm::Approved;
        }
        self.set(false, key)
            .map_or(Confirm::Refused, Confirm::Applied)
    }
}

/// The trust a state shows on the contact.
pub(crate) fn trust_of(state: VerifyState) -> TrustLevel {
    match state {
        VerifyState::Default => TrustLevel::Unverified,
        VerifyState::Verified => TrustLevel::Verified,
        VerifyState::Changed => TrustLevel::Changed,
    }
}

/// The synthetic number of `key`: 60 digits and 74 scannable bytes.
pub(crate) fn number(key: IdentityFingerprint, state: VerifyState) -> SafetyNumber {
    let byte = |i: usize| key.0.get(i % key.0.len().max(1)).copied().unwrap_or(0);
    let digits: String = (0..60)
        .map(|i| char::from(b'0' + u8::try_from((usize::from(byte(i)) + i * 7) % 10).unwrap_or(0)))
        .collect();
    let mut scannable = vec![0x08, 0x02, 0x12, 0x22, 0x0a, 0x20];
    scannable.extend((0..32).map(byte));
    scannable.extend([0x1a, 0x22, 0x0a, 0x20]);
    scannable.extend((0..32).map(|i| byte(i).wrapping_add(0x5a)));
    SafetyNumber {
        digits: Redacted::new(digits),
        scannable: Redacted::new(scannable),
        key,
        state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(b: u8) -> IdentityFingerprint {
        IdentityFingerprint([5].into_iter().chain([b; 32]).collect())
    }

    /// The held key moves on after each answer and the last one stays; a Mark of another key
    /// is refused and changes nothing.
    #[test]
    fn the_held_key_moves_on_and_a_stale_mark_is_refused() {
        let mut h = Held::new(
            &[
                (key(1), VerifyState::Default),
                (key(2), VerifyState::Default),
            ],
            0,
        )
        .expect("keys");
        let first = h.answer();
        assert!(first.key == key(1), "the first answer is the first key");
        assert!(
            h.set(true, &key(1)).is_none(),
            "the fake holds the second key now"
        );
        let second = h.answer();
        assert!(
            second.key == key(2) && h.answer().key == key(2),
            "the last key stays"
        );
        assert!(
            first.digits.expose() != second.digits.expose(),
            "another key, other digits"
        );
        assert_eq!(second.scannable.expose().len(), 74);
        let applied = h.set(true, &key(2)).expect("the held key");
        assert_eq!(
            (applied.was, applied.now),
            (VerifyState::Default, VerifyState::Verified)
        );
        assert_eq!(h.answer().state, VerifyState::Verified);
    }

    /// A person whose key the fake learns late: the first asks are "no key yet" (`None`), then
    /// the keys come as for any other person.
    #[test]
    fn the_first_asks_of_a_late_key_have_no_number() {
        let mut h = Held::new(&[(key(1), VerifyState::Default)], 2).expect("keys");
        assert!(h.ask().is_none() && h.ask().is_none(), "no key yet, twice");
        let n = h.ask().expect("the third ask has the key");
        assert!(n.key == key(1), "the first key");
        assert!(h.ask().is_some(), "and it stays");
        let mut now = Held::new(&[(key(1), VerifyState::Default)], 0).expect("keys");
        assert!(now.ask().is_some(), "no late asks: a number at once");
    }

    /// Confirm: an approval for a person not in Changed; in Changed the held key becomes
    /// Default and another key is refused without a change.
    #[test]
    fn confirm_applies_only_the_held_key_of_a_changed_person() {
        let mut h = Held::new(&[(key(1), VerifyState::Changed)], 0).expect("keys");
        assert_eq!(h.confirm(&key(2)), Confirm::Refused);
        assert_eq!(h.answer().state, VerifyState::Changed, "nothing changed");
        let Confirm::Applied(a) = h.confirm(&key(1)) else {
            panic!("the held key confirms");
        };
        assert_eq!((a.was, a.now), (VerifyState::Changed, VerifyState::Default));
        assert_eq!(
            h.confirm(&key(2)),
            Confirm::Approved,
            "Default: approval only"
        );
    }
}

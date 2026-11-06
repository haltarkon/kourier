//! The rehearsal network of Threema (TR T8): the fake that PLAYS Threema for the run tool's
//! rehearsal (`FakeNetwork::rehearsal_threema()`), and the synthetic constants it speaks with. It
//! is a fake for a debug build with a throwaway profile and no network at all; nothing here is a
//! model of what Threema really sends, and `kourier-fake` holds no Threema code (no type of
//! `kourier-threema` or libthreema): the shapes below are copied from what the real backend
//! states.
//!
//! What it plays, in the order a real run meets it:
//!
//! - The descriptor is Threema's (`FakeNetwork::playing(NetworkKind::Threema, ..)`) WITHOUT the
//!   risk notice (the warning page is the UI's and has tests of its own; no dev command ticks it).
//!   It states no roaming setting (`roaming_writes: None`): Threema writes none yet.
//! - The QR login shows ONE code (the real actor shows one for the whole link) that the phone
//!   scans at once, then the three emoji ([`crate::FAKE_EMOJI`]) for [`Timings::emoji`] (the
//!   phone's confirmation comes by itself when that time is over), then `linking` and the link.
//! - A fresh link brings what the real join's second zone brings: the contact of the direct chat,
//!   its 1:1 chat and a group, all with NO history (Threema pushes none to a new device), while
//!   the account says `Syncing`, then `Online`. A resumed account goes online and replays nothing:
//!   the script cursor is durable.
//! - [`FakeControl::arrive`](crate::FakeControl::arrive) puts one text from the other member into
//!   the first chat of a kind of the RUNNING account, on cue.
//!
//! THE ALLOW-LIST IS NOT HERE (decision M7): the fake never judges a Threema ID, so it links
//! whatever the run profile's list says. The refusal of an ID that is not on the list is the
//! real backend's (`kourier-threema` `join/rules.rs`, B1's gate) and has its own tests; the
//! rehearsal proves the run tool's handling of the list file, not the backend's check of it.
//!
//! The ids have the shapes the real backend gives: users `threema:<8 characters>` and a group
//! `threema:<creator ID>.<16 hex digits>`. Every value is synthetic, and no constant is a
//! substring of another (a test pins it, and that every `pub const .. : &str` of this file is in
//! that test), so a search of the files of a rehearsal for a value finds a leak and nothing
//! else. The IDs are the strings the run tool hashes in its leak search too.

use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_model::{
    CannotSend, ConvKey, ConversationFeatures, MemberRole, NetworkKind, UserId, Window,
};

use crate::network::FakeNetwork;
use crate::script::{Delivery, LoginScript, Script, ScriptError, Step};
use crate::scripts::{contact, conversation, member};

/// The rehearsal account's own profile name.
pub const OWN_NAME: &str = "Rehearsal Thowner";
/// The other person's name in the rehearsal (the run tool searches the files of a rehearsal for it).
pub const PEER_NAME: &str = "Rehearsal Thpeer";
/// The rehearsal group's title.
pub const GROUP_NAME: &str = "Rehearsal Thgroup";
/// The rehearsal account's own Threema ID: 8 characters, synthetic. The run tool's rehearsal types it
/// as the test ID and searches everything it wrote for it and for its digest.
pub const OWN_ID: &str = "REHOWN01";
/// The other person's Threema ID, synthetic (the creator of the group).
pub const PEER_ID: &str = "REHPEER2";
/// The group's id as the 16 hex digits of the key (`threema:<creator>.<digits>`).
pub const GROUP_ID_HEX: &str = "7c1e5a93b04d2f68";
/// The text of an arrival: the one place every fake reads (`rehearsal::ARRIVAL_WORDS`; a test pins
/// that they are the same words).
pub const ARRIVAL_WORDS: &str = "Rehearsal arrival";

/// How long the rehearsal's walk takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timings {
    /// The QR code is shown this long if nobody scans it (the phone scans at once, so it is only
    /// the page's own `valid_for`).
    pub code: Duration,
    /// The three emoji stand this long, and the phone confirms by itself when it is over.
    pub emoji: Duration,
}

impl Timings {
    /// What a rehearsal run uses: a code of three minutes (the real one waits for its scan up to
    /// five) and twenty seconds of emoji, which leaves a driver room to ask and answer the
    /// comparison before the link goes on.
    pub const REAL: Self = Self {
        code: Duration::from_secs(180),
        emoji: Duration::from_secs(20),
    };
}

/// The features the Threema backend states for every chat in TH5b (its `join::events::features`,
/// without the `files` limits the rehearsal never uses): edits and deletes are taken without a
/// window, the send side is off and says `NotYetBuilt` until TH7. `kourier-threema` cannot be
/// compared here (the edge is the other way round), so a change of the backend's own statement is
/// for its twin test to catch; the rehearsal's chats then only differ in what no step of the run
/// tool reads.
pub fn threema_features() -> ConversationFeatures {
    let open = Window {
        max_age_secs: None,
        max_count: None,
    };
    ConversationFeatures {
        edit_receive: Some(open),
        delete_receive: Some(open),
        cannot_send: Some(CannotSend::NotYetBuilt),
        ..ConversationFeatures::default()
    }
}

fn id(value: &str) -> Result<UserId, ScriptError> {
    Ok(UserId::from_parts("threema", value)?)
}

/// The conversation key of the rehearsal's group (`threema:<creator ID>.<16 hex digits>`).
fn group_key() -> ConvKey {
    ConvKey::Group(format!("threema:{PEER_ID}.{GROUP_ID_HEX}").into())
}

/// The rehearsal account's script (see the module docs). The same script plays a fresh start and
/// a resumed one.
///
/// # Errors
/// Never in practice: the ids are constants of a valid shape.
pub fn script(timings: Timings) -> Result<Script, ScriptError> {
    let own = id(OWN_ID)?;
    let peer = id(PEER_ID)?;
    let direct = ConvKey::Direct(peer.clone());
    let group = group_key();
    let features = threema_features();

    // One delivery per chat, like zone 2 of a join: the contact, then its chat, no message.
    let direct_chat = Step::Deliver(Delivery::new(vec![
        contact(&peer, Some(PEER_NAME), None),
        BackendEvent::ConversationUpsert(conversation(&direct, None, None, &features)),
    ]));
    let group_chat = Step::Deliver(Delivery::new(vec![BackendEvent::ConversationUpsert(
        conversation(
            &group,
            Some(GROUP_NAME),
            Some(vec![
                member(&peer, MemberRole::Admin),
                member(&own, MemberRole::Member),
            ]),
            &features,
        ),
    )]));

    let mut s = Script::new("rehearsal-threema", vec![own]).with_emoji_for(timings.emoji);
    // It plays Threema as the real run sees it: no roaming setting is written yet.
    s.roaming_writes = None;
    s.display_name = Some(OWN_NAME.to_owned());
    s.login = LoginScript::Qr {
        ask_device_name: false,
        codes: 1,
        valid_for: timings.code,
        scanned_at: Some(0),
        linked_as: None,
    };
    s.steps = vec![
        Step::Conn(kourier_model::ConnState::Syncing { fraction: None }),
        direct_chat,
        group_chat,
        Step::CatchUpDone,
    ];
    Ok(s)
}

impl FakeNetwork {
    /// The fake that rehearses a real Threema run (TR T8, [`crate::threema`]): Threema as the UI
    /// sees it but WITHOUT the risk notice, a QR login of one code that the phone scans at once
    /// followed by the three emoji, a direct chat and a group served on link with no history, and
    /// [`FakeControl::arrive`](crate::FakeControl::arrive) for one message on cue. It judges no
    /// Threema ID. Every id and name is synthetic.
    ///
    /// # Panics
    /// Never: the script's ids are constants of a valid shape.
    pub fn rehearsal_threema() -> Self {
        Self::rehearsal_threema_with(Timings::REAL)
    }

    /// [`rehearsal_threema`](Self::rehearsal_threema) with other waits (tests use milliseconds).
    ///
    /// # Panics
    /// Never: the script's ids are constants of a valid shape.
    pub fn rehearsal_threema_with(timings: Timings) -> Self {
        let script = script(timings).expect("the rehearsal's ids are valid");
        Self::playing(NetworkKind::Threema, script).without_risk_notice()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kourier_backend_api::Network as _;

    /// Every `&str` constant of the module (the order is the file's).
    const CONSTANTS: [(&str, &str); 7] = [
        ("OWN_NAME", OWN_NAME),
        ("PEER_NAME", PEER_NAME),
        ("GROUP_NAME", GROUP_NAME),
        ("OWN_ID", OWN_ID),
        ("PEER_ID", PEER_ID),
        ("GROUP_ID_HEX", GROUP_ID_HEX),
        ("ARRIVAL_WORDS", ARRIVAL_WORDS),
    ];

    #[test]
    fn no_constant_is_a_substring_of_another_and_the_ids_have_the_real_shapes() {
        for (i, (name, value)) in CONSTANTS.iter().enumerate() {
            assert!(!value.is_empty(), "{name}");
            for (j, (other_name, other)) in CONSTANTS.iter().enumerate() {
                if i != j {
                    assert!(!other.contains(value), "{name} is inside {other_name}");
                }
            }
        }
        let mut values: Vec<&str> = CONSTANTS.iter().map(|(_, v)| *v).collect();
        values.sort_unstable();
        values.dedup();
        assert_eq!(values.len(), CONSTANTS.len());

        // A Threema ID is 8 characters of upper-case letters and digits; the two differ.
        for threema in [OWN_ID, PEER_ID] {
            assert_eq!(threema.len(), 8, "{threema}");
            assert!(
                threema
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()),
                "{threema}"
            );
            let user = id(threema).expect("id");
            assert_eq!((user.scheme(), user.value()), ("threema", threema));
        }
        // The group's digits are 16 lower-case hex digits, and its key survives the round trip.
        assert_eq!(GROUP_ID_HEX.len(), 16);
        assert!(
            GROUP_ID_HEX
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        let key = group_key();
        assert_eq!(key.validate(), Ok(()));
        assert_eq!(ConvKey::parse(&key.encode()).expect("a key"), key);
        assert_eq!(
            ARRIVAL_WORDS,
            crate::rehearsal::ARRIVAL_WORDS,
            "the arrival's text comes from the one place every fake reads"
        );
    }

    /// A constant added to the module but not to [`CONSTANTS`] would escape the substring test.
    #[test]
    fn every_string_constant_of_the_module_is_in_the_substring_test() {
        let source = include_str!("threema.rs");
        let named: Vec<&str> = source
            .lines()
            .filter_map(|l| l.strip_prefix("pub const "))
            .filter(|l| l.contains(": &str ="))
            .filter_map(|l| l.split(':').next())
            .collect();
        let listed: Vec<&str> = CONSTANTS.iter().map(|(n, _)| *n).collect();
        assert_eq!(named, listed, "the file's order");
    }

    #[test]
    fn the_real_timings_are_a_long_code_and_twenty_seconds_of_emoji() {
        assert_eq!(Timings::REAL.code, Duration::from_secs(180));
        assert_eq!(Timings::REAL.emoji, Duration::from_secs(20));
        let s = script(Timings::REAL).expect("script");
        assert_eq!(s.emoji_for, Duration::from_secs(20));
        assert_eq!(s.own_ids, vec![id(OWN_ID).expect("id")]);
        assert_eq!(s.roaming_writes, None, "Threema writes no setting yet");
        assert!(!s.honour_number_tail, "Threema has no number tail");
        match s.login {
            LoginScript::Qr {
                ask_device_name,
                codes,
                valid_for,
                scanned_at,
                ..
            } => assert_eq!(
                (ask_device_name, codes, valid_for, scanned_at),
                (false, 1, Duration::from_secs(180), Some(0))
            ),
            other => panic!("not a QR login: {other:?}"),
        }
        // Syncing, the two chats, the catch-up boundary: no history rows anywhere.
        assert_eq!(s.steps.len(), 4);
        for step in &s.steps {
            if let Step::Deliver(delivery) = step {
                assert!(
                    delivery
                        .events
                        .iter()
                        .all(|e| !matches!(e, BackendEvent::HistoryBatch { .. })),
                    "Threema pushes no history"
                );
            }
        }
    }

    #[test]
    fn the_chats_state_what_the_backend_states_and_the_descriptor_is_threemas_without_its_warning()
    {
        let features = threema_features();
        assert_eq!(features.cannot_send, Some(CannotSend::NotYetBuilt));
        assert!(features.edit_receive.is_some() && features.delete_receive.is_some());
        assert!(features.edit.is_none() && features.delete_for_everyone.is_none());

        let net = FakeNetwork::rehearsal_threema_with(Timings {
            code: Duration::from_millis(10),
            emoji: Duration::from_millis(10),
        });
        assert_eq!(net.kind(), NetworkKind::Threema);
        let descriptor = net.descriptor();
        assert_eq!(descriptor.display_name, "Threema");
        assert!(descriptor.experimental);
        assert!(
            descriptor.risk_notice.is_none(),
            "the rehearsal asks nothing"
        );
        // The plain playing fake is Threema with its warning: the rehearsal removed exactly that.
        let playing =
            FakeNetwork::playing(NetworkKind::Threema, script(Timings::REAL).expect("script"));
        assert!(playing.descriptor().risk_notice.is_some());
    }
}

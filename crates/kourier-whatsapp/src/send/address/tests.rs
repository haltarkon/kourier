//! The address of a send (W5a, chunk 3). Synthetic ids only: numbers of the fictitious 555-01xx range with a tail
//! of 8 digits that is a substring of no other value here.

use std::collections::HashMap;

use kourier_model::{ConvKey, MsgTarget, TargetAuthor, UserId};
use wacore_binary::Jid;

use super::{Addressing, KeyFor, Mapping, Namespace, Unaddressable, in_namespace, jid_of};
use crate::map::{OwnIds, conv_of};

const PEER_PN: &str = "15550147310593";
const PEER_LID: &str = "77700182640371";
const AUTHOR_PN: &str = "15550129047586";
const AUTHOR_LID: &str = "77700135928460";
const OWN_PN: &str = "15550163581027";
const OWN_LID: &str = "77700174203958";
const GROUP: &str = "120363000091736248";
const OLD_GROUP: &str = "15550158264913-1600038475";

/// The store's pairs, both ways.
#[derive(Default)]
struct Pairs(HashMap<String, String>, HashMap<String, String>);

impl Pairs {
    fn with(pairs: &[(&str, &str)]) -> Self {
        let mut map = Self::default();
        for (pn, lid) in pairs {
            map.0.insert((*pn).into(), (*lid).into());
            map.1.insert((*lid).into(), (*pn).into());
        }
        map
    }
}

impl Mapping for Pairs {
    fn lid_of(&self, pn: &str) -> Option<String> {
        self.0.get(pn).cloned()
    }
    fn pn_of(&self, lid: &str) -> Option<String> {
        self.1.get(lid).cloned()
    }
}

fn user(scheme: &str, value: &str) -> UserId {
    UserId::from_parts(scheme, value).expect("a valid synthetic user")
}

fn own() -> OwnIds {
    OwnIds {
        lid: Some(user("lid", OWN_LID)),
        pn: Some(user("pn", OWN_PN)),
    }
}

fn target(conv: &ConvKey, author: TargetAuthor) -> MsgTarget {
    MsgTarget {
        conv: Some(conv.clone()),
        author,
        local: "3EB0C4F1A9D2E7B6058314".into(),
        alternates: Vec::new(),
    }
}

#[test]
fn a_key_and_its_jid_go_both_ways() {
    let keys = [
        ConvKey::Direct(user("pn", PEER_PN)),
        ConvKey::Direct(user("lid", PEER_LID)),
        ConvKey::Group(format!("wag:{GROUP}").into()),
        ConvKey::Group(format!("wag:{OLD_GROUP}").into()),
    ];
    let jids = [
        format!("{PEER_PN}@s.whatsapp.net"),
        format!("{PEER_LID}@lid"),
        format!("{GROUP}@g.us"),
        format!("{OLD_GROUP}@g.us"),
    ];
    for (key, text) in keys.iter().zip(&jids) {
        let jid = jid_of(key).expect("a WhatsApp chat has an address");
        assert_eq!(&jid.to_string(), text, "a key is written on its own server");
        assert_eq!(
            conv_of(&jid).as_ref(),
            Some(key),
            "the receive map reads the chat back as the key"
        );
        let parsed: Jid = text.parse().expect("a synthetic chat id parses");
        assert_eq!(
            jid_of(&conv_of(&parsed).expect("a chat")).ok(),
            Some(parsed),
            "and the other way"
        );
    }
    // A hosted device's chat is the ordinary person, and goes back on the ordinary server.
    let hosted: Jid = format!("{PEER_PN}@hosted")
        .parse()
        .expect("a hosted id parses");
    assert_eq!(
        jid_of(&conv_of(&hosted).expect("a person")).map(|j| j.to_string()),
        Ok(format!("{PEER_PN}@s.whatsapp.net"))
    );
    let refused = [
        (
            ConvKey::Direct(user("aci", "0f6c1e9a-41b2-4c55-9d7e-3a8b2c5d1e40")),
            Unaddressable::NotAChat,
        ),
        (
            ConvKey::Direct(user("pn", "+15550147310593")),
            Unaddressable::Malformed,
        ),
        (
            ConvKey::Direct(user("lid", "77700182640371@lid")),
            Unaddressable::Malformed,
        ),
        (
            ConvKey::Group("gv2:c3ludGhldGljIGdyb3Vw".into()),
            Unaddressable::NotAChat,
        ),
        (
            ConvKey::Group("wag:120363000091736248@g.us".into()),
            Unaddressable::Malformed,
        ),
        (ConvKey::Group("wag:1-2-3".into()), Unaddressable::Malformed),
        (
            ConvKey::Broadcast("wab:15550191837265".into()),
            Unaddressable::NotAChat,
        ),
        (
            ConvKey::Channel("wan:120363000091736248".into()),
            Unaddressable::NotAChat,
        ),
    ];
    for (key, why) in refused {
        assert_eq!(
            jid_of(&key),
            Err(why),
            "a key that is no WhatsApp chat of ours is refused"
        );
    }
}

#[test]
fn a_direct_chat_takes_the_accounts_namespace_when_the_mapping_is_known() {
    let known = Pairs::with(&[(PEER_PN, PEER_LID)]);
    let unknown = Pairs::default();
    let own = own();
    let pn_chat = ConvKey::Direct(user("pn", PEER_PN));
    let lid_chat = ConvKey::Direct(user("lid", PEER_LID));
    let pn = format!("{PEER_PN}@s.whatsapp.net");
    let lid = format!("{PEER_LID}@lid");
    // (migrated, mapping known, chat) -> the chat on the wire: upstream's `resolve_dm_wire_jid`.
    let cases: [(bool, &dyn Mapping, &ConvKey, &str); 8] = [
        (true, &known, &pn_chat, &lid),
        (true, &unknown, &pn_chat, &pn),
        (true, &known, &lid_chat, &lid),
        (true, &unknown, &lid_chat, &lid),
        (false, &known, &lid_chat, &pn),
        (false, &unknown, &lid_chat, &lid),
        (false, &known, &pn_chat, &pn),
        (false, &unknown, &pn_chat, &pn),
    ];
    for (migrated, map, chat, wire) in cases {
        let address = Addressing {
            migrated,
            map,
            own: &own,
        };
        assert_eq!(
            address.chat(chat).map(|j| j.to_string()).as_deref(),
            Ok(wire),
            "a 1:1 chat is written in the account's namespace where the other form is known (migrated: {migrated})"
        );
    }
    // A group is written as it is, whatever the account.
    let group = ConvKey::Group(format!("wag:{GROUP}").into());
    let address = Addressing {
        migrated: true,
        map: &known,
        own: &own,
    };
    assert_eq!(
        address.chat(&group).map(|j| j.to_string()),
        Ok(format!("{GROUP}@g.us"))
    );
    // A device part is never kept on the wire form of a person.
    let device: Jid = format!("{PEER_PN}:3@s.whatsapp.net")
        .parse()
        .expect("a device id parses");
    assert_eq!(
        in_namespace(&device, Namespace::Lid, &known).to_string(),
        lid
    );
}

#[test]
fn own_ids_are_written_in_the_namespace_asked_and_else_in_the_other() {
    let map = Pairs::default();
    let both = own();
    let address = Addressing {
        migrated: false,
        map: &map,
        own: &both,
    };
    assert_eq!(
        address.own_in(Namespace::Lid).map(|j| j.to_string()),
        Ok(format!("{OWN_LID}@lid"))
    );
    assert_eq!(
        address.own_in(Namespace::Pn).map(|j| j.to_string()),
        Ok(format!("{OWN_PN}@s.whatsapp.net"))
    );
    // Our own number asked as a person in LID gives our LID, without the mapping.
    assert_eq!(
        address
            .person(&user("pn", OWN_PN), Namespace::Lid)
            .map(|j| j.to_string()),
        Ok(format!("{OWN_LID}@lid"))
    );
    let lid_only = OwnIds {
        lid: Some(user("lid", OWN_LID)),
        pn: None,
    };
    let address = Addressing {
        migrated: false,
        map: &map,
        own: &lid_only,
    };
    assert_eq!(
        address.own_in(Namespace::Pn).map(|j| j.to_string()),
        Ok(format!("{OWN_LID}@lid"))
    );
    let none = OwnIds::default();
    let address = Addressing {
        migrated: false,
        map: &map,
        own: &none,
    };
    assert_eq!(address.own_in(Namespace::Lid), Err(Unaddressable::NoOwnId));
}

/// (conv, author, group mode) -> (remoteJid, fromMe, participant).
type KeyCase<'a> = (
    &'a ConvKey,
    TargetAuthor,
    Option<Namespace>,
    &'a str,
    bool,
    Option<&'a str>,
);

#[test]
fn a_target_key_says_who_wrote_it() {
    let map = Pairs::with(&[(PEER_PN, PEER_LID), (AUTHOR_PN, AUTHOR_LID)]);
    let own = own();
    let address = Addressing {
        migrated: true,
        map: &map,
        own: &own,
    };
    let dm = ConvKey::Direct(user("pn", PEER_PN));
    let group = ConvKey::Group(format!("wag:{GROUP}").into());
    let peer_lid = format!("{PEER_LID}@lid");
    let group_jid = format!("{GROUP}@g.us");
    let author_lid = format!("{AUTHOR_LID}@lid");
    let author_pn = format!("{AUTHOR_PN}@s.whatsapp.net");
    // (conv, author, group mode) -> (remoteJid, fromMe, participant).
    let cases: Vec<KeyCase<'_>> = vec![
        // 1:1, our message: fromMe, the chat in the account's namespace (D20: LID on a migrated account).
        (&dm, TargetAuthor::Own, None, &peer_lid, true, None),
        // 1:1, our own id named as the author is ours too.
        (
            &dm,
            TargetAuthor::User(user("lid", OWN_LID)),
            None,
            &peer_lid,
            true,
            None,
        ),
        // 1:1, the peer's message.
        (
            &dm,
            TargetAuthor::User(user("lid", PEER_LID)),
            None,
            &peer_lid,
            false,
            None,
        ),
        // Group, our message: fromMe and NO participant (whatsmeow `BuildMessageKey`, upstream's sender revoke).
        (
            &group,
            TargetAuthor::Own,
            Some(Namespace::Lid),
            &group_jid,
            true,
            None,
        ),
        // Group, another's: the author in the group's addressing mode.
        (
            &group,
            TargetAuthor::User(user("pn", AUTHOR_PN)),
            Some(Namespace::Lid),
            &group_jid,
            false,
            Some(&author_lid),
        ),
        (
            &group,
            TargetAuthor::User(user("lid", AUTHOR_LID)),
            Some(Namespace::Pn),
            &group_jid,
            false,
            Some(&author_pn),
        ),
        // The mode not known: the author as the core holds him when that is a LID (a number is refused, fix F20:
        // `a_key_never_names_an_author_by_number_in_a_group_that_hides_numbers`).
        (
            &group,
            TargetAuthor::User(user("lid", AUTHOR_LID)),
            None,
            &group_jid,
            false,
            Some(&author_lid),
        ),
    ];
    for (conv, author, mode, remote, from_me, participant) in cases {
        let key = address
            .key(conv, &target(conv, author), mode, KeyFor::Revoke)
            .expect("a target of this chat has a key");
        assert_eq!(
            key.remote_jid.as_deref(),
            Some(remote),
            "the key names the chat"
        );
        assert_eq!(
            key.from_me,
            Some(from_me),
            "fromMe says whether the target is ours"
        );
        assert_eq!(
            key.participant.as_deref(),
            participant,
            "a group key names another's message by its author"
        );
        assert_eq!(
            key.id.as_deref(),
            Some("3EB0C4F1A9D2E7B6058314"),
            "the key's id is the target's"
        );
    }
    // Refused: an unknown author (a group key needs its participant), a target stated in another chat.
    assert_eq!(
        address.key(
            &group,
            &target(&group, TargetAuthor::Unknown),
            Some(Namespace::Lid),
            KeyFor::Revoke
        ),
        Err(Unaddressable::UnknownAuthor)
    );
    assert_eq!(
        address.key(
            &dm,
            &target(&dm, TargetAuthor::Unknown),
            None,
            KeyFor::Revoke
        ),
        Err(Unaddressable::UnknownAuthor)
    );
    assert_eq!(
        address.key(
            &dm,
            &target(&group, TargetAuthor::Own),
            None,
            KeyFor::Revoke
        ),
        Err(Unaddressable::OtherConversation)
    );
    // A target without a stated conversation is the request's.
    let mut loose = target(&dm, TargetAuthor::Own);
    loose.conv = None;
    assert!(address.key(&dm, &loose, None, KeyFor::Revoke).is_ok());
}

/// Fix F10 (interop MEDIUM-2): in a GROUP the key of an edit and of a reaction to OUR message names us as the
/// `participant`, in the group's addressing mode (WhatsApp Web, upstream's `message_ref_addon_key` and
/// `edit_message_inner`); the sender revoke's key keeps none (an admin revoke is the one with `fromMe = false`).
#[test]
fn the_key_of_an_edit_or_a_reaction_to_our_own_message_in_a_group_names_us_and_a_revoke_does_not() {
    let map = Pairs::with(&[(PEER_PN, PEER_LID), (AUTHOR_PN, AUTHOR_LID)]);
    let own = own();
    let address = Addressing {
        migrated: true,
        map: &map,
        own: &own,
    };
    let group = ConvKey::Group(format!("wag:{GROUP}").into());
    let dm = ConvKey::Direct(user("pn", PEER_PN));
    let (own_lid, own_pn) = (format!("{OWN_LID}@lid"), format!("{OWN_PN}@s.whatsapp.net"));
    let ours = |author| target(&group, author);
    for (used_for, mode, want) in [
        (KeyFor::Edit, Some(Namespace::Lid), Some(own_lid.as_str())),
        (KeyFor::Edit, Some(Namespace::Pn), Some(own_pn.as_str())),
        (KeyFor::Edit, None, Some(own_lid.as_str())),
        (
            KeyFor::Reaction,
            Some(Namespace::Lid),
            Some(own_lid.as_str()),
        ),
        (KeyFor::Reaction, Some(Namespace::Pn), Some(own_pn.as_str())),
        (KeyFor::Revoke, Some(Namespace::Lid), None),
        (KeyFor::Revoke, Some(Namespace::Pn), None),
    ] {
        // Our own message by the core's `Own` and by one of our ids: the same key.
        for author in [TargetAuthor::Own, TargetAuthor::User(user("pn", OWN_PN))] {
            let key = address
                .key(&group, &ours(author), mode, used_for)
                .expect("our message has a key");
            assert_eq!(key.from_me, Some(true), "{used_for:?} {mode:?}: fromMe");
            assert_eq!(
                key.participant.as_deref(),
                want,
                "{used_for:?} {mode:?}: the participant of our own message"
            );
        }
        // Another's message: the author, whatever the key is for. A 1:1 chat: never a participant.
        let theirs = address
            .key(
                &group,
                &ours(TargetAuthor::User(user("lid", AUTHOR_LID))),
                Some(Namespace::Lid),
                used_for,
            )
            .expect("a member's message has a key");
        assert_eq!(
            theirs.participant.as_deref(),
            Some(format!("{AUTHOR_LID}@lid").as_str()),
            "{used_for:?}: another's message names its author"
        );
        let chat = address
            .key(&dm, &target(&dm, TargetAuthor::Own), None, used_for)
            .expect("a 1:1 key");
        assert_eq!(
            chat.participant, None,
            "{used_for:?}: no participant in a 1:1 chat"
        );
    }
}

/// Fix F20 (re-check N2, privacy): the `participant` of a KEY that names ANOTHER's message is read by every member of
/// the group, like a mention. In a group whose mode is `Lid`, and in one whose mode is unknown, a person the core holds
/// by NUMBER is never written as the number: the key is refused, `NumberInLidGroup` for a LID group (for good, until the
/// mapping learns the LID) and `GroupModeUnknown` for an unknown mode (the mode is asked again), whatever the key is for.
/// A person with a known LID is written as the LID in a LID group; a group addressed by numbers names numbers; a LID is
/// no number; a 1:1 chat is not affected. Mutants: the key writes the number again (`person_in` instead of the
/// public rule); the unknown mode reported as a LID group.
#[test]
fn a_key_never_names_an_author_by_number_in_a_group_that_hides_numbers() {
    let unmapped = Pairs::default();
    let mapped = Pairs::with(&[(AUTHOR_PN, AUTHOR_LID)]);
    let own = own();
    let group = ConvKey::Group(format!("wag:{GROUP}").into());
    let dm = ConvKey::Direct(user("pn", PEER_PN));
    let by_number = TargetAuthor::User(user("pn", AUTHOR_PN));
    let by_lid = TargetAuthor::User(user("lid", AUTHOR_LID));
    let lid_text = format!("{AUTHOR_LID}@lid");
    let pn_text = format!("{AUTHOR_PN}@s.whatsapp.net");
    let key_of = |map: &dyn Mapping,
                  conv: &ConvKey,
                  author: &TargetAuthor,
                  mode: Option<Namespace>,
                  used_for: KeyFor| {
        let address = Addressing {
            migrated: true,
            map,
            own: &own,
        };
        address.key(conv, &target(conv, author.clone()), mode, used_for)
    };
    for used_for in [KeyFor::Edit, KeyFor::Reaction, KeyFor::Revoke] {
        // A number without a known LID: refused in a LID group and in a group of unknown mode, never written.
        assert_eq!(
            key_of(
                &unmapped,
                &group,
                &by_number,
                Some(Namespace::Lid),
                used_for
            ),
            Err(Unaddressable::NumberInLidGroup),
            "{used_for:?}: a LID group would read the number"
        );
        assert_eq!(
            key_of(&unmapped, &group, &by_number, None, used_for),
            Err(Unaddressable::GroupModeUnknown),
            "{used_for:?}: a group of unknown mode may hide numbers"
        );
        // Unknown mode: every number is withheld, mapped or not (F7's rule); a LID is written as it is.
        assert_eq!(
            key_of(&mapped, &group, &by_number, None, used_for),
            Err(Unaddressable::GroupModeUnknown),
            "{used_for:?}: a mapped number is a number while the mode is unknown"
        );
        assert_eq!(
            key_of(&mapped, &group, &by_lid, None, used_for)
                .expect("a LID is no number")
                .participant
                .as_deref(),
            Some(lid_text.as_str()),
            "{used_for:?}: a LID author in a group of unknown mode"
        );
        // A number whose LID is known is written as the LID in a LID group.
        assert_eq!(
            key_of(&mapped, &group, &by_number, Some(Namespace::Lid), used_for)
                .expect("a mapped number has a LID")
                .participant
                .as_deref(),
            Some(lid_text.as_str()),
            "{used_for:?}: a mapped author in a LID group"
        );
        // A group addressed by numbers names numbers.
        assert_eq!(
            key_of(&unmapped, &group, &by_number, Some(Namespace::Pn), used_for)
                .expect("a number group names numbers")
                .participant
                .as_deref(),
            Some(pn_text.as_str()),
            "{used_for:?}: a group addressed by numbers"
        );
        // A 1:1 chat has no participant, whatever the mode a caller passes.
        assert_eq!(
            key_of(&unmapped, &dm, &by_number, None, used_for)
                .expect("a 1:1 key")
                .participant,
            None,
            "{used_for:?}: no participant in a 1:1 chat"
        );
    }
}

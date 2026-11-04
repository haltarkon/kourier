//! The key rule of a history row's message secrets (W4, C13): the same (chat, sender, id) upstream's own
//! seeding writes. Every id, number and secret is synthetic.

use wacore::msg_secret::MsgSecretPolicy;
use waproto::buffa::MessageField;

use super::{Jid, entries};
use crate::history::testing::Msg;
use crate::session::MSG_SECRET_POLICY;

const PEER: &str = "15550100101@s.whatsapp.net";
const OWN_PN: &str = "15550100999@s.whatsapp.net";
const OWN_LID: &str = "99900100999@lid";
const GROUP: &str = "120363000000000001@g.us";
const MEMBER: &str = "15550100104@s.whatsapp.net";
const MEMBER_LID: &str = "99900100104@lid";
const SECRET: [u8; 32] = [0x5A; 32];

fn jid(text: &str) -> Jid {
    text.parse().expect("a jid")
}

/// (chat, sender, id) of every entry the rule gives for `message` in `chat`.
fn keys(chat: &str, message: &Msg) -> Vec<(String, String, String)> {
    let (pn, lid) = (jid(OWN_PN), jid(OWN_LID));
    entries(chat, &message.info, Some(&pn), Some(&lid))
        .into_iter()
        .map(|e| {
            (
                e.chat.to_string(),
                e.sender.to_string(),
                e.msg_id.to_string(),
            )
        })
        .collect()
}

fn forwarded(mut message: Msg) -> Msg {
    let mut context = waproto::whatsapp::ContextInfo::default();
    context.is_forwarded = Some(true);
    let mut text = waproto::whatsapp::message::ExtendedTextMessage::default();
    text.text = Some("fwd".into());
    text.context_info = MessageField::some(context);
    let mut body = waproto::whatsapp::Message::default();
    body.extended_text_message = MessageField::some(text);
    message.info.message = MessageField::some(body);
    message
}

#[test]
fn a_direct_chat_keys_the_secret_by_the_chat_for_both_sides_of_it() {
    let theirs = Msg::text(PEER, "M1", 1_700_000_100, "hi").secret(&SECRET);
    assert_eq!(
        keys(PEER, &theirs),
        [(PEER.to_owned(), PEER.to_owned(), "M1".to_owned())],
        "the sender of a message in a one-to-one chat is the chat itself"
    );

    let mine = Msg::mine(
        PEER,
        "M2",
        1_700_000_101,
        "hello",
        crate::history::testing::Status::READ,
    )
    .secret(&SECRET);
    assert_eq!(
        keys(PEER, &mine),
        [
            (PEER.to_owned(), OWN_LID.to_owned(), "M2".to_owned()),
            (PEER.to_owned(), OWN_PN.to_owned(), "M2".to_owned()),
        ],
        "a message of ours is keyed by both of our ids, the LID first"
    );
}

#[test]
fn a_group_message_is_keyed_by_the_participant_of_the_key_else_of_the_message() {
    let on_key = Msg::text(GROUP, "G1", 1_700_000_100, "x")
        .from(MEMBER)
        .secret(&SECRET);
    assert_eq!(
        keys(GROUP, &on_key),
        [(GROUP.to_owned(), MEMBER.to_owned(), "G1".to_owned())]
    );

    // The participant only on the message, none on the key.
    let mut only_on_message = Msg::text(GROUP, "G2", 1_700_000_101, "y").secret(&SECRET);
    only_on_message.info.participant = Some(MEMBER_LID.to_owned());
    assert_eq!(
        keys(GROUP, &only_on_message),
        [(GROUP.to_owned(), MEMBER_LID.to_owned(), "G2".to_owned())]
    );

    // Both: the key's wins (upstream's order).
    let mut both = Msg::text(GROUP, "G3", 1_700_000_102, "z")
        .from(MEMBER)
        .secret(&SECRET);
    both.info.participant = Some(MEMBER_LID.to_owned());
    assert_eq!(
        keys(GROUP, &both),
        [(GROUP.to_owned(), MEMBER.to_owned(), "G3".to_owned())]
    );

    // A group message nobody sent: no key can be named.
    let nobody = Msg::text(GROUP, "G4", 1_700_000_103, "w").secret(&SECRET);
    assert!(keys(GROUP, &nobody).is_empty());
}

#[test]
fn what_upstream_leaves_out_is_left_out() {
    // Not a secret of 32 bytes.
    let short = Msg::text(PEER, "S1", 1_700_000_100, "a").secret(&[1; 16]);
    assert!(keys(PEER, &short).is_empty());
    // No secret at all.
    let none = Msg::text(PEER, "S2", 1_700_000_100, "b");
    assert!(keys(PEER, &none).is_empty());
    // A forwarded message: the secret is the original's.
    let fwd = forwarded(Msg::text(PEER, "S3", 1_700_000_100, "c").secret(&SECRET));
    assert!(keys(PEER, &fwd).is_empty());
    // A chat id that is no jid.
    let odd = Msg::text(PEER, "S4", 1_700_000_100, "d").secret(&SECRET);
    assert!(keys("not a jid", &odd).is_empty());
    // An empty message id.
    let nameless = Msg::text(PEER, "", 1_700_000_100, "e").secret(&SECRET);
    assert!(keys(PEER, &nameless).is_empty());
}

#[test]
fn the_secret_may_sit_in_the_messages_own_context_info() {
    let mut message = Msg::text(PEER, "C1", 1_700_000_100, "ctx");
    let mut context = waproto::whatsapp::MessageContextInfo::default();
    context.message_secret = Some(SECRET.to_vec());
    if let Some(body) = message.info.message.as_option_mut() {
        body.message_context_info = MessageField::some(context);
    }
    let found = entries(PEER, &message.info, None, None);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].secret, SECRET);
    assert_eq!(found[0].expires_at, 0, "kept as long as the message");
    assert_eq!(found[0].message_ts, 1_700_000_100);
}

#[test]
fn the_retention_is_never_because_the_client_keeps_every_secret() {
    // `entries` writes `expires_at = 0`: that is only right while the client's policy bounds nothing.
    assert_eq!(MSG_SECRET_POLICY, MsgSecretPolicy::Full);
    assert!(!MSG_SECRET_POLICY.bounds_retention());
}

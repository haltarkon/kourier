//! The blob builder against upstream's own reader: what `HistorySyncStream` yields is what was put in.

use wacore::history_sync::{HistorySyncStream, MAX_DECOMPRESSED};
use waproto::whatsapp as wa;

use super::{Chat, Chunk, Msg, Status, SyncType};

const GROUP: &str = "120000000000000001@g.us";
const FRIEND_LID: &str = "333333333333333@lid";
const FRIEND_PN: &str = "15550000002@s.whatsapp.net";
const MEMBER_LID: &str = "444444444444444@lid";
const KEY: [u8; 32] = [0x2a; 32];

/// A chunk with every part a later test reaches for: a 1:1 chat known by both of its ids, a group, our
/// own message with a status, a timer and a secret, a reaction, a push name and a pair of ids.
fn chunk() -> Chunk {
    let one_to_one = Chat::new(FRIEND_PN)
        .lid(FRIEND_LID)
        .pn(FRIEND_PN)
        .unread(2)
        .timer(86_400)
        .active_at(1_700_000_300)
        .message(
            Msg::text(FRIEND_LID, "A1", 1_700_000_100, "hello").reaction(
                None,
                "+",
                1_700_000_150_000,
            ),
        )
        .message(
            Msg::mine(FRIEND_LID, "A2", 1_700_000_200, "hi back", Status::READ)
                .timer(86_400, Some(1_700_000_201))
                .secret(&KEY),
        );
    let group = Chat::new(GROUP)
        .name("A group")
        .locked()
        .end_of_history()
        .message(Msg::text(GROUP, "B1", 1_700_000_050, "in the group").from(MEMBER_LID));
    Chunk::new(SyncType::INITIAL_BOOTSTRAP, 2, 40)
        .chat(one_to_one)
        .chat(group)
        .push_name(MEMBER_LID, "A Member")
        .pair(FRIEND_PN, FRIEND_LID)
}

#[test]
fn a_blob_round_trips_through_the_stream() {
    let blob = chunk().blob();
    let mut stream = HistorySyncStream::new(&blob, MAX_DECOMPRESSED);

    // The chats come one at a time, in the order they were put in, with every field.
    let first = stream.next_conversation().expect("read").expect("a chat");
    assert_eq!(first.id, FRIEND_PN);
    assert_eq!(first.lid_jid.as_deref(), Some(FRIEND_LID));
    assert_eq!(first.pn_jid.as_deref(), Some(FRIEND_PN));
    assert_eq!(first.unread_count, Some(2));
    assert_eq!(first.ephemeral_expiration, Some(86_400));
    assert_eq!(first.conversation_timestamp, Some(1_700_000_300));
    assert_eq!(first.messages.len(), 2);
    let theirs = first.messages[0].message.as_option().expect("a message");
    assert_eq!(theirs.key.id.as_deref(), Some("A1"));
    assert_eq!(theirs.key.from_me, Some(false));
    assert_eq!(theirs.message_timestamp, Some(1_700_000_100));
    assert_eq!(
        theirs.message.conversation.as_deref(),
        Some("hello"),
        "the text"
    );
    assert_eq!(theirs.reactions.len(), 1);
    assert_eq!(theirs.reactions[0].text.as_deref(), Some("+"));
    assert_eq!(theirs.reactions[0].key.from_me, Some(true), "ours");
    let ours = first.messages[1].message.as_option().expect("a message");
    assert_eq!(ours.key.from_me, Some(true));
    assert_eq!(ours.status, Some(wa::web_message_info::Status::READ));
    assert_eq!(ours.ephemeral_duration, Some(86_400));
    assert_eq!(ours.ephemeral_start_timestamp, Some(1_700_000_201));
    assert_eq!(ours.message_secret.as_deref(), Some(&KEY[..]));

    let second = stream.next_conversation().expect("read").expect("a chat");
    assert_eq!(second.id, GROUP);
    assert_eq!(second.name.as_deref(), Some("A group"));
    assert_eq!(second.locked, Some(true));
    assert_eq!(second.end_of_history_transfer, Some(true));
    let member = second.messages[0].message.as_option().expect("a message");
    assert_eq!(member.key.participant.as_deref(), Some(MEMBER_LID));
    assert_eq!(member.participant.as_deref(), Some(MEMBER_LID));

    assert!(stream.next_conversation().expect("read").is_none());

    // The rest of the chunk comes last: sync type, order, progress, names, pairs.
    let rest = stream.remainder().expect("the remainder");
    assert_eq!(
        rest.sync_type,
        wa::history_sync::HistorySyncType::INITIAL_BOOTSTRAP
    );
    assert_eq!(rest.chunk_order, Some(2));
    assert_eq!(rest.progress, Some(40));
    assert_eq!(rest.pushnames.len(), 1);
    assert_eq!(rest.pushnames[0].id.as_deref(), Some(MEMBER_LID));
    assert_eq!(rest.pushnames[0].pushname.as_deref(), Some("A Member"));
    assert_eq!(rest.phone_number_to_lid_mappings.len(), 1);
    assert_eq!(
        rest.phone_number_to_lid_mappings[0].pn_jid.as_deref(),
        Some(FRIEND_PN)
    );
    assert_eq!(
        rest.phone_number_to_lid_mappings[0].lid_jid.as_deref(),
        Some(FRIEND_LID)
    );
}

#[test]
fn a_long_chat_is_really_compressed_and_still_reads() {
    let long = Chunk::new(SyncType::RECENT, 1, 100).chat(Chat::new(FRIEND_LID).texts(
        "T",
        1_700_000_000,
        620,
    ));
    let (proto, blob) = (long.proto(), long.blob());
    assert!(
        blob.len() * 4 < proto.len(),
        "a zlib stream of {} bytes for {} of protobuf is not compressed",
        blob.len(),
        proto.len()
    );
    let mut stream = HistorySyncStream::new(&blob, MAX_DECOMPRESSED);
    let chat = stream.next_conversation().expect("read").expect("a chat");
    assert_eq!(chat.messages.len(), 620);
    let last = chat.messages[619].message.as_option().expect("a message");
    assert_eq!(last.key.id.as_deref(), Some("T619"));
    assert_eq!(last.message_timestamp, Some(1_700_000_619));
}

#[test]
fn a_damaged_blob_does_not_read() {
    let mut blob = chunk().blob();
    // Cut the stream in the middle: zlib's own checksum is gone with the end.
    blob.truncate(blob.len() / 2);
    let mut stream = HistorySyncStream::new(&blob, MAX_DECOMPRESSED);
    let mut outcome = Ok(());
    while outcome.is_ok() {
        match stream.next_conversation() {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => outcome = Err(e),
        }
    }
    assert!(
        outcome.is_err() || stream.remainder().is_err(),
        "a cut stream must fail somewhere"
    );
}

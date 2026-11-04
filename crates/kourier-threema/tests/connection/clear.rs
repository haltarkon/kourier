//! TH5b R1: THE USER'S CLEAR through the real actor (no connection needed: the command loop runs it). The core
//! sends `Command::ClearConversation` for a 1:1 chat and then for a group; the actor deletes every `deferred` row
//! of that chat (by the rules of the map's `place`, whatever the row's kind and time) and confirms the command
//! with `CommandPersisted` in the same zone; rows of another chat, of a group of the same contact, a group row whose
//! container names another group than its `ConversationId`, a list, an envelope without content and a type the
//! pinned enum lacks are kept. A second start ships nothing again. Each test is a parent/child `unshare -rn` pair
//! like its neighbours.

use kourier_backend_api::{BackendEvent, Command};
use kourier_db::rusqlite::Connection;
use kourier_model::{CmdId, ConvKey, UserId};
use kourier_threema::store;
use prost::Message as _;

use super::harness::{self, H, scene};
use super::reflect::{PHONE, deferred};
use crate::support;
use crate::support::key;
use crate::support::mock::START_MS;
use crate::support::mock::body::{self, ty};
use crate::support::mock::envelope::{Author, Conv, Msg};

/// The chat the first clear empties, and another chat (synthetic IDs).
const PEER_A: &str = "B5CLEAR1";
const PEER_B: &str = "B5CLEAR2";
/// A group of each (its creator the contact).
const GROUP_A: u64 = 0x0B05_0000_0000_00A1;
const GROUP_B: u64 = 0x0B05_0000_0000_00B2;

fn user(id: &str) -> UserId {
    UserId::from_parts("threema", id).expect("a user")
}

/// A group's key written out from the spec's rule (creator, a dot, the 16 hex digits of the little-endian bytes),
/// not through the crate's `group_key`.
fn group_key_text(creator: &str, group_id: u64) -> ConvKey {
    let digits: String = group_id
        .to_le_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    ConvKey::Group(format!("threema:{creator}.{digits}").into())
}

/// The ten seeded rows (all of THIS build's version, so the start leaves them; kind `envelope`), in id order: the
/// chat each belongs to is in its comment.
fn rows() -> Vec<Vec<u8>> {
    let author = Author::new(PHONE);
    let vote = |n: u8| Msg::raw([n; 8], START_MS - 5_000, ty::POLL_VOTE, body::filler(20, n));
    let in_container = |creator: &str, group: u64, t: u8, n: u8| {
        Msg::raw(
            [n; 8],
            START_MS - 5_000,
            t,
            body::group_member_container(creator, group, &body::filler(16, n)),
        )
    };
    [
        // 1, chat A: the contact's poll vote.
        author.incoming(PEER_A, &vote(1)),
        // 2, chat A: our own other device's call offer.
        author.outgoing(
            &Conv::contact(PEER_A),
            &Msg::raw(
                [2; 8],
                START_MS - 5_000,
                ty::CALL_OFFER,
                body::filler(30, 2),
            ),
        ),
        // 3, chat B: its contact's poll vote.
        author.incoming(PEER_B, &vote(3)),
        // 4, group A (NOT chat A): contact A's vote in his group.
        author.incoming(PEER_A, &vote(4).in_group(PEER_A, GROUP_A)),
        // 5, group A: our own group call start.
        author.outgoing(
            &Conv::group(PEER_A, GROUP_A),
            &in_container(PEER_A, GROUP_A, ty::GROUP_CALL_START, 5),
        ),
        // 6, no chat: our own group vote aimed at group A whose container names group B (`place` drops it as
        // `group-mismatch`).
        author.outgoing(
            &Conv::group(PEER_A, GROUP_A),
            &in_container(PEER_B, GROUP_B, ty::GROUP_POLL_VOTE, 6),
        ),
        // 7, group B: contact B's vote in his group.
        author.incoming(PEER_B, &vote(7).in_group(PEER_B, GROUP_B)),
        // 8, no chat: our own vote to a distribution list.
        author.outgoing(&Conv::List(9), &vote(8)),
        // 9, no chat: an envelope without content.
        author.empty(),
        // 10, no chat: contact A's message of a type the pinned enum lacks (its form is unknown).
        author.incoming(
            PEER_A,
            &Msg::raw([10; 8], START_MS - 5_000, ty::UNKNOWN, body::filler(12, 10)),
        ),
    ]
    .iter()
    .map(|e| e.encode_to_vec())
    .collect()
}

/// A clear of `conv` as the core sends it (the anchors and times say nothing to Threema).
fn clear(cmd: i64, conv: ConvKey) -> Command {
    Command::ClearConversation {
        cmd: CmdId(cmd),
        conv,
        newest: Vec::new(),
        newest_non_expiring: Vec::new(),
        remove: cmd % 2 == 0,
        before_ms: Some(i64::try_from(START_MS).expect("ms fit i64") - 1),
        system_before_ms: None,
    }
}

/// The core side of one clear: the ONE envelope that comes is exactly `[CommandPersisted { cmd }]`, durable; it
/// is acked (the core committed it).
async fn confirmed(h: &mut H, cmd: i64, case: &str) {
    let envelope = h.envelope().await;
    let ack = envelope.ack.expect("a durable envelope");
    assert_eq!(
        envelope.events,
        [BackendEvent::CommandPersisted { cmd: CmdId(cmd) }],
        "{case}: the core did not get exactly the command's confirmation"
    );
    ack.complete(Ok(()));
}

/// The kept rows' envelopes are exactly the seeded rows `kept` (1-based), in order, and the kept sum is the
/// table's own count and byte sum.
fn assert_kept(h: &H, kept: &[usize], case: &str) {
    let seeded = rows();
    let left: Vec<Vec<u8>> = deferred(h).into_iter().map(|(.., e)| e).collect();
    let expected: Vec<Vec<u8>> = kept.iter().map(|n| seeded[n - 1].clone()).collect();
    assert!(
        left == expected,
        "{case}: the file does not hold exactly the rows {kept:?}"
    );
    let db = store::open(&h.db_file(), &key()).expect("open the file");
    let counted: (i64, i64) = db
        .read(|c: &Connection| {
            c.query_row(
                "SELECT count(*), coalesce(sum(length(envelope)), 0) FROM deferred",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(kourier_db::Error::from)
        })
        .expect("count the rows");
    let total = db
        .read(|c: &Connection| store::deferred_total(c))
        .expect("the kept sum");
    assert_eq!(
        counted,
        (total.rows, total.bytes),
        "{case}: the kept sum drifted"
    );
}

#[test]
fn a_clear_deletes_the_deferred_rows_of_its_chat_and_confirms_the_command() {
    support::offline::run_child(
        module_path!(),
        "a_clear_deletes_the_deferred_rows_of_its_chat_and_confirms_the_command_child",
    );
}

/// R1: ten deferred rows (votes, calls, a group call; of a 1:1 chat A, a chat B, the groups of A and B, and rows of
/// no chat). The core's clear of chat A deletes A's vote and our call offer to A (not A's vote in his GROUP, not A's
/// message of an unknown type) and `CommandPersisted` reaches the core; the clear of group A deletes A's group vote
/// and our group call (not the vote aimed at group A whose container names group B); every other row is kept with an
/// exact kept sum. The anchors, the times and `remove` change nothing. A second start ships nothing again (the
/// confirmations were acked) and keeps the six rows.
#[tokio::test]
async fn a_clear_deletes_the_deferred_rows_of_its_chat_and_confirms_the_command_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut s = scene(5101)
        .with_contacts(&[PEER_A, PEER_B])
        .with_group(PEER_A, GROUP_A)
        .with_group(PEER_B, GROUP_B);
    let at = i64::try_from(START_MS).expect("ms fit i64") - 60_000;
    s.deferred = rows().into_iter().map(|e| (at, e)).collect();
    s.deferred_version = 2;
    let mut h = harness::start(s);
    h.sync().await;
    h.nothing_for_the_core();
    assert_kept(&h, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10], "the start");
    h.send(clear(501, ConvKey::Direct(user(PEER_A)))).await;
    confirmed(&mut h, 501, "chat A").await;
    assert_kept(&h, &[3, 4, 5, 6, 7, 8, 9, 10], "after chat A's clear");
    h.send(clear(502, group_key_text(PEER_A, GROUP_A))).await;
    confirmed(&mut h, 502, "group A").await;
    assert_kept(&h, &[3, 6, 7, 8, 9, 10], "after group A's clear");
    // A chat with no row: confirmed all the same ("also when its network has nothing to tell").
    h.send(clear(503, ConvKey::Direct(user(PEER_A)))).await;
    confirmed(&mut h, 503, "chat A again").await;
    assert_kept(
        &h,
        &[3, 6, 7, 8, 9, 10],
        "after a clear with nothing to delete",
    );
    let mut h = h.restart().await;
    h.sync().await;
    h.nothing_for_the_core();
    assert_kept(&h, &[3, 6, 7, 8, 9, 10], "the second start");
    h.finish().await;
}

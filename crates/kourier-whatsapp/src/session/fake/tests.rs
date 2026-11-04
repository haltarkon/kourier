//! The double's send half (W5a, chunk 7): what the actor's transmit tests (chunk 9a on) script and read. Synthetic
//! ids only (the fictitious 555-01xx range, tails of 8 digits that are substrings of no other value).

use wacore_binary::{Jid, Server};
use waproto::whatsapp as wa;

use super::{Call, FakeSession};
use crate::send::address::Namespace;
use crate::session::{Devices, Groups, Outgoing, ReadKind, SendFailure, Session};

const PEER: &str = "15550136920574";
const AUTHOR: &str = "77700149263085";
const GROUP: &str = "120363000082947163";

#[allow(
    clippy::field_reassign_with_default,
    reason = "wa::Message is non_exhaustive: a struct literal cannot name its fields"
)]
fn outgoing(id: &str) -> Outgoing {
    let mut message = wa::Message::default();
    message.conversation = Some(String::from("synthetic text"));
    Outgoing {
        to: Jid::new(PEER, Server::Pn),
        id: id.to_owned(),
        message,
        expiration: Some(86_400),
        devices: Devices::Cached,
        groups: Groups::Cached,
    }
}

#[tokio::test]
async fn the_double_answers_sends_as_scripted_and_keeps_every_frame() {
    let session = FakeSession::new();
    session.script_send(Err(SendFailure::NoRecipientDevice));
    let first = session.send(outgoing("3EB0AA0000000000000001")).await;
    let mut refreshed = outgoing("3EB0AA0000000000000002");
    refreshed.devices = Devices::Refresh;
    let second = session.send(refreshed.clone()).await;
    assert!(
        first == Err(SendFailure::NoRecipientDevice),
        "the scripted failure was not answered"
    );
    assert!(second == Ok(()), "an unscripted send is not written");
    assert!(
        session.sent() == vec![outgoing("3EB0AA0000000000000001"), refreshed],
        "the frames given to the double are not kept whole and in order"
    );
    assert!(
        session.count(&Call::Send(String::from("3EB0AA0000000000000001"))) == 1,
        "the send is not in the call log"
    );
}

#[tokio::test]
async fn the_double_keeps_receipts_typing_and_answers_the_addressing_inputs() {
    let session = FakeSession::new();
    let group = Jid::group(GROUP);
    let author = Jid::new(AUTHOR, Server::Lid);
    let ids = vec![String::from("3EB0BB0000000000000001")];
    assert!(
        session
            .mark_read(
                group.clone(),
                Some(author.clone()),
                ids.clone(),
                ReadKind::Played
            )
            .await
            == Ok(()),
        "a receipt is refused"
    );
    let receipts = session.receipts();
    assert!(
        receipts.len() == 1
            && receipts[0].chat == group
            && receipts[0].sender.as_ref() == Some(&author)
            && receipts[0].ids == ids
            && receipts[0].kind == ReadKind::Played,
        "the receipt is not kept as asked"
    );
    let peer = Jid::new(PEER, Server::Pn);
    session
        .mark_read(peer.clone(), None, ids.clone(), ReadKind::Read)
        .await
        .expect("a read");
    assert!(
        session
            .receipts()
            .last()
            .is_some_and(|r| r.chat == peer && r.sender.is_none() && r.kind == ReadKind::Read),
        "a direct chat's read is not kept as asked"
    );
    session.typing(peer.clone(), true).await.expect("typing");
    session.typing(peer.clone(), false).await.expect("paused");
    assert!(
        session.typed() == vec![(peer.clone(), true), (peer, false)],
        "the typing states are not kept in order"
    );
    assert!(!session.lid_migrated().await, "a new double is migrated");
    session.set_migrated(true);
    assert!(session.lid_migrated().await, "the migration is not set");
    session.script_group_mode(Ok(Namespace::Pn));
    session.script_group_mode(Err(SendFailure::IqTimeout));
    assert!(session.group_mode(group.clone()).await == Ok(Namespace::Pn));
    assert!(session.group_mode(group.clone()).await == Err(SendFailure::IqTimeout));
    assert!(
        session.group_mode(group).await == Ok(Namespace::Lid),
        "an unscripted group is not in LID mode"
    );
}

/// W6a2 (B3): the double records every group read as one call, keeps the groups a batch was asked for, and answers what
/// was scripted, in order (nothing scripted: no group, no result, an empty blocklist).
#[tokio::test]
async fn the_double_answers_group_and_blocklist_reads_as_scripted_and_keeps_what_was_asked() {
    use crate::session::AskFailure;
    use whatsapp_rust::GroupMetadataResult;

    let session = FakeSession::new();
    let asked = [
        Jid::new("120363000082947163", Server::Group),
        Jid::new("120363000082947164", Server::Group),
    ];
    session.script_participating(Err(AskFailure::Timeout));
    session.script_batch(Ok(vec![GroupMetadataResult::NotFound(asked[1].clone())]));
    session.script_blocklist(Err(AskFailure::NotConnected));

    assert!(session.participating().await == Err(AskFailure::Timeout));
    assert!(
        session.participating().await.map(|list| list.len()) == Ok(0),
        "nothing scripted: no group"
    );
    let batch = session.fetch_groups(asked.to_vec()).await;
    assert!(
        matches!(&batch, Ok(list) if matches!(list.as_slice(), [GroupMetadataResult::NotFound(id)] if *id == asked[1])),
        "the scripted result was not answered"
    );
    assert!(
        session
            .fetch_groups(vec![asked[0].clone()])
            .await
            .map(|l| l.len())
            == Ok(0),
        "nothing scripted: no result"
    );
    assert!(session.fetched() == vec![asked.to_vec(), vec![asked[0].clone()]]);
    assert!(session.blocklist().await == Err(AskFailure::NotConnected));
    assert!(session.blocklist().await == Ok(Vec::new()));
    assert_eq!(
        session.calls(),
        [
            Call::Participating,
            Call::Participating,
            Call::FetchGroups(2),
            Call::FetchGroups(1),
            Call::Blocklist,
            Call::Blocklist
        ]
    );
}

/// W6a2 (B6): the double records a single query as one call, keeps the group, and answers what was scripted, in order
/// (nothing scripted: the server says it is not there).
#[tokio::test]
async fn the_double_answers_a_single_group_query_as_scripted_and_keeps_what_was_asked() {
    use crate::session::AskFailure;
    use whatsapp_rust::GroupMetadataResult;

    let session = FakeSession::new();
    let asked = Jid::new("120363000082947165", Server::Group);
    session.script_group(Err(AskFailure::Timeout));
    session.script_group(Ok(GroupMetadataResult::Forbidden(asked.clone())));
    assert!(session.fetch_group(asked.clone()).await == Err(AskFailure::Timeout));
    assert!(matches!(
        session.fetch_group(asked.clone()).await,
        Ok(GroupMetadataResult::Forbidden(id)) if id == asked
    ));
    assert!(matches!(
        session.fetch_group(asked.clone()).await,
        Ok(GroupMetadataResult::NotFound(id)) if id == asked
    ));
    assert!(session.fetched_one() == vec![asked.clone(); 3]);
    assert_eq!(
        session.calls(),
        [Call::FetchGroup, Call::FetchGroup, Call::FetchGroup]
    );
}

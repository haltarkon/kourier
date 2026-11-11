//! D6 in the backend (W5a, D7): what starts a direct chat, one person under two ids, and the stated features.
//! Synthetic ids only: the fictitious 555-01xx range, tails of 8 digits that are substrings of no other value.

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_db::DbKey;
use kourier_db::rusqlite::params;
use kourier_model::{Body, ConvKey, ConversationPatch, Origin, SystemEvent, UserId};

use super::{How, forget_statements, holds, mark_stated, needs_statement, note, note_messages};
use crate::store::{KourierStore, StoreOwner, exec};

const PEER_PN: &str = "15550138264907";
const PEER_LID: &str = "77700191736452";
const CALLER: &str = "15550172940385";
const ECHO: &str = "15550126583719";
const OWN_THIS: &str = "15550154079263";
const STRANGER: &str = "15550169318054";
const GROUP: &str = "120363000047285619";

fn person(scheme: &str, value: &str) -> UserId {
    UserId::from_parts(scheme, value).expect("a user")
}

fn direct(scheme: &str, value: &str) -> ConvKey {
    ConvKey::Direct(person(scheme, value))
}

fn group() -> ConvKey {
    ConvKey::Group(format!("wag:{GROUP}").into())
}

fn message(conv: &ConvKey, origin: Origin, body: Body) -> BackendEvent {
    BackendEvent::Message(IncomingMessage::new(
        conv.clone(),
        "M1",
        origin,
        body,
        1_000,
        1_000,
    ))
}

fn from_peer(conv: &ConvKey, body: Body) -> BackendEvent {
    let author = conv
        .direct_peer()
        .cloned()
        .unwrap_or_else(|| person("pn", PEER_PN));
    message(
        conv,
        Origin::Remote {
            author,
            device: None,
        },
        body,
    )
}

fn open() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([9; 32]),
        "wa-started",
    )
    .expect("open");
    (dir, owner)
}

async fn started(store: &KourierStore, conv: &ConvKey) -> bool {
    let conv = conv.clone();
    store
        .read(move |conn| holds(conn, &conv))
        .await
        .expect("look")
}

async fn how(store: &KourierStore, conv: &ConvKey) -> Option<i64> {
    let key = conv.encode();
    store
        .read(move |conn| {
            crate::store::first(
                conn,
                "read how",
                "SELECT how FROM chat_started WHERE conv = ?1",
                [key],
                |r| r.get(0),
            )
        })
        .await
        .expect("read")
}

#[tokio::test]
async fn a_message_of_the_peer_or_of_our_other_device_starts_a_direct_chat_and_nothing_else_does() {
    let (_dir, owner) = open();
    let store = owner.store();
    let peer = direct("pn", PEER_PN);
    let caller = direct("pn", CALLER);
    let echo = direct("pn", ECHO);
    let own_this = direct("pn", OWN_THIS);
    let stranger = direct("pn", STRANGER);
    let events = vec![
        // A call line of a stranger: the core has the chat, the phone does not.
        from_peer(&caller, Body::System(SystemEvent::Joined)),
        // Ours from this device (only the ledger's ack proves that one).
        message(&own_this, Origin::OwnThisDevice, Body::text("mine")),
        // A chat upsert alone.
        BackendEvent::ConversationUpsert(ConversationPatch::new(stranger.clone())),
        from_peer(&peer, Body::text("hi")),
        message(
            &echo,
            Origin::OwnOtherDevice { device: None },
            Body::text("from the phone"),
        ),
        from_peer(&peer, Body::text("again")),
        from_peer(&group(), Body::text("in a group")),
    ];
    let noted = store
        .write("note", move |tx| note_messages(tx, &events))
        .await
        .expect("note");
    assert_eq!(noted, 2, "the peer's chat once and the echo's chat");
    assert_eq!(how(&store, &peer).await, Some(How::Peer as i64));
    assert_eq!(how(&store, &echo).await, Some(How::OwnOtherDevice as i64));
    for (conv, what) in [
        (&caller, "a call line"),
        (&own_this, "an own message not acked"),
        (&stranger, "a chat upsert"),
    ] {
        assert!(!started(&store, conv).await, "{what} started a chat");
    }
    assert!(started(&store, &peer).await && started(&store, &echo).await);
    // A group is always started and gets no row; a list never is.
    assert!(started(&store, &group()).await, "a group is started");
    assert_eq!(how(&store, &group()).await, None, "a group has no row");
    let list = ConvKey::Broadcast("wab:15550100000".into());
    assert!(!started(&store, &list).await, "a list is never started");
    // The first reason is kept.
    let again = peer.clone();
    store
        .write("note", move |tx| note(tx, &again, How::Acked))
        .await
        .expect("note");
    assert_eq!(
        how(&store, &peer).await,
        Some(How::Peer as i64),
        "the first reason stays"
    );
    owner.close();
}

/// Fix F2: the one rule of the live and the history paths. A call record the class table maps to an unsupported
/// body (`call_log_messsage`) proves no start, as no call line does; any other message of the peer does, an
/// unsupported kind included. Mutant: the call kinds not excluded.
#[tokio::test]
async fn a_record_of_a_call_proves_no_start_and_another_unsupported_message_does() {
    let (_dir, owner) = open();
    let store = owner.store();
    let unsupported = |kind: &str| Body::Unsupported {
        kind_hint: kind.to_owned(),
        summary: None,
    };
    let (logged, group_call, scheduled, other) = (
        direct("pn", CALLER),
        direct("pn", ECHO),
        direct("pn", STRANGER),
        direct("pn", PEER_PN),
    );
    let events = vec![
        from_peer(&logged, unsupported("call_log_messsage")),
        from_peer(&group_call, unsupported("bcall_message")),
        from_peer(&scheduled, unsupported("scheduled_call_creation_message")),
        from_peer(&other, unsupported("payment_invite_message")),
    ];
    let noted = store
        .write("note", move |tx| note_messages(tx, &events))
        .await
        .expect("note");
    assert_eq!(noted, 1, "only the message of a person starts a chat");
    for (conv, what) in [
        (&logged, "a call log message"),
        (&group_call, "a call record of a group"),
        (&scheduled, "a scheduled call"),
    ] {
        assert!(!started(&store, conv).await, "{what} started a chat");
    }
    assert!(started(&store, &other).await);
    owner.close();
}

#[tokio::test]
async fn a_chat_started_under_one_id_of_a_person_holds_under_the_other() {
    let (_dir, owner) = open();
    let store = owner.store();
    let (pn, lid) = (direct("pn", PEER_PN), direct("lid", PEER_LID));
    let noted = lid.clone();
    store
        .write("note", move |tx| note(tx, &noted, How::Peer))
        .await
        .expect("note");
    assert!(
        !started(&store, &pn).await,
        "no mapping yet: the number's chat is not proven"
    );
    store
        .write("learn", |tx| {
            exec(
                tx,
                "learn",
                "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                 VALUES (?1, ?2, 0, 0, 'test')",
                params![PEER_LID, PEER_PN],
            )
        })
        .await
        .expect("learn");
    assert!(
        started(&store, &pn).await,
        "the LID's start holds for the number"
    );
    // And the other way: a start under the number holds for the LID.
    let (dir2, owner2) = open();
    let store2 = owner2.store();
    let noted = pn.clone();
    store2
        .write("note", move |tx| {
            exec(
                tx,
                "learn",
                "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                 VALUES (?1, ?2, 0, 0, 'test')",
                params![PEER_LID, PEER_PN],
            )?;
            note(tx, &noted, How::History)
        })
        .await
        .expect("note");
    assert!(
        started(&store2, &lid).await,
        "the number's start holds for the LID"
    );
    owner2.close();
    drop(dir2);
    owner.close();
}

/// Fix F3 (the recycled number): a number that got a new LID (it was re-registered by another person; the old
/// LID still answers) lends no start to the new one. The frame goes to the number's CURRENT LID, so the started
/// fact is read by the same resolution: a `pn:` chat sees its own row and its current LID's; a `lid:` chat its own
/// row, and its number's row only while it is the number's current LID. New facts of a number go under its
/// current LID. Mutants: every LID of the number counted (the old `forms`); the "current LID" condition of a
/// `lid:` chat dropped; the fact of a number noted under the number.
#[tokio::test]
async fn a_start_is_not_lent_to_another_lid_of_the_same_number() {
    const OLD_LID: &str = "77700112384956";
    const NEW_LID: &str = "77700123495067";
    let (_dir, owner) = open();
    let store = owner.store();
    store
        .write("learn", |tx| {
            for (lid, at) in [(OLD_LID, 1), (NEW_LID, 2)] {
                exec(
                    tx,
                    "learn",
                    "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                     VALUES (?1, ?2, ?3, ?3, 'test')",
                    params![lid, PEER_PN, at],
                )?;
            }
            Ok(())
        })
        .await
        .expect("learn");
    let (number, old, new) = (
        direct("pn", PEER_PN),
        direct("lid", OLD_LID),
        direct("lid", NEW_LID),
    );
    // The older LID's person started the chat.
    let noted = old.clone();
    store
        .write("note", move |tx| note(tx, &noted, How::Peer))
        .await
        .expect("note");
    assert!(started(&store, &old).await, "its own row");
    assert!(
        !started(&store, &new).await,
        "the newer LID is another person's: the older LID's start is not lent (F3)"
    );
    assert!(
        !started(&store, &number).await,
        "the number writes to its CURRENT LID, which nobody started"
    );

    // A fact of the number (noted before the number had a LID, or by a build that knew none) holds for the number
    // and for its current LID, and not for the older one.
    let (_dir2, owner2) = open();
    let store2 = owner2.store();
    store2
        .write("learn and note", |tx| {
            for (lid, at) in [(OLD_LID, 1), (NEW_LID, 2)] {
                exec(
                    tx,
                    "learn",
                    "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                     VALUES (?1, ?2, ?3, ?3, 'test')",
                    params![lid, PEER_PN, at],
                )?;
            }
            exec(
                tx,
                "a fact of the number",
                "INSERT INTO chat_started(conv, how) VALUES (?1, 0)",
                [direct("pn", PEER_PN).encode()],
            )
            .map(drop)
        })
        .await
        .expect("learn");
    assert!(started(&store2, &number).await, "the number's own row");
    assert!(
        started(&store2, &new).await,
        "the current LID of the number sees the number's row"
    );
    assert!(
        !started(&store2, &old).await,
        "an older LID of the number does not see the number's row (F3)"
    );

    // A new fact of the number goes under its current LID.
    let (_dir3, owner3) = open();
    let store3 = owner3.store();
    store3
        .write("learn", |tx| {
            for (lid, at) in [(OLD_LID, 1), (NEW_LID, 2)] {
                exec(
                    tx,
                    "learn",
                    "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                     VALUES (?1, ?2, ?3, ?3, 'test')",
                    params![lid, PEER_PN, at],
                )?;
            }
            Ok(())
        })
        .await
        .expect("learn");
    let noted = number.clone();
    store3
        .write("note", move |tx| note(tx, &noted, How::Peer))
        .await
        .expect("note");
    assert_eq!(how(&store3, &new).await, Some(How::Peer as i64));
    assert_eq!(how(&store3, &number).await, None, "noted under the LID");
    assert!(started(&store3, &number).await && started(&store3, &new).await);
    assert!(!started(&store3, &old).await);
    owner.close();
    owner2.close();
    owner3.close();
}

/// Fix F4 (the robustness MEDIUM; the review's experiment E1): migration 5 on a store that ran under W3/W4. The
/// file at schema 4 holds a direct chat the phone's history named, a message secret and an inbox ledger row of the
/// peer's, and W5a opens it. NOTHING of that PROVES a real message of the chat under F2's rule (a chats row is no
/// start; a ledger row or a secret may belong to a reaction, an edit or a call record, and the inbox keeps no kind),
/// so migration 5 backfills no started fact: the chat states `NotAccepted` and a send is refused until the peer's
/// next message lifts it (the fail-safe; the docs say "the peer writes first"). Mutant: a backfill in migration 5
/// from `history_chats`.
#[test]
fn a_store_that_ran_before_w5a_opens_with_no_started_fact_and_its_chats_are_not_accepted() {
    use kourier_db::{Migrations, migration};
    use kourier_model::CannotSend;

    const BEFORE_W5A: Migrations = Migrations::new(
        "whatsapp",
        &[
            migration!(1, "0001_init"),
            migration!(2, "0002_inbox"),
            migration!(3, "0003_inbox_age"),
            migration!(4, "0004_history"),
        ],
    );
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("backend.db");
    let key = DbKey::from([12; 32]);
    let chat = direct("pn", PEER_PN);
    let jid = format!("{PEER_PN}@s.whatsapp.net");
    {
        let mut conn = kourier_db::open(&path, &key).expect("open");
        kourier_db::migrate(&mut conn, &kourier_db::outbox::MIGRATIONS).expect("outbox");
        kourier_db::migrate(&mut conn, &BEFORE_W5A).expect("schema 4");
        conn.execute(
            "INSERT INTO history_chats(conv, more, hint_stated, last_chunk, sync_type, ended) \
             VALUES (?1, 0, 1, 1, 0, 1)",
            [chat.encode()],
        )
        .expect("a history chat");
        conn.execute(
            "INSERT INTO inbox(chat, sender, id, origin, received_at, catch_up, state) \
             VALUES (?1, ?1, 'M1', 'e2e', 1, 0, 'done')",
            [&jid],
        )
        .expect("an inbox ledger row");
        conn.execute(
            "INSERT INTO msg_secrets(chat, sender, msg_id, secret) VALUES (?1, ?1, 'M1', x'0102')",
            [&jid],
        )
        .expect("a message secret");
    }
    let mut conn = crate::db::open(&path, &key).expect("W5a opens a store of schema 4");
    let version: i64 = conn
        .query_row(
            "SELECT version FROM schema_version WHERE component = 'whatsapp'",
            [],
            |r| r.get(0),
        )
        .expect("version");
    assert_eq!(version, 9, "migrated to this build's schema (W8: 9)");
    let started_rows: i64 = conn
        .query_row("SELECT count(*) FROM chat_started", [], |r| r.get(0))
        .expect("count");
    assert_eq!(
        started_rows, 0,
        "no source of the old store proves a real message: migration 5 backfills no start"
    );
    assert!(
        !holds(&conn, &chat).expect("look"),
        "refused until the peer writes"
    );
    // The start sweep states the chat as it is: not accepted.
    let tx = conn.transaction().expect("tx");
    let sweep = crate::features::sweep(&tx).expect("sweep");
    tx.commit().expect("commit");
    let stated: Vec<_> = sweep
        .iter()
        .flatten()
        .filter_map(|event| match event {
            BackendEvent::ConversationUpsert(patch) if patch.key == chat => patch.features.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(stated.len(), 1, "the history's chat is stated once");
    assert_eq!(stated[0].cannot_send, Some(CannotSend::NotAccepted));
    // The peer's next message lifts it.
    let tx = conn.transaction().expect("tx");
    note_messages(&tx, &[from_peer(&chat, Body::text("synthetic hello"))]).expect("note");
    tx.commit().expect("commit");
    assert!(holds(&conn, &chat).expect("look"), "the peer wrote first");
}

#[tokio::test]
async fn a_chat_is_stated_again_when_the_version_or_the_fact_changes_and_after_a_rebuild() {
    let (_dir, owner) = open();
    let store = owner.store();
    let conv = direct("pn", PEER_PN);
    let needs = |version: u32, started: bool| {
        let conv = conv.clone();
        let store = store.clone();
        async move {
            store
                .read(move |conn| needs_statement(conn, &conv, version, started))
                .await
                .expect("look")
        }
    };
    assert!(needs(1, false).await, "never stated");
    let marked = conv.clone();
    store
        .write("mark", move |tx| mark_stated(tx, &marked, 1, false))
        .await
        .expect("mark");
    assert!(!needs(1, false).await, "stated once per version and fact");
    assert!(needs(1, true).await, "the fact changed");
    assert!(needs(2, false).await, "the version changed");
    store
        .write("forget", forget_statements)
        .await
        .expect("forget");
    assert!(needs(1, false).await, "a rebuild states every chat again");
    owner.close();
}

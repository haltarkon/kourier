//! TH6 L5: CONTACTS, THE DIRECTORY AND THE `CONTACT_SYNC` TRANSACTION against the offline fake (brief §2.10, the
//! critic's T-H3 and T-L2). Kourier leads with the phone offline (or following): a text of a sender the file does not
//! know is looked up in the directory, the contact is CREATED in a transaction (the phone gets the `ContactSync.Create`
//! at the commit, before the text's reflection) and the text maps; a stored contact's new nickname and a group's
//! contact made DIRECT are one `ContactSync.Update` each; a transaction the phone holds is waited for and, once the
//! phone made the contact itself, skipped; a directory that fails (429, unreachable, no answer in time) HOLDS only the
//! unknown sender's messages on that socket while a known sender's text goes, and the socket is recycled gracefully
//! until the directory answers (never the counted backoff); a sender the directory does not know is discarded and
//! acked. Every test ends with the guard oracle and the violations check (`finish`).

use kourier_backend_api::{BackendEvent, Command};
use kourier_model::ConnState;
use libthreema::protobuf::d2d;
use libthreema::protobuf::d2d::transaction_scope::Scope;
use libthreema::protobuf::d2d_sync::contact::AcquaintanceLevel;

use super::harness::{self, DEVICE, H, ME, SERVER_GROUP, scene, traced};
use super::lines::{
    WALL, at, count_line, counts_of, equals_the_trace, no_id, online_as, wait_lines,
};
use super::reflect::PHONE;
use crate::support;
use crate::support::mock::body::{self, ty};
use crate::support::mock::d2m::ConnId;
use crate::support::mock::envelope::{Author, ContactDelta, Reflect as _};
use crate::support::mock::peer::Letter;
use crate::support::mock::phone::PhoneEvent;
use crate::support::mock::txn::{TxnEnd, TxnSeen};
use crate::support::warns::{Line, Warns};

/// A stored contact.
const ANN: &str = "PEER0001";
/// A stored contact known only through a group (`GROUP_OR_DELETED`).
const BEN: &str = "PEER0002";
/// A sender the file does not know (the directory does).
const NEW: &str = "PEER0009";
/// D2's words.
const HELD: &str = "a chat server message is held for the phone";
/// The DEBUG line of a discard (its field `rule`).
const DISCARDED: &str = "a received message was discarded by the receive steps";
/// NEW's group (the file does not hold it).
const NEWS: u64 = 0x4444_0000_0000_0001;
/// ANN's group the file does not hold either.
const ANNS_UNKNOWN: u64 = 0x5555_0000_0000_0001;

/// Waits (a wall BOUND) until `conn` sent `n` CSP acks of incoming messages.
async fn wait_acks(h: &H, conn: ConnId, n: usize) {
    let world = &h.s.world;
    assert!(
        h.net()
            .wait(
                || traced(world, conn, "in", "csp-incoming-message-ack") >= n,
                WALL
            )
            .await,
        "{} of {n} acks came",
        traced(world, conn, "in", "csp-incoming-message-ack")
    );
    assert_eq!(
        traced(world, conn, "in", "csp-incoming-message-ack"),
        n,
        "more acks than expected"
    );
}

/// The core side that acks what it gets until `done` holds for the events so far (a wall bound per envelope).
async fn core_until(h: &mut H, done: impl Fn(&[BackendEvent]) -> bool) -> Vec<BackendEvent> {
    let mut events = Vec::new();
    while !done(&events) {
        let envelope = h.envelope().await;
        if let Some(ack) = envelope.ack {
            ack.complete(Ok(()));
        }
        events.extend(envelope.events);
    }
    events
}

/// Moves both clocks from one deadline of the actor to the next (each a deadline it waits for) until `done` holds; a
/// bound of 400 looks. The next deadline is read only once the actor is PARKED ([`H::wait_parked`]; `sync` alone let a
/// loaded run read a list that held only a pending lookup's bound and jump 30 s past the keep-alive). The failure names
/// the time, the deadlines and the sockets.
async fn advance_until(h: &H, what: &str, done: impl Fn() -> bool) {
    for _ in 0..400 {
        h.sync().await;
        if done() {
            return;
        }
        let deadlines = h.wait_parked().await;
        if done() {
            return;
        }
        h.advance_to(deadlines[0].max(h.now())).await;
    }
    h.sync().await;
    assert!(
        done(),
        "{what} never came: at +{} ms, deadlines {:?}, {} sockets, {:?}; {}",
        h.now().saturating_sub(at(0)),
        h.clock
            .deadlines()
            .iter()
            .map(|d| d.saturating_sub(at(0)))
            .collect::<Vec<_>>(),
        h.net().opened().len(),
        h.state(),
        sockets_told(h)
    );
}

/// Each socket's end as the fake saw it (the close code, whether the client closed, its CSP acks) and the lookups'
/// statuses: what a failure of [`advance_until`] names.
fn sockets_told(h: &H) -> String {
    let m = h.s.world.mediator();
    let sockets: Vec<String> = h
        .net()
        .opened()
        .iter()
        .map(|conn| {
            format!(
                "{:?} by client {} acks {}",
                m.closed(*conn),
                traced(&h.s.world, *conn, "in", "close"),
                traced(&h.s.world, *conn, "in", "csp-incoming-message-ack")
            )
        })
        .collect();
    let lookups: Vec<Option<u16>> = h
        .net()
        .directory()
        .asked()
        .iter()
        .map(|a| a.status)
        .collect();
    format!("sockets {sockets:?}, lookups {lookups:?}")
}

fn messages(events: &[BackendEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, BackendEvent::Message(_)))
        .count()
}

fn contacts(events: &[BackendEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, BackendEvent::ContactUpsert(_)))
        .count()
}

/// Kourier's own transactions: (begun, rejected, committed).
fn kourier_transactions(h: &H, conn: ConnId) -> (usize, usize, usize) {
    let seen = h.s.world.mediator().transactions();
    let begun = seen
        .iter()
        .filter(|t| matches!(t, TxnSeen::Begun { conn: c, .. } if *c == conn))
        .count();
    let rejected = seen
        .iter()
        .filter(|t| matches!(t, TxnSeen::Rejected { conn: c, .. } if *c == conn))
        .count();
    let committed = seen
        .iter()
        .filter(|t| {
            matches!(t, TxnSeen::Ended { conn: c, how: TxnEnd::Committed, released: 1, .. } if *c == conn)
        })
        .count();
    (begun, rejected, committed)
}

/// Every scope a client sealed opens with DGTSK as `CONTACT_SYNC`.
fn scopes_are_contact_sync(h: &H) {
    let scopes = h.s.world.transaction_scopes(&h.s.keys);
    assert!(!scopes.is_empty(), "no transaction");
    assert!(
        scopes.iter().all(|s| *s == Some(Scope::ContactSync)),
        "a scope that is not CONTACT_SYNC"
    );
}

/// The `ContactSync`s the phone viewed, as (create?, identity, nickname, acquaintance level).
fn synced(view: &[&d2d::ContactSync]) -> Vec<(bool, String, Option<String>, Option<i32>)> {
    view.iter()
        .map(|c| match &c.action {
            Some(d2d::contact_sync::Action::Create(c)) => {
                let c = c.contact.as_ref().expect("a contact");
                (
                    true,
                    c.identity.clone(),
                    c.nickname.clone(),
                    c.acquaintance_level,
                )
            }
            Some(d2d::contact_sync::Action::Update(u)) => {
                let c = u.contact.as_ref().expect("a contact");
                (
                    false,
                    c.identity.clone(),
                    c.nickname.clone(),
                    c.acquaintance_level,
                )
            }
            None => panic!("a contact sync without an action"),
        })
        .collect()
}

#[test]
fn an_unknown_sender_is_looked_up_created_in_a_transaction_and_its_text_maps() {
    support::offline::run_child(
        module_path!(),
        "an_unknown_sender_is_looked_up_created_in_a_transaction_and_its_text_maps_child",
    );
}

/// 22.4.2: two texts of a sender the file does not know wait at the chat server. The first is looked up ONCE in the
/// directory, its contact CREATED in a `CONTACT_SYNC` transaction (begin, the `ContactSync.Create` reflected inside,
/// commit), then ZONE RECEIVE applies the committed delta first and maps the text; the second goes as a stored
/// contact's text (no second lookup, no second transaction). The core gets the contact and both texts; the phone, back
/// as follower, views the create (nickname, DIRECT) before the two texts.
#[tokio::test]
async fn an_unknown_sender_is_looked_up_created_in_a_transaction_and_its_text_maps_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_601).with_peer(ANN);
    s.world.add_identity(NEW);
    let mut new = s.world.peer(NEW);
    new.send(&Letter::text(ME, 0x61, "synthetic text").nickname("Newbie"));
    new.send(&Letter::text(ME, 0x62, "synthetic second"));
    let mut phone = s.world.phone(&s.dgk, SERVER_GROUP, PHONE);
    phone.connect();
    phone.disconnect();
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    let conn = h.wait_connected().await;
    wait_acks(&h, conn, 2).await;
    let events = h.ack_until("Online as leader", online_as("leader")).await;
    assert_eq!(
        (contacts(&events), messages(&events)),
        (1, 2),
        "the core's contact and texts"
    );
    let asked = h.net().directory().asked();
    assert_eq!(asked.len(), 1, "not one lookup");
    assert_eq!(asked[0].ids, [NEW], "the lookup's IDs");
    assert_eq!(
        kourier_transactions(&h, conn),
        (1, 0, 1),
        "one transaction, committed with its one reflect"
    );
    scopes_are_contact_sync(&h);
    assert_eq!(
        h.file_rows("contact"),
        2,
        "the new contact is not in the file"
    );
    phone.connect();
    phone.incoming();
    let view = phone.view();
    assert_eq!(
        synced(&view.contacts()),
        [(
            true,
            NEW.to_owned(),
            Some("Newbie".to_owned()),
            Some(AcquaintanceLevel::Direct as i32)
        )],
        "the create the phone viewed"
    );
    assert_eq!(
        view.incoming_ids(),
        [(NEW.to_owned(), 0x61), (NEW.to_owned(), 0x62)],
        "the texts the phone viewed"
    );
    let kinds = view.kinds();
    let create_at = kinds.iter().position(|k| k.contains("contact"));
    let text_at = kinds.iter().position(|k| k.contains("incoming"));
    assert!(
        create_at < text_at,
        "the create came after a text: {kinds:?}"
    );
    h.finish().await;
}

#[test]
fn a_new_nickname_and_a_groups_contact_made_direct_are_one_update_each() {
    support::offline::run_child(
        module_path!(),
        "a_new_nickname_and_a_groups_contact_made_direct_are_one_update_each_child",
    );
}

/// 22.5: ANN (stored as "Old") sends a text under the nickname "New": ONE `ContactSync.Update` with the nickname only;
/// BEN (stored, known only through a group) sends a 1:1 text, which creates a direct contact: ONE update with the level
/// DIRECT only. Each in its own transaction, each text mapped after its update.
#[tokio::test]
async fn a_new_nickname_and_a_groups_contact_made_direct_are_one_update_each_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_602)
        .with_contact_as(ANN, Some("Old"), false)
        .with_contact_as(BEN, None, true);
    let mut ann = s.world.peer(ANN);
    let mut ben = s.world.peer(BEN);
    ann.send(&Letter::text(ME, 0x21, "synthetic text").nickname("New"));
    ben.send(&Letter::text(ME, 0x22, "synthetic text"));
    let mut phone = s.world.phone(&s.dgk, SERVER_GROUP, PHONE);
    phone.connect();
    phone.disconnect();
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    let conn = h.wait_connected().await;
    wait_acks(&h, conn, 2).await;
    let events = h.ack_until("Online as leader", online_as("leader")).await;
    assert_eq!(messages(&events), 2, "the texts");
    assert!(contacts(&events) >= 2, "the two contacts' upserts");
    assert_eq!(
        h.net().directory().asked().len(),
        0,
        "a stored sender was looked up"
    );
    assert_eq!(
        kourier_transactions(&h, conn),
        (2, 0, 2),
        "two transactions"
    );
    scopes_are_contact_sync(&h);
    phone.connect();
    phone.incoming();
    let view = phone.view();
    assert_eq!(
        synced(&view.contacts()),
        [
            (false, ANN.to_owned(), Some("New".to_owned()), None),
            (
                false,
                BEN.to_owned(),
                None,
                Some(AcquaintanceLevel::Direct as i32)
            ),
        ],
        "the updates the phone viewed"
    );
    assert_eq!(
        view.incoming_ids(),
        [(ANN.to_owned(), 0x21), (BEN.to_owned(), 0x22)]
    );
    h.finish().await;
}

#[test]
fn a_rejected_transaction_waits_for_its_end_and_skips_the_create_the_phone_made() {
    support::offline::run_child(
        module_path!(),
        "a_rejected_transaction_waits_for_its_end_and_skips_the_create_the_phone_made_child",
    );
}

/// Rejected -> Ended -> the precondition again -> skip: Kourier leads and the phone follows; the phone holds a
/// `CONTACT_SYNC` transaction. A text of an unknown sender comes: looked up, Kourier's begin is REJECTED (the phone
/// holds the lock) and it waits. The phone creates that contact inside its transaction and commits: Kourier applies the
/// phone's reflection, gets `TransactionEnded`, finds the contact in the file and begins NOTHING more; the text maps and
/// is acked.
#[tokio::test]
async fn a_rejected_transaction_waits_for_its_end_and_skips_the_create_the_phone_made_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_603).with_peer(ANN);
    let new_key = s.world.add_identity(NEW).ck.public;
    let mut new = s.world.peer(NEW);
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    let conn = h.wait_connected().await;
    h.ack_until("Online as leader", online_as("leader")).await;
    let mut phone = h.s.world.phone(&h.s.dgk, SERVER_GROUP, PHONE);
    phone.connect();
    assert!(
        matches!(
            phone.begin_transaction(Scope::ContactSync, 0),
            PhoneEvent::TxnBegun
        ),
        "the phone did not get the lock"
    );
    new.send(&Letter::text(ME, 0x31, "synthetic text"));
    h.net().flush();
    let m = h.s.world.mediator().clone();
    assert!(
        h.net()
            .wait(|| kourier_transactions(&h, conn).1 == 1, WALL)
            .await,
        "Kourier's begin was not rejected"
    );
    h.sync().await;
    assert_eq!(
        traced(&h.s.world, conn, "in", "csp-incoming-message-ack"),
        0,
        "acked before the contact"
    );
    phone.reflect_envelope(&Author::new(PHONE).contact_create(ContactDelta::create(
        NEW,
        new_key,
        at(1),
    )));
    assert!(phone.commit_transaction(), "the phone's commit");
    h.net().flush();
    wait_acks(&h, conn, 1).await;
    let events = core_until(&mut h, |e| messages(e) == 1).await;
    assert_eq!(contacts(&events), 1, "the phone's contact reached the core");
    assert_eq!(
        kourier_transactions(&h, conn),
        (0, 1, 0),
        "Kourier began again after the phone made the contact"
    );
    assert_eq!(m.transaction_holder(&h.s.keys.path_public), None);
    assert_eq!(h.net().directory().asked().len(), 1, "not one lookup");
    scopes_are_contact_sync(&h);
    phone.incoming();
    assert_eq!(
        phone.view().incoming_ids(),
        [(NEW.to_owned(), 0x31)],
        "the phone's view"
    );
    assert!(
        phone.view().contacts().is_empty(),
        "Kourier reflected a contact"
    );
    h.finish().await;
}

#[test]
fn a_directory_that_fails_holds_only_the_unknown_senders_and_the_socket_recovers() {
    support::offline::run_child(
        module_path!(),
        "a_directory_that_fails_holds_only_the_unknown_senders_and_the_socket_recovers_child",
    );
}

/// Critic T-H3: the directory answers 429 on the first socket, cannot be reached on the second and does not answer
/// within 30 s on the third. On each, the unknown sender's two texts are HELD (`directory-down`, one lookup per socket:
/// the second text is held without a call) while ANN's text goes on the first; each socket is recycled gracefully 60 s
/// after its first hold (never the counted backoff: no `NeedsUserAction`, no wait between the sockets). On the fourth
/// the directory answers: the contact is created and both texts are reflected and acked.
#[tokio::test]
async fn a_directory_that_fails_holds_only_the_unknown_senders_and_the_socket_recovers_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_604).with_peer(ANN);
    s.world.add_identity(NEW);
    let mut new = s.world.peer(NEW);
    let mut ann = s.world.peer(ANN);
    new.send(&Letter::text(ME, 0x41, "synthetic text"));
    ann.send(&Letter::text(ME, 0x42, "synthetic text"));
    new.send(&Letter::text(ME, 0x43, "synthetic second"));
    let m = s.world.mediator().clone();
    s.world.net().directory().knobs(|k| k.status = Some(429));
    let warns = Warns::install_info();
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    let first = h.wait_connected().await;
    wait_acks(&h, first, 1).await;
    let events = h.ack_until("Online as leader", online_as("leader")).await;
    assert_eq!(messages(&events), 1, "ANN's text");
    assert_eq!(h.net().directory().asked().len(), 1, "not one lookup");
    assert_eq!(m.csp_queue_len(ME), 2, "the held texts left the server");
    // The second socket: the directory cannot be reached.
    h.net().directory().knobs(|k| {
        k.status = None;
        k.fail = Some(kourier_threema::NetError::Failed("https"));
    });
    let down = |l: &Line| l.message == HELD && l.field("rule") == Some("directory-down");
    let held = || warns.lines().iter().filter(|l| down(l)).count();
    advance_until(&h, "the second socket's holds", || {
        h.net().opened().len() == 2 && held() == 4
    })
    .await;
    assert!(h.now() >= at(60), "recycled before 60 s");
    let second = h.net().opened()[1];
    assert_eq!(h.net().directory().asked().len(), 2, "not one lookup more");
    // The third socket: the directory holds the request past the lookup's 30 s.
    h.net().directory().knobs(|k| k.fail = None);
    h.net().hold_directory(true);
    advance_until(&h, "the third socket's lookup", || {
        h.net().opened().len() == 3 && h.net().directory_held() == 1
    })
    .await;
    let third = h.net().opened()[2];
    advance_until(&h, "the third socket's holds", || held() == 6).await;
    // The fourth socket: the directory answers.
    h.net().hold_directory(false);
    advance_until(&h, "the fourth socket's acks", || {
        h.net().opened().len() == 4
            && traced(
                &h.s.world,
                h.net().opened()[3],
                "in",
                "csp-incoming-message-ack",
            ) == 2
    })
    .await;
    let fourth = h.net().opened()[3];
    assert!(h.now() <= at(300), "the sockets waited for a backoff");
    assert_eq!(m.csp_queue_len(ME), 0, "a text is still queued");
    assert_eq!(kourier_transactions(&h, fourth), (1, 0, 1), "the create");
    for conn in [first, second, third] {
        assert_eq!(
            traced(&h.s.world, conn, "in", "csp-incoming-message-ack"),
            usize::from(conn == first),
            "a held text was acked"
        );
        assert_eq!(kourier_transactions(&h, conn), (0, 0, 0), "a transaction");
    }
    let ends = wait_lines(&warns, 3, count_line("end")).await;
    for (end, conn) in ends.iter().zip([first, second, third]) {
        assert_eq!(
            (end.field("held"), end.field("held_by")),
            (Some("2"), Some("directory-down:2")),
            "the socket's holds"
        );
        assert_eq!(counts_of(end)["out_message_ack"], u64::from(conn == first));
        equals_the_trace(&h, conn, end);
    }
    assert!(
        !matches!(h.state(), ConnState::NeedsUserAction { .. }),
        "the account holds"
    );
    let core = core_until(&mut h, |e| messages(e) == 2).await;
    assert_eq!(contacts(&core), 1, "the contact reached the core");
    h.finish().await;
}

#[test]
fn a_sender_the_directory_does_not_know_is_discarded_and_acked() {
    support::offline::run_child(
        module_path!(),
        "a_sender_the_directory_does_not_know_is_discarded_and_acked_child",
    );
}

/// Step 5: the directory leaves the sender out of its answer (never existed or revoked): the text is discarded and
/// ACKED (no nonce, nothing reflected, no contact, no transaction); an invalid answer is not cached, so the sender's
/// next text is looked up again and discarded the same way.
#[tokio::test]
async fn a_sender_the_directory_does_not_know_is_discarded_and_acked_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_605).with_peer(ANN);
    s.world.add_identity(NEW);
    let mut new = s.world.peer(NEW);
    new.send(&Letter::text(ME, 0x51, "synthetic text"));
    new.send(&Letter::text(ME, 0x52, "synthetic second"));
    s.world.net().directory().knobs(|k| {
        k.omit.insert(NEW.to_owned());
    });
    let m = s.world.mediator().clone();
    m.declare_discard(NEW, 0x51);
    m.declare_discard(NEW, 0x52);
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    let conn = h.wait_connected().await;
    wait_acks(&h, conn, 2).await;
    h.ack_until("Online as leader", online_as("leader")).await;
    assert_eq!(h.net().directory().asked().len(), 2, "not two lookups");
    assert_eq!(m.csp_queue_len(ME), 0, "a discarded text stayed");
    assert!(m.reflects().is_empty(), "a discarded text was reflected");
    assert_eq!(kourier_transactions(&h, conn), (0, 0, 0), "a transaction");
    assert_eq!(h.file_rows("contact"), 1, "a contact was made");
    assert_eq!(h.file_rows("csp_inbound"), 0, "a discarded text has a row");
    assert_eq!(
        h.file_rows("csp_nonce"),
        0,
        "a step-5 discard wrote its nonce"
    );
    h.finish().await;
}

/// NEW's `group-setup` of `group_id` with `members`.
fn news_setup(id: u64, group_id: u64, members: &[&str]) -> Letter {
    Letter::new(
        ME,
        id,
        ty::GROUP_SETUP,
        &body::group_creator_container(group_id, &body::group_setup(members)),
    )
}

/// A text in NEW's group [`NEWS`].
fn text_in_news(id: u64) -> Letter {
    Letter::new(
        ME,
        id,
        ty::GROUP_TEXT,
        &body::group_member_container(NEW, NEWS, &body::text("synthetic group text")),
    )
}

/// The rules of the DEBUG discard lines, in order.
fn discarded_by(warns: &Warns) -> Vec<String> {
    warns
        .lines()
        .iter()
        .filter(|l| l.message == DISCARDED)
        .filter_map(|l| l.field("rule").map(|r| r.trim_matches('"').to_owned()))
        .collect()
}

#[test]
fn a_strangers_setup_held_for_the_directory_holds_a_contacts_text_in_its_group() {
    support::offline::run_child(
        module_path!(),
        "a_strangers_setup_held_for_the_directory_holds_a_contacts_text_in_its_group_child",
    );
}

/// TH6 FIX-A, S-B1 (ITS MUTANT, the blind mark never set, dies here): the directory answers 503. The stranger NEW's
/// `group-setup` (his group, with the user and ANN) is HELD `directory-down`: a BLIND hold, the box was never opened and
/// its group is unknown to the steps. ANN, a stored contact, then writes in that group, which the file does not hold:
/// the Common Group Receive Steps would discard her text and ACK it (`group-unknown`), lost for every device although
/// the setup that makes the group waits at the server; the socket's blind mark holds it `group-held-before`. Nothing is
/// acked on the first socket. The directory back and the socket recycled (T-H3, 60 s after the first hold), the second
/// socket holds the setup for the phone (`group-sync-needed`: a real change, no contact transaction) and ANN's text
/// behind it (`group-held-before`, T-B1 by its group). Nothing is acked or reflected, no job and no contact is made,
/// both stay at the server; the guard oracle runs in `finish`.
#[tokio::test]
async fn a_strangers_setup_held_for_the_directory_holds_a_contacts_text_in_its_group_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_606).with_peer(ANN);
    s.world.add_identity(NEW);
    let mut new = s.world.peer(NEW);
    let mut ann = s.world.peer(ANN);
    new.send(&news_setup(0x71, NEWS, &[ME, ANN]));
    ann.send(&text_in_news(0x72));
    s.world.net().directory().knobs(|k| k.status = Some(503));
    let m = s.world.mediator().clone();
    let warns = Warns::install_info();
    let h = harness::start(s);
    h.send(Command::Connect).await;
    let first = h.wait_connected().await;
    let held = wait_lines(&warns, 2, |l: &Line| l.message == HELD).await;
    let rules: Vec<&str> = held.iter().filter_map(|l| l.field("rule")).collect();
    assert_eq!(
        rules,
        ["directory-down", "group-held-before"],
        "the first socket's holds"
    );
    h.sync().await;
    assert_eq!(
        traced(&h.s.world, first, "in", "csp-incoming-message-ack"),
        0,
        "an ack on the first socket"
    );
    assert_eq!(m.csp_queue_len(ME), 2, "a message left the server");
    assert_eq!(h.net().directory().asked().len(), 1, "not one lookup");
    // The directory answers again; the socket is recycled 60 s after its first hold.
    h.net().directory().knobs(|k| k.status = None);
    let holds = || warns.lines().iter().filter(|l| l.message == HELD).count();
    advance_until(&h, "the second socket's holds", || {
        h.net().opened().len() == 2 && holds() == 4
    })
    .await;
    let second = h.net().opened()[1];
    let rules: Vec<String> = warns
        .lines()
        .iter()
        .filter(|l| l.message == HELD)
        .skip(2)
        .filter_map(|l| l.field("rule").map(str::to_owned))
        .collect();
    assert_eq!(
        rules,
        ["group-sync-needed", "group-held-before"],
        "the second socket's holds"
    );
    assert!(h.net().close_device(DEVICE, 4000), "no socket to close");
    let ends = wait_lines(&warns, 2, count_line("end")).await;
    for (end, conn, held_by) in [
        (&ends[0], first, "directory-down:1,group-held-before:1"),
        (&ends[1], second, "group-held-before:1,group-sync-needed:1"),
    ] {
        assert_eq!(
            (end.field("held"), end.field("held_by")),
            (Some("2"), Some(held_by)),
            "the socket's holds"
        );
        assert_eq!(counts_of(end)["out_message_ack"], 0, "an ack");
        equals_the_trace(&h, conn, end);
        assert_eq!(kourier_transactions(&h, conn), (0, 0, 0), "a transaction");
    }
    assert_eq!(m.csp_queue_len(ME), 2, "a message left the server");
    assert!(m.reflects().is_empty(), "a held message was reflected");
    assert_eq!(h.file_rows("send_task"), 0, "a job was written");
    assert_eq!(h.file_rows("contact"), 1, "a contact was made");
    assert_eq!(h.file_rows("csp_inbound"), 0, "a held message has a row");
    h.finish().await;
}

#[test]
fn a_strangers_group_control_is_discarded_at_22_4_1_and_a_strangers_setup_makes_the_contact_first()
{
    support::offline::run_child(
        module_path!(),
        "a_strangers_group_control_is_discarded_at_22_4_1_and_a_strangers_setup_makes_the_contact_first_child",
    );
}

/// TH6 FIX-A, P-L1: the stranger NEW (the directory knows him) sends a `group-leave` of a group of ANN's the file does
/// not hold and a `group-name` of his own unknown group: both create no implicit direct contact, so the spec's 22.4.1
/// discards them and acks them with NO job (before the fix the Common Group Receive Steps wrote a `group-sync-request`
/// to ANN and to NEW for each: a stranger could fill the job budget). Then NEW's `group-setup` WITHOUT the user for a
/// group the file does not know: the type creates the contact (22.4.2, one `CONTACT_SYNC` transaction), then the setup's
/// own step 4.1 discards it, acked. One lookup, one contact, no row of the send queue, nothing reflected but the
/// transaction's `ContactSync`.
#[tokio::test]
async fn a_strangers_group_control_is_discarded_at_22_4_1_and_a_strangers_setup_makes_the_contact_first_child()
 {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_607).with_peer(ANN);
    s.world.add_identity(NEW);
    let mut new = s.world.peer(NEW);
    new.send(
        &Letter::new(
            ME,
            0x81,
            ty::GROUP_LEAVE,
            &body::group_member_container(ANN, ANNS_UNKNOWN, &[]),
        )
        .discarded(),
    );
    new.send(
        &Letter::new(
            ME,
            0x82,
            ty::GROUP_NAME,
            &body::group_creator_container(NEWS, &body::group_name("synthetic name")),
        )
        .discarded(),
    );
    new.send(&news_setup(0x83, NEWS + 1, &[ANN]).discarded());
    let mut phone = s.world.phone(&s.dgk, SERVER_GROUP, PHONE);
    phone.connect();
    phone.disconnect();
    let m = s.world.mediator().clone();
    let warns = Warns::install_info_and_debug_of("kourier_threema");
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    let conn = h.wait_connected().await;
    wait_acks(&h, conn, 3).await;
    let events = h.ack_until("Online as leader", online_as("leader")).await;
    assert_eq!(
        (contacts(&events), messages(&events)),
        (1, 0),
        "the core's contact and messages"
    );
    assert_eq!(
        discarded_by(&warns),
        [
            "csp-sender-unknown",
            "csp-sender-unknown",
            "group-setup-not-member"
        ],
        "the discards' rules"
    );
    assert_eq!(h.net().directory().asked().len(), 1, "not one lookup");
    assert_eq!(
        kourier_transactions(&h, conn),
        (1, 0, 1),
        "the setup's contact transaction"
    );
    scopes_are_contact_sync(&h);
    assert_eq!(h.file_rows("contact"), 2, "NEW is not a contact");
    assert_eq!(h.file_rows("send_task"), 0, "a job was written");
    assert_eq!(m.reflects().len(), 1, "not the ContactSync alone");
    assert_eq!(m.csp_queue_len(ME), 0, "a message stayed");
    phone.connect();
    phone.incoming();
    let view = phone.view();
    assert_eq!(
        synced(&view.contacts()),
        [(
            true,
            NEW.to_owned(),
            Some(String::new()),
            Some(AcquaintanceLevel::Direct as i32)
        )],
        "the create the phone viewed (no nickname given: L5's create states it empty)"
    );
    assert!(view.incoming_ids().is_empty(), "a discard was reflected");
    phone.disconnect();
    h.finish().await;
}

#[test]
fn a_recycle_for_room_with_nothing_acked_waits_one_backoff_step() {
    support::offline::run_child(
        module_path!(),
        "a_recycle_for_room_with_nothing_acked_waits_one_backoff_step_child",
    );
}

/// TH6 FIX-B, P-L6 (critic T-H2's recycle, the case without progress): 300 texts of a sender the file does not know wait
/// while the directory holds the first lookup past its 30 s. The head waits, so the FIFO fills and the boxes over its
/// bound are HELD (`fifo-full`); when the lookup gives up, the other unknown senders are held without a call
/// (`directory-down`): NOT ONE message is acked. The socket is recycled for the refused boxes, but with nothing acked
/// the next socket comes only after ONE backoff step (+5 s of the actor's clock), not at once, and not counted: the
/// second recycle is again one step (+5 s), never the doubled wait of a counted failure. (A recycle after an ack stays at
/// once: `a_backlog_of_300_is_held_over_the_fifo_...` in `leader.rs` pins that half with no time passing.)
#[tokio::test]
async fn a_recycle_for_room_with_nothing_acked_waits_one_backoff_step_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_605);
    s.world.add_identity(NEW);
    let mut new = s.world.peer(NEW);
    for n in 0..300_u64 {
        new.send(&Letter::text(ME, 0x1000 + n, "synthetic backlog"));
    }
    let m = s.world.mediator().clone();
    assert_eq!(m.csp_queue_len(ME), 300);
    let warns = Warns::install_info();
    let h = harness::start(s);
    h.net().hold_directory(true);
    h.send(Command::Connect).await;
    let first = h.wait_connected().await;
    let waiting = |h: &H| {
        matches!(
            h.state(),
            ConnState::Offline {
                retry_at_ms: Some(_)
            }
        )
    };
    advance_until(&h, "the first socket's end", || waiting(&h)).await;
    let ConnState::Offline {
        retry_at_ms: Some(retry),
    } = h.state()
    else {
        panic!("not waiting");
    };
    let closed_at = h.now();
    assert_eq!(retry, closed_at + 5_000, "not one backoff step");
    assert!(
        closed_at >= at(30) && closed_at < at(60),
        "the lookup's end, not T-H3's 60 s"
    );
    assert_eq!(h.net().opened().len(), 1, "a second socket at once");
    let ends = wait_lines(&warns, 1, count_line("end")).await;
    let counted = counts_of(&ends[0]);
    assert_eq!(
        (
            counted["in_csp_message"],
            counted["out_reflect_inbound"],
            counted["out_message_ack"]
        ),
        (300, 0, 0),
        "the first socket's frames"
    );
    assert_eq!(ends[0].field("held"), Some("300"), "every message held");
    let held_by = ends[0].field("held_by").unwrap_or_default();
    assert!(
        held_by.contains("fifo-full:") && held_by.contains("directory-down:"),
        "the holds: {held_by}"
    );
    equals_the_trace(&h, first, &ends[0]);
    // The second socket, one step later; it is recycled the same way and waits the same single step.
    advance_until(&h, "the second socket", || h.net().opened().len() == 2).await;
    assert_eq!(h.now(), retry, "the second socket's time");
    advance_until(&h, "the second socket's end", || {
        waiting(&h) && h.now() > retry
    })
    .await;
    let ConnState::Offline {
        retry_at_ms: Some(again),
    } = h.state()
    else {
        panic!("not waiting");
    };
    assert_eq!(
        again,
        h.now() + 5_000,
        "a counted wait after the second recycle"
    );
    assert_eq!(h.net().opened().len(), 2, "a third socket at once");
    no_id(&warns);
    h.finish().await;
}

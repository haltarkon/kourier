//! The fake mediator's TRANSACTIONS (TH6 L2b, `support/mock/txn.rs`, `TP/md.md` "Transactions", `TP/md-d2m.proto`
//! 0x40-0x45) against the REAL patched libthreema D2M client ("Kourier") and the fake's phone: one lock per device
//! group, `TransactionRejected` with the holder's device and scope, the holder's reflects acked at once and released
//! at the commit, `TransactionEnded` to the others, a commit with nothing held (the precondition that fails after
//! `BeginTransactionAck`), the holder's end that drops its reflects, the TTL's 4011, the scopes opened with DGTSK and
//! the violations. No runtime, no `unshare`. Every test ends with the violations check.

use std::time::Duration;

use libthreema::common::D2xDeviceId;
use libthreema::d2m::payload::{
    BeginTransaction, IncomingPayload, OutgoingPayload, Reflect, ReflectFlags,
};
use libthreema::protobuf::d2d;
use libthreema::protobuf::d2d::transaction_scope::Scope;
use prost::Message as _;

use super::d2m::{Client, Group, SERVER_GROUP, existing, group, new_device, online, pump, send};
use crate::support::mock::crypto::seal_nonce_ahead;
use crate::support::mock::d2m::{DEFAULT_MAX_TRANSACTION_TTL, TRANSACTION_TTL_REACHED};
use crate::support::mock::envelope::{Author, ContactDelta};
use crate::support::mock::phone::{Phone, PhoneEvent};
use crate::support::mock::txn::{TxnSeen, seal_scope};
use crate::support::mock::{MockWorld, world};

/// Kourier's device id and the phone's.
const KOURIER: u64 = 0x0A11_0000_0000_0001;
const PHONE: u64 = 0x0A11_0000_0000_0002;
/// The contact a `ContactSync` names.
const CONTACT: &str = "CONTACT1";

/// A world, its group, the phone online (it leads: the first device) and Kourier online (a follower).
fn scene(seed: u64) -> (MockWorld, Group, Phone, Client) {
    let w = world(seed);
    let g = group(&w);
    let mut phone = w.phone(&g.dgk, SERVER_GROUP, PHONE);
    phone.connect();
    let mut k = online(&w, &g, new_device(KOURIER));
    k.payloads.clear();
    phone.take_events();
    (w, g, phone, k)
}

/// Kourier begins a transaction of `scope` with `ttl` (`None`: the server's maximum).
fn begin(w: &MockWorld, g: &Group, k: &mut Client, scope: Scope, ttl: Option<Duration>) {
    let encrypted_scope = seal_scope(&g.keys, &w.random(), scope);
    send(
        w,
        k,
        OutgoingPayload::BeginTransaction(BeginTransaction {
            encrypted_scope,
            ttl,
        }),
    );
}

/// Kourier reflects a `ContactSync` (sealed with DGRK); the sealed bytes.
fn reflect_contact(
    w: &MockWorld,
    g: &Group,
    k: &mut Client,
    reflect_id: u32,
    nick: &str,
) -> Vec<u8> {
    let envelope: d2d::Envelope =
        Author::new(KOURIER).contact_update(ContactDelta::new(CONTACT).nickname(nick));
    let sealed = seal_nonce_ahead(&g.keys.reflect, &w.random(), &envelope.encode_to_vec());
    send(
        w,
        k,
        OutgoingPayload::Reflect(Reflect {
            flags: ReflectFlags(0),
            reflect_id,
            envelope: sealed.clone(),
        }),
    );
    sealed
}

/// What Kourier got since the last call, as short names (devices and reflect ids as numbers).
fn got(k: &mut Client) -> Vec<String> {
    std::mem::take(&mut k.payloads)
        .into_iter()
        .map(|p| match p {
            IncomingPayload::BeginTransactionAck => "begin-ack".to_owned(),
            IncomingPayload::CommitTransactionAck => "commit-ack".to_owned(),
            IncomingPayload::TransactionRejected(r) => {
                let D2xDeviceId(d) = r.device_id;
                format!("rejected:{d:#x}")
            }
            IncomingPayload::TransactionEnded(e) => {
                let D2xDeviceId(d) = e.device_id;
                format!("ended:{d:#x}")
            }
            IncomingPayload::ReflectAck(a) => format!("reflect-ack:{}", a.reflect_id),
            IncomingPayload::Reflected(_) => "reflected".to_owned(),
            IncomingPayload::RolePromotedToLeader => "promoted".to_owned(),
            _ => "other".to_owned(),
        })
        .collect()
}

/// The mediator's record of the transactions as (connection's device, what) pairs.
fn record(w: &MockWorld) -> Vec<(u64, String)> {
    w.mediator()
        .transactions()
        .into_iter()
        .map(|t| match t {
            TxnSeen::Begun { device, ttl_ms, .. } => (device, format!("begun {ttl_ms}")),
            TxnSeen::Rejected { device, holder, .. } => (device, format!("rejected {holder:#x}")),
            TxnSeen::Held { reflect_id, .. } => (0, format!("held {reflect_id}")),
            TxnSeen::Ended {
                device,
                how,
                released,
                dropped,
                ..
            } => (device, format!("ended {how:?} {released} {dropped}")),
        })
        .collect()
}

/// Kourier holds the group's lock: its `ContactSync` reflect is acked at once and reaches NOBODY until the commit; the
/// phone's begin meanwhile is rejected with Kourier's device and Kourier's scope; at the commit the reflection goes to
/// the phone first, then `TransactionEnded`; the phone then takes the lock, and its commit tells Kourier. Every scope
/// opens with DGTSK as what was sealed.
#[test]
fn a_transaction_holds_its_reflects_until_the_commit_and_rejects_another_begin_meanwhile() {
    let (w, g, mut phone, mut k) = scene(1301);
    let m = w.mediator();
    begin(&w, &g, &mut k, Scope::ContactSync, None);
    assert_eq!(got(&mut k), vec!["begin-ack"], "the lock");
    assert_eq!(m.transaction_holder(&g.keys.path_public), Some(KOURIER));
    let sealed = reflect_contact(&w, &g, &mut k, 1, "Pat");
    assert_eq!(got(&mut k), vec!["reflect-ack:1"], "acked at once [U]");
    assert!(
        phone.take_reflected().is_empty(),
        "nothing before the commit"
    );
    assert_eq!(
        m.queue_len(&g.keys.path_public, PHONE),
        Some(0),
        "not queued"
    );

    assert_eq!(
        phone.begin_transaction(Scope::GroupSync, 0),
        PhoneEvent::TxnRejected { holder: KOURIER },
        "one transaction at a time per group"
    );
    let rejected = m.transactions();
    let Some(TxnSeen::Begun {
        scope: kourier_scope,
        ..
    }) = rejected.first()
    else {
        panic!("Kourier's begin is not the first record");
    };
    assert!(
        rejected
            .iter()
            .any(|t| matches!(t, TxnSeen::Rejected { holder, .. } if *holder == KOURIER)),
        "the rejection is recorded"
    );
    assert!(!kourier_scope.is_empty(), "the holder's scope is kept");

    send(&w, &mut k, OutgoingPayload::CommitTransaction);
    assert_eq!(got(&mut k), vec!["commit-ack"], "the commit");
    let events = phone.take_events();
    let at_reflected = events
        .iter()
        .position(|e| matches!(e, PhoneEvent::Reflected { .. }));
    let at_ended = events
        .iter()
        .position(|e| *e == PhoneEvent::TxnEnded { holder: KOURIER });
    assert!(
        at_reflected.is_some() && at_ended.is_some() && at_reflected < at_ended,
        "the reflection, then TransactionEnded: {events:?}"
    );
    let reflected = phone.take_reflected();
    assert!(
        reflected.len() == 1 && reflected[0].1 == sealed,
        "the held reflection byte for byte"
    );
    assert_eq!(m.transaction_holder(&g.keys.path_public), None, "released");

    assert_eq!(
        phone.begin_transaction(Scope::GroupSync, 0),
        PhoneEvent::TxnBegun,
        "the phone takes the lock now"
    );
    pump(m, &mut k);
    assert!(
        got(&mut k).is_empty(),
        "nothing to Kourier at another's begin"
    );
    assert!(phone.commit_transaction(), "the phone's commit");
    pump(m, &mut k);
    assert_eq!(got(&mut k), vec![format!("ended:{PHONE:#x}")]);

    let max = DEFAULT_MAX_TRANSACTION_TTL.as_millis();
    assert_eq!(
        record(&w),
        vec![
            (KOURIER, format!("begun {max}")),
            (0, "held 1".to_owned()),
            (PHONE, format!("rejected {KOURIER:#x}")),
            (KOURIER, "ended Committed 1 0".to_owned()),
            (PHONE, format!("begun {max}")),
            (PHONE, "ended Committed 0 0".to_owned()),
        ]
    );
    assert_eq!(
        w.transaction_scopes(&g.keys),
        vec![
            Some(Scope::ContactSync),
            Some(Scope::GroupSync),
            Some(Scope::GroupSync)
        ],
        "every scope opens with DGTSK"
    );
    assert!(
        !k.failed && k.closed.is_none(),
        "the client took every answer"
    );
    w.violations().assert_empty();
}

/// T-L2 (`TP/md.md:96-98`, case 3): the precondition fails after `BeginTransactionAck`, so the holder commits with
/// NOTHING held: acked, the other device is told `TransactionEnded` (its retry signal) and gets no reflection, and its
/// own begin goes through.
#[test]
fn a_commit_with_nothing_held_ends_the_transaction_gracefully() {
    let (w, g, mut phone, mut k) = scene(1302);
    let m = w.mediator();
    begin(&w, &g, &mut k, Scope::ContactSync, None);
    assert_eq!(
        phone.begin_transaction(Scope::ContactSync, 0),
        PhoneEvent::TxnRejected { holder: KOURIER }
    );
    send(&w, &mut k, OutgoingPayload::CommitTransaction);
    assert_eq!(got(&mut k), vec!["begin-ack", "commit-ack"]);
    assert_eq!(
        phone.take_events(),
        vec![PhoneEvent::TxnEnded { holder: KOURIER }],
        "the retry signal and nothing else"
    );
    assert_eq!(
        phone.begin_transaction(Scope::ContactSync, 0),
        PhoneEvent::TxnBegun
    );
    assert!(phone.commit_transaction());
    assert_eq!(
        m.queue_len(&g.keys.path_public, KOURIER),
        Some(0),
        "no reflection"
    );
    w.violations().assert_empty();
}

/// THE HOLDER'S END: Kourier's connection closes while it holds a transaction with two reflects inside: the reflects
/// are DROPPED [U], the phone is told `TransactionEnded` and gets nothing; a LOST holder (1006) ends it the same way;
/// the lock is free afterwards.
#[test]
fn the_holders_end_drops_its_held_reflects_and_tells_the_others() {
    let (w, g, mut phone, mut k) = scene(1303);
    let m = w.mediator();
    begin(&w, &g, &mut k, Scope::ContactSync, None);
    reflect_contact(&w, &g, &mut k, 1, "Pat");
    reflect_contact(&w, &g, &mut k, 2, "Sam");
    assert_eq!(
        got(&mut k),
        vec!["begin-ack", "reflect-ack:1", "reflect-ack:2"]
    );
    m.client_close(k.conn(), 1000);
    assert_eq!(
        phone.take_events(),
        vec![PhoneEvent::TxnEnded { holder: KOURIER }],
        "told, and no reflection"
    );
    assert_eq!(m.queue_len(&g.keys.path_public, PHONE), Some(0), "dropped");
    assert_eq!(m.transaction_holder(&g.keys.path_public), None);

    // The phone holds one and its connection is lost: Kourier, back online, is told.
    let mut k = online(&w, &g, existing(KOURIER));
    k.payloads.clear();
    assert_eq!(
        phone.begin_transaction(Scope::GroupSync, 0),
        PhoneEvent::TxnBegun
    );
    m.lost(phone.conn().expect("connected"));
    pump(m, &mut k);
    assert_eq!(
        got(&mut k),
        vec![format!("ended:{PHONE:#x}"), "promoted".to_owned()],
        "told first, then promoted: the phone led"
    );
    begin(&w, &g, &mut k, Scope::ContactSync, None);
    assert_eq!(got(&mut k), vec!["begin-ack"], "the lock is free");
    send(&w, &mut k, OutgoingPayload::CommitTransaction);
    assert_eq!(
        record(&w)
            .into_iter()
            .filter(|(_, what)| what.starts_with("ended"))
            .collect::<Vec<_>>(),
        vec![
            (KOURIER, "ended Disconnected 0 2".to_owned()),
            (PHONE, "ended Disconnected 0 0".to_owned()),
            (KOURIER, "ended Committed 0 0".to_owned()),
        ]
    );
    w.violations().assert_empty();
}

/// THE TTL: Kourier's 5 s transaction closes it with 4011 at 5 s (not at 4) and the phone is told; `ttl = 0` is the
/// server's maximum (the phone closed 4011 after 60 s); a TTL over the maximum is cut to it.
#[test]
fn the_ttl_closes_the_holder_with_4011_and_zero_is_the_servers_maximum() {
    let (w, g, mut phone, mut k) = scene(1304);
    let m = w.mediator();
    begin(
        &w,
        &g,
        &mut k,
        Scope::ContactSync,
        Some(Duration::from_secs(5)),
    );
    m.advance(Duration::from_secs(4));
    pump(m, &mut k);
    assert!(k.closed.is_none(), "not before its TTL");
    m.advance(Duration::from_secs(1));
    pump(m, &mut k);
    assert_eq!(k.closed, Some(TRANSACTION_TTL_REACHED), "4011 at the TTL");
    assert_eq!(
        phone.take_events(),
        vec![PhoneEvent::TxnEnded { holder: KOURIER }]
    );

    assert_eq!(
        phone.begin_transaction(Scope::GroupSync, 0),
        PhoneEvent::TxnBegun
    );
    m.advance(DEFAULT_MAX_TRANSACTION_TTL - Duration::from_secs(1));
    assert!(phone.take_events().is_empty(), "still held");
    m.advance(Duration::from_secs(1));
    assert_eq!(
        phone.take_events(),
        vec![PhoneEvent::Closed(TRANSACTION_TTL_REACHED)],
        "the server's maximum"
    );

    m.knobs(|k| k.max_transaction_ttl = Duration::from_secs(10));
    let mut k = online(&w, &g, existing(KOURIER));
    begin(
        &w,
        &g,
        &mut k,
        Scope::ContactSync,
        Some(Duration::from_secs(3600)),
    );
    let last = record(&w).pop().expect("a record");
    assert_eq!(
        last,
        (KOURIER, "begun 10000".to_owned()),
        "cut to the maximum"
    );
    assert_eq!(
        record(&w)
            .into_iter()
            .filter(|(_, what)| what.starts_with("ended"))
            .map(|(_, what)| what)
            .collect::<Vec<_>>(),
        vec!["ended Ttl 0 0", "ended Ttl 0 0"]
    );
    w.violations().assert_empty();
}

/// What the client does wrong: a commit without a transaction (4010), a second begin of the holder (4010; its
/// transaction ends with the close), a scope that does not open with DGTSK (the mediator cannot tell; the test side's
/// open names it).
#[test]
fn a_commit_without_a_transaction_a_second_begin_and_an_unopened_scope_are_violations() {
    let (w, g, mut phone, mut k) = scene(1305);
    let m = w.mediator();
    send(&w, &mut k, OutgoingPayload::CommitTransaction);
    assert_eq!(k.closed, Some(4010), "a commit without a transaction");

    let mut k = online(&w, &g, existing(KOURIER));
    begin(&w, &g, &mut k, Scope::ContactSync, None);
    begin(&w, &g, &mut k, Scope::ContactSync, None);
    assert_eq!(k.closed, Some(4010), "a second begin of the holder");
    assert!(
        phone
            .take_events()
            .contains(&PhoneEvent::TxnEnded { holder: KOURIER }),
        "its transaction ended with the close"
    );

    let mut k = online(&w, &g, existing(KOURIER));
    k.payloads.clear();
    let garbage: [u8; 48] = w.random();
    send(
        &w,
        &mut k,
        OutgoingPayload::BeginTransaction(BeginTransaction {
            encrypted_scope: garbage.to_vec(),
            ttl: None,
        }),
    );
    assert_eq!(
        got(&mut k),
        vec!["begin-ack"],
        "the mediator cannot open it"
    );
    assert_eq!(
        w.transaction_scopes(&g.keys),
        vec![Some(Scope::ContactSync), None]
    );
    assert_eq!(m.transaction_holder(&g.keys.path_public), Some(KOURIER));
    assert_eq!(
        w.violations().rules(),
        vec![
            "d2m-commit-without-transaction",
            "d2m-begin-while-holding",
            "d2m-transaction-scope-unopened"
        ]
    );
}

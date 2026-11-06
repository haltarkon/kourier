//! TH6 L2a: what the fake RECORDS of a leader's work and THE GUARD ORACLE over it. Rule 9 holds: the
//! generated protobuf types and the fake's own crypto only, nothing of Kourier's leader, follower,
//! send, join or proto code.
//!
//! The mediator (which holds the chat server) keeps one ordered list of the moments that matter to
//! the guard ([`Recorded`]): every incoming message the chat server sent on a connection, every
//! `message-ack` a client sent for one, every `Reflect` the mediator took and every `ReflectAck` it
//! sent. The order is the order under the mediator's lock, i.e. the order in which the frames were
//! handled. It also keeps what the peers POSTED to an identity ([`Posted`], `Peer::send`) and the
//! messages a test DECLARED a spec discard (`Mediator::declare_discard`).
//!
//! THE GUARD ORACLE ([`MockWorld::leader_violations`]): for every `message-ack` a client (any device
//! but the phone) sent, the message must have a reflected `IncomingMessage` (opened with DGRK, sent
//! by a device that is not the phone) whose `ReflectAck` went out BEFORE that `message-ack`, or be
//! declared a discard; else `leader-acked-unreflected` (no reflect at all) or
//! `leader-ack-before-reflect-ack` (a reflect, but its ack came later or never). Two reflects of one
//! (sender, message id) under different D2D nonces are `leader-reflected-twice` (a resend must be the
//! stored bytes); a reflected `IncomingMessage` whose type, body or CSP nonce is not what the peer
//! posted, or whose `created_at` is neither the metadata's time nor a clamp of a future one, is
//! `leader-reflect-differs`.
//!
//! A TYPING INDICATOR (TH6 FIX-A, S-L3 = P-L2) is reflected EPHEMERALLY and the mediator acks no ephemeral reflect
//! (`md-d2m.struct.yml:217-221`, "An acknowledgement must not be sent"), so its `message-ack` (owed when the flags lack
//! `0x04`) is right after an ephemeral reflect of its `IncomingMessage` of type `typing-indicator` that went BEFORE
//! the ack (compared with the posted letter as any reflect); an ephemeral reflect of any other type proves nothing.
//!
//! THE ORDER IS PROVED ONLY UNDER `Knobs::withhold_reflect_acks`: the fake acks a reflect at once, so
//! a client that sends the `message-ack` without waiting for the reflect-ack still shows it after
//! the reflect-ack in the list. A test that pins the order holds the reflect-acks back and releases
//! them (`Mediator::release_reflect_acks`) after it checked that no `message-ack` came.

use std::collections::{BTreeMap, BTreeSet};

use libthreema::protobuf::common::CspE2eMessageType;
use libthreema::protobuf::d2d;
use prost::Message as _;

use super::MockWorld;
use super::crypto::{DeviceGroupKeys, open_nonce_ahead};
use super::d2m::{ConnId, EPHEMERAL};

/// One moment of the leader's work, in the order the mediator handled it. `Debug` prints numbers
/// only (connections, message ids, reflect ids), never a sender.
#[derive(Clone, PartialEq, Eq)]
pub enum Recorded {
    /// The chat server sent an incoming message (`message-with-metadata-box`) on `conn`.
    Delivered {
        conn: u64,
        sender: [u8; 8],
        message_id: u64,
    },
    /// The client of `conn` sent a `message-ack` for an incoming message. `known`: the server had sent
    /// that message on this connection and waited for its ack (else the ack is also the chat
    /// server's violation `csp-ack-for-a-message-not-sent`).
    CspAck {
        conn: u64,
        sender: [u8; 8],
        message_id: u64,
        known: bool,
    },
    /// The mediator took a `Reflect` of `conn`; `index` is its place in `Mediator::reflects()`.
    Reflect {
        conn: u64,
        reflect_id: u32,
        index: usize,
    },
    /// The mediator sent the `ReflectAck` of `reflect_id` to the client of `conn` (at the reflect, or
    /// at `release_reflect_acks` under `withhold_reflect_acks`).
    ReflectAck { conn: u64, reflect_id: u32 },
}

impl std::fmt::Debug for Recorded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Delivered {
                conn, message_id, ..
            } => write!(f, "Delivered(conn {conn}, id {message_id})"),
            Self::CspAck {
                conn,
                message_id,
                known,
                ..
            } => write!(f, "CspAck(conn {conn}, id {message_id}, known {known})"),
            Self::Reflect {
                conn,
                reflect_id,
                index,
            } => write!(f, "Reflect(conn {conn}, id {reflect_id}, #{index})"),
            Self::ReflectAck { conn, reflect_id } => {
                write!(f, "ReflectAck(conn {conn}, id {reflect_id})")
            }
        }
    }
}

/// One message a peer posted to an identity (`Peer::send`), as the oracle compares the reflection with
/// it. `Debug` prints the message id, the type, the flags and the lengths only.
#[derive(Clone, PartialEq, Eq)]
pub struct Posted {
    /// The sender's Threema ID.
    pub sender: String,
    /// The identity whose queue holds it (the head may name another receiver: `Letter::wrong_receiver`).
    pub to: String,
    pub message_id: u64,
    /// The metadata's `created-at` in ms (the head's seconds x 1000 when the letter has no metadata).
    pub created_ms: u64,
    pub flags: u8,
    /// The container's type.
    pub ty: u8,
    /// The container's data without its padding (what `IncomingMessage.body` carries).
    pub body: Vec<u8>,
    /// The CSP nonce of the boxes (what `IncomingMessage.nonce` carries).
    pub nonce: [u8; 24],
    /// The whole `message-with-metadata-box` struct as the chat server queued it.
    pub bytes: Vec<u8>,
    /// The test declared that the receiver may ack it without a reflect (a spec discard).
    pub discard: bool,
    /// Whether the chat server queued it (a "no server queuing" message with nobody unblocked is
    /// dropped).
    pub queued: bool,
}

impl std::fmt::Debug for Posted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Posted(id {}, type {:#04x}, flags {:#04x}, body {} B, {} B, discard {}, queued {})",
            self.message_id,
            self.ty,
            self.flags,
            self.body.len(),
            self.bytes.len(),
            self.discard,
            self.queued
        )
    }
}

/// What the mediator keeps for the oracle (part of its state; `Mediator::leader_log` hands a copy).
#[derive(Clone, Default)]
pub struct LeaderLog {
    pub events: Vec<Recorded>,
    pub posted: Vec<Posted>,
    /// (sender, message id) a test declared a spec discard.
    pub declared: BTreeSet<([u8; 8], u64)>,
    /// The virtual time each `Reflect` was taken at, by its place in `Mediator::reflects()`.
    pub reflect_ms: Vec<u64>,
}

impl std::fmt::Debug for LeaderLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaderLog")
            .field("events", &self.events)
            .field("posted", &self.posted.len())
            .field("declared", &self.declared.len())
            .finish()
    }
}

/// One reflected `IncomingMessage` of the client, as the oracle found it.
struct Reflection {
    /// Its place in the record.
    at: usize,
    conn: u64,
    reflect_id: u32,
    /// The D2D nonce (the first 24 bytes of the sealed envelope).
    d2d_nonce: [u8; 24],
}

impl MockWorld {
    /// THE GUARD ORACLE (module doc): checks the record of the leader's work against the guard and
    /// records each breach in the world's violations (so the test's last check sees it), with the
    /// message id as its number. `keys` are the device group's (the phone's: the mediator never holds
    /// them), `phone` the phone's device id (its reflects and acks are the phone's business, not the
    /// client's). Returns the rules broken, in the order found. Calling it twice records twice.
    pub fn leader_violations(&self, keys: &DeviceGroupKeys, phone: u64) -> Vec<&'static str> {
        let m = self.mediator();
        let LeaderLog {
            events,
            posted,
            declared,
            reflect_ms,
        } = m.leader_log();
        let reflects = m.reflects();
        let device_of: BTreeMap<u64, Option<u64>> = events
            .iter()
            .map(|e| match e {
                Recorded::Delivered { conn, .. }
                | Recorded::CspAck { conn, .. }
                | Recorded::Reflect { conn, .. }
                | Recorded::ReflectAck { conn, .. } => *conn,
            })
            .collect::<BTreeSet<u64>>()
            .into_iter()
            .map(|conn| (conn, m.device_of(ConnId(conn))))
            .collect();
        let clients = |conn: &u64| device_of.get(conn).copied().flatten() != Some(phone);

        let mut found: Vec<(&'static str, u64)> = Vec::new();
        // The client's reflected IncomingMessages by (sender, message id).
        let mut reflected: BTreeMap<(String, u64), Vec<Reflection>> = BTreeMap::new();
        // The client's EPHEMERAL reflects of a typing indicator, by (sender, message id): their places in the record.
        let mut typing: BTreeMap<(String, u64), Vec<usize>> = BTreeMap::new();
        let mut acked_at: BTreeMap<(u64, u32), usize> = BTreeMap::new();
        for (at, event) in events.iter().enumerate() {
            match event {
                Recorded::Reflect {
                    conn,
                    reflect_id,
                    index,
                } if clients(conn) => {
                    let Some(seen) = reflects.get(*index) else {
                        continue;
                    };
                    if seen.bytes.len() < 24 {
                        continue;
                    }
                    let Some(plain) = open_nonce_ahead(&keys.reflect, &seen.bytes) else {
                        continue;
                    };
                    let Ok(envelope) = d2d::Envelope::decode(plain.as_slice()) else {
                        continue;
                    };
                    let Some(d2d::envelope::Content::IncomingMessage(message)) = envelope.content
                    else {
                        continue;
                    };
                    let key = (message.sender_identity.clone(), message.message_id);
                    let at_ms = reflect_ms.get(*index).copied().unwrap_or(0);
                    if differs(&posted, &message, at_ms) {
                        found.push(("leader-reflect-differs", message.message_id));
                    }
                    if seen.flags & EPHEMERAL != 0 {
                        if message.r#type == CspE2eMessageType::TypingIndicator as i32 {
                            typing.entry(key).or_default().push(at);
                        }
                        continue;
                    }
                    let d2d_nonce: [u8; 24] = seen.bytes[..24].try_into().expect("24 bytes");
                    let list = reflected.entry(key).or_default();
                    if list.iter().any(|r| r.d2d_nonce != d2d_nonce) {
                        found.push(("leader-reflected-twice", message.message_id));
                    }
                    list.push(Reflection {
                        at,
                        conn: *conn,
                        reflect_id: *reflect_id,
                        d2d_nonce,
                    });
                }
                Recorded::ReflectAck { conn, reflect_id } => {
                    acked_at.entry((*conn, *reflect_id)).or_insert(at);
                }
                Recorded::CspAck {
                    conn,
                    sender,
                    message_id,
                    ..
                } if clients(conn) => {
                    let key = (String::from_utf8_lossy(sender).into_owned(), *message_id);
                    if typing
                        .get(&key)
                        .is_some_and(|places| places.iter().any(|place| *place < at))
                    {
                        continue;
                    }
                    let list = reflected.get(&key);
                    let before = list.is_some_and(|list| {
                        list.iter().any(|r| {
                            r.at < at
                                && acked_at
                                    .get(&(r.conn, r.reflect_id))
                                    .is_some_and(|ack| *ack < at)
                        })
                    });
                    if before {
                        continue;
                    }
                    if list.is_some_and(|list| !list.is_empty()) {
                        found.push(("leader-ack-before-reflect-ack", *message_id));
                    } else if !declared.contains(&(*sender, *message_id))
                        && !posted.iter().any(|p| {
                            p.discard
                                && p.sender.as_bytes() == sender.as_slice()
                                && p.message_id == *message_id
                        })
                    {
                        found.push(("leader-acked-unreflected", *message_id));
                    }
                }
                _ => {}
            }
        }
        for (rule, message_id) in &found {
            self.violations().record(rule, &[("message", *message_id)]);
        }
        found.into_iter().map(|(rule, _)| rule).collect()
    }
}

/// Whether a reflected `IncomingMessage` differs from what the peer posted under the same sender, id
/// and CSP nonce: no such letter (a nonce or an id that no letter had), another type or body, or a
/// `created_at` that is neither the metadata's ms nor, for a letter dated after the reflect, a time
/// clamped below it [I: T-L3, "clamped to now as Desktop does"]. A message no peer posted (the
/// chat server's opaque test messages, `Mediator::csp_incoming`) is not compared.
fn differs(posted: &[Posted], message: &d2d::IncomingMessage, reflect_ms: u64) -> bool {
    let same_id: Vec<&Posted> = posted
        .iter()
        .filter(|p| p.sender == message.sender_identity && p.message_id == message.message_id)
        .collect();
    if same_id.is_empty() {
        return false;
    }
    let Some(letter) = same_id
        .iter()
        .find(|p| p.nonce.as_slice() == message.nonce.as_slice())
    else {
        return true;
    };
    let time_ok = message.created_at == letter.created_ms
        || (letter.created_ms > reflect_ms && message.created_at < letter.created_ms);
    i32::from(letter.ty) != message.r#type || letter.body != message.body || !time_ok
}

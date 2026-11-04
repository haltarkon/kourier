//! The fake CHAT SERVER (CSP) inside the mediator's Proxy payloads (TH2 M4): one [`CspConn`] per
//! D2M connection (the handshake C1-C5, the frames with strict counters C6, the payloads C7-C13) and
//! one [`ChatServer`] per world (the identities' incoming queues, acks, device cookies, what the
//! clients sent, the push-token calls). Written from the spec (`TP/csp.struct.yml`) with the fake's
//! own crypto; from libthreema nothing at all (rule 9). Where the spec and libthreema differ the
//! fake follows libthreema, which both of Threema's own clients agree with (S-mock §3.2):
//!
//! - `login-ack` carries `reserved (4) || current-time (u64-le ms) || queued-messages (u32-le)`,
//!   not 16 zero bytes (Desktop only checks that there are 16 bytes);
//! - extension `0x02` is read as a bit mask (bit 0: the metadata-box form; bit 1: the client answers
//!   echo requests); a mask without bit 0, or a login without the extension (the spec's legacy
//!   `0x00` form, `csp.struct.yml:355-356`, which the fake does not serve), gets a violation and a
//!   `close-error` right after the login;
//! - incoming messages go out only in the metadata-box form (`0x02`, never the legacy one).
//!
//! One more thing is not a difference of the library but Desktop's practice, where the spec is
//! silent: a CSP `client-hello` that came before the D2M handshake ended (Desktop forwards it in
//! `start()`) is held and answered only after the handshake.
//!
//! Two idle timers run beside each other (G2): the chat server's (this file's, set by
//! `set-connection-idle-timeout`, reset only by CSP frames) and the mediator's own (`d2m.rs`).
//! Keep the CSP leg alive with CSP echo requests, not with D2M traffic.
//!
//! The mediator (`d2m.rs`) feeds the Proxy payloads of a connection in order to
//! [`ChatServer::receive`] and wraps what comes out in Proxy payloads. Bytes are reassembled across
//! Proxy payloads (the client may split anywhere).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::RngCore as _;

use super::crypto::{CspNonces, KeyPair, csp_nonce, hsalsa_shared, open, seal, server_vouch};
use super::{Dir, Identities, Trace, Violations, lock};

/// The most bytes of one CSP frame (`TP/csp.struct.yml` "Size Limitations": "up to 8192 bytes
/// within a single frame"). The fake counts the WHOLE frame, the 2-byte length included, so the box
/// may have 8,190 bytes. [U: real server] whether the length field counts.
pub const MAX_FRAME: usize = 8_192;
/// The frame's length field (u16-le).
pub const FRAME_HEADER: usize = 2;
/// `client-hello`: TCK.public (32) and CCK (16), no frame.
pub const CLIENT_HELLO: usize = 48;
/// The `login` box: `login-data` (128) and its tag (16).
pub const LOGIN_BOX: usize = 144;
const LOGIN_DATA: usize = 128;
/// The `login-ack` box: 16 bytes of data and the tag.
pub const LOGIN_ACK: usize = 32;
const MAGIC: &[u8; 30] = b"threema-clever-extension-field";
/// How long a message with the short-lived flag stays queued.
pub const SHORT_LIVED_KEEP: Duration = Duration::from_secs(60);
/// The chat server's idle timeout until the client sends `set-connection-idle-timeout`
/// (`TP/csp.struct.yml:1001-1016`: "default one of 5 minutes"; valid for the connection only; "The
/// client must ensure that it sends echo requests or other traffic frequently").
pub const DEFAULT_CSP_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// The close code of the mediator when the chat server dropped the connection
/// (`TP/md-d2m.struct.yml` 4000). [U: real server] what the real chat server does with a refused
/// login, an oversized frame or a box that does not open (the fake drops the connection).
pub const CHAT_SERVER_CLOSED: u16 = 4000;

/// The container types (`TP/csp.struct.yml` `payload.container.type`).
pub mod kind {
    pub const ECHO_REQUEST: u8 = 0x00;
    pub const OUTGOING_MESSAGE: u8 = 0x01;
    pub const INCOMING_MESSAGE: u8 = 0x02;
    pub const UNBLOCK_INCOMING_MESSAGES: u8 = 0x03;
    pub const SET_PUSH_TOKEN: u8 = 0x20;
    pub const SET_PUSH_TOKEN_EXTRA: u8 = 0x24;
    pub const DELETE_PUSH_TOKEN: u8 = 0x25;
    pub const SET_CONNECTION_IDLE_TIMEOUT: u8 = 0x30;
    pub const ECHO_RESPONSE: u8 = 0x80;
    pub const OUTGOING_MESSAGE_ACK: u8 = 0x81;
    pub const INCOMING_MESSAGE_ACK: u8 = 0x82;
    pub const QUEUE_SEND_COMPLETE: u8 = 0xd0;
    pub const DEVICE_COOKIE_CHANGE_INDICATION: u8 = 0xd2;
    pub const CLEAR_DEVICE_COOKIE_CHANGE_INDICATION: u8 = 0xd3;
    pub const CLOSE_ERROR: u8 = 0xe0;
    pub const ALERT: u8 = 0xe1;
}

/// The message flags (`TP/csp.struct.yml` `message-flags`).
pub mod flag {
    pub const SEND_PUSH: u8 = 0x01;
    pub const NO_SERVER_QUEUING: u8 = 0x02;
    pub const NO_SERVER_ACK: u8 = 0x04;
    pub const SHORT_LIVED: u8 = 0x20;
}

fn kind_name(t: u8) -> &'static str {
    match t {
        kind::ECHO_REQUEST => "csp-echo-request",
        kind::OUTGOING_MESSAGE => "csp-outgoing-message",
        kind::INCOMING_MESSAGE => "csp-incoming-message",
        kind::UNBLOCK_INCOMING_MESSAGES => "csp-unblock-incoming-messages",
        kind::SET_PUSH_TOKEN | kind::SET_PUSH_TOKEN_EXTRA => "csp-set-push-token",
        kind::DELETE_PUSH_TOKEN => "csp-delete-push-token",
        kind::SET_CONNECTION_IDLE_TIMEOUT => "csp-set-connection-idle-timeout",
        kind::ECHO_RESPONSE => "csp-echo-response",
        kind::OUTGOING_MESSAGE_ACK => "csp-outgoing-message-ack",
        kind::INCOMING_MESSAGE_ACK => "csp-incoming-message-ack",
        kind::QUEUE_SEND_COMPLETE => "csp-queue-send-complete",
        kind::DEVICE_COOKIE_CHANGE_INDICATION => "csp-device-cookie-change-indication",
        kind::CLEAR_DEVICE_COOKIE_CHANGE_INDICATION => "csp-clear-device-cookie-change-indication",
        kind::CLOSE_ERROR => "csp-close-error",
        kind::ALERT => "csp-alert",
        _ => "csp-unknown",
    }
}

/// An "evil server" mode (the client must refuse each, `TP/csp.struct.yml:135-142`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Evil {
    #[default]
    None,
    /// `SCK == CCK`.
    SckEqualsCck,
    /// The repeated CCK in the `server-challenge-response` is not the client's.
    WrongRepeatedCck,
}

/// The chat server's knobs (a part of the mediator's knobs).
#[derive(Clone, Debug, Default)]
pub struct CspKnobs {
    pub evil: Evil,
    /// Split every struct and frame the server sends at random byte boundaries over several Proxy
    /// payloads (libthreema reassembles). [U: real server] how the real mediator chunks.
    pub chunk_proxy: bool,
    /// The server rule of `created-at` (`TP/csp.struct.yml:572-583`): replaced by now when it lies
    /// in the future or the short-lived flag is set. Off by default (the bytes stay the client's).
    pub replace_created_at: bool,
    /// TH3 (critic M4): added to the login-ack's time ON TOP of the mediator's
    /// [`super::d2m::Knobs::clock_skew_ms`] (ms, may be negative), so that a test can make the chat
    /// server's clock alone wrong. Default 0.
    pub clock_skew_ms: i64,
    /// TH3 (critic M4): the chat server takes the client's echo requests (counted in
    /// [`ChatServer::echo_requests`]) and answers none. Default off.
    pub withhold_echo: bool,
    /// TH3 fix P-L7/P-L9: the chat server takes the client's login and holds everything it sends
    /// from the login-ack on (the ack and what follows it) until [`super::d2m::Mediator::release_login_ack`]:
    /// a chat server that is slow to answer the login. Default off.
    pub withhold_login_ack: bool,
    /// TH6a S3 (critic H2, scenario A): the chat server CLOSES the connection, with this D2M close
    /// code, when a box (`0x01`) to the receiver arrives; the box is not kept, not acked and not
    /// shown to the peer, only counted ([`ChatServer::refused`]). Sticky until
    /// [`CspKnobs::stop_closing_on_box_to`]. [U: real server] what the real chat server does with a
    /// message it refuses (a `close-error` first? which code?); the fake only closes.
    pub close_on_box: Vec<BoxClose>,
    /// TH6a S3 (critic H2, scenario B): the chat server takes a box to the receiver (kept for the peer)
    /// and sends NO `message-ack` for it until [`super::d2m::Mediator::release_message_acks`], which
    /// also stops holding; an ack held on a connection that closed meanwhile is lost with it.
    /// [U: real server] a silent chat server.
    pub withhold_message_ack: Vec<[u8; 8]>,
    /// TH6a S3 (critic M7, brief item R9): a box whose nonce the chat server took before (any
    /// connection, any receiver) is DROPPED: not kept, not acked, only counted
    /// ([`ChatServer::repeats_dropped`]). Off: a repeat is kept and acked as any box (the fake has no
    /// memory of ids). [U: real server] whether the real chat server acks a byte-identical repeat or
    /// drops it silently; no client on disk proves it (Desktop's repeat has a fresh nonce).
    pub drop_repeats: bool,
}

/// One rule of [`CspKnobs::close_on_box`]: the receiver (never printed) and the D2M close code.
#[derive(Clone, PartialEq, Eq)]
pub struct BoxClose {
    pub receiver: [u8; 8],
    pub code: u16,
}

impl std::fmt::Debug for BoxClose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BoxClose(code {})", self.code)
    }
}

impl CspKnobs {
    /// Every box to `receiver` closes the connection with `code` (see [`CspKnobs::close_on_box`]).
    /// A second call for the same receiver replaces the code.
    #[track_caller]
    pub fn close_on_box_to(&mut self, receiver: &str, code: u16) {
        let receiver = id_bytes(receiver).expect("a test's identities have 8 bytes");
        self.close_on_box.retain(|r| r.receiver != receiver);
        self.close_on_box.push(BoxClose { receiver, code });
    }

    /// Lifts [`CspKnobs::close_on_box_to`] for `receiver`.
    #[track_caller]
    pub fn stop_closing_on_box_to(&mut self, receiver: &str) {
        let receiver = id_bytes(receiver).expect("a test's identities have 8 bytes");
        self.close_on_box.retain(|r| r.receiver != receiver);
    }

    /// The acks of boxes to `receiver` are held (see [`CspKnobs::withhold_message_ack`]).
    #[track_caller]
    pub fn withhold_message_ack_of(&mut self, receiver: &str) {
        let receiver = id_bytes(receiver).expect("a test's identities have 8 bytes");
        if !self.withhold_message_ack.contains(&receiver) {
            self.withhold_message_ack.push(receiver);
        }
    }
}

/// A device cookie the fake saw at a login: compared by a test with the stored one, never printed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SeenCookie(pub [u8; 16]);

impl std::fmt::Debug for SeenCookie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SeenCookie")
    }
}

/// What the client told the server at its login (no key; the identity is in the world's table).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoginInfo {
    /// Extension `0x00`.
    pub client_info: Option<String>,
    /// Extension `0x01`.
    pub csp_device_id: Option<u64>,
    /// Extension `0x02`, read as a bit mask.
    pub features: Option<u8>,
    /// Whether extension `0x03` (the device cookie) came.
    pub sent_device_cookie: bool,
    /// TH3 fix P-L3: the cookie of extension `0x03` (redacted in `Debug`).
    pub device_cookie: Option<SeenCookie>,
    /// Extension types the fake does not know (ignored). [U: real server] what it does with them.
    pub unknown_extensions: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    AwaitingClientHello,
    AwaitingLogin,
    PostHandshake,
    Closed,
}

/// The chat server's side of one D2M connection.
pub struct CspConn {
    phase: Phase,
    inbox: Vec<u8>,
    tck: [u8; 32],
    tsk: Option<KeyPair>,
    /// `X25519HSalsa20(TSK.secret, TCK.public)`: the session's one key for both directions.
    key: [u8; 32],
    client: CspNonces,
    server: CspNonces,
    identity: Option<[u8; 8]>,
    info: LoginInfo,
    unblocked: bool,
    queue_send_complete: bool,
    /// The (sender, message id) of every incoming message sent on this connection.
    sent: BTreeSet<([u8; 8], u64)>,
    /// The ones of them that expect an ack (flag 0x04 clear) and have none yet (G10): an ack for
    /// one of these is fine even when its queue entry expired meanwhile.
    awaiting_ack: BTreeSet<([u8; 8], u64)>,
}

impl CspConn {
    pub fn new() -> Self {
        Self {
            phase: Phase::AwaitingClientHello,
            inbox: Vec::new(),
            tck: [0; 32],
            tsk: None,
            key: [0; 32],
            client: CspNonces::new([0; 16]),
            server: CspNonces::new([0; 16]),
            identity: None,
            info: LoginInfo::default(),
            unblocked: false,
            queue_send_complete: false,
            sent: BTreeSet::new(),
            awaiting_ack: BTreeSet::new(),
        }
    }

    pub fn logged_in(&self) -> bool {
        self.phase == Phase::PostHandshake
    }

    pub fn identity(&self) -> Option<[u8; 8]> {
        self.identity.filter(|_| self.phase == Phase::PostHandshake)
    }

    pub fn unblocked(&self) -> bool {
        self.unblocked && self.phase == Phase::PostHandshake
    }

    pub fn info(&self) -> &LoginInfo {
        &self.info
    }

    /// The session key and SCK (for a test's independent check of what the server sent).
    pub fn session(&self) -> Option<([u8; 32], [u8; 16])> {
        self.tsk.as_ref().map(|_| (self.key, self.server.cookie()))
    }
}

impl Default for CspConn {
    fn default() -> Self {
        Self::new()
    }
}

/// What the chat server asks the mediator to do for one connection.
#[derive(Clone, PartialEq, Eq)]
pub enum CspOut {
    /// Bytes for the client, in one or more Proxy payloads.
    Proxy(Vec<u8>),
    /// Close the D2M connection with this code.
    Close(u16),
    /// `set-connection-idle-timeout`.
    IdleTimeout(Duration),
}

/// What the chat server needs of the mediator for one call.
pub struct Cx<'a> {
    pub conn: u64,
    pub now_ms: u64,
    pub clock_skew_ms: i64,
    pub violations: &'a Violations,
    pub trace: &'a Trace,
    pub rng: &'a mut ChaCha20Rng,
    pub knobs: &'a CspKnobs,
    pub leader_mode: bool,
    pub is_leader: bool,
    pub out: Vec<CspOut>,
}

impl Cx<'_> {
    fn violation(&self, rule: &'static str) {
        self.violations.record(rule, &[("conn", self.conn)]);
    }

    /// A breach that ends the connection: recorded, and dropped as the chat server would.
    fn refuse(&mut self, cc: &mut CspConn, rule: &'static str) {
        self.violation(rule);
        cc.phase = Phase::Closed;
        self.out.push(CspOut::Close(CHAT_SERVER_CLOSED));
    }
}

/// One message in an identity's incoming queue.
#[derive(Clone)]
pub struct Queued {
    pub sender: [u8; 8],
    pub message_id: u64,
    pub flags: u8,
    pub queued_ms: u64,
    /// The whole `message-with-metadata-box` struct.
    pub payload: Vec<u8>,
}

/// One message a client sent (`0x01`), as the server keeps it for the peer (TH5/TH7 open it).
#[derive(Clone, PartialEq, Eq)]
pub struct Sent {
    pub sender: [u8; 8],
    pub receiver: [u8; 8],
    pub message_id: u64,
    pub created_at: u32,
    pub flags: u8,
    /// The whole payload (the struct as the server keeps it; `created-at` replaced when the knob
    /// says so).
    pub bytes: Vec<u8>,
    /// TH6a S3: the D2M connection it came in on (a resend on a new socket has another).
    pub conn: u64,
    /// TH6a S3: whether the server replaced `created-at` ([`CspKnobs::replace_created_at`]); the
    /// peer's check of head against metadata skips such a message.
    pub replaced: bool,
}

impl std::fmt::Debug for Sent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Sent(id {}, flags {:#04x}, {} B)",
            self.message_id,
            self.flags,
            self.bytes.len()
        )
    }
}

/// The device cookie the server keeps per identity, and the change flag.
#[derive(Clone, Copy)]
struct Cookie {
    stored: [u8; 16],
    changed: bool,
}

/// The chat server of one world.
pub struct ChatServer {
    identities: Identities,
    sk: KeyPair,
    queues: BTreeMap<[u8; 8], Vec<Queued>>,
    cookies: BTreeMap<[u8; 8], Cookie>,
    sent: Vec<Sent>,
    push_calls: Vec<u8>,
    echo_responses: Vec<Vec<u8>>,
    /// The client's echo requests that reached the chat server.
    echo_requests: usize,
    idle_requests: Vec<u16>,
    /// TH6a S3: the boxes `CspKnobs::close_on_box` refused: (connection, receiver, message id).
    refused: Vec<(u64, [u8; 8], u64)>,
    /// TH6a S3: the `message-ack`s `CspKnobs::withhold_message_ack` holds, in order.
    held_acks: Vec<HeldAck>,
    /// TH6a S3: the nonces of the boxes taken so far (`CspKnobs::drop_repeats`).
    taken_nonces: BTreeSet<[u8; 24]>,
    /// TH6a S3: the (connection, receiver, message id) of the repeats `drop_repeats` dropped.
    repeats_dropped: Vec<(u64, [u8; 8], u64)>,
}

/// A `message-ack` the chat server holds back (`CspKnobs::withhold_message_ack`).
#[derive(Clone, PartialEq, Eq)]
pub struct HeldAck {
    pub conn: u64,
    pub receiver: [u8; 8],
    pub message_id: u64,
}

impl std::fmt::Debug for HeldAck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HeldAck(conn {})", self.conn)
    }
}

/// An 8-byte Threema ID of the wire as the fake's table writes it.
pub fn id_bytes(id: &str) -> Option<[u8; 8]> {
    id.as_bytes().try_into().ok()
}

/// One server frame: `u16-le(length) || box`.
fn frame(boxed: Vec<u8>) -> Vec<u8> {
    let length = u16::try_from(boxed.len()).expect("a frame of the fake fits u16");
    let mut out = Vec::with_capacity(FRAME_HEADER + boxed.len());
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(&boxed);
    assert!(
        out.len() <= MAX_FRAME,
        "the fake built a CSP frame over the limit"
    );
    out
}

/// The `message-with-metadata-box` (`0x02`) of an incoming message: sender, receiver, message id,
/// created-at, flags, reserved, metadata-length 0, the legacy nickname (zeros), no metadata box,
/// the nonce and the message box (opaque here; the peer builder of TH5 makes real ones).
pub fn incoming_message(
    sender: &[u8; 8],
    receiver: &[u8; 8],
    message_id: u64,
    created_at: u32,
    flags: u8,
    nonce: &[u8; 24],
    message_box: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + 24 + message_box.len());
    out.extend_from_slice(sender);
    out.extend_from_slice(receiver);
    out.extend_from_slice(&message_id.to_le_bytes());
    out.extend_from_slice(&created_at.to_le_bytes());
    out.push(flags);
    out.push(0);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&[0u8; 32]);
    out.extend_from_slice(nonce);
    out.extend_from_slice(message_box);
    out
}

impl ChatServer {
    pub fn new(identities: Identities, sk: KeyPair) -> Self {
        Self {
            identities,
            sk,
            queues: BTreeMap::new(),
            cookies: BTreeMap::new(),
            sent: Vec::new(),
            push_calls: Vec::new(),
            echo_responses: Vec::new(),
            echo_requests: 0,
            idle_requests: Vec::new(),
            refused: Vec::new(),
            held_acks: Vec::new(),
            taken_nonces: BTreeSet::new(),
            repeats_dropped: Vec::new(),
        }
    }

    pub fn sent(&self) -> &[Sent] {
        &self.sent
    }

    pub fn push_calls(&self) -> &[u8] {
        &self.push_calls
    }

    pub fn echo_responses(&self) -> &[Vec<u8>] {
        &self.echo_responses
    }

    /// How many echo requests of clients reached the chat server.
    pub fn echo_requests(&self) -> usize {
        self.echo_requests
    }

    pub fn idle_requests(&self) -> &[u16] {
        &self.idle_requests
    }

    /// TH6a S3: the boxes `CspKnobs::close_on_box` refused: (connection, receiver, message id).
    pub fn refused(&self) -> &[(u64, [u8; 8], u64)] {
        &self.refused
    }

    /// TH6a S3: the acks `CspKnobs::withhold_message_ack` holds now.
    pub fn held_acks(&self) -> &[HeldAck] {
        &self.held_acks
    }

    /// TH6a S3: the repeats `CspKnobs::drop_repeats` dropped: (connection, receiver, message id).
    pub fn repeats_dropped(&self) -> &[(u64, [u8; 8], u64)] {
        &self.repeats_dropped
    }

    /// TH6a S3: takes the held acks of `receiver` out, in order (`Mediator::release_message_acks`).
    pub fn take_held_acks(&mut self, receiver: &[u8; 8]) -> Vec<HeldAck> {
        let (taken, kept) = std::mem::take(&mut self.held_acks)
            .into_iter()
            .partition(|a| a.receiver == *receiver);
        self.held_acks = kept;
        taken
    }

    /// TH6a S3: the `message-ack` of an outgoing message (`0x81`: the receiver and the id), sent now.
    pub fn send_message_ack(
        &self,
        cc: &mut CspConn,
        cx: &mut Cx<'_>,
        receiver: &[u8; 8],
        message_id: u64,
    ) {
        let mut ack = receiver.to_vec();
        ack.extend_from_slice(&message_id.to_le_bytes());
        self.send(cc, cx, kind::OUTGOING_MESSAGE_ACK, &ack);
    }

    pub fn queue_len(&self, receiver: &[u8; 8]) -> usize {
        self.queues.get(receiver).map_or(0, Vec::len)
    }

    /// Queues an incoming message for `receiver`. `deliverable` says whether a connection of the
    /// receiver is unblocked now (the mediator then calls [`Self::deliver`] for it). A message with
    /// "no server queuing" is dropped when none is (`false` returned).
    pub fn enqueue(&mut self, receiver: &[u8; 8], message: Queued, deliverable: bool) -> bool {
        if message.flags & flag::NO_SERVER_QUEUING != 0 && !deliverable {
            return false;
        }
        self.queues.entry(*receiver).or_default().push(message);
        true
    }

    /// Drops what outlived its time: short-lived messages after 60 s. [U: real server] whether a
    /// short-lived message that went out and was not acked comes again before its 60 s end.
    pub fn expire(&mut self, now_ms: u64) {
        let keep = u64::try_from(SHORT_LIVED_KEEP.as_millis()).expect("60 s fit u64");
        for queue in self.queues.values_mut() {
            queue.retain(|q| {
                q.flags & flag::SHORT_LIVED == 0 || now_ms.saturating_sub(q.queued_ms) < keep
            });
        }
    }

    /// Sends a container (type, reserved, data) as one frame with the next SSN.
    fn send(&self, cc: &mut CspConn, cx: &mut Cx<'_>, t: u8, data: &[u8]) {
        if cc.phase != Phase::PostHandshake {
            return;
        }
        let mut container = Vec::with_capacity(4 + data.len());
        container.extend_from_slice(&[t, 0, 0, 0]);
        container.extend_from_slice(data);
        let ssn = cc.server.next_number();
        let boxed = cc
            .server
            .seal(&cc.key, &container)
            .expect("the server's sequence number");
        cx.trace.frame(
            Dir::Out,
            cx.conn,
            kind_name(t),
            &[("ssn", ssn), ("len", data.len() as u64)],
        );
        cx.out.push(CspOut::Proxy(frame(boxed)));
    }

    /// A script call: an echo request from the server. [I] Only to a client that declared the
    /// feature (extension 0x02, bit 1: "supports reception of echo-requests"); a call for one that
    /// did not is a bug of the test: nothing is sent and the call is recorded as a violation, which
    /// fails the test at its end.
    pub fn echo_request(&self, cc: &mut CspConn, cx: &mut Cx<'_>, data: &[u8]) {
        if !cc.info.features.is_some_and(|f| f & 0b10 != 0) {
            cx.violation("csp-echo-request-without-client-support");
            return;
        }
        self.send(cc, cx, kind::ECHO_REQUEST, data);
    }

    /// A script call: `close-error{can-reconnect, message}`; the mediator then closes.
    pub fn close_error(&self, cc: &mut CspConn, cx: &mut Cx<'_>, can_reconnect: bool) {
        let mut data = vec![u8::from(can_reconnect)];
        data.extend_from_slice(b"closed by the test");
        self.send(cc, cx, kind::CLOSE_ERROR, &data);
        cc.phase = Phase::Closed;
    }

    /// A script call: an `alert`.
    pub fn alert(&self, cc: &mut CspConn, cx: &mut Cx<'_>) {
        self.send(cc, cx, kind::ALERT, b"an alert of the test");
    }

    /// Sends what the identity's queue holds and this connection has not sent yet, in order, if
    /// the connection is unblocked (C9, C10). What expects no ack ("no server ack") leaves the
    /// queue once it went out.
    pub fn deliver(&mut self, cc: &mut CspConn, cx: &mut Cx<'_>) {
        if !cc.unblocked() {
            return;
        }
        let Some(identity) = cc.identity else {
            return;
        };
        let pending: Vec<Queued> = self
            .queues
            .get(&identity)
            .map(|q| {
                q.iter()
                    .filter(|m| !cc.sent.contains(&(m.sender, m.message_id)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        for m in &pending {
            self.send(cc, cx, kind::INCOMING_MESSAGE, &m.payload);
            cc.sent.insert((m.sender, m.message_id));
            if m.flags & flag::NO_SERVER_ACK == 0 {
                cc.awaiting_ack.insert((m.sender, m.message_id));
            }
        }
        if let Some(queue) = self.queues.get_mut(&identity) {
            queue.retain(|m| {
                m.flags & flag::NO_SERVER_ACK == 0 || !cc.sent.contains(&(m.sender, m.message_id))
            });
        }
    }

    /// Bytes of the Proxy payloads of one connection, in order.
    pub fn receive(&mut self, cc: &mut CspConn, cx: &mut Cx<'_>, bytes: &[u8]) {
        if cc.phase == Phase::Closed {
            return;
        }
        cc.inbox.extend_from_slice(bytes);
        loop {
            let before = cc.inbox.len();
            match cc.phase {
                Phase::AwaitingClientHello => self.client_hello(cc, cx),
                Phase::AwaitingLogin => self.login(cc, cx),
                Phase::PostHandshake => self.read_frame(cc, cx),
                Phase::Closed => return,
            }
            if cc.inbox.len() == before || cc.phase == Phase::Closed {
                return;
            }
        }
    }

    /// C1, C2: the `client-hello` and the `server-hello`.
    fn client_hello(&self, cc: &mut CspConn, cx: &mut Cx<'_>) {
        if cc.inbox.len() < CLIENT_HELLO {
            return;
        }
        let hello: Vec<u8> = cc.inbox.drain(..CLIENT_HELLO).collect();
        cc.tck.copy_from_slice(&hello[..32]);
        let mut cck = [0u8; 16];
        cck.copy_from_slice(&hello[32..]);
        cx.trace
            .frame(Dir::In, cx.conn, "csp-client-hello", &[("len", 48)]);
        let mut tsk = [0u8; 32];
        cx.rng.fill_bytes(&mut tsk);
        let tsk = KeyPair::from_secret(tsk);
        let mut sck = [0u8; 16];
        cx.rng.fill_bytes(&mut sck);
        if cx.knobs.evil == Evil::SckEqualsCck {
            sck = cck;
        } else if sck == cck {
            // Server rule: SCK and CCK are not equal.
            sck[0] ^= 1;
        }
        let (Some(hello_key), Some(key)) = (
            hsalsa_shared(&self.sk.secret, &cc.tck),
            hsalsa_shared(&tsk.secret, &cc.tck),
        ) else {
            cx.refuse(cc, "csp-client-hello-key-not-contributory");
            return;
        };
        let mut repeated = cck;
        if cx.knobs.evil == Evil::WrongRepeatedCck {
            repeated[0] ^= 1;
        }
        let mut response = Vec::with_capacity(48);
        response.extend_from_slice(&tsk.public);
        response.extend_from_slice(&repeated);
        cc.client = CspNonces::new(cck);
        cc.server = CspNonces::new(sck);
        let boxed = cc
            .server
            .seal(&hello_key, &response)
            .expect("SSN 1 of the server-hello");
        let mut out = sck.to_vec();
        out.extend_from_slice(&boxed);
        cc.key = key;
        cc.tsk = Some(tsk);
        cc.phase = Phase::AwaitingLogin;
        cx.trace
            .frame(Dir::Out, cx.conn, "csp-server-hello", &[("len", 80)]);
        cx.out.push(CspOut::Proxy(out));
    }

    /// C3-C5: the `login` (and its extensions) and the `login-ack`.
    fn login(&mut self, cc: &mut CspConn, cx: &mut Cx<'_>) {
        if cc.inbox.len() < LOGIN_BOX {
            return;
        }
        // A look at the login data with CSN 1 to learn the extensions' length; the counter moves
        // only when the whole login is there.
        let Some(data) = open(
            &cc.key,
            &csp_nonce(&cc.client.cookie(), cc.client.next_number()),
            &cc.inbox[..LOGIN_BOX],
        ) else {
            cx.refuse(cc, "csp-login-does-not-open");
            return;
        };
        let indicator = &data[8..40];
        let extensions_length = if indicator[..30] == MAGIC[..] {
            usize::from(u16::from_le_bytes([indicator[30], indicator[31]]))
        } else {
            0
        };
        if cc.inbox.len() < LOGIN_BOX + extensions_length {
            return;
        }
        let login: Vec<u8> = cc.inbox.drain(..LOGIN_BOX + extensions_length).collect();
        let data = cc
            .client
            .open(&cc.key, &login[..LOGIN_BOX])
            .expect("it opened above");
        cx.trace.frame(
            Dir::In,
            cx.conn,
            "csp-login",
            &[("len", login.len() as u64)],
        );
        debug_assert_eq!(data.len(), LOGIN_DATA);
        let extensions = if extensions_length > 0 {
            match cc.client.open(&cc.key, &login[LOGIN_BOX..]) {
                Some(e) => e,
                None => {
                    cx.refuse(cc, "csp-login-extensions-do-not-open");
                    return;
                }
            }
        } else {
            Vec::new()
        };
        let mut identity = [0u8; 8];
        identity.copy_from_slice(&data[..8]);
        let sck_echo = &data[40..56];
        let reserved1 = &data[56..80];
        let vouch = &data[80..112];
        let reserved2 = &data[112..128];
        if sck_echo != cc.server.cookie() {
            cx.refuse(cc, "csp-login-repeated-sck-wrong");
            return;
        }
        if reserved1.iter().chain(reserved2).any(|b| *b != 0) {
            cx.refuse(cc, "csp-login-reserved-not-zero");
            return;
        }
        let known = std::str::from_utf8(&identity)
            .ok()
            .and_then(|id| lock(&self.identities).get(id).map(|i| i.ck.public));
        let Some(ck_public) = known else {
            cx.refuse(cc, "csp-login-unknown-identity");
            return;
        };
        let tsk_secret = cc.tsk.as_ref().expect("after the server-hello").secret;
        let expected = server_vouch(
            &self.sk.secret,
            &tsk_secret,
            &ck_public,
            &cc.server.cookie(),
            &cc.tck,
        );
        if expected.as_ref().map(|v| &v[..]) != Some(vouch) {
            cx.refuse(cc, "csp-login-vouch-wrong");
            return;
        }
        let Some(info) = parse_extensions(&extensions) else {
            cx.refuse(cc, "csp-login-extensions-malformed");
            return;
        };
        let cookie = extension_cookie(&extensions);
        cc.info = info;
        cc.identity = Some(identity);
        cc.phase = Phase::PostHandshake;
        // C4: the device cookie of the last connection; a different one (or none while one is
        // stored: "as if the client had sent an all-zero device cookie") sets the flag.
        let changed = match (self.cookies.get(&identity).copied(), cookie) {
            (None, None) => false,
            (None, Some(c)) => {
                self.cookies.insert(
                    identity,
                    Cookie {
                        stored: c,
                        changed: false,
                    },
                );
                false
            }
            (Some(stored), presented) => {
                let presented = presented.unwrap_or([0; 16]);
                let changed = stored.changed || stored.stored != presented;
                self.cookies.insert(
                    identity,
                    Cookie {
                        stored: presented,
                        changed,
                    },
                );
                changed
            }
        };
        // C5 (libthreema's layout): reserved (4), current time (ms), queued messages.
        let now = cx
            .now_ms
            .checked_add_signed(cx.clock_skew_ms)
            .and_then(|t| t.checked_add_signed(cx.knobs.clock_skew_ms))
            .expect("a skew inside u64 ms");
        let queued = u32::try_from(self.queue_len(&identity)).expect("a queue fits u32");
        let mut ack = vec![0u8; 4];
        ack.extend_from_slice(&now.to_le_bytes());
        ack.extend_from_slice(&queued.to_le_bytes());
        let ssn = cc.server.next_number();
        let boxed = cc
            .server
            .seal(&cc.key, &ack)
            .expect("SSN 2 of the login-ack");
        cx.trace.frame(
            Dir::Out,
            cx.conn,
            "csp-login-ack",
            &[("ssn", ssn), ("queued", u64::from(queued))],
        );
        cx.out.push(CspOut::Proxy(boxed));
        if cc.info.features.is_none_or(|f| f & 0b1 == 0) {
            // The spec's rule for an unsupported payload version: the handshake completes, then
            // a close-error. [I] the fake reads the byte as libthreema's bit mask. A login without
            // extension 0x02 selects the legacy form (`csp.struct.yml:355-356`: "the server must
            // assume that version 0x00 has been selected"), which the fake does not serve: [I] it
            // refuses with its own rule rather than serve a form libthreema cannot read.
            cx.violation(if cc.info.features.is_none() {
                "csp-no-payload-version-extension"
            } else {
                "csp-no-metadata-box-support"
            });
            self.close_error(cc, cx, false);
            cx.out.push(CspOut::Close(CHAT_SERVER_CLOSED));
            return;
        }
        if changed {
            self.send(cc, cx, kind::DEVICE_COOKIE_CHANGE_INDICATION, &[]);
        }
    }

    /// C6: one frame of the payload phase (strict CSN), then its payload.
    fn read_frame(&mut self, cc: &mut CspConn, cx: &mut Cx<'_>) {
        if cc.inbox.len() < FRAME_HEADER {
            return;
        }
        let length = usize::from(u16::from_le_bytes([cc.inbox[0], cc.inbox[1]]));
        if FRAME_HEADER + length > MAX_FRAME {
            cx.trace.frame(
                Dir::In,
                cx.conn,
                "csp-oversized",
                &[("len", (FRAME_HEADER + length) as u64)],
            );
            cx.refuse(cc, "csp-frame-over-8192-bytes");
            return;
        }
        if cc.inbox.len() < FRAME_HEADER + length {
            return;
        }
        let whole: Vec<u8> = cc.inbox.drain(..FRAME_HEADER + length).collect();
        let csn = cc.client.next_number();
        let Some(container) = cc.client.open(&cc.key, &whole[FRAME_HEADER..]) else {
            cx.refuse(cc, "csp-box-does-not-open");
            return;
        };
        if container.len() < 4 {
            cx.refuse(cc, "csp-container-under-the-header");
            return;
        }
        let (t, data) = (container[0], &container[4..]);
        cx.trace.frame(
            Dir::In,
            cx.conn,
            kind_name(t),
            &[("csn", csn), ("len", data.len() as u64)],
        );
        self.payload(cc, cx, t, data);
    }

    /// C7-C13: what the client may send (libthreema's list, S-mock §3.3); anything else is a bug.
    fn payload(&mut self, cc: &mut CspConn, cx: &mut Cx<'_>, t: u8, data: &[u8]) {
        let identity = cc.identity.expect("logged in");
        match t {
            kind::ECHO_REQUEST => {
                self.echo_requests += 1;
                if !cx.knobs.withhold_echo {
                    self.send(cc, cx, kind::ECHO_RESPONSE, data);
                }
            }
            kind::ECHO_RESPONSE => self.echo_responses.push(data.to_vec()),
            kind::OUTGOING_MESSAGE => self.outgoing(cc, cx, identity, data),
            kind::INCOMING_MESSAGE_ACK => {
                let (Some(sender), Some(id)) = (
                    data.get(..8).and_then(|s| <[u8; 8]>::try_from(s).ok()),
                    data.get(8..16)
                        .and_then(|b| b.try_into().ok())
                        .map(u64::from_le_bytes),
                ) else {
                    cx.refuse(cc, "csp-message-ack-malformed");
                    return;
                };
                if data.len() != 16 {
                    cx.refuse(cc, "csp-message-ack-malformed");
                    return;
                }
                // G10: the ack is fine for a pair delivered on this connection that expects one and
                // has none yet, also when its short-lived queue entry ran out (0x20: "only be
                // queued for 60 seconds", `csp.struct.yml:622-623`, but the message WAS sent and
                // the receiver acks every message without 0x04, `:866-870`).
                // [U: real server] what the server does with an ack for a message it did not send
                // on this connection, one that expected none (0x04: "No acknowledgement should be
                // sent", `:617-620`) or a second ack: recorded here.
                if cc.awaiting_ack.remove(&(sender, id)) {
                    let queue = self.queues.entry(identity).or_default();
                    if let Some(at) = queue
                        .iter()
                        .position(|m| m.sender == sender && m.message_id == id)
                    {
                        queue.remove(at);
                    }
                } else {
                    cx.violation("csp-ack-for-a-message-not-sent");
                }
            }
            kind::UNBLOCK_INCOMING_MESSAGES => {
                // M5 of the lane decisions: only in leader mode, and only the leader (C9).
                if !cx.leader_mode {
                    cx.violation("csp-unblock-outside-leader-mode");
                    return;
                }
                if !cx.is_leader {
                    cx.violation("csp-unblock-from-a-follower");
                    return;
                }
                cc.unblocked = true;
                self.deliver(cc, cx);
                if !cc.queue_send_complete {
                    cc.queue_send_complete = true;
                    self.send(cc, cx, kind::QUEUE_SEND_COMPLETE, &[]);
                }
            }
            kind::SET_PUSH_TOKEN | kind::SET_PUSH_TOKEN_EXTRA | kind::DELETE_PUSH_TOKEN => {
                // C13: recorded only (Kourier has no push; a test asserts that none came).
                self.push_calls.push(t);
            }
            kind::SET_CONNECTION_IDLE_TIMEOUT => {
                let Some(seconds) = data
                    .get(..2)
                    .and_then(|b| b.try_into().ok())
                    .map(u16::from_le_bytes)
                    .filter(|_| data.len() == 2)
                else {
                    cx.refuse(cc, "csp-idle-timeout-malformed");
                    return;
                };
                // C8: seconds (the spec; libthreema's doc says minutes). [U: real server] what the
                // server does outside 30..=600 s: the fake takes it and records it.
                self.idle_requests.push(seconds);
                cx.out
                    .push(CspOut::IdleTimeout(Duration::from_secs(u64::from(seconds))));
            }
            kind::CLEAR_DEVICE_COOKIE_CHANGE_INDICATION => {
                if let Some(c) = self.cookies.get_mut(&identity) {
                    c.changed = false;
                }
            }
            _ => cx.refuse(cc, "csp-unexpected-payload-type"),
        }
    }

    /// C11: an outgoing message: the sender must be the login identity; acked unless "no server
    /// ack"; kept for the peer.
    fn outgoing(&mut self, cc: &mut CspConn, cx: &mut Cx<'_>, identity: [u8; 8], data: &[u8]) {
        if data.len() < 64 + 24 {
            cx.refuse(cc, "csp-outgoing-message-malformed");
            return;
        }
        let mut bytes = data.to_vec();
        let sender: [u8; 8] = bytes[..8].try_into().expect("8 bytes");
        let receiver: [u8; 8] = bytes[8..16].try_into().expect("8 bytes");
        let message_id = u64::from_le_bytes(bytes[16..24].try_into().expect("8 bytes"));
        let mut created_at = u32::from_le_bytes(bytes[24..28].try_into().expect("4 bytes"));
        let flags = bytes[28];
        // [I] `csp.struct.yml:633` "Reserved, must be set to zero" (byte 29); the server goes on.
        if bytes[29] != 0 {
            cx.violation("csp-outgoing-reserved-not-zero");
        }
        // [I] the metadata box (`metadata-length` bytes after the 64-byte head) and the 24-byte
        // nonce must fit the payload.
        let metadata_length = usize::from(u16::from_le_bytes([bytes[30], bytes[31]]));
        if 64 + metadata_length + 24 > bytes.len() {
            cx.refuse(cc, "csp-outgoing-metadata-length-overruns");
            return;
        }
        if sender != identity {
            cx.refuse(cc, "csp-outgoing-sender-is-not-the-login-identity");
            return;
        }
        // TH6a S3 (critic H2, scenario A): a box the server refuses by closing the connection. It is
        // read (traced as any frame) and then dropped: not kept, not acked, not queued for the peer.
        // [U: real server] what the real chat server does (and says) before it closes.
        if let Some(rule) = cx
            .knobs
            .close_on_box
            .iter()
            .find(|r| r.receiver == receiver)
        {
            cx.trace.frame(
                Dir::Out,
                cx.conn,
                "csp-outgoing-message-refused",
                &[("code", u64::from(rule.code))],
            );
            self.refused.push((cx.conn, receiver, message_id));
            cc.phase = Phase::Closed;
            cx.out.push(CspOut::Close(rule.code));
            return;
        }
        // TH6a S3 (critic M7): the nonce of a box is taken once; with the knob on a second box with
        // the same nonce vanishes (no ack: the client waits for one). [U: real server]
        let nonce_at = 64 + metadata_length;
        let nonce: [u8; 24] = bytes[nonce_at..nonce_at + 24]
            .try_into()
            .expect("24 bytes: the length check above");
        let first_time = self.taken_nonces.insert(nonce);
        if !first_time && cx.knobs.drop_repeats {
            cx.trace.frame(
                Dir::In,
                cx.conn,
                "csp-outgoing-repeat-dropped",
                &[("len", bytes.len() as u64)],
            );
            self.repeats_dropped.push((cx.conn, receiver, message_id));
            return;
        }
        let now_s = u32::try_from(cx.now_ms / 1000).expect("seconds fit u32");
        let mut replaced = false;
        if cx.knobs.replace_created_at && (created_at > now_s || flags & flag::SHORT_LIVED != 0) {
            created_at = now_s;
            bytes[24..28].copy_from_slice(&created_at.to_le_bytes());
            replaced = true;
        }
        self.sent.push(Sent {
            sender,
            receiver,
            message_id,
            created_at,
            flags,
            bytes,
            conn: cx.conn,
            replaced,
        });
        if flags & flag::NO_SERVER_ACK == 0 {
            // TH6a S3 (critic H2, scenario B): a chat server that takes the box and stays silent.
            if cx.knobs.withhold_message_ack.contains(&receiver) {
                cx.trace
                    .frame(Dir::Out, cx.conn, "csp-outgoing-message-ack-withheld", &[]);
                self.held_acks.push(HeldAck {
                    conn: cx.conn,
                    receiver,
                    message_id,
                });
            } else {
                self.send_message_ack(cc, cx, &receiver, message_id);
            }
        }
    }
}

/// The extensions of a login (`TP/csp.struct.yml:268-398`); `None` when one runs over the end.
fn parse_extensions(mut bytes: &[u8]) -> Option<LoginInfo> {
    let mut info = LoginInfo::default();
    while !bytes.is_empty() {
        let t = *bytes.first()?;
        let length = usize::from(u16::from_le_bytes([*bytes.get(1)?, *bytes.get(2)?]));
        let payload = bytes.get(3..3 + length)?;
        match t {
            0x00 => info.client_info = Some(String::from_utf8(payload.to_vec()).ok()?),
            0x01 => info.csp_device_id = Some(u64::from_le_bytes(payload.try_into().ok()?)),
            0x02 => info.features = Some(*payload.first().filter(|_| length == 1)?),
            0x03 => {
                info.device_cookie = Some(SeenCookie(payload.try_into().ok()?));
                info.sent_device_cookie = true;
            }
            // [I] unknown extension types are ignored.
            other => info.unknown_extensions.push(other),
        }
        bytes = &bytes[3 + length..];
    }
    Some(info)
}

fn extension_cookie(mut bytes: &[u8]) -> Option<[u8; 16]> {
    while bytes.len() >= 3 {
        let length = usize::from(u16::from_le_bytes([bytes[1], bytes[2]]));
        let payload = bytes.get(3..3 + length)?;
        if bytes[0] == 0x03 {
            return payload.try_into().ok();
        }
        bytes = &bytes[3 + length..];
    }
    None
}

/// Splits `bytes` at random boundaries (the chunk knob): at least one piece.
pub fn chunks(bytes: Vec<u8>, rng: &mut ChaCha20Rng) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut rest = bytes.as_slice();
    while rest.len() > 1 {
        let cut = 1 + usize::try_from(rng.next_u32()).expect("u32 fits usize") % rest.len();
        out.push(rest[..cut].to_vec());
        rest = &rest[cut..];
    }
    if !rest.is_empty() || out.is_empty() {
        out.push(rest.to_vec());
    }
    out
}

/// The opened `login-ack` data of a test's independent check (Desktop's rule: 16 bytes).
pub fn open_login_ack(key: &[u8; 32], sck: &[u8; 16], boxed: &[u8]) -> Option<Vec<u8>> {
    open(key, &csp_nonce(sck, 2), boxed)
}

/// A server box the test seals itself (an evil frame with a chosen SSN).
pub fn seal_frame(key: &[u8; 32], sck: &[u8; 16], ssn: u64, container: &[u8]) -> Vec<u8> {
    frame(seal(key, &csp_nonce(sck, ssn), container))
}

//! The fake MEDIATOR (D2M). Part 1 (TH2 M2): the container codec and its size limits, the
//! handshake (the URL's `ClientUrlInfo`, `ServerHello` with the challenge, the `ClientHello` check,
//! `ServerInfo` with a clock-skew knob), the device slots and their policies, the duplicate
//! connection, scripted close codes and the idle timeout. Part 2 (TH2 M3): `Reflect` with its
//! `ReflectAck`, one reflection queue per device slot with that slot's own `reflected-id`s,
//! ephemeral reflects, `ReflectedAck` and the redelivery of what was not acked, 4012, the backlog
//! then `ReflectionQueueDry` then `RolePromotedToLeader`, the scripted leader rule, the queue limit
//! (4114), and the hook the chat server (M4) asks before it takes an unblock. RP-b: `DropDevice` with
//! its `DropDeviceAck`, the freed slot and the 4113 of the dropped device; `Knobs::{withhold_drop_ack, close_at_next_drop,
//! register_slot_after_next_4115}`. Written from the spec
//! (`TP/md-d2m.proto`, `TP/md-d2m.struct.yml`) with the fake's own crypto; from libthreema only its
//! generated protobuf types (rule 9). The chat server inside the Proxy payloads (TH2 M4) is
//! `csp.rs`: every Proxy payload of a logged-in connection goes to it in order, and what it answers
//! goes back in Proxy payloads.
//!
//! Sans-I/O: [`Mediator::connect`] takes the URL the client built, [`Mediator::receive`] one
//! WebSocket binary message, and what the fake answers waits in the connection's outbox for
//! [`Mediator::take`] ([`Out::Datagram`] or [`Out::Close`]). One `Mediator` serves every device
//! group of a world and every connection (the 4112 of a duplicate connection reaches the OLDER
//! one).
//!
//! The server never holds a device group key: it knows a group by DGPK.public (the URL) and keeps
//! the encrypted device info as opaque bytes (the test side opens it with DGDIK).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use libthreema::protobuf::d2m;
use prost::Message as _;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{RngCore as _, SeedableRng as _};

use super::crypto::{KeyPair, hsalsa_shared, open_nonce_ahead};
use super::csp::{
    CHAT_SERVER_CLOSED, ChatServer, CspConn, CspKnobs, CspOut, Cx, DEFAULT_CSP_IDLE_TIMEOUT,
    LoginInfo, Queued, Sent, chunks, id_bytes,
};
use super::{Clock, Dir, Identities, Trace, Violations, lock};

/// The most bytes one D2M message (a container: type, reserved, payload) may have
/// (`TP/md-d2m.struct.yml` "Size Limitations": "65536 bytes for a payload container struct").
pub const MAX_DATAGRAM: usize = 65_536;
/// The container's header: type (1) and reserved (3).
pub const CONTAINER_HEADER: usize = 4;
/// The length of a challenge response: nonce (24), tag (16), challenge (32).
const RESPONSE_LENGTH: usize = 24 + 16 + 32;
/// The most bytes of an encrypted `d2d.Envelope` (`TP/md-d2m.struct.yml` "Size Limitations":
/// "65516 bytes for an encrypted `d2d.Envelope` in a `reflected` struct"): what fits a `Reflected`
/// (container 4 + header 16) in [`MAX_DATAGRAM`].
pub const MAX_ENVELOPE: usize = MAX_DATAGRAM - CONTAINER_HEADER - REFLECTED_HEADER;
/// `Reflect.header-length` today (`8`; the spec: "the byte length of all fields prior to the
/// `envelope` field").
pub const REFLECT_HEADER: usize = 8;
/// `Reflected.header-length` the fake writes (`16`).
pub const REFLECTED_HEADER: usize = 16;
/// The ephemeral marker of `Reflect.flags` / `Reflected.flags`.
pub const EPHEMERAL: u16 = 0x0001;
/// The default queue limit per slot. [U: real server] the real limit (D18) is not in the spec.
pub const DEFAULT_QUEUE_LIMIT: usize = 10_000;
/// How long a VOLATILE slot outlives its last connection. [U: real server] the spec says only
/// that such a slot is removed some time after the device disconnected.
pub const VOLATILE_KEEP: Duration = Duration::from_secs(5 * 60);
/// The MEDIATOR's own idle timeout of a connection (close 4013, `TP/md-d2m.struct.yml:98`): reset
/// by ANY frame of the client. [U: real server] the spec gives no figure for it (the 5 minutes are
/// the chat server's default, `csp.rs` `DEFAULT_CSP_IDLE_TIMEOUT`, borrowed). A second timer, the
/// CHAT SERVER's, runs beside it for a connection whose CSP login finished: it is reset only by
/// the client's CSP bytes and set by `set-connection-idle-timeout` (G2).
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// The container's payload types (`TP/md-d2m.struct.yml` `container.type`).
pub mod kind {
    pub const PROXY: u8 = 0x00;
    pub const SERVER_HELLO: u8 = 0x10;
    pub const CLIENT_HELLO: u8 = 0x11;
    pub const SERVER_INFO: u8 = 0x12;
    pub const REFLECTION_QUEUE_DRY: u8 = 0x20;
    pub const ROLE_PROMOTED_TO_LEADER: u8 = 0x21;
    pub const GET_DEVICES_INFO: u8 = 0x30;
    pub const DEVICES_INFO: u8 = 0x31;
    pub const DROP_DEVICE: u8 = 0x32;
    pub const DROP_DEVICE_ACK: u8 = 0x33;
    pub const SET_SHARED_DEVICE_DATA: u8 = 0x34;
    pub const BEGIN_TRANSACTION: u8 = 0x40;
    pub const BEGIN_TRANSACTION_ACK: u8 = 0x41;
    pub const COMMIT_TRANSACTION: u8 = 0x42;
    pub const COMMIT_TRANSACTION_ACK: u8 = 0x43;
    pub const TRANSACTION_REJECTED: u8 = 0x44;
    pub const TRANSACTION_ENDED: u8 = 0x45;
    pub const REFLECT: u8 = 0x80;
    pub const REFLECT_ACK: u8 = 0x81;
    pub const REFLECTED: u8 = 0x82;
    pub const REFLECTED_ACK: u8 = 0x83;
}

/// The trace's name of a payload type.
fn kind_name(t: u8) -> &'static str {
    match t {
        kind::PROXY => "proxy",
        kind::SERVER_HELLO => "server-hello",
        kind::CLIENT_HELLO => "client-hello",
        kind::SERVER_INFO => "server-info",
        kind::REFLECTION_QUEUE_DRY => "reflection-queue-dry",
        kind::ROLE_PROMOTED_TO_LEADER => "role-promoted-to-leader",
        kind::GET_DEVICES_INFO => "get-devices-info",
        kind::DEVICES_INFO => "devices-info",
        kind::DROP_DEVICE => "drop-device",
        kind::DROP_DEVICE_ACK => "drop-device-ack",
        kind::SET_SHARED_DEVICE_DATA => "set-shared-device-data",
        kind::BEGIN_TRANSACTION => "begin-transaction",
        kind::BEGIN_TRANSACTION_ACK => "begin-transaction-ack",
        kind::COMMIT_TRANSACTION => "commit-transaction",
        kind::COMMIT_TRANSACTION_ACK => "commit-transaction-ack",
        kind::TRANSACTION_REJECTED => "transaction-rejected",
        kind::TRANSACTION_ENDED => "transaction-ended",
        kind::REFLECT => "reflect",
        kind::REFLECT_ACK => "reflect-ack",
        kind::REFLECTED => "reflected",
        kind::REFLECTED_ACK => "reflected-ack",
        _ => "unknown",
    }
}

/// A container: `type || reserved (3 zero bytes) || payload`.
pub fn datagram(t: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(CONTAINER_HEADER + payload.len());
    out.extend_from_slice(&[t, 0, 0, 0]);
    out.extend_from_slice(payload);
    out
}

/// A container's type and payload; the reserved bytes are ignored (the spec: "ignored by the
/// receiver"). `None` under the header's length.
pub fn split(datagram: &[u8]) -> Option<(u8, &[u8])> {
    if datagram.len() < CONTAINER_HEADER {
        return None;
    }
    Some((datagram[0], &datagram[CONTAINER_HEADER..]))
}

/// What the fake sends on one connection, in order.
#[derive(Clone, PartialEq, Eq)]
pub enum Out {
    /// One WebSocket binary message.
    Datagram(Vec<u8>),
    /// The WebSocket close with this code; nothing follows.
    Close(u16),
}

impl std::fmt::Debug for Out {
    /// The type and the length only, never the bytes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Datagram(d) => write!(
                f,
                "Datagram({}, {} B)",
                d.first().map_or("empty", |t| kind_name(*t)),
                d.len()
            ),
            Self::Close(code) => write!(f, "Close({code})"),
        }
    }
}

/// One connection of the mediator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnId(pub u64);

/// Why a connect was refused before any WebSocket message (the connector reports it as a failed
/// connect). [U: real server] the real mediator's reaction to a bad URL is not in the spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectRefused {
    /// Not `wss://<host>.invalid/<prefix8>/<hex>`.
    NotTheMediatorsUrl,
    /// The last segment is not a hex `ClientUrlInfo`, or the prefixes do not match its key.
    BadClientUrlInfo,
    /// The `ClientUrlInfo` hex has an upper-case digit (`TP/md-d2m.proto:26`: "hex-encoded
    /// (lowercase)"). [I]
    UpperCaseClientUrlInfo,
    /// `{deviceGroupIdPrefix8}` is ONE hex digit although DGPK.public starts below 0x10: the form
    /// libthreema wrote before its patch P9 (the spec: "the first two hex nibbles"; Desktop writes
    /// two). Its own rule so that a failure says why (G5).
    PrefixEightHasOneDigit,
    /// No device group of this world has that DGPK.public (a wrong device group key).
    UnknownDeviceGroup,
    /// The server group is not `[0-9a-zA-Z]+` or not the group's.
    BadServerGroup,
}

impl ConnectRefused {
    /// The violation rule a refusal records.
    fn rule(self) -> &'static str {
        match self {
            Self::PrefixEightHasOneDigit => "mediator-url-prefix8-one-digit",
            Self::UpperCaseClientUrlInfo => "mediator-url-client-url-info-not-lowercase",
            _ => "mediator-connect-refused",
        }
    }
}

/// The fake's knobs (each a script call; defaults as the real server is assumed to behave).
#[derive(Clone, Debug)]
pub struct Knobs {
    /// `ServerHello.version`: the highest version the fake speaks (`V0` = 0).
    pub server_version: u32,
    /// `ServerInfo.max_device_slots`. [U: real server] the real number (Desktop's sandbox says 4).
    pub max_device_slots: u32,
    /// Added to the virtual clock in `ServerInfo.current_time` (ms, may be negative).
    pub clock_skew_ms: i64,
    /// The mediator's idle timeout of new connections (4013).
    pub idle_timeout: Duration,
    /// How many entries one slot's reflection queue may hold; one more closes the slot's
    /// connection with 4114 and deletes the slot (D18). [U: real server] the real limit.
    pub queue_limit: usize,
    /// Whether the fake picks a leader by itself: the first device of a group that finished its
    /// login, and when the leader goes, the longest connected other one (D13). Off: only
    /// [`Mediator::promote`] makes one. [U: real server] the real rule is the mediator's own.
    pub auto_leader: bool,
    /// L4: `RolePromotedToLeader` BEFORE `ReflectionQueueDry` at a login (default off: Dry first,
    /// as Desktop asserts). [U: real server] the spec does not order them; a knob so that both
    /// orders can be tested.
    pub promote_before_dry: bool,
    /// M5 (lane decisions): a test opts into LEADER MODE; until then an
    /// `unblock-incoming-messages` from any connection is a violation (TH3-TH5 never send one).
    pub leader_mode: bool,
    /// Whether the Proxy payloads reach the chat server (default on). Off: the mediator only keeps
    /// them (the D2M-only tests of M2 send Proxy payloads that are no CSP).
    pub chat_server: bool,
    /// The chat server's own knobs.
    pub csp: CspKnobs,
    /// TH3 (critic H2): HOLDS `ReflectionQueueDry` at a login, and the promotion that would follow
    /// it, until [`Mediator::release_dry`], so a test sees the client's state between the login and
    /// Dry as a STABLE state. Default off (Dry at once, as the real mediator is assumed to send it).
    pub hold_dry: bool,
    /// TH3 (critic M4): the NEXT 4115 of a slot-state mismatch also deletes the device's slot (once;
    /// the knob goes back off), so the client's one retry with EXISTING is told 4115 again: the slot
    /// "dropped in between". Default off.
    pub drop_slot_at_next_4115: bool,
    /// TH3 fix P-L9: the mediator sends NO `ServerHello` on new connections (a server that accepted
    /// the upgrade and says nothing; traced `server-hello-withheld`) until
    /// [`Mediator::release_server_hello`]. Default off.
    pub withhold_server_hello: bool,
    /// TH4 J5a (critic M1): the device of the NEXT `ClientHello` already has a slot (made just before
    /// the slot check; once, the knob goes back off), so a client that expects NEW is told 4115 on the
    /// real path, as when its drawn device id were taken. Default off.
    pub slot_exists_at_next_hello: bool,
    /// TH4 J5b (critic M3): the NEXT `ClientHello` that passes every check REGISTERS its device (the
    /// slot and its device info are kept, as the real mediator keeps them) but gets NO `ServerInfo`
    /// (traced `server-info-withheld`; once, the knob goes back off): the connection stays before its
    /// handshake's end (Proxy payloads held), so a test sees a client that is registered on the
    /// mediator and has not heard so. There is no release: what the client does next is the test.
    pub withhold_server_info: bool,
    /// TH4 P2 (critic M1's belt): the NEXT `ServerInfo` (once, the knob goes back off) says this
    /// `device_slot_state` instead of the slot's real one. SPEC-IMPOSSIBLE, BELT ONLY: the spec makes the
    /// mediator close with 4115 when a `ClientHello`'s expected state is not the slot's, so a
    /// `ServerInfo` can never carry another state than the one the device expected; this knob lets a test
    /// reach the join's check of that ("ServerInfo not NEW") anyway. [U: real server] never seen.
    pub server_info_slot_state: Option<d2m::DeviceSlotState>,
    /// TH5 A2 (decision B1): reflections the mediator queues for the NEXT device that registers a NEW
    /// slot (a join's first connection; once, the list is taken) and delivers right after its first
    /// `ServerInfo`, in order, before `ReflectionQueueDry`: the window between the join's first
    /// `ServerInfo` and its zone 2, in which a phone that is online reflects what it just got. A
    /// non-ephemeral one takes the slot's next `reflected-id`, sits in the slot's queue (so it is in
    /// `ServerInfo.reflection_queue_length`, and redelivered at the next login unless acked) with the
    /// virtual time of the `ClientHello`; an ephemeral one is sent after the backlog and never kept.
    /// See [`Mediator::reflect_during_join`]. Default empty. [U: real server] whether the real
    /// mediator counts a reflection that comes between a new slot's registration and its first
    /// `ServerInfo` in `reflection_queue_length` and sends it before Dry (the spec orders neither); this
    /// is the order a device that is online at that moment can see at worst.
    pub reflect_during_join: Vec<JoinReflect>,
    /// TH6a S3 (critic H2 and the send core's order): the mediator queues a (non-ephemeral) `Reflect`
    /// for the other devices as always but HOLDS its `ReflectAck` to the sender until
    /// [`Mediator::release_reflect_acks`]; the knob stays on, so later reflects are held too until it
    /// is set back. The order "no box before the reflect-ack" is the CLIENT's rule (the send steps
    /// await the acks, `TP/csp.struct.yml:1989`); the fake enforces nothing, a test reads the trace.
    /// [U: real server] how long the real mediator takes to ack a reflection.
    pub withhold_reflect_acks: bool,
    /// RP-b: a `DropDevice` is taken and recorded ([`Mediator::drops`], traced `drop-device-withheld`) but NOTHING
    /// follows: no ack, no freed slot, no 4113 (a mediator that went silent on the drop). The knob stays on until it
    /// is set back. Default off.
    pub withhold_drop_ack: bool,
    /// RP-b (P-M1): the NEXT `DropDevice` (once, the knob goes back to `None`) is recorded ([`Mediator::drops`], traced
    /// `drop-device-closed`) and the dropping connection is closed with this code INSTEAD of an answer: no ack, no freed
    /// slot (a mediator that ends the socket another way than 4113 / 4114). Default `None`.
    pub close_at_next_drop: Option<u16>,
    /// RP-b (P-M2): the NEXT `ClientHello` told 4115 for a device the group holds no slot for (once, the knob goes back
    /// off) ALSO registers that device's slot right after the close (traced `slot-made-after-the-close`): a NEW hello of
    /// an abandoned socket that the mediator processed late. The spec orders frames only within one connection.
    /// [U: real server] whether (and how late) the real mediator can do that. Default off.
    pub register_slot_after_next_4115: bool,
}

/// One `DropDevice` the mediator took (RP-b): the connection that sent it and the device it named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DropSeen {
    /// The connection that sent it.
    pub conn: ConnId,
    /// The device id it named.
    pub device: u64,
}

/// One `Reflect` the mediator took (TH6a S3): in order, ephemeral ones too. `Debug` prints the
/// numbers and the length only.
#[derive(Clone, PartialEq, Eq)]
pub struct ReflectSeen {
    /// The connection that sent it.
    pub conn: ConnId,
    /// The `reflect-id` the client gave it.
    pub reflect_id: u32,
    /// The reflect flags (`EPHEMERAL` or 0).
    pub flags: u16,
    /// The sealed envelope exactly as the client sent it (nonce, tag, ciphertext).
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for ReflectSeen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ReflectSeen(conn {}, id {}, flags {:#06x}, {} B)",
            self.conn.0,
            self.reflect_id,
            self.flags,
            self.bytes.len()
        )
    }
}

/// One entry of [`Knobs::reflect_during_join`]: the flags and the SEALED envelope (what the phone
/// reflects). `Debug` prints the flags and the length only.
#[derive(Clone)]
pub struct JoinReflect {
    pub flags: u16,
    pub envelope: Vec<u8>,
}

impl std::fmt::Debug for JoinReflect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "JoinReflect({:#06x}, {} B)",
            self.flags,
            self.envelope.len()
        )
    }
}

/// One `ReflectedAck` the mediator took (TH5 A2, L11): the recorded list is
/// [`Mediator::reflected_acks`], the hook [`Mediator::on_reflected_ack`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AckSeen {
    /// The connection that sent it.
    pub conn: ConnId,
    /// The device of that connection.
    pub device: u64,
    /// The `reflected-id` it named.
    pub reflected_id: u32,
    /// The virtual clock (ms since the Unix epoch) when the ack arrived.
    pub at_ms: u64,
    /// Whether the slot's queue held that id (an unknown id is a violation and a 4012).
    pub known: bool,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            server_version: d2m::ProtocolVersion::V0 as u32,
            max_device_slots: 4,
            clock_skew_ms: 0,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            queue_limit: DEFAULT_QUEUE_LIMIT,
            auto_leader: true,
            promote_before_dry: false,
            leader_mode: false,
            chat_server: true,
            csp: CspKnobs::default(),
            hold_dry: false,
            drop_slot_at_next_4115: false,
            withhold_server_hello: false,
            slot_exists_at_next_hello: false,
            withhold_server_info: false,
            server_info_slot_state: None,
            reflect_during_join: Vec::new(),
            withhold_reflect_acks: false,
            withhold_drop_ack: false,
            close_at_next_drop: None,
            register_slot_after_next_4115: false,
        }
    }
}

/// What the client's `ClientHello` said (TH3: the actor's tests read it; the enums as the spec's
/// protobuf types).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelloSeen {
    pub device: u64,
    pub expected: d2m::DeviceSlotState,
    pub exhausted: d2m::client_hello::DeviceSlotsExhaustedPolicy,
    pub expiration: d2m::DeviceSlotExpirationPolicy,
}

/// One device slot of a group.
#[derive(Clone)]
struct Slot {
    expiration: d2m::DeviceSlotExpirationPolicy,
    /// `ClientHello.encrypted_device_info`, opaque to the server.
    device_info: Vec<u8>,
    /// When the last connection of the slot ended (ms); `None` while one is open or never.
    last_disconnect_ms: Option<u64>,
    /// The reflection queue: what other devices reflected and this one has not acked, in order.
    queue: Vec<Entry>,
    /// The `reflected-id` the next entry gets (per slot, monotonic, wraps; starts at a random
    /// value so that two slots' ids differ). [U: real server] where the real ids start.
    next_reflected_id: u32,
}

/// One entry of a slot's reflection queue.
#[derive(Clone)]
struct Entry {
    reflected_id: u32,
    flags: u16,
    timestamp: u64,
    envelope: Vec<u8>,
}

/// A `Reflected` payload (header-length 16, reserved, flags, reflected-id, timestamp, envelope).
pub fn reflected_payload(
    flags: u16,
    reflected_id: u32,
    timestamp: u64,
    envelope: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(REFLECTED_HEADER + envelope.len());
    out.push(u8::try_from(REFLECTED_HEADER).expect("16 fits u8"));
    out.push(0);
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&reflected_id.to_le_bytes());
    out.extend_from_slice(&timestamp.to_le_bytes());
    out.extend_from_slice(envelope);
    out
}

/// A `ReflectAck` payload (reserved 4, reflect-id, timestamp).
pub fn reflect_ack_payload(reflect_id: u32, timestamp: u64) -> Vec<u8> {
    let mut out = vec![0u8; 4];
    out.extend_from_slice(&reflect_id.to_le_bytes());
    out.extend_from_slice(&timestamp.to_le_bytes());
    out
}

/// A `Reflect` payload's flags, reflect-id and envelope (the fake's own decoder: header-length at
/// least 8, the known fields at fixed offsets, the envelope after `header-length` bytes).
pub fn parse_reflect(payload: &[u8]) -> Option<(u16, u32, &[u8])> {
    let header = usize::from(*payload.first()?);
    if header < REFLECT_HEADER || payload.len() < header {
        return None;
    }
    let flags = u16::from_le_bytes(payload[2..4].try_into().ok()?);
    let id = u32::from_le_bytes(payload[4..8].try_into().ok()?);
    Some((flags, id, &payload[header..]))
}

struct Group {
    server_group: String,
    slots: BTreeMap<u64, Slot>,
    /// What `SetSharedDeviceData` stored last (D17, later): empty for a fresh group.
    shared_device_data: Vec<u8>,
    /// The one connection of the group that is leader (D13), if any.
    leader: Option<ConnId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    AwaitingClientHello,
    PostHandshake,
    Closed,
}

struct Conn {
    group: [u8; 32],
    esk: KeyPair,
    challenge: [u8; 32],
    phase: Phase,
    device: Option<u64>,
    out: Vec<Out>,
    /// The code this connection was closed with, by either side.
    closed: Option<u16>,
    /// The last frame of ANY kind from the client (the mediator's idle timer).
    last_frame_ms: u64,
    idle_timeout: Duration,
    /// The last Proxy bytes from the client, i.e. the last CSP traffic the chat server saw (its idle
    /// timer: D2M frames do not count, the chat server never sees them).
    last_csp_ms: u64,
    /// The chat server's idle timeout of this connection: CSP's default until the client sends
    /// `set-connection-idle-timeout` (seconds).
    csp_idle_timeout: Duration,
    /// Proxy payloads that came before the handshake ended: held, then handed on in order.
    /// [U: real server] whether the real mediator holds a Proxy before the ClientHello (Desktop
    /// sends its CSP client-hello that early, `layer-3-codec.ts:931-958`).
    held_proxy: Vec<Vec<u8>>,
    /// Proxy payloads for the chat server, in order (M4 hands them to the CSP server).
    proxied: Vec<Vec<u8>>,
    /// The reflect-ids this connection used ("unique per connection").
    reflect_ids: std::collections::BTreeSet<u32>,
    /// The chat server's side of this connection.
    csp: CspConn,
    /// The client's `ClientHello`, once it came and parsed.
    hello: Option<HelloSeen>,
    /// `Knobs::hold_dry`: Dry is owed to this connection (sent by `release_dry`), and whether the
    /// promotion that would have followed it is owed too.
    dry_held: bool,
    promote_owed: bool,
    /// TH3 (critic M4): [`Mediator::black_hole`]: the connection takes the client's frames and
    /// does nothing with them, sends nothing and runs no timer (a link that went silent).
    black_hole: bool,
    /// TH3 fix P-L7/P-L9 (`CspKnobs::withhold_login_ack`): the chat server's Proxy bytes from the
    /// login-ack on are held in `login_held` until [`Mediator::release_login_ack`].
    login_hold: bool,
    login_held: Vec<Vec<u8>>,
    /// `Knobs::withhold_server_hello`: the `ServerHello` of this connection, sent by
    /// [`Mediator::release_server_hello`].
    hello_held: Option<Vec<u8>>,
    /// TH6a S3 (`Knobs::withhold_reflect_acks`): the `ReflectAck`s owed to this connection, in order
    /// (the reflect-id and the payload as it was made when the reflect was stored).
    reflect_acks_held: Vec<(u32, Vec<u8>)>,
}

struct State {
    clock: Clock,
    violations: Violations,
    trace: Trace,
    rng: ChaCha20Rng,
    knobs: Knobs,
    groups: BTreeMap<[u8; 32], Group>,
    conns: BTreeMap<ConnId, Conn>,
    next_conn: u64,
    chat: ChatServer,
    /// Every `ReflectedAck` taken, in order (TH5 A2).
    acks: Vec<AckSeen>,
    /// The hook of [`Mediator::on_reflected_ack`] and the acks it has not been told yet (called after
    /// the mediator's lock is released, so it may read the mediator).
    ack_hook: Option<AckHook>,
    ack_calls: Vec<AckSeen>,
    /// Every `Reflect` taken, in order (TH6a S3).
    reflects: Vec<ReflectSeen>,
    /// Every `DropDevice` taken, in order (RP-b).
    drops: Vec<DropSeen>,
}

/// The test's callback on every `ReflectedAck`.
type AckHook = Arc<dyn Fn(&AckSeen) + Send + Sync>;

/// The fake mediator of one world: a cloneable handle (the connector of M5 holds one per
/// connection task).
#[derive(Clone)]
pub struct Mediator(Arc<Mutex<State>>);

impl std::fmt::Debug for Mediator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Mediator")
    }
}

/// Whether `group` has the form of a server group the MEDIATOR takes: `^[0-9a-zA-Z]+$`
/// (`TP/md-d2m.proto:37-39`, `ClientUrlInfo.server_group`, what the mediator itself parses). The
/// join protocol (`md-d2d-join.proto:246`) and libthreema's `ChatServerGroup` also allow `-`; the
/// pinned spec contradicts itself, so the fake follows the mediator's own rule.
/// [U: real server] whether the real mediator refuses a hyphen; a test documents the refusal
/// (`mock::d2m::a_server_group_with_a_hyphen_is_refused`) for RT1 to check.
pub fn is_server_group(group: &str) -> bool {
    !group.is_empty() && group.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// The `ClientUrlInfo` of a mediator URL `wss://<name>-<prefix4>.invalid/<prefix8>/<hex>` (D1;
/// `TP/md-d2m.proto:26-40`), with both prefixes checked against its device group id.
pub fn client_url_info(url: &str) -> Result<d2m::ClientUrlInfo, ConnectRefused> {
    let rest = url
        .strip_prefix("wss://")
        .ok_or(ConnectRefused::NotTheMediatorsUrl)?;
    let (host, path) = rest
        .split_once('/')
        .ok_or(ConnectRefused::NotTheMediatorsUrl)?;
    let name = host
        .strip_suffix(".invalid")
        .ok_or(ConnectRefused::NotTheMediatorsUrl)?;
    let (prefix8, hex) = path
        .split_once('/')
        .ok_or(ConnectRefused::NotTheMediatorsUrl)?;
    if hex.is_empty() || hex.contains('/') {
        return Err(ConnectRefused::NotTheMediatorsUrl);
    }
    // [I] `TP/md-d2m.proto:26`: "hex-encoded (lowercase)".
    if hex.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err(ConnectRefused::UpperCaseClientUrlInfo);
    }
    let bytes = unhex(hex).ok_or(ConnectRefused::BadClientUrlInfo)?;
    let info = d2m::ClientUrlInfo::decode(bytes.as_slice())
        .map_err(|_| ConnectRefused::BadClientUrlInfo)?;
    let first = *info
        .device_group_id
        .first()
        .ok_or(ConnectRefused::BadClientUrlInfo)?;
    let prefix4 = name.rsplit('-').next().unwrap_or_default();
    if info.device_group_id.len() != 32 || prefix4 != format!("{:x}", first >> 4) {
        return Err(ConnectRefused::BadClientUrlInfo);
    }
    // `{deviceGroupIdPrefix8}`: the first two hex nibbles (`oppf.schema.yml`; Desktop's
    // `slice(0, 2)`; libthreema since its patch P9). [U: real server] which form the real mediator
    // routes: Desktop's, the one the official client sends, is assumed.
    if prefix8 != format!("{first:02x}") {
        return Err(if first < 0x10 && prefix8 == format!("{first:x}") {
            ConnectRefused::PrefixEightHasOneDigit
        } else {
            ConnectRefused::BadClientUrlInfo
        });
    }
    Ok(info)
}

impl Mediator {
    /// A mediator on the world's clock, violations and trace, with its own RNG from `seed`, and
    /// its chat server with the permanent key pair `sk` and the world's identity table.
    pub fn new(
        clock: Clock,
        violations: Violations,
        trace: Trace,
        seed: u64,
        sk: KeyPair,
        identities: Identities,
    ) -> Self {
        Self(Arc::new(Mutex::new(State {
            clock,
            violations,
            trace,
            rng: ChaCha20Rng::seed_from_u64(seed),
            knobs: Knobs::default(),
            groups: BTreeMap::new(),
            conns: BTreeMap::new(),
            next_conn: 1,
            chat: ChatServer::new(identities, sk),
            acks: Vec::new(),
            ack_hook: None,
            ack_calls: Vec::new(),
            reflects: Vec::new(),
            drops: Vec::new(),
        })))
    }

    /// Changes the knobs (for the connections that start afterwards, and for the next
    /// `ServerInfo`).
    pub fn knobs(&self, change: impl FnOnce(&mut Knobs)) {
        change(&mut lock(&self.0).knobs);
    }

    /// Makes the device group known by `path_public` (DGPK.public) and its chat server group.
    #[track_caller]
    pub fn add_group(&self, path_public: [u8; 32], server_group: &str) {
        // Any non-empty group: a test may add one the mediator's rule refuses (a hyphen) to
        // document the refusal at the connect.
        assert!(!server_group.is_empty(), "a test's server group");
        let fresh = lock(&self.0)
            .groups
            .insert(
                path_public,
                Group {
                    server_group: server_group.to_owned(),
                    slots: BTreeMap::new(),
                    shared_device_data: Vec::new(),
                    leader: None,
                },
            )
            .is_none();
        assert!(fresh, "a device group was added twice");
    }

    /// A WebSocket connect to `url`: refused, or a new connection whose `ServerHello` waits in its
    /// outbox.
    pub fn connect(&self, url: &str) -> Result<ConnId, ConnectRefused> {
        let mut s = lock(&self.0);
        let refused = |s: &State, why: ConnectRefused| {
            s.violations.record(why.rule(), &[]);
            Err(why)
        };
        let info = match client_url_info(url) {
            Ok(info) => info,
            Err(why) => return refused(&s, why),
        };
        let mut group_id = [0u8; 32];
        group_id.copy_from_slice(&info.device_group_id);
        // [U: real server] the real mediator probably makes a group on its first connect; the
        // fake knows only the groups a test added, so a wrong DGK shows here.
        let Some(group) = s.groups.get(&group_id) else {
            return refused(&s, ConnectRefused::UnknownDeviceGroup);
        };
        // [U: real server] whether the mediator compares the server group with anything.
        if !is_server_group(&info.server_group) || info.server_group != group.server_group {
            return refused(&s, ConnectRefused::BadServerGroup);
        }
        let id = ConnId(s.next_conn);
        s.next_conn += 1;
        let mut esk = [0u8; 32];
        s.rng.fill_bytes(&mut esk);
        let mut challenge = [0u8; 32];
        s.rng.fill_bytes(&mut challenge);
        let esk = KeyPair::from_secret(esk);
        let now = s.clock.now_ms();
        let idle_timeout = s.knobs.idle_timeout;
        let hello = d2m::ServerHello {
            version: s.knobs.server_version,
            esk: esk.public.to_vec(),
            challenge: challenge.to_vec(),
        };
        s.conns.insert(
            id,
            Conn {
                group: group_id,
                esk,
                challenge,
                phase: Phase::AwaitingClientHello,
                device: None,
                out: Vec::new(),
                closed: None,
                last_frame_ms: now,
                idle_timeout,
                last_csp_ms: now,
                csp_idle_timeout: DEFAULT_CSP_IDLE_TIMEOUT,
                held_proxy: Vec::new(),
                proxied: Vec::new(),
                reflect_ids: std::collections::BTreeSet::new(),
                csp: CspConn::new(),
                hello: None,
                dry_held: false,
                promote_owed: false,
                black_hole: false,
                login_hold: false,
                login_held: Vec::new(),
                hello_held: None,
                reflect_acks_held: Vec::new(),
            },
        );
        if s.knobs.withhold_server_hello {
            s.conn(id).hello_held = Some(hello.encode_to_vec());
            s.trace.frame(Dir::Out, id.0, "server-hello-withheld", &[]);
        } else {
            s.send(id, kind::SERVER_HELLO, &hello.encode_to_vec(), &[]);
        }
        Ok(id)
    }

    /// One WebSocket binary message from the client of `conn`. A message to a closed connection is
    /// dropped (it crossed the close).
    pub fn receive(&self, conn: ConnId, datagram: Vec<u8>) {
        let (hook, calls) = {
            let mut s = lock(&self.0);
            receive_locked(&mut s, conn, datagram);
            (s.ack_hook.clone(), std::mem::take(&mut s.ack_calls))
        };
        // TH5 A2: the test's hook runs with the lock released (it may read the mediator). An ack reaches
        // `reflected_acks` only AFTER its hook returned: the hook runs on the client's thread and may be slow
        // (it opens the client's file), and a test that waited for the ack and then read what the hook
        // recorded found nothing (`connection::reflect::a_reflected_text_is_in_the_file_when_its_ack_comes`,
        // two strikes in TH5a: `target/threema/th5a/flake-ack2.log`).
        if let Some(hook) = hook {
            for call in &calls {
                hook(call);
            }
        }
        if !calls.is_empty() {
            lock(&self.0).acks.extend(calls);
        }
    }

    /// The client of `conn` closed the WebSocket with `code`. The client's own codes the spec and
    /// Desktop use are fine: 1000 (a normal close), 4010 (protocol error), 4013 (its idle timeout)
    /// and 4110 (`TP/md-d2m.struct.yml:71-73`: the client disconnects with 4110 when the server's
    /// version is unsupported; Desktop does, `layer-3-codec.ts:782-786`). Any other code is a
    /// violation (D8). [U: real server] whether the real mediator minds another client code; the
    /// spec names no other.
    pub fn client_close(&self, conn: ConnId, code: u16) {
        let mut s = lock(&self.0);
        if s.conns.get(&conn).is_none_or(|c| c.phase == Phase::Closed) {
            return;
        }
        s.trace
            .frame(Dir::In, conn.0, "close", &[("code", u64::from(code))]);
        if !matches!(code, 1000 | 4010 | 4013 | 4110) {
            s.violations
                .record("client-close-code", &[("code", u64::from(code))]);
        }
        s.end(conn, code, false);
    }

    /// The client of `conn` went away WITHOUT a close frame (a dead TCP connection, a killed client:
    /// the EOF of a child's pipe in the parent-resident fake, TH5a A3, `pipe.rs`): the connection
    /// ends with 1006, the code no frame carries, as a real server sees it. Not a violation: a
    /// client cannot close cleanly when it dies.
    pub fn lost(&self, conn: ConnId) {
        let mut s = lock(&self.0);
        if s.conns.get(&conn).is_none_or(|c| c.phase == Phase::Closed) {
            return;
        }
        s.trace.frame(Dir::In, conn.0, "lost", &[("code", 1006)]);
        s.end(conn, 1006, false);
    }

    /// The fake closes `conn` with `code` (a script call; any code of D8 or another). 4113
    /// ("dropped by another device", the end of a `DropDevice`, whose ack says the slot "has been
    /// free'd", `TP/md-d2m.proto:224-229`) and 4114 ("dropped by the server") free the device's slot
    /// like the real ones do, so a reconnect with EXISTING is told 4115 (G11). [U: real server]
    /// whether the slot goes at once.
    pub fn close(&self, conn: ConnId, code: u16) {
        let mut s = lock(&self.0);
        let slot = s
            .conns
            .get(&conn)
            .and_then(|c| c.device.map(|d| (c.group, d)));
        s.end(conn, code, true);
        if matches!(code, 4113 | 4114)
            && let Some((group, device)) = slot
        {
            let freed = s
                .groups
                .get_mut(&group)
                .is_some_and(|g| g.slots.remove(&device).is_some());
            if freed {
                s.trace.frame(
                    Dir::Out,
                    conn.0,
                    "slot-deleted-close",
                    &[("device", device), ("code", u64::from(code))],
                );
            }
        }
    }

    /// What the fake sent on `conn` since the last call, in order.
    pub fn take(&self, conn: ConnId) -> Vec<Out> {
        lock(&self.0)
            .conns
            .get_mut(&conn)
            .map(|c| std::mem::take(&mut c.out))
            .unwrap_or_default()
    }

    /// Moves the virtual clock and runs the fake's timers (idle 4013, VOLATILE slots).
    pub fn advance(&self, by: Duration) {
        let s = lock(&self.0);
        s.clock.advance(by);
        drop(s);
        self.tick();
    }

    /// Runs the fake's timers at the clock's time.
    pub fn tick(&self) {
        let mut s = lock(&self.0);
        let now = s.clock.now_ms();
        let idle: Vec<ConnId> = s
            .conns
            .iter()
            .filter(|(_, c)| {
                c.phase != Phase::Closed
                    && !c.black_hole
                    && now.saturating_sub(c.last_frame_ms) >= millis(c.idle_timeout)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in idle {
            s.end(id, 4013, true);
        }
        // G2: the chat server's own timer, for a connection whose CSP login finished: only CSP
        // bytes reset it (D2M frames never reach the chat server), and when it runs out the CSP leg
        // ends and the mediator closes the connection. [U: real server] the close code (4000
        // "chat server connection closed" is taken) and which timer wins when both end together
        // (the mediator's).
        let csp_idle: Vec<ConnId> = s
            .conns
            .iter()
            .filter(|(_, c)| {
                c.phase == Phase::PostHandshake
                    && !c.black_hole
                    && c.csp.logged_in()
                    && now.saturating_sub(c.last_csp_ms) >= millis(c.csp_idle_timeout)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in csp_idle {
            s.trace.frame(Dir::Out, id.0, "csp-idle-timeout", &[]);
            s.end(id, CHAT_SERVER_CLOSED, true);
        }
        s.chat.expire(now);
        let keep = millis(VOLATILE_KEEP);
        // G1: a slot with an open connection stays whatever its time says.
        let open: std::collections::BTreeSet<([u8; 32], u64)> = s
            .conns
            .values()
            .filter(|c| c.phase != Phase::Closed)
            .filter_map(|c| c.device.map(|d| (c.group, d)))
            .collect();
        for (group_id, group) in &mut s.groups {
            group.slots.retain(|device, slot| {
                slot.expiration != d2m::DeviceSlotExpirationPolicy::Volatile
                    || open.contains(&(*group_id, *device))
                    || slot
                        .last_disconnect_ms
                        .is_none_or(|t| now.saturating_sub(t) < keep)
            });
        }
    }

    /// Sets the MEDIATOR's idle timeout of one connection (the 4013 timer, reset by every frame).
    /// The chat server's timer is set by the client's `set-connection-idle-timeout`.
    pub fn set_idle_timeout(&self, conn: ConnId, timeout: Duration) {
        if let Some(c) = lock(&self.0).conns.get_mut(&conn) {
            c.idle_timeout = timeout;
        }
    }

    /// The chat server's idle timeout of `conn` (CSP's default, or what the client asked for).
    pub fn csp_idle_timeout(&self, conn: ConnId) -> Option<Duration> {
        lock(&self.0).conns.get(&conn).map(|c| c.csp_idle_timeout)
    }

    /// The code `conn` was closed with, by either side.
    pub fn closed(&self, conn: ConnId) -> Option<u16> {
        lock(&self.0).conns.get(&conn).and_then(|c| c.closed)
    }

    /// Whether `conn` finished the handshake and is open.
    pub fn is_online(&self, conn: ConnId) -> bool {
        lock(&self.0)
            .conns
            .get(&conn)
            .is_some_and(|c| c.phase == Phase::PostHandshake)
    }

    /// The device id `conn` logged in as.
    pub fn device_of(&self, conn: ConnId) -> Option<u64> {
        lock(&self.0).conns.get(&conn).and_then(|c| c.device)
    }

    /// The device ids that hold a slot in the group of `path_public`.
    pub fn slots(&self, path_public: &[u8; 32]) -> Vec<u64> {
        lock(&self.0)
            .groups
            .get(path_public)
            .map(|g| g.slots.keys().copied().collect())
            .unwrap_or_default()
    }

    /// A script call (TH4 J5b): the slot of `device` in the group of `path_public` is gone while no
    /// connection of the device is open (another device dropped it, or the server did, while this one
    /// was offline), so the device's next connect that expects EXISTING is told 4115. Whether there was
    /// one.
    #[track_caller]
    pub fn drop_slot(&self, path_public: &[u8; 32], device: u64) -> bool {
        let mut s = lock(&self.0);
        let open = s.conns.values().any(|c| {
            &c.group == path_public && c.device == Some(device) && c.phase != Phase::Closed
        });
        assert!(!open, "a test dropped the slot of a connected device");
        s.groups
            .get_mut(path_public)
            .is_some_and(|g| g.slots.remove(&device).is_some())
    }

    /// The encrypted device info of a slot (opaque here; the test opens it with DGDIK).
    pub fn device_info(&self, path_public: &[u8; 32], device: u64) -> Option<Vec<u8>> {
        lock(&self.0)
            .groups
            .get(path_public)
            .and_then(|g| g.slots.get(&device))
            .map(|s| s.device_info.clone())
    }

    /// The Proxy payloads of `conn` for the chat server, in order (held ones first).
    pub fn proxied(&self, conn: ConnId) -> Vec<Vec<u8>> {
        lock(&self.0)
            .conns
            .get(&conn)
            .map(|c| c.proxied.clone())
            .unwrap_or_default()
    }

    /// How many Proxy payloads of `conn` wait for the end of its handshake.
    pub fn held_proxy_count(&self, conn: ConnId) -> usize {
        lock(&self.0)
            .conns
            .get(&conn)
            .map_or(0, |c| c.held_proxy.len())
    }

    /// Makes `conn` the leader of its group and sends it `RolePromotedToLeader` (a script call,
    /// D13). A group has one leader at a time: a test that promotes while another connection
    /// leads fails.
    #[track_caller]
    pub fn promote(&self, conn: ConnId) {
        let mut s = lock(&self.0);
        let c = &s.conns[&conn];
        assert!(
            c.phase == Phase::PostHandshake,
            "only a logged-in connection can lead"
        );
        let group = c.group;
        let current = s.groups[&group].leader;
        assert!(
            current.is_none() || current == Some(conn),
            "a group has one leader at a time: close the leader first"
        );
        s.make_leader(group, conn);
    }

    /// `Knobs::hold_dry`: sends `conn` the Dry it is owed, then the promotion that would have
    /// followed it when the group still has no leader. Whether a Dry was held (`false` for a
    /// connection that got its Dry, or is closed).
    pub fn release_dry(&self, conn: ConnId) -> bool {
        let mut s = lock(&self.0);
        let Some(c) = s.conns.get_mut(&conn) else {
            return false;
        };
        if !c.dry_held || c.phase != Phase::PostHandshake {
            return false;
        }
        c.dry_held = false;
        let (promote, group) = (std::mem::take(&mut c.promote_owed), c.group);
        s.send(conn, kind::REFLECTION_QUEUE_DRY, &[], &[]);
        if promote && s.groups[&group].leader.is_none() {
            s.make_leader(group, conn);
        }
        true
    }

    /// TH5 A2 (decision B1): adds one reflection to [`Knobs::reflect_during_join`]: `sealed` is an
    /// envelope sealed under DGRK (`Phone::seal`), `flags` the reflect flags (`EPHEMERAL` or 0). The NEXT
    /// device that registers a NEW slot gets the list right after its first `ServerInfo`, in the order
    /// added, before Dry.
    pub fn reflect_during_join(&self, flags: u16, sealed: Vec<u8>) {
        lock(&self.0).knobs.reflect_during_join.push(JoinReflect {
            flags,
            envelope: sealed,
        });
    }

    /// TH5 A2 (L11): every `ReflectedAck` the mediator took so far, in order, each with the virtual
    /// time it arrived at and whether the slot's queue held the id. An ack shows here only after the
    /// hook of [`Mediator::on_reflected_ack`] returned for it.
    pub fn reflected_acks(&self) -> Vec<AckSeen> {
        lock(&self.0).acks.clone()
    }

    /// TH5 A2 (L11): the hook. `hook` runs once per `ReflectedAck`, in order, right after the
    /// mediator processed it and with its lock released (the hook may read the mediator, a file or the
    /// world's clock; it must not block). Replaces an earlier hook. This is how a test sees the state
    /// of the client's file at the moment of the ack ("commit, then ack"). The list
    /// [`Mediator::reflected_acks`] is kept either way.
    pub fn on_reflected_ack(&self, hook: impl Fn(&AckSeen) + Send + Sync + 'static) {
        lock(&self.0).ack_hook = Some(Arc::new(hook));
    }

    /// `Knobs::withhold_server_hello`: sends `conn` the `ServerHello` it was owed. Whether one was
    /// held.
    pub fn release_server_hello(&self, conn: ConnId) -> bool {
        let mut s = lock(&self.0);
        let held = s.conns.get_mut(&conn).and_then(|c| c.hello_held.take());
        held.is_some_and(|hello| {
            s.send(conn, kind::SERVER_HELLO, &hello, &[]);
            true
        })
    }

    /// `CspKnobs::withhold_login_ack`: sends `conn` what the chat server held from the login-ack on,
    /// in order, and stops holding. Whether anything was held.
    pub fn release_login_ack(&self, conn: ConnId) -> bool {
        let mut s = lock(&self.0);
        let Some(c) = s.conns.get_mut(&conn) else {
            return false;
        };
        let was_held = std::mem::take(&mut c.login_hold);
        let held = std::mem::take(&mut c.login_held);
        for bytes in held {
            s.send_proxy(conn, bytes);
        }
        was_held
    }

    /// TH6a S3 (`Knobs::withhold_reflect_acks`): sends `conn` the `ReflectAck`s it was owed, in
    /// order, each with the timestamp of the moment its reflect was stored. How many went. The knob
    /// itself is not changed.
    pub fn release_reflect_acks(&self, conn: ConnId) -> usize {
        let mut s = lock(&self.0);
        let Some(c) = s.conns.get_mut(&conn) else {
            return 0;
        };
        let held = std::mem::take(&mut c.reflect_acks_held);
        let count = held.len();
        for (reflect_id, ack) in held {
            s.send(
                conn,
                kind::REFLECT_ACK,
                &ack,
                &[("reflect-id", u64::from(reflect_id))],
            );
        }
        count
    }

    /// TH6a S3: how many `ReflectAck`s `conn` is owed (`Knobs::withhold_reflect_acks`).
    pub fn reflect_acks_held(&self, conn: ConnId) -> usize {
        lock(&self.0)
            .conns
            .get(&conn)
            .map_or(0, |c| c.reflect_acks_held.len())
    }

    /// TH6a S3: every `Reflect` the mediator took, in order, ephemeral ones too: the connection, the
    /// client's reflect-id, the flags and the sealed envelope as sent. A test compares the bytes of a
    /// repeat with the first.
    pub fn reflects(&self) -> Vec<ReflectSeen> {
        lock(&self.0).reflects.clone()
    }

    /// RP-b: every `DropDevice` the mediator took, in order (withheld ones too): the connection and the device named.
    pub fn drops(&self) -> Vec<DropSeen> {
        lock(&self.0).drops.clone()
    }

    /// What the `ClientHello` of `conn` said, if one came and parsed.
    pub fn hello(&self, conn: ConnId) -> Option<HelloSeen> {
        lock(&self.0).conns.get(&conn).and_then(|c| c.hello)
    }

    /// The leader of the group of `path_public`.
    pub fn leader(&self, path_public: &[u8; 32]) -> Option<ConnId> {
        lock(&self.0).groups.get(path_public).and_then(|g| g.leader)
    }

    /// Whether `conn` is its group's leader.
    pub fn is_leader(&self, conn: ConnId) -> bool {
        let s = lock(&self.0);
        s.conns
            .get(&conn)
            .is_some_and(|c| s.groups.get(&c.group).and_then(|g| g.leader) == Some(conn))
    }

    /// D14, the chat server's hook (M4): whether `conn` may unblock incoming messages: only the
    /// leader, and only in a test that opted into leader mode (M5 of the lane decisions).
    pub fn unblock_allowed(&self, conn: ConnId) -> bool {
        lock(&self.0).knobs.leader_mode && self.is_leader(conn)
    }

    /// How many entries the reflection queue of `device` in the group of `path_public` holds.
    pub fn queue_len(&self, path_public: &[u8; 32], device: u64) -> Option<usize> {
        lock(&self.0)
            .groups
            .get(path_public)
            .and_then(|g| g.slots.get(&device))
            .map(|s| s.queue.len())
    }

    /// TH3 (critic M4): `conn` goes silent, a black hole: from now on the client's frames are taken
    /// and dropped (traced `black-holed`), nothing more is sent on it and neither idle timer
    /// closes it; a close of the CLIENT still ends it. A link that died without a close, for the
    /// keep-alive and the `Resumed` staleness rule.
    pub fn black_hole(&self, conn: ConnId) {
        let mut s = lock(&self.0);
        let Some(c) = s.conns.get_mut(&conn) else {
            return;
        };
        c.black_hole = true;
        s.trace.frame(Dir::Out, conn.0, "black-hole", &[]);
    }

    /// Sends a raw datagram to `conn` (a script call for what the client must refuse).
    pub fn send_raw(&self, conn: ConnId, datagram: Vec<u8>) {
        let mut s = lock(&self.0);
        let t = datagram.first().copied().unwrap_or(0xff);
        s.push(conn, t, datagram, &[]);
    }
}

/// An 8-byte ID of the wire as text (the fake's table writes IDs as ASCII).
fn id_text(id: &[u8; 8]) -> String {
    String::from_utf8_lossy(id).into_owned()
}

fn len(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).expect("a length fits u64")
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).expect("a timeout fits u64 ms")
}

/// One WebSocket binary message from the client of `conn`, with the lock held ([`Mediator::receive`]).
fn receive_locked(s: &mut State, conn: ConnId, datagram: Vec<u8>) {
    let now = s.clock.now_ms();
    let Some(c) = s.conns.get_mut(&conn) else {
        return;
    };
    if c.phase == Phase::Closed {
        return;
    }
    if c.black_hole {
        s.trace
            .frame(Dir::In, conn.0, "black-holed", &[("len", len(&datagram))]);
        return;
    }
    c.last_frame_ms = now;
    if datagram.len() > MAX_DATAGRAM {
        s.trace
            .frame(Dir::In, conn.0, "oversized", &[("len", len(&datagram))]);
        s.protocol_error(conn, "d2m-message-over-65536-bytes");
        return;
    }
    let Some((t, payload)) = split(&datagram) else {
        s.trace
            .frame(Dir::In, conn.0, "short", &[("len", len(&datagram))]);
        s.protocol_error(conn, "d2m-message-under-the-header");
        return;
    };
    s.trace
        .frame(Dir::In, conn.0, kind_name(t), &[("len", len(payload))]);
    let phase = s.conns[&conn].phase;
    match (phase, t) {
        (Phase::AwaitingClientHello, kind::PROXY) => {
            s.conn(conn).held_proxy.push(payload.to_vec());
        }
        (Phase::AwaitingClientHello, kind::CLIENT_HELLO) => {
            let payload = payload.to_vec();
            s.client_hello(conn, &payload);
        }
        (Phase::PostHandshake, kind::PROXY) => {
            let payload = payload.to_vec();
            s.conn(conn).proxied.push(payload.clone());
            s.hand_to_chat_server(conn, &payload);
        }
        (Phase::PostHandshake, kind::REFLECT) => {
            let payload = payload.to_vec();
            s.reflect(conn, &payload);
        }
        (Phase::PostHandshake, kind::REFLECTED_ACK) => {
            let payload = payload.to_vec();
            s.reflected_ack(conn, &payload);
        }
        // RP-b: the own device's drop (and a later step's drop of another device).
        (Phase::PostHandshake, kind::DROP_DEVICE) => {
            let payload = payload.to_vec();
            s.drop_device(conn, &payload);
        }
        // The client's post-handshake payloads that later steps of the fake serve
        // (devices, shared data: TH10; transactions: TH6).
        (
            Phase::PostHandshake,
            kind::GET_DEVICES_INFO
            | kind::SET_SHARED_DEVICE_DATA
            | kind::BEGIN_TRANSACTION
            | kind::COMMIT_TRANSACTION,
        ) => {
            s.protocol_error(conn, "d2m-payload-not-in-the-fake-yet");
        }
        _ => s.protocol_error(conn, "d2m-unexpected-payload-type"),
    }
}

impl State {
    fn conn(&mut self, id: ConnId) -> &mut Conn {
        self.conns
            .get_mut(&id)
            .expect("a connection of this mediator")
    }

    /// Queues one datagram of type `t` on `conn` (never more than the limit: that would be the
    /// fake's own bug).
    fn send(&mut self, conn: ConnId, t: u8, payload: &[u8], ids: &[(&'static str, u64)]) {
        self.push(conn, t, datagram(t, payload), ids);
    }

    fn push(&mut self, conn: ConnId, t: u8, datagram: Vec<u8>, ids: &[(&'static str, u64)]) {
        assert!(
            datagram.len() <= MAX_DATAGRAM,
            "the fake built a D2M message over the limit"
        );
        if self
            .conns
            .get(&conn)
            .is_none_or(|c| c.phase == Phase::Closed || c.black_hole)
        {
            return;
        }
        let mut traced = ids.to_vec();
        traced.push(("len", len(&datagram)));
        self.trace.frame(Dir::Out, conn.0, kind_name(t), &traced);
        self.conn(conn).out.push(Out::Datagram(datagram));
    }

    /// Ends `conn` with `code`; `by_server` queues the close for the client. The slot keeps the
    /// time of the disconnect.
    fn end(&mut self, conn: ConnId, code: u16, by_server: bool) {
        let now = self.clock.now_ms();
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        if c.phase == Phase::Closed {
            return;
        }
        c.phase = Phase::Closed;
        c.closed = Some(code);
        if by_server {
            c.out.push(Out::Close(code));
        }
        let (group, device) = (c.group, c.device);
        if by_server {
            self.trace
                .frame(Dir::Out, conn.0, "close", &[("code", u64::from(code))]);
        }
        // G1: the slot's time of disconnect is set only when no other open connection of the device
        // remains (after a 4112 the newer connection of the same device holds the slot).
        let device_still_open = device.is_some_and(|d| {
            self.conns.iter().any(|(id, o)| {
                *id != conn && o.group == group && o.device == Some(d) && o.phase != Phase::Closed
            })
        });
        if let Some(device) = device
            && !device_still_open
            && let Some(slot) = self
                .groups
                .get_mut(&group)
                .and_then(|g| g.slots.get_mut(&device))
        {
            slot.last_disconnect_ms = Some(now);
        }
        let Some(g) = self.groups.get_mut(&group) else {
            return;
        };
        if g.leader != Some(conn) {
            return;
        }
        g.leader = None;
        // D13: the next connected device of the group leads (its Dry went at its login).
        // [U: real server] who the real mediator promotes, and when: after a 4112 of the LEADER's
        // older connection the leadership goes to another device, not to the same device's new
        // connection (it is still before its ServerInfo here).
        if self.knobs.auto_leader
            && let Some(next) = self.longest_connected(group)
        {
            self.make_leader(group, next);
        }
    }

    /// The group's logged-in connection that came first.
    fn longest_connected(&self, group: [u8; 32]) -> Option<ConnId> {
        self.conns
            .iter()
            .find(|(_, c)| c.group == group && c.phase == Phase::PostHandshake)
            .map(|(id, _)| *id)
    }

    /// The open connection of `device` in `group`, logged in.
    fn online_conn(&self, group: [u8; 32], device: u64) -> Option<ConnId> {
        self.conns
            .iter()
            .find(|(_, c)| {
                c.group == group && c.device == Some(device) && c.phase == Phase::PostHandshake
            })
            .map(|(id, _)| *id)
    }

    fn make_leader(&mut self, group: [u8; 32], conn: ConnId) {
        self.groups.get_mut(&group).expect("a known group").leader = Some(conn);
        self.send(conn, kind::ROLE_PROMOTED_TO_LEADER, &[], &[]);
    }

    /// D10: a `Reflect` from `conn`: into the queue of every other slot of the group (offline ones
    /// too) with that slot's next `reflected-id`, sent at once to a slot that is online, and acked
    /// to the sender with the time it was stored. An ephemeral one goes only to the devices online
    /// now, is never queued and never acked.
    fn reflect(&mut self, conn: ConnId, payload: &[u8]) {
        let Some((flags, reflect_id, envelope)) = parse_reflect(payload) else {
            self.protocol_error(conn, "reflect-malformed");
            return;
        };
        if envelope.len() > MAX_ENVELOPE {
            // [U: real server] what the mediator does with an envelope that no Reflected can carry.
            self.protocol_error(conn, "reflect-envelope-over-65516-bytes");
            return;
        }
        if !self.conn(conn).reflect_ids.insert(reflect_id) {
            // [U: real server] the code for a repeated reflect-id.
            self.protocol_error(conn, "reflect-id-repeated");
            return;
        }
        let (group, sender) = {
            let c = &self.conns[&conn];
            (
                c.group,
                c.device.expect("a logged-in connection has a device"),
            )
        };
        // G1: a logged-in device always holds its slot; if it does not, the fake lost it (or a
        // test did): a violation, not a panic of the oracle.
        if !self.groups[&group].slots.contains_key(&sender) {
            self.protocol_error(conn, "slot-of-a-logged-in-device-is-missing");
            return;
        }
        let now = self.clock.now_ms();
        let ephemeral = flags & EPHEMERAL != 0;
        self.reflects.push(ReflectSeen {
            conn,
            reflect_id,
            flags,
            bytes: envelope.to_vec(),
        });
        let others: Vec<u64> = self.groups[&group]
            .slots
            .keys()
            .copied()
            .filter(|d| *d != sender)
            .collect();
        let limit = self.knobs.queue_limit;
        for device in others {
            let online = self.online_conn(group, device);
            if ephemeral && online.is_none() {
                continue;
            }
            let slot = self
                .groups
                .get_mut(&group)
                .and_then(|g| g.slots.get_mut(&device))
                .expect("a slot of the group");
            // [U: real server] an ephemeral entry takes an id of the slot's sequence too.
            let entry = Entry {
                reflected_id: slot.next_reflected_id,
                flags,
                timestamp: now,
                envelope: envelope.to_vec(),
            };
            slot.next_reflected_id = slot.next_reflected_id.wrapping_add(1);
            if !ephemeral {
                slot.queue.push(entry.clone());
                if slot.queue.len() > limit {
                    // D18: over the limit the slot goes; its device must link again.
                    if let Some(c) = online {
                        self.end(c, 4114, true);
                    }
                    self.groups
                        .get_mut(&group)
                        .expect("a known group")
                        .slots
                        .remove(&device);
                    self.trace.frame(
                        Dir::Out,
                        0,
                        "slot-deleted-queue-full",
                        &[("device", device)],
                    );
                    continue;
                }
            }
            if let Some(c) = online {
                self.send_reflected(c, &entry);
            }
        }
        if !ephemeral {
            let ack = reflect_ack_payload(reflect_id, now);
            if self.knobs.withhold_reflect_acks {
                self.trace.frame(
                    Dir::Out,
                    conn.0,
                    "reflect-ack-withheld",
                    &[("reflect-id", u64::from(reflect_id))],
                );
                self.conn(conn).reflect_acks_held.push((reflect_id, ack));
            } else {
                self.send(
                    conn,
                    kind::REFLECT_ACK,
                    &ack,
                    &[("reflect-id", u64::from(reflect_id))],
                );
            }
        }
    }

    fn send_reflected(&mut self, conn: ConnId, e: &Entry) {
        self.send(
            conn,
            kind::REFLECTED,
            &reflected_payload(e.flags, e.reflected_id, e.timestamp, &e.envelope),
            &[("reflected-id", u64::from(e.reflected_id))],
        );
    }

    /// D11: a `ReflectedAck` removes the entry from the slot's queue; an id the slot does not hold
    /// closes with 4012 (and is recorded).
    fn reflected_ack(&mut self, conn: ConnId, payload: &[u8]) {
        let Some(id) = payload
            .get(4..8)
            .and_then(|b| b.try_into().ok())
            .map(u32::from_le_bytes)
        else {
            self.protocol_error(conn, "reflected-ack-malformed");
            return;
        };
        let (group, device) = {
            let c = &self.conns[&conn];
            (
                c.group,
                c.device.expect("a logged-in connection has a device"),
            )
        };
        let Some(slot) = self
            .groups
            .get_mut(&group)
            .and_then(|g| g.slots.get_mut(&device))
        else {
            // G1: a violation, not a panic.
            self.protocol_error(conn, "slot-of-a-logged-in-device-is-missing");
            return;
        };
        let queue = &mut slot.queue;
        let known = queue.iter().position(|e| e.reflected_id == id);
        if let Some(at) = known {
            queue.remove(at);
        }
        let seen = AckSeen {
            conn,
            device,
            reflected_id: id,
            at_ms: self.clock.now_ms(),
            known: known.is_some(),
        };
        // Published to `acks` only after the hook ran (`Mediator::receive`).
        self.ack_calls.push(seen);
        if known.is_some() {
            return;
        }
        self.violations
            .record("reflected-ack-for-an-unknown-id", &[("conn", conn.0)]);
        self.end(conn, 4012, true);
    }

    /// RP-b: a `DropDevice` from `conn` (`TP/md-d2m.proto:214-229`): the named device of `conn`'s group is dropped: its
    /// slot freed (its reflection queue with it), `DropDeviceAck` with the same id to `conn`, then every open connection
    /// of that device closed 4113 ("dropped by other device"; when the device drops ITSELF its own connection gets the
    /// ack first, then the 4113). A device the group holds no slot for is acked all the same and frees nothing (traced
    /// `freed 0`). Under [`Knobs::withhold_drop_ack`] the drop is recorded and nothing follows. [U: real server] whether
    /// a device may drop itself (Desktop does, `selfKickFromMediator`), whether the dropping connection gets 4113 after
    /// the ack, and what an id without a slot gets.
    fn drop_device(&mut self, conn: ConnId, payload: &[u8]) {
        let Ok(d2m::DropDevice { device_id }) = d2m::DropDevice::decode(payload) else {
            self.protocol_error(conn, "drop-device-malformed");
            return;
        };
        self.drops.push(DropSeen {
            conn,
            device: device_id,
        });
        if let Some(code) = self.knobs.close_at_next_drop.take() {
            self.trace.frame(
                Dir::Out,
                conn.0,
                "drop-device-closed",
                &[("device", device_id), ("code", u64::from(code))],
            );
            self.end(conn, code, true);
            return;
        }
        if self.knobs.withhold_drop_ack {
            self.trace.frame(
                Dir::Out,
                conn.0,
                "drop-device-withheld",
                &[("device", device_id)],
            );
            return;
        }
        let group = self.conns[&conn].group;
        let freed = self
            .groups
            .get_mut(&group)
            .is_some_and(|g| g.slots.remove(&device_id).is_some());
        self.trace.frame(
            Dir::Out,
            conn.0,
            "drop-device-freed",
            &[("device", device_id), ("freed", u64::from(freed))],
        );
        let ack = d2m::DropDeviceAck { device_id };
        self.send(
            conn,
            kind::DROP_DEVICE_ACK,
            &ack.encode_to_vec(),
            &[("device", device_id)],
        );
        let open: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, c)| {
                c.group == group && c.device == Some(device_id) && c.phase != Phase::Closed
            })
            .map(|(id, _)| *id)
            .collect();
        for id in open {
            self.end(id, 4113, true);
        }
    }

    /// A breach of the protocol by the client: recorded, and closed with 4010 as the real server
    /// would. [U: real server] the spec names 4010 "Protocol error" but not when it is sent.
    fn protocol_error(&mut self, conn: ConnId, rule: &'static str) {
        self.violations.record(rule, &[("conn", conn.0)]);
        self.end(conn, 4010, true);
    }

    /// D3-D7: the `ClientHello` and what follows it.
    fn client_hello(&mut self, conn: ConnId, payload: &[u8]) {
        let Ok(hello) = d2m::ClientHello::decode(payload) else {
            self.protocol_error(conn, "client-hello-malformed");
            return;
        };
        if hello.version > self.knobs.server_version {
            // `TP/md-d2m.struct.yml` "Version Negotiation": the server closes with 4110.
            self.violations
                .record("client-version-above-the-servers", &[("conn", conn.0)]);
            self.end(conn, 4110, true);
            return;
        }
        let (group_id, esk_secret, challenge) = {
            let c = &self.conns[&conn];
            (c.group, c.esk.secret, c.challenge)
        };
        // The response: the challenge in a nonce-ahead box under X25519HSalsa20(ESK, DGPK.public).
        let opened = hsalsa_shared(&esk_secret, &group_id)
            .and_then(|key| open_nonce_ahead(&key, &hello.response));
        if hello.response.len() != RESPONSE_LENGTH || opened.as_deref() != Some(&challenge[..]) {
            // [U: real server] the code of a failed authentication.
            self.protocol_error(conn, "challenge-response-does-not-open");
            return;
        }
        let exhausted = d2m::client_hello::DeviceSlotsExhaustedPolicy::try_from(
            hello.device_slots_exhausted_policy,
        );
        let expiration =
            d2m::DeviceSlotExpirationPolicy::try_from(hello.device_slot_expiration_policy);
        let expected = d2m::DeviceSlotState::try_from(hello.expected_device_slot_state);
        let (Ok(exhausted), Ok(expiration), Ok(expected)) = (exhausted, expiration, expected)
        else {
            self.protocol_error(conn, "client-hello-unknown-enum");
            return;
        };
        let device = hello.device_id;
        self.conn(conn).hello = Some(HelloSeen {
            device,
            expected,
            exhausted,
            expiration,
        });
        if exhausted == d2m::client_hello::DeviceSlotsExhaustedPolicy::DropLeastRecent {
            // M6: Kourier never lets the mediator drop another device; the fake goes on as REJECT.
            self.violations
                .record("drop-least-recent-policy", &[("device", device)]);
        }
        self.trace.frame(
            Dir::In,
            conn.0,
            "client-hello-ok",
            &[("device", device), ("expected", expected as u64)],
        );
        if std::mem::take(&mut self.knobs.slot_exists_at_next_hello) {
            let first_id = self.rng.next_u32();
            let group = self.groups.get_mut(&group_id).expect("a known group");
            group.slots.entry(device).or_insert(Slot {
                expiration,
                device_info: Vec::new(),
                last_disconnect_ms: Some(self.clock.now_ms()),
                queue: Vec::new(),
                next_reflected_id: first_id,
            });
            self.trace
                .frame(Dir::Out, conn.0, "slot-made-before-the-check", &[]);
        }
        let max = self.knobs.max_device_slots;
        let group = self.groups.get_mut(&group_id).expect("a known group");
        let actual = if group.slots.contains_key(&device) {
            d2m::DeviceSlotState::Existing
        } else {
            d2m::DeviceSlotState::New
        };
        if expected != actual {
            // D5: "expected_device_slot_state != actual" closes 4115 before the registration.
            // [U: real server] the order of the checks: challenge, then 4115, then 4111 (the spec
            // only says 4115 comes "before being registered").
            self.end(conn, 4115, true);
            // RP-b (P-M2): a NEW hello of an abandoned socket, processed late: the slot comes after the close.
            if actual == d2m::DeviceSlotState::New
                && std::mem::take(&mut self.knobs.register_slot_after_next_4115)
            {
                let first_id = self.rng.next_u32();
                let now = self.clock.now_ms();
                let group = self.groups.get_mut(&group_id).expect("a known group");
                group.slots.insert(
                    device,
                    Slot {
                        expiration,
                        device_info: Vec::new(),
                        last_disconnect_ms: Some(now),
                        queue: Vec::new(),
                        next_reflected_id: first_id,
                    },
                );
                self.trace.frame(
                    Dir::Out,
                    conn.0,
                    "slot-made-after-the-close",
                    &[("device", device)],
                );
            }
            if std::mem::take(&mut self.knobs.drop_slot_at_next_4115) {
                let group = self.groups.get_mut(&group_id).expect("a known group");
                if group.slots.remove(&device).is_some() {
                    self.trace.frame(
                        Dir::Out,
                        conn.0,
                        "slot-deleted-close",
                        &[("device", device), ("code", 4115)],
                    );
                }
            }
            return;
        }
        if actual == d2m::DeviceSlotState::New
            && group.slots.len() >= usize::try_from(max).expect("u32 fits usize")
        {
            self.end(conn, 4111, true);
            return;
        }
        let first_id = self.rng.next_u32();
        let slot = group.slots.entry(device).or_insert(Slot {
            expiration,
            device_info: Vec::new(),
            last_disconnect_ms: None,
            queue: Vec::new(),
            next_reflected_id: first_id,
        });
        slot.expiration = expiration;
        slot.device_info = hello.encrypted_device_info;
        slot.last_disconnect_ms = None;
        // TH5 A2 (B1): what a phone that is online reflects while a NEW device finishes joining. A
        // slot that existed before gets nothing (the list is the NEXT new slot's, taken once).
        let mut volatile = Vec::new();
        let mut joined = 0u64;
        if actual == d2m::DeviceSlotState::New {
            let now = self.clock.now_ms();
            for pending in std::mem::take(&mut self.knobs.reflect_during_join) {
                joined += 1;
                let entry = Entry {
                    reflected_id: slot.next_reflected_id,
                    flags: pending.flags,
                    timestamp: now,
                    envelope: pending.envelope,
                };
                slot.next_reflected_id = slot.next_reflected_id.wrapping_add(1);
                if entry.flags & EPHEMERAL == 0 {
                    slot.queue.push(entry);
                } else {
                    volatile.push(entry);
                }
            }
        }
        let shared = group.shared_device_data.clone();
        let backlog = slot.queue.clone();
        // D6: the OLDER connection of the same device gets 4112. The new connection holds the device
        // already (G1: its slot must not take the older one's time of disconnect).
        self.conn(conn).device = Some(device);
        let older: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(id, c)| {
                **id != conn
                    && c.group == group_id
                    && c.device == Some(device)
                    && c.phase != Phase::Closed
            })
            .map(|(id, _)| *id)
            .collect();
        for id in older {
            self.end(id, 4112, true);
        }
        if std::mem::take(&mut self.knobs.withhold_server_info) {
            self.trace.frame(
                Dir::Out,
                conn.0,
                "server-info-withheld",
                &[("device", device)],
            );
            return;
        }
        let now = self.clock.now_ms();
        let current_time = now
            .checked_add_signed(self.knobs.clock_skew_ms)
            .expect("a skew inside u64 ms");
        let info = d2m::ServerInfo {
            current_time,
            max_device_slots: max,
            device_slot_state: self
                .knobs
                .server_info_slot_state
                .take()
                .map_or(actual as i32, |state| state as i32),
            encrypted_shared_device_data: shared,
            reflection_queue_length: u32::try_from(backlog.len()).expect("a queue fits u32"),
        };
        let c = self.conn(conn);
        c.device = Some(device);
        c.phase = Phase::PostHandshake;
        let held = std::mem::take(&mut c.held_proxy);
        c.proxied.extend(held.iter().cloned());
        self.send(
            conn,
            kind::SERVER_INFO,
            &info.encode_to_vec(),
            &[("device", device), ("state", actual as u64)],
        );
        // D11, D12: the backlog in queue order, then Dry ("transmitted", not acked), then the
        // promotion if this device leads now.
        for entry in &backlog {
            self.send_reflected(conn, entry);
        }
        if joined > 0 {
            self.trace.frame(
                Dir::Out,
                conn.0,
                "reflect-during-join",
                &[("count", joined)],
            );
        }
        for entry in &volatile {
            self.send_reflected(conn, entry);
        }
        let leads = self.knobs.auto_leader && self.groups[&group_id].leader.is_none();
        if leads && self.knobs.promote_before_dry {
            self.make_leader(group_id, conn);
        }
        if self.knobs.hold_dry {
            // H2 of TH3: Dry (and the promotion after it) waits for `release_dry`.
            let promote = leads && !self.knobs.promote_before_dry;
            let c = self.conn(conn);
            c.dry_held = true;
            c.promote_owed = promote;
            self.trace.frame(Dir::Out, conn.0, "dry-held", &[]);
        } else {
            self.send(conn, kind::REFLECTION_QUEUE_DRY, &[], &[]);
            if leads && !self.knobs.promote_before_dry {
                self.make_leader(group_id, conn);
            }
        }
        // §3.2: a CSP client-hello that came before the handshake is answered only now.
        for payload in held {
            self.hand_to_chat_server(conn, &payload);
        }
    }

    /// Hands Proxy bytes of `conn` to the chat server.
    fn hand_to_chat_server(&mut self, conn: ConnId, bytes: &[u8]) {
        // G2: CSP bytes are the only thing that keeps the chat server's idle timer from running out.
        let now = self.clock.now_ms();
        self.conn(conn).last_csp_ms = now;
        if self.knobs.chat_server {
            self.with_csp(conn, |chat, cc, cx| chat.receive(cc, cx, bytes));
        }
    }

    /// Chat server bytes to the client in Proxy payloads (chunked when the knob says so).
    fn send_proxy(&mut self, conn: ConnId, bytes: Vec<u8>) {
        if self.knobs.csp.chunk_proxy {
            for piece in chunks(bytes, &mut self.rng) {
                self.send(conn, kind::PROXY, &piece, &[]);
            }
        } else {
            self.send(conn, kind::PROXY, &bytes, &[]);
        }
    }

    /// Runs `f` on the chat server and the CSP side of `conn`, then does what it asked: Proxy
    /// payloads out (chunked when the knob says so), a close, an idle timeout.
    fn with_csp(
        &mut self,
        conn: ConnId,
        f: impl FnOnce(&mut ChatServer, &mut CspConn, &mut Cx<'_>),
    ) {
        let Some(c) = self.conns.get(&conn) else {
            return;
        };
        if c.phase != Phase::PostHandshake {
            return;
        }
        let is_leader = self.groups.get(&c.group).and_then(|g| g.leader) == Some(conn);
        let mut cc = std::mem::take(&mut self.conn(conn).csp);
        let was_in = cc.logged_in();
        let mut cx = Cx {
            conn: conn.0,
            now_ms: self.clock.now_ms(),
            clock_skew_ms: self.knobs.clock_skew_ms,
            violations: &self.violations,
            trace: &self.trace,
            rng: &mut self.rng,
            knobs: &self.knobs.csp,
            leader_mode: self.knobs.leader_mode,
            is_leader,
            out: Vec::new(),
        };
        f(&mut self.chat, &mut cc, &mut cx);
        let out = cx.out;
        // TH3 fix P-L7/P-L9: the call that accepted the login starts the hold (the login-ack is its
        // first output), when the knob says so.
        if !was_in && cc.logged_in() && self.knobs.csp.withhold_login_ack {
            self.conn(conn).login_hold = true;
            self.trace
                .frame(Dir::Out, conn.0, "login-ack-withheld", &[]);
        }
        self.conn(conn).csp = cc;
        for o in out {
            match o {
                CspOut::Proxy(bytes) if self.conn(conn).login_hold => {
                    self.conn(conn).login_held.push(bytes);
                }
                CspOut::Proxy(bytes) => self.send_proxy(conn, bytes),
                CspOut::Close(code) => self.end(conn, code, true),
                CspOut::IdleTimeout(d) => self.conn(conn).csp_idle_timeout = d,
            }
        }
    }

    /// The logged-in, unblocked connection of the CSP identity `receiver`.
    fn unblocked_conn(&self, receiver: &[u8; 8]) -> Option<ConnId> {
        self.conns
            .iter()
            .find(|(_, c)| {
                c.phase == Phase::PostHandshake
                    && c.csp.unblocked()
                    && c.csp.identity() == Some(*receiver)
            })
            .map(|(id, _)| *id)
    }
}

/// The chat server's script calls and queries (M4).
impl Mediator {
    /// A message from `sender` for `receiver` arrives at the chat server: queued for the receiver
    /// (C10) and sent at once on its unblocked connection, if one is. The message box is opaque
    /// test bytes (the peer builder of TH5 makes real ones). Returns `false` when it was dropped
    /// (flag "no server queuing" and nobody unblocked).
    #[track_caller]
    pub fn csp_incoming(
        &self,
        receiver: &str,
        sender: &str,
        message_id: u64,
        flags: u8,
        message_box: &[u8],
    ) -> bool {
        let (Some(receiver), Some(sender)) = (id_bytes(receiver), id_bytes(sender)) else {
            panic!("a test's identities have 8 bytes");
        };
        let mut s = lock(&self.0);
        let now = s.clock.now_ms();
        let mut nonce = [0u8; 24];
        s.rng.fill_bytes(&mut nonce);
        let created_at = u32::try_from(now / 1000).expect("seconds fit u32");
        let payload = super::csp::incoming_message(
            &sender,
            &receiver,
            message_id,
            created_at,
            flags,
            &nonce,
            message_box,
        );
        let target = s.unblocked_conn(&receiver);
        let message = Queued {
            sender,
            message_id,
            flags,
            queued_ms: now,
            payload,
        };
        let queued = s.chat.enqueue(&receiver, message, target.is_some());
        if queued && let Some(conn) = target {
            s.with_csp(conn, |chat, cc, cx| chat.deliver(cc, cx));
        }
        queued
    }

    /// How many incoming messages wait for `receiver` (sent or not, not acked).
    pub fn csp_queue_len(&self, receiver: &str) -> usize {
        id_bytes(receiver).map_or(0, |r| lock(&self.0).chat.queue_len(&r))
    }

    /// The server sends an echo request on `conn`.
    pub fn csp_echo_request(&self, conn: ConnId, data: &[u8]) {
        lock(&self.0).with_csp(conn, |chat, cc, cx| chat.echo_request(cc, cx, data));
    }

    /// The server sends `close-error{can_reconnect}` on `conn`, then the mediator closes it with
    /// `then_code` (a code that may contradict it: the `close-error` decides, `close.rs`).
    pub fn csp_close_error(&self, conn: ConnId, can_reconnect: bool, then_code: u16) {
        let mut s = lock(&self.0);
        s.with_csp(conn, |chat, cc, cx| chat.close_error(cc, cx, can_reconnect));
        s.end(conn, then_code, true);
    }

    /// The server sends an `alert` on `conn`.
    pub fn csp_alert(&self, conn: ConnId) {
        lock(&self.0).with_csp(conn, |chat, cc, cx| chat.alert(cc, cx));
    }

    /// Whether `conn` finished the CSP login.
    pub fn csp_logged_in(&self, conn: ConnId) -> bool {
        lock(&self.0)
            .conns
            .get(&conn)
            .is_some_and(|c| c.csp.logged_in())
    }

    /// What the client of `conn` said at its CSP login.
    pub fn csp_login(&self, conn: ConnId) -> Option<LoginInfo> {
        lock(&self.0)
            .conns
            .get(&conn)
            .filter(|c| c.csp.logged_in())
            .map(|c| c.csp.info().clone())
    }

    /// The CSP session key and SCK of `conn` (a test's independent check of the server's boxes).
    pub fn csp_session(&self, conn: ConnId) -> Option<([u8; 32], [u8; 16])> {
        lock(&self.0).conns.get(&conn).and_then(|c| c.csp.session())
    }

    /// What clients sent (`0x01`), in order.
    pub fn csp_sent(&self) -> Vec<Sent> {
        lock(&self.0).chat.sent().to_vec()
    }

    /// The push-token payload types clients sent.
    pub fn csp_push_calls(&self) -> Vec<u8> {
        lock(&self.0).chat.push_calls().to_vec()
    }

    /// The echo responses clients sent, in order.
    pub fn csp_echo_responses(&self) -> Vec<Vec<u8>> {
        lock(&self.0).chat.echo_responses().to_vec()
    }

    /// How many echo requests of clients reached the chat server (answered or withheld).
    pub fn csp_echo_requests(&self) -> usize {
        lock(&self.0).chat.echo_requests()
    }

    /// The idle timeouts clients asked for (seconds).
    pub fn csp_idle_requests(&self) -> Vec<u16> {
        lock(&self.0).chat.idle_requests().to_vec()
    }

    /// TH6a S3 (`CspKnobs::close_on_box_to`): the boxes the chat server refused by closing, in order:
    /// the D2M connection, the receiver's ID and the message id.
    pub fn csp_refused(&self) -> Vec<(ConnId, String, u64)> {
        lock(&self.0)
            .chat
            .refused()
            .iter()
            .map(|(conn, receiver, id)| (ConnId(*conn), id_text(receiver), *id))
            .collect()
    }

    /// TH6a S3 (`CspKnobs::withhold_message_ack_of`): how many acks the chat server holds for the
    /// boxes to `receiver`.
    pub fn csp_held_acks(&self, receiver: &str) -> usize {
        id_bytes(receiver).map_or(0, |r| {
            lock(&self.0)
                .chat
                .held_acks()
                .iter()
                .filter(|a| a.receiver == r)
                .count()
        })
    }

    /// TH6a S3 (`CspKnobs::withhold_message_ack_of`): sends the held `message-ack`s of the boxes to
    /// `receiver`, in order, on the connections they came from (one that closed meanwhile lost them)
    /// and STOPS holding for `receiver`. How many were sent.
    pub fn release_message_acks(&self, receiver: &str) -> usize {
        let Some(receiver) = id_bytes(receiver) else {
            return 0;
        };
        let mut s = lock(&self.0);
        s.knobs.csp.withhold_message_ack.retain(|r| *r != receiver);
        let held = s.chat.take_held_acks(&receiver);
        let mut sent = 0;
        for ack in held {
            let conn = ConnId(ack.conn);
            let open = s
                .conns
                .get(&conn)
                .is_some_and(|c| c.phase == Phase::PostHandshake);
            if !open {
                continue;
            }
            s.with_csp(conn, |chat, cc, cx| {
                chat.send_message_ack(cc, cx, &ack.receiver, ack.message_id);
            });
            sent += 1;
        }
        sent
    }

    /// TH6a S3 (`CspKnobs::drop_repeats`): the repeats the chat server dropped, in order: the D2M
    /// connection, the receiver's ID and the message id.
    pub fn csp_repeats_dropped(&self) -> Vec<(ConnId, String, u64)> {
        lock(&self.0)
            .chat
            .repeats_dropped()
            .iter()
            .map(|(conn, receiver, id)| (ConnId(*conn), id_text(receiver), *id))
            .collect()
    }

    /// The mediator's idle timeout of `conn` (4013).
    pub fn idle_timeout(&self, conn: ConnId) -> Option<Duration> {
        lock(&self.0).conns.get(&conn).map(|c| c.idle_timeout)
    }
}

//! The scripted PHONE of a device group (TH2 M3; S-mock §4.2): a test-side device, not a server.
//! It holds the device group's keys (the mediator never does), logs in to the fake mediator with
//! its own minimal D2M client written from the spec on the fake's crypto (a second implementation
//! beside libthreema's, so the mediator is not only ever checked against one client), reflects
//! envelopes it seals with DGRK, and opens and acks what the others reflected.
//!
//! Sans-I/O like the mediator: every call works on the mediator's outbox of the phone's
//! connection at once ([`Phone::pump`]).
//!
//! TH6 L2a, THE PHONE'S FOLLOWER VIEW ([`Phone::incoming`], [`Phone::view`]): the phone opens what the
//! leader reflected (DGRK), acks it (an ephemeral one is never acked: the mediator keeps none), DISCARDS a
//! second copy by its D2D nonce (`TP/md.md`: a device discards a reflected envelope whose nonce it has
//! seen), and keeps the `IncomingMessage`s, `ContactSync`s and the other kinds it saw. A reflection that
//! does not open or decode is the leader's breach (`phone-envelope-does-not-open`,
//! `phone-envelope-undecodable`). THE LEADER RULE the phone meets is the mediator's (D13, `d2m.rs`): the
//! first device of a group that finishes its login leads, a device that comes online while another leads
//! stays follower, and when the leader's connection ends the longest connected other device is promoted
//! (`PhoneEvent::Promoted`; [`Phone::leads`]).
//!
//! TH6 L2b: THE PHONE'S TRANSACTIONS ([`Phone::begin_transaction`], [`Phone::commit_transaction`]: the phone begins
//! one and HOLDS it until the test commits or disconnects, so Kourier meets `TransactionRejected` and later
//! `TransactionEnded`), and THE PHONE TAKES THE QUEUE ([`Phone::lead_csp_queue`]): only while its connection leads,
//! it takes what the chat server queued for the user as if its CSP leg had it delivered ([I]: the fake's phone has
//! no CSP leg; `Mediator::csp_take_for_leader`) and runs the receive steps that matter to a takeover, from the
//! peers' posted letters (`leader.rs` `Posted`: the phone stands in for its own decryption): step 2 (a CSP nonce it
//! saw in its view or used itself: ack, discard), a box it cannot open (no peer posted it: ack, discard), a letter
//! bent to be discarded (ack, discard), step 14 (a (sender, id) it viewed or reflected: ack, discard), else it
//! reflects an `IncomingMessage` (sealed with DGRK) and acks. Each is recorded ([`Led`]) and in the mediator's
//! record under the phone's connection (the oracle skips the phone). [U: real phone] the phone's own steps.

use libthreema::protobuf::{d2d, d2m};
use prost::Message as _;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{RngCore as _, SeedableRng as _};

use super::crypto::{DeviceGroupKeys, hsalsa_shared, open_nonce_ahead, seal_nonce_ahead};
use super::csp::flag;
use super::d2m::{ConnId, EPHEMERAL, Mediator, Out, REFLECT_HEADER, datagram, kind, split};
use super::peer::{HEAD, NONCE};
use super::txn::seal_scope;
use super::{MockWorld, Violations};
use libthreema::protobuf::d2d::transaction_scope::Scope;

/// The label the phone's device info carries.
pub const PHONE_LABEL: &str = "Phone (fake)";

/// A `d2d.Envelope` of an incoming text message (T0's builder, `d2m_mock.rs:151-164`): the phone
/// received it from `sender` and reflects it. `device_id` is the reflecting device's.
pub fn incoming_text(
    device_id: u64,
    sender: &str,
    message_id: u64,
    created_at: u64,
    text: &str,
    nonce: [u8; 24],
) -> d2d::Envelope {
    d2d::Envelope {
        #[expect(deprecated, reason = "the spec's padding field; empty here")]
        padding: vec![],
        device_id,
        protocol_version: d2d::ProtocolVersion::V03 as u32,
        content: Some(d2d::envelope::Content::IncomingMessage(
            d2d::IncomingMessage {
                sender_identity: sender.to_owned(),
                message_id,
                created_at,
                r#type: 0x01,
                body: text.as_bytes().to_vec(),
                nonce: nonce.to_vec(),
            },
        )),
    }
}

/// Opens an envelope the way every device of the group does: nonce-ahead secretbox under DGRK,
/// then the protobuf. `None` when it does not open or decode.
pub fn open_envelope(keys: &DeviceGroupKeys, sealed: &[u8]) -> Option<d2d::Envelope> {
    let plain = open_nonce_ahead(&keys.reflect, sealed)?;
    d2d::Envelope::decode(plain.as_slice()).ok()
}

/// Threema for Android 6.6.0's title of a linked device whose info it cannot use (`md_device_invalid_label`).
pub const INVALID_TITLE: &str = "Device";
/// Its line under that title (`md_device_invalid_hint`).
pub const INVALID_DETAILS: &str = "Information could not be obtained";

/// One row of the phone's linked-devices list as Threema for Android 6.6.0 makes it from a `DevicesInfo` entry
/// (read from its bytecode, `target/threema/devinfo/report.md`; [U: real phone] not run): the info opens or it is
/// the app's `INVALID_DEVICE_INFO` (platform UNSPECIFIED), and a row is VALID only when its platform is not
/// UNSPECIFIED (`LinkedDeviceInfoUiModel.isValid`), whatever its label. `Debug` prints no label or detail.
#[derive(Clone, PartialEq, Eq)]
pub struct LinkedRow {
    pub device: u64,
    /// The platform the phone read (UNSPECIFIED for an info that does not open or decode, or a number it lacks).
    pub platform: d2d::device_info::Platform,
    /// `platform != UNSPECIFIED`.
    pub valid: bool,
    /// The title: the label when valid (empty stays empty), else [`INVALID_TITLE`].
    pub title: String,
    /// The line under it: `<app version> · <platform details>` when valid (`md_device_version_and_platform_info`
    /// "%s · %s"), else [`INVALID_DETAILS`].
    pub details: String,
    /// "Currently active": the entry has `connected_since`.
    pub active: bool,
}

impl std::fmt::Debug for LinkedRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "LinkedRow({:?}, valid {}, active {})",
            self.platform, self.valid, self.active
        )
    }
}

/// What the phone makes of one slot's `encrypted_device_info` (`MultiDeviceKeys.decryptDeviceInfo`): the first 24
/// bytes are the nonce, the rest a secretbox under DGDIK (the fake's own derivation), then a `d2d.DeviceInfo`; the
/// padding is ignored.
pub fn phone_row(keys: &DeviceGroupKeys, device: u64, sealed: &[u8], active: bool) -> LinkedRow {
    let info = open_nonce_ahead(&keys.device_info, sealed)
        .and_then(|plain| d2d::DeviceInfo::decode(plain.as_slice()).ok());
    let platform = info
        .as_ref()
        .and_then(|i| d2d::device_info::Platform::try_from(i.platform).ok())
        .unwrap_or(d2d::device_info::Platform::Unspecified);
    let valid = platform != d2d::device_info::Platform::Unspecified;
    let (title, details) = match (&info, valid) {
        (Some(i), true) => (
            i.label.clone(),
            format!("{} · {}", i.app_version, i.platform_details),
        ),
        _ => (INVALID_TITLE.to_owned(), INVALID_DETAILS.to_owned()),
    };
    LinkedRow {
        device,
        platform,
        valid,
        title,
        details,
        active,
    }
}

/// What the phone got from the mediator, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhoneEvent {
    ServerInfo {
        queue_length: u32,
    },
    Reflected {
        id: u32,
        flags: u16,
        timestamp: u64,
    },
    ReflectAck {
        id: u32,
        timestamp: u64,
    },
    Dry,
    Promoted,
    Proxy,
    Closed(u16),
    /// TH6 L2b: `DevicesInfo` (the number of devices it lists).
    DevicesInfo {
        devices: usize,
    },
    /// TH6 L2b: `BeginTransactionAck`.
    TxnBegun,
    /// TH6 L2b: `TransactionRejected` naming the device that holds the lock.
    TxnRejected {
        holder: u64,
    },
    /// TH6 L2b: `CommitTransactionAck`.
    TxnCommitted,
    /// TH6 L2b: `TransactionEnded` naming the device whose transaction ended.
    TxnEnded {
        holder: u64,
    },
}

/// TH6 L2b: what the leading phone did with one message it took from the chat server's queue
/// ([`Phone::lead_csp_queue`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedOutcome {
    /// It reflected an `IncomingMessage` under this reflect-id, then acked (unless "no server ack").
    Reflected { reflect_id: u32 },
    /// Step 14: the (sender, id) was viewed or reflected before: acked, discarded.
    Seen,
    /// Step 2: the CSP nonce was used before: acked, discarded.
    NonceUsed,
    /// No peer posted it (the chat server's opaque test box): the phone cannot open it: acked, discarded.
    Unopened,
    /// The letter was bent to be discarded by the receive steps (`Posted::discard`): acked, discarded.
    Discarded,
}

/// TH6 L2b: one message the leading phone took. `Debug` prints the id and the outcome only.
#[derive(Clone, PartialEq, Eq)]
pub struct Led {
    pub sender: String,
    pub message_id: u64,
    pub outcome: LedOutcome,
}

impl std::fmt::Debug for Led {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Led(id {}, {:?})", self.message_id, self.outcome)
    }
}

/// One reflection the phone took into its view (TH6 L2a).
#[derive(Clone, PartialEq)]
pub struct Viewed {
    /// The `reflected-id` it came under.
    pub reflected_id: u32,
    /// Whether it came ephemeral (not queued, not acked).
    pub ephemeral: bool,
    /// The D2D nonce (the first 24 bytes of the sealed envelope).
    pub d2d_nonce: [u8; 24],
    pub envelope: d2d::Envelope,
}

impl std::fmt::Debug for Viewed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Viewed(id {}, ephemeral {}, {})",
            self.reflected_id,
            self.ephemeral,
            content_name(&self.envelope)
        )
    }
}

/// The name of an envelope's content (the oneof's field), `none` without one.
pub fn content_name(envelope: &d2d::Envelope) -> &'static str {
    use d2d::envelope::Content;
    match &envelope.content {
        None => "none",
        Some(Content::OutgoingMessage(_)) => "outgoing_message",
        Some(Content::OutgoingMessageUpdate(_)) => "outgoing_message_update",
        Some(Content::IncomingMessage(_)) => "incoming_message",
        Some(Content::IncomingMessageUpdate(_)) => "incoming_message_update",
        Some(Content::ContactSync(_)) => "contact_sync",
        Some(_) => "other",
    }
}

/// What the phone saw as a follower (TH6 L2a): every reflection it opened once, in order, and how many
/// second copies it discarded by their D2D nonce. `Debug` prints counts only.
#[derive(Clone, Default)]
pub struct View {
    pub seen: Vec<Viewed>,
    /// Second copies discarded by their D2D nonce.
    pub replays: usize,
}

impl std::fmt::Debug for View {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("View")
            .field("seen", &self.seen.len())
            .field("replays", &self.replays)
            .finish()
    }
}

impl View {
    /// The `IncomingMessage`s, in order.
    pub fn incoming(&self) -> Vec<&d2d::IncomingMessage> {
        self.seen
            .iter()
            .filter_map(|v| match &v.envelope.content {
                Some(d2d::envelope::Content::IncomingMessage(m)) => Some(m),
                _ => None,
            })
            .collect()
    }

    /// The (sender, message id) of the `IncomingMessage`s, in order.
    pub fn incoming_ids(&self) -> Vec<(String, u64)> {
        self.incoming()
            .into_iter()
            .map(|m| (m.sender_identity.clone(), m.message_id))
            .collect()
    }

    /// The `ContactSync`s, in order.
    pub fn contacts(&self) -> Vec<&d2d::ContactSync> {
        self.seen
            .iter()
            .filter_map(|v| match &v.envelope.content {
                Some(d2d::envelope::Content::ContactSync(c)) => Some(c),
                _ => None,
            })
            .collect()
    }

    /// The content names of everything seen, in order.
    pub fn kinds(&self) -> Vec<&'static str> {
        self.seen
            .iter()
            .map(|v| content_name(&v.envelope))
            .collect()
    }
}

/// The phone of one device group.
pub struct Phone {
    mediator: Mediator,
    keys: DeviceGroupKeys,
    server_group: String,
    device: u64,
    rng: ChaCha20Rng,
    conn: Option<ConnId>,
    logged_in_once: bool,
    next_reflect_id: u32,
    events: Vec<PhoneEvent>,
    /// The envelopes of the `Reflected` it got: (reflected-id, flags, sealed envelope).
    reflected: Vec<(u32, u16, Vec<u8>)>,
    /// TH6 L2a: the follower view and the world's violations (a reflection it cannot open).
    view: View,
    violations: Violations,
    /// Whether the mediator promoted this connection (reset at every connect).
    promoted: bool,
    /// TH6 L2b: what it did as leader with the chat server's queue, and the CSP nonces it took that way.
    led: Vec<Led>,
    led_nonces: Vec<[u8; 24]>,
    /// The last `DevicesInfo` it got.
    devices_info: Option<d2m::DevicesInfo>,
}

impl std::fmt::Debug for Phone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Phone")
            .field("device", &self.device)
            .finish()
    }
}

impl MockWorld {
    /// The phone of the group of `dgk` (a group the test added with `Mediator::add_group`), device
    /// id `device`, not connected yet.
    pub fn phone(&self, dgk: &[u8; 32], server_group: &str, device: u64) -> Phone {
        Phone {
            mediator: self.mediator().clone(),
            keys: DeviceGroupKeys::derive(dgk),
            server_group: server_group.to_owned(),
            device,
            rng: ChaCha20Rng::seed_from_u64(self.random_u64()),
            conn: None,
            logged_in_once: false,
            next_reflect_id: 1,
            events: Vec::new(),
            reflected: Vec::new(),
            view: View::default(),
            violations: self.violations().clone(),
            promoted: false,
            led: Vec::new(),
            led_nonces: Vec::new(),
            devices_info: None,
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Phone {
    pub fn device(&self) -> u64 {
        self.device
    }

    pub fn keys(&self) -> &DeviceGroupKeys {
        &self.keys
    }

    pub fn conn(&self) -> Option<ConnId> {
        self.conn
    }

    /// The URL a device of this group dials (D1): `wss://mediator-<p4>.invalid/<p8>/<hex>`.
    pub fn url(&self) -> String {
        let info = d2m::ClientUrlInfo {
            device_group_id: self.keys.path_public.to_vec(),
            server_group: self.server_group.clone(),
        };
        let first = self.keys.path_public[0];
        format!(
            "wss://mediator-{:x}.invalid/{first:02x}/{}",
            first >> 4,
            hex(&info.encode_to_vec())
        )
    }

    fn nonce(&mut self) -> [u8; 24] {
        let mut n = [0u8; 24];
        self.rng.fill_bytes(&mut n);
        n
    }

    /// Connects and logs in (`expected` NEW the first time, EXISTING after), with REJECT and
    /// PERSISTENT as Kourier's policies; what the mediator sends lands in [`Phone::events`].
    #[track_caller]
    pub fn connect(&mut self) {
        let conn = self
            .mediator
            .connect(&self.url())
            .expect("the fake took the phone's URL");
        self.conn = Some(conn);
        self.next_reflect_id = 1;
        self.promoted = false;
        let outs = self.mediator.take(conn);
        let [Out::Datagram(hello)] = outs.as_slice() else {
            panic!("the phone's first message from the fake is not one ServerHello");
        };
        let (t, payload) = split(hello).expect("a container");
        assert!(t == kind::SERVER_HELLO, "the phone got no ServerHello");
        let hello = d2m::ServerHello::decode(payload).expect("a ServerHello");
        let esk: [u8; 32] = hello.esk.as_slice().try_into().expect("a 32-byte ESK");
        let key = hsalsa_shared(&self.keys.path_secret, &esk).expect("a contributory ESK");
        let nonce = self.nonce();
        let response = seal_nonce_ahead(&key, &nonce, &hello.challenge);
        let info = d2d::DeviceInfo {
            #[expect(deprecated, reason = "the spec's padding field; empty here")]
            padding: vec![],
            platform: d2d::device_info::Platform::Android as i32,
            platform_details: "fake".to_owned(),
            app_version: "0".to_owned(),
            label: PHONE_LABEL.to_owned(),
        };
        let nonce = self.nonce();
        let client_hello = d2m::ClientHello {
            version: d2m::ProtocolVersion::V0 as u32,
            response,
            device_id: self.device,
            device_slots_exhausted_policy: d2m::client_hello::DeviceSlotsExhaustedPolicy::Reject
                as i32,
            device_slot_expiration_policy: d2m::DeviceSlotExpirationPolicy::Persistent as i32,
            expected_device_slot_state: if self.logged_in_once {
                d2m::DeviceSlotState::Existing as i32
            } else {
                d2m::DeviceSlotState::New as i32
            },
            encrypted_device_info: seal_nonce_ahead(
                &self.keys.device_info,
                &nonce,
                &info.encode_to_vec(),
            ),
        };
        self.mediator.receive(
            conn,
            datagram(kind::CLIENT_HELLO, &client_hello.encode_to_vec()),
        );
        self.logged_in_once = true;
        self.pump();
    }

    /// Takes what the mediator sent the phone.
    pub fn pump(&mut self) {
        let Some(conn) = self.conn else {
            return;
        };
        for out in self.mediator.take(conn) {
            let d = match out {
                Out::Close(code) => {
                    self.events.push(PhoneEvent::Closed(code));
                    continue;
                }
                Out::Datagram(d) => d,
            };
            let (t, p) = split(&d).expect("the fake sent a container");
            let event = match t {
                kind::SERVER_INFO => PhoneEvent::ServerInfo {
                    queue_length: d2m::ServerInfo::decode(p)
                        .expect("a ServerInfo")
                        .reflection_queue_length,
                },
                kind::REFLECTION_QUEUE_DRY => PhoneEvent::Dry,
                kind::ROLE_PROMOTED_TO_LEADER => {
                    self.promoted = true;
                    PhoneEvent::Promoted
                }
                kind::PROXY => PhoneEvent::Proxy,
                kind::REFLECT_ACK => PhoneEvent::ReflectAck {
                    id: u32::from_le_bytes(p[4..8].try_into().expect("an id")),
                    timestamp: u64::from_le_bytes(p[8..16].try_into().expect("a timestamp")),
                },
                kind::REFLECTED => {
                    let header = usize::from(p[0]);
                    let id = u32::from_le_bytes(p[4..8].try_into().expect("an id"));
                    let flags = u16::from_le_bytes(p[2..4].try_into().expect("flags"));
                    self.reflected.push((id, flags, p[header..].to_vec()));
                    PhoneEvent::Reflected {
                        id,
                        flags,
                        timestamp: u64::from_le_bytes(p[8..16].try_into().expect("a timestamp")),
                    }
                }
                kind::DEVICES_INFO => {
                    let info = d2m::DevicesInfo::decode(p).expect("a DevicesInfo");
                    let devices = info.augmented_device_info.len();
                    self.devices_info = Some(info);
                    PhoneEvent::DevicesInfo { devices }
                }
                kind::BEGIN_TRANSACTION_ACK => {
                    d2m::BeginTransactionAck::decode(p).expect("a BeginTransactionAck");
                    PhoneEvent::TxnBegun
                }
                kind::COMMIT_TRANSACTION_ACK => {
                    d2m::CommitTransactionAck::decode(p).expect("a CommitTransactionAck");
                    PhoneEvent::TxnCommitted
                }
                kind::TRANSACTION_REJECTED => PhoneEvent::TxnRejected {
                    holder: d2m::TransactionRejected::decode(p)
                        .expect("a TransactionRejected")
                        .device_id,
                },
                kind::TRANSACTION_ENDED => PhoneEvent::TxnEnded {
                    holder: d2m::TransactionEnded::decode(p)
                        .expect("a TransactionEnded")
                        .device_id,
                },
                _ => panic!("the fake sent the phone a type it does not know"),
            };
            self.events.push(event);
        }
    }

    /// The phone's linked-devices list (RT1 attempt 2): it asks `GetDevicesInfo` on its connection and makes a
    /// [`LinkedRow`] of every OTHER device's entry with [`phone_row`], by device id. Panics when the phone is not
    /// logged in or no `DevicesInfo` comes.
    #[track_caller]
    pub fn linked_devices(&mut self) -> Vec<LinkedRow> {
        use d2m::devices_info::augmented_device_info::ConnectionState;
        let conn = self.conn.expect("the phone is connected");
        self.devices_info = None;
        self.mediator.receive(
            conn,
            datagram(
                kind::GET_DEVICES_INFO,
                &d2m::GetDevicesInfo {}.encode_to_vec(),
            ),
        );
        self.pump();
        let info = self.devices_info.take().expect("a DevicesInfo came");
        info.augmented_device_info
            .iter()
            .filter(|(device, _)| **device != self.device)
            .map(|(device, entry)| {
                let active = matches!(
                    entry.connection_state,
                    Some(ConnectionState::ConnectedSince(_))
                );
                phone_row(&self.keys, *device, &entry.encrypted_device_info, active)
            })
            .collect()
    }

    /// Seals `envelope` under DGRK with a fresh nonce.
    pub fn seal(&mut self, envelope: &d2d::Envelope) -> Vec<u8> {
        let nonce = self.nonce();
        seal_nonce_ahead(&self.keys.reflect, &nonce, &envelope.encode_to_vec())
    }

    /// Seals raw `plaintext` (bytes that need not be a `d2d.Envelope`: TH5 A2's garbage and unknown-oneof
    /// cases) under DGRK with a fresh nonce.
    pub fn seal_bytes(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let nonce = self.nonce();
        seal_nonce_ahead(&self.keys.reflect, &nonce, plaintext)
    }

    /// Reflects `sealed` (a sealed envelope) with `flags`; returns its reflect-id.
    #[track_caller]
    pub fn reflect_sealed(&mut self, sealed: &[u8], flags: u16) -> u32 {
        let conn = self.conn.expect("the phone is connected");
        let id = self.next_reflect_id;
        self.next_reflect_id += 1;
        let mut p = vec![u8::try_from(REFLECT_HEADER).expect("8 fits u8"), 0];
        p.extend_from_slice(&flags.to_le_bytes());
        p.extend_from_slice(&id.to_le_bytes());
        p.extend_from_slice(sealed);
        self.mediator.receive(conn, datagram(kind::REFLECT, &p));
        self.pump();
        id
    }

    /// Seals and reflects `envelope`; returns the sealed bytes (what every other device must get
    /// byte for byte).
    pub fn reflect(&mut self, envelope: &d2d::Envelope) -> Vec<u8> {
        let sealed = self.seal(envelope);
        self.reflect_sealed(&sealed, 0);
        sealed
    }

    /// Acks the `Reflected` with `id`.
    pub fn ack(&mut self, id: u32) {
        let conn = self.conn.expect("the phone is connected");
        let mut p = vec![0u8; 4];
        p.extend_from_slice(&id.to_le_bytes());
        self.mediator
            .receive(conn, datagram(kind::REFLECTED_ACK, &p));
        self.pump();
    }

    /// Closes its connection with 1000 (the phone goes offline).
    pub fn disconnect(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.mediator.client_close(conn, 1000);
        }
    }

    /// What the mediator sent, since the last call.
    pub fn take_events(&mut self) -> Vec<PhoneEvent> {
        self.pump();
        std::mem::take(&mut self.events)
    }

    /// The envelopes of the `Reflected` it got, since the last call: (reflected-id, sealed).
    pub fn take_reflected(&mut self) -> Vec<(u32, Vec<u8>)> {
        self.pump();
        std::mem::take(&mut self.reflected)
            .into_iter()
            .map(|(id, _, sealed)| (id, sealed))
            .collect()
    }

    /// TH6 L2b: begins a transaction of `scope` (sealed with DGTSK, `ttl_s` seconds, 0 = the server's maximum) and
    /// returns what the mediator answered: `TxnBegun` (the phone now HOLDS the group's lock until
    /// [`Phone::commit_transaction`] or its disconnect) or `TxnRejected`. Other events stay for [`Phone::take_events`].
    #[track_caller]
    pub fn begin_transaction(&mut self, scope: Scope, ttl_s: u32) -> PhoneEvent {
        let conn = self.conn.expect("the phone is connected");
        let nonce = self.nonce();
        let begin = d2m::BeginTransaction {
            encrypted_scope: seal_scope(&self.keys, &nonce, scope),
            ttl: ttl_s,
        };
        self.mediator.receive(
            conn,
            datagram(kind::BEGIN_TRANSACTION, &begin.encode_to_vec()),
        );
        self.answer(|e| matches!(e, PhoneEvent::TxnBegun | PhoneEvent::TxnRejected { .. }))
    }

    /// TH6 L2b: commits the phone's transaction; whether `CommitTransactionAck` came.
    #[track_caller]
    pub fn commit_transaction(&mut self) -> bool {
        let conn = self.conn.expect("the phone is connected");
        self.mediator.receive(
            conn,
            datagram(
                kind::COMMIT_TRANSACTION,
                &d2m::CommitTransaction {}.encode_to_vec(),
            ),
        );
        self.pump();
        self.answer(|e| matches!(e, PhoneEvent::TxnCommitted)) == PhoneEvent::TxnCommitted
    }

    /// The first event since the last [`Phone::take_events`] that `is` takes, taken out of the list; `Closed(0)` when
    /// none came.
    fn answer(&mut self, is: impl Fn(&PhoneEvent) -> bool) -> PhoneEvent {
        self.pump();
        match self.events.iter().position(is) {
            Some(at) => self.events.remove(at),
            None => PhoneEvent::Closed(0),
        }
    }

    /// TH6 L2b, THE PHONE TAKES THE QUEUE (module doc): only while the phone's connection leads (`None` otherwise, nothing
    /// taken), it takes what the chat server queued for `user`, runs the takeover's receive steps on each message in
    /// order and acks it (but a "no server ack" one); returns what it did with each.
    pub fn lead_csp_queue(&mut self, user: &str) -> Option<Vec<Led>> {
        self.incoming();
        let conn = self.conn?;
        if !self.leads() {
            return None;
        }
        let taken = self.mediator.csp_take_for_leader(conn, user)?;
        let posted = self.mediator.leader_log().posted;
        let mut out = Vec::new();
        for m in taken {
            let sender = String::from_utf8_lossy(&m.sender).into_owned();
            let meta_len = m
                .payload
                .get(30..32)
                .map_or(0, |b| usize::from(u16::from_le_bytes([b[0], b[1]])));
            let nonce: Option<[u8; 24]> = m
                .payload
                .get(HEAD + meta_len..HEAD + meta_len + NONCE)
                .and_then(|n| n.try_into().ok());
            let letter = posted.iter().find(|p| {
                p.sender == sender && p.message_id == m.message_id && p.bytes == m.payload
            });
            let seen_nonce = nonce.is_some_and(|n| {
                self.led_nonces.contains(&n)
                    || self
                        .view
                        .incoming()
                        .iter()
                        .any(|v| v.nonce.as_slice() == n.as_slice())
            });
            let seen_id = self
                .view
                .incoming_ids()
                .contains(&(sender.clone(), m.message_id))
                || self.led.iter().any(|l| {
                    l.sender == sender
                        && l.message_id == m.message_id
                        && matches!(l.outcome, LedOutcome::Reflected { .. })
                });
            let outcome = match letter {
                _ if seen_nonce => LedOutcome::NonceUsed,
                None => LedOutcome::Unopened,
                Some(letter) if letter.discard => LedOutcome::Discarded,
                Some(_) if seen_id => LedOutcome::Seen,
                Some(letter) => {
                    let envelope = d2d::Envelope {
                        #[expect(deprecated, reason = "the spec's padding field; empty here")]
                        padding: vec![],
                        device_id: self.device,
                        protocol_version: d2d::ProtocolVersion::V03 as u32,
                        content: Some(d2d::envelope::Content::IncomingMessage(
                            d2d::IncomingMessage {
                                sender_identity: sender.clone(),
                                message_id: m.message_id,
                                created_at: letter.created_ms,
                                r#type: i32::from(letter.ty),
                                body: letter.body.clone(),
                                nonce: letter.nonce.to_vec(),
                            },
                        )),
                    };
                    let sealed = self.seal(&envelope);
                    let reflect_id = self.reflect_sealed(&sealed, 0);
                    LedOutcome::Reflected { reflect_id }
                }
            };
            if let Some(n) = nonce
                && outcome != LedOutcome::Unopened
            {
                self.led_nonces.push(n);
            }
            if m.flags & flag::NO_SERVER_ACK == 0 {
                self.mediator
                    .csp_ack_for_leader(conn, user, m.sender, m.message_id);
            }
            let led = Led {
                sender,
                message_id: m.message_id,
                outcome,
            };
            self.led.push(led.clone());
            out.push(led);
        }
        self.pump();
        Some(out)
    }

    /// TH6 L2b: everything the phone did as leader with the chat server's queue so far.
    pub fn led(&self) -> &[Led] {
        &self.led
    }

    /// Whether the mediator promoted the phone's current connection to leader (TH6 L2a).
    pub fn leads(&mut self) -> bool {
        self.pump();
        self.promoted && self.conn.is_some_and(|c| self.mediator.is_leader(c))
    }

    /// TH6 L2a, THE FOLLOWER VIEW (module doc): opens and acks what the leader reflected since the last call
    /// (the backlog of a login too), discards a second copy by its D2D nonce (acked all the same), and returns
    /// what was new, in order. A reflection that does not open or decode is acked, recorded as a violation and
    /// left out.
    pub fn incoming(&mut self) -> Vec<Viewed> {
        self.pump();
        let mut new = Vec::new();
        for (id, flags, sealed) in std::mem::take(&mut self.reflected) {
            let ephemeral = flags & EPHEMERAL != 0;
            if !ephemeral {
                self.ack(id);
            }
            let Some(plain) = open_nonce_ahead(&self.keys.reflect, &sealed) else {
                self.violations.record(
                    "phone-envelope-does-not-open",
                    &[("reflected", u64::from(id))],
                );
                continue;
            };
            let d2d_nonce: [u8; 24] = sealed[..24]
                .try_into()
                .expect("an opened box has its nonce");
            if self.view.seen.iter().any(|v| v.d2d_nonce == d2d_nonce) {
                self.view.replays += 1;
                continue;
            }
            let Ok(envelope) = d2d::Envelope::decode(plain.as_slice()) else {
                self.violations.record(
                    "phone-envelope-undecodable",
                    &[("reflected", u64::from(id))],
                );
                continue;
            };
            let viewed = Viewed {
                reflected_id: id,
                ephemeral,
                d2d_nonce,
                envelope,
            };
            self.view.seen.push(viewed.clone());
            new.push(viewed);
        }
        new
    }

    /// What the phone saw as a follower so far ([`Phone::incoming`] feeds it).
    pub fn view(&mut self) -> &View {
        self.incoming();
        &self.view
    }
}

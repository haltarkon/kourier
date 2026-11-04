//! The scripted PHONE of a device group (TH2 M3; S-mock §4.2): a test-side device, not a server.
//! It holds the device group's keys (the mediator never does), logs in to the fake mediator with
//! its own minimal D2M client written from the spec on the fake's crypto (a second implementation
//! beside libthreema's, so the mediator is not only ever checked against one client), reflects
//! envelopes it seals with DGRK, and opens and acks what the others reflected.
//!
//! Sans-I/O like the mediator: every call works on the mediator's outbox of the phone's
//! connection at once ([`Phone::pump`]).

use libthreema::protobuf::{d2d, d2m};
use prost::Message as _;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{RngCore as _, SeedableRng as _};

use super::MockWorld;
use super::crypto::{DeviceGroupKeys, hsalsa_shared, open_nonce_ahead, seal_nonce_ahead};
use super::d2m::{ConnId, Mediator, Out, REFLECT_HEADER, datagram, kind, split};

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

/// What the phone got from the mediator, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhoneEvent {
    ServerInfo { queue_length: u32 },
    Reflected { id: u32, flags: u16, timestamp: u64 },
    ReflectAck { id: u32, timestamp: u64 },
    Dry,
    Promoted,
    Proxy,
    Closed(u16),
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
    /// The envelopes of the `Reflected` it got: (reflected-id, sealed envelope).
    reflected: Vec<(u32, Vec<u8>)>,
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
                kind::ROLE_PROMOTED_TO_LEADER => PhoneEvent::Promoted,
                kind::PROXY => PhoneEvent::Proxy,
                kind::REFLECT_ACK => PhoneEvent::ReflectAck {
                    id: u32::from_le_bytes(p[4..8].try_into().expect("an id")),
                    timestamp: u64::from_le_bytes(p[8..16].try_into().expect("a timestamp")),
                },
                kind::REFLECTED => {
                    let header = usize::from(p[0]);
                    let id = u32::from_le_bytes(p[4..8].try_into().expect("an id"));
                    self.reflected.push((id, p[header..].to_vec()));
                    PhoneEvent::Reflected {
                        id,
                        flags: u16::from_le_bytes(p[2..4].try_into().expect("flags")),
                        timestamp: u64::from_le_bytes(p[8..16].try_into().expect("a timestamp")),
                    }
                }
                _ => panic!("the fake sent the phone a type it does not know"),
            };
            self.events.push(event);
        }
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
    }
}

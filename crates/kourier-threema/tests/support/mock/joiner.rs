//! The scripted EXISTING DEVICE of a join (TH4 J2): the phone that scans Kourier's QR and plays the
//! Rendezvous Responder Device (RRD) and nominator of `TP/md-d2d-rendezvous.proto`, then ED of
//! `TP/md-d2d-join.proto` (`Begin`, `BlobData`*, `EssentialData`, waits for `Registered`).
//!
//! A SECOND implementation beside libthreema's (rule 9): the QR is decoded with this file's own
//! prost struct and its own url-safe base64; the keys are the fake's crypto on registry crates
//! (BLAKE2b keyed/salted/personal as [`blake2b_mac`], the unkeyed salted path hash on the registry
//! `Blake2bVarCore`, X25519 + HSalsa20 as [`hsalsa_shared`], ChaCha20-Poly1305 from the registry
//! `chacha20poly1305` crate with the spec's nonce `u32-le(PID) || u32-le(SN) || 0000`); the frame
//! is `extra.transport.frame` (`TP/extra.struct.yml`: `u32-le(length) || data`).
//!
//! Sans-I/O: it speaks to the world's [`Relay`] as the RRD side; [`Joiner::pump`] takes what the
//! relay holds for it and answers at once (the fake's connector pumps every joiner of the world at
//! each flush). What the CLIENT (the RID) does wrong is recorded as a `rendezvous-*` or `join-*`
//! violation (a frame over 16 KiB before the nomination, a QR the phone refuses: direct TCP, a path id
//! twice, ...), and the joiner closes as the spec says. Its own behaviour is set by [`JoinerKnobs`].
//!
//! Nothing here formats a key, the path hash, an ID or a URL.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use blake2::Blake2bVarCore;
use blake2::digest::core_api::{Buffer, UpdateCore as _, VariableOutputCore as _};
use chacha20poly1305::aead::Aead as _;
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit as _, Nonce};
use libthreema::protobuf::{common, d2d_join, d2d_rendezvous};
use prost::Message as _;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{RngCore as _, SeedableRng as _};

use super::crypto::{blake2b_mac, hsalsa_shared, x25519_public};
use super::d2m::Out;
use super::essential::Essential;
use super::relay::{Relay, Role, SideId};
use super::{Clock, MockWorld, Violations, lock};

use d2d_rendezvous::handshake::{rid_to_rrd, rrd_to_rid};

/// The text in front of the base64 of a join QR (Desktop's `getJoinUri`).
pub const QR_PREFIX: &str = "threema://device-group/join#";

const PERSONAL: &[u8] = b"3ma-rendezvous";

/// `url.DeviceGroupJoinRequestOrOffer` (`TP/url-payloads.proto`), the fake's OWN struct (libthreema
/// does not build this file): fields 1-4, with libthreema's generated `RendezvousInit` inside.
#[derive(Clone, PartialEq, prost::Message)]
pub struct JoinRequestOrOffer {
    /// `Version` (`V1_0` = 0), read as its varint.
    #[prost(int32, tag = "1")]
    pub version: i32,
    #[prost(message, optional, tag = "2")]
    pub variant: Option<Variant>,
    #[prost(message, optional, tag = "3")]
    pub rendezvous_init: Option<d2d_rendezvous::RendezvousInit>,
    #[prost(uint32, tag = "4")]
    pub d2d_protocol_version: u32,
}

/// `DeviceGroupJoinRequestOrOffer.Variant`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Variant {
    #[prost(oneof = "VariantType", tags = "1, 2")]
    pub r#type: Option<VariantType>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub enum VariantType {
    #[prost(message, tag = "1")]
    RequestToJoin(common::Unit),
    #[prost(message, tag = "2")]
    OfferToJoin(common::Unit),
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// URL-safe base64 WITH `=` padding (what Desktop's `btoa` + `+/` -> `-_` writes), the fake's own.
pub fn url_safe_base64(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(B64[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decodes [`url_safe_base64`]: padded to a multiple of 4, the url-safe alphabet only. [U: real
/// phone] whether the phone also takes the unpadded form; Kourier writes Desktop's padded one.
pub fn url_safe_base64_decode(text: &str) -> Option<Vec<u8>> {
    let b = text.as_bytes();
    if b.is_empty() || !b.len().is_multiple_of(4) {
        return None;
    }
    let value = |c: u8| B64.iter().position(|x| *x == c).map(|v| v as u32);
    let chunks = b.len() / 4;
    let mut out = Vec::with_capacity(chunks * 3);
    for (ci, chunk) in b.chunks(4).enumerate() {
        let pad = chunk.iter().rev().take_while(|c| **c == b'=').count();
        if pad > 2 || (pad > 0 && ci + 1 != chunks) {
            return None;
        }
        let mut n = 0u32;
        for c in &chunk[..4 - pad] {
            n = n << 6 | value(*c)?;
        }
        n <<= 6 * pad;
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    Some(out)
}

/// `BLAKE2b(key=<key>, salt=<salt>, personal='3ma-rendezvous')` (RIDAK, RRDAK, STK, RIDTK, RRDTK).
pub fn rendezvous_key(key: &[u8], salt: &[u8]) -> [u8; 32] {
    blake2b_mac(key, salt, PERSONAL, &[])
}

/// `RPH = BLAKE2b(out-length=32, salt='ph', personal='3ma-rendezvous', input=STK)`: UNKEYED, so not
/// the registry `Blake2bMac` (whose empty key still feeds a key block); the variable core with the
/// parameter block of an unkeyed 32-byte hash.
pub fn path_hash(stk: &[u8; 32]) -> [u8; 32] {
    let mut core = Blake2bVarCore::new_with_params(b"ph", PERSONAL, 0, 32);
    let mut buffer = Buffer::<Blake2bVarCore>::default();
    buffer.digest_blocks(stk, |blocks| core.update_blocks(blocks));
    let mut out = Default::default();
    core.finalize_variable_core(&mut buffer, &mut out);
    let mut rph = [0u8; 32];
    rph.copy_from_slice(&out[..32]);
    rph
}

/// The spec's nonce: `u32-le(PID) || u32-le(SN) || <4 zero bytes>`.
fn nonce(pid: u32, sn: u32) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..4].copy_from_slice(&pid.to_le_bytes());
    n[4..8].copy_from_slice(&sn.to_le_bytes());
    n
}

/// ChaCha20-Poly1305 of one frame: `ciphertext || tag(16)`.
pub fn seal_frame(key: &[u8; 32], pid: u32, sn: u32, plaintext: &[u8]) -> Vec<u8> {
    ChaCha20Poly1305::new(Key::from_slice(key))
        .encrypt(Nonce::from_slice(&nonce(pid, sn)), plaintext)
        .expect("chacha20poly1305 seal")
}

/// Opens what [`seal_frame`] made; `None` when the tag does not match.
pub fn open_frame(key: &[u8; 32], pid: u32, sn: u32, boxed: &[u8]) -> Option<Vec<u8>> {
    ChaCha20Poly1305::new(Key::from_slice(key))
        .decrypt(Nonce::from_slice(&nonce(pid, sn)), boxed)
        .ok()
}

/// One `extra.transport.frame`: `u32-le(length) || data`.
pub fn frame(data: &[u8]) -> Vec<u8> {
    let length = u32::try_from(data.len()).expect("a frame fits u32");
    let mut out = length.to_le_bytes().to_vec();
    out.extend_from_slice(data);
    out
}

/// Where the joiner is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinStep {
    /// Not scanned yet.
    Idle,
    /// Hello sent, waiting for the RID's AuthHello.
    Scanned,
    /// Auth sent; the path awaits the joiner's nomination.
    Authenticated,
    /// Nominate sent; RPH known.
    Nominated,
    /// `Begin` sent.
    Begun,
    /// `EssentialData` sent; waiting for `Registered`.
    EssentialSent,
    /// `Registered` came.
    Registered,
    /// The path ended (by either side or the relay).
    Closed,
}

/// The joiner's own behaviour (defaults: a phone that does everything right at once).
#[derive(Clone, Debug, Default)]
pub struct JoinerKnobs {
    /// Runs on another AK than the QR's (the RID cannot open its Hello).
    pub wrong_ak: bool,
    /// How long after the handshake it nominates (virtual time; the spec's "brief timeout").
    pub nominate_after: Duration,
    /// Never nominates.
    pub never_nominate: bool,
    /// Does not send `Begin` (and what follows) until [`Joiner::release_begin`].
    pub withhold_begin: bool,
    /// Cuts each `BlobData` frame over this many WebSocket messages (the spec allows a frame
    /// across datagrams).
    pub blob_split: Option<usize>,
    /// Sends `(count, bytes)` UNREFERENCED `BlobData` before the referenced ones (H4).
    pub blob_flood: Option<(usize, usize)>,
    /// Sends the `EssentialData` twice.
    pub essential_twice: bool,
    /// Closes with this code once it reached this step.
    pub close_with: Option<(JoinStep, u16)>,
    /// Leaves out the first referenced blob.
    pub skip_blob: bool,
    /// S7: sends one extra `BlobData` with an id of this many bytes (and no data) before the
    /// referenced ones; any length but 16 is a blob id the spec does not have.
    pub blob_id_len: Option<usize>,
}

/// Why a scan did not start a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanError {
    /// The QR text is not a join request the phone takes (recorded as a `join-qr-*` violation).
    Unreadable(&'static str),
}

/// A frame the RID sends before the joiner nominated may be 16 KiB at most (libthreema's
/// `MAX_LENGTH_BEFORE_NOMINATION`, `LT/d2d_rendezvous/frame.rs`); after it 100 MiB.
pub const MAX_FRAME_BEFORE_NOMINATION: usize = 16_384;

struct Inner {
    relay: Relay,
    clock: Clock,
    violations: Violations,
    rng: ChaCha20Rng,
    essential: Essential,
    knobs: JoinerKnobs,
    step: JoinStep,
    side: Option<SideId>,
    pid: u32,
    ak: [u8; 32],
    challenge: [u8; 16],
    etk_secret: [u8; 32],
    /// The RRD's encryption key and sequence number (RRDAK, then RRDTK; the number goes on).
    out_key: [u8; 32],
    out_sn: u32,
    /// The RID's (RIDAK, then RIDTK).
    in_key: [u8; 32],
    in_sn: u32,
    /// RIDTK and RRDTK once the handshake is done.
    transport: Option<([u8; 32], [u8; 32])>,
    authenticated_at_ms: u64,
    rph: Option<[u8; 32]>,
    buffer: Vec<u8>,
    /// ULP plaintexts the RID sent after nomination, in order.
    ulp_in: Vec<Vec<u8>>,
    /// The code the path ended with as the joiner saw it (sent by it, or to it).
    closed: Option<u16>,
    /// What the joiner sent, as (frame name, WebSocket messages).
    sent: Vec<(&'static str, usize)>,
}

/// The scripted existing device: a cloneable handle.
#[derive(Clone)]
pub struct Joiner(Arc<Mutex<Inner>>);

impl std::fmt::Debug for Joiner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Joiner")
    }
}

impl MockWorld {
    /// A joiner that will hand over `essential`, pumped at every flush of the world's network.
    pub fn joiner(&self, essential: Essential, knobs: JoinerKnobs) -> Joiner {
        let joiner = Joiner(Arc::new(Mutex::new(Inner {
            relay: self.relay().clone(),
            clock: self.clock().clone(),
            violations: self.violations().clone(),
            rng: ChaCha20Rng::seed_from_u64(self.random_u64()),
            essential,
            knobs,
            step: JoinStep::Idle,
            side: None,
            pid: 0,
            ak: [0; 32],
            challenge: [0; 16],
            etk_secret: [0; 32],
            out_key: [0; 32],
            out_sn: 1,
            in_key: [0; 32],
            in_sn: 1,
            transport: None,
            authenticated_at_ms: 0,
            rph: None,
            buffer: Vec::new(),
            ulp_in: Vec::new(),
            closed: None,
            sent: Vec::new(),
        })));
        self.net().add_joiner(joiner.clone());
        joiner
    }
}

/// Decodes a join QR text as the phone does; the failing check's rule on error.
pub fn decode_qr(text: &str) -> Result<JoinRequestOrOffer, &'static str> {
    let b64 = text.strip_prefix(QR_PREFIX).ok_or("join-qr-prefix")?;
    let bytes = url_safe_base64_decode(b64).ok_or("join-qr-base64")?;
    let offer = JoinRequestOrOffer::decode(bytes.as_slice()).map_err(|_| "join-qr-protobuf")?;
    if offer.version != 0 {
        return Err("join-qr-version");
    }
    if !matches!(
        offer.variant.as_ref().and_then(|v| v.r#type.as_ref()),
        Some(VariantType::RequestToJoin(_))
    ) {
        return Err("join-qr-variant");
    }
    // [U: real phone] which D2D protocol versions the phone takes in a join: V0_2 (Desktop) and
    // V0_3 (C9, what libthreema's reflections carry).
    if !matches!(offer.d2d_protocol_version, 2 | 3) {
        return Err("join-qr-d2d-protocol-version");
    }
    let init = offer
        .rendezvous_init
        .as_ref()
        .ok_or("join-qr-no-rendezvous-init")?;
    if init.version != 0 {
        return Err("join-qr-rendezvous-version");
    }
    if init.ak.len() != 32 {
        return Err("join-qr-ak-length");
    }
    let path = init
        .relayed_web_socket
        .as_ref()
        .ok_or("join-qr-no-relayed-path")?;
    if !path.url.starts_with("wss://") {
        return Err("join-qr-url-not-wss");
    }
    // "If any `path_id` is not unique, abort these steps." (`TP/md-d2d-rendezvous.proto`, P10)
    let mut ids = vec![path.path_id];
    if let Some(direct) = init.direct_tcp_server.as_ref() {
        ids.extend(direct.ip_addresses.iter().map(|a| a.path_id));
    }
    let unique = ids.iter().enumerate().all(|(i, id)| !ids[..i].contains(id));
    if !unique {
        return Err("join-qr-path-id-not-unique");
    }
    // Kourier makes no TCP server (a join QR of this lane carries one relayed path only; P10).
    if init.direct_tcp_server.is_some() {
        return Err("join-qr-direct-tcp");
    }
    Ok(offer)
}

impl Inner {
    fn record(&self, rule: &'static str) {
        self.violations
            .record(rule, &[("pid", u64::from(self.pid))]);
    }

    fn side(&self) -> SideId {
        self.side.expect("the joiner scanned")
    }

    /// Seals `plaintext` with the current RRD key and sends it as one frame (cut into `pieces`
    /// WebSocket messages).
    fn send(&mut self, name: &'static str, plaintext: &[u8], pieces: usize) {
        if self.closed.is_some() {
            return;
        }
        let boxed = seal_frame(&self.out_key, self.pid, self.out_sn, plaintext);
        self.out_sn = self.out_sn.checked_add(1).expect("RRDSN overflow");
        let bytes = frame(&boxed);
        let pieces = pieces.clamp(1, bytes.len());
        let size = bytes.len().div_ceil(pieces);
        let side = self.side();
        let mut count = 0;
        for piece in bytes.chunks(size) {
            self.relay.receive(side, piece.to_vec());
            count += 1;
        }
        self.sent.push((name, count));
    }

    fn close(&mut self, code: u16) {
        if self.closed.is_some() {
            return;
        }
        if let Some(side) = self.side {
            self.relay.client_close(side, code);
        }
        self.closed = Some(code);
        self.step = JoinStep::Closed;
    }

    /// Moves to `step`, and closes when the knob says so.
    fn reach(&mut self, step: JoinStep) {
        self.step = step;
        if let Some((at, code)) = self.knobs.close_with
            && at == step
        {
            self.close(code);
        }
    }

    fn scan(&mut self, text: &str) -> Result<(), ScanError> {
        let offer = decode_qr(text).map_err(|rule| {
            self.violations.record(rule, &[]);
            ScanError::Unreadable(rule)
        })?;
        let init = offer.rendezvous_init.expect("checked");
        let path = init.relayed_web_socket.expect("checked");
        self.ak.copy_from_slice(&init.ak);
        if self.knobs.wrong_ak {
            for b in &mut self.ak {
                *b ^= 0x5a;
            }
        }
        self.pid = path.path_id;
        let side = self.relay.connect(&path.url, Role::Rrd);
        self.side = Some(side);
        self.rng.fill_bytes(&mut self.challenge);
        self.rng.fill_bytes(&mut self.etk_secret);
        self.out_key = rendezvous_key(&self.ak, b"rrda");
        self.in_key = rendezvous_key(&self.ak, b"rida");
        let hello = rrd_to_rid::Hello {
            challenge: self.challenge.to_vec(),
            etk: x25519_public(&self.etk_secret).to_vec(),
        };
        self.send("hello", &hello.encode_to_vec(), 1);
        self.reach(JoinStep::Scanned);
        Ok(())
    }

    fn open_in(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        let plain = open_frame(&self.in_key, self.pid, self.in_sn, data)?;
        self.in_sn = self.in_sn.checked_add(1)?;
        Some(plain)
    }

    fn on_auth_hello(&mut self, data: &[u8]) {
        let Some(plain) = self.open_in(data) else {
            self.record("rendezvous-auth-hello-refused");
            self.close(4000);
            return;
        };
        let checked = rid_to_rrd::AuthHello::decode(plain.as_slice())
            .ok()
            .filter(|a| a.response == self.challenge && a.challenge.len() == 16)
            .and_then(|a| {
                let etk: [u8; 32] = a.etk.as_slice().try_into().ok()?;
                Some((a.challenge, etk))
            });
        let Some((challenge, rid_etk)) = checked else {
            self.record("rendezvous-auth-hello-refused");
            self.close(4000);
            return;
        };
        let Some(shared) = hsalsa_shared(&self.etk_secret, &rid_etk) else {
            self.record("rendezvous-etk-non-contributory");
            self.close(4000);
            return;
        };
        let mut key = [0u8; 64];
        key[..32].copy_from_slice(&self.ak);
        key[32..].copy_from_slice(&shared);
        let stk = rendezvous_key(&key, b"st");
        let ridtk = rendezvous_key(&stk, b"ridt");
        let rrdtk = rendezvous_key(&stk, b"rrdt");
        self.rph = Some(path_hash(&stk));
        self.transport = Some((ridtk, rrdtk));
        let auth = rrd_to_rid::Auth {
            response: challenge,
        };
        self.send("auth", &auth.encode_to_vec(), 1);
        // From here on both directions continue their sequence numbers under the transport keys.
        self.out_key = rrdtk;
        self.in_key = ridtk;
        self.authenticated_at_ms = self.clock.now_ms();
        self.reach(JoinStep::Authenticated);
    }

    fn nominate(&mut self) {
        self.send("nominate", &d2d_rendezvous::Nominate {}.encode_to_vec(), 1);
        self.reach(JoinStep::Nominated);
        if self.step == JoinStep::Nominated && !self.knobs.withhold_begin {
            self.send_join_data();
        }
    }

    fn ed_to_nd(content: d2d_join::ed_to_nd::Content) -> Vec<u8> {
        d2d_join::EdToNd {
            content: Some(content),
        }
        .encode_to_vec()
    }

    fn send_join_data(&mut self) {
        use d2d_join::ed_to_nd::Content;
        self.send(
            "begin",
            &Self::ed_to_nd(Content::Begin(d2d_join::Begin {})),
            1,
        );
        self.reach(JoinStep::Begun);
        if self.step != JoinStep::Begun {
            return;
        }
        let pieces = self.knobs.blob_split.unwrap_or(1);
        if let Some(len) = self.knobs.blob_id_len {
            let mut id = vec![0u8; len];
            self.rng.fill_bytes(&mut id);
            let blob = common::BlobData {
                id,
                data: Vec::new(),
            };
            self.send(
                "blob-data",
                &Self::ed_to_nd(Content::BlobData(blob)),
                pieces,
            );
        }
        if let Some((count, bytes)) = self.knobs.blob_flood {
            for _ in 0..count {
                let mut id = [0u8; 16];
                self.rng.fill_bytes(&mut id);
                let blob = common::BlobData {
                    id: id.to_vec(),
                    data: vec![0x5a; bytes],
                };
                self.send(
                    "blob-data",
                    &Self::ed_to_nd(Content::BlobData(blob)),
                    pieces,
                );
            }
        }
        let skip = usize::from(self.knobs.skip_blob);
        for blob in self.essential.blobs.clone().into_iter().skip(skip) {
            self.send(
                "blob-data",
                &Self::ed_to_nd(Content::BlobData(blob)),
                pieces,
            );
        }
        let data = Self::ed_to_nd(Content::EssentialData(self.essential.data.clone()));
        self.send("essential-data", &data, 1);
        if self.knobs.essential_twice {
            self.send("essential-data", &data, 1);
        }
        self.reach(JoinStep::EssentialSent);
    }

    /// A frame of the RID after the handshake.
    fn on_transport_frame(&mut self, data: &[u8]) {
        let opened = self.open_in(data);
        if self.step == JoinStep::Authenticated {
            // Before the joiner nominated, the RID must stay silent: a Nominate (empty under
            // RIDTK) is a nomination by the RID, anything else an ULP frame too early.
            let rule = match opened {
                Some(p) if p.is_empty() => "rendezvous-rid-nominated",
                _ => "rendezvous-ulp-before-nomination",
            };
            self.record(rule);
            self.close(4000);
            return;
        }
        let Some(plain) = opened else {
            self.record("rendezvous-frame-does-not-open");
            self.close(4000);
            return;
        };
        self.ulp_in.push(plain.clone());
        let registered = matches!(
            d2d_join::NdToEd::decode(plain.as_slice()).map(|m| m.content),
            Ok(Some(d2d_join::nd_to_ed::Content::Registered(_)))
        );
        match (registered, self.step) {
            (true, JoinStep::EssentialSent) => self.reach(JoinStep::Registered),
            (true, _) => {
                self.record("join-registered-out-of-order");
                self.close(4100);
            }
            (false, _) => {
                // [I] after `Registered` further messages belong to the History Exchange
                // Protocol, which the fake does not speak; before it, ND sends nothing else.
                self.record("join-unexpected-nd-to-ed");
                self.close(4100);
            }
        }
    }

    fn on_frame(&mut self, data: &[u8]) {
        match self.step {
            JoinStep::Scanned => self.on_auth_hello(data),
            JoinStep::Idle | JoinStep::Closed => {}
            _ => self.on_transport_frame(data),
        }
    }

    fn pump(&mut self) -> bool {
        let Some(side) = self.side else {
            return false;
        };
        let mut moved = false;
        if self.closed.is_none() {
            for out in self.relay.take(side) {
                moved = true;
                match out {
                    Out::Datagram(d) => self.buffer.extend_from_slice(&d),
                    Out::Close(code) => {
                        self.closed = Some(code);
                        self.step = JoinStep::Closed;
                    }
                }
            }
            while self.closed.is_none() && self.buffer.len() >= 4 {
                let length =
                    u32::from_le_bytes(self.buffer[..4].try_into().expect("4 bytes")) as usize;
                // P10: before the joiner's own nomination the RID's frames are 16 KiB at most, judged
                // at the HEADER as libthreema's decoder does.
                if matches!(self.step, JoinStep::Scanned | JoinStep::Authenticated)
                    && length > MAX_FRAME_BEFORE_NOMINATION
                {
                    self.record("rendezvous-oversized-frame");
                    self.close(4000);
                    moved = true;
                    break;
                }
                if self.buffer.len() < 4 + length {
                    break;
                }
                let data: Vec<u8> = self.buffer.drain(..4 + length).skip(4).collect();
                self.on_frame(&data);
                moved = true;
            }
        }
        let due = self.authenticated_at_ms
            + u64::try_from(self.knobs.nominate_after.as_millis()).expect("a knob fits u64 ms");
        if self.step == JoinStep::Authenticated
            && self.closed.is_none()
            && !self.knobs.never_nominate
            && self.clock.now_ms() >= due
        {
            self.nominate();
            moved = true;
        }
        moved
    }
}

impl Joiner {
    /// Scans a QR text: decodes it, connects to the relay as the RRD and sends Hello.
    pub fn scan(&self, text: &str) -> Result<(), ScanError> {
        lock(&self.0).scan(text)
    }

    /// Takes what the relay holds for the joiner and answers; runs its nomination timer. Whether
    /// anything moved.
    pub fn pump(&self) -> bool {
        lock(&self.0).pump()
    }

    /// Sends `Begin`, the blobs and `EssentialData` (after the knob `withhold_begin`).
    pub fn release_begin(&self) {
        let mut j = lock(&self.0);
        if j.step == JoinStep::Nominated {
            j.send_join_data();
        }
    }

    /// Closes the path now with `code`.
    pub fn close(&self, code: u16) {
        lock(&self.0).close(code);
    }

    /// Sends `plaintext` as one ULP frame (after nomination); whether it was sent.
    pub fn send_ulp(&self, plaintext: &[u8]) -> bool {
        let mut j = lock(&self.0);
        if j.transport.is_none() || j.closed.is_some() {
            return false;
        }
        j.send("ulp", plaintext, 1);
        true
    }

    pub fn step(&self) -> JoinStep {
        lock(&self.0).step
    }

    /// The path hash the phone would show (after the handshake).
    pub fn rph(&self) -> Option<[u8; 32]> {
        lock(&self.0).rph
    }

    /// The relay side the joiner speaks on (after a scan).
    pub fn side(&self) -> Option<SideId> {
        lock(&self.0).side
    }

    /// The code the path ended with, as the joiner saw it.
    pub fn closed(&self) -> Option<u16> {
        lock(&self.0).closed
    }

    /// The ULP plaintexts the RID sent after nomination, in order.
    pub fn ulp_received(&self) -> Vec<Vec<u8>> {
        lock(&self.0).ulp_in.clone()
    }

    /// What the joiner sent: (frame name, WebSocket messages it took), in order.
    pub fn sent(&self) -> Vec<(&'static str, usize)> {
        lock(&self.0).sent.clone()
    }

    /// Changes the knobs (read at each step).
    pub fn knobs(&self, change: impl FnOnce(&mut JoinerKnobs)) {
        change(&mut lock(&self.0).knobs);
    }
}

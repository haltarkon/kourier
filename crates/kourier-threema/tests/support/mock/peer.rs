//! The fake's CSP PEER (TH6a S3): a receiver written from the spec that opens what the client
//! sent, so the send core is checked against a second implementation of the format. It reads
//! `Mediator::csp_sent()` (what the chat server kept for it) and applies the RECEIVER's rules of
//! `message-with-metadata-box` (`TP/csp.struct.yml`, "When receiving this payload", steps 2 to 14)
//! plus the SENDER's rules of the struct's fields, with the fake's own crypto and the generated
//! `csp_e2e.MessageMetadata`; from Kourier's send code, and from libthreema's protocol code, nothing
//! (rule 9).
//!
//! Where the spec makes the receiver DISCARD and ack (a nonce used before, a message id received
//! before) the peer reports the message with [`Discard`] and is no violation: a resend of the very
//! same bytes is meant to be discarded by nonce, a resend under a fresh nonce by id. Where a discard
//! means the client broke a rule (a box that does not open, a metadata id that differs, a padding
//! that is no PKCS#7) the peer records a VIOLATION by a `peer-*` rule name in the world's list, which
//! every test ends by checking, and reports no [`Opened`] for that message.
//!
//! TH6 L2a: the peer also SENDS to the user ([`Peer::send`] of a [`Letter`]): a real
//! `message-with-metadata-box` from the spec (head, metadata box under the MMK, the container with its
//! PKCS#7 padding under the shared key S, one nonce for both boxes), sealed with the fake's own crypto
//! and queued at the chat server for the user's identity (`Mediator::csp_post`), which delivers it to
//! the device group's unblocked leader and redelivers it at every login until it is acked. A letter
//! can be bent the ways the receive steps discard (a reused nonce, another receiver in the head, a box
//! that does not open, a metadata id that differs, no metadata, a type the spec does not know, an FS
//! envelope written by hand, any padding); what it was is kept for the guard oracle (`leader.rs`).
//!
//! Marks: `[I]` where the peer reads the spec's "should" or a recommendation as a rule (the
//! oracle is not more forgiving than the real receiver), `[U: real phone]` where the pinned spec
//! does not say what a real app does. Nothing here prints a key, an ID, a nonce or a body: the
//! violations carry the rule name and the position of the message in `csp_sent` only, and
//! `Debug` of [`Opened`] prints the type, the flags and the length.

use std::collections::{BTreeMap, BTreeSet};

use libthreema::protobuf::{common, csp_e2e};
use prost::Message as _;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{RngCore as _, SeedableRng as _};

use super::crypto::{KeyPair, blake2b_mac, hsalsa_shared, open, seal};
use super::csp::{Sent, flag};
use super::d2m::Mediator;
use super::leader::Posted;
use super::{Clock, Identities, MockWorld, Violations, lock};

/// The fixed head of a `message-with-metadata-box`: sender (8), receiver (8), message id (8),
/// created-at (4), flags (1), reserved (1), metadata-length (2), legacy nickname (32).
pub const HEAD: usize = 64;
/// The nonce of the boxes.
pub const NONCE: usize = 24;
/// The legacy nickname field.
pub const LEGACY_NICKNAME: usize = 32;
/// The message flags the spec names (`TP/csp.struct.yml` `message-flags`); the other bits are unknown.
const KNOWN_FLAGS: u8 = flag::SEND_PUSH
    | flag::NO_SERVER_QUEUING
    | flag::NO_SERVER_ACK
    | 0x10 // reserved, formerly the group marker
    | flag::SHORT_LIVED
    | 0x80; // no automatic delivery receipts
/// "the total size of `padded-data` should be at least 32 bytes" (`container`).
const PADDED_FLOOR: usize = 32;

/// Why a receiver discards a message that is no breach of the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discard {
    /// Step 2: the nonce was used before (the same bytes again).
    Nonce,
    /// Step 14: the message id was received before from this sender (a fresh nonce).
    Id,
}

/// One message the peer opened.
#[derive(Clone, PartialEq, Eq)]
pub struct Opened {
    /// The sender's Threema ID.
    pub sender: String,
    pub message_id: u64,
    /// The head's `created-at` (seconds).
    pub created_at_s: u32,
    /// The metadata's `created-at` (ms).
    pub created_at_ms: u64,
    pub flags: u8,
    /// The container's message type.
    pub ty: u8,
    /// The container's data with the PKCS#7 padding removed.
    pub body: Vec<u8>,
    /// The metadata's nickname (`None`: the field is absent, `Some("")`: cleared).
    pub nickname: Option<String>,
    pub nonce: [u8; NONCE],
    /// What the receiver does with it (`None`: the receive steps go on).
    pub discard: Option<Discard>,
    /// The whole struct as the chat server kept it.
    pub bytes: Vec<u8>,
    /// The D2M connection it came in on.
    pub conn: u64,
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Opened(type {:#04x}, flags {:#04x}, body {} B, discard {:?})",
            self.ty,
            self.flags,
            self.body.len(),
            self.discard
        )
    }
}

/// A receiver in the world: an identity of the chat server's table with its client key, and what a
/// real receiver keeps: the nonces it used and the message ids it received.
pub struct Peer {
    id: String,
    ck: KeyPair,
    identities: Identities,
    mediator: Mediator,
    violations: Violations,
    /// How many entries of `csp_sent` `drain` has looked at.
    cursor: usize,
    /// The nonces used, with the bytes of the message that used them and its place in `opened`.
    used_nonces: BTreeMap<[u8; NONCE], (Vec<u8>, usize)>,
    /// (sender, message id) of every message received.
    seen_ids: BTreeSet<(String, u64)>,
    /// Everything opened so far, in order (discarded ones too).
    opened: Vec<Opened>,
    /// TH6 L2a: the world's clock (a letter's `created-at`) and the nonces of what it sends.
    clock: Clock,
    rng: ChaCha20Rng,
}

impl std::fmt::Debug for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Peer")
            .field("opened", &self.opened.len())
            .finish()
    }
}

impl MockWorld {
    /// The peer for `id`, which must be in the identity table (`add_identity`): the receiver of
    /// what the client sends to that ID.
    #[track_caller]
    pub fn peer(&self, id: &str) -> Peer {
        let identity = self
            .identity(id)
            .expect("a peer is an identity of the world's table");
        Peer {
            id: id.to_owned(),
            ck: identity.ck,
            identities: self.identities.clone(),
            mediator: self.mediator().clone(),
            violations: self.violations().clone(),
            cursor: 0,
            used_nonces: BTreeMap::new(),
            seen_ids: BTreeSet::new(),
            opened: Vec::new(),
            clock: self.clock().clone(),
            // From the world's seed and the ID, not from the world's RNG: a peer made for sending does not move
            // the nonces and ids of the rest of the world.
            rng: ChaCha20Rng::seed_from_u64(id.bytes().fold(self.seed() ^ 0x5045_4552, |h, b| {
                h.rotate_left(8) ^ u64::from(b)
            })),
        }
    }
}

/// How a [`Letter`]'s metadata box is made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LetterMeta {
    /// Sealed under the MMK, a `csp_e2e.MessageMetadata` with the letter's id, time and nickname.
    Normal,
    /// `metadata-length` 0 and no box (an old client's form; the receiver goes on without it).
    Absent,
    /// Sealed under the MMK, but the plaintext is no protobuf.
    Garbage,
}

/// A message a peer sends TO the user (TH6 L2a; module doc): every field of the struct the spec names,
/// each one a test may bend. Build with [`Letter::new`] / [`Letter::text`] and the methods; `Debug`
/// prints the id, the type, the flags and the length only.
#[derive(Clone)]
pub struct Letter {
    /// The identity whose queue at the chat server takes it (the user).
    pub to: String,
    pub message_id: u64,
    /// The metadata's `created-at` (ms); the head carries it in seconds. `None`: the world's clock now.
    pub created_ms: Option<u64>,
    /// The head's `created-at` in seconds when it is not the metadata's.
    pub head_s: Option<u32>,
    pub flags: u8,
    /// The container's type (any byte: 0x7e is a type the pinned spec does not know, 0xff the reserved one).
    pub ty: u8,
    /// The container's data before the padding.
    pub data: Vec<u8>,
    /// The padding bytes when they are not PKCS#7 up to 32 bytes of padded data.
    pub padding: Option<Vec<u8>>,
    /// The metadata's nickname (`None`: absent, `Some("")`: cleared).
    pub nickname: Option<String>,
    pub meta: LetterMeta,
    /// The metadata's message id when it is not the head's.
    pub meta_id: Option<u64>,
    /// The head's receiver when it is not `to`.
    pub head_receiver: Option<String>,
    /// The head's legacy nickname when it is not all zeros (the rule for a receiver that is no Gateway ID).
    pub legacy: Option<[u8; LEGACY_NICKNAME]>,
    /// The nonce of both boxes; `None`: a fresh one from the peer's RNG. A nonce of an earlier letter
    /// reuses it (the receiver's step 2).
    pub nonce: Option<[u8; NONCE]>,
    /// One byte of the message box flipped: the box does not open (the receiver's step 7).
    pub broken_box: bool,
    /// The test declares that the receiver may ack it without a reflect (a spec discard); the bending
    /// methods that make one set it.
    pub discard: bool,
}

impl std::fmt::Debug for Letter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Letter(id {}, type {:#04x}, flags {:#04x}, {} B)",
            self.message_id,
            self.ty,
            self.flags,
            self.data.len()
        )
    }
}

impl Letter {
    /// A letter of type `ty` with `data` to `to`, flags "send push" (0x01), metadata with no nickname,
    /// PKCS#7 padding, a fresh nonce, the world's time.
    pub fn new(to: &str, message_id: u64, ty: u8, data: &[u8]) -> Self {
        Self {
            to: to.to_owned(),
            message_id,
            created_ms: None,
            head_s: None,
            flags: flag::SEND_PUSH,
            ty,
            data: data.to_vec(),
            padding: None,
            nickname: None,
            meta: LetterMeta::Normal,
            meta_id: None,
            head_receiver: None,
            legacy: None,
            nonce: None,
            broken_box: false,
            discard: false,
        }
    }

    /// A text message (type 0x01, `text` as UTF-8).
    pub fn text(to: &str, message_id: u64, text: &str) -> Self {
        Self::new(to, message_id, 0x01, text.as_bytes())
    }

    #[must_use]
    pub fn flags(mut self, flags: u8) -> Self {
        self.flags = flags;
        self
    }

    #[must_use]
    pub fn created_ms(mut self, ms: u64) -> Self {
        self.created_ms = Some(ms);
        self
    }

    #[must_use]
    pub fn nickname(mut self, nickname: &str) -> Self {
        self.nickname = Some(nickname.to_owned());
        self
    }

    #[must_use]
    pub fn padding(mut self, padding: &[u8]) -> Self {
        self.padding = Some(padding.to_vec());
        self
    }

    #[must_use]
    pub fn meta(mut self, meta: LetterMeta) -> Self {
        self.meta = meta;
        self
    }

    #[must_use]
    pub fn legacy(mut self, legacy: [u8; LEGACY_NICKNAME]) -> Self {
        self.legacy = Some(legacy);
        self
    }

    /// The nonce of an earlier letter again (step 2: a spec discard).
    #[must_use]
    pub fn reused_nonce(mut self, nonce: [u8; NONCE]) -> Self {
        self.nonce = Some(nonce);
        self.discard = true;
        self
    }

    /// The head names `receiver`, not the user (step 3: a spec discard).
    #[must_use]
    pub fn wrong_receiver(mut self, receiver: &str) -> Self {
        self.head_receiver = Some(receiver.to_owned());
        self.discard = true;
        self
    }

    /// The message box does not open (step 7: a spec discard).
    #[must_use]
    pub fn broken_box(mut self) -> Self {
        self.broken_box = true;
        self.discard = true;
        self
    }

    /// The metadata names another message id (step 13: a spec discard).
    #[must_use]
    pub fn meta_id(mut self, id: u64) -> Self {
        self.meta_id = Some(id);
        self.discard = true;
        self
    }

    /// The test declares the letter a spec discard (any other reason).
    #[must_use]
    pub fn discarded(mut self) -> Self {
        self.discard = true;
        self
    }
}

impl Peer {
    /// This peer's Threema ID.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// TH6 L2a: seals `letter` from this peer to `letter.to` by the spec (module doc) and hands it to the chat
    /// server, which queues it for that identity and delivers it to its unblocked leader. Returns what was
    /// posted (the nonce, the unpadded body, the bytes; `queued`).
    #[track_caller]
    pub fn send(&mut self, letter: &Letter) -> Posted {
        let receiver_key = lock(&self.identities)
            .get(&letter.to)
            .map(|i| i.ck.public)
            .expect("a letter goes to an identity of the world's table");
        // S = X25519HSalsa20(<sender.CK>.secret, <receiver.CK>.public)
        let shared = hsalsa_shared(&self.ck.secret, &receiver_key).expect("contributory keys");
        let nonce = letter.nonce.unwrap_or_else(|| {
            let mut n = [0u8; NONCE];
            self.rng.fill_bytes(&mut n);
            n
        });
        let created_ms = letter.created_ms.unwrap_or_else(|| self.clock.now_ms());
        // The container: type, then the data and its padding (PKCS#7, the padded data at least 32 bytes).
        let mut container = vec![letter.ty];
        container.extend_from_slice(&letter.data);
        match &letter.padding {
            Some(padding) => container.extend_from_slice(padding),
            None => {
                let pad = PADDED_FLOOR.saturating_sub(letter.data.len()).max(1);
                container.extend(std::iter::repeat_n(
                    u8::try_from(pad).expect("a pad of at most 32"),
                    pad,
                ));
            }
        }
        let mut message_box = seal(&shared, &nonce, &container);
        if letter.broken_box
            && let Some(last) = message_box.last_mut()
        {
            *last ^= 0x01;
        }
        let metadata = csp_e2e::MessageMetadata {
            #[expect(
                deprecated,
                reason = "the spec's padding field: 16 bytes, ignored by the receiver"
            )]
            padding: vec![0; 16],
            message_id: letter.meta_id.unwrap_or(letter.message_id),
            created_at: created_ms,
            nickname: letter.nickname.clone(),
        }
        .encode_to_vec();
        let metadata_box = match letter.meta {
            LetterMeta::Absent => Vec::new(),
            LetterMeta::Normal => seal(&metadata_key(&shared), &nonce, &metadata),
            LetterMeta::Garbage => seal(&metadata_key(&shared), &nonce, &[0x0a, 0xff]),
        };
        let head_s = letter
            .head_s
            .unwrap_or_else(|| u32::try_from(created_ms / 1000).expect("seconds fit u32"));
        let head_receiver = letter.head_receiver.as_deref().unwrap_or(&letter.to);
        let mut bytes = Vec::with_capacity(HEAD + metadata_box.len() + NONCE + message_box.len());
        bytes.extend_from_slice(self.id.as_bytes());
        bytes.extend_from_slice(head_receiver.as_bytes());
        bytes.extend_from_slice(&letter.message_id.to_le_bytes());
        bytes.extend_from_slice(&head_s.to_le_bytes());
        bytes.push(letter.flags);
        bytes.push(0);
        bytes.extend_from_slice(
            &u16::try_from(metadata_box.len())
                .expect("a metadata box fits u16")
                .to_le_bytes(),
        );
        bytes.extend_from_slice(&letter.legacy.unwrap_or([0; LEGACY_NICKNAME]));
        assert_eq!(bytes.len(), HEAD, "the head is 64 bytes");
        bytes.extend_from_slice(&metadata_box);
        bytes.extend_from_slice(&nonce);
        bytes.extend_from_slice(&message_box);
        // What `IncomingMessage.created_at` carries: the metadata's ms, the head's seconds x 1000 without it.
        let created_ms = match letter.meta {
            LetterMeta::Normal => created_ms,
            LetterMeta::Absent | LetterMeta::Garbage => u64::from(head_s) * 1000,
        };
        self.mediator.csp_post(Posted {
            sender: self.id.clone(),
            to: letter.to.clone(),
            message_id: letter.message_id,
            created_ms,
            flags: letter.flags,
            ty: letter.ty,
            body: letter.data.clone(),
            nonce,
            bytes,
            discard: letter.discard,
            queued: false,
        })
    }

    /// Opens what the chat server kept for this peer since the last call, in order; the messages
    /// for other IDs are skipped. A message that broke a rule is a violation and not in the list.
    pub fn drain(&mut self) -> Vec<Opened> {
        let sent = self.mediator.csp_sent();
        let mut out = Vec::new();
        for (at, one) in sent.iter().enumerate().skip(self.cursor) {
            if one.receiver.as_slice() == self.id.as_bytes()
                && let Some(opened) = self.open_sent(one, at)
            {
                out.push(opened);
            }
        }
        self.cursor = sent.len();
        out
    }

    /// Everything this peer opened so far, in order, discarded copies included.
    pub fn opened(&self) -> &[Opened] {
        &self.opened
    }

    fn fail(&self, at: usize, rule: &'static str) -> Option<Opened> {
        self.violations
            .record(rule, &[("sent", u64::try_from(at).unwrap_or(u64::MAX))]);
        None
    }

    /// Applies the receiver's steps to ONE message (`at` is its place in `csp_sent`, for the
    /// violation). `None` and a violation when the client broke a rule. A test may call it with a
    /// [`Sent`] it made itself.
    #[track_caller]
    pub fn open_sent(&mut self, sent: &Sent, at: usize) -> Option<Opened> {
        assert!(
            sent.receiver.as_slice() == self.id.as_bytes(),
            "a message for another peer"
        );
        let bytes = &sent.bytes;
        if bytes.len() < HEAD + NONCE {
            return self.fail(at, "peer-head-malformed");
        }
        // The head, by the spec's offsets.
        let sender: [u8; 8] = bytes[..8].try_into().expect("8 bytes");
        let message_id = u64::from_le_bytes(bytes[16..24].try_into().expect("8 bytes"));
        let created_s = u32::from_le_bytes(bytes[24..28].try_into().expect("4 bytes"));
        let flags = bytes[28];
        let metadata_length = usize::from(u16::from_le_bytes([bytes[30], bytes[31]]));
        let legacy: [u8; LEGACY_NICKNAME] = bytes[32..HEAD].try_into().expect("32 bytes");
        if HEAD + metadata_length + NONCE > bytes.len() {
            return self.fail(at, "peer-head-malformed");
        }
        let metadata_box = &bytes[HEAD..HEAD + metadata_length];
        let nonce: [u8; NONCE] = bytes[HEAD + metadata_length..HEAD + metadata_length + NONCE]
            .try_into()
            .expect("24 bytes");
        let message_box = &bytes[HEAD + metadata_length + NONCE..];

        // Step 2: a nonce used before. The same bytes are a resend and the receiver discards it;
        // another message under a used nonce is a breach of the client (a nonce is used once).
        if let Some((first, index)) = self.used_nonces.get(&nonce) {
            if first != bytes {
                return self.fail(at, "peer-nonce-reused-with-other-bytes");
            }
            let mut again = self.opened[*index].clone();
            again.discard = Some(Discard::Nonce);
            again.conn = sent.conn;
            self.opened.push(again.clone());
            return Some(again);
        }

        let Ok(sender_text) = std::str::from_utf8(&sender).map(str::to_owned) else {
            return self.fail(at, "peer-sender-unknown");
        };
        let Some(sender_key) = lock(&self.identities)
            .get(&sender_text)
            .map(|i| i.ck.public)
        else {
            return self.fail(at, "peer-sender-unknown");
        };
        if flags & !KNOWN_FLAGS != 0 {
            return self.fail(at, "peer-flags-unknown-bits");
        }
        // `S = X25519HSalsa20(<sender.CK>.secret, <receiver.CK>.public)`, which the receiver gets
        // from its own secret and the sender's public key.
        let Some(shared) = hsalsa_shared(&self.ck.secret, &sender_key) else {
            return self.fail(at, "peer-key-not-contributory");
        };

        // Step 6 (and "for outgoing messages, a metadata box should always be present").
        if metadata_length == 0 {
            return self.fail(at, "peer-no-metadata-box");
        }
        let mmk = metadata_key(&shared);
        let Some(metadata_plain) = open(&mmk, &nonce, metadata_box) else {
            return self.fail(at, "peer-metadata-does-not-open");
        };
        let Ok(metadata) = csp_e2e::MessageMetadata::decode(metadata_plain.as_slice()) else {
            return self.fail(at, "peer-metadata-undecodable");
        };
        // Step 13: the metadata's id is the head's.
        if metadata.message_id != message_id {
            return self.fail(at, "peer-metadata-id");
        }
        // "the original timestamp is still available in an attached `csp-e2e.MessageMetadata`": the
        // head holds the same time in seconds unless the server replaced it.
        let metadata_s = u32::try_from(metadata.created_at / 1000).unwrap_or(u32::MAX);
        if !sent.replaced && created_s != metadata_s {
            return self.fail(at, "peer-created-at-differs");
        }
        // `MessageMetadata.nickname`: "Do not send the user's Threema ID".
        if metadata.nickname.as_deref() == Some(sender_text.as_str()) {
            return self.fail(at, "peer-nickname-is-the-id");
        }
        let gateway = self.id.starts_with('*');
        if !legacy_nickname_ok(gateway, &legacy, metadata.nickname.as_deref()) {
            return self.fail(
                at,
                if gateway {
                    "peer-legacy-nickname-differs"
                } else {
                    "peer-legacy-nickname-not-zero"
                },
            );
        }

        // Step 7: the box, then the container: type, padded data.
        let Some(plain) = open(&shared, &nonce, message_box) else {
            return self.fail(at, "peer-box-does-not-open");
        };
        let Some((&ty, padded)) = plain.split_first() else {
            return self.fail(at, "peer-container-empty");
        };
        let Some(body) = unpad(padded) else {
            return self.fail(at, "peer-padding");
        };
        // [I] "should be at least 32 bytes": the oracle is not more forgiving.
        if padded.len() < PADDED_FLOOR {
            return self.fail(at, "peer-padded-data-under-32");
        }
        // Steps 8 and 9.
        if ty == 0xff {
            return self.fail(at, "peer-type-0xff");
        }
        if ty == 0 || common::CspE2eMessageType::try_from(i32::from(ty)).is_err() {
            return self.fail(at, "peer-type-unknown");
        }

        // Step 14: an id received before from this sender, under another nonce.
        let discard = if self.seen_ids.insert((sender_text.clone(), message_id)) {
            None
        } else {
            Some(Discard::Id)
        };
        let opened = Opened {
            sender: sender_text,
            message_id,
            created_at_s: created_s,
            created_at_ms: metadata.created_at,
            flags,
            ty,
            body: body.to_vec(),
            nickname: metadata.nickname,
            nonce,
            discard,
            bytes: bytes.clone(),
            conn: sent.conn,
        };
        self.used_nonces
            .insert(nonce, (bytes.clone(), self.opened.len()));
        self.opened.push(opened.clone());
        Some(opened)
    }
}

/// The message metadata key of the spec (`message-with-metadata-box.metadata-container`):
/// `MMK = BLAKE2b(key=S, salt='mm', personal='3ma-csp')` with `S = X25519HSalsa20(<sender.CK>.secret,
/// <receiver.CK>.public)`, a 32-byte output over no input.
pub fn metadata_key(shared: &[u8; 32]) -> [u8; 32] {
    blake2b_mac(shared, b"mm", b"3ma-csp", &[])
}

/// `data` of PKCS#7-padded `padded` (`container.padded-data`): the last byte is the pad length
/// (1 to 255), and that many trailing bytes all equal it. `None` when it is no such padding.
pub fn unpad(padded: &[u8]) -> Option<&[u8]> {
    let pad = usize::from(*padded.last()?);
    if pad == 0 || pad > padded.len() {
        return None;
    }
    let (data, tail) = padded.split_at(padded.len() - pad);
    tail.iter().all(|b| usize::from(*b) == pad).then_some(data)
}

/// The legacy nickname field of the head (`legacy-sender-nickname`): "When sending a message towards
/// a Threema Gateway ID (starts with a `*`), add the same nickname as included in the encrypted
/// metadata box. Otherwise, set it to all zeroes." Zero-padded to 32 bytes; a longer nickname is
/// cut, and [I] the cut is on a character (the spec advises it) so the field stays valid UTF-8.
fn legacy_nickname_ok(
    gateway: bool,
    legacy: &[u8; LEGACY_NICKNAME],
    nickname: Option<&str>,
) -> bool {
    if !gateway {
        return legacy.iter().all(|b| *b == 0);
    }
    let used = legacy
        .iter()
        .rposition(|b| *b != 0)
        .map_or(0, |last| last + 1);
    let (text, zeros) = legacy.split_at(used);
    if zeros.iter().any(|b| *b != 0) {
        return false;
    }
    let want = nickname.unwrap_or("").as_bytes();
    if want.len() <= LEGACY_NICKNAME {
        return text == want;
    }
    // [I] cut on a character: the longest prefix of at most 32 bytes that ends between two characters.
    let Some(nickname) = nickname else {
        return false;
    };
    let mut end = LEGACY_NICKNAME;
    while !nickname.is_char_boundary(end) {
        end -= 1;
    }
    text == &nickname.as_bytes()[..end]
}

//! The PEER of the receive tests (W3a chunk C8): a fictitious contact that encrypts messages to the
//! test account the way a real WhatsApp client does, so the REAL client under test decrypts them,
//! hands them to Kourier's durability hook and sends its delivery receipt.
//!
//! It is a plain libsignal initiator (the session and identity stores are two `HashMap`s, after the
//! stores of upstream's own tests, MIT, `wacore/libsignal/tests/session_divergence.rs`): it reads
//! the account's published keys (identity key, signed prekey and its signature, registration id,
//! and when a test asks for one a one-time prekey), runs `process_prekey_bundle`, and encrypts
//! `wa::Message::text(..)` padded the way WhatsApp pads it. Every message of a session that the
//! account has not answered is a `pkmsg`, so each stanza decrypts from the account's keys alone.
//!
//! W5a (chunk 2b) gives it a DECRYPT half, so a send test reads what the real client wrote: as the
//! RESPONDER ([`Peer::responder`]: the peer publishes a bundle of its own, signed prekey and one-time
//! prekey, that the scripted server hands out through [`super::wire::prekeys`]; the client's first
//! frame to it is a `pkmsg` made from that bundle) and as the INITIATOR (a peer of [`Peer::new`] that
//! has written first: the client answers on the session the peer's `pkmsg` made, a `msg`).
//! [`Peer::read`] takes a `<message>` frame of [`super::wire::Wire::frame_nodes`], finds the `<enc>`
//! addressed to this device in its `<participants>`, decrypts and unpads it as a receiving phone does.
//!
//! Everything here is SYNTHETIC: the account device is made from fresh random keys, the numbers
//! are in the 555 block, and nothing leaves the process. The stanzas are returned as nodes for the
//! scripted server ([`super::wire::Wire::send`]) to push; a kill test can also keep them, because
//! a stanza is plain data that a second process injects again (the server's redelivery).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use wacore::libsignal::protocol::{
    CiphertextMessage, Direction, GenericSignedPreKey, IdentityChange, IdentityKey,
    IdentityKeyPair, IdentityKeyStore, KeyPair, PreKeyBundle, PreKeyId, PreKeyRecord,
    PreKeySignalMessage, PreKeyStore, ProtocolAddress, SenderKeyDistributionMessage, SenderKeyName,
    SenderKeyRecord, SenderKeyStore, SessionRecord, SessionStore, SignalMessage,
    SignalProtocolError, SignedPreKeyId, SignedPreKeyRecord, SignedPreKeyStore, Timestamp,
    UsePQRatchet, create_sender_key_distribution_message, group_decrypt, group_encrypt,
    message_decrypt, message_encrypt, process_prekey_bundle,
    process_sender_key_distribution_message,
};
use wacore::messages::{MessageUtils, decode_plaintext};
use wacore::proto_helpers::MessageBuilderExt;
use wacore::store::Device;
use wacore_binary::builder::NodeBuilder;
use wacore_binary::{Jid, Node};
use waproto::whatsapp as wa;

use super::wire::Bundle;

/// The synthetic LID of the test account (a 15-digit number, as WhatsApp's are).
pub const ACCOUNT_LID: &str = "100000000000001";
/// The synthetic phone number of the test account (the reserved 555-01XX block).
pub const ACCOUNT_PN: &str = "15550000001";
/// The peer's synthetic LID.
pub const PEER_LID: &str = "333333333333333";

/// A device that a finished pairing leaves: fresh random keys with a REAL signed-prekey signature
/// (the peer verifies it), the synthetic numbers above, and an advertisement identity of zeros
/// (what upstream's own fixture holds; nothing in the receive path checks it).
pub fn account_device() -> Device {
    let mut device = Device::new();
    device.pn = Some(
        format!("{ACCOUNT_PN}:1@s.whatsapp.net")
            .parse::<Jid>()
            .expect("the account's pn jid"),
    );
    device.lid = Some(
        format!("{ACCOUNT_LID}:1@lid")
            .parse::<Jid>()
            .expect("the account's lid jid"),
    );
    device.push_name = "Synthetic account".to_owned();
    let mut identity = wa::ADVSignedDeviceIdentity::default();
    identity.details = Some(vec![0; 32]);
    identity.account_signature_key = Some(vec![0; 32]);
    identity.account_signature = Some(vec![0; 64]);
    identity.device_signature = Some(vec![0; 64]);
    device.account = Some(Arc::new(identity));
    device
}

/// A fictitious contact with one device, encrypting to one account device.
pub struct Peer {
    jid: Jid,
    /// How the peer addresses the account's device (a key of its own stores; the account's side
    /// derives its own address for the peer from the stanza's `from`).
    to: ProtocolAddress,
    identities: Identities,
    sessions: Sessions,
    /// The peer's own one-time and signed prekeys (empty for a peer of [`Peer::new`], which never
    /// published any): what a `pkmsg` to the peer is made from.
    prekeys: PreKeys,
    signed: SignedPreKeys,
    /// The bundle the peer published, for [`super::wire::prekeys`] (a responder only).
    published: Option<Bundle>,
    counter: u64,
}

impl Peer {
    /// A peer that has fetched `account`'s keys: `with_one_time` also carries a one-time prekey
    /// (id and public key) in the bundle it uses, so the first message names it and the account
    /// removes it on use (the prekey-removal write of the kill loop).
    pub async fn new(account: &Device, with_one_time: Option<(u32, KeyPair)>) -> Self {
        let jid = format!("{PEER_LID}:0@lid").parse().expect("the peer's jid");
        Self::with_jid(account, with_one_time, jid).await
    }

    /// The account's OWN PHONE (W4 C1, the history probe): device 0 of the account's own user, as
    /// `<ACCOUNT_LID>:0@lid` (`by_number` false) or `<ACCOUNT_PN>:0@s.whatsapp.net` (true). Upstream
    /// honours a history-sync notification only from the account itself (`is_from_me`), so the probe
    /// and W4's Wire tests send it from here. Fresh keys like any peer: nothing in the receive path
    /// checks the primary device's identity against the account's advertisement.
    pub async fn own_phone(account: &Device, by_number: bool) -> Self {
        let jid = if by_number {
            format!("{ACCOUNT_PN}:0@s.whatsapp.net")
        } else {
            format!("{ACCOUNT_LID}:0@lid")
        };
        Self::with_jid(account, None, jid.parse().expect("the phone's jid")).await
    }

    /// As [`Peer::new`], for the device `jid`.
    async fn with_jid(account: &Device, with_one_time: Option<(u32, KeyPair)>, jid: Jid) -> Self {
        let mut rng = rand::make_rng::<rand::rngs::StdRng>();
        let pair = IdentityKeyPair::generate(&mut rng);
        let peer = Self {
            jid,
            to: ProtocolAddress::new(ACCOUNT_LID, 1u32.into()),
            identities: Identities {
                pair,
                registration_id: 4_242,
                known: HashMap::new(),
            },
            sessions: Sessions::default(),
            prekeys: PreKeys::default(),
            signed: SignedPreKeys::default(),
            published: None,
            counter: 0,
        };
        let bundle = PreKeyBundle::new(
            account.registration_id,
            1u32.into(),
            with_one_time.map(|(id, pair)| (id.into(), pair.public_key)),
            account.signed_pre_key_id.into(),
            account.signed_pre_key.public_key,
            account.signed_pre_key_signature.to_vec(),
            IdentityKey::new(account.identity_key.public_key),
        )
        .expect("the account's bundle");
        let mut peer = peer;
        process_prekey_bundle(
            &peer.to,
            &mut peer.sessions,
            &mut peer.identities,
            &bundle,
            &mut rng,
            UsePQRatchet::No,
        )
        .await
        .expect("the peer's session with the account");
        peer
    }

    /// A contact the ACCOUNT writes to first (W5a 2b): no session yet, a fresh identity, and a published
    /// bundle ([`Peer::bundle`]: registration id, identity, signed prekey `signed_id` with a REAL signature
    /// that upstream verifies, one-time prekey `one_time_id`) for the server's `encrypt` rule. Device 0 of
    /// [`PEER_LID`]. The account's device is [`ACCOUNT_LID`]`:1`, the sender of every frame it writes.
    pub async fn responder(signed_id: u32, one_time_id: u32) -> Self {
        let jid = format!("{PEER_LID}:0@lid").parse().expect("the peer's jid");
        Self::responder_as(jid, signed_id, one_time_id)
    }

    /// The account's OWN PHONE as a responder (W5a 2c): device 0 of the account's own LID user, with a
    /// published bundle, so the client's copy of a message it sends (a `DeviceSentMessage` to its other
    /// own devices) is a `pkmsg` this peer reads with [`Peer::read`]. The client writes that copy only
    /// when the `<success>` names the account's DEVICE LID (`wire::success_as_the_server_writes_it`) and
    /// the `usync` rule lists device 0 for the account's number.
    pub async fn own_phone_responder(signed_id: u32, one_time_id: u32) -> Self {
        let jid = format!("{ACCOUNT_LID}:0@lid")
            .parse()
            .expect("the phone's jid");
        Self::responder_as(jid, signed_id, one_time_id)
    }

    /// A responder for the device `jid`.
    fn responder_as(jid: Jid, signed_id: u32, one_time_id: u32) -> Self {
        let mut rng = rand::make_rng::<rand::rngs::StdRng>();
        let pair = IdentityKeyPair::generate(&mut rng);
        let signed_pair = KeyPair::generate(&mut rng);
        let signature = pair
            .private_key()
            .calculate_signature(&signed_pair.public_key.serialize(), &mut rng)
            .expect("the peer signs its signed prekey");
        let one_time_pair = KeyPair::generate(&mut rng);
        let raw = |key: &KeyPair| -> [u8; 32] {
            key.public_key
                .public_key_bytes()
                .try_into()
                .expect("a 32-byte public key")
        };
        let bundle = Bundle {
            registration_id: 4_343,
            identity: pair
                .identity_key()
                .public_key()
                .public_key_bytes()
                .try_into()
                .expect("a 32-byte identity key"),
            signed_id,
            signed: raw(&signed_pair),
            signed_signature: signature,
            one_time: Some((one_time_id, raw(&one_time_pair))),
        };
        let mut prekeys = PreKeys::default();
        prekeys.0.insert(
            one_time_id.into(),
            PreKeyRecord::new(one_time_id.into(), &one_time_pair),
        );
        let mut signed = SignedPreKeys::default();
        signed.0.insert(
            signed_id.into(),
            SignedPreKeyRecord::new(
                signed_id.into(),
                Timestamp::from_epoch_millis(0),
                &signed_pair,
                &signature,
            ),
        );
        Self {
            jid,
            to: ProtocolAddress::new(ACCOUNT_LID, 1u32.into()),
            identities: Identities {
                pair,
                registration_id: bundle.registration_id,
                known: HashMap::new(),
            },
            sessions: Sessions::default(),
            prekeys,
            signed,
            published: Some(bundle),
            counter: 0,
        }
    }

    /// The bundle a [`Peer::responder`] published, for [`super::wire::prekeys`] under [`Peer::jid`].
    pub fn bundle(&self) -> Bundle {
        self.published
            .clone()
            .expect("only a responder publishes a bundle")
    }

    /// Reads a `<message>` frame the account's client wrote (W5a 2b): the `<enc>` of the `<to>` that names
    /// this device (compared as a JID: upstream writes device 0 without its `:0`), decrypted as the
    /// account's device's message (`pkmsg` from this peer's published keys, `msg` on a session either
    /// side began) and unpadded by the `<enc>`'s `v`. An error is a sentence for the test's failure.
    pub async fn read(&mut self, frame: &Node) -> Result<wa::Message, String> {
        let enc = frame
            .get_optional_child("participants")
            .into_iter()
            .flat_map(|list| list.get_children_by_tag("to"))
            .find(|to| {
                to.attrs
                    .get("jid")
                    .and_then(|v| v.as_str().parse::<Jid>().ok())
                    .as_ref()
                    == Some(&self.jid)
            })
            .ok_or("the frame has no <to> for this peer's device")?
            .get_optional_child("enc")
            .ok_or("the peer's <to> has no <enc>")?;
        let attr = |key: &str| enc.attrs.get(key).map(|v| v.as_str().into_owned());
        let version: u8 = attr("v")
            .ok_or("the <enc> has no v")?
            .parse()
            .map_err(|_| "the <enc>'s v is not a number")?;
        let bytes = match &enc.content {
            Some(wacore_binary::NodeContent::Bytes(bytes)) => bytes.clone(),
            _ => return Err("the <enc> carries no bytes".to_owned()),
        };
        let ciphertext = match attr("type").as_deref() {
            Some("pkmsg") => CiphertextMessage::PreKeySignalMessage(
                PreKeySignalMessage::try_from(bytes.as_slice())
                    .map_err(|_| "the pkmsg does not parse")?,
            ),
            Some("msg") => CiphertextMessage::SignalMessage(
                SignalMessage::try_from(bytes.as_slice()).map_err(|_| "the msg does not parse")?,
            ),
            _ => return Err("the <enc> is neither a pkmsg nor a msg".to_owned()),
        };
        let mut rng = rand::make_rng::<rand::rngs::StdRng>();
        let plain = message_decrypt(
            &ciphertext,
            &self.to,
            &mut self.sessions,
            &mut self.identities,
            &mut self.prekeys,
            &self.signed,
            &mut rng,
            UsePQRatchet::No,
        )
        .await
        .map_err(|e| format!("the peer cannot decrypt the frame: {e}"))?;
        decode_plaintext(&plain.plaintext, version)
            .map_err(|_| "the decrypted bytes are no padded wa::Message".to_owned())
    }

    /// Reads a GROUP `<message>` frame the account's client wrote (W5b, decision G22): the sender key from the
    /// `<enc>` of this device's `<to>` ([`Peer::read`]), then the frame's `skmsg` decrypted under it and unpadded
    /// by its `v`, as a member's phone reads a group message whose sender key came with it. An error is a sentence
    /// for the test's failure.
    pub async fn read_group(&mut self, frame: &Node) -> Result<wa::Message, String> {
        let first = self.read(frame).await?;
        let distribution = first
            .sender_key_distribution_message
            .as_option()
            .ok_or("the peer's <to> carries no sender key")?;
        let group = distribution
            .group_id
            .clone()
            .ok_or("the sender key names no group")?;
        let skdm = SenderKeyDistributionMessage::try_from(
            distribution
                .axolotl_sender_key_distribution_message
                .as_deref()
                .ok_or("the sender key has no body")?,
        )
        .map_err(|_| "the sender key does not parse")?;
        // Any sender name serves: the peer keeps this one sender key for this one frame.
        let name = SenderKeyName::from_parts(&group, "account");
        let mut keys = SenderKeys::default();
        process_sender_key_distribution_message(&name, &skdm, &mut keys)
            .await
            .map_err(|e| format!("the peer cannot take the sender key: {e}"))?;
        let enc = frame
            .get_children_by_tag("enc")
            .find(|enc| {
                enc.attrs
                    .get("type")
                    .map(|v| v.as_str().into_owned())
                    .as_deref()
                    == Some("skmsg")
            })
            .ok_or("the frame has no skmsg")?;
        let version: u8 = enc
            .attrs
            .get("v")
            .ok_or("the skmsg has no v")?
            .as_str()
            .parse()
            .map_err(|_| "the skmsg's v is not a number")?;
        let bytes = match &enc.content {
            Some(wacore_binary::NodeContent::Bytes(bytes)) => bytes.clone(),
            _ => return Err("the skmsg carries no bytes".to_owned()),
        };
        let plain = group_decrypt(&bytes, &mut keys, &name)
            .await
            .map_err(|e| format!("the peer cannot decrypt the skmsg: {e}"))?;
        decode_plaintext(&plain, version)
            .map_err(|_| "the skmsg's bytes are no padded wa::Message".to_owned())
    }

    /// The peer's JID as it appears in a stanza's `from`.
    pub fn jid(&self) -> &Jid {
        &self.jid
    }

    /// The next message id of this peer (WhatsApp's look: `3EB0` and hex digits).
    fn next_id(&mut self) -> String {
        self.counter += 1;
        format!("3EB0{:012X}", self.counter)
    }

    /// A text message as the `<message>` stanza the server would deliver (live, not offline).
    pub async fn text(&mut self, text: &str) -> Node {
        let id = self.next_id();
        self.text_with_id(text, &id).await
    }

    /// As [`Peer::text`] with the id chosen (a redelivery names the same id again).
    pub async fn text_with_id(&mut self, text: &str, id: &str) -> Node {
        self.message_with_id(&wa::Message::text(text), id, "text")
            .await
    }

    /// Any `wa::Message` as the `<message>` stanza the server would deliver, under the id chosen and
    /// with the stanza's `type` (`text`, `reaction`, `media`: the scripted client does not look at
    /// it, a real one routes by it). W3b (C9a): the content tests send formatted texts, reactions,
    /// edits, revokes and pictures through the real client.
    pub async fn message_with_id(&mut self, message: &wa::Message, id: &str, kind: &str) -> Node {
        let plain = MessageUtils::encode_and_pad(message);
        let encrypted = message_encrypt(&plain, &self.to, &mut self.sessions, &mut self.identities)
            .await
            .expect("the peer encrypts");
        let enc_kind = match &encrypted {
            CiphertextMessage::PreKeySignalMessage(_) => "pkmsg",
            _ => "msg",
        };
        let enc = NodeBuilder::new("enc")
            .attr("v", "2")
            .attr("type", enc_kind)
            .bytes(encrypted.serialize().to_vec())
            .build();
        NodeBuilder::new("message")
            .attr("from", self.jid.to_string())
            .attr("id", id)
            .attr("t", "1700000100")
            .attr("type", kind)
            .attr("notify", "Synthetic peer")
            .children([enc])
            .build()
    }

    /// A GROUP message of this peer as the `<message>` stanza the server delivers to a member (W6a2 B11): the
    /// sender key for `group` comes in a `pkmsg`/`msg` of the peer's 1:1 session (a `Message` holding only the
    /// distribution, as a phone sends it), then the message itself as an `skmsg` under that key, and the stanza names
    /// the group in `from` and the peer in `participant` (a LID group: `addressing_mode="lid"`). A fresh sender key
    /// for every call, which a receiving client takes up like any new key of a member.
    pub async fn group_message_with_id(
        &mut self,
        group: &Jid,
        message: &wa::Message,
        id: &str,
        kind: &str,
    ) -> Node {
        let name = SenderKeyName::from_parts(&group.to_string(), "peer");
        let mut keys = SenderKeys::default();
        let mut rng = rand::make_rng::<rand::rngs::StdRng>();
        let skdm = create_sender_key_distribution_message(&name, &mut keys, &mut rng)
            .await
            .expect("the peer makes a sender key");
        let wrapper = MessageUtils::encode_and_pad_skdm_wrapper(group, skdm.serialized());
        let encrypted =
            message_encrypt(&wrapper, &self.to, &mut self.sessions, &mut self.identities)
                .await
                .expect("the peer encrypts the sender key");
        let enc_kind = match &encrypted {
            CiphertextMessage::PreKeySignalMessage(_) => "pkmsg",
            _ => "msg",
        };
        let key_enc = NodeBuilder::new("enc")
            .attr("v", "2")
            .attr("type", enc_kind)
            .bytes(encrypted.serialize().to_vec())
            .build();
        let plain = MessageUtils::encode_and_pad(message);
        let sealed = group_encrypt(&mut keys, &name, &plain, &mut rng)
            .await
            .expect("the peer encrypts for the group");
        let group_enc = NodeBuilder::new("enc")
            .attr("v", "2")
            .attr("type", "skmsg")
            .bytes(sealed.serialized().to_vec())
            .build();
        NodeBuilder::new("message")
            .attr("from", group.to_string())
            .attr("participant", self.jid.to_string())
            .attr("addressing_mode", "lid")
            .attr("id", id)
            .attr("t", "1700000100")
            .attr("type", kind)
            .attr("notify", "Synthetic peer")
            .children([key_enc, group_enc])
            .build()
    }

    /// As [`Peer::text`], marked as part of the offline delivery (`offline="1"`): a message the
    /// server held while the device was away.
    pub async fn offline_text(&mut self, text: &str) -> Node {
        let mut node = self.text(text).await;
        node.attrs.insert("offline", "1");
        node
    }
}

/// The peer's identity store.
struct Identities {
    pair: IdentityKeyPair,
    registration_id: u32,
    known: HashMap<ProtocolAddress, IdentityKey>,
}

#[async_trait]
impl IdentityKeyStore for Identities {
    async fn get_identity_key_pair(
        &self,
    ) -> wacore::libsignal::protocol::error::Result<IdentityKeyPair> {
        Ok(self.pair.clone())
    }

    async fn get_local_registration_id(&self) -> wacore::libsignal::protocol::error::Result<u32> {
        Ok(self.registration_id)
    }

    async fn save_identity(
        &mut self,
        address: &ProtocolAddress,
        identity: &IdentityKey,
    ) -> wacore::libsignal::protocol::error::Result<IdentityChange> {
        let changed = self
            .known
            .get(address)
            .is_some_and(|previous| previous != identity);
        self.known.insert(address.clone(), *identity);
        Ok(IdentityChange::from_changed(changed))
    }

    async fn is_trusted_identity(
        &self,
        _: &ProtocolAddress,
        _: &IdentityKey,
        _: Direction,
    ) -> wacore::libsignal::protocol::error::Result<bool> {
        Ok(true)
    }

    async fn get_identity(
        &self,
        address: &ProtocolAddress,
    ) -> wacore::libsignal::protocol::error::Result<Option<IdentityKey>> {
        Ok(self.known.get(address).cloned())
    }
}

/// The peer's session store.
#[derive(Default)]
struct Sessions(HashMap<ProtocolAddress, SessionRecord>);

#[async_trait]
impl SessionStore for Sessions {
    async fn load_session(
        &self,
        address: &ProtocolAddress,
    ) -> wacore::libsignal::protocol::error::Result<Option<SessionRecord>> {
        Ok(self.0.get(address).cloned())
    }

    async fn has_session(
        &self,
        address: &ProtocolAddress,
    ) -> wacore::libsignal::protocol::error::Result<bool> {
        Ok(self.0.contains_key(address))
    }

    async fn store_session(
        &mut self,
        address: &ProtocolAddress,
        record: SessionRecord,
    ) -> wacore::libsignal::protocol::error::Result<()> {
        self.0.insert(address.clone(), record);
        Ok(())
    }
}

/// The sender keys of one group frame ([`Peer::read_group`]).
#[derive(Default)]
struct SenderKeys(HashMap<String, SenderKeyRecord>);

#[async_trait]
impl SenderKeyStore for SenderKeys {
    async fn store_sender_key(
        &mut self,
        name: &SenderKeyName,
        record: SenderKeyRecord,
    ) -> wacore::libsignal::protocol::error::Result<()> {
        self.0.insert(name.cache_key().to_owned(), record);
        Ok(())
    }

    async fn load_sender_key(
        &self,
        name: &SenderKeyName,
    ) -> wacore::libsignal::protocol::error::Result<Option<SenderKeyRecord>> {
        Ok(self.0.get(name.cache_key()).cloned())
    }
}

/// The peer's one-time prekeys (a responder's published one; a `pkmsg` names it).
#[derive(Default)]
struct PreKeys(HashMap<PreKeyId, PreKeyRecord>);

#[async_trait]
impl PreKeyStore for PreKeys {
    async fn get_pre_key(
        &self,
        id: PreKeyId,
    ) -> wacore::libsignal::protocol::error::Result<PreKeyRecord> {
        self.0
            .get(&id)
            .cloned()
            .ok_or(SignalProtocolError::InvalidPreKeyId)
    }

    async fn save_pre_key(
        &mut self,
        id: PreKeyId,
        record: &PreKeyRecord,
    ) -> wacore::libsignal::protocol::error::Result<()> {
        self.0.insert(id, record.clone());
        Ok(())
    }

    async fn remove_pre_key(
        &mut self,
        id: PreKeyId,
    ) -> wacore::libsignal::protocol::error::Result<()> {
        self.0.remove(&id);
        Ok(())
    }
}

/// The peer's signed prekeys.
#[derive(Default)]
struct SignedPreKeys(HashMap<SignedPreKeyId, SignedPreKeyRecord>);

#[async_trait]
impl SignedPreKeyStore for SignedPreKeys {
    async fn get_signed_pre_key(
        &self,
        id: SignedPreKeyId,
    ) -> wacore::libsignal::protocol::error::Result<SignedPreKeyRecord> {
        self.0
            .get(&id)
            .cloned()
            .ok_or(SignalProtocolError::InvalidSignedPreKeyId)
    }

    async fn save_signed_pre_key(
        &mut self,
        id: SignedPreKeyId,
        record: &SignedPreKeyRecord,
    ) -> wacore::libsignal::protocol::error::Result<()> {
        self.0.insert(id, record.clone());
        Ok(())
    }
}

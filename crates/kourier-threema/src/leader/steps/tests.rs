//! The leader's receive steps, one test per row of the brief's §2.3 (and per case the critic added: T-B1, T-M1,
//! T-M3, T-L3, T-L4): a payload sealed here with libthreema's P5 seals under the sender's key, decided over a fixture
//! view, the verdict and its rule name compared literally. Synthetic IDs and keys only.

use std::collections::{BTreeMap, HashSet};

use libthreema::common::keys::ClientKey;
use libthreema::common::{MessageId, Nonce};
use libthreema::protobuf::common::{self, CspE2eMessageType, DeltaImage, Identities};
use libthreema::protobuf::d2d_sync;

use super::*;
use crate::send::encode;

const OWN: &str = "A6OWN001";
const ANN: &str = "A6ANN002";
const BOB: &str = "A6BOB003";
const NEW: &str = "A6NEW004";
const NOW: u64 = 1_700_000_000_000;
const GID: u64 = 0x0102_0304_0506_0708;

fn ck(id: &str) -> ClientKey {
    let seed = id
        .bytes()
        .fold(7_u8, |h, b| h.wrapping_mul(31).wrapping_add(b));
    ClientKey::from([seed; 32])
}

fn own_ck() -> ClientKey {
    ClientKey::from([0x22; 32])
}

/// The public key of `id`'s client key (libthreema prints it as hex; it has no byte accessor).
fn public(id: &str) -> [u8; 32] {
    let hex = ck(id).public_key().to_string();
    let mut out = [0_u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("hex");
    }
    out
}

/// A contact of `id` at `level` with its real public key.
fn contact(id: &str, level: d2d_sync::contact::AcquaintanceLevel) -> d2d_sync::Contact {
    d2d_sync::Contact {
        identity: id.to_owned(),
        public_key: Some(public(id).to_vec()),
        acquaintance_level: Some(level as i32),
        ..Default::default()
    }
}

fn direct(id: &str) -> d2d_sync::Contact {
    contact(id, d2d_sync::contact::AcquaintanceLevel::Direct)
}

fn key(creator: &str) -> GroupRef {
    GroupRef {
        creator: creator.to_owned(),
        group_id: GID,
    }
}

fn group(creator: &str, members: &[&str], name: &str) -> d2d_sync::Group {
    d2d_sync::Group {
        group_identity: Some(common::GroupIdentity {
            group_id: GID,
            creator_identity: creator.to_owned(),
        }),
        name: Some(name.to_owned()),
        user_state: Some(d2d_sync::group::UserState::Member as i32),
        member_identities: Some(Identities {
            identities: members.iter().map(|m| (*m).to_owned()).collect(),
        }),
        ..Default::default()
    }
}

fn removed() -> Option<DeltaImage> {
    Some(DeltaImage {
        image: Some(common::delta_image::Image::Removed(common::Unit {})),
    })
}

#[derive(Default)]
struct Fixture {
    contacts: BTreeMap<String, d2d_sync::Contact>,
    settings: Option<d2d_sync::Settings>,
    groups: BTreeMap<(String, u64), d2d_sync::Group>,
    nonces: HashSet<[u8; 24]>,
    cache: BTreeMap<String, ContactInit>,
    held: HashSet<GroupRef>,
    /// S-B1: a blind hold stands on the socket.
    blind: bool,
    sync_throttled: HashSet<GroupRef>,
    answer_throttled: HashSet<(u64, String)>,
}

impl Fixture {
    fn with(mut self, c: d2d_sync::Contact) -> Self {
        self.contacts.insert(c.identity.clone(), c);
        self
    }
    fn with_group(mut self, g: d2d_sync::Group) -> Self {
        let id = g.group_identity.clone().expect("an identity");
        self.groups.insert((id.creator_identity, id.group_id), g);
        self
    }
    fn blocking(mut self, blocked: &[&str], block_unknown: bool) -> Self {
        use d2d_sync::settings::UnknownContactPolicy;
        self.settings = Some(d2d_sync::Settings {
            unknown_contact_policy: Some(if block_unknown {
                UnknownContactPolicy::BlockUnknown as i32
            } else {
                UnknownContactPolicy::AllowUnknown as i32
            }),
            blocked_identities: Some(Identities {
                identities: blocked.iter().map(|b| (*b).to_owned()).collect(),
            }),
            ..Default::default()
        });
        self
    }
    fn cache(mut self, id: &str) -> Self {
        self.cache.insert(id.to_owned(), init(id));
        self
    }
}

fn init(id: &str) -> ContactInit {
    ContactInit {
        identity: id.to_owned(),
        public_key: public(id),
        work: false,
        inactive: false,
        feature_mask: 0,
    }
}

impl receivers::View for Fixture {
    fn contact(&self, identity: &str) -> Result<Option<d2d_sync::Contact>> {
        Ok(self.contacts.get(identity).cloned())
    }
    fn settings(&self) -> Result<Option<d2d_sync::Settings>> {
        Ok(self.settings.clone())
    }
    fn identities_in_groups(&self) -> Result<std::collections::HashSet<String>> {
        let mut out = std::collections::HashSet::new();
        for ((creator, _), g) in &self.groups {
            if is_member(g) {
                out.insert(creator.clone());
                out.extend(
                    g.member_identities
                        .iter()
                        .flat_map(|m| m.identities.clone()),
                );
            }
        }
        Ok(out)
    }
}

impl View for Fixture {
    fn own(&self) -> &str {
        OWN
    }
    fn now_ms(&self) -> u64 {
        NOW
    }
    fn csp_nonce_used(&self, nonce: &Nonce) -> Result<bool> {
        Ok(self.nonces.contains(&nonce.0))
    }
    fn group(&self, creator: &str, group_id: u64) -> Result<Option<d2d_sync::Group>> {
        Ok(self.groups.get(&(creator.to_owned(), group_id)).cloned())
    }
    fn cached(&self, identity: &str) -> Option<ContactInit> {
        self.cache.get(identity).cloned()
    }
    fn held_group(&self, group: &GroupRef) -> bool {
        self.blind || self.held.contains(group)
    }
    fn blind(&self) -> bool {
        self.blind
    }
    fn sync_request_throttled(&self, group: &GroupRef) -> bool {
        self.sync_throttled.contains(group)
    }
    fn answer_throttled(&self, group_id: u64, sender: &str) -> bool {
        self.answer_throttled
            .contains(&(group_id, sender.to_owned()))
    }
}

/// One payload as a peer seals it (the spec's layout, libthreema's P5 seals): every field can be bent.
struct Msg {
    sender: String,
    sealer: ClientKey,
    receiver: String,
    id: u64,
    ty: u8,
    data: Vec<u8>,
    meta: Option<(u64, u64, Option<String>)>,
    flags: u8,
    created_s: u32,
    legacy: [u8; 32],
    nonce: [u8; 24],
}

impl Msg {
    fn new(sender: &str, ty: u8, data: &[u8]) -> Self {
        Self {
            sender: sender.to_owned(),
            sealer: ck(sender),
            receiver: OWN.to_owned(),
            id: 0x1001,
            ty,
            data: data.to_vec(),
            meta: Some((0x1001, NOW - 5_000, None)),
            flags: 0x01,
            created_s: u32::try_from((NOW - 5_000) / 1000).expect("seconds"),
            legacy: [0; 32],
            nonce: [0x5a; 24],
        }
    }
    fn text(sender: &str) -> Self {
        Self::new(sender, 0x01, b"hello")
    }
    fn group_text(sender: &str, creator: &str) -> Self {
        Self::new(
            sender,
            0x41,
            &crate::send::body::group_member_container(
                libthreema::common::ThreemaId::try_from(creator).expect("an ID"),
                GID,
                b"hi group",
            ),
        )
    }
    fn creator(sender: &str, ty: u8, inner: &[u8]) -> Self {
        Self::new(
            sender,
            ty,
            &crate::send::body::group_creator_container(GID, inner),
        )
    }
    fn member(sender: &str, creator: &str, ty: u8, inner: &[u8]) -> Self {
        Self::new(
            sender,
            ty,
            &crate::send::body::group_member_container(
                libthreema::common::ThreemaId::try_from(creator).expect("an ID"),
                GID,
                inner,
            ),
        )
    }
    fn nickname(mut self, nickname: &str) -> Self {
        if let Some(meta) = &mut self.meta {
            meta.2 = Some(nickname.to_owned());
        }
        self
    }
    fn bytes(&self) -> Vec<u8> {
        let own = own_ck().public_key();
        let key = self.sealer.derive_csp_e2e_key(&own).expect("contributory");
        let nonce = Nonce(self.nonce);
        let mut container = vec![self.ty];
        container.extend_from_slice(&self.data);
        let pad = if container.len() - 1 < 31 {
            32 - (container.len() - 1)
        } else {
            3
        };
        container.resize(container.len() + pad, u8::try_from(pad).expect("a pad"));
        let message = key.encrypt_message(&nonce, container).expect("sealed");
        let meta = self.meta.as_ref().map_or_else(Vec::new, |(id, at, nick)| {
            let plain = encode::metadata(MessageId(*id), *at, nick.as_deref());
            key.encrypt_metadata(&nonce, plain.to_vec())
                .expect("sealed")
        });
        let mut out = Vec::new();
        out.extend_from_slice(self.sender.as_bytes());
        out.extend_from_slice(self.receiver.as_bytes());
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.created_s.to_le_bytes());
        out.push(self.flags);
        out.push(0);
        out.extend_from_slice(&u16::try_from(meta.len()).expect("short").to_le_bytes());
        out.extend_from_slice(&self.legacy);
        out.extend_from_slice(&meta);
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&message);
        out
    }
}

fn decide_with(view: &Fixture, msg: &Msg, looked: Option<&Looked>, contact_done: bool) -> Verdict {
    let ck = own_ck();
    let bytes = msg.bytes();
    decide(
        view,
        &Input {
            bytes: &bytes,
            ck: &ck,
            looked,
            contact_done,
        },
    )
    .expect("the fixture never fails")
}

fn verdict(view: &Fixture, msg: &Msg) -> Verdict {
    decide_with(view, msg, None, false)
}

/// The discard's rule, ack, nonce and jobs; panics on any other verdict.
#[track_caller]
fn discarded(v: Verdict) -> (&'static str, bool, bool, Vec<Job>) {
    match v {
        Verdict::Discard(d) => (d.rule, d.ack, d.nonce, d.jobs),
        other => panic!("not a discard: {other:?}"),
    }
}

#[track_caller]
fn held(v: Verdict) -> (&'static str, Option<GroupRef>) {
    match v {
        Verdict::Hold(h) => (h.rule, h.group),
        other => panic!("not a hold: {other:?}"),
    }
}

#[track_caller]
fn kept(v: Verdict) -> Keep {
    match v {
        Verdict::Process(k) => k,
        other => panic!("not kept: {other:?}"),
    }
}

/// Every rule name is unique and of the log's form (`[a-z-]`, the count line's `held_by` keys).
#[test]
fn every_rule_name_is_unique_and_a_plain_word() {
    let set: HashSet<&str> = RULES.iter().copied().collect();
    assert_eq!(set.len(), RULES.len());
    for rule in RULES {
        assert!(
            !rule.is_empty()
                && rule
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'-' || b.is_ascii_digit()),
            "{rule}"
        );
    }
}

/// THE ROW TEXT: a text from a direct contact is kept: reflected, a receipt, acked, the nonce marked, the body the
/// unpadded data, the time the metadata's (T-L3); `0x80` takes the receipt away, `0x04` the ack.
#[test]
fn a_text_from_a_direct_contact_is_processed_with_its_receipt() {
    let view = Fixture::default().with(direct(ANN));
    let keep = kept(verdict(&view, &Msg::text(ANN)));
    assert_eq!(keep.outer_type, CspE2eMessageType::Text);
    assert_eq!(keep.body.as_slice(), b"hello");
    assert_eq!(keep.created_at_ms, NOW - 5_000);
    assert!(keep.reflect && keep.receipt && keep.ack && keep.nonce);
    assert!(keep.jobs.is_empty());
    let mut no_receipt = Msg::text(ANN);
    no_receipt.flags = 0x81;
    assert!(!kept(verdict(&view, &no_receipt)).receipt);
    let mut no_ack = Msg::text(ANN);
    no_ack.flags = 0x05;
    assert!(!kept(verdict(&view, &no_ack)).ack);
    assert!(
        !format!("{:?}", verdict(&view, &Msg::text(ANN))).contains(ANN),
        "no ID in Debug"
    );
}

/// T-L3: the metadata's time clamped to now; without metadata the head's seconds x 1000.
#[test]
fn the_reflected_time_is_the_metadatas_clamped_or_the_heads() {
    let view = Fixture::default().with(direct(ANN));
    let mut future = Msg::text(ANN);
    future.meta = Some((0x1001, NOW + 60_000, None));
    assert_eq!(kept(verdict(&view, &future)).created_at_ms, NOW);
    let mut legacy = Msg::text(ANN);
    legacy.meta = None;
    legacy.created_s = 1_600_000_000;
    assert_eq!(
        kept(verdict(&view, &legacy)).created_at_ms,
        1_600_000_000_000
    );
}

/// STEP 2: a used nonce is discarded and acked, no nonce written again.
#[test]
fn step_2_a_used_nonce_is_discarded() {
    let mut view = Fixture::default().with(direct(ANN));
    view.nonces.insert([0x5a; 24]);
    assert_eq!(
        discarded(verdict(&view, &Msg::text(ANN))),
        (RULE_NONCE_USED, true, false, vec![])
    );
}

/// STEP 3: another receiver.
#[test]
fn step_3_another_receiver_is_discarded() {
    let view = Fixture::default().with(direct(ANN));
    let mut msg = Msg::text(ANN);
    msg.receiver = BOB.to_owned();
    assert_eq!(discarded(verdict(&view, &msg)).0, RULE_RECEIVER);
}

/// STEPS 4-5: the user as sender; an unknown sender waits for the directory (LOOKUP), then is invalid or goes on with
/// its init; a cached one goes on at once; a special contact uses its builtin key.
#[test]
fn steps_4_and_5_the_valid_contacts_lookup() {
    let view = Fixture::default();
    let mut own = Msg::text(OWN);
    own.sealer = own_ck();
    assert_eq!(discarded(verdict(&view, &own)).0, RULE_SENDER_OWN);
    assert!(matches!(verdict(&view, &Msg::text(BOB)), Verdict::Lookup));
    assert_eq!(
        discarded(decide_with(
            &view,
            &Msg::text(BOB),
            Some(&Looked::Invalid),
            false
        ))
        .0,
        RULE_SENDER_INVALID
    );
    let looked = Looked::Valid(init(BOB));
    let Verdict::Contact(Change::Create {
        init: got,
        nickname,
    }) = decide_with(
        &view,
        &Msg::text(BOB).nickname(" Bob "),
        Some(&looked),
        false,
    )
    else {
        panic!("a new direct contact");
    };
    assert_eq!(got, init(BOB));
    assert_eq!(nickname.as_deref(), Some("Bob"), "trimmed");
    let cached = Fixture::default().cache(BOB);
    assert!(matches!(
        verdict(&cached, &Msg::text(BOB)),
        Verdict::Contact(Change::Create { .. })
    ));
    // A special contact: the builtin key, never the directory (a payload sealed with another key does not open).
    assert_eq!(
        discarded(verdict(&view, &Msg::text("*3MAPUSH"))).0,
        open::RULE_METADATA
    );
}

/// STEPS 6-8: a metadata box or a message box that does not open, no padding, `0xff`, a short payload; none marks a
/// nonce.
#[test]
fn steps_6_to_8_what_does_not_open_is_discarded_by_its_rule() {
    let view = Fixture::default().with(direct(ANN));
    let mut wrong_key = Msg::text(ANN);
    wrong_key.sealer = ck(BOB);
    assert_eq!(discarded(verdict(&view, &wrong_key)).0, open::RULE_METADATA);
    wrong_key.meta = None;
    assert_eq!(
        discarded(verdict(&view, &wrong_key)),
        (open::RULE_UNDECRYPTABLE, true, false, vec![])
    );
    let ff = Msg::new(ANN, 0xff, b"x");
    assert_eq!(
        discarded(verdict(&view, &ff)),
        (open::RULE_LEGACY_FF, true, false, vec![])
    );
    let ck = own_ck();
    let bytes = Msg::text(ANN).bytes();
    let short = decide(
        &view,
        &Input {
            bytes: &bytes[..70],
            ck: &ck,
            looked: None,
            contact_done: false,
        },
    )
    .expect("decides");
    assert_eq!(discarded(short).0, open::RULE_HEAD);
}

/// STEPS 9/16 (C2): a type the pinned enum lacks is HELD, never acked; the invalid type 0x00 is step 10's discard.
#[test]
fn step_9_an_unknown_type_is_held() {
    let view = Fixture::default().with(direct(ANN));
    assert_eq!(
        held(verdict(&view, &Msg::new(ANN, 0x7e, b"new"))),
        (RULE_UNKNOWN_TYPE, None)
    );
    assert_eq!(
        discarded(verdict(&view, &Msg::new(ANN, 0x00, b"x"))).0,
        RULE_BODY
    );
}

/// STEP 12 (C8): an FS envelope with `encapsulated` from a stored contact is REJECTED (the session id and the message
/// id for the job, acked); from a sender not stored it is only discarded; another content is ignored; both mark the
/// nonce.
#[test]
fn step_12_an_fs_envelope_is_rejected_or_ignored() {
    #[derive(Clone, PartialEq, prost::Message)]
    struct Envelope {
        #[prost(bytes = "vec", tag = "1")]
        session_id: Vec<u8>,
        #[prost(bytes = "vec", optional, tag = "2")]
        init: Option<Vec<u8>>,
        #[prost(bytes = "vec", optional, tag = "6")]
        encapsulated: Option<Vec<u8>>,
    }
    let encapsulated = Envelope {
        session_id: vec![9; 16],
        init: None,
        encapsulated: Some(vec![0x08, 0x01]),
    }
    .encode_to_vec();
    let view = Fixture::default().with(direct(ANN)).cache(BOB);
    let Verdict::RejectFs(reject) = verdict(&view, &Msg::new(ANN, 0xa0, &encapsulated)) else {
        panic!("rejected");
    };
    assert_eq!(reject.to, ANN);
    assert_eq!(reject.session_id, vec![9; 16]);
    assert_eq!(reject.message_id, MessageId(0x1001));
    assert_eq!(reject.group, None, "a 1:1 message names no group");
    assert!(reject.ack);
    assert_eq!(
        discarded(verdict(&view, &Msg::new(BOB, 0xa0, &encapsulated))),
        (RULE_FS_REJECTED, true, true, vec![])
    );
    // P-L4: an `Encapsulated` of a GROUP message (field 6, `GroupIdentity`) is told to the job.
    #[derive(Clone, PartialEq, prost::Message)]
    struct Encapsulated {
        #[prost(uint64, tag = "2")]
        counter: u64,
        #[prost(message, optional, tag = "6")]
        group_identity: Option<common::GroupIdentity>,
    }
    let in_group = Envelope {
        session_id: vec![9; 16],
        init: None,
        encapsulated: Some(
            Encapsulated {
                counter: 3,
                group_identity: Some(common::GroupIdentity {
                    group_id: 0x77,
                    creator_identity: BOB.to_owned(),
                }),
            }
            .encode_to_vec(),
        ),
    }
    .encode_to_vec();
    let Verdict::RejectFs(reject) = verdict(&view, &Msg::new(ANN, 0xa0, &in_group)) else {
        panic!("rejected");
    };
    assert_eq!(
        reject.group,
        Some(GroupRef {
            creator: BOB.to_owned(),
            group_id: 0x77
        })
    );
    // An `Encapsulated` that does not read names no group: the 1:1 form, still rejected.
    let garbled = Envelope {
        session_id: vec![9; 16],
        init: None,
        encapsulated: Some(vec![0x32, 0xff]),
    }
    .encode_to_vec();
    let Verdict::RejectFs(reject) = verdict(&view, &Msg::new(ANN, 0xa0, &garbled)) else {
        panic!("rejected");
    };
    assert_eq!(reject.group, None);
    let init_only = Envelope {
        session_id: vec![9; 16],
        init: Some(vec![]),
        encapsulated: None,
    }
    .encode_to_vec();
    assert_eq!(
        discarded(verdict(&view, &Msg::new(ANN, 0xa0, &init_only))),
        (RULE_FS_IGNORED, true, true, vec![])
    );
    assert_eq!(
        discarded(verdict(&view, &Msg::new(ANN, 0xa0, &[0xff, 0xff]))).0,
        RULE_BODY
    );
}

/// STEP 13: the metadata's id is not the head's.
#[test]
fn step_13_an_id_mismatch_is_discarded() {
    let view = Fixture::default().with(direct(ANN));
    let mut msg = Msg::text(ANN);
    msg.meta = Some((0x2002, NOW, None));
    assert_eq!(
        discarded(verdict(&view, &msg)),
        (RULE_ID_MISMATCH, true, true, vec![])
    );
}

/// STEPS 18-19: an explicitly blocked sender, an unknown one under "block unknown" (Desktop `:1241`), a blocked member's
/// group text (`:1275`); a group setup's dedicated steps (`:1486`: a blocked creator of an unknown group); an exempt
/// group control from a blocked creator goes on to its own steps (`:1366`).
#[test]
fn steps_18_and_19_the_blocked_steps() {
    let view = Fixture::default().with(direct(ANN)).blocking(&[ANN], false);
    assert_eq!(
        discarded(verdict(&view, &Msg::text(ANN))),
        (RULE_BLOCKED, true, true, vec![])
    );
    let strict = Fixture::default().blocking(&[], true).cache(BOB);
    assert_eq!(discarded(verdict(&strict, &Msg::text(BOB))).0, RULE_BLOCKED);
    let members = Fixture::default()
        .with(direct(ANN))
        .with(direct(BOB))
        .blocking(&[BOB], false)
        .with_group(group(ANN, &[BOB, OWN], "g"));
    assert_eq!(
        discarded(verdict(&members, &Msg::group_text(BOB, ANN))).0,
        RULE_BLOCKED
    );
    let setup = Msg::creator(ANN, 0x4a, &[OWN.as_bytes(), BOB.as_bytes()].concat());
    assert_eq!(
        discarded(verdict(&view, &setup)).0,
        RULE_BLOCKED,
        "unknown group, blocked creator"
    );
    let known = view.with_group(group(ANN, &[], "g"));
    assert_eq!(
        held(verdict(&known, &setup)).0,
        RULE_GROUP_SYNC_NEEDED,
        "a known group passes the dedicated steps"
    );
    let rename = Msg::creator(ANN, 0x4b, b"new name");
    assert_eq!(
        held(verdict(&known, &rename)).0,
        RULE_GROUP_SYNC_NEEDED,
        "exempt from blocking"
    );
}

/// STEPS 20-21: a special contact's own type and another type (the decision after the open; the boxes of a special
/// contact open only with its builtin key's secret, which nobody here holds, so the open is proved by its refusal of
/// another key); from anybody else the special types are discarded at 22.7.
#[test]
fn steps_20_and_21_the_special_contacts() {
    use CspE2eMessageType as T;
    assert_eq!(special_rule("*3MAPUSH", T::WebSessionResume), RULE_SPECIAL);
    assert_eq!(special_rule("*3MAPUSH", T::Text), RULE_SPECIAL_TYPE);
    assert_eq!(
        special_rule("*3MAPUSH", T::WorkSyncDelta),
        RULE_SPECIAL_TYPE
    );
    assert_eq!(special_rule("*3MAW0RK", T::WorkSyncDelta), RULE_SPECIAL);
    assert_eq!(
        special_rule("*3MAW0RK", T::WebSessionResume),
        RULE_SPECIAL_TYPE
    );
    let mut push = Msg::new("*3MAW0RK", 0xfd, b"");
    push.meta = None;
    assert_eq!(
        discarded(verdict(&Fixture::default(), &push)).0,
        open::RULE_UNDECRYPTABLE
    );
    let others = Fixture::default().with(direct(ANN));
    assert_eq!(
        discarded(verdict(&others, &Msg::new(ANN, 0xfe, b"{}"))).0,
        RULE_SPECIAL_ONLY
    );
    assert_eq!(
        discarded(verdict(&others, &Msg::new(ANN, 0xfd, b""))).0,
        RULE_SPECIAL_ONLY
    );
}

/// STEP 22: the nickname and the contact: an existing contact's new nickname and its level raised to direct in ONE
/// update; a non-direct type does not raise it (Desktop `:620`); after the change committed the steps go on.
#[test]
fn step_22_the_contact_change() {
    use d2d_sync::contact::AcquaintanceLevel;
    let mut ann = contact(ANN, AcquaintanceLevel::GroupOrDeleted);
    ann.nickname = Some("Ann".to_owned());
    let view = Fixture::default().with(ann);
    let v = verdict(&view, &Msg::text(ANN).nickname("Annie"));
    assert_eq!(
        format!("{v:?}"),
        "Contact(Update { nickname: true, direct: true, .. })"
    );
    let Verdict::Contact(Change::Update {
        nickname,
        direct,
        identity,
    }) = v
    else {
        panic!("an update");
    };
    assert_eq!(
        (identity.as_str(), nickname.as_deref(), direct),
        (ANN, Some("Annie"), true)
    );
    // The same nickname and a type that creates no direct contact: no change.
    let receipt = Msg::new(ANN, 0x80, &[1, 1, 2, 3, 4, 5, 6, 7, 8]).nickname("Ann");
    assert!(matches!(verdict(&view, &receipt), Verdict::Process(_)));
    // No nickname in the metadata: no change of it; a legacy sender's zeroes clear it (footnote 1).
    let mut legacy = Msg::text(ANN);
    legacy.meta = None;
    let Verdict::Contact(Change::Update { nickname, .. }) = verdict(&view, &legacy) else {
        panic!("an update");
    };
    assert_eq!(nickname.as_deref(), Some(""));
    // T-L4: a legacy nickname that is not UTF-8 is no nickname, never a discard.
    legacy.legacy = [0xff; 32];
    let Verdict::Contact(Change::Update {
        nickname, direct, ..
    }) = verdict(&view, &legacy)
    else {
        panic!("an update");
    };
    assert_eq!((nickname, direct), (None, true));
    // After the change committed: processed.
    assert!(matches!(
        decide_with(&view, &Msg::text(ANN).nickname("Annie"), None, true),
        Verdict::Process(_)
    ));
}

/// 22.4.1 AND T-B1: a group text from an unknown sender is discarded and acked, EXCEPT when its group has a held
/// control message: then it is HELD (the member a held setup added is not a contact).
#[test]
fn step_22_4_1_an_unknown_senders_group_text_and_the_held_group() {
    let mut view = Fixture::default()
        .with(direct(ANN))
        .with_group(group(ANN, &[OWN], "g"))
        .cache(NEW);
    assert!(
        !view.contacts.contains_key(NEW),
        "the new member is not a contact"
    );
    assert_eq!(
        discarded(verdict(&view, &Msg::group_text(NEW, ANN))),
        (RULE_SENDER_UNKNOWN, true, true, vec![])
    );
    // The setup that adds NEW is held, and the socket keeps its group.
    let setup = Msg::creator(ANN, 0x4a, &[OWN.as_bytes(), NEW.as_bytes()].concat());
    let (rule, of) = held(verdict(&view, &setup));
    assert_eq!(
        (rule, of.as_ref()),
        (RULE_GROUP_SYNC_NEEDED, Some(&key(ANN)))
    );
    view.held.insert(key(ANN));
    assert_eq!(
        held(verdict(&view, &Msg::group_text(NEW, ANN))),
        (RULE_GROUP_HELD_BEFORE, None)
    );
    // A known member's text in the held group is processed (a held change does not hold what the steps keep).
    view.contacts.insert(BOB.to_owned(), direct(BOB));
    view.groups
        .insert((ANN.to_owned(), GID), group(ANN, &[BOB, OWN], "g"));
    assert!(matches!(
        verdict(&view, &Msg::group_text(BOB, ANN)),
        Verdict::Process(_)
    ));
}

/// 22.7 THE COMMON GROUP RECEIVE STEPS: unknown group (a sync request, none within the hour, none to a blocked creator,
/// none for the user's own group), left (a leave, or an empty setup from the creator), a sender not a member (a sync
/// request, or an empty setup), a member's text kept with no receipt; each discard HELD when its group is held.
#[test]
fn step_22_7_the_common_group_receive_steps() {
    let base = Fixture::default().with(direct(ANN)).with(direct(BOB));
    let (rule, _, nonce, jobs) = discarded(verdict(&base, &Msg::group_text(BOB, ANN)));
    assert_eq!(
        (rule, nonce, jobs),
        (
            RULE_GROUP_UNKNOWN,
            true,
            vec![Job::SyncRequest { group: key(ANN) }]
        )
    );
    let mut throttled = Fixture::default().with(direct(ANN)).with(direct(BOB));
    throttled.sync_throttled.insert(key(ANN));
    assert_eq!(
        discarded(verdict(&throttled, &Msg::group_text(BOB, ANN))).3,
        vec![]
    );
    let blocked = Fixture::default().with(direct(BOB)).blocking(&[ANN], false);
    assert_eq!(
        discarded(verdict(&blocked, &Msg::group_text(BOB, ANN))).0,
        RULE_GROUP_UNKNOWN_BLOCKED
    );
    assert_eq!(
        discarded(verdict(&base, &Msg::group_text(BOB, OWN))).0,
        RULE_GROUP_UNKNOWN_OWN
    );

    let mut left = group(ANN, &[BOB], "g");
    left.user_state = Some(d2d_sync::group::UserState::Left as i32);
    let view = Fixture::default()
        .with(direct(ANN))
        .with(direct(BOB))
        .with_group(left);
    let (rule, _, _, jobs) = discarded(verdict(&view, &Msg::group_text(BOB, ANN)));
    assert_eq!(
        (rule, jobs),
        (
            RULE_GROUP_LEFT,
            vec![Job::Leave {
                to: BOB.to_owned(),
                group: key(ANN)
            }]
        )
    );
    let mut own_left = group(OWN, &[BOB], "g");
    own_left.user_state = Some(d2d_sync::group::UserState::Left as i32);
    let view = Fixture::default().with(direct(BOB)).with_group(own_left);
    assert_eq!(
        discarded(verdict(&view, &Msg::group_text(BOB, OWN))).3,
        vec![Job::EmptySetup {
            to: BOB.to_owned(),
            group_id: GID
        }]
    );

    let mut view = Fixture::default()
        .with(direct(ANN))
        .with(direct(BOB))
        .with_group(group(ANN, &[OWN], "g"))
        .with_group(group(OWN, &[ANN], "mine"));
    assert_eq!(
        discarded(verdict(&view, &Msg::group_text(BOB, ANN))),
        (
            RULE_GROUP_NOT_MEMBER,
            true,
            true,
            vec![Job::SyncRequest { group: key(ANN) }]
        )
    );
    assert_eq!(
        discarded(verdict(&view, &Msg::group_text(BOB, OWN))).3,
        vec![Job::EmptySetup {
            to: BOB.to_owned(),
            group_id: GID
        }]
    );
    let keep = kept(verdict(&view, &Msg::group_text(ANN, ANN)));
    assert!(
        keep.reflect && !keep.receipt && keep.ack,
        "no receipt to a group"
    );
    view.held.insert(key(ANN));
    assert_eq!(
        held(verdict(&view, &Msg::group_text(BOB, ANN))).0,
        RULE_GROUP_HELD_BEFORE
    );
}

/// T-M1: GROUP CONTROL against the stored group: the periodic re-send of an unchanged setup, name, picture or deleted
/// picture is discarded and acked; a real change is HELD with its group; a held group holds every control message;
/// a leave from a member is a change, from a non-member none, from the creator a discard.
#[test]
fn group_control_holds_only_real_changes() {
    let mut stored = group(ANN, &[BOB], "Rhein");
    let blob = [0xb1; 16];
    stored.profile_picture = Some(DeltaImage {
        image: Some(common::delta_image::Image::Updated(common::Image {
            r#type: 0,
            blob: Some(common::Blob {
                id: blob.to_vec(),
                ..Default::default()
            }),
        })),
    });
    let mut view = Fixture::default()
        .with(direct(ANN))
        .with(direct(BOB))
        .with_group(stored.clone());
    let no_change = |v: Verdict| discarded(v).0;
    let same_setup = Msg::creator(
        ANN,
        0x4a,
        &[BOB.as_bytes(), OWN.as_bytes(), BOB.as_bytes()].concat(),
    );
    assert_eq!(
        no_change(verdict(&view, &same_setup)),
        RULE_GROUP_NO_CHANGE,
        "duplicates and order do not count"
    );
    assert_eq!(
        no_change(verdict(&view, &Msg::creator(ANN, 0x4b, b"Rhein"))),
        RULE_GROUP_NO_CHANGE
    );
    let picture = [blob.as_slice(), &[0, 1, 0, 0], &[7; 32]].concat();
    assert_eq!(
        no_change(verdict(&view, &Msg::creator(ANN, 0x50, &picture))),
        RULE_GROUP_NO_CHANGE
    );
    for (ty, inner) in [
        (
            0x4a_u8,
            [BOB.as_bytes(), OWN.as_bytes(), NEW.as_bytes()].concat(),
        ),
        (0x4b, b"Main".to_vec()),
        (
            0x50,
            [[0xb2; 16].as_slice(), &[0, 1, 0, 0], &[7; 32]].concat(),
        ),
        (0x54, vec![]),
    ] {
        assert_eq!(
            held(verdict(&view, &Msg::creator(ANN, ty, &inner))),
            (RULE_GROUP_SYNC_NEEDED, Some(key(ANN))),
            "{ty:#04x}"
        );
    }
    // A deleted picture is no change only when the file says REMOVED.
    stored.profile_picture = removed();
    view.groups.insert((ANN.to_owned(), GID), stored);
    assert_eq!(
        no_change(verdict(&view, &Msg::creator(ANN, 0x54, &[]))),
        RULE_GROUP_NO_CHANGE
    );
    // A setup without the user for a group the file does not know: the spec's discard.
    let other = Msg::creator(BOB, 0x4a, ANN.as_bytes());
    assert_eq!(discarded(verdict(&view, &other)).0, RULE_SETUP_NOT_MEMBER);
    // Leaves.
    assert_eq!(
        held(verdict(&view, &Msg::member(BOB, ANN, 0x4c, &[]))).0,
        RULE_GROUP_SYNC_NEEDED
    );
    view.contacts.insert(NEW.to_owned(), direct(NEW));
    assert_eq!(
        no_change(verdict(&view, &Msg::member(NEW, ANN, 0x4c, &[]))),
        RULE_GROUP_NO_CHANGE
    );
    assert_eq!(
        discarded(verdict(&view, &Msg::member(ANN, ANN, 0x4c, &[]))).0,
        RULE_LEAVE_CREATOR
    );
    // A held group holds even the re-send that would be no change.
    view.held.insert(key(ANN));
    assert_eq!(
        held(verdict(&view, &Msg::creator(ANN, 0x4b, b"Rhein"))).0,
        RULE_GROUP_HELD_BEFORE
    );
    // Members that are no IDs: step 10's discard.
    view.held.clear();
    assert_eq!(
        discarded(verdict(&view, &Msg::creator(ANN, 0x4a, b"short"))).0,
        RULE_BODY
    );
}

/// C7 AND T-M3: the creator's answer: not the creator, an unknown group, within the hour: discarded; a sender not a
/// member: an empty setup, reflected and acked; a member and a picture EXPLICITLY removed: the three-job answer; a
/// picture set or never stated: HELD.
#[test]
fn the_creators_answer_and_the_picture_rule() {
    let mut mine = group(OWN, &[ANN], "mine");
    let mut view = Fixture::default()
        .with(direct(ANN))
        .with(direct(BOB))
        .with_group(mine.clone());
    let request = |sender: &str| Msg::member(sender, OWN, 0x51, &[]);
    assert_eq!(
        held(verdict(&view, &request(ANN))),
        (RULE_CREATOR_PICTURE, None),
        "a picture never stated"
    );
    mine.profile_picture = removed();
    view.groups.insert((OWN.to_owned(), GID), mine);
    let keep = kept(verdict(&view, &request(ANN)));
    assert_eq!(
        keep.jobs,
        vec![Job::Answer {
            to: ANN.to_owned(),
            group_id: GID
        }]
    );
    assert!(keep.reflect && keep.ack && !keep.receipt);
    assert_eq!(
        kept(verdict(&view, &request(BOB))).jobs,
        vec![Job::EmptySetup {
            to: BOB.to_owned(),
            group_id: GID
        }]
    );
    view.answer_throttled.insert((GID, ANN.to_owned()));
    assert_eq!(
        discarded(verdict(&view, &request(ANN))).0,
        RULE_SYNC_THROTTLED
    );
    let theirs = Fixture::default()
        .with(direct(ANN))
        .with_group(group(BOB, &[ANN, OWN], "t"));
    assert_eq!(
        discarded(verdict(&theirs, &Msg::member(ANN, BOB, 0x51, &[]))).0,
        RULE_SYNC_NOT_CREATOR
    );
    assert_eq!(
        discarded(verdict(&view, &Msg::member(ANN, BOB, 0x51, &[]))).0,
        RULE_GROUP_UNKNOWN
    );
}

/// THE ROW TYPING: an event and an ephemeral reflect; the row EMPTY: the spec's discard; a receipt as a reaction marks
/// the nonce, a plain receipt does not.
#[test]
fn typing_is_ephemeral_empty_is_discarded_and_a_receipt_marks_its_nonce_by_its_status() {
    let view = Fixture::default().with(direct(ANN));
    let mut typing = Msg::new(ANN, 0x90, &[1]);
    typing.flags = 0x06;
    let Verdict::Ephemeral(e) = verdict(&view, &typing) else {
        panic!("ephemeral");
    };
    assert_eq!(
        (e.outer_type, e.body.as_slice(), e.ack),
        (CspE2eMessageType::TypingIndicator, [1_u8].as_slice(), false)
    );
    // S-L3 = P-L2: a typing indicator without `0x04` owes its ack (step 25).
    typing.flags = 0x02;
    let Verdict::Ephemeral(e) = verdict(&view, &typing) else {
        panic!("ephemeral");
    };
    assert!(e.ack, "a typing indicator without 0x04 owes no ack");
    typing.flags = 0x06;
    assert_eq!(
        discarded(verdict(&view, &Msg::new(ANN, 0xfc, &[]))),
        (RULE_EMPTY, true, true, vec![])
    );
    let receipt = |status: u8| Msg::new(ANN, 0x80, &[&[status][..], &[1; 8]].concat());
    assert!(
        !kept(verdict(&view, &receipt(1))).nonce,
        "a received receipt: no replay protection"
    );
    assert!(
        kept(verdict(&view, &receipt(3))).nonce,
        "a reaction: protected"
    );
    // The unknown sender of a typing indicator waits for the directory too (the spec's order, T-H3's subject).
    assert!(matches!(
        verdict(&Fixture::default(), &typing),
        Verdict::Lookup
    ));
}

/// TH6 FIX-A, S-B1: while a BLIND hold stands on the socket, every group counts as held: a stored contact's text in a
/// group the file does not know (the Common Group Receive Steps' `group-unknown`, acked) and in a group the contact is
/// no member of, a stranger's group text (22.4.1) and every group control are HELD `group-held-before`; a 1:1 text and
/// a member's text in a stored group still go.
#[test]
fn a_blind_hold_holds_every_group_message_the_steps_would_discard() {
    let mut view = Fixture::default()
        .with(direct(ANN))
        .with(direct(BOB))
        .with_group(group(BOB, &[ANN, OWN], "g"))
        .cache(NEW);
    assert_eq!(
        discarded(verdict(&view, &Msg::group_text(ANN, NEW))).0,
        RULE_GROUP_UNKNOWN,
        "without the mark: the unknown group's discard"
    );
    view.blind = true;
    for msg in [
        Msg::group_text(ANN, NEW),
        Msg::group_text(NEW, BOB),
        Msg::creator(BOB, 0x4b, b"g"),
    ] {
        assert_eq!(held(verdict(&view, &msg)).0, RULE_GROUP_HELD_BEFORE);
    }
    assert!(matches!(
        verdict(&view, &Msg::text(ANN)),
        Verdict::Process(_)
    ));
    assert!(matches!(
        verdict(&view, &Msg::group_text(ANN, BOB)),
        Verdict::Process(_)
    ));
}

/// TH6 re-check R-L2: the blind mark reaches 1:1 messages too. A stranger's reaction (no implicit direct contact: 22.4.1)
/// is discarded and acked while no blind hold stands, and HELD `group-held-before` while one does: the message it refers
/// to may be one the steps could not place. A stranger's text (it makes the contact, 22.4.2) and a stored contact's
/// 1:1 messages are not affected.
#[test]
fn a_blind_hold_holds_a_strangers_one_to_one_message_that_22_4_1_would_discard() {
    let mut view = Fixture::default().with(direct(ANN)).cache(NEW);
    let reaction = Msg::new(NEW, 0x82, &[0x01; 9]);
    assert_eq!(
        discarded(verdict(&view, &reaction)),
        (RULE_SENDER_UNKNOWN, true, true, vec![]),
        "without the mark: 22.4.1's discard"
    );
    view.blind = true;
    assert_eq!(
        held(verdict(&view, &reaction)),
        (RULE_GROUP_HELD_BEFORE, None)
    );
    assert!(matches!(
        verdict(&view, &Msg::text(NEW)),
        Verdict::Contact(Change::Create { .. })
    ));
    assert!(matches!(
        verdict(&view, &Msg::text(ANN)),
        Verdict::Process(_)
    ));
}

/// TH6 FIX-A, P-L1: group control from a sender the file does not hold follows 22.4.1: `group-leave`, `group-name` and
/// the two picture types create no implicit direct contact, so they are discarded `csp-sender-unknown` and acked with
/// NO job (no `group-sync-request` a stranger could make the user send), or HELD when their group is; a stranger's
/// `group-setup` that its own steps discard makes the contact first (22.4.2, the `Create` with the nickname), then, the
/// contact done, is discarded by its rule; one that changes a group is held for the phone (T-M1).
#[test]
fn a_strangers_group_control_follows_step_22_4_1() {
    let mut view = Fixture::default()
        .with(direct(ANN))
        .with_group(group(ANN, &[OWN], "g"))
        .cache(NEW);
    for msg in [
        Msg::member(NEW, ANN, 0x4c, &[]),
        Msg::member(NEW, BOB, 0x4c, &[]),
        Msg::creator(NEW, 0x4b, b"mine"),
        Msg::creator(NEW, 0x54, &[]),
        Msg::creator(
            NEW,
            0x50,
            &[[0xb2; 16].as_slice(), &[0, 1, 0, 0], &[7; 32]].concat(),
        ),
    ] {
        assert_eq!(
            discarded(verdict(&view, &msg)),
            (RULE_SENDER_UNKNOWN, true, true, vec![]),
            "{:#04x}",
            msg.ty
        );
    }
    let not_member = Msg::creator(NEW, 0x4a, ANN.as_bytes()).nickname("Stranger");
    let Verdict::Contact(Change::Create { init, nickname }) = verdict(&view, &not_member) else {
        panic!("the contact first");
    };
    assert_eq!(
        (init.identity.as_str(), nickname.as_deref()),
        (NEW, Some("Stranger"))
    );
    assert_eq!(
        discarded(decide_with(&view, &not_member, None, true)).0,
        RULE_SETUP_NOT_MEMBER,
        "the contact done: the setup's own discard"
    );
    let adds_me = Msg::creator(NEW, 0x4a, &[OWN.as_bytes(), ANN.as_bytes()].concat());
    let (rule, of) = held(verdict(&view, &adds_me));
    assert_eq!(
        (rule, of.map(|g| g.creator)),
        (RULE_GROUP_SYNC_NEEDED, Some(NEW.to_owned()))
    );
    view.held.insert(GroupRef {
        creator: NEW.to_owned(),
        group_id: GID,
    });
    assert_eq!(
        held(verdict(&view, &Msg::creator(NEW, 0x4b, b"mine"))),
        (RULE_GROUP_HELD_BEFORE, None),
        "its group is held"
    );
}

/// TH6 FIX-A, P-M1: a `group-delete-profile-picture` of a group whose stored picture is ABSENT (Desktop leaves the field
/// out for a group made without one), an image-less `DeltaImage` or one explicitly removed is no change (discarded,
/// acked); with a stored picture it is a change (held). The SET half stays a hold: a `group-set-profile-picture` under
/// any blob id but the stored one is held (the LIMIT until TH8: a creator's re-upload reads as a change).
#[test]
fn a_deleted_picture_of_a_group_without_one_is_no_change_and_a_new_blob_is_held() {
    let mut stored = group(ANN, &[BOB], "Rhein");
    assert!(stored.profile_picture.is_none(), "the field absent");
    let mut view = Fixture::default()
        .with(direct(ANN))
        .with(direct(BOB))
        .with_group(stored.clone());
    let delete = Msg::creator(ANN, 0x54, &[]);
    let set = Msg::creator(
        ANN,
        0x50,
        &[[0xb2; 16].as_slice(), &[0, 1, 0, 0], &[7; 32]].concat(),
    );
    for picture in [None, Some(DeltaImage { image: None }), removed()] {
        stored.profile_picture = picture;
        view.groups.insert((ANN.to_owned(), GID), stored.clone());
        assert_eq!(discarded(verdict(&view, &delete)).0, RULE_GROUP_NO_CHANGE);
        assert_eq!(
            held(verdict(&view, &set)),
            (RULE_GROUP_SYNC_NEEDED, Some(key(ANN))),
            "a set picture under a new blob id"
        );
    }
}

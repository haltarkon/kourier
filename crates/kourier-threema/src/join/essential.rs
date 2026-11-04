//! The checks of a join's `EssentialData` before anything is written (TH4 J4a: brief §2.7 step 1,
//! critic H5, decisions C5, C7, C12, E3), and what passes them: [`Import`].
//!
//! EVERY bad field or item REFUSES the whole join (C12, Desktop's choice, `join.ts:390-424` and
//! `validate/join/essential-data.ts`): nothing is dropped silently but the blobs nothing references
//! (C5) and repeated hashed nonces (kept once, counted, as Desktop does). A refusal is a [`Refused`]
//! that names the RULE only (a `&'static str` of [`rule`]), never an ID, a name or a key; the join fails
//! `Unsupported` at step `linking` (E3) and the log line names the rule only.
//!
//! The checks:
//! - the own identity: 8 characters libthreema's `ThreemaId` accepts; CK 32 bytes, the device cookie
//!   16, the chat server group `[A-Za-z0-9-]+` (libthreema's `ChatServerGroup`, schema 1's CHECK); DGK 32;
//! - no Threema Work data (`work_credentials`, `mdm_parameters`: the Consumer flavour has none, E3);
//! - the user profile and the settings present (Desktop requires both);
//! - the bounds of C7 ([`Bounds::REAL`]): over one -> refused. S2: they are counted TWICE, first on the
//!   wire bytes by [`wire_check`] BEFORE the prost decode (a 2-byte empty entry becomes a struct of
//!   hundreds of bytes in memory, so 100 MiB of them cannot be decoded first), then on the decoded data;
//! - each contact: an ID `ThreemaId` accepts, a public key of 32 bytes, no ID twice, not the own ID;
//! - each group: a group identity; its creator is a contact or the own ID; each member is a contact and
//!   not the own ID; no `(creator, group id)` twice;
//! - each distribution list: no id twice (its members are not checked: Kourier imports no list yet, as
//!   Desktop, `join.ts` `TODO(DESK-236)`) [I];
//! - each picture (the profile's, each contact's two, each group's) that names a blob: a blob id of 16
//!   bytes (libthreema's `BlobId::LENGTH`, Desktop's `BLOB_ID_LENGTH`) that came as `BlobData` before the
//!   data (`TP/md-d2d-join.proto` "When receiving this message" step 2); a picture "removed" names none;
//! - each hashed nonce 32 bytes (HMAC-SHA256);
//! - each `last_update_at` (contacts, groups, lists) at most `i64::MAX` ms (decision E4: bad data, so
//!   every stored time is a real i64).
//!
//! The keys leave the message at once (into [`Zeroizing`]), before any check. The device ids are drawn
//! here (step 4 of the spec): a random non-zero u64 each for D2M and CSP.

use std::collections::{HashMap, HashSet};
use std::mem;

use kourier_model::BackendError;
use libthreema::common::{ChatServerGroup, ThreemaId};
use libthreema::protobuf::common::{self, delta_image};
use libthreema::protobuf::{d2d_join, d2d_sync};
use prost::Message as _;
use rand::rngs::StdRng;
use rand::{RngCore, SeedableRng as _};
use zeroize::Zeroizing;

use super::Received;
use crate::store::{Identity, SlotState};

/// The names of the rules: what a refusal says (the error's detail and the log line), and nothing else.
pub(crate) mod rule {
    pub(crate) const NO_IDENTITY_DATA: &str = "the identity data is missing";
    pub(crate) const OWN_ID: &str = "the own ID is not a Threema ID";
    pub(crate) const CLIENT_KEY: &str = "the client key is not 32 bytes";
    pub(crate) const DEVICE_COOKIE: &str = "the device cookie is not 16 bytes";
    pub(crate) const SERVER_GROUP: &str = "the chat server group is not [A-Za-z0-9-]+";
    pub(crate) const NO_DEVICE_GROUP_DATA: &str = "the device group data is missing";
    pub(crate) const DEVICE_GROUP_KEY: &str = "the device group key is not 32 bytes";
    pub(crate) const WORK_CREDENTIALS: &str = "Threema Work is not supported";
    pub(crate) const MDM_PARAMETERS: &str = "Threema Work is not supported (MDM parameters)";
    pub(crate) const NO_USER_PROFILE: &str = "the user profile is missing";
    pub(crate) const NO_SETTINGS: &str = "the settings are missing";
    pub(crate) const TOO_MANY_CONTACTS: &str = "more contacts than Kourier imports";
    pub(crate) const TOO_MANY_GROUPS: &str = "more groups than Kourier imports";
    pub(crate) const TOO_MANY_LISTS: &str = "more distribution lists than Kourier imports";
    pub(crate) const TOO_MANY_MEMBERS: &str =
        "more members in a group or list than Kourier imports";
    pub(crate) const TOO_MANY_MEMBERS_IN_ALL: &str =
        "more member entries in all groups and lists than Kourier imports";
    pub(crate) const TOO_MANY_BLOCKED: &str = "more blocked IDs than Kourier imports";
    pub(crate) const TOO_MANY_EXCLUDED: &str =
        "more IDs excluded from the sync than Kourier imports";
    pub(crate) const TOO_MANY_CSP_NONCES: &str = "more CSP hashed nonces than Kourier imports";
    pub(crate) const TOO_MANY_D2D_NONCES: &str = "more D2D hashed nonces than Kourier imports";
    pub(crate) const PROFILE_TOO_LARGE: &str = "the user profile is larger than Kourier imports";
    pub(crate) const SETTINGS_TOO_LARGE: &str = "the settings are larger than Kourier imports";
    pub(crate) const NONCE: &str = "a hashed nonce is not 32 bytes";
    pub(crate) const NO_CONTACT: &str = "a contact entry holds no contact";
    pub(crate) const CONTACT_ID: &str = "a contact's ID is not a Threema ID";
    pub(crate) const CONTACT_KEY: &str = "a contact's public key is not 32 bytes";
    pub(crate) const DUPLICATE_CONTACT: &str = "a contact is listed twice";
    pub(crate) const OWN_ID_AS_CONTACT: &str = "the own ID is a contact";
    pub(crate) const NO_GROUP: &str = "a group entry holds no group";
    pub(crate) const NO_GROUP_IDENTITY: &str = "a group has no group identity";
    pub(crate) const CREATOR_UNKNOWN: &str =
        "a group's creator is neither a contact nor the own ID";
    pub(crate) const MEMBER_UNKNOWN: &str = "a group's member is not a contact";
    pub(crate) const OWN_ID_AS_MEMBER: &str = "the own ID is a member of a group";
    pub(crate) const DUPLICATE_GROUP: &str = "a group is listed twice";
    pub(crate) const NO_LIST: &str = "a distribution list entry holds no list";
    pub(crate) const DUPLICATE_LIST: &str = "a distribution list is listed twice";
    pub(crate) const PICTURE_WITHOUT_BLOB: &str = "a picture names no blob";
    pub(crate) const BLOB_ID: &str = "a blob id is not 16 bytes";
    pub(crate) const MISSING_BLOB: &str = "a referenced blob was not received";
    pub(crate) const LAST_UPDATE_AT: &str = "a last update time is past i64::MAX";
}

/// The length of a Blob ID (libthreema's `BlobId::LENGTH`, Desktop's `BLOB_ID_LENGTH`, 0002's CHECK).
pub(crate) const BLOB_ID_LEN: usize = 16;

/// The most wire bytes of `EssentialData.user_profile`, summed over its occurrences (they merge in
/// prost). Re-check M1: its `profile_picture_share_with.allow_list` and `identity_links.links` are
/// repeated fields that [`wire_check`] does not count one by one; an empty link is two bytes on the wire
/// and a struct of tens of bytes once decoded, so the bytes are bounded instead. A real profile (a
/// nickname, a picture's blob reference, a few links, an allow-list) is a few KiB.
pub(crate) const MAX_PROFILE_BYTES: usize = 1 << 20;

/// The most wire bytes of `EssentialData.settings`, summed over its occurrences (re-check M1, a belt
/// beside C2's counts): 100,000 blocked IDs and 100,000 excluded ones are about 1 MB each on the wire.
pub(crate) const MAX_SETTINGS_BYTES: usize = 4 << 20;

/// The most of each kind one join imports (decision C7; over one the join is refused). The blobs are
/// bounded on arrival by the join task (`MAX_BLOBS`, `MAX_BLOB_BYTES`, critic H4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Bounds {
    pub(crate) contacts: usize,
    pub(crate) groups: usize,
    /// Distribution lists (S2; C7 named none, and a list is decoded all the same).
    pub(crate) lists: usize,
    /// Members of ONE group, and of ONE distribution list (S2).
    pub(crate) members: usize,
    /// Member entries summed over ALL groups and ALL distribution lists (C1: 10,000 groups of 10,000
    /// members each would still be a hundred million empty strings in a 100 MiB frame).
    pub(crate) members_total: usize,
    /// Entries of `Settings.blocked_identities`, and of `Settings.exclude_from_sync_identities`, each (C2).
    pub(crate) identities: usize,
    /// Per store (CSP and D2D each). On the wire every entry counts ([`wire_check`]); on the decoded data
    /// each distinct nonce does (the repeats are dropped).
    pub(crate) nonces: usize,
}

impl Bounds {
    /// C7: 100,000 contacts, 10,000 groups, 1,000,000 hashed nonces per store; S2: 10,000 distribution
    /// lists and 10,000 members per group or list; C1: 1,000,000 member entries in all groups and lists
    /// together; C2: 100,000 blocked IDs and 100,000 IDs excluded from the sync. NO limit of a group's size is in the protocol spec
    /// (`md-d2d-sync.proto`, `md-d2d-join.proto`) or in Threema Desktop's source (the only 256 in it is the
    /// example value of a UI component's test), so the number is the fallback the lead named: 10,000.
    pub(crate) const REAL: Self = Self {
        contacts: 100_000,
        groups: 10_000,
        lists: 10_000,
        members: 10_000,
        members_total: 1_000_000,
        identities: 100_000,
        nonces: 1_000_000,
    };
}

/// Why the data of a join was refused: the rule's name ([`rule`]), nothing of the data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Refused(pub(crate) &'static str);

impl Refused {
    /// The error of the `Failed` step at `linking` (E3: `Unsupported`; the viewmodel's words are its
    /// generic ones).
    pub(crate) fn error(self) -> BackendError {
        BackendError::unsupported(self.0)
    }
}

/// Why the wire bytes of an `EdToNd` were not taken on to the decode (S2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wire {
    /// The bytes are not a well-formed protobuf message (the decode would say so).
    Malformed,
    /// A count is over its bound; the rule's name ([`rule`]).
    Over(&'static str),
}

/// The field numbers [`wire_check`] walks (`md-d2d-join.proto`, `md-d2d-sync.proto`, `common.proto`).
mod field {
    /// `EdToNd.essential_data`.
    pub(super) const ESSENTIAL_DATA: u32 = 3;
    /// `EssentialData.user_profile`.
    pub(super) const USER_PROFILE: u32 = 4;
    /// `EssentialData.settings`.
    pub(super) const SETTINGS: u32 = 5;
    /// `EssentialData.contacts` / `.groups` / `.distribution_lists` / `.csp_hashed_nonces` /
    /// `.d2d_hashed_nonces`.
    pub(super) const CONTACTS: u32 = 7;
    pub(super) const GROUPS: u32 = 8;
    pub(super) const LISTS: u32 = 9;
    pub(super) const CSP_NONCES: u32 = 10;
    pub(super) const D2D_NONCES: u32 = 11;
    /// `AugmentedGroup.group` and `AugmentedDistributionList.distribution_list`.
    pub(super) const ENTRY_OBJECT: u32 = 1;
    /// `Group.member_identities` and `DistributionList.member_identities` (a `common.Identities`).
    pub(super) const GROUP_MEMBERS: u32 = 8;
    pub(super) const LIST_MEMBERS: u32 = 6;
    /// `Settings.blocked_identities` and `Settings.exclude_from_sync_identities` (`common.Identities`).
    pub(super) const BLOCKED: u32 = 9;
    pub(super) const EXCLUDED: u32 = 10;
    /// `common.Identities.identities`.
    pub(super) const IDENTITIES: u32 = 1;
}

/// Calls `each(tag, payload)` for every length-delimited field of the message in `buf` (the payload
/// borrowed from `buf`, nothing copied or allocated); every other field is skipped by prost's
/// `skip_field`. A truncated or invalid field is [`Wire::Malformed`].
fn each_field<'a>(
    mut buf: &'a [u8],
    mut each: impl FnMut(u32, &'a [u8]) -> Result<(), Wire>,
) -> Result<(), Wire> {
    use prost::encoding::{DecodeContext, WireType, decode_key, decode_varint, skip_field};
    while !buf.is_empty() {
        let (tag, wire_type) = decode_key(&mut buf).map_err(|_| Wire::Malformed)?;
        if wire_type == WireType::LengthDelimited {
            let len = decode_varint(&mut buf).map_err(|_| Wire::Malformed)?;
            let len = usize::try_from(len).map_err(|_| Wire::Malformed)?;
            if len > buf.len() {
                return Err(Wire::Malformed);
            }
            let (payload, rest) = buf.split_at(len);
            buf = rest;
            each(tag, payload)?;
        } else {
            skip_field(wire_type, tag, &mut buf, DecodeContext::default())
                .map_err(|_| Wire::Malformed)?;
        }
    }
    Ok(())
}

/// The members of one `AugmentedGroup` / `AugmentedDistributionList` (`entry`): the entries of
/// `object.member_identities.identities`, counted over every occurrence of those fields (prost merges
/// repeated ones), refused above `bounds.members`; every one is also added to `total` (all groups and
/// lists together, C1), refused above `bounds.members_total`.
fn wire_members(
    entry: &[u8],
    members_field: u32,
    bounds: &Bounds,
    total: &mut usize,
) -> Result<(), Wire> {
    let mut members = 0_usize;
    each_field(entry, |tag, object| {
        if tag != field::ENTRY_OBJECT {
            return Ok(());
        }
        each_field(object, |tag, identities| {
            if tag != members_field {
                return Ok(());
            }
            each_field(identities, |tag, _| {
                if tag == field::IDENTITIES {
                    members += 1;
                    *total += 1;
                    if members > bounds.members {
                        return Err(Wire::Over(rule::TOO_MANY_MEMBERS));
                    }
                    if *total > bounds.members_total {
                        return Err(Wire::Over(rule::TOO_MANY_MEMBERS_IN_ALL));
                    }
                }
                Ok(())
            })
        })
    })
}

/// The entries of `Identities.identities` in `identities` (a `common.Identities`), added to `count` (the
/// occurrences of the settings merge in prost, so the count runs on across them), refused above `bound`.
fn wire_identities(
    identities: &[u8],
    count: &mut usize,
    bound: usize,
    too_many: &'static str,
) -> Result<(), Wire> {
    each_field(identities, |tag, _| {
        if tag == field::IDENTITIES {
            *count += 1;
            if *count > bound {
                return Err(Wire::Over(too_many));
            }
        }
        Ok(())
    })
}

/// S2: the C7 counts on the WIRE BYTES of one `EdToNd` frame, before anything is decoded. prost keeps a
/// nested message inline, so an empty `AugmentedContact` (`3a 00`, two bytes) is a struct of hundreds of
/// bytes once decoded: a frame of 100 MiB (libthreema's `MAX_LENGTH_AFTER_NOMINATION`) could allocate
/// tens of GB in `EdToNd::decode`. This walks the frame with prost's `decode_key` / `skip_field`
/// (nothing allocated, no field decoded) and refuses a frame with more than `bounds.contacts` contacts,
/// `.groups` groups, `.lists` lists, `.nonces` entries of either hashed-nonce store (repeats count here:
/// they are decoded all the same), `.members` members of one group or list, `.members_total` members in
/// all groups and lists together (C1) or `.identities` entries of either of the settings' two ID lists
/// (C2), or a user profile over [`MAX_PROFILE_BYTES`] or settings over [`MAX_SETTINGS_BYTES`] (M1);
/// exactly N passes. A frame without `EssentialData` passes (nothing to count).
pub(crate) fn wire_check(frame: &[u8], bounds: &Bounds) -> Result<(), Wire> {
    let (mut contacts, mut groups, mut lists, mut csp, mut d2d) = (0_usize, 0, 0, 0, 0);
    let (mut members_in_all, mut blocked, mut excluded) = (0_usize, 0_usize, 0_usize);
    let (mut profile_bytes, mut settings_bytes) = (0_usize, 0_usize);
    let over = |n: usize, bound: usize, rule: &'static str| {
        if n > bound {
            Err(Wire::Over(rule))
        } else {
            Ok(())
        }
    };
    each_field(frame, |tag, essential| {
        if tag != field::ESSENTIAL_DATA {
            return Ok(());
        }
        each_field(essential, |tag, entry| match tag {
            field::USER_PROFILE => {
                profile_bytes += entry.len();
                over(profile_bytes, MAX_PROFILE_BYTES, rule::PROFILE_TOO_LARGE)
            }
            field::SETTINGS => {
                settings_bytes += entry.len();
                over(settings_bytes, MAX_SETTINGS_BYTES, rule::SETTINGS_TOO_LARGE)?;
                each_field(entry, |tag, identities| match tag {
                    field::BLOCKED => wire_identities(
                        identities,
                        &mut blocked,
                        bounds.identities,
                        rule::TOO_MANY_BLOCKED,
                    ),
                    field::EXCLUDED => wire_identities(
                        identities,
                        &mut excluded,
                        bounds.identities,
                        rule::TOO_MANY_EXCLUDED,
                    ),
                    _ => Ok(()),
                })
            }
            field::CONTACTS => {
                contacts += 1;
                over(contacts, bounds.contacts, rule::TOO_MANY_CONTACTS)
            }
            field::GROUPS => {
                groups += 1;
                over(groups, bounds.groups, rule::TOO_MANY_GROUPS)?;
                wire_members(entry, field::GROUP_MEMBERS, bounds, &mut members_in_all)
            }
            field::LISTS => {
                lists += 1;
                over(lists, bounds.lists, rule::TOO_MANY_LISTS)?;
                wire_members(entry, field::LIST_MEMBERS, bounds, &mut members_in_all)
            }
            field::CSP_NONCES => {
                csp += 1;
                over(csp, bounds.nonces, rule::TOO_MANY_CSP_NONCES)
            }
            field::D2D_NONCES => {
                d2d += 1;
                over(d2d, bounds.nonces, rule::TOO_MANY_D2D_NONCES)
            }
            _ => Ok(()),
        })
    })
}

/// A contact as the phone sent it (`d2d_sync.Contact`, its ID checked).
pub(crate) struct Contact {
    pub(crate) sync: d2d_sync::Contact,
    /// When the conversation was last updated (ms); `None`: no conversation.
    pub(crate) last_update_at: Option<u64>,
}

/// A group as the phone sent it (`d2d_sync.Group`), with its key.
pub(crate) struct Group {
    /// The creator's Threema ID (a contact or the own ID).
    pub(crate) creator: String,
    /// `common.GroupIdentity.group_id` (a `fixed64`).
    pub(crate) group_id: u64,
    pub(crate) sync: d2d_sync::Group,
    pub(crate) last_update_at: u64,
}

/// A distribution list as the phone sent it (`d2d_sync.DistributionList`), with its id.
pub(crate) struct List {
    /// `distribution_list_id` (a `fixed64`).
    pub(crate) id: u64,
    pub(crate) sync: d2d_sync::DistributionList,
    pub(crate) last_update_at: u64,
}

/// The data of a join that passed every check: what the import writes (J4b). MEMORY only until then;
/// the keys are in `Zeroizing`. `Debug` prints counts only.
///
/// `pub` (and `store::Import`, doc-hidden) only so the crate's integration tests can hand one to
/// `store::import_join`; its fields and `Import::check` stay crate-private, and the tests make one
/// only through [`Import::checked_for_tests`].
pub struct Import {
    /// The own Threema ID (8 characters; personal data).
    pub(crate) threema_id: String,
    pub(crate) client_key: Zeroizing<[u8; 32]>,
    pub(crate) server_group: String,
    pub(crate) device_group_key: Zeroizing<[u8; 32]>,
    pub(crate) device_cookie: Zeroizing<[u8; 16]>,
    /// Drawn here: random, never 0.
    pub(crate) d2m_device_id: u64,
    /// Drawn here: random, never 0.
    pub(crate) csp_device_id: u64,
    pub(crate) profile: d2d_sync::UserProfile,
    pub(crate) settings: d2d_sync::Settings,
    /// In the phone's order; no ID twice, none the own ID.
    pub(crate) contacts: Vec<Contact>,
    /// In the phone's order; no key twice.
    pub(crate) groups: Vec<Group>,
    /// In the phone's order; no id twice.
    pub(crate) lists: Vec<List>,
    /// The referenced blobs only, each id once (16 bytes), in the order of first reference.
    pub(crate) blobs: Vec<common::BlobData>,
    /// Each once, in the phone's order.
    pub(crate) csp_nonces: Vec<[u8; 32]>,
    /// Each once, in the phone's order.
    pub(crate) d2d_nonces: Vec<[u8; 32]>,
}

impl std::fmt::Debug for Import {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Import")
            .field("contacts", &self.contacts.len())
            .field("groups", &self.groups.len())
            .field("lists", &self.lists.len())
            .field("blobs", &self.blobs.len())
            .field("csp_nonces", &self.csp_nonces.len())
            .field("d2d_nonces", &self.d2d_nonces.len())
            .finish_non_exhaustive()
    }
}

impl Import {
    /// The checks (module doc) on what the phone handed over, the device ids drawn from `rng`. A
    /// refusal is logged once, by its rule only.
    pub(crate) fn check(
        mut received: Box<Received>,
        bounds: Bounds,
        rng: &mut impl RngCore,
    ) -> Result<Self, Refused> {
        let checked = checked(&mut received, bounds, rng);
        if let Err(Refused(rule)) = checked {
            tracing::warn!(rule, "the phone's data of the join was refused");
        }
        checked
    }

    /// For the crate's integration tests only (the store's and the kill loop's): `essential` (an encoded
    /// `EssentialData`, e.g. the fake's builder's) and `blobs` (each `(id, data)` of a `BlobData`, in the
    /// order they came) through `Import::check` with the real bounds, the device ids drawn from a
    /// `StdRng` seeded with `seed` (the same input and seed: the same `Import`). A refusal is the rule's
    /// name. Nothing in the application calls it.
    #[doc(hidden)]
    pub fn checked_for_tests(
        essential: &[u8],
        blobs: Vec<(Vec<u8>, Vec<u8>)>,
        seed: u64,
    ) -> Result<Self, &'static str> {
        let data = d2d_join::EssentialData::decode(essential)
            .map_err(|_| "the EssentialData does not decode")?;
        let blobs = blobs
            .into_iter()
            .map(|(id, data)| common::BlobData { id, data })
            .collect();
        let received = Box::new(Received { data, blobs });
        Self::check(received, Bounds::REAL, &mut StdRng::seed_from_u64(seed))
            .map_err(|Refused(rule)| rule)
    }

    /// The identity row of this device as the import writes it: slot `new` (the mediator has not seen
    /// the device yet), the label and the time of the link.
    pub(crate) fn identity(&self, device_label: String, linked_at_ms: i64) -> Identity {
        Identity {
            threema_id: self.threema_id.clone(),
            client_key: self.client_key.clone(),
            server_group: self.server_group.clone(),
            device_group_key: self.device_group_key.clone(),
            device_cookie: self.device_cookie.clone(),
            d2m_device_id: self.d2m_device_id,
            csp_device_id: self.csp_device_id,
            slot_state: SlotState::New,
            device_label,
            linked_at_ms,
        }
    }
}

/// A fixed-size secret, copied into memory that is wiped on drop.
fn fixed<const N: usize>(bytes: &[u8], rule: &'static str) -> Result<Zeroizing<[u8; N]>, Refused> {
    if bytes.len() != N {
        return Err(Refused(rule));
    }
    let mut out = Zeroizing::new([0_u8; N]);
    out.copy_from_slice(bytes);
    Ok(out)
}

/// A time (ms) SQLite's INTEGER cannot hold as the same number (E4).
fn past_i64(ms: u64) -> bool {
    i64::try_from(ms).is_err()
}

fn is_threema_id(id: &str) -> bool {
    ThreemaId::try_from(id).is_ok()
}

/// A random device id that is not 0.
fn device_id(rng: &mut impl RngCore) -> u64 {
    loop {
        let id = rng.next_u64();
        if id != 0 {
            return id;
        }
    }
}

/// The blob id a picture names (`None`: no picture, or "removed").
fn picture_blob(
    picture: Option<&common::DeltaImage>,
) -> Result<Option<[u8; BLOB_ID_LEN]>, Refused> {
    match picture.and_then(|p| p.image.as_ref()) {
        None | Some(delta_image::Image::Removed(_)) => Ok(None),
        Some(delta_image::Image::Updated(image)) => {
            let blob = image
                .blob
                .as_ref()
                .ok_or(Refused(rule::PICTURE_WITHOUT_BLOB))?;
            let id = <[u8; BLOB_ID_LEN]>::try_from(blob.id.as_slice())
                .map_err(|_| Refused(rule::BLOB_ID))?;
            Ok(Some(id))
        }
    }
}

/// The hashed nonces of one store, each once, in order; over `bound` distinct ones -> `too_many`.
/// Returns them and how many repeats were dropped.
fn nonces(
    raw: Vec<Vec<u8>>,
    bound: usize,
    too_many: &'static str,
) -> Result<(Vec<[u8; 32]>, usize), Refused> {
    let mut seen = HashSet::with_capacity(raw.len().min(bound));
    let mut out = Vec::with_capacity(raw.len().min(bound));
    let mut repeats = 0;
    for hash in raw {
        let hash = <[u8; 32]>::try_from(hash.as_slice()).map_err(|_| Refused(rule::NONCE))?;
        if seen.insert(hash) {
            if out.len() == bound {
                return Err(Refused(too_many));
            }
            out.push(hash);
        } else {
            repeats += 1;
        }
    }
    Ok((out, repeats))
}

/// The body of `Import::check` (a test calls it on a `&mut` to see what is left in the message).
fn checked(
    received: &mut Received,
    bounds: Bounds,
    rng: &mut impl RngCore,
) -> Result<Import, Refused> {
    let data = &mut received.data;
    // The keys leave the message first, into memory that is wiped on drop (`Received`'s own drop
    // wipes what is left).
    let ck = data
        .identity_data
        .as_mut()
        .map(|i| Zeroizing::new(mem::take(&mut i.ck)));
    let cookie = data
        .identity_data
        .as_mut()
        .map(|i| Zeroizing::new(mem::take(&mut i.csp_device_cookie)));
    let dgk = data
        .device_group_data
        .as_mut()
        .map(|g| Zeroizing::new(mem::take(&mut g.dgk)));

    // The own identity and the device group. The identity data STAYS in the message with its keys
    // already out (S9: the test sees CK and the cookie gone, so a dropped `mem::take` above fails it).
    let identity = data
        .identity_data
        .as_mut()
        .ok_or(Refused(rule::NO_IDENTITY_DATA))?;
    let own = mem::take(&mut identity.identity);
    let server_group = mem::take(&mut identity.csp_server_group);
    if !is_threema_id(&own) {
        return Err(Refused(rule::OWN_ID));
    }
    let client_key = fixed::<32>(
        ck.as_deref().map_or(&[][..], Vec::as_slice),
        rule::CLIENT_KEY,
    )?;
    let device_cookie = fixed::<16>(
        cookie.as_deref().map_or(&[][..], Vec::as_slice),
        rule::DEVICE_COOKIE,
    )?;
    if ChatServerGroup::try_from(server_group.as_str()).is_err() {
        return Err(Refused(rule::SERVER_GROUP));
    }
    let Some(dgk) = dgk else {
        return Err(Refused(rule::NO_DEVICE_GROUP_DATA));
    };
    let device_group_key = fixed::<32>(&dgk, rule::DEVICE_GROUP_KEY)?;

    // Consumer only (E3).
    if data.work_credentials.is_some() {
        return Err(Refused(rule::WORK_CREDENTIALS));
    }
    if data.mdm_parameters.is_some() {
        return Err(Refused(rule::MDM_PARAMETERS));
    }
    let profile = data
        .user_profile
        .take()
        .ok_or(Refused(rule::NO_USER_PROFILE))?;
    let settings = data.settings.take().ok_or(Refused(rule::NO_SETTINGS))?;

    // The bounds (C7) before any work per item.
    if data.contacts.len() > bounds.contacts {
        return Err(Refused(rule::TOO_MANY_CONTACTS));
    }
    if data.groups.len() > bounds.groups {
        return Err(Refused(rule::TOO_MANY_GROUPS));
    }
    if data.distribution_lists.len() > bounds.lists {
        return Err(Refused(rule::TOO_MANY_LISTS));
    }
    // The members of each group and list (S2) and of ALL of them together (C1); the settings' two ID
    // lists (C2).
    let entries_of = |m: Option<&common::Identities>| m.map_or(0, |m| m.identities.len());
    let sizes = data
        .groups
        .iter()
        .map(|g| entries_of(g.group.as_ref().and_then(|g| g.member_identities.as_ref())))
        .chain(data.distribution_lists.iter().map(|l| {
            entries_of(
                l.distribution_list
                    .as_ref()
                    .and_then(|l| l.member_identities.as_ref()),
            )
        }));
    let mut members_in_all = 0_usize;
    for members in sizes {
        if members > bounds.members {
            return Err(Refused(rule::TOO_MANY_MEMBERS));
        }
        members_in_all = members_in_all.saturating_add(members);
    }
    if members_in_all > bounds.members_total {
        return Err(Refused(rule::TOO_MANY_MEMBERS_IN_ALL));
    }
    if entries_of(settings.blocked_identities.as_ref()) > bounds.identities {
        return Err(Refused(rule::TOO_MANY_BLOCKED));
    }
    if entries_of(settings.exclude_from_sync_identities.as_ref()) > bounds.identities {
        return Err(Refused(rule::TOO_MANY_EXCLUDED));
    }

    // Each contact.
    let mut ids: HashSet<String> = HashSet::with_capacity(data.contacts.len());
    let mut contacts = Vec::with_capacity(data.contacts.len());
    for entry in mem::take(&mut data.contacts) {
        let sync = entry.contact.ok_or(Refused(rule::NO_CONTACT))?;
        if !is_threema_id(&sync.identity) {
            return Err(Refused(rule::CONTACT_ID));
        }
        if sync.public_key.as_ref().map(Vec::len) != Some(32) {
            return Err(Refused(rule::CONTACT_KEY));
        }
        if sync.identity == own {
            return Err(Refused(rule::OWN_ID_AS_CONTACT));
        }
        if !ids.insert(sync.identity.clone()) {
            return Err(Refused(rule::DUPLICATE_CONTACT));
        }
        if entry.last_update_at.is_some_and(past_i64) {
            return Err(Refused(rule::LAST_UPDATE_AT));
        }
        contacts.push(Contact {
            sync,
            last_update_at: entry.last_update_at,
        });
    }

    // Each group (after the contacts: its people must be among them, `join.ts:390-424`).
    let mut keys: HashSet<(String, u64)> = HashSet::with_capacity(data.groups.len());
    let mut groups = Vec::with_capacity(data.groups.len());
    for entry in mem::take(&mut data.groups) {
        let sync = entry.group.ok_or(Refused(rule::NO_GROUP))?;
        let key = sync
            .group_identity
            .as_ref()
            .ok_or(Refused(rule::NO_GROUP_IDENTITY))?;
        let (creator, group_id) = (key.creator_identity.clone(), key.group_id);
        if creator != own && !ids.contains(&creator) {
            return Err(Refused(rule::CREATOR_UNKNOWN));
        }
        let members = sync
            .member_identities
            .as_ref()
            .map_or(&[][..], |m| m.identities.as_slice());
        for member in members {
            if *member == own {
                return Err(Refused(rule::OWN_ID_AS_MEMBER));
            }
            if !ids.contains(member) {
                return Err(Refused(rule::MEMBER_UNKNOWN));
            }
        }
        if !keys.insert((creator.clone(), group_id)) {
            return Err(Refused(rule::DUPLICATE_GROUP));
        }
        if past_i64(entry.last_update_at) {
            return Err(Refused(rule::LAST_UPDATE_AT));
        }
        groups.push(Group {
            creator,
            group_id,
            sync,
            last_update_at: entry.last_update_at,
        });
    }

    // Each distribution list.
    let mut list_ids: HashSet<u64> = HashSet::with_capacity(data.distribution_lists.len());
    let mut lists = Vec::with_capacity(data.distribution_lists.len());
    for entry in mem::take(&mut data.distribution_lists) {
        let sync = entry.distribution_list.ok_or(Refused(rule::NO_LIST))?;
        let id = sync.distribution_list_id;
        if !list_ids.insert(id) {
            return Err(Refused(rule::DUPLICATE_LIST));
        }
        if past_i64(entry.last_update_at) {
            return Err(Refused(rule::LAST_UPDATE_AT));
        }
        lists.push(List {
            id,
            sync,
            last_update_at: entry.last_update_at,
        });
    }

    // The pictures' blobs: each one referenced must have come (C5: the others are dropped). A blob id
    // that came twice: the later bytes (Desktop's map does the same) [I].
    // `referenced` keeps the order of the first reference; `seen` makes each lookup O(1) (S12: up to
    // ~210,000 references, a `Vec::contains` per reference was quadratic on the actor's thread).
    let mut referenced: Vec<[u8; BLOB_ID_LEN]> = Vec::new();
    let mut seen: HashSet<[u8; BLOB_ID_LEN]> = HashSet::new();
    let mut reference = |id: Option<[u8; BLOB_ID_LEN]>| {
        if let Some(id) = id
            && seen.insert(id)
        {
            referenced.push(id);
        }
    };
    reference(picture_blob(profile.profile_picture.as_ref())?);
    for contact in &contacts {
        reference(picture_blob(
            contact.sync.contact_defined_profile_picture.as_ref(),
        )?);
        reference(picture_blob(
            contact.sync.user_defined_profile_picture.as_ref(),
        )?);
    }
    for group in &groups {
        reference(picture_blob(group.sync.profile_picture.as_ref())?);
    }
    let mut came: HashMap<Vec<u8>, Vec<u8>> = mem::take(&mut received.blobs)
        .into_iter()
        .map(|b| (b.id, b.data))
        .collect();
    let mut blobs = Vec::with_capacity(referenced.len());
    for id in referenced {
        let data = came
            .remove(id.as_slice())
            .ok_or(Refused(rule::MISSING_BLOB))?;
        blobs.push(common::BlobData {
            id: id.to_vec(),
            data,
        });
    }
    if !came.is_empty() {
        tracing::debug!(
            dropped = came.len(),
            "blobs of the join that nothing references are dropped"
        );
    }

    // The hashed nonces, each once.
    let data = &mut received.data;
    let (csp_nonces, csp_repeats) = nonces(
        mem::take(&mut data.csp_hashed_nonces),
        bounds.nonces,
        rule::TOO_MANY_CSP_NONCES,
    )?;
    let (d2d_nonces, d2d_repeats) = nonces(
        mem::take(&mut data.d2d_hashed_nonces),
        bounds.nonces,
        rule::TOO_MANY_D2D_NONCES,
    )?;
    if csp_repeats + d2d_repeats > 0 {
        tracing::warn!(
            csp = csp_repeats,
            d2d = d2d_repeats,
            "the phone sent hashed nonces more than once; each is kept once"
        );
    }

    // Step 4 of the spec: the device ids.
    let d2m_device_id = device_id(rng);
    let csp_device_id = device_id(rng);
    Ok(Import {
        threema_id: own,
        client_key,
        server_group,
        device_group_key,
        device_cookie,
        d2m_device_id,
        csp_device_id,
        profile,
        settings,
        contacts,
        groups,
        lists,
        blobs,
        csp_nonces,
        d2d_nonces,
    })
}

#[cfg(test)]
mod tests {
    use libthreema::protobuf::d2d_join::EssentialData;
    use libthreema::protobuf::d2d_join::essential_data::{
        AugmentedContact, AugmentedDistributionList, AugmentedGroup, DeviceGroupData, IdentityData,
    };

    use super::*;

    /// Synthetic IDs (8 characters libthreema accepts), each a substring of no other test value.
    const OWN: &str = "J4AOWN01";
    const C0: &str = "J4AC0000";
    const C1: &str = "J4AC0001";
    const STRANGER: &str = "J4ASTRNG";

    fn ck() -> Vec<u8> {
        (0..32).map(|i| 0xc0 ^ i).collect()
    }
    fn cookie() -> Vec<u8> {
        (0..16).map(|i| 0x60 ^ i).collect()
    }
    fn dgk() -> Vec<u8> {
        (0..32).map(|i| 0x90 ^ i).collect()
    }

    fn contact(id: &str) -> AugmentedContact {
        AugmentedContact {
            contact: Some(d2d_sync::Contact {
                identity: id.to_owned(),
                public_key: Some(vec![0x11; 32]),
                ..Default::default()
            }),
            last_update_at: Some(5),
        }
    }

    fn group(creator: &str, group_id: u64, members: &[&str]) -> AugmentedGroup {
        AugmentedGroup {
            group: Some(d2d_sync::Group {
                group_identity: Some(common::GroupIdentity {
                    group_id,
                    creator_identity: creator.to_owned(),
                }),
                member_identities: Some(common::Identities {
                    identities: members.iter().map(|m| (*m).to_owned()).collect(),
                }),
                ..Default::default()
            }),
            last_update_at: 7,
        }
    }

    fn list(id: u64) -> AugmentedDistributionList {
        AugmentedDistributionList {
            distribution_list: Some(d2d_sync::DistributionList {
                distribution_list_id: id,
                ..Default::default()
            }),
            last_update_at: 9,
        }
    }

    fn picture(blob_id: Vec<u8>) -> common::DeltaImage {
        common::DeltaImage {
            image: Some(delta_image::Image::Updated(common::Image {
                r#type: 0,
                blob: Some(common::Blob {
                    id: blob_id,
                    ..Default::default()
                }),
            })),
        }
    }

    /// Data that passes: two contacts, two groups (one the own, one a contact's), a list, a profile
    /// picture whose blob came, two nonces of each store.
    fn good() -> Received {
        Received {
            data: EssentialData {
                identity_data: Some(IdentityData {
                    identity: OWN.to_owned(),
                    ck: ck(),
                    csp_device_cookie: cookie(),
                    csp_server_group: "ds7".to_owned(),
                }),
                work_credentials: None,
                device_group_data: Some(DeviceGroupData { dgk: dgk() }),
                user_profile: Some(d2d_sync::UserProfile {
                    profile_picture: Some(picture(vec![0xb1; 16])),
                    ..Default::default()
                }),
                settings: Some(d2d_sync::Settings::default()),
                mdm_parameters: None,
                contacts: vec![contact(C0), contact(C1)],
                groups: vec![group(OWN, u64::MAX, &[C0, C1]), group(C0, 1, &[C1])],
                distribution_lists: vec![list(1 << 63)],
                csp_hashed_nonces: vec![vec![1; 32], vec![2; 32]],
                d2d_hashed_nonces: vec![vec![3; 32], vec![4; 32]],
            },
            blobs: vec![common::BlobData {
                id: vec![0xb1; 16],
                data: vec![0xd1; 40],
            }],
        }
    }

    fn rng() -> StdRng {
        StdRng::seed_from_u64(0x004a_3461)
    }

    fn run(received: Received, bounds: Bounds) -> Result<Import, Refused> {
        Import::check(Box::new(received), bounds, &mut rng())
    }

    fn refused(received: Received) -> Option<&'static str> {
        run(received, Bounds::REAL).err().map(|Refused(rule)| rule)
    }

    #[test]
    fn good_data_passes_whole() {
        let import = run(good(), Bounds::REAL).expect("good data");
        assert!(import.threema_id == OWN, "the own ID");
        assert!(*import.client_key == ck()[..], "the client key");
        assert!(*import.device_cookie == cookie()[..], "the device cookie");
        assert!(
            *import.device_group_key == dgk()[..],
            "the device group key"
        );
        assert_eq!(import.server_group, "ds7");
        assert_eq!(
            (
                import.contacts.len(),
                import.groups.len(),
                import.lists.len()
            ),
            (2, 2, 1)
        );
        assert_eq!(import.contacts[1].last_update_at, Some(5));
        assert_eq!(
            (import.groups[0].group_id, import.groups[1].group_id),
            (u64::MAX, 1)
        );
        assert!(
            import.groups[0].creator == OWN && import.groups[1].creator == C0,
            "the group keys"
        );
        assert_eq!(import.groups[0].last_update_at, 7);
        assert_eq!(
            (import.lists[0].id, import.lists[0].last_update_at),
            (1 << 63, 9)
        );
        assert_eq!(import.lists[0].sync.distribution_list_id, 1 << 63);
        assert!(import.profile.profile_picture.is_some(), "the profile");
        assert_eq!(import.settings, d2d_sync::Settings::default());
        assert!(import.contacts[0].sync.identity == C0, "the first contact");
        assert_eq!(
            import.blobs,
            vec![common::BlobData {
                id: vec![0xb1; 16],
                data: vec![0xd1; 40]
            }]
        );
        assert_eq!(import.csp_nonces, vec![[1; 32], [2; 32]]);
        assert_eq!(import.d2d_nonces, vec![[3; 32], [4; 32]]);
        // The identity row of the import: slot new, the label and the time as given.
        let row = import.identity("Kourier (j4a-host)".to_owned(), 77);
        assert!(
            row.threema_id == OWN
                && row.client_key == import.client_key
                && row.device_group_key == import.device_group_key
                && row.device_cookie == import.device_cookie
                && row.server_group == "ds7"
                && row.d2m_device_id == import.d2m_device_id
                && row.csp_device_id == import.csp_device_id
                && row.slot_state == SlotState::New
                && row.device_label == "Kourier (j4a-host)"
                && row.linked_at_ms == 77,
            "the identity row is not the import's with slot new"
        );
    }

    /// What makes good data bad, and the rule that must refuse it.
    type Spoil = fn(&mut Received);

    /// One past the last time an i64 holds (E4).
    const PAST_I64: u64 = i64::MAX as u64 + 1;

    fn identity(r: &mut Received) -> &mut IdentityData {
        r.data.identity_data.as_mut().expect("identity data")
    }

    fn first_group(r: &mut Received) -> &mut d2d_sync::Group {
        r.data.groups[0].group.as_mut().expect("a group")
    }

    fn cases() -> Vec<(Spoil, &'static str)> {
        vec![
            (|r| r.data.identity_data = None, rule::NO_IDENTITY_DATA),
            (|r| identity(r).identity.push('Q'), rule::OWN_ID),
            (
                |r| {
                    identity(r).identity.pop();
                },
                rule::OWN_ID,
            ),
            (
                |r| identity(r).identity = "j4aown01".to_owned(),
                rule::OWN_ID,
            ),
            (|r| identity(r).ck.push(0), rule::CLIENT_KEY),
            (
                |r| {
                    identity(r).ck.pop();
                },
                rule::CLIENT_KEY,
            ),
            (
                |r| identity(r).csp_device_cookie.push(0),
                rule::DEVICE_COOKIE,
            ),
            (
                |r| {
                    identity(r).csp_device_cookie.pop();
                },
                rule::DEVICE_COOKIE,
            ),
            (|r| identity(r).csp_server_group.clear(), rule::SERVER_GROUP),
            (
                |r| identity(r).csp_server_group = "d_s".to_owned(),
                rule::SERVER_GROUP,
            ),
            (
                |r| r.data.device_group_data = None,
                rule::NO_DEVICE_GROUP_DATA,
            ),
            (
                |r| r.data.device_group_data.as_mut().expect("dgk").dgk.push(0),
                rule::DEVICE_GROUP_KEY,
            ),
            (
                |r| {
                    r.data.device_group_data.as_mut().expect("dgk").dgk.pop();
                },
                rule::DEVICE_GROUP_KEY,
            ),
            (
                |r| {
                    r.data.work_credentials = Some(d2d_sync::ThreemaWorkCredentials::default());
                },
                rule::WORK_CREDENTIALS,
            ),
            (
                |r| r.data.mdm_parameters = Some(d2d_sync::MdmParameters::default()),
                rule::MDM_PARAMETERS,
            ),
            (|r| r.data.user_profile = None, rule::NO_USER_PROFILE),
            (|r| r.data.settings = None, rule::NO_SETTINGS),
            (|r| r.data.csp_hashed_nonces.push(vec![5; 31]), rule::NONCE),
            (|r| r.data.d2d_hashed_nonces.push(vec![5; 33]), rule::NONCE),
            (|r| r.data.contacts[0].contact = None, rule::NO_CONTACT),
            (
                |r| r.data.contacts.push(contact("J4AC002")),
                rule::CONTACT_ID,
            ),
            (
                |r| r.data.contacts.push(contact("J4AC00002")),
                rule::CONTACT_ID,
            ),
            (
                |r| r.data.contacts.push(contact("j4ac0002")),
                rule::CONTACT_ID,
            ),
            (
                |r| {
                    r.data.contacts[0].contact.as_mut().expect("c").public_key = None;
                },
                rule::CONTACT_KEY,
            ),
            (
                |r| {
                    r.data.contacts[0].contact.as_mut().expect("c").public_key = Some(vec![1; 31]);
                },
                rule::CONTACT_KEY,
            ),
            (
                |r| {
                    r.data.contacts[0].contact.as_mut().expect("c").public_key = Some(vec![1; 33]);
                },
                rule::CONTACT_KEY,
            ),
            (
                |r| r.data.contacts.push(contact(C1)),
                rule::DUPLICATE_CONTACT,
            ),
            (
                |r| r.data.contacts.push(contact(OWN)),
                rule::OWN_ID_AS_CONTACT,
            ),
            (|r| r.data.groups[0].group = None, rule::NO_GROUP),
            (
                |r| first_group(r).group_identity = None,
                rule::NO_GROUP_IDENTITY,
            ),
            (
                |r| r.data.groups.push(group(STRANGER, 2, &[C0])),
                rule::CREATOR_UNKNOWN,
            ),
            (
                |r| r.data.groups.push(group(OWN, 2, &[C0, STRANGER])),
                rule::MEMBER_UNKNOWN,
            ),
            (
                |r| r.data.groups.push(group(C0, 2, &[C1, OWN])),
                rule::OWN_ID_AS_MEMBER,
            ),
            (
                |r| r.data.groups.push(group(C0, 1, &[])),
                rule::DUPLICATE_GROUP,
            ),
            (
                |r| r.data.distribution_lists[0].distribution_list = None,
                rule::NO_LIST,
            ),
            (
                |r| r.data.distribution_lists.push(list(1 << 63)),
                rule::DUPLICATE_LIST,
            ),
            (
                |r| {
                    let picture = r.data.user_profile.as_mut().expect("profile");
                    picture.profile_picture = Some(common::DeltaImage {
                        image: Some(delta_image::Image::Updated(common::Image::default())),
                    });
                },
                rule::PICTURE_WITHOUT_BLOB,
            ),
            (
                |r| {
                    let c = r.data.contacts[0].contact.as_mut().expect("c");
                    c.contact_defined_profile_picture = Some(picture(vec![0xb2; 15]));
                },
                rule::BLOB_ID,
            ),
            (
                |r| {
                    let c = r.data.contacts[1].contact.as_mut().expect("c");
                    c.user_defined_profile_picture = Some(picture(vec![0xb2; 17]));
                },
                rule::BLOB_ID,
            ),
            (
                |r| first_group(r).profile_picture = Some(picture(vec![0xb3; 16])),
                rule::MISSING_BLOB,
            ),
            (|r| r.blobs.clear(), rule::MISSING_BLOB),
            (
                |r| r.data.contacts[1].last_update_at = Some(PAST_I64),
                rule::LAST_UPDATE_AT,
            ),
            (
                |r| r.data.groups[1].last_update_at = PAST_I64,
                rule::LAST_UPDATE_AT,
            ),
            (
                |r| r.data.distribution_lists[0].last_update_at = PAST_I64,
                rule::LAST_UPDATE_AT,
            ),
        ]
    }

    /// Each bad field and each item rule of H5 refuses the join with ITS rule.
    #[test]
    fn each_bad_field_and_item_is_refused_by_its_own_rule() {
        for (n, (spoil, rule)) in cases().into_iter().enumerate() {
            let mut bad = good();
            spoil(&mut bad);
            let got = refused(bad);
            assert!(
                got == Some(rule),
                "case {n}: expected the rule \"{rule}\", got {got:?}"
            );
        }
        // Every rule but the bounds has a case.
        let named: HashSet<&str> = cases().iter().map(|(_, rule)| *rule).collect();
        assert_eq!(named.len(), 29, "the item and field rules with a case");
    }

    /// Every rule has a name of its own.
    #[test]
    fn the_rule_names_are_distinct() {
        let all = [
            rule::NO_IDENTITY_DATA,
            rule::OWN_ID,
            rule::CLIENT_KEY,
            rule::DEVICE_COOKIE,
            rule::SERVER_GROUP,
            rule::NO_DEVICE_GROUP_DATA,
            rule::DEVICE_GROUP_KEY,
            rule::WORK_CREDENTIALS,
            rule::MDM_PARAMETERS,
            rule::NO_USER_PROFILE,
            rule::NO_SETTINGS,
            rule::TOO_MANY_CONTACTS,
            rule::TOO_MANY_GROUPS,
            rule::TOO_MANY_LISTS,
            rule::TOO_MANY_MEMBERS,
            rule::TOO_MANY_MEMBERS_IN_ALL,
            rule::TOO_MANY_BLOCKED,
            rule::TOO_MANY_EXCLUDED,
            rule::TOO_MANY_CSP_NONCES,
            rule::TOO_MANY_D2D_NONCES,
            rule::PROFILE_TOO_LARGE,
            rule::SETTINGS_TOO_LARGE,
            rule::NONCE,
            rule::NO_CONTACT,
            rule::CONTACT_ID,
            rule::CONTACT_KEY,
            rule::DUPLICATE_CONTACT,
            rule::OWN_ID_AS_CONTACT,
            rule::NO_GROUP,
            rule::NO_GROUP_IDENTITY,
            rule::CREATOR_UNKNOWN,
            rule::MEMBER_UNKNOWN,
            rule::OWN_ID_AS_MEMBER,
            rule::DUPLICATE_GROUP,
            rule::NO_LIST,
            rule::DUPLICATE_LIST,
            rule::PICTURE_WITHOUT_BLOB,
            rule::BLOB_ID,
            rule::MISSING_BLOB,
            rule::LAST_UPDATE_AT,
        ];
        assert_eq!(all.iter().collect::<HashSet<_>>().len(), all.len());
    }

    /// The edges that pass: a Gateway ID as a contact and as the own ID, a server group with a dash, a
    /// "removed" picture (it names no blob), a group with no member list, the own ID as creator.
    #[test]
    fn the_good_edges_pass() {
        let mut edge = good();
        edge.data.contacts.push(contact("*J4AGATE"));
        identity(&mut edge).csp_server_group = "some-group-42".to_owned();
        let c = edge.data.contacts[0].contact.as_mut().expect("c");
        c.user_defined_profile_picture = Some(common::DeltaImage {
            image: Some(delta_image::Image::Removed(common::Unit {})),
        });
        first_group(&mut edge).member_identities = None;
        edge.data.groups.push(group("*J4AGATE", 3, &[C0]));
        // E4: i64::MAX itself is a time the store holds.
        let max = i64::MAX as u64;
        edge.data.contacts[1].last_update_at = Some(max);
        edge.data.groups[1].last_update_at = max;
        edge.data.distribution_lists[0].last_update_at = max;
        let import = run(edge, Bounds::REAL).expect("the edges pass");
        assert_eq!((import.contacts.len(), import.groups.len()), (3, 3));
        assert_eq!(
            (
                import.contacts[1].last_update_at,
                import.groups[1].last_update_at,
                import.lists[0].last_update_at
            ),
            (Some(max), max, max),
            "i64::MAX passes (E4)"
        );
        assert_eq!(
            import.blobs.len(),
            1,
            "a removed picture references no blob"
        );

        let mut gateway = good();
        identity(&mut gateway).identity = "*J4AOWNG".to_owned();
        // The first group's creator was the old own ID.
        gateway.data.groups.remove(0);
        assert!(
            run(gateway, Bounds::REAL).is_ok(),
            "a Gateway ID as the own ID"
        );
    }

    #[test]
    fn work_credentials_refuse_the_join_unsupported() {
        let mut work = good();
        work.data.work_credentials = Some(d2d_sync::ThreemaWorkCredentials {
            username: "j4a work user".to_owned(),
            password: "j4a work password".to_owned(),
        });
        let error = run(work, Bounds::REAL).expect_err("work").error();
        assert_eq!(error.kind, kourier_model::ErrorKind::Unsupported);
        assert_eq!(error.detail, "Threema Work is not supported");
    }

    /// C5: a referenced blob must have come; one nothing references is dropped; a blob id that came
    /// twice keeps the later bytes; a blob referenced twice is kept once.
    #[test]
    fn a_missing_referenced_blob_refuses_and_an_unreferenced_one_is_dropped() {
        let mut missing = good();
        missing.blobs[0].id = vec![0xb9; 16];
        assert_eq!(refused(missing), Some(rule::MISSING_BLOB));

        let mut extra = good();
        extra.blobs.push(common::BlobData {
            id: vec![0xe1; 16],
            data: vec![1; 8],
        });
        extra.blobs.push(common::BlobData {
            id: vec![0xe2; 3],
            data: vec![2; 8],
        });
        extra.blobs.push(common::BlobData {
            id: vec![0xb1; 16],
            data: vec![0xd2; 4],
        });
        let c = extra.data.contacts[0].contact.as_mut().expect("c");
        c.contact_defined_profile_picture = Some(picture(vec![0xb1; 16]));
        let import = run(extra, Bounds::REAL).expect("an unreferenced blob is no error");
        assert_eq!(
            import.blobs,
            vec![common::BlobData {
                id: vec![0xb1; 16],
                data: vec![0xd2; 4]
            }],
            "only the referenced blob, once, with the later bytes"
        );
    }

    #[test]
    fn repeated_hashed_nonces_are_kept_once() {
        let mut repeats = good();
        repeats.data.csp_hashed_nonces = vec![vec![1; 32], vec![2; 32], vec![1; 32], vec![1; 32]];
        repeats.data.d2d_hashed_nonces = vec![vec![3; 32], vec![3; 32]];
        let import = run(repeats, Bounds::REAL).expect("repeats are no error");
        assert_eq!(import.csp_nonces, vec![[1; 32], [2; 32]]);
        assert_eq!(import.d2d_nonces, vec![[3; 32]]);
    }

    /// Small test bounds (the parameter; the real ones are pinned below and one test runs at them).
    const SMALL: Bounds = Bounds {
        contacts: 3,
        groups: 2,
        lists: 2,
        members: 3,
        members_total: 6,
        identities: 3,
        nonces: 4,
    };

    fn contact_id(i: usize) -> String {
        format!("J4B{i:05}")
    }

    fn with_contacts(n: usize) -> Received {
        let mut r = good();
        r.data.groups.clear();
        r.data.contacts = (0..n).map(|i| contact(&contact_id(i))).collect();
        r
    }

    fn with_groups(n: usize) -> Received {
        let mut r = good();
        r.data.groups = (0..n).map(|g| group(OWN, g as u64, &[])).collect();
        r
    }

    fn with_nonces(n: usize, csp: bool) -> Received {
        let mut r = good();
        let hashes: Vec<Vec<u8>> = (0..n)
            .map(|i| {
                let mut h = vec![0; 32];
                h[..8].copy_from_slice(&(i as u64).to_le_bytes());
                h
            })
            .collect();
        if csp {
            r.data.csp_hashed_nonces = hashes;
        } else {
            r.data.d2d_hashed_nonces = hashes;
        }
        r
    }

    #[test]
    fn each_bound_passes_at_n_and_refuses_at_n_plus_one() {
        assert!(run(with_contacts(3), SMALL).is_ok(), "3 contacts");
        assert_eq!(
            run(with_contacts(4), SMALL).err(),
            Some(Refused(rule::TOO_MANY_CONTACTS))
        );
        assert!(run(with_groups(2), SMALL).is_ok(), "2 groups");
        assert_eq!(
            run(with_groups(3), SMALL).err(),
            Some(Refused(rule::TOO_MANY_GROUPS))
        );
        for (csp, too_many) in [
            (true, rule::TOO_MANY_CSP_NONCES),
            (false, rule::TOO_MANY_D2D_NONCES),
        ] {
            assert!(run(with_nonces(4, csp), SMALL).is_ok(), "4 nonces");
            assert_eq!(
                run(with_nonces(5, csp), SMALL).err(),
                Some(Refused(too_many))
            );
            // The bound counts each nonce once: 4 distinct and their repeats pass.
            let mut repeated = with_nonces(4, csp);
            let list = if csp {
                &mut repeated.data.csp_hashed_nonces
            } else {
                &mut repeated.data.d2d_hashed_nonces
            };
            list.extend(list.clone());
            assert!(
                run(repeated, SMALL).is_ok(),
                "4 distinct nonces, each twice"
            );
        }
    }

    /// C7's numbers.
    #[test]
    fn the_real_bounds_are_c7s() {
        assert_eq!(
            Bounds::REAL,
            Bounds {
                contacts: 100_000,
                groups: 10_000,
                lists: 10_000,
                members: 10_000,
                members_total: 1_000_000,
                identities: 100_000,
                nonces: 1_000_000
            }
        );
    }

    /// The real bounds at N and N + 1 (contacts and groups; the nonces' loop is the one of the small
    /// test, its real bound only pinned: a million hashes twice is slow at opt-level 0).
    #[test]
    fn the_real_contact_and_group_bounds_hold_at_n_and_n_plus_one() {
        let n = Bounds::REAL.contacts;
        assert!(
            run(with_contacts(n), Bounds::REAL).is_ok(),
            "100,000 contacts"
        );
        assert_eq!(
            run(with_contacts(n + 1), Bounds::REAL).err(),
            Some(Refused(rule::TOO_MANY_CONTACTS))
        );
        let n = Bounds::REAL.groups;
        assert!(run(with_groups(n), Bounds::REAL).is_ok(), "10,000 groups");
        assert_eq!(
            run(with_groups(n + 1), Bounds::REAL).err(),
            Some(Refused(rule::TOO_MANY_GROUPS))
        );
    }

    // S2: THE WIRE COUNT ------------------------------------------------------------------------------

    /// One length-delimited field as it stands on the wire: key, length, payload.
    fn wire(tag: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        prost::encoding::encode_key(tag, prost::encoding::WireType::LengthDelimited, &mut out);
        prost::encoding::encode_varint(payload.len() as u64, &mut out);
        out.extend_from_slice(payload);
        out
    }

    /// An `EdToNd { EssentialData }` frame whose `EssentialData` body is `body`.
    fn frame_of(body: &[u8]) -> Vec<u8> {
        wire(3, body)
    }

    /// `n` empty entries of the repeated field `tag` of `EssentialData` (two bytes each).
    fn empties(tag: u32, n: usize) -> Vec<u8> {
        (0..n).flat_map(|_| wire(tag, &[])).collect()
    }

    /// An `AugmentedGroup` (field 8) or `AugmentedDistributionList` (field 9) entry with `members` member
    /// strings (empty ones: two bytes each), the object at field 1, its `Identities` at `members_field`.
    fn entry_with_members(tag: u32, members_field: u32, members: usize) -> Vec<u8> {
        let identities = empties(1, members);
        let object = wire(members_field, &identities);
        wire(tag, &wire(1, &object))
    }

    fn over(frame: &[u8], bounds: &Bounds) -> Option<&'static str> {
        match wire_check(frame, bounds) {
            Ok(()) => None,
            Err(Wire::Over(rule)) => Some(rule),
            Err(Wire::Malformed) => panic!("a frame the test built is malformed"),
        }
    }

    /// Each wire count passes at N and is refused at N + 1, on the wire form (the small bounds).
    #[test]
    fn the_wire_counts_pass_at_n_and_refuse_at_n_plus_one() {
        let b = &SMALL;
        for (tag, n, rule) in [
            (7, b.contacts, rule::TOO_MANY_CONTACTS),
            (8, b.groups, rule::TOO_MANY_GROUPS),
            (9, b.lists, rule::TOO_MANY_LISTS),
            (10, b.nonces, rule::TOO_MANY_CSP_NONCES),
            (11, b.nonces, rule::TOO_MANY_D2D_NONCES),
        ] {
            assert_eq!(over(&frame_of(&empties(tag, n)), b), None, "{rule}: N");
            assert_eq!(
                over(&frame_of(&empties(tag, n + 1)), b),
                Some(rule),
                "{rule}: N + 1"
            );
        }
        // The members of one group, and of one list; a second group with as many is no sum.
        for (tag, members_field, rule) in [
            (8, 8, rule::TOO_MANY_MEMBERS),
            (9, 6, rule::TOO_MANY_MEMBERS),
        ] {
            let at = entry_with_members(tag, members_field, b.members);
            let past = entry_with_members(tag, members_field, b.members + 1);
            assert_eq!(over(&frame_of(&at), b), None, "members: N");
            let two = [at.clone(), at.clone()].concat();
            assert_eq!(over(&frame_of(&two), b), None, "members: N in each of two");
            assert_eq!(over(&frame_of(&past), b), Some(rule), "members: N + 1");
        }
        // Occurrences of the same fields merge in prost, so they count together.
        let half = wire(1, &wire(8, &empties(1, 2)));
        let both = wire(8, &[half.clone(), half].concat());
        assert_eq!(
            over(&frame_of(&both), b),
            Some(rule::TOO_MANY_MEMBERS),
            "members across two occurrences of the object"
        );
        // Both nonce stores are counted apart.
        let both_stores = [empties(10, b.nonces), empties(11, b.nonces)].concat();
        assert_eq!(over(&frame_of(&both_stores), b), None, "N in each store");
    }

    /// M1: the user profile and the settings are bounded by their wire bytes, summed over the
    /// occurrences (they merge in prost): exactly the bound passes, one byte more is refused.
    #[test]
    fn the_profile_and_the_settings_are_bounded_by_their_bytes() {
        for (tag, max, rule) in [
            (4, MAX_PROFILE_BYTES, rule::PROFILE_TOO_LARGE),
            (5, MAX_SETTINGS_BYTES, rule::SETTINGS_TOO_LARGE),
        ] {
            // One unknown field 99 (skipped by the walk and by the decode) as a body of exactly `len`
            // bytes: its payload grows until key + length + payload is `len`.
            let body_of = |len: usize| {
                let mut n = len.saturating_sub(8);
                while wire(99, &vec![0; n]).len() < len {
                    n += 1;
                }
                let body = wire(99, &vec![0; n]);
                assert_eq!(body.len(), len, "a body of exactly {len} bytes");
                body
            };
            assert_eq!(
                over(&frame_of(&wire(tag, &body_of(max))), &Bounds::REAL),
                None,
                "{rule}: N"
            );
            assert_eq!(
                over(&frame_of(&wire(tag, &body_of(max + 1))), &Bounds::REAL),
                Some(rule),
                "{rule}: N + 1"
            );
            // Two occurrences of half the bound plus one byte each are over it together.
            let half = body_of(max / 2 + 1);
            let two = [wire(tag, &half), wire(tag, &half)].concat();
            assert_eq!(
                over(&frame_of(&two), &Bounds::REAL),
                Some(rule),
                "{rule}: summed"
            );
        }
    }

    /// An `EssentialData` body whose `Settings` (field 5) holds `blocked` entries of
    /// `blocked_identities` (field 9) and `excluded` entries of `exclude_from_sync_identities` (field 10).
    fn settings_with(blocked: usize, excluded: usize) -> Vec<u8> {
        let body = [
            wire(9, &empties(1, blocked)),
            wire(10, &empties(1, excluded)),
        ]
        .concat();
        wire(5, &body)
    }

    /// C2 on the wire form (the small bounds): each of the settings' two ID lists passes at N and is
    /// refused at N + 1, they are counted apart, and occurrences of the settings count together.
    #[test]
    fn the_settings_id_lists_pass_at_n_and_refuse_at_n_plus_one_on_the_wire() {
        let b = &SMALL;
        let n = b.identities;
        assert_eq!(over(&frame_of(&settings_with(n, 0)), b), None, "blocked: N");
        assert_eq!(
            over(&frame_of(&settings_with(0, n)), b),
            None,
            "excluded: N"
        );
        assert_eq!(
            over(&frame_of(&settings_with(n, n)), b),
            None,
            "N in each, counted apart"
        );
        assert_eq!(
            over(&frame_of(&settings_with(n + 1, 0)), b),
            Some(rule::TOO_MANY_BLOCKED),
            "blocked: N + 1"
        );
        assert_eq!(
            over(&frame_of(&settings_with(0, n + 1)), b),
            Some(rule::TOO_MANY_EXCLUDED),
            "excluded: N + 1"
        );
        // Two occurrences of the settings merge in prost: their entries count together.
        let twice = [settings_with(2, 0), settings_with(2, 0)].concat();
        assert_eq!(
            over(&frame_of(&twice), b),
            Some(rule::TOO_MANY_BLOCKED),
            "blocked across two occurrences of the settings"
        );
        // Other fields of the settings are not counted, whatever they hold.
        let other = wire(5, &[wire(1, &[0x01]), wire(8, &empties(1, 50))].concat());
        assert_eq!(over(&frame_of(&other), b), None, "other settings fields");
    }

    /// C1 on the wire form (the small bounds, 6 member entries in all): N in all passes whichever way
    /// they are spread over groups and lists, N + 1 is refused with the rule of the SUM.
    #[test]
    fn the_members_in_all_pass_at_n_and_refuse_at_n_plus_one_on_the_wire() {
        let b = &SMALL;
        let group = |m| entry_with_members(8, 8, m);
        let list = |m| entry_with_members(9, 6, m);
        let at = [group(3), group(3)].concat();
        assert_eq!(over(&frame_of(&at), b), None, "two groups of 3: N in all");
        let at = [group(3), list(3)].concat();
        assert_eq!(
            over(&frame_of(&at), b),
            None,
            "a group and a list of 3: N in all"
        );
        assert_eq!(
            over(&frame_of(&[group(3), group(3), list(1)].concat()), b),
            Some(rule::TOO_MANY_MEMBERS_IN_ALL),
            "N + 1 in all, the last one in a list"
        );
        assert_eq!(
            over(&frame_of(&[group(3), list(3), group(1)].concat()), b),
            Some(rule::TOO_MANY_MEMBERS_IN_ALL),
            "N + 1 in all, the last one in a group"
        );
        // Each one's own bound still comes first (a group of 4 is the per-group rule).
        assert_eq!(
            over(&frame_of(&group(4)), b),
            Some(rule::TOO_MANY_MEMBERS),
            "one group over its own bound"
        );
    }

    /// The REAL bounds on the wire form (C7 and S2): N passes, N + 1 is refused, for every count.
    #[test]
    fn the_real_wire_counts_hold_at_n_and_n_plus_one() {
        let b = &Bounds::REAL;
        for (tag, n, rule) in [
            (7, b.contacts, rule::TOO_MANY_CONTACTS),
            (8, b.groups, rule::TOO_MANY_GROUPS),
            (9, b.lists, rule::TOO_MANY_LISTS),
            (10, b.nonces, rule::TOO_MANY_CSP_NONCES),
            (11, b.nonces, rule::TOO_MANY_D2D_NONCES),
        ] {
            assert_eq!(over(&frame_of(&empties(tag, n)), b), None, "{rule}: N");
            assert_eq!(
                over(&frame_of(&empties(tag, n + 1)), b),
                Some(rule),
                "{rule}: N + 1"
            );
        }
        for (tag, members_field) in [(8, 8), (9, 6)] {
            let at = entry_with_members(tag, members_field, b.members);
            let past = entry_with_members(tag, members_field, b.members + 1);
            assert_eq!(over(&frame_of(&at), b), None, "10,000 members");
            assert_eq!(
                over(&frame_of(&past), b),
                Some(rule::TOO_MANY_MEMBERS),
                "10,001 members"
            );
        }
        // C1: 100 groups of 10,000 members are 1,000,000 in all; one more member anywhere is refused,
        // in a group or in a list.
        let full: Vec<u8> = (0..b.members_total / b.members)
            .flat_map(|_| entry_with_members(8, 8, b.members))
            .collect();
        assert_eq!(over(&frame_of(&full), b), None, "1,000,000 members in all");
        for extra in [entry_with_members(8, 8, 1), entry_with_members(9, 6, 1)] {
            let past = [full.clone(), extra].concat();
            assert_eq!(
                over(&frame_of(&past), b),
                Some(rule::TOO_MANY_MEMBERS_IN_ALL),
                "1,000,001 members in all"
            );
        }
        // C2: 100,000 blocked IDs, and 100,000 excluded from the sync.
        let n = b.identities;
        assert_eq!(
            over(&frame_of(&settings_with(n, n)), b),
            None,
            "100,000 each"
        );
        assert_eq!(
            over(&frame_of(&settings_with(n + 1, 0)), b),
            Some(rule::TOO_MANY_BLOCKED),
            "100,001 blocked"
        );
        assert_eq!(
            over(&frame_of(&settings_with(0, n + 1)), b),
            Some(rule::TOO_MANY_EXCLUDED),
            "100,001 excluded"
        );
    }

    /// The scan and the decode agree: a frame of real prost messages (contacts, groups with members, lists,
    /// nonces) passes the bounds that equal its counts and is refused by each bound one lower.
    #[test]
    fn the_wire_scan_and_the_decode_agree_on_the_counts() {
        use libthreema::protobuf::d2d_join::{EdToNd, ed_to_nd::Content};
        let mut data = mem::take(&mut good().data);
        data.contacts = (0..3).map(|i| contact(&contact_id(i))).collect();
        data.groups = vec![
            group(OWN, 1, &[&contact_id(0), &contact_id(1), &contact_id(2)]),
            group(OWN, 2, &[&contact_id(0)]),
        ];
        data.distribution_lists = vec![list(1), list(2)];
        data.settings = Some(d2d_sync::Settings {
            blocked_identities: Some(common::Identities {
                identities: vec![String::new(); 3],
            }),
            exclude_from_sync_identities: Some(common::Identities {
                identities: vec![String::new(); 2],
            }),
            ..Default::default()
        });
        data.csp_hashed_nonces = vec![vec![1; 32]; 4];
        data.d2d_hashed_nonces = vec![vec![2; 32]; 4];
        let frame = EdToNd {
            content: Some(Content::EssentialData(data)),
        }
        .encode_to_vec();
        assert_eq!(
            wire_check(&frame, &SMALL),
            Ok(()),
            "the counts equal the bounds"
        );
        for (tighter, rule) in [
            (
                Bounds {
                    contacts: 2,
                    ..SMALL
                },
                rule::TOO_MANY_CONTACTS,
            ),
            (Bounds { groups: 1, ..SMALL }, rule::TOO_MANY_GROUPS),
            (Bounds { lists: 1, ..SMALL }, rule::TOO_MANY_LISTS),
            (
                Bounds {
                    members: 2,
                    ..SMALL
                },
                rule::TOO_MANY_MEMBERS,
            ),
            // The groups hold 3 + 1 member entries in all.
            (
                Bounds {
                    members_total: 3,
                    ..SMALL
                },
                rule::TOO_MANY_MEMBERS_IN_ALL,
            ),
            (
                Bounds {
                    identities: 2,
                    ..SMALL
                },
                rule::TOO_MANY_BLOCKED,
            ),
            (Bounds { nonces: 3, ..SMALL }, rule::TOO_MANY_CSP_NONCES),
        ] {
            assert_eq!(over(&frame, &tighter), Some(rule), "{rule}");
        }
        // The excluded list alone (2 entries) over a bound of 1.
        let mut excluded = mem::take(&mut good().data);
        excluded.settings = Some(d2d_sync::Settings {
            exclude_from_sync_identities: Some(common::Identities {
                identities: vec![String::new(); 2],
            }),
            ..Default::default()
        });
        let frame = EdToNd {
            content: Some(Content::EssentialData(excluded)),
        }
        .encode_to_vec();
        let one = Bounds {
            identities: 1,
            ..SMALL
        };
        assert_eq!(over(&frame, &one), Some(rule::TOO_MANY_EXCLUDED));
    }

    /// What is not an `EssentialData` is not counted, and bytes that are no message are `Malformed`.
    #[test]
    fn other_frames_pass_and_broken_bytes_are_malformed() {
        use libthreema::protobuf::d2d_join::{Begin, EdToNd, ed_to_nd::Content};
        let begin = EdToNd {
            content: Some(Content::Begin(Begin {})),
        }
        .encode_to_vec();
        assert_eq!(wire_check(&begin, &SMALL), Ok(()));
        assert_eq!(wire_check(&[], &SMALL), Ok(()));
        for bad in [
            vec![0xff],
            vec![0x1a, 0x05, 0x00],
            frame_of(&[0x3a, 0x7f]),
            vec![
                0x1a, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01,
            ],
        ] {
            assert_eq!(wire_check(&bad, &SMALL), Err(Wire::Malformed));
        }
    }

    /// The post-decode checks of the same bounds (the belt behind the wire count).
    #[test]
    fn lists_and_members_are_bounded_on_the_decoded_data_too() {
        let mut lists = good();
        lists.data.distribution_lists = (0..3).map(list).collect();
        assert_eq!(
            run(lists, SMALL).err(),
            Some(Refused(rule::TOO_MANY_LISTS)),
            "3 lists over a bound of 2"
        );
        let mut members = good();
        members.data.contacts = (0..4).map(|i| contact(&contact_id(i))).collect();
        let ids: Vec<String> = (0..4).map(contact_id).collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        members.data.groups = vec![group(OWN, 1, &refs)];
        let bounds = Bounds {
            contacts: 4,
            members: 3,
            ..SMALL
        };
        assert_eq!(
            run(members, bounds).err(),
            Some(Refused(rule::TOO_MANY_MEMBERS)),
            "4 members over a bound of 3"
        );
        let mut one = good();
        one.data.distribution_lists = vec![list(1)];
        if let Some(l) = one.data.distribution_lists[0].distribution_list.as_mut() {
            l.member_identities = Some(common::Identities {
                identities: (0..4).map(contact_id).collect(),
            });
        }
        assert_eq!(
            run(one, SMALL).err(),
            Some(Refused(rule::TOO_MANY_MEMBERS)),
            "4 list members over a bound of 3"
        );
    }

    /// C1 and C2 on the DECODED data (the belt behind the wire count; the small bounds).
    #[test]
    fn the_members_in_all_and_the_settings_lists_are_bounded_on_the_decoded_data_too() {
        let ids: Vec<String> = (0..3).map(contact_id).collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        // Two groups of 3 members are N = 6 in all; a list with one more is N + 1.
        let spread = |list_members: usize| {
            let mut r = good();
            r.data.contacts = (0..3).map(|i| contact(&contact_id(i))).collect();
            r.data.groups = vec![group(OWN, 1, &refs), group(OWN, 2, &refs)];
            r.data.distribution_lists = vec![list(1)];
            if let Some(l) = r.data.distribution_lists[0].distribution_list.as_mut() {
                l.member_identities = Some(common::Identities {
                    identities: vec![String::new(); list_members],
                });
            }
            r
        };
        assert!(run(spread(0), SMALL).is_ok(), "6 members in all");
        assert_eq!(
            run(spread(1), SMALL).err(),
            Some(Refused(rule::TOO_MANY_MEMBERS_IN_ALL)),
            "7 members in all, the 7th in a list"
        );
        let mut groups_only = spread(0);
        groups_only.data.groups.push(group(OWN, 3, &refs[..1]));
        assert_eq!(
            run(groups_only, Bounds { groups: 3, ..SMALL }).err(),
            Some(Refused(rule::TOO_MANY_MEMBERS_IN_ALL)),
            "7 members in all, the 7th in a third group"
        );
        // C2: N passes, N + 1 is refused, each list on its own.
        let with = |blocked: usize, excluded: usize| {
            let mut r = good();
            r.data.settings = Some(d2d_sync::Settings {
                blocked_identities: Some(common::Identities {
                    identities: vec![String::new(); blocked],
                }),
                exclude_from_sync_identities: Some(common::Identities {
                    identities: vec![String::new(); excluded],
                }),
                ..Default::default()
            });
            r
        };
        assert!(run(with(3, 3), SMALL).is_ok(), "3 blocked, 3 excluded");
        assert_eq!(
            run(with(4, 0), SMALL).err(),
            Some(Refused(rule::TOO_MANY_BLOCKED))
        );
        assert_eq!(
            run(with(0, 4), SMALL).err(),
            Some(Refused(rule::TOO_MANY_EXCLUDED))
        );
    }

    /// C1 at the real number on the decoded data: 1,000,001 member entries in 101 groups are refused
    /// with the rule of the sum (1,000,000 pass that rule: the data is then refused for a member who is
    /// no contact, a later rule).
    #[test]
    fn the_real_members_in_all_bound_holds_on_the_decoded_data() {
        let b = Bounds::REAL;
        let many = |extra: usize| {
            let mut r = good();
            let full = || vec!["J4AMEMBR".to_owned(); b.members];
            r.data.groups = (0..b.members_total / b.members)
                .map(|g| group(OWN, g as u64, &[]))
                .collect();
            for g in &mut r.data.groups {
                if let Some(group) = g.group.as_mut() {
                    group.member_identities = Some(common::Identities { identities: full() });
                }
            }
            r.data.groups.push(group(OWN, u64::MAX, &[]));
            if let Some(last) = r.data.groups.last_mut().and_then(|g| g.group.as_mut()) {
                last.member_identities = Some(common::Identities {
                    identities: vec!["J4AMEMBR".to_owned(); extra],
                });
            }
            r
        };
        assert_eq!(
            run(many(1), b).err(),
            Some(Refused(rule::TOO_MANY_MEMBERS_IN_ALL)),
            "1,000,001 members in all"
        );
        assert_eq!(
            run(many(0), b).err(),
            Some(Refused(rule::MEMBER_UNKNOWN)),
            "1,000,000 members in all pass the rule of the sum"
        );
    }

    /// A random source that hands out the given values, then counts up from 100.
    struct Script(Vec<u64>, u64);

    impl RngCore for Script {
        fn next_u32(&mut self) -> u32 {
            self.next_u64() as u32
        }

        fn next_u64(&mut self) -> u64 {
            if self.0.is_empty() {
                self.1 += 1;
                self.1
            } else {
                self.0.remove(0)
            }
        }

        fn fill_bytes(&mut self, dest: &mut [u8]) {
            for chunk in dest.chunks_mut(8) {
                let bytes = self.next_u64().to_le_bytes();
                chunk.copy_from_slice(&bytes[..chunk.len()]);
            }
        }
    }

    #[test]
    fn the_device_ids_are_random_non_zero_and_apart() {
        let mut zeros = Script(vec![0, 0, 5, 0, 7], 100);
        let import = Import::check(Box::new(good()), Bounds::REAL, &mut zeros).expect("good");
        assert_eq!(
            (import.d2m_device_id, import.csp_device_id),
            (5, 7),
            "a drawn 0 is drawn again"
        );
        let mut seeded = rng();
        let a = Import::check(Box::new(good()), Bounds::REAL, &mut seeded).expect("good");
        let b = Import::check(Box::new(good()), Bounds::REAL, &mut seeded).expect("good");
        let ids = [
            a.d2m_device_id,
            a.csp_device_id,
            b.d2m_device_id,
            b.csp_device_id,
        ];
        assert!(ids.iter().all(|id| *id != 0), "a device id is 0");
        assert_eq!(
            ids.iter().collect::<HashSet<_>>().len(),
            4,
            "the device ids are not drawn apart"
        );
    }

    /// The keys leave the message before the first check: a refused message holds none any more.
    #[test]
    fn the_keys_leave_the_message_before_any_check() {
        let mut work = good();
        work.data.work_credentials = Some(d2d_sync::ThreemaWorkCredentials::default());
        let refusal = checked(&mut work, Bounds::REAL, &mut rng()).err();
        assert_eq!(refusal, Some(Refused(rule::WORK_CREDENTIALS)));
        // S9: the identity data stays in the message, so CK and the cookie are SEEN to be gone (taking
        // the whole struct out would hide a key that was never moved).
        assert!(
            work.data
                .identity_data
                .as_ref()
                .is_some_and(|i| i.ck.is_empty() && i.csp_device_cookie.is_empty()),
            "CK or the device cookie stayed in the message"
        );
        assert!(
            work.data
                .device_group_data
                .as_ref()
                .is_some_and(|g| g.dgk.is_empty()),
            "DGK stayed in the message"
        );
    }

    /// S12: ~210,000 picture references (100,000 contacts with two distinct blob ids each, the profile's)
    /// come out in the order of first reference, once each, in linear time: a `Vec::contains` per
    /// reference would need ~2 x 10^10 comparisons. The work runs in a thread so a regression FAILS at
    /// the bound instead of hanging the run.
    #[test]
    fn many_picture_references_are_collected_in_linear_time() {
        let n = Bounds::REAL.contacts;
        let id = |k: usize| -> Vec<u8> {
            let mut id = vec![0u8; BLOB_ID_LEN];
            id[..8].copy_from_slice(&(k as u64).to_le_bytes());
            id
        };
        let mut received = good();
        received.data.groups.clear();
        received.data.contacts = (0..n)
            .map(|i| {
                let mut c = contact(&contact_id(i));
                let sync = c.contact.as_mut().expect("a contact");
                sync.contact_defined_profile_picture = Some(picture(id(2 * i + 1)));
                sync.user_defined_profile_picture = Some(picture(id(2 * i + 2)));
                c
            })
            .collect();
        received.blobs = (0..=2 * n)
            .map(|k| common::BlobData {
                id: id(k),
                data: vec![1],
            })
            .collect();
        received.data.user_profile = Some(d2d_sync::UserProfile {
            profile_picture: Some(picture(id(0))),
            ..Default::default()
        });
        let (done, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(run(received, Bounds::REAL).map(|import| import.blobs));
        });
        let blobs = result
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the references were not collected in time")
            .expect("the data passes");
        assert_eq!(blobs.len(), 2 * n + 1, "not every referenced blob came out");
        let order_kept = blobs
            .iter()
            .enumerate()
            .all(|(k, b)| b.id[..8] == (k as u64).to_le_bytes());
        assert!(
            order_kept,
            "the blobs are not in the order of first reference"
        );
    }

    /// A refusal and the import print no ID, no key, no name.
    #[test]
    fn nothing_prints_an_id_or_a_key() {
        let import = run(good(), Bounds::REAL).expect("good");
        let hex = |b: &[u8]| b.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let printed = format!("{import:?} {import:#?}");
        for needle in [
            OWN.to_owned(),
            C0.to_owned(),
            hex(&ck())[..16].to_owned(),
            hex(&dgk())[..16].to_owned(),
            format!("{:?}", &ck()[..4])
                .trim_matches(['[', ']'])
                .to_owned(),
            import.d2m_device_id.to_string(),
        ] {
            assert!(
                !printed.contains(&needle),
                "Debug of the import prints a test value"
            );
        }
        for (spoil, rule) in cases() {
            let mut bad = good();
            spoil(&mut bad);
            let refusal = run(bad, Bounds::REAL).expect_err("bad");
            let text = format!("{refusal:?} {:?}", refusal.error());
            assert!(
                !text.contains(OWN) && !text.contains(C0) && !text.contains(STRANGER),
                "the refusal by \"{rule}\" names an ID"
            );
        }
    }
}

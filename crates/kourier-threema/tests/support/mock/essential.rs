//! The BUILDER of a join's `EssentialData` (TH4 J2; `TP/md-d2d-join.proto` `EssentialData`): what the
//! scripted existing device ([`super::joiner`]) hands a new device. Its identity, CK, DGK, device
//! cookie and server group are those of a [`DeviceGroup`] the world's mediator and chat server
//! know, so a device that imported it can log in to the fake (J5).
//!
//! Synthetic data only: contact IDs are `C<6 digits>X`, each a substring of no other value the
//! builder writes (names say `First n`, `Last n`, `Group n`), so a test can look for one in a log or
//! a file without a false hit. Per-item knobs ([`Bad`]) make the data a join must REFUSE (C12).
//!
//! Nothing here formats a key or an ID: `Debug` prints counts.

use libthreema::protobuf::{common, d2d_join, d2d_sync};
use prost::Message as _;

use super::crypto::seal;
use super::{MockWorld, TestIdentity};

use d2d_join::essential_data::{AugmentedContact, AugmentedGroup, DeviceGroupData, IdentityData};

/// The server group of a join's device group: letters and digits, the mediator's own rule
/// (`TP/md-d2m.proto:37-39`; TH2's tests use the same).
pub const JOIN_SERVER_GROUP: &str = "ds7";

/// A contact ID of 7 characters (the bad item [`Bad::ShortContactId`]).
pub const SHORT_ID: &str = "SHORT7Z";

/// A member ID that is no contact (the bad item [`Bad::MemberNotAContact`]).
pub const STRANGER_ID: &str = "S000000Y";

/// The synthetic ID of contact `i` (0-based): `C` + six digits + `X`.
pub fn contact_id(i: usize) -> String {
    assert!(i < 1_000_000, "a test's contact count");
    format!("C{i:06}X")
}

/// A device group of the world as a phone holds it: the identity (in the chat server's table), the
/// DGK (its path key known to the mediator), the device cookie and the server group.
#[derive(Clone)]
pub struct DeviceGroup {
    pub identity: TestIdentity,
    pub dgk: [u8; 32],
    pub cookie: [u8; 16],
    pub server_group: String,
}

impl std::fmt::Debug for DeviceGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeviceGroup")
    }
}

impl MockWorld {
    /// A new device group of `id`: the identity is added to the chat server's table, a fresh DGK's
    /// path key to the mediator (server group [`JOIN_SERVER_GROUP`]), and a fresh device cookie.
    #[track_caller]
    pub fn device_group(&self, id: &str) -> DeviceGroup {
        let identity = self.add_identity(id);
        let dgk = self.new_device_group_key();
        let keys = super::crypto::DeviceGroupKeys::derive(&dgk);
        self.mediator()
            .add_group(keys.path_public, JOIN_SERVER_GROUP);
        DeviceGroup {
            identity,
            dgk,
            cookie: self.random(),
            server_group: JOIN_SERVER_GROUP.to_owned(),
        }
    }
}

/// One bad item a later test needs (each one makes the data a join must refuse).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bad {
    /// A contact whose ID has 7 characters ([`SHORT_ID`]).
    ShortContactId,
    /// The first contact twice.
    DuplicateContact,
    /// A group whose member is no contact ([`STRANGER_ID`]); needs `groups >= 1`.
    MemberNotAContact,
    /// The own ID as a contact.
    OwnIdAsContact,
    /// Work credentials (no Consumer build has them).
    WorkCredentials,
}

/// What the builder makes.
#[derive(Clone, Debug, Default)]
pub struct Spec {
    pub contacts: usize,
    pub groups: usize,
    /// How many hashed nonces of EACH kind (CSP and D2D).
    pub hashed_nonces: usize,
    /// A profile picture (one blob, sent as `BlobData` before the `EssentialData`).
    pub profile_picture: bool,
    pub bad: Vec<Bad>,
}

/// What the joiner sends after `Begin`: the referenced blobs, then the data.
#[derive(Clone)]
pub struct Essential {
    pub blobs: Vec<common::BlobData>,
    pub data: d2d_join::EssentialData,
}

impl std::fmt::Debug for Essential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Essential")
            .field("blobs", &self.blobs.len())
            .field("contacts", &self.data.contacts.len())
            .field("groups", &self.data.groups.len())
            .finish()
    }
}

impl Essential {
    /// The encoded `EssentialData`.
    pub fn encoded(&self) -> Vec<u8> {
        self.data.encode_to_vec()
    }
}

fn contact(w: &MockWorld, id: &str, n: usize) -> AugmentedContact {
    let public_key: [u8; 32] = w.random();
    AugmentedContact {
        contact: Some(d2d_sync::Contact {
            identity: id.to_owned(),
            public_key: Some(public_key.to_vec()),
            created_at: Some(super::START_MS - 86_400_000),
            first_name: Some(format!("First {n}")),
            last_name: Some(format!("Last {n}")),
            // The zero values of the spec's enums: UNVERIFIED, NONE, REGULAR, DIRECT, ACTIVE,
            // INITIAL (`TP/md-d2d-sync.proto`, `Contact`).
            verification_level: Some(0),
            work_verification_level: Some(0),
            identity_type: Some(0),
            acquaintance_level: Some(0),
            activity_state: Some(0),
            // [I] the voice-message and group bits (`common.proto` `CspFeatureMaskFlag`).
            feature_mask: Some(0x0003),
            sync_state: Some(0),
            ..Default::default()
        }),
        last_update_at: Some(super::START_MS - 3_600_000),
    }
}

/// The `EssentialData` of `group` as `spec` asks, with the blobs it references.
pub fn essential(w: &MockWorld, group: &DeviceGroup, spec: &Spec) -> Essential {
    let own = group.identity.id.clone();
    let mut contacts: Vec<AugmentedContact> = (0..spec.contacts)
        .map(|i| contact(w, &contact_id(i), i))
        .collect();
    let mut groups: Vec<AugmentedGroup> = (0..spec.groups)
        .map(|g| {
            // Members: the contacts, at most three per group; the creator is the own ID for even
            // groups and the first member for odd ones.
            let members: Vec<String> = (0..spec.contacts.min(3))
                .map(|i| contact_id((g + i) % spec.contacts))
                .collect();
            let creator = if g % 2 == 1 && !members.is_empty() {
                members[0].clone()
            } else {
                own.clone()
            };
            AugmentedGroup {
                group: Some(d2d_sync::Group {
                    group_identity: Some(common::GroupIdentity {
                        group_id: w.random_u64(),
                        creator_identity: creator,
                    }),
                    name: Some(format!("Group {g}")),
                    created_at: Some(super::START_MS - 7_200_000),
                    // MEMBER
                    user_state: Some(0),
                    member_identities: Some(common::Identities {
                        identities: members,
                    }),
                    ..Default::default()
                }),
                last_update_at: super::START_MS - 1_800_000,
            }
        })
        .collect();
    let mut work_credentials = None;
    for bad in &spec.bad {
        match bad {
            Bad::ShortContactId => contacts.push(contact(w, SHORT_ID, spec.contacts)),
            Bad::DuplicateContact => {
                let first = contacts
                    .first()
                    .cloned()
                    .expect("a duplicate needs a contact");
                contacts.push(first);
            }
            Bad::MemberNotAContact => {
                let g = groups
                    .first_mut()
                    .and_then(|g| g.group.as_mut())
                    .and_then(|g| g.member_identities.as_mut())
                    .expect("a stranger needs a group");
                g.identities.push(STRANGER_ID.to_owned());
            }
            Bad::OwnIdAsContact => contacts.push(contact(w, &own, spec.contacts + 1)),
            Bad::WorkCredentials => {
                work_credentials = Some(d2d_sync::ThreemaWorkCredentials {
                    username: "work user".to_owned(),
                    password: "work password".to_owned(),
                });
            }
        }
    }
    let mut blobs = Vec::new();
    let profile_picture = spec.profile_picture.then(|| {
        let (id, key, nonce): ([u8; 16], [u8; 32], [u8; 24]) = (w.random(), w.random(), w.random());
        // [U: real phone] the bytes of a join's `BlobData`: the blob as the blob mirror would
        // hold it (encrypted with its key), as "the usual Blob scheme" reads.
        let picture: Vec<u8> = (0..512u32).map(|i| (i % 251) as u8).collect();
        blobs.push(common::BlobData {
            id: id.to_vec(),
            data: seal(&key, &nonce, &picture),
        });
        common::DeltaImage {
            image: Some(common::delta_image::Image::Updated(common::Image {
                r#type: 0,
                blob: Some(common::Blob {
                    id: id.to_vec(),
                    nonce: nonce.to_vec(),
                    key: key.to_vec(),
                    uploaded_at: super::START_MS - 600_000,
                }),
            })),
        }
    });
    // [I] the hashed nonces are HMAC-SHA256 outputs (32 bytes) ND cannot check; random here.
    let nonces = |n: usize| -> Vec<Vec<u8>> {
        (0..n)
            .map(|_| {
                let h: [u8; 32] = w.random();
                h.to_vec()
            })
            .collect()
    };
    let data = d2d_join::EssentialData {
        identity_data: Some(IdentityData {
            identity: own,
            ck: group.identity.ck.secret.to_vec(),
            csp_device_cookie: group.cookie.to_vec(),
            csp_server_group: group.server_group.clone(),
        }),
        work_credentials,
        device_group_data: Some(DeviceGroupData {
            dgk: group.dgk.to_vec(),
        }),
        user_profile: Some(d2d_sync::UserProfile {
            nickname: Some("Own nickname".to_owned()),
            profile_picture,
            ..Default::default()
        }),
        settings: Some(d2d_sync::Settings::default()),
        mdm_parameters: None,
        contacts,
        groups,
        distribution_lists: Vec::new(),
        csp_hashed_nonces: nonces(spec.hashed_nonces),
        d2d_hashed_nonces: nonces(spec.hashed_nonces),
    };
    Essential { blobs, data }
}

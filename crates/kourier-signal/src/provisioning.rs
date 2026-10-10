//! Device linking crypto (§6.5): the provisioning cipher, the `sgnl://linkdevice` QR URL,
//! `ProvisionMessage` validation and device-name encryption.
//!
//! Ports of `SD/ts/textsecure/ProvisioningCipher.node.ts` (envelope decrypt),
//! `SD/ts/util/signalRoutes.std.ts` (`linkDeviceRoute.toAppUrl`),
//! `SD/ts/Crypto.node.ts:115-161` (`encryptDeviceName`/`decryptDeviceName`) and
//! `SD/ts/util/normalizeDeviceName.std.ts`. The provisioning socket itself lives in `net`.
//!
//! Every error is log-safe: none carries key material, ids, numbers or names (§9.1).

use std::fmt;
use std::str::FromStr as _;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit as _, Mac as _};
use libsignal_account_keys::{AccountEntropyPool, SvrKey};
use libsignal_core::curve::{KeyPair, PrivateKey, PublicKey};
use libsignal_core::{Aci, Pni};
use libsignal_protocol::{IdentityKey, IdentityKeyPair};
use prost::Message as _;
use rand::{CryptoRng, Rng};
use sha2::Sha256;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::proto;
use crate::service_id::validate_e164;

pub(crate) mod link;

/// HKDF info of the provisioning cipher.
const PROVISIONING_INFO: &[u8] = b"TextSecure Provisioning Message";
/// The only `ProvisionEnvelope.body` version.
const PROVISIONING_VERSION: u8 = 1;
const IV_LEN: usize = 16;
const MAC_LEN: usize = 32;
const AES_BLOCK_LEN: usize = 16;
const KEY_LEN: usize = 32;
/// `version ‖ iv ‖ ≥ 1 cipher block ‖ mac`.
const MIN_BODY_LEN: usize = 1 + IV_LEN + AES_BLOCK_LEN + MAC_LEN;

/// Longest device name in UTF-16 code units (`SD/ts/types/InstallScreen.std.ts:44`).
pub(crate) const MAX_DEVICE_NAME_UTF16: usize = 50;
const DEVICE_NAME_SYNTHETIC_IV_LEN: usize = 16;

/// Why a provisioning envelope or message was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ProvisioningError {
    /// The envelope is not a `ProvisionEnvelope` protobuf.
    #[error("provisioning envelope is not a valid protobuf")]
    MalformedEnvelope,
    /// The body's version byte is not 1.
    #[error("unsupported provisioning message version {0}")]
    BadVersion(u8),
    /// The body is too short to hold version, IV, one cipher block and MAC.
    #[error("provisioning message is truncated")]
    Truncated,
    /// The HMAC over version, IV and ciphertext does not match.
    #[error("provisioning message MAC mismatch")]
    BadMac,
    /// AES-CBC decryption or its PKCS#7 padding failed.
    #[error("provisioning message could not be decrypted")]
    BadCiphertext,
    /// The plaintext is not a `ProvisionMessage` protobuf.
    #[error("provision message is not a valid protobuf")]
    MalformedMessage,
    /// A required field is absent or empty.
    #[error("provision message is missing the {0}")]
    Missing(&'static str),
    /// A field is present but malformed or inconsistent.
    #[error("provision message has an invalid {0}")]
    Invalid(&'static str),
    /// The account has no phone number; M1 does not link E164-less accounts (§6.5).
    #[error(
        "this Signal account has no phone number, which Kourier cannot link yet; \
         add a number on the phone or wait for a later Kourier version"
    )]
    NoPhoneNumber,
    /// A relink scanned by a different account than the one being relinked (§4.3 step 6).
    #[error("the phone belongs to a different Signal account than the one being relinked")]
    WrongAccount,
    /// A primitive failed in a way that cannot happen with valid lengths.
    #[error("provisioning crypto failed")]
    Crypto,
}

/// Our half of one provisioning attempt: the X25519 key pair whose public key goes into the QR
/// code. One cipher per provisioning socket (Desktop creates one per rotation).
pub(crate) struct ProvisioningCipher {
    key_pair: KeyPair,
}

impl fmt::Debug for ProvisioningCipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProvisioningCipher([redacted])")
    }
}

impl ProvisioningCipher {
    /// A fresh key pair.
    pub(crate) fn generate<R: Rng + CryptoRng>(rng: &mut R) -> Self {
        Self {
            key_pair: KeyPair::generate(rng),
        }
    }

    /// The public key the phone encrypts to.
    pub(crate) fn public_key(&self) -> &PublicKey {
        &self.key_pair.public_key
    }

    /// The QR code URL for the provisioning `address` the server sent on this socket, with
    /// the `capabilities` this link offers ([`CAPABILITY_HISTORY_TRANSFER`] or none).
    pub(crate) fn link_device_url(&self, address: &str, capabilities: &[&str]) -> String {
        link_device_url(address, self.public_key(), capabilities)
    }

    /// Verifies and decrypts a `ProvisionEnvelope` into the raw `ProvisionMessage` bytes:
    /// version byte, then HMAC-SHA256 over `version ‖ iv ‖ ciphertext`, then AES-256-CBC with
    /// keys from HKDF-SHA256(ECDH, salt 32×0, "TextSecure Provisioning Message").
    pub(crate) fn open(&self, envelope: &[u8]) -> Result<Zeroizing<Vec<u8>>, ProvisioningError> {
        let envelope = proto::ProvisionEnvelope::decode(envelope)
            .map_err(|_| ProvisioningError::MalformedEnvelope)?;
        let their_public = non_empty(envelope.public_key)
            .ok_or(ProvisioningError::Missing("envelope public key"))?;
        let their_public = PublicKey::deserialize(&their_public)
            .map_err(|_| ProvisioningError::Invalid("envelope public key"))?;
        let body = non_empty(envelope.body).ok_or(ProvisioningError::Missing("envelope body"))?;

        if body[0] != PROVISIONING_VERSION {
            return Err(ProvisioningError::BadVersion(body[0]));
        }
        if body.len() < MIN_BODY_LEN {
            return Err(ProvisioningError::Truncated);
        }
        let (signed, their_mac) = body.split_at(body.len() - MAC_LEN);
        let iv = &signed[1..=IV_LEN];
        let ciphertext = &signed[1 + IV_LEN..];

        let keys = derive_provisioning_keys(&self.key_pair.private_key, &their_public)?;
        let (aes_key, mac_key) = keys.split_at(KEY_LEN);
        let mut mac =
            Hmac::<Sha256>::new_from_slice(mac_key).map_err(|_| ProvisioningError::Crypto)?;
        mac.update(signed);
        mac.verify_slice(their_mac)
            .map_err(|_| ProvisioningError::BadMac)?;

        signal_crypto::aes_256_cbc_decrypt(ciphertext, aes_key, iv)
            .map(Zeroizing::new)
            .map_err(|_| ProvisioningError::BadCiphertext)
    }

    /// [`Self::open`] plus [`ProvisionedAccount::from_message`]. `expect_own` is the ACI of the
    /// account being relinked, if any.
    pub(crate) fn decrypt(
        &self,
        envelope: &[u8],
        expect_own: Option<Aci>,
    ) -> Result<ProvisionedAccount, ProvisioningError> {
        let plaintext = self.open(envelope)?;
        let message = proto::ProvisionMessage::decode(plaintext.as_slice())
            .map_err(|_| ProvisioningError::MalformedMessage)?;
        ProvisionedAccount::from_message(message, expect_own)
    }
}

/// HKDF-SHA256 over the X25519 agreement: 32 bytes AES key, then 32 bytes MAC key. Desktop
/// derives 96 bytes and ignores the last 32; HKDF output prefixes are identical.
fn derive_provisioning_keys(
    ours: &PrivateKey,
    theirs: &PublicKey,
) -> Result<Zeroizing<[u8; 2 * KEY_LEN]>, ProvisioningError> {
    let shared = Zeroizing::new(
        ours.calculate_agreement(theirs)
            .map_err(|_| ProvisioningError::Invalid("envelope public key"))?,
    );
    let mut keys = Zeroizing::new([0u8; 2 * KEY_LEN]);
    Hkdf::<Sha256>::new(Some(&[0u8; 32]), &shared)
        .expand(PROVISIONING_INFO, keys.as_mut())
        .map_err(|_| ProvisioningError::Crypto)?;
    Ok(keys)
}

fn non_empty(bytes: Option<Vec<u8>>) -> Option<Vec<u8>> {
    bytes.filter(|b| !b.is_empty())
}

/// The account a phone provisioned us into, validated (§6.5 step 3). Secret bytes are
/// `Zeroizing`; `Debug` is redacted.
pub(crate) struct ProvisionedAccount {
    /// The account's ACI.
    pub(crate) aci: Aci,
    /// The account's PNI.
    pub(crate) pni: Pni,
    /// The account's phone number in E.164 form.
    pub(crate) number: String,
    /// ACI identity key pair (public half derived from the private key).
    pub(crate) aci_identity: IdentityKeyPair,
    /// PNI identity key pair.
    pub(crate) pni_identity: IdentityKeyPair,
    /// One-time code for `PUT /v1/devices/link`.
    pub(crate) provisioning_code: Zeroizing<String>,
    /// Our profile key.
    pub(crate) profile_key: Zeroizing<[u8; 32]>,
    /// The master (SVR) key: as sent, or derived from the account entropy pool.
    pub(crate) master_key: Zeroizing<[u8; 32]>,
    /// The account entropy pool, when the phone sent one.
    pub(crate) account_entropy_pool: Option<Zeroizing<String>>,
    /// Media root backup key (32 bytes), when sent.
    pub(crate) media_root_backup_key: Option<Zeroizing<[u8; 32]>>,
    /// Ephemeral backup key of the history transfer (32 bytes): sent when the linking URL
    /// offered `backup5` and the phone's user chose to transfer (`transfer.rs`).
    pub(crate) ephemeral_backup_key: Option<Zeroizing<[u8; 32]>>,
    /// The phone's read-receipt setting.
    pub(crate) read_receipts: bool,
    /// The provisioning phone's user agent (e.g. `OWA`), when sent.
    #[allow(dead_code, reason = "diagnostics only")]
    pub(crate) user_agent: Option<String>,
}

impl fmt::Debug for ProvisionedAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProvisionedAccount")
            .field("aci", &crate::service_id::SignalUser::Aci(self.aci))
            .field("pni", &crate::service_id::SignalUser::Pni(self.pni))
            .field("has_entropy_pool", &self.account_entropy_pool.is_some())
            .field("read_receipts", &self.read_receipts)
            .finish_non_exhaustive()
    }
}

impl ProvisionedAccount {
    /// Validates a decrypted `ProvisionMessage`. Required: ACI, ACI identity, PNI, PNI identity,
    /// phone number (M1), provisioning code, 32-byte profile key, and a master key or account
    /// entropy pool. A non-empty identity public key must match its private key. With
    /// `expect_own` (relink), a different ACI is [`ProvisioningError::WrongAccount`].
    pub(crate) fn from_message(
        mut message: proto::ProvisionMessage,
        expect_own: Option<Aci>,
    ) -> Result<Self, ProvisioningError> {
        // Move every secret out of the protobuf first, so it is wiped whichever way this returns.
        let aci_private = Zeroizing::new(message.aci_identity_key_private.take());
        let pni_private = Zeroizing::new(message.pni_identity_key_private.take());
        let provisioning_code = Zeroizing::new(message.provisioning_code.take());
        let profile_key = Zeroizing::new(message.profile_key.take());
        let master_key = Zeroizing::new(message.master_key.take());
        let account_entropy_pool = Zeroizing::new(message.account_entropy_pool.take());
        let media_root_backup_key = Zeroizing::new(message.media_root_backup_key.take());
        let ephemeral_backup_key = Zeroizing::new(message.ephemeral_backup_key.take());

        let aci = uuid_from_wire(message.aci_binary.as_deref(), message.aci.as_deref(), None)
            .ok_or(ProvisioningError::Invalid("ACI"))?
            .map(Aci::from)
            .ok_or(ProvisioningError::Missing("ACI"))?;
        if expect_own.is_some_and(|own| own != aci) {
            return Err(ProvisioningError::WrongAccount);
        }
        let aci_identity = identity_key_pair(
            aci_private.as_deref(),
            message.aci_identity_key_public.as_deref(),
            "ACI identity key",
        )?
        .ok_or(ProvisioningError::Missing("ACI identity key"))?;

        let pni = uuid_from_wire(
            message.pni_binary.as_deref(),
            message.pni.as_deref(),
            Some("PNI:"),
        )
        .ok_or(ProvisioningError::Invalid("PNI"))?
        .map(Pni::from);
        let pni_identity = identity_key_pair(
            pni_private.as_deref(),
            message.pni_identity_key_public.as_deref(),
            "PNI identity key",
        )?;
        let number = message.number.filter(|n| !n.is_empty());
        let (pni, pni_identity, number) = match (pni, pni_identity, number) {
            (Some(pni), Some(identity), Some(number)) => (pni, identity, number),
            // E164-less accounts (Desktop's "nopni" linking) send no number; M1 refuses them.
            (_, _, None) => return Err(ProvisioningError::NoPhoneNumber),
            (None, _, Some(_)) => return Err(ProvisioningError::Missing("PNI")),
            (Some(_), None, Some(_)) => return Err(ProvisioningError::Missing("PNI identity key")),
        };
        validate_e164(&number).map_err(|_| ProvisioningError::Invalid("phone number"))?;

        let provisioning_code = provisioning_code
            .as_deref()
            .filter(|c| !c.is_empty())
            .map(|c| Zeroizing::new(c.to_owned()))
            .ok_or(ProvisioningError::Missing("provisioning code"))?;
        let profile_key = key32(profile_key.as_deref(), "profile key")?
            .ok_or(ProvisioningError::Missing("profile key"))?;

        let account_entropy_pool = account_entropy_pool
            .as_deref()
            .filter(|p| !p.is_empty())
            .map(|p| Zeroizing::new(p.to_owned()));
        let derived_master_key = account_entropy_pool
            .as_ref()
            .map(|pool| {
                AccountEntropyPool::from_str(pool)
                    .map(|pool| Zeroizing::new(pool.derive_svr_key()))
                    .map_err(|_| ProvisioningError::Invalid("account entropy pool"))
            })
            .transpose()?;
        let master_key = key32(master_key.as_deref(), "master key")?
            .or(derived_master_key)
            .ok_or(ProvisioningError::Missing(
                "master key or account entropy pool",
            ))?;

        Ok(Self {
            aci,
            pni,
            number,
            aci_identity,
            pni_identity,
            provisioning_code,
            profile_key,
            master_key,
            account_entropy_pool,
            media_root_backup_key: key32(
                media_root_backup_key.as_deref(),
                "media root backup key",
            )?,
            ephemeral_backup_key: key32(ephemeral_backup_key.as_deref(), "ephemeral backup key")?,
            read_receipts: message.read_receipts.unwrap_or(false),
            user_agent: message.user_agent.filter(|u| !u.is_empty()),
        })
    }

    /// The Storage Service key, `HMAC-SHA256(masterKey, "Storage Service Encryption")` (§6.5
    /// step 6).
    pub(crate) fn storage_service_key(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(SvrKey::new(*self.master_key).derive_storage_service_key())
    }
}

/// A UUID from its binary (16 raw bytes, preferred when non-empty) or string form. `None` when
/// malformed, `Some(None)` when both are absent or empty. `tag` is an optional string prefix
/// that is accepted and stripped (`PNI:`; the provisioning message sends PNIs untagged).
fn uuid_from_wire(
    binary: Option<&[u8]>,
    string: Option<&str>,
    tag: Option<&str>,
) -> Option<Option<Uuid>> {
    if let Some(bytes) = binary.filter(|b| !b.is_empty()) {
        return Uuid::from_slice(bytes).ok().map(Some);
    }
    match string.filter(|s| !s.is_empty()) {
        Some(s) => {
            let s = tag.and_then(|t| s.strip_prefix(t)).unwrap_or(s);
            Uuid::try_parse(s).ok().map(Some)
        }
        None => Some(None),
    }
}

/// An identity key pair from a 32-byte private key; a non-empty public key must match it.
fn identity_key_pair(
    private: Option<&[u8]>,
    public: Option<&[u8]>,
    what: &'static str,
) -> Result<Option<IdentityKeyPair>, ProvisioningError> {
    let Some(private) = private.filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    let private = PrivateKey::deserialize(private).map_err(|_| ProvisioningError::Invalid(what))?;
    let pair = IdentityKeyPair::try_from(private).map_err(|_| ProvisioningError::Invalid(what))?;
    if let Some(public) = public.filter(|p| !p.is_empty()) {
        let public = IdentityKey::decode(public).map_err(|_| ProvisioningError::Invalid(what))?;
        if &public != pair.identity_key() {
            return Err(ProvisioningError::Invalid(what));
        }
    }
    Ok(Some(pair))
}

/// An optional 32-byte key: empty or absent is `None`, any other length is invalid.
fn key32(
    bytes: Option<&[u8]>,
    what: &'static str,
) -> Result<Option<Zeroizing<[u8; 32]>>, ProvisioningError> {
    let Some(bytes) = bytes.filter(|b| !b.is_empty()) else {
        return Ok(None);
    };
    <[u8; 32]>::try_from(bytes)
        .map(|key| Some(Zeroizing::new(key)))
        .map_err(|_| ProvisioningError::Invalid(what))
}

/// The capability in the linking URL that asks the phone for the account's history
/// (`Provisioner.preload.ts`: `backup5` when link-and-sync is enabled). The Android phone
/// reads it from the URL (`LinkDeviceViewModel.kt`: `Capability.LinkAndSync`), asks its user
/// "Transfer message history" or "Don't transfer", and only for a transfer puts an ephemeral
/// backup key into the provisioning message.
pub(crate) const CAPABILITY_HISTORY_TRANSFER: &str = "backup5";

/// The QR code URL, exactly as §6.5 specifies and Desktop's `linkDeviceRoute.toAppUrl` builds
/// it: `sgnl://linkdevice?uuid=<address>&pub_key=<std base64 of the 33-byte key>
/// &capabilities=<comma-separated list>`, each value form-urlencoded like `URLSearchParams`.
/// Kourier offers [`CAPABILITY_HISTORY_TRANSFER`] on a fresh link and nothing on a relink;
/// never `nopni` (E164-less accounts are refused, §6.5). Not log-safe (QR contents).
pub(crate) fn link_device_url(
    address: &str,
    public_key: &PublicKey,
    capabilities: &[&str],
) -> String {
    let pub_key = STANDARD.encode(public_key.serialize());
    format!(
        "sgnl://linkdevice?uuid={}&pub_key={}&capabilities={}",
        form_urlencode(address),
        form_urlencode(&pub_key),
        form_urlencode(&capabilities.join(",")),
    )
}

/// The `application/x-www-form-urlencoded` byte serializer of the WHATWG URL standard (what
/// `URLSearchParams.toString` uses): ASCII alphanumerics and `*-._` stay, space becomes `+`,
/// every other UTF-8 byte is `%XX` with uppercase hex.
fn form_urlencode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                out.push(char::from(byte));
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(char::from(HEX[usize::from(byte >> 4)]));
                out.push(char::from(HEX[usize::from(byte & 0x0f)]));
            }
        }
    }
    out
}

/// Why a device name could not be encrypted or decrypted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[allow(dead_code, reason = "the device list (S6) decrypts device names")]
pub(crate) enum DeviceNameError {
    /// The name is empty after normalization (Desktop then sends no name at all).
    #[error("device name is empty")]
    Empty,
    /// Not base64 of a `DeviceName` protobuf, or a field is missing or has the wrong length.
    #[error("encrypted device name is malformed")]
    Malformed,
    /// The synthetic IV does not authenticate the plaintext (wrong key or tampering).
    #[error("encrypted device name does not authenticate")]
    BadSyntheticIv,
    /// The decrypted name is not UTF-8.
    #[error("decrypted device name is not UTF-8")]
    NotUtf8,
    /// A primitive failed in a way that cannot happen with valid lengths.
    #[error("device name crypto failed")]
    Crypto,
}

/// Desktop's `normalizeDeviceName` plus the `MAX_DEVICE_NAME_LENGTH` cut: trim, drop NULs, keep
/// at most 50 UTF-16 code units (never splitting a character).
pub(crate) fn normalize_device_name(raw: &str) -> String {
    let mut units = 0;
    raw.trim()
        .chars()
        .filter(|c| *c != '\0')
        .take_while(|c| {
            units += c.len_utf16();
            units <= MAX_DEVICE_NAME_UTF16
        })
        .collect()
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Result<Zeroizing<[u8; 32]>, DeviceNameError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| DeviceNameError::Crypto)?;
    mac.update(data);
    Ok(Zeroizing::new(mac.finalize().into_bytes().into()))
}

/// AES-256-CTR with a zero 128-bit counter block, as Desktop's `encryptAesCtr(key, _, 0¹⁶)`.
/// `Aes256Ctr32` increments only the low 32 bits; names are far shorter than 2³² blocks.
fn aes_ctr_zero_iv(key: &[u8], data: &mut [u8]) -> Result<(), DeviceNameError> {
    let mut ctr = signal_crypto::Aes256Ctr32::from_key(key, &[0u8; 12], 0)
        .map_err(|_| DeviceNameError::Crypto)?;
    ctr.process(data);
    Ok(())
}

/// The two keys of the device-name SIV scheme, derived from the X25519 master secret.
struct DeviceNameKeys {
    /// `HMAC(secret, "auth")`: keys the synthetic IV.
    auth: Zeroizing<[u8; 32]>,
    /// `HMAC(secret, "cipher")`: keys the per-name cipher key.
    cipher: Zeroizing<[u8; 32]>,
}

impl DeviceNameKeys {
    fn derive(master_secret: &[u8]) -> Result<Self, DeviceNameError> {
        Ok(Self {
            auth: hmac_sha256(master_secret, b"auth")?,
            cipher: hmac_sha256(master_secret, b"cipher")?,
        })
    }
}

/// Encrypts a device name to our ACI identity key (`SD/ts/Crypto.node.ts:115-140`): ECDH with a
/// fresh ephemeral key, synthetic IV = HMAC(HMAC(secret, "auth"), name)[..16], AES-256-CTR under
/// HMAC(HMAC(secret, "cipher"), syntheticIv). Returns the base64 `DeviceName` protobuf that
/// `PUT /v1/devices/link` carries. The name should already be normalized.
pub(crate) fn encrypt_device_name<R: Rng + CryptoRng>(
    rng: &mut R,
    name: &str,
    identity_public: &PublicKey,
) -> Result<String, DeviceNameError> {
    if name.is_empty() {
        return Err(DeviceNameError::Empty);
    }
    let ephemeral = KeyPair::generate(rng);
    let master_secret = Zeroizing::new(
        ephemeral
            .private_key
            .calculate_agreement(identity_public)
            .map_err(|_| DeviceNameError::Crypto)?,
    );
    let keys = DeviceNameKeys::derive(&master_secret)?;
    let synthetic_iv = hmac_sha256(keys.auth.as_slice(), name.as_bytes())?;
    let synthetic_iv = &synthetic_iv[..DEVICE_NAME_SYNTHETIC_IV_LEN];
    let cipher_key = hmac_sha256(keys.cipher.as_slice(), synthetic_iv)?;

    let mut ciphertext = name.as_bytes().to_vec();
    aes_ctr_zero_iv(cipher_key.as_slice(), &mut ciphertext)?;

    let encoded = proto::DeviceName {
        ephemeral_public: Some(ephemeral.public_key.serialize().into_vec()),
        synthetic_iv: Some(synthetic_iv.to_vec()),
        ciphertext: Some(ciphertext),
    }
    .encode_to_vec();
    Ok(STANDARD.encode(encoded))
}

/// Decrypts a base64 `DeviceName` (padding optional) with our ACI identity private key
/// (`SD/ts/Crypto.node.ts:142-161`), checking the synthetic IV in constant time.
#[allow(dead_code, reason = "the device list (S6) decrypts device names")]
pub(crate) fn decrypt_device_name(
    encoded: &str,
    identity_private: &PrivateKey,
) -> Result<String, DeviceNameError> {
    let bytes = crate::wire_b64::LENIENT
        .decode(encoded)
        .map_err(|_| DeviceNameError::Malformed)?;
    let name =
        proto::DeviceName::decode(bytes.as_slice()).map_err(|_| DeviceNameError::Malformed)?;
    let (Some(ephemeral_public), Some(synthetic_iv), Some(ciphertext)) =
        (name.ephemeral_public, name.synthetic_iv, name.ciphertext)
    else {
        return Err(DeviceNameError::Malformed);
    };
    if synthetic_iv.len() != DEVICE_NAME_SYNTHETIC_IV_LEN {
        return Err(DeviceNameError::Malformed);
    }
    let ephemeral_public =
        PublicKey::deserialize(&ephemeral_public).map_err(|_| DeviceNameError::Malformed)?;
    let master_secret = Zeroizing::new(
        identity_private
            .calculate_agreement(&ephemeral_public)
            .map_err(|_| DeviceNameError::Malformed)?,
    );
    let keys = DeviceNameKeys::derive(&master_secret)?;
    let cipher_key = hmac_sha256(keys.cipher.as_slice(), &synthetic_iv)?;

    let mut plaintext = Zeroizing::new(ciphertext);
    aes_ctr_zero_iv(cipher_key.as_slice(), &mut plaintext)?;

    let mut mac = Hmac::<Sha256>::new_from_slice(keys.auth.as_slice())
        .map_err(|_| DeviceNameError::Crypto)?;
    mac.update(&plaintext);
    mac.verify_truncated_left(&synthetic_iv)
        .map_err(|_| DeviceNameError::BadSyntheticIv)?;

    String::from_utf8(plaintext.to_vec()).map_err(|_| DeviceNameError::NotUtf8)
}

/// The phone's side of the cipher, for tests: what a primary device sends in
/// `ProvisionEnvelope`, with a caller-chosen ephemeral key and IV so vectors are reproducible.
#[cfg(test)]
pub(crate) fn seal_provision_envelope(
    ephemeral: &KeyPair,
    iv: [u8; IV_LEN],
    recipient: &PublicKey,
    plaintext: &[u8],
) -> Vec<u8> {
    let keys = derive_provisioning_keys(&ephemeral.private_key, recipient).unwrap();
    let (aes_key, mac_key) = keys.split_at(KEY_LEN);
    let ciphertext = signal_crypto::aes_256_cbc_encrypt(plaintext, aes_key, &iv).unwrap();
    let mut body = Vec::with_capacity(1 + IV_LEN + ciphertext.len() + MAC_LEN);
    body.push(PROVISIONING_VERSION);
    body.extend_from_slice(&iv);
    body.extend_from_slice(&ciphertext);
    let mut mac = Hmac::<Sha256>::new_from_slice(mac_key).unwrap();
    mac.update(&body);
    body.extend_from_slice(&mac.finalize().into_bytes());
    proto::ProvisionEnvelope {
        public_key: Some(ephemeral.public_key.serialize().into_vec()),
        body: Some(body),
    }
    .encode_to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn cipher_from(private: &[u8]) -> ProvisioningCipher {
        let private_key = PrivateKey::deserialize(private).unwrap();
        ProvisioningCipher {
            key_pair: KeyPair::try_from(private_key).unwrap(),
        }
    }

    // Independent known-answer vector, computed with pyca/cryptography (X25519, HKDF-SHA256,
    // AES-256-CBC/PKCS#7, HMAC-SHA256) following ProvisioningCipher.node.ts: recipient private
    // key 01..20, phone ephemeral private key 65..84, IV c8..d7. Signal Desktop's own tests have
    // no provisioning vector.
    const VECTOR_RECIPIENT_PUBLIC: &str =
        "0507a37cbc142093c8b755dc1b10e86cb426374ad16aa853ed0bdfc0b2b86d1c7c";
    const VECTOR_ENVELOPE: &str = "0a21055714769d116bf76436ae74bc793d2c30ad1903c59ac5273805c7e2698b\
        410c36126101c8c9cacbcccdcecfd0d1d2d3d4d5d6d7fce86a77c53504e65abf0668a6f42d92e44b67c7b9709d\
        cf06e0dd231199ea44603d800129788958d2d089402e87bc852f72c6ae6a346f4731077365f2f901c2e5ee10c2\
        7a7c0077bdd46d8fbab1fee4";
    const VECTOR_PLAINTEXT: &[u8] = b"Kourier provisioning known-answer vector, v1";

    fn vector_keys() -> (ProvisioningCipher, KeyPair) {
        let recipient: Vec<u8> = (1..=32).collect();
        let ephemeral: Vec<u8> = (101..=132).collect();
        let ephemeral = KeyPair::try_from(PrivateKey::deserialize(&ephemeral).unwrap()).unwrap();
        (cipher_from(&recipient), ephemeral)
    }

    #[test]
    fn known_answer_vector_opens_and_reseals_identically() {
        let (cipher, ephemeral) = vector_keys();
        assert_eq!(
            cipher.public_key().serialize().as_ref(),
            hex(VECTOR_RECIPIENT_PUBLIC).as_slice()
        );
        let envelope = hex(VECTOR_ENVELOPE);
        assert_eq!(cipher.open(&envelope).unwrap().as_slice(), VECTOR_PLAINTEXT);

        let iv: [u8; 16] = std::array::from_fn(|i| 200 + i as u8);
        let resealed =
            seal_provision_envelope(&ephemeral, iv, cipher.public_key(), VECTOR_PLAINTEXT);
        assert_eq!(resealed, envelope);
    }

    #[test]
    fn tampering_is_detected_before_decryption() {
        let (cipher, _) = vector_keys();
        let envelope = hex(VECTOR_ENVELOPE);
        let parsed = proto::ProvisionEnvelope::decode(envelope.as_slice()).unwrap();
        let body = parsed.body.clone().unwrap();
        let reencode = |body: Vec<u8>| {
            proto::ProvisionEnvelope {
                public_key: parsed.public_key.clone(),
                body: Some(body),
            }
            .encode_to_vec()
        };

        let mut version = body.clone();
        version[0] = 2;
        assert_eq!(
            cipher.open(&reencode(version)).unwrap_err(),
            ProvisioningError::BadVersion(2)
        );
        for index in [1, 20, body.len() - 1] {
            let mut flipped = body.clone();
            flipped[index] ^= 1;
            assert_eq!(
                cipher.open(&reencode(flipped)).unwrap_err(),
                ProvisioningError::BadMac,
                "byte {index}"
            );
        }
        assert_eq!(
            cipher
                .open(&reencode(body[..MIN_BODY_LEN - 1].to_vec()))
                .unwrap_err(),
            ProvisioningError::Truncated
        );
        assert_eq!(
            cipher.open(&reencode(Vec::new())).unwrap_err(),
            ProvisioningError::Missing("envelope body")
        );
        assert_eq!(
            cipher.open(b"\xff\xff").unwrap_err(),
            ProvisioningError::MalformedEnvelope
        );

        // The right envelope for someone else's key fails the MAC.
        let other = ProvisioningCipher::generate(&mut rand::rng());
        assert_eq!(
            other.open(&envelope).unwrap_err(),
            ProvisioningError::BadMac
        );
    }

    struct Phone {
        aci: Aci,
        pni: Pni,
        aci_identity: IdentityKeyPair,
        pni_identity: IdentityKeyPair,
    }

    fn phone() -> Phone {
        let mut rng = rand::rng();
        Phone {
            aci: Aci::from(Uuid::from_u128(0x9d06_52a3_dcc3_4d11_975f_74d6_1598_733f)),
            pni: Pni::from(Uuid::from_u128(0x1111_2222_3333_4444_5555_6666_7777_8888)),
            aci_identity: IdentityKeyPair::generate(&mut rng),
            pni_identity: IdentityKeyPair::generate(&mut rng),
        }
    }

    /// What a current Android/iOS primary sends (binary service ids, AEP, no master key).
    fn full_message(phone: &Phone) -> proto::ProvisionMessage {
        proto::ProvisionMessage {
            aci_identity_key_public: Some(phone.aci_identity.identity_key().serialize().into_vec()),
            aci_identity_key_private: Some(phone.aci_identity.private_key().serialize()),
            pni_identity_key_public: Some(phone.pni_identity.identity_key().serialize().into_vec()),
            pni_identity_key_private: Some(phone.pni_identity.private_key().serialize()),
            aci_binary: Some(Uuid::from(phone.aci).as_bytes().to_vec()),
            pni_binary: Some(Uuid::from(phone.pni).as_bytes().to_vec()),
            number: Some("+14155550101".to_owned()),
            provisioning_code: Some("123456".to_owned()),
            user_agent: Some("OWA".to_owned()),
            profile_key: Some(vec![7; 32]),
            read_receipts: Some(true),
            provisioning_version: Some(1),
            account_entropy_pool: Some("a".repeat(64)),
            media_root_backup_key: Some(vec![9; 32]),
            ..Default::default()
        }
    }

    #[test]
    fn round_trip_through_the_cipher_yields_the_account() {
        let mut rng = rand::rng();
        let cipher = ProvisioningCipher::generate(&mut rng);
        let phone = phone();
        let plaintext = full_message(&phone).encode_to_vec();
        let ephemeral = KeyPair::generate(&mut rng);
        let envelope =
            seal_provision_envelope(&ephemeral, [3; 16], cipher.public_key(), &plaintext);

        let account = cipher.decrypt(&envelope, None).unwrap();
        assert_eq!(account.aci, phone.aci);
        assert_eq!(account.pni, phone.pni);
        assert_eq!(account.number, "+14155550101");
        assert_eq!(
            account.aci_identity.identity_key(),
            phone.aci_identity.identity_key()
        );
        assert_eq!(
            account.pni_identity.identity_key(),
            phone.pni_identity.identity_key()
        );
        assert_eq!(account.provisioning_code.as_str(), "123456");
        assert_eq!(*account.profile_key, [7; 32]);
        let pool = AccountEntropyPool::from_str(&"a".repeat(64)).unwrap();
        assert_eq!(*account.master_key, pool.derive_svr_key());
        // §6.5 step 6, computed here without libsignal: HMAC-SHA256(masterKey, label).
        let mut storage_mac = Hmac::<Sha256>::new_from_slice(&pool.derive_svr_key()).unwrap();
        storage_mac.update(b"Storage Service Encryption");
        assert_eq!(
            account.storage_service_key().as_slice(),
            storage_mac.finalize().into_bytes().as_slice()
        );
        assert_eq!(account.media_root_backup_key.as_deref(), Some(&[9; 32]));
        assert_eq!(account.ephemeral_backup_key, None);
        assert!(account.read_receipts);
        assert_eq!(account.user_agent.as_deref(), Some("OWA"));

        // Relink of the same account passes; a different account fails before anything else.
        assert!(cipher.decrypt(&envelope, Some(phone.aci)).is_ok());
        let other = Aci::from(Uuid::from_u128(5));
        assert_eq!(
            cipher.decrypt(&envelope, Some(other)).unwrap_err(),
            ProvisioningError::WrongAccount
        );
        // Debug is log-safe.
        let debug = format!("{account:?}");
        assert!(
            !debug.contains("4155") && !debug.contains("123456"),
            "{debug}"
        );
    }

    #[test]
    fn legacy_string_ids_and_master_key_are_accepted() {
        let phone = phone();
        let mut message = full_message(&phone);
        message.aci_binary = None;
        message.pni_binary = Some(Vec::new());
        message.aci = Some(Uuid::from(phone.aci).to_string().to_uppercase());
        message.pni = Some(Uuid::from(phone.pni).to_string());
        message.account_entropy_pool = None;
        message.master_key = Some(vec![0x2a; 32]);
        message.aci_identity_key_public = None;
        let account = ProvisionedAccount::from_message(message, None).unwrap();
        assert_eq!(account.aci, phone.aci);
        assert_eq!(account.pni, phone.pni);
        assert_eq!(*account.master_key, [0x2a; 32]);
        // Known answer (Python hmac; also libsignal-account-keys' own test vector).
        assert_eq!(
            account.storage_service_key().as_slice(),
            hex("3f31b618172a9f8ad45e290788e6176736e6161d4ea0e8050f8553521f59c200")
        );
        assert!(account.account_entropy_pool.is_none());
    }

    #[test]
    fn validation_rejects_incomplete_or_inconsistent_messages() {
        let phone = phone();
        let check = |edit: &dyn Fn(&mut proto::ProvisionMessage), expected: ProvisioningError| {
            let mut message = full_message(&phone);
            edit(&mut message);
            assert_eq!(
                ProvisionedAccount::from_message(message, None).unwrap_err(),
                expected
            );
        };
        check(&|m| m.aci_binary = None, ProvisioningError::Missing("ACI"));
        check(
            &|m| m.aci_binary = Some(vec![1; 15]),
            ProvisioningError::Invalid("ACI"),
        );
        check(
            &|m| m.aci_identity_key_private = None,
            ProvisioningError::Missing("ACI identity key"),
        );
        check(
            &|m| m.aci_identity_key_public = m.pni_identity_key_public.clone(),
            ProvisioningError::Invalid("ACI identity key"),
        );
        check(
            &|m| m.aci_identity_key_private = Some(vec![1; 31]),
            ProvisioningError::Invalid("ACI identity key"),
        );
        // E164-less account (Desktop's "nopni" linking): refused in M1 with a clear message.
        check(
            &|m| {
                m.pni_binary = None;
                m.pni_identity_key_private = None;
                m.number = None;
            },
            ProvisioningError::NoPhoneNumber,
        );
        check(
            &|m| m.number = Some(String::new()),
            ProvisioningError::NoPhoneNumber,
        );
        check(&|m| m.pni_binary = None, ProvisioningError::Missing("PNI"));
        check(
            &|m| m.pni_identity_key_private = None,
            ProvisioningError::Missing("PNI identity key"),
        );
        check(
            &|m| m.number = Some("4155550101".to_owned()),
            ProvisioningError::Invalid("phone number"),
        );
        check(
            &|m| m.provisioning_code = None,
            ProvisioningError::Missing("provisioning code"),
        );
        check(
            &|m| m.profile_key = None,
            ProvisioningError::Missing("profile key"),
        );
        check(
            &|m| m.profile_key = Some(vec![1; 16]),
            ProvisioningError::Invalid("profile key"),
        );
        check(
            &|m| m.account_entropy_pool = None,
            ProvisioningError::Missing("master key or account entropy pool"),
        );
        check(
            &|m| m.account_entropy_pool = Some("A".repeat(64)),
            ProvisioningError::Invalid("account entropy pool"),
        );
        check(
            &|m| m.master_key = Some(vec![1; 33]),
            ProvisioningError::Invalid("master key"),
        );
        check(
            &|m| m.ephemeral_backup_key = Some(vec![1; 3]),
            ProvisioningError::Invalid("ephemeral backup key"),
        );
    }

    #[test]
    fn link_url_matches_section_6_5() {
        let (cipher, _) = vector_keys();
        // Expected string computed with Python's urllib.parse.urlencode, which agrees with
        // URLSearchParams on every character that can occur here.
        assert_eq!(
            cipher.link_device_url("Ab-cd_ef/gh+ij=", &[]),
            "sgnl://linkdevice?uuid=Ab-cd_ef%2Fgh%2Bij%3D\
             &pub_key=BQejfLwUIJPIt1XcGxDobLQmN0rRaqhT7QvfwLK4bRx8&capabilities="
        );
        // A fresh link asks for the history transfer; a list is joined with an encoded comma,
        // as `URLSearchParams` writes Desktop's `nopni,backup5`.
        assert!(
            cipher
                .link_device_url("x", &[CAPABILITY_HISTORY_TRANSFER])
                .ends_with("&capabilities=backup5")
        );
        assert!(
            cipher
                .link_device_url("x", &["nopni", "backup5"])
                .ends_with("&capabilities=nopni%2Cbackup5")
        );
        // The pub_key decodes back to the 33-byte serialized key.
        let url = cipher.link_device_url("x", &[]);
        let pub_key = url
            .split('&')
            .find_map(|p| p.strip_prefix("pub_key="))
            .unwrap();
        assert_eq!(
            STANDARD.decode(pub_key).unwrap(),
            cipher.public_key().serialize().into_vec()
        );
    }

    #[test]
    fn form_urlencode_matches_url_search_params() {
        // URLSearchParams keeps *-._ and alphanumerics, turns space into +, escapes the rest
        // (including ~, which Python leaves alone).
        assert_eq!(form_urlencode("aZ09*-._"), "aZ09*-._");
        assert_eq!(form_urlencode("a b~+/=&?"), "a+b%7E%2B%2F%3D%26%3F");
        assert_eq!(form_urlencode("é"), "%C3%A9");
    }

    #[test]
    fn device_name_round_trips_and_normalizes() {
        let mut rng = rand::rng();
        let identity = IdentityKeyPair::generate(&mut rng);
        let name = normalize_device_name("  Kourier on\0 Plasma  ");
        assert_eq!(name, "Kourier on Plasma");
        let encrypted = encrypt_device_name(&mut rng, &name, identity.public_key()).unwrap();
        assert_eq!(
            decrypt_device_name(&encrypted, identity.private_key()).unwrap(),
            name
        );
        // A different identity cannot read it.
        let other = IdentityKeyPair::generate(&mut rng);
        assert_eq!(
            decrypt_device_name(&encrypted, other.private_key()).unwrap_err(),
            DeviceNameError::BadSyntheticIv
        );
        assert_eq!(
            encrypt_device_name(&mut rng, "", identity.public_key()).unwrap_err(),
            DeviceNameError::Empty
        );
    }

    #[test]
    fn device_name_fails_if_synthetic_iv_is_changed() {
        // Desktop's Crypto_test "fails if iv is changed".
        let mut rng = rand::rng();
        let identity = IdentityKeyPair::generate(&mut rng);
        let encrypted =
            encrypt_device_name(&mut rng, "v1.19.0 on Windows 10", identity.public_key()).unwrap();
        let mut name =
            proto::DeviceName::decode(STANDARD.decode(&encrypted).unwrap().as_slice()).unwrap();
        name.synthetic_iv = Some(vec![0x42; 16]);
        let tampered = STANDARD.encode(name.encode_to_vec());
        assert_eq!(
            decrypt_device_name(&tampered, identity.private_key()).unwrap_err(),
            DeviceNameError::BadSyntheticIv
        );
        name.synthetic_iv = Some(vec![0x42; 15]);
        let short = STANDARD.encode(name.encode_to_vec());
        assert_eq!(
            decrypt_device_name(&short, identity.private_key()).unwrap_err(),
            DeviceNameError::Malformed
        );
        assert_eq!(
            decrypt_device_name("%%%", identity.private_key()).unwrap_err(),
            DeviceNameError::Malformed
        );
    }

    #[test]
    fn device_name_fixed_vector_from_libsignal_service_rs() {
        // From libsignal-service-rs src/account_manager.rs `decrypt_device_name` test
        // (AGPL-3.0-only): a name encrypted by a Signal client to this identity key.
        let identity_private = PrivateKey::deserialize(
            &STANDARD
                .decode("0CgxHjwwblXjvX8sD5wZDWdYToMRf+CZSlgaUrxCGVo=")
                .unwrap(),
        )
        .unwrap();
        let encoded = STANDARD.encode(
            proto::DeviceName {
                ephemeral_public: Some(
                    STANDARD
                        .decode("BcZS+Lt6yAKbEpXnRX+I5wHqesuvu93Q2V+fjidwW8R6")
                        .unwrap(),
                ),
                synthetic_iv: Some(STANDARD.decode("86gekHGmltnnZ9QARhiFcg==").unwrap()),
                ciphertext: Some(
                    STANDARD
                        .decode("MtJ9/9KBWLBVAxfZJD4pLKzP4q+iodRJeCc+/A==")
                        .unwrap(),
                ),
            }
            .encode_to_vec(),
        );
        assert_eq!(
            decrypt_device_name(&encoded, &identity_private).unwrap(),
            "Nokia 3310 Millenial Edition"
        );
        // Unpadded base64 is accepted too.
        assert_eq!(
            decrypt_device_name(encoded.trim_end_matches('='), &identity_private).unwrap(),
            "Nokia 3310 Millenial Edition"
        );
    }

    #[test]
    fn normalize_cuts_at_fifty_utf16_units_without_splitting() {
        assert_eq!(normalize_device_name(&"a".repeat(60)).len(), 50);
        // 😀 is two UTF-16 units: 25 of them are exactly 50 units, a 26th would exceed the limit.
        let emoji = "😀".repeat(30);
        let cut = normalize_device_name(&emoji);
        assert_eq!(cut.chars().count(), 25);
        let mixed = format!("{}😀", "a".repeat(49));
        assert_eq!(normalize_device_name(&mixed), "a".repeat(49));
    }
}

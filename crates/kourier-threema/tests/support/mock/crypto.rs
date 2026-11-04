//! The fake's OWN crypto, written from the spec with the registry crates (lane plan §2.4): never
//! libthreema's crypto or protocol code, so the real client against the fake is a check by two
//! implementations. The derivations are pinned to Desktop's known-answer vectors
//! (`desktop_vectors.rs`, test `mock::crypto::`), and the path key's public half to the URL the
//! real `D2mProtocol` builds.
//!
//! - Device group keys: `TP/md.md:19-27` (BLAKE2b-MAC, 32 bytes, key = DGK, salt `p` `r` `di` `sdd`
//!   `ts`, personal `3ma-mdev`); the REGISTRY blake2 0.10.6 takes the key as `&[u8]` (critic L2:
//!   T0's `Some(dgk)` was the patched copy's signature).
//! - `X25519HSalsa20`: X25519, then HSalsa20 with a zero nonce (NaCl's `crypto_box_beforenm`);
//!   the D2M challenge (`TP/md-d2m.proto:86-99`) and CSP's transport and vouch keys.
//! - Nonce-ahead secretbox `nonce(24) || tag(16) || ciphertext` (D2M challenge response, device
//!   info, reflected envelopes, shared data, transaction scopes).
//! - CSP nonces `cookie(16) || u64-le(sequence number)`, the counter starting at 1 per direction
//!   (`TP/csp.struct.yml:66-76`), strict: the fake never skips or repeats a number.
//! - The login vouch (`TP/csp.struct.yml:235-245`).
//!
//! Nothing here formats a key: errors are `None` or a `&'static str`.

use blake2::Blake2bMac;
use blake2::digest::Mac as _;
use blake2::digest::consts::U32;
use crypto_secretbox::XSalsa20Poly1305;
use crypto_secretbox::aead::generic_array::GenericArray;
use crypto_secretbox::aead::{Aead as _, KeyInit as _};
use x25519_dalek::{PublicKey, StaticSecret};

type Blake2bMac256 = Blake2bMac<U32>;

/// BLAKE2b-MAC with a 32-byte output; `key` up to 64 bytes, `salt` and `personal` up to 16.
pub fn blake2b_mac(key: &[u8], salt: &[u8], personal: &[u8], input: &[u8]) -> [u8; 32] {
    let mut mac =
        Blake2bMac256::new_with_salt_and_personal(key, salt, personal).expect("blake2b parameters");
    mac.update(input);
    mac.finalize().into_bytes().into()
}

/// One multi-device key from the device group key: `BLAKE2b(key=DGK, salt, personal='3ma-mdev')`.
pub fn derive(dgk: &[u8; 32], salt: &[u8]) -> [u8; 32] {
    blake2b_mac(dgk, salt, b"3ma-mdev", &[])
}

/// The keys of one device group. The fake's SERVER objects never hold this (S-mock §2.4): the
/// mediator knows only [`DeviceGroupKeys::path_public`]; the test-side phone and oracle hold the
/// rest.
#[derive(Clone)]
pub struct DeviceGroupKeys {
    /// DGPK, the X25519 secret key of the device group path.
    pub path_secret: [u8; 32],
    /// DGPK.public, what the mediator knows the group by (the URL's `ClientUrlInfo`).
    pub path_public: [u8; 32],
    /// DGRK: reflected envelopes.
    pub reflect: [u8; 32],
    /// DGDIK: the device info.
    pub device_info: [u8; 32],
    /// DGSDDK: the shared device data.
    pub shared_device_data: [u8; 32],
    /// DGTSK: transaction scopes.
    pub transaction_scope: [u8; 32],
}

impl DeviceGroupKeys {
    pub fn derive(dgk: &[u8; 32]) -> Self {
        let path_secret = derive(dgk, b"p");
        Self {
            path_secret,
            path_public: x25519_public(&path_secret),
            reflect: derive(dgk, b"r"),
            device_info: derive(dgk, b"di"),
            shared_device_data: derive(dgk, b"sdd"),
            transaction_scope: derive(dgk, b"ts"),
        }
    }
}

impl std::fmt::Debug for DeviceGroupKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeviceGroupKeys")
    }
}

/// The X25519 public key of a secret key (clamped as X25519 does).
pub fn x25519_public(secret: &[u8; 32]) -> [u8; 32] {
    PublicKey::from(&StaticSecret::from(*secret)).to_bytes()
}

/// `X25519HSalsa20(secret, public)`: the shared secret through HSalsa20 with a zero nonce. `None`
/// when the exchange was not contributory (a low-order public key), as libthreema refuses it too.
pub fn hsalsa_shared(secret: &[u8; 32], public: &[u8; 32]) -> Option<[u8; 32]> {
    use salsa20::cipher::consts::U10;
    let shared = StaticSecret::from(*secret).diffie_hellman(&PublicKey::from(*public));
    if !shared.was_contributory() {
        return None;
    }
    Some(
        salsa20::hsalsa::<U10>(
            GenericArray::from_slice(shared.as_bytes()),
            &GenericArray::default(),
        )
        .into(),
    )
}

/// XSalsa20-Poly1305 with an explicit nonce: `tag(16) || ciphertext`.
pub fn seal(key: &[u8; 32], nonce: &[u8; 24], plaintext: &[u8]) -> Vec<u8> {
    XSalsa20Poly1305::new(GenericArray::from_slice(key))
        .encrypt(GenericArray::from_slice(nonce), plaintext)
        .expect("secretbox seal")
}

/// Opens what [`seal`] made; `None` when the tag does not match.
pub fn open(key: &[u8; 32], nonce: &[u8; 24], boxed: &[u8]) -> Option<Vec<u8>> {
    XSalsa20Poly1305::new(GenericArray::from_slice(key))
        .decrypt(GenericArray::from_slice(nonce), boxed)
        .ok()
}

/// Nonce-ahead secretbox: `nonce(24) || tag(16) || ciphertext`.
pub fn seal_nonce_ahead(key: &[u8; 32], nonce: &[u8; 24], plaintext: &[u8]) -> Vec<u8> {
    let mut out = nonce.to_vec();
    out.extend(seal(key, nonce, plaintext));
    out
}

/// Opens a nonce-ahead box; `None` when it is too short or the tag does not match.
pub fn open_nonce_ahead(key: &[u8; 32], data: &[u8]) -> Option<Vec<u8>> {
    let (nonce, boxed) = data.split_at_checked(24)?;
    open(key, nonce.try_into().ok()?, boxed)
}

/// A CSP nonce: `cookie(16) || u64-le(sequence number)`.
pub fn csp_nonce(cookie: &[u8; 16], sequence_number: u64) -> [u8; 24] {
    let mut nonce = [0u8; 24];
    nonce[..16].copy_from_slice(cookie);
    nonce[16..].copy_from_slice(&sequence_number.to_le_bytes());
    nonce
}

/// One direction's nonces of a CSP connection: the cookie and the next sequence number, which
/// starts at 1 and is used exactly once (`CSN+` / `SSN+`). The fake opens the client's boxes with
/// the next number only: a skipped or repeated number does not open (strict counter).
pub struct CspNonces {
    cookie: [u8; 16],
    next: u64,
}

impl CspNonces {
    pub fn new(cookie: [u8; 16]) -> Self {
        Self { cookie, next: 1 }
    }

    pub fn cookie(&self) -> [u8; 16] {
        self.cookie
    }

    /// The sequence number the next box uses.
    pub fn next_number(&self) -> u64 {
        self.next
    }

    /// The nonce of the next box; the counter moves on.
    pub fn take(&mut self) -> Result<[u8; 24], &'static str> {
        let nonce = csp_nonce(&self.cookie, self.next);
        self.next = self
            .next
            .checked_add(1)
            .ok_or("csp sequence number overflow")?;
        Ok(nonce)
    }

    /// Seals with the next nonce.
    pub fn seal(&mut self, key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, &'static str> {
        Ok(seal(key, &self.take()?, plaintext))
    }

    /// Opens with the next nonce; the counter moves on only when the box opened.
    pub fn open(&mut self, key: &[u8; 32], boxed: &[u8]) -> Option<Vec<u8>> {
        let plain = open(key, &csp_nonce(&self.cookie, self.next), boxed)?;
        self.next = self.next.checked_add(1)?;
        Some(plain)
    }
}

/// The login vouch as the SERVER computes it (`TP/csp.struct.yml:235-245`): with
/// `SS1 = X25519HSalsa20(SK.secret, CK.public)` and `SS2 = X25519HSalsa20(TSK.secret, CK.public)`
/// (the same secrets the client gets from `CK.secret` and the servers' publics),
/// `VouchKey = BLAKE2b(key=SS1 || SS2, salt='v2', personal='3ma-csp')` and
/// `vouch = BLAKE2b(out-length=32, key=VouchKey, input=SCK || TCK.public)`. `None` when an
/// exchange was not contributory.
pub fn server_vouch(
    sk_secret: &[u8; 32],
    tsk_secret: &[u8; 32],
    ck_public: &[u8; 32],
    sck: &[u8; 16],
    tck_public: &[u8; 32],
) -> Option<[u8; 32]> {
    let ss1 = hsalsa_shared(sk_secret, ck_public)?;
    let ss2 = hsalsa_shared(tsk_secret, ck_public)?;
    Some(vouch_from_secrets(&ss1, &ss2, sck, tck_public))
}

/// The vouch from the two shared secrets (either side's).
pub fn vouch_from_secrets(
    ss1: &[u8; 32],
    ss2: &[u8; 32],
    sck: &[u8; 16],
    tck_public: &[u8; 32],
) -> [u8; 32] {
    let mut secret = [0u8; 64];
    secret[..32].copy_from_slice(ss1);
    secret[32..].copy_from_slice(ss2);
    let vouch_key = blake2b_mac(&secret, b"v2", b"3ma-csp", &[]);
    let mut input = [0u8; 48];
    input[..16].copy_from_slice(sck);
    input[16..].copy_from_slice(tck_public);
    blake2b_mac(&vouch_key, &[], &[], &input)
}

/// An X25519 key pair.
#[derive(Clone)]
pub struct KeyPair {
    pub secret: [u8; 32],
    pub public: [u8; 32],
}

impl KeyPair {
    pub fn from_secret(secret: [u8; 32]) -> Self {
        Self {
            secret,
            public: x25519_public(&secret),
        }
    }
}

impl std::fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeyPair")
    }
}

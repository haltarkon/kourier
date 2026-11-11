//! Key structures (or keys moved into associated ciphers).
use core::{array::TryFromSliceError, fmt};

#[cfg(any(test, feature = "cli"))]
use anyhow;
use educe::Educe;
use libthreema_dependencies::{
    crypto::{
        aead::AeadInPlace as _,
        argon2, blake2b, chacha20,
        cipher::KeyInit as _,
        digest::{FixedOutput as _, Mac as _},
        salsa20, x25519,
    },
    data_encoding::HEXLOWER_PERMISSIVE,
    rand::{self, Rng as _},
};
use libthreema_macros::{ConstantTimeEq, Name, concat_fixed_bytes};
use zeroize::ZeroizeOnDrop;

use crate::{
    common::{Nonce, ThreemaId},
    crypto::{DecryptionFailed, EncryptionFailed, aead::AeadRandomNonceAhead as _},
    utils::debug::{Name as _, debug_static_secret},
};

/// Key for solving authentication challenges during the CSP handshake (aka _vouch key_).
pub(crate) struct CspAuthenticationKey(pub(crate) blake2b::Blake2bMac256);

/// Cipher associated to the Message Key (MK).
pub(crate) struct MessageCipher(pub(crate) salsa20::XSalsa20Poly1305);

/// Cipher associated to the Message Metadata Key (MMK).
pub(crate) struct MessageMetadataCipher(pub(crate) salsa20::XSalsa20Poly1305);

/// Shared secret context for usage between two identities (i.e. client to client).
///
/// Patch P5 (Kourier): `pub` with its four boxes (the field stays private), each the line libthreema's
/// outgoing and incoming messages run.
pub struct CspE2eKey(x25519::SharedSecretHSalsa20);
impl CspE2eKey {
    /// Seal a message container (already PKCS#7-padded) with the Message Key (MK): XSalsa20-Poly1305
    /// with `nonce` and no associated data, as libthreema's outgoing message does
    /// (`encrypt_message_container_in_place`). Returns the tag followed by the ciphertext.
    ///
    /// # Errors
    ///
    /// Returns [`EncryptionFailed`] if the cipher refuses the buffer.
    pub fn encrypt_message(&self, nonce: &Nonce, mut plaintext: Vec<u8>) -> Result<Vec<u8>, EncryptionFailed> {
        self.message_cipher()
            .0
            .encrypt_in_place(&nonce.0.into(), b"", &mut plaintext)
            .map_err(|_| EncryptionFailed)?;
        Ok(plaintext)
    }

    /// Open a message container sealed with the Message Key (MK): the tag followed by the ciphertext, as
    /// libthreema's incoming message opens it. Returns the plaintext with its PKCS#7 padding.
    ///
    /// # Errors
    ///
    /// Returns [`DecryptionFailed`] if the input is shorter than a tag or the box does not open.
    pub fn decrypt_message(&self, nonce: &Nonce, tag_and_ciphertext: &[u8]) -> Result<Vec<u8>, DecryptionFailed> {
        let mut buffer = tag_and_ciphertext.to_vec();
        self.message_cipher()
            .0
            .decrypt_in_place(&nonce.0.into(), b"", &mut buffer)
            .map_err(|_| DecryptionFailed)?;
        Ok(buffer)
    }

    /// Seal the encoded message metadata with the Message Metadata Key (MMK): XSalsa20-Poly1305 with
    /// `nonce` (the message's nonce) and no associated data, as libthreema's outgoing message does
    /// (`encrypt_metadata_in_place`). Returns the tag followed by the ciphertext.
    ///
    /// # Errors
    ///
    /// Returns [`EncryptionFailed`] if the cipher refuses the buffer.
    pub fn encrypt_metadata(&self, nonce: &Nonce, mut plaintext: Vec<u8>) -> Result<Vec<u8>, EncryptionFailed> {
        self.message_metadata_cipher()
            .0
            .encrypt_in_place(&nonce.0.into(), b"", &mut plaintext)
            .map_err(|_| EncryptionFailed)?;
        Ok(plaintext)
    }

    /// Open message metadata sealed with the Message Metadata Key (MMK): the tag followed by the
    /// ciphertext, as libthreema's incoming message opens it. Returns the encoded metadata.
    ///
    /// # Errors
    ///
    /// Returns [`DecryptionFailed`] if the input is shorter than a tag or the box does not open.
    pub fn decrypt_metadata(&self, nonce: &Nonce, tag_and_ciphertext: &[u8]) -> Result<Vec<u8>, DecryptionFailed> {
        let mut buffer = tag_and_ciphertext.to_vec();
        self.message_metadata_cipher()
            .0
            .decrypt_in_place(&nonce.0.into(), b"", &mut buffer)
            .map_err(|_| DecryptionFailed)?;
        Ok(buffer)
    }

    /// Get the Message Key (MK).
    ///
    /// IMPORTANT: This key should not be used any other purpose but for messages. Otherwise, it's
    /// just another story of _payload confusion_.
    #[must_use]
    pub(crate) fn message_cipher(&self) -> MessageCipher {
        MessageCipher(salsa20::XSalsa20Poly1305::new(self.0.as_bytes().into()))
    }

    /// Derive the Message Metadata Key (MMK).
    #[must_use]
    pub(crate) fn message_metadata_cipher(&self) -> MessageMetadataCipher {
        let mmk =
            blake2b::Blake2bMac256::new_with_salt_and_personal(Some(self.0.as_bytes()), b"mm", b"3ma-csp")
                .expect("Blake2bMac256 failed")
                .finalize_fixed();
        MessageMetadataCipher(salsa20::XSalsa20Poly1305::new(&mmk))
    }
}

/// Key for solving authentication challenges of the directory server.
pub(crate) struct DirectoryAuthenticationKey(pub(crate) blake2b::Blake2bMac256);

/// Key for solving authentication challenges of the work directory server.
pub(crate) struct WorkDirectoryAuthenticationKey(pub(crate) blake2b::Blake2bMac256);

/// The Client key (often internally referred to as `CK` in the code and documentation) is a 32 bytes long,
/// permanent secret key associated to the Threema ID.
///
/// IMPORTANT: This is **THE** key which requires ultimate care!
#[derive(Name, Educe, ZeroizeOnDrop)]
#[educe(Debug)]
pub struct ClientKey(#[educe(Debug(method(debug_static_secret)))] x25519::StaticSecret);
impl ClientKey {
    /// Byte length of the client key.
    pub const LENGTH: usize = x25519::KEY_LENGTH;

    /// Sample a random client key.
    #[must_use]
    pub fn random() -> Self {
        let mut client_key = [0_u8; Self::LENGTH];
        rand::thread_rng().fill(&mut client_key);
        Self::from(client_key)
    }

    /// Get the public key associated with this client key secret.
    #[must_use]
    pub fn public_key(&self) -> PublicKey {
        PublicKey(x25519::PublicKey::from(&self.0))
    }

    /// Byte representation of the client key.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; Self::LENGTH] {
        self.0.as_bytes()
    }

    /// Derive the key to solve an authentication challenge during the CSP handshake (aka _vouch key_).
    ///
    /// `None` is returned in case the shared secret is derived from a non-contributory key.
    #[must_use]
    pub(crate) fn derive_csp_authentication_key(
        &self,
        permanent_server_key: &PublicKey,
        temporary_server_key: &PublicKey,
    ) -> Option<CspAuthenticationKey> {
        // Calculate the secret as the concatenation of two shared secrets
        let secret: [u8; 2 * x25519::SharedSecretHSalsa20::LENGTH] = concat_fixed_bytes!(
            // Compute first half as X25519HSalsa20(CK.secret, SK.public)
            self.0
                .diffie_hellman_hsalsa20(&permanent_server_key.0)?
                .to_bytes(),
            // Compute second half as X25519HSalsa20(CK.secret, temporary_server_key.public)
            self.0
                .diffie_hellman_hsalsa20(&temporary_server_key.0)?
                .to_bytes(),
        );

        // Apply Blake2b to obtain the CSP authentication secret (aka _vouch key_)
        let key = blake2b::Blake2bMac256::new_with_salt_and_personal(Some(&secret), b"v2", b"3ma-csp")
            .expect("Blake2bMac256 failed")
            .finalize()
            .into_bytes();
        Some(CspAuthenticationKey(
            blake2b::Blake2bMac256::new_with_salt_and_personal(Some(&key), &[], &[])
                .expect("Blake2bMac256 failed"),
        ))
    }

    /// Derive the key to solve an authentication challenge against the directory server.
    ///
    /// `None` is returned in case the shared secret is derived from a non-contributory key.
    #[must_use]
    pub(crate) fn derive_directory_authentication_key(
        &self,
        challenge_public_key: &PublicKey,
    ) -> Option<DirectoryAuthenticationKey> {
        let secret = self.0.diffie_hellman_hsalsa20(&challenge_public_key.0)?;
        let key =
            blake2b::Blake2bMac256::new_with_salt_and_personal(Some(secret.as_bytes()), b"dir", b"3ma-csp")
                .expect("Blake2bMac256 failed")
                .finalize()
                .into_bytes();
        Some(DirectoryAuthenticationKey(
            blake2b::Blake2bMac256::new_with_salt_and_personal(Some(&key), &[], &[])
                .expect("Blake2bMac256 failed"),
        ))
    }

    /// Derive the key to solve an authentication challenge against the work directory server.
    #[must_use]
    pub(crate) fn derive_work_directory_authentication_key(
        &self,
        challenge_public_key: &PublicKey,
    ) -> Option<WorkDirectoryAuthenticationKey> {
        let secret = self.0.diffie_hellman_hsalsa20(&challenge_public_key.0)?;
        let key =
            blake2b::Blake2bMac256::new_with_salt_and_personal(Some(secret.as_bytes()), b"wdir", b"3ma-csp")
                .expect("Blake2bMac256 failed")
                .finalize()
                .into_bytes();
        Some(WorkDirectoryAuthenticationKey(
            blake2b::Blake2bMac256::new_with_salt_and_personal(Some(&key), &[], &[])
                .expect("Blake2bMac256 failed"),
        ))
    }

    /// Derive the shared secret for usage between two identities (i.e. client to client).
    ///
    /// `None` is returned in case the shared secret is derived from a non-contributory key.
    #[must_use]
    pub fn derive_csp_e2e_key(&self, client_public_key: &PublicKey) -> Option<CspE2eKey> {
        Some(CspE2eKey(self.0.diffie_hellman_hsalsa20(&client_public_key.0)?))
    }
}
impl From<[u8; Self::LENGTH]> for ClientKey {
    fn from(bytes: [u8; Self::LENGTH]) -> Self {
        Self(x25519::StaticSecret::from(bytes))
    }
}
impl TryFrom<&[u8]> for ClientKey {
    type Error = TryFromSliceError;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        Ok(Self::from(TryInto::<[u8; Self::LENGTH]>::try_into(bytes)?))
    }
}
#[cfg(any(test, feature = "cli"))]
impl From<&RawClientKey> for ClientKey {
    fn from(client_key: &RawClientKey) -> Self {
        Self(client_key.0.clone())
    }
}

/// Also the client key but [`Clone`].
///
/// For example needed for the CLI which requires this struct to be [`Clone`] but we don't want
/// [`ClientKey`] to be [`Clone`].
#[cfg(any(test, feature = "cli"))]
#[derive(Clone, Educe, ZeroizeOnDrop)]
#[educe(Debug)]
pub struct RawClientKey(#[educe(Debug(method(debug_static_secret)))] x25519::StaticSecret);
#[cfg(any(test, feature = "cli"))]
impl RawClientKey {
    /// Convert a hex string to a [`ClientKey`].
    ///
    /// # Errors
    ///
    /// Returns a string describing the error.
    #[cfg(any(test, feature = "cli"))]
    pub fn from_hex(string: &str) -> anyhow::Result<Self> {
        use anyhow::Context as _;

        let bytes = HEXLOWER_PERMISSIVE.decode(string.as_bytes())?;
        let bytes: [u8; ClientKey::LENGTH] = bytes.as_slice().try_into().context(format!(
            "must be {} bytes, got {}",
            ClientKey::LENGTH,
            bytes.len()
        ))?;
        Ok(Self(x25519::StaticSecret::from(bytes)))
    }
}

/// Public portion associated to an X25519 secret key.
#[derive(Clone, Copy, Eq, Hash, PartialEq, Name)]
pub struct PublicKey(pub(crate) x25519::PublicKey);
impl PublicKey {
    /// Byte length of the public portion of an X25519 secret key.
    pub const LENGTH: usize = x25519::KEY_LENGTH;

    /// Convert a hex string to a [`PublicKey`].
    ///
    /// # Errors
    ///
    /// Returns a string describing the error.
    #[cfg(any(test, feature = "cli"))]
    pub fn from_hex(string: &str) -> anyhow::Result<Self> {
        use anyhow::Context as _;

        let bytes = HEXLOWER_PERMISSIVE.decode(string.as_bytes())?;
        Self::try_from(bytes.as_slice()).context(format!(
            "must be {} bytes, got {}",
            Self::LENGTH,
            bytes.len()
        ))
    }
}
impl From<&x25519::StaticSecret> for PublicKey {
    fn from(private_key: &x25519::StaticSecret) -> Self {
        Self(x25519::PublicKey::from(private_key))
    }
}
impl From<[u8; Self::LENGTH]> for PublicKey {
    fn from(bytes: [u8; Self::LENGTH]) -> Self {
        Self(x25519::PublicKey::from(bytes))
    }
}
impl TryFrom<&[u8]> for PublicKey {
    type Error = TryFromSliceError;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        Ok(Self::from(TryInto::<[u8; Self::LENGTH]>::try_into(bytes)?))
    }
}
impl fmt::Display for PublicKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&HEXLOWER_PERMISSIVE.encode(self.0.as_bytes()))
    }
}
impl fmt::Debug for PublicKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple(Self::NAME)
            .field(&self.to_string())
            .finish()
    }
}

/// Cipher associated to the key computed as `X25519HSalsa20(DGPK.secret, ephemeral_server_key)`.
pub(crate) struct DeviceGroupPathAuthenticationCipher(pub(crate) salsa20::XSalsa20Poly1305);

/// The Device Group Path Key (DGPK).
pub(crate) struct DeviceGroupPathKey(x25519::StaticSecret);
impl DeviceGroupPathKey {
    /// Byte length of the device group key.
    pub(crate) const LENGTH: usize = 32;

    /// Get the public key associated with this client key secret.
    ///
    /// This public key is used as the Mediator Device ID.
    pub(crate) fn public_key(&self) -> PublicKey {
        PublicKey::from(&self.0)
    }

    pub(crate) fn authentication_cipher(
        self,
        ephemeral_server_key: &PublicKey,
    ) -> Option<DeviceGroupPathAuthenticationCipher> {
        Some(DeviceGroupPathAuthenticationCipher(
            salsa20::XSalsa20Poly1305::new(
                self.0
                    .diffie_hellman_hsalsa20(&ephemeral_server_key.0)?
                    .as_bytes()
                    .into(),
            ),
        ))
    }
}

/// Cipher associated to the Device Group Reflect Key (DGRK).
pub(crate) struct DeviceGroupReflectCipher(pub(crate) salsa20::XSalsa20Poly1305);

/// Cipher associated to the Device Group Device Info Key (DGDIK).
pub(crate) struct DeviceGroupDeviceInfoCipher(pub(crate) salsa20::XSalsa20Poly1305);

/// Cipher associated to the Device Group Transaction Scope Key (DGTSK).
pub(crate) struct DeviceGroupTransactionScopeCipher(pub(crate) salsa20::XSalsa20Poly1305);

#[derive(Educe, ZeroizeOnDrop)]
#[educe(Debug)]
/// The Device Group Key (DGK).
pub struct DeviceGroupKey(#[educe(Debug(method(debug_static_secret)))] x25519::StaticSecret);
impl DeviceGroupKey {
    /// Byte length of the device group key.
    pub const LENGTH: usize = 32;

    /// Sample a random Device Group Key.
    #[must_use]
    pub fn random() -> Self {
        let mut device_group_key = [0_u8; Self::LENGTH];
        rand::thread_rng().fill(&mut device_group_key);
        Self::from(device_group_key)
    }

    /// The public half of the Device Group Path Key: the blob mirror's device group id and the prefix of its hosts
    /// (Kourier patch P12).
    #[must_use]
    pub fn path_public_key(&self) -> [u8; DeviceGroupPathKey::LENGTH] {
        *self.path_key().public_key().0.as_bytes()
    }

    /// Derive the Device Group Path Key (DGPK).
    #[must_use]
    pub(crate) fn path_key(&self) -> DeviceGroupPathKey {
        let path_key: [u8; DeviceGroupPathKey::LENGTH] = self.derive_key(b"p").into();
        DeviceGroupPathKey(x25519::StaticSecret::from(path_key))
    }

    /// Derive the Device Group Reflect Key (DGRK).
    #[must_use]
    pub(crate) fn reflect_key(&self) -> DeviceGroupReflectCipher {
        DeviceGroupReflectCipher(salsa20::XSalsa20Poly1305::new(&self.derive_key(b"r")))
    }

    /// Derive the Device Group Device Info Key (DGDIK).
    #[must_use]
    pub(crate) fn device_info_key(&self) -> DeviceGroupDeviceInfoCipher {
        DeviceGroupDeviceInfoCipher(salsa20::XSalsa20Poly1305::new(&self.derive_key(b"di")))
    }

    /// Derive the Device Group Transaction Scope Key (DGTSK).
    #[must_use]
    pub(crate) fn transaction_scope_key(&self) -> DeviceGroupTransactionScopeCipher {
        DeviceGroupTransactionScopeCipher(salsa20::XSalsa20Poly1305::new(&self.derive_key(b"ts")))
    }

    /// Open a reflected `d2d.Envelope` (`Reflected.envelope`) with the Device Group Reflect Key (DGRK): the
    /// 24-byte nonce ahead of an XSalsa20-Poly1305 box without associated data, the inverse of the seal
    /// of libthreema's reflection (`ReflectPayload::encode_and_encrypt`). Patch P5 (Kourier).
    ///
    /// Returns the nonce (what the D2D nonce store keeps, hashed) and the envelope's bytes as sent
    /// (padding included, not decoded).
    ///
    /// # Errors
    ///
    /// Returns [`DecryptionFailed`] if the input is shorter than a nonce and a tag or the box does not open.
    pub fn open_reflected(&self, nonce_and_ciphertext: &[u8]) -> Result<(Nonce, Vec<u8>), DecryptionFailed> {
        let mut buffer = nonce_and_ciphertext.to_vec();
        let nonce = self
            .reflect_key()
            .0
            .decrypt_in_place_random_nonce_ahead(b"", &mut buffer)
            .map_err(|_| DecryptionFailed)?;
        Ok((nonce.into(), buffer))
    }

    /// Seal encoded (and padded) `d2d.Envelope` bytes with the Device Group Reflect Key (DGRK) and a random
    /// nonce ahead, as libthreema's reflection does (`ReflectPayload::encode_and_encrypt`). Patch P5 (Kourier).
    ///
    /// Returns the nonce and `nonce || tag || ciphertext` (what `Reflect.envelope` carries).
    ///
    /// # Errors
    ///
    /// Returns [`EncryptionFailed`] if the cipher refuses the buffer.
    pub fn seal_reflected(&self, mut plaintext: Vec<u8>) -> Result<(Nonce, Vec<u8>), EncryptionFailed> {
        let nonce = self
            .reflect_key()
            .0
            .encrypt_in_place_random_nonce_ahead(b"", &mut plaintext)
            .map_err(|_| EncryptionFailed)?;
        Ok((nonce.into(), plaintext))
    }

    /// Open an encrypted `d2d.TransactionScope` (`BeginTransaction`, `TransactionRejected` and
    /// `TransactionEnded`) with the Device Group Transaction Scope Key (DGTSK): nonce ahead, as libthreema's
    /// begin-transaction task opens it. Returns the encoded scope. Patch P5 (Kourier).
    ///
    /// # Errors
    ///
    /// Returns [`DecryptionFailed`] if the input is shorter than a nonce and a tag or the box does not open.
    pub fn open_transaction_scope(&self, nonce_and_ciphertext: &[u8]) -> Result<Vec<u8>, DecryptionFailed> {
        let mut buffer = nonce_and_ciphertext.to_vec();
        let _ = self
            .transaction_scope_key()
            .0
            .decrypt_in_place_random_nonce_ahead(b"", &mut buffer)
            .map_err(|_| DecryptionFailed)?;
        Ok(buffer)
    }

    /// Seal an encoded `d2d.TransactionScope` with the Device Group Transaction Scope Key (DGTSK) and a
    /// random nonce ahead, as libthreema's transaction manager does. Returns `nonce || tag || ciphertext`.
    /// Patch P5 (Kourier).
    ///
    /// # Errors
    ///
    /// Returns [`EncryptionFailed`] if the cipher refuses the buffer.
    pub fn seal_transaction_scope(&self, mut plaintext: Vec<u8>) -> Result<Vec<u8>, EncryptionFailed> {
        let _ = self
            .transaction_scope_key()
            .0
            .encrypt_in_place_random_nonce_ahead(b"", &mut plaintext)
            .map_err(|_| EncryptionFailed)?;
        Ok(plaintext)
    }

    /// Open an encrypted `d2d.DeviceInfo` (`DevicesInfo.augmented_device_info[..].encrypted_device_info`) with
    /// the Device Group Device Info Key (DGDIK): nonce ahead, the inverse of the seal of libthreema's
    /// `ClientHello`. Returns the encoded (padded) device info. Patch P5 (Kourier).
    ///
    /// # Errors
    ///
    /// Returns [`DecryptionFailed`] if the input is shorter than a nonce and a tag or the box does not open.
    pub fn open_device_info(&self, nonce_and_ciphertext: &[u8]) -> Result<Vec<u8>, DecryptionFailed> {
        let mut buffer = nonce_and_ciphertext.to_vec();
        let _ = self
            .device_info_key()
            .0
            .decrypt_in_place_random_nonce_ahead(b"", &mut buffer)
            .map_err(|_| DecryptionFailed)?;
        Ok(buffer)
    }

    fn derive_key(&self, salt: &[u8]) -> salsa20::Key {
        blake2b::Blake2bMac256::new_with_salt_and_personal(Some(self.0.as_bytes()), salt, b"3ma-mdev")
            .expect("Blake2bMac256 failed")
            .finalize_fixed()
    }
}
impl From<[u8; Self::LENGTH]> for DeviceGroupKey {
    fn from(bytes: [u8; Self::LENGTH]) -> Self {
        Self(x25519::StaticSecret::from(bytes))
    }
}
impl TryFrom<&[u8]> for DeviceGroupKey {
    type Error = TryFromSliceError;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        Ok(Self::from(TryInto::<[u8; Self::LENGTH]>::try_into(bytes)?))
    }
}
#[cfg(feature = "cli")]
impl From<&RawDeviceGroupKey> for DeviceGroupKey {
    fn from(device_group_key: &RawDeviceGroupKey) -> Self {
        Self(device_group_key.0.clone())
    }
}

/// Also the Device Group Key but [`Clone`].
///
/// For example needed for the CLI which requires this struct to be [`Clone`] but we don't want
/// [`DeviceGroupKey`] to be [`Clone`].
#[cfg(feature = "cli")]
#[derive(Clone, Educe, ZeroizeOnDrop)]
#[educe(Debug)]
pub struct RawDeviceGroupKey(#[educe(Debug(method(debug_static_secret)))] x25519::StaticSecret);
#[cfg(feature = "cli")]
impl RawDeviceGroupKey {
    /// Convert a hex string to a [`DeviceGroupKey`].
    ///
    /// # Errors
    ///
    /// Returns a string describing the error.
    #[cfg(any(test, feature = "cli"))]
    pub fn from_hex(string: &str) -> anyhow::Result<Self> {
        use anyhow::Context as _;

        let bytes = HEXLOWER_PERMISSIVE.decode(string.as_bytes())?;
        let bytes: [u8; DeviceGroupKey::LENGTH] = bytes.as_slice().try_into().context(format!(
            "must be {} bytes, got {}",
            DeviceGroupKey::LENGTH,
            bytes.len()
        ))?;
        Ok(Self(x25519::StaticSecret::from(bytes)))
    }
}

/// Remote Secret Hash (RSH) derived from a Remote Secret (RS).
#[derive(Clone, ConstantTimeEq, Name)]
pub struct RemoteSecretHash(pub [u8; Self::LENGTH]);
impl RemoteSecretHash {
    /// Byte length of the remote secret hash.
    pub const LENGTH: usize = 32;

    /// Convert a hex string to a [`RemoteSecretHash`].
    ///
    /// # Errors
    ///
    /// Returns a string describing the error.
    #[cfg(feature = "cli")]
    pub fn from_hex(string: &str) -> anyhow::Result<Self> {
        use anyhow::Context as _;

        let bytes = HEXLOWER_PERMISSIVE.decode(string.as_bytes())?;
        let bytes: [u8; Self::LENGTH] = bytes.as_slice().try_into().context(format!(
            "must be {} bytes, got {}",
            Self::LENGTH,
            bytes.len()
        ))?;
        Ok(Self(bytes))
    }

    /// Derive the associated Remote Secret Hash tied to an identity (RSHID).
    #[expect(clippy::missing_panics_doc, reason = "Panic will never happen")]
    #[must_use]
    pub fn derive_for_identity(&self, identity: ThreemaId) -> RemoteSecretHashForIdentity {
        let input: [u8; Self::LENGTH + ThreemaId::LENGTH] = concat_fixed_bytes!(self.0, identity.to_bytes());
        RemoteSecretHashForIdentity(
            blake2b::Blake2bMac256::new_with_salt_and_personal(Some(&input), b"rshid", b"3ma-rs")
                .expect("Blake2bMac256 failed")
                .finalize_fixed()
                .into(),
        )
    }
}
impl TryFrom<&[u8]> for RemoteSecretHash {
    type Error = TryFromSliceError;

    fn try_from(remote_secret_hash: &[u8]) -> Result<Self, Self::Error> {
        Ok(Self(<[u8; Self::LENGTH]>::try_from(remote_secret_hash)?))
    }
}
impl fmt::Display for RemoteSecretHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&HEXLOWER_PERMISSIVE.encode(&self.0))
    }
}
impl fmt::Debug for RemoteSecretHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple(Self::NAME)
            .field(&self.to_string())
            .finish()
    }
}

/// Remote Secret Hash tied to an identity (RSHID) derived from a Remote Secret Hash (RSH).
#[derive(Clone, ConstantTimeEq, Name)]
pub struct RemoteSecretHashForIdentity(pub [u8; Self::LENGTH]);
impl RemoteSecretHashForIdentity {
    /// Byte length of the remote secret hash tied to an identity.
    pub const LENGTH: usize = 32;
}
impl TryFrom<&[u8]> for RemoteSecretHashForIdentity {
    type Error = TryFromSliceError;

    fn try_from(remote_secret_hash_for_identity: &[u8]) -> Result<Self, Self::Error> {
        Ok(Self(<[u8; Self::LENGTH]>::try_from(
            remote_secret_hash_for_identity,
        )?))
    }
}
impl fmt::Display for RemoteSecretHashForIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&HEXLOWER_PERMISSIVE.encode(&self.0))
    }
}
impl fmt::Debug for RemoteSecretHashForIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple(Self::NAME)
            .field(&self.to_string())
            .finish()
    }
}

/// A Remote Secret (RS).
///
/// Concrete usage depends on the implementation:
///
/// - Android/Desktop: Should be used to derive a key to be able to encrypt/decrypt the _intermediate key
///   storage_ that is sandwiched between any outer protection by the platform or a custom passphrase and the
///   keys protected by it.
/// - iOS: Should be used to derive the Wonky Field Cipher Key (WFCK) that can then be used to encrypt/decrypt
///   various pieces of data stored in the iOS keychain services as well as files on disk and protecting
///   _some_ fields in the database (hence it's name).
///
/// Note: An implementation must always make some kind of derivation and not use the secret as-is.
#[derive(Educe, ZeroizeOnDrop)]
#[educe(Debug)]
pub struct RemoteSecret(pub [u8; Self::LENGTH]);
impl RemoteSecret {
    /// Byte length of the remote secret.
    pub const LENGTH: usize = 32;

    /// Sample a random Remote Secret.
    #[must_use]
    pub fn random() -> Self {
        let mut remote_secret = [0_u8; Self::LENGTH];
        rand::thread_rng().fill(&mut remote_secret);
        Self(remote_secret)
    }

    /// Derive the associated Remote Secret Hash (RSH).
    #[expect(clippy::missing_panics_doc, reason = "Panic will never happen")]
    #[must_use]
    pub fn derive_hash(&self) -> RemoteSecretHash {
        RemoteSecretHash(
            blake2b::Blake2bMac256::new_with_salt_and_personal(Some(&self.0), b"rsh", b"3ma-rs")
                .expect("Blake2bMac256 failed")
                .finalize_fixed()
                .into(),
        )
    }

    /// Derive the Wonky Field Cipher Key (WFCK).
    #[expect(clippy::missing_panics_doc, reason = "Panic will never happen")]
    #[must_use]
    pub fn wonky_field_cipher_key(&self) -> WonkyFieldCipherKey {
        WonkyFieldCipherKey(
            blake2b::Blake2bMac256::new_with_salt_and_personal(Some(&self.0), b"wfck", b"3ma-rs")
                .expect("Blake2bMac256 failed")
                .finalize_fixed()
                .into(),
        )
    }
}
impl TryFrom<&[u8]> for RemoteSecret {
    type Error = TryFromSliceError;

    fn try_from(remote_secret: &[u8]) -> Result<Self, Self::Error> {
        Ok(Self(<[u8; Self::LENGTH]>::try_from(remote_secret)?))
    }
}

/// Wonky Field Cipher Key (WFCK).
///
/// This key is derived from the [`RemoteSecret`] and used solely on iOS for the wonky field encryption.
#[derive(ZeroizeOnDrop)]
pub struct WonkyFieldCipherKey(pub(crate) [u8; Self::LENGTH]);
impl WonkyFieldCipherKey {
    /// Byte length of the Wonky Field Cipher Key.
    pub const LENGTH: usize = 32;
}

/// Remote Secret Authentication Token (RSAT) associated to a Remote Secret (RS).
#[derive(Clone, ZeroizeOnDrop)]
pub struct RemoteSecretAuthenticationToken(pub [u8; Self::LENGTH]);
impl RemoteSecretAuthenticationToken {
    /// Byte length of the remote secret hash.
    pub const LENGTH: usize = 32;

    /// Convert a hex string to a [`RemoteSecretAuthenticationToken`].
    ///
    /// # Errors
    ///
    /// Returns a string describing the error.
    #[cfg(feature = "cli")]
    pub fn from_hex(string: &str) -> anyhow::Result<Self> {
        use anyhow::Context as _;

        let bytes = HEXLOWER_PERMISSIVE.decode(string.as_bytes())?;
        let bytes: [u8; Self::LENGTH] = bytes.as_slice().try_into().context(format!(
            "must be {} bytes, got {}",
            Self::LENGTH,
            bytes.len()
        ))?;
        Ok(Self(bytes))
    }
}
impl TryFrom<&[u8]> for RemoteSecretAuthenticationToken {
    type Error = TryFromSliceError;

    fn try_from(authentication_token: &[u8]) -> Result<Self, Self::Error> {
        Ok(Self(<[u8; Self::LENGTH]>::try_from(authentication_token)?))
    }
}

/// A salt used to derive the [`DataBackupRootKey`] from the data backup password.
#[derive(Name, ZeroizeOnDrop)]
pub(crate) struct DataBackupSalt(pub(crate) [u8; Self::LENGTH]);
impl DataBackupSalt {
    /// Byte length of a data backup salt.
    pub(crate) const LENGTH: usize = 32;

    pub(crate) fn random() -> Self {
        let mut salt = [0_u8; Self::LENGTH];
        rand::thread_rng().fill(&mut salt);
        Self(salt)
    }
}

/// Key to encrypt the layout of the data backup.
#[derive(Name, ZeroizeOnDrop)]
pub(crate) struct DataBackupLayoutKey(pub(crate) chacha20::ChaCha20Poly1305);

/// Key to encrypt pages of the data backup.
#[derive(ZeroizeOnDrop)]
pub(crate) struct DataBackupPageKey(pub(crate) chacha20::ChaCha20Poly1305);

/// The data backup root key derived from the data backup password.
///
/// This key is used to derive the [`DataBackupLayoutKey`] and the [`DataBackupPageKey`].
#[derive(ZeroizeOnDrop)]
pub(crate) struct DataBackupRootKey([u8; Self::LENGTH]);
impl DataBackupRootKey {
    /// Byte length of the data backup root key.
    pub(crate) const LENGTH: usize = 32;

    #[expect(clippy::needless_pass_by_value, reason = "Prevent reuse")]
    pub(crate) fn from_password(password: String, salt: &DataBackupSalt) -> Self {
        let mut root_key = [0; Self::LENGTH];
        argon2::Argon2::new(
            argon2::Argon2id,
            argon2::Version::V0x13,
            argon2::Params::new(
                128 * 1024, // 128 MiB memory
                8,          // iterations
                1,          // No parallelization
                Some(Self::LENGTH),
            )
            .expect("Parameters are within required bounds"),
        )
        .hash_password_into(password.as_bytes(), &salt.0, &mut root_key)
        .expect("Argon2id does not fail with controlled inputs");

        Self(root_key)
    }

    fn derive_key(&self, salt: &[u8]) -> chacha20::ChaCha20Poly1305 {
        let derived_key: [u8; chacha20::KEY_LENGTH] =
            blake2b::Blake2bMac256::new_with_salt_and_personal(Some(&self.0), salt, b"3ma-bup")
                .expect("Blake2bMac256 failed")
                .finalize_fixed()
                .into();
        chacha20::ChaCha20Poly1305::new(&derived_key.into())
    }

    pub(crate) fn layout_key(&self) -> DataBackupLayoutKey {
        DataBackupLayoutKey(self.derive_key(b"layout"))
    }

    pub(crate) fn page_key(&self) -> DataBackupPageKey {
        DataBackupPageKey(self.derive_key(b"page"))
    }
}

#[cfg(test)]
mod tests {
    use libthreema_dependencies::crypto::x25519;
    use rstest::rstest;

    use crate::{
        common::{
            Nonce,
            keys::{ClientKey, DeviceGroupKey, DeviceGroupPathKey, PublicKey},
        },
        crypto::{DecryptionFailed, aead::AeadRandomNonceAhead as _},
    };

    #[rstest]
    #[case(PublicKey::from([0; PublicKey::LENGTH]), PublicKey::from([0; PublicKey::LENGTH]))]
    #[case(PublicKey::from([0; PublicKey::LENGTH]), PublicKey::from([1; PublicKey::LENGTH]))]
    #[case(PublicKey::from([1; PublicKey::LENGTH]), PublicKey::from([0; PublicKey::LENGTH]))]
    fn client_key_derive_csp_authentication_key_non_contributory(
        #[case] permanent_server_key: PublicKey,
        #[case] temporary_server_key: PublicKey,
    ) {
        assert!(
            ClientKey::from([1; PublicKey::LENGTH])
                .derive_csp_authentication_key(&permanent_server_key, &temporary_server_key,)
                .is_none()
        );
    }

    #[test]
    fn client_key_derive_directory_authentication_key_non_contributory() {
        assert!(
            ClientKey::from([1; PublicKey::LENGTH])
                .derive_directory_authentication_key(&PublicKey::from([0; PublicKey::LENGTH]))
                .is_none()
        );
    }

    #[test]
    fn client_key_derive_work_directory_authentication_key_non_contributory() {
        assert!(
            ClientKey::from([1; PublicKey::LENGTH])
                .derive_work_directory_authentication_key(&PublicKey::from([0; PublicKey::LENGTH]))
                .is_none()
        );
    }

    /// Patch P5: what `seal_reflected` seals opens with the line libthreema's own code would run for DGRK
    /// (`reflect_key()` and the nonce-ahead decrypt), nonce and bytes unchanged.
    #[test]
    fn p5_seal_reflected_opens_with_libthreemas_reflect_key() {
        let key = DeviceGroupKey::from([0x5a; DeviceGroupKey::LENGTH]);
        let plaintext = b"an envelope's padded bytes".to_vec();
        let (nonce, mut sealed) = key.seal_reflected(plaintext.clone()).unwrap();
        assert_eq!(sealed.len(), Nonce::LENGTH + 16 + plaintext.len(), "nonce || tag || ciphertext");
        assert!(sealed[..Nonce::LENGTH] == nonce.0, "the nonce is ahead");
        let opened_nonce = key
            .reflect_key()
            .0
            .decrypt_in_place_random_nonce_ahead(b"", &mut sealed)
            .unwrap();
        assert!(Nonce::from(opened_nonce) == nonce, "the same nonce");
        assert!(sealed == plaintext, "the same bytes");
    }

    /// Patch P5: `seal_transaction_scope` opens with the line of libthreema's begin-transaction task
    /// (`transaction_scope_key()` and the nonce-ahead decrypt), and the device info sealed with the line of
    /// libthreema's `ClientHello` (`device_info_key()` and the nonce-ahead encrypt) opens with
    /// `open_device_info`.
    #[test]
    fn p5_scope_and_device_info_cross_libthreemas_lines() {
        let key = DeviceGroupKey::from([0x33; DeviceGroupKey::LENGTH]);
        let scope = b"an encoded transaction scope".to_vec();
        let mut sealed = key.seal_transaction_scope(scope.clone()).unwrap();
        let _ = key
            .transaction_scope_key()
            .0
            .decrypt_in_place_random_nonce_ahead(b"", &mut sealed)
            .unwrap();
        assert!(sealed == scope, "the scope opens with DGTSK");

        let device_info = b"an encoded, padded device info".to_vec();
        let mut sealed = device_info.clone();
        let _ = key
            .device_info_key()
            .0
            .encrypt_in_place_random_nonce_ahead(b"", &mut sealed)
            .unwrap();
        assert!(key.open_device_info(&sealed).unwrap() == device_info, "the device info opens with DGDIK");
    }

    /// Patch P5: every `open_*` refuses a box of another key, of another derived key of the same group, one
    /// flipped byte anywhere (nonce, tag, ciphertext), and inputs shorter than a nonce and a tag (0, 24, 39
    /// bytes).
    #[test]
    fn p5_opens_refuse_a_wrong_key_a_flipped_byte_and_short_input() {
        type Open = fn(&DeviceGroupKey, &[u8]) -> Result<Vec<u8>, DecryptionFailed>;
        let opens: [(&str, Open); 3] = [
            ("reflected", |key, data| key.open_reflected(data).map(|(_, bytes)| bytes)),
            ("transaction scope", DeviceGroupKey::open_transaction_scope),
            ("device info", DeviceGroupKey::open_device_info),
        ];
        let key = DeviceGroupKey::from([0x11; DeviceGroupKey::LENGTH]);
        let other = DeviceGroupKey::from([0x12; DeviceGroupKey::LENGTH]);
        let plaintext = b"0123456789abcdef".to_vec();
        let seal = |key: &DeviceGroupKey, which: usize| -> Vec<u8> {
            let mut buffer = plaintext.clone();
            let _ = match which {
                0 => key.reflect_key().0.encrypt_in_place_random_nonce_ahead(b"", &mut buffer),
                1 => key.transaction_scope_key().0.encrypt_in_place_random_nonce_ahead(b"", &mut buffer),
                _ => key.device_info_key().0.encrypt_in_place_random_nonce_ahead(b"", &mut buffer),
            }
            .unwrap();
            buffer
        };
        for (which, (name, open)) in opens.iter().enumerate() {
            let sealed = seal(&key, which);
            assert!(open(&key, &sealed).unwrap() == plaintext, "{name}: its own box opens");
            assert!(open(&other, &seal(&other, which)).is_ok(), "{name}: the other group's own box opens");
            assert_eq!(open(&other, &sealed), Err(DecryptionFailed), "{name}: another group's key");
            for (index, _) in sealed.iter().enumerate() {
                let mut flipped = sealed.clone();
                flipped[index] ^= 0x01;
                assert_eq!(open(&key, &flipped), Err(DecryptionFailed), "{name}: byte {index} flipped");
            }
            for length in [0, Nonce::LENGTH, Nonce::LENGTH + 15] {
                assert_eq!(open(&key, &sealed[..length]), Err(DecryptionFailed), "{name}: {length} bytes");
            }
            for (other_which, (other_name, _)) in opens.iter().enumerate() {
                if other_which != which {
                    assert_eq!(
                        open(&key, &seal(&key, other_which)),
                        Err(DecryptionFailed),
                        "{name} must not open a box of the {other_name} key"
                    );
                }
            }
        }
    }

    #[test]
    fn device_group_path_key_derive_authentication_cipher_non_contributory() {
        assert!(
            DeviceGroupPathKey(x25519::StaticSecret::from([1_u8; DeviceGroupPathKey::LENGTH]))
                .authentication_cipher(&PublicKey::from([0; PublicKey::LENGTH]))
                .is_none()
        );
    }
}

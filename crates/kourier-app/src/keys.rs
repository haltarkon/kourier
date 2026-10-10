//! Keys at rest (§5.2): the master key, the `core.db` key, per-account `key.wrap` files and
//! account secrets.
//!
//! - Master key: 32 random bytes in the Secret Service under `{application, kind: master-key,
//!   profile}`, or, in passphrase mode, Argon2id(passphrase, salt) with salt and parameters in
//!   `kourier.toml` ([`PassphraseConfig`]). There is no plaintext fallback.
//! - `core.db` key = HKDF-SHA256(master, info = `"kourier core.db v1"`).
//! - Per account: a random `backend.db` key lives only in `accounts/<uuid>/key.wrap`, as
//!   AES-256-GCM(KEK, random nonce, db_key) with the file header as associated data, where
//!   KEK = HKDF-SHA256(ikm = master, salt = account secret, info = `"kourier account kek v1" ‖
//!   uuid`) and `uuid` is the hyphenated lowercase text that names the account directory. The
//!   account secret is 32 random bytes in its own Secret Service item `{application, kind:
//!   account-secret, account: <uuid>, profile}`; in passphrase mode it sits in the `key.wrap`
//!   header instead.
//! - Removing an account deletes its secret first ([`Keyring::shred_account`]): from then on
//!   its `key.wrap` cannot be opened, even with the master key.
//!
//! `key.wrap` v1 layout: `KOURWRAP`, version (1), mode (1 = Secret Service, 2 = passphrase),
//! 16-byte uuid, network name (u8 length + ASCII), [32-byte account secret in passphrase mode]
//! — that is the header — then a 12-byte nonce and the 48-byte ciphertext.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use hkdf::Hkdf;
use kourier_desktop::secret::{SecretError, SecretService};
use kourier_model::{AccountUuid, NetworkKind};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use zeroize::Zeroizing;

use crate::paths::create_private_dir;

/// The `application` attribute of every Kourier Secret Service item: the application id,
/// defined once in `kourier_desktop` (architecture §8.10, R6).
pub use crate::desktop::APP_ID;
/// File name of an account's key wrap.
pub const KEY_WRAP_FILE: &str = "key.wrap";

const WRAP_MAGIC: [u8; 8] = *b"KOURWRAP";
const WRAP_VERSION: u8 = 1;
const MODE_SECRET_SERVICE: u8 = 1;
const MODE_PASSPHRASE: u8 = 2;
const NONCE_LEN: usize = 12;
const WRAPPED_LEN: usize = 32 + 16;
const CORE_DB_INFO: &[u8] = b"kourier core.db v1";
const ACCOUNT_KEK_INFO: &[u8] = b"kourier account kek v1";
const PASSPHRASE_CHECK_INFO: &[u8] = b"kourier passphrase check v1";

/// A 32-byte key, zeroized on drop. `Debug` is redacted; deref gives the raw bytes for the
/// composition root to hand to `kourier_db::DbKey` or `kourier_backend_api::DbKey`. Never log
/// them.
#[derive(Clone)]
pub struct Key32(Zeroizing<[u8; 32]>);

impl Key32 {
    fn new(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }
}

impl std::ops::Deref for Key32 {
    type Target = [u8; 32];

    fn deref(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for Key32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Key32(<redacted>)")
    }
}

/// Key management errors. None of them carries key material.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    /// The Secret Service failed (unavailable, locked, dismissed, …).
    #[error(transparent)]
    Secret(#[from] SecretError),
    /// The profile has data but the Secret Service has no master key for it. Never answered by
    /// creating a new key: that would orphan every database.
    #[error("the master key is missing from the Secret Service, but this profile has data")]
    MasterKeyMissing,
    /// The profile has data but its development keyring file is gone (debug builds,
    /// `KOURIER_DEV_KEYRING`). Never answered by creating a new key: the account
    /// directories made with the old one would not open, and a start that cannot open them
    /// must not be the one that decides what happens to them.
    #[error(
        "the development keyring file (config/dev-keyring.json) is missing, but this profile has data; no new key was made. Restore the whole profile directory from a backup"
    )]
    DevKeyringMissing,
    /// A Secret Service item does not hold 32 bytes.
    #[error("the Secret Service holds a malformed {0}")]
    MalformedSecret(&'static str),
    /// The account secret is gone: the account was removed (shredded).
    #[error("the account secret is missing; the account was removed")]
    AccountSecretMissing,
    /// `key.wrap` did not authenticate: wrong master key, wrong account secret, or tampering.
    #[error("key.wrap cannot be decrypted")]
    Unwrap,
    /// `key.wrap` is not a v1 key wrap.
    #[error("key.wrap is malformed: {0}")]
    BadWrap(&'static str),
    /// `key.wrap` names another account than its directory.
    #[error("key.wrap belongs to another account")]
    WrongAccount,
    /// The account directory already has a `key.wrap`.
    #[error("the account already has a key.wrap")]
    WrapExists,
    /// A `key.wrap` written in the other key mode.
    #[error("key.wrap was written in {0} mode")]
    ModeMismatch(&'static str),
    /// The passphrase does not derive this profile's master key.
    #[error("wrong passphrase")]
    WrongPassphrase,
    /// Invalid Argon2id parameters.
    #[error("invalid passphrase parameters: {0}")]
    Kdf(String),
    /// File system error.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Where account secrets live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyMode {
    /// Master key and account secrets in the Secret Service.
    SecretService,
    /// Master key from a passphrase; account secrets in their `key.wrap` headers.
    Passphrase,
}

impl KeyMode {
    fn name(self) -> &'static str {
        match self {
            KeyMode::SecretService => "Secret Service",
            KeyMode::Passphrase => "passphrase",
        }
    }
}

/// The Secret Service operations key management needs; implemented by
/// [`kourier_desktop::secret::SecretService`] and by test doubles.
pub trait SecretStore {
    /// The secret of the item matching `attributes`.
    fn lookup(
        &self,
        attributes: &[(&str, &str)],
    ) -> impl Future<Output = Result<Option<Zeroizing<Vec<u8>>>, SecretError>>;
    /// Stores `secret`, replacing an item with the same attributes.
    fn store(
        &self,
        label: &str,
        attributes: &[(&str, &str)],
        secret: &[u8],
    ) -> impl Future<Output = Result<(), SecretError>>;
    /// Deletes every item matching `attributes`.
    fn delete(
        &self,
        attributes: &[(&str, &str)],
    ) -> impl Future<Output = Result<usize, SecretError>>;
}

impl SecretStore for SecretService {
    fn lookup(
        &self,
        attributes: &[(&str, &str)],
    ) -> impl Future<Output = Result<Option<Zeroizing<Vec<u8>>>, SecretError>> {
        SecretService::lookup(self, attributes)
    }

    fn store(
        &self,
        label: &str,
        attributes: &[(&str, &str)],
        secret: &[u8],
    ) -> impl Future<Output = Result<(), SecretError>> {
        SecretService::store(self, label, attributes, secret)
    }

    fn delete(
        &self,
        attributes: &[(&str, &str)],
    ) -> impl Future<Output = Result<usize, SecretError>> {
        SecretService::delete(self, attributes)
    }
}

impl<T: SecretStore + ?Sized> SecretStore for &T {
    fn lookup(
        &self,
        attributes: &[(&str, &str)],
    ) -> impl Future<Output = Result<Option<Zeroizing<Vec<u8>>>, SecretError>> {
        (**self).lookup(attributes)
    }

    fn store(
        &self,
        label: &str,
        attributes: &[(&str, &str)],
        secret: &[u8],
    ) -> impl Future<Output = Result<(), SecretError>> {
        (**self).store(label, attributes, secret)
    }

    fn delete(
        &self,
        attributes: &[(&str, &str)],
    ) -> impl Future<Output = Result<usize, SecretError>> {
        (**self).delete(attributes)
    }
}

/// The store type of a passphrase-only [`Keyring`]: it has no values.
pub enum NoSecretStore {}

impl SecretStore for NoSecretStore {
    async fn lookup(&self, _: &[(&str, &str)]) -> Result<Option<Zeroizing<Vec<u8>>>, SecretError> {
        match *self {}
    }

    async fn store(&self, _: &str, _: &[(&str, &str)], _: &[u8]) -> Result<(), SecretError> {
        match *self {}
    }

    async fn delete(&self, _: &[(&str, &str)]) -> Result<usize, SecretError> {
        match *self {}
    }
}

/// Argon2id cost parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    /// Memory in KiB.
    pub m_kib: u32,
    /// Passes.
    pub t: u32,
    /// Lanes.
    pub p: u32,
}

impl Default for KdfParams {
    /// 64 MiB, 3 passes, 4 lanes: RFC 9106's second recommended option (§4), for when 2 GiB
    /// is too much memory.
    fn default() -> Self {
        Self {
            m_kib: 64 * 1024,
            t: 3,
            p: 4,
        }
    }
}

/// What passphrase mode keeps in `kourier.toml`: no secret, only the salt, the cost and a
/// check value that tells a wrong passphrase apart before any database is touched.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassphraseConfig {
    /// Random Argon2id salt.
    #[serde(with = "hex16")]
    pub salt: [u8; 16],
    /// Argon2id cost.
    pub kdf: KdfParams,
    /// HKDF(master, `"kourier passphrase check v1"`), first 16 bytes.
    #[serde(with = "hex16")]
    pub check: [u8; 16],
}

impl fmt::Debug for PassphraseConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PassphraseConfig")
            .field("kdf", &self.kdf)
            .finish_non_exhaustive()
    }
}

/// The master key, zeroized on drop.
struct MasterKey(Key32);

impl MasterKey {
    fn generate() -> Self {
        Self(random32())
    }

    fn from_secret(bytes: &[u8]) -> Option<Self> {
        let arr: [u8; 32] = bytes.try_into().ok()?;
        Some(Self(Key32::new(arr)))
    }

    /// HKDF-SHA256 with this key as input key material.
    fn derive(&self, salt: Option<&[u8]>, info: &[&[u8]]) -> Key32 {
        let mut out = Key32::new([0u8; 32]);
        // 32 bytes is far below HKDF-SHA256's 8160-byte limit, so expand cannot fail.
        let _ = Hkdf::<Sha256>::new(salt, &self.0[..]).expand_multi_info(info, out.0.as_mut());
        out
    }
}

/// An unwrapped account key.
pub struct UnwrappedAccount {
    /// From the header.
    pub uuid: AccountUuid,
    /// From the header.
    pub network: NetworkKind,
    /// The `backend.db` key.
    pub db_key: Key32,
}

impl fmt::Debug for UnwrappedAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnwrappedAccount")
            .field("uuid", &self.uuid)
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

/// The readable part of a `key.wrap` (rebuilding `core.db`, §4.3 step 8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrapHeader {
    /// The account.
    pub uuid: AccountUuid,
    /// Its network.
    pub network: NetworkKind,
    /// Where its account secret lives.
    pub mode: KeyMode,
}

/// The unlocked keys of one profile.
pub struct Keyring<S> {
    master: MasterKey,
    mode: Mode<S>,
}

enum Mode<S> {
    SecretService { store: S, profile: String },
    Passphrase,
}

impl<S> fmt::Debug for Keyring<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Keyring")
            .field("mode", &self.mode())
            .finish_non_exhaustive()
    }
}

impl<S> Keyring<S> {
    /// Secret Service or passphrase mode.
    pub fn mode(&self) -> KeyMode {
        match self.mode {
            Mode::SecretService { .. } => KeyMode::SecretService,
            Mode::Passphrase => KeyMode::Passphrase,
        }
    }

    /// The `core.db` key.
    pub fn core_db_key(&self) -> Key32 {
        self.master.derive(None, &[CORE_DB_INFO])
    }

    /// Passphrase mode for a new profile: a fresh salt; the returned config goes into
    /// `kourier.toml`.
    pub fn create_passphrase(
        passphrase: &[u8],
        kdf: KdfParams,
    ) -> Result<(Self, PassphraseConfig), KeyError> {
        let mut salt = [0u8; 16];
        rand::rng().fill_bytes(&mut salt);
        let master = argon2id(passphrase, &salt, kdf)?;
        let check = passphrase_check(&master);
        let keyring = Self {
            master,
            mode: Mode::Passphrase,
        };
        Ok((keyring, PassphraseConfig { salt, kdf, check }))
    }

    /// Passphrase mode for an existing profile; [`KeyError::WrongPassphrase`] when the check
    /// value does not match.
    pub fn with_passphrase(passphrase: &[u8], config: &PassphraseConfig) -> Result<Self, KeyError> {
        let master = argon2id(passphrase, &config.salt, config.kdf)?;
        if passphrase_check(&master) != config.check {
            return Err(KeyError::WrongPassphrase);
        }
        Ok(Self {
            master,
            mode: Mode::Passphrase,
        })
    }
}

impl<S: SecretStore> Keyring<S> {
    /// Secret Service mode: loads this profile's master key. When there is none, a new one is
    /// created only if `fresh_profile` (no `core.db`, no accounts, see
    /// [`Paths::has_data`](crate::paths::Paths::has_data)); otherwise
    /// [`KeyError::MasterKeyMissing`].
    pub async fn with_secret_service(
        store: S,
        profile: &str,
        fresh_profile: bool,
    ) -> Result<Self, KeyError> {
        let attrs = master_attrs(profile);
        let master = match store.lookup(&attrs).await? {
            Some(bytes) => {
                MasterKey::from_secret(&bytes).ok_or(KeyError::MalformedSecret("master key"))?
            }
            None if fresh_profile => {
                let master = MasterKey::generate();
                store
                    .store("Kourier master key", &attrs, &master.0[..])
                    .await?;
                tracing::info!("created a new master key in the Secret Service");
                master
            }
            None => return Err(KeyError::MasterKeyMissing),
        };
        Ok(Self {
            master,
            mode: Mode::SecretService {
                store,
                profile: profile.to_owned(),
            },
        })
    }

    /// Creates `dir` (0700), a fresh account secret and `backend.db` key, and `dir/key.wrap`
    /// (0600, atomic). The secret is stored before the wrap is written, so a crash in between
    /// leaves at most an orphan secret, never a wrap that cannot be opened.
    pub async fn create_account(
        &self,
        dir: &Path,
        uuid: AccountUuid,
        network: NetworkKind,
    ) -> Result<Key32, KeyError> {
        create_private_dir(dir)?;
        let wrap_path = dir.join(KEY_WRAP_FILE);
        if wrap_path.exists() {
            return Err(KeyError::WrapExists);
        }
        let secret = random32();
        let header = match &self.mode {
            Mode::SecretService { store, profile } => {
                let uuid_text = uuid.to_string();
                let label = format!("Kourier account key ({})", uuid.short());
                store
                    .store(&label, &account_attrs(profile, &uuid_text), &secret[..])
                    .await?;
                encode_header(uuid, network, None)
            }
            Mode::Passphrase => encode_header(uuid, network, Some(&secret)),
        };
        let db_key = random32();
        let kek = self.kek(&secret, uuid);
        let mut nonce = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce);
        let wrapped = cipher(&kek)
            .encrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: &db_key[..],
                    aad: &header,
                },
            )
            .map_err(|_| KeyError::Unwrap)?;
        // `encode_header` reserved room for the nonce and ciphertext: appending never
        // reallocates, so no copy of an inline secret is left in freed memory.
        let mut file = header;
        file.extend_from_slice(&nonce);
        file.extend_from_slice(&wrapped);
        write_atomic(&wrap_path, &file)?;
        Ok(db_key)
    }

    /// Unwraps `dir/key.wrap`. [`KeyError::AccountSecretMissing`] after the account was shredded.
    pub async fn open_account(&self, dir: &Path) -> Result<UnwrappedAccount, KeyError> {
        let bytes = Zeroizing::new(std::fs::read(dir.join(KEY_WRAP_FILE))?);
        let parsed = parse_wrap(&bytes)?;
        let dir_uuid = dir
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.parse::<AccountUuid>().ok());
        if dir_uuid.is_some_and(|d| d != parsed.header.uuid) {
            return Err(KeyError::WrongAccount);
        }
        let secret: Key32 = match (&self.mode, parsed.inline_secret) {
            (Mode::SecretService { store, profile }, None) => {
                let uuid_text = parsed.header.uuid.to_string();
                let found = store
                    .lookup(&account_attrs(profile, &uuid_text))
                    .await?
                    .ok_or(KeyError::AccountSecretMissing)?;
                let arr: [u8; 32] = found
                    .as_slice()
                    .try_into()
                    .map_err(|_| KeyError::MalformedSecret("account secret"))?;
                Key32::new(arr)
            }
            (Mode::Passphrase, Some(secret)) => Key32::new(secret),
            (_, None) => return Err(KeyError::ModeMismatch(KeyMode::SecretService.name())),
            (_, Some(_)) => return Err(KeyError::ModeMismatch(KeyMode::Passphrase.name())),
        };
        let kek = self.kek(&secret, parsed.header.uuid);
        let plain = Zeroizing::new(
            cipher(&kek)
                .decrypt(
                    &Nonce::from(parsed.nonce),
                    Payload {
                        msg: parsed.wrapped,
                        aad: parsed.header_bytes,
                    },
                )
                .map_err(|_| KeyError::Unwrap)?,
        );
        let db_key: [u8; 32] = plain
            .as_slice()
            .try_into()
            .map_err(|_| KeyError::BadWrap("key length"))?;
        Ok(UnwrappedAccount {
            uuid: parsed.header.uuid,
            network: parsed.header.network,
            db_key: Key32::new(db_key),
        })
    }

    /// The shred (§4.3 step 7): deletes the account secret, after which `key.wrap` cannot be
    /// opened. In passphrase mode the secret is in `key.wrap`, so the file is deleted. Run it
    /// before deleting the `accounts` row and the directory ([`remove_account_dir`]).
    pub async fn shred_account(&self, dir: &Path, uuid: AccountUuid) -> Result<(), KeyError> {
        match &self.mode {
            Mode::SecretService { store, profile } => {
                let uuid_text = uuid.to_string();
                let n = store.delete(&account_attrs(profile, &uuid_text)).await?;
                tracing::info!(account = %uuid.short(), items = n, "account secret deleted");
            }
            Mode::Passphrase => match std::fs::remove_file(dir.join(KEY_WRAP_FILE)) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                _ => fsync_dir(dir)?,
            },
        }
        Ok(())
    }

    fn kek(&self, secret: &[u8; 32], uuid: AccountUuid) -> Key32 {
        let uuid_text = uuid.to_string();
        self.master
            .derive(Some(secret), &[ACCOUNT_KEK_INFO, uuid_text.as_bytes()])
    }
}

/// Reads the header of a `key.wrap` without any key (network and uuid for a rebuild).
pub fn read_wrap_header(path: &Path) -> Result<WrapHeader, KeyError> {
    let bytes = Zeroizing::new(std::fs::read(path)?);
    Ok(parse_wrap(&bytes)?.header)
}

/// Deletes an account directory (`key.wrap`, `backend.db` with `-wal`/`-shm`, blobs). After
/// [`Keyring::shred_account`].
pub fn remove_account_dir(dir: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn master_attrs(profile: &str) -> [(&'static str, &str); 3] {
    [
        ("application", APP_ID),
        ("kind", "master-key"),
        ("profile", profile),
    ]
}

fn account_attrs<'a>(profile: &'a str, uuid: &'a str) -> [(&'static str, &'a str); 4] {
    [
        ("application", APP_ID),
        ("kind", "account-secret"),
        ("account", uuid),
        ("profile", profile),
    ]
}

fn cipher(key: &[u8; 32]) -> Aes256Gcm {
    Aes256Gcm::new(key.into())
}

fn random32() -> Key32 {
    let mut k = Key32::new([0u8; 32]);
    rand::rng().fill_bytes(k.0.as_mut());
    k
}

fn argon2id(passphrase: &[u8], salt: &[u8; 16], kdf: KdfParams) -> Result<MasterKey, KeyError> {
    let params =
        Params::new(kdf.m_kib, kdf.t, kdf.p, Some(32)).map_err(|e| KeyError::Kdf(e.to_string()))?;
    let mut out = Key32::new([0u8; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase, salt, out.0.as_mut())
        .map_err(|e| KeyError::Kdf(e.to_string()))?;
    Ok(MasterKey(out))
}

fn passphrase_check(master: &MasterKey) -> [u8; 16] {
    let full = master.derive(None, &[PASSPHRASE_CHECK_INFO]);
    let mut check = [0u8; 16];
    check.copy_from_slice(&full[..16]);
    check
}

fn encode_header(
    uuid: AccountUuid,
    network: NetworkKind,
    inline_secret: Option<&Key32>,
) -> Zeroizing<Vec<u8>> {
    let name = network.as_str().as_bytes();
    // Room for the whole file: header, inline secret, nonce and ciphertext.
    let mut h = Zeroizing::new(Vec::with_capacity(
        8 + 3 + 16 + name.len() + 32 + NONCE_LEN + WRAPPED_LEN,
    ));
    h.extend_from_slice(&WRAP_MAGIC);
    h.push(WRAP_VERSION);
    h.push(if inline_secret.is_some() {
        MODE_PASSPHRASE
    } else {
        MODE_SECRET_SERVICE
    });
    h.extend_from_slice(uuid.0.as_bytes());
    // Network names are short ASCII constants (`NetworkKind::as_str`).
    h.push(u8::try_from(name.len()).unwrap_or(u8::MAX));
    h.extend_from_slice(name);
    if let Some(secret) = inline_secret {
        h.extend_from_slice(&secret[..]);
    }
    h
}

struct ParsedWrap<'a> {
    header: WrapHeader,
    header_bytes: &'a [u8],
    inline_secret: Option<[u8; 32]>,
    nonce: [u8; NONCE_LEN],
    wrapped: &'a [u8],
}

fn parse_wrap(bytes: &[u8]) -> Result<ParsedWrap<'_>, KeyError> {
    let mut r = Cursor { bytes, pos: 0 };
    if r.take(8)? != WRAP_MAGIC {
        return Err(KeyError::BadWrap("not a key.wrap"));
    }
    if r.take(1)?[0] != WRAP_VERSION {
        return Err(KeyError::BadWrap("unsupported version"));
    }
    let mode = match r.take(1)?[0] {
        MODE_SECRET_SERVICE => KeyMode::SecretService,
        MODE_PASSPHRASE => KeyMode::Passphrase,
        _ => return Err(KeyError::BadWrap("unknown mode")),
    };
    let uuid_bytes: [u8; 16] = r.array()?;
    let uuid = AccountUuid(uuid::Uuid::from_bytes(uuid_bytes));
    let name_len = usize::from(r.take(1)?[0]);
    let name = r.take(name_len)?;
    let network = NetworkKind::ALL
        .into_iter()
        .find(|n| n.as_str().as_bytes() == name)
        .ok_or(KeyError::BadWrap("unknown network"))?;
    let inline_secret = match mode {
        KeyMode::Passphrase => Some(r.array::<32>()?),
        KeyMode::SecretService => None,
    };
    let header_len = r.pos;
    let nonce: [u8; NONCE_LEN] = r.array()?;
    let wrapped = r.take(WRAPPED_LEN)?;
    if r.pos != bytes.len() {
        return Err(KeyError::BadWrap("trailing bytes"));
    }
    Ok(ParsedWrap {
        header: WrapHeader {
            uuid,
            network,
            mode,
        },
        header_bytes: &bytes[..header_len],
        inline_secret,
        nonce,
        wrapped,
    })
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], KeyError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(KeyError::BadWrap("truncated"))?;
        let out = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], KeyError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }
}

/// tmp file (0600), fsync, rename, fsync of the directory.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let _ = std::fs::remove_file(&tmp);
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        fsync_dir(dir)?;
    }
    Ok(())
}

fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

mod hex16 {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub(super) fn serialize<S: Serializer>(bytes: &[u8; 16], s: S) -> Result<S::Ok, S::Error> {
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        s.serialize_str(&hex)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 16], D::Error> {
        let s = String::deserialize(d)?;
        if s.len() != 32 || !s.is_ascii() {
            return Err(D::Error::custom("expected 32 hex digits"));
        }
        let mut out = [0u8; 16];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)
                .map_err(|_| D::Error::custom("expected 32 hex digits"))?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};
    use std::os::unix::fs::PermissionsExt;

    type Attrs = BTreeSet<(String, String)>;

    /// An in-memory Secret Service: items keyed by their full attribute set; lookups match
    /// items that contain all requested attributes (like the real service).
    #[derive(Default)]
    struct MemoryStore {
        items: RefCell<BTreeMap<Attrs, Vec<u8>>>,
    }

    fn attrs(a: &[(&str, &str)]) -> Attrs {
        a.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    impl SecretStore for MemoryStore {
        async fn lookup(
            &self,
            a: &[(&str, &str)],
        ) -> Result<Option<Zeroizing<Vec<u8>>>, SecretError> {
            let want = attrs(a);
            Ok(self
                .items
                .borrow()
                .iter()
                .find(|(have, _)| want.is_subset(have))
                .map(|(_, v)| Zeroizing::new(v.clone())))
        }

        async fn store(&self, _: &str, a: &[(&str, &str)], s: &[u8]) -> Result<(), SecretError> {
            self.items.borrow_mut().insert(attrs(a), s.to_vec());
            Ok(())
        }

        async fn delete(&self, a: &[(&str, &str)]) -> Result<usize, SecretError> {
            let want = attrs(a);
            let mut items = self.items.borrow_mut();
            let before = items.len();
            items.retain(|have, _| !want.is_subset(have));
            Ok(before - items.len())
        }
    }

    const FAST: KdfParams = KdfParams {
        m_kib: 64,
        t: 1,
        p: 1,
    };

    fn uuid() -> AccountUuid {
        AccountUuid::new_random()
    }

    #[tokio::test]
    async fn master_key_is_created_once_and_found_again() {
        let store = MemoryStore::default();
        assert!(matches!(
            Keyring::with_secret_service(&store, "default", false).await,
            Err(KeyError::MasterKeyMissing)
        ));
        let k1 = Keyring::with_secret_service(&store, "default", true)
            .await
            .unwrap();
        let k2 = Keyring::with_secret_service(&store, "default", false)
            .await
            .unwrap();
        assert_eq!(*k1.core_db_key(), *k2.core_db_key());
        assert_eq!(k1.mode(), KeyMode::SecretService);
        let items = store.items.borrow();
        assert_eq!(items.len(), 1);
        let expected = attrs(&[
            ("application", "app.kourier.Kourier"),
            ("kind", "master-key"),
            ("profile", "default"),
        ]);
        assert!(items.contains_key(&expected), "§5.2 attributes");
        let master = items.get(&expected).unwrap();
        assert_ne!(&master[..], &k1.core_db_key()[..], "core key is derived");
    }

    #[tokio::test]
    async fn profiles_have_separate_master_keys() {
        let store = MemoryStore::default();
        let a = Keyring::with_secret_service(&store, "a", true)
            .await
            .unwrap();
        let b = Keyring::with_secret_service(&store, "b", true)
            .await
            .unwrap();
        assert_ne!(*a.core_db_key(), *b.core_db_key());
    }

    #[tokio::test]
    async fn malformed_master_key_is_refused() {
        let store = MemoryStore::default();
        store
            .store("x", &master_attrs("default"), &[1, 2, 3])
            .await
            .unwrap();
        assert!(matches!(
            Keyring::with_secret_service(&store, "default", true).await,
            Err(KeyError::MalformedSecret(_))
        ));
    }

    #[tokio::test]
    async fn account_wrap_round_trip_and_header() {
        let store = MemoryStore::default();
        let keys = Keyring::with_secret_service(&store, "default", true)
            .await
            .unwrap();
        let dir_root = tempfile::tempdir().unwrap();
        let id = uuid();
        let dir = dir_root.path().join(id.to_string());
        let db_key = keys
            .create_account(&dir, id, NetworkKind::Signal)
            .await
            .unwrap();
        let wrap = dir.join(KEY_WRAP_FILE);
        assert_eq!(
            std::fs::metadata(&wrap).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let bytes = std::fs::read(&wrap).unwrap();
        assert!(
            !bytes.windows(32).any(|w| w == &db_key[..]),
            "key is wrapped"
        );

        let opened = keys.open_account(&dir).await.unwrap();
        assert_eq!(*opened.db_key, *db_key);
        assert_eq!((opened.uuid, opened.network), (id, NetworkKind::Signal));
        assert_eq!(
            read_wrap_header(&wrap).unwrap(),
            WrapHeader {
                uuid: id,
                network: NetworkKind::Signal,
                mode: KeyMode::SecretService
            }
        );
        let expected = attrs(&[
            ("application", "app.kourier.Kourier"),
            ("kind", "account-secret"),
            ("account", &id.to_string()),
            ("profile", "default"),
        ]);
        assert!(
            store.items.borrow().contains_key(&expected),
            "§5.2 attributes"
        );
        assert!(matches!(
            keys.create_account(&dir, id, NetworkKind::Signal).await,
            Err(KeyError::WrapExists)
        ));
    }

    #[tokio::test]
    async fn deleting_the_account_secret_makes_the_wrap_unopenable() {
        let store = MemoryStore::default();
        let keys = Keyring::with_secret_service(&store, "default", true)
            .await
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let (a, b) = (uuid(), uuid());
        let (dir_a, dir_b) = (
            root.path().join(a.to_string()),
            root.path().join(b.to_string()),
        );
        keys.create_account(&dir_a, a, NetworkKind::Signal)
            .await
            .unwrap();
        keys.create_account(&dir_b, b, NetworkKind::Fake)
            .await
            .unwrap();

        keys.shred_account(&dir_a, a).await.unwrap();
        assert!(matches!(
            keys.open_account(&dir_a).await,
            Err(KeyError::AccountSecretMissing)
        ));
        // Even a new secret under the same attributes (the master key alone) cannot open it.
        store
            .store("x", &account_attrs("default", &a.to_string()), &[7; 32])
            .await
            .unwrap();
        assert!(matches!(
            keys.open_account(&dir_a).await,
            Err(KeyError::Unwrap)
        ));
        // The other account is untouched.
        keys.open_account(&dir_b).await.unwrap();

        remove_account_dir(&dir_a).unwrap();
        assert!(!dir_a.exists());
        remove_account_dir(&dir_a).unwrap();
    }

    #[tokio::test]
    async fn tampered_moved_or_foreign_wraps_fail() {
        let store = MemoryStore::default();
        let keys = Keyring::with_secret_service(&store, "default", true)
            .await
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let id = uuid();
        let dir = root.path().join(id.to_string());
        keys.create_account(&dir, id, NetworkKind::Signal)
            .await
            .unwrap();
        let wrap = dir.join(KEY_WRAP_FILE);
        let original = std::fs::read(&wrap).unwrap();

        // Header fields are authenticated: switching the network breaks the tag.
        let mut swapped = original.clone();
        let pos = swapped.windows(6).position(|w| w == b"Signal").unwrap();
        swapped.splice(pos - 1..pos + 6, [4u8, b'F', b'a', b'k', b'e']);
        std::fs::write(&wrap, &swapped).unwrap();
        assert!(matches!(
            keys.open_account(&dir).await,
            Err(KeyError::Unwrap)
        ));

        for i in [original.len() - 1, original.len() - 50] {
            let mut bad = original.clone();
            bad[i] ^= 1;
            std::fs::write(&wrap, &bad).unwrap();
            assert!(matches!(
                keys.open_account(&dir).await,
                Err(KeyError::Unwrap)
            ));
        }
        std::fs::write(&wrap, &original[..original.len() - 1]).unwrap();
        assert!(matches!(
            keys.open_account(&dir).await,
            Err(KeyError::BadWrap(_))
        ));
        std::fs::write(&wrap, &original).unwrap();

        // Copied into another account's directory.
        let other = root.path().join(uuid().to_string());
        std::fs::create_dir(&other).unwrap();
        std::fs::copy(&wrap, other.join(KEY_WRAP_FILE)).unwrap();
        assert!(matches!(
            keys.open_account(&other).await,
            Err(KeyError::WrongAccount)
        ));

        // Another profile's master key, even with this account's secret.
        let foreign = Keyring::with_secret_service(&store, "other", true)
            .await
            .unwrap();
        assert!(matches!(
            foreign.open_account(&dir).await,
            Err(KeyError::AccountSecretMissing)
        ));
        let id_text = id.to_string();
        let secret = store
            .lookup(&account_attrs("default", &id_text))
            .await
            .unwrap()
            .unwrap();
        store
            .store("x", &account_attrs("other", &id_text), &secret)
            .await
            .unwrap();
        assert!(matches!(
            foreign.open_account(&dir).await,
            Err(KeyError::Unwrap)
        ));
    }

    #[tokio::test]
    async fn passphrase_mode() {
        let (keys, config) =
            Keyring::<NoSecretStore>::create_passphrase(b"correct horse", FAST).unwrap();
        assert_eq!(keys.mode(), KeyMode::Passphrase);
        let again = Keyring::<NoSecretStore>::with_passphrase(b"correct horse", &config).unwrap();
        assert_eq!(*keys.core_db_key(), *again.core_db_key());
        assert!(matches!(
            Keyring::<NoSecretStore>::with_passphrase(b"wrong horse", &config),
            Err(KeyError::WrongPassphrase)
        ));

        // The config is what kourier.toml stores: no secret in it.
        let json = serde_json::to_string(&config).unwrap();
        let back: PassphraseConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, config);

        let root = tempfile::tempdir().unwrap();
        let id = uuid();
        let dir = root.path().join(id.to_string());
        let db_key = keys
            .create_account(&dir, id, NetworkKind::Signal)
            .await
            .unwrap();
        assert_eq!(*again.open_account(&dir).await.unwrap().db_key, *db_key);
        assert_eq!(
            read_wrap_header(&dir.join(KEY_WRAP_FILE)).unwrap().mode,
            KeyMode::Passphrase
        );

        // A Secret Service keyring refuses a passphrase-mode wrap, and vice versa.
        let store = MemoryStore::default();
        let ss = Keyring::with_secret_service(&store, "default", true)
            .await
            .unwrap();
        assert!(matches!(
            ss.open_account(&dir).await,
            Err(KeyError::ModeMismatch(_))
        ));

        keys.shred_account(&dir, id).await.unwrap();
        assert!(
            !dir.join(KEY_WRAP_FILE).exists(),
            "the shred deletes the file"
        );
    }
}

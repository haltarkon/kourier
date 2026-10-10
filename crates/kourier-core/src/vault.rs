//! Access to per-account keys and directories (§4.3, §5.1, §5.2), implemented by the app.
//!
//! The core never names `kourier-app`: the composition root implements [`AccountVault`] on top of
//! its keyring (`key.wrap` files and Secret Service account secrets) and injects it. Tests and
//! fixture mode use [`crate::testing::DirVault`].

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use kourier_backend_api::DbKey;
use kourier_model::{AccountUuid, NetworkKind};

/// The future type of every [`AccountVault`] method.
pub type VaultFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, VaultError>> + Send + 'a>>;

/// Why a vault operation failed. Log-safe.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    /// The account has no `key.wrap` (never created, wiped, or removed).
    #[error("the account has no key.wrap")]
    Missing,
    /// The `key.wrap` exists but its account secret is gone: the account was shredded (§5.2).
    #[error("the account secret is missing")]
    SecretMissing,
    /// The `key.wrap` and its secret are there, and the profile's key does not open it: a
    /// master key that is not this profile's (a new one made for a profile that had data, a
    /// directory restored without its keys) or a damaged file. Nothing says the account is
    /// gone, so nothing of it is deleted: the data may open again with the right key.
    #[error("key.wrap does not open with this profile's key")]
    WrongKey,
    /// `create` found an existing `key.wrap`.
    #[error("the account already has a key.wrap")]
    Exists,
    /// File system trouble.
    #[error("account files: {0}")]
    Io(#[from] std::io::Error),
    /// Anything else (Secret Service, keyring locked, …); log-safe text.
    #[error("account vault: {0}")]
    Other(String),
}

/// One `accounts/<uuid>/key.wrap` found on disk (its header names the network).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VaultEntry {
    /// The account uuid.
    pub uuid: AccountUuid,
    /// Its network.
    pub network: NetworkKind,
}

/// Per-account key material and directories (§5.2). Every method is idempotent where that makes
/// sense (wiping or removing twice is not an error).
pub trait AccountVault: Send + Sync + 'static {
    /// Creates `accounts/<uuid>/` (0700) with a fresh account secret and `key.wrap`, and returns
    /// the new `backend.db` key. Fails with [`VaultError::Exists`] if a `key.wrap` is there; a
    /// stale `backend.db` from an earlier link must have been wiped first.
    fn create(&self, uuid: AccountUuid, network: NetworkKind) -> VaultFuture<'_, DbKey>;

    /// Unwraps the `backend.db` key of an existing account.
    fn unwrap(&self, uuid: AccountUuid) -> VaultFuture<'_, DbKey>;

    /// Every `key.wrap` on disk (for rebuilding `core.db`, §4.3 step 8).
    fn list(&self) -> VaultFuture<'_, Vec<VaultEntry>>;

    /// Unlink wipe (§4.3 step 5): deletes the account secret, `backend.db` (with `-wal` and
    /// `-shm`) and `key.wrap`. The directory and the blobs stay (history stays readable).
    fn wipe_credentials(&self, uuid: AccountUuid) -> VaultFuture<'_, ()>;

    /// The shred (§4.3 step 7): deletes the account secret, after which no surviving disk block
    /// of `backend.db` can be decrypted.
    fn shred(&self, uuid: AccountUuid) -> VaultFuture<'_, ()>;

    /// Deletes `accounts/<uuid>/` and everything in it.
    fn remove_dir(&self, uuid: AccountUuid) -> VaultFuture<'_, ()>;

    /// `accounts/<uuid>/backend.db`.
    fn backend_db(&self, uuid: AccountUuid) -> PathBuf;

    /// `accounts/<uuid>/blobs`.
    fn blob_dir(&self, uuid: AccountUuid) -> PathBuf;
}

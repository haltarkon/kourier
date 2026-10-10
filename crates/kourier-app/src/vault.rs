//! [`AppVault`]: the core's [`AccountVault`] (§4.3, §5.2) over this profile's [`Keyring`] and
//! its `accounts/<uuid>/` directories.
//!
//! The keyring is either in Secret Service mode (account secrets in their own Secret Service
//! items) or in passphrase mode (secrets inside `key.wrap`, used by fixture mode and, later, the
//! passphrase dialog). Both are concrete types so the vault's futures are `Send`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kourier_backend_api::DbKey;
use kourier_core::{AccountVault, VaultEntry, VaultError, VaultFuture};
use kourier_desktop::secret::SecretService;
use kourier_model::{AccountUuid, NetworkKind};

use crate::keys::{
    KEY_WRAP_FILE, Key32, KeyError, KeyMode, Keyring, NoSecretStore, read_wrap_header,
    remove_account_dir,
};
use crate::paths::Paths;

/// The unlocked keyring of a profile, in either key mode.
pub enum AnyKeyring {
    /// Master key and account secrets in the Secret Service.
    SecretService(Keyring<SecretService>),
    /// Master key from a passphrase; account secrets in `key.wrap`.
    Passphrase(Keyring<NoSecretStore>),
}

impl std::fmt::Debug for AnyKeyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnyKeyring")
            .field("mode", &self.mode())
            .finish_non_exhaustive()
    }
}

impl AnyKeyring {
    /// Secret Service or passphrase mode.
    pub fn mode(&self) -> KeyMode {
        match self {
            Self::SecretService(k) => k.mode(),
            Self::Passphrase(k) => k.mode(),
        }
    }

    /// The `core.db` key.
    pub fn core_db_key(&self) -> Key32 {
        match self {
            Self::SecretService(k) => k.core_db_key(),
            Self::Passphrase(k) => k.core_db_key(),
        }
    }

    async fn create_account(
        &self,
        dir: &Path,
        uuid: AccountUuid,
        network: NetworkKind,
    ) -> Result<Key32, KeyError> {
        match self {
            Self::SecretService(k) => k.create_account(dir, uuid, network).await,
            Self::Passphrase(k) => k.create_account(dir, uuid, network).await,
        }
    }

    async fn open_account(&self, dir: &Path) -> Result<Key32, KeyError> {
        let unwrapped = match self {
            Self::SecretService(k) => k.open_account(dir).await?,
            Self::Passphrase(k) => k.open_account(dir).await?,
        };
        Ok(unwrapped.db_key)
    }

    async fn shred_account(&self, dir: &Path, uuid: AccountUuid) -> Result<(), KeyError> {
        match self {
            Self::SecretService(k) => k.shred_account(dir, uuid).await,
            Self::Passphrase(k) => k.shred_account(dir, uuid).await,
        }
    }
}

/// The [`AccountVault`] of one profile.
#[derive(Clone, Debug)]
pub struct AppVault {
    keyring: Arc<AnyKeyring>,
    paths: Paths,
}

impl AppVault {
    /// A vault over `keyring` for the accounts below `paths.accounts_dir()`.
    pub fn new(keyring: Arc<AnyKeyring>, paths: Paths) -> Self {
        Self { keyring, paths }
    }

    fn dir(&self, uuid: AccountUuid) -> PathBuf {
        self.paths.account_dir(&uuid)
    }
}

/// Maps a key error to the vault's log-safe error.
fn vault_error(e: KeyError) -> VaultError {
    match e {
        KeyError::Io(e) if e.kind() == io::ErrorKind::NotFound => VaultError::Missing,
        KeyError::Io(e) => VaultError::Io(e),
        KeyError::WrapExists => VaultError::Exists,
        // Only a secret that is truly gone says "shredded" (the core then removes what is
        // left of the account). A wrap that is there with its secret and does not open is
        // another matter: a wrong master key or a damaged file, and nothing may be deleted.
        KeyError::AccountSecretMissing => VaultError::SecretMissing,
        KeyError::Unwrap => VaultError::WrongKey,
        other => VaultError::Other(other.to_string()),
    }
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

impl AccountVault for AppVault {
    fn create(&self, uuid: AccountUuid, network: NetworkKind) -> VaultFuture<'_, DbKey> {
        Box::pin(async move {
            let key = self
                .keyring
                .create_account(&self.dir(uuid), uuid, network)
                .await
                .map_err(vault_error)?;
            Ok(DbKey::new(*key))
        })
    }

    fn unwrap(&self, uuid: AccountUuid) -> VaultFuture<'_, DbKey> {
        Box::pin(async move {
            let key = self
                .keyring
                .open_account(&self.dir(uuid))
                .await
                .map_err(vault_error)?;
            Ok(DbKey::new(*key))
        })
    }

    fn list(&self) -> VaultFuture<'_, Vec<VaultEntry>> {
        Box::pin(async move {
            let dir = self.paths.accounts_dir();
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(VaultError::Io(e)),
            };
            let mut out = Vec::new();
            for entry in entries {
                let entry = entry?;
                let Some(uuid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|n| n.parse::<AccountUuid>().ok())
                else {
                    continue;
                };
                let wrap = entry.path().join(KEY_WRAP_FILE);
                if !wrap.is_file() {
                    continue;
                }
                match read_wrap_header(&wrap) {
                    Ok(h) if h.uuid == uuid => out.push(VaultEntry {
                        uuid,
                        network: h.network,
                    }),
                    Ok(_) => {
                        tracing::warn!(account = %uuid.short(), "key.wrap names another account; ignored");
                    }
                    Err(e) => {
                        tracing::warn!(account = %uuid.short(), error = %e, "unreadable key.wrap; ignored");
                    }
                }
            }
            out.sort_by_key(|e| e.uuid.to_string());
            Ok(out)
        })
    }

    fn wipe_credentials(&self, uuid: AccountUuid) -> VaultFuture<'_, ()> {
        Box::pin(async move {
            let dir = self.dir(uuid);
            self.keyring
                .shred_account(&dir, uuid)
                .await
                .map_err(vault_error)?;
            for name in [
                "backend.db",
                "backend.db-wal",
                "backend.db-shm",
                KEY_WRAP_FILE,
            ] {
                remove_if_exists(&dir.join(name))?;
            }
            Ok(())
        })
    }

    fn shred(&self, uuid: AccountUuid) -> VaultFuture<'_, ()> {
        Box::pin(async move {
            self.keyring
                .shred_account(&self.dir(uuid), uuid)
                .await
                .map_err(vault_error)
        })
    }

    fn remove_dir(&self, uuid: AccountUuid) -> VaultFuture<'_, ()> {
        Box::pin(async move { Ok(remove_account_dir(&self.dir(uuid))?) })
    }

    fn backend_db(&self, uuid: AccountUuid) -> PathBuf {
        self.paths.backend_db(&uuid)
    }

    fn blob_dir(&self, uuid: AccountUuid) -> PathBuf {
        self.paths.blobs_dir(&uuid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::KdfParams;
    use crate::paths::Profile;
    use std::ffi::OsString;

    fn vault(home: &Path) -> AppVault {
        let home = OsString::from(home);
        let paths = Paths::from_env(Profile::default(), |k| {
            (k == crate::paths::HOME_ENV).then(|| home.clone())
        })
        .expect("paths");
        paths.create_dirs().expect("dirs");
        let kdf = KdfParams {
            m_kib: 64,
            t: 1,
            p: 1,
        };
        let (keyring, _) = Keyring::create_passphrase(b"test", kdf).expect("keyring");
        AppVault::new(Arc::new(AnyKeyring::Passphrase(keyring)), paths)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn create_unwrap_list_wipe_shred_remove() {
        let tmp = tempfile::tempdir().expect("tmp");
        let v = vault(tmp.path());
        let uuid = AccountUuid::new_random();
        assert!(matches!(v.unwrap(uuid).await, Err(VaultError::Missing)));
        let key = v.create(uuid, NetworkKind::Fake).await.expect("create");
        assert!(matches!(
            v.create(uuid, NetworkKind::Fake).await,
            Err(VaultError::Exists)
        ));
        assert_eq!(v.unwrap(uuid).await.expect("unwrap").expose(), key.expose());
        assert_eq!(
            v.list().await.expect("list"),
            vec![VaultEntry {
                uuid,
                network: NetworkKind::Fake
            }]
        );
        assert!(v.backend_db(uuid).starts_with(tmp.path()));
        std::fs::write(v.backend_db(uuid), b"x").expect("backend.db");
        std::fs::create_dir_all(v.blob_dir(uuid)).expect("blobs");

        v.wipe_credentials(uuid).await.expect("wipe");
        assert!(!v.backend_db(uuid).exists());
        assert!(v.blob_dir(uuid).exists(), "blobs stay after an unlink");
        assert!(v.list().await.expect("list").is_empty());
        assert!(matches!(v.unwrap(uuid).await, Err(VaultError::Missing)));
        v.wipe_credentials(uuid)
            .await
            .expect("wiping twice is fine");

        // A fresh link of the same account id after the wipe.
        v.create(uuid, NetworkKind::Fake).await.expect("relink");
        v.shred(uuid).await.expect("shred");
        assert!(v.unwrap(uuid).await.is_err());
        v.remove_dir(uuid).await.expect("remove");
        assert!(!v.dir(uuid).exists());
        v.remove_dir(uuid).await.expect("removing twice is fine");
    }

    /// A `key.wrap` made under another master key (a profile restored without its key file, a
    /// new development keyring for a profile that had data) is reported as a wrong key, not
    /// as a missing secret: the core deletes the directory of an account whose secret is
    /// gone, and this one's is not.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn a_wrap_of_another_master_key_is_a_wrong_key_not_a_shredded_account() {
        let tmp = tempfile::tempdir().expect("tmp");
        let made = vault(tmp.path());
        let uuid = AccountUuid::new_random();
        made.create(uuid, NetworkKind::Fake).await.expect("create");
        std::fs::write(made.backend_db(uuid), b"the account's keys").expect("backend.db");

        // The same directories under a freshly made master key.
        let other = vault(tmp.path());
        assert!(matches!(
            other.unwrap(uuid).await,
            Err(VaultError::WrongKey)
        ));
        assert_eq!(
            other.list().await.expect("list").len(),
            1,
            "the account is still listed"
        );
        assert!(other.backend_db(uuid).exists());
        // The right key still opens it.
        made.unwrap(uuid).await.expect("the original key");
    }
}

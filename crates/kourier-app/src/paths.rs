//! Where Kourier keeps its files (§5.1, §9.2).
//!
//! ```text
//! $XDG_DATA_HOME/kourier/            core.db, accounts/<uuid>/{key.wrap, backend.db, blobs/}
//! $XDG_CONFIG_HOME/kourier/          kourier.toml
//! $XDG_STATE_HOME/kourier/logs/      kourier.log
//! $XDG_CACHE_HOME/kourier/outgoing/  archives of folders waiting in the composer (`folders`)
//! $XDG_RUNTIME_DIR/kourier/          single-instance lock, media/ (tmpfs)
//! ```
//!
//! `--profile <name>` uses `kourier-<name>` directories. `KOURIER_HOME=<dir>` puts everything
//! under `<dir>/<kourier or kourier-name>/{data,config,state,cache,run}` (tests, `dev-data/`).

use kourier_model::AccountUuid;
use std::ffi::OsString;
use std::fs::DirBuilder;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Overrides every directory (tests, `dev-data/`).
pub const HOME_ENV: &str = "KOURIER_HOME";

/// The name of the default profile (`kourier` directories).
const DEFAULT_PROFILE: &str = "default";

/// Path resolution errors.
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// A profile name outside `[A-Za-z0-9_-]{1,32}`.
    #[error("invalid profile name (use 1-32 letters, digits, '-' or '_')")]
    InvalidProfile,
    /// Neither `$HOME` nor the XDG variable is set to an absolute path.
    #[error("cannot locate the {0} directory: set HOME or {0}")]
    NoBaseDir(&'static str),
    /// `$XDG_RUNTIME_DIR` is unset; plaintext media must not fall back to a disk directory.
    #[error("XDG_RUNTIME_DIR is not set")]
    NoRuntimeDir,
    /// `KOURIER_HOME` could not be made absolute.
    #[error("invalid {HOME_ENV}: {0}")]
    InvalidHome(io::Error),
}

/// A profile: the default one or a named one (`--profile <name>`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profile(Option<String>);

impl Profile {
    /// `--profile <name>`. `default` names the default profile itself: a separate
    /// `kourier-default` profile would share its Secret Service `profile` attribute.
    pub fn named(name: &str) -> Result<Self, PathError> {
        let ok = (1..=32).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !ok {
            return Err(PathError::InvalidProfile);
        }
        Ok(Self((name != DEFAULT_PROFILE).then(|| name.to_owned())))
    }

    /// `default` for the default profile.
    pub fn name(&self) -> &str {
        self.0.as_deref().unwrap_or(DEFAULT_PROFILE)
    }

    /// `kourier` or `kourier-<name>`.
    pub fn dir_name(&self) -> String {
        match &self.0 {
            None => "kourier".to_owned(),
            Some(name) => format!("kourier-{name}"),
        }
    }
}

/// The resolved directories of one profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    /// `core.db` and `accounts/`.
    pub data: PathBuf,
    /// `kourier.toml`.
    pub config: PathBuf,
    /// Logs and crash reports.
    pub state: PathBuf,
    /// Files that can be made again, on the disk: the archives of folders the composer packs.
    pub cache: PathBuf,
    /// Lock and plaintext media cache (tmpfs).
    pub runtime: PathBuf,
    profile: Profile,
    home: Option<PathBuf>,
}

impl Paths {
    /// Resolves the directories from the process environment.
    pub fn resolve(profile: Profile) -> Result<Self, PathError> {
        Self::from_env(profile, |k| std::env::var_os(k))
    }

    /// Resolves the directories from `env` (testable form of [`resolve`](Self::resolve)).
    pub fn from_env(
        profile: Profile,
        env: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Self, PathError> {
        let dir = profile.dir_name();
        if let Some(home) = env(HOME_ENV).filter(|v| !v.is_empty()) {
            let home = std::path::absolute(PathBuf::from(home)).map_err(PathError::InvalidHome)?;
            let base = home.join(&dir);
            return Ok(Self {
                data: base.join("data"),
                config: base.join("config"),
                state: base.join("state"),
                cache: base.join("cache"),
                runtime: base.join("run"),
                profile,
                home: Some(home),
            });
        }
        // XDG base directories: only absolute values count (XDG spec).
        let abs = |k: &str| env(k).map(PathBuf::from).filter(|p| p.is_absolute());
        let user_home = abs("HOME");
        let base = |var: &'static str, fallback: &str| {
            abs(var)
                .or_else(|| user_home.as_ref().map(|h| h.join(fallback)))
                .ok_or(PathError::NoBaseDir(var))
        };
        Ok(Self {
            data: base("XDG_DATA_HOME", ".local/share")?.join(&dir),
            config: base("XDG_CONFIG_HOME", ".config")?.join(&dir),
            state: base("XDG_STATE_HOME", ".local/state")?.join(&dir),
            cache: base("XDG_CACHE_HOME", ".cache")?.join(&dir),
            runtime: abs("XDG_RUNTIME_DIR")
                .ok_or(PathError::NoRuntimeDir)?
                .join(&dir),
            profile,
            home: None,
        })
    }

    /// The profile these paths belong to.
    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// The `profile` attribute of this profile's Secret Service items (§5.2). Under
    /// `KOURIER_HOME` it also names the directory, so a test or `dev-data/` profile never finds,
    /// replaces or deletes the real profile's master key.
    pub fn secret_profile(&self) -> String {
        match &self.home {
            None => self.profile.name().to_owned(),
            Some(home) => format!("{}@{}", self.profile.name(), home.display()),
        }
    }

    /// `core.db`.
    pub fn core_db(&self) -> PathBuf {
        self.data.join("core.db")
    }

    /// `accounts/`.
    pub fn accounts_dir(&self) -> PathBuf {
        self.data.join("accounts")
    }

    /// `accounts/<uuid>/`.
    pub fn account_dir(&self, uuid: &AccountUuid) -> PathBuf {
        self.accounts_dir().join(uuid.to_string())
    }

    /// `accounts/<uuid>/key.wrap`.
    pub fn key_wrap(&self, uuid: &AccountUuid) -> PathBuf {
        self.account_dir(uuid).join(crate::keys::KEY_WRAP_FILE)
    }

    /// `accounts/<uuid>/backend.db`.
    pub fn backend_db(&self, uuid: &AccountUuid) -> PathBuf {
        self.account_dir(uuid).join("backend.db")
    }

    /// `accounts/<uuid>/blobs/`.
    pub fn blobs_dir(&self, uuid: &AccountUuid) -> PathBuf {
        self.account_dir(uuid).join("blobs")
    }

    /// `kourier.toml`.
    pub fn config_file(&self) -> PathBuf {
        self.config.join("kourier.toml")
    }

    /// The config home the XDG autostart entry goes below (`<it>/autostart/`): the session's
    /// `$XDG_CONFIG_HOME` (or `~/.config`), or this profile's own config directory under
    /// `KOURIER_HOME`, where the entry is inert (tests never touch the user's autostart).
    pub fn autostart_config_home(&self) -> PathBuf {
        match &self.home {
            Some(_) => self.config.clone(),
            None => self
                .config
                .parent()
                .map_or_else(|| self.config.clone(), Path::to_path_buf),
        }
    }

    /// `logs/`.
    pub fn log_dir(&self) -> PathBuf {
        self.state.join("logs")
    }

    /// `media/`, the plaintext cache (§5.8).
    pub fn media_dir(&self) -> PathBuf {
        self.runtime.join("media")
    }

    /// `outgoing/` in the cache directory: where a folder is packed into one archive for the
    /// composer ([`crate::folders::ArchiveFiles`]; on the disk, not in memory).
    pub fn archives_dir(&self) -> PathBuf {
        self.cache.join("outgoing")
    }

    /// Whether this profile already has data: then a missing master key is an error, never a
    /// reason to create a new one.
    pub fn has_data(&self) -> bool {
        self.core_db().exists()
            || std::fs::read_dir(self.accounts_dir()).is_ok_and(|mut d| d.next().is_some())
    }

    /// Creates the data, config, state, cache and runtime directories with mode 0700.
    pub fn create_dirs(&self) -> io::Result<()> {
        for dir in [
            &self.data,
            &self.accounts_dir(),
            &self.config,
            &self.state,
            &self.cache,
            &self.runtime,
        ] {
            create_private_dir(dir)?;
        }
        Ok(())
    }
}

/// The file in the data directory a running Kourier holds an exclusive lock on.
pub const LOCK_FILE: &str = "lock";

/// Why a profile's lock could not be taken.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another process holds it: Kourier already runs on this profile.
    #[error("another Kourier process is using this profile")]
    Held,
    /// The lock file could not be created or locked.
    #[error("cannot lock the profile: {0}")]
    Io(#[from] io::Error),
}

/// One process per profile: an exclusive advisory lock (`flock`) on `data/lock`, held while
/// the value lives and released by the kernel when the process ends, however it ends.
///
/// The desktop's single instance is a D-Bus name (§8.8), which does not exist offscreen,
/// without a session bus or with `KOURIER_DESKTOP=0`. Two processes on one linked profile
/// would use the same device credentials (the server closes the older socket with 4409, and
/// they would take the connection from each other) and advance the same ratchets from two
/// places, which breaks sessions with every contact. The lock is on the data itself, so it
/// holds however the second process was started.
#[derive(Debug)]
pub struct ProfileLock {
    _file: std::fs::File,
}

impl ProfileLock {
    /// Takes the lock of `paths` (creating the data directory, mode 0700, when it is not
    /// there). [`LockError::Held`] when another process has it; never waits.
    pub fn acquire(paths: &Paths) -> Result<Self, LockError> {
        use std::os::unix::fs::OpenOptionsExt as _;
        create_private_dir(&paths.data)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(paths.data.join(LOCK_FILE))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(LockError::Held),
            Err(std::fs::TryLockError::Error(e)) => Err(LockError::Io(e)),
        }
    }
}

/// Creates `dir` (and missing parents) with mode 0700 and tightens an existing `dir` to 0700.
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let mut perms = std::fs::metadata(dir)?.permissions();
    if perms.mode() & 0o777 != 0o700 {
        perms.set_mode(0o700);
        std::fs::set_permissions(dir, perms)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// The lock is exclusive, is not data, and is free again when its holder is gone.
    #[test]
    fn one_holder_per_profile() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = OsString::from(tmp.path());
        let paths = Paths::from_env(Profile::default(), |k| {
            (k == HOME_ENV).then(|| home.clone())
        })
        .expect("paths");
        let first = ProfileLock::acquire(&paths).expect("the first holder");
        assert!(matches!(ProfileLock::acquire(&paths), Err(LockError::Held)));
        assert!(paths.data.join(LOCK_FILE).is_file());
        assert!(
            !paths.has_data(),
            "the lock file does not make a profile count as one with data"
        );
        // Another profile of the same home has a lock of its own.
        let other = Paths::from_env(Profile::named("work").expect("name"), |k| {
            (k == HOME_ENV).then(|| home.clone())
        })
        .expect("paths");
        let _other = ProfileLock::acquire(&other).expect("another profile");
        drop(first);
        let _again = ProfileLock::acquire(&paths).expect("free again");
    }

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn xdg_defaults_and_overrides() {
        let p = Paths::from_env(
            Profile::default(),
            env(&[("HOME", "/home/u"), ("XDG_RUNTIME_DIR", "/run/user/1000")]),
        )
        .unwrap();
        assert_eq!(p.data, Path::new("/home/u/.local/share/kourier"));
        assert_eq!(p.config, Path::new("/home/u/.config/kourier"));
        assert_eq!(p.state, Path::new("/home/u/.local/state/kourier"));
        assert_eq!(p.cache, Path::new("/home/u/.cache/kourier"));
        assert_eq!(
            p.archives_dir(),
            Path::new("/home/u/.cache/kourier/outgoing"),
            "archives are on the disk, not in the runtime directory"
        );
        assert_eq!(p.runtime, Path::new("/run/user/1000/kourier"));
        assert_eq!(p.media_dir(), Path::new("/run/user/1000/kourier/media"));
        assert_eq!(
            p.core_db(),
            Path::new("/home/u/.local/share/kourier/core.db")
        );
        assert_eq!(p.secret_profile(), "default");

        let p = Paths::from_env(
            Profile::named("work").unwrap(),
            env(&[
                ("HOME", "/home/u"),
                ("XDG_DATA_HOME", "/data"),
                ("XDG_CONFIG_HOME", "relative/ignored"),
                ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ]),
        )
        .unwrap();
        assert_eq!(p.data, Path::new("/data/kourier-work"));
        assert_eq!(p.config, Path::new("/home/u/.config/kourier-work"));
        assert_eq!(p.secret_profile(), "work");
        let uuid: AccountUuid = "6f1c1e9a-5d1b-4c3e-9a50-0d4f2b1f7c11".parse().unwrap();
        assert_eq!(
            p.key_wrap(&uuid),
            Path::new("/data/kourier-work/accounts/6f1c1e9a-5d1b-4c3e-9a50-0d4f2b1f7c11/key.wrap")
        );
    }

    #[test]
    fn kourier_home_puts_everything_in_one_directory() {
        let p = Paths::from_env(
            Profile::named("t1").unwrap(),
            env(&[("KOURIER_HOME", "/tmp/kh"), ("HOME", "/home/u")]),
        )
        .unwrap();
        for dir in [&p.data, &p.config, &p.state, &p.cache, &p.runtime] {
            assert!(dir.starts_with("/tmp/kh/kourier-t1"), "{}", dir.display());
        }
        // Never the real profile's Secret Service items.
        assert_eq!(p.secret_profile(), "t1@/tmp/kh");
    }

    #[test]
    fn missing_runtime_dir_and_bad_profiles_are_errors() {
        assert!(matches!(
            Paths::from_env(Profile::default(), env(&[("HOME", "/home/u")])),
            Err(PathError::NoRuntimeDir)
        ));
        for bad in ["", "a/b", "..", "x y", &"n".repeat(33)] {
            assert!(Profile::named(bad).is_err(), "{bad:?}");
        }
        // `--profile default` is the default profile, never a second one with the same
        // Secret Service attribute.
        assert_eq!(Profile::named("default").unwrap(), Profile::default());
        assert_eq!(Profile::named("default").unwrap().dir_name(), "kourier");
    }

    #[test]
    fn create_dirs_is_private() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::from_env(
            Profile::default(),
            env(&[("KOURIER_HOME", tmp.path().to_str().unwrap())]),
        )
        .unwrap();
        assert!(!p.has_data());
        p.create_dirs().unwrap();
        for dir in [
            &p.data,
            &p.config,
            &p.state,
            &p.cache,
            &p.runtime,
            &p.accounts_dir(),
        ] {
            let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", dir.display());
        }
        assert!(!p.has_data(), "empty directories are not data");
    }
}

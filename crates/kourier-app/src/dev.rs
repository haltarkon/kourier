//! Development hooks of debug builds (never compiled into release builds).
//!
//! `KOURIER_DEV_KEYRING=passphrase` together with `KOURIER_HOME`: the profile keeps its
//! master key in a passphrase-mode keyring with a fixed passphrase and a cheap KDF instead of
//! the Secret Service, so scripted runs (`cargo xtask demo`, `cargo xtask smoke`) never add
//! items to the desktop's keyring and the profile directory is the whole login. The
//! passphrase is the public constant below, so the database encryption protects nothing in
//! such a profile: what protects it at rest is the disk it lies on and its 0700 mode. The salt
//! and check value live in `<config>/dev-keyring.json`, so the profile reopens after a
//! restart. That file is made once, for a profile without data: when it is missing from a
//! profile that has data, the start fails ([`KeyError::DevKeyringMissing`]) instead of making
//! a new key, under which the account directories would not open.
//!
//! `KOURIER_DEV_EXPECT_NUMBER_TAIL=<digits>`: the last digits of the phone number of the
//! account a first link must find (`BackendConfig::expect_number_suffix`): the wrong phone
//! scanning the QR code links nothing.
//!
//! `KOURIER_DEV_NO_DUMP=1`: the debug build makes itself non-dumpable like a release build
//! ([`crate::process`]), for runs with a real account.
//!
//! `KOURIER_DEV_DEMO_STATE=challenge|offline|clock`: the state the fake network's demo account
//! ends in once it is linked.

use std::ffi::OsString;
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::keys::{KdfParams, KeyError, Keyring, NoSecretStore, PassphraseConfig};
use crate::paths::{HOME_ENV, Paths};

/// The environment variable that selects the development keyring (`passphrase`).
pub const DEV_KEYRING_ENV: &str = "KOURIER_DEV_KEYRING";

/// The development keyring's settings file, below the profile's config directory.
pub const DEV_KEYRING_FILE: &str = "dev-keyring.json";

/// The environment variable that sets the state the fake network's demo account ends in once
/// it is linked (`challenge`, `offline`, `clock`): for tests that need an account in that
/// state on a real profile, where fixture mode's `--page` does not apply.
#[cfg(feature = "fake")]
pub const DEV_DEMO_STATE_ENV: &str = "KOURIER_DEV_DEMO_STATE";

/// The demo account's end state [`DEV_DEMO_STATE_ENV`] asks for (online without it).
#[cfg(feature = "fake")]
pub fn demo_state(env: impl Fn(&str) -> Option<OsString>) -> crate::FixtureState {
    use crate::FixtureState;
    match env(DEV_DEMO_STATE_ENV).as_deref().and_then(|v| v.to_str()) {
        Some("challenge") => FixtureState::Challenge,
        Some("offline") => FixtureState::Offline,
        Some("clock") => FixtureState::ClockSkew,
        _ => FixtureState::Online,
    }
}

/// The fixed passphrase: these profiles are throwaway test data.
const PASSPHRASE: &[u8] = b"kourier dev profile";

/// Cheap Argon2id settings (nothing here is secret).
const KDF: KdfParams = KdfParams {
    m_kib: 64,
    t: 1,
    p: 1,
};

/// Whether `env` asks for the development keyring: `KOURIER_DEV_KEYRING=passphrase` and a
/// non-empty `KOURIER_HOME` (the user's real profile never gets it).
pub fn wants_dev_keyring(env: impl Fn(&str) -> Option<OsString>) -> bool {
    let home = env(HOME_ENV).is_some_and(|h| !h.is_empty());
    let mode = env(DEV_KEYRING_ENV).is_some_and(|m| m == "passphrase");
    home && mode
}

/// The environment variable with the last digits of the phone number a first link must find.
pub const EXPECT_NUMBER_TAIL_ENV: &str = "KOURIER_DEV_EXPECT_NUMBER_TAIL";

/// The digits [`EXPECT_NUMBER_TAIL_ENV`] names: 2 to 15 of them, nothing else. `Ok(None)`
/// when the variable is unset or empty; a value that is not digits is an error, so a typo
/// never turns the check off silently.
pub fn expected_number_tail(
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<String>, String> {
    let Some(value) = env(EXPECT_NUMBER_TAIL_ENV).filter(|v| !v.trim().is_empty()) else {
        return Ok(None);
    };
    let digits = value.trim();
    if (2..=15).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit()) {
        Ok(Some(digits.to_owned()))
    } else {
        // (The value is part of a phone number: not echoed.)
        Err(format!("{EXPECT_NUMBER_TAIL_ENV} must be 2 to 15 digits"))
    }
}

/// Opens the development keyring of `paths`. It is created only for a profile without data
/// ([`Paths::has_data`]): a profile that has data and no keyring file fails with
/// [`KeyError::DevKeyringMissing`].
pub fn open_keyring(paths: &Paths) -> Result<Keyring<NoSecretStore>, KeyError> {
    open_in(&paths.config, !paths.has_data())
}

fn open_in(config_dir: &Path, fresh_profile: bool) -> Result<Keyring<NoSecretStore>, KeyError> {
    let file = config_dir.join(DEV_KEYRING_FILE);
    match std::fs::read(&file) {
        Ok(bytes) => {
            let config: PassphraseConfig = serde_json::from_slice(&bytes)
                .map_err(|e| KeyError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;
            Keyring::with_passphrase(PASSPHRASE, &config)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound && !fresh_profile => {
            Err(KeyError::DevKeyringMissing)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let (keyring, config) = Keyring::create_passphrase(PASSPHRASE, KDF)?;
            let json = serde_json::to_vec(&config)
                .map_err(|e| KeyError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;
            crate::paths::create_private_dir(config_dir)?;
            let mut f = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&file)?;
            f.write_all(&json)?;
            f.sync_all()?;
            Ok(keyring)
        }
        Err(e) => Err(KeyError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "fake")]
    #[test]
    fn the_demo_account_ends_where_the_environment_says() {
        use crate::FixtureState;
        let with = |value: Option<&'static str>| {
            demo_state(move |k| {
                (k == DEV_DEMO_STATE_ENV)
                    .then_some(value)
                    .flatten()
                    .map(OsString::from)
            })
        };
        assert_eq!(with(None), FixtureState::Online);
        assert_eq!(with(Some("challenge")), FixtureState::Challenge);
        assert_eq!(with(Some("offline")), FixtureState::Offline);
        assert_eq!(with(Some("clock")), FixtureState::ClockSkew);
        // Nothing else changes anything (the logged-out state would make linking pointless).
        assert_eq!(with(Some("loggedout")), FixtureState::Online);
        assert_eq!(with(Some("")), FixtureState::Online);
    }

    #[test]
    fn only_with_a_kourier_home() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == k)
                    .map(|(_, v)| OsString::from(v))
            }
        };
        assert!(wants_dev_keyring(env(&[
            (HOME_ENV, "/x"),
            (DEV_KEYRING_ENV, "passphrase")
        ])));
        assert!(!wants_dev_keyring(env(&[(DEV_KEYRING_ENV, "passphrase")])));
        assert!(!wants_dev_keyring(env(&[
            (HOME_ENV, ""),
            (DEV_KEYRING_ENV, "passphrase")
        ])));
        assert!(!wants_dev_keyring(env(&[
            (HOME_ENV, "/x"),
            (DEV_KEYRING_ENV, "secret")
        ])));
    }

    #[test]
    fn reopens_with_the_same_master_key() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path().join("config");
        let first = open_in(&dir, true).expect("create");
        let again = open_in(&dir, true).expect("reopen");
        assert_eq!(*first.core_db_key(), *again.core_db_key());
        // A profile with data reopens its keyring too.
        let with_data = open_in(&dir, false).expect("reopen with data");
        assert_eq!(*first.core_db_key(), *with_data.core_db_key());
        std::fs::write(dir.join(DEV_KEYRING_FILE), b"{").expect("write");
        assert!(open_in(&dir, true).is_err());
    }

    /// A profile that has data and lost its keyring file (a partial restore, a tidy-up of
    /// `config/`) gets no new key: under one, its account directories would not open, and
    /// the start would treat them as the rest of removed accounts.
    #[test]
    fn a_profile_with_data_never_gets_a_new_key() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = OsString::from(tmp.path());
        let paths = Paths::from_env(crate::paths::Profile::default(), |k| {
            (k == HOME_ENV).then(|| home.clone())
        })
        .expect("paths");
        paths.create_dirs().expect("dirs");
        let first = open_keyring(&paths).expect("a fresh profile gets its key");
        // The profile gets data: an account directory.
        let account = paths
            .accounts_dir()
            .join("00000000-0000-4000-8000-000000000001");
        std::fs::create_dir_all(&account).expect("account dir");
        std::fs::write(account.join("backend.db"), b"keys").expect("backend.db");
        assert!(paths.has_data());
        let again = open_keyring(&paths).expect("reopen");
        assert_eq!(*first.core_db_key(), *again.core_db_key());

        // The keyring file is lost.
        let file = paths.config.join(DEV_KEYRING_FILE);
        std::fs::remove_file(&file).expect("remove the keyring file");
        assert!(matches!(
            open_keyring(&paths),
            Err(KeyError::DevKeyringMissing)
        ));
        assert!(!file.exists(), "no new keyring was written");
        assert!(account.join("backend.db").exists());
        // Even the whole config directory gone changes nothing about that.
        std::fs::remove_dir_all(&paths.config).expect("remove config");
        assert!(matches!(
            open_keyring(&paths),
            Err(KeyError::DevKeyringMissing)
        ));
        // Only core.db left (no account directory) is data as well.
        std::fs::remove_dir_all(&account).expect("remove the account");
        std::fs::write(paths.core_db(), b"db").expect("core.db");
        assert!(matches!(
            open_keyring(&paths),
            Err(KeyError::DevKeyringMissing)
        ));
    }

    #[test]
    fn the_expected_number_tail_is_digits_or_an_error() {
        let with = |value: Option<&'static str>| {
            expected_number_tail(move |k| {
                (k == EXPECT_NUMBER_TAIL_ENV)
                    .then_some(value)
                    .flatten()
                    .map(str::to_owned)
            })
        };
        assert_eq!(with(None), Ok(None));
        assert_eq!(with(Some("")), Ok(None));
        assert_eq!(with(Some("1234")), Ok(Some("1234".to_owned())));
        assert_eq!(with(Some(" 0042 ")), Ok(Some("0042".to_owned())));
        for bad in ["7", "72a4", "+7234", "72 34", "7234567890123456"] {
            let error = with(Some(bad)).expect_err(bad);
            assert!(!error.contains('7'), "the value is not echoed: {error}");
        }
    }
}

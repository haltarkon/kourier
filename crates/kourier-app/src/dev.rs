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
//! `KOURIER_DEV_REHEARSAL=whatsapp` (debug builds with the `fake` and `whatsapp` features): the
//! WhatsApp slot of the registry holds the scripted fake that plays a real WhatsApp run
//! (`kourier_fake::FakeNetwork::rehearsal`) INSTEAD of the real backend, so the real client is
//! never made and nothing of WhatsApp is contacted. For `cargo xtask wa-run rehearse` only.
//! `KOURIER_DEV_REHEARSAL=telegram` (debug builds with the `fake` and `telegram` features) does the
//! same for Telegram (`kourier_fake::FakeNetwork::rehearsal_telegram`): the real parts are never
//! made, no API pair and no helper files are needed.
//! `KOURIER_DEV_REHEARSAL=threema` (debug builds with the `fake` and `threema` features) does the
//! same for Threema (`kourier_fake::FakeNetwork::rehearsal_threema`): `dial::live_parts` is never
//! called, so no dialer, client or socket exists, whatever the run profile's list of test IDs says
//! (the fake judges no ID). One value at a time ([`Rehearsal`]).
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

/// The environment variable that puts a scripted rehearsal fake in the slot of the network it names
/// (`whatsapp`, `telegram` or `threema`; see the module documentation). Every debug build reads it: one that
/// holds no rehearsal network refuses a start that sets it ([`rehearsal_refused`]).
pub const REHEARSAL_ENV: &str = "KOURIER_DEV_REHEARSAL";

/// Whether this build holds a rehearsal network: the test network (`fake`) and at least one backend
/// a rehearsal plays. Without it a start that asks for a rehearsal is refused, not ignored.
pub const HAS_REHEARSAL_NETWORK: bool = cfg!(all(
    feature = "fake",
    any(
        feature = "whatsapp",
        feature = "telegram",
        feature = "threema"
    )
));

/// The words of the refusal when a rehearsal is asked for in a build without a rehearsal network.
const NO_REHEARSAL_NETWORK: &str = "this build has no rehearsal network";

/// Refuses a start of a debug build WITHOUT a rehearsal network ([`HAS_REHEARSAL_NETWORK`]) whose
/// environment sets [`REHEARSAL_ENV`] to anything (TR R0a fix G-5, review F5): the switch would
/// otherwise be ignored and the REAL backend (and, for Telegram, its confirmation) would run while
/// the person believes it is a rehearsal. One fixed sentence, never the value.
pub fn rehearsal_refused(env: impl Fn(&str) -> Option<OsString>) -> Result<(), String> {
    rehearsal_refused_as(cfg!(debug_assertions), HAS_REHEARSAL_NETWORK, env)
}

/// [`rehearsal_refused`] with the compiled-in facts as arguments (the kind of build and whether it
/// holds a rehearsal network), so the default build's tests run the row of a build without one. A
/// build that holds the network judges the switch itself ([`rehearsal_requested`]); a release build
/// reads nothing.
fn rehearsal_refused_as(
    debug: bool,
    has_rehearsal_network: bool,
    env: impl Fn(&str) -> Option<OsString>,
) -> Result<(), String> {
    if !debug || has_rehearsal_network {
        return Ok(());
    }
    if env(REHEARSAL_ENV).is_some_and(|value| !value.is_empty()) {
        return Err(format!("{REHEARSAL_ENV}: {NO_REHEARSAL_NETWORK}"));
    }
    Ok(())
}

/// The values [`rehearsal_requested`] takes in this build, for its refusal's words: a build
/// without a network's backend does not name that network.
#[cfg(all(
    feature = "fake",
    any(feature = "whatsapp", feature = "telegram", feature = "threema")
))]
const REHEARSAL_WORDS: &str = match (
    cfg!(feature = "whatsapp"),
    cfg!(feature = "telegram"),
    cfg!(feature = "threema"),
) {
    (true, true, true) => "`whatsapp`, `telegram`, `threema` or unset",
    (true, true, false) => "`whatsapp`, `telegram` or unset",
    (true, false, true) => "`whatsapp`, `threema` or unset",
    (false, true, true) => "`telegram`, `threema` or unset",
    (true, false, false) => "`whatsapp` or unset",
    (false, true, false) => "`telegram` or unset",
    (false, false, _) => "`threema` or unset",
};

/// Which network `env` asks to rehearse: exactly `whatsapp`, `telegram` or `threema` (each only in a
/// build that holds that backend and the test network), unset or empty is none, anything else is an
/// error that does not repeat the value (a typo must not start a run against the real backend while
/// the person thinks it is a rehearsal). A release build never reads the variable.
#[cfg(all(
    feature = "fake",
    any(feature = "whatsapp", feature = "telegram", feature = "threema")
))]
pub fn rehearsal_requested(
    env: impl Fn(&str) -> Option<OsString>,
) -> Result<crate::app::Rehearsal, String> {
    rehearsal_requested_as(cfg!(debug_assertions), env)
}

/// [`rehearsal_requested`] with the kind of build as an argument (`debug` false reads nothing and
/// asks for none), so the release rule has a test: this module itself exists in debug builds only.
#[cfg(all(
    feature = "fake",
    any(feature = "whatsapp", feature = "telegram", feature = "threema")
))]
fn rehearsal_requested_as(
    debug: bool,
    env: impl Fn(&str) -> Option<OsString>,
) -> Result<crate::app::Rehearsal, String> {
    use crate::app::Rehearsal;
    if !debug {
        return Ok(Rehearsal::None);
    }
    match env(REHEARSAL_ENV).as_deref().and_then(|v| v.to_str()) {
        None | Some("") => Ok(Rehearsal::None),
        #[cfg(feature = "whatsapp")]
        Some("whatsapp") => Ok(Rehearsal::WhatsApp),
        #[cfg(feature = "telegram")]
        Some("telegram") => Ok(Rehearsal::Telegram),
        #[cfg(feature = "threema")]
        Some("threema") => Ok(Rehearsal::Threema),
        Some(_) => Err(format!("{REHEARSAL_ENV} must be {REHEARSAL_WORDS}")),
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

/// The environment variable with the last digits of the phone number a first link must find
/// (defined in [`crate::process`], where Restart Now also needs the name in a release build).
pub use crate::process::EXPECT_NUMBER_TAIL_ENV;

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

    /// The sentinel the real-run tool hands the app when its person typed no digits is 15
    /// zeros: no real number ends in them, so a first link is refused rather than unchecked. The
    /// longest tail the check takes must therefore be accepted. Mutant: the range `2..=12`.
    #[test]
    fn the_sentinel_tail_is_accepted() {
        let sentinel = "0".repeat(15);
        let got = expected_number_tail(|k| (k == EXPECT_NUMBER_TAIL_ENV).then(|| sentinel.clone()));
        assert_eq!(got, Ok(Some(sentinel.clone())));
        // One more digit is too many.
        let too_long = format!("{sentinel}0");
        assert!(
            expected_number_tail(|k| (k == EXPECT_NUMBER_TAIL_ENV).then(|| too_long.clone()))
                .is_err()
        );
    }

    /// A5: the rehearsal switch names ONE network by an exact word. Unset and empty are none; each
    /// network's word is taken only in a build that holds that backend; anything else (other case,
    /// blanks around, a list, another network, a flag word) is an error that does not repeat the
    /// value and states the accepted words; a release build reads nothing. Mutant: any value taken as
    /// Telegram (or as WhatsApp), or the release check dropped.
    #[cfg(all(
        feature = "fake",
        any(feature = "whatsapp", feature = "telegram", feature = "threema")
    ))]
    #[test]
    fn rehearsal_names_one_network() {
        use crate::app::Rehearsal;

        let with_in = |debug: bool, value: Option<&'static str>| {
            rehearsal_requested_as(debug, move |k| {
                (k == REHEARSAL_ENV)
                    .then_some(value)
                    .flatten()
                    .map(OsString::from)
            })
        };
        let with = |value| with_in(true, value);
        // The public reader asks for this build's kind: a debug test reads.
        assert_eq!(
            rehearsal_requested(|k| (k == REHEARSAL_ENV).then(|| OsString::from("")))
                .expect("empty is none"),
            Rehearsal::None
        );
        assert_eq!(with(None), Ok(Rehearsal::None));
        assert_eq!(with(Some("")), Ok(Rehearsal::None));
        #[cfg(feature = "whatsapp")]
        assert_eq!(with(Some("whatsapp")), Ok(Rehearsal::WhatsApp));
        #[cfg(feature = "telegram")]
        assert_eq!(with(Some("telegram")), Ok(Rehearsal::Telegram));
        #[cfg(feature = "threema")]
        assert_eq!(with(Some("threema")), Ok(Rehearsal::Threema));
        #[cfg_attr(
            all(feature = "whatsapp", feature = "telegram", feature = "threema"),
            allow(unused_mut)
        )]
        let mut bad = vec![
            "WhatsApp",
            "Telegram",
            "TELEGRAM",
            "Threema",
            "THREEMA",
            "threema ",
            " threema",
            "threema,telegram",
            "signal",
            "1",
            "true",
            "real",
            "tg",
            "whatsapp ",
            " whatsapp",
            "telegram ",
            " telegram",
            "whatsapp,telegram",
            "telegram,whatsapp",
            "whatsapp telegram",
            "telegram\n",
        ];
        // A network this build does not hold is not a word it takes.
        #[cfg(not(feature = "whatsapp"))]
        bad.push("whatsapp");
        #[cfg(not(feature = "telegram"))]
        bad.push("telegram");
        #[cfg(not(feature = "threema"))]
        bad.push("threema");
        for bad in bad {
            // The refusal is the fixed sentence and nothing else: the value is not repeated.
            assert_eq!(
                with(Some(bad)),
                Err(format!("{REHEARSAL_ENV} must be {REHEARSAL_WORDS}")),
                "{bad:?}"
            );
        }
        // The words name exactly the networks this build can play.
        assert_eq!(
            REHEARSAL_WORDS.contains("`whatsapp`"),
            cfg!(feature = "whatsapp")
        );
        assert_eq!(
            REHEARSAL_WORDS.contains("`telegram`"),
            cfg!(feature = "telegram")
        );
        assert_eq!(
            REHEARSAL_WORDS.contains("`threema`"),
            cfg!(feature = "threema")
        );
        // A release build reads nothing: no network, and no error for a typo either.
        for value in [
            Some("whatsapp"),
            Some("telegram"),
            Some("threema"),
            Some("signal"),
            None,
        ] {
            assert_eq!(with_in(false, value), Ok(Rehearsal::None), "{value:?}");
        }
    }

    /// G-5 (review F5): a debug build WITHOUT a rehearsal network refuses a set, non-empty rehearsal
    /// switch with one fixed sentence that never repeats the value (the real backend must not run
    /// while the person believes it is a rehearsal); unset and empty are fine, a build that holds
    /// the network judges the switch itself, and a release build reads nothing. The compiled-in
    /// facts are arguments, so this runs in the default build. Mutants: the refusal removed (any
    /// value passes); the release row dropped; the empty value refused.
    #[test]
    fn a_build_without_a_rehearsal_network_refuses_the_switch() {
        use std::os::unix::ffi::OsStringExt as _;

        let with = |debug: bool, network: bool, value: Option<OsString>| {
            rehearsal_refused_as(debug, network, move |k| {
                (k == REHEARSAL_ENV).then(|| value.clone()).flatten()
            })
        };
        let word = |v: &str| Some(OsString::from(v));
        let mut sentences = std::collections::BTreeSet::new();
        for value in [
            word("telegram"),
            word("whatsapp"),
            word("threema"),
            word("real"),
            word("1"),
            word(" "),
            word("secret-sentinel-value"),
            Some(OsString::from_vec(vec![0xff, 0xfe])),
        ] {
            let error = with(true, false, value.clone()).expect_err("a switch in a build without");
            assert!(error.contains(NO_REHEARSAL_NETWORK), "{error}");
            sentences.insert(error);
        }
        // One sentence for every value: nothing of the value is repeated.
        assert_eq!(sentences.len(), 1, "{sentences:?}");
        // Unset and empty are no request.
        assert_eq!(with(true, false, None), Ok(()));
        assert_eq!(with(true, false, word("")), Ok(()));
        // A build that holds the network judges the switch itself; a release build reads nothing.
        for value in [word("telegram"), word("real"), None] {
            assert_eq!(with(true, true, value.clone()), Ok(()), "{value:?}");
            assert_eq!(with(false, false, value.clone()), Ok(()), "{value:?}");
            assert_eq!(with(false, true, value.clone()), Ok(()), "{value:?}");
        }
        // The public reader asks this build's facts: a debug test holds the network or does not.
        let asked = rehearsal_refused(|k| (k == REHEARSAL_ENV).then(|| OsString::from("telegram")));
        assert_eq!(asked.is_err(), !HAS_REHEARSAL_NETWORK);
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

//! Backend configuration from the environment (§9.2).
//!
//! The server environment is production unless a debug build is told otherwise with
//! `KOURIER_SIGNAL_ENV`:
//!
//! - `production` (or unset): the real service.
//! - `staging`: Signal's staging service.
//! - `local:<file>`: a local test server described by a mock-harness handshake (the JSON of its
//!   `ready` line, `tests/mock-harness/README.md`): `host`, `port`, `ca_pem`, `trust_root` and
//!   `zk_public_params`. The CA is written next to the logs as `mock-ca.pem`.
//!
//! Release builds refuse any value but `production`, so a stray variable can never point a
//! user's install somewhere else.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use kourier_backend_api::{BackendConfig, BackendEnv, LocalTestEnv};
use serde::Deserialize;

/// The environment variable that selects the Signal environment in debug builds.
pub const SIGNAL_ENV: &str = "KOURIER_SIGNAL_ENV";

/// Why the backend configuration is unusable. Log-safe (paths are the user's own files).
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A value this build does not accept.
    #[error(
        "{SIGNAL_ENV}={0:?} is not valid here (production, or in debug builds staging or local:<handshake.json>)"
    )]
    Invalid(String),
    /// The handshake file could not be read.
    #[error("cannot read the mock handshake {path}: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// The handshake is not the JSON the harness prints.
    #[error("the mock handshake {path} is not a harness ready line: {detail}")]
    Parse {
        /// The file.
        path: PathBuf,
        /// serde's category and position only.
        detail: String,
    },
    /// The CA file could not be written.
    #[error("cannot write the mock CA certificate: {0}")]
    WriteCa(std::io::Error),
    /// What a first link must find is not usable (log-safe: the value is not repeated).
    #[error("{0}")]
    Expectation(String),
}

/// The fields of the harness handshake Kourier needs. The harness writes them in camelCase
/// (`caPem`, `trustRoot`, `zkPublicParams`); snake_case is accepted too.
#[derive(Deserialize)]
struct Handshake {
    host: String,
    port: u16,
    #[serde(alias = "caPem")]
    ca_pem: String,
    #[serde(alias = "trustRoot")]
    trust_root: String,
    #[serde(alias = "zkPublicParams")]
    zk_public_params: String,
}

/// The backend configuration for this process: the environment from `env(SIGNAL_ENV)`, files
/// it needs written below `state_dir`.
pub fn backend_config(
    env: impl Fn(&str) -> Option<String>,
    state_dir: &Path,
) -> Result<BackendConfig, ConfigError> {
    let value = env(SIGNAL_ENV).unwrap_or_default();
    let backend_env = parse_env(value.trim(), cfg!(debug_assertions), state_dir)?;
    // Debug builds: the account a first link must find (`KOURIER_DEV_EXPECT_NUMBER_TAIL`).
    #[cfg(debug_assertions)]
    let expect_number_suffix = crate::dev::expected_number_tail(&env)
        .map_err(ConfigError::Expectation)?
        .map(kourier_model::Redacted::new);
    #[cfg(not(debug_assertions))]
    let expect_number_suffix = None;
    Ok(BackendConfig {
        env: backend_env,
        expect_number_suffix,
        ..BackendConfig::default()
    })
}

fn parse_env(value: &str, debug: bool, state_dir: &Path) -> Result<BackendEnv, ConfigError> {
    match value {
        "" | "production" => return Ok(BackendEnv::Production),
        _ if !debug => return Err(ConfigError::Invalid(value.to_owned())),
        "staging" => return Ok(BackendEnv::Staging),
        _ => {}
    }
    let Some(file) = value.strip_prefix("local:").filter(|f| !f.is_empty()) else {
        return Err(ConfigError::Invalid(value.to_owned()));
    };
    let path = PathBuf::from(file);
    let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })?;
    let hs: Handshake = serde_json::from_str(&text).map_err(|e| ConfigError::Parse {
        path: path.clone(),
        detail: format!(
            "{:?} at line {} column {}",
            e.classify(),
            e.line(),
            e.column()
        ),
    })?;
    crate::paths::create_private_dir(state_dir).map_err(ConfigError::WriteCa)?;
    let ca = state_dir.join("mock-ca.pem");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&ca)
        .map_err(ConfigError::WriteCa)?;
    f.write_all(hs.ca_pem.as_bytes())
        .map_err(ConfigError::WriteCa)?;
    Ok(BackendEnv::LocalTest(LocalTestEnv {
        host: hs.host,
        port: hs.port,
        ca_cert: Some(ca),
        trust_root: Some(hs.trust_root),
        zk_params: Some(hs.zk_public_params),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_by_default_and_release_refuses_others() {
        let tmp = tempfile::tempdir().expect("tmp");
        assert_eq!(
            parse_env("", false, tmp.path()).expect("default"),
            BackendEnv::Production
        );
        assert_eq!(
            parse_env("production", false, tmp.path()).expect("prod"),
            BackendEnv::Production
        );
        assert!(matches!(
            parse_env("staging", false, tmp.path()),
            Err(ConfigError::Invalid(_))
        ));
        assert!(matches!(
            parse_env("local:/x", false, tmp.path()),
            Err(ConfigError::Invalid(_))
        ));
        assert_eq!(
            parse_env("staging", true, tmp.path()).expect("staging"),
            BackendEnv::Staging
        );
        assert!(matches!(
            parse_env("nonsense", true, tmp.path()),
            Err(ConfigError::Invalid(_))
        ));
    }

    /// Debug builds hand the account a first link must find on to the backend; a value that
    /// is not digits fails the start instead of turning the check off.
    #[cfg(debug_assertions)]
    #[test]
    fn the_expected_account_of_a_first_link() {
        let tmp = tempfile::tempdir().expect("tmp");
        let with = |tail: Option<&'static str>| {
            backend_config(
                move |k| match k {
                    crate::dev::EXPECT_NUMBER_TAIL_ENV => tail.map(str::to_owned),
                    _ => None,
                },
                tmp.path(),
            )
        };
        assert_eq!(with(None).expect("none").expect_number_suffix, None);
        let config = with(Some("4321")).expect("digits");
        assert_eq!(config.env, BackendEnv::Production);
        assert_eq!(
            config
                .expect_number_suffix
                .as_ref()
                .map(|s| s.expose().as_str()),
            Some("4321")
        );
        assert!(
            !format!("{config:?}").contains("4321"),
            "the digits are not printed"
        );
        assert!(matches!(
            with(Some("43x1")),
            Err(ConfigError::Expectation(_))
        ));
    }

    #[test]
    fn local_handshake() {
        let tmp = tempfile::tempdir().expect("tmp");
        let hs = tmp.path().join("hs.json");
        std::fs::write(
            &hs,
            r#"{"type":"ready","protocol":1,"host":"localhost","ip":"127.0.0.1","port":4433,
               "ca_pem":"-----BEGIN CERTIFICATE-----\nAA==\n-----END CERTIFICATE-----\n",
               "trust_root":"BQ==","zk_public_params":"AA==","extra":{"x":1}}"#,
        )
        .expect("write");
        let env = parse_env(&format!("local:{}", hs.display()), true, tmp.path()).expect("local");
        let BackendEnv::LocalTest(local) = env else {
            panic!("not local: {env:?}");
        };
        assert_eq!((local.host.as_str(), local.port), ("localhost", 4433));
        let ca = local.ca_cert.expect("ca");
        assert!(
            std::fs::read_to_string(&ca)
                .expect("ca file")
                .contains("BEGIN CERTIFICATE")
        );
        assert_eq!(local.trust_root.as_deref(), Some("BQ=="));
        assert!(matches!(
            parse_env("local:/does/not/exist", true, tmp.path()),
            Err(ConfigError::Read { .. })
        ));
        // The harness's own spelling (its `ready` line verbatim).
        std::fs::write(
            &hs,
            r#"{"type":"ready","host":"localhost","port":4434,"caPem":"PEM","trustRoot":"BQ==",
               "zkPublicParams":"AA==","caDer":"AA=="}"#,
        )
        .expect("write");
        let env = parse_env(&format!("local:{}", hs.display()), true, tmp.path()).expect("camel");
        assert!(matches!(env, BackendEnv::LocalTest(ref l) if l.port == 4434));
        std::fs::write(&hs, "{").expect("write");
        assert!(matches!(
            parse_env(&format!("local:{}", hs.display()), true, tmp.path()),
            Err(ConfigError::Parse { .. })
        ));
    }
}

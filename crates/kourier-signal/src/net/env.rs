//! Signal environments (§6.4): production, staging, and a local test server such as
//! `@signalapp/mock-server` driven by `tests/mock-harness`.
//!
//! Chat endpoints come from libsignal-net (`env::PROD`, `env::STAGING`); the local one is modelled
//! on `LS/rust/bridge/shared/testing/src/net_env.rs:20-41` (HTTP/2, the test CA as DER). Storage,
//! CDN, sealed-sender trust roots and zkgroup server parameters come from Signal Desktop's
//! `config/default.json` (staging) and `config/production.json` at commit 832279c2.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::net::IpAddr;
use std::num::NonZeroU16;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use libsignal_core::curve::PublicKey;
use libsignal_net::chat::{RECOMMENDED_CHAT_WS_CONFIG, ws};
use libsignal_net::connect_state::ServiceName;
use libsignal_net::env::{ConnectionConfig, DomainConfig, StaticIpOrder};
use libsignal_net::infra::certs::RootCertificates;
use libsignal_net::infra::dns::lookup_result::LookupResult;
use libsignal_net::infra::route::HttpVersion;

/// Which Signal service the backend talks to; part of the backend configuration.
#[derive(Clone, Debug, Default)]
pub enum SignalEnvironment {
    /// The production service (`chat.signal.org`).
    #[default]
    Production,
    /// Signal's staging service (`chat.staging.signal.org`).
    Staging,
    /// A local test server, e.g. `@signalapp/mock-server`.
    LocalTest(LocalTestEnv),
}

/// A local test server, described by the mock harness handshake (`tests/mock-harness`, the
/// `ready` line): every field is decoded already.
#[derive(Clone)]
pub struct LocalTestEnv {
    /// TLS server name, also used for DNS (`localhost`; the mock certificate's SANs are
    /// `localhost`, `127.0.0.1` and `::1`).
    pub host: String,
    /// The address the server listens on: the static DNS fallback for `host`. libsignal-net
    /// still asks the system resolver first (`/etc/hosts` answers `localhost`), because its
    /// static-map-only resolver needs the `test-util` feature.
    pub ip: IpAddr,
    /// The server port.
    pub port: u16,
    /// The test CA certificate, DER.
    pub ca_der: Vec<u8>,
    /// The sealed-sender trust root, a 33-byte serialized public key.
    pub trust_root: Vec<u8>,
    /// zkgroup `ServerPublicParams`, serialized.
    pub zk_params: Vec<u8>,
    /// Base URL of the Storage Service (may carry a path prefix, e.g. `/storageService`).
    pub storage_url: String,
    /// Base URL of each CDN by number (0, 2, 3).
    pub cdn_urls: BTreeMap<u32, String>,
}

impl fmt::Debug for LocalTestEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalTestEnv")
            .field("host", &self.host)
            .field("ip", &self.ip)
            .field("port", &self.port)
            .field("storage_url", &self.storage_url)
            .field("cdn_urls", &self.cdn_urls)
            .finish_non_exhaustive()
    }
}

/// An environment that cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum EnvError {
    /// Port 0.
    #[error("port must not be 0")]
    ZeroPort,
    /// The host name is empty.
    #[error("host must not be empty")]
    EmptyHost,
    /// The CA certificate is empty or not PEM/DER.
    #[error("invalid CA certificate")]
    InvalidCa,
    /// A trust root is not a serialized public key.
    #[error("invalid sealed-sender trust root")]
    InvalidTrustRoot,
    /// The zkgroup server parameters are empty or not base64.
    #[error("invalid zkgroup server parameters")]
    InvalidZkParams,
    /// A storage or CDN URL is not `https://`.
    #[error("invalid {0} URL")]
    InvalidUrl(&'static str),
    /// A local test environment lacks a field (CA, trust root, zk params).
    #[error("local test environment without {0}")]
    Incomplete(&'static str),
    /// The CA file of a local test environment could not be read.
    #[error("cannot read the local test CA: {0}")]
    CaFile(String),
    /// The host of a local test environment is neither an IP address nor `localhost`.
    #[error("local test host must be an IP address or localhost")]
    UnresolvableHost,
}

impl SignalEnvironment {
    /// The environment named by the backend configuration (`BackendConfig::env`). A local test
    /// server is described by its host and port, a CA file (PEM) and the base64 trust root and
    /// zkgroup parameters of the mock harness handshake; storage lives at `/storageService` and
    /// the CDNs at `/` (0, 2) and `/cdn3` (3) of the same origin, as `@signalapp/mock-server`
    /// serves them.
    pub(crate) fn from_backend_env(
        env: &kourier_backend_api::BackendEnv,
    ) -> Result<Self, EnvError> {
        use kourier_backend_api::BackendEnv;
        match env {
            BackendEnv::Production => Ok(Self::Production),
            BackendEnv::Staging => Ok(Self::Staging),
            BackendEnv::LocalTest(local) => {
                let ca_path = local.ca_cert.as_ref().ok_or(EnvError::Incomplete("CA"))?;
                let pem = std::fs::read_to_string(ca_path)
                    .map_err(|e| EnvError::CaFile(e.kind().to_string()))?;
                let trust_root = local
                    .trust_root
                    .as_deref()
                    .ok_or(EnvError::Incomplete("trust root"))?;
                let zk_params = local
                    .zk_params
                    .as_deref()
                    .ok_or(EnvError::Incomplete("zkgroup parameters"))?;
                let ip = match local.host.parse::<IpAddr>() {
                    Ok(ip) => ip,
                    Err(_) if local.host == "localhost" => IpAddr::from([127, 0, 0, 1]),
                    Err(_) => return Err(EnvError::UnresolvableHost),
                };
                let base = format!("https://{}:{}", local.host, local.port);
                Ok(Self::LocalTest(LocalTestEnv {
                    host: local.host.clone(),
                    ip,
                    port: local.port,
                    ca_der: pem_to_der(&pem)?,
                    trust_root: STANDARD
                        .decode(trust_root)
                        .map_err(|_| EnvError::InvalidTrustRoot)?,
                    zk_params: STANDARD
                        .decode(zk_params)
                        .map_err(|_| EnvError::InvalidZkParams)?,
                    storage_url: format!("{base}/storageService"),
                    cdn_urls: BTreeMap::from([
                        (0, base.clone()),
                        (2, base.clone()),
                        (3, format!("{base}/cdn3")),
                    ]),
                }))
            }
        }
    }
}

/// The name of the server an account is bound to when it links (`account.env`, §6.5 step 6):
/// `production`, `staging`, or `local:<host>:<port>` for a local test server (the mock listens
/// on a port of its own per instance, and another instance knows nothing of the account).
/// Log-safe: no part of it is secret or personal.
pub(crate) fn binding(env: &kourier_backend_api::BackendEnv) -> String {
    use kourier_backend_api::BackendEnv;
    match env {
        BackendEnv::Production => "production".to_owned(),
        BackendEnv::Staging => "staging".to_owned(),
        BackendEnv::LocalTest(local) => format!("local:{}:{}", local.host, local.port),
    }
}

/// Whether an account bound to `bound` may be used with `env`. A linked account talks only to
/// the server that linked it: any other server answers its credentials with 401 or 403, which
/// reads as "this device was unlinked" and ends in a wipe of the login (§4.3 step 5), and a
/// test profile's credentials have no business at the real service. So a mismatch is refused
/// before anything connects, and nothing is deleted.
///
/// `bound` is `None` for an account linked before the binding was stored (`backend.db` schema
/// 12 and older). Kourier had never talked to a real server then, so such an account can only
/// belong to a local test server: it is refused everywhere else.
pub(crate) fn check_binding(
    bound: Option<&str>,
    env: &kourier_backend_api::BackendEnv,
) -> crate::error::Result<()> {
    let now = binding(env);
    match bound {
        Some(bound) if bound == now => Ok(()),
        None if matches!(env, kourier_backend_api::BackendEnv::LocalTest(_)) => Ok(()),
        Some(bound) => Err(crate::error::Error::WrongEnvironment {
            linked: bound.to_owned(),
            now,
        }),
        None => Err(crate::error::Error::WrongEnvironment {
            linked: "a local test server".to_owned(),
            now,
        }),
    }
}

/// Which kind of service an environment points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnvKind {
    /// Production.
    Production,
    /// Staging.
    Staging,
    /// A local test server.
    LocalTest,
}

/// A resolved environment: everything the transport and the backend need to reach Signal.
pub(crate) struct SignalEnv {
    /// Which service this is.
    pub(crate) kind: EnvKind,
    /// The chat service (websocket and gRPC host).
    pub(crate) chat: DomainConfig,
    /// Websocket timeouts of the chat service.
    pub(crate) chat_ws_config: ws::Config,
    /// Storage Service base URL.
    pub(crate) storage_url: String,
    /// CDN base URLs by number.
    pub(crate) cdn_urls: BTreeMap<u32, String>,
    /// The only CA trusted for storage and CDN HTTPS (Desktop passes it as `ca` for every
    /// `_ajax` call, `SD/ts/textsecure/WebAPI.preload.ts:429`).
    pub(crate) https_ca_der: Vec<u8>,
    /// Sealed-sender certificate trust roots.
    pub(crate) trust_roots: Vec<PublicKey>,
    /// zkgroup `ServerPublicParams`, serialized.
    pub(crate) zk_params: Vec<u8>,
}

impl fmt::Debug for SignalEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignalEnv")
            .field("kind", &self.kind)
            .field("chat_host", &self.chat.connect.hostname)
            .field("chat_port", &self.chat.connect.port)
            .finish_non_exhaustive()
    }
}

/// Signal Messenger's CA for storage and CDN hosts (`SD/config/default.json:23`, valid to 2032).
const SIGNAL_CA_PEM: &str = include_str!("signal-messenger-ca.pem");

const PROD_TRUST_ROOTS: [&str; 2] = [
    "BXu6QIKVz5MA8gstzfOgRQGqyLqOwNKHL6INkv3IHWMF",
    "BUkY0I+9+oPgDCn4+Ac6Iu813yvqkDr/ga8DzLxFxuk6",
];
const STAGING_TRUST_ROOTS: [&str; 2] = [
    "BbqY1DzohE4NUZoVF+L18oUPrK3kILllLEJh2UnPSsEx",
    "BYhU6tPjqP46KGZEzRs1OL4U39V5dlPJ/X09ha4rErkm",
];

const PROD_ZK_PARAMS: &str = concat!(
    "AMhf5ywVwITZMsff/eCyudZx9JDmkkkbV6PInzG4p8x3VqVJSFiMvnvlEKWuRob/1eaIetR31IYeAbm0NdOuHH8Q",
    "i+Rexi1wLlpzIo1gstHWBfZzy1+qHRV5A4TqPp15YzBPm0WSggW6PbSn+F4lf57VCnHF7p8SvzAA2ZZJPYJURt8X",
    "7bbg+H3i+PEjH9DXItNEqs2sNcug37xZQDLm7X36nOoGPs54XsEGzPdEV+itQNGUFEjY6X9Uv+Acuks7NpyGvCoK",
    "xGwgKgE5XyJ+nNKlyHHOLb6N1NuHyBrZrgtY/JYJHRooo5CEqYKBqdFnmbTVGEkCvJKxLnjwKWf+fEPoWeQFj5Ob",
    "DjcKMZf2Jm2Ae69x+ikU5gBXsRmoF94GXTLfN0/vLt98KDPnxwAQL9j5V1jGOY8jQl6MLxEs56cwXN0dqCnImzVH",
    "3TZT1cJ8SW1BRX6qIVxEzjsSGx3yxF3suAilPMqGRp4ffyopjMD1JXiKR2RwLKzizUe5e8XyGOy9fplzhw3jVzTR",
    "yUZTRSZKkMLWcQ/gv0E4aONNqs4P+NameAZYOD12qRkxosQQP5uux6B2nRyZ7sAV54DgFyLiRcq1FvwKw2EPQdk4",
    "HDoePrO/RNUbyNddnM/mMgj4FW65xCoT1LmjrIjsv/Ggdlx46ueczhMgtBunx1/w8k8V+l8LVZ8gAT6wkU5J+DPQ",
    "alQguMg12Jzug3q4TbdHiGCmD9EunCwOmsLuLJkz6EcSYXtrlDEnAM+hicw7iergYLLlMXpfTdGxJCWJmP4zqUFe",
    "TTmsmhsjGBt7NiEB/9pFFEB3pSbf4iiUukw63Eo8Aqnf4iwob6X1QviCWuc8t0LUlT9vALgh/f2DPVOOmR0RW6bg",
    "Rvc7DSF20V/omg+YBw==",
);

const STAGING_ZK_PARAMS: &str = concat!(
    "ABSY21VckQcbSXVNCGRYJcfWHiAMZmpTtTELcDmxgdFbtp/bWsSxZdMKzfCp8rvIs8ocCU3B37fT3r4Mi5qAemeG",
    "eR2X+/YmOGR5ofui7tD5mDQfstAI9i+4WpMtIe8KC3wU5w3Inq3uNWVmoGtpKndsNfwJrCg0Hd9zmObhypUnSkfY",
    "n2ooMOOnBpfdanRtrvetZUayDMSC5iSRcXKpdlukrpzzsCIvEwjwQlJYVPOQPj4V0F4UXXBdHSLK05uoPBCQG8G9",
    "rYIGedYsClJXnbrgGYG3eMTG5hnx4X4ntARBgELuMWWUEEfSK0mjXg+/2lPmWcTZWR9nkqgQQP0tbzuiPm74H2wM",
    "O4u1Wafe+UwyIlIT9L7KLS19Aw8r4sPrXZSSsOZ6s7M1+rTJN0bI5CKY2PX29y5Ok3jSWufIKcgKOnWoP67d5b2d",
    "u2ZVJjpjfibNIHbT/cegy/sBLoFwtHogVYUewANUAXIaMPyCLRArsKhfJ5wBtTminG/PAvuBdJ70Z/bXVPf8TVsR",
    "292zQ65xwvWTejROW6AZX6aqucUjlENAErBme1YHmOSpU6tr6doJ66dPzVAWIanmO/5mgjNEDeK7DDqQdB1xd03H",
    "T2Qs2TxY3kCK8aAb/0iM0HQiXjxZ9HIgYhbtvGEnDKW5ILSUydqH/KBhW4Pb0jZWnqN/YgbWDKeJxnDbYcUob5ZY",
    "5Lt5ZCMKuaGUvCJRrCtuugSMaqjowCGRempsDdJEt+cMaalhZ6gczklJB/IbdwENW9KeVFPoFNFzhxWUIS5ML9ri",
    "VYhAtE6JE5jX0xiHNVIIPthb458cfA8daR0nYfYAUKogQArm0iBezOO+mPk5vCNWI+wwkyFCqNDXz/qxl1gAntuC",
    "JtSfq9OC3NkdhQlgYQ==",
);

impl SignalEnv {
    /// Resolves the configured environment.
    pub(crate) fn resolve(environment: &SignalEnvironment) -> Result<Self, EnvError> {
        match environment {
            SignalEnvironment::Production => Self::signal(
                EnvKind::Production,
                &libsignal_net::env::PROD,
                "https://storage.signal.org",
                [
                    (0, "https://cdn.signal.org"),
                    (2, "https://cdn2.signal.org"),
                    (3, "https://cdn3.signal.org"),
                ],
                &PROD_TRUST_ROOTS,
                PROD_ZK_PARAMS,
            ),
            SignalEnvironment::Staging => Self::signal(
                EnvKind::Staging,
                &libsignal_net::env::STAGING,
                "https://storage-staging.signal.org",
                [
                    (0, "https://cdn-staging.signal.org"),
                    (2, "https://cdn2-staging.signal.org"),
                    (3, "https://cdn3-staging.signal.org"),
                ],
                &STAGING_TRUST_ROOTS,
                STAGING_ZK_PARAMS,
            ),
            SignalEnvironment::LocalTest(local) => Self::local_test(local),
        }
    }

    fn signal(
        kind: EnvKind,
        env: &libsignal_net::env::Env<'static>,
        storage_url: &str,
        cdn_urls: [(u32, &str); 3],
        trust_roots: &[&str],
        zk_params: &str,
    ) -> Result<Self, EnvError> {
        let trust_roots = trust_roots
            .iter()
            .map(|root| {
                let bytes = STANDARD
                    .decode(root)
                    .map_err(|_| EnvError::InvalidTrustRoot)?;
                PublicKey::deserialize(&bytes).map_err(|_| EnvError::InvalidTrustRoot)
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            kind,
            chat: env.chat_domain_config.clone(),
            chat_ws_config: env.chat_ws_config,
            storage_url: storage_url.to_owned(),
            cdn_urls: cdn_urls
                .into_iter()
                .map(|(n, url)| (n, url.to_owned()))
                .collect(),
            https_ca_der: pem_to_der(SIGNAL_CA_PEM)?,
            trust_roots,
            zk_params: STANDARD
                .decode(zk_params)
                .map_err(|_| EnvError::InvalidZkParams)?,
        })
    }

    fn local_test(local: &LocalTestEnv) -> Result<Self, EnvError> {
        let port = NonZeroU16::new(local.port).ok_or(EnvError::ZeroPort)?;
        if local.host.is_empty() {
            return Err(EnvError::EmptyHost);
        }
        if local.ca_der.is_empty() {
            return Err(EnvError::InvalidCa);
        }
        if local.zk_params.is_empty() {
            return Err(EnvError::InvalidZkParams);
        }
        let trust_root =
            PublicKey::deserialize(&local.trust_root).map_err(|_| EnvError::InvalidTrustRoot)?;
        check_https_url(&local.storage_url, "storage")?;
        for url in local.cdn_urls.values() {
            check_https_url(url, "CDN")?;
        }
        // libsignal-net wants `'static` host names and fallback addresses. A test process builds
        // a handful of these, so leaking a few bytes per environment is fine.
        let hostname: &'static str = Box::leak(local.host.clone().into_boxed_str());
        let (ip_v4, ip_v6): (&'static [_], &'static [_]) = match local.ip {
            IpAddr::V4(v4) => (Box::leak(Box::new([v4])), &[]),
            IpAddr::V6(v6) => (&[], Box::leak(Box::new([v6]))),
        };
        Ok(Self {
            kind: EnvKind::LocalTest,
            chat: DomainConfig {
                connect: ConnectionConfig {
                    service: ServiceName("chat"),
                    hostname,
                    port,
                    cert: RootCertificates::FromDer(Cow::Owned(local.ca_der.clone())),
                    min_tls_version: None,
                    http_version: Some(HttpVersion::Http2),
                    confirmation_header_name: None,
                    proxy: None,
                },
                ip_v4,
                ip_v6,
            },
            chat_ws_config: RECOMMENDED_CHAT_WS_CONFIG,
            storage_url: local.storage_url.clone(),
            cdn_urls: local.cdn_urls.clone(),
            https_ca_der: local.ca_der.clone(),
            trust_roots: vec![trust_root],
            zk_params: local.zk_params.clone(),
        })
    }

    /// The chat host's static addresses, the last DNS fallback after the system resolver and
    /// DoH: shuffled for Signal's services (as the bridge does), the handshake address for a
    /// local test server.
    pub(crate) fn chat_static_dns(&self) -> HashMap<&'static str, LookupResult> {
        let entry = match self.kind {
            EnvKind::LocalTest => self.chat.static_fallback(StaticIpOrder::HARDCODED),
            EnvKind::Production | EnvKind::Staging => self
                .chat
                .static_fallback(StaticIpOrder::Shuffled(&mut rand::rng())),
        };
        HashMap::from([entry])
    }
}

fn check_https_url(url: &str, what: &'static str) -> Result<(), EnvError> {
    let rest = url
        .strip_prefix("https://")
        .ok_or(EnvError::InvalidUrl(what))?;
    if rest.is_empty() || rest.starts_with('/') {
        return Err(EnvError::InvalidUrl(what));
    }
    Ok(())
}

/// The first certificate of a PEM block, as DER.
fn pem_to_der(pem: &str) -> Result<Vec<u8>, EnvError> {
    let body = pem
        .split("-----BEGIN CERTIFICATE-----")
        .nth(1)
        .and_then(|rest| rest.split("-----END CERTIFICATE-----").next())
        .ok_or(EnvError::InvalidCa)?;
    let base64: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    STANDARD.decode(base64).map_err(|_| EnvError::InvalidCa)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    fn backend_local(port: u16) -> kourier_backend_api::BackendEnv {
        kourier_backend_api::BackendEnv::LocalTest(kourier_backend_api::LocalTestEnv {
            host: "localhost".to_owned(),
            port,
            ca_cert: None,
            trust_root: None,
            zk_params: None,
        })
    }

    /// An account talks only to the server that linked it; an account from before the
    /// binding was stored can only be a local test server's.
    #[test]
    fn an_account_is_held_to_the_server_that_linked_it() {
        use kourier_backend_api::BackendEnv;
        assert_eq!(binding(&BackendEnv::Production), "production");
        assert_eq!(binding(&BackendEnv::Staging), "staging");
        assert_eq!(binding(&backend_local(4433)), "local:localhost:4433");

        for env in [
            BackendEnv::Production,
            BackendEnv::Staging,
            backend_local(4433),
        ] {
            assert!(check_binding(Some(&binding(&env)), &env).is_ok(), "{env:?}");
        }
        // The kept production login started with a leftover mock or staging environment: the
        // case that would otherwise end in a wipe.
        for other in [BackendEnv::Staging, backend_local(4433)] {
            let Err(crate::error::Error::WrongEnvironment { linked, now }) =
                check_binding(Some("production"), &other)
            else {
                panic!("production credentials must not go to {other:?}");
            };
            assert_eq!((linked.as_str(), now), ("production", binding(&other)));
        }
        // A mock profile at the real service, and at another mock instance.
        assert!(check_binding(Some("local:localhost:4433"), &BackendEnv::Production).is_err());
        assert!(check_binding(Some("local:localhost:4433"), &BackendEnv::Staging).is_err());
        assert!(check_binding(Some("local:localhost:4433"), &backend_local(4434)).is_err());
        // Linked before the binding existed: a test profile, never the real service's.
        assert!(check_binding(None, &backend_local(1)).is_ok());
        assert!(check_binding(None, &BackendEnv::Production).is_err());
        assert!(check_binding(None, &BackendEnv::Staging).is_err());
        // The core is told not to restart such an account, in words the user can act on.
        let error = check_binding(Some("production"), &backend_local(4433))
            .expect_err("refused")
            .to_backend_error();
        assert_eq!(error.kind, kourier_model::ErrorKind::Unsupported);
        assert!(
            error.detail.contains("production") && error.detail.contains("local:localhost:4433")
        );
        assert!(error.user_message.is_some());
    }

    /// A local env with dummy (but well-formed) values, for tests that never connect.
    pub(crate) fn dummy_local_env(port: u16) -> LocalTestEnv {
        LocalTestEnv {
            host: "localhost".to_owned(),
            ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
            ca_der: pem_to_der(SIGNAL_CA_PEM).unwrap(),
            trust_root: STANDARD.decode(PROD_TRUST_ROOTS[0]).unwrap(),
            zk_params: vec![0; 16],
            storage_url: format!("https://localhost:{port}/storageService"),
            cdn_urls: BTreeMap::from([
                (0, format!("https://localhost:{port}")),
                (3, format!("https://localhost:{port}/cdn3")),
            ]),
        }
    }

    #[test]
    fn production_and_staging_resolve_from_libsignal_and_desktop_config() {
        let prod = SignalEnv::resolve(&SignalEnvironment::Production).unwrap();
        assert_eq!(prod.kind, EnvKind::Production);
        assert_eq!(prod.chat.connect.hostname, "grpc.chat.signal.org");
        assert_eq!(prod.chat.connect.port.get(), 443);
        assert_eq!(prod.chat.connect.http_version, Some(HttpVersion::Http2));
        assert_eq!(prod.storage_url, "https://storage.signal.org");
        assert_eq!(prod.cdn_urls.keys().copied().collect::<Vec<_>>(), [0, 2, 3]);
        assert_eq!(prod.trust_roots.len(), 2);
        assert!(!prod.zk_params.is_empty());
        assert!(
            prod.chat_static_dns()["grpc.chat.signal.org"]
                .iter()
                .next()
                .is_some()
        );

        let staging = SignalEnv::resolve(&SignalEnvironment::Staging).unwrap();
        assert_eq!(
            staging.chat.connect.hostname,
            "grpc.chat.staging.signal.org"
        );
        assert_ne!(staging.zk_params, prod.zk_params);
        assert_ne!(staging.trust_roots, prod.trust_roots);
        assert!(staging.storage_url.contains("staging"));
    }

    #[test]
    fn signal_ca_decodes_to_der() {
        let der = pem_to_der(SIGNAL_CA_PEM).unwrap();
        // A DER certificate is a SEQUENCE with a two-byte length.
        assert_eq!(&der[..2], &[0x30, 0x82]);
        assert_eq!(
            der.len(),
            usize::from(u16::from_be_bytes([der[2], der[3]])) + 4
        );
        assert_eq!(pem_to_der("garbage"), Err(EnvError::InvalidCa));
    }

    #[test]
    fn local_test_env_points_only_at_the_given_address() {
        let local = dummy_local_env(43_210);
        let env = SignalEnv::resolve(&SignalEnvironment::LocalTest(local.clone())).unwrap();
        assert_eq!(env.kind, EnvKind::LocalTest);
        assert_eq!(env.chat.connect.hostname, "localhost");
        assert_eq!(env.chat.connect.port.get(), 43_210);
        assert_eq!(env.chat.connect.http_version, Some(HttpVersion::Http2));
        assert!(matches!(
            env.chat.connect.cert,
            RootCertificates::FromDer(_)
        ));
        assert!(env.chat.connect.proxy.is_none());
        assert_eq!(env.chat.ip_v4, [Ipv4Addr::LOCALHOST]);
        let dns = env.chat_static_dns();
        assert_eq!(dns.len(), 1);
        assert_eq!(
            dns["localhost"].iter().collect::<Vec<_>>(),
            [IpAddr::V4(Ipv4Addr::LOCALHOST)]
        );
        assert_eq!(env.trust_roots.len(), 1);
        assert_eq!(env.https_ca_der, local.ca_der);

        let format = format!("{local:?}");
        assert!(!format.contains("ca_der"), "{format}");
    }

    #[test]
    fn local_test_env_is_validated() {
        let check = |edit: &dyn Fn(&mut LocalTestEnv), expected: EnvError| {
            let mut local = dummy_local_env(1234);
            edit(&mut local);
            assert_eq!(
                SignalEnv::resolve(&SignalEnvironment::LocalTest(local)).unwrap_err(),
                expected
            );
        };
        check(&|l| l.port = 0, EnvError::ZeroPort);
        check(&|l| l.host.clear(), EnvError::EmptyHost);
        check(&|l| l.ca_der.clear(), EnvError::InvalidCa);
        check(&|l| l.trust_root = vec![5; 3], EnvError::InvalidTrustRoot);
        check(&|l| l.zk_params.clear(), EnvError::InvalidZkParams);
        check(
            &|l| l.storage_url = "http://localhost:1/storage".to_owned(),
            EnvError::InvalidUrl("storage"),
        );
        check(
            &|l| {
                l.cdn_urls.insert(2, "https:///x".to_owned());
            },
            EnvError::InvalidUrl("CDN"),
        );
    }
}

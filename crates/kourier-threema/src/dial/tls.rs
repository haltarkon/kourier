//! The TLS client configurations of the real connectors and what their errors mean (TH3 §2.12,
//! decisions M8, M10, C8).
//!
//! TWO configurations share ONE [`PinnedVerifier`] (decision M8): the WebSocket's offers NO ALPN (a
//! server that speaks HTTP/2 would otherwise pick `h2` and tungstenite's HTTP/1.1 upgrade would
//! fail), the HTTPS client's offers `h2` and `http/1.1`. The provider is ring, passed explicitly (no
//! process-wide default: another backend may install its own). The time is rustls' per handshake
//! (`DefaultTimeProvider`) in production; tests pass a fixed one (decision M7).
//!
//! THE TARGET CHECK runs before any DNS lookup or TCP connect (decision M10): only `wss://` (the
//! WebSocket) or `https://` (HTTPS) to a host ONE label under the pinned domain; anything else is
//! [`NetError::Untrusted`]. The verifier checks the same host again in the handshake.

use std::sync::Arc;

use rustls::ClientConfig;
use rustls::crypto::CryptoProvider;
use rustls::time_provider::{DefaultTimeProvider, TimeProvider};

use super::pins::{PinSet, PinnedVerifier, Refusal, refusal};
use crate::parts::{NetError, UntrustedKind};

/// The ALPN protocols the HTTPS client offers (the WebSocket offers none).
pub(crate) const HTTPS_ALPN: [&[u8]; 2] = [b"h2", b"http/1.1"];

/// The two client configurations, sharing one verifier.
#[derive(Clone, Debug)]
pub(crate) struct TlsConfigs {
    /// The WebSocket's: no ALPN.
    pub(crate) ws: Arc<ClientConfig>,
    /// The HTTPS client's: `h2`, `http/1.1`.
    pub(crate) https: Arc<ClientConfig>,
}

impl TlsConfigs {
    /// Both configurations over `verifier`, with `provider`'s algorithms and `time`'s clock.
    pub(crate) fn new(
        verifier: Arc<PinnedVerifier>,
        provider: Arc<CryptoProvider>,
        time: Arc<dyn TimeProvider>,
    ) -> Result<Self, rustls::Error> {
        let make = |alpn: Vec<Vec<u8>>| -> Result<ClientConfig, rustls::Error> {
            let mut config = ClientConfig::builder_with_details(provider.clone(), time.clone())
                .with_safe_default_protocol_versions()?
                .dangerous()
                .with_custom_certificate_verifier(verifier.clone())
                .with_no_client_auth();
            config.alpn_protocols = alpn;
            Ok(config)
        };
        Ok(Self {
            ws: Arc::new(make(Vec::new())?),
            https: Arc::new(make(HTTPS_ALPN.iter().map(|p| p.to_vec()).collect())?),
        })
    }

    /// Production: Desktop's six pins (`pins::THREEMA_CH_PINS`), Mozilla's roots, ring, rustls' clock.
    /// Fails only if rustls refuses ring's defaults (it does not; logged by the caller).
    pub(crate) fn production() -> Result<Self, NetError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier =
            PinnedVerifier::production().map_err(|_| NetError::Failed("tls: verifier"))?;
        Self::new(Arc::new(verifier), provider, Arc::new(DefaultTimeProvider))
            .map_err(|_| NetError::Failed("tls: config"))
    }
}

/// The host and port of `url` when its scheme is `scheme` (`wss` or `https`) and its host is ONE
/// label under `pins`' domain: the check before any DNS lookup (decision M10). The URL is parsed
/// as tungstenite parses the WebSocket's request (`http::Uri`).
pub(crate) fn pinned_target(
    url: &str,
    scheme: &str,
    pins: &PinSet,
) -> Result<(String, u16), NetError> {
    let uri: http::Uri = url
        .parse()
        .map_err(|_| NetError::Untrusted(UntrustedKind::UnpinnedHost))?;
    if uri.scheme_str() != Some(scheme) {
        return Err(NetError::Untrusted(UntrustedKind::NotTls));
    }
    let host = uri
        .host()
        .filter(|host| pins.covers(host))
        .ok_or(NetError::Untrusted(UntrustedKind::UnpinnedHost))?;
    Ok((host.to_owned(), uri.port_u16().unwrap_or(443)))
}

/// The crate's error for a failed TLS connection or request: a certificate refusal found anywhere
/// in `error`'s chain is [`NetError::Untrusted`] (every pin failed, an unknown issuer, ...) or, for
/// a date error on a pinned leaf, [`NetError::CertificateDates`] (transient: decision C8);
/// anything else is `Failed(op)`. Log-safe: kinds only, never the certificate or the host.
pub(crate) fn net_error(op: &'static str, error: &(dyn std::error::Error + 'static)) -> NetError {
    match refusal_in(error) {
        Some(found) if found.is_transient() => NetError::CertificateDates,
        Some(found) => NetError::Untrusted(untrusted_kind(found)),
        None => NetError::Failed(op),
    }
}

/// The certificate refusal in `error`'s chain, if any: a rustls error itself, or one inside an
/// `io::Error` (tokio-rustls wraps its handshake errors so; `io::Error::source` skips the inner
/// error, so it is read through `get_ref`).
fn refusal_in(error: &(dyn std::error::Error + 'static)) -> Option<Refusal> {
    let mut next = Some(error);
    while let Some(error) = next {
        if let Some(tls) = error.downcast_ref::<rustls::Error>() {
            return refusal(tls);
        }
        if let Some(inner) = error
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
            && let Some(found) = refusal_in(inner)
        {
            return Some(found);
        }
        next = error.source();
    }
    None
}

fn untrusted_kind(refusal: Refusal) -> UntrustedKind {
    match refusal {
        Refusal::UnpinnedHost => UntrustedKind::UnpinnedHost,
        Refusal::PinMismatch => UntrustedKind::PinMismatch,
        Refusal::UnknownIssuer => UntrustedKind::UnknownIssuer,
        Refusal::BadName => UntrustedKind::BadName,
        // Dates never gets here (`net_error`); a certificate refused for anything else is untrusted.
        Refusal::BadCertificate | Refusal::Dates => UntrustedKind::BadCertificate,
    }
}

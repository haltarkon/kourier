//! Threema's certificate pins and the verifier that holds every TLS connection of the crate to them
//! (TH3 §2.12, decisions M6, M7, C8).
//!
//! THE ORDER: host -> pin -> chain.
//! 1. The host must be ONE label under the pinned domain (Desktop's `include-subdomains` rule for
//!    `*.threema.ch`, `tls-cert-verifier.ts:203-207`, with a non-empty label): every host Kourier's
//!    consumer multi-device client reaches is one (libthreema's production config). Any other host
//!    is refused before a byte of the certificate is read (stricter than Desktop, which lets an
//!    unpinned host through).
//! 2. SHA-256 of the LEAF's whole DER SubjectPublicKeyInfo must be one of the pins (Desktop hashes
//!    the leaf, `tls-cert-verifier.ts:214, 252-267`; RFC 7469 §2.4).
//! 3. The chain must be valid for the name and the handshake's time under the Mozilla roots of
//!    `webpki-roots` (Desktop demands `isIssuedByKnownRoot` as well).
//!
//! Every refusal is UNTRUSTED (the actor holds `NeedsUserAction(UpdateApp)`, never a loop) except a
//! DATE error, which can only concern a pinned leaf (the pin is checked first) and which a wrong local
//! clock causes as well as an expired certificate: that one is transient ([`Refusal::is_transient`]).
//!
//! THE TIME is the one rustls hands [`ServerCertVerifier::verify_server_cert`] for each handshake
//! (decision M7): nothing here captures a time, so a verifier made once per actor keeps accepting a
//! rotated certificate and refuses an expired one. Tests give a fixed time through the call (or
//! through the config's time provider in the handshake tests).
//!
//! ROTATION: the six pins are Threema's current and backup keys [I]; a rotation inside them is
//! invisible. Bump them with Desktop's at every libthreema bump (`VENDORED.md`'s update recipe).
use std::sync::Arc;

use base64::Engine as _;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{
    CertificateError, DigitallySignedStruct, Error, OtherError, RootCertStore, SignatureScheme,
};
use sha2::{Digest, Sha256};

/// The domain whose hosts are pinned: each host is ONE label under it.
pub(crate) const THREEMA_DOMAIN: &str = "threema.ch";

/// Desktop's SHA-256 SPKI pins of `*.threema.ch`, base64 as Desktop writes them
/// (`threema-desktop/apps/desktop/config/vite.config.ts:366-371`, at `93a6615c`).
/// [U: real server] that they still match the production hosts' leaf keys (RT1).
pub(crate) const THREEMA_CH_PINS: [&str; 6] = [
    "vGQZ8hm2h+km+q7rnJ7kF9S17BwSY0rbhwjz6nIupf0=",
    "3L0bKTNfTwVUCjYqqhZXJIO03qC00bSnuxZFsb09OUo=",
    "NN6Lb+2AE7CN3HWZKoWOe6mmHROOnywWoKZYWL1oHIU=",
    "efJCZofFPR3oV/bBk0wmehqnhy3Vv+s9P+3sjhHem/E=",
    "2Vpy8qUQCqc2+Lg6BgRO8G6e6vh7NmvVHTljfwP/Pfk=",
    "KKBJHJn1PQSdNTmoAfhxqWTO61r8O8bPi/JeGtP/6gg=",
];

/// A pinned domain and the SHA-256 hashes of the leaf SPKIs its hosts may show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PinSet {
    domain: String,
    spki_sha256: Vec<[u8; 32]>,
}

impl PinSet {
    /// The production pins: Desktop's six of `*.threema.ch`.
    pub(crate) fn production() -> Self {
        Self::new(
            THREEMA_DOMAIN,
            THREEMA_CH_PINS
                .iter()
                .filter_map(|pin| decode_pin(pin))
                .collect(),
        )
    }

    /// A pin set for the hosts one label under `domain`.
    pub(crate) fn new(domain: &str, spki_sha256: Vec<[u8; 32]>) -> Self {
        Self {
            domain: domain.to_ascii_lowercase(),
            spki_sha256,
        }
    }

    /// The pinned hashes.
    #[cfg(test)]
    pub(crate) fn hashes(&self) -> &[[u8; 32]] {
        &self.spki_sha256
    }

    /// Whether `host` is ONE non-empty label under the pinned domain (ASCII case ignored).
    pub(crate) fn covers(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        host.strip_suffix(&self.domain)
            .and_then(|rest| rest.strip_suffix('.'))
            .is_some_and(|label| !label.is_empty() && !label.contains('.'))
    }

    /// Whether the DER SubjectPublicKeyInfo `spki` hashes to one of the pins.
    fn holds(&self, spki: &[u8]) -> bool {
        let hash: [u8; 32] = Sha256::digest(spki).into();
        self.spki_sha256.contains(&hash)
    }
}

/// One base64 pin as its 32 bytes; `None` for anything else (a pin that does not decode pins
/// nothing: fewer pins is stricter, and a test holds the table at six).
pub(crate) fn decode_pin(pin: &str) -> Option<[u8; 32]> {
    base64::engine::general_purpose::STANDARD
        .decode(pin)
        .ok()?
        .try_into()
        .ok()
}

/// Why the verifier refused a certificate, as the connectors report it (log-safe: a kind, never the
/// certificate or the host).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The host is not one label under the pinned domain.
    UnpinnedHost,
    /// The leaf's key is none of the pins.
    PinMismatch,
    /// A pinned leaf whose chain ends in no known root (an intercepting CA, for one).
    UnknownIssuer,
    /// A pinned leaf that is not for this host.
    BadName,
    /// Anything else wrong with the certificate: its encoding, a signature, its use, an extension.
    BadCertificate,
    /// A pinned leaf outside its validity dates (an expired certificate, or the local clock is wrong).
    Dates,
}

impl Refusal {
    /// Whether the connection may be retried with backoff (C8: only a date error); every other
    /// refusal is untrusted: held for the user, never a loop.
    pub(crate) fn is_transient(self) -> bool {
        self == Self::Dates
    }
}

/// The verifier's own refusals, carried in rustls' `CertificateError::Other`.
#[derive(Debug, thiserror::Error)]
enum PinError {
    #[error("the host is not pinned")]
    UnpinnedHost,
    #[error("the leaf's key is not pinned")]
    PinMismatch,
}

fn refuse(error: PinError) -> Error {
    Error::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(error))))
}

/// What a rustls error says about the certificate; `None` for an error that is not about the
/// certificate (the connection, the protocol).
pub(crate) fn refusal(error: &Error) -> Option<Refusal> {
    let Error::InvalidCertificate(error) = error else {
        return None;
    };
    Some(match error {
        CertificateError::Other(OtherError(inner)) => match inner.downcast_ref::<PinError>() {
            Some(PinError::UnpinnedHost) => Refusal::UnpinnedHost,
            Some(PinError::PinMismatch) => Refusal::PinMismatch,
            None => Refusal::BadCertificate,
        },
        CertificateError::Expired
        | CertificateError::ExpiredContext { .. }
        | CertificateError::NotValidYet
        | CertificateError::NotValidYetContext { .. } => Refusal::Dates,
        CertificateError::UnknownIssuer => Refusal::UnknownIssuer,
        CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. } => {
            Refusal::BadName
        }
        _ => Refusal::BadCertificate,
    })
}

/// The Mozilla roots of `webpki-roots`: the production trust store of the chain check.
pub(crate) fn mozilla_roots() -> RootCertStore {
    RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    }
}

/// The certificate verifier of every TLS connection of the crate: host -> pin -> chain.
#[derive(Debug)]
pub(crate) struct PinnedVerifier {
    pins: PinSet,
    chain: Arc<WebPkiServerVerifier>,
}

impl PinnedVerifier {
    /// The verifier for `pins` with the chain checked under `roots` by `provider`'s algorithms.
    /// Fails only for an empty trust store.
    pub(crate) fn new(
        pins: PinSet,
        roots: RootCertStore,
        provider: Arc<CryptoProvider>,
    ) -> Result<Self, rustls::client::VerifierBuilderError> {
        let chain =
            WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider).build()?;
        Ok(Self { pins, chain })
    }

    /// Production: Desktop's pins of `*.threema.ch` under the Mozilla roots, ring's algorithms.
    pub(crate) fn production() -> Result<Self, rustls::client::VerifierBuilderError> {
        Self::new(
            PinSet::production(),
            mozilla_roots(),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        // 1. The host, before a byte of the certificate is read.
        let pinned_host = match server_name {
            ServerName::DnsName(name) => self.pins.covers(name.as_ref()),
            _ => false,
        };
        if !pinned_host {
            return Err(refuse(PinError::UnpinnedHost));
        }
        // 2. The pin: the leaf's whole DER SPKI.
        let leaf = ParsedCertificate::try_from(end_entity)?;
        if !self.pins.holds(leaf.subject_public_key_info().as_ref()) {
            return Err(refuse(PinError::PinMismatch));
        }
        // 3. The chain to a known root, the name and the dates, at the handshake's time.
        self.chain
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.chain.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.chain.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.chain.supported_verify_schemes()
    }
}

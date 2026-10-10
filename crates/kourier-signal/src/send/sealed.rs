//! Sealed sender on the send side (§6.7 step 3): which recipients get sealed messages and with
//! what access, and the two sender certificates.
//!
//! - Access (`SD/ts/util/getSendOptions.preload.ts:57-155`): sealed sender is disabled for a
//!   recipient after a 401/403; an "unrestricted" recipient gets the zero access key; otherwise
//!   their access key (from their profile key), or the zero key when we have none yet. Sync
//!   messages and Note to Self are never sealed.
//! - Certificates (`SD/ts/services/senderCertificate.preload.ts`): one with our number and one
//!   without (`?includeE164=false`), cached in `sender_certs` and fetched again within an hour of
//!   their expiry. The one with our number is used only when we share our number with everybody
//!   (`SD/ts/util/phoneNumberSharingMode.preload.ts`), otherwise the one without.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kourier_db::rusqlite::{Connection, OptionalExtension};
use libsignal_core::ServiceId;
use libsignal_protocol::{PublicKey, SenderCertificate, Timestamp};
use serde::Deserialize;

use super::SendContext;
use crate::db;
use crate::error::{Error, Result};
use crate::net::{ChatRequest, SignalTransport, parse_json};
use crate::peers::{self, ACCESS_KEY_LEN, SealedSenderState, ZERO_ACCESS_KEY};

/// A certificate is fetched again when it expires within this time
/// (`SENDER_CERTIFICATE_EXPIRATION_BUFFER`).
pub(crate) const REFRESH_BEFORE_EXPIRY: Duration = Duration::from_secs(60 * 60);

/// Path of the sender certificate.
const CERTIFICATE_PATH: &str = "/v1/certificate/delivery";

/// Which sender certificate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CertKind {
    /// Carries our number.
    WithE164,
    /// Without our number.
    WithoutE164,
}

impl CertKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::WithE164 => "with_e164",
            Self::WithoutE164 => "without_e164",
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::WithE164 => CERTIFICATE_PATH,
            Self::WithoutE164 => "/v1/certificate/delivery?includeE164=false",
        }
    }
}

/// How a sealed send to one recipient is authorized.
#[derive(Clone)]
pub(crate) enum SealAccess {
    /// `Unidentified-Access-Key` (the zero key for unrestricted access).
    Key {
        /// The key.
        key: [u8; ACCESS_KEY_LEN],
        /// The zero key was used because we have no access key (unknown state): a success
        /// makes the recipient "unrestricted".
        zero_key: bool,
    },
    /// `Group-Send-Token`: a serialized `GroupSendFullToken` for this recipient, for a member
    /// of a group we send to whose sealed sender is disabled (`getSendOptions`).
    GroupSend(Vec<u8>),
}

/// How a sealed send to one recipient is authorized, and our certificate.
#[derive(Clone)]
pub(crate) struct Seal {
    /// The authorization.
    pub(crate) access: SealAccess,
    /// Our sender certificate.
    pub(crate) certificate: SenderCertificate,
}

/// The header of sealed sends and prekey fetches with an access key.
const ACCESS_KEY_HEADER: &str = "unidentified-access-key";
/// The header of sealed sends with a group send token.
const GROUP_SEND_TOKEN_HEADER: &str = "group-send-token";

impl Seal {
    /// The authorization header: name and (secret) value.
    pub(crate) fn header(&self) -> (&'static str, String) {
        match &self.access {
            SealAccess::Key { key, .. } => (ACCESS_KEY_HEADER, STANDARD.encode(key)),
            SealAccess::GroupSend(token) => (GROUP_SEND_TOKEN_HEADER, STANDARD.encode(token)),
        }
    }

    /// The state a successful sealed send leaves (`handleMessageSendResult`): unknown becomes
    /// enabled (with an access key) or unrestricted (zero key); others stay, and a group send
    /// token says nothing about the recipient's own setting.
    pub(crate) fn state_after_success(&self, before: SealedSenderState) -> SealedSenderState {
        match (before, &self.access) {
            (SealedSenderState::Unknown, SealAccess::Key { zero_key: true, .. }) => {
                SealedSenderState::Unrestricted
            }
            (SealedSenderState::Unknown, SealAccess::Key { .. }) => SealedSenderState::Enabled,
            (other, _) => other,
        }
    }
}

/// The access a sealed send to `peer` would use, or `None` when it must go unsealed.
pub(crate) fn access(conn: &Connection, peer: ServiceId) -> Result<Option<([u8; 16], bool)>> {
    let row = peers::load(conn, &peer.service_id_string())?;
    Ok(match row.sealed {
        SealedSenderState::Disabled => None,
        SealedSenderState::Unrestricted => Some((ZERO_ACCESS_KEY, true)),
        SealedSenderState::Enabled | SealedSenderState::Unknown => match row.access_key {
            Some(key) => Some((key, false)),
            None => Some((ZERO_ACCESS_KEY, true)),
        },
    })
}

/// The seal of a send to `peer`: access plus a valid certificate. `group_token` is the peer's
/// group send token when the send belongs to a group: it seals the send when sealed sender is
/// disabled for them. `None` sends unsealed (sealed sender disabled and no token, or no
/// certificate could be had: a sealed send is an optimization, never a reason not to deliver).
pub(crate) async fn seal_for<T: SignalTransport>(
    ctx: &SendContext<'_, T>,
    peer: ServiceId,
    group_token: Option<&[u8]>,
) -> Result<Option<Seal>> {
    let access = match (ctx.db.read(|c| access(c, peer))?, group_token) {
        (Some((key, zero_key)), _) => SealAccess::Key { key, zero_key },
        (None, Some(token)) => SealAccess::GroupSend(token.to_vec()),
        (None, None) => return Ok(None),
    };
    let settings = ctx.db.read(db::synced_settings)?;
    let kind = if settings.share_number_with_everybody {
        CertKind::WithE164
    } else {
        CertKind::WithoutE164
    };
    match certificate(ctx, kind).await {
        Ok(certificate) => Ok(Some(Seal {
            access,
            certificate,
        })),
        Err(e @ Error::Db(_)) => Err(e),
        Err(e) => {
            tracing::warn!(error = %e, "no sender certificate; sending unsealed");
            Ok(None)
        }
    }
}

#[derive(Deserialize)]
struct CertificateBody {
    certificate: String,
}

/// The cached certificate of `kind` when it is valid for more than [`REFRESH_BEFORE_EXPIRY`].
fn cached(conn: &Connection, kind: CertKind, now: i64) -> Result<Option<SenderCertificate>> {
    let row: Option<(Vec<u8>, i64)> = conn
        .prepare_cached("SELECT certificate, expires_at FROM sender_certs WHERE kind = ?1")?
        .query_row([kind.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let refresh_ms = i64::try_from(REFRESH_BEFORE_EXPIRY.as_millis()).unwrap_or(i64::MAX);
    match row {
        Some((bytes, expires_at)) if expires_at.saturating_sub(refresh_ms) > now => {
            Ok(Some(SenderCertificate::deserialize(&bytes)?))
        }
        _ => Ok(None),
    }
}

/// Checks a fetched certificate (`senderCertificate.preload.ts` `#fetchCertificate`): it
/// decodes, has not expired, carries no number when asked without one, and is signed by a
/// trust root of the environment. Returns its expiry (ms).
pub(crate) fn check_certificate(
    certificate: &SenderCertificate,
    kind: CertKind,
    trust_roots: &[PublicKey],
    now: i64,
) -> Result<i64> {
    let expires_at = i64::try_from(certificate.expiration()?.epoch_millis()).unwrap_or(i64::MAX);
    if expires_at <= now {
        return Err(Error::Decode("sender certificate already expired"));
    }
    if kind == CertKind::WithoutE164 && certificate.sender_e164()?.is_some() {
        return Err(Error::Decode(
            "sender certificate without number has a number",
        ));
    }
    let at = Timestamp::from_epoch_millis(u64::try_from(now).unwrap_or(0));
    if !trust_roots.is_empty() && !certificate.validate_with_trust_roots(trust_roots, at)? {
        return Err(Error::Decode(
            "sender certificate not signed by a trust root",
        ));
    }
    Ok(expires_at)
}

/// Our sender certificate of `kind`: the cached one, or a fresh one from
/// `GET /v1/certificate/delivery` on the authenticated socket.
pub(crate) async fn certificate<T: SignalTransport>(
    ctx: &SendContext<'_, T>,
    kind: CertKind,
) -> Result<SenderCertificate> {
    let now = ctx.now();
    if let Some(certificate) = ctx.db.read(|c| cached(c, kind, now))? {
        return Ok(certificate);
    }
    let response = ctx
        .net
        .request(ctx.socket, ChatRequest::get(kind.path())?)
        .await?;
    if !response.status.is_success() {
        return Err(Error::Status {
            what: "sender certificate",
            status: response.status.as_u16(),
        });
    }
    let body: CertificateBody = parse_json(&response)?;
    let bytes = crate::wire_b64::LENIENT
        .decode(body.certificate.as_bytes())
        .map_err(|_| Error::Decode("sender certificate base64"))?;
    let certificate = SenderCertificate::deserialize(&bytes)?;
    let expires_at = check_certificate(&certificate, kind, ctx.trust_roots, now)?;
    ctx.db.zone(|tx| -> Result<()> {
        tx.prepare_cached(
            "INSERT INTO sender_certs(kind, certificate, expires_at, fetched_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(kind) DO UPDATE SET certificate = excluded.certificate,
               expires_at = excluded.expires_at, fetched_at = excluded.fetched_at",
        )?
        .execute((kind.as_str(), &bytes, expires_at, now))?;
        Ok(())
    })?;
    tracing::debug!(kind = kind.as_str(), "sender certificate fetched");
    Ok(certificate)
}

#[cfg(test)]
pub(crate) mod tests {
    use libsignal_core::DeviceId;
    use libsignal_protocol::{KeyPair, ServerCertificate};

    use super::*;

    /// A trust root, and a sender certificate for `aci` (with `e164`) expiring at `expires`.
    pub(crate) fn certificate_for(
        aci: &str,
        e164: Option<&str>,
        expires: u64,
    ) -> (KeyPair, SenderCertificate) {
        let rng = &mut rand::rng();
        let trust_root = KeyPair::generate(rng);
        let server_key = KeyPair::generate(rng);
        let server =
            ServerCertificate::new(1, server_key.public_key, &trust_root.private_key, rng).unwrap();
        let sender_key = KeyPair::generate(rng);
        let certificate = SenderCertificate::new(
            aci.to_owned(),
            e164.map(str::to_owned),
            sender_key.public_key,
            DeviceId::new(2).unwrap(),
            Timestamp::from_epoch_millis(expires),
            server,
            &server_key.private_key,
            rng,
        )
        .unwrap();
        (trust_root, certificate)
    }

    #[test]
    fn certificates_are_checked_for_expiry_number_and_signer() {
        let aci = "9d0652a3-dcc3-4d11-975f-74d61598733f";
        let (root, with_number) = certificate_for(aci, Some("+15550100"), 10_000);
        let roots = [root.public_key];
        assert_eq!(
            check_certificate(&with_number, CertKind::WithE164, &roots, 5_000).unwrap(),
            10_000
        );
        assert!(check_certificate(&with_number, CertKind::WithoutE164, &roots, 5_000).is_err());
        assert!(check_certificate(&with_number, CertKind::WithE164, &roots, 10_000).is_err());
        let other_root = KeyPair::generate(&mut rand::rng()).public_key;
        assert!(check_certificate(&with_number, CertKind::WithE164, &[other_root], 5_000).is_err());
        let (root, without) = certificate_for(aci, None, 10_000);
        assert!(check_certificate(&without, CertKind::WithoutE164, &[root.public_key], 1).is_ok());
    }

    #[test]
    fn certificates_are_reused_until_an_hour_before_expiry() {
        let fixture = crate::receive::tests::fixture();
        let hour: i64 = 3_600_000;
        let (_, certificate) = certificate_for(
            "9d0652a3-dcc3-4d11-975f-74d61598733f",
            None,
            u64::try_from(10 * hour).unwrap(),
        );
        let bytes = certificate.serialized().unwrap().to_vec();
        fixture
            .db
            .zone(|tx| -> Result<()> {
                tx.execute(
                    "INSERT INTO sender_certs(kind, certificate, expires_at, fetched_at)
                     VALUES ('without_e164', ?1, ?2, 0)",
                    (&bytes, 10 * hour),
                )?;
                assert!(cached(tx, CertKind::WithoutE164, 8 * hour)?.is_some());
                assert!(
                    cached(tx, CertKind::WithoutE164, 9 * hour)?.is_none(),
                    "refresh"
                );
                assert!(cached(tx, CertKind::WithE164, 0)?.is_none());
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn access_follows_the_sealed_sender_state() {
        let fixture = crate::receive::tests::fixture();
        let aci = libsignal_core::Aci::from(uuid::Uuid::from_u128(3));
        let peer = ServiceId::from(aci);
        let sid = peer.service_id_string();
        fixture
            .db
            .zone(|tx| -> Result<()> {
                assert_eq!(
                    access(tx, peer)?,
                    Some((ZERO_ACCESS_KEY, true)),
                    "no key yet"
                );
                peers::store_profile_key(tx, aci, &[2; 32], 1)?;
                let key = peers::derive_access_key(&[2; 32])?;
                assert_eq!(access(tx, peer)?, Some((key, false)));
                peers::set_sealed_state(tx, &sid, SealedSenderState::Disabled, 2)?;
                assert_eq!(access(tx, peer)?, None);
                peers::set_sealed_state(tx, &sid, SealedSenderState::Unrestricted, 3)?;
                assert_eq!(access(tx, peer)?, Some((ZERO_ACCESS_KEY, true)));
                Ok(())
            })
            .unwrap();
        let (_, certificate) = certificate_for(&sid, None, u64::MAX / 2);
        let seal = |zero_key| Seal {
            access: SealAccess::Key {
                key: ZERO_ACCESS_KEY,
                zero_key,
            },
            certificate: certificate.clone(),
        };
        assert_eq!(
            seal(true).state_after_success(SealedSenderState::Unknown),
            SealedSenderState::Unrestricted
        );
        assert_eq!(
            seal(false).state_after_success(SealedSenderState::Unknown),
            SealedSenderState::Enabled
        );
        assert_eq!(
            seal(false).state_after_success(SealedSenderState::Unrestricted),
            SealedSenderState::Unrestricted
        );
        assert_eq!(
            seal(true).header(),
            (
                "unidentified-access-key",
                "AAAAAAAAAAAAAAAAAAAAAA==".to_owned()
            )
        );
        let token = Seal {
            access: SealAccess::GroupSend(vec![1, 2, 3]),
            certificate: certificate.clone(),
        };
        assert_eq!(token.header(), ("group-send-token", "AQID".to_owned()));
        assert_eq!(
            token.state_after_success(SealedSenderState::Disabled),
            SealedSenderState::Disabled,
            "a group token says nothing about the member's own setting"
        );
    }
}

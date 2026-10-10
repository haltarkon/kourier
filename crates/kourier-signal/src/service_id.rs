//! Signal service ids ↔ Kourier [`UserId`]s (§3.1): `aci:<uuid>`, `pni:<uuid>`, `e164:+<digits>`.
//!
//! The grammar is frozen once data exists, so the canonical form is enforced on the way in:
//! lowercase hyphenated UUIDs, and E.164 numbers as `+` with 1-15 digits and no leading zero.
//! Wire forms are accepted as Desktop accepts them: binary (16-byte ACI, 17-byte tagged) wins
//! over the string form when both are present (`SD/ts/textsecure/ProvisioningCipher.node.ts`).

use std::fmt;

use kourier_model::{IdError, UserId};
use libsignal_core::{Aci, E164, Pni, ServiceId};
use uuid::Uuid;

/// [`UserId`] scheme of an ACI.
pub(crate) const ACI_SCHEME: &str = "aci";
/// [`UserId`] scheme of a PNI.
pub(crate) const PNI_SCHEME: &str = "pni";
/// [`UserId`] scheme of a phone number.
pub(crate) const E164_SCHEME: &str = "e164";

/// Longest E.164 number (ITU-T E.164 §6.1: 15 digits).
const E164_MAX_DIGITS: usize = 15;

/// A user id or wire service id that is not one of Signal's forms. Carries no value (log-safe).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ServiceIdError {
    /// The user id belongs to another network's grammar.
    #[error("not a Signal user id (scheme {0:?})")]
    UnknownScheme(String),
    /// The UUID part is missing, malformed or not in canonical lowercase hyphenated form.
    #[error("malformed {0} uuid")]
    BadUuid(&'static str),
    /// The phone number is not `+` followed by 1-15 digits without a leading zero.
    #[error("malformed phone number")]
    BadE164,
    /// A binary or string wire service id did not parse.
    #[error("malformed wire service id")]
    BadWire,
    /// The service id has the wrong kind (a PNI where an ACI is required, or the reverse).
    #[error("wrong kind of service id")]
    WrongKind,
    /// The kourier-model grammar rejected the id.
    #[error("invalid user id: {0}")]
    Model(#[from] IdError),
}

/// A Signal user as a [`UserId`] names it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalUser {
    /// `aci:<uuid>`.
    Aci(Aci),
    /// `pni:<uuid>`.
    Pni(Pni),
    /// `e164:+<digits>`.
    E164(E164),
}

impl SignalUser {
    /// The service id, if this is an ACI or PNI.
    #[allow(dead_code, reason = "contact sync (S9)")]
    pub(crate) fn service_id(self) -> Option<ServiceId> {
        match self {
            Self::Aci(aci) => Some(aci.into()),
            Self::Pni(pni) => Some(pni.into()),
            Self::E164(_) => None,
        }
    }

    /// The Kourier user id.
    #[allow(dead_code, reason = "contact sync (S9)")]
    pub(crate) fn to_user_id(self) -> Result<UserId, ServiceIdError> {
        match self {
            Self::Aci(aci) => aci_user_id(aci),
            Self::Pni(pni) => pni_user_id(pni),
            Self::E164(e164) => e164_user_id(&e164.to_string()),
        }
    }
}

/// Log-safe: the last four hex digits only, phone numbers fully hidden (§9.1).
impl fmt::Debug for SignalUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Aci(aci) => write!(f, "aci:…{}", last4(Uuid::from(*aci))),
            Self::Pni(pni) => write!(f, "pni:…{}", last4(Uuid::from(*pni))),
            Self::E164(_) => f.write_str("e164:[redacted]"),
        }
    }
}

fn last4(uuid: Uuid) -> String {
    let simple = uuid.simple().to_string();
    simple[simple.len() - 4..].to_owned()
}

/// `aci:<uuid>`.
pub(crate) fn aci_user_id(aci: Aci) -> Result<UserId, ServiceIdError> {
    Ok(UserId::from_parts(
        ACI_SCHEME,
        &Uuid::from(aci).hyphenated().to_string(),
    )?)
}

/// `pni:<uuid>`.
pub(crate) fn pni_user_id(pni: Pni) -> Result<UserId, ServiceIdError> {
    Ok(UserId::from_parts(
        PNI_SCHEME,
        &Uuid::from(pni).hyphenated().to_string(),
    )?)
}

/// `aci:<uuid>` or `pni:<uuid>`.
pub(crate) fn service_id_user_id(service_id: ServiceId) -> Result<UserId, ServiceIdError> {
    match service_id {
        ServiceId::Aci(aci) => aci_user_id(aci),
        ServiceId::Pni(pni) => pni_user_id(pni),
    }
}

/// `e164:+<digits>` from a number that is already in E.164 form.
#[allow(dead_code, reason = "contact sync (S9)")]
pub(crate) fn e164_user_id(number: &str) -> Result<UserId, ServiceIdError> {
    validate_e164(number)?;
    Ok(UserId::from_parts(E164_SCHEME, number)?)
}

/// Checks the strict E.164 form `+[1-9][0-9]{0,14}`. libsignal's `E164::from_str` is looser
/// (no `+` required, leading zeros dropped), which would let two spellings name one user.
pub(crate) fn validate_e164(number: &str) -> Result<(), ServiceIdError> {
    let digits = number.strip_prefix('+').ok_or(ServiceIdError::BadE164)?;
    let well_formed = (1..=E164_MAX_DIGITS).contains(&digits.len())
        && digits.bytes().all(|b| b.is_ascii_digit())
        && !digits.starts_with('0');
    if well_formed {
        Ok(())
    } else {
        Err(ServiceIdError::BadE164)
    }
}

/// Parses a canonical lowercase hyphenated UUID.
fn parse_canonical_uuid(value: &str, kind: &'static str) -> Result<Uuid, ServiceIdError> {
    let uuid = Uuid::try_parse(value).map_err(|_| ServiceIdError::BadUuid(kind))?;
    if uuid.hyphenated().to_string() == value {
        Ok(uuid)
    } else {
        Err(ServiceIdError::BadUuid(kind))
    }
}

/// Parses a Kourier user id into a Signal user.
pub(crate) fn parse_user_id(user_id: &UserId) -> Result<SignalUser, ServiceIdError> {
    let value = user_id.value();
    match user_id.scheme() {
        ACI_SCHEME => Ok(SignalUser::Aci(Aci::from(parse_canonical_uuid(
            value, ACI_SCHEME,
        )?))),
        PNI_SCHEME => Ok(SignalUser::Pni(Pni::from(parse_canonical_uuid(
            value, PNI_SCHEME,
        )?))),
        E164_SCHEME => {
            validate_e164(value)?;
            let e164 = value.parse::<E164>().map_err(|_| ServiceIdError::BadE164)?;
            Ok(SignalUser::E164(e164))
        }
        other => Err(ServiceIdError::UnknownScheme(other.to_owned())),
    }
}

/// A service id from its wire fields: the binary form when it is non-empty, else the string form
/// (`<uuid>` for an ACI, `PNI:<uuid>` for a PNI). `Ok(None)` when both are absent or empty.
pub(crate) fn service_id_from_wire(
    binary: Option<&[u8]>,
    string: Option<&str>,
) -> Result<Option<ServiceId>, ServiceIdError> {
    if let Some(bytes) = binary.filter(|b| !b.is_empty()) {
        return ServiceId::parse_from_service_id_binary(bytes)
            .map(Some)
            .ok_or(ServiceIdError::BadWire);
    }
    match string.filter(|s| !s.is_empty()) {
        Some(s) => ServiceId::parse_from_service_id_string(s)
            .map(Some)
            .ok_or(ServiceIdError::BadWire),
        None => Ok(None),
    }
}

/// An ACI from its wire fields (see [`service_id_from_wire`]); a PNI is [`ServiceIdError::WrongKind`].
pub(crate) fn aci_from_wire(
    binary: Option<&[u8]>,
    string: Option<&str>,
) -> Result<Option<Aci>, ServiceIdError> {
    match service_id_from_wire(binary, string)? {
        Some(ServiceId::Aci(aci)) => Ok(Some(aci)),
        Some(ServiceId::Pni(_)) => Err(ServiceIdError::WrongKind),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "9d0652a3-dcc3-4d11-975f-74d61598733f";

    fn uuid() -> Uuid {
        Uuid::parse_str(UUID).unwrap()
    }

    #[test]
    fn aci_and_pni_round_trip_through_user_ids() {
        let aci = Aci::from(uuid());
        let id = aci_user_id(aci).unwrap();
        assert_eq!(id.as_str(), format!("aci:{UUID}"));
        assert_eq!(parse_user_id(&id).unwrap(), SignalUser::Aci(aci));

        let pni = Pni::from(uuid());
        let id = pni_user_id(pni).unwrap();
        assert_eq!(id.as_str(), format!("pni:{UUID}"));
        assert_eq!(parse_user_id(&id).unwrap(), SignalUser::Pni(pni));
        assert_eq!(service_id_user_id(pni.into()).unwrap(), id);
        assert_eq!(SignalUser::Pni(pni).to_user_id().unwrap(), id);
    }

    #[test]
    fn e164_round_trips_and_is_strict() {
        let id = e164_user_id("+14155550101").unwrap();
        assert_eq!(id.as_str(), "e164:+14155550101");
        let user = parse_user_id(&id).unwrap();
        assert!(matches!(user, SignalUser::E164(_)));
        assert_eq!(user.to_user_id().unwrap(), id);
        assert_eq!(user.service_id(), None);

        for bad in [
            "14155550101",
            "+",
            "+0123",
            "++14155550101",
            "+1415555010a",
            "+1234567890123456",
            "+1 415",
        ] {
            assert_eq!(validate_e164(bad), Err(ServiceIdError::BadE164), "{bad}");
        }
        assert!(validate_e164("+123456789012345").is_ok());
    }

    #[test]
    fn non_canonical_uuids_are_rejected() {
        for bad in [
            UUID.to_uppercase(),
            UUID.replace('-', ""),
            format!("{{{UUID}}}"),
            "not-a-uuid".to_owned(),
        ] {
            let id = UserId::from_parts(ACI_SCHEME, &bad).unwrap();
            assert_eq!(
                parse_user_id(&id),
                Err(ServiceIdError::BadUuid(ACI_SCHEME)),
                "{bad}"
            );
        }
        let other = UserId::parse("lid:12345").unwrap();
        assert!(matches!(
            parse_user_id(&other),
            Err(ServiceIdError::UnknownScheme(s)) if s == "lid"
        ));
    }

    #[test]
    fn wire_forms_prefer_binary_and_accept_both() {
        let aci = Aci::from(uuid());
        let pni = Pni::from(uuid());
        let other = Aci::from(Uuid::from_u128(1));

        assert_eq!(
            service_id_from_wire(
                Some(&aci.service_id_binary()),
                Some(&other.service_id_string())
            ),
            Ok(Some(aci.into()))
        );
        assert_eq!(
            service_id_from_wire(Some(&[]), Some(UUID)),
            Ok(Some(aci.into()))
        );
        assert_eq!(
            service_id_from_wire(None, Some(&format!("PNI:{UUID}"))),
            Ok(Some(pni.into()))
        );
        assert_eq!(
            service_id_from_wire(Some(&pni.service_id_binary()), None),
            Ok(Some(pni.into()))
        );
        assert_eq!(service_id_from_wire(None, Some("")), Ok(None));
        assert_eq!(
            service_id_from_wire(Some(&[1, 2, 3]), None),
            Err(ServiceIdError::BadWire)
        );
        assert_eq!(
            aci_from_wire(Some(&pni.service_id_binary()), None),
            Err(ServiceIdError::WrongKind)
        );
        assert_eq!(aci_from_wire(None, Some(UUID)), Ok(Some(aci)));
    }

    #[test]
    fn debug_is_log_safe() {
        let aci = SignalUser::Aci(Aci::from(uuid()));
        assert_eq!(format!("{aci:?}"), "aci:…733f");
        let id = e164_user_id("+14155550101").unwrap();
        let e164 = parse_user_id(&id).unwrap();
        assert!(!format!("{e164:?}").contains("4155"));
    }
}

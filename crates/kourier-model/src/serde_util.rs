//! Serde helpers: byte fields as standard base64 strings (compact and readable in JSON columns).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serializer};

/// `#[serde(with = "crate::serde_util::b64")]` for `Vec<u8>`.
pub(crate) mod b64 {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        STANDARD
            .decode(s.as_bytes())
            .map_err(|_| D::Error::custom("invalid base64"))
    }
}

/// `#[serde(with = "crate::serde_util::b64_opt")]` for `Option<Vec<u8>>`.
pub(crate) mod b64_opt {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(
        bytes: &Option<Vec<u8>>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match bytes {
            Some(b) => s.serialize_some(&STANDARD.encode(b)),
            None => s.serialize_none(),
        }
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Vec<u8>>, D::Error> {
        let s = Option::<String>::deserialize(d)?;
        s.map(|s| {
            STANDARD
                .decode(s.as_bytes())
                .map_err(|_| D::Error::custom("invalid base64"))
        })
        .transpose()
    }
}

/// Decodes standard base64 into exactly `N` bytes (used for keys).
pub(crate) fn decode_fixed<const N: usize>(s: &str) -> Option<[u8; N]> {
    let v = STANDARD.decode(s.as_bytes()).ok()?;
    v.try_into().ok()
}

/// Encodes bytes as standard base64.
pub(crate) fn encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

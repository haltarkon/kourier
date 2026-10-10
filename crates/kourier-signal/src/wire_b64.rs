//! Base64 as the Signal server writes it into JSON.
//!
//! The server pads some fields and not others: a prekey bundle's `signature`s and a profile's
//! `unidentifiedAccess` come without the trailing `=`, keys of 33 bytes need none, sender
//! certificates and group credentials are padded. Signal Desktop's `Bytes.fromBase64` and
//! libsignal's own JSON types (`serde_with`'s `Base64`, which decodes with
//! `DecodePaddingMode::Indifferent`; `LS/rust/net/chat/src/ws/keys.rs`) take both, so every
//! base64 value read from a server answer is decoded with [`LENIENT`]. Found on the first run
//! against the real server (2026-10-07): the mock pads everything, and the strict decoder
//! refused every prekey bundle, so nothing could be sent to a device without a session.
//!
//! Encoding is unchanged: what Kourier sends stays padded, as Desktop sends it.

use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};

/// Standard alphabet; decodes with or without padding (and encodes padded).
pub(crate) const LENIENT: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

#[cfg(test)]
mod tests {
    use super::LENIENT;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    #[test]
    fn padded_and_unpadded_values_decode_alike() {
        // 64 bytes (a signature) and 32 bytes (an access-key verifier) both need padding.
        for len in [1usize, 2, 3, 32, 33, 64] {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            let padded = STANDARD.encode(&bytes);
            let bare = padded.trim_end_matches('=');
            assert_eq!(
                LENIENT.decode(&padded).unwrap(),
                bytes,
                "padded, {len} bytes"
            );
            assert_eq!(
                LENIENT.decode(bare).unwrap(),
                bytes,
                "unpadded, {len} bytes"
            );
            assert_eq!(LENIENT.encode(&bytes), padded, "encoding stays padded");
        }
        assert!(LENIENT.decode("not base64!").is_err());
        assert!(LENIENT.decode("AAA=A").is_err(), "padding in the middle");
    }
}

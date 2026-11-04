//! The key of TDLib's own database (TG-1 plan §2.7): HKDF-SHA256 of the account's random
//! `backend.db` key, domain-separated by its own info string and the account's uuid, so the two
//! databases never share a key, and shredding the account secret makes both unreadable.

use hkdf::Hkdf;
use kourier_db::DbKey;
use kourier_model::AccountUuid;
use sha2::Sha256;
use zeroize::Zeroizing;

/// The HKDF info prefix; the account's uuid (hyphenated text, as the app's key hierarchy writes
/// it) follows it.
pub(crate) const TDLIB_KEY_INFO: &[u8] = b"kourier tdlib database v1";

/// `HKDF-SHA256(ikm = db_key, salt = none, info = TDLIB_KEY_INFO ‖ uuid)`, 32 bytes.
pub(crate) fn tdlib_key(db_key: &DbKey, uuid: AccountUuid) -> Zeroizing<[u8; 32]> {
    let uuid_text = uuid.to_string();
    let mut out = Zeroizing::new([0u8; 32]);
    // 32 bytes is far below HKDF-SHA256's 8160-byte limit, so expand cannot fail.
    let _ = Hkdf::<Sha256>::new(None, db_key.as_bytes())
        .expand_multi_info(&[TDLIB_KEY_INFO, uuid_text.as_bytes()], out.as_mut());
    out
}

/// The key as `setTdlibParameters.database_encryption_key` wants it: standard base64, in a buffer
/// that is zeroized on drop.
pub(crate) fn tdlib_key_base64(db_key: &DbKey, uuid: AccountUuid) -> Zeroizing<String> {
    use base64::Engine as _;
    let key = tdlib_key(db_key, uuid);
    let mut out = Zeroizing::new(String::with_capacity(44));
    base64::engine::general_purpose::STANDARD.encode_string(key.as_slice(), &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid(text: &str) -> AccountUuid {
        serde_json::from_value(serde_json::Value::String(text.to_owned())).expect("uuid")
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The vector was computed once with Python's `hmac` (RFC 5869 by hand, salt = 32 zero bytes):
    /// `python3 -I -c 'import hmac,hashlib; ikm=bytes(range(32));
    /// info=b"kourier tdlib database v1"+b"00112233-4455-6677-8899-aabbccddeeff";
    /// prk=hmac.new(b"\0"*32,ikm,hashlib.sha256).digest();
    /// print(hmac.new(prk,info+b"\x01",hashlib.sha256).digest().hex())'`
    #[test]
    fn the_tdlib_key_is_hkdf_of_the_db_key_and_differs_per_account() {
        let mut raw = [0u8; 32];
        for (i, b) in raw.iter_mut().enumerate() {
            *b = u8::try_from(i).expect("small");
        }
        let db_key = DbKey::from_bytes(raw);
        let one = uuid("00112233-4455-6677-8899-aabbccddeeff");
        let two = uuid("00112233-4455-6677-8899-aabbccddeef0");
        let key = tdlib_key(&db_key, one);
        assert_eq!(
            hex(key.as_slice()),
            "da1f2423aa56f153420d48a2c2256952303861902c1dbcbbaa8133664f5d9a4c",
            "the key is not HKDF-SHA256 of the vector"
        );
        assert_ne!(*key, raw, "the key is the backend key itself");
        assert_eq!(
            hex(tdlib_key(&db_key, two).as_slice()),
            "a3a967e6c7eec1d0044c5dbf8db15fc1983d75c1714bad15e4a08cbc5bfc2203",
            "two accounts do not get different keys"
        );
        let b64 = tdlib_key_base64(&db_key, one);
        assert_eq!(b64.len(), 44);
        assert_eq!(b64.as_str(), "2h8kI6pW8VNCDUiiwiVpUjA4YZAsHby7qoEzZk9dmkw=");
    }
}

//! The raw database key.

use std::fmt;
use zeroize::Zeroizing;

/// A raw 32-byte SQLCipher key, zeroized on drop. SQLCipher gets it as `x'<64 hex>'`, so opening
/// runs no PBKDF2 (§5.2).
#[derive(Clone)]
pub struct DbKey(Zeroizing<[u8; 32]>);

impl DbKey {
    /// Wraps raw key bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// The raw key bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// `PRAGMA key = "x'…'"`, in a buffer that is zeroized on drop.
    pub(crate) fn pragma(&self) -> Zeroizing<String> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut sql = Zeroizing::new(String::with_capacity(84));
        sql.push_str("PRAGMA key = \"x'");
        for b in self.0.iter() {
            sql.push(char::from(HEX[usize::from(b >> 4)]));
            sql.push(char::from(HEX[usize::from(b & 0x0f)]));
        }
        sql.push_str("'\"");
        sql
    }
}

impl From<[u8; 32]> for DbKey {
    fn from(bytes: [u8; 32]) -> Self {
        Self::from_bytes(bytes)
    }
}

impl fmt::Debug for DbKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DbKey(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pragma_is_raw_hex_key() {
        let mut bytes = [0u8; 32];
        bytes[0] = 0xab;
        bytes[31] = 0x01;
        let sql = DbKey::from(bytes).pragma();
        assert!(sql.starts_with("PRAGMA key = \"x'ab00"));
        assert!(sql.ends_with("0001'\""));
        assert_eq!(sql.len(), "PRAGMA key = \"x''\"".len() + 64);
    }

    #[test]
    fn debug_is_redacted() {
        assert_eq!(format!("{:?}", DbKey::from([7; 32])), "DbKey(<redacted>)");
    }
}

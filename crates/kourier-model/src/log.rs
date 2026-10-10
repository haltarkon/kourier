//! Log-safety helpers (architecture §9.1): [`Redacted`] and the log-safe forms of ids.
//!
//! Never logged: message bodies, captions, file names, names, phone numbers, usernames, keys,
//! passwords, tokens, URLs with credentials, QR contents. Service ids print as `aci:…1f2e`,
//! conversation keys as `conv#a1b2`.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ids::{ConvKey, NetMsgId, UserId};
use crate::patch::Patch;

/// A value that must never be printed: `Debug` and `Display` write `[redacted]`. Serde is
/// transparent (the value is stored as is), so wrap secrets that cross the boundary in it.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Redacted<T>(T);

impl<T> Redacted<T> {
    /// Wraps a value.
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// The wrapped value. Do not log it.
    pub fn expose(&self) -> &T {
        &self.0
    }

    /// Unwraps the value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> From<T> for Redacted<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

impl<T> fmt::Debug for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl<T> fmt::Display for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// How a personal-data field prints in a log-safe `Debug`: its shape (`None`, `Clear`, a
/// length), never its content. Used by [`redacting_debug!`](crate::redacting_debug).
pub trait RedactedShape {
    /// Writes the shape of `self`.
    fn fmt_shape(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result;
}

impl RedactedShape for str {
    fn fmt_shape(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[redacted; {} bytes]", self.len())
    }
}

impl RedactedShape for String {
    fn fmt_shape(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt_shape(f)
    }
}

impl RedactedShape for Path {
    fn fmt_shape(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted path]")
    }
}

impl RedactedShape for PathBuf {
    fn fmt_shape(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_path().fmt_shape(f)
    }
}

impl RedactedShape for i32 {
    fn fmt_shape(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl<T> RedactedShape for Option<T> {
    fn fmt_shape(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Some(_) => f.write_str("Some([redacted])"),
            None => f.write_str("None"),
        }
    }
}

impl<T> RedactedShape for Vec<T> {
    fn fmt_shape(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[redacted; {} items]", self.len())
    }
}

impl<T> RedactedShape for Patch<T> {
    fn fmt_shape(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Patch::Unchanged => f.write_str("Unchanged"),
            Patch::Clear => f.write_str("Clear"),
            Patch::Set(_) => f.write_str("Set([redacted])"),
        }
    }
}

/// `Debug` adapter that prints a field's [`RedactedShape`].
pub struct Shape<'a, T: ?Sized>(pub &'a T);

impl<T: RedactedShape + ?Sized> fmt::Debug for Shape<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt_shape(f)
    }
}

/// Implements a log-safe `Debug` for a struct: fields in the first list print normally, fields
/// in `redact` print only their [`RedactedShape`]. The struct is destructured, so a new field
/// that is in neither list fails to compile.
///
/// ```
/// struct Card { id: u32, name: String }
/// kourier_model::redacting_debug!(Card { id } redact { name });
/// let s = format!("{:?}", Card { id: 7, name: "Alice".into() });
/// assert_eq!(s, "Card { id: 7, name: [redacted; 5 bytes] }");
/// ```
#[macro_export]
macro_rules! redacting_debug {
    ($ty:ident { $($field:ident),* $(,)? } redact { $($secret:ident),* $(,)? }) => {
        impl ::std::fmt::Debug for $ty {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                let $ty { $($field,)* $($secret,)* } = self;
                f.debug_struct(stringify!($ty))
                    $(.field(stringify!($field), $field))*
                    $(.field(stringify!($secret), &$crate::log::Shape($secret)))*
                    .finish()
            }
        }
    };
}

/// Schemes whose values are phone numbers: printed without any digit.
const PHONE_SCHEMES: &[&str] = &["e164", "pn", "phone", "tel"];

/// Log-safe [`UserId`] display, from [`UserId::log_safe`]: `aci:…1f2e` shows the last 4
/// characters of values that are at least 8 ASCII-alphanumeric-ending characters long;
/// phone numbers and short values print as `e164:…`.
pub struct LogSafeUserId<'a>(pub(crate) &'a UserId);

impl fmt::Display for LogSafeUserId<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scheme = self.0.scheme();
        let value = self.0.value();
        let phone = PHONE_SCHEMES.contains(&scheme) || value.starts_with('+');
        let n = value.chars().count();
        let tail: String = value.chars().skip(n.saturating_sub(4)).collect();
        if phone || n < 8 || !tail.chars().all(|c| c.is_ascii_alphanumeric()) {
            write!(f, "{scheme}:…")
        } else {
            write!(f, "{scheme}:…{tail}")
        }
    }
}

/// Log-safe [`ConvKey`] display, from [`ConvKey::log_safe`]: `conv#a1b2`, a stable 16-bit hash
/// of the encoded key (correlates log lines without revealing the key).
pub struct LogSafeConvKey<'a>(pub(crate) &'a ConvKey);

impl fmt::Display for LogSafeConvKey<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let h = crate::stable_hash(self.0.encode().as_bytes());
        write!(f, "conv#{:04x}", h & 0xffff)
    }
}

/// Log-safe [`NetMsgId`] display, from [`NetMsgId::log_safe`]: the local part (a timestamp or
/// random id, not personal data) and the log-safe author.
pub struct LogSafeNetMsgId<'a>(pub(crate) &'a NetMsgId);

impl fmt::Display for LogSafeNetMsgId<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "m1:{}", self.0.local.escape_debug())?;
        match &self.0.author {
            Some(a) => write!(f, "@{}", a.log_safe()),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid(s: &str) -> UserId {
        UserId::parse(s).expect("valid")
    }

    #[test]
    fn redacted_never_prints_content() {
        let r = Redacted::new(String::from("hunter2-secret"));
        for s in [
            format!("{r}"),
            format!("{r:?}"),
            format!("{r:#?}"),
            format!("{r:>40}"),
            format!("{:?}", Some(&r)),
            format!("{:#?}", vec![r.clone(), r.clone()]),
        ] {
            assert!(!s.contains("hunter2"), "{s}");
            assert!(s.contains("[redacted]"), "{s}");
        }
        assert_eq!(r.expose(), "hunter2-secret");
        let json = serde_json::to_string(&r).expect("serialize");
        assert_eq!(json, "\"hunter2-secret\"");
        let back: Redacted<String> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, r);
        assert_eq!(back.into_inner(), "hunter2-secret");
    }

    #[test]
    fn service_id_short_form() {
        let a = uid("aci:5f3c2a1e-9b8d-4c7e-a6f5-0123456789ab");
        assert_eq!(a.log_safe().to_string(), "aci:…89ab");
        assert_eq!(
            uid("pni:0000000000001f2e").log_safe().to_string(),
            "pni:…1f2e"
        );
    }

    #[test]
    fn phone_numbers_and_short_ids_are_hidden() {
        for (id, want) in [
            ("e164:+4915112345678", "e164:…"),
            ("pn:4915112345678", "pn:…"),
            ("x:+4915112345678", "x:…"),
            ("threema:ECHO", "threema:…"),
            ("lid:123456789@lid", "lid:…"),
        ] {
            assert_eq!(uid(id).log_safe().to_string(), want, "{id}");
        }
    }

    #[test]
    fn redacting_debug_prints_shapes_only() {
        struct Row {
            id: u32,
            name: Option<String>,
            nick: Patch<String>,
            gone: Patch<String>,
            path: PathBuf,
            tags: Vec<String>,
        }
        crate::redacting_debug!(Row { id } redact { name, nick, gone, path, tags });
        let r = Row {
            id: 3,
            name: Some("Alice".into()),
            nick: Patch::Set("Al".into()),
            gone: Patch::Clear,
            path: PathBuf::from("/home/alice/pic.png"),
            tags: vec!["x".into()],
        };
        let s = format!("{r:?}");
        assert_eq!(
            s,
            "Row { id: 3, name: Some([redacted]), nick: Set([redacted]), gone: Clear, \
             path: [redacted path], tags: [redacted; 1 items] }"
        );
    }

    #[test]
    fn conv_key_is_hashed() {
        let k = ConvKey::Group("gv2:c2VjcmV0LWdyb3VwLWlk".into());
        let s = k.log_safe().to_string();
        assert!(s.starts_with("conv#") && s.len() == 9, "{s}");
        assert_eq!(s, k.log_safe().to_string(), "stable");
        assert!(!format!("{k:?}").contains("c2Vjcm"));
        let d = ConvKey::Direct(uid("e164:+4915112345678"));
        assert!(!format!("{d:?}").contains("4915"));
    }
}

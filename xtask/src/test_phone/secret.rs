//! [`Secret`]: a text read from the phone that cannot be printed, logged, cloned or kept.
//!
//! THE RULE for every run tool: an ID, a name or a code read from the phone is used in memory only
//! (hashed, compared) and the tool hands on a digest or a yes/no. It is NEVER printed, logged,
//! written to a file, put in an error message or on a command line. `Secret` has no `Debug`,
//! `Display`, `Clone` or `Serialize`; the only way to the bytes is [`Secret::with`], which consumes it,
//! so the closure does the comparison and returns the (non-secret) result. The buffer is overwritten
//! before it is freed. Safe Rust cannot promise that no earlier copy lingers (the tree holds the
//! text until it is dropped, and the allocator may have moved a grown buffer), so this limits the
//! exposure and is no substitute for the rule.

pub struct Secret {
    buf: Vec<u8>,
}

impl Secret {
    pub(super) fn new(buf: Vec<u8>) -> Secret {
        Secret { buf }
    }

    /// Consume the secret into a closure. Return only a digest or a yes/no.
    pub fn with<R>(mut self, f: impl FnOnce(&[u8]) -> R) -> R {
        let r = f(&self.buf);
        zero(&mut self.buf);
        r
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        zero(&mut self.buf);
    }
}

/// Overwrite with zeros, keeping the length; `black_box` stops the writes being optimised away.
pub(super) fn zero(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        *b = 0;
    }
    std::hint::black_box(&*buf);
}

pub(super) fn scrub(mut buf: Vec<u8>) {
    zero(&mut buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::{Debug, Display};
    use std::marker::PhantomData;

    // Autoref-free "does T implement X" probe: the inherent const wins when the bound holds.
    trait No {
        const IS: bool = false;
    }
    impl<T> No for T {}
    macro_rules! implements {
        ($t:ty, $tr:path) => {{
            #[allow(dead_code)]
            struct P<T>(PhantomData<T>);
            impl<T: $tr> P<T> {
                #[allow(dead_code)]
                const IS: bool = true;
            }
            <P<$t>>::IS
        }};
    }
    #[test]
    fn test_phone_secret_has_no_debug_display_clone() {
        assert!(
            implements!(String, Debug),
            "the probe must see Debug on String"
        );
        assert!(!implements!(Secret, Debug));
        assert!(!implements!(Secret, Display));
        assert!(!implements!(Secret, Clone));
        assert!(implements!(String, Clone));
    }

    #[test]
    fn test_phone_secret_is_consumed_by_a_closure() {
        let s = Secret::new(b"abc".to_vec());
        let n = s.with(|b| b.iter().map(|&x| x as u32).sum::<u32>());
        assert_eq!(n, 97 + 98 + 99);
    }

    #[test]
    fn test_phone_secret_zeroing_overwrites_the_buffer() {
        let mut v = b"top secret".to_vec();
        let len = v.len();
        zero(&mut v);
        assert_eq!(v.len(), len);
        assert!(v.iter().all(|&b| b == 0));
        // `with` zeroes through the same function; `Drop` too.
        let mut s = Secret::new(b"xyz".to_vec());
        zero(&mut s.buf);
        assert!(s.buf.iter().all(|&b| b == 0));
    }
}

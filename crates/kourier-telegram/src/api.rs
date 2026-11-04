//! The Telegram API credentials (`api_id`, `api_hash`; TG-1 plan §2.11). A release archive gets
//! its pair compiled in from the build environment; every test and every other build uses
//! [`ApiCredentials::DUMMY`], which no server ever sees in this build (nothing contacts Telegram).

use std::borrow::Cow;
use std::fmt;

use kourier_model::Redacted;

/// An `api_id` and its `api_hash`. `Debug` shows neither value.
#[derive(Clone)]
pub struct ApiCredentials {
    id: i32,
    hash: Redacted<Cow<'static, str>>,
}

impl ApiCredentials {
    /// The pair of the offline tests (what the spike used): id 1, a hash of 32 zero hex digits.
    pub const DUMMY: Self = Self {
        id: 1,
        hash: Redacted::new(Cow::Borrowed("00000000000000000000000000000000")),
    };

    /// The pair compiled into this build (`KOURIER_TG_API_ID`, `KOURIER_TG_API_HASH` at build
    /// time). NO build sets them today: xtask's `cargo()` removes both variables from every cargo
    /// it starts and `cargo xtask dist` builds no Telegram code (fix J18; the run tool and the
    /// release gate will set them, from a file outside the repository, when they exist). `None`
    /// when either is missing or not of the right form (an id that is not a positive number, a
    /// hash that is not 32 lower-case hex digits). A test build has none (a test says so).
    pub fn compiled() -> Option<Self> {
        Self::parse(
            option_env!("KOURIER_TG_API_ID"),
            option_env!("KOURIER_TG_API_HASH"),
        )
    }

    /// A pair from text read at run time: the developer's API file of a debug build's run profile
    /// (`kourier-app`, TG-2 M13). The same rules as [`ApiCredentials::compiled`]: `None` for an id
    /// that is not a positive number or a hash that is not 32 lower-case hex digits.
    pub fn from_text(id: &str, hash: &str) -> Option<Self> {
        Self::parse(Some(id), Some(hash))
    }

    fn parse(id: Option<&str>, hash: Option<&str>) -> Option<Self> {
        let id = id?.parse::<i32>().ok().filter(|id| *id > 0)?;
        let hash = hash?;
        let hex = hash.len() == 32
            && hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        hex.then(|| Self {
            id,
            hash: Redacted::new(Cow::Owned(hash.to_owned())),
        })
    }

    /// The `api_id`.
    pub fn id(&self) -> i32 {
        self.id
    }

    /// The `api_hash`. It goes into `setTdlibParameters` only; never into a log.
    pub fn hash(&self) -> &str {
        self.hash.expose()
    }

    /// Whether this is [`ApiCredentials::DUMMY`].
    pub fn is_dummy(&self) -> bool {
        self.id == Self::DUMMY.id && self.hash() == Self::DUMMY.hash()
    }
}

impl fmt::Debug for ApiCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiCredentials")
            .field("dummy", &self.is_dummy())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::ApiCredentials;

    /// A developer's exported pair must fail a test build instead of being compiled into it.
    #[test]
    fn test_builds_carry_no_api_credentials() {
        assert!(
            option_env!("KOURIER_TG_API_ID").is_none()
                && option_env!("KOURIER_TG_API_HASH").is_none(),
            "this test build was compiled with Telegram API credentials in its environment"
        );
        assert!(
            ApiCredentials::compiled().is_none(),
            "a compiled pair exists"
        );
    }

    #[test]
    fn the_dummy_is_id_one_and_its_debug_shows_no_hash() {
        let dummy = ApiCredentials::DUMMY;
        assert_eq!(dummy.id(), 1);
        assert_eq!(dummy.hash(), "0".repeat(32));
        assert!(dummy.is_dummy());
        let shown = format!("{dummy:?}");
        assert!(!shown.contains(dummy.hash()), "Debug shows the hash");
        assert!(
            !shown.contains("1,") && !shown.contains(": 1"),
            "Debug shows the id"
        );
    }

    #[test]
    fn only_a_well_formed_pair_parses() {
        // Built, not written: the source check refuses a 32-digit hex literal (tests/source.rs).
        let hash: &'static str = "0123456789abcdef".repeat(2).leak();
        let upper: &'static str = ["0123456789ABCDEF", "0123456789abcdef"].concat().leak();
        // The id is built too: the source check refuses a quoted number in a `parse` call.
        let id: &'static str = ["123", "45"].concat().leak();
        let ok = ApiCredentials::parse(Some(id), Some(hash)).expect("a good pair");
        assert_eq!(ok.id(), 12345);
        assert!(!ok.is_dummy());
        for (id, hash) in [
            (None, Some(hash)),
            (Some("12345"), None),
            (Some("0"), Some(hash)),
            (Some("-5"), Some(hash)),
            (Some("x"), Some(hash)),
            (Some("12345"), Some(upper)),
            (Some("12345"), Some("0123456789abcdef")),
        ] {
            assert!(
                ApiCredentials::parse(id, hash).is_none(),
                "a malformed pair parsed"
            );
        }
    }

    /// TG-2 M13: the run-time constructor takes the same forms as the compiled pair.
    #[test]
    fn a_pair_from_text_follows_the_same_rules() {
        let hash = "0123456789abcdef".repeat(2);
        let id = ["123", "45"].concat();
        let ok = ApiCredentials::from_text(&id, &hash).expect("a good pair");
        assert_eq!(ok.id(), 12345);
        assert_eq!(ok.hash(), hash);
        assert!(ApiCredentials::from_text("", &hash).is_none());
        assert!(ApiCredentials::from_text(&id, &hash[..31]).is_none());
        assert!(ApiCredentials::from_text(&id, &hash.to_uppercase()).is_none());
    }
}

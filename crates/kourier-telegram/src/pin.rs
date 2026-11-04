//! The TDLib this backend is built for (TG-1 plan §3): the commit, the version and the patch level
//! of the pin file `third_party/tdlib/tdlib.pin`, as constants. The helper's hello names the
//! library's version, commit and Kourier patch tag; the link refuses a helper whose library is
//! another one (protocol AND commit AND patch tag, never the version alone: a stock, 0001-only or
//! pre-0002 library has the pinned commit too, pre3 K5).

/// A TDLib build, by its source commit and its version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pin {
    /// The full commit id of `tdlib/td` the library was built from.
    pub commit: &'static str,
    /// TDLib's version (`getOption("version")`).
    pub version: &'static str,
    /// The newest Kourier patch the library carries: the number of the last patch in the pin
    /// file's list, which patch 0002 makes TDLib answer as the synchronous option
    /// `kourier_patch_level` (the hello's `patch_level`). Each later patch changes this value in
    /// its own hunk and here.
    pub patch_level: &'static str,
}

/// The pinned TDLib. Bumped only together with the pin file (a test compares the two).
pub const PIN: Pin = Pin {
    commit: "0e88653dcbf4e44c73cb655a05b58cd749a91169",
    version: "1.8.69",
    patch_level: "0002",
};

#[cfg(test)]
mod tests {
    use super::PIN;

    const PIN_FILE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../third_party/tdlib/tdlib.pin"
    ));

    /// The quoted value of `key = "…"` in the pin file (the file's one-line format).
    fn value(key: &str) -> Option<&'static str> {
        PIN_FILE.lines().find_map(|line| {
            let (k, v) = line.split_once('=')?;
            (k.trim() == key).then(|| v.trim().trim_matches('"'))
        })
    }

    #[test]
    fn the_pin_constant_matches_the_pin_file() {
        assert_eq!(value("commit"), Some(PIN.commit), "commit differs");
        assert_eq!(value("version"), Some(PIN.version), "version differs");
        assert_eq!(PIN.commit.len(), 40, "the commit is not a full id");
        // The patch level is the number of the pin file's last patch.
        let patches = value("patches").expect("patches");
        let last = patches
            .trim_matches(|c| c == '[' || c == ']')
            .split(',')
            .map(|p| p.trim().trim_matches('"'))
            .next_back()
            .expect("a patch");
        assert_eq!(
            last.split('-').next(),
            Some(PIN.patch_level),
            "the patch level is not the last patch's number"
        );
        assert_eq!(
            value("soname"),
            Some(format!("libtdjson.so.{}", PIN.version).as_str()),
            "the soname does not carry the version"
        );
    }

    /// L5 (LC-20): the scripted TDLib's default hello is the pinned build's. The supervisor's tests run
    /// `ScriptedTd` with `Script::default()` against parts that expect `PIN` (`link::check_pin` compares
    /// protocol, version, commit and patch tag), so a bump that forgets `kourier-tdlib`'s default would
    /// fail them all with a refusal at the hello; this test names the coupling.
    #[test]
    fn the_scripted_default_is_the_pin() {
        let script = kourier_tdlib::scripted::Script::default();
        assert_eq!(script.version, PIN.version, "the scripted version");
        assert_eq!(script.commit, PIN.commit, "the scripted commit");
        assert_eq!(
            script.patch_level, PIN.patch_level,
            "the scripted patch tag"
        );
    }
}

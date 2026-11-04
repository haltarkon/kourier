//! `tdlib::checked_library`: the pinned library as the run tool takes it.

use super::Scratch;
use super::error_of;
use crate::tdlib::checked_library_in;
use crate::tg_run::stage::sha256_of;

/// The pin's SONAME, read from the pin file (the file the library check reads too).
fn soname() -> String {
    let pin =
        std::fs::read_to_string(crate::root().join("third_party/tdlib/tdlib.pin")).expect("pin");
    pin.lines()
        .find_map(|l| l.trim().strip_prefix("soname"))
        .and_then(|l| l.split('"').nth(1))
        .expect("a soname line")
        .to_owned()
}

/// A missing library says where it comes from: the server, and the fetch line.
#[test]
fn a_missing_library_says_how_to_fetch_it() {
    let scratch = Scratch::new("library-missing");
    let error = error_of(checked_library_in(&crate::root(), &scratch.0));
    assert!(error.contains("srv -f target/tdlib"), "{error}");
    assert!(error.contains("cargo xtask tdlib build"), "{error}");
}

/// A file that is not the pinned library is refused (the ELF checks, the genuine-keys row and the
/// manifest, which are `tdlib path`'s), in fixed words that name no path.
#[test]
fn a_file_that_is_not_the_pinned_library_is_refused() {
    let scratch = Scratch::new("library-bad");
    std::fs::write(scratch.0.join(soname()), "not a library\n").expect("file");
    std::fs::write(scratch.0.join("manifest.toml"), "sha256 = \"0\"\n").expect("manifest");
    let error = error_of(checked_library_in(&crate::root(), &scratch.0));
    assert!(error.contains("could not be examined"), "{error}");
    assert!(
        !error.contains(scratch.0.to_str().expect("utf-8")),
        "a path: {error}"
    );
}

/// H-3: the library comes with the sum of the bytes that were checked. Where the pinned library is on
/// this machine (the lane's builder has it; its run sets `KOURIER_REQUIRE_TDLIB=1`, and then a missing
/// library fails) the sum is the file's own, from another read. Mutant: a sum of other bytes.
#[test]
fn the_checked_library_comes_with_the_sum_of_the_bytes_that_were_checked() {
    let dir = crate::target_dir().join("tdlib");
    let lib = dir.join(soname());
    if !lib.is_file() {
        assert!(
            std::env::var_os("KOURIER_REQUIRE_TDLIB").is_none(),
            "KOURIER_REQUIRE_TDLIB is set and the pinned library is not here"
        );
        eprintln!("SKIPPED: the pinned library is not on this machine");
        return;
    }
    let (path, sum) = checked_library_in(&crate::root(), &dir).expect("the pinned library passes");
    assert_eq!(path, lib);
    assert_eq!(sum, sha256_of(&lib).expect("the file's own sum"));
    assert_eq!(sum.len(), 64);
}

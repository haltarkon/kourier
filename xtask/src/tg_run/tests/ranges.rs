//! The address list (X8, X8b): the VERIFIED line and the hash of the two blocks it is tied to.

use super::error_of;
use super::preflight::{World, ranges_body, verified_ranges};
use crate::tg_run::preflight::{RANGES_RS, check_ranges, list_bytes, list_sha256};

const REASON: &str = "fetched with the user's go first";

/// X8: an UNVERIFIED text is refused and says why; a VERIFIED line that matches its blocks passes;
/// the override passes with a warning (his word); a missing, doubled or malformed line is refused.
/// Mutant: `return Ok(())` at the top of the marker check.
#[test]
fn the_address_list_must_be_verified() {
    let mut w = World::new("ranges");
    // UNVERIFIED (the lane's own file before his go), the word anywhere, and no marker at all.
    for text in [
        format!("//! UNVERIFIED LIST.\n{}", ranges_body()),
        format!("{}// the list is UNVERIFIED\n", verified_ranges()),
        ranges_body(),
    ] {
        w.git.ranges = text;
        let error = error_of(w.run(&w.args(&[])));
        assert!(error.contains(REASON), "{error}");
        assert!(error.contains("Nothing was started"), "{error}");
        assert_eq!(
            w.library_calls.get(),
            0,
            "the library was checked on an unverified list"
        );
    }
    // A good one passes.
    w.git.ranges = verified_ranges();
    assert_eq!(
        w.run(&w.args(&[])).expect("passes").ranges_verified,
        Some(true)
    );
    // The shape of the line.
    let hash = list_sha256(&ranges_body()).expect("hash");
    let fetched = "ab".repeat(32);
    let good_line = format!("//! VERIFIED 2026-11-01 sha256:{fetched} list-sha256:{hash} source");
    check_ranges(&format!("{good_line}\n{}", ranges_body())).expect("the shape is fine");
    let bad_lines = [
        format!("//! VERIFIED 2026-13-01 sha256:{fetched} list-sha256:{hash} source"),
        format!("//! VERIFIED 2026-11-1 sha256:{fetched} list-sha256:{hash} source"),
        format!(
            "//! VERIFIED 2026-11-01 sha256:{} list-sha256:{hash} source",
            "ab".repeat(31)
        ),
        format!(
            "//! VERIFIED 2026-11-01 sha256:{} list-sha256:{hash} source",
            "AB".repeat(32)
        ),
        format!("//! VERIFIED 2026-11-01 list-sha256:{hash} sha256:{fetched} source"),
        format!("//! VERIFIED 2026-11-01 sha256:{fetched} list-sha256:{hash}"),
        format!("//! VERIFIED 2026-11-01 sha256:{fetched}"),
        "//! VERIFIED".to_owned(),
    ];
    for line in &bad_lines {
        let error = error_of(check_ranges(&format!("{line}\n{}", ranges_body())));
        assert!(error.contains(REASON), "{line}: {error}");
    }
    // Two lines are not one; none is not one.
    let two = format!("{good_line}\n{good_line}\n{}", ranges_body());
    assert!(error_of(check_ranges(&two)).contains("2 VERIFIED line(s)"));
    assert!(error_of(check_ranges(&ranges_body())).contains("0 VERIFIED line(s)"));
    // The override passes with a warning, whatever the file says, and the library is still checked.
    w.git.ranges = ranges_body();
    let cleared = w
        .run(&w.args(&["--allow-unverified-ranges"]))
        .expect("passes");
    assert_eq!(cleared.ranges_verified, Some(false));
    assert!(
        cleared.warnings.iter().any(|m| m.contains("NOT verified")),
        "{:?}",
        cleared.warnings
    );
    assert!(w.library_calls.get() > 0);
}

/// X8b (critic 12): the VERIFIED line is tied to the list. A block edited after it was verified is
/// refused; a comment, the line's date or its source is not an edit of the list. Mutant: the
/// list-hash compare dropped.
#[test]
fn a_list_edited_after_it_was_verified_is_refused() {
    let mut w = World::new("hash");
    let edited = verified_ranges().replace(
        "(Ipv4Addr::new(10, 1, 0, 0), 16)",
        "(Ipv4Addr::new(10, 2, 0, 0), 16)",
    );
    assert_ne!(edited, verified_ranges());
    w.git.ranges = edited;
    let error = error_of(w.run(&w.args(&[])));
    assert!(error.contains("edited after it was verified"), "{error}");
    assert!(error.contains(REASON), "{error}");
    assert_eq!(w.library_calls.get(), 0);
    // The v6 block counts too.
    let edited = verified_ranges().replace("0x2001, 0xdb8", "0x2001, 0xdb9");
    assert_ne!(edited, verified_ranges());
    w.git.ranges = edited;
    assert!(error_of(w.run(&w.args(&[]))).contains("edited after it was verified"));
    // A comment above a block, the line's date and its source are not the list.
    let moved = verified_ranges()
        .replace(
            "/// Telegram's IPv4 blocks.",
            "/// Telegram's IPv4 blocks, as published.",
        )
        .replace("2026-11-01", "2026-11-02")
        .replace("fetched with his go", "fetched again with his go");
    w.git.ranges = moved;
    w.run(&w.args(&[])).expect("only the blocks are the list");
}

/// The bytes the hash covers: the two blocks in their order, from their first line to their `];`,
/// each line ended by a newline; a missing, doubled or unended block is an error naming it.
#[test]
fn the_list_hash_covers_exactly_the_two_blocks() {
    let body = ranges_body();
    let bytes = String::from_utf8(list_bytes(&body).expect("bytes")).expect("utf-8");
    let want = "pub(crate) const TELEGRAM_V4: &[(Ipv4Addr, u8)] = &[\n    (Ipv4Addr::new(10, 0, 0, 0), 8),\n    (Ipv4Addr::new(10, 1, 0, 0), 16),\n];\npub(crate) const TELEGRAM_V6: &[(Ipv6Addr, u8)] = &[\n    (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),\n];\n";
    assert_eq!(bytes, want);
    let hash = list_sha256(&body).expect("hash");
    assert_eq!(hash.len(), 64);
    assert!(
        hash.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
    // The order is the blocks' own (v4 then v6) whatever order the file has them in.
    let swapped = {
        let v4 = body.find("/// Telegram's IPv4").expect("v4");
        let v6 = body.find("/// Telegram's IPv6").expect("v6");
        let end = body.find("\npub(crate) const PORTS").expect("ports");
        format!(
            "{}{}\n{}\n{}",
            &body[..v4],
            &body[v6..end],
            &body[v4..v6],
            &body[end..]
        )
    };
    assert_eq!(list_sha256(&swapped).expect("hash"), hash);
    // The same text with CRLF line ends is the same list.
    assert_eq!(
        list_sha256(&body.replace('\n', "\r\n")).expect("hash"),
        hash
    );
    // The prose, the ports and the marker line are not the list.
    assert_eq!(
        list_sha256(&format!("//! anything\n{body}// trailing\n")).expect("hash"),
        hash
    );
    assert_eq!(list_sha256(&verified_ranges()).expect("hash"), hash);
    // Broken files.
    let no_v6 = body.replace("TELEGRAM_V6", "TELEGRAM_V9");
    assert!(error_of(list_bytes(&no_v6)).contains("no block"), "no v6");
    let twice = format!("{body}{body}");
    assert!(error_of(list_bytes(&twice)).contains("twice"));
    let unended = body.replace("\n];\n\n/// Telegram's IPv6", "\n)\n\n/// Telegram's IPv6");
    assert_ne!(unended, body);
    // V4's `];` is gone: the block runs to V6's end, which is the hash of another text, not an error.
    assert_ne!(list_sha256(&unended).expect("hash"), hash);
    let no_end = body.replace("];", ")");
    assert!(error_of(list_bytes(&no_end)).contains("no end"));
}

/// The real file of the working tree has the two blocks under the names the tool reads (a rename of
/// either would otherwise break `tg-run ranges` and every connect with a message about a block).
#[test]
fn the_real_address_file_has_the_blocks_the_tool_hashes() {
    let text = std::fs::read_to_string(crate::root().join(RANGES_RS)).expect("ranges.rs");
    let bytes = list_bytes(&text).expect("both blocks");
    let text = String::from_utf8(bytes).expect("utf-8");
    assert!(text.starts_with("pub(crate) const TELEGRAM_V4"));
    assert!(text.contains("pub(crate) const TELEGRAM_V6"));
    assert_eq!(list_sha256(&text).map(|h| h.len()).ok(), Some(64));
}

/// The real file passes the check `connect` makes on it: an edit of a block without a new fetch
/// fails here, in ci, not at the first real run.
#[test]
fn the_real_address_file_is_verified() {
    let text = std::fs::read_to_string(crate::root().join(RANGES_RS)).expect("ranges.rs");
    check_ranges(&text).expect("the VERIFIED line matches the two blocks");
}

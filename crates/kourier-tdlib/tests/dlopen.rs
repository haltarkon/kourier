//! The real-library loader's errors (TG-1 plan §5.4 T-a9). No TDLib is loaded: a missing file,
//! and a system library that is not TDLib.

use std::path::Path;

use kourier_tdlib::dlopen::{LibError, open};

#[test]
fn a_missing_library_and_a_library_without_tdjson_are_typed_errors() {
    assert_eq!(
        open(Path::new("/nonexistent/libtdjson.so")).err(),
        Some(LibError::Open)
    );
    // zlib is on every machine that builds Kourier and has none of TDLib's entry points.
    assert_eq!(
        open(Path::new("libz.so.1")).err(),
        Some(LibError::Symbol("td_create_client_id"))
    );
}

//! The blob builder of the unit tests (W4 C2): the hand-made chunks every history test starts from.
//!
//! The builder is ONE source file, `testing/blob.rs`: this module is its door in the library's test
//! build, and the integration tests include the same file through `tests/support/history.rs`, so the
//! two can never drift apart.

mod blob;

pub(crate) use blob::*;

#[cfg(test)]
mod tests;

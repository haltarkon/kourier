//! The blob builder of the history tests, the very source file of the library's unit tests
//! (`src/history/testing/blob.rs`, W4 C2): a hand-made `HistorySync` chunk is built the same way in
//! both, and a change to the builder is seen by both.

#[path = "../../src/history/testing/blob.rs"]
mod blob;

pub use blob::*;

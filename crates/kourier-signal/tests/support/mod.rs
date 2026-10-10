//! Helpers shared by the integration tests (and the `skeleton` example) of `kourier-signal`.

// Every test binary compiles this module and uses only part of it.
#![allow(dead_code)]

pub mod fake_core;
pub mod harness;

//! Test-only crate: end-to-end tests of the real `kourier-core` runtime driving real backends
//! against local test servers (docs/architecture.md §10.1). Everything is in `tests/`:
//!
//! - `tests/signal_core.rs`: `kourier-core` + `kourier-signal` + `@signalapp/mock-server`
//!   (through `tests/mock-harness`, §10.3): link through the facade, receive, restart, reply.
//!
//! The harness tests skip with a message when Node ≥ 24 or the harness's `node_modules` is
//! missing; `KOURIER_REQUIRE_MOCK_HARNESS=1` turns the skip into a failure.

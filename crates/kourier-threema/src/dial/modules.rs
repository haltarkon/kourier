//! The module paths of the transport crates this directory links, for the log rule
//! ([`crate::logs`], TH4 J7).
//!
//! The list lives HERE because `tests/builder.rs` rule 1 lets this directory alone name a socket, TLS,
//! WebSocket or HTTP crate (comments included); `crate::logs` re-exports it without writing a name.
//! A record's module path starts with the crate's library name (a `-` is `_`); the rule judges whole
//! first segments, so a longer name is not one of them.

/// The crates `src/dial/` links for its TLS, WebSocket and HTTPS, by the first segment of the module path.
/// `rustls`, `tokio_rustls`, `tokio_tungstenite` and `reqwest` are Threema's own dependencies; the rest
/// come under them and log through `log` or `tracing`: `tungstenite`, `hyper`, `hyper_util`,
/// `hyper_rustls`, `h2`, `tower`, `tower_http`, `rustls_platform_verifier` (reqwest names it; the dial hands
/// reqwest its own verifier, so it is silent today) and rustls-webpki, whose library is `webpki`.
///
/// Left out on purpose: the `http` types crate, `http-body*`, `want`, `webpki-roots` and `rustls-pki-types`
/// (types and data, no log call), and `tokio`, `tokio-util`, `mio`, `socket2`, `url` (shared by everything
/// in the process; `mio` and `tokio-util` log a TRACE line of their own at most). `tests/builder.rs` reads
/// the lock and this crate's manifest: a name here is a package of the build, and a new direct dependency
/// that is a socket, TLS, WebSocket or HTTP crate is named here or listed there with the reason it logs
/// nothing.
pub const TRANSPORT_MODULES: &[&str] = &[
    "rustls",
    "tokio_rustls",
    "webpki",
    "rustls_platform_verifier",
    "tokio_tungstenite",
    "tungstenite",
    "reqwest",
    "hyper",
    "hyper_util",
    "hyper_rustls",
    "h2",
    "tower",
    "tower_http",
];

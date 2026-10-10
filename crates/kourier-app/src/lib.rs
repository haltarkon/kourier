//! Composition root (§2.2, §8.2): the tokio runtime, XDG paths, the master key and per-account
//! key wraps, the registry of compiled-in networks (cargo features `signal`, `fake`), logging,
//! and the core; the UI gets an [`AppHandle`].
//!
//! - [`paths`]: XDG directories, `KOURIER_HOME`, `--profile`.
//! - [`keys`]: master key (Secret Service or passphrase), `core.db` key, `key.wrap` files.
//! - [`vault`]: the core's `AccountVault` over the keyring.
//! - [`config`]: the backend environment (`KOURIER_SIGNAL_ENV` in debug builds).
//! - `dev` (debug builds only): the development keyring (`KOURIER_DEV_KEYRING`).
//! - [`logging`]: `tracing` to stderr and a rotating file.
//! - [`prefs`]: app preferences in `kourier.toml` (§8.9).
//! - [`desktop`]: notifications, tray, single instance, autostart (§8.7, §8.8).
//! - [`app`]: [`App::start`], [`AppHandle`], [`App::shutdown`]; fixture mode.
//! - [`media`]: avatar handles for the UI's image provider and pasted composer files (§8.6).
//! - [`folders`]: a folder given to the composer: the walk, and the one archive made of it.
//! - [`about`]: name, version, summary, licence, links and credits, as plain data for a front-end's
//!   About page (§8.10).
//! - [`process`]: release builds are non-dumpable (§9.4; cargo feature `dumpable` skips it); the
//!   panic hook that hands a redacted one-line message to the front-end's crash reporter.
//! - [`trace`]: timing marks for the quality targets (`KOURIER_TRACE_STARTUP=1`, §1.3).
//! - `perf` (cargo feature `perf`): the reference profile `cargo xtask perf` measures.
//!
//! The UI uses the core only through [`facade`] (re-exported `kourier_core::facade`).

pub mod about;
pub mod app;
pub mod config;
pub mod desktop;
#[cfg(debug_assertions)]
pub mod dev;
#[cfg(feature = "fake")]
mod fixture_clock;
#[cfg(feature = "fake")]
mod fixture_media;
pub mod folders;
pub mod keys;
pub mod logging;
pub mod media;
pub mod paths;
#[cfg(feature = "perf")]
pub mod perf;
pub mod prefs;
pub mod process;
pub mod trace;
pub mod vault;

pub use app::{
    App, AppError, AppHandle, Arrivals, EXPIRING_TEXT, FixtureInfo, FixtureOptions, FixtureState,
    FixtureTransfer, Launch, NetworkInfo, StartOptions, hostname, set_private_umask,
};
pub use keys::{KeyError, KeyMode, Keyring, PassphraseConfig};
pub use kourier_core::facade;
/// The states of a history transfer, for [`FixtureTransfer`] (the binary names them and
/// depends on no model crate).
pub use kourier_model::{HistoryTransfer, HistoryTransferFailure};
pub use paths::{Paths, Profile};
pub use prefs::{NotificationContent, Prefs};

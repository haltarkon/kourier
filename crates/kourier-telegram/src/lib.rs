//! The Telegram backend of Kourier (architecture §7.5; TG-1 decisions D1), on TDLib.
//!
//! TDLib runs in a helper process per account (`kourier-tdlib`, which dlopens the pinned
//! libtdjson); this crate supervises it through the frame protocol of `kourier_tdlib` and never
//! loads TDLib itself. The shape follows the WhatsApp backend: a `Network` whose `spawn` starts
//! one account thread, a `backend.db` of the component `telegram`, and a REQUIRED seam (the
//! helper parts) that only `kourier-app` fills with the real one.
//!
//! Offline in this build (TG-1b): nothing here contacts Telegram. Starting a login starts the
//! account's gate and helper, runs TDLib's start up to its phone-number step, stops the helper and
//! answers a typed "not yet"; the application's dialer refuses every connection; the only API
//! credentials a test sees are the dummy pair.
//!
//! Modules: `pin` (the TDLib this build expects), `api` (the API credentials), `keys` (the key
//! of TDLib's database), `db` (the account's `backend.db`), `error`, `parts` (the required seam),
//! and `real`, the ONE module that builds the application's parts (only `kourier-app` calls it).
//! The link (the app's end of the helper protocol), the descriptor, the network and the account
//! actor follow it; `gate` is the SOCKS5 gate TDLib's connections go through and `supervisor`
//! starts and stops the helper (TG-1b).

mod actor;
mod api;
mod content;
mod db;
mod descriptor;
mod error;
mod gate;
mod keys;
mod link;
mod login;
mod markup;
mod network;
mod number;
mod outbox;
mod parts;
mod pin;
mod presence;
pub mod real;
mod receive;
#[cfg(test)]
mod replay;
mod session;
mod states;
mod store;
mod supervisor;
mod sync;
mod tdapi;
#[cfg(test)]
mod test_log;

pub use api::ApiCredentials;
pub use error::OpenError;
pub use network::TelegramNetwork;
pub use parts::{HelperArgs, HelperChild, HelperFiles, HelperProcess, TgEnv, TgParts};
pub use pin::{PIN, Pin};

/// The offline child of the real-library tests, for `kourier-app`'s ONE application-level test of
/// them (TG-1b B-5, TG-1a Q4): the test binary re-executed under `unshare -rn` with every address
/// routed to `lo`, and the trap listeners that count every direct attempt there. Test support
/// only; the real library is never loaded outside such a child.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod offline {
    pub use kourier_tdlib::spawn::{
        OfflineRun, in_offline_child, offline_available, run_self_offline,
    };
    pub use kourier_tdlib::traps::Traps;

    /// The detail of the typed answer of a login whose parts carry no API pair.
    pub use crate::actor::NO_API_KEY;

    /// Whether the TCP dialer of the Telegram mode would be PERMITTED to dial `to` in this process:
    /// the answer of its permission check (the list, its ports and the tripwire), asked WITHOUT
    /// opening a socket and without naming any mode (TR R0a fix G-1). Outside the offline child it
    /// is `false` for every address, a listed one included: a test of any binary that links this
    /// crate with `test-support` proves its tripwire is armed by asking this.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn a_dial_is_permitted(to: std::net::SocketAddr) -> bool {
        crate::gate::dialer::tcp_permission(to).is_ok()
    }

    /// An address inside the gate's list on a listed port, derived from the list's own constants (a
    /// test never writes a Telegram address): what a test hands to [`a_dial_is_permitted`].
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn a_listed_address() -> std::net::SocketAddr {
        use crate::gate::ranges;
        let first = ranges::TELEGRAM_V4[0].0;
        std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::from_bits(first.to_bits() + 1)),
            ranges::PORTS[0],
        )
    }
}

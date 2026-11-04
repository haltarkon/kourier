//! The TDLib helper of Kourier's Telegram backend (architecture §7.5; TG-1 decisions D1).
//!
//! TDLib never enters Kourier's own process. Each Telegram account gets one helper process, the
//! binary `kourier-tdlib` of this crate, which opens the pinned shared library at run time and
//! speaks TDLib's JSON interface as tagged lines over ONE Unix socket on fd 3. This crate holds
//! the parts both sides share, so the frame format has one source of truth:
//!
//! - the frame protocol (`protocol`, TG-1a C5);
//! - the serve loop the helper runs (`serve`, C6/C7): it hardens TDLib's log before it creates any
//!   client, refuses log and proxy requests from the app, keeps ONE receiving thread, answers
//!   control frames from a queue of their own, and closes every client on quit;
//! - the engine trait with two engines: the real library opened at run time (`dlopen`, C7: the
//!   one file that names TDLib's entry points) and, behind the feature `scripted`, a scripted
//!   TDLib for tests that models TDLib's request parking;
//! - the spawner of the helper process (`spawn`, C7: the one file that names a process command).
//!
//! Nothing here contacts any server: the real library is only loaded by the helper binary, and a
//! test loads it only inside an `unshare -rn` child (TG-1b).

/// The version of the frame protocol; the hello carries it and the app refuses another. 2 (TG-1b):
/// the helper takes `--gate-port` and locks itself down, a `G` frame must name that port; a
/// TG-1a helper (protocol 1) is not a helper this app runs. 3 (TG-2): the helper refuses the
/// requests Kourier never makes (`serve::FORBIDDEN_REQUESTS`) with `F refused-request`; a helper of
/// protocol 2 would forward them, so it is not run either (the app and the helper are one build).
/// 4 (TG-2b, pre3): the helper takes `--database-dir` and `--files-dir`, locks the filesystem to
/// them and refuses parameters naming other directories (`F refused-parameters-paths`); the
/// forbidden list gains the call requests (P-8). A protocol-3 helper is not run. 5 (LC-20, the TDLib
/// 1.8.69 bump): the forbidden list is derived by class (the account's doors: a third party, the
/// password and its recovery e-mail, the login e-mail, sessions, the phone number, account creation
/// and deletion, money: 165 names, 84 before) with the READs named in `serve::ACCOUNT_READS`, and the
/// proxy list gains the two web-proxy-carrier requests (11 names, 9 before). A protocol-4 helper would
/// forward what this app refuses, so it is not run.
pub const PROTOCOL: u32 = 5;

/// The environment variable that names the helper's protocol descriptor.
pub const FD_ENV: &str = "KOURIER_TDLIB_FD";

/// The descriptor the helper speaks the protocol on (stdin and stdout are `/dev/null`).
pub const HELPER_FD: i32 = 3;

pub mod dlopen;
pub mod harden;
pub mod lockdown;
pub mod protocol;
pub mod refuse;
#[cfg(feature = "scripted")]
pub mod scripted;
pub mod serve;
pub mod spawn;
pub mod td;
/// The trap listeners of the offline real-library tests: test support, behind the feature
/// `test-support` (TG-1b fix G8), never in a release build.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod traps;

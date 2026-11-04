//! What the phone is told about this device (W2, chunk C1b).
//!
//! Two things reach the phone, and both are decided here and only here:
//!
//! - The registration payload's `DeviceProps` (what the phone's "Linked devices" list names this
//!   device): `os` is `Kourier (<host name>)` (his decision D3), the platform type is `DESKTOP`,
//!   the version is this crate's, and the sync fields are the browser row of WA Web's table
//!   (`require_full_sync: false`, no days limit, upstream's default `HistorySyncConfig`; older
//!   history is fetched on demand later, lane decision 7). They are applied at EVERY build of the
//!   client, but the phone reads them at pairing only: a renamed computer keeps the old name on the
//!   phone until the next link.
//! - The pair-code request's `companion_platform_display` (what the phone shows when the code is
//!   typed): with `display_os: None` upstream derives the OS from the free-form `DeviceProps.os` by
//!   SUBSTRING (`mac`, `windows`, `ios`, `android`, ...), so a host called `imac` or `windows-box`
//!   would show "Chrome (Mac OS)" or "Chrome (Windows)". [`pair_code_options`] sends the canonical
//!   `Linux` instead, so the phone says "Chrome (Linux)" for every host (critic H3). The browser
//!   part ("Chrome") comes from the `DESKTOP` platform type.

#![allow(
    dead_code,
    reason = "the actor and the pair-code flow that use these arrive in C3 and C4"
)]

use wacore::companion_reg::CompanionOs;
use wacore::pair_code::PairCodeOptions;
use wacore::store::device::{DevicePropsOverride, default_history_sync_config};
use waproto::whatsapp::device_props::{AppVersion, PlatformType};

/// The longest host name used in the device name, in characters.
pub(crate) const HOST_MAX_CHARS: usize = 48;

/// The name this computer has for the phone: `Kourier (<host>)`, `Kourier` when the host name is
/// empty once control characters are gone. The host is cut at [`HOST_MAX_CHARS`] characters on a
/// character border.
pub(crate) fn device_name(host: &str) -> String {
    let host: String = host
        .chars()
        .filter(|c| !c.is_control() && !is_bidi_control(*c))
        .collect();
    let host: String = host.trim().chars().take(HOST_MAX_CHARS).collect();
    let host = host.trim();
    if host.is_empty() {
        "Kourier".to_owned()
    } else {
        format!("Kourier ({host})")
    }
}

/// The marks that reorder or isolate text (U+061C, U+200E, U+200F, U+202A-202E, U+2066-2069).
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// This crate's version as the phone's `DeviceProps.version`.
fn app_version() -> AppVersion {
    let part = |s: &str| s.parse::<u32>().unwrap_or(0);
    let mut version = AppVersion::default();
    version.primary = Some(part(env!("CARGO_PKG_VERSION_MAJOR")));
    version.secondary = Some(part(env!("CARGO_PKG_VERSION_MINOR")));
    version.tertiary = Some(part(env!("CARGO_PKG_VERSION_PATCH")));
    version
}

/// The `DeviceProps` of the registration payload, as an override of upstream's defaults (which
/// are `os: "rust"` and `UNKNOWN`). It sets every field the override has, so no default of
/// upstream's decides what the phone is told.
pub(crate) fn device_props(host: &str) -> DevicePropsOverride {
    DevicePropsOverride::new()
        .with_os(device_name(host))
        .with_version(app_version())
        .with_platform_type(PlatformType::DESKTOP)
        .with_require_full_sync(false)
        .with_history_sync_config(default_history_sync_config())
}

/// The options of a pair-code request for `phone_number` (digits with the country code, no `+`):
/// the push notification as WA Web asks for it, a random code, the browser part from the platform
/// type, and the OS part fixed to `Linux` (see the module doc).
pub(crate) fn pair_code_options(phone_number: &str) -> PairCodeOptions {
    PairCodeOptions {
        phone_number: phone_number.to_owned(),
        show_push_notification: true,
        custom_code: None,
        platform_id: None,
        display_os: Some(CompanionOs::Linux.wire_str().to_owned()),
    }
}

#[cfg(test)]
mod tests;

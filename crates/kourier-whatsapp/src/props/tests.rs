//! T-3 and T-4: the registration payload names Kourier, and the pair-code display is
//! "Chrome (Linux)" whatever the host is called.

use wacore::pair_code::resolve_companion_platform;
use wacore::store::device::{Device, default_history_sync_config};
use waproto::buffa::Message;
use waproto::whatsapp as wa;

use super::*;

/// The `DeviceProps` a device built with `props` sends in its REGISTRATION payload (no number yet):
/// the bytes the phone reads, decoded again.
fn registered_props(props: DevicePropsOverride) -> wa::DeviceProps {
    let mut device = Device::new();
    device.set_device_props(props);
    let payload = device.get_client_payload();
    let bytes = payload
        .device_pairing_data
        .as_option()
        .expect("a registration payload has the pairing data")
        .device_props
        .clone()
        .expect("the pairing data holds the device props");
    wa::DeviceProps::decode_from_slice(bytes.as_slice()).expect("DeviceProps decode")
}

#[test]
fn the_registration_payload_names_kourier() {
    let props = registered_props(device_props("testhost"));
    assert_eq!(props.os.as_deref(), Some("Kourier (testhost)"));
    assert_eq!(props.platform_type, Some(PlatformType::DESKTOP));
    // The browser row of WA Web's table: no full backfill, upstream's default sync claims.
    assert_eq!(props.require_full_sync, Some(false));
    assert_eq!(
        props.history_sync_config.as_option(),
        Some(&default_history_sync_config())
    );
    let version = props.version.as_option().expect("version");
    assert_eq!(
        (version.primary, version.secondary, version.tertiary),
        (
            Some(env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap()),
            Some(env!("CARGO_PKG_VERSION_MINOR").parse().unwrap()),
            Some(env!("CARGO_PKG_VERSION_PATCH").parse().unwrap())
        )
    );
    // Upstream's own default identity never reaches the phone.
    assert_ne!(props.os.as_deref(), Some("rust"));
    assert_ne!(props.platform_type, Some(PlatformType::UNKNOWN));
}

#[test]
fn a_host_name_is_cleaned_and_cut() {
    assert_eq!(device_name("laptop"), "Kourier (laptop)");
    assert_eq!(device_name(""), "Kourier");
    assert_eq!(device_name("  \n\t "), "Kourier");
    assert_eq!(device_name("a\u{0}b\nc\u{202e}d"), "Kourier (abcd)");
    let long = device_name(&"x".repeat(200));
    assert_eq!(long, format!("Kourier ({})", "x".repeat(HOST_MAX_CHARS)));
    // A cut never splits a character.
    let wide = device_name(&"ж".repeat(200));
    assert_eq!(wide, format!("Kourier ({})", "ж".repeat(HOST_MAX_CHARS)));
    // The cut runs after the clean-up, so control characters do not use up the length.
    let noisy = format!("{}{}", "\u{1}".repeat(100), "y".repeat(10));
    assert_eq!(device_name(&noisy), format!("Kourier ({})", "y".repeat(10)));
}

#[test]
fn the_payload_carries_the_cleaned_name_for_a_hostile_host() {
    let props = registered_props(device_props(&format!("\u{7}{}", "z".repeat(200))));
    let os = props.os.expect("os");
    assert_eq!(os, format!("Kourier ({})", "z".repeat(HOST_MAX_CHARS)));
    assert!(!os.chars().any(char::is_control));
}

fn display_for(host: &str, options: &PairCodeOptions) -> String {
    let mut device = Device::new();
    device.set_device_props(device_props(host));
    resolve_companion_platform(options, &device.device_props).1
}

/// Host names that make upstream's substring rule pick another OS than Linux.
const TRICKY_HOSTS: [&str; 10] = [
    "testhost",
    "",
    "imac",
    "mac-mini",
    "MacBook-Pro",
    "windows-box",
    "ios-lab",
    "android-dev",
    "my-iphone",
    "darwin-ci",
];

#[test]
fn the_phone_number_way_shows_chrome_on_linux_for_every_host() {
    let options = pair_code_options("15555550100");
    for host in TRICKY_HOSTS {
        assert_eq!(
            display_for(host, &options),
            "Chrome (Linux)",
            "host {host:?}"
        );
    }
}

#[test]
fn upstreams_own_derivation_would_have_shown_the_hosts_os() {
    // The premise of `display_os: Some("Linux")`: without it a host called `imac` shows another OS.
    // If upstream stops deriving from the name, this fails and the override can go.
    let plain = PairCodeOptions::default();
    assert_eq!(display_for("imac", &plain), "Chrome (Mac OS)");
    assert_eq!(display_for("windows-box", &plain), "Chrome (Windows)");
    assert_eq!(display_for("plain", &plain), "Chrome (Linux)");
}

#[test]
fn the_pair_code_options_ask_for_the_push_and_a_random_code() {
    let options = pair_code_options("15555550100");
    assert_eq!(options.phone_number, "15555550100");
    assert!(options.show_push_notification);
    assert_eq!(options.custom_code, None);
    assert_eq!(options.platform_id, None);
    assert_eq!(options.display_os.as_deref(), Some("Linux"));
}

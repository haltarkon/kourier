//! The D2M leg's setup: libthreema's `D2mContext` from the stored identity and the parts' servers
//! (brief §2.5). The policies are Kourier's: REJECT when every slot is used (never drop another
//! device; Desktop's choice, `layer-3-codec.ts:842-843`) and PERSISTENT (the slot stays while
//! Kourier is closed); the expected slot state is the STORED one and the label the stored one,
//! fixed at the join. The device info a phone lists says the platform DESKTOP
//! ([`device_info_client`]).

use libthreema::common::config::MediatorServerBaseUrl;
use libthreema::common::keys::DeviceGroupKey;
use libthreema::common::{ChatServerGroup, ClientInfo, D2xDeviceId};
use libthreema::d2m::D2mContext;
use libthreema::protobuf::d2m::client_hello::DeviceSlotsExhaustedPolicy;
use libthreema::protobuf::d2m::{DeviceSlotExpirationPolicy, DeviceSlotState};

use crate::ServerParams;
use crate::store::{Identity, SlotState};

/// The CSP client-info's locale (call C14): fixed, it says nothing about the user.
const LOCALE: &str = "en/US";

/// Kourier as libthreema's patch P4 names a third-party client.
pub(crate) fn client_info() -> ClientInfo {
    ClientInfo::ThirdParty {
        name: "Kourier".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        locale: LOCALE.to_owned(),
        os_name: std::env::consts::OS.to_owned(),
        os_architecture: std::env::consts::ARCH.to_owned(),
    }
}

/// What libthreema derives the ClientHello's `d2d.DeviceInfo` from (`D2mContext::client_info`, read only
/// by `to_device_info` in the ClientHello, `third_party/libthreema/lib/src/d2m/mod.rs` "Encrypt the device
/// info"; the CSP login and the user agent keep [`client_info`]). RT1 attempt 2: P4's `ThirdParty` states the
/// platform UNSPECIFIED, and Threema for Android 6.6.0 shows such a device as "Device" with "Information could
/// not be obtained" whatever its label (`LinkedDeviceInfoUiModel.isValid()` is `platform != UNSPECIFIED`,
/// the same text as a device info that does not open). So the platform is DESKTOP, the spec's "Desktop
/// application" (`md-d2d.proto` `DeviceInfo.Platform`) and what Threema Desktop states
/// (`layer-3-codec.ts:890`); the details stay P4's `Kourier <version> (<os>)` and the version Kourier's. The
/// server never reads it (DGDIK). If a libthreema update reads this field for anything else, look again.
pub(crate) fn device_info_client() -> ClientInfo {
    ClientInfo::Desktop {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        locale: LOCALE.to_owned(),
        renderer_name: "Kourier".to_owned(),
        renderer_version: env!("CARGO_PKG_VERSION").to_owned(),
        os_name: std::env::consts::OS.to_owned(),
        os_architecture: std::env::consts::ARCH.to_owned(),
    }
}

/// The D2M context of one connect. `expected` is what the slot is believed to be (the stored state,
/// or N5's one retry). An error names the field that did not convert (never its value).
pub(crate) fn context(
    identity: &Identity,
    server: &ServerParams,
    expected: SlotState,
) -> Result<D2mContext, &'static str> {
    Ok(D2mContext {
        mediator_server_url: MediatorServerBaseUrl::try_from(server.mediator_url.clone())
            .map_err(|_| "the mediator URL")?,
        device_group_key: DeviceGroupKey::from(*identity.device_group_key),
        csp_server_group: ChatServerGroup::try_from(identity.server_group.as_str())
            .map_err(|_| "the server group")?,
        device_id: D2xDeviceId(identity.d2m_device_id),
        device_slots_exhausted_policy: DeviceSlotsExhaustedPolicy::Reject,
        device_slot_expiration_policy: DeviceSlotExpirationPolicy::Persistent,
        expected_device_slot_state: match expected {
            SlotState::New => DeviceSlotState::New,
            SlotState::Existing => DeviceSlotState::Existing,
        },
        client_info: device_info_client(),
        device_label: identity.device_label.clone(),
    })
}

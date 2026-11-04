//! The D2M leg's setup: libthreema's `D2mContext` from the stored identity and the parts' servers
//! (brief §2.5). The policies are Kourier's: REJECT when every slot is used (never drop another
//! device; Desktop's choice, `layer-3-codec.ts:842-843`) and PERSISTENT (the slot stays while
//! Kourier is closed); the expected slot state is the STORED one and the label the stored one,
//! fixed at the join.

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
        client_info: client_info(),
        device_label: identity.device_label.clone(),
    })
}

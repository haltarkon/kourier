//! Accounts, their synced settings and Kourier-local preferences (architecture §3.2).

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ids::{AccountId, AccountUuid, NetworkKind, UserId};
use crate::patch::{Patch, apply_set};

/// An account row (§3.2, §5.4). `Debug` hides the display name.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Core rowid.
    pub id: AccountId,
    /// Stable uuid.
    pub uuid: AccountUuid,
    /// Network.
    pub network: NetworkKind,
    /// Lifecycle state.
    pub state: AccountState,
    /// Our ids on the network (Signal: `[aci, pni?]`; WhatsApp: `[lid, pn]`; Threema: `[id]`).
    #[serde(default)]
    pub own_ids: Vec<UserId>,
    /// Our device id.
    #[serde(default)]
    pub own_device: Option<u32>,
    /// Own profile name, for the account switcher.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Settings synced from the network.
    #[serde(default)]
    pub settings: AccountSettings,
    /// Kourier-only preferences.
    #[serde(default)]
    pub local: AccountLocalPrefs,
}

crate::redacting_debug!(Account {
    id, uuid, network, state, own_ids, own_device, settings, local
} redact { display_name });

/// Account lifecycle (§4.3).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AccountState {
    /// Login in progress.
    Linking,
    /// Linked and usable.
    Active,
    /// Unlinked; history stays readable, "Link again" is offered.
    LoggedOut {
        /// Log-safe reason.
        #[serde(default)]
        reason: String,
    },
    /// Being removed.
    Removing,
    /// "Unlink…" is in progress: the device logs out on the network. A start that finds it
    /// (the logout was cut off) makes one more attempt and then finishes it as `LoggedOut`
    /// with the chats kept (link feedback L4).
    Unlinking,
}

/// What became of this device on the network when it logged out on the user's request ("Remove
/// Account…", "Unlink…"; architecture §4.3 step 7, link feedback L4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeviceLogout {
    /// The network confirmed that the device is gone from the account, or no longer knew it
    /// (the phone had removed it already).
    Removed,
    /// There was no device to remove: the link never finished, or the account was logged out
    /// already (its credentials are gone).
    NoDevice,
    /// The network could not be told (offline, no answer in time, refused): the device may still
    /// be listed among the phone's linked devices.
    NotReached,
}

/// A backend's report of its logout ([`DeviceLogout`] and the name the device was linked with,
/// which the phone lists). `Debug` hides the name.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogoutReport {
    /// What became of the device.
    pub outcome: DeviceLogout,
    /// The device's name as the phone shows it, when the backend knows it (personal data: it is
    /// often the computer's host name).
    #[serde(default)]
    pub device_name: Option<String>,
}

crate::redacting_debug!(LogoutReport { outcome } redact { device_name });

impl LogoutReport {
    /// A report without a name.
    pub fn new(outcome: DeviceLogout) -> Self {
        Self {
            outcome,
            device_name: None,
        }
    }
}

/// Settings the network syncs (owned by the backend, reported with `AccountSettings` events).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountSettings {
    /// Send read receipts.
    pub read_receipts: bool,
    /// Send and show typing indicators.
    pub typing_indicators: bool,
    /// Generate link previews.
    pub link_previews: bool,
    /// Show sealed-sender indicators.
    pub sealed_sender_indicators: bool,
    /// Default disappearing timer for new chats.
    pub universal_timer: Option<Duration>,
    /// Whether our phone number is shared with contacts.
    pub share_phone_number: Option<bool>,
}

impl Default for AccountSettings {
    fn default() -> Self {
        Self {
            read_receipts: true,
            typing_indicators: true,
            link_previews: false,
            sealed_sender_indicators: false,
            universal_timer: None,
            share_phone_number: None,
        }
    }
}

/// Field-wise change of [`AccountSettings`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountSettingsPatch {
    /// Read receipts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_receipts: Option<bool>,
    /// Typing indicators.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub typing_indicators: Option<bool>,
    /// Link previews.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_previews: Option<bool>,
    /// Sealed-sender indicators.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sealed_sender_indicators: Option<bool>,
    /// Universal timer.
    #[serde(skip_serializing_if = "Patch::is_unchanged")]
    pub universal_timer: Patch<Duration>,
    /// Phone number sharing.
    #[serde(skip_serializing_if = "Patch::is_unchanged")]
    pub share_phone_number: Patch<bool>,
}

impl AccountSettingsPatch {
    /// Applies the patch; returns whether any field was touched.
    pub fn apply(self, s: &mut AccountSettings) -> bool {
        let mut touched = apply_set(self.read_receipts, &mut s.read_receipts);
        touched |= apply_set(self.typing_indicators, &mut s.typing_indicators);
        touched |= apply_set(self.link_previews, &mut s.link_previews);
        touched |= apply_set(
            self.sealed_sender_indicators,
            &mut s.sealed_sender_indicators,
        );
        touched |= self.universal_timer.apply(&mut s.universal_timer);
        touched |= self.share_phone_number.apply(&mut s.share_phone_number);
        touched
    }
}

/// Kourier-only per-account preferences (never sent to the network; backends see them read-only
/// through `AccountContext::local_prefs`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountLocalPrefs {
    /// Desktop notifications for this account.
    pub notifications: bool,
    /// Muted on this computer until (ms); `i64::MAX` = forever.
    pub muted_until: Option<i64>,
    /// What is downloaded without a click.
    pub auto_download: AutoDownloadPolicy,
}

impl Default for AccountLocalPrefs {
    fn default() -> Self {
        Self {
            notifications: true,
            muted_until: None,
            auto_download: AutoDownloadPolicy::default(),
        }
    }
}

/// Automatic attachment download policy (§5.8 defaults: images, stickers, voice notes and files
/// up to 25 MiB; videos on click).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoDownloadPolicy {
    /// Images.
    pub images: bool,
    /// Stickers.
    pub stickers: bool,
    /// Voice notes.
    pub voice: bool,
    /// Videos.
    pub videos: bool,
    /// Other files up to this size.
    pub files_max_bytes: u64,
}

impl Default for AutoDownloadPolicy {
    fn default() -> Self {
        Self {
            images: true,
            stickers: true,
            voice: true,
            videos: false,
            files_max_bytes: 25 * 1024 * 1024,
        }
    }
}

/// One of our own devices (answer to `ListOwnDevices`). `Debug` hides the name.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnDevice {
    /// Device id.
    pub id: u32,
    /// Device name.
    #[serde(default)]
    pub name: Option<String>,
    /// Linked at (ms).
    #[serde(default)]
    pub created_at: Option<i64>,
    /// Last seen (ms).
    #[serde(default)]
    pub last_seen: Option<i64>,
    /// This Kourier instance.
    #[serde(default)]
    pub this_device: bool,
}

crate::redacting_debug!(OwnDevice { id, created_at, last_seen, this_device } redact { name });

/// Change of our own profile (`ChangeProfile`). The avatar is a path to an image file.
/// `Debug` shows only which fields change.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfilePatch {
    /// Profile name.
    #[serde(skip_serializing_if = "Patch::is_unchanged")]
    pub name: Patch<String>,
    /// About text.
    #[serde(skip_serializing_if = "Patch::is_unchanged")]
    pub about: Patch<String>,
    /// Avatar image file.
    #[serde(skip_serializing_if = "Patch::is_unchanged")]
    pub avatar: Patch<PathBuf>,
}

crate::redacting_debug!(ProfilePatch {} redact { name, about, avatar });

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_patch_semantics() {
        let mut s = AccountSettings {
            universal_timer: Some(Duration::from_secs(60)),
            ..AccountSettings::default()
        };
        assert!(!AccountSettingsPatch::default().apply(&mut s));
        let p = AccountSettingsPatch {
            read_receipts: Some(false),
            universal_timer: Patch::Clear,
            ..AccountSettingsPatch::default()
        };
        assert!(p.apply(&mut s));
        assert!(!s.read_receipts && s.typing_indicators);
        assert_eq!(s.universal_timer, None);
    }

    #[test]
    fn local_prefs_defaults_survive_missing_fields() {
        let p: AccountLocalPrefs = serde_json::from_str("{}").expect("deserialize");
        assert!(p.notifications && p.auto_download.images && !p.auto_download.videos);
    }
}

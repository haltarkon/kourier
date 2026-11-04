//! App preferences in `kourier.toml` (architecture §8.9): tray, autostart, start hidden,
//! compact list, round avatars, notification content, what Send does with a file that holds
//! where it was taken (`location-files`), the experimental networks that are turned on. Account
//! settings live in `core.db`, not here.
//!
//! A missing file means the defaults. A file that cannot be parsed is not overwritten silently:
//! loading reports the defaults and the error, and the next save replaces it.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use kourier_model::NetworkKind;
use serde::{Deserialize, Deserializer, Serialize};

/// What a message notification shows (§8.7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotificationContent {
    /// The sender (conversation) and the message text.
    #[default]
    NameAndMessage,
    /// The sender only ("New message").
    NameOnly,
    /// Neither ("Kourier: New message").
    Hidden,
}

impl NotificationContent {
    /// The settings page's combo box index (0, 1, 2).
    pub fn index(self) -> i32 {
        match self {
            NotificationContent::NameAndMessage => 0,
            NotificationContent::NameOnly => 1,
            NotificationContent::Hidden => 2,
        }
    }

    /// The mode at a combo box index (out of range = the most private one).
    pub fn from_index(index: i32) -> Self {
        match index {
            0 => NotificationContent::NameAndMessage,
            1 => NotificationContent::NameOnly,
            _ => NotificationContent::Hidden,
        }
    }
}

/// What Send does with a file that holds where it was taken (§8.9, "Files with location data";
/// one pref for every network). The composer asks under [`LocationFiles::Ask`]; the core
/// refuses a located file that was not agreed to leave with its location whatever the choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocationFiles {
    /// Ask before such a file is sent unchanged.
    #[default]
    Ask,
    /// Send such files with their location, without a question.
    Always,
    /// Keep such files in the list; everything else is sent.
    Never,
}

impl LocationFiles {
    /// The settings page's combo box index (0, 1, 2).
    pub fn index(self) -> i32 {
        match self {
            LocationFiles::Ask => 0,
            LocationFiles::Always => 1,
            LocationFiles::Never => 2,
        }
    }

    /// The choice at a combo box index (out of range = Ask, the one that sends nothing
    /// unasked).
    pub fn from_index(index: i32) -> Self {
        match index {
            1 => LocationFiles::Always,
            2 => LocationFiles::Never,
            _ => LocationFiles::Ask,
        }
    }
}

/// Written by hand so that a value this version does not know (a typo in a hand-edited file, a
/// choice from a newer version) reads as Ask instead of failing the whole file, which would
/// reset every other preference to its default.
impl<'de> Deserialize<'de> for LocationFiles {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = LocationFiles;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("\"ask\", \"always\" or \"never\"")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<LocationFiles, E> {
                Ok(match v {
                    "always" => LocationFiles::Always,
                    "never" => LocationFiles::Never,
                    _ => LocationFiles::Ask,
                })
            }
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<LocationFiles, E> {
                Ok(LocationFiles::Ask)
            }
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<LocationFiles, E> {
                Ok(LocationFiles::Ask)
            }
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<LocationFiles, E> {
                Ok(LocationFiles::Ask)
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<LocationFiles, E> {
                Ok(LocationFiles::Ask)
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// The preferences.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Prefs {
    /// Show a tray icon; closing the window keeps Kourier running there. Off by default (KDE
    /// HIG: a tray icon is opt-in, and closing the window quits): a profile that never chose
    /// has none, one that saved a choice keeps it.
    pub tray: bool,
    /// Start with the session (XDG autostart entry).
    pub autostart: bool,
    /// Autostart hidden in the tray (`--background`).
    pub start_hidden: bool,
    /// Compact conversation list.
    pub compact: bool,
    /// Avatars are circles. Off, they are squares with the corners of the front-end's theme
    /// (a front-end that has no such corners draws plain squares). On by default: a profile
    /// from before the setting keeps its round avatars.
    pub round_avatars: bool,
    /// Notification content.
    pub notification_content: NotificationContent,
    /// What Send does with a file that holds where it was taken (absent or unknown = Ask).
    pub location_files: LocationFiles,
    /// The experimental networks the user turned on (`networks-on`, the names of
    /// [`NetworkKind::as_str`]; absent = none). It is read when Kourier starts: the app
    /// registers an experimental network only while it is on, so a change takes effect at the
    /// next start ("Restart Now"), and the core never has a runtime switch for a network.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub networks_on: Vec<String>,
    /// A one-shot request to open the link page of this network once the window is up
    /// (`pending-link`): written by "Turn On and Restart", read and CLEARED at the start that
    /// follows (so it counts once, and only when that network is on then).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_link: Option<String>,
    /// "Show chats of several accounts together" (`blend-accounts`): on, Shift+click or
    /// Ctrl+click adds an account to the list; off, a click switches the account. On by
    /// default (a profile from before the setting blends too).
    pub blend_accounts: bool,
    /// The accounts whose chats the list leaves out (`hidden-accounts`, account uuids as
    /// strings; absent = every account shown). Written by the app's list, not by the
    /// Settings page; read leniently: an entry that is not a uuid string is skipped.
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "lenient_uuids"
    )]
    pub hidden_accounts: Vec<String>,
    /// The folder the last successful "Save As…" wrote into (`last-save-folder`, an absolute
    /// path; absent = none yet): the next save dialog starts there. Written by the app after a
    /// save that succeeded, never by the Settings page (it has no entry for it), never logged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_save_folder: Option<String>,
}

/// `hidden-accounts` as a list of the uuid strings it holds: an entry that is not a string or
/// not a uuid (a hand edit, a newer version) is skipped, and a value that is not a list at all
/// reads as empty, so the whole file never fails over it (that would reset every other
/// preference to its default).
fn lenient_uuids<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    /// One entry: `Some` for a uuid string, `None` for anything else.
    struct Entry(Option<String>);
    impl<'de> Deserialize<'de> for Entry {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            let value = toml::Value::deserialize(deserializer)?;
            Ok(Entry(match value {
                toml::Value::String(s) => uuid::Uuid::parse_str(&s).ok().map(|u| u.to_string()),
                _ => None,
            }))
        }
    }
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a list of account uuids")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Vec<String>, A::Error> {
            let mut out = Vec::new();
            while let Some(Entry(entry)) = seq.next_element::<Entry>()? {
                if let Some(uuid) = entry
                    && !out.contains(&uuid)
                {
                    out.push(uuid);
                }
            }
            Ok(out)
        }
        fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Vec<String>, E> {
            Ok(Vec::new())
        }
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Vec<String>, E> {
            Ok(Vec::new())
        }
        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Vec<String>, E> {
            Ok(Vec::new())
        }
        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Vec<String>, E> {
            Ok(Vec::new())
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Vec<String>, E> {
            Ok(Vec::new())
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Vec<String>, A::Error> {
            while map
                .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                .is_some()
            {}
            Ok(Vec::new())
        }
    }
    deserializer.deserialize_any(Visitor)
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            tray: false,
            autostart: false,
            start_hidden: false,
            compact: false,
            round_avatars: true,
            notification_content: NotificationContent::NameAndMessage,
            location_files: LocationFiles::Ask,
            networks_on: Vec::new(),
            pending_link: None,
            blend_accounts: true,
            hidden_accounts: Vec::new(),
            last_save_folder: None,
        }
    }
}

/// Why the preferences could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum PrefsError {
    /// The file could not be read or written.
    #[error("kourier.toml: {0}")]
    Io(#[from] std::io::Error),
    /// The file is not valid TOML for the preferences.
    #[error("kourier.toml is invalid: {0}")]
    Parse(String),
    /// The preferences could not be serialized.
    #[error("cannot write kourier.toml: {0}")]
    Serialize(String),
}

impl Prefs {
    /// Whether the user turned the experimental network `kind` on (names compare without
    /// regard to case: the file is meant to be edited by hand too).
    pub fn network_on(&self, kind: NetworkKind) -> bool {
        self.networks_on
            .iter()
            .any(|n| n.eq_ignore_ascii_case(kind.as_str()))
    }

    /// Turns the experimental network `kind` on or off (one entry per kind, in the order they
    /// were turned on).
    pub fn set_network_on(&mut self, kind: NetworkKind, on: bool) {
        self.networks_on
            .retain(|n| !n.eq_ignore_ascii_case(kind.as_str()));
        if on {
            self.networks_on.push(kind.as_str().to_owned());
        }
    }

    /// The network of the one-shot link request, when the file names a known one.
    pub fn pending_link_kind(&self) -> Option<NetworkKind> {
        let name = self.pending_link.as_deref()?;
        NetworkKind::ALL
            .into_iter()
            .find(|k| k.as_str().eq_ignore_ascii_case(name))
    }

    /// Reads `path`; a missing file gives the defaults.
    pub fn load(path: &Path) -> Result<Self, PrefsError> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|e| PrefsError::Parse(e.message().to_owned()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Reads `path`, falling back to the defaults (with a warning) when it is unreadable.
    pub fn load_or_default(path: &Path) -> Self {
        Self::load(path).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "using the default preferences");
            Self::default()
        })
    }

    /// Writes `path` atomically with mode 0600.
    pub fn save(&self, path: &Path) -> Result<(), PrefsError> {
        let text = toml::to_string(self).map_err(|e| PrefsError::Serialize(e.to_string()))?;
        let tmp = path.with_extension("toml.tmp");
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(b"# Kourier preferences (written by the app).\n")?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic account uuids.
    const UUID_A: &str = "00000000-0000-4000-8000-00000000000a";
    const UUID_B: &str = "00000000-0000-4000-8000-00000000000b";

    /// Blended chats: a profile from before the setting blends and hides nothing, and the
    /// file stays free of `hidden-accounts` while nothing is hidden.
    #[test]
    fn an_old_file_blends_and_shows_every_account() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kourier.toml");
        let none = Prefs::load(&path).expect("missing");
        assert!(none.blend_accounts && none.hidden_accounts.is_empty());
        std::fs::write(&path, "tray = true\ncompact = true\n").expect("write");
        let old = Prefs::load(&path).expect("old");
        assert!(old.blend_accounts && old.hidden_accounts.is_empty() && old.tray);
        Prefs::default().save(&path).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("blend-accounts = true"), "{text}");
        assert!(!text.contains("hidden-accounts"), "{text}");
    }

    /// A bad `hidden-accounts` entry (not a uuid, not a string) is skipped and the rest of
    /// the list and of the file is kept; a value that is no list reads as none hidden (L3).
    #[test]
    fn a_bad_hidden_account_entry_is_skipped_and_the_rest_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kourier.toml");
        let upper = UUID_B.to_uppercase();
        for list in [
            format!(
                "[\"{UUID_A}\", \"not-a-uuid\", 7, true, {{ a = 1 }}, [\"{UUID_A}\"], \"{upper}\"]"
            ),
            format!("[3, \"{UUID_A}\", \"{UUID_B}\", \"{UUID_A}\"]"),
        ] {
            std::fs::write(
                &path,
                format!("tray = true\nblend-accounts = false\nhidden-accounts = {list}\n"),
            )
            .expect("write");
            let p = Prefs::load(&path).expect("a bad entry is not an error");
            assert_eq!(p.hidden_accounts, [UUID_A, UUID_B], "{list}");
            assert!(p.tray && !p.blend_accounts, "{list}");
        }
        for value in ["\"bogus\"", "3", "true", "{ a = 1 }"] {
            std::fs::write(&path, format!("tray = true\nhidden-accounts = {value}\n"))
                .expect("write");
            let p = Prefs::load(&path).expect("not a list is not an error");
            assert!(p.hidden_accounts.is_empty() && p.tray, "{value}");
        }
    }

    #[test]
    fn round_trip_and_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kourier.toml");
        assert_eq!(Prefs::load(&path).expect("missing"), Prefs::default());
        let prefs = Prefs {
            tray: false,
            autostart: true,
            start_hidden: true,
            compact: true,
            round_avatars: false,
            notification_content: NotificationContent::NameOnly,
            location_files: LocationFiles::Never,
            networks_on: vec!["WhatsApp".to_owned()],
            pending_link: Some("WhatsApp".to_owned()),
            blend_accounts: false,
            hidden_accounts: vec![UUID_A.to_owned(), UUID_B.to_owned()],
            last_save_folder: Some("/home/ari/Documents".to_owned()),
        };
        prefs.save(&path).expect("save");
        assert_eq!(Prefs::load(&path).expect("load"), prefs);
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.contains("notification-content = \"name-only\""),
            "{text}"
        );
        assert!(text.contains("round-avatars = false"), "{text}");
        assert!(text.contains("location-files = \"never\""), "{text}");
        assert!(text.contains("networks-on = [\"WhatsApp\"]"), "{text}");
        assert!(text.contains("pending-link = \"WhatsApp\""), "{text}");
        assert!(text.contains("blend-accounts = false"), "{text}");
        assert!(
            text.contains("last-save-folder = \"/home/ari/Documents\""),
            "{text}"
        );
        assert!(
            text.contains(&format!("hidden-accounts = [\"{UUID_A}\", \"{UUID_B}\"]")),
            "{text}"
        );
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_new_profile_has_no_tray_and_a_saved_choice_is_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kourier.toml");
        // No file, or a file that never mentioned the tray: off.
        assert!(!Prefs::load(&path).expect("missing").tray);
        std::fs::write(&path, "compact = true\n").expect("write");
        let p = Prefs::load(&path).expect("partial");
        assert!(!p.tray && p.compact);
        // A profile that switched it on (this version, or an earlier one that saved the
        // old default) keeps it, and so does one that switched it off.
        std::fs::write(&path, "tray = true\n").expect("write");
        assert!(Prefs::load(&path).expect("on").tray);
        let on = Prefs {
            tray: true,
            ..Prefs::default()
        };
        on.save(&path).expect("save");
        assert!(Prefs::load(&path).expect("saved on").tray);
        Prefs::default().save(&path).expect("save off");
        assert!(!Prefs::load(&path).expect("saved off").tray);
    }

    #[test]
    fn avatars_are_round_unless_the_file_says_otherwise() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kourier.toml");
        assert!(Prefs::default().round_avatars);
        assert!(Prefs::load(&path).expect("missing").round_avatars);
        // A file from before the setting keeps round avatars, whatever else it holds.
        std::fs::write(&path, "tray = true\ncompact = true\n").expect("write");
        assert!(Prefs::load(&path).expect("old").round_avatars);
        std::fs::write(&path, "round-avatars = false\n").expect("write");
        let square = Prefs::load(&path).expect("square");
        assert!(!square.round_avatars);
        // Saving other preferences keeps the choice.
        let changed = Prefs {
            tray: true,
            ..square
        };
        changed.save(&path).expect("save");
        assert!(!Prefs::load(&path).expect("saved").round_avatars);
        Prefs::default().save(&path).expect("save default");
        assert!(Prefs::load(&path).expect("default").round_avatars);
    }

    /// The experimental networks are all off for a profile that never chose (no key in the
    /// file, or a file from before the setting), and the file stays free of both keys until a
    /// network is turned on.
    #[test]
    fn no_experimental_network_is_on_unless_the_file_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kourier.toml");
        let none = Prefs::load(&path).expect("missing");
        assert!(none.networks_on.is_empty() && none.pending_link.is_none());
        assert!(!none.network_on(NetworkKind::WhatsApp));
        std::fs::write(&path, "tray = true\ncompact = true\n").expect("write");
        let old = Prefs::load(&path).expect("old");
        assert!(!old.network_on(NetworkKind::WhatsApp) && old.pending_link_kind().is_none());
        Prefs::default().save(&path).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            !text.contains("networks-on") && !text.contains("pending-link"),
            "{text}"
        );
    }

    /// Turning a network on and off keeps one entry per kind and leaves the rest of the file
    /// alone; the names are read without regard to case and an unknown name turns nothing on.
    #[test]
    fn a_network_is_turned_on_and_off_by_name() {
        let mut p = Prefs::default();
        p.set_network_on(NetworkKind::WhatsApp, true);
        p.set_network_on(NetworkKind::WhatsApp, true);
        assert_eq!(p.networks_on, ["WhatsApp"]);
        assert!(p.network_on(NetworkKind::WhatsApp));
        assert!(!p.network_on(NetworkKind::Threema));
        p.set_network_on(NetworkKind::WhatsApp, false);
        assert!(p.networks_on.is_empty() && !p.network_on(NetworkKind::WhatsApp));
        let by_hand = Prefs {
            networks_on: vec!["whatsapp".to_owned(), "Nonesuch".to_owned()],
            pending_link: Some("WHATSAPP".to_owned()),
            ..Prefs::default()
        };
        assert!(by_hand.network_on(NetworkKind::WhatsApp));
        assert!(!by_hand.network_on(NetworkKind::Fake));
        assert_eq!(by_hand.pending_link_kind(), Some(NetworkKind::WhatsApp));
        let unknown = Prefs {
            pending_link: Some("Nonesuch".to_owned()),
            ..Prefs::default()
        };
        assert_eq!(unknown.pending_link_kind(), None);
    }

    #[test]
    fn partial_and_invalid_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kourier.toml");
        std::fs::write(&path, "tray = false\nunknown-key = 3\n").expect("write");
        let p = Prefs::load(&path).expect("partial");
        assert!(!p.tray);
        assert_eq!(p.notification_content, NotificationContent::NameAndMessage);
        std::fs::write(&path, "tray = \"yes\"").expect("write");
        assert!(matches!(Prefs::load(&path), Err(PrefsError::Parse(_))));
        assert_eq!(Prefs::load_or_default(&path), Prefs::default());
    }

    #[test]
    fn content_index() {
        for c in [
            NotificationContent::NameAndMessage,
            NotificationContent::NameOnly,
            NotificationContent::Hidden,
        ] {
            assert_eq!(NotificationContent::from_index(c.index()), c);
        }
        assert_eq!(
            NotificationContent::from_index(9),
            NotificationContent::Hidden
        );
    }

    /// "Files with location data": Ask for a profile that never chose, each choice round
    /// trips through the file, and a combo index out of range is Ask.
    #[test]
    fn location_files_ask_unless_the_file_says_otherwise() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kourier.toml");
        assert_eq!(Prefs::default().location_files, LocationFiles::Ask);
        assert_eq!(
            Prefs::load(&path).expect("missing").location_files,
            LocationFiles::Ask
        );
        std::fs::write(&path, "tray = true\n").expect("write");
        assert_eq!(
            Prefs::load(&path).expect("old").location_files,
            LocationFiles::Ask
        );
        for choice in [
            LocationFiles::Ask,
            LocationFiles::Always,
            LocationFiles::Never,
        ] {
            let p = Prefs {
                location_files: choice,
                ..Prefs::default()
            };
            p.save(&path).expect("save");
            assert_eq!(Prefs::load(&path).expect("load"), p);
            assert_eq!(LocationFiles::from_index(choice.index()), choice);
        }
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("location-files = \"never\""), "{text}");
        assert_eq!(LocationFiles::from_index(7), LocationFiles::Ask);
        assert_eq!(LocationFiles::from_index(-1), LocationFiles::Ask);
    }

    /// A `location-files` value this version does not know reads as Ask and keeps every
    /// other preference (F8: a failed parse would reset the tray and the networks).
    #[test]
    fn an_unknown_location_files_value_keeps_the_other_prefs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kourier.toml");
        for value in ["\"bogus\"", "\"Always\"", "3", "true"] {
            std::fs::write(
                &path,
                format!("tray = true\nnetworks-on = [\"WhatsApp\"]\nlocation-files = {value}\n"),
            )
            .expect("write");
            let p = Prefs::load(&path).expect("an unknown value is not an error");
            assert_eq!(p.location_files, LocationFiles::Ask, "{value}");
            assert!(p.tray, "{value}");
            assert!(p.network_on(NetworkKind::WhatsApp), "{value}");
        }
        std::fs::write(&path, "location-files = \"always\"\n").expect("write");
        assert_eq!(
            Prefs::load(&path).expect("always").location_files,
            LocationFiles::Always
        );
    }
}

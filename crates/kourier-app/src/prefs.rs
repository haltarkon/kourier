//! App preferences in `kourier.toml` (architecture §8.9): tray, autostart, start hidden,
//! compact list, round avatars, notification content, what Send does with a file that holds
//! where it was taken (`location-files`), the experimental networks that are turned on. Account
//! settings live in `core.db`, not here.
//!
//! A missing file means the defaults. A file that cannot be parsed is not overwritten silently:
//! loading reports the defaults and the error, and the next save replaces it.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use kourier_model::NetworkKind;
use serde::{Deserialize, Deserializer, Serialize};

/// Set (to anything but empty) by `cargo xtask threema-run` for the app it starts. That start keeps
/// the run tool's own rules: the switch is `networks-on` alone, and a link takes only the IDs on
/// the run profile's list (`threema.rs`). Every other start has Threema on and takes any ID.
pub const THREEMA_RUN_TOOL_ENV: &str = "KOURIER_THREEMA_RUN";

/// Whether the run tool started this process ([`THREEMA_RUN_TOOL_ENV`]).
pub(crate) fn threema_run_tool(env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    env(THREEMA_RUN_TOOL_ENV).is_some_and(|value| !value.is_empty())
}

/// The name of the run tool's own directory inside a profile (`xtask` `threema_run` `TOOL_DIR`; the
/// names test compares them).
pub(crate) const THREEMA_RUN_TOOL_DIR: &str = "threema-run";

/// Who started this process, for Threema (`threema::threema_registration`, `start_prefs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ThreemaStart {
    /// A person's start: Threema on by default, any ID.
    Person,
    /// The run tool's start ([`THREEMA_RUN_TOOL_ENV`]): the old gates.
    RunTool,
    /// A scripted start (`--dev-control`) or a profile that holds the run tool's directory, outside
    /// the run tool: Threema is absent, whatever `networks-on` says.
    Shut,
}

/// The decision: the run tool wins; else a scripted start, or a profile that holds the run tool's
/// directory (`marker`), shuts Threema out; else it is a person's.
pub(crate) fn threema_start(run_tool: bool, scripted: bool, marker: bool) -> ThreemaStart {
    if run_tool {
        ThreemaStart::RunTool
    } else if scripted || marker {
        ThreemaStart::Shut
    } else {
        ThreemaStart::Person
    }
}

/// Whether `dir` holds anything at the run tool's directory name: a real directory, a symbolic
/// link or a file, and also a name that cannot be looked at (fail closed: only "not found" is no).
pub(crate) fn holds_run_tool_dir(dir: &std::path::Path) -> bool {
    match std::fs::symlink_metadata(dir.join(THREEMA_RUN_TOOL_DIR)) {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

/// [`threema_start`] for a start over `paths`: the profile is `KOURIER_HOME` (where the tool keeps
/// its directory) and the data directory the start resolved.
pub(crate) fn threema_start_of(
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    scripted: bool,
    paths: &crate::paths::Paths,
) -> ThreemaStart {
    let run_tool = threema_run_tool(env);
    let marker = !run_tool
        && (paths.home_dir().is_some_and(holds_run_tool_dir) || holds_run_tool_dir(&paths.data));
    let start = threema_start(run_tool, scripted, marker);
    if start == ThreemaStart::Shut {
        tracing::warn!(
            "Threema is not offered in this start: a scripted start or a run tool's profile outside cargo xtask threema-run"
        );
    }
    start
}

/// The experimental networks that are on until the user turns them off (Threema: the user's
/// decision of 2026-11-09, "enable Threema by default globally").
const ON_BY_DEFAULT: [NetworkKind; 1] = [NetworkKind::Threema];

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
    /// The networks that are on by default and the user turned off (`networks-off`, absent =
    /// none): [`Prefs::with_default_networks`] leaves these off.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub networks_off: Vec<String>,
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
    /// Chat rows carry a faint wash of their account's color (`tint-rows`). On by default.
    pub tint_rows: bool,
    /// The chat page's pattern of small drawings behind the messages (`chat-wallpaper`). On by
    /// default.
    pub chat_wallpaper: bool,
    /// The accounts given a color of their own (`account-tints`: account uuid to `#rrggbb`;
    /// absent = each account has its network's color). Written by the app's account menu, not
    /// by the Settings page; read leniently: an entry that is not a uuid and a color is skipped.
    #[serde(
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "lenient_tints"
    )]
    pub account_tints: BTreeMap<String, String>,
}

/// `#rrggbb`, lower or upper case: the only form `account-tints` holds.
pub fn is_tint(value: &str) -> bool {
    value.len() == 7 && value.starts_with('#') && value[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// `account-tints` as the entries that are a uuid and a color: anything else (a hand edit, a
/// newer version) is skipped, so the whole file never fails over it.
fn lenient_tints<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    let value = toml::Value::deserialize(deserializer)?;
    let toml::Value::Table(table) = value else {
        return Ok(BTreeMap::new());
    };
    Ok(table
        .into_iter()
        .filter_map(|(k, v)| {
            let uuid = uuid::Uuid::parse_str(&k).ok()?.to_string();
            let tint = v.as_str().filter(|t| is_tint(t))?.to_owned();
            Some((uuid, tint))
        })
        .collect())
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
            networks_off: Vec::new(),
            pending_link: None,
            blend_accounts: true,
            hidden_accounts: Vec::new(),
            last_save_folder: None,
            tint_rows: true,
            chat_wallpaper: true,
            account_tints: BTreeMap::new(),
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
        Self::names(&self.networks_on, kind)
    }

    /// Whether `list` names `kind`.
    fn names(list: &[String], kind: NetworkKind) -> bool {
        list.iter().any(|n| n.eq_ignore_ascii_case(kind.as_str()))
    }

    /// These preferences with the networks that are on by default turned on, unless the user
    /// turned them off (`networks-off`). A normal start reads its preferences through this; the
    /// file itself changes only when something else saves it.
    #[must_use]
    pub fn with_default_networks(mut self) -> Self {
        for kind in ON_BY_DEFAULT {
            if !Self::names(&self.networks_off, kind) && !Self::names(&self.networks_on, kind) {
                self.networks_on.push(kind.as_str().to_owned());
            }
        }
        self
    }

    /// Turns the experimental network `kind` on or off (one entry per kind, in the order they
    /// were turned on). Turning off a network that is on by default is remembered in
    /// `networks-off`, or the next start would turn it on again.
    pub fn set_network_on(&mut self, kind: NetworkKind, on: bool) {
        let name = kind.as_str();
        self.networks_on.retain(|n| !n.eq_ignore_ascii_case(name));
        self.networks_off.retain(|n| !n.eq_ignore_ascii_case(name));
        if on {
            self.networks_on.push(name.to_owned());
        } else if ON_BY_DEFAULT.contains(&kind) {
            self.networks_off.push(name.to_owned());
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

    /// WHO STARTED THIS PROCESS, for Threema: written out (run tool, scripted, marker) -> start. The run
    /// tool wins; a scripted start or a marker shuts Threema out; only the rest is a person's.
    /// Mutants: the run-tool arm moved below the shut arm; `scripted || marker` -> `scripted` or `marker`.
    #[test]
    fn the_run_tool_wins_and_a_scripted_start_or_a_marker_shuts_threema_out() {
        use ThreemaStart::{Person, RunTool, Shut};
        let cases = [
            (false, false, false, Person),
            (false, false, true, Shut),
            (false, true, false, Shut),
            (false, true, true, Shut),
            (true, false, false, RunTool),
            (true, false, true, RunTool),
            (true, true, false, RunTool),
            (true, true, true, RunTool),
        ];
        for (run_tool, scripted, marker, expected) in cases {
            assert_eq!(
                threema_start(run_tool, scripted, marker),
                expected,
                "run_tool={run_tool}, scripted={scripted}, marker={marker}"
            );
        }
    }

    /// Anything at the run tool's directory name counts: a directory, a symbolic link (dangling too),
    /// a file, a name that cannot be looked at; nothing there does not. Mutant: `metadata` instead
    /// of `symlink_metadata` -> the dangling link is not seen.
    #[test]
    fn anything_at_the_run_tools_directory_name_shuts_a_profile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let at = dir.path().join(THREEMA_RUN_TOOL_DIR);
        assert!(!holds_run_tool_dir(dir.path()));
        std::fs::create_dir(&at).expect("dir");
        assert!(holds_run_tool_dir(dir.path()));
        std::fs::remove_dir(&at).expect("rm");
        std::os::unix::fs::symlink("/nonexistent-target", &at).expect("link");
        assert!(holds_run_tool_dir(dir.path()));
        std::fs::remove_file(&at).expect("rm");
        std::fs::write(&at, b"x").expect("file");
        assert!(holds_run_tool_dir(dir.path()));
        // A name that cannot be looked at counts too (fail closed): below a file the look fails
        // with "not a directory", not "not found". Mutant: every error -> false.
        assert!(holds_run_tool_dir(&at));
        assert_eq!(THREEMA_RUN_TOOL_DIR, "threema-run");
    }

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
            networks_off: vec!["Threema".to_owned()],
            pending_link: Some("WhatsApp".to_owned()),
            blend_accounts: false,
            hidden_accounts: vec![UUID_A.to_owned(), UUID_B.to_owned()],
            last_save_folder: Some("/home/ari/Documents".to_owned()),
            tint_rows: false,
            chat_wallpaper: false,
            account_tints: BTreeMap::from([(UUID_A.to_owned(), "#3a76f0".to_owned())]),
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
        assert!(p.networks_off.is_empty(), "WhatsApp is off by default");
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

    /// `account-tints` keeps what is a uuid and a color, a value of any other shape reads as empty, and neither costs
    /// the file's other preferences.
    #[test]
    fn account_tints_are_read_leniently() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kourier.toml");
        let good = "12345678-1234-1234-1234-123456789abc";
        std::fs::write(
            &path,
            format!(
                "tray = true\n[account-tints]\n\"{good}\" = \"#3a76f0\"\n\"nope\" = \"#000000\"\n\
                 \"22345678-1234-1234-1234-123456789abc\" = \"blue\"\n"
            ),
        )
        .expect("write");
        let prefs = Prefs::load(&path).expect("a file with a bad entry still loads");
        assert!(prefs.tray);
        assert_eq!(
            prefs.account_tints,
            BTreeMap::from([(good.to_owned(), "#3a76f0".to_owned())])
        );
        std::fs::write(&path, "tray = true\naccount-tints = 7\n").expect("write");
        let prefs = Prefs::load(&path).expect("another shape still loads");
        assert!(prefs.tray);
        assert!(prefs.account_tints.is_empty());
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

    /// Threema is on until the user turns it off, the choice is kept, and the other networks stay
    /// off by default (the user's decision of 2026-11-09). Mutants: the `networks-off` check
    /// removed -> the turned-off case comes back on; off not remembered -> the same.
    #[test]
    fn threema_is_on_by_default_until_the_user_turns_it_off() {
        let fresh = Prefs::default().with_default_networks();
        assert!(fresh.network_on(NetworkKind::Threema));
        assert!(!fresh.network_on(NetworkKind::WhatsApp));
        assert!(!fresh.network_on(NetworkKind::Telegram));
        assert!(
            !Prefs::default().network_on(NetworkKind::Threema),
            "the file's own list says nothing of the default"
        );

        let mut off = fresh.clone();
        off.set_network_on(NetworkKind::Threema, false);
        assert_eq!(off.networks_off, ["Threema"]);
        assert!(!off.network_on(NetworkKind::Threema));
        let text = toml::to_string(&off).expect("serialize");
        assert!(text.contains("networks-off = [\"Threema\"]"), "{text}");
        let read: Prefs = toml::from_str(&text).expect("parse");
        let again = read.with_default_networks();
        assert!(!again.network_on(NetworkKind::Threema), "it came back on");

        let mut on = again;
        on.set_network_on(NetworkKind::Threema, true);
        assert!(on.networks_off.is_empty() && on.network_on(NetworkKind::Threema));
        // Twice over: one entry.
        let twice = on.clone().with_default_networks();
        assert_eq!(twice.networks_on, ["Threema"]);
        // By hand, in any case.
        let by_hand = Prefs {
            networks_off: vec!["threema".to_owned()],
            ..Prefs::default()
        };
        assert!(
            !by_hand
                .with_default_networks()
                .network_on(NetworkKind::Threema)
        );
    }
}

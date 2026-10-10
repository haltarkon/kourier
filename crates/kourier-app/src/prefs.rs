//! App preferences in `kourier.toml` (architecture §8.9): tray, autostart, start hidden,
//! compact list, round avatars, notification content. Account settings live in `core.db`,
//! not here.
//!
//! A missing file means the defaults. A file that cannot be parsed is not overwritten silently:
//! loading reports the defaults and the error, and the next save replaces it.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

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
        };
        prefs.save(&path).expect("save");
        assert_eq!(Prefs::load(&path).expect("load"), prefs);
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.contains("notification-content = \"name-only\""),
            "{text}"
        );
        assert!(text.contains("round-avatars = false"), "{text}");
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
}

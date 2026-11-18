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

/// How a forward to another account or network looks (`forward-look`; the Settings default of
/// the forward dialog and the composer's forward card). The viewmodel's look is mapped from it in
/// the front end; a forward on the same account ignores it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ForwardLookPref {
    /// The content alone ("Plain copy").
    Plain,
    /// A "Forwarded from" line above each run of messages ("Forwarded header").
    #[default]
    Header,
    /// The header, and the body quoted ("Header + quoted body").
    Quoted,
    /// The messages as one picture ("As picture").
    Picture,
}

impl ForwardLookPref {
    /// The settings page's combo box index (0 to 3, in the order of the variants).
    pub fn index(self) -> i32 {
        match self {
            ForwardLookPref::Plain => 0,
            ForwardLookPref::Header => 1,
            ForwardLookPref::Quoted => 2,
            ForwardLookPref::Picture => 3,
        }
    }

    /// The choice at a combo box index (out of range = Header, the default).
    pub fn from_index(index: i32) -> Self {
        match index {
            0 => ForwardLookPref::Plain,
            2 => ForwardLookPref::Quoted,
            3 => ForwardLookPref::Picture,
            _ => ForwardLookPref::Header,
        }
    }
}

/// Written by hand like [`LocationFiles`]: a value this version does not know reads as Header
/// and keeps every other preference.
impl<'de> Deserialize<'de> for ForwardLookPref {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = ForwardLookPref;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("\"plain\", \"header\", \"quoted\" or \"picture\"")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<ForwardLookPref, E> {
                Ok(match v {
                    "plain" => ForwardLookPref::Plain,
                    "quoted" => ForwardLookPref::Quoted,
                    "picture" => ForwardLookPref::Picture,
                    _ => ForwardLookPref::Header,
                })
            }
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<ForwardLookPref, E> {
                Ok(ForwardLookPref::Header)
            }
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<ForwardLookPref, E> {
                Ok(ForwardLookPref::Header)
            }
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<ForwardLookPref, E> {
                Ok(ForwardLookPref::Header)
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<ForwardLookPref, E> {
                Ok(ForwardLookPref::Header)
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// What an old `round-avatars = false` reads as on the slider `avatar-corners`: the square avatar of
/// old had the theme's own corner radius (Kirigami's `cornerRadius`, 5 px), about a fifth of half a
/// list avatar's side (`Style.listAvatarSize`, 2.6 grid units), so the look stays as it was.
pub const OLD_SQUARE_AVATAR_CORNERS: u8 = 20;

/// The preferences.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    /// How round avatars are (`avatar-corners`, 0 to 100: the corner radius as a percentage of
    /// half the side, 0 a square, 100 a circle). 100 by default. It replaced the switch
    /// `round-avatars`: a file that has only the old key reads `true` as 100 and `false` as
    /// [`OLD_SQUARE_AVATAR_CORNERS`] ([`Self::load`]); the old key is never written again.
    pub avatar_corners: u8,
    /// The old switch, read only to carry its value over (`round-avatars`); always `None` after
    /// [`Self::load`].
    #[serde(rename = "round-avatars", skip_serializing)]
    pub legacy_round_avatars: Option<bool>,
    /// How round the message bubbles are (`message-corners`, 0 to 100): the corner radius is
    /// this tenth of the theme's corner radius, so 30 (the default) is the bubble of old (three
    /// times the theme's radius) and 0 a square one.
    pub message_corners: u8,
    /// A chat left with a draft moves up in the list (`drafts-on-top`), as in Signal Desktop. On
    /// by default.
    pub drafts_on_top: bool,
    /// The chat list shows the accounts' names and tags (`show-account-tags`). On by default.
    pub show_account_tags: bool,
    /// The short tags the person gave accounts (`account-tags`: account uuid to a tag of up to
    /// [`TAG_MAX`] characters; absent = the account's name shows). Local only, never sent to a
    /// network. Written by the app's account menu; read leniently like `account-tints`.
    #[serde(
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "lenient_tags"
    )]
    pub account_tags: BTreeMap<String, String>,
    /// Notification content.
    pub notification_content: NotificationContent,
    /// What Send does with a file that holds where it was taken (absent or unknown = Ask).
    pub location_files: LocationFiles,
    /// How a forward to another account or network looks by default (absent or unknown =
    /// Header).
    pub forward_look: ForwardLookPref,
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
    /// The pinned emoji (`emoji-pins`): the reaction picker's quick row, and the first three are
    /// the one-click reactions in a message's hover bar. Absent = Signal's six (`None`; an empty
    /// list is a person's choice to pin none and is kept). One list for every account, local
    /// only. NOT listed by the Settings page: the app's emoji lists are its one writer, through
    /// `DesktopHandle::update_prefs`. Read leniently (`lenient_emoji`).
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_emoji_pins"
    )]
    pub emoji_pins: Option<Vec<String>>,
    /// The starred emoji (`emoji-favourites`), the picker's first tab. Same owner and rules.
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "lenient_emoji_favourites"
    )]
    pub emoji_favourites: Vec<String>,
    /// The emoji used last, newest first, at most [`EMOJI_RECENT_MAX`] (`emoji-recent`).
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "lenient_emoji_recent"
    )]
    pub emoji_recent: Vec<String>,
    /// The colour of 3D models without colours of their own (`model-colour`: `#rrggbb`; absent =
    /// the desktop's accent colour). ONE owner, the Settings page's controller (LISTED in
    /// `SettingsRust::prefs()`); the viewer's toolbar button writes through the same property.
    /// Read leniently: anything but `#rrggbb` reads as the accent.
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "lenient_model_colour"
    )]
    pub model_colour: String,
    /// The printer box shown in the 3D viewer (`model-box`: a row id of
    /// `kourier_viewmodel::printer_boxes`, "256x256x256"; absent = none). Same owner as
    /// `model-colour`. Read leniently: anything but a short `NxNxN` reads as none (whether the
    /// row exists is the front-end's to judge).
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "lenient_model_box"
    )]
    pub model_box: String,
    /// The slicer "Open in Slicer" starts first (`model-slicer`: an id of
    /// `crate::slicers::SLICERS`; absent = OrcaSlicer). Same owner as `model-colour`: written
    /// from the viewer only after a start that happened. Read leniently: anything but a table id
    /// reads as the default (empty).
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "lenient_model_slicer"
    )]
    pub model_slicer: String,
    /// The conversation the first chat pane showed when Kourier last ran (`open-chat`, a core
    /// conversation id; absent = none, 0 and anything that is no positive integer read as none).
    /// ONE owner, the App (`DesktopHandle::update_prefs`), not listed by the Settings page; the
    /// restore after the first frame of the list reads it (`restore_plan`, kourier-qt).
    #[serde(
        skip_serializing_if = "is_no_chat",
        deserialize_with = "lenient_chat_id"
    )]
    pub open_chat: i64,
    /// The conversation of the second pane while the split was on (`split-chat`; absent = no
    /// split). Same owner and reading as `open-chat`.
    #[serde(
        skip_serializing_if = "is_no_chat",
        deserialize_with = "lenient_chat_id"
    )]
    pub split_chat: i64,
    /// The first pane's share of the room the two chats have (`split-width`, from
    /// [`SPLIT_WIDTH_MIN`] to [`SPLIT_WIDTH_MAX`], default [`SPLIT_WIDTH_DEFAULT`] and absent
    /// while it is the default). Same owner. A stored value outside the range is clamped, one
    /// that is no number is the default.
    #[serde(
        skip_serializing_if = "is_default_split_width",
        deserialize_with = "lenient_split_width"
    )]
    pub split_width: f64,
}

/// The smallest and the largest share the divider of the split may keep, and the default.
pub const SPLIT_WIDTH_MIN: f64 = 0.25;
/// See [`SPLIT_WIDTH_MIN`].
pub const SPLIT_WIDTH_MAX: f64 = 0.75;
/// See [`SPLIT_WIDTH_MIN`].
pub const SPLIT_WIDTH_DEFAULT: f64 = 0.5;

/// `Prefs` holds no NaN (`split_width` is clamped on every way in), so equality is total.
impl Eq for Prefs {}

fn is_no_chat(id: &i64) -> bool {
    *id <= 0
}

fn is_default_split_width(w: &f64) -> bool {
    (*w - SPLIT_WIDTH_DEFAULT).abs() < f64::EPSILON
}

/// `open-chat`, `split-chat`: a positive integer, else none (0).
fn lenient_chat_id<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let value = toml::Value::deserialize(d)?;
    Ok(value.as_integer().filter(|n| *n > 0).unwrap_or(0))
}

/// The share a value of `split-width` stands for: a number is clamped to the range, anything
/// else (and NaN) is the default.
pub fn clamp_split_width(value: f64) -> f64 {
    if value.is_nan() {
        SPLIT_WIDTH_DEFAULT
    } else {
        value.clamp(SPLIT_WIDTH_MIN, SPLIT_WIDTH_MAX)
    }
}

/// `split-width`: a float or an integer, clamped; else the default.
fn lenient_split_width<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    let value = toml::Value::deserialize(d)?;
    Ok(match value {
        toml::Value::Float(f) => clamp_split_width(f),
        toml::Value::Integer(n) => clamp_split_width(n as f64),
        _ => SPLIT_WIDTH_DEFAULT,
    })
}

/// `model-slicer`: a table id of [`crate::slicers`], else the default (empty).
fn lenient_model_slicer<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let value = toml::Value::deserialize(d)?;
    Ok(value
        .as_str()
        .filter(|t| crate::slicers::find(t).is_some())
        .map(str::to_owned)
        .unwrap_or_default())
}

/// `model-colour`: a `#rrggbb` string, else the accent (empty).
fn lenient_model_colour<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let value = toml::Value::deserialize(d)?;
    Ok(value
        .as_str()
        .filter(|t| is_tint(t))
        .map(str::to_ascii_lowercase)
        .unwrap_or_default())
}

/// `model-box`: `NxNxN` of digits, at most 16 characters, else none (empty).
fn lenient_model_box<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let value = toml::Value::deserialize(d)?;
    Ok(value
        .as_str()
        .filter(|t| {
            t.len() <= 16
                && t.split('x').count() == 3
                && t.split('x')
                    .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(str::to_owned)
        .unwrap_or_default())
}

/// Most pinned emoji (the reaction picker's quick row).
pub const EMOJI_PINS_MAX: usize = 6;
/// Most recently used emoji kept.
pub const EMOJI_RECENT_MAX: usize = 24;
/// Most favourites kept.
pub const EMOJI_FAVOURITES_MAX: usize = 500;

/// A list of emoji from the file: an entry that is not a string, is empty, is longer than 40 bytes
/// (the longest emoji sequence is 35) or holds a control character is skipped, a repeat is dropped,
/// the list is cut to `max`, and a value that is not a list reads as `None`. Whether an entry is an
/// emoji of the table is the front-end's to judge (this crate has no table); a stale entry harms
/// nothing and goes at the next write of the list.
fn read_emoji_list<'de, D: Deserializer<'de>>(
    deserializer: D,
    max: usize,
) -> Result<Option<Vec<String>>, D::Error> {
    let value = toml::Value::deserialize(deserializer)?;
    let toml::Value::Array(items) = value else {
        return Ok(None);
    };
    let mut out: Vec<String> = Vec::new();
    for item in items {
        if out.len() == max {
            break;
        }
        if let toml::Value::String(s) = item
            && !s.is_empty()
            && s.len() <= 40
            && !s.chars().any(char::is_control)
            && !out.contains(&s)
        {
            out.push(s);
        }
    }
    Ok(Some(out))
}

fn lenient_emoji_pins<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    read_emoji_list(d, EMOJI_PINS_MAX)
}

fn lenient_emoji_favourites<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(read_emoji_list(d, EMOJI_FAVOURITES_MAX)?.unwrap_or_default())
}

fn lenient_emoji_recent<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(read_emoji_list(d, EMOJI_RECENT_MAX)?.unwrap_or_default())
}

/// The longest account tag, in characters.
pub const TAG_MAX: usize = 8;

/// A tag as the preference keeps it: one line, trimmed, at most [`TAG_MAX`] characters; empty
/// when nothing is left.
pub fn clean_tag(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(TAG_MAX)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// `account-tags` as the entries that are a uuid and a non-empty tag (cut to [`TAG_MAX`]).
fn lenient_tags<'de, D: Deserializer<'de>>(
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
            let tag = clean_tag(v.as_str()?);
            (!tag.is_empty()).then_some((uuid, tag))
        })
        .collect())
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
            avatar_corners: 100,
            legacy_round_avatars: None,
            message_corners: 30,
            drafts_on_top: true,
            show_account_tags: true,
            account_tags: BTreeMap::new(),
            notification_content: NotificationContent::NameAndMessage,
            location_files: LocationFiles::Ask,
            forward_look: ForwardLookPref::Header,
            networks_on: Vec::new(),
            networks_off: Vec::new(),
            pending_link: None,
            blend_accounts: true,
            hidden_accounts: Vec::new(),
            last_save_folder: None,
            tint_rows: true,
            chat_wallpaper: true,
            account_tints: BTreeMap::new(),
            emoji_pins: None,
            emoji_favourites: Vec::new(),
            emoji_recent: Vec::new(),
            model_colour: String::new(),
            model_box: String::new(),
            model_slicer: String::new(),
            open_chat: 0,
            split_chat: 0,
            split_width: SPLIT_WIDTH_DEFAULT,
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
                let mut prefs: Self =
                    toml::from_str(&text).map_err(|e| PrefsError::Parse(e.message().to_owned()))?;
                // The switch `round-avatars` became the slider `avatar-corners`: an old file
                // keeps its look (false the small corner of old, true a circle); a file with the
                // slider keeps the slider.
                let has_slider = text
                    .parse::<toml::Table>()
                    .is_ok_and(|t| t.contains_key("avatar-corners"));
                if let Some(round) = prefs.legacy_round_avatars.take()
                    && !has_slider
                {
                    prefs.avatar_corners = if round {
                        100
                    } else {
                        OLD_SQUARE_AVATAR_CORNERS
                    };
                }
                prefs.avatar_corners = prefs.avatar_corners.min(100);
                prefs.message_corners = prefs.message_corners.min(100);
                Ok(prefs)
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

    /// Split view: a profile from before the keys has no chat to reopen, no split and the
    /// middle share, and the file stays free of the three keys while they hold those.
    #[test]
    fn the_split_keys_default_and_stay_out_of_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kourier.toml");
        let none = Prefs::load(&path).expect("missing");
        assert_eq!((none.open_chat, none.split_chat), (0, 0));
        assert_eq!(none.split_width, SPLIT_WIDTH_DEFAULT);
        std::fs::write(&path, "tray = true\n").expect("write");
        let old = Prefs::load(&path).expect("old");
        assert_eq!(
            (old.open_chat, old.split_chat, old.split_width),
            (0, 0, 0.5)
        );
        Prefs::default().save(&path).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        for key in ["open-chat", "split-chat", "split-width"] {
            assert!(!text.contains(key), "{key}: {text}");
        }
    }

    /// A bad value of the split keys is the default, a width outside the range is clamped, and
    /// the rest of the file is kept (the whole file never fails over one key).
    #[test]
    fn a_bad_split_value_reads_as_the_default_and_a_wide_one_is_clamped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kourier.toml");
        for (text, chat, width) in [
            (
                "open-chat = 7\nsplit-chat = 9\nsplit-width = 0.6\n",
                (7, 9),
                0.6,
            ),
            (
                "open-chat = -3\nsplit-chat = 0\nsplit-width = 0.1\n",
                (0, 0),
                0.25,
            ),
            (
                "open-chat = \"x\"\nsplit-chat = 2.5\nsplit-width = 3\n",
                (0, 0),
                0.75,
            ),
            (
                "open-chat = true\nsplit-chat = [1]\nsplit-width = \"wide\"\n",
                (0, 0),
                0.5,
            ),
            ("split-width = nan\n", (0, 0), 0.5),
            ("split-width = 0.0\n", (0, 0), 0.25),
        ] {
            std::fs::write(&path, format!("tray = true\n{text}")).expect("write");
            let p = Prefs::load(&path).expect("a bad value is not an error");
            assert_eq!(
                ((p.open_chat, p.split_chat), p.split_width),
                (chat, width),
                "{text}"
            );
            assert!(p.tray, "{text}");
        }
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
            avatar_corners: 40,
            legacy_round_avatars: None,
            message_corners: 55,
            drafts_on_top: false,
            show_account_tags: false,
            account_tags: BTreeMap::from([(UUID_B.to_owned(), "Work".to_owned())]),
            notification_content: NotificationContent::NameOnly,
            location_files: LocationFiles::Never,
            forward_look: ForwardLookPref::Quoted,
            networks_on: vec!["WhatsApp".to_owned()],
            networks_off: vec!["Threema".to_owned()],
            pending_link: Some("WhatsApp".to_owned()),
            blend_accounts: false,
            hidden_accounts: vec![UUID_A.to_owned(), UUID_B.to_owned()],
            last_save_folder: Some("/home/ari/Documents".to_owned()),
            tint_rows: false,
            chat_wallpaper: false,
            account_tints: BTreeMap::from([(UUID_A.to_owned(), "#3a76f0".to_owned())]),
            emoji_pins: Some(vec!["🐶".to_owned()]),
            emoji_favourites: vec!["😀".to_owned(), "🐶".to_owned()],
            emoji_recent: vec!["🐶".to_owned()],
            model_colour: "#e0529c".to_owned(),
            model_box: "256x256x256".to_owned(),
            model_slicer: "cura".to_owned(),
            open_chat: 41,
            split_chat: 42,
            split_width: 0.375,
        };
        prefs.save(&path).expect("save");
        assert_eq!(Prefs::load(&path).expect("load"), prefs);
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.contains("notification-content = \"name-only\""),
            "{text}"
        );
        assert!(text.contains("avatar-corners = 40"), "{text}");
        assert!(text.contains("model-colour = \"#e0529c\""), "{text}");
        assert!(text.contains("model-box = \"256x256x256\""), "{text}");
        assert!(text.contains("model-slicer = \"cura\""), "{text}");
        assert!(text.contains("open-chat = 41"), "{text}");
        assert!(text.contains("split-chat = 42"), "{text}");
        assert!(text.contains("split-width = 0.375"), "{text}");
        assert!(!text.contains("round-avatars"), "{text}");
        assert!(text.contains("location-files = \"never\""), "{text}");
        assert!(text.contains("forward-look = \"quoted\""), "{text}");
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

    /// The slider replaced the switch: no key, or only a file from before: a circle; the old
    /// `false` the small corner of old, the old `true` a circle; a file with the slider keeps the slider whatever
    /// the old key says; the old key is never written back.
    /// Mutants: `if round { 100 } else { OLD_SQUARE_AVATAR_CORNERS }` swapped; `!has_slider` dropped.
    #[test]
    fn the_old_round_avatars_switch_carries_over_to_the_corner_slider() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kourier.toml");
        assert_eq!(Prefs::default().avatar_corners, 100);
        assert_eq!(Prefs::load(&path).expect("missing").avatar_corners, 100);
        std::fs::write(&path, "tray = true\ncompact = true\n").expect("write");
        assert_eq!(Prefs::load(&path).expect("old").avatar_corners, 100);
        std::fs::write(&path, "round-avatars = false\n").expect("write");
        let square = Prefs::load(&path).expect("square");
        assert_eq!(square.avatar_corners, 20);
        assert_eq!(square.legacy_round_avatars, None);
        std::fs::write(&path, "round-avatars = true\n").expect("write");
        assert_eq!(Prefs::load(&path).expect("round").avatar_corners, 100);
        std::fs::write(&path, "round-avatars = false\navatar-corners = 35\n").expect("write");
        assert_eq!(Prefs::load(&path).expect("both").avatar_corners, 35);
        // Saving writes the slider alone, and it survives.
        let changed = Prefs {
            tray: true,
            ..square
        };
        changed.save(&path).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.contains("avatar-corners = 20") && !text.contains("round-avatars"),
            "{text}"
        );
        assert_eq!(Prefs::load(&path).expect("saved").avatar_corners, 20);
        // A value past the end is cut.
        std::fs::write(&path, "avatar-corners = 250\nmessage-corners = 200\n").expect("write");
        let big = Prefs::load(&path).expect("big");
        assert_eq!((big.avatar_corners, big.message_corners), (100, 100));
        assert_eq!(
            Prefs::load(&dir.path().join("none"))
                .expect("d")
                .message_corners,
            30
        );
    }

    /// Tags: one line, at most 8 characters, only for an account uuid, never empty.
    #[test]
    fn account_tags_are_read_leniently_and_cut_to_eight_characters() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kourier.toml");
        std::fs::write(
            &path,
            format!(
                "[account-tags]\n\"{UUID_A}\" = \"  Work  tag long\"\n\"nope\" = \"x\"\n\"{UUID_B}\" = \"  \"\n"
            ),
        )
        .expect("write");
        let p = Prefs::load(&path).expect("load");
        assert_eq!(
            p.account_tags,
            BTreeMap::from([(UUID_A.to_owned(), "Work tag".to_owned())])
        );
        assert_eq!(clean_tag("abcdefghij"), "abcdefgh");
        assert_eq!(clean_tag("  a\nb "), "a b");
        assert!(p.drafts_on_top && p.show_account_tags);
        std::fs::write(&path, "account-tags = 3\n").expect("write");
        assert!(Prefs::load(&path).expect("x").account_tags.is_empty());
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

    /// "Forward to another account as": Header for a profile that never chose, each choice
    /// round trips through the file and a combo index out of range is Header.
    #[test]
    fn forward_look_is_header_unless_the_file_says_otherwise() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kourier.toml");
        assert_eq!(Prefs::default().forward_look, ForwardLookPref::Header);
        assert_eq!(
            Prefs::load(&path).expect("missing").forward_look,
            ForwardLookPref::Header
        );
        std::fs::write(&path, "tray = true\n").expect("write");
        assert_eq!(
            Prefs::load(&path).expect("old").forward_look,
            ForwardLookPref::Header
        );
        for (choice, index, word) in [
            (ForwardLookPref::Plain, 0, "plain"),
            (ForwardLookPref::Header, 1, "header"),
            (ForwardLookPref::Quoted, 2, "quoted"),
            (ForwardLookPref::Picture, 3, "picture"),
        ] {
            let p = Prefs {
                forward_look: choice,
                ..Prefs::default()
            };
            p.save(&path).expect("save");
            assert_eq!(Prefs::load(&path).expect("load"), p);
            assert_eq!(choice.index(), index);
            assert_eq!(ForwardLookPref::from_index(index), choice);
            let text = std::fs::read_to_string(&path).expect("read");
            assert!(
                text.contains(&format!("forward-look = \"{word}\"")),
                "{text}"
            );
        }
        assert_eq!(ForwardLookPref::from_index(7), ForwardLookPref::Header);
        assert_eq!(ForwardLookPref::from_index(-1), ForwardLookPref::Header);
    }

    /// A `forward-look` value this version does not know reads as Header and keeps every other
    /// preference.
    #[test]
    fn an_unknown_forward_look_value_keeps_the_other_prefs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kourier.toml");
        for value in ["\"bogus\"", "\"Picture\"", "3", "true"] {
            std::fs::write(
                &path,
                format!("tray = true\nnetworks-on = [\"WhatsApp\"]\nforward-look = {value}\n"),
            )
            .expect("write");
            let p = Prefs::load(&path).expect("an unknown value is not an error");
            assert_eq!(p.forward_look, ForwardLookPref::Header, "{value}");
            assert!(p.tray, "{value}");
            assert!(p.network_on(NetworkKind::WhatsApp), "{value}");
        }
        std::fs::write(&path, "forward-look = \"picture\"\n").expect("write");
        assert_eq!(
            Prefs::load(&path).expect("picture").forward_look,
            ForwardLookPref::Picture
        );
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

    /// The 3D model preferences: `#rrggbb` (kept lower case) and `NxNxN`, anything else reads as
    /// the default, and the defaults write nothing.
    #[test]
    fn the_model_preferences_are_read_leniently() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("kourier.toml");
        std::fs::write(
            &path,
            "tray = true\nmodel-colour = \"#E0529C\"\nmodel-box = \"256x256x260\"\n",
        )
        .expect("write");
        let p = Prefs::load(&path).expect("load");
        assert_eq!(
            (p.model_colour.as_str(), p.model_box.as_str()),
            ("#e0529c", "256x256x260")
        );
        std::fs::write(
            &path,
            "tray = true\nmodel-colour = 5\nmodel-box = [\"a\"]\n",
        )
        .expect("write");
        let p = Prefs::load(&path).expect("a bad value never fails the file");
        assert!(p.tray && p.model_colour.is_empty() && p.model_box.is_empty());
        std::fs::write(&path, "model-colour = \"red\"\nmodel-box = \"1x2\"\n").expect("write");
        let p = Prefs::load(&path).expect("load");
        assert!(p.model_colour.is_empty() && p.model_box.is_empty());
        Prefs::default().save(&path).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(!text.contains("model-"), "defaults write nothing: {text}");
    }

    /// `model-slicer` is a table id; anything else (a number, a list, an unknown or odd-cased id)
    /// reads as the default, never as a parse error that would reset the other preferences.
    #[test]
    fn the_slicer_preference_is_read_leniently() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("kourier.toml");
        for (text, expected) in [
            ("model-slicer = \"cura\"\n", "cura"),
            ("model-slicer = \"bambustudio\"\n", "bambustudio"),
            ("model-slicer = \"Cura\"\n", ""),
            ("model-slicer = \"gimp\"\n", ""),
            ("model-slicer = 5\n", ""),
            ("model-slicer = [\"cura\"]\n", ""),
        ] {
            std::fs::write(&path, format!("tray = true\n{text}")).expect("write");
            let p = Prefs::load(&path).expect("a bad value never fails the file");
            assert!(p.tray, "{text}");
            assert_eq!(p.model_slicer, expected, "{text}");
        }
        let prefs = Prefs {
            model_slicer: "prusaslicer".to_owned(),
            ..Prefs::default()
        };
        prefs.save(&path).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("model-slicer = \"prusaslicer\""), "{text}");
        Prefs::default().save(&path).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            !text.contains("model-slicer"),
            "the default writes nothing: {text}"
        );
    }

    /// The emoji lists: absent pins are Signal's six (`None`), an empty list is kept as the choice
    /// to pin none, a bad entry is skipped, a list is cut to its limit, and a value of another
    /// shape never costs the file's other preferences.
    #[test]
    fn the_emoji_lists_are_read_leniently() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kourier.toml");
        let none = Prefs::default();
        assert!(none.emoji_pins.is_none() && none.emoji_favourites.is_empty());
        none.save(&path).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(!text.contains("emoji-"), "{text}");
        std::fs::write(&path, "tray = true\nemoji-pins = []\n").expect("write");
        let p = Prefs::load(&path).expect("load");
        assert_eq!(p.emoji_pins, Some(Vec::new()));
        p.save(&path).expect("save");
        assert!(
            std::fs::read_to_string(&path)
                .expect("read")
                .contains("emoji-pins = []")
        );
        let many: Vec<String> = (0..40).map(|i| format!("\"x{i}\"")).collect();
        std::fs::write(
            &path,
            format!(
                "tray = true\nemoji-pins = [\"a\", 7, \"\", \"a\", \"b\\u0007\", \"c\", {}]\n\
                 emoji-recent = [{}]\nemoji-favourites = \"no\"\n",
                many[..8].join(", "),
                many.join(", ")
            ),
        )
        .expect("write");
        let p = Prefs::load(&path).expect("a bad entry still loads");
        assert!(p.tray);
        assert_eq!(p.emoji_pins.as_deref().map(<[String]>::len), Some(6));
        assert_eq!(p.emoji_pins.as_ref().expect("pins")[..2], ["a", "c"]);
        assert_eq!(p.emoji_recent.len(), EMOJI_RECENT_MAX);
        assert!(p.emoji_favourites.is_empty());
        std::fs::write(
            &path,
            "tray = true\nemoji-pins = 5\nemoji-recent = {a = 1}\n",
        )
        .expect("write");
        let p = Prefs::load(&path).expect("another shape still loads");
        assert!(p.tray && p.emoji_pins.is_none() && p.emoji_recent.is_empty());
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

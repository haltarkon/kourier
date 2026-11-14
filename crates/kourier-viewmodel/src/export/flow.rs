//! The pieces of the export flow that need no source: the dialog's view, the job the worker
//! runs, the words of the end. Qt-free; `core_source/exporting.rs` keeps the state machine.

use std::path::PathBuf;

use kourier_core::facade::{
    ExportFailure, ExportOutcome, ExportPlan, Refusal, RequestId, SaveMode, SaveTarget,
};
use kourier_model::{AttachmentId, ConversationId};
use rand::RngCore;
use serde_json::json;

use super::{BuildInput, Built, DialogNumbers, Ticks};
use crate::i18n::{trc, trfc, trnc};
use crate::time::file_size;

/// The most failed file names an end notice names.
const NAMED_FAILED: usize = 3;

/// What the dialog shows: the numbers per ticks, with the sizes written out. The words are the
/// dialog's (QML, `L10n`); every number, with a tick or without, is made here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogView {
    /// The plan's numbers.
    pub numbers: DialogNumbers,
}

impl DialogView {
    /// The view of `numbers`.
    pub fn new(numbers: DialogNumbers) -> Self {
        Self { numbers }
    }

    /// The ticks the dialog opens with: the three kinds on, disappearing off.
    pub const OPENING: Ticks = Ticks {
        pictures: true,
        videos: true,
        other_files: true,
        disappearing: false,
    };

    /// The JSON text the dialog opens with ([`Self::to_json_for`] at [`Self::OPENING`]).
    pub fn to_json(&self) -> String {
        self.to_json_for(Self::OPENING)
    }

    /// The JSON text the dialog reads for `ticks` (numbers and sizes only; no name of any file).
    /// `pictures`, `videos` and `other` are the kind lines (what each kind holds with the
    /// disappearing tick); `messages` and `totalSize` are what goes into the archive.
    pub fn to_json_for(&self, ticks: Ticks) -> String {
        let n = &self.numbers;
        let shown = n.shown(ticks);
        let kind = |c: &super::ClassCount| json!({"count": c.count, "bytes": c.bytes, "size": file_size(c.bytes)});
        json!({
            "messages": shown.messages,
            "disappearing": n.disappearing,
            "viewOnce": n.left_out_view_once,
            "gone": n.left_out_gone,
            "pictures": kind(&shown.offered_pictures),
            "videos": kind(&shown.offered_videos),
            "other": kind(&shown.offered_other),
            "totalSize": file_size(shown.total_bytes),
            "notHere": shown.not_here,
        })
        .to_string()
    }
}

/// The work to do off the Qt thread: the documents of one export. Everything is owned and
/// `Send`; the salt is made when it runs.
pub struct ExportJob {
    /// The export's request id.
    pub request: RequestId,
    pub(crate) conv: ConversationId,
    pub(crate) input: BuildInput,
    pub(crate) dir: PathBuf,
    pub(crate) name: String,
}

/// What a worker made: ready for `UiCommand::ExportArchive`.
pub struct ExportBuilt {
    /// The export's request id.
    pub request: RequestId,
    pub(crate) conv: ConversationId,
    pub(crate) target: SaveTarget,
    pub(crate) mode: SaveMode,
    pub(crate) with_disappearing: bool,
    pub(crate) built: Built,
}

impl std::fmt::Debug for ExportBuilt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportBuilt")
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

impl ExportBuilt {
    /// The messages and files that go into the archive (for the tests and the report).
    pub fn built(&self) -> &Built {
        &self.built
    }
}

impl ExportJob {
    /// Builds the documents. A name that is a file already was confirmed by the file dialog
    /// (it asks before it replaces): that one is replaced; any other name is never replaced,
    /// so a file that appeared since the dialog stays.
    pub fn run(mut self) -> ExportBuilt {
        // A fresh salt per export, from the thread-local CSPRNG (ChaCha, seeded from the OS).
        rand::rng().fill_bytes(&mut self.input.salt);
        let with_disappearing = self.input.ticks.disappearing;
        let taken = std::fs::symlink_metadata(self.dir.join(&self.name)).is_ok();
        let built = super::build(self.input);
        ExportBuilt {
            request: self.request,
            conv: self.conv,
            target: SaveTarget {
                dir: self.dir,
                name: self.name,
            },
            mode: if taken {
                SaveMode::Replace
            } else {
                SaveMode::NoReplace
            },
            with_disappearing,
            built,
        }
    }
}

/// The name of an archive the dialog offers: `<chat> <date> <time>.zip`, cleaned.
pub fn suggested_archive_name(chat: &str, date: &str, time: &str) -> String {
    crate::save_name::clean_name(&format!("{chat} {date} {time}.zip"))
}

/// How an export ended, for the window.
#[derive(Clone, PartialEq, Eq)]
pub struct ExportEnd {
    /// The notice (plain text; it may name files).
    pub text: String,
    /// The selection mode ends (a finished export) and the composer comes back.
    pub selection_ends: bool,
    /// The archive that was written, for "Show in Folder".
    pub saved: Option<PathBuf>,
    /// The chat of the export (`None`: an end that has no chat).
    pub conv: Option<ConversationId>,
}

impl ExportEnd {
    /// Whether the selection mode of the chat `open` ends with this end: only a finished export
    /// ends the selection, and only of its own chat (a chat opened since keeps its picking).
    pub fn ends_selection_in(&self, open: ConversationId) -> bool {
        self.selection_ends && self.conv == Some(open)
    }
}

/// Prints no text, name or path: lengths and `is_some` only.
impl std::fmt::Debug for ExportEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportEnd")
            .field("text_len", &self.text.len())
            .field("selection_ends", &self.selection_ends)
            .field("saved", &self.saved.is_some())
            .field("conv", &self.conv)
            .finish()
    }
}

/// Why a chat is refused, in plain words (the button's tooltip and the notice).
pub fn refusal_text(refusal: Refusal) -> String {
    match refusal {
        Refusal::ExportRestricted => trc(
            "@info:tooltip why Export Selected is off",
            "This chat's network does not allow saving its messages.",
        )
        .into_owned(),
        Refusal::ExportUnstated => trc(
            "@info:tooltip why Export Selected is off",
            "Kourier cannot tell yet whether this chat may be saved.",
        )
        .into_owned(),
        Refusal::ExportProtected => trc(
            "@info:tooltip why Export Selected is off",
            "This chat is protected, so it cannot be exported.",
        )
        .into_owned(),
        _ => trc(
            "@info:tooltip why Export Selected is off",
            "This chat cannot be exported.",
        )
        .into_owned(),
    }
}

/// The tooltip of Export Selected: empty when the chat may be exported.
pub fn export_off_text(verdict: Result<(), Refusal>) -> String {
    verdict.err().map(refusal_text).unwrap_or_default()
}

/// A notice that is not the end of a run (nothing was written, the selection stays). `conv` is
/// the chat of the export.
pub fn plain_end(text: String, conv: Option<ConversationId>) -> ExportEnd {
    ExportEnd {
        text,
        selection_ends: false,
        saved: None,
        conv,
    }
}

/// The longest a failed file's name gets in the end notice, in characters.
const NAME_CHARS: usize = 60;

/// A failed file's name as the notice shows it: cleaned like a file name (no control or bidi
/// character, no line break), cut to [`NAME_CHARS`] characters with "…".
pub fn shown_file_name(name: &str) -> String {
    let clean = crate::save_name::clean_name(name);
    if clean.chars().count() <= NAME_CHARS {
        return clean;
    }
    let mut cut: String = clean.chars().take(NAME_CHARS).collect();
    cut.push('…');
    cut
}

/// The notice for the end of a run. `failed` are the names of the files that could not be read
/// (on the screen only, never in a log); `path` is the archive's place.
pub fn end_of(
    outcome: &ExportOutcome,
    failed: &[String],
    path: PathBuf,
    conv: Option<ConversationId>,
) -> ExportEnd {
    match outcome {
        ExportOutcome::Done {
            messages,
            failed: failed_ids,
            ..
        } => {
            let mut text = trnc(
                "@info:status, %1 how many messages were exported",
                "Exported %1 message",
                "Exported %1 messages",
                u64::from(*messages),
            );
            if !failed_ids.is_empty() {
                text.push_str(". ");
                text.push_str(&trnc(
                    "@info:status, %1 how many files",
                    "%1 file could not be read and is not in the archive.",
                    "%1 files could not be read and are not in the archive.",
                    failed_ids.len() as u64,
                ));
                if !failed.is_empty() {
                    let mut names = failed
                        .iter()
                        .take(NAMED_FAILED)
                        .map(|n| shown_file_name(n))
                        .collect::<Vec<_>>()
                        .join(", ");
                    if failed.len() > NAMED_FAILED {
                        names.push('…');
                    }
                    text.push(' ');
                    text.push_str(&trfc(
                        "@info:status, %1 the names of the files",
                        "Not saved: %1",
                        &[&names],
                    ));
                }
            }
            ExportEnd {
                text,
                selection_ends: true,
                saved: Some(path),
                conv,
            }
        }
        ExportOutcome::Cancelled => plain_end(
            trc("@info:status", "Export cancelled. Nothing was saved.").into_owned(),
            conv,
        ),
        ExportOutcome::Failed(ExportFailure::Changed) => plain_end(
            trc(
                "@info:status",
                "Messages changed while exporting. Try again.",
            )
            .into_owned(),
            conv,
        ),
        ExportOutcome::Failed(ExportFailure::ChatRefused(r)) => plain_end(refusal_text(*r), conv),
        ExportOutcome::Failed(f) => {
            let why = match f {
                ExportFailure::Exists => trc(
                    "@info:status why an export failed",
                    "a file with that name is already there",
                ),
                ExportFailure::NoFolder => {
                    trc("@info:status why an export failed", "the folder is gone")
                }
                ExportFailure::NotWritable => trc(
                    "@info:status why an export failed",
                    "the folder is not writable",
                ),
                ExportFailure::NoSpace => trc(
                    "@info:status why an export failed",
                    "there is no space left on the disk",
                ),
                _ => trc(
                    "@info:status why an export failed",
                    "the file could not be written",
                ),
            };
            plain_end(
                trfc(
                    "@info:status, %1 why",
                    "Could not export the messages: %1.",
                    &[&why],
                ),
                conv,
            )
        }
    }
}

/// The language tag of the page (`en`, `uk`) from the environment's message language.
pub fn page_language() -> String {
    let raw = ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.is_empty())
        .unwrap_or_default();
    let first = raw.split(':').next().unwrap_or("");
    let tag: String = first
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_lowercase();
    if tag.is_empty() || tag == "c" || tag == "posix" {
        "en".to_owned()
    } else {
        tag
    }
}

/// `+03:00`: a UTC offset in seconds as the manifest writes a time zone it has no name for.
pub fn offset_zone(secs: i32) -> String {
    let sign = if secs < 0 { '-' } else { '+' };
    let a = secs.unsigned_abs();
    format!("{sign}{:02}:{:02}", a / 3600, a % 3600 / 60)
}

/// The names of the files of `plan`, by attachment (on the screen only, for the end notice).
pub fn file_names(plan: &ExportPlan) -> std::collections::HashMap<AttachmentId, String> {
    let mut out = std::collections::HashMap::new();
    for pm in &plan.messages {
        for f in &pm.files {
            if let Some(n) = f.file_name.as_deref().filter(|n| !n.is_empty()) {
                out.insert(f.attachment, n.to_owned());
            }
        }
    }
    out
}

/// What the flow tells the window, taken with `UiSource::take_export`.
#[derive(Clone, PartialEq, Eq)]
pub enum ExportUpdate {
    /// The plan came: the dialog opens with these numbers ([`DialogView::to_json`]).
    Dialog(String),
    /// The dialog's Export… was pressed: the save dialog opens with this file name (the window
    /// adds the folder).
    SaveName(String),
    /// Bytes written so far and in all (`total` 0: the documents are still being made).
    Progress {
        /// Bytes written.
        done: u64,
        /// Bytes in all.
        total: u64,
    },
    /// The run (or the refusal before it) is over.
    Ended(ExportEnd),
    /// A new export was asked while one stands: the notice only (the running export's progress
    /// stays).
    Refused(String),
}

/// Prints no text, name, path or JSON: lengths and `is_some` only.
impl std::fmt::Debug for ExportUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dialog(json) => f.debug_tuple("Dialog").field(&json.len()).finish(),
            Self::SaveName(name) => f.debug_tuple("SaveName").field(&name.len()).finish(),
            Self::Progress { done, total } => f
                .debug_struct("Progress")
                .field("done", done)
                .field("total", total)
                .finish(),
            Self::Ended(end) => f.debug_tuple("Ended").field(end).finish(),
            Self::Refused(text) => f.debug_tuple("Refused").field(&text.len()).finish(),
        }
    }
}

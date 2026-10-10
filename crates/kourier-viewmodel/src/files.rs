//! The composer's file strip (§8.3): the files waiting to be sent, however many and of
//! whatever kinds, with what the core says about them.
//!
//! Kourier has no limit of its own here (architecture §1.1 goal 2). Signal Desktop's composer
//! takes at most 32 attachments, several only when all are pictures or videos, and one other
//! file alone (`SD/ts/state/ducks/composer.preload.ts` `preProcessAttachment`); none of that is
//! enforced by the server or needed by other clients, so none of it is copied. The strip takes
//! whatever the user adds, in one go, and shows what the network makes of it:
//!
//! - how the selection will be split into messages ([`FileRow::part`], [`StripSummary`]), from
//!   the core's preview (`Query::FilePlan`), which is what `SendFiles` then does;
//! - which files the network cannot take, each with its reason ([`FileRow::reason`]). Those
//!   stay in the strip, marked, and never hold the others back: a send takes every file that
//!   can go ([`FileStrip::sendable`]).
//!
//! Two more things are said, quietly, without refusing anything: a picture or a video can be
//! sent as the file it is ([`StripFile::as_file`]: not re-encoded, not part of an album), and
//! a file of a type that some client of the network will not save is marked as such
//! ([`FileRow::note`]; which types, the network states in its features).
//!
//! A folder is not a file and is never marked as one that cannot go (user decision,
//! 2026-10-07). It becomes an entry of its own kind ([`StripFile::folder`]) that stands where
//! it was dropped while Kourier counts what it holds and then asks, every time
//! ([`FileStrip::folder_question`], one question for all the folders that wait): add the
//! files inside, or send them as one `.zip`. The answer turns the entry, in its place, into
//! those files or into that archive; until then the entry is not part of a send, and the
//! files around it go as usual.
//!
//! Nothing here touches the disk: sizes, whether a file is a picture that may be shown as
//! its own preview and whether an entry is a folder come from a look at the file off the UI
//! thread ([`FileStrip::set_facts`]), what a folder holds from a walk off the UI thread
//! ([`FileStrip::folder_counted`]), and kinds from the core ([`FileStrip::apply_plan`]);
//! until then a kind is guessed from the file name.
//!
//! Paths and names are personal data: `Debug` prints neither.

use std::path::{Path, PathBuf};

use kourier_core::facade::{FileRefusal, FilesPreview};
use kourier_model::AttachmentKind;

use crate::i18n::{trc, trfc, trnc};
use crate::media::{icon_for_file_name, kind_for_file_name, kind_name};
use crate::time::file_size;

/// One file waiting in the strip.
#[derive(Clone, PartialEq, Eq)]
pub struct StripFile {
    /// Stable identity of the entry while it is in the strip (the list model's key).
    pub id: u64,
    /// The file (personal data: never logged).
    pub path: PathBuf,
    /// Its name, as shown.
    pub name: String,
    /// What it is: read from the content once the core looked at it, guessed from the name
    /// until then.
    pub kind: AttachmentKind,
    /// [`Self::kind`] comes from the content.
    pub inspected: bool,
    /// Its size in bytes, once known (never for what is no regular file).
    pub size: Option<u64>,
    /// The file was looked at for its size and content (off the UI thread).
    pub sized: bool,
    /// Its first bytes are those of a raster picture format the preview may decode
    /// ([`FileFacts::picture`]).
    pub picture: bool,
    /// It was made by a paste and is deleted when it is no longer needed.
    pub pasted: bool,
    /// Why the network cannot take it; `None`: it can be sent.
    pub refusal: Option<FileRefusal>,
    /// The message it will travel in (0-based, in sending order), once the core planned it.
    pub part: Option<usize>,
    /// The user chose to send it as the file it is: a picture is not re-encoded or scaled, a
    /// video is not shown inline, neither joins an album (`UiCommand::SendFiles` `as_file`).
    pub as_file: bool,
    /// It is a folder, at this stage of becoming files or an archive; `None` for a file.
    pub folder: Option<FolderEntry>,
}

crate::safe_debug!(StripFile {
    id,
    kind,
    inspected,
    size,
    sized,
    pasted,
    refusal,
    part,
    as_file,
    folder
});

/// Where a folder in the strip stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FolderStage {
    /// What it holds is being counted.
    Counting,
    /// Counted: the question is open ([`FileStrip::folder_question`]).
    Asking,
    /// The answer was "add the files inside": they are being listed.
    Listing,
    /// The answer was the archive: it is being packed.
    Packing,
}

impl FolderStage {
    /// Stable name for the list's `stage` role.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Counting => "counting",
            Self::Asking => "asking",
            Self::Listing => "listing",
            Self::Packing => "packing",
        }
    }
}

/// What a folder holds and what a walk of it leaves out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FolderCount {
    /// Regular files below the folder, subfolders included.
    pub files: u64,
    /// Their sizes added up.
    pub bytes: u64,
    /// Left out: files and folders whose name starts with a dot.
    pub hidden: u64,
    /// Left out: symbolic links (never followed).
    pub links: u64,
    /// Left out: what is no file (sockets, pipes, devices) and what could not be read.
    pub other: u64,
}

impl FolderCount {
    /// Everything that was left out.
    pub fn skipped(&self) -> u64 {
        self.hidden + self.links + self.other
    }
}

/// A folder in the strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FolderEntry {
    /// Where it stands.
    pub stage: FolderStage,
    /// What it holds (while [`FolderStage::Counting`]: so far).
    pub count: FolderCount,
    /// While [`FolderStage::Packing`]: bytes packed so far, of [`FolderCount::bytes`].
    pub done: u64,
    /// Nobody has started counting it yet ([`FileStrip::take_new_folders`]).
    fresh: bool,
}

/// One file of a folder as a walk found it: what [`FileStrip::folder_listed`] puts in the
/// folder's place. Its size and whether it is a picture are known already.
#[derive(Clone, PartialEq, Eq)]
pub struct ListedFile {
    /// The file (personal data: never logged).
    pub path: PathBuf,
    /// Its size in bytes.
    pub size: u64,
    /// [`FileFacts::picture`].
    pub picture: bool,
}

crate::safe_debug!(ListedFile { size, picture });

/// What counting a folder came to ([`FileStrip::folder_counted`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Counted {
    /// The folder waits for the user's answer.
    Asking,
    /// It holds no file that could be sent and left the strip; its name, for the notice.
    Empty(String),
    /// The entry is not in this strip (any more).
    Gone,
}

/// A folder the user answered about: what the work on it needs.
#[derive(Clone, PartialEq, Eq)]
pub struct FolderJob {
    /// The entry.
    pub id: u64,
    /// The folder (personal data: never logged).
    pub path: PathBuf,
    /// Its name, as shown (the archive is named after it).
    pub name: String,
}

crate::safe_debug!(FolderJob { id });

/// What a chat takes as one plain file: decides whether a folder can go as an archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArchiveRule {
    /// The chat takes files that are neither pictures, videos nor sound (an archive is one).
    pub accepted: bool,
    /// The largest such file it takes, in bytes; `None` when it states no limit.
    pub max: Option<u64>,
}

impl Default for ArchiveRule {
    /// No rule known: nothing is held back here, the core has the last word.
    fn default() -> Self {
        Self {
            accepted: true,
            max: None,
        }
    }
}

/// How many files "add the files inside" puts into the list without asking once more. Not a
/// limit: after the second yes every file is added. It is Kourier's own guard against a
/// slip (a project folder with ten thousand files, each of which would become a message),
/// set where a list stops being something a person reads.
pub const LARGE_ADD: u64 = 1_000;

/// The question about the folders that wait in the strip: what the dialog says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FolderQuestion {
    /// The entries it is about.
    pub ids: Vec<u64>,
    /// "Send a Folder".
    pub title: String,
    /// What the folders hold: "“Raw footage” holds 124 files, 3.2 MB in all."
    pub text: String,
    /// What a walk leaves out, when it leaves anything out; else empty.
    pub skipped: String,
    /// The first answer.
    pub add_label: String,
    /// What it does.
    pub add_hint: String,
    /// The second answer.
    pub zip_label: String,
    /// What it does, or why it is not possible here.
    pub zip_hint: String,
    /// The second answer can be given: every archive would be a file the chat takes, by
    /// the sizes of the files added up.
    pub can_zip: bool,
    /// The first answer would put more than [`LARGE_ADD`] files into the list: it is asked
    /// about once more.
    pub large: bool,
    /// The second question.
    pub confirm: String,
    /// Its "yes".
    pub confirm_label: String,
    /// Files in all the folders.
    pub files: u64,
    /// Their sizes added up.
    pub bytes: u64,
}

/// One row of the strip as the list shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct FileRow {
    /// Identity of the entry (the diff key).
    pub id: u64,
    /// The file name.
    pub name: String,
    /// Stable kind name (`image`, `gif`, `video`, `audio`, `voice`, `file`).
    pub kind: &'static str,
    /// Breeze icon for the type.
    pub icon: &'static str,
    /// The file itself may be shown as its preview: a raster picture by its first bytes
    /// ([`FileFacts::picture`]).
    pub picture: bool,
    /// The file, for that preview (personal data: never logged).
    pub path: PathBuf,
    /// "1.2 MB"; empty while unknown.
    pub size: String,
    /// Why it cannot be sent; empty when it can.
    pub reason: String,
    /// The number of the message it travels in, counting from 1; 0 while that is not known
    /// and for a file that cannot be sent.
    pub part: u32,
    /// It is the first file of its message.
    pub part_first: bool,
    /// It is the last file of its message.
    pub part_last: bool,
    /// The line that joins the files of one message passes this row, which is not one of
    /// them (a file that stays behind stands between two files of one message).
    pub part_through: bool,
    /// The file is a picture or a video: it can be sent as the file it is instead.
    pub can_original: bool,
    /// It is sent as the file it is ([`StripFile::as_file`]).
    pub original: bool,
    /// Something worth knowing that holds nothing back: some client of the network will not
    /// save a file of this type; for a folder, what is happening to it. Empty for nothing.
    pub note: String,
    /// For a folder, where it stands ([`FolderStage::as_str`]); empty for a file.
    pub stage: &'static str,
    /// For a folder that is being packed: how far, in thousandths.
    pub progress: u16,
}

crate::safe_debug!(FileRow {
    id,
    kind,
    picture,
    part,
    part_first,
    part_last,
    part_through,
    can_original,
    original,
    stage,
    progress
});

/// What a look at a file found (off the UI thread).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileFacts {
    /// Its size in bytes; `None` for what is no regular file or cannot be looked at.
    pub size: Option<u64>,
    /// Its first bytes are those of a raster picture format on the UI's allow-list. Only
    /// then is the file itself shown as its preview: the toolkit picks an image decoder by
    /// content, and the installed decoders include PostScript and PDF, so a file is never
    /// handed to it on the strength of its name or of a type that was derived from the name.
    pub picture: bool,
    /// It is a folder (a link to one counts: the user pointed at it).
    pub folder: bool,
}

/// What the line above the strip says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StripSummary {
    /// Files in the strip (folders that wait are not files).
    pub files: usize,
    /// Folders in the strip: being counted, asked about, listed or packed.
    pub folders: usize,
    /// Of those, how many the network cannot take.
    pub refused: usize,
    /// Messages the files that can go will become (0 while the core has not planned them).
    /// With [`Self::lead_text`] the caption adds [`Self::caption_messages`].
    pub messages: usize,
    /// The caption goes first as text of its own (the first file can carry none, or the
    /// caption is longer than one message).
    pub lead_text: bool,
    /// How many messages the caption becomes when it goes first (0 when it rides on the
    /// first message with files).
    pub caption_messages: usize,
    /// The send would be refused as a whole: files cannot quote here and there is no text to
    /// carry the quote.
    pub reply_unsendable: bool,
    /// Bytes of all files, once every size is known.
    pub bytes: Option<u64>,
    /// "7 files, 4 messages · 12.4 MB".
    pub text: String,
    /// A second line when there is something to explain; else empty.
    pub hint: String,
}

/// The answer to one plan request: what the core makes of the selection that had this serial.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanAnswer {
    /// The strip's [`FileStrip::serial`] when the plan was asked for.
    pub serial: u64,
    /// The core's preview.
    pub preview: FilesPreview,
}

/// What a plan request needs: taken from the strip, sent to the source.
#[derive(Clone, PartialEq, Eq)]
pub struct PlanRequest {
    /// The strip's serial; the answer carries it back.
    pub serial: u64,
    /// The files, in order (personal data: never logged).
    pub paths: Vec<PathBuf>,
    /// Per file: it would be sent as the file it is ([`StripFile::as_file`]).
    pub as_file: Vec<bool>,
    /// The user wrote a caption.
    pub caption: bool,
    /// The caption itself, when its length matters to the plan: it is longer than one message
    /// of the chat carries, so it goes first as several (personal data: never logged).
    pub caption_text: Option<String>,
    /// The send would quote a message.
    pub reply: bool,
}

impl std::fmt::Debug for PlanRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanRequest")
            .field("serial", &self.serial)
            .field("files", &self.paths.len())
            .field("as_file", &self.as_file.iter().filter(|a| **a).count())
            .field("caption", &self.caption)
            .field("long_caption", &self.caption_text.is_some())
            .field("reply", &self.reply)
            .finish()
    }
}

/// One file as a send or a return hands it over: the path, whether a paste made it, whether
/// it goes as the file it is.
#[derive(Clone, PartialEq, Eq)]
pub struct HandedFile {
    /// The file (personal data: never logged).
    pub path: PathBuf,
    /// A paste made it.
    pub pasted: bool,
    /// [`StripFile::as_file`].
    pub as_file: bool,
}

crate::safe_debug!(HandedFile { pasted, as_file });

/// What the network of a conversation says that the strip only tells the user about.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StripRules {
    /// File-name extensions (lower case) some client of the network will not save
    /// (`ConversationFeatures::hard_to_save`).
    pub hard_to_save: std::collections::BTreeSet<String>,
    /// The network's name, for the note ("Signal").
    pub network: String,
}

impl StripRules {
    /// The note for a file called `name`; empty when there is nothing to say.
    fn note(&self, name: &str) -> String {
        let trimmed = name.trim_end_matches(|c: char| c == '.' || c.is_whitespace());
        let hard = trimmed
            .rsplit_once('.')
            .is_some_and(|(_, ext)| self.hard_to_save.contains(&ext.to_lowercase()));
        if !hard {
            return String::new();
        }
        if self.network.is_empty() {
            trc(
                "@info:tooltip file type warning",
                "Some apps cannot save this type of file",
            )
            .into_owned()
        } else {
            trfc(
                "@info:tooltip file type warning, %1 the name of a network",
                "Some %1 apps cannot save this type of file",
                &[&self.network],
            )
        }
    }
}

/// The files waiting in the composer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileStrip {
    files: Vec<StripFile>,
    next_id: u64,
    /// Counts every change of what a plan depends on (the files, the caption and reply flags).
    serial: u64,
    /// The serial the plan in `files` belongs to.
    planned: Option<u64>,
    caption: bool,
    /// The caption, kept only while it is longer than one message (see [`PlanRequest`]).
    caption_text: Option<String>,
    reply: bool,
    messages: usize,
    lead_text: bool,
    caption_messages: usize,
    reply_unsendable: bool,
    rules: StripRules,
}

/// The file name of a path, as shown.
fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

impl FileStrip {
    /// An empty strip.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty strip whose entry ids and serials start above `base`. A composer that keeps
    /// one strip per chat gives each its own range (`n << 32`): what was asked about the
    /// files of one chat (their sizes, their plan) is answered by id and by serial, and an
    /// answer that arrives after the user went on to another chat must never fit that
    /// chat's strip by accident.
    pub fn starting_at(base: u64) -> Self {
        Self {
            next_id: base,
            serial: base,
            ..Self::default()
        }
    }

    /// The files, in the order they will be sent.
    pub fn files(&self) -> &[StripFile] {
        &self.files
    }

    /// How many files wait.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether nothing waits.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Changes with everything a plan depends on.
    pub fn serial(&self) -> u64 {
        self.serial
    }

    /// Whether the core's plan for the current selection is in (folders that wait are not
    /// part of a plan: with nothing but folders there is nothing to plan).
    pub fn planned(&self) -> bool {
        self.files.iter().all(|f| f.folder.is_some()) || self.planned == Some(self.serial)
    }

    /// Where the files a plan is about stand in the strip: the positions of the entries
    /// that are not folders, in order. A plan numbers the files it was asked about; this
    /// maps its numbers back.
    fn plan_positions(&self) -> Vec<usize> {
        self.files
            .iter()
            .enumerate()
            .filter(|(_, f)| f.folder.is_none())
            .map(|(i, _)| i)
            .collect()
    }

    fn changed(&mut self) {
        self.serial += 1;
        if self.files.is_empty() {
            self.planned = None;
            self.messages = 0;
            self.lead_text = false;
            self.caption_messages = 0;
            self.reply_unsendable = false;
        }
    }

    /// Adds files at the end, any number of any kinds; `pasted` marks files a paste made.
    /// Returns how many were added (all of them: nothing is refused here).
    pub fn add(&mut self, paths: impl IntoIterator<Item = PathBuf>, pasted: bool) -> usize {
        self.add_handed(paths.into_iter().map(|path| HandedFile {
            path,
            pasted,
            as_file: false,
        }))
    }

    /// Adds files at the end, each with what is known about it (files that come back from a
    /// send keep the user's choices). Returns how many were added.
    pub fn add_handed(&mut self, files: impl IntoIterator<Item = HandedFile>) -> usize {
        let before = self.files.len();
        for file in files {
            let name = name_of(&file.path);
            self.next_id += 1;
            self.files.push(StripFile {
                id: self.next_id,
                kind: kind_for_file_name(&name),
                inspected: false,
                size: None,
                sized: false,
                picture: false,
                pasted: file.pasted,
                refusal: None,
                part: None,
                as_file: file.as_file,
                folder: None,
                path: file.path,
                name,
            });
        }
        let added = self.files.len() - before;
        if added > 0 {
            self.changed();
        }
        added
    }

    /// Whether entry `index` is a picture or a video, which can be sent as the file it is
    /// instead ([`Self::set_as_file`]). Known from the content once the core looked, from
    /// the name until then.
    pub fn can_be_original(&self, index: usize) -> bool {
        self.files
            .get(index)
            .is_some_and(StripFile::can_be_original)
    }

    /// Chooses how entry `index` is sent: as the file it is (`true`), or as what it holds.
    /// Returns whether that changed anything (only a picture or a video has the choice).
    pub fn set_as_file(&mut self, index: usize, as_file: bool) -> bool {
        let Some(file) = self.files.get_mut(index) else {
            return false;
        };
        if !file.can_be_original() || file.as_file == as_file {
            return false;
        }
        file.as_file = as_file;
        self.changed();
        true
    }

    /// What the conversation's network says about files that the strip passes on to the
    /// user. Returns whether the rows may read differently now.
    pub fn set_rules(&mut self, rules: StripRules) -> bool {
        if self.rules == rules {
            return false;
        }
        self.rules = rules;
        !self.files.is_empty()
    }

    /// Takes entry `index` out.
    pub fn remove(&mut self, index: usize) -> Option<StripFile> {
        if index >= self.files.len() {
            return None;
        }
        let file = self.files.remove(index);
        self.changed();
        Some(file)
    }

    /// Takes the entry with identity `id` out.
    pub fn remove_id(&mut self, id: u64) -> Option<StripFile> {
        let index = self.files.iter().position(|f| f.id == id)?;
        self.remove(index)
    }

    /// Empties the strip; returns what was in it.
    pub fn clear(&mut self) -> Vec<StripFile> {
        let files = std::mem::take(&mut self.files);
        if !files.is_empty() {
            self.changed();
        }
        files
    }

    /// The caption and reply flags a plan depends on. Returns whether one of them changed
    /// while files wait (a new plan is needed).
    pub fn set_context(&mut self, caption: bool, reply: bool) -> bool {
        self.set_caption(caption, None, reply)
    }

    /// What a plan depends on besides the files: whether there is a caption, the caption
    /// itself when it is longer than one message of the chat carries (`long`, else `None`:
    /// its length does not matter then), and whether the send quotes a message. Returns
    /// whether any of it changed while files wait (a new plan is needed).
    pub fn set_caption(&mut self, caption: bool, long: Option<String>, reply: bool) -> bool {
        if self.caption == caption && self.reply == reply && self.caption_text == long {
            return false;
        }
        self.caption = caption;
        self.caption_text = long;
        self.reply = reply;
        if self.files.is_empty() {
            return false;
        }
        self.changed();
        true
    }

    /// What to ask the core for when the plan is not current; `None` when it is (or nothing
    /// waits).
    pub fn plan_request(&self) -> Option<PlanRequest> {
        if self.planned() {
            return None;
        }
        let files = || self.files.iter().filter(|f| f.folder.is_none());
        Some(PlanRequest {
            serial: self.serial,
            paths: files().map(|f| f.path.clone()).collect(),
            as_file: files().map(|f| f.as_file).collect(),
            caption: self.caption,
            caption_text: self.caption_text.clone(),
            reply: self.reply,
        })
    }

    /// Takes the core's answer. `false` when it is about an earlier selection (a newer
    /// request is on its way) or does not fit the files.
    pub fn apply_plan(&mut self, answer: &PlanAnswer) -> bool {
        let preview = &answer.preview;
        let at = self.plan_positions();
        if answer.serial != self.serial || preview.kinds.len() != at.len() {
            return false;
        }
        for (position, kind) in at.iter().zip(&preview.kinds) {
            let file = &mut self.files[*position];
            if let Some(kind) = kind {
                file.kind = *kind;
                file.inspected = true;
            }
            file.refusal = None;
            file.part = None;
        }
        for r in &preview.refused {
            if let Some(file) = at.get(r.index).and_then(|i| self.files.get_mut(*i)) {
                file.refusal = Some(r.reason);
                file.part = None;
            }
        }
        for (part, positions) in preview.plan.messages.iter().enumerate() {
            for position in positions {
                if let Some(file) = at.get(*position).and_then(|i| self.files.get_mut(*i)) {
                    file.part = Some(part);
                }
            }
        }
        self.messages = preview.plan.messages.len();
        self.lead_text = preview.plan.lead_text;
        // (A caption that goes first is at least one message, whatever an older core says.)
        self.caption_messages = if preview.plan.lead_text {
            preview.plan.caption_messages.max(1)
        } else {
            0
        };
        self.reply_unsendable = preview.plan.reply_unsendable;
        self.planned = Some(self.serial);
        true
    }

    /// The core could not say how the selection with this serial would be sent (the chat is
    /// gone, the core stopped): the strip stops waiting and shows the files without a split.
    /// Returns whether that was the current selection.
    pub fn plan_unavailable(&mut self, serial: u64) -> bool {
        if serial != self.serial || self.files.is_empty() {
            return false;
        }
        for file in &mut self.files {
            file.part = None;
        }
        self.messages = 0;
        self.lead_text = false;
        self.caption_messages = 0;
        self.reply_unsendable = false;
        self.planned = Some(self.serial);
        true
    }

    /// Puts files back that a send was refused for, each with its reason (the rest of that
    /// send went out). They go to the front: they were picked before whatever waits now.
    pub fn restore_refused(&mut self, files: Vec<(HandedFile, FileRefusal)>) {
        if files.is_empty() {
            return;
        }
        let mut back = Vec::with_capacity(files.len());
        for (file, reason) in files {
            let HandedFile {
                path,
                pasted,
                as_file,
            } = file;
            let name = name_of(&path);
            self.next_id += 1;
            back.push(StripFile {
                id: self.next_id,
                kind: match reason {
                    FileRefusal::KindNotAccepted { kind } => kind,
                    _ => kind_for_file_name(&name),
                },
                inspected: matches!(reason, FileRefusal::KindNotAccepted { .. }),
                size: None,
                sized: false,
                picture: false,
                pasted,
                refusal: Some(reason),
                part: None,
                as_file,
                folder: None,
                path,
                name,
            });
        }
        back.append(&mut self.files);
        self.files = back;
        self.changed();
    }

    /// Takes what a look at the files found, done off the UI thread, by entry identity
    /// (entries removed meanwhile are skipped). Returns whether anything changed.
    pub fn set_facts(&mut self, facts: &[(u64, FileFacts)]) -> bool {
        let by_id: std::collections::HashMap<u64, FileFacts> = facts.iter().copied().collect();
        let mut changed = false;
        let mut folders = false;
        for file in &mut self.files {
            let Some(found) = by_id.get(&file.id) else {
                continue;
            };
            if found.folder {
                if file.folder.is_none() {
                    // Not a file that cannot go: an entry of its own kind, to be counted
                    // and asked about. Whatever the core said of it as a file is void.
                    file.folder = Some(FolderEntry {
                        stage: FolderStage::Counting,
                        count: FolderCount::default(),
                        done: 0,
                        fresh: true,
                    });
                    file.size = None;
                    file.picture = false;
                    file.sized = true;
                    file.refusal = None;
                    file.part = None;
                    file.as_file = false;
                    file.inspected = false;
                    changed = true;
                    folders = true;
                }
            } else if file.folder.is_none()
                && (file.size != found.size || file.picture != found.picture || !file.sized)
            {
                file.size = found.size;
                file.picture = found.picture;
                file.sized = true;
                changed = true;
            }
        }
        if folders {
            // The files a plan is about are fewer now.
            self.changed();
        }
        changed
    }

    /// The folders nobody has started counting yet: `(identity, path)`. Each is handed out
    /// once.
    pub fn take_new_folders(&mut self) -> Vec<(u64, PathBuf)> {
        self.files
            .iter_mut()
            .filter_map(|f| {
                let folder = f.folder.as_mut()?;
                std::mem::take(&mut folder.fresh).then(|| (f.id, f.path.clone()))
            })
            .collect()
    }

    /// Whether the strip holds the entry `id`.
    pub fn contains(&self, id: u64) -> bool {
        self.files.iter().any(|f| f.id == id)
    }

    /// The folder entry `id`, if it is in the strip and still a folder.
    fn folder_mut(&mut self, id: u64) -> Option<&mut FolderEntry> {
        self.files
            .iter_mut()
            .find(|f| f.id == id)
            .and_then(|f| f.folder.as_mut())
    }

    /// The count of folder `id` so far, while it is being counted. Returns whether the row
    /// reads differently.
    pub fn folder_counting(&mut self, id: u64, so_far: FolderCount) -> bool {
        match self.folder_mut(id) {
            Some(folder) if folder.stage == FolderStage::Counting && folder.count != so_far => {
                folder.count = so_far;
                true
            }
            _ => false,
        }
    }

    /// Folder `id` was counted: it waits for the answer now, or, when it holds no file that
    /// could be sent, leaves the strip.
    pub fn folder_counted(&mut self, id: u64, count: FolderCount) -> Counted {
        let Some(at) = self
            .files
            .iter()
            .position(|f| f.id == id && f.folder.is_some())
        else {
            return Counted::Gone;
        };
        if count.files == 0 {
            let gone = self.files.remove(at);
            self.changed();
            return Counted::Empty(shown_name(&gone));
        }
        if let Some(folder) = self.files[at].folder.as_mut() {
            folder.count = count;
            if folder.stage == FolderStage::Counting {
                folder.stage = FolderStage::Asking;
            }
        }
        Counted::Asking
    }

    /// The question about the folders that wait, once every one of them is counted (one
    /// question for all that were given together); `None` while there is nothing to ask.
    /// `rule` is what the chat takes as one file: an archive of a folder whose files add up
    /// to more is not offered, and "add the files inside" is.
    pub fn folder_question(&self, rule: ArchiveRule) -> Option<FolderQuestion> {
        let folders = || {
            self.files
                .iter()
                .filter_map(|f| f.folder.as_ref().map(|folder| (f, folder)))
        };
        if folders().any(|(_, f)| f.stage == FolderStage::Counting) {
            return None;
        }
        let asking: Vec<(&StripFile, &FolderEntry)> = folders()
            .filter(|(_, f)| f.stage == FolderStage::Asking)
            .collect();
        let (first, _) = asking.first()?;
        let n = asking.len() as u64;
        let sum = |pick: fn(&FolderCount) -> u64| {
            asking
                .iter()
                .fold(0u64, |sum, (_, f)| sum.saturating_add(pick(&f.count)))
        };
        let (files, bytes) = (sum(|c| c.files), sum(|c| c.bytes));
        let files_text = trnc(
            "@info a count of files, part of a sentence",
            "%1 file",
            "%1 files",
            files,
        );
        let size = file_size(bytes);
        let text = if n == 1 {
            trfc(
                "@info, %1 the name of a folder, %2 how many files, %3 their size",
                "“%1” holds %2, %3 in all.",
                &[&shown_name(first), &files_text, &size],
            )
        } else {
            trfc(
                "@info, %1 how many folders, %2 how many files, %3 their size",
                "The %1 folders hold %2, %3 in all.",
                &[&n.to_string(), &files_text, &size],
            )
        };
        let mut left_out: Vec<String> = Vec::new();
        let hidden = sum(|c| c.hidden);
        if hidden > 0 {
            left_out.push(trnc(
                "@info what a folder walk left out, part of a list",
                "%1 hidden item",
                "%1 hidden items",
                hidden,
            ));
        }
        let links = sum(|c| c.links);
        if links > 0 {
            left_out.push(trnc(
                "@info what a folder walk left out, part of a list",
                "%1 link (links are not followed)",
                "%1 links (links are not followed)",
                links,
            ));
        }
        let other = sum(|c| c.other);
        if other > 0 {
            left_out.push(trnc(
                "@info what a folder walk left out, part of a list",
                "%1 item that could not be read",
                "%1 items that could not be read",
                other,
            ));
        }
        let skipped = if left_out.is_empty() {
            String::new()
        } else {
            trfc(
                "@info, %1 a comma-separated list of what was left out",
                "Left out: %1.",
                &[&left_out.join(", ")],
            )
        };
        // The archive of a folder is one file: the chat must take such files, and each
        // folder's files must add up to no more than it takes (the estimate of the
        // archive's size; checked before anything is packed).
        let too_large = rule.max.and_then(|max| {
            asking
                .iter()
                .find(|(_, f)| f.count.bytes > max)
                .map(|(file, f)| (*file, f.count.bytes, max))
        });
        let can_zip = rule.accepted && too_large.is_none();
        let zip_hint = match (rule.accepted, too_large) {
            (false, _) => trc(
                "@info:tooltip why a folder cannot be sent as .zip",
                "This chat does not take files of this kind.",
            )
            .into_owned(),
            (true, Some((_, bytes, max))) if n == 1 => trfc(
                "@info:tooltip why a folder cannot be sent as .zip, %1 their size, %2 the limit",
                "Not possible here: the files are %1, and this chat takes files up to %2.",
                &[&file_size(bytes), &file_size(max)],
            ),
            (true, Some((file, bytes, max))) => trfc(
                "@info:tooltip why a folder cannot be sent as .zip, %1 a folder name, %2 their size, %3 the limit",
                "Not possible here: the files in “%1” are %2, and this chat takes files up to %3.",
                &[&shown_name(file), &file_size(bytes), &file_size(max)],
            ),
            (true, None) if n == 1 => trfc(
                "@info:tooltip what sending a folder as .zip does, %1 the file name, %2 its size",
                "Packed into one file, “%1”, of about %2.",
                &[&archive_name(&shown_name(first)), &size],
            ),
            (true, None) => trfc(
                "@info:tooltip what sending folders as .zip does, %1 their size",
                "Each folder is packed into one file, about %1 in all.",
                &[&size],
            ),
        };
        Some(FolderQuestion {
            ids: asking.iter().map(|(f, _)| f.id).collect(),
            title: trnc(
                "@title:window question about folders, %1 how many",
                "Send a Folder",
                "Send %1 Folders",
                n,
            ),
            text,
            skipped,
            add_label: trc(
                "@action:button add the files inside a folder",
                "Add the Files Inside",
            )
            .into_owned(),
            add_hint: trnc(
                "@info:tooltip what adding the files of a folder does, %1 how many files",
                "%1 file joins the list and is sent as it is.",
                "%1 files join the list and are sent as they are.",
                files,
            ),
            zip_label: trnc(
                "@action:button send folders as .zip, %1 how many",
                "Send as One .zip",
                "Send as %1 .zip Files",
                n,
            ),
            zip_hint,
            can_zip,
            large: files > LARGE_ADD,
            confirm: trnc(
                "@info what adding many files does, %1 how many files",
                "This puts %1 file into the list.",
                "This puts %1 files into the list. They are sent one after another, each as it is.",
                files,
            ),
            confirm_label: trnc(
                "@action:button add the files of folders, %1 how many",
                "Add %1 File",
                "Add All %1 Files",
                files,
            ),
            files,
            bytes,
        })
    }

    /// The answer "add the files inside": the folders that waited for it are being listed
    /// now. Returns them, for the walks.
    pub fn answer_add(&mut self) -> Vec<FolderJob> {
        self.answer(FolderStage::Listing)
    }

    /// The answer "send as one .zip": the folders that waited for it are being packed now.
    /// Returns them, for the packing.
    pub fn answer_zip(&mut self) -> Vec<FolderJob> {
        self.answer(FolderStage::Packing)
    }

    fn answer(&mut self, stage: FolderStage) -> Vec<FolderJob> {
        let mut jobs = Vec::new();
        for file in &mut self.files {
            if let Some(folder) = file.folder.as_mut()
                && folder.stage == FolderStage::Asking
            {
                folder.stage = stage;
                folder.done = 0;
                jobs.push(FolderJob {
                    id: file.id,
                    path: file.path.clone(),
                    name: shown_name(file),
                });
            }
        }
        jobs
    }

    /// The answer "cancel": the folders that waited for an answer leave the strip. Returns
    /// how many.
    pub fn answer_cancel(&mut self) -> usize {
        let before = self.files.len();
        self.files.retain(|f| {
            f.folder
                .as_ref()
                .is_none_or(|folder| folder.stage != FolderStage::Asking)
        });
        let gone = before - self.files.len();
        if gone > 0 {
            self.changed();
        }
        gone
    }

    /// The files of folder `id` take its place, in the order of the walk. They need no
    /// further look: their sizes are known. Returns how many were added; a folder that turns
    /// out to hold nothing (any more) just leaves.
    pub fn folder_listed(&mut self, id: u64, files: Vec<ListedFile>) -> usize {
        let Some(at) = self
            .files
            .iter()
            .position(|f| f.id == id && f.folder.is_some())
        else {
            return 0;
        };
        let added = files.len();
        let mut inside = Vec::with_capacity(added);
        for file in files {
            let name = name_of(&file.path);
            self.next_id += 1;
            inside.push(StripFile {
                id: self.next_id,
                kind: kind_for_file_name(&name),
                inspected: false,
                size: Some(file.size),
                sized: true,
                picture: file.picture,
                pasted: false,
                refusal: None,
                part: None,
                as_file: false,
                folder: None,
                path: file.path,
                name,
            });
        }
        self.files.splice(at..=at, inside);
        self.changed();
        added
    }

    /// How far the packing of folder `id` is. Returns whether the row reads differently.
    pub fn folder_packing(&mut self, id: u64, done: u64) -> bool {
        match self.folder_mut(id) {
            Some(folder) if folder.stage == FolderStage::Packing => {
                let total = folder.count.bytes;
                let done = done.min(total);
                // (Shown in hundredths: less than that is no change.)
                let changed = permille(done, total) / 10 != permille(folder.done, total) / 10;
                folder.done = done;
                changed
            }
            _ => false,
        }
    }

    /// The archive of folder `id` takes its place: a file Kourier made (it is deleted when
    /// it is no longer needed, like a pasted picture), `size` bytes. `false` when the entry
    /// is gone: the caller then deletes the archive.
    pub fn folder_packed(&mut self, id: u64, archive: PathBuf, size: u64) -> bool {
        let Some(file) = self
            .files
            .iter_mut()
            .find(|f| f.id == id && f.folder.is_some())
        else {
            return false;
        };
        let name = name_of(&archive);
        file.kind = kind_for_file_name(&name);
        file.name = name;
        file.path = archive;
        file.folder = None;
        file.size = Some(size);
        file.sized = true;
        file.picture = false;
        file.pasted = true;
        file.inspected = false;
        self.changed();
        true
    }

    /// The other strip's entries join this one's, behind them (two chats became one, and
    /// each had files waiting). The entries keep their identities: every strip numbers its
    /// own from a range of its own ([`Self::starting_at`]).
    pub fn absorb(&mut self, mut other: FileStrip) {
        if other.files.is_empty() {
            return;
        }
        self.files.append(&mut other.files);
        self.changed();
    }

    /// The entries that were not looked at for their size yet: `(identity, path)`.
    pub fn unsized_files(&self) -> Vec<(u64, PathBuf)> {
        self.files
            .iter()
            .filter(|f| !f.sized)
            .map(|f| (f.id, f.path.clone()))
            .collect()
    }

    /// The files a send takes now: every file the core did not refuse, in order.
    pub fn sendable(&self) -> Vec<PathBuf> {
        self.files
            .iter()
            .filter(|f| f.goes())
            .map(|f| f.path.clone())
            .collect()
    }

    /// For each file of [`Self::sendable`], in the same order: it goes as the file it is.
    pub fn sendable_as_file(&self) -> Vec<bool> {
        self.files
            .iter()
            .filter(|f| f.goes())
            .map(|f| f.as_file)
            .collect()
    }

    /// A send took the files that can go: they leave the strip, the refused ones stay with
    /// their marks, and so do folders that are not files or an archive yet. Returns the
    /// files that left.
    pub fn take_sendable(&mut self) -> Vec<StripFile> {
        let (sent, kept): (Vec<StripFile>, Vec<StripFile>) = std::mem::take(&mut self.files)
            .into_iter()
            .partition(StripFile::goes);
        self.files = kept;
        if !sent.is_empty() {
            self.changed();
            // What stays is refused: there is nothing left to plan.
            if !self.files.is_empty() {
                self.planned = Some(self.serial);
                self.messages = 0;
                self.lead_text = false;
                self.caption_messages = 0;
                self.reply_unsendable = false;
            }
        }
        sent
    }

    /// Whether a send has anything to take.
    pub fn can_send(&self) -> bool {
        self.files.iter().any(StripFile::goes) && !(self.planned() && self.reply_unsendable)
    }

    /// The rows of the list, in order.
    pub fn rows(&self) -> Vec<FileRow> {
        let planned = self.planned();
        let split = planned && self.messages + self.caption_messages > 1;
        let mut rows: Vec<FileRow> = self
            .files
            .iter()
            .map(|f| {
                if let Some(folder) = &f.folder {
                    return folder_row(f, folder);
                }
                // "Not a file" from the core before the look at the entry is in: it may be
                // a folder, which is not refused but asked about. Nothing is said until then.
                let refusal = f.refusal.filter(|r| f.sized || *r != FileRefusal::NotAFile);
                FileRow {
                    id: f.id,
                    name: f.name.clone(),
                    kind: kind_name(f.kind),
                    icon: match (f.kind, refusal) {
                        // A name that hides what the file is gets the icon of what it is.
                        (AttachmentKind::Image | AttachmentKind::Gif, _) if f.inspected => {
                            "image-x-generic"
                        }
                        (AttachmentKind::Video, _) if f.inspected => "video-x-generic",
                        _ => icon_for_file_name(&f.name),
                    },
                    picture: f.picture && f.refusal.is_none(),
                    path: f.path.clone(),
                    size: f.size.map(file_size).unwrap_or_default(),
                    reason: refusal.map(refusal_text).unwrap_or_default(),
                    // The caption sent first takes the first numbers; the files follow.
                    part: match f.part {
                        Some(part) if split => {
                            u32::try_from(part + 1 + self.caption_messages).unwrap_or(u32::MAX)
                        }
                        _ => 0,
                    },
                    part_first: false,
                    part_last: false,
                    part_through: false,
                    can_original: f.can_be_original(),
                    original: f.as_file && f.can_be_original(),
                    note: if f.refusal.is_none() {
                        self.rules.note(&f.name)
                    } else {
                        String::new()
                    },
                    stage: "",
                    progress: 0,
                }
            })
            .collect();
        // Where each message begins and ends among the files that go (refused ones between
        // them are passed over).
        let mut last: Option<usize> = None;
        for i in 0..rows.len() {
            if rows[i].part == 0 {
                continue;
            }
            let joins = last.is_some_and(|l| rows[l].part == rows[i].part);
            rows[i].part_first = !joins;
            rows[i].part_last = true;
            if let (true, Some(l)) = (joins, last) {
                rows[l].part_last = false;
                // What stands between two files of one message is passed by their line.
                for between in &mut rows[l + 1..i] {
                    between.part_through = true;
                }
            }
            last = Some(i);
        }
        rows
    }

    /// The line above the list.
    pub fn summary(&self) -> StripSummary {
        let folders = self.files.iter().filter(|f| f.folder.is_some()).count();
        let files = self.files.len() - folders;
        let refused = self.files.iter().filter(|f| f.refusal.is_some()).count();
        let planned = self.planned() && files > 0;
        let messages = if planned { self.messages } else { 0 };
        // The total once every file was looked at (what is no file adds nothing; a folder
        // that waits is not counted before it is files or an archive).
        let bytes = self.files.iter().all(|f| f.sized).then(|| {
            self.files
                .iter()
                .fold(0u64, |sum, f| sum.saturating_add(f.size.unwrap_or(0)))
        });
        let mut text = match (files, folders) {
            (_, 0) => trnc(
                "@info summary of the composer's list, %1 how many files",
                "%1 file",
                "%1 files",
                files as u64,
            ),
            (0, _) => trnc(
                "@info summary of the composer's list, %1 how many folders",
                "%1 folder",
                "%1 folders",
                folders as u64,
            ),
            _ => format!(
                "{}, {}",
                trnc(
                    "@info summary of the composer's list, %1 how many files",
                    "%1 file",
                    "%1 files",
                    files as u64
                ),
                trnc(
                    "@info summary of the composer's list, %1 how many folders",
                    "%1 folder",
                    "%1 folders",
                    folders as u64
                )
            ),
        };
        let caption_messages = if planned { self.caption_messages } else { 0 };
        let total = messages + caption_messages;
        if total > 0 && (total > 1 || refused > 0) {
            text.push_str(", ");
            text.push_str(&trnc(
                "@info summary of the composer's list, %1 how many messages",
                "%1 message",
                "%1 messages",
                total as u64,
            ));
        }
        if let Some(bytes) = bytes.filter(|_| files > 0) {
            text.push_str(" · ");
            text.push_str(&file_size(bytes));
        }
        let mut hints: Vec<String> = Vec::new();
        if refused > 0 {
            // (Two messages, not a plural: no number is shown, and in many languages the
            // form for one is also the form for 21.)
            hints.push(if refused == files && files == 1 {
                trc(
                    "@info:tooltip why the one file cannot be sent",
                    "This file cannot be sent here.",
                )
                .into_owned()
            } else if refused == files {
                trc(
                    "@info:tooltip why the files, two or more, cannot be sent",
                    "None of these files can be sent here.",
                )
                .into_owned()
            } else {
                trnc(
                    "@info:tooltip why some files cannot be sent, %1 how many files",
                    "%1 file cannot be sent here and stays in this list.",
                    "%1 files cannot be sent here and stay in this list.",
                    refused as u64,
                )
            });
        }
        if planned && self.reply_unsendable {
            hints.push(
                trc(
                    "@info:tooltip about sending files with a reply",
                    "A reply with files needs a text here: write one, or drop the reply.",
                )
                .into_owned(),
            );
        } else if caption_messages > 1 {
            hints.push(trnc(
                "@info:tooltip about a long text sent with files, %1 how many messages",
                "Your text is too long for one message: it is sent first, as %1 message.",
                "Your text is too long for one message: it is sent first, as %1 messages.",
                caption_messages as u64,
            ));
        } else if planned && self.lead_text {
            hints.push(
                trc(
                    "@info:tooltip about sending files with a text",
                    "Your text is sent first, as a message of its own.",
                )
                .into_owned(),
            );
        }
        StripSummary {
            files,
            folders,
            refused,
            messages,
            lead_text: planned && self.lead_text,
            caption_messages,
            reply_unsendable: planned && self.reply_unsendable,
            bytes: bytes.filter(|_| files > 0),
            text: if files + folders == 0 {
                String::new()
            } else {
                text
            },
            hint: hints.join(" "),
        }
    }
}

/// A share of a whole in thousandths (a whole of nothing is done).
fn permille(done: u64, total: u64) -> u16 {
    if total == 0 {
        return 1000;
    }
    let share = u128::from(done.min(total)) * 1000 / u128::from(total);
    u16::try_from(share).unwrap_or(1000)
}

/// The name an entry is called by: its file name, or "Folder" for a path without one (`/`).
fn shown_name(file: &StripFile) -> String {
    if file.name.is_empty() {
        trc("@item:intext name of a folder without a name", "Folder").into_owned()
    } else {
        file.name.clone()
    }
}

/// The file name of the archive of a folder called `folder`.
fn archive_name(folder: &str) -> String {
    format!("{folder}.zip")
}

/// The row of a folder: what it holds where a file has its size, what is happening to it
/// where a file has its note.
fn folder_row(file: &StripFile, folder: &FolderEntry) -> FileRow {
    let count = &folder.count;
    let holds = if count.files == 0 && folder.stage == FolderStage::Counting {
        String::new()
    } else {
        format!(
            "{} · {}",
            trnc(
                "@info:status a folder in the composer's list, %1 how many files",
                "%1 file",
                "%1 files",
                count.files
            ),
            file_size(count.bytes)
        )
    };
    let progress = match folder.stage {
        FolderStage::Packing => permille(folder.done, count.bytes),
        _ => 0,
    };
    let note = match folder.stage {
        FolderStage::Counting => trc(
            "@info:status a folder in the composer's list",
            "Looking inside…",
        )
        .into_owned(),
        FolderStage::Asking => trc(
            "@info:status a folder in the composer's list",
            "Folder: waiting for an answer",
        )
        .into_owned(),
        FolderStage::Listing => trc(
            "@info:status a folder in the composer's list",
            "Adding the files…",
        )
        .into_owned(),
        FolderStage::Packing => trfc(
            "@info:status a folder in the composer's list, %1 progress in percent",
            "Packing… %1%",
            &[&(progress / 10).to_string()],
        ),
    };
    FileRow {
        id: file.id,
        name: shown_name(file),
        kind: "folder",
        icon: "folder",
        picture: false,
        path: file.path.clone(),
        size: holds,
        reason: String::new(),
        part: 0,
        part_first: false,
        part_last: false,
        part_through: false,
        can_original: false,
        original: false,
        note,
        stage: folder.stage.as_str(),
        progress,
    }
}

impl StripFile {
    /// A send takes it: a file the network did not refuse. A folder is not a file yet.
    fn goes(&self) -> bool {
        self.refusal.is_none() && self.folder.is_none()
    }

    /// A picture or a video that the network can take: it has the choice of going as the
    /// file it is.
    fn can_be_original(&self) -> bool {
        self.goes()
            && matches!(
                self.kind,
                AttachmentKind::Image | AttachmentKind::Gif | AttachmentKind::Video
            )
    }
}

/// What a kind is called in "this chat does not take …".
fn kind_plural(kind: AttachmentKind) -> std::borrow::Cow<'static, str> {
    match kind {
        AttachmentKind::Image => trc("@item:intext kind of file, plural", "pictures"),
        AttachmentKind::Gif => trc("@item:intext kind of file, plural", "GIFs"),
        AttachmentKind::Video => trc("@item:intext kind of file, plural", "videos"),
        AttachmentKind::Audio => trc("@item:intext kind of file, plural", "sound files"),
        AttachmentKind::Voice => trc("@item:intext kind of file, plural", "voice messages"),
        AttachmentKind::File => trc("@item:intext kind of file, plural", "files of this kind"),
    }
}

/// Why a file cannot be sent, as the strip says it. None of these reasons is Kourier's own:
/// the first two are about the file, the others are what the network or the chat takes.
pub fn refusal_text(reason: FileRefusal) -> String {
    match reason {
        FileRefusal::Unreadable => {
            trc("@info:status why a file cannot be sent", "Cannot be read").into_owned()
        }
        // (A folder never gets here: it is asked about, not refused. What is left is what
        // is neither: a socket, a pipe, a device.)
        FileRefusal::NotAFile => {
            trc("@info:status why a file cannot be sent", "Not a file").into_owned()
        }
        FileRefusal::TooLarge { max, chat: true } => trfc(
            "@info:status why a file cannot be sent, %1 the size limit",
            "Too large for this chat (up to %1)",
            &[&file_size(max)],
        ),
        FileRefusal::TooLarge { max, chat: false } => trfc(
            "@info:status why a file cannot be sent, %1 the size limit",
            "Too large for this network (up to %1)",
            &[&file_size(max)],
        ),
        FileRefusal::KindNotAccepted { kind } => trfc(
            "@info:status why a file cannot be sent, %1 a kind of file in the plural, like pictures",
            "This chat does not take %1",
            &[&kind_plural(kind)],
        ),
    }
}

/// What a walk left out, as the end of a notice: " Left out: 2 hidden items, 1 link." (with
/// the leading space), or nothing.
fn left_out(count: &FolderCount) -> String {
    let mut parts: Vec<String> = Vec::new();
    if count.hidden > 0 {
        parts.push(trnc(
            "@info what a folder walk left out, part of a list",
            "%1 hidden item",
            "%1 hidden items",
            count.hidden,
        ));
    }
    if count.links > 0 {
        parts.push(trnc(
            "@info what a folder walk left out, part of a list",
            "%1 link",
            "%1 links",
            count.links,
        ));
    }
    if count.other > 0 {
        parts.push(trnc(
            "@info what a folder walk left out, part of a list",
            "%1 item that could not be read",
            "%1 items that could not be read",
            count.other,
        ));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(
            " {}",
            trfc(
                "@info, %1 a comma-separated list of what was left out",
                "Left out: %1.",
                &[&parts.join(", ")]
            )
        )
    }
}

/// The notice once the files of a folder called `name` joined the list: how many, and what
/// the walk left out (`count`).
pub fn folder_added_notice(name: &str, added: usize, count: &FolderCount) -> String {
    let text = if added == 0 {
        trfc(
            "@info:status notice about a folder, %1 the name of the folder",
            "“%1” holds no files to send.",
            &[name],
        )
    } else {
        // TRANSLATORS: the two parts are joined with a space: "3 files were added from
        // “Photos”."
        format!(
            "{} {}",
            trnc(
                "@info:status notice about a folder, the first part of a sentence, %1 how many files",
                "%1 file was added from",
                "%1 files were added from",
                added as u64
            ),
            trfc(
                "@info:status notice about a folder, the end of a sentence, %1 the name of the folder",
                "“%1”.",
                &[name]
            )
        )
    };
    format!("{text}{}", left_out(count))
}

/// The notice for a folder called `name` that holds nothing that could be sent.
pub fn folder_empty_notice(name: &str, count: &FolderCount) -> String {
    folder_added_notice(name, 0, count)
}

/// The notice for a folder called `name` that could not be read at all.
pub fn folder_unreadable_notice(name: &str) -> String {
    trfc(
        "@info:status notice about a folder, %1 the name of the folder",
        "“%1” could not be read.",
        &[name],
    )
}

/// The notice for a folder called `name` whose archive could not be written; `full`: the
/// disk has no room for it.
pub fn folder_unpacked_notice(name: &str, full: bool) -> String {
    if full {
        trfc(
            "@info:status notice about a folder, %1 the name of the folder",
            "“%1” could not be packed: there is not enough room on the disk.",
            &[name],
        )
    } else {
        trfc(
            "@info:status notice about a folder, %1 the name of the folder",
            "“%1” could not be packed.",
            &[name],
        )
    }
}

/// The notice for an archive that lacks `missing` files of its folder called `name` (they
/// went away or could not be read while it was packed); empty when it lacks none.
pub fn folder_packed_notice(name: &str, missing: u64) -> String {
    if missing == 0 {
        return String::new();
    }
    // TRANSLATORS: the two parts are joined with a space: "2 files could not be read and are
    // not in “Photos.zip”."
    format!(
        "{} {}",
        trnc(
            "@info:status notice about a folder, the first part of a sentence, %1 how many files",
            "%1 file could not be read and is not in",
            "%1 files could not be read and are not in",
            missing
        ),
        trfc(
            "@info:status notice about a folder, the end of a sentence, %1 the name of the folder",
            "“%1”.",
            &[&archive_name(name)]
        )
    )
}

/// The notice after a send that left files behind: how many, and that the rest went. `here`:
/// the chat of that send is the open one, so its list shows the files with their reasons;
/// else (the user went on to another chat while the core was still copying) they wait in
/// that chat's list, which shows again when the chat is opened.
pub fn refused_notice(refused: usize, sent: usize, here: bool) -> String {
    match (refused, sent, here) {
        (0, _, _) => String::new(),
        // (One file or several without their number: two messages, not a plural; in many
        // languages the form for one is also the form for 21.)
        (1, 0, true) => trc(
            "@info:status notice after a send that left the one file behind",
            "This file cannot be sent here. The list says why.",
        )
        .into_owned(),
        (_, 0, true) => trc(
            "@info:status notice after a send that left the files, two or more, behind",
            "None of these files can be sent here. The list says why.",
        )
        .into_owned(),
        (_, _, true) => trnc(
            "@info:status notice after a send that left files behind, %1 how many files",
            "%1 file could not be sent and is still in the list, which says why. The others are on their way.",
            "%1 files could not be sent and are still in the list, which says why. The others are on their way.",
            refused as u64,
        ),
        (1, 0, false) => trc(
            "@info:status notice after a send that left the one file behind in another chat",
            "A file for another chat could not be sent. It waits in that chat.",
        )
        .into_owned(),
        (_, 0, false) => trc(
            "@info:status notice after a send that left the files, two or more, behind in another chat",
            "The files for another chat could not be sent. They wait in that chat.",
        )
        .into_owned(),
        (_, _, false) => trnc(
            "@info:status notice after a send that left files behind in another chat, %1 how many files",
            "%1 file for another chat could not be sent and waits there. The others are on their way.",
            "%1 files for another chat could not be sent and wait there. The others are on their way.",
            refused as u64,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kourier_core::facade::RefusedFile;
    use kourier_model::FilePlan;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names
            .iter()
            .map(|n| PathBuf::from(format!("/t/{n}")))
            .collect()
    }

    fn preview(
        kinds: &[Option<AttachmentKind>],
        refused: &[(usize, FileRefusal)],
        messages: &[&[usize]],
    ) -> FilesPreview {
        FilesPreview {
            kinds: kinds.to_vec(),
            refused: refused
                .iter()
                .map(|(index, reason)| RefusedFile {
                    index: *index,
                    reason: *reason,
                })
                .collect(),
            plan: FilePlan {
                lead_text: false,
                caption_messages: 0,
                messages: messages.iter().map(|m| m.to_vec()).collect(),
                reply_unsendable: false,
            },
        }
    }

    use AttachmentKind::{File, Image, Video};

    #[test]
    fn any_number_of_files_of_any_kinds_is_taken_in_one_go() {
        let mut strip = FileStrip::new();
        // More than Desktop's 32, mixed kinds, a duplicate: nothing is refused here.
        let mut names: Vec<String> = (0..300).map(|i| format!("f{i}.jpg")).collect();
        names.push("notes.pdf".into());
        names.push("f0.jpg".into());
        let all: Vec<PathBuf> = names
            .iter()
            .map(|n| PathBuf::from(format!("/t/{n}")))
            .collect();
        assert_eq!(strip.add(all.clone(), false), 302);
        assert_eq!(strip.len(), 302);
        assert_eq!(strip.sendable(), all);
        let rows = strip.rows();
        assert_eq!(rows.len(), 302);
        // Before the core looked: kinds by name, no previews, no split shown.
        assert_eq!(rows[0].kind, "image");
        assert_eq!(rows[300].kind, "file");
        assert!(
            rows.iter()
                .all(|r| !r.picture && r.part == 0 && r.reason.is_empty())
        );
        let summary = strip.summary();
        assert_eq!(summary.text, "302 files");
        assert_eq!(
            (summary.files, summary.messages, summary.refused),
            (302, 0, 0)
        );
        assert!(!strip.planned());
        // Ids are unique: the list model keys on them.
        let mut ids: Vec<u64> = rows.iter().map(|r| r.id).collect();
        ids.dedup();
        assert_eq!(ids.len(), 302);
    }

    #[test]
    fn the_plan_shows_the_split_and_marks_what_cannot_go() {
        let mut strip = FileStrip::new();
        strip.add(
            paths(&[
                "a.jpg", "b.jpg", "c.mp4", "d.pdf", "huge.zip", "e.jpg", "socket",
            ]),
            false,
        );
        let request = strip.plan_request().expect("a plan is needed");
        assert_eq!(request.paths.len(), 7);
        assert!(!request.caption && !request.reply);
        let too_large = FileRefusal::TooLarge {
            max: 100_000_000,
            chat: false,
        };
        let answer = PlanAnswer {
            serial: request.serial,
            preview: preview(
                &[
                    Some(Image),
                    Some(Image),
                    Some(Video),
                    Some(File),
                    Some(File),
                    // The name said picture; the content says otherwise.
                    Some(File),
                    None,
                ],
                &[(4, too_large), (6, FileRefusal::NotAFile)],
                &[&[0, 1, 2], &[3], &[5]],
            ),
        };
        assert!(strip.apply_plan(&answer));
        assert!(strip.planned() && strip.plan_request().is_none());
        let rows = strip.rows();
        let parts: Vec<u32> = rows.iter().map(|r| r.part).collect();
        assert_eq!(parts, [1, 1, 1, 2, 0, 3, 0]);
        let firsts: Vec<bool> = rows.iter().map(|r| r.part_first).collect();
        assert_eq!(firsts, [true, false, false, true, false, true, false]);
        let lasts: Vec<bool> = rows.iter().map(|r| r.part_last).collect();
        assert_eq!(lasts, [false, false, true, true, false, true, false]);
        // No preview yet: nobody has looked into the files.
        assert!(rows.iter().all(|r| !r.picture));
        assert_eq!(rows[5].kind, "file");
        assert_eq!(
            rows[4].reason,
            "Too large for this network (up to 100.0 MB)"
        );
        // What the core calls no file may be a folder, which is asked about and not refused:
        // nothing is said of it before the look at the entry is in.
        assert_eq!(rows[6].reason, "");
        let socket = strip.files()[6].id;
        assert!(strip.set_facts(&[(
            socket,
            FileFacts {
                size: None,
                picture: false,
                folder: false
            }
        )]));
        assert!(strip.planned(), "a look changes no plan");
        let rows = strip.rows();
        assert_eq!(rows[6].reason, "Not a file");
        let summary = strip.summary();
        assert_eq!(summary.text, "7 files, 3 messages");
        assert_eq!((summary.messages, summary.refused), (3, 2));
        assert!(
            summary.hint.starts_with("2 files cannot be sent here"),
            "{}",
            summary.hint
        );
        // A send takes what can go; the refused files stay, marked, and block nothing.
        assert!(strip.can_send());
        assert_eq!(
            strip.sendable(),
            paths(&["a.jpg", "b.jpg", "c.mp4", "d.pdf", "e.jpg"])
        );
        let sent = strip.take_sendable();
        assert_eq!(sent.len(), 5);
        let left = strip.rows();
        assert_eq!(left.len(), 2);
        assert!(left.iter().all(|r| !r.reason.is_empty() && r.part == 0));
        assert!(!strip.can_send());
        assert!(strip.plan_request().is_none(), "nothing left to plan");
        assert_eq!(
            strip.summary().hint,
            "None of these files can be sent here."
        );
    }

    #[test]
    fn an_answer_for_an_earlier_selection_is_dropped() {
        let mut strip = FileStrip::new();
        strip.add(paths(&["a.jpg", "b.jpg"]), false);
        let first = strip.plan_request().expect("request");
        strip.add(paths(&["c.jpg"]), false);
        let stale = PlanAnswer {
            serial: first.serial,
            preview: preview(&[Some(Image), Some(Image)], &[], &[&[0, 1]]),
        };
        assert!(!strip.apply_plan(&stale));
        assert!(!strip.planned());
        let second = strip.plan_request().expect("request");
        assert_eq!(second.paths.len(), 3);
        assert_ne!(second.serial, first.serial);
        // Removing a file, and the caption or reply changing, need a new plan too.
        let fresh = PlanAnswer {
            serial: second.serial,
            preview: preview(&[Some(Image); 3], &[], &[&[0, 1, 2]]),
        };
        assert!(strip.apply_plan(&fresh));
        assert_eq!(strip.summary().text, "3 files");
        assert!(strip.set_context(true, false));
        assert!(!strip.planned());
        // The same files, but the answer was made before there was a caption.
        assert!(
            !strip.apply_plan(&fresh),
            "an answer from before the caption"
        );
        assert!(!strip.planned());
        assert!(strip.plan_request().expect("request").caption);
        assert!(!strip.set_context(true, false), "unchanged");
        assert!(strip.remove(0).is_some());
        assert_eq!(strip.plan_request().expect("request").paths.len(), 2);
        // The core cannot say (the chat went away): no split is shown and nothing waits.
        let asked = strip.plan_request().expect("request");
        assert!(
            !strip.plan_unavailable(asked.serial + 1),
            "another selection"
        );
        assert!(strip.plan_unavailable(asked.serial));
        assert!(strip.planned() && strip.plan_request().is_none());
        assert_eq!(strip.summary().text, "2 files");
        assert!(strip.rows().iter().all(|r| r.part == 0));
        assert!(strip.can_send(), "the send itself says what is wrong");
        strip.clear();
        assert!(strip.planned() && strip.plan_request().is_none());
        assert_eq!(strip.summary(), StripSummary::default());
        // Without files the flags change nothing to plan.
        assert!(!strip.set_context(false, true));
    }

    #[test]
    fn a_caption_sent_first_counts_as_a_message() {
        let mut strip = FileStrip::new();
        strip.add(paths(&["memo.ogg", "a.jpg"]), false);
        strip.set_context(true, true);
        let request = strip.plan_request().expect("request");
        let mut p = preview(
            &[Some(AttachmentKind::Voice), Some(Image)],
            &[],
            &[&[0], &[1]],
        );
        p.plan.lead_text = true;
        assert!(strip.apply_plan(&PlanAnswer {
            serial: request.serial,
            preview: p.clone(),
        }));
        let summary = strip.summary();
        assert_eq!(summary.text, "2 files, 3 messages");
        assert!(summary.lead_text);
        assert_eq!(
            summary.hint,
            "Your text is sent first, as a message of its own."
        );
        // The text is message 1, so the files are 2 and 3.
        let parts: Vec<u32> = strip.rows().iter().map(|r| r.part).collect();
        assert_eq!(parts, [2, 3]);
        // A quote the files cannot carry and no text for it: the send cannot go as it is.
        strip.set_context(false, true);
        let request = strip.plan_request().expect("request");
        p.plan.lead_text = false;
        p.plan.reply_unsendable = true;
        assert!(strip.apply_plan(&PlanAnswer {
            serial: request.serial,
            preview: p,
        }));
        assert!(!strip.can_send());
        assert!(strip.summary().hint.contains("needs a text"));
    }

    #[test]
    fn sizes_arrive_later_and_add_up() {
        let mut strip = FileStrip::new();
        strip.add(paths(&["a.jpg", "b.pdf", "c.bin"]), false);
        let wanted = strip.unsized_files();
        assert_eq!(wanted.len(), 3);
        assert_eq!(strip.summary().bytes, None);
        let fact = |size: Option<u64>, picture: bool| FileFacts {
            size,
            picture,
            folder: false,
        };
        // One file went away before its size came in.
        let removed = strip.remove(1).expect("b");
        assert!(strip.set_facts(&[
            (wanted[0].0, fact(Some(1_200_000), true)),
            (removed.id, fact(Some(5), false)),
            // Named like any file, but a picture by its first bytes.
            (wanted[2].0, fact(Some(800_000), true)),
        ]));
        let rows = strip.rows();
        assert_eq!(rows[0].size, "1.2 MB");
        assert_eq!(rows[1].size, "800.0 KB");
        // The preview follows the content, not the name.
        assert!(rows[0].picture && rows[1].picture);
        assert_eq!(strip.summary().text, "2 files · 2.0 MB");
        assert!(strip.unsized_files().is_empty());
        assert!(
            !strip.set_facts(&[(wanted[0].0, fact(Some(1_200_000), true))]),
            "no change"
        );
        // A folder has no size: it is not asked about again and the total does not wait
        // for it.
        strip.add(paths(&["folder"]), false);
        assert_eq!(strip.summary().bytes, None, "not looked at yet");
        let wanted = strip.unsized_files();
        assert_eq!(wanted.len(), 1);
        assert!(strip.set_facts(&[(wanted[0].0, fact(None, false))]));
        assert!(strip.unsized_files().is_empty());
        assert_eq!(strip.rows()[2].size, "");
        assert_eq!(strip.summary().text, "3 files · 2.0 MB");
    }

    #[test]
    fn files_a_send_was_refused_for_come_back_marked() {
        let mut strip = FileStrip::new();
        strip.add(paths(&["later.jpg"]), true);
        let handed = |name: &str, pasted: bool, as_file: bool| HandedFile {
            path: PathBuf::from(format!("/t/{name}")),
            pasted,
            as_file,
        };
        strip.restore_refused(vec![
            (handed("gone.txt", false, false), FileRefusal::Unreadable),
            (
                handed("clip.bin", true, true),
                FileRefusal::KindNotAccepted { kind: Video },
            ),
        ]);
        let rows = strip.rows();
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["gone.txt", "clip.bin", "later.jpg"]);
        assert_eq!(rows[0].reason, "Cannot be read");
        assert_eq!(rows[1].reason, "This chat does not take videos");
        assert_eq!(rows[1].kind, "video");
        assert!(strip.files()[1].pasted && strip.files()[2].pasted);
        // The choice made for a file comes back with it (it shows again once it can go).
        assert!(strip.files()[1].as_file && !strip.files()[0].as_file);
        assert!(
            !rows[1].original && !rows[1].can_original,
            "refused: no choice"
        );
        assert_eq!(strip.sendable(), paths(&["later.jpg"]));
        // Sizes find restored entries although they sit in front with newer ids.
        let wanted = strip.unsized_files();
        let facts: Vec<(u64, FileFacts)> = wanted
            .iter()
            .map(|(id, _)| {
                (
                    *id,
                    FileFacts {
                        size: Some(10),
                        picture: true,
                        folder: false,
                    },
                )
            })
            .collect();
        assert!(strip.set_facts(&facts));
        let rows = strip.rows();
        assert!(rows.iter().all(|r| r.size == "10 B"));
        // A file the network refuses is not previewed, whatever it is.
        assert_eq!(
            rows.iter().map(|r| r.picture).collect::<Vec<_>>(),
            [false, false, true]
        );
        assert_eq!(refused_notice(0, 3, true), "");
        assert_eq!(refused_notice(0, 3, false), "");
        assert!(refused_notice(2, 3, true).starts_with("2 files could not be sent"));
        assert!(refused_notice(1, 0, true).starts_with("This file cannot be sent here"));
        // For a chat that is not open the notice says where the files wait.
        for (refused, sent) in [(1, 0), (3, 0), (1, 5), (3, 5)] {
            let elsewhere = refused_notice(refused, sent, false);
            assert!(elsewhere.contains("another chat"), "{elsewhere}");
            assert!(elsewhere.contains("wait"), "{elsewhere}");
        }
        assert!(refused_notice(3, 5, false).starts_with("3 files for another chat"));
    }

    #[test]
    fn answers_for_one_chats_strip_never_fit_anothers() {
        // Two chats, each with one file waiting, added the same way.
        let mut first = FileStrip::starting_at(1 << 32);
        let mut second = FileStrip::starting_at(2 << 32);
        first.add(paths(&["a.jpg"]), false);
        second.add(paths(&["b.bin"]), false);
        let asked = first.plan_request().expect("request");
        let looked = first.unsized_files();
        assert_ne!(asked.serial, second.plan_request().expect("request").serial);
        assert_ne!(looked[0].0, second.unsized_files()[0].0);
        // The answers for the first chat arrive when the second one is open: neither the
        // plan nor, above all, "this file is a picture" (which decides whether the file
        // itself is handed to an image decoder) lands on the other chat's file.
        let answer = PlanAnswer {
            serial: asked.serial,
            preview: preview(&[Some(Image)], &[], &[&[0]]),
        };
        assert!(!second.apply_plan(&answer));
        assert!(!second.planned());
        let facts = [(
            looked[0].0,
            FileFacts {
                size: Some(1),
                picture: true,
                folder: false,
            },
        )];
        assert!(!second.set_facts(&facts));
        assert!(!second.rows()[0].picture);
        // Back in the first chat they still fit.
        assert!(first.apply_plan(&answer));
        assert!(first.set_facts(&facts));
        assert!(first.rows()[0].picture);
        // Plain strips (one composer, one strip) keep counting from zero.
        let mut plain = FileStrip::new();
        plain.add(paths(&["a.jpg"]), false);
        assert_eq!(plain.rows()[0].id, 1);
    }

    #[test]
    fn a_picture_or_a_video_can_go_as_the_file_it_is() {
        let mut strip = FileStrip::new();
        strip.add(paths(&["a.jpg", "b.jpg", "clip.mp4", "notes.pdf"]), false);
        let rows = strip.rows();
        assert_eq!(
            rows.iter().map(|r| r.can_original).collect::<Vec<_>>(),
            [true, true, true, false]
        );
        assert!(rows.iter().all(|r| !r.original));
        // A document has no such choice; an index outside the list is nothing.
        assert!(!strip.set_as_file(3, true));
        assert!(!strip.set_as_file(9, true));
        let before = strip.plan_request().expect("request").serial;
        assert!(strip.set_as_file(1, true));
        assert!(!strip.set_as_file(1, true), "unchanged");
        // The choice changes how the files pack: a new plan is asked for, with the flags.
        let request = strip.plan_request().expect("request");
        assert_ne!(request.serial, before);
        assert_eq!(request.as_file, [false, true, false, false]);
        // The core still says what the file holds (a picture), and packs it alone.
        assert!(strip.apply_plan(&PlanAnswer {
            serial: request.serial,
            preview: preview(
                &[Some(Image), Some(Image), Some(Video), Some(File)],
                &[],
                &[&[0], &[1], &[2], &[3]],
            ),
        }));
        let rows = strip.rows();
        assert!(rows[1].original && rows[1].can_original);
        assert_eq!(rows[1].kind, "image");
        assert_eq!(strip.sendable_as_file(), [false, true, false, false]);
        // A name that looked like a picture and is none loses the choice with the plan.
        strip.add(paths(&["fake.jpg"]), false);
        assert!(strip.set_as_file(4, true));
        let request = strip.plan_request().expect("request");
        assert!(strip.apply_plan(&PlanAnswer {
            serial: request.serial,
            preview: preview(
                &[
                    Some(Image),
                    Some(Image),
                    Some(Video),
                    Some(File),
                    Some(File)
                ],
                &[],
                &[&[0], &[1], &[2], &[3], &[4]],
            ),
        }));
        let rows = strip.rows();
        assert!(!rows[4].can_original && !rows[4].original);
        // The send hands the flags over in the order of the files that go.
        let sent = strip.take_sendable();
        assert_eq!(
            sent.iter().map(|f| f.as_file).collect::<Vec<_>>(),
            [false, true, false, false, true]
        );
    }

    #[test]
    fn a_long_caption_counts_every_message_it_becomes() {
        let mut strip = FileStrip::new();
        strip.add(paths(&["a.jpg", "b.jpg"]), false);
        // A caption of ordinary length: only that there is one matters.
        assert!(strip.set_caption(true, None, false));
        assert!(
            !strip.set_caption(true, None, false),
            "typing on changes nothing"
        );
        assert_eq!(strip.plan_request().expect("request").caption_text, None);
        // Longer than one message: the text itself decides the count.
        let long = "x".repeat(70_000);
        assert!(strip.set_caption(true, Some(long.clone()), false));
        let request = strip.plan_request().expect("request");
        assert_eq!(request.caption_text.as_deref(), Some(long.as_str()));
        let mut p = preview(&[Some(Image), Some(Image)], &[], &[&[0, 1]]);
        p.plan.lead_text = true;
        p.plan.caption_messages = 2;
        assert!(strip.apply_plan(&PlanAnswer {
            serial: request.serial,
            preview: p,
        }));
        let summary = strip.summary();
        assert_eq!(summary.text, "2 files, 3 messages");
        assert_eq!((summary.messages, summary.caption_messages), (1, 2));
        assert_eq!(
            summary.hint,
            "Your text is too long for one message: it is sent first, as 2 messages."
        );
        // The two texts are messages 1 and 2; the album is message 3.
        assert_eq!(
            strip.rows().iter().map(|r| r.part).collect::<Vec<_>>(),
            [3, 3]
        );
        // Every further change of a long caption is a new question.
        assert!(strip.set_caption(true, Some(format!("{long}y")), false));
        assert!(!strip.planned());
    }

    #[test]
    fn a_refused_file_between_two_of_one_message_does_not_break_their_line() {
        let mut strip = FileStrip::new();
        strip.add(
            paths(&["a.jpg", "gone.jpg", "folder", "b.jpg", "c.pdf"]),
            false,
        );
        let request = strip.plan_request().expect("request");
        assert!(strip.apply_plan(&PlanAnswer {
            serial: request.serial,
            preview: preview(
                &[Some(Image), None, None, Some(Image), Some(File)],
                &[(1, FileRefusal::Unreadable), (2, FileRefusal::NotAFile)],
                &[&[0, 3], &[4]],
            ),
        }));
        let rows = strip.rows();
        assert_eq!(
            rows.iter().map(|r| r.part).collect::<Vec<_>>(),
            [1, 0, 0, 1, 2]
        );
        // The line of message 1 runs from its first file through the two that stay behind
        // to its last; nothing passes the row after it.
        assert_eq!(
            rows.iter().map(|r| r.part_through).collect::<Vec<_>>(),
            [false, true, true, false, false]
        );
        assert!(rows[0].part_first && !rows[0].part_last);
        assert!(!rows[3].part_first && rows[3].part_last);
        // A refused file after the last file of a message is passed by nothing.
        let mut tail = FileStrip::new();
        tail.add(paths(&["a.jpg", "b.jpg", "folder"]), false);
        let request = tail.plan_request().expect("request");
        assert!(tail.apply_plan(&PlanAnswer {
            serial: request.serial,
            preview: preview(
                &[Some(Image), Some(Image), None],
                &[(2, FileRefusal::NotAFile)],
                &[&[0], &[1]],
            ),
        }));
        assert!(tail.rows().iter().all(|r| !r.part_through));
    }

    #[test]
    fn a_type_some_client_will_not_save_is_noted_and_still_sent() {
        let mut strip = FileStrip::new();
        strip.add(
            paths(&["tool.apk", "Setup.EXE", "notes.pdf", "exe", "folder.exe"]),
            false,
        );
        assert!(strip.rows().iter().all(|r| r.note.is_empty()));
        let rules = StripRules {
            hard_to_save: ["apk".to_owned(), "exe".to_owned()].into(),
            network: "Signal".into(),
        };
        assert!(strip.set_rules(rules.clone()));
        assert!(!strip.set_rules(rules), "unchanged");
        let request = strip.plan_request().expect("request");
        assert!(strip.apply_plan(&PlanAnswer {
            serial: request.serial,
            preview: preview(
                &[Some(File), Some(File), Some(File), Some(File), None],
                &[(4, FileRefusal::NotAFile)],
                &[&[0], &[1], &[2], &[3]],
            ),
        }));
        let rows = strip.rows();
        let note = "Some Signal apps cannot save this type of file";
        assert_eq!(rows[0].note, note);
        assert_eq!(rows[1].note, note, "whatever the case of the name");
        assert_eq!(rows[2].note, "");
        assert_eq!(rows[3].note, "", "a name is not an extension");
        assert_eq!(rows[4].note, "", "a file that stays behind has its reason");
        // A note refuses nothing: all four files go, and the summary has no warning.
        assert_eq!(strip.sendable().len(), 4);
        assert_eq!(strip.summary().refused, 1);
        assert!(!strip.summary().hint.contains("save"));
        // Another chat's network states nothing: the notes go.
        assert!(strip.set_rules(StripRules::default()));
        assert!(strip.rows().iter().all(|r| r.note.is_empty()));
    }

    fn is_folder(strip: &mut FileStrip, index: usize) -> u64 {
        let id = strip.files()[index].id;
        assert!(strip.set_facts(&[(
            id,
            FileFacts {
                size: None,
                picture: false,
                folder: true,
            },
        )]));
        id
    }

    fn count(files: u64, bytes: u64) -> FolderCount {
        FolderCount {
            files,
            bytes,
            ..FolderCount::default()
        }
    }

    fn listed(names: &[&str]) -> Vec<ListedFile> {
        names
            .iter()
            .map(|n| ListedFile {
                path: PathBuf::from(format!("/t/Raw footage/{n}")),
                size: 1_000,
                picture: n.ends_with(".png"),
            })
            .collect()
    }

    #[test]
    fn a_folder_is_asked_about_and_never_marked_as_a_file_that_cannot_go() {
        let mut strip = FileStrip::new();
        strip.add(paths(&["a.jpg", "Raw footage", "b.pdf"]), false);
        // Before anybody looked, all three are asked about as files.
        let first = strip.plan_request().expect("a plan");
        assert_eq!(first.paths.len(), 3);
        // The look says the second one is a folder: it is counted, and a plan is about the
        // two files only.
        let folder = is_folder(&mut strip, 1);
        assert_eq!(
            strip.take_new_folders(),
            [(folder, PathBuf::from("/t/Raw footage"))]
        );
        assert!(strip.take_new_folders().is_empty(), "handed out once");
        let request = strip.plan_request().expect("a new plan");
        assert_eq!(request.paths, paths(&["a.jpg", "b.pdf"]));
        assert_ne!(request.serial, first.serial);
        // The answer about the three (the core calls the folder "not a file") is stale.
        let stale = PlanAnswer {
            serial: first.serial,
            preview: preview(
                &[Some(Image), None, Some(File)],
                &[(1, FileRefusal::NotAFile)],
                &[&[0], &[2]],
            ),
        };
        assert!(!strip.apply_plan(&stale));
        // The answer about the two files numbers them 0 and 1: they stand first and third.
        let answer = PlanAnswer {
            serial: request.serial,
            preview: preview(&[Some(Image), Some(File)], &[], &[&[0], &[1]]),
        };
        assert!(strip.apply_plan(&answer));
        let rows = strip.rows();
        assert_eq!(rows.iter().map(|r| r.part).collect::<Vec<_>>(), [1, 0, 2]);
        assert_eq!(
            (
                rows[1].kind,
                rows[1].icon,
                rows[1].stage,
                rows[1].reason.as_str()
            ),
            ("folder", "folder", "counting", "")
        );
        assert_eq!(rows[1].note, "Looking inside…");
        assert_eq!(rows[1].size, "");
        assert!(!rows[1].can_original && !rows[1].picture);
        // No question while it is being counted; the count shows as it grows.
        assert_eq!(strip.folder_question(ArchiveRule::default()), None);
        assert!(strip.folder_counting(folder, count(40, 2_000_000)));
        assert!(!strip.folder_counting(folder, count(40, 2_000_000)));
        assert_eq!(strip.rows()[1].size, "40 files · 2.0 MB");
        assert!(strip.planned(), "a count changes no plan");
        // Counted: the question.
        let counted = FolderCount {
            files: 124,
            bytes: 3_200_000,
            hidden: 12,
            links: 3,
            other: 1,
        };
        assert_eq!(strip.folder_counted(folder, counted), Counted::Asking);
        let rows = strip.rows();
        assert_eq!(rows[1].stage, "asking");
        assert_eq!(rows[1].size, "124 files · 3.2 MB");
        let q = strip
            .folder_question(ArchiveRule {
                accepted: true,
                max: Some(100_000_000),
            })
            .expect("the question");
        assert_eq!(q.ids, [folder]);
        assert_eq!(q.title, "Send a Folder");
        assert_eq!(q.text, "“Raw footage” holds 124 files, 3.2 MB in all.");
        assert_eq!(
            q.skipped,
            "Left out: 12 hidden items, 3 links (links are not followed), 1 item that could not be read."
        );
        assert_eq!(q.add_label, "Add the Files Inside");
        assert_eq!(
            q.add_hint,
            "124 files join the list and are sent as they are."
        );
        assert_eq!(q.zip_label, "Send as One .zip");
        assert_eq!(
            q.zip_hint,
            "Packed into one file, “Raw footage.zip”, of about 3.2 MB."
        );
        assert!(q.can_zip && !q.large);
        assert_eq!((q.files, q.bytes), (124, 3_200_000));
        // The line above the list names both; only files count as files.
        let summary = strip.summary();
        assert_eq!(summary.text, "2 files, 1 folder, 2 messages");
        assert_eq!((summary.files, summary.folders, summary.refused), (2, 1, 0));
        assert_eq!(summary.hint, "");
        // A send takes the files; the folder stays where it is, undecided, and is neither
        // sent nor called a file that could not go.
        assert!(strip.can_send());
        assert_eq!(strip.sendable(), paths(&["a.jpg", "b.pdf"]));
        assert_eq!(strip.sendable_as_file(), [false, false]);
        assert_eq!(strip.take_sendable().len(), 2);
        assert_eq!(strip.len(), 1);
        assert!(!strip.can_send());
        assert!(strip.planned() && strip.plan_request().is_none());
        assert_eq!(strip.summary().text, "1 folder");
        assert_eq!(strip.summary().refused, 0);
        assert!(strip.folder_question(ArchiveRule::default()).is_some());
        // "Cancel": it leaves.
        assert_eq!(strip.answer_cancel(), 1);
        assert!(strip.is_empty());
        assert_eq!(strip.folder_question(ArchiveRule::default()), None);
        assert_eq!(strip.summary().text, "");
    }

    #[test]
    fn the_files_inside_take_the_folders_place() {
        let mut strip = FileStrip::new();
        strip.add(paths(&["first.jpg", "Raw footage", "last.pdf"]), false);
        let folder = is_folder(&mut strip, 1);
        strip.take_new_folders();
        assert_eq!(
            strip.folder_counted(folder, count(3, 3_000)),
            Counted::Asking
        );
        let jobs = strip.answer_add();
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            (jobs[0].id, jobs[0].path.clone(), jobs[0].name.as_str()),
            (folder, PathBuf::from("/t/Raw footage"), "Raw footage")
        );
        assert!(strip.answer_add().is_empty(), "answered once");
        assert_eq!(strip.rows()[1].stage, "listing");
        assert_eq!(strip.rows()[1].note, "Adding the files…");
        assert_eq!(strip.folder_question(ArchiveRule::default()), None);
        let before = strip.serial();
        // The walk's files, in its order, stand where the folder stood.
        assert_eq!(
            strip.folder_listed(folder, listed(&["a.png", "sub/b.csv", "sub/c.png"])),
            3
        );
        let names: Vec<&str> = strip.files().iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["first.jpg", "a.png", "b.csv", "c.png", "last.pdf"]);
        assert_ne!(strip.serial(), before, "a new plan is needed");
        assert_eq!(strip.plan_request().expect("a plan").paths.len(), 5);
        // They need no look of their own: size and "is a picture" came with the walk.
        let inside = &strip.files()[1..4];
        assert!(inside.iter().all(|f| f.sized && f.size == Some(1_000)));
        assert!(inside.iter().all(|f| !f.pasted && f.folder.is_none()));
        assert_eq!(
            inside.iter().map(|f| f.picture).collect::<Vec<_>>(),
            [true, false, true]
        );
        let unsized_now: Vec<PathBuf> = strip.unsized_files().into_iter().map(|(_, p)| p).collect();
        assert_eq!(unsized_now, paths(&["first.jpg", "last.pdf"]));
        assert_eq!(strip.summary().folders, 0);
        // A second answer for the same folder finds nothing to replace.
        assert_eq!(strip.folder_listed(folder, listed(&["late.png"])), 0);
        assert_eq!(strip.len(), 5);
        // A folder whose files went away before they were listed just leaves.
        strip.add(paths(&["Emptied"]), false);
        let emptied = is_folder(&mut strip, 5);
        assert_eq!(strip.folder_counted(emptied, count(2, 10)), Counted::Asking);
        strip.answer_add();
        assert_eq!(strip.folder_listed(emptied, Vec::new()), 0);
        assert_eq!(strip.len(), 5);
    }

    #[test]
    fn the_archive_takes_the_folders_place() {
        let mut strip = FileStrip::new();
        strip.add(paths(&["first.jpg", "Raw footage"]), false);
        let folder = is_folder(&mut strip, 1);
        strip.take_new_folders();
        assert_eq!(
            strip.folder_counted(folder, count(124, 4_000_000)),
            Counted::Asking
        );
        let jobs = strip.answer_zip();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "Raw footage");
        let row = &strip.rows()[1];
        assert_eq!((row.stage, row.progress), ("packing", 0));
        assert_eq!(row.note, "Packing… 0%");
        // Progress shows in whole percent; less is no change of the row.
        let serial = strip.serial();
        assert!(!strip.folder_packing(folder, 10_000));
        assert!(strip.folder_packing(folder, 1_800_000));
        let row = &strip.rows()[1];
        assert_eq!(row.progress, 450);
        assert_eq!(row.note, "Packing… 45%");
        assert_eq!(row.size, "124 files · 4.0 MB");
        assert_eq!(strip.serial(), serial, "packing changes no plan");
        // Nothing of it is sent meanwhile.
        assert_eq!(strip.sendable(), paths(&["first.jpg"]));
        // Done: one file in the folder's place, made by Kourier (deleted when it is no
        // longer needed), its size known.
        let archive = PathBuf::from("/cache/outgoing/ab12/Raw footage.zip");
        assert!(strip.folder_packed(folder, archive.clone(), 1_234_567));
        let file = &strip.files()[1];
        assert_eq!(file.name, "Raw footage.zip");
        assert_eq!(
            (file.path.clone(), file.size, file.sized),
            (archive.clone(), Some(1_234_567), true)
        );
        assert!(file.pasted && file.folder.is_none() && !file.picture);
        assert_eq!(file.id, folder, "the same entry");
        assert_eq!(strip.rows()[1].stage, "");
        assert_eq!(strip.plan_request().expect("a plan").paths.len(), 2);
        assert_eq!(strip.summary().text, "2 files");
        // An archive that arrives for an entry that is gone is not taken (the caller
        // deletes it).
        assert!(!strip.folder_packed(folder, archive.clone(), 1));
        assert!(!strip.folder_packed(999, archive, 1));
        assert!(!strip.folder_packing(folder, 5));
    }

    #[test]
    fn one_question_for_all_the_folders_and_no_archive_larger_than_the_chat_takes() {
        let mut strip = FileStrip::new();
        strip.add(paths(&["Small", "Huge", "note.txt", "Empty"]), false);
        let small = is_folder(&mut strip, 0);
        let huge = is_folder(&mut strip, 1);
        let empty = is_folder(&mut strip, 3);
        assert_eq!(strip.take_new_folders().len(), 3);
        let rule = ArchiveRule {
            accepted: true,
            max: Some(100_000_000),
        };
        assert_eq!(
            strip.folder_counted(small, count(10, 5_000_000)),
            Counted::Asking
        );
        // One question for the folders given together: not before all are counted.
        assert_eq!(strip.folder_question(rule), None);
        assert_eq!(
            strip.folder_counted(huge, count(2_000, 3_200_000_000)),
            Counted::Asking
        );
        assert_eq!(strip.folder_question(rule), None);
        // A folder with nothing to send leaves and is named for the notice.
        assert_eq!(
            strip.folder_counted(
                empty,
                FolderCount {
                    hidden: 4,
                    ..FolderCount::default()
                }
            ),
            Counted::Empty("Empty".into())
        );
        assert_eq!(strip.folder_counted(empty, count(1, 1)), Counted::Gone);
        assert_eq!(strip.len(), 3);
        let q = strip.folder_question(rule).expect("the question");
        assert_eq!(q.ids, [small, huge]);
        assert_eq!(q.title, "Send 2 Folders");
        assert_eq!(q.text, "The 2 folders hold 2010 files, 3.2 GB in all.");
        assert_eq!(q.skipped, "");
        assert_eq!(q.zip_label, "Send as 2 .zip Files");
        // The size is checked before anything is packed: one folder cannot fit, so the
        // archive is not offered, and adding the files is.
        assert!(!q.can_zip);
        assert_eq!(
            q.zip_hint,
            "Not possible here: the files in “Huge” are 3.2 GB, and this chat takes files up to 100.0 MB."
        );
        // Two thousand files: adding them is asked about once more.
        assert!(q.large);
        assert_eq!(
            q.confirm,
            "This puts 2010 files into the list. They are sent one after another, each as it is."
        );
        assert_eq!(q.confirm_label, "Add All 2010 Files");
        // Without a limit both fit; a chat that takes no plain files takes no archive.
        let free = strip.folder_question(ArchiveRule::default()).expect("q");
        assert!(free.can_zip);
        assert_eq!(
            free.zip_hint,
            "Each folder is packed into one file, about 3.2 GB in all."
        );
        let none = strip
            .folder_question(ArchiveRule {
                accepted: false,
                max: None,
            })
            .expect("q");
        assert!(!none.can_zip);
        assert_eq!(none.zip_hint, "This chat does not take files of this kind.");
        // Exactly at the limit fits; a single folder that does not says so without its name.
        strip.answer_cancel();
        strip.add(paths(&["Exact"]), false);
        let exact = is_folder(&mut strip, 1);
        assert_eq!(
            strip.folder_counted(exact, count(1_000, 100_000_000)),
            Counted::Asking
        );
        let q = strip.folder_question(rule).expect("q");
        assert!(
            q.can_zip && !q.large,
            "a thousand files are added without a second question"
        );
        assert_eq!(
            strip.folder_counted(exact, count(1_001, 100_000_001)),
            Counted::Asking
        );
        let q = strip.folder_question(rule).expect("q");
        assert!(!q.can_zip && q.large);
        assert_eq!(
            q.zip_hint,
            "Not possible here: the files are 100.0 MB, and this chat takes files up to 100.0 MB."
        );
        // Both answers apply to every folder that waited.
        strip.add(paths(&["Second"]), false);
        let second = is_folder(&mut strip, 2);
        assert_eq!(strip.folder_counted(second, count(1, 1)), Counted::Asking);
        let jobs = strip.answer_zip();
        assert_eq!(
            jobs.iter().map(|j| j.id).collect::<Vec<_>>(),
            [exact, second]
        );
        assert_eq!(strip.folder_question(rule), None);
    }

    #[test]
    fn a_folder_the_core_saw_first_is_still_asked_about() {
        // The plan can arrive before the look: the core calls the folder "not a file".
        let mut strip = FileStrip::new();
        strip.add(paths(&["a.jpg", "Raw footage"]), false);
        let request = strip.plan_request().expect("a plan");
        let answer = PlanAnswer {
            serial: request.serial,
            preview: preview(&[Some(Image), None], &[(1, FileRefusal::NotAFile)], &[&[0]]),
        };
        assert!(strip.apply_plan(&answer));
        // Nothing is said of it yet, and it is not sent.
        assert_eq!(strip.rows()[1].reason, "");
        assert_eq!(strip.sendable(), paths(&["a.jpg"]));
        // The look: a folder. The mark goes, the question comes.
        let folder = is_folder(&mut strip, 1);
        assert_eq!(strip.files()[1].refusal, None);
        assert_eq!(strip.summary().refused, 0);
        assert_eq!(strip.folder_counted(folder, count(2, 20)), Counted::Asking);
        assert!(strip.folder_question(ArchiveRule::default()).is_some());

        // A send that went out before anybody looked: the core refuses the folder and it
        // comes back. It is looked at then, and asked about like any other folder.
        let mut strip = FileStrip::new();
        strip.restore_refused(vec![(
            HandedFile {
                path: PathBuf::from("/t/Raw footage"),
                pasted: false,
                as_file: false,
            },
            FileRefusal::NotAFile,
        )]);
        assert_eq!(
            strip.rows()[0].reason,
            "",
            "not called a file that cannot go"
        );
        assert_eq!(strip.unsized_files().len(), 1, "it is looked at");
        let folder = is_folder(&mut strip, 0);
        assert_eq!(strip.take_new_folders().len(), 1);
        assert_eq!(strip.folder_counted(folder, count(2, 20)), Counted::Asking);
        assert_eq!(strip.rows()[0].stage, "asking");
    }

    #[test]
    fn two_chats_that_become_one_join_their_files() {
        let mut survivor = FileStrip::starting_at(1 << 32);
        survivor.add(paths(&["mine.jpg"]), false);
        let mut merged = FileStrip::starting_at(2 << 32);
        merged.add(paths(&["theirs.pdf", "Folder"]), true);
        let folder = is_folder(&mut merged, 1);
        merged.take_new_folders();
        assert_eq!(merged.folder_counted(folder, count(3, 30)), Counted::Asking);
        let ids: Vec<u64> = merged.files().iter().map(|f| f.id).collect();
        let before = survivor.serial();
        survivor.absorb(merged);
        let names: Vec<&str> = survivor.files().iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["mine.jpg", "theirs.pdf", "Folder"]);
        // The entries are the same ones: what was asked about them (a look, a count, a
        // packing) is still answered by their ids.
        assert_eq!(
            survivor.files()[1..]
                .iter()
                .map(|f| f.id)
                .collect::<Vec<_>>(),
            ids
        );
        assert!(survivor.files()[1].pasted);
        assert!(survivor.contains(folder));
        assert_ne!(survivor.serial(), before, "the joined list needs a plan");
        assert_eq!(survivor.plan_request().expect("a plan").paths.len(), 2);
        assert!(survivor.folder_question(ArchiveRule::default()).is_some());
        // Nothing joins from an empty list.
        let serial = survivor.serial();
        survivor.absorb(FileStrip::starting_at(3 << 32));
        assert_eq!(survivor.serial(), serial);
    }

    #[test]
    fn notices_about_folders_say_what_happened_and_what_was_left_out() {
        let counted = FolderCount {
            files: 124,
            bytes: 1,
            hidden: 12,
            links: 1,
            other: 0,
        };
        assert_eq!(
            folder_added_notice("Raw footage", 124, &counted),
            "124 files were added from “Raw footage”. Left out: 12 hidden items, 1 link."
        );
        assert_eq!(
            folder_added_notice("One", 1, &FolderCount::default()),
            "1 file was added from “One”."
        );
        assert_eq!(
            folder_empty_notice(
                "Dotfiles",
                &FolderCount {
                    hidden: 4,
                    other: 2,
                    ..FolderCount::default()
                }
            ),
            "“Dotfiles” holds no files to send. Left out: 4 hidden items, 2 items that could not be read."
        );
        assert_eq!(
            folder_unreadable_notice("Locked"),
            "“Locked” could not be read."
        );
        assert_eq!(
            folder_unpacked_notice("Big", false),
            "“Big” could not be packed."
        );
        assert!(folder_unpacked_notice("Big", true).ends_with("not enough room on the disk."));
        assert_eq!(folder_packed_notice("Busy", 0), "");
        assert_eq!(
            folder_packed_notice("Busy", 2),
            "2 files could not be read and are not in “Busy.zip”."
        );
        assert_eq!(counted.skipped(), 13);
    }

    #[test]
    fn debug_shows_no_names_or_paths() {
        let mut strip = FileStrip::new();
        strip.add(paths(&["secret-name.pdf"]), false);
        let shown = format!("{strip:?} {:?} {:?}", strip.rows(), strip.plan_request());
        assert!(!shown.contains("secret"), "{shown}");
    }
}

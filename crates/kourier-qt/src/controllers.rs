//! Controllers without rows: composer, login and settings (§8.4).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{Mutex, PoisonError};

use cxx_qt::{CxxQtThread, CxxQtType, Threading};
use cxx_qt_lib::{QList, QMap, QMapPair_QString_QVariant, QString, QStringList, QUrl, QVariant};
use kourier_app::desktop::DesktopHandle;
use kourier_app::folders::{self, Cancel, FolderError, FolderSummary, Packed};
use kourier_app::{AppHandle, NotificationContent, Prefs};
use kourier_model::{ConversationId, MessageId, NetworkKind};
use kourier_viewmodel::files::{
    Counted, FileFacts, FileStrip, FolderCount, FolderJob, FolderQuestion, HandedFile, ListedFile,
    PlanAnswer, StripFile, folder_added_notice, folder_empty_notice, folder_packed_notice,
    folder_unpacked_notice, folder_unreadable_notice, refused_notice,
};
use kourier_viewmodel::i18n::{trc, trnc};
use kourier_viewmodel::source::{FinishedSend, RefusedSend, SendOutcome};
use kourier_viewmodel::{Changes, LoginView, LoginViewKind, SourceError};

use crate::app::pinned;
use crate::bridge::qobject::{self, App, AttachmentListModel, ComposerController};
use crate::models;
use crate::shared::Shared;

/// Merges `changes` and applies them after the current invokable returned (re-entrancy rule,
/// §8.5).
pub(crate) fn defer(shared: &Shared, app: Option<&CxxQtThread<App>>, changes: Changes) {
    if !changes.any() {
        return;
    }
    shared.borrow_mut().pending.merge(changes);
    if let Some(app) = app
        && app.queue(|app| app.apply_pending()).is_err()
    {
        tracing::debug!("App destroyed before a refresh");
    }
}

/// The pasted files of sends the core has not answered yet. A pasted file (a clipboard image
/// written to the tmpfs paste directory) is Kourier's own copy: it must stay until the core
/// has copied it, and should not stay longer. The core says when: every `SendFiles` carries a
/// request id and ends with "stored" or with a refusal that names it.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct PastedOut {
    /// `(request, file)`, in sending order.
    files: Vec<(u64, PathBuf)>,
}

impl PastedOut {
    /// Pasted files went out with `request`.
    pub(crate) fn sent(&mut self, request: u64, files: impl IntoIterator<Item = PathBuf>) {
        self.files.extend(files.into_iter().map(|f| (request, f)));
    }

    /// The core refused `request`. The files in `returning` go back to the composer, which
    /// looks after them again; the others went out again with `resent` and wait for that
    /// request now. Returns the files nobody needs any more (none, unless a file neither
    /// came back nor was sent again).
    pub(crate) fn refused(
        &mut self,
        request: u64,
        returning: &HashSet<PathBuf>,
        resent: Option<u64>,
    ) -> Vec<PathBuf> {
        let mut gone = Vec::new();
        self.files.retain_mut(|(of, path)| {
            if *of != request {
                return true;
            }
            if returning.contains(path) {
                return false;
            }
            match resent {
                Some(again) => {
                    *of = again;
                    true
                }
                None => {
                    gone.push(path.clone());
                    false
                }
            }
        });
        gone
    }

    /// `request` is over. Returns the files to delete: all of the request when the core has
    /// stored its messages (it has its own copies) or refused it (what came back was taken
    /// out by [`Self::refused`]); none when the answer was lost with lagged events, because
    /// the core may still be copying. Those are forgotten here and go when the app exits.
    pub(crate) fn finished(&mut self, request: u64, outcome: SendOutcome) -> Vec<PathBuf> {
        let mut gone = Vec::new();
        self.files.retain(|(of, path)| {
            if *of != request {
                return true;
            }
            if outcome != SendOutcome::Unknown {
                gone.push(path.clone());
            }
            false
        });
        gone
    }

    /// Whether nothing waits.
    pub(crate) fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Rust side of `ComposerController`.
///
/// The strip of files waiting to be sent has no limit of Kourier's own (architecture §1.1
/// goal 2; `kourier_viewmodel::files`): any number of files of any kinds is added in one go.
/// Adding does no I/O on the Qt thread: sizes are read on a worker thread and what the
/// network makes of the selection comes from the core's preview, both later.
pub struct ComposerRust {
    pub(crate) reply_to_id: i64,
    pub(crate) reply_author: QString,
    pub(crate) reply_text: QString,
    pub(crate) reply_icon: QString,
    pub(crate) reply_thumb: QString,
    pub(crate) edit_id: i64,
    pub(crate) edit_caption: bool,
    pub(crate) edit_text: QString,
    pub(crate) error: QString,
    pub(crate) notice: QString,
    /// The list model of the strip (owned by `App`, like the controller itself).
    pub(crate) files: *mut AttachmentListModel,
    pub(crate) attachment_count: i32,
    pub(crate) sendable_count: i32,
    pub(crate) refused_count: i32,
    pub(crate) message_count: i32,
    pub(crate) planned: bool,
    pub(crate) summary: QString,
    pub(crate) hint: QString,
    pub(crate) preparing: i32,
    /// Of `preparing`, the files the core has copied already.
    pub(crate) preparing_done: i32,
    /// The question about the folders that wait in the open chat's list (a map of what the
    /// dialog says; empty while there is nothing to ask).
    pub(crate) folder_question: QVariant,
    /// There is such a question.
    pub(crate) folder_asking: bool,
    pub(crate) shared: Option<Shared>,
    pub(crate) app: Option<Box<CxxQtThread<App>>>,
    /// The running app (pasted files, the worker threads); `None` in pure UI tests.
    pub(crate) handle: Option<AppHandle>,
    /// The conversation the strip's files are for (the open one).
    pub(crate) conversation: i64,
    strip: FileStrip,
    /// The files waiting in the chats that are not open, by conversation: like the text, a
    /// selection stays with its chat when the user looks at another one and is there again
    /// on the way back (for this session; pasted files among them stay on disk that long).
    parked: HashMap<i64, FileStrip>,
    /// How many strips were made: each gets its own range of entry ids and serials, so the
    /// answer to a question about one chat's files never fits another chat's strip.
    strips_made: u64,
    /// The text area holds a text (the caption of the files).
    has_text: bool,
    /// The caption, kept only while it is longer than one message of the chat carries: the
    /// plan then depends on its text.
    long_caption: Option<String>,
    /// Counts publishes: the list model's generation.
    generation: u64,
    /// Pasted files of sends the core has not answered yet.
    pasted_out: PastedOut,
    /// The work on folders that is running off the Qt thread (counting, listing, packing),
    /// by the entry it is for: used to stop it when the entry leaves its list.
    jobs: HashMap<u64, Cancel>,
    /// The question about the open list's folders, and whether the user is at its second
    /// step (adding very many files is asked about once more).
    asked: Option<FolderQuestion>,
    confirming: bool,
    /// The step that is shown with `asked` (what `folder_question` says now).
    shown_confirming: bool,
}

impl Default for ComposerRust {
    fn default() -> Self {
        Self {
            reply_to_id: 0,
            reply_author: QString::default(),
            reply_text: QString::default(),
            reply_icon: QString::default(),
            reply_thumb: QString::default(),
            edit_id: 0,
            edit_caption: false,
            edit_text: QString::default(),
            error: QString::default(),
            notice: QString::default(),
            files: std::ptr::null_mut(),
            attachment_count: 0,
            sendable_count: 0,
            refused_count: 0,
            message_count: 0,
            planned: true,
            summary: QString::default(),
            hint: QString::default(),
            preparing: 0,
            preparing_done: 0,
            folder_question: QVariant::from(&QMap::<QMapPair_QString_QVariant>::default()),
            folder_asking: false,
            shared: None,
            app: None,
            handle: None,
            conversation: 0,
            strip: FileStrip::new(),
            parked: HashMap::new(),
            strips_made: 0,
            has_text: false,
            long_caption: None,
            generation: 0,
            pasted_out: PastedOut::default(),
            jobs: HashMap::new(),
            asked: None,
            confirming: false,
            shown_confirming: false,
        }
    }
}

/// Where a finished paste goes: the app (to write the file), the composer (to show it) and
/// the conversation that was open when the user pasted (its list gets the file, also when
/// another chat is open by the time the picture is encoded and written).
static PASTE: Mutex<Option<(AppHandle, CxxQtThread<ComposerController>, i64)>> = Mutex::new(None);

/// The local path of a `file://` URL or a plain absolute path.
fn local_path(url: &QString) -> Option<PathBuf> {
    let text = url.to_string();
    if text.starts_with('/') {
        return Some(PathBuf::from(text));
    }
    QUrl::from(url)
        .to_local_file()
        .map(|p| PathBuf::from(p.to_string()))
        .filter(|p| p.is_absolute())
}

/// Called on a worker thread with the clipboard image as PNG: writes it into the tmpfs paste
/// directory and adds it to the composer on the Qt thread. A panic stays here (cxx would abort
/// the app if it unwound into C++).
fn pasted(png: Vec<u8>) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| store_paste(png))).is_err() {
        tracing::error!("storing a pasted image panicked");
    }
}

fn store_paste(png: Vec<u8>) {
    let target = PASTE.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let Some((handle, composer, conversation)) = target else {
        return;
    };
    if png.is_empty() {
        return;
    }
    let name = format!(
        "{}.png",
        trc(
            "@item:intext name of a file made from a pasted picture",
            "Pasted image"
        )
    );
    match handle.save_pasted(&name, &png) {
        Ok(path) => {
            let handle2 = handle.clone();
            let path2 = path.clone();
            if composer
                .queue(move |c| c.push_pasted(conversation, path))
                .is_err()
            {
                handle2.release_pasted(&path2);
            }
        }
        Err(e) => tracing::warn!(error = %e, "a pasted image could not be stored"),
    }
}

/// A look at each file, on a worker thread: its size (`None` for what is no regular file or
/// cannot be looked at), whether its first bytes are those of a picture format on the
/// decoder allow-list ([`crate::media::Codec::sniff`]), and whether it is a folder. Only a
/// file of such a format is later handed to a QML `Image` as its own preview: Qt picks the
/// decoder by content, and the installed plugins include PostScript (Ghostscript) and PDF
/// (§8.6). A folder is not a file that cannot go: the composer asks what to do with it.
fn look_at_all(files: Vec<(u64, PathBuf)>) -> Vec<(u64, FileFacts)> {
    files
        .into_iter()
        .map(|(id, path)| {
            // (Through a link: the user pointed at what it leads to.)
            let meta = std::fs::metadata(&path).ok();
            let folder = meta.as_ref().is_some_and(std::fs::Metadata::is_dir);
            let size = meta.filter(std::fs::Metadata::is_file).map(|m| m.len());
            let picture = size.is_some() && crate::media::sniff_file(&path).is_some();
            (
                id,
                FileFacts {
                    size,
                    picture,
                    folder,
                },
            )
        })
        .collect()
}

/// What a walk counted, as the list keeps it.
fn folder_count(summary: &FolderSummary) -> FolderCount {
    FolderCount {
        files: summary.files,
        bytes: summary.bytes,
        hidden: summary.skipped.hidden,
        links: summary.skipped.links,
        other: summary.skipped.special + summary.skipped.unreadable,
    }
}

/// The files of a folder for the list, with what the list would otherwise have to look up
/// for each: the size (the walk has it) and whether it is a picture by content. Blocking.
fn list_folder(
    path: &std::path::Path,
    cancel: &Cancel,
) -> Result<(Vec<ListedFile>, FolderCount), FolderError> {
    let listing = folders::list(path, cancel)?;
    let count = folder_count(&FolderSummary {
        files: listing.files.len() as u64,
        bytes: listing.bytes(),
        skipped: listing.skipped,
    });
    let mut files = Vec::with_capacity(listing.files.len());
    for file in listing.files {
        if cancel.is_cancelled() {
            return Err(FolderError::Cancelled);
        }
        let picture = crate::media::sniff_file(&file.path).is_some();
        files.push(ListedFile {
            path: file.path,
            size: file.size,
            picture,
        });
    }
    Ok((files, count))
}

/// What a finished job says to the composer, on the Qt thread.
enum FolderDone {
    /// A folder was counted.
    Counted(Result<FolderSummary, FolderError>),
    /// Its files were listed.
    Listed(Result<(Vec<ListedFile>, FolderCount), FolderError>),
    /// Its archive was written (at this path).
    Packed(PathBuf, Result<Packed, FolderError>),
}

impl ComposerRust {
    /// A strip for a chat that has none yet, with entry ids and serials of its own.
    fn fresh_strip(&mut self) -> FileStrip {
        self.strips_made += 1;
        FileStrip::starting_at(self.strips_made << 32)
    }

    /// Makes `conversation` the one the composer writes to. The files waiting for the chat
    /// that is left stay with it, and the ones that waited for the new chat are the open
    /// list again. Returns the files that were dropped: without a chat (`0`) there is
    /// nowhere to keep them.
    fn switch_to(&mut self, conversation: i64) -> Vec<StripFile> {
        let old = self.conversation;
        if old == conversation {
            return Vec::new();
        }
        let mut leaving = std::mem::take(&mut self.strip);
        let dropped = if old == 0 {
            leaving.clear()
        } else {
            if !leaving.is_empty() {
                self.parked.insert(old, leaving);
            }
            Vec::new()
        };
        self.strip = match self.parked.remove(&conversation) {
            Some(waiting) => waiting,
            None => self.fresh_strip(),
        };
        self.conversation = conversation;
        // The text area tells what it holds for the new chat.
        self.long_caption = None;
        dropped
    }

    /// The list of `conversation`: the open one, or the one that waits for a chat that is
    /// not open (made now when that chat has none). What arrives for a chat after the user
    /// went on to another one (a pasted picture that was still being written, the files of
    /// a refused send) goes to the chat it was for, never to the one that happens to be
    /// open.
    fn strip_of(&mut self, conversation: i64) -> &mut FileStrip {
        if conversation == self.conversation {
            return &mut self.strip;
        }
        let made = &mut self.strips_made;
        self.parked.entry(conversation).or_insert_with(|| {
            *made += 1;
            FileStrip::starting_at(*made << 32)
        })
    }
}

impl ComposerRust {
    /// The list that holds entry `id`: the open one or one that waits for another chat
    /// (a job on a folder goes on when the user looks at another chat).
    fn strip_with(&mut self, id: u64) -> Option<(&mut FileStrip, bool)> {
        if self.strip.contains(id) {
            return Some((&mut self.strip, true));
        }
        self.parked
            .values_mut()
            .find(|strip| strip.contains(id))
            .map(|strip| (strip, false))
    }

    /// Stops the work on the folders among `files`: they left their list.
    fn stop_jobs(&mut self, files: &[StripFile]) {
        for file in files.iter().filter(|f| f.folder.is_some()) {
            if let Some(cancel) = self.jobs.remove(&file.id) {
                cancel.cancel();
            }
        }
    }

    /// `from` was merged into `into` (§3.3): the files that waited for the chat that went
    /// wait for the one that is left, behind the files that one had. Returns whether the
    /// open list changed.
    fn merge(&mut self, from: i64, into: i64) -> bool {
        if from == into || from == 0 || into == 0 {
            return false;
        }
        if self.conversation == from {
            // The open chat goes on under the surviving id, joined by what waited there.
            self.conversation = into;
            match self.parked.remove(&into) {
                Some(mut waiting) => {
                    waiting.absorb(std::mem::take(&mut self.strip));
                    self.strip = waiting;
                    true
                }
                None => false,
            }
        } else if let Some(moving) = self.parked.remove(&from) {
            if self.conversation == into {
                self.strip.absorb(moving);
                true
            } else {
                match self.parked.entry(into) {
                    std::collections::hash_map::Entry::Occupied(mut there) => {
                        there.get_mut().absorb(moving);
                    }
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        slot.insert(moving);
                    }
                }
                false
            }
        } else {
            false
        }
    }
}

impl qobject::ComposerController {
    /// Shows the strip as it is: the list, the counts, the summary line.
    fn publish(mut self: Pin<&mut Self>) {
        let rows = self.strip.rows();
        let summary = self.strip.summary();
        let sendable = self.strip.files().len() - summary.folders - summary.refused;
        let planned = self.strip.planned();
        let can_send = self.strip.can_send();
        self.as_mut().rust_mut().generation += 1;
        let generation = self.generation;
        if let Some(model) = pinned(self.files) {
            models::set_rows(model, generation, rows);
        }
        let int = |n: usize| i32::try_from(n).unwrap_or(i32::MAX);
        // Every row of the list: files, and folders that are not files or an archive yet.
        let listed = int(summary.files + summary.folders);
        if *self.attachment_count() != listed {
            self.as_mut().set_attachment_count(listed);
        }
        // What a send would take now: nothing while a quote cannot go with the files.
        let sendable = if can_send { int(sendable) } else { 0 };
        if *self.sendable_count() != sendable {
            self.as_mut().set_sendable_count(sendable);
        }
        if *self.refused_count() != int(summary.refused) {
            self.as_mut().set_refused_count(int(summary.refused));
        }
        let messages = int(summary.messages + summary.caption_messages);
        if *self.message_count() != messages {
            self.as_mut().set_message_count(messages);
        }
        if *self.planned() != planned {
            self.as_mut().set_planned(planned);
        }
        let text = QString::from(summary.text.as_str());
        if *self.summary() != text {
            self.as_mut().set_summary(text);
        }
        let hint = QString::from(summary.hint.as_str());
        if *self.hint() != hint {
            self.as_mut().set_hint(hint);
        }
        self.refresh_question();
    }

    /// The question about the folders that wait in the open list, as the dialog shows it
    /// (nothing while a folder is still being counted, or none waits).
    fn refresh_question(mut self: Pin<&mut Self>) {
        let conversation = ConversationId(self.conversation);
        let rule = self
            .shared
            .as_ref()
            .map(|s| s.borrow().source.archive_rule(conversation))
            .unwrap_or_default();
        let question = self.strip.folder_question(rule);
        // The second step is about one set of folders: another set starts at the first.
        let same = |a: &Option<FolderQuestion>, b: &Option<FolderQuestion>| {
            a.as_ref().map(|q| &q.ids) == b.as_ref().map(|q| &q.ids)
        };
        let confirming = self.confirming && same(&question, &self.asked);
        let unchanged = question == self.asked && confirming == self.shown_confirming;
        {
            let mut rust = self.as_mut().rust_mut();
            rust.confirming = confirming;
            rust.shown_confirming = confirming;
            if !unchanged {
                rust.asked.clone_from(&question);
            }
        }
        if unchanged {
            return;
        }
        let mut map = QMap::<QMapPair_QString_QVariant>::default();
        if let Some(q) = &question {
            let mut text = |key: &str, value: &str| {
                map.insert(QString::from(key), QVariant::from(&QString::from(value)));
            };
            text("title", &q.title);
            text("text", &q.text);
            text("skipped", &q.skipped);
            text("addLabel", &q.add_label);
            text("addHint", &q.add_hint);
            text("zipLabel", &q.zip_label);
            text("zipHint", &q.zip_hint);
            text("confirm", &q.confirm);
            text("confirmLabel", &q.confirm_label);
            map.insert(QString::from("canZip"), QVariant::from(&q.can_zip));
            map.insert(QString::from("confirming"), QVariant::from(&confirming));
        }
        self.as_mut().set_folder_question(QVariant::from(&map));
        let asking = question.is_some();
        if *self.folder_asking() != asking {
            self.as_mut().set_folder_asking(asking);
        }
    }

    /// The answer to the question about the folders: `add` (the files inside join the
    /// list; when they are very many, this asks once more and the next `add` does it),
    /// `zip` (each folder is packed into one archive), `back` (from the second question to
    /// the first) or `cancel` (the folders leave the list).
    pub fn answer_folders(mut self: Pin<&mut Self>, answer: &QString) {
        let Some(question) = self.asked.clone() else {
            return;
        };
        match answer.to_string().as_str() {
            "add" if question.large && !self.confirming => {
                self.as_mut().rust_mut().confirming = true;
                self.refresh_question();
            }
            "add" => {
                let jobs = self.as_mut().rust_mut().strip.answer_add();
                self.as_mut().rust_mut().confirming = false;
                for job in jobs {
                    self.as_mut().start_listing(job);
                }
                self.publish();
            }
            "zip" if question.can_zip => {
                let jobs = self.as_mut().rust_mut().strip.answer_zip();
                self.as_mut().rust_mut().confirming = false;
                self.as_mut().start_packing(jobs);
                self.publish();
            }
            "back" => {
                self.as_mut().rust_mut().confirming = false;
                self.refresh_question();
            }
            "cancel" => {
                self.as_mut().rust_mut().confirming = false;
                if self.as_mut().rust_mut().strip.answer_cancel() > 0 {
                    self.strip_changed();
                } else {
                    self.refresh_question();
                }
            }
            _ => {}
        }
    }

    /// Runs `work` for the folder entry `id` on a worker thread and hands what it returns
    /// to [`Self::folder_done`] on the Qt thread. `work` gets the handle that stops it and
    /// a way to report the count so far.
    fn spawn_folder_job(
        mut self: Pin<&mut Self>,
        id: u64,
        work: impl FnOnce(&Cancel, &mut dyn FnMut(FolderSummary)) -> FolderDone + Send + 'static,
    ) {
        let Some(handle) = self.handle.clone() else {
            return;
        };
        let cancel = Cancel::new();
        if let Some(old) = self.as_mut().rust_mut().jobs.insert(id, cancel.clone()) {
            old.cancel();
        }
        let composer = self.qt_thread();
        // The app closing stops the work too (a worker thread outlives the runtime's wait).
        let closing = handle.shutdown_token();
        handle.runtime().spawn_blocking(move || {
            let reporter = composer.clone();
            let stop = cancel.clone();
            let mut report = move |so_far: FolderSummary| {
                if closing.is_cancelled() {
                    stop.cancel();
                }
                // (A composer that is gone: the job ends with its cancel or its work.)
                let _ = reporter.queue(move |c| c.folder_progress(id, &so_far));
            };
            let done = work(&cancel, &mut report);
            if composer.queue(move |c| c.folder_done(id, done)).is_err() {
                tracing::debug!("the composer went before a folder was looked into");
            }
        });
    }

    /// Counts the folders nobody has counted yet, in every list (off the Qt thread).
    fn count_new_folders(mut self: Pin<&mut Self>) {
        let fresh: Vec<(u64, PathBuf)> = {
            let mut rust = self.as_mut().rust_mut();
            let mut fresh = rust.strip.take_new_folders();
            for strip in rust.parked.values_mut() {
                fresh.extend(strip.take_new_folders());
            }
            fresh
        };
        for (id, path) in fresh {
            self.as_mut().spawn_folder_job(id, move |cancel, report| {
                let mut tick = |so_far: &FolderSummary| report(*so_far);
                FolderDone::Counted(folders::summarize(&path, cancel, &mut tick))
            });
        }
    }

    /// Lists the files of a folder the user wants in the list (off the Qt thread).
    fn start_listing(self: Pin<&mut Self>, job: FolderJob) {
        self.spawn_folder_job(job.id, move |cancel, _| {
            FolderDone::Listed(list_folder(&job.path, cancel))
        });
    }

    /// Packs the folders the user wants as archives, one after the other (off the Qt
    /// thread; one worker for all of them: they read the same disk).
    fn start_packing(mut self: Pin<&mut Self>, jobs: Vec<FolderJob>) {
        let Some(handle) = self.handle.clone() else {
            return;
        };
        let composer = self.qt_thread();
        let mut work = Vec::with_capacity(jobs.len());
        for job in jobs {
            let cancel = Cancel::new();
            if let Some(old) = self.as_mut().rust_mut().jobs.insert(job.id, cancel.clone()) {
                old.cancel();
            }
            work.push((job, cancel));
        }
        let runtime = handle.runtime().clone();
        // The app closing stops the packing too (a worker thread outlives the runtime's
        // wait; the archive store hands out no place after its wipe either).
        let closing = handle.shutdown_token();
        runtime.spawn_blocking(move || {
            for (job, cancel) in work {
                let id = job.id;
                if closing.is_cancelled() {
                    cancel.cancel();
                }
                let packed = match handle.reserve_archive(&job.name) {
                    Ok(dest) => {
                        let reporter = composer.clone();
                        let (closing, stop) = (closing.clone(), cancel.clone());
                        let mut progress = move |done: u64, _total: u64| {
                            if closing.is_cancelled() {
                                stop.cancel();
                            }
                            let so_far = FolderSummary {
                                bytes: done,
                                ..FolderSummary::default()
                            };
                            let _ = reporter.queue(move |c| c.folder_progress(id, &so_far));
                        };
                        let result =
                            folders::pack(&job.path, &job.name, &dest, &cancel, &mut progress);
                        if result.is_err() {
                            // Nothing was left there: the place goes too.
                            handle.release_pasted(&dest);
                        }
                        (dest, result)
                    }
                    Err(e) => (PathBuf::new(), Err(e)),
                };
                let late = packed.1.is_ok().then(|| packed.0.clone());
                let done = FolderDone::Packed(packed.0, packed.1);
                if composer.queue(move |c| c.folder_done(id, done)).is_err()
                    && let Some(path) = late
                {
                    // The composer is gone (the app is closing): nobody takes the archive.
                    handle.release_pasted(&path);
                }
            }
        });
    }

    /// Fixture pages only (`App.fixtureFolders`): `add` answers the question as the user
    /// would; `zip` packs the first folder for real and leaves the others standing part
    /// way, without a worker, so a picture of packing does not depend on the disk.
    pub(crate) fn fixture_folders(mut self: Pin<&mut Self>, answer: &str) -> bool {
        if self.asked.is_none() {
            return false;
        }
        if answer != "zip" {
            self.answer_folders(&QString::from(answer));
            return true;
        }
        let mut jobs = self.as_mut().rust_mut().strip.answer_zip().into_iter();
        let Some(first) = jobs.next() else {
            return false;
        };
        for job in jobs {
            let bytes = self
                .strip
                .files()
                .iter()
                .find(|f| f.id == job.id)
                .and_then(|f| f.folder)
                .map_or(0, |f| f.count.bytes);
            let part = u64::try_from((u128::from(bytes) * 45).div_ceil(100)).unwrap_or(bytes);
            self.as_mut().rust_mut().strip.folder_packing(job.id, part);
        }
        self.as_mut().start_packing(vec![first]);
        self.publish();
        true
    }

    /// How far the work on folder `id` is: its count so far, or the bytes packed.
    fn folder_progress(mut self: Pin<&mut Self>, id: u64, so_far: &FolderSummary) {
        let (changed, open) = {
            let mut rust = self.as_mut().rust_mut();
            match rust.strip_with(id) {
                Some((strip, open)) => (
                    strip.folder_counting(id, folder_count(so_far))
                        || strip.folder_packing(id, so_far.bytes),
                    open,
                ),
                None => (false, false),
            }
        };
        if changed && open {
            self.publish();
        }
    }

    /// The work on folder `id` ended.
    fn folder_done(mut self: Pin<&mut Self>, id: u64, done: FolderDone) {
        self.as_mut().rust_mut().jobs.remove(&id);
        let name = |strip: &FileStrip| {
            strip
                .files()
                .iter()
                .find(|f| f.id == id)
                .map(|f| f.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| {
                    trc("@item:intext name of a folder without a name", "Folder").into_owned()
                })
        };
        // What the user is told (only about the open chat's list; another chat's list
        // shows the result when it is opened).
        let mut notice = None;
        let mut orphan = None;
        let (changed, open) = {
            let mut rust = self.as_mut().rust_mut();
            let Some((strip, open)) = rust.strip_with(id) else {
                // The entry left its list while the work ran: an archive made for it goes.
                if let FolderDone::Packed(path, Ok(_)) = &done
                    && let Some(handle) = &rust.handle
                {
                    handle.release_pasted(path);
                }
                return;
            };
            let folder = name(strip);
            let changed = match done {
                FolderDone::Counted(Ok(summary)) => {
                    let count = folder_count(&summary);
                    match strip.folder_counted(id, count) {
                        Counted::Empty(folder) => {
                            notice = Some(folder_empty_notice(&folder, &count));
                            true
                        }
                        Counted::Asking => true,
                        Counted::Gone => false,
                    }
                }
                FolderDone::Listed(Ok((files, count))) => {
                    let added = strip.folder_listed(id, files);
                    notice = Some(folder_added_notice(&folder, added, &count));
                    true
                }
                FolderDone::Packed(path, Ok(packed)) => {
                    if strip.folder_packed(id, path.clone(), packed.bytes_out) {
                        let text = folder_packed_notice(&folder, packed.missing);
                        notice = (!text.is_empty()).then_some(text);
                        true
                    } else {
                        orphan = Some(path);
                        false
                    }
                }
                // Stopped because the entry left, or was asked to: nothing to say.
                FolderDone::Counted(Err(FolderError::Cancelled))
                | FolderDone::Listed(Err(FolderError::Cancelled))
                | FolderDone::Packed(_, Err(FolderError::Cancelled)) => false,
                FolderDone::Packed(_, Err(FolderError::Empty)) => {
                    notice = Some(folder_empty_notice(&folder, &FolderCount::default()));
                    strip.remove_id(id).is_some()
                }
                FolderDone::Packed(_, Err(e)) => {
                    tracing::warn!(error = %e, "a folder could not be packed");
                    let full = matches!(e, FolderError::Io(std::io::ErrorKind::StorageFull));
                    notice = Some(folder_unpacked_notice(&folder, full));
                    strip.remove_id(id).is_some()
                }
                FolderDone::Counted(Err(e)) | FolderDone::Listed(Err(e)) => {
                    tracing::warn!(error = %e, "a folder could not be read");
                    notice = Some(folder_unreadable_notice(&folder));
                    strip.remove_id(id).is_some()
                }
            };
            (changed, open)
        };
        if let (Some(path), Some(handle)) = (orphan, &self.handle) {
            handle.release_pasted(&path);
        }
        if !open {
            return;
        }
        if changed {
            self.as_mut().strip_changed();
        }
        if let Some(text) = notice {
            self.set_notice(QString::from(text.as_str()));
        }
    }

    /// The strip changed: show it, ask the core what it makes of the selection and read the
    /// sizes that are not known yet (both off the Qt thread).
    fn strip_changed(mut self: Pin<&mut Self>) {
        self.as_mut().publish();
        self.as_mut().replan();
        self.as_mut().stat_files();
        self.count_new_folders();
    }

    /// Asks the core how the files would be sent, unless its answer for this selection is in.
    fn replan(mut self: Pin<&mut Self>) {
        let reply = *self.reply_to_id() > 0;
        let has_text = self.has_text;
        let long = self.long_caption.clone();
        if self
            .as_mut()
            .rust_mut()
            .strip
            .set_caption(has_text, long, reply)
        {
            self.as_mut().publish();
        }
        let conversation = self.conversation;
        let (Some(shared), Some(request)) = (self.shared.clone(), self.strip.plan_request()) else {
            return;
        };
        if conversation != 0 {
            shared
                .borrow_mut()
                .source
                .plan_files(ConversationId(conversation), request);
        }
    }

    /// Looks at the files that were not looked at yet (size, picture by content), on a worker
    /// thread.
    fn stat_files(self: Pin<&mut Self>) {
        let wanted = self.strip.unsized_files();
        let Some(handle) = self.handle.clone() else {
            return;
        };
        if wanted.is_empty() {
            return;
        }
        let composer = self.qt_thread();
        handle.runtime().spawn_blocking(move || {
            let facts = look_at_all(wanted);
            if composer.queue(move |c| c.facts_ready(&facts)).is_err() {
                tracing::debug!("the composer went before its files were looked at");
            }
        });
    }

    /// What a worker thread found about the files arrived. A file without a size
    /// (unreadable, a socket) is not looked at again: the core's plan says what is wrong
    /// with it. A folder among them becomes a folder entry, is left out of the plan and is
    /// counted now.
    pub(crate) fn facts_ready(mut self: Pin<&mut Self>, facts: &[(u64, FileFacts)]) {
        // (Also for a list the user has left meanwhile: an entry is known by its id.)
        let open = {
            let mut rust = self.as_mut().rust_mut();
            for strip in rust.parked.values_mut() {
                strip.set_facts(facts);
            }
            rust.strip.set_facts(facts)
        };
        if open {
            self.as_mut().publish();
            // A folder among them is no file: the plan is about the files only.
            self.as_mut().replan();
        }
        self.count_new_folders();
    }

    /// The core's answer to a plan request arrived.
    pub(crate) fn plan_ready(mut self: Pin<&mut Self>, answer: &PlanAnswer) {
        if self.as_mut().rust_mut().strip.apply_plan(answer) {
            self.publish();
        }
    }

    /// The core could not answer the plan request with this serial.
    pub(crate) fn plan_failed(mut self: Pin<&mut Self>, serial: u64) {
        if self.as_mut().rust_mut().strip.plan_unavailable(serial) {
            self.publish();
        }
    }

    /// The conversation the composer writes to changed. The files waiting for the chat that
    /// is left stay with it, and the ones that waited for the new chat are there again.
    pub(crate) fn set_conversation(mut self: Pin<&mut Self>, conversation: i64) {
        if self.conversation == conversation {
            self.refresh_preparing();
            return;
        }
        let dropped = self.as_mut().rust_mut().switch_to(conversation);
        self.as_mut().rust_mut().stop_jobs(&dropped);
        if let Some(h) = &self.handle {
            for f in dropped.iter().filter(|f| f.pasted) {
                h.release_pasted(&f.path);
            }
        }
        // (The second step of a question does not follow to another chat.)
        self.as_mut().rust_mut().confirming = false;
        self.as_mut().refresh_rules();
        self.as_mut().strip_changed();
        self.refresh_preparing();
    }

    /// `from` was merged into `into` (§3.3), whether or not either is the open chat: the
    /// files that waited for `from` go on waiting for `into`, and when both had files
    /// waiting, the two lists become one.
    pub(crate) fn merge_conversations(mut self: Pin<&mut Self>, from: i64, into: i64) {
        if self.as_mut().rust_mut().merge(from, into) {
            self.strip_changed();
        }
    }

    /// `conversation` is gone for good (the user deleted the chat): the files that waited
    /// for it are dropped.
    pub(crate) fn forget_conversation(mut self: Pin<&mut Self>, conversation: i64) {
        let open = self.conversation == conversation;
        let files = {
            let mut rust = self.as_mut().rust_mut();
            let mut files = rust
                .parked
                .remove(&conversation)
                .map(|mut strip| strip.clear())
                .unwrap_or_default();
            if open {
                files.extend(rust.strip.clear());
            }
            rust.stop_jobs(&files);
            files
        };
        if let Some(h) = &self.handle {
            for f in files.iter().filter(|f| f.pasted) {
                h.release_pasted(&f.path);
            }
        }
        if open && !files.is_empty() {
            self.strip_changed();
        }
    }

    /// How many chats that are not open have files waiting in their composer.
    pub fn parked_chats(&self) -> usize {
        self.parked.len()
    }

    /// What the open conversation's network says about files (types some client will not
    /// save): the list passes it on beside the files it is about.
    pub(crate) fn refresh_rules(mut self: Pin<&mut Self>) {
        let conversation = self.conversation;
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let rules = shared
            .borrow()
            .source
            .strip_rules(ConversationId(conversation));
        if self.as_mut().rust_mut().strip.set_rules(rules) {
            self.publish();
        } else {
            // What the chat takes as one file may be known only now.
            self.refresh_question();
        }
    }

    /// How many files the core is still preparing for the open conversation, and how many of
    /// them it has copied.
    pub(crate) fn refresh_preparing(mut self: Pin<&mut Self>) {
        let conversation = ConversationId(self.conversation);
        let (count, done) = self.shared.as_ref().map_or((0, 0), |s| {
            let state = s.borrow();
            (
                state.source.preparing_files(conversation),
                state.source.prepared_files(conversation),
            )
        });
        let count = i32::try_from(count).unwrap_or(i32::MAX);
        let done = i32::try_from(done).unwrap_or(i32::MAX).min(count);
        if *self.preparing_done() != done {
            self.as_mut().set_preparing_done(done);
        }
        if *self.preparing() != count {
            self.as_mut().set_preparing(count);
        }
    }

    /// Sends of files ended: the pasted files made for them are deleted once the core has
    /// its own copies (or will never take them).
    pub(crate) fn sends_finished(mut self: Pin<&mut Self>, finished: &[FinishedSend]) {
        if self.pasted_out.is_empty() {
            return;
        }
        let mut gone = Vec::new();
        for send in finished {
            gone.extend(
                self.as_mut()
                    .rust_mut()
                    .pasted_out
                    .finished(send.request, send.outcome),
            );
        }
        if let Some(handle) = &self.handle {
            for path in &gone {
                handle.release_pasted(path);
            }
        }
    }

    /// Adds files to the strip of the open conversation.
    pub(crate) fn push_files(mut self: Pin<&mut Self>, paths: Vec<PathBuf>, pasted: bool) {
        if self.as_mut().rust_mut().strip.add(paths, pasted) > 0 {
            self.strip_changed();
        }
    }

    /// A pasted picture was written for `conversation`, the chat that was open when the user
    /// pasted (pastes land here on the Qt thread). It joins that chat's list, also when the
    /// user has gone on to another chat meanwhile: it is there when that chat is opened.
    pub(crate) fn push_pasted(mut self: Pin<&mut Self>, conversation: i64, path: PathBuf) {
        let open = self.conversation == conversation;
        let added = self
            .as_mut()
            .rust_mut()
            .strip_of(conversation)
            .add([path], true);
        if open && added > 0 {
            self.strip_changed();
        }
    }

    /// Adds `urls` to the strip in one go.
    pub fn add_files(mut self: Pin<&mut Self>, urls: &QStringList) -> i32 {
        let all = QList::<QString>::from(urls);
        let paths: Vec<PathBuf> = all.iter().filter_map(local_path).collect();
        let skipped = usize::try_from(all.len()).unwrap_or(0) - paths.len();
        let added = paths.len();
        self.as_mut().push_files(paths, false);
        if skipped > 0 {
            // Not a refusal of a file: a link to something on the web is no file to send.
            self.as_mut().set_notice(QString::from(
                trnc(
                    "@info:status after files were dropped, %1 how many items",
                    "%1 dropped item is not a file on this computer and was left out.",
                    "%1 dropped items are not files on this computer and were left out.",
                    skipped as u64,
                )
                .as_str(),
            ));
        } else if added > 0 && !self.notice().is_empty() {
            self.as_mut().set_notice(QString::default());
        }
        i32::try_from(added).unwrap_or(i32::MAX)
    }

    /// Adds one file to the strip.
    pub fn add_file(self: Pin<&mut Self>, url: &QString) -> bool {
        let mut list = QStringList::default();
        list.append(url.clone());
        self.add_files(&list) > 0
    }

    /// Removes entry `index` of the strip (a pasted file is deleted).
    pub fn remove_attachment(mut self: Pin<&mut Self>, index: i32) {
        let Some(file) = usize::try_from(index)
            .ok()
            .and_then(|i| self.as_mut().rust_mut().strip.remove(i))
        else {
            return;
        };
        // (A folder that is being counted, listed or packed: that work stops.)
        self.as_mut()
            .rust_mut()
            .stop_jobs(std::slice::from_ref(&file));
        if file.pasted
            && let Some(h) = &self.handle
        {
            h.release_pasted(&file.path);
        }
        self.strip_changed();
    }

    /// Empties the strip (pasted files are deleted).
    pub fn clear_attachments(mut self: Pin<&mut Self>) {
        let files = self.as_mut().rust_mut().strip.clear();
        if files.is_empty() {
            return;
        }
        self.as_mut().rust_mut().stop_jobs(&files);
        if let Some(h) = &self.handle {
            for f in files.iter().filter(|f| f.pasted) {
                h.release_pasted(&f.path);
            }
        }
        self.strip_changed();
    }

    /// Adds what the clipboard holds: files copied in a file manager, else an image (its PNG
    /// is encoded off the Qt thread). `false`: neither, the text area pastes text.
    pub fn paste(mut self: Pin<&mut Self>) -> bool {
        let files = qobject::clipboard_files();
        if !files.is_empty() {
            return self.add_files(&files) > 0;
        }
        let Some(handle) = self.handle.clone() else {
            return false;
        };
        *PASTE.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((handle, self.qt_thread(), self.conversation));
        let started = qobject::paste_clipboard_image(pasted);
        if started {
            self.as_mut().set_notice(QString::default());
        }
        started
    }

    /// The text area holds a text or not (the caption of the files).
    pub fn set_has_text(mut self: Pin<&mut Self>, has_text: bool) {
        if self.has_text == has_text {
            return;
        }
        let mut rust = self.as_mut().rust_mut();
        rust.has_text = has_text;
        if !has_text {
            rust.long_caption = None;
        }
        self.replan();
    }

    /// What the text area holds (told when it rests). Only a caption longer than one message
    /// of the chat carries is kept: it goes first as several messages, and the plan counts
    /// them from its text.
    pub fn set_caption(mut self: Pin<&mut Self>, text: &QString) {
        let text = text.to_string();
        let has_text = !text.trim().is_empty();
        let conversation = ConversationId(self.conversation);
        let max = self
            .shared
            .as_ref()
            .and_then(|s| s.borrow().source.max_text_bytes(conversation))
            .and_then(|m| usize::try_from(m).ok());
        let long = max
            .is_some_and(|max| text.trim().len() > max)
            .then_some(text);
        if self.has_text == has_text && self.long_caption == long {
            return;
        }
        let mut rust = self.as_mut().rust_mut();
        rust.has_text = has_text;
        rust.long_caption = long;
        self.replan();
    }

    /// Chooses how entry `index` of the strip is sent: as the file it is, or as what it holds.
    pub fn set_original(mut self: Pin<&mut Self>, index: i32, original: bool) {
        let changed = usize::try_from(index)
            .is_ok_and(|i| self.as_mut().rust_mut().strip.set_as_file(i, original));
        if changed {
            self.strip_changed();
        }
    }

    /// Queues the strip's files that can be sent with `text` as their caption. The files the
    /// network cannot take stay in the strip with their reasons.
    fn send_files(
        mut self: Pin<&mut Self>,
        shared: &Shared,
        conversation: i64,
        text: &str,
    ) -> bool {
        if !self.strip.can_send() {
            // A quote the files cannot carry and no text for it: the strip's hint says so.
            let hint = self.strip.summary().hint;
            self.as_mut().set_notice(QString::from(hint.as_str()));
            return false;
        }
        let paths = self.strip.sendable();
        let as_file = self.strip.sendable_as_file();
        let reply = (*self.reply_to_id() > 0).then(|| MessageId(*self.reply_to_id()));
        let result = shared.borrow_mut().source.send_files(
            ConversationId(conversation),
            paths,
            as_file,
            text,
            reply,
        );
        match result {
            Ok(request) => {
                let sent = self.as_mut().rust_mut().strip.take_sendable();
                // (Folders that are not files or an archive yet stay too; they were not
                // refused, and their rows say where they stand.)
                let left = self.strip.summary().refused;
                // The core copies the pasted ones first: they are deleted when it says the
                // request is over (`sends_finished`).
                self.as_mut().rust_mut().pasted_out.sent(
                    request,
                    sent.iter().filter(|f| f.pasted).map(|f| f.path.clone()),
                );
                self.as_mut().cancel_reply();
                self.as_mut().strip_changed();
                self.as_mut().refresh_preparing();
                self.as_mut().set_error(QString::default());
                // What stayed behind is said once more where the user looks after a send.
                self.as_mut().set_notice(QString::from(
                    refused_notice(left, sent.len(), true).as_str(),
                ));
                defer(
                    shared,
                    self.app.as_deref(),
                    Changes {
                        timeline: true,
                        conversations: true,
                        ..Changes::default()
                    },
                );
                true
            }
            Err(SourceError::Empty) => false,
            Err(e) => {
                self.as_mut()
                    .set_error(QString::from(e.to_string().as_str()));
                false
            }
        }
    }

    /// Sends `text` to `conversation` (or the strip's files with `text` as their caption);
    /// `true` when the source accepted it.
    pub fn send(mut self: Pin<&mut Self>, conversation: i64, text: &QString) -> bool {
        let Some(shared) = self.shared.clone() else {
            return false;
        };
        if *self.edit_id() > 0 {
            return self.send_edit(&shared, conversation, &text.to_string());
        }
        // Files that can go are sent with the text as their caption. When none can, the
        // text goes by itself: files the network cannot take hold nothing back.
        if !self.strip.sendable().is_empty() {
            return self.send_files(&shared, conversation, &text.to_string());
        }
        let reply = (*self.reply_to_id() > 0).then(|| MessageId(*self.reply_to_id()));
        let body = text.to_string();
        let result =
            shared
                .borrow_mut()
                .source
                .send_text(ConversationId(conversation), &body, reply);
        match result {
            Ok(_) => {
                self.as_mut().cancel_reply();
                self.as_mut().set_error(QString::default());
                self.as_mut().set_notice(QString::default());
                defer(
                    &shared,
                    self.app.as_deref(),
                    Changes {
                        timeline: true,
                        conversations: true,
                        ..Changes::default()
                    },
                );
                true
            }
            Err(SourceError::Empty) => false,
            Err(e) => {
                self.as_mut()
                    .set_error(QString::from(e.to_string().as_str()));
                false
            }
        }
    }

    /// Replaces the text (or caption) of the message being edited with `text`.
    fn send_edit(mut self: Pin<&mut Self>, shared: &Shared, conversation: i64, text: &str) -> bool {
        let message = MessageId(*self.edit_id());
        let result =
            shared
                .borrow_mut()
                .source
                .edit_message(ConversationId(conversation), message, text);
        match result {
            // Sent, or nothing changed: either way the edit is over.
            Ok(_) => {
                self.as_mut().set_error(QString::default());
                self.as_mut().set_notice(QString::default());
                self.as_mut().leave_edit();
                defer(
                    shared,
                    self.app.as_deref(),
                    Changes {
                        timeline: true,
                        conversations: true,
                        ..Changes::default()
                    },
                );
                true
            }
            // A text cannot become empty: stay in edit mode.
            Err(SourceError::Empty) => false,
            // The window ended while the user typed: say so and keep their text.
            Err(SourceError::CannotSend(why)) => {
                self.as_mut().set_notice(QString::from(why.as_str()));
                false
            }
            Err(e) => {
                self.as_mut()
                    .set_error(QString::from(e.to_string().as_str()));
                false
            }
        }
    }

    /// Shows the reply banner for `message` of `conversation`.
    pub fn reply_to(mut self: Pin<&mut Self>, conversation: i64, message: i64) -> bool {
        let quote = self.shared.as_ref().and_then(|s| {
            s.borrow()
                .source
                .reply_target(ConversationId(conversation), MessageId(message))
        });
        let Some(quote) = quote else {
            return false;
        };
        // A reply and an edit exclude each other.
        self.as_mut().cancel_edit();
        self.as_mut()
            .set_reply_author(QString::from(quote.author.as_str()));
        self.as_mut()
            .set_reply_text(QString::from(quote.text.as_str()));
        self.as_mut()
            .set_reply_icon(QString::from(quote.icon.as_str()));
        self.as_mut()
            .set_reply_thumb(QString::from(quote.thumb_url.as_str()));
        self.as_mut().set_reply_to_id(message);
        // Whether files can carry the quote is part of the plan.
        self.replan();
        true
    }

    /// Hides the reply banner.
    pub fn cancel_reply(mut self: Pin<&mut Self>) {
        if *self.reply_to_id() != 0 {
            self.as_mut().set_reply_to_id(0);
            self.as_mut().set_reply_author(QString::default());
            self.as_mut().set_reply_text(QString::default());
            self.as_mut().set_reply_icon(QString::default());
            self.as_mut().set_reply_thumb(QString::default());
            self.replan();
        }
    }

    /// Starts editing `message` of `conversation`.
    pub fn start_edit(mut self: Pin<&mut Self>, conversation: i64, message: i64) -> bool {
        let target = self.shared.as_ref().and_then(|s| {
            s.borrow()
                .source
                .edit_target(ConversationId(conversation), MessageId(message))
        });
        let Some(target) = target else {
            return false;
        };
        if *self.edit_id() == message {
            return true;
        }
        // Another edit in progress gives the text area back first.
        self.as_mut().cancel_edit();
        self.as_mut().cancel_reply();
        let text = QString::from(target.text.as_str());
        self.as_mut().set_edit_caption(target.caption);
        self.as_mut().set_edit_text(text.clone());
        self.as_mut().set_edit_id(message);
        self.edit_started(text);
        true
    }

    /// Starts editing the newest own message of `conversation` that can be edited.
    pub fn edit_last(self: Pin<&mut Self>, conversation: i64) -> bool {
        let last = self.shared.as_ref().and_then(|s| {
            s.borrow()
                .source
                .last_editable(ConversationId(conversation))
        });
        match last {
            Some(message) => self.start_edit(conversation, message.0),
            None => false,
        }
    }

    fn leave_edit(mut self: Pin<&mut Self>) {
        if *self.edit_id() == 0 {
            return;
        }
        self.as_mut().set_edit_id(0);
        self.as_mut().set_edit_caption(false);
        self.as_mut().set_edit_text(QString::default());
        self.edit_finished();
    }

    /// Leaves edit mode without changing the message.
    pub fn cancel_edit(self: Pin<&mut Self>) {
        self.leave_edit();
    }

    /// Files of a `SendFiles` the core did not take. The ones it named come back marked with
    /// their reasons (the others went out without them). A request refused as a whole comes
    /// back with its quote, unless the user is already composing something else in that chat
    /// (its caption returned as the draft). Files for a chat that is not open go back into
    /// that chat's list: they are there, with their reasons, when the user opens it.
    pub(crate) fn restore_refused(mut self: Pin<&mut Self>, refused: RefusedSend, open: i64) {
        let named = refused.files.iter().any(|f| f.reason.is_some());
        let here = refused.conv.0 == open;
        let busy = !named && here && (!self.strip.is_empty() || *self.edit_id() != 0);
        let returning: HashSet<PathBuf> = if busy {
            HashSet::new()
        } else {
            refused.paths().cloned().collect()
        };
        // The pasted files of the request: what comes back belongs to a list again, what
        // went out again waits for that request, anything else is no longer needed.
        let mut gone = self.as_mut().rust_mut().pasted_out.refused(
            refused.request,
            &returning,
            refused.resent_request,
        );
        if busy {
            // Not restored: pasted files of that send are no longer needed.
            gone.extend(refused.paths().cloned());
        }
        if let Some(h) = &self.handle {
            for path in &gone {
                h.release_pasted(path);
            }
        }
        if busy {
            return;
        }
        let is_pasted = |path: &PathBuf| self.handle.as_ref().is_some_and(|h| h.is_pasted(path));
        let mut marked = Vec::new();
        let mut whole = Vec::new();
        for file in refused.files {
            let handed = HandedFile {
                pasted: is_pasted(&file.path),
                as_file: file.as_file,
                path: file.path,
            };
            match file.reason {
                Some(reason) => marked.push((handed, reason)),
                None => whole.push(handed),
            }
        }
        {
            let mut rust = self.as_mut().rust_mut();
            let strip = if here {
                &mut rust.strip
            } else {
                rust.strip_of(refused.conv.0)
            };
            strip.restore_refused(marked);
            strip.add_handed(whole);
        }
        if !here {
            return;
        }
        self.as_mut().strip_changed();
        if let Some(reply) = refused.reply_to
            && *self.reply_to_id() == 0
        {
            self.reply_to(open, reply.0);
        }
    }

    /// Stores the text of a conversation the composer leaves.
    pub fn save_draft(self: Pin<&mut Self>, conversation: i64, text: &QString) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        if conversation == 0 {
            return;
        }
        shared
            .borrow_mut()
            .source
            .save_draft(ConversationId(conversation), &text.to_string());
        defer(
            &shared,
            self.app.as_deref(),
            Changes {
                conversations: true,
                ..Changes::default()
            },
        );
    }

    /// The draft of `conversation`.
    pub fn draft(&self, conversation: i64) -> QString {
        self.shared.as_ref().map_or_else(QString::default, |s| {
            QString::from(
                s.borrow()
                    .source
                    .draft(ConversationId(conversation))
                    .as_str(),
            )
        })
    }

    /// The user is typing (`true`) or stopped (`false`) in `conversation`.
    pub fn set_typing(self: Pin<&mut Self>, conversation: i64, on: bool) {
        if let Some(shared) = self.shared.clone()
            && conversation != 0
        {
            shared
                .borrow_mut()
                .source
                .set_typing(ConversationId(conversation), on);
        }
    }

    /// Hides the notice.
    pub fn clear_notice(mut self: Pin<&mut Self>) {
        if let Some(shared) = self.shared.clone() {
            shared.borrow_mut().source.clear_notice();
        }
        if !self.notice().is_empty() {
            self.as_mut().set_notice(QString::default());
        }
    }
}

/// Rust side of `LoginController`.
pub struct LoginRust {
    pub(crate) network: QString,
    pub(crate) kind: QString,
    pub(crate) title: QString,
    pub(crate) hint: QString,
    pub(crate) qr: QString,
    pub(crate) code: QString,
    pub(crate) progress: f64,
    pub(crate) error: QString,
    pub(crate) retry: bool,
    pub(crate) leave: QString,
    pub(crate) fields: QVariant,
    pub(crate) shared: Option<Shared>,
    pub(crate) app: Option<Box<CxxQtThread<App>>>,
    /// Values typed into the current input step, by field id.
    values: Vec<(String, String)>,
    /// `(step id, txn)` of the shown step: new steps reset the typed values.
    step: (String, u64),
}

impl Default for LoginRust {
    fn default() -> Self {
        Self {
            network: QString::default(),
            kind: QString::from("waiting"),
            title: QString::default(),
            hint: QString::default(),
            qr: QString::default(),
            code: QString::default(),
            progress: -1.0,
            error: QString::default(),
            retry: false,
            leave: QString::default(),
            fields: QVariant::default(),
            shared: None,
            app: None,
            values: Vec::new(),
            step: (String::new(), 0),
        }
    }
}

fn fields_variant(v: &LoginView) -> QVariant {
    let mut list = QList::<QVariant>::default();
    for f in &v.fields {
        let mut m = QMap::<QMapPair_QString_QVariant>::default();
        let s = |t: &str| QVariant::from(&QString::from(t));
        m.insert(QString::from("id"), s(&f.id));
        m.insert(QString::from("label"), s(&f.label));
        m.insert(QString::from("kind"), s(&f.kind));
        m.insert(QString::from("value"), s(&f.value));
        m.insert(QString::from("required"), QVariant::from(&f.required));
        list.append(QVariant::from(&m));
    }
    QVariant::from(&list)
}

impl qobject::LoginController {
    /// Starts linking on `network`.
    pub fn start(mut self: Pin<&mut Self>, network: &QString) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let name = network.to_string();
        let view = match NetworkKind::from_str(&name) {
            Ok(kind) => shared.borrow_mut().source.start_login(kind),
            Err(_) => Err(SourceError::Unavailable),
        };
        self.as_mut().set_network(network.clone());
        match view {
            Ok(v) => self.show(&v),
            Err(e) => self.show(&LoginView {
                kind: LoginViewKind::Failed,
                title: trc("@title:window", "Linking Failed").into_owned(),
                error: match e {
                    SourceError::Unavailable => trc(
                        "@info:status",
                        "Linking is not available in this build yet.",
                    )
                    .into_owned(),
                    other => other.to_string(),
                },
                retry: true,
                progress: -1.0,
                ..LoginView::default()
            }),
        }
    }

    /// Sets the value of an input field of the current step.
    pub fn set_field(mut self: Pin<&mut Self>, id: &QString, value: &QString) {
        let (id, value) = (id.to_string(), value.to_string());
        let mut rust = self.as_mut().rust_mut();
        match rust.values.iter_mut().find(|(k, _)| *k == id) {
            Some(entry) => entry.1 = value,
            None => rust.values.push((id, value)),
        }
    }

    /// Answers the current input step with the field values.
    pub fn submit(mut self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let values = self.values.clone();
        let result = shared.borrow_mut().source.submit_login(values);
        match result {
            Ok(()) => {
                // Waiting for the next step.
                self.as_mut().set_kind(QString::from("waiting"));
            }
            Err(e) => {
                self.as_mut()
                    .set_error(QString::from(e.to_string().as_str()));
            }
        }
    }

    /// Abandons the login.
    pub fn cancel(self: Pin<&mut Self>) {
        if let Some(shared) = self.shared.clone() {
            shared.borrow_mut().source.cancel_login();
            // The link page no longer shows a history transfer of the account: the banner
            // takes it over.
            defer(
                &shared,
                self.app.as_deref(),
                Changes {
                    transfer: true,
                    ..Changes::default()
                },
            );
        }
        self.show(&LoginView {
            progress: -1.0,
            ..LoginView::default()
        });
    }

    /// Shows a login step.
    pub fn show(mut self: Pin<&mut Self>, v: &LoginView) {
        let step = (v.step_id.clone(), v.txn);
        if self.step != step {
            let mut rust = self.as_mut().rust_mut();
            rust.step = step;
            rust.values = v
                .fields
                .iter()
                .map(|f| (f.id.clone(), f.value.clone()))
                .collect();
        }
        self.as_mut().set_title(QString::from(v.title.as_str()));
        self.as_mut().set_hint(QString::from(v.hint.as_str()));
        self.as_mut().set_qr(QString::from(v.qr.as_str()));
        self.as_mut().set_code(QString::from(v.code.as_str()));
        self.as_mut().set_progress(v.progress);
        self.as_mut().set_error(QString::from(v.error.as_str()));
        self.as_mut().set_retry(v.retry);
        self.as_mut().set_leave(QString::from(v.leave.as_str()));
        self.as_mut().set_fields(fields_variant(v));
        // `kind` last: QML switches on it and then reads the fields above.
        self.set_kind(QString::from(v.kind.as_str()));
    }
}

#[cfg(debug_assertions)]
impl qobject::LoginController {
    /// Development control (`fill_login`): sets field values of the current input step, by id
    /// or in field order, and shows them in the form.
    pub(crate) fn dev_fill(
        mut self: Pin<&mut Self>,
        values: &crate::dev::protocol::LoginValues,
    ) -> Result<(), String> {
        use crate::dev::protocol::LoginValues;
        let ids: Vec<String> = self.values.iter().map(|(id, _)| id.clone()).collect();
        let pairs: Vec<(String, String)> = match values {
            LoginValues::ById(pairs) => {
                if let Some((id, _)) = pairs.iter().find(|(id, _)| !ids.contains(id)) {
                    return Err(format!("the login step has no field {id:?}"));
                }
                pairs.clone()
            }
            LoginValues::InOrder(list) => {
                if list.len() > ids.len() {
                    return Err(format!(
                        "{} values for {} fields of the login step",
                        list.len(),
                        ids.len()
                    ));
                }
                ids.iter().cloned().zip(list.iter().cloned()).collect()
            }
        };
        for (id, value) in &pairs {
            self.as_mut()
                .set_field(&QString::from(id.as_str()), &QString::from(value.as_str()));
        }
        // Show the typed values: the form's text fields are built from `fields`.
        let view = self
            .shared
            .as_ref()
            .and_then(|s| s.borrow().source.login_view());
        if let Some(mut view) = view {
            for f in &mut view.fields {
                if let Some((_, v)) = self.values.iter().find(|(id, _)| *id == f.id) {
                    f.value.clone_from(v);
                }
            }
            self.as_mut().set_fields(fields_variant(&view));
        }
        Ok(())
    }
}

/// Rust side of `SettingsController`.
pub struct SettingsRust {
    pub(crate) tray: bool,
    pub(crate) autostart: bool,
    pub(crate) start_hidden: bool,
    pub(crate) compact: bool,
    pub(crate) round_avatars: bool,
    /// The theme's avatar corner as a share of a list avatar's side, as QML last said (not
    /// saved: the theme is the front-end's).
    pub(crate) avatar_corner: f64,
    pub(crate) notification_content: i32,
    /// What happens to disappearing messages whose time has come while this computer's
    /// clock is unconfirmed: the core's name of the option (`hide`, `keep`, `erase`); empty
    /// until the core has said.
    pub(crate) expiry_policy: QString,
    /// Where changes are saved and applied (none in pure UI tests).
    pub(crate) desktop: Option<DesktopHandle>,
    /// The core's own settings are read and changed through the source.
    pub(crate) shared: Option<Shared>,
    pub(crate) app: Option<Box<CxxQtThread<App>>>,
}

impl Default for SettingsRust {
    fn default() -> Self {
        Self::from_prefs(&Prefs::default(), None)
    }
}

impl SettingsRust {
    /// The controller state for `prefs`.
    pub(crate) fn from_prefs(prefs: &Prefs, desktop: Option<DesktopHandle>) -> Self {
        Self {
            tray: prefs.tray,
            autostart: prefs.autostart,
            start_hidden: prefs.start_hidden,
            compact: prefs.compact,
            round_avatars: prefs.round_avatars,
            avatar_corner: 0.0,
            notification_content: prefs.notification_content.index(),
            expiry_policy: QString::default(),
            desktop,
            shared: None,
            app: None,
        }
    }

    fn prefs(&self) -> Prefs {
        Prefs {
            tray: self.tray,
            autostart: self.autostart,
            start_hidden: self.start_hidden,
            compact: self.compact,
            round_avatars: self.round_avatars,
            notification_content: NotificationContent::from_index(self.notification_content),
        }
    }
}

impl qobject::SettingsController {
    /// Stores the change, tells QML and hands the preferences to the desktop integration.
    fn update(mut self: Pin<&mut Self>, f: impl FnOnce(&mut SettingsRust) -> bool) {
        if !f(&mut self.as_mut().rust_mut()) {
            return;
        }
        if let Some(desktop) = &self.desktop {
            desktop.set_prefs(self.prefs());
        }
        self.changed();
    }

    /// `tray`.
    pub fn set_tray_pref(self: Pin<&mut Self>, value: bool) {
        self.update(|s| std::mem::replace(&mut s.tray, value) != value);
    }

    /// `autostart`.
    pub fn set_autostart_pref(self: Pin<&mut Self>, value: bool) {
        self.update(|s| std::mem::replace(&mut s.autostart, value) != value);
    }

    /// `startHidden`.
    pub fn set_start_hidden_pref(self: Pin<&mut Self>, value: bool) {
        self.update(|s| std::mem::replace(&mut s.start_hidden, value) != value);
    }

    /// `compact`.
    pub fn set_compact_pref(self: Pin<&mut Self>, value: bool) {
        self.update(|s| std::mem::replace(&mut s.compact, value) != value);
    }

    /// `roundAvatars`. Notification avatars read it from the preferences at each use.
    pub fn set_round_avatars_pref(self: Pin<&mut Self>, value: bool) {
        self.update(|s| std::mem::replace(&mut s.round_avatars, value) != value);
    }

    /// `avatarCorner`: kept for QML and handed to the notification avatars. Not a preference,
    /// so the desktop integration is not told.
    pub fn set_avatar_corner_pref(mut self: Pin<&mut Self>, value: f64) {
        if (self.avatar_corner - value).abs() < 1e-6 {
            return;
        }
        self.as_mut().rust_mut().avatar_corner = value;
        crate::desktop::set_theme_corner(value);
        self.changed();
    }

    /// `notificationContent` (0 name and message, 1 name only, 2 nothing).
    pub fn set_notification_content_pref(self: Pin<&mut Self>, value: i32) {
        let value = NotificationContent::from_index(value).index();
        self.update(|s| std::mem::replace(&mut s.notification_content, value) != value);
    }

    /// The settings page is shown: what the core keeps is asked for (`expiryPolicy` follows
    /// when the answer is in). Nothing is read for the page before that.
    pub fn load(self: Pin<&mut Self>) {
        if let Some(shared) = self.shared.clone() {
            shared.borrow_mut().source.load_settings();
        }
    }

    /// `expiryPolicy`: chooses the option by the core's name for it. It holds at once and
    /// the core stores it; a name the core does not know changes nothing.
    pub fn set_expiry_policy_pref(mut self: Pin<&mut Self>, value: &QString) {
        if *value == self.expiry_policy {
            return;
        }
        let Some(shared) = self.shared.clone() else {
            return;
        };
        if !shared
            .borrow_mut()
            .source
            .set_expiry_policy(&value.to_string())
        {
            // Not taken: the page shows what holds.
            self.changed();
            return;
        }
        self.as_mut().rust_mut().expiry_policy = value.clone();
        self.as_mut().changed();
        // What the open chat and the list show follows the option.
        defer(
            &shared,
            self.app.as_deref(),
            Changes {
                timeline: true,
                conversations: true,
                ..Changes::default()
            },
        );
    }

    /// What the core says the option is (its answer, or its word after a change failed).
    pub(crate) fn show_expiry_policy(mut self: Pin<&mut Self>, name: Option<&'static str>) {
        let name = QString::from(name.unwrap_or_default());
        if self.expiry_policy != name {
            self.as_mut().rust_mut().expiry_policy = name;
            self.changed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_file_stays_until_the_core_answers_its_send() {
        let p = |name: &str| PathBuf::from(format!("/run/paste/{name}"));
        let mut out = PastedOut::default();
        out.sent(1, [p("a.png"), p("b.png")]);
        out.sent(2, [p("c.png")]);
        // Hundreds of files, or a few large ones, take the core as long as they take: nothing
        // is deleted by time. Another request's answer leaves these alone.
        assert_eq!(out.finished(7, SendOutcome::Stored), Vec::<PathBuf>::new());
        // Stored: the core has its own copies, the pasted ones go.
        assert_eq!(
            out.finished(1, SendOutcome::Stored),
            vec![p("a.png"), p("b.png")]
        );
        assert_eq!(out.finished(1, SendOutcome::Stored), Vec::<PathBuf>::new());
        // An answer lost with lagged events: the core may still be copying, so the file
        // stays on disk; it is only no longer tracked.
        assert_eq!(out.finished(2, SendOutcome::Unknown), Vec::<PathBuf>::new());
        assert!(out.is_empty());
    }

    #[test]
    fn what_arrives_late_goes_to_the_chat_it_was_for() {
        let p = |name: &str| PathBuf::from(format!("/t/{name}"));
        let names = |strip: &FileStrip| -> Vec<String> {
            strip.files().iter().map(|f| f.name.clone()).collect()
        };
        let mut c = ComposerRust::default();
        assert!(c.switch_to(7).is_empty());
        c.strip.add([p("a.png")], false);
        // The user pastes a picture in chat 7 and opens chat 9 before it is written.
        assert!(c.switch_to(9).is_empty());
        assert!(c.strip.is_empty(), "chat 9 has its own, empty list");
        c.strip.add([p("for-nine.pdf")], false);
        assert_eq!(c.strip_of(7).add([p("pasted.png")], true), 1);
        assert_eq!(names(&c.strip), ["for-nine.pdf"], "not in the open chat");
        // Back in chat 7 both are there, the pasted one marked as such (it is deleted
        // when it leaves the list).
        assert!(c.switch_to(7).is_empty());
        assert_eq!(names(&c.strip), ["a.png", "pasted.png"]);
        assert!(c.strip.files()[1].pasted && !c.strip.files()[0].pasted);
        // Chat 9 kept its own; a chat left with nothing waiting keeps no list.
        assert_eq!(c.parked.len(), 1);
        c.strip.clear();
        assert!(c.switch_to(9).is_empty());
        assert_eq!(names(&c.strip), ["for-nine.pdf"]);
        assert!(c.parked.is_empty());
        // For the open chat it is the open list; for a chat without a list one is made,
        // with ids of its own (an answer about one chat's file never fits another's).
        assert_eq!(c.strip_of(9).len(), 1);
        c.strip_of(11).add([p("late.png")], true);
        let (here, there) = (c.strip.files()[0].id, c.strip_of(11).files()[0].id);
        assert_ne!(here >> 32, there >> 32);
        // Without a chat there is nowhere to keep files: they are handed back to be dropped.
        assert!(c.switch_to(0).is_empty());
        c.strip.add([p("nowhere.png")], true);
        let dropped = c.switch_to(11);
        assert_eq!(dropped.len(), 1);
        assert!(dropped[0].pasted);
        assert_eq!(names(&c.strip), ["late.png"]);
    }

    #[test]
    fn files_waiting_for_a_merged_chat_follow_the_merge_and_two_lists_join() {
        let p = |name: &str| PathBuf::from(format!("/t/{name}"));
        let names = |strip: &FileStrip| -> Vec<String> {
            strip.files().iter().map(|f| f.name.clone()).collect()
        };
        // Chats 7 and 9 have files waiting; chat 11 is the open one, with none.
        let mut c = ComposerRust::default();
        c.switch_to(7);
        c.strip.add([p("seven.pdf")], false);
        c.switch_to(9);
        c.strip.add([p("nine-a.png"), p("nine-b.png")], true);
        c.switch_to(11);
        assert_eq!(c.parked.len(), 2);

        // 7 is merged into 9, neither is open: 9's list has its own files first, then 7's.
        // The open list is not touched.
        assert!(!c.merge(7, 9));
        assert!(
            !c.parked.contains_key(&7),
            "nothing waits for a chat that is gone"
        );
        assert_eq!(
            names(&c.parked[&9]),
            ["nine-a.png", "nine-b.png", "seven.pdf"]
        );
        assert!(c.parked[&9].files()[0].pasted && !c.parked[&9].files()[2].pasted);
        assert!(c.strip.is_empty());
        // The entries are the same ones (answers by id still find them), in one list.
        let ids: Vec<u64> = c.parked[&9].files().iter().map(|f| f.id).collect();
        assert_eq!(
            ids.iter().collect::<HashSet<_>>().len(),
            3,
            "no two entries share an id"
        );
        assert!(c.strip_with(ids[2]).is_some_and(|(_, open)| !open));

        // 9 is merged into the open chat 11, which has a file of its own by now: one list,
        // the open chat's file first. That list changed.
        c.strip.add([p("eleven.txt")], false);
        assert!(c.merge(9, 11));
        assert_eq!(
            names(&c.strip),
            ["eleven.txt", "nine-a.png", "nine-b.png", "seven.pdf"]
        );
        assert!(c.parked.is_empty());
        assert_eq!(c.conversation, 11);
        assert!(c.strip_with(ids[2]).is_some_and(|(_, open)| open));

        // The open chat itself is merged away, into a chat with nothing waiting: its list
        // goes on under the surviving id, unchanged.
        assert!(!c.merge(11, 20));
        assert_eq!(c.conversation, 20);
        assert_eq!(names(&c.strip).len(), 4);
        // ... and into a chat that has files waiting: joined, the survivor's first.
        c.switch_to(30);
        c.strip.add([p("thirty.png")], false);
        c.switch_to(20);
        assert!(c.merge(20, 30));
        assert_eq!(c.conversation, 30);
        assert_eq!(
            names(&c.strip),
            [
                "thirty.png",
                "eleven.txt",
                "nine-a.png",
                "nine-b.png",
                "seven.pdf"
            ]
        );
        assert!(c.parked.is_empty(), "one list for the one chat");
        // A chat that only gets a new id while nothing else waits for it keeps its list,
        // and coming back to the surviving chat finds it.
        c.switch_to(40);
        assert!(!c.merge(30, 31));
        assert_eq!(names(&c.parked[&31]).len(), 5);
        c.switch_to(31);
        assert_eq!(names(&c.strip).len(), 5);
        // Merges that are about nothing change nothing.
        assert!(!c.merge(31, 31));
        assert!(!c.merge(0, 31) && !c.merge(31, 0));
        assert!(!c.merge(77, 78));
        assert_eq!((c.conversation, c.parked.len()), (31, 0));
    }

    #[test]
    fn work_on_a_folder_stops_when_its_entry_leaves() {
        let mut c = ComposerRust::default();
        c.switch_to(7);
        c.strip.add(
            [PathBuf::from("/t/Folder"), PathBuf::from("/t/a.png")],
            false,
        );
        let (folder, file) = (c.strip.files()[0].id, c.strip.files()[1].id);
        c.strip.set_facts(&[(
            folder,
            FileFacts {
                size: None,
                picture: false,
                folder: true,
            },
        )]);
        let (counting, other) = (Cancel::new(), Cancel::new());
        c.jobs.insert(folder, counting.clone());
        c.jobs.insert(999, other.clone());
        // A file that leaves stops nothing.
        let gone = c.strip.remove_id(file).expect("the file");
        c.stop_jobs(std::slice::from_ref(&gone));
        assert!(!counting.is_cancelled());
        // The folder leaves: its job is told to stop, and only its job.
        let gone = c.strip.remove_id(folder).expect("the folder");
        c.stop_jobs(std::slice::from_ref(&gone));
        assert!(counting.is_cancelled());
        assert!(!other.is_cancelled());
        assert!(!c.jobs.contains_key(&folder) && c.jobs.contains_key(&999));
    }

    #[test]
    fn pasted_files_of_a_refused_send_follow_where_they_went() {
        let p = |name: &str| PathBuf::from(format!("/run/paste/{name}"));
        let mut out = PastedOut::default();
        out.sent(1, [p("good.png"), p("bad.png"), p("also-good.png")]);
        // The core named one file: it is back in the list (the list looks after it), the
        // others went out again as request 2 and must stay until that one is answered.
        let back: HashSet<PathBuf> = [p("bad.png")].into();
        assert_eq!(out.refused(1, &back, Some(2)), Vec::<PathBuf>::new());
        assert_eq!(out.finished(1, SendOutcome::Refused), Vec::<PathBuf>::new());
        assert_eq!(
            out.finished(2, SendOutcome::Stored),
            vec![p("good.png"), p("also-good.png")]
        );
        assert!(out.is_empty());
        // A request refused as a whole: everything comes back, nothing is deleted.
        out.sent(3, [p("x.png")]);
        let back: HashSet<PathBuf> = [p("x.png")].into();
        assert_eq!(out.refused(3, &back, None), Vec::<PathBuf>::new());
        assert!(out.is_empty());
        // Refused and not taken back (the user composes something else): no longer needed.
        out.sent(4, [p("y.png")]);
        assert_eq!(out.refused(4, &HashSet::new(), None), vec![p("y.png")]);
        assert!(out.is_empty());
    }
}

//! The Qt-thread half of the development control channel: what each command does to the UI.
//! Navigation goes through the root window's own QML functions (`openLink`,
//! `openConversation`, `showList`, …), so the page stack ends up exactly where a click would
//! leave it.

use std::path::{Path, PathBuf};
use std::pin::Pin;

use cxx_qt_lib::{QString, QStringList};
use kourier_app::{AppHandle, RehearsalAct, RehearsalKind};
use kourier_viewmodel::{AccountStatus, Receipt, RowKey, RowKind};
use serde_json::{Value, json};

use super::DevError;
use super::guard;
use super::protocol::{ActOn, Action, LoginValues, PageName, Until, receipt_name, receipt_rank};
use crate::app::pinned;
use crate::bridge::qobject::{self, QObject};
use crate::dev_ffi::qt as ffi;
use crate::models::ListModel as _;

fn window() -> Result<*mut QObject, DevError> {
    let w = ffi::main_window();
    if w.is_null() {
        Err(DevError::NotFound(
            "the main window does not exist yet".into(),
        ))
    } else {
        Ok(w)
    }
}

fn call(method: &str) -> Result<(), DevError> {
    let w = window()?;
    // SAFETY: `w` is the live root window (Qt thread).
    if unsafe { ffi::invoke_qml(w, &QString::from(method)) } {
        Ok(())
    } else {
        Err(DevError::Failed(format!(
            "the window has no QML function {method}()"
        )))
    }
}

fn status_name(s: AccountStatus) -> &'static str {
    match s {
        AccountStatus::Online => "online",
        AccountStatus::Connecting => "connecting",
        AccountStatus::Offline => "offline",
        AccountStatus::NeedsAction => "action",
        AccountStatus::LoggedOut => "logged_out",
    }
}

impl qobject::App {
    /// `add_account`: what clicking the network on the welcome page does.
    pub(crate) fn dev_add_account(self: Pin<&mut Self>, network: &str) -> Result<Value, DevError> {
        let w = window()?;
        // SAFETY: `w` is the live root window (Qt thread).
        let done = unsafe {
            ffi::invoke_qml_string(w, &QString::from("openLink"), &QString::from(network))
        };
        if !done {
            return Err(DevError::Failed(
                "the window has no openLink(string)".into(),
            ));
        }
        let kind = pinned(self.login).map(|l| l.kind().to_string());
        Ok(json!({"kind": kind}))
    }

    /// `fill_login` / `submit_login`: types values into the current step's fields (the form
    /// shows them) and optionally presses Continue.
    pub(crate) fn dev_login(
        self: Pin<&mut Self>,
        values: Option<&LoginValues>,
        submit: bool,
    ) -> Result<Value, DevError> {
        let Some(mut login) = pinned(self.login) else {
            return Err(DevError::Gone);
        };
        let kind = login.kind().to_string();
        if kind != "input" {
            return Err(DevError::Failed(format!(
                "the login shows no input step (kind {kind:?})"
            )));
        }
        if let Some(values) = values {
            login.as_mut().dev_fill(values).map_err(DevError::Failed)?;
        }
        if submit {
            login.as_mut().submit();
            // A refused submit (a required field empty, the backend gone) stays on the step.
            let shown = login.kind().to_string();
            if shown != "waiting" {
                let why = login.error().to_string();
                return Err(DevError::Failed(if why.is_empty() {
                    format!("the login step was not submitted (kind {shown:?})")
                } else {
                    why
                }));
            }
        }
        Ok(json!({"kind": login.kind().to_string()}))
    }

    /// `open`: the first conversation whose title contains `needle` (case-insensitive).
    pub(crate) fn dev_open(self: Pin<&mut Self>, needle: &str) -> Result<Value, DevError> {
        let needle = needle.to_lowercase();
        let id = pinned(self.conversations).and_then(|m| {
            m.store()
                .view
                .iter()
                .find(|c| c.title.to_lowercase().contains(&needle))
                .map(|c| c.id.0)
        });
        let Some(id) = id else {
            return Err(DevError::NotFound(
                "no conversation title contains that text".into(),
            ));
        };
        open_in_window(id)?;
        Ok(json!({"conversation": id}))
    }

    /// `open_id`: the conversation with this id, when the list has it.
    pub(crate) fn dev_open_id(self: Pin<&mut Self>, id: i64) -> Result<Value, DevError> {
        let listed =
            pinned(self.conversations).is_some_and(|m| m.store().view.iter().any(|c| c.id.0 == id));
        if !listed {
            return Err(DevError::NotFound(
                "the conversation list has no such conversation".into(),
            ));
        }
        open_in_window(id)?;
        Ok(json!({"conversation": id}))
    }

    /// `chats`: the conversation list as marks (ids, kinds, counts and states; no titles and
    /// no previews).
    pub(crate) fn dev_chats(self: Pin<&mut Self>) -> Value {
        use kourier_model::ConvKind;
        let chats: Vec<Value> = pinned(self.conversations)
            .map(|m| {
                m.store()
                    .view
                    .iter()
                    .map(|c| {
                        json!({
                            "conversation": c.id.0,
                            "kind": match c.kind {
                                ConvKind::Direct => "direct",
                                ConvKind::Group => "group",
                                ConvKind::Broadcast | ConvKind::Channel => "other",
                            },
                            "self": c.is_self,
                            "unread": c.unread,
                            "last_activity": c.last_activity,
                            "newest_own": receipt_name(c.receipt),
                            // The phone's chat settings as the SHOWN list has them (T2,
                            // T-1): a pinned chat, a muted one (a synced or a local mute
                            // that has not run out), one marked unread. `archived` is not
                            // here: this view is the inbox, so no row of it is archived;
                            // the core's own row is `chat_facts` (M12).
                            "pinned": c.pinned(),
                            "muted": c.muted,
                            "marked_unread": c.marked_unread,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        json!({"chats": chats})
    }

    /// `page`: shows a page through the window's navigation functions.
    pub(crate) fn dev_page(self: Pin<&mut Self>, name: PageName) -> Result<Value, DevError> {
        match name {
            PageName::Welcome => call("openWelcome")?,
            PageName::List => call("showList")?,
            PageName::Settings => call("openSettings")?,
            PageName::Chat => {
                let id = pinned(self.timeline).map_or(0, |t| *t.conversation_id());
                if id == 0 {
                    return Err(DevError::NotFound("no conversation is open".into()));
                }
                open_in_window(id)?;
            }
            PageName::Drawer => {
                let w = window()?;
                // SAFETY: `w` is the live root window; the drawer is its child (Qt thread).
                unsafe {
                    let drawer = ffi::object_property(w, &QString::from("globalDrawer"));
                    if drawer.is_null() {
                        return Err(DevError::NotFound("the window has no drawer".into()));
                    }
                    // Wide: the collapsed sidebar expands; narrow: the modal drawer opens.
                    ffi::set_bool_property(drawer, &QString::from("collapsed"), false);
                    ffi::invoke_qml(drawer, &QString::from("open"));
                }
            }
        }
        Ok(json!({}))
    }

    /// `attach`: adds files to the composer's strip in one go, the way the file dialog or a
    /// drop does (any number, of any kinds). What the conversation makes of them (how many
    /// messages, which files it cannot take) shows in `state.composer` once the core has
    /// looked at them (`wait` for `files_planned`).
    pub(crate) fn dev_attach(self: Pin<&mut Self>, paths: &[PathBuf]) -> Result<Value, DevError> {
        let Some(timeline) = pinned(self.timeline) else {
            return Err(DevError::Gone);
        };
        if *timeline.conversation_id() == 0 {
            return Err(DevError::NotFound("no conversation is open".into()));
        }
        let mut urls = QStringList::default();
        for path in paths {
            let Some(text) = path.to_str() else {
                return Err(DevError::Failed("a path is not UTF-8".into()));
            };
            urls.append(QString::from(text));
        }
        let Some(mut composer) = pinned(self.composer) else {
            return Err(DevError::Gone);
        };
        let added = composer.as_mut().add_files(&urls);
        Ok(json!({"added": added, "attachments": *composer.attachment_count()}))
    }

    /// `challenge`: answers the challenge an account waits on, through the invokable the
    /// challenge dialog uses. Returns the account.
    pub(crate) fn dev_challenge(self: Pin<&mut Self>, answer: &str) -> Result<Value, DevError> {
        let Some(account) = self.challenged_account() else {
            return Err(DevError::NotFound(
                "no account waits for a challenge answer".into(),
            ));
        };
        self.submit_challenge(account.0, &QString::from(answer));
        Ok(json!({"account": account.0}))
    }

    /// `act`: what the user does to a message of the open conversation, through the same
    /// invokables the timeline's menu and the composer use. Returns the message's id.
    ///
    /// `conversation` and `message` are the driver's guards (C7, [`super::guard`]): checked here, on
    /// the Qt thread, in the call that acts, a chat that is not the open one or a message that is
    /// not one of its live messages of the kind `on` names is refused with nothing done.
    pub(crate) fn dev_act(
        mut self: Pin<&mut Self>,
        on: ActOn,
        action: &Action,
        conversation: Option<i64>,
        message: Option<i64>,
    ) -> Result<Value, DevError> {
        let conv = pinned(self.timeline).map_or(0, |t| *t.conversation_id());
        if conv == 0 && conversation.is_none() {
            return Err(DevError::NotFound("no conversation is open".into()));
        }
        guard::the_named_chat_is_open(conv, conversation)?;
        let live: Vec<(Option<i64>, bool)> = pinned(self.timeline)
            .map(|t| {
                t.store()
                    .view
                    .iter()
                    .filter(|r| r.kind == RowKind::Message && !r.deleted)
                    .map(|r| (r.message_id().map(|id| id.0), r.own))
                    .collect()
            })
            .unwrap_or_default();
        let message = guard::the_message_to_act_on(live, on, message)?;
        let refused = |why: &str| Err(DevError::Failed(why.to_owned()));
        match action {
            Action::React { emoji, remove } => {
                self.as_mut().react(message, &QString::from(emoji), *remove);
            }
            Action::Delete { for_everyone } => {
                self.as_mut().delete_message(message, *for_everyone);
            }
            Action::Reply { text } => {
                let Some(mut composer) = pinned(self.composer) else {
                    return Err(DevError::Gone);
                };
                if !composer.as_mut().reply_to(conv, message) {
                    return refused("the message cannot be replied to");
                }
                if !composer.as_mut().send(conv, &QString::from(text)) {
                    return refused("the composer did not send the reply");
                }
            }
            Action::Edit { text } => {
                let Some(mut composer) = pinned(self.composer) else {
                    return Err(DevError::Gone);
                };
                // A driver that named a chat edits from a plain composer only (F3): the user's reply, edit in
                // progress or files are not the tool's to take over.
                if conversation.is_some() {
                    guard::the_composer_is_plain(
                        *composer.edit_id(),
                        *composer.reply_to_id(),
                        composer
                            .attachment_count()
                            .saturating_add(*composer.preparing()),
                    )?;
                }
                if !composer.as_mut().start_edit(conv, message) {
                    return refused("the message cannot be edited");
                }
                if !composer.as_mut().send(conv, &QString::from(text)) {
                    return refused("the composer did not send the edit");
                }
            }
        }
        Ok(json!({"message": message}))
    }

    /// `send`: the composer's send in the open conversation. Returns the conversation and the
    /// row key of the newest own message before the send (the caller waits for a newer one).
    ///
    /// `conversation` is the driver's guard (C7, [`super::guard`]): when it names a chat that is
    /// not the open one, or the composer is not plain (a reply picked, an edit in progress, files in
    /// the strip: F3), the send is refused on this very thread, in this very call, and nothing
    /// is sent.
    pub(crate) fn dev_send(
        self: Pin<&mut Self>,
        text: &str,
        conversation: Option<i64>,
    ) -> Result<(i64, Option<RowKey>), DevError> {
        let conv = pinned(self.timeline).map_or(0, |t| *t.conversation_id());
        if conv == 0 && conversation.is_none() {
            return Err(DevError::NotFound("no conversation is open".into()));
        }
        guard::the_named_chat_is_open(conv, conversation)?;
        let before = self.dev_newest_own_key();
        let Some(mut composer) = pinned(self.composer) else {
            return Err(DevError::Gone);
        };
        // A driver that named a chat sends from a plain composer only (F3): a reply he picked, an edit in
        // progress or files in the strip would go out with, or instead of, the tool's text.
        if conversation.is_some() {
            guard::the_composer_is_plain(
                *composer.edit_id(),
                *composer.reply_to_id(),
                composer
                    .attachment_count()
                    .saturating_add(*composer.preparing()),
            )?;
        }
        if composer.as_mut().send(conv, &QString::from(text)) {
            Ok((conv, before))
        } else {
            let why = composer.error().to_string();
            Err(DevError::Failed(if why.is_empty() {
                "the composer did not send".into()
            } else {
                why
            }))
        }
    }

    /// Whether `until` holds, and what was observed (counts and states only).
    pub(crate) fn dev_check(self: Pin<&mut Self>, until: Until) -> (bool, Value) {
        match until {
            Until::Loaded => (*self.loaded(), json!(*self.loaded())),
            Until::AccountActive => {
                let statuses: Vec<AccountStatus> = pinned(self.accounts)
                    .map(|m| m.store().view.iter().map(|a| a.status).collect())
                    .unwrap_or_default();
                let names: Vec<&str> = statuses.iter().map(|s| status_name(*s)).collect();
                (statuses.contains(&AccountStatus::Online), json!(names))
            }
            Until::ConversationsAtLeast(n) => {
                let count = pinned(self.conversations).map_or(0, |m| m.store().view.len());
                (count >= n, json!(count))
            }
            Until::MessagesAtLeast(n) => {
                let count = self.dev_messages();
                (count >= n, json!(count))
            }
            Until::Login(kind) => {
                let shown = pinned(self.login).map(|l| l.kind().to_string());
                (shown.as_deref() == Some(kind.as_str()), json!(shown))
            }
            Until::OutgoingState(want) => {
                let state = self.dev_newest_own();
                let done = match (state.and_then(receipt_rank), receipt_rank(want)) {
                    (Some(have), Some(need)) => have >= need,
                    _ => false,
                };
                (done, json!(state.map_or("none", receipt_name)))
            }
            Until::Challenge(waiting) => {
                let account = self.challenged_account().map(|a| a.0);
                (account.is_some() == waiting, json!(account))
            }
            Until::NonePending => {
                let (pending, preparing) = self.dev_pending();
                (
                    pending == 0 && preparing == 0,
                    json!({"pending": pending, "preparing": preparing}),
                )
            }
            Until::FilesPlanned => {
                let composer = self.dev_composer();
                (composer["planned"] == json!(true), composer)
            }
        }
    }

    /// Own messages of the open conversation that are queued or being sent, and files the
    /// core is still preparing for it.
    fn dev_pending(&self) -> (usize, i32) {
        let pending = pinned(self.timeline).map_or(0, |t| {
            t.store()
                .view
                .iter()
                .filter(|r| r.kind == RowKind::Message && r.receipt == Receipt::Pending)
                .count()
        });
        let preparing = pinned(self.composer).map_or(0, |c| *c.preparing());
        (pending, preparing)
    }

    /// The composer's file strip: counts only.
    fn dev_composer(&self) -> Value {
        match pinned(self.composer) {
            Some(c) => json!({
                "files": *c.attachment_count(),
                "sendable": *c.sendable_count(),
                "refused": *c.refused_count(),
                "messages": *c.message_count(),
                "planned": *c.planned(),
                "preparing": *c.preparing(),
                "prepared": *c.preparing_done(),
                // A reply he picked and an edit in progress (F3: a guarded send goes from a plain composer only).
                "replying": *c.reply_to_id() > 0,
                "editing": *c.edit_id() > 0,
                // Chats that are not open and have files waiting in their composer.
                "parked": c.parked_chats(),
            }),
            None => Value::Null,
        }
    }

    /// The message rows of the open conversation, newest first: what each one shows, as marks
    /// a driver can assert (no texts, no names; a reaction's emoji is the reaction itself).
    fn dev_rows(&self) -> Vec<Value> {
        /// More rows than a scripted scene looks at; the window holds up to 500.
        const ROWS: usize = 200;
        pinned(self.timeline).map_or_else(Vec::new, |t| {
            t.store()
                .view
                .iter()
                .filter(|r| r.kind == RowKind::Message)
                .take(ROWS)
                .map(|r| {
                    let kinds = r.attachment_kinds();
                    let reactions: Vec<Value> = r
                        .reactions
                        .iter()
                        .map(|c| json!({"emoji": c.emoji, "count": c.count, "mine": c.mine}))
                        .collect();
                    json!({
                        "message": r.message_id().map(|m| m.0),
                        "own": r.own,
                        "state": receipt_name(r.receipt),
                        "edited": r.edited,
                        "deleted": r.deleted,
                        "text": r.has_text(),
                        "quote": r.quote().map(|q| q.state.as_str()),
                        "attachments": kinds.len(),
                        "kinds": kinds,
                        "reactions": reactions,
                    })
                })
                .collect()
        })
    }

    fn dev_messages(&self) -> usize {
        pinned(self.timeline).map_or(0, |t| {
            t.store()
                .view
                .iter()
                .filter(|r| r.kind == RowKind::Message)
                .count()
        })
    }

    /// The row key of the newest own message of the open conversation.
    pub(crate) fn dev_newest_own_key(&self) -> Option<RowKey> {
        pinned(self.timeline).and_then(|t| {
            t.store()
                .view
                .iter()
                .find(|r| r.kind == RowKind::Message && r.own)
                .map(|r| r.key)
        })
    }

    /// The receipt of the newest own message of the open conversation (row 0 is the newest).
    fn dev_newest_own(&self) -> Option<Receipt> {
        pinned(self.timeline).and_then(|t| {
            t.store()
                .view
                .iter()
                .find(|r| r.kind == RowKind::Message && r.own)
                .map(|r| r.receipt)
        })
    }

    /// The media blocks of the open conversation, newest message first: whose message, the
    /// block type, its download state and whether the row hands QML a picture to show (kinds
    /// and states only).
    fn dev_media(&self) -> Vec<Value> {
        use kourier_viewmodel::Block;
        pinned(self.timeline).map_or_else(Vec::new, |t| {
            t.store()
                .view
                .iter()
                .filter(|r| r.kind == RowKind::Message)
                .flat_map(|r| {
                    r.blocks.iter().flat_map(move |block| match block {
                        // Every picture of an album is a block of its own here.
                        Block::Image(_) | Block::Album(_) => block
                            .images()
                            .iter()
                            .map(|image| {
                                json!({
                                    "own": r.own,
                                    "block": "image",
                                    "state": image.state.as_str(),
                                    "shown": !image.url.is_empty(),
                                })
                            })
                            .collect::<Vec<Value>>(),
                        Block::File(file) => vec![json!({
                            "own": r.own,
                            "block": "file",
                            "state": file.state.as_str(),
                            "shown": true,
                        })],
                        Block::Text(_) | Block::Quote(_) | Block::Placeholder(_) => Vec::new(),
                    })
                })
                .collect()
        })
    }

    /// `drag`: a synthetic left-button drag in window coordinates (`steps` moves).
    pub(crate) fn dev_drag(
        self: Pin<&mut Self>,
        from: (f64, f64),
        to: (f64, f64),
        steps: u32,
    ) -> Result<Value, DevError> {
        let w = window()?;
        let steps = i32::try_from(steps).unwrap_or(10);
        // SAFETY: `w` is the live root window (Qt thread).
        if unsafe { ffi::drag_mouse(w, from.0, from.1, to.0, to.1, steps) } {
            Ok(json!({}))
        } else {
            Err(DevError::Failed("the window took no mouse events".into()))
        }
    }

    /// `close`: the window's close button (hides to the tray when one is shown).
    pub(crate) fn dev_close(self: Pin<&mut Self>) -> Result<Value, DevError> {
        let w = window()?;
        // SAFETY: `w` is the live root window (Qt thread).
        unsafe { ffi::close_window(w) };
        Ok(json!({}))
    }

    /// `state`: counts and states for the driver's log (no texts, no names).
    pub(crate) fn dev_state(self: Pin<&mut Self>) -> Value {
        let accounts: Vec<&str> = pinned(self.accounts)
            .map(|m| {
                m.store()
                    .view
                    .iter()
                    .map(|a| status_name(a.status))
                    .collect()
            })
            .unwrap_or_default();
        let size = window()
            .ok()
            // SAFETY: the live root window (Qt thread).
            .map(|w| unsafe { ffi::window_size(w) })
            .filter(|s| *s >= 0)
            .map(|s| [s / 65536, s % 65536]);
        let visible = window()
            .ok()
            // SAFETY: the live root window (Qt thread).
            .map(|w| unsafe { ffi::window_visible(w) });
        let transfer = self.dev_transfer();
        json!({
            "loaded": *self.loaded(),
            "window_visible": visible,
            "tray": *self.tray_status(),
            "unread": *self.unread_count(),
            "accounts": accounts,
            "conversations": pinned(self.conversations).map_or(0, |m| m.store().view.len()),
            "open": pinned(self.timeline).map_or(0, |t| *t.conversation_id()),
            // The open chat's disappearing-messages timer in seconds (0: off), as its header
            // shows it; `null` while no chat is open or its header is still loading. A
            // driver that compares the timer before a send must never read "off" from a
            // header that is not there yet.
            "timer_secs": pinned(self.timeline)
                .and_then(|t| t.header_loaded().then(|| *t.timer_secs())),
            "messages": self.dev_messages(),
            "newest_own": self.dev_newest_own().map(receipt_name),
            "rows": self.dev_rows(),
            "media": self.dev_media(),
            "composer": self.dev_composer(),
            "challenge": self.challenged_account().map(|a| a.0),
            "banner": self.banner_kind().to_string(),
            "login": pinned(self.login).map(|l| l.kind().to_string()),
            "window": size,
            // Whether the run was started with the number tail a first link is checked against
            // (the tail itself is never shown): a run tool refuses to say "scan now" unless true.
            "expected_number_check": self.expected_number_check(),
            // The history the network pushes after a link, as the account banner shows it: the
            // phase is `history` while the phone is still sending (the STATE banner never says
            // so), `""` when nothing is shown; `progress` 0 … 1 or null when it is not known.
            "transfer": {"phase": transfer.0, "progress": transfer.1},
            // WhatsApp's usage-statistics counters (the real backend only, else null): a run
            // that expects `sent == 0` must see `looked` grow, or the 0 proves nothing.
            "wam": self.dev_app().and_then(AppHandle::wam_counts)
                .map(|(sent, looked)| json!({"sent": sent, "looked": looked})),
            // Whether this run plays WhatsApp with the rehearsal fake (no real client, no network).
            "rehearsal": self.dev_app().is_some_and(AppHandle::is_rehearsal),
            // The frames the rehearsal fake has taken (sends, reacts, edits, deletes, revokes,
            // timer sets; T2 C3): the run tool's proof that a read-only step sent nothing.
            // `null` outside a rehearsal: "not measured" is never a 0.
            "frames": self.dev_app().and_then(AppHandle::rehearsal_frames),
        })
    }

    /// `rehearsal_arrive`: the rehearsal network's phone puts one text from the other member
    /// into the first chat of `kind` of the running account, as live traffic. Refused in every
    /// run that does not play WhatsApp with the rehearsal fake (a real run has no such door).
    pub(crate) fn dev_rehearsal_arrive(
        self: Pin<&mut Self>,
        kind: RehearsalKind,
    ) -> Result<Value, DevError> {
        let app = self.dev_app().ok_or(DevError::Gone)?;
        app.rehearsal_arrive(kind)
            .map_err(|e| DevError::Failed(format!("refused: {e}")))?;
        Ok(json!({"kind": match kind {
            RehearsalKind::Direct => "direct",
            RehearsalKind::Group => "group",
        }}))
    }

    /// `rehearsal_act`: the rehearsal network's phone plays `act` in the first chat of `kind` of
    /// the running account (T2 T-5). Refused like `rehearsal_arrive` in every run that does not
    /// play WhatsApp with the rehearsal fake; the fake takes no frame for it.
    pub(crate) fn dev_rehearsal_act(
        self: Pin<&mut Self>,
        kind: RehearsalKind,
        act: RehearsalAct,
    ) -> Result<Value, DevError> {
        let app = self.dev_app().ok_or(DevError::Gone)?;
        app.rehearsal_act(kind, act)
            .map_err(|e| DevError::Failed(format!("refused: {e}")))?;
        Ok(json!({"kind": match kind {
            RehearsalKind::Direct => "direct",
            RehearsalKind::Group => "group",
        }, "act": act.name()}))
    }

    /// Resizes the window (the screenshot's `size`).
    pub(crate) fn dev_resize(self: Pin<&mut Self>, w: u32, h: u32) -> Result<(), DevError> {
        let win = window()?;
        let (w, h) = (
            i32::try_from(w).map_err(|_| DevError::Failed("width".into()))?,
            i32::try_from(h).map_err(|_| DevError::Failed("height".into()))?,
        );
        // SAFETY: the live root window (Qt thread).
        if unsafe { ffi::resize_window(win, w, h) } {
            Ok(())
        } else {
            Err(DevError::Failed("the root object is not a window".into()))
        }
    }

    /// Grabs the whole window into `path` (PNG).
    pub(crate) fn dev_grab(self: Pin<&mut Self>, path: &Path) -> Result<Value, DevError> {
        let win = window()?;
        let file = QString::from(path.to_string_lossy().as_ref());
        // SAFETY: the live root window (Qt thread).
        let luma = unsafe { qobject::grab_window_to_file(win, &file) };
        if luma < 0 {
            return Err(DevError::Failed(
                "grabbing or saving the window failed".into(),
            ));
        }
        // SAFETY: as above.
        let size = unsafe { ffi::window_size(win) };
        Ok(json!({
            "path": path.to_string_lossy(),
            "luma": luma,
            "width": size / 65536,
            "height": size % 65536,
        }))
    }
}

fn open_in_window(id: i64) -> Result<(), DevError> {
    let w = window()?;
    // Conversation ids are row ids far below 2^53, so the QML `real` holds them exactly.
    #[allow(clippy::cast_precision_loss)]
    let id = id as f64;
    // SAFETY: `w` is the live root window (Qt thread).
    if unsafe { ffi::invoke_qml_number(w, &QString::from("openConversation"), id) } {
        Ok(())
    } else {
        Err(DevError::Failed(
            "the window has no openConversation(real)".into(),
        ))
    }
}

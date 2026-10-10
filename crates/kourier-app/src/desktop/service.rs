//! The desktop task: one loop on the runtime that owns the notification client, the
//! [`NotificationBook`], the tray and the instance requests, and turns core events into
//! notifications and desktop signals into [`DesktopEvent`]s and core commands.
//!
//! Everything runs in order in this one task, so a replacement always knows the id the
//! previous `Notify` returned. Log safety (§9.1): only ids, counts and kinds are logged.
//!
//! The notification server is connected in the background: the first call may wait for D-Bus
//! activation (Plasma's `plasma_waitforname` while plasmashell starts after an autostarted
//! Kourier). New messages and summaries wait meanwhile; a server that appears later (or
//! restarts) is connected again, and the ids of the old one are forgotten.

use std::path::PathBuf;
use std::sync::Arc;

use kourier_core::facade::{
    AccountRow, ConversationDetail, CoreHandle, MessageRow, Notification as CoreNotification,
    Query, QueryData, TimelineAnchor, TimelineQuery, UiCommand, UiEvent,
};
use kourier_desktop::autostart::{self, AutostartEntry};
use kourier_desktop::instance::InstanceRequest;
use kourier_desktop::notifications::{self, NotificationClient, NotificationEvent};
use kourier_desktop::tray::{Tray, TrayConfig, TrayEvent, TrayState};
use kourier_desktop::{Bus, DesktopError};
use kourier_model::{AccountId, AccountState, ConversationId, MessageId, Text};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;

use super::policy::{self, Burst, MessageContent, NotificationBook, Target, Texts};
use super::{Command, Connection, DesktopEvent, PlainPresenter, Presenter};
use crate::prefs::{NotificationContent, Prefs};

/// A connected notification client and its signals (when the subscription worked).
type Connected = (
    NotificationClient,
    Option<mpsc::UnboundedReceiver<NotificationEvent>>,
);

/// Files the task writes.
pub(crate) struct Files {
    pub prefs: PathBuf,
    pub autostart: PathBuf,
    pub entry: AutostartEntry,
}

pub(crate) struct Service {
    core: CoreHandle,
    events: mpsc::UnboundedSender<DesktopEvent>,
    presenter: Arc<dyn Presenter>,
    texts: Texts,
    prefs: Prefs,
    files: Files,
    client: Option<NotificationClient>,
    /// The session bus (none without desktop services).
    session: Option<Bus>,
    /// A notification server connection in progress.
    connecting: Option<JoinHandle<Result<Connected, DesktopError>>>,
    /// Catch-up summaries that arrived while connecting.
    queued_summaries: Vec<(AccountId, u32, u32)>,
    book: NotificationBook,
    accounts: Vec<AccountRow>,
    tray: Option<Tray>,
    tray_tx: mpsc::UnboundedSender<TrayEvent>,
    tray_rx: mpsc::UnboundedReceiver<TrayEvent>,
    attached: bool,
    unread: u32,
    /// `Notify` events of conversations whose notification changed a moment ago, collected
    /// until it has rested ([`policy::Pace`]).
    pending: Vec<CoreNotification>,
    flush_at: Option<tokio::time::Instant>,
    /// Which conversations' notifications rest, and until when.
    pace: policy::Pace<tokio::time::Instant>,
}

/// Sleeps until `at`; forever without one.
async fn sleep_opt(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Waits for an optional task; pending forever without one.
async fn join_opt<T>(task: &mut Option<JoinHandle<T>>) -> Result<T, JoinError> {
    match task {
        Some(task) => task.await,
        None => std::future::pending().await,
    }
}

/// Connects to the notification server and subscribes to its signals.
async fn connect_server(bus: Bus) -> Result<Connected, DesktopError> {
    let client = NotificationClient::connect(&bus).await?;
    let events = match client.subscribe().await {
        Ok(rx) => Some(rx),
        Err(e) => {
            tracing::warn!(error = %e, "notification signals unavailable");
            None
        }
    };
    Ok((client, events))
}

/// Receives from an optional channel; pending forever without one.
async fn recv_opt<T>(rx: &mut Option<mpsc::UnboundedReceiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

impl Service {
    pub(crate) fn new(
        core: CoreHandle,
        events: mpsc::UnboundedSender<DesktopEvent>,
        prefs: Prefs,
        files: Files,
    ) -> Self {
        let (tray_tx, tray_rx) = mpsc::unbounded_channel();
        Self {
            core,
            events,
            presenter: Arc::new(PlainPresenter),
            texts: Texts::default(),
            prefs,
            files,
            client: None,
            session: None,
            connecting: None,
            queued_summaries: Vec::new(),
            book: NotificationBook::default(),
            accounts: Vec::new(),
            tray: None,
            tray_tx,
            tray_rx,
            attached: false,
            unread: 0,
            pending: Vec::new(),
            flush_at: None,
            pace: policy::Pace::default(),
        }
    }

    pub(crate) async fn run(
        mut self,
        mut connection: Connection,
        mut commands: mpsc::UnboundedReceiver<Command>,
        stop: CancellationToken,
    ) {
        // Subscribe first: nothing the core emits from now on is missed.
        let mut core_rx = self.core.subscribe();
        let mut instance_rx = connection.instance.as_mut().and_then(|i| i.take_requests());
        let mut notification_rx = None;
        let mut server_rx = None;
        self.session = connection.bus.clone();
        if let Some(bus) = &connection.bus {
            // Watch first, so a server that appears while the first attempt fails is seen.
            match notifications::watch_server(bus).await {
                Ok(rx) => server_rx = Some(rx),
                Err(e) => tracing::warn!(error = %e, "cannot watch the notification server"),
            }
            self.connect_notifications();
        }
        // The autostart entry is only written when the user changes the setting: a run of
        // another build (a debug binary) must not repoint the user's entry at itself.
        self.refresh_accounts().await;
        loop {
            // New messages wait while the server is being connected.
            let flush_at = self.flush_at.filter(|_| self.connecting.is_none());
            tokio::select! {
                _ = stop.cancelled() => break,
                c = commands.recv() => match c {
                    Some(c) => self.on_command(c).await,
                    None => break,
                },
                e = core_rx.recv() => match e {
                    Ok(e) => self.on_core(e).await,
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(lost = n, "desktop lagged behind the core; rechecking notifications");
                        self.recheck_all().await;
                    }
                    Err(RecvError::Closed) => break,
                },
                n = recv_opt(&mut notification_rx) => match n {
                    Some(n) => self.on_notification(n).await,
                    None => notification_rx = None,
                },
                c = join_opt(&mut self.connecting) => {
                    self.connecting = None;
                    if let Some(rx) = self.on_connected(c).await {
                        notification_rx = Some(rx);
                    }
                }
                s = recv_opt(&mut server_rx) => match s {
                    Some(owner) => self.on_server_appeared(&owner),
                    None => server_rx = None,
                },
                r = recv_opt(&mut instance_rx) => match r {
                    Some(r) => self.on_instance(r),
                    None => instance_rx = None,
                },
                t = self.tray_rx.recv() => {
                    if let Some(t) = t {
                        self.on_tray(t);
                    }
                }
                () = sleep_opt(flush_at) => {
                    self.flush_at = None;
                    for burst in policy::coalesce(std::mem::take(&mut self.pending)) {
                        let conv = burst.newest.conv;
                        self.show_message(burst).await;
                        self.pace.shown(conv, tokio::time::Instant::now());
                    }
                }
            }
        }
        // Quit: our notifications and the tray icon go with us.
        if let Some(task) = self.connecting.take() {
            task.abort();
        }
        if let Some(client) = &self.client {
            for id in self.book.close_all() {
                let _ = client.close(id).await;
            }
        }
        if let Some(tray) = self.tray.take() {
            tray.shutdown().await;
        }
        drop(connection);
        tracing::debug!("desktop integration stopped");
    }

    /// Starts connecting to the notification server (unless connected or connecting).
    fn connect_notifications(&mut self) {
        if self.client.is_some() || self.connecting.is_some() {
            return;
        }
        if let Some(bus) = self.session.clone() {
            self.connecting = Some(tokio::spawn(connect_server(bus)));
        }
    }

    /// The connection attempt finished: keep the client and show what waited meanwhile.
    async fn on_connected(
        &mut self,
        result: Result<Result<Connected, DesktopError>, JoinError>,
    ) -> Option<mpsc::UnboundedReceiver<NotificationEvent>> {
        let summaries = std::mem::take(&mut self.queued_summaries);
        match result {
            Ok(Ok((client, events))) => {
                tracing::info!(
                    inline_reply = client.capabilities().inline_reply(),
                    "notifications available"
                );
                self.client = Some(client);
                for (account, messages, conversations) in summaries {
                    self.show_summary(account, messages, conversations).await;
                }
                events
            }
            Ok(Err(e)) => {
                tracing::info!(error = %e, "no notifications");
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "notification connection task failed");
                None
            }
        }
    }

    /// The notification server got an owner: connect when not connected yet. A different
    /// owner than the connected one is a restarted (or replaced) server: our ids mean nothing
    /// to it (a `replaces_id` could hit another app's notification), so they are forgotten
    /// and the client (with its capabilities) is renewed.
    fn on_server_appeared(&mut self, owner: &str) {
        if let Some(client) = &self.client
            && client.owner() != Some(owner)
        {
            let forgotten = self.book.close_all().len();
            tracing::info!(forgotten, "the notification server changed");
            self.client = None;
        }
        self.connect_notifications();
    }

    fn emit(&self, event: DesktopEvent) {
        let _ = self.events.send(event);
    }

    async fn on_command(&mut self, c: Command) {
        match c {
            Command::Attach(presenter) => {
                self.texts = presenter.texts();
                self.presenter = presenter;
                self.attached = true;
                self.apply_tray().await;
            }
            Command::Prefs(prefs) => {
                let old = std::mem::replace(&mut self.prefs, prefs.clone());
                if let Err(e) = super::save_prefs(self.files.prefs.clone(), prefs.clone()).await {
                    tracing::warn!(error = %e, "preferences not saved");
                }
                if old.autostart != prefs.autostart || old.start_hidden != prefs.start_hidden {
                    self.sync_autostart().await;
                }
                if old.tray != prefs.tray {
                    self.apply_tray().await;
                }
            }
            Command::Focused(focused) => {
                if focused {
                    let ids = self.book.close_summaries();
                    self.close_ids(ids).await;
                }
            }
            Command::Unread(n) => {
                self.unread = n;
                if let Some(tray) = &self.tray {
                    tray.set_state(TrayState {
                        unread: n,
                        tooltip: self.presenter.unread_tooltip(n),
                    })
                    .await;
                }
            }
        }
    }

    /// Writes or removes the autostart entry to match the preferences.
    async fn sync_autostart(&mut self) {
        let path = self.files.autostart.clone();
        let mut entry = self.files.entry.clone();
        entry.background = self.prefs.start_hidden && self.prefs.tray;
        let on = self.prefs.autostart;
        let result = tokio::task::spawn_blocking(move || {
            if on {
                autostart::enable(&path, &entry)
            } else {
                autostart::disable(&path)
            }
        })
        .await;
        match result {
            Ok(Ok(())) => tracing::debug!(on, "autostart entry synced"),
            Ok(Err(e)) => tracing::warn!(error = %e, "autostart entry not written"),
            Err(e) => tracing::warn!(error = %e, "autostart task failed"),
        }
    }

    /// Starts or stops the tray to match the preferences (once the UI is attached).
    async fn apply_tray(&mut self) {
        if !self.attached {
            return;
        }
        let want = self.prefs.tray && self.session.is_some();
        match (want, self.tray.is_some()) {
            (true, false) => {
                let (icons, attention_icons) = self.presenter.tray_icons();
                let config = TrayConfig {
                    id: super::APP_ID.to_owned(),
                    title: self.texts.app_name.clone(),
                    icon_name: super::APP_ID.to_owned(),
                    icons,
                    attention_icons,
                    show_label: self.texts.tray_show.clone(),
                    quit_label: self.texts.tray_quit.clone(),
                };
                match Tray::spawn(config, self.tray_tx.clone()).await {
                    Ok(tray) => {
                        tray.set_state(TrayState {
                            unread: self.unread,
                            tooltip: self.presenter.unread_tooltip(self.unread),
                        })
                        .await;
                        self.tray = Some(tray);
                        self.emit(DesktopEvent::TrayAvailable(true));
                    }
                    Err(e) => {
                        tracing::info!(error = %e, "no tray");
                        self.emit(DesktopEvent::TrayAvailable(false));
                    }
                }
            }
            (false, true) => {
                if let Some(tray) = self.tray.take() {
                    tray.shutdown().await;
                }
                self.emit(DesktopEvent::TrayAvailable(false));
            }
            // Without desktop services (headless) the tray state stays unknown.
            (false, false) => {
                if self.session.is_some() {
                    self.emit(DesktopEvent::TrayAvailable(false));
                }
            }
            (true, true) => {}
        }
    }

    fn on_tray(&mut self, t: TrayEvent) {
        match t {
            TrayEvent::Activate { token } => self.emit(DesktopEvent::Toggle { token }),
            TrayEvent::Show { token } => self.emit(DesktopEvent::Activate {
                token,
                conversation: None,
            }),
            TrayEvent::Quit => self.emit(DesktopEvent::Quit),
            TrayEvent::Available(a) => {
                if self.tray.is_some() {
                    self.emit(DesktopEvent::TrayAvailable(a));
                }
            }
        }
    }

    fn on_instance(&mut self, r: InstanceRequest) {
        tracing::info!(request = ?r, "activation request from another launch");
        match r {
            InstanceRequest::Activate { token } | InstanceRequest::Action { token, .. } => {
                self.emit(DesktopEvent::Activate {
                    token,
                    conversation: None,
                });
            }
            InstanceRequest::Open { uris, token } => {
                let uris = super::handled_uris(uris);
                self.emit(DesktopEvent::Open { uris, token });
            }
        }
    }

    async fn on_notification(&mut self, n: NotificationEvent) {
        match n {
            NotificationEvent::ActivationToken { id, token } => self.book.on_token(id, token),
            NotificationEvent::Action { id, key } => {
                if let Some(a) = self.book.on_action(id, &key) {
                    let conversation = match a.target {
                        Target::Conversation(c) => Some(c),
                        Target::Summary(_) => None,
                    };
                    self.emit(DesktopEvent::Activate {
                        token: a.token,
                        conversation,
                    });
                }
            }
            NotificationEvent::Replied { id, text } => {
                let Some(conv) = self.book.on_reply(id) else {
                    return;
                };
                let text = text.trim();
                if text.is_empty() {
                    return;
                }
                tracing::info!(conv = conv.0, "inline reply");
                let sent = self
                    .core
                    .send(UiCommand::SendText {
                        conv,
                        text: Text::plain(text),
                        reply_to: None,
                    })
                    .and_then(|()| self.core.send(UiCommand::MarkRead { conv }));
                if let Err(e) = sent {
                    tracing::warn!(error = %e, "inline reply not sent");
                }
                if let Some(id) = self.book.close_conversation(conv) {
                    self.close_ids(vec![id]).await;
                }
            }
            NotificationEvent::Closed { id, .. } => self.book.on_closed(id),
        }
    }

    async fn on_core(&mut self, e: UiEvent) {
        match e {
            UiEvent::Notify(n) => {
                crate::trace::notify_event();
                let now = tokio::time::Instant::now();
                // Something is collected for this conversation already, or the server is
                // still being connected: it joins what waits (in order).
                let waits =
                    self.connecting.is_some() || self.pending.iter().any(|p| p.conv == n.conv);
                match self.pace.admit(n.conv, now) {
                    // The first message of a conversation is announced at once: nothing is
                    // gained by holding a single message back.
                    policy::Admit::Now if !waits => {
                        let conv = n.conv;
                        self.show_message(Burst {
                            newest: n,
                            earlier: Vec::new(),
                        })
                        .await;
                        self.pace.shown(conv, tokio::time::Instant::now());
                    }
                    // Its notification changed a moment ago: collected until it has rested.
                    admit => {
                        let due = match admit {
                            policy::Admit::At(at) => at,
                            policy::Admit::Now => now + policy::COALESCE,
                        };
                        self.pending.push(n);
                        self.flush_at = Some(self.flush_at.map_or(due, |at| at.min(due)));
                    }
                }
            }
            UiEvent::CatchUpSummary {
                account,
                messages,
                conversations,
            } => self.show_summary(account, messages, conversations).await,
            UiEvent::ConversationsChanged { ids } => {
                for conv in ids {
                    if self.book.is_tracked(conv) {
                        self.check_read(conv).await;
                    }
                }
            }
            UiEvent::ConversationsRemoved { ids } => {
                let closed: Vec<u32> = ids
                    .into_iter()
                    .filter_map(|c| self.book.close_conversation(c))
                    .collect();
                self.close_ids(closed).await;
            }
            UiEvent::ConversationMerged { from, .. } => {
                if let Some(id) = self.book.close_conversation(from) {
                    self.close_ids(vec![id]).await;
                }
            }
            UiEvent::MessagesChanged { conv, ids } => {
                let shown = self.book.messages(conv);
                if shown.iter().any(|(m, _)| ids.contains(m)) {
                    self.check_messages(conv).await;
                }
            }
            UiEvent::TimelineInvalidated { conv } => {
                if self.book.is_tracked(conv) {
                    self.check_messages(conv).await;
                }
            }
            UiEvent::AccountsChanged => self.refresh_accounts().await,
            UiEvent::AccountRemoved { account } => {
                let ids = self.book.close_account(account);
                self.close_ids(ids).await;
                self.refresh_accounts().await;
            }
            _ => {}
        }
    }

    async fn close_ids(&mut self, ids: Vec<u32>) {
        let Some(client) = &self.client else {
            return;
        };
        for id in ids {
            if let Err(e) = client.close(id).await {
                tracing::debug!(error = %e, "close failed");
            }
        }
    }

    async fn refresh_accounts(&mut self) {
        match self.core.query(Query::Accounts).await {
            Ok(QueryData::Accounts(rows)) => {
                self.accounts = rows
                    .into_iter()
                    .filter(|a| a.state == AccountState::Active)
                    .collect();
            }
            Ok(_) => {}
            Err(e) => tracing::debug!(error = %e, "accounts query failed"),
        }
    }

    /// The account label for `x-kde-origin-name`, when there are several accounts.
    fn origin(&self, account: AccountId) -> Option<String> {
        if self.accounts.len() < 2 {
            return None;
        }
        self.accounts
            .iter()
            .find(|a| a.id == account)
            .map(|a| self.presenter.account_label(a))
    }

    async fn conversation(&self, conv: ConversationId) -> Option<ConversationDetail> {
        match self.core.query(Query::Conversation(conv)).await {
            Ok(QueryData::Conversation(d)) => d.map(|d| *d),
            Ok(_) => None,
            Err(e) => {
                tracing::debug!(error = %e, "conversation query failed");
                None
            }
        }
    }

    async fn message(&self, conv: ConversationId, id: MessageId) -> Result<Option<MessageRow>, ()> {
        let q = Query::Timeline(TimelineQuery {
            conv,
            anchor: TimelineAnchor::Around(id),
            limit: 2,
        });
        match self.core.query(q).await {
            Ok(QueryData::Timeline(page)) => Ok(page.rows.into_iter().find(|r| r.id == id)),
            Ok(_) => Err(()),
            Err(e) => {
                tracing::debug!(error = %e, "timeline query failed");
                Err(())
            }
        }
    }

    async fn show_message(&mut self, burst: Burst) {
        if self.client.is_none() {
            return;
        }
        let n = burst.newest;
        let Some(detail) = self.conversation(n.conv).await else {
            return;
        };
        // Read meanwhile (on the phone, or here while the burst was collected).
        if policy::read_now(&detail.row) {
            return;
        }
        let Ok(Some(msg)) = self.message(n.conv, n.message).await else {
            return;
        };
        // Deleted meanwhile, or a missed call that was answered or declined on the phone
        // while the burst was collected: nothing to announce.
        if msg.deleted.is_some() || policy::call_moved_on(&msg) {
            return;
        }
        let mode = self.prefs.notification_content;
        let title = self.presenter.conversation_title(&detail);
        let line = self.presenter.message_line(&detail, &msg);
        let shown = self.book.messages(n.conv);
        let new = burst
            .earlier
            .iter()
            .chain(std::iter::once(&n.message))
            .filter(|m| !shown.iter().any(|(s, _)| s == *m))
            .count();
        let count = shown.len() + new;
        let count_line = (count > 1).then(|| self.presenter.new_messages(count));
        let origin = self.origin(n.account);
        let avatar = match mode {
            NotificationContent::Hidden => None,
            _ => self.presenter.avatar(&title, &detail),
        };
        let can_reply = !detail.row.blocked && !detail.row.request_pending;
        let content = policy::message_notification(MessageContent {
            mode,
            title: &title,
            line: &line,
            count_line: count_line.as_deref(),
            origin: origin.as_deref(),
            avatar,
            can_reply,
            replaces_id: self.book.replaces_for(n.conv),
            texts: &self.texts,
        });
        let Some(client) = &self.client else {
            return;
        };
        match client.notify(&content).await {
            Ok(id) => {
                crate::trace::notify_shown();
                tracing::debug!(conv = n.conv.0, id, count, "notified");
                // Earlier messages of the burst were new (revision 0) when they arrived.
                for earlier in burst.earlier {
                    self.book.shown_message(n.conv, n.account, earlier, 0, id);
                }
                self.book
                    .shown_message(n.conv, n.account, n.message, msg.revision, id);
            }
            Err(e) => tracing::warn!(error = %e, "notification failed"),
        }
    }

    async fn show_summary(&mut self, account: AccountId, messages: u32, conversations: u32) {
        if self.connecting.is_some() {
            // One summary per account: a later one replaces a waiting one.
            self.queued_summaries.retain(|(a, _, _)| *a != account);
            self.queued_summaries
                .push((account, messages, conversations));
            return;
        }
        let Some(client) = &self.client else {
            return;
        };
        let text = self.presenter.catch_up(messages, conversations);
        let origin = self.origin(account);
        let content = policy::summary_notification(
            &text,
            origin.as_deref(),
            self.book.summary_replaces(account),
            &self.texts,
        );
        match client.notify(&content).await {
            Ok(id) => {
                tracing::info!(messages, conversations, "catch-up summary shown");
                self.book.shown_summary(account, id);
            }
            Err(e) => tracing::warn!(error = %e, "summary notification failed"),
        }
    }

    /// Closes `conv`'s notification once it is read (here or on another device) or gone.
    async fn check_read(&mut self, conv: ConversationId) {
        let read = match self.conversation(conv).await {
            None => true,
            Some(d) => policy::read_now(&d.row),
        };
        if read && let Some(id) = self.book.close_conversation(conv) {
            tracing::debug!(conv = conv.0, "read: notification closed");
            self.close_ids(vec![id]).await;
        }
    }

    /// Closes `conv`'s notification when a message it showed was deleted, edited or expired,
    /// or when the message it shows (the newest) is a call line that moved on. An older call
    /// line that moved on under a newer message is only forgotten ([`policy::withdraw`]).
    async fn check_messages(&mut self, conv: ConversationId) {
        let shown = self.book.messages(conv);
        let newest = shown.last().map(|(m, _)| *m);
        for (message, revision) in shown {
            let Ok(row) = self.message(conv, message).await else {
                continue;
            };
            let close = match policy::withdraw(revision, row.as_ref(), newest == Some(message)) {
                policy::Withdraw::Keep => false,
                // (A notification that stands for nothing any more goes too.)
                policy::Withdraw::Forget => self.book.forget_message(conv, message),
                policy::Withdraw::Close => true,
            };
            if close {
                if let Some(id) = self.book.close_conversation(conv) {
                    tracing::debug!(
                        conv = conv.0,
                        "deleted, edited or a call that moved on: notification closed"
                    );
                    self.close_ids(vec![id]).await;
                }
                return;
            }
        }
    }

    async fn recheck_all(&mut self) {
        for conv in self.book.conversations() {
            self.check_read(conv).await;
            if self.book.is_tracked(conv) {
                self.check_messages(conv).await;
            }
        }
    }
}

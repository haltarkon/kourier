//! The development control channel (debug builds only, `kourier --dev-control`): JSON lines
//! on stdin drive the running UI the way a user would (add an account, type the device name,
//! open a conversation, attach any number of files, send from the composer, react to, reply
//! to, edit or delete a message, answer a challenge, show a page, grab the window), JSON lines
//! on stdout answer them and report login steps and account states (`account_state`:
//! `account`, `network`, `state`, `conn` and `device`, this device's number on the account).
//!
//! `state` reports what the open conversation shows as marks a driver can assert, newest
//! message first in `rows`: `message` (id), `own`, `state` (`none`, `pending`, `sent`,
//! `delivered`, `read`, `failed`), `edited`, `deleted`, `text` (it shows a text or caption),
//! `quote` (`null`, or `known`, `missing`, `deleted`), `attachments` (count), `kinds`
//! (`image`, `gif`, `video`, `sticker`, `voice`, `audio`, `file`, one per attachment) and
//! `reactions` (`emoji`, `count`, `mine`). `composer` has the file strip's counts (`files`,
//! `sendable`, `refused`, `messages`, `planned`, `preparing`), `challenge` the account that
//! waits for a challenge answer (or `null`) and `banner` the account banner's kind.
//! `timer_secs` is the open conversation's disappearing-messages timer in seconds (0: off)
//! and `null` until that conversation's header is loaded (a moment after `open_id`), so "off"
//! is never read from a header that is not there yet. `cargo xtask demo` drives the
//! real app on Signal's mock server with it. Wire format: [`protocol`].
//!
//! Commands run concurrently on the app's tokio runtime; everything that touches Qt runs on
//! the Qt thread through [`CxxQtThread::queue`] (never blocking it). Waits poll the models
//! every [`POLL`]. Closing stdin quits the app. None of this exists in release builds: the
//! module, its C++ shim and its flag are compiled only with debug assertions.

mod exec;
pub mod protocol;

use std::io::{BufRead as _, Write as _};
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cxx_qt::CxxQtThread;
use kourier_app::AppHandle;
use kourier_app::facade::{Query, QueryData, UiEvent};
use kourier_model::{AccountState, ConnState, LoginStepKind};
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{mpsc, oneshot};

use crate::bridge::qobject::App;
use crate::dev_ffi;
use protocol::{Command, Request};

/// How often `wait` re-checks its condition.
pub const POLL: Duration = Duration::from_millis(100);

/// How long `send` waits for its message to show up in the open timeline.
pub const SEND_SHOWS_WITHIN: Duration = Duration::from_secs(15);

/// Serializes stdout lines from every thread.
static STDOUT: Mutex<()> = Mutex::new(());

/// Writes one protocol line to stdout.
pub(crate) fn emit(line: &Value) {
    let _guard = STDOUT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut out = std::io::stdout().lock();
    // A closed stdout means the driver is gone; stdin's end quits the app.
    let _ = writeln!(out, "{line}").and_then(|()| out.flush());
}

/// Why a command failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DevError {
    /// What the command names does not exist (no such conversation, no window yet).
    #[error("{0}")]
    NotFound(String),
    /// A `wait` condition did not hold in time.
    #[error("{0}")]
    Timeout(String),
    /// The UI refused or could not do it.
    #[error("{0}")]
    Failed(String),
    /// The UI is gone (shutting down).
    #[error("the UI has shut down")]
    Gone,
}

impl DevError {
    fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "notFound",
            Self::Timeout(_) => "timeout",
            Self::Failed(_) => "failed",
            Self::Gone => "gone",
        }
    }
}

/// Runs `f` on the Qt thread and returns its result.
async fn on_qt<T: Send + 'static>(
    thread: &CxxQtThread<App>,
    f: impl FnOnce(Pin<&mut App>) -> T + Send + 'static,
) -> Result<T, DevError> {
    let (tx, rx) = oneshot::channel();
    thread
        .queue(move |app| {
            let _ = tx.send(f(app));
        })
        .map_err(|_| DevError::Gone)?;
    rx.await.map_err(|_| DevError::Gone)
}

/// Starts the channel: the stdin reader, the command dispatcher and the event reporter.
pub(crate) fn start(app: &AppHandle, thread: CxxQtThread<App>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let reader = std::thread::Builder::new()
        .name("kourier-dev-stdin".into())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                if tx.send(protocol::parse_line(&line)).is_err() {
                    return;
                }
            }
            // `None` on the channel: stdin ended.
        });
    if let Err(e) = reader {
        tracing::error!(error = %e, "dev control: cannot start the stdin reader");
        return;
    }
    let stop = app.shutdown_token();
    let commands = thread.clone();
    app.runtime().spawn(async move {
        loop {
            let item = tokio::select! {
                _ = stop.cancelled() => return,
                item = rx.recv() => item,
            };
            match item {
                Some(Ok(request)) => {
                    tokio::spawn(handle(request, commands.clone()));
                }
                Some(Err((id, e))) => emit(&protocol::err(&id, e.code(), &e.to_string())),
                None => {
                    tracing::info!("dev control: stdin closed; quitting");
                    let _ = commands.queue(|_| dev_ffi::qt::quit_app(0));
                    return;
                }
            }
        }
    });
    spawn_events(app);
    emit(&protocol::event("channel", json!({"protocol": 1})));
}

/// Runs one request and writes its response.
async fn handle(request: Request, thread: CxxQtThread<App>) {
    let quit = request.command == Command::Quit;
    let line = match run(request.command, &thread).await {
        Ok(result) => protocol::ok(&request.id, result),
        Err(e) => protocol::err(&request.id, e.code(), &e.to_string()),
    };
    emit(&line);
    if quit {
        let _ = thread.queue(|_| dev_ffi::qt::quit_app(0));
    }
}

async fn run(command: Command, thread: &CxxQtThread<App>) -> Result<Value, DevError> {
    match command {
        Command::AddAccount { network } => {
            on_qt(thread, move |app| app.dev_add_account(&network)).await?
        }
        Command::FillLogin { values } => {
            on_qt(thread, move |app| app.dev_login(Some(&values), false)).await?
        }
        Command::SubmitLogin { values } => {
            on_qt(thread, move |app| app.dev_login(values.as_ref(), true)).await?
        }
        Command::Open { title } => on_qt(thread, move |app| app.dev_open(&title)).await?,
        Command::OpenId { conversation } => {
            on_qt(thread, move |app| app.dev_open_id(conversation)).await?
        }
        Command::Chats => Ok(on_qt(thread, |app| app.dev_chats()).await?),
        Command::Page { name } => on_qt(thread, move |app| app.dev_page(name)).await?,
        Command::Attach { paths } => on_qt(thread, move |app| app.dev_attach(&paths)).await?,
        Command::Challenge { answer } => {
            on_qt(thread, move |app| app.dev_challenge(&answer)).await?
        }
        Command::Act { on, action } => on_qt(thread, move |app| app.dev_act(on, &action)).await?,
        Command::Send { text } => {
            let (conv, before) = on_qt(thread, move |app| app.dev_send(&text)).await??;
            // Answer once the message is in the timeline, so a following `wait` on its state
            // looks at this message and not at an older one.
            let started = Instant::now();
            loop {
                let newest = on_qt(thread, |app| app.dev_newest_own_key()).await?;
                if newest.is_some() && newest != before {
                    return Ok(json!({"conversation": conv}));
                }
                if started.elapsed() >= SEND_SHOWS_WITHIN {
                    return Err(DevError::Timeout(
                        "the sent message did not appear in the timeline".into(),
                    ));
                }
                tokio::time::sleep(POLL / 2).await;
            }
        }
        Command::Drag { from, to, steps } => {
            on_qt(thread, move |app| app.dev_drag(from, to, steps)).await?
        }
        Command::Close => on_qt(thread, |app| app.dev_close()).await?,
        Command::State => Ok(on_qt(thread, |app| app.dev_state()).await?),
        Command::Quit => Ok(json!({})),
        Command::Wait { until, timeout } => {
            let started = Instant::now();
            loop {
                let (done, observed) = on_qt(thread, move |app| app.dev_check(until)).await?;
                let waited = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                if done {
                    return Ok(json!({"waited_ms": waited, "observed": observed}));
                }
                if started.elapsed() >= timeout {
                    return Err(DevError::Timeout(format!(
                        "{until} did not hold within {} ms (observed {observed})",
                        timeout.as_millis()
                    )));
                }
                tokio::time::sleep(POLL).await;
            }
        }
        Command::Screenshot { path, size, settle } => {
            if let Some((w, h)) = size {
                on_qt(thread, move |app| app.dev_resize(w, h)).await??;
            }
            tokio::time::sleep(settle).await;
            on_qt(thread, move |app| app.dev_grab(&path)).await?
        }
    }
}

/// The environment variable that keeps the link out of `login_qr` events (`0`).
pub const QR_URL_ENV: &str = "KOURIER_DEV_QR_URL";

/// Whether `login_qr` events carry the link: yes, unless [`QR_URL_ENV`] is `0`.
fn qr_url_wanted(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    env(QR_URL_ENV).is_none_or(|v| v != "0")
}

/// Reports login steps, link URLs, account states and failures as events.
fn spawn_events(app: &AppHandle) {
    let mut rx = app.subscribe();
    let stop = app.shutdown_token();
    let handle = app.clone();
    let qr_url = qr_url_wanted(|k| std::env::var_os(k));
    app.runtime().spawn(async move {
        let mut last_states: Vec<Value> = Vec::new();
        loop {
            let event = tokio::select! {
                _ = stop.cancelled() => return,
                r = rx.recv() => match r {
                    Ok(e) => e,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => return,
                },
            };
            match &event {
                UiEvent::LoginStep { account, step } => {
                    emit(&protocol::event(
                        "login_step",
                        json!({"account": account.0, "step": step.step_id, "kind": login_kind(&step.kind)}),
                    ));
                    match &step.kind {
                        // The link is what the phone scans. A driver that plays the phone
                        // (the mock) needs it; any other run keeps it inside the process.
                        LoginStepKind::ShowQr { .. } if !qr_url => emit(&protocol::event(
                            "login_qr",
                            json!({"account": account.0, "shown": true}),
                        )),
                        LoginStepKind::ShowQr { data, .. } => emit(&protocol::event(
                            "login_qr",
                            json!({"account": account.0, "url": data.expose()}),
                        )),
                        LoginStepKind::Failed { error } => emit(&protocol::event(
                            "error",
                            json!({"source": "login", "account": account.0, "message": error.to_string()}),
                        )),
                        _ => {}
                    }
                }
                UiEvent::AccountError { account, detail, .. } => emit(&protocol::event(
                    "error",
                    json!({"source": "account", "account": account.0, "message": detail}),
                )),
                UiEvent::ActionFailed { action, error, .. } => emit(&protocol::event(
                    "error",
                    json!({"source": "action", "action": action, "message": error.to_string()}),
                )),
                _ => {}
            }
            if matches!(
                event,
                UiEvent::AccountAdded { .. }
                    | UiEvent::AccountRemoved { .. }
                    | UiEvent::AccountsChanged
                    | UiEvent::ConnState { .. }
            ) && let Ok(QueryData::Accounts(rows)) = handle.query(Query::Accounts).await
            {
                let states: Vec<Value> = rows
                    .iter()
                    .map(|a| {
                        json!({
                            "account": a.id.0,
                            "network": a.network.as_str(),
                            "state": account_state(&a.state),
                            "conn": conn_state(&a.conn),
                            // This device's number on the account (the phone lists it).
                            "device": a.own_device,
                        })
                    })
                    .collect();
                for s in &states {
                    if !last_states.contains(s) {
                        emit(&protocol::event("account_state", s.clone()));
                    }
                }
                last_states = states;
            }
        }
    });
}

fn login_kind(k: &LoginStepKind) -> &'static str {
    match k {
        LoginStepKind::ShowQr { .. } => "qr",
        LoginStepKind::ShowCode { .. } => "code",
        LoginStepKind::ConfirmEmoji { .. } => "emoji",
        LoginStepKind::Input { .. } => "input",
        LoginStepKind::Progress { .. } => "progress",
        LoginStepKind::Done => "done",
        LoginStepKind::Failed { .. } => "failed",
    }
}

fn account_state(s: &AccountState) -> &'static str {
    match s {
        AccountState::Linking => "linking",
        AccountState::Active => "active",
        AccountState::LoggedOut { .. } => "logged_out",
        AccountState::Removing => "removing",
        AccountState::Unlinking => "unlinking",
    }
}

fn conn_state(c: &ConnState) -> &'static str {
    match c {
        ConnState::Offline { .. } => "offline",
        ConnState::Connecting => "connecting",
        ConnState::Syncing { .. } => "syncing",
        ConnState::Online { .. } => "online",
        ConnState::NeedsUserAction(_) => "needs_action",
        ConnState::Restricted { .. } => "restricted",
        ConnState::LoggedOut { .. } => "logged_out",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The link stays in the process only when the driver says so.
    #[test]
    fn the_qr_link_is_reported_unless_turned_off() {
        let with = |value: Option<&'static str>| {
            qr_url_wanted(move |k| {
                (k == QR_URL_ENV)
                    .then_some(value)
                    .flatten()
                    .map(std::ffi::OsString::from)
            })
        };
        assert!(with(None));
        assert!(with(Some("1")));
        assert!(with(Some("")));
        assert!(!with(Some("0")));
    }
}

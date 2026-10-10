//! The wire format of the development control channel: JSON lines, the same shapes as the mock
//! harness (`tests/mock-harness/README.md`).
//!
//! - Request (stdin): `{"id": <any>, "cmd": "<name>", "args": {...}}` (`args` optional).
//! - Response (stdout): `{"type": "response", "id": <echoed>, "ok": true, "result": {...}}` or
//!   `{"type": "response", "id": <echoed>, "ok": false, "error": {"code", "message"}}`; `id`
//!   is `null` for a line that is not a request.
//! - Event (stdout): `{"type": "event", "event": "<name>", "data": {...}}`.
//!
//! Nothing here echoes message texts, names or numbers back, except `login_qr`'s link URL (the
//! driver hands it to the mock phone). With `KOURIER_DEV_QR_URL=0` in the environment the
//! event says only that a code is shown (`{"account", "shown": true}`): a run with a real
//! account, where the code is scanned from the window, never lets the link leave the process.
//!
//! A driver that must not learn names (`cargo xtask smoke`: its log is read by others) finds
//! chats with `chats` (ids, kinds and counts only) and opens one with `open_id`.

use std::path::PathBuf;
use std::time::Duration;

use kourier_viewmodel::timeline::Receipt;
use serde_json::{Map, Value, json};

/// The longest `wait` timeout.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(600);
/// `wait` without `timeout_ms`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long `screenshot` lets layout and animations settle before the grab (after a resize).
pub const DEFAULT_SETTLE: Duration = Duration::from_millis(1200);

/// The message an [`Command::Act`] is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActOn {
    /// The newest own message that is not deleted.
    Own,
    /// The newest message of someone else that is not deleted.
    Other,
}

/// What [`Command::Act`] does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Adds our reaction (`remove`: takes it back).
    React {
        /// The emoji.
        emoji: String,
        /// Take it back.
        remove: bool,
    },
    /// Replies with `text`, quoting the message (the composer's reply mode, then send).
    Reply {
        /// The reply.
        text: String,
    },
    /// Replaces its text with `text` (the composer's edit mode, then send).
    Edit {
        /// The new text.
        text: String,
    },
    /// Deletes it, for everyone or on this account only.
    Delete {
        /// For everyone.
        for_everyone: bool,
    },
}

/// A parsed request.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// Echoed in the response.
    pub id: Value,
    /// What to do.
    pub command: Command,
}

/// The commands.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Starts linking an account of `network` (as clicking it on the welcome page).
    AddAccount {
        /// `Signal`, `Fake`, …
        network: String,
    },
    /// Types values into the current login step's fields (shown in the form).
    FillLogin {
        /// The values.
        values: LoginValues,
    },
    /// Fills (optionally) and submits the current login step.
    SubmitLogin {
        /// The values, or the ones shown.
        values: Option<LoginValues>,
    },
    /// Opens the first conversation whose title contains `title` (case-insensitive).
    Open {
        /// The substring.
        title: String,
    },
    /// Opens the conversation with this id, as a click on its row does (ids come from
    /// `chats`). For drivers that never handle titles.
    OpenId {
        /// The conversation's id.
        conversation: i64,
    },
    /// The conversation list as marks: per row `conversation` (id), `kind` (`direct`,
    /// `group`, `other`), `self` (the note to self), `unread`, `last_activity` (ms) and
    /// `newest_own` (the send state of the previewed message when it is ours, else `none`).
    /// No titles, no previews.
    Chats,
    /// Shows a page.
    Page {
        /// Which.
        name: PageName,
    },
    /// Adds files to the composer of the open conversation in one go, as the file dialog or a
    /// drop does (sent with the next `send`, whose text becomes the caption): `path` for one,
    /// `paths` for any number. The result's `attachments` is the number of files waiting; what
    /// the selection becomes (messages, files the chat does not take) is in `state.composer`
    /// once the core has looked at it.
    Attach {
        /// The files, in order.
        paths: Vec<PathBuf>,
    },
    /// Sends `text` from the composer of the open conversation. With files waiting, `text` may
    /// be empty (no caption).
    Send {
        /// The text.
        text: String,
    },
    /// Answers the challenge an account waits on with what the browser would hand back (the
    /// solution token or the whole `signalcaptcha://` link), through the invokable the
    /// challenge dialog uses.
    Challenge {
        /// The answer (a secret of the test run: never echoed).
        answer: String,
    },
    /// What the user does to a message of the open conversation: a reaction, a reply that
    /// quotes it, an edit, a delete.
    Act {
        /// Which message.
        on: ActOn,
        /// What to do with it.
        action: Action,
    },
    /// Waits until a condition holds.
    Wait {
        /// The condition.
        until: Until,
        /// Give up after this long.
        timeout: Duration,
    },
    /// Grabs the whole window into a PNG (optionally resized first).
    Screenshot {
        /// The PNG.
        path: PathBuf,
        /// Resize the window first.
        size: Option<(u32, u32)>,
        /// How long to let the UI settle before the grab.
        settle: Duration,
    },
    /// Drags the left mouse button from `from` to `to` (window pixels) in `steps` moves: a
    /// scroll-bar drag, a swipe.
    Drag {
        /// Press here.
        from: (f64, f64),
        /// Release here.
        to: (f64, f64),
        /// Intermediate moves (1 … 100).
        steps: u32,
    },
    /// Closes the main window like its close button (to the tray when one is shown, else
    /// Kourier quits).
    Close,
    /// Counts and states for the driver's log: no texts or names.
    State,
    /// Leaves the event loop (exit code 0).
    Quit,
}

/// Values of a login step's fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginValues {
    /// By field id: `{"device_name": "Kourier"}`.
    ById(Vec<(String, String)>),
    /// In the order of the step's fields: `["Kourier"]`.
    InOrder(Vec<String>),
}

/// Pages `page` shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageName {
    /// The welcome page (add an account).
    Welcome,
    /// The conversation list.
    List,
    /// The open conversation.
    Chat,
    /// Settings.
    Settings,
    /// The accounts drawer (sidebar when wide).
    Drawer,
}

/// Conditions of `wait`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Until {
    /// The first answers arrived and the start page is chosen.
    Loaded,
    /// Some account is active and online.
    AccountActive,
    /// The conversation list has at least this many rows.
    ConversationsAtLeast(usize),
    /// The open conversation shows at least this many messages.
    MessagesAtLeast(usize),
    /// The newest own message of the open conversation reached this state (`Sent`,
    /// `Delivered` or `Read`; a later state counts).
    OutgoingState(Receipt),
    /// The login page shows a step of this kind (`input`, `qr`, `progress`, `done`, `failed`,
    /// `waiting`, …).
    Login(LoginKind),
    /// Some account waits for a challenge answer (`true`), or none does (`false`).
    Challenge(bool),
    /// No own message of the open conversation is still queued or being sent (every message
    /// of a send that became several has an answer).
    NonePending,
    /// The composer's files were looked at by the core: `state.composer` says how they will
    /// be sent.
    FilesPlanned,
}

/// A login page kind for [`Until::Login`] (the `kind` property of `LoginController`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoginKind([u8; 16], usize);

impl LoginKind {
    /// Up to 16 ASCII letters.
    pub fn new(kind: &str) -> Option<Self> {
        let bytes = kind.as_bytes();
        if bytes.is_empty() || bytes.len() > 16 || !bytes.iter().all(u8::is_ascii_alphabetic) {
            return None;
        }
        let mut buf = [0u8; 16];
        buf[..bytes.len()].copy_from_slice(bytes);
        Some(Self(buf, bytes.len()))
    }

    /// The kind.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0[..self.1]).unwrap_or_default()
    }
}

impl Until {
    /// Parses `account_active`, `loaded`, `conversations_at_least:N`, `messages_at_least:N`,
    /// `outgoing_state:<Sent|Delivered|Read>`, `login:<kind>`, `challenge`, `no_challenge`,
    /// `none_pending`, `files_planned`.
    pub fn parse(s: &str) -> Result<Self, ParseError> {
        let bad = || ParseError::BadRequest(format!("unknown wait condition {s:?}"));
        let (name, arg) = match s.split_once(':') {
            Some((n, a)) => (n, Some(a)),
            None => (s, None),
        };
        let count = |a: Option<&str>| -> Result<usize, ParseError> {
            a.and_then(|a| a.trim().parse::<usize>().ok())
                .ok_or_else(bad)
        };
        match (name, arg) {
            ("loaded", None) => Ok(Self::Loaded),
            ("account_active", None) => Ok(Self::AccountActive),
            ("challenge", None) => Ok(Self::Challenge(true)),
            ("no_challenge", None) => Ok(Self::Challenge(false)),
            ("none_pending", None) => Ok(Self::NonePending),
            ("files_planned", None) => Ok(Self::FilesPlanned),
            ("conversations_at_least", a) => Ok(Self::ConversationsAtLeast(count(a)?)),
            ("messages_at_least", a) => Ok(Self::MessagesAtLeast(count(a)?)),
            ("outgoing_state", Some(state)) => match state.to_ascii_lowercase().as_str() {
                "sent" => Ok(Self::OutgoingState(Receipt::Sent)),
                "delivered" => Ok(Self::OutgoingState(Receipt::Delivered)),
                "read" => Ok(Self::OutgoingState(Receipt::Read)),
                _ => Err(bad()),
            },
            ("login", Some(kind)) => LoginKind::new(kind).map(Self::Login).ok_or_else(bad),
            _ => Err(bad()),
        }
    }
}

impl std::fmt::Display for Until {
    /// The wire form (`messages_at_least:20`).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Loaded => f.write_str("loaded"),
            Self::AccountActive => f.write_str("account_active"),
            Self::ConversationsAtLeast(n) => write!(f, "conversations_at_least:{n}"),
            Self::MessagesAtLeast(n) => write!(f, "messages_at_least:{n}"),
            Self::OutgoingState(r) => write!(f, "outgoing_state:{}", receipt_name(*r)),
            Self::Login(k) => write!(f, "login:{}", k.as_str()),
            Self::Challenge(true) => f.write_str("challenge"),
            Self::Challenge(false) => f.write_str("no_challenge"),
            Self::NonePending => f.write_str("none_pending"),
            Self::FilesPlanned => f.write_str("files_planned"),
        }
    }
}

/// Rank of an outgoing state for "this state or a later one" (`None` for states that never
/// satisfy a wait: pending, failed, not own).
pub fn receipt_rank(r: Receipt) -> Option<u8> {
    match r {
        Receipt::Sent => Some(1),
        Receipt::Delivered => Some(2),
        Receipt::Read => Some(3),
        Receipt::None | Receipt::Pending | Receipt::Failed => None,
    }
}

/// A stable name of an outgoing state.
pub fn receipt_name(r: Receipt) -> &'static str {
    match r {
        Receipt::None => "none",
        Receipt::Pending => "pending",
        Receipt::Sent => "sent",
        Receipt::Delivered => "delivered",
        Receipt::Read => "read",
        Receipt::Failed => "failed",
    }
}

/// Why a line is not a request.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// Not JSON, or not an object.
    #[error("not a JSON request object: {0}")]
    Json(String),
    /// A known command with missing or wrong arguments.
    #[error("{0}")]
    BadRequest(String),
    /// An unknown command.
    #[error("unknown command {0:?}")]
    UnknownCommand(String),
}

impl ParseError {
    /// The error code on the wire.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Json(_) | Self::BadRequest(_) => "badRequest",
            Self::UnknownCommand(_) => "unknownCommand",
        }
    }
}

/// Parses one line; on failure also returns the request's `id` when it had one.
pub fn parse_line(line: &str) -> Result<Request, (Value, ParseError)> {
    let value: Value =
        serde_json::from_str(line).map_err(|e| (Value::Null, ParseError::Json(e.to_string())))?;
    let Value::Object(obj) = value else {
        return Err((Value::Null, ParseError::Json("not an object".into())));
    };
    let id = obj.get("id").cloned().unwrap_or(Value::Null);
    parse_request(&obj)
        .map(|command| Request {
            id: id.clone(),
            command,
        })
        .map_err(|e| (id, e))
}

fn parse_request(obj: &Map<String, Value>) -> Result<Command, ParseError> {
    let cmd = obj
        .get("cmd")
        .and_then(Value::as_str)
        .ok_or_else(|| ParseError::BadRequest("\"cmd\" must be a string".into()))?;
    let empty = Map::new();
    let args = match obj.get("args") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(a)) => a,
        Some(_) => return Err(ParseError::BadRequest("\"args\" must be an object".into())),
    };
    let string = |name: &str| -> Result<String, ParseError> {
        args.get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| ParseError::BadRequest(format!("{cmd}: \"{name}\" must be a string")))
    };
    Ok(match cmd {
        "add_account" => Command::AddAccount {
            network: string("network")?,
        },
        "fill_login" => Command::FillLogin {
            values: login_values(args.get("values"), cmd)?
                .ok_or_else(|| ParseError::BadRequest("fill_login: \"values\" missing".into()))?,
        },
        "submit_login" => Command::SubmitLogin {
            values: login_values(args.get("values"), cmd)?,
        },
        "open" => {
            let title = string("title")?;
            if title.trim().is_empty() {
                return Err(ParseError::BadRequest("open: empty \"title\"".into()));
            }
            Command::Open { title }
        }
        "open_id" => Command::OpenId {
            conversation: args
                .get("conversation")
                .and_then(Value::as_i64)
                .filter(|id| *id > 0)
                .ok_or_else(|| {
                    ParseError::BadRequest(
                        "open_id: \"conversation\" must be a conversation id".into(),
                    )
                })?,
        },
        "chats" => Command::Chats,
        "page" => Command::Page {
            name: match string("name")?.as_str() {
                "welcome" => PageName::Welcome,
                "list" => PageName::List,
                "chat" => PageName::Chat,
                "settings" => PageName::Settings,
                "drawer" => PageName::Drawer,
                other => {
                    return Err(ParseError::BadRequest(format!(
                        "page: unknown page {other:?} (welcome, list, chat, settings, drawer)"
                    )));
                }
            },
        },
        "attach" => {
            let bad = || {
                ParseError::BadRequest(
                    "attach: \"path\" (a string) or \"paths\" (strings) must be absolute paths"
                        .into(),
                )
            };
            let paths: Vec<PathBuf> = match (args.get("path"), args.get("paths")) {
                (Some(Value::String(one)), None) => vec![PathBuf::from(one)],
                (None, Some(Value::Array(many))) => many
                    .iter()
                    .map(|v| v.as_str().map(PathBuf::from))
                    .collect::<Option<_>>()
                    .ok_or_else(bad)?,
                _ => return Err(bad()),
            };
            if paths.is_empty() || paths.iter().any(|p| !p.is_absolute()) {
                return Err(bad());
            }
            Command::Attach { paths }
        }
        "send" => {
            // An empty text is a send of the waiting files without a caption; without files
            // the composer refuses it.
            let text = match args.get("text") {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(text)) => text.clone(),
                Some(_) => {
                    return Err(ParseError::BadRequest(
                        "send: \"text\" must be a string".into(),
                    ));
                }
            };
            Command::Send { text }
        }
        "challenge" => {
            let answer = string("answer")?;
            if answer.trim().is_empty() {
                return Err(ParseError::BadRequest("challenge: empty \"answer\"".into()));
            }
            Command::Challenge { answer }
        }
        "act" => {
            let on = match string("on")?.as_str() {
                "own" => ActOn::Own,
                "other" => ActOn::Other,
                other => {
                    return Err(ParseError::BadRequest(format!(
                        "act: unknown \"on\" {other:?} (own, other)"
                    )));
                }
            };
            let text = || -> Result<String, ParseError> {
                let text = string("text")?;
                if text.trim().is_empty() {
                    return Err(ParseError::BadRequest("act: empty \"text\"".into()));
                }
                Ok(text)
            };
            let flag = |name: &str| args.get(name).and_then(Value::as_bool).unwrap_or(false);
            let action = match string("action")?.as_str() {
                "react" => Action::React {
                    emoji: string("emoji")?,
                    remove: flag("remove"),
                },
                "reply" => Action::Reply { text: text()? },
                "edit" => Action::Edit { text: text()? },
                "delete" => Action::Delete {
                    for_everyone: flag("for_everyone"),
                },
                other => {
                    return Err(ParseError::BadRequest(format!(
                        "act: unknown \"action\" {other:?} (react, reply, edit, delete)"
                    )));
                }
            };
            Command::Act { on, action }
        }
        "wait" => Command::Wait {
            until: Until::parse(&string("until")?)?,
            timeout: duration_ms(args.get("timeout_ms"), DEFAULT_TIMEOUT, cmd, "timeout_ms")?,
        },
        "screenshot" => {
            let path = PathBuf::from(string("path")?);
            if path.as_os_str().is_empty() {
                return Err(ParseError::BadRequest("screenshot: empty \"path\"".into()));
            }
            let size = match args.get("size") {
                None | Some(Value::Null) => None,
                Some(v) => Some(parse_size(v)?),
            };
            Command::Screenshot {
                path,
                size,
                settle: duration_ms(args.get("settle_ms"), DEFAULT_SETTLE, cmd, "settle_ms")?,
            }
        }
        "drag" => {
            let point = |key: &str| -> Result<(f64, f64), ParseError> {
                let a = args
                    .get(key)
                    .and_then(Value::as_array)
                    .filter(|a| a.len() == 2);
                a.and_then(|a| Some((a[0].as_f64()?, a[1].as_f64()?)))
                    .filter(|(x, y)| (0.0..=16384.0).contains(x) && (0.0..=16384.0).contains(y))
                    .ok_or_else(|| {
                        ParseError::BadRequest(format!("drag: \"{key}\" must be [x, y]"))
                    })
            };
            let steps = match args.get("steps") {
                None | Some(Value::Null) => 10,
                Some(v) => v
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .filter(|n| (1..=100).contains(n))
                    .ok_or_else(|| ParseError::BadRequest("drag: \"steps\" is 1 … 100".into()))?,
            };
            Command::Drag {
                from: point("from")?,
                to: point("to")?,
                steps,
            }
        }
        "close" => Command::Close,
        "state" => Command::State,
        "quit" => Command::Quit,
        other => return Err(ParseError::UnknownCommand(other.to_owned())),
    })
}

fn login_values(v: Option<&Value>, cmd: &str) -> Result<Option<LoginValues>, ParseError> {
    let bad = || {
        ParseError::BadRequest(format!(
            "{cmd}: \"values\" must be an object of strings or an array of strings"
        ))
    };
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(m)) => m
            .iter()
            .map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
            .collect::<Option<Vec<_>>>()
            .map(|v| Some(LoginValues::ById(v)))
            .ok_or_else(bad),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .map(|v| Some(LoginValues::InOrder(v)))
            .ok_or_else(bad),
        Some(_) => Err(bad()),
    }
}

fn duration_ms(
    v: Option<&Value>,
    default: Duration,
    cmd: &str,
    name: &str,
) -> Result<Duration, ParseError> {
    match v {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_u64()
            .map(Duration::from_millis)
            .filter(|d| *d <= MAX_TIMEOUT)
            .ok_or_else(|| {
                ParseError::BadRequest(format!(
                    "{cmd}: \"{name}\" must be milliseconds up to {}",
                    MAX_TIMEOUT.as_millis()
                ))
            }),
    }
}

/// `"1200x800"`, `[1200, 800]` or `{"width": 1200, "height": 800}`; 200 … 8192 each.
pub fn parse_size(v: &Value) -> Result<(u32, u32), ParseError> {
    let pair = match v {
        Value::String(s) => s
            .split_once('x')
            .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?))),
        Value::Array(a) if a.len() == 2 => a[0].as_u64().zip(a[1].as_u64()),
        Value::Object(o) => o
            .get("width")
            .and_then(Value::as_u64)
            .zip(o.get("height").and_then(Value::as_u64)),
        _ => None,
    };
    pair.and_then(|(w, h)| Some((u32::try_from(w).ok()?, u32::try_from(h).ok()?)))
        .filter(|(w, h)| (200..=8192).contains(w) && (200..=8192).contains(h))
        .ok_or_else(|| {
            ParseError::BadRequest("screenshot: \"size\" must be WxH, 200 … 8192 each".into())
        })
}

/// A successful response line.
pub fn ok(id: &Value, result: Value) -> Value {
    json!({"type": "response", "id": id, "ok": true, "result": result})
}

/// A failed response line.
pub fn err(id: &Value, code: &str, message: &str) -> Value {
    json!({"type": "response", "id": id, "ok": false, "error": {"code": code, "message": message}})
}

/// An event line.
pub fn event(name: &str, data: Value) -> Value {
    json!({"type": "event", "event": name, "data": data})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(line: &str) -> Command {
        parse_line(line).expect("parses").command
    }

    fn fails(line: &str) -> (Value, String) {
        let (id, e) = parse_line(line).expect_err("rejected");
        (id, e.code().to_owned())
    }

    #[test]
    fn chats_by_id() {
        assert_eq!(cmd(r#"{"cmd":"chats"}"#), Command::Chats);
        assert_eq!(
            cmd(r#"{"cmd":"open_id","args":{"conversation":7}}"#),
            Command::OpenId { conversation: 7 }
        );
        for bad in [
            r#"{"cmd":"open_id"}"#,
            r#"{"cmd":"open_id","args":{"conversation":"7"}}"#,
            r#"{"cmd":"open_id","args":{"conversation":0}}"#,
            r#"{"cmd":"open_id","args":{"conversation":-3}}"#,
        ] {
            assert_eq!(fails(bad).1, "badRequest", "{bad}");
        }
    }

    #[test]
    fn message_actions() {
        assert_eq!(
            cmd(r#"{"cmd":"act","args":{"on":"other","action":"react","emoji":"👍"}}"#),
            Command::Act {
                on: ActOn::Other,
                action: Action::React {
                    emoji: "👍".into(),
                    remove: false
                }
            }
        );
        assert_eq!(
            cmd(r#"{"cmd":"act","args":{"on":"other","action":"reply","text":"yes"}}"#),
            Command::Act {
                on: ActOn::Other,
                action: Action::Reply { text: "yes".into() }
            }
        );
        assert_eq!(
            cmd(r#"{"cmd":"act","args":{"on":"own","action":"edit","text":"fixed"}}"#),
            Command::Act {
                on: ActOn::Own,
                action: Action::Edit {
                    text: "fixed".into()
                }
            }
        );
        assert_eq!(
            cmd(r#"{"cmd":"act","args":{"on":"own","action":"delete","for_everyone":true}}"#),
            Command::Act {
                on: ActOn::Own,
                action: Action::Delete { for_everyone: true }
            }
        );
        for bad in [
            r#"{"cmd":"act","args":{"on":"all","action":"delete"}}"#,
            r#"{"cmd":"act","args":{"on":"own","action":"forward"}}"#,
            r#"{"cmd":"act","args":{"on":"own","action":"edit","text":" "}}"#,
            r#"{"cmd":"act","args":{"on":"own","action":"react"}}"#,
        ] {
            assert_eq!(fails(bad).1, "badRequest", "{bad}");
        }
    }

    #[test]
    fn commands() {
        assert_eq!(
            cmd(r#"{"id":1,"cmd":"add_account","args":{"network":"Signal"}}"#),
            Command::AddAccount {
                network: "Signal".into()
            }
        );
        assert_eq!(
            cmd(r#"{"id":2,"cmd":"submit_login","args":{"values":["Kourier demo"]}}"#),
            Command::SubmitLogin {
                values: Some(LoginValues::InOrder(vec!["Kourier demo".into()]))
            }
        );
        assert_eq!(
            cmd(r#"{"cmd":"submit_login"}"#),
            Command::SubmitLogin { values: None }
        );
        assert_eq!(
            cmd(r#"{"cmd":"fill_login","args":{"values":{"device_name":"K"}}}"#),
            Command::FillLogin {
                values: LoginValues::ById(vec![("device_name".into(), "K".into())])
            }
        );
        assert_eq!(
            cmd(r#"{"cmd":"open","args":{"title":"book"}}"#),
            Command::Open {
                title: "book".into()
            }
        );
        assert_eq!(
            cmd(r#"{"cmd":"page","args":{"name":"settings"}}"#),
            Command::Page {
                name: PageName::Settings
            }
        );
        assert_eq!(
            cmd(r#"{"cmd":"attach","args":{"path":"/tmp/a.png"}}"#),
            Command::Attach {
                paths: vec![PathBuf::from("/tmp/a.png")]
            }
        );
        assert_eq!(
            cmd(r#"{"cmd":"attach","args":{"paths":["/tmp/a.png","/tmp/b.pdf"]}}"#),
            Command::Attach {
                paths: vec![PathBuf::from("/tmp/a.png"), PathBuf::from("/tmp/b.pdf")]
            }
        );
        assert_eq!(
            cmd(r#"{"cmd":"send","args":{"text":"hi"}}"#),
            Command::Send { text: "hi".into() }
        );
        // Files without a caption.
        assert_eq!(
            cmd(r#"{"cmd":"send"}"#),
            Command::Send {
                text: String::new()
            }
        );
        assert_eq!(
            cmd(r#"{"cmd":"challenge","args":{"answer":"signalcaptcha://x.y"}}"#),
            Command::Challenge {
                answer: "signalcaptcha://x.y".into()
            }
        );
        assert_eq!(
            cmd(r#"{"cmd":"wait","args":{"until":"messages_at_least:20","timeout_ms":5000}}"#),
            Command::Wait {
                until: Until::MessagesAtLeast(20),
                timeout: Duration::from_secs(5)
            }
        );
        assert_eq!(
            cmd(r#"{"cmd":"screenshot","args":{"path":"a.png","size":"420x800"}}"#),
            Command::Screenshot {
                path: "a.png".into(),
                size: Some((420, 800)),
                settle: DEFAULT_SETTLE
            }
        );
        assert_eq!(
            cmd(r#"{"cmd":"screenshot","args":{"path":"a.png","size":[1200,800],"settle_ms":0}}"#),
            Command::Screenshot {
                path: "a.png".into(),
                size: Some((1200, 800)),
                settle: Duration::ZERO
            }
        );
        assert_eq!(cmd(r#"{"cmd":"quit","args":null}"#), Command::Quit);
        assert_eq!(
            cmd(r#"{"cmd":"drag","args":{"from":[10,700],"to":[10.5,300],"steps":5}}"#),
            Command::Drag {
                from: (10.0, 700.0),
                to: (10.5, 300.0),
                steps: 5
            }
        );
        assert_eq!(cmd(r#"{"cmd":"state"}"#), Command::State);
        assert_eq!(cmd(r#"{"cmd":"close"}"#), Command::Close);
    }

    #[test]
    fn wait_conditions() {
        assert_eq!(Until::parse("loaded"), Ok(Until::Loaded));
        assert_eq!(Until::parse("account_active"), Ok(Until::AccountActive));
        assert_eq!(
            Until::parse("conversations_at_least:3"),
            Ok(Until::ConversationsAtLeast(3))
        );
        assert_eq!(
            Until::parse("outgoing_state:Sent"),
            Ok(Until::OutgoingState(Receipt::Sent))
        );
        assert_eq!(
            Until::parse("outgoing_state:read"),
            Ok(Until::OutgoingState(Receipt::Read))
        );
        match Until::parse("login:qr") {
            Ok(Until::Login(k)) => assert_eq!(k.as_str(), "qr"),
            other => panic!("{other:?}"),
        }
        assert_eq!(Until::parse("challenge"), Ok(Until::Challenge(true)));
        assert_eq!(Until::parse("no_challenge"), Ok(Until::Challenge(false)));
        for wire in [
            "loaded",
            "account_active",
            "challenge",
            "no_challenge",
            "none_pending",
            "files_planned",
            "conversations_at_least:3",
            "messages_at_least:20",
            "outgoing_state:delivered",
            "login:input",
        ] {
            assert_eq!(Until::parse(wire).expect(wire).to_string(), wire);
        }
        for bad in [
            "outgoing_state:Pending",
            "outgoing_state",
            "messages_at_least",
            "messages_at_least:x",
            "account_active:1",
            "challenge:1",
            "none_pending:2",
            "login:",
            "login:q r",
            "nope",
        ] {
            assert!(Until::parse(bad).is_err(), "{bad}");
        }
        assert!(receipt_rank(Receipt::Read) > receipt_rank(Receipt::Sent));
        assert_eq!(receipt_rank(Receipt::Pending), None);
    }

    #[test]
    fn rejects() {
        assert_eq!(fails("not json"), (Value::Null, "badRequest".into()));
        assert_eq!(fails("[1]"), (Value::Null, "badRequest".into()));
        assert_eq!(
            fails(r#"{"id":7,"cmd":"fly"}"#),
            (json!(7), "unknownCommand".into())
        );
        assert_eq!(
            fails(r#"{"id":"a","cmd":"open","args":{}}"#),
            (json!("a"), "badRequest".into())
        );
        for line in [
            r#"{"cmd":"open","args":{"title":"  "}}"#,
            r#"{"cmd":"send","args":{"text":7}}"#,
            r#"{"cmd":"attach","args":{"path":"relative.png"}}"#,
            r#"{"cmd":"attach","args":{"paths":[]}}"#,
            r#"{"cmd":"attach","args":{"paths":["/a",3]}}"#,
            r#"{"cmd":"attach","args":{"path":"/a","paths":["/b"]}}"#,
            r#"{"cmd":"attach","args":{}}"#,
            r#"{"cmd":"challenge","args":{"answer":" "}}"#,
            r#"{"cmd":"challenge"}"#,
            r#"{"cmd":"page","args":{"name":"nowhere"}}"#,
            r#"{"cmd":"wait","args":{"until":"loaded","timeout_ms":-1}}"#,
            r#"{"cmd":"wait","args":{"until":"loaded","timeout_ms":999999999}}"#,
            r#"{"cmd":"screenshot","args":{"path":"a.png","size":"10x10"}}"#,
            r#"{"cmd":"screenshot","args":{"path":""}}"#,
            r#"{"cmd":"drag","args":{"from":[1],"to":[1,2]}}"#,
            r#"{"cmd":"drag","args":{"from":[1,2],"to":[1,2],"steps":0}}"#,
            r#"{"cmd":"fill_login","args":{}}"#,
            r#"{"cmd":"submit_login","args":{"values":[1]}}"#,
            r#"{"cmd":"add_account","args":"Signal"}"#,
            r#"{"cmd":3}"#,
        ] {
            assert_eq!(fails(line).1, "badRequest", "{line}");
        }
    }

    #[test]
    fn output_lines() {
        assert_eq!(
            ok(&json!(1), json!({"a": 1})),
            json!({"type":"response","id":1,"ok":true,"result":{"a":1}})
        );
        assert_eq!(
            err(&Value::Null, "timeout", "x")["error"]["code"],
            json!("timeout")
        );
        assert_eq!(
            event("login_qr", json!({"url": "sgnl://x"}))["event"],
            json!("login_qr")
        );
    }
}

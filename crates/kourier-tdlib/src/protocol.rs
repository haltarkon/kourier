//! The frame protocol between the app and the helper (TG-1 plan §2.3; decisions D3 M1, M4).
//!
//! One frame per line: UTF-8, ending in `\n`, the first byte a tag, then (for frames with a
//! payload) ONE space and the payload. TDLib's JSON never holds a raw newline; a payload that does
//! is refused when it is encoded. A line longer than [`MAX_LINE`] ends the helper.
//!
//! | Direction | Frame | Meaning |
//! |---|---|---|
//! | app -> helper | `C <seq>` | create a client; answered `K <seq> <client>` |
//! | app -> helper | `G <client> <port> <user> <pass>` | the gate of that client, ONCE per client: port 1..=65535, user and pass 1..=127 bytes (TDLib's limits; the helper writes `addProxy` itself and checks its answer) |
//! | app -> helper | `S <client> <json>` | `td_send` |
//! | app -> helper | `E <seq> <json>` | `td_execute`; answered `X <seq> <json>` |
//! | app -> helper | `Q` (or EOF) | close every client and exit |
//! | helper -> app | `H <json>` | hello, once, after the log hardening |
//! | helper -> app | `U <json>` | one `td_receive` result verbatim |
//! | helper -> app | `F <code> [<extra>]` | a fixed code, never text; with the refused `S` request's `@extra` (a refused `E` is also answered `X <seq>` with a fixed error); `gate-proxy-failed` says the helper closed a client whose gate proxy TDLib did not take as written, `files-option-failed` one whose `store_all_files_in_files_directory` option TDLib did not accept |
//! | helper -> app | `P` | heartbeat of the receiving thread (at least every 30 s) |
//! | helper -> app | `B <json>` | bye, the last frame |
//!
//! Nothing here prints a payload: the errors name only what was wrong with a line, and the
//! `Debug` of a frame that carries JSON or gate credentials shows its length, not its content.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read};

use serde::{Deserialize, Serialize};

/// The longest line either side accepts, `\n` excluded.
pub const MAX_LINE: usize = 32 << 20;

/// The ports TDLib walks for a data centre (`ConnectionCreator.cpp`): no gate may listen on one of
/// them. The gate's address is not checked by Landlock (it names a port only), so a gate port that
/// is also a data-centre port would let a TDLib without its proxy reach every data centre on that
/// port (TG-1b fix G6). `kourier-telegram`'s `gate/ranges.rs` names the same three ports for what
/// the gate forwards; a test there keeps the two lists equal.
pub const DC_PORTS: [u16; 3] = [443, 80, 5222];

/// Whether `port` may be a gate's port: unprivileged (1024 or more) and none of [`DC_PORTS`]. The
/// gate binds again until the system's pick passes (G6); the helper's command line and its `G`
/// frames refuse any other.
pub fn gate_port_allowed(port: u16) -> bool {
    port >= 1024 && !DC_PORTS.contains(&port)
}

/// The `@extra` values from this one up belong to the helper's own requests (below 2^53, so every
/// JSON reader keeps them exact). The app numbers its requests from 1 and never reaches it; a
/// request of the app that carries one is a bad frame.
pub const HELPER_EXTRA_MIN: u64 = 1 << 52;

/// The `@extra` of the `addProxy` the helper writes for a client's gate. The helper checks the
/// answer itself (fix F2: an error, a proxy that is not enabled, another server or another port
/// closes that client with `F gate-proxy-failed`) and passes it on to the app, which routes it to
/// its own typed place; the answers of the helper's other requests (the proxy clean-up, `close`)
/// are not passed on.
pub const GATE_PROXY_EXTRA: u64 = HELPER_EXTRA_MIN + 1;

/// The longest gate user name or password: TDLib's SOCKS5 login takes less than 128 bytes of each
/// (`tdnet/td/net/Socks5.cpp:59-63`; a longer one fails every connection).
pub const MAX_CREDENTIAL: usize = 127;

/// A frame the app writes to the helper.
#[derive(Clone, PartialEq, Eq)]
pub enum ToHelper {
    /// `td_create_client_id()`; answered by [`FromHelper::Created`] with the same `seq`.
    Create { seq: u64 },
    /// The SOCKS5 gate of `client`: the helper writes TDLib's `addProxy` for it itself, right
    /// before it forwards the client's `setTdlibParameters` (decisions D3 M1).
    Gate {
        client: i32,
        port: u16,
        user: String,
        pass: String,
    },
    /// `td_send(client, json)`.
    Send { client: i32, json: String },
    /// `td_execute(json)`; answered by [`FromHelper::Executed`] with the same `seq`.
    Execute { seq: u64, json: String },
    /// Close every client and exit.
    Quit,
}

/// A frame the helper writes to the app.
#[derive(Clone, PartialEq, Eq)]
pub enum FromHelper {
    Hello(Hello),
    Created {
        seq: u64,
        client: i32,
    },
    Update(String),
    Executed {
        seq: u64,
        json: String,
    },
    /// A fixed code; `extra` is the `@extra` of the request it refuses, when there was one.
    Failure {
        code: FailureCode,
        extra: Option<u64>,
    },
    Heartbeat,
    Bye(Bye),
}

/// The helper's hello: the protocol version and what the library says it is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub tdlib: String,
    pub commit: String,
    /// Kourier's patch tag of the library: `getOption("kourier_patch_level")` through `td_execute`
    /// (the newest patch of `third_party/tdlib/patches/`, a synchronous option patch 0002 adds;
    /// pre3 K5). Empty where the library does not know the option (a stock, 0001-only or pre-0002
    /// library); the app's `check_pin` refuses anything but the pin's value, so a hello without the
    /// field decodes to the empty tag and is refused as a wrong helper, not as a bad frame.
    #[serde(default)]
    pub patch_level: String,
    /// `on` or `unavailable` (the helper's Landlock, `lockdown::apply`); `off` where no lock-down
    /// ran (the serve loop under a test).
    pub landlock: String,
}

/// The helper's last frame: the clients TDLib reported closed, and whether its deadline forced the
/// end.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bye {
    pub closed: Vec<i32>,
    pub forced: bool,
}

/// The fixed codes of an `F` frame. The fatal ones end the helper.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FailureCode {
    /// TDLib's log could not be hardened; no client was created (fatal).
    LogHardeningFailed,
    /// The app asked for a log request; it was not forwarded.
    RefusedLogRequest,
    /// The app asked for a proxy request; the helper owns the proxy rule (D3 M1).
    RefusedProxyRequest,
    /// The app asked for a request Kourier never makes (`serve::FORBIDDEN_REQUESTS`, TG-2 M5); it was
    /// not forwarded. Not fatal.
    RefusedRequest,
    /// `setTdlibParameters` for a client without a gate: refused (D3 M1).
    RefusedParametersWithoutGate,
    /// A line that is no frame (fatal).
    BadFrame,
    /// TDLib reported a fatal error and is about to abort (fatal).
    TdlibFatal,
    /// The library could not be opened (fatal).
    LibOpenFailed,
    /// The library lacks an entry point (fatal).
    LibSymbolMissing,
    /// TDLib's answer to the helper's own `addProxy` for a client's gate was an error, a proxy
    /// that is not enabled, another server or another port: the helper closed THAT client (fix
    /// F2). Fatal for the client, not for the helper.
    GateProxyFailed,
    /// The helper's network lock-down (`lockdown::apply`) failed; no client was created (fatal).
    LockDownFailed,
    /// `setTdlibParameters` names a `database_directory` or `files_directory` other than the
    /// helper's `--database-dir`/`--files-dir` (the only directories its filesystem lock-down
    /// allows; TG-2b, since PROTOCOL 4): refused before TDLib sees it. Not fatal.
    RefusedParametersPaths,
    /// TDLib's answer to the helper's own `setOption store_all_files_in_files_directory` was not
    /// `ok` (pre3 K2): the helper closed THAT client, whose downloads would land in the database
    /// directory, where the filesystem rules give no REFER. Fatal for the client, not for the
    /// helper (like [`FailureCode::GateProxyFailed`]).
    FilesOptionFailed,
}

impl FailureCode {
    const ALL: [FailureCode; 13] = [
        Self::LogHardeningFailed,
        Self::RefusedLogRequest,
        Self::RefusedProxyRequest,
        Self::RefusedRequest,
        Self::RefusedParametersWithoutGate,
        Self::BadFrame,
        Self::TdlibFatal,
        Self::LibOpenFailed,
        Self::LibSymbolMissing,
        Self::GateProxyFailed,
        Self::LockDownFailed,
        Self::RefusedParametersPaths,
        Self::FilesOptionFailed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::LogHardeningFailed => "log-hardening-failed",
            Self::RefusedLogRequest => "refused-log-request",
            Self::RefusedProxyRequest => "refused-proxy-request",
            Self::RefusedRequest => "refused-request",
            Self::RefusedParametersWithoutGate => "refused-parameters-without-gate",
            Self::BadFrame => "bad-frame",
            Self::TdlibFatal => "tdlib-fatal",
            Self::LibOpenFailed => "lib-open-failed",
            Self::LibSymbolMissing => "lib-symbol-missing",
            Self::GateProxyFailed => "gate-proxy-failed",
            Self::LockDownFailed => "lock-down-failed",
            Self::RefusedParametersPaths => "refused-parameters-paths",
            Self::FilesOptionFailed => "files-option-failed",
        }
    }

    pub fn parse(s: &str) -> Option<FailureCode> {
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }

    /// Whether the helper ends after this code.
    pub fn is_fatal(self) -> bool {
        !matches!(
            self,
            Self::RefusedLogRequest
                | Self::RefusedProxyRequest
                | Self::RefusedRequest
                | Self::RefusedParametersWithoutGate
                | Self::RefusedParametersPaths
                | Self::GateProxyFailed
                | Self::FilesOptionFailed
        )
    }
}

impl fmt::Display for FailureCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What was wrong with a line. Never carries the line's content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    Empty,
    UnknownTag,
    /// The tag is not followed by exactly one space and a payload (or a payload-less frame has one).
    Shape,
    BadNumber,
    BadJson,
    NotUtf8,
    TooLong,
    /// A payload to encode holds a newline.
    Newline,
    /// The stream ended inside a line.
    Truncated,
    Io(io::ErrorKind),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Empty => "an empty line",
            Self::UnknownTag => "an unknown frame tag",
            Self::Shape => "a frame of the wrong shape",
            Self::BadNumber => "a frame with a bad number",
            Self::BadJson => "a frame with bad JSON",
            Self::NotUtf8 => "a frame that is not UTF-8",
            Self::TooLong => "a frame longer than the limit",
            Self::Newline => "a payload with a newline",
            Self::Truncated => "a stream that ended inside a frame",
            Self::Io(kind) => return write!(f, "an I/O error ({kind:?})"),
        };
        f.write_str(s)
    }
}

impl std::error::Error for FrameError {}

impl fmt::Debug for ToHelper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Create { seq } => write!(f, "Create({seq})"),
            Self::Gate { client, .. } => write!(f, "Gate(client {client})"),
            Self::Send { client, json } => write!(f, "Send(client {client}, {} bytes)", json.len()),
            Self::Execute { seq, json } => write!(f, "Execute({seq}, {} bytes)", json.len()),
            Self::Quit => f.write_str("Quit"),
        }
    }
}

impl fmt::Debug for FromHelper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hello(h) => write!(f, "Hello(protocol {})", h.protocol),
            Self::Created { seq, client } => write!(f, "Created({seq}, client {client})"),
            Self::Update(json) => write!(f, "Update({} bytes)", json.len()),
            Self::Executed { seq, json } => write!(f, "Executed({seq}, {} bytes)", json.len()),
            Self::Failure { code, extra } => write!(f, "Failure({code}, {extra:?})"),
            Self::Heartbeat => f.write_str("Heartbeat"),
            Self::Bye(b) => write!(f, "{b:?}"),
        }
    }
}

fn no_newline(s: &str) -> Result<&str, FrameError> {
    if s.contains('\n') || s.contains('\r') {
        Err(FrameError::Newline)
    } else {
        Ok(s)
    }
}

/// A gate credential: printable ASCII without spaces (base64url in practice), 1 to
/// [`MAX_CREDENTIAL`] bytes (TDLib's SOCKS5 login takes no more, fix F1).
fn credential(s: &str) -> Result<&str, FrameError> {
    if s.is_empty() || s.len() > MAX_CREDENTIAL || !s.bytes().all(|b| b.is_ascii_graphic()) {
        Err(FrameError::Shape)
    } else {
        Ok(s)
    }
}

/// A gate's port: 1..=65535 (TDLib: `Proxy.cpp:28-30`, `port <= 0 || port > 65535` is
/// "Wrong port number"); the `u16` holds the upper bound.
fn gate_port(port: u16) -> Result<u16, FrameError> {
    if port == 0 {
        Err(FrameError::BadNumber)
    } else {
        Ok(port)
    }
}

impl ToHelper {
    /// Appends the frame and its `\n` to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), FrameError> {
        let line = match self {
            Self::Create { seq } => format!("C {seq}"),
            Self::Gate {
                client,
                port,
                user,
                pass,
            } => format!(
                "G {client} {} {} {}",
                gate_port(*port)?,
                credential(user)?,
                credential(pass)?
            ),
            Self::Send { client, json } => format!("S {client} {}", no_newline(json)?),
            Self::Execute { seq, json } => format!("E {seq} {}", no_newline(json)?),
            Self::Quit => "Q".to_owned(),
        };
        push_line(out, &line)
    }
}

impl FromHelper {
    /// Appends the frame and its `\n` to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), FrameError> {
        fn json<T: Serialize>(v: &T) -> Result<String, FrameError> {
            serde_json::to_string(v).map_err(|_| FrameError::BadJson)
        }
        let line = match self {
            Self::Hello(h) => format!("H {}", json(h)?),
            Self::Created { seq, client } => format!("K {seq} {client}"),
            Self::Update(j) => format!("U {}", no_newline(j)?),
            Self::Executed { seq, json } => format!("X {seq} {}", no_newline(json)?),
            Self::Failure {
                code,
                extra: Some(e),
            } => format!("F {code} {e}"),
            Self::Failure { code, extra: None } => format!("F {code}"),
            Self::Heartbeat => "P".to_owned(),
            Self::Bye(b) => format!("B {}", json(b)?),
        };
        push_line(out, &line)
    }
}

fn push_line(out: &mut Vec<u8>, line: &str) -> Result<(), FrameError> {
    if line.len() > MAX_LINE {
        return Err(FrameError::TooLong);
    }
    out.extend_from_slice(line.as_bytes());
    out.push(b'\n');
    Ok(())
}

/// Splits a line into its tag and its payload; `payload` says whether the tag wants one.
fn split(line: &[u8], payload: bool) -> Result<&str, FrameError> {
    let line = std::str::from_utf8(line).map_err(|_| FrameError::NotUtf8)?;
    if payload {
        // The tag, ONE space, then a non-empty payload.
        if line.len() < 3 || line.as_bytes()[1] != b' ' {
            return Err(FrameError::Shape);
        }
        Ok(&line[2..])
    } else if line.len() == 1 {
        Ok("")
    } else {
        Err(FrameError::Shape)
    }
}

fn number<T: std::str::FromStr>(s: &str) -> Result<T, FrameError> {
    // Digits only: no sign, no spaces, no leading `+`.
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(FrameError::BadNumber);
    }
    s.parse().map_err(|_| FrameError::BadNumber)
}

/// `<number> <rest>`.
fn number_and_rest<T: std::str::FromStr>(s: &str) -> Result<(T, &str), FrameError> {
    let (n, rest) = s.split_once(' ').ok_or(FrameError::Shape)?;
    if rest.is_empty() {
        return Err(FrameError::Shape);
    }
    Ok((number(n)?, rest))
}

/// Decodes one line (without its `\n`) the app wrote.
pub fn decode_to_helper(line: &[u8]) -> Result<ToHelper, FrameError> {
    let tag = *line.first().ok_or(FrameError::Empty)?;
    if line.len() > MAX_LINE {
        return Err(FrameError::TooLong);
    }
    match tag {
        b'C' => Ok(ToHelper::Create {
            seq: number(split(line, true)?)?,
        }),
        b'G' => {
            let p = split(line, true)?;
            let parts: Vec<&str> = p.split(' ').collect();
            let [client, port, user, pass] = parts.as_slice() else {
                return Err(FrameError::Shape);
            };
            Ok(ToHelper::Gate {
                client: number(client)?,
                port: gate_port(number(port)?)?,
                user: credential(user)?.to_owned(),
                pass: credential(pass)?.to_owned(),
            })
        }
        b'S' => {
            let (client, json) = number_and_rest(split(line, true)?)?;
            Ok(ToHelper::Send {
                client,
                json: json.to_owned(),
            })
        }
        b'E' => {
            let (seq, json) = number_and_rest(split(line, true)?)?;
            Ok(ToHelper::Execute {
                seq,
                json: json.to_owned(),
            })
        }
        b'Q' => split(line, false).map(|_| ToHelper::Quit),
        _ => Err(FrameError::UnknownTag),
    }
}

/// Decodes one line (without its `\n`) the helper wrote.
pub fn decode_from_helper(line: &[u8]) -> Result<FromHelper, FrameError> {
    let tag = *line.first().ok_or(FrameError::Empty)?;
    if line.len() > MAX_LINE {
        return Err(FrameError::TooLong);
    }
    match tag {
        b'H' => serde_json::from_str(split(line, true)?)
            .map(FromHelper::Hello)
            .map_err(|_| FrameError::BadJson),
        b'K' => {
            let (seq, client) = number_and_rest(split(line, true)?)?;
            Ok(FromHelper::Created {
                seq,
                client: number(client)?,
            })
        }
        b'U' => Ok(FromHelper::Update(split(line, true)?.to_owned())),
        b'X' => {
            let (seq, json) = number_and_rest(split(line, true)?)?;
            Ok(FromHelper::Executed {
                seq,
                json: json.to_owned(),
            })
        }
        b'F' => {
            let p = split(line, true)?;
            let (code, extra) = match p.split_once(' ') {
                Some((c, e)) => (c, Some(number(e)?)),
                None => (p, None),
            };
            Ok(FromHelper::Failure {
                code: FailureCode::parse(code).ok_or(FrameError::Shape)?,
                extra,
            })
        }
        b'P' => split(line, false).map(|_| FromHelper::Heartbeat),
        b'B' => serde_json::from_str(split(line, true)?)
            .map(FromHelper::Bye)
            .map_err(|_| FrameError::BadJson),
        _ => Err(FrameError::UnknownTag),
    }
}

/// Reads `\n`-terminated lines of at most [`MAX_LINE`] bytes.
pub struct LineReader<R> {
    inner: BufReader<R>,
    max: usize,
}

impl<R: Read> LineReader<R> {
    pub fn new(r: R) -> Self {
        Self::with_limit(r, MAX_LINE)
    }

    /// A reader with another limit (tests).
    pub fn with_limit(r: R, max: usize) -> Self {
        Self {
            inner: BufReader::with_capacity(64 * 1024, r),
            max,
        }
    }

    /// The next line without its `\n`; `None` at a clean end of the stream.
    pub fn next_line(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        let mut line = Vec::new();
        loop {
            let buf = match self.inner.fill_buf() {
                Ok(b) => b,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(FrameError::Io(e.kind())),
            };
            if buf.is_empty() {
                return if line.is_empty() {
                    Ok(None)
                } else {
                    Err(FrameError::Truncated)
                };
            }
            let (take, done) = match buf.iter().position(|&b| b == b'\n') {
                Some(i) => (i, true),
                None => (buf.len(), false),
            };
            if line.len() + take > self.max {
                return Err(FrameError::TooLong);
            }
            line.extend_from_slice(&buf[..take]);
            self.inner.consume(if done { take + 1 } else { take });
            if done {
                return Ok(Some(line));
            }
        }
    }
}

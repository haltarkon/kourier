//! The app's end of the frame protocol to one helper (TG-1 plan §2.3; decisions D3 H3, M5).
//!
//! Two threads per link, so neither side's reading ever waits on its own writing:
//! - the WRITER drains an unbounded queue of frames: a request never awaits writer capacity;
//!   control requests (`E`, `C`) are capped in flight instead ([`MAX_CONTROL_IN_FLIGHT`]), which
//!   bounds the helper's control queue;
//! - the READER routes every answer itself (`@extra` of an `S` request, `seq` of an `E` or a `C`)
//!   to the request that waits for it, and puts everything else (updates, heartbeats, the bye,
//!   failures) into a bounded inbound queue. Every await of the actor that waits for an answer
//!   goes through [`drive`], which keeps draining that queue, so a burst of updates can never stop
//!   an answer behind it.
//!
//! An answer nobody waits for (an unknown `@extra`, the late answer of a timed-out request) is a
//! STRAY: counted, never forwarded. Nothing here logs or keeps a frame's content: errors are fixed
//! classes, TDLib's error message is dropped and only its code kept.
//!
//! Frames travel to the writer in zeroizing buffers and the writer zeroes its own buffer after every
//! write; a request whose JSON holds a secret (the database key, a typed code, password or e-mail
//! address) goes through [`HelperLink::request_secret`], which never builds a `serde_json::Value` or
//! a `format!` string of it (TG-1b Q-C3-1, TG-2 M10).

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::io::{self, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use kourier_tdlib::PROTOCOL;
use kourier_tdlib::protocol::{
    Bye, FailureCode, FrameError, FromHelper, GATE_PROXY_EXTRA, HELPER_EXTRA_MIN, Hello,
    LineReader, MAX_LINE, ToHelper, decode_from_helper,
};
use serde_json::Value;
use tokio::sync::{Semaphore, mpsc, oneshot};
use zeroize::{Zeroize, Zeroizing};

use crate::error::{LinkError, TdClass};
use crate::pin::{PIN, Pin};

/// How many inbound frames (updates, heartbeats, ...) wait for the actor before the reader waits.
pub(crate) const INBOUND_QUEUE: usize = 1024;

/// How many `E` and `C` requests may wait for their answer at once.
pub(crate) const MAX_CONTROL_IN_FLIGHT: usize = 64;

/// The link's timeouts.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LinkOptions {
    /// How long the helper may take to send its hello (`open`; the supervisor has its own).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) hello_timeout: Duration,
    /// How long a request may wait for its answer.
    pub(crate) request_timeout: Duration,
}

impl Default for LinkOptions {
    fn default() -> Self {
        Self {
            hello_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(60),
        }
    }
}

/// What the reader hands to the actor: everything that is not the answer of a request.
pub(crate) enum Inbound {
    /// The helper's first frame.
    Hello(Hello),
    /// A TDLib update (JSON), not an answer of a request.
    Update(String),
    /// The helper's receiver is alive (D3 M4).
    Heartbeat,
    /// A non-fatal refusal that names no request.
    Refused(FailureCode),
    /// A fatal code: the helper is ending.
    Fatal(FailureCode),
    /// TDLib's answer to the `addProxy` the helper wrote for `client`'s gate (fix F2; D3 B1): `ok`
    /// when it is an `addedProxy` that is enabled and names 127.0.0.1, `port` the port it names.
    /// The helper checked it too (a bad one closed the client and wrote `F gate-proxy-failed`);
    /// the supervisor compares `port` with the gate it started. Nothing else of the answer is kept.
    GateProxy {
        client: i32,
        ok: bool,
        port: Option<u16>,
    },
    /// The helper's last frame.
    Bye(Bye),
    /// The helper wrote a line that is no frame; the reader stopped.
    Garbled,
    /// The helper's end is gone; the reader stopped.
    Closed,
}

impl fmt::Debug for Inbound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Hello(_) => "Hello",
            Self::Update(_) => "Update",
            Self::Heartbeat => "Heartbeat",
            Self::Refused(_) => "Refused",
            Self::Fatal(_) => "Fatal",
            Self::GateProxy { .. } => "GateProxy",
            Self::Bye(_) => "Bye",
            Self::Garbled => "Garbled",
            Self::Closed => "Closed",
        };
        f.write_str(name)
    }
}

type Answer<T> = oneshot::Sender<Result<T, LinkError>>;

/// The requests that wait for an answer.
#[derive(Default)]
struct Pending {
    sends: HashMap<u64, Answer<Value>>,
    executes: HashMap<u64, Answer<String>>,
    creates: HashMap<u64, Answer<i32>>,
    /// The reader stopped: nothing can be answered any more.
    closed: bool,
}

struct Shared {
    pending: Mutex<Pending>,
    strays: AtomicU64,
    next: AtomicU64,
    control: Semaphore,
    opts: LinkOptions,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Pending> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn stray(&self) {
        self.strays.fetch_add(1, Ordering::SeqCst);
    }
}

/// The app's end of one helper's link. Cheap to clone; the writer ends (and the helper sees the
/// end of its input) when the last clone is dropped.
#[derive(Clone)]
pub(crate) struct HelperLink {
    shared: Arc<Shared>,
    /// Encoded frames (a frame is encoded by the one who writes it, so a bad one fails only that
    /// request, fix F14), each zeroed when the writer drops it (TG-2 M10).
    out: std::sync::mpsc::Sender<Zeroizing<Vec<u8>>>,
}

impl HelperLink {
    /// Starts the reader and the writer over `sock`; the receiver gets the inbound frames.
    pub(crate) fn start(
        sock: UnixStream,
        opts: LinkOptions,
    ) -> std::io::Result<(Self, mpsc::Receiver<Inbound>)> {
        let read = sock.try_clone()?;
        let (out, out_rx) = std::sync::mpsc::channel();
        let (in_tx, in_rx) = mpsc::channel(INBOUND_QUEUE);
        let shared = Arc::new(Shared {
            pending: Mutex::new(Pending::default()),
            strays: AtomicU64::new(0),
            next: AtomicU64::new(1),
            control: Semaphore::new(MAX_CONTROL_IN_FLIGHT),
            opts,
        });
        // Both threads log to the subscriber of the thread that started the link (the app's global
        // one; a test's own capture).
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let to_writer = dispatch.clone();
        std::thread::Builder::new()
            .name("tg-link-w".to_owned())
            .spawn(move || {
                tracing::dispatcher::with_default(&to_writer, || writer(sock, &out_rx));
            })?;
        let routed = shared.clone();
        std::thread::Builder::new()
            .name("tg-link-r".to_owned())
            .spawn(move || {
                tracing::dispatcher::with_default(&dispatch, || reader(read, &routed, &in_tx));
            })?;
        Ok((Self { shared, out }, in_rx))
    }

    /// The hello, checked against the pinned TDLib, then ONE client: a helper of another build
    /// never gets a `C` frame.
    // Used by the link's tests (and, for `execute`, by the watchdog's probe next).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn open(
        &self,
        inbound: &mut mpsc::Receiver<Inbound>,
    ) -> Result<i32, LinkError> {
        let hello = tokio::time::timeout(self.shared.opts.hello_timeout, async {
            loop {
                match inbound.recv().await {
                    Some(Inbound::Heartbeat) => {}
                    Some(Inbound::Hello(h)) => return Ok(h),
                    Some(Inbound::Closed) | None => return Err(LinkError::Closed),
                    Some(_) => return Err(LinkError::Frame),
                }
            }
        })
        .await
        .unwrap_or(Err(LinkError::HelloTimeout))?;
        check_pin(&hello, &PIN)?;
        self.create_client().await
    }

    /// `td_create_client_id()` in the helper.
    pub(crate) async fn create_client(&self) -> Result<i32, LinkError> {
        let _permit = self
            .shared
            .control
            .acquire()
            .await
            .map_err(|_| LinkError::Closed)?;
        let seq = self.shared.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.register(|p| p.creates.insert(seq, tx))?;
        self.write(&ToHelper::Create { seq }, |p| {
            p.creates.remove(&seq);
        })?;
        self.wait(rx, |p| {
            p.creates.remove(&seq);
        })
        .await
    }

    /// One TDLib request of `client`: its `@extra` is set here; the answer is TDLib's object, or a
    /// typed error (a TDLib error keeps only its code).
    pub(crate) async fn request(&self, client: i32, mut req: Value) -> Result<Value, LinkError> {
        let extra = self.shared.next.fetch_add(1, Ordering::SeqCst);
        if extra >= HELPER_EXTRA_MIN {
            return Err(LinkError::Closed);
        }
        let Some(obj) = req.as_object_mut() else {
            return Err(LinkError::Frame);
        };
        obj.insert("@extra".to_owned(), Value::from(extra));
        let (tx, rx) = oneshot::channel();
        self.register(|p| p.sends.insert(extra, tx))?;
        let json = req.to_string();
        drop(req);
        self.write(&ToHelper::Send { client, json }, |p| {
            p.sends.remove(&extra);
        })?;
        self.wait(rx, |p| {
            p.sends.remove(&extra);
        })
        .await
    }

    /// One TDLib request whose JSON holds a secret: TDLib's database key (the parameters), a typed
    /// code, password or e-mail address (TG-1b Q-C3-1, TG-2 M10). The caller builds `json` as text
    /// ([`secret_json`]); the `@extra` is inserted here by TEXT (no `Value` ever holds the secret),
    /// the frame is built without `format!` in a buffer reserved at its full size
    /// ([`secret_frame`]), and every copy the APP makes (the text, the frame, the writer's buffer) is
    /// zeroed when dropped.
    ///
    /// What stays plain, said plainly: the helper's line buffer and its `serde_json` probe of the
    /// line (`kourier_tdlib::serve`), and TDLib's own copies (it keeps the password for SRP, the key
    /// for its database). This path covers the app's side only.
    ///
    /// `json` must be ONE JSON object without an `@extra` of its own and with nothing after its last
    /// `}`; anything else (no object, a raw newline, a frame over [`MAX_LINE`]) fails only this
    /// call, typed `Frame`, before anything is written. The answer is the same as [`Self::request`]'s.
    pub(crate) async fn request_secret(
        &self,
        client: i32,
        json: Zeroizing<String>,
    ) -> Result<Value, LinkError> {
        let extra = self.shared.next.fetch_add(1, Ordering::SeqCst);
        if extra >= HELPER_EXTRA_MIN {
            return Err(LinkError::Closed);
        }
        let frame = secret_frame(client, json.as_str(), extra)?;
        drop(json);
        let (tx, rx) = oneshot::channel();
        self.register(|p| p.sends.insert(extra, tx))?;
        // A failed send hands the frame back inside the error, which zeroes it as it drops.
        if self.out.send(frame).is_err() {
            self.shared.lock().sends.remove(&extra);
            return Err(LinkError::Closed);
        }
        self.wait(rx, |p| {
            p.sends.remove(&extra);
        })
        .await
    }

    /// One synchronous TDLib request (`td_execute` in the helper).
    // Used by the link's tests (and, for `execute`, by the watchdog's probe next).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn execute(&self, req: &Value) -> Result<Value, LinkError> {
        let _permit = self
            .shared
            .control
            .acquire()
            .await
            .map_err(|_| LinkError::Closed)?;
        let seq = self.shared.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.register(|p| p.executes.insert(seq, tx))?;
        self.write(
            &ToHelper::Execute {
                seq,
                json: req.to_string(),
            },
            |p| {
                p.executes.remove(&seq);
            },
        )?;
        let answer = self
            .wait(rx, |p| {
                p.executes.remove(&seq);
            })
            .await?;
        let value: Value = serde_json::from_str(&answer).map_err(|_| LinkError::Frame)?;
        td_result(value)
    }

    /// The SOCKS5 gate of `client`, once per client (the helper takes a second one for a bad frame
    /// and ends): the helper writes `addProxy` for it right before the client's parameters and
    /// checks TDLib's answer (fix F2). A port outside 1..=65535 or a credential outside 1..=127
    /// bytes is refused here, as `Frame`, before anything is written.
    pub(crate) fn gate(
        &self,
        client: i32,
        port: u16,
        user: String,
        pass: String,
    ) -> Result<(), LinkError> {
        self.write(
            &ToHelper::Gate {
                client,
                port,
                user,
                pass,
            },
            |_| {},
        )
    }

    /// Asks the helper to close every client and end.
    pub(crate) fn quit(&self) {
        let _ = self.write(&ToHelper::Quit, |_| {});
    }

    /// How many answers arrived that no request waited for.
    // Used by the link's tests (and, for `execute`, by the watchdog's probe next).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn strays(&self) -> u64 {
        self.shared.strays.load(Ordering::SeqCst)
    }

    fn register<T>(&self, put: impl FnOnce(&mut Pending) -> T) -> Result<(), LinkError> {
        let mut p = self.shared.lock();
        if p.closed {
            return Err(LinkError::Closed);
        }
        put(&mut p);
        Ok(())
    }

    /// Encodes `frame` and queues the bytes. A frame that cannot be encoded (a payload over the
    /// line limit or with a newline, a gate the protocol refuses) fails only this call, as `Frame`:
    /// the link and every other request go on (fix F14).
    fn write(&self, frame: &ToHelper, forget: impl FnOnce(&mut Pending)) -> Result<(), LinkError> {
        let mut bytes = Zeroizing::new(Vec::new());
        if frame.encode(&mut bytes).is_err() {
            forget(&mut self.shared.lock());
            return Err(LinkError::Frame);
        }
        if self.out.send(bytes).is_err() {
            forget(&mut self.shared.lock());
            return Err(LinkError::Closed);
        }
        Ok(())
    }

    async fn wait<T>(
        &self,
        rx: oneshot::Receiver<Result<T, LinkError>>,
        forget: impl FnOnce(&mut Pending),
    ) -> Result<T, LinkError> {
        match tokio::time::timeout(self.shared.opts.request_timeout, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err(LinkError::Closed),
            Err(_) => {
                // Its late answer is a stray from now on.
                forget(&mut self.shared.lock());
                Err(LinkError::Timeout)
            }
        }
    }
}

/// A helper is the pinned one when it speaks this protocol AND its library is the pinned commit
/// and version (the version alone would accept a rebuilt library of another commit) AND carries
/// the pinned Kourier patch tag (pre3 K5: a stock, 0001-only or pre-0002 library has the pinned
/// commit too, but answers no `kourier_patch_level` or an older one; a missing tag is the empty
/// string and never equals the pin's).
pub(crate) fn check_pin(hello: &Hello, pin: &Pin) -> Result<(), LinkError> {
    if hello.protocol == PROTOCOL
        && hello.commit == pin.commit
        && hello.tdlib == pin.version
        && hello.patch_level == pin.patch_level
    {
        Ok(())
    } else {
        Err(LinkError::VersionMismatch)
    }
}

/// `S <client> <json with its @extra>\n` for [`HelperLink::request_secret`], built byte by byte into a
/// zeroizing buffer reserved at the frame's full size: no `format!`, no growth (a reallocation would
/// leave a copy of the secret behind; TG-2 M10). The `@extra` goes in by TEXT before the object's
/// last `}` (with no comma when the object is empty). `Frame` for a text that is not one object
/// ending in `}` (nothing after it), that holds a raw newline, or a line over [`MAX_LINE`]; the
/// object's inside is not parsed (a parser's scratch buffers would keep copies), so its shape is
/// the caller's: [`secret_json`] of a `#[derive(Serialize)]` struct.
pub(crate) fn secret_frame(
    client: i32,
    json: &str,
    extra: u64,
) -> Result<Zeroizing<Vec<u8>>, LinkError> {
    let bytes = json.as_bytes();
    let Some((&b'}', body)) = bytes.split_last() else {
        return Err(LinkError::Frame);
    };
    if body.first() != Some(&b'{') || bytes.iter().any(|&b| b == b'\n' || b == b'\r') {
        return Err(LinkError::Frame);
    }
    // `body` is the text without its last `}`: `{` and whatever the object holds.
    let empty = body[1..].iter().all(u8::is_ascii_whitespace);
    let sep: &[u8] = if empty {
        b"\"@extra\":"
    } else {
        b",\"@extra\":"
    };
    let client_text = client.to_string();
    let extra_text = extra.to_string();
    // "S " + client + " " + body + sep + extra + "}", then the "\n".
    let line = 2 + client_text.len() + 1 + body.len() + sep.len() + extra_text.len() + 1;
    if line > MAX_LINE {
        return Err(LinkError::Frame);
    }
    let mut frame = Zeroizing::new(Vec::with_capacity(line + 1));
    frame.extend_from_slice(b"S ");
    frame.extend_from_slice(client_text.as_bytes());
    frame.push(b' ');
    frame.extend_from_slice(body);
    frame.extend_from_slice(sep);
    frame.extend_from_slice(extra_text.as_bytes());
    frame.extend_from_slice(b"}\n");
    Ok(frame)
}

/// `value` as JSON text in a zeroizing buffer of exactly its size (TG-2 M10): the text is measured
/// first by a writer that keeps nothing, then written into a buffer reserved at that size by a
/// writer that refuses to grow it, so no reallocation leaves a copy behind; JSON escaping (a `"`,
/// a `\`, a newline, non-ASCII) is serde_json's. `value` is a BORROWED `#[derive(Serialize)]`
/// struct: no `serde_json::Value` (whose plain `String`s would keep the secret) is ever built. A
/// failed serialization (none happens for structs of strings, numbers and booleans) comes back as
/// an EMPTY text, which [`HelperLink::request_secret`] refuses as `Frame`; the partial text is
/// zeroed.
pub(crate) fn secret_json<T: serde::Serialize>(value: &T) -> Zeroizing<String> {
    let mut count = Count(0);
    if serde_json::to_writer(&mut count, value).is_err() {
        return Zeroizing::new(String::new());
    }
    let mut buf: Zeroizing<Vec<u8>> = Zeroizing::new(Vec::with_capacity(count.0));
    if serde_json::to_writer(Bounded(&mut buf), value).is_err() {
        buf.zeroize();
        return Zeroizing::new(String::new());
    }
    // Moved out, not copied: the buffer left behind is empty.
    let bytes: Vec<u8> = std::mem::take(&mut *buf);
    match String::from_utf8(bytes) {
        Ok(text) => Zeroizing::new(text),
        Err(e) => {
            // serde_json writes UTF-8 only; zeroed all the same.
            let mut bytes = e.into_bytes();
            bytes.zeroize();
            Zeroizing::new(String::new())
        }
    }
}

/// A writer that only counts what it is given.
struct Count(usize);

impl Write for Count {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0 += data.len();
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A writer into a `Vec` that never grows it past its capacity (it fails instead).
struct Bounded<'a>(&'a mut Vec<u8>);

impl Write for Bounded<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.0.len() + data.len() > self.0.capacity() {
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        }
        self.0.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Awaits `answer` while handing every inbound frame to `on_inbound`: the one way the actor waits
/// for a helper's answer, so the inbound queue never stops the answer behind it (D3 H3).
pub(crate) async fn drive<T>(
    inbound: &mut mpsc::Receiver<Inbound>,
    answer: impl Future<Output = T>,
    mut on_inbound: impl FnMut(Inbound),
) -> T {
    tokio::pin!(answer);
    loop {
        tokio::select! {
            biased;
            out = &mut answer => return out,
            Some(frame) = inbound.recv() => on_inbound(frame),
        }
    }
}

/// TDLib's text when its database is locked by another process (`FileFd.cpp:421, 503-507`): both
/// parts are needed. The same prefix also starts the error of any other failed `fcntl` lock (no
/// lock service on the file system), which is permanent and has no "already in use" (G12).
const LOCKED: &str = "Can't lock file";
const IN_USE: &str = "already in use";

/// TDLib's answer as a result: an `error` object keeps only its code, the fixed class
/// [`LinkError::DatabaseLocked`] (L8) or a [`TdClass`] (TG-2, [`classify`]); its message is looked
/// at for that and dropped.
fn td_result(value: Value) -> Result<Value, LinkError> {
    if value.get("@type").and_then(Value::as_str) == Some("error") {
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if message.contains(LOCKED) && message.contains(IN_USE) {
            return Err(LinkError::DatabaseLocked);
        }
        let code = value
            .get("code")
            .and_then(Value::as_i64)
            .and_then(|c| i32::try_from(c).ok())
            .unwrap_or(0);
        return Err(match classify(code, message) {
            Some((class, retry_after_s)) => LinkError::Auth {
                code,
                class,
                retry_after_s,
            },
            None => LinkError::Td { code },
        });
    }
    Ok(value)
}

/// TDLib's 429 text (`NetQueryDelayer.cpp:105`, read back by `Global::get_retry_after`).
const RETRY_AFTER: &str = "Too Many Requests: retry after ";

/// The class of TDLib's error `message` (TG-2 brief §2.8), and a 429's wait in seconds; `None` for
/// a message of no known class. Exact equality only, except the two prefix forms TDLib writes
/// itself; the message is never kept.
pub(crate) fn classify(code: i32, message: &str) -> Option<(TdClass, Option<u32>)> {
    if code == 429 {
        // TDLib's own parser takes only a positive number after the prefix (`Global.cpp:173-189`).
        let wait = message
            .strip_prefix(RETRY_AFTER)
            .and_then(|n| n.parse::<u32>().ok())
            .filter(|&n| n > 0);
        return Some((TdClass::TooManyRequests, wait));
    }
    let class = match message {
        "PHONE_NUMBER_INVALID" => TdClass::PhoneNumberInvalid,
        "PHONE_NUMBER_BANNED" => TdClass::PhoneNumberBanned,
        "PHONE_NUMBER_FLOOD" => TdClass::PhoneNumberFlood,
        "PHONE_CODE_INVALID" => TdClass::PhoneCodeInvalid,
        "PHONE_CODE_EXPIRED" => TdClass::PhoneCodeExpired,
        "PHONE_CODE_EMPTY" => TdClass::PhoneCodeEmpty,
        "PASSWORD_HASH_INVALID" => TdClass::PasswordHashInvalid,
        "EMAIL_INVALID" => TdClass::EmailInvalid,
        "EMAIL_CODE_INVALID" => TdClass::EmailCodeInvalid,
        "EMAIL_NOT_ALLOWED" => TdClass::EmailNotAllowed,
        "CODE_INVALID" => TdClass::CodeInvalid,
        "FROZEN_METHOD_INVALID" => TdClass::FrozenMethodInvalid,
        "Invalid recovery code" => TdClass::InvalidRecoveryCode,
        "Application update is needed to log in" => TdClass::UpdateNeeded,
        "Another authorization query has started" => TdClass::AnotherQuery,
        m if m.starts_with("Call to ") && m.ends_with(" unexpected") => TdClass::Unexpected,
        _ => return None,
    };
    Some((class, None))
}

/// The `@extra` of an answer, if it has one (only the top level is looked at).
fn extra_of(json: &str) -> Option<u64> {
    #[derive(serde::Deserialize)]
    struct Probe {
        #[serde(rename = "@extra")]
        extra: Option<u64>,
    }
    serde_json::from_str::<Probe>(json).ok()?.extra
}

/// The typed form of TDLib's answer to the helper's gate `addProxy`; `None` when it names no client
/// or is no JSON object. Reads the type, `is_enabled`, `proxy.server` and `proxy.port` and nothing
/// else (the answer's text, an error's message, is dropped here).
fn gate_answer(json: &str) -> Option<Inbound> {
    let v: Value = serde_json::from_str(json).ok()?;
    let client = i32::try_from(v.get("@client_id")?.as_i64()?).ok()?;
    let proxy = v.get("proxy");
    let ok = v.get("@type").and_then(Value::as_str) == Some("addedProxy")
        && v.get("is_enabled").and_then(Value::as_bool) == Some(true)
        && proxy.and_then(|p| p.get("server")).and_then(Value::as_str) == Some("127.0.0.1");
    let port = proxy
        .and_then(|p| p.get("port"))
        .and_then(Value::as_i64)
        .and_then(|p| u16::try_from(p).ok());
    Some(Inbound::GateProxy { client, ok, port })
}

/// Routes one frame: an answer to its request (or the stray count), anything else to the actor.
fn route(shared: &Shared, frame: FromHelper) -> Option<Inbound> {
    match frame {
        FromHelper::Hello(h) => Some(Inbound::Hello(h)),
        FromHelper::Heartbeat => Some(Inbound::Heartbeat),
        FromHelper::Bye(b) => Some(Inbound::Bye(b)),
        FromHelper::Created { seq, client } => {
            let waiting = shared.lock().creates.remove(&seq);
            match waiting {
                Some(tx) => drop(tx.send(Ok(client))),
                None => shared.stray(),
            }
            None
        }
        FromHelper::Executed { seq, json } => {
            let waiting = shared.lock().executes.remove(&seq);
            match waiting {
                Some(tx) => drop(tx.send(Ok(json))),
                None => shared.stray(),
            }
            None
        }
        FromHelper::Update(json) => {
            let Some(extra) = extra_of(&json) else {
                return Some(Inbound::Update(json));
            };
            if extra >= HELPER_EXTRA_MIN {
                // The helper's own range (fix F2): only the gate's `addedProxy` is passed on, as
                // its own typed frame; any other answer of the helper's requests never reaches the
                // app, so one that does is a protocol fault.
                return Some(
                    (extra == GATE_PROXY_EXTRA)
                        .then(|| gate_answer(&json))
                        .flatten()
                        .unwrap_or(Inbound::Garbled),
                );
            }
            let waiting = shared.lock().sends.remove(&extra);
            match waiting {
                Some(tx) => {
                    let answer = serde_json::from_str::<Value>(&json)
                        .map_err(|_| LinkError::Frame)
                        .and_then(td_result);
                    drop(tx.send(answer));
                }
                None => shared.stray(),
            }
            None
        }
        FromHelper::Failure {
            code,
            extra: Some(extra),
        } => {
            let waiting = shared.lock().sends.remove(&extra);
            match waiting {
                Some(tx) => drop(tx.send(Err(LinkError::Refused(code)))),
                None => shared.stray(),
            }
            None
        }
        FromHelper::Failure { code, extra: None } if code.is_fatal() => Some(Inbound::Fatal(code)),
        FromHelper::Failure { code, extra: None } => Some(Inbound::Refused(code)),
    }
}

fn reader(sock: UnixStream, shared: &Shared, inbound: &mpsc::Sender<Inbound>) {
    let mut lines = LineReader::new(sock);
    let last = loop {
        let line = match lines.next_line() {
            Ok(Some(line)) => line,
            // The helper's end is gone: cleanly, inside a line, or by an I/O error.
            Ok(None) | Err(FrameError::Truncated | FrameError::Io(_)) => break Inbound::Closed,
            // A line over the limit (or not UTF-8): the helper broke the protocol, it did not
            // just end (fix F14).
            Err(_) => break Inbound::Garbled,
        };
        let Ok(frame) = decode_from_helper(&line) else {
            break Inbound::Garbled;
        };
        if let Some(frame) = route(shared, frame) {
            if matches!(frame, Inbound::Garbled) {
                break frame;
            }
            // An actor that dropped its receiver still gets its answers routed.
            let _ = inbound.blocking_send(frame);
        }
    };
    // A fixed class and a count: never the line that ended the stream.
    tracing::debug!(
        garbled = matches!(last, Inbound::Garbled),
        strays = shared.strays.load(Ordering::SeqCst),
        "the TDLib helper link closed"
    );
    {
        let mut p = shared.lock();
        p.closed = true;
        // Dropping the senders answers every waiting request with `Closed`.
        p.sends.clear();
        p.executes.clear();
        p.creates.clear();
    }
    let _ = inbound.blocking_send(last);
}

/// The writer's buffer: the frames of a burst are gathered into it and written with one call.
const WRITE_BUFFER: usize = 64 * 1024;

/// Writes the frames in order. Its buffer is zeroed after every write (`zeroize`, not `clear`: a
/// cleared `Vec` keeps its bytes) and never grows: a frame larger than the room left is written
/// after the buffer, from its own zeroizing buffer (TG-2 M10). Every frame is zeroed as it drops.
fn writer(mut sock: UnixStream, frames: &std::sync::mpsc::Receiver<Zeroizing<Vec<u8>>>) {
    let Ok(end) = sock.try_clone() else {
        return;
    };
    let mut buf: Zeroizing<Vec<u8>> = Zeroizing::new(Vec::with_capacity(WRITE_BUFFER));
    'frames: while let Ok(first) = frames.recv() {
        let mut next = Some(first);
        while let Some(frame) = next.take() {
            if buf.len() + frame.len() > buf.capacity() {
                if write_out(&mut sock, &mut buf).is_err() {
                    break 'frames;
                }
                if frame.len() > buf.capacity() {
                    if sock.write_all(frame.as_slice()).is_err() {
                        break 'frames;
                    }
                    next = frames.try_recv().ok();
                    continue;
                }
            }
            buf.extend_from_slice(frame.as_slice());
            next = frames.try_recv().ok();
        }
        if write_out(&mut sock, &mut buf).is_err() {
            break;
        }
    }
    buf.zeroize();
    // The helper reads the end of its input: it closes every client and says bye.
    let _ = end.shutdown(Shutdown::Write);
}

/// Writes what `buf` holds, then zeroes it (whether the write worked or not); its capacity stays.
fn write_out(sock: &mut UnixStream, buf: &mut Zeroizing<Vec<u8>>) -> io::Result<()> {
    let written = if buf.is_empty() {
        Ok(())
    } else {
        sock.write_all(buf.as_slice())
    };
    buf.zeroize();
    written
}

#[cfg(test)]
mod tests;

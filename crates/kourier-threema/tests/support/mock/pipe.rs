//! THE PARENT-RESIDENT FAKE (TH5a A3, decisions C7 and M10; scout-mock §8.3, brief §2.12): the fake
//! (mediator and chat server, [`Net`]) stays in the PARENT test process, the actor runs in an
//! `unshare -rn` CHILD, and the two talk over two INHERITED ANONYMOUS PIPES, so a kill test can kill
//! the child at a failpoint while the fake keeps its state (slots, queues, un-acked reflections), and
//! the next child reconnects to the same fake. Pipes are no sockets (rule 1 and rule 9 hold: no
//! socket name here) and work in the empty network namespace.
//!
//! THE FDS: the parent makes two `std::io::pipe`s (close-on-exec), moves the CHILD's two ends to
//! numbers above 9 (`F_DUPFD_CLOEXEC`, so the `dup2` below can never overwrite one with the other),
//! and the child's `pre_exec` `dup2`s them to [`CHILD_READS`] (3) and [`CHILD_WRITES`] (4), which
//! clears close-on-exec on those two copies only. After the spawn the parent CLOSES its copies of the
//! child's ends ([`ChildEnds`] dropped): only then does the child's death give the parent an EOF.
//! Every other pipe of either process is close-on-exec, so nothing else reaches the child.
//!
//! THE FRAMING: one frame = `u32` little-endian length of what follows, a tag byte, the body. A
//! WebSocket message is one frame. Child -> parent: `CONNECT id kind url`, `SEND id bytes`,
//! `CLOSE id code reason`, `HTTPS id request` (TH6 L2b: the whole request, [`encode_request`]). Parent -> child:
//! `SETUP` (the servers' parameters and the fake's clock, first), `CONNECTED id`, `FAILED id error`, `BINARY id
//! bytes`, `CLOSED id code reason` (the stream of that connection ends after it), `HTTPS_FAILED id error` and
//! `HTTPS_OK id response` (TH6 L2b: the fake's HTTPS client answered, [`encode_response`]; the directory).
//!
//! THE PARENT'S LOOP ([`serve`]): a thread of its own with a current-thread runtime (I/O only, no
//! timer) reads the child's frames and drives the world's in-process connector ([`Net::open`]),
//! exactly what an actor in the parent would; one task per connection moves its events to the
//! child. At the EOF (the child died or ended) every mediator connection the child had open is
//! LOST: the fake ends it with 1006 ([`Net::lose`]), as for a dead TCP connection, and keeps
//! everything else. One child at a time per [`serve`]; a second child gets a new [`serve`] on the
//! SAME [`Net`].
//!
//! THE CHILD'S SIDE ([`open_child`], [`PipedConnector`]): reads and writes go through
//! `tokio::io::unix::AsyncFd` on non-blocking fds (neither process ever blocks a thread on the
//! other, so the two directions cannot deadlock); one reader task hands each frame to its
//! connection. Limits: a connect the actor gave up on (its deadline) may still open a connection in
//! the parent (the in-process connector drops it); a relay side of a dead child is not lost (TH5's
//! kill tests use the mediator only); a stalled relay send holds the parent's reader.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::process::CommandExt as _;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use kourier_threema::{
    HttpsClient, HttpsMethod, HttpsRequest, HttpsResponse, NetError, ServerParams, ThreemaParts,
    UntrustedKind, WsConnector, WsEvent, WsKind, WsRequest, WsSender,
};
use tokio::io::unix::AsyncFd;
use tokio::sync::oneshot;

use super::connector::Net;
use super::d2m::ConnId;
use super::lock;

/// The fd the child reads the parent's frames from.
pub const CHILD_READS: RawFd = 3;
/// The fd the child writes its frames to.
pub const CHILD_WRITES: RawFd = 4;
/// The largest frame either side takes (a rendezvous message is up to 100 MiB + 4).
const MAX_FRAME: usize = 128 << 20;

mod tag {
    pub const CONNECT: u8 = 1;
    pub const SEND: u8 = 2;
    pub const CLOSE: u8 = 3;
    pub const HTTPS: u8 = 4;
    pub const SETUP: u8 = 10;
    pub const CONNECTED: u8 = 11;
    pub const FAILED: u8 = 12;
    pub const BINARY: u8 = 13;
    pub const CLOSED: u8 = 14;
    pub const HTTPS_FAILED: u8 = 15;
    pub const HTTPS_OK: u8 = 16;
}

/// The operation names a [`NetError`] of the fake carries (they are `&'static str`).
const OPS: [&str; 5] = [
    "websocket",
    "https",
    "websocket send",
    "websocket close",
    "pipe",
];

fn frame(tag: u8, parts: &[&[u8]]) -> Vec<u8> {
    let len: usize = 1 + parts.iter().map(|p| p.len()).sum::<usize>();
    let mut out = Vec::with_capacity(4 + len);
    out.extend_from_slice(&u32::try_from(len).expect("a frame fits u32").to_le_bytes());
    out.push(tag);
    for part in parts {
        out.extend_from_slice(part);
    }
    out
}

fn id_of(body: &[u8]) -> u32 {
    u32::from_le_bytes(body[..4].try_into().expect("a frame starts with its id"))
}

fn code_of(body: &[u8]) -> u16 {
    u16::from_le_bytes(body[4..6].try_into().expect("a close carries its code"))
}

fn encode_error(error: &NetError) -> [u8; 2] {
    let op = |name: &str| {
        u8::try_from(
            OPS.iter()
                .position(|o| *o == name)
                .expect("an operation name the pipe knows"),
        )
        .expect("few ops")
    };
    match error {
        NetError::Refused(name) => [0, op(name)],
        NetError::Failed(name) => [1, op(name)],
        NetError::Untrusted(kind) => [
            2,
            match kind {
                UntrustedKind::NotTls => 0,
                UntrustedKind::UnpinnedHost => 1,
                UntrustedKind::PinMismatch => 2,
                UntrustedKind::UnknownIssuer => 3,
                UntrustedKind::BadName => 4,
                UntrustedKind::BadCertificate => 5,
            },
        ],
        NetError::CertificateDates => [3, 0],
    }
}

fn decode_error(bytes: &[u8]) -> NetError {
    let op = OPS[usize::from(bytes[1]).min(OPS.len() - 1)];
    match bytes[0] {
        0 => NetError::Refused(op),
        1 => NetError::Failed(op),
        2 => NetError::Untrusted(match bytes[1] {
            0 => UntrustedKind::NotTls,
            1 => UntrustedKind::UnpinnedHost,
            2 => UntrustedKind::PinMismatch,
            3 => UntrustedKind::UnknownIssuer,
            4 => UntrustedKind::BadName,
            _ => UntrustedKind::BadCertificate,
        }),
        _ => NetError::CertificateDates,
    }
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&u32::try_from(bytes.len()).expect("fits").to_le_bytes());
    out.extend_from_slice(bytes);
}

fn take_bytes<'a>(body: &mut &'a [u8]) -> &'a [u8] {
    let len = u32::from_le_bytes(body[..4].try_into().expect("a length")) as usize;
    let (bytes, rest) = body[4..].split_at(len);
    *body = rest;
    bytes
}

/// TH6 L2b: an HTTPS request as the child writes it: method (0 GET, 1 POST, 2 PUT), the URL, the headers (count,
/// then name and value each), the body, `max_body` (a flag byte, then a `u64`).
fn encode_request(request: &HttpsRequest) -> Vec<u8> {
    let mut out = vec![match request.method {
        HttpsMethod::Get => 0,
        HttpsMethod::Post => 1,
        HttpsMethod::Put => 2,
    }];
    put_bytes(&mut out, request.url.as_bytes());
    put_headers(&mut out, &request.headers);
    put_bytes(&mut out, &request.body);
    match request.max_body {
        None => out.push(0),
        Some(most) => {
            out.push(1);
            out.extend_from_slice(&u64::try_from(most).expect("fits u64").to_le_bytes());
        }
    }
    out
}

fn decode_request(mut body: &[u8]) -> HttpsRequest {
    let method = match body[0] {
        0 => HttpsMethod::Get,
        1 => HttpsMethod::Post,
        _ => HttpsMethod::Put,
    };
    body = &body[1..];
    let url = String::from_utf8_lossy(take_bytes(&mut body)).into_owned();
    let headers = take_headers(&mut body);
    let request_body = take_bytes(&mut body).to_vec();
    let max_body = (body[0] == 1).then(|| {
        usize::try_from(u64::from_le_bytes(body[1..9].try_into().expect("a u64"))).expect("fits")
    });
    HttpsRequest {
        method,
        url,
        headers,
        body: request_body,
        max_body,
    }
}

/// TH6 L2b: an HTTPS response as the parent writes it: the status, the headers, the body.
fn encode_response(response: &HttpsResponse) -> Vec<u8> {
    let mut out = response.status.to_le_bytes().to_vec();
    put_headers(&mut out, &response.headers);
    put_bytes(&mut out, &response.body);
    out
}

fn decode_response(mut body: &[u8]) -> HttpsResponse {
    let status = u16::from_le_bytes(body[..2].try_into().expect("a status"));
    body = &body[2..];
    let headers = take_headers(&mut body);
    HttpsResponse {
        status,
        headers,
        body: take_bytes(&mut body).to_vec(),
    }
}

fn put_headers(out: &mut Vec<u8>, headers: &[(String, String)]) {
    out.extend_from_slice(&u32::try_from(headers.len()).expect("fits").to_le_bytes());
    for (name, value) in headers {
        put_bytes(out, name.as_bytes());
        put_bytes(out, value.as_bytes());
    }
}

fn take_headers(body: &mut &[u8]) -> Vec<(String, String)> {
    let count = u32::from_le_bytes(body[..4].try_into().expect("a count"));
    *body = &body[4..];
    (0..count)
        .map(|_| {
            let name = String::from_utf8_lossy(take_bytes(body)).into_owned();
            let value = String::from_utf8_lossy(take_bytes(body)).into_owned();
            (name, value)
        })
        .collect()
}

/// What the parent sends first: the servers' parameters the child's parts carry, and the fake's
/// clock (the child's actor clock starts there).
pub struct Setup {
    pub params: ServerParams,
    pub now_ms: u64,
}

impl std::fmt::Debug for Setup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Setup")
    }
}

fn encode_setup(setup: &Setup) -> Vec<u8> {
    let p = &setup.params;
    let mut body = Vec::new();
    body.extend_from_slice(&setup.now_ms.to_le_bytes());
    for url in [
        &p.mediator_url,
        &p.rendezvous_url,
        &p.directory_url,
        &p.blob_url,
    ] {
        put_bytes(&mut body, url.as_bytes());
    }
    put_bytes(&mut body, &p.chat_server_keys.concat());
    frame(tag::SETUP, &[&body])
}

fn decode_setup(body: &[u8]) -> Setup {
    let now_ms = u64::from_le_bytes(body[..8].try_into().expect("the clock"));
    let mut rest = &body[8..];
    let mut url = || String::from_utf8(take_bytes(&mut rest).to_vec()).expect("a URL");
    let (mediator_url, rendezvous_url, directory_url, blob_url) = (url(), url(), url(), url());
    let keys = take_bytes(&mut rest);
    let chat_server_keys = keys.as_chunks::<32>().0.to_vec();
    Setup {
        params: ServerParams {
            mediator_url,
            rendezvous_url,
            directory_url,
            blob_url,
            chat_server_keys,
        },
        now_ms,
    }
}

/// Sets `O_NONBLOCK` (and close-on-exec) on `fd`: `AsyncFd` needs a non-blocking descriptor.
fn non_blocking(fd: &OwnedFd) {
    let raw = fd.as_raw_fd();
    // SAFETY: fcntl on a descriptor this value owns; no memory is passed.
    let ok = unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFL);
        flags >= 0
            && libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) == 0
            && libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) == 0
    };
    assert!(ok, "a pipe end could not be made non-blocking");
}

/// A pipe end registered with the reactor.
fn registered<T: std::os::fd::AsRawFd>(end: T) -> AsyncFd<T> {
    // SAFETY: `end` is a `PipeReader` or `PipeWriter` made from an `OwnedFd`: it owns its
    // descriptor, which stays open and the same while the `AsyncFd` holds it.
    unsafe { AsyncFd::register(end) }.expect("register the pipe")
}

/// The reading half of a pipe: whole frames.
struct Reader {
    fd: AsyncFd<std::io::PipeReader>,
    buf: Vec<u8>,
}

impl Reader {
    fn new(fd: OwnedFd) -> Self {
        non_blocking(&fd);
        Self {
            fd: registered(std::io::PipeReader::from(fd)),
            buf: Vec::new(),
        }
    }

    /// The next frame (tag, body), `None` at the EOF (the other process closed its end or died).
    async fn frame(&mut self) -> Option<(u8, Vec<u8>)> {
        loop {
            if self.buf.len() >= 4 {
                let len = u32::from_le_bytes(self.buf[..4].try_into().expect("4 bytes")) as usize;
                assert!(
                    (1..=MAX_FRAME).contains(&len),
                    "a pipe frame of a length the framing never writes"
                );
                if self.buf.len() >= 4 + len {
                    let rest = self.buf.split_off(4 + len);
                    let whole = std::mem::replace(&mut self.buf, rest);
                    return Some((whole[4], whole[5..].to_vec()));
                }
            }
            let mut guard = self.fd.readable().await.ok()?;
            let mut chunk = vec![0u8; 64 << 10];
            match guard.try_io(|inner| inner.get_ref().read(&mut chunk)) {
                Ok(Ok(0)) => return None,
                Ok(Ok(n)) => self.buf.extend_from_slice(&chunk[..n]),
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Ok(Err(_)) => return None,
                Err(_would_block) => {}
            }
        }
    }
}

/// A frame queued for the writer, and whom to tell once it is in the pipe.
type Queued = (Vec<u8>, Option<oneshot::Sender<()>>);

/// The writing half of a pipe, behind ONE task (frames never interleave): what is queued on the
/// returned sender is written in order, and its `oneshot` (if any) is told once the frame is IN the
/// pipe (a child that exits right after a close must not take the close frame with it: the parent
/// reads what is in the pipe before the EOF); the task ends when the other side is gone.
fn spawn_writer(fd: OwnedFd) -> async_channel::Sender<Queued> {
    non_blocking(&fd);
    let fd = registered(std::io::PipeWriter::from(fd));
    let (tx, rx) = async_channel::unbounded::<Queued>();
    tokio::spawn(async move {
        while let Ok((bytes, written)) = rx.recv().await {
            let mut at = 0;
            while at < bytes.len() {
                let Ok(mut guard) = fd.writable().await else {
                    return;
                };
                match guard.try_io(|inner| inner.get_ref().write(&bytes[at..])) {
                    Ok(Ok(n)) => at += n,
                    Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    // EPIPE: the reader is gone (SIGPIPE is ignored in Rust programs).
                    Ok(Err(_)) => return,
                    Err(_would_block) => {}
                }
            }
            if let Some(written) = written {
                let _ = written.send(());
            }
        }
    });
    tx
}

// ---------------------------------------------------------------------------------------------
// The parent.

/// The CHILD's two pipe ends, held by the parent from [`serve`] until the spawn: [`ChildEnds::install`]
/// sets the `dup2` of the child's `pre_exec`, and DROPPING this value after the spawn closes the
/// parent's copies (else the child's death never shows as an EOF).
pub struct ChildEnds {
    reads: OwnedFd,
    writes: OwnedFd,
}

impl std::fmt::Debug for ChildEnds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChildEnds")
    }
}

/// A copy of `fd` at a number above 9 (close-on-exec); the original is closed.
fn above_nine(fd: OwnedFd) -> OwnedFd {
    // SAFETY: F_DUPFD_CLOEXEC on a descriptor `fd` owns; the new descriptor is owned below.
    let copy = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    assert!(copy >= 10, "a pipe end could not be moved above fd 9");
    // SAFETY: `copy` is a fresh descriptor nothing else owns.
    unsafe { OwnedFd::from_raw_fd(copy) }
}

impl ChildEnds {
    /// The child of `command` finds its read end at [`CHILD_READS`] and its write end at
    /// [`CHILD_WRITES`]: `dup2` in `pre_exec` (after std's stdio set-up), from numbers above 9.
    pub fn install(&self, command: &mut std::process::Command) {
        let (reads, writes) = (self.reads.as_raw_fd(), self.writes.as_raw_fd());
        assert!(reads > 9 && writes > 9, "the child's ends lie above fd 9");
        // SAFETY: the closure runs between fork and exec and calls only dup2 (async-signal-safe).
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(reads, CHILD_READS) < 0 || libc::dup2(writes, CHILD_WRITES) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

/// The parent's loop for ONE child (see the module doc): its end comes as one message.
pub struct Bridge {
    ended: std::sync::mpsc::Receiver<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Bridge")
    }
}

impl Bridge {
    /// Waits (a wall BOUND, never a measure) until the loop saw the child's EOF and lost its
    /// connections; whether it did within `within`.
    pub fn wait_end(&mut self, within: Duration) -> bool {
        if self.ended.recv_timeout(within).is_err() {
            return false;
        }
        if let Some(thread) = self.thread.take() {
            thread.join().expect("the bridge's thread joins");
        }
        true
    }
}

/// Starts the parent's loop for one child on `net` (the SAME fake for every child of a test): the
/// child's ends to [`ChildEnds::install`] and the loop. The loop sends [`Setup`] first.
pub fn serve(net: &Net, setup: &Setup) -> (ChildEnds, Bridge) {
    let (child_reads, parent_writes) = std::io::pipe().expect("a pipe");
    let (parent_reads, child_writes) = std::io::pipe().expect("a pipe");
    let ends = ChildEnds {
        reads: above_nine(child_reads.into()),
        writes: above_nine(child_writes.into()),
    };
    let (parent_reads, parent_writes): (OwnedFd, OwnedFd) =
        (parent_reads.into(), parent_writes.into());
    let (net, first) = (net.clone(), encode_setup(setup));
    let (done, ended) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .expect("the bridge's runtime");
        rt.block_on(parent_loop(net, parent_reads, parent_writes, first));
        drop(rt);
        let _ = done.send(());
    });
    (
        ends,
        Bridge {
            ended,
            thread: Some(thread),
        },
    )
}

type Senders = Arc<Mutex<BTreeMap<u32, Arc<dyn WsSender>>>>;

async fn parent_loop(net: Net, reads: OwnedFd, writes: OwnedFd, first: Vec<u8>) {
    let out = spawn_writer(writes);
    let _ = out.send((first, None)).await;
    let mut reader = Reader::new(reads);
    let senders: Senders = Arc::default();
    let conns: Arc<Mutex<Vec<ConnId>>> = Arc::default();
    while let Some((tag, body)) = reader.frame().await {
        let id = id_of(&body);
        match tag {
            tag::CONNECT => {
                let kind = if body[4] == 0 {
                    WsKind::Mediator
                } else {
                    WsKind::Rendezvous
                };
                let url = String::from_utf8(body[5..].to_vec()).expect("a URL");
                let (net, out, senders, conns) =
                    (net.clone(), out.clone(), senders.clone(), conns.clone());
                // A task: a held connect (`Net::hold_connects`) must not hold the other frames.
                tokio::spawn(async move {
                    match net.open(WsRequest { url, kind }).await {
                        Ok((sender, events, conn)) => {
                            if let Some(conn) = conn {
                                lock(&conns).push(conn);
                            }
                            lock(&senders).insert(id, Arc::from(sender));
                            let _ = out
                                .send((frame(tag::CONNECTED, &[&id.to_le_bytes()]), None))
                                .await;
                            while let Ok(event) = events.recv().await {
                                let bytes = match event {
                                    WsEvent::Binary(b) => {
                                        frame(tag::BINARY, &[&id.to_le_bytes(), &b])
                                    }
                                    WsEvent::Closed { code, reason } => frame(
                                        tag::CLOSED,
                                        &[
                                            &id.to_le_bytes(),
                                            &code.to_le_bytes(),
                                            reason.as_bytes(),
                                        ],
                                    ),
                                };
                                let _ = out.send((bytes, None)).await;
                            }
                        }
                        Err(error) => {
                            let _ = out
                                .send((
                                    frame(tag::FAILED, &[&id.to_le_bytes(), &encode_error(&error)]),
                                    None,
                                ))
                                .await;
                        }
                    }
                });
            }
            tag::SEND => {
                let sender = lock(&senders).get(&id).cloned();
                if let Some(sender) = sender {
                    let _ = sender.send(body[4..].to_vec()).await;
                }
            }
            tag::CLOSE => {
                let sender = lock(&senders).remove(&id);
                if let Some(sender) = sender {
                    let reason = String::from_utf8_lossy(&body[6..]).into_owned();
                    let _ = sender.close(code_of(&body), reason).await;
                }
            }
            tag::HTTPS => {
                // TH6 L2b: the child's whole request to the world's HTTPS client (the directory, or a refusal), in a
                // task: a held directory request (`Net::hold_directory`) must not hold the other frames.
                let request = decode_request(&body[4..]);
                let (net, out) = (net.clone(), out.clone());
                tokio::spawn(async move {
                    let answer = match net.https().execute(request).await {
                        Ok(response) => frame(
                            tag::HTTPS_OK,
                            &[&id.to_le_bytes(), &encode_response(&response)],
                        ),
                        Err(error) => frame(
                            tag::HTTPS_FAILED,
                            &[&id.to_le_bytes(), &encode_error(&error)],
                        ),
                    };
                    let _ = out.send((answer, None)).await;
                });
            }
            _ => panic!("a frame the child never writes"),
        }
    }
    // The EOF: the child is gone. Every mediator connection it had is lost (1006), the rest of the
    // fake's state stays.
    let lost = std::mem::take(&mut *lock(&conns));
    for conn in lost {
        net.lose(conn);
    }
}

// ---------------------------------------------------------------------------------------------
// The child.

/// The child's end of the pipes (made once per child by [`open_child`]).
pub struct ChildPipe {
    setup: Setup,
    inner: Arc<Shared>,
}

impl std::fmt::Debug for ChildPipe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChildPipe")
    }
}

#[derive(Default)]
struct Shared {
    out: Mutex<Option<async_channel::Sender<Queued>>>,
    next: AtomicU32,
    connects: Mutex<BTreeMap<u32, oneshot::Sender<Result<(), NetError>>>>,
    https: Mutex<BTreeMap<u32, oneshot::Sender<Result<HttpsResponse, NetError>>>>,
    streams: Mutex<BTreeMap<u32, async_channel::Sender<WsEvent>>>,
    open: Mutex<BTreeSet<u32>>,
}

impl Shared {
    async fn write(&self, bytes: Vec<u8>) -> Result<(), NetError> {
        let out = lock(&self.out).clone().ok_or(NetError::Failed("pipe"))?;
        let (written, wait) = oneshot::channel();
        out.send((bytes, Some(written)))
            .await
            .map_err(|_| NetError::Failed("pipe"))?;
        wait.await.map_err(|_| NetError::Failed("pipe"))
    }
}

/// Whether the descriptor `fd` of this process is a pipe.
fn is_pipe(fd: RawFd) -> bool {
    std::fs::read_link(format!("/proc/self/fd/{fd}"))
        .is_ok_and(|link| link.to_string_lossy().starts_with("pipe:"))
}

/// The descriptors of this process that are pipes, in order (for the child's check that nothing but
/// its stdio and [`CHILD_READS`] / [`CHILD_WRITES`] reached it).
pub fn pipe_fds() -> Vec<RawFd> {
    let mut fds: Vec<RawFd> = std::fs::read_dir("/proc/self/fd")
        .expect("/proc/self/fd")
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .filter(|fd| is_pipe(*fd))
        .collect();
    fds.sort_unstable();
    fds
}

/// IN THE CHILD, inside its runtime: takes fds 3 and 4 (they must be the parent's pipes), reads the
/// parent's [`Setup`] and starts the one reader task.
pub async fn open_child() -> ChildPipe {
    assert!(
        is_pipe(CHILD_READS) && is_pipe(CHILD_WRITES),
        "fds 3 and 4 are not the parent's pipes"
    );
    // SAFETY: fds 3 and 4 were dup2'ed into this process by the parent's pre_exec and nothing else
    // of this process owns them (they are taken once).
    let (reads, writes) = unsafe {
        (
            OwnedFd::from_raw_fd(CHILD_READS),
            OwnedFd::from_raw_fd(CHILD_WRITES),
        )
    };
    let mut reader = Reader::new(reads);
    let (tag, body) = reader.frame().await.expect("the parent's setup");
    assert_eq!(tag, tag::SETUP, "the parent's first frame is the setup");
    let setup = decode_setup(&body);
    let inner = Arc::new(Shared::default());
    *lock(&inner.out) = Some(spawn_writer(writes));
    let shared = inner.clone();
    tokio::spawn(async move {
        while let Some((tag, body)) = reader.frame().await {
            let id = id_of(&body);
            match tag {
                tag::CONNECTED => {
                    if let Some(tx) = lock(&shared.connects).remove(&id) {
                        let _ = tx.send(Ok(()));
                    }
                }
                tag::FAILED => {
                    if let Some(tx) = lock(&shared.connects).remove(&id) {
                        let _ = tx.send(Err(decode_error(&body[4..])));
                    }
                }
                tag::BINARY => {
                    let stream = lock(&shared.streams).get(&id).cloned();
                    if let Some(stream) = stream {
                        let _ = stream.try_send(WsEvent::Binary(body[4..].to_vec()));
                    }
                }
                tag::CLOSED => {
                    lock(&shared.open).remove(&id);
                    let stream = lock(&shared.streams).remove(&id);
                    if let Some(stream) = stream {
                        let _ = stream.try_send(WsEvent::Closed {
                            code: code_of(&body),
                            reason: String::from_utf8_lossy(&body[6..]).into_owned(),
                        });
                    }
                }
                tag::HTTPS_FAILED => {
                    if let Some(tx) = lock(&shared.https).remove(&id) {
                        let _ = tx.send(Err(decode_error(&body[4..])));
                    }
                }
                tag::HTTPS_OK => {
                    if let Some(tx) = lock(&shared.https).remove(&id) {
                        let _ = tx.send(Ok(decode_response(&body[4..])));
                    }
                }
                _ => panic!("a frame the parent never writes"),
            }
        }
        // The parent is gone: every stream ends, every pending call fails.
        lock(&shared.streams).clear();
        lock(&shared.open).clear();
        lock(&shared.connects).clear();
        lock(&shared.https).clear();
        lock(&shared.out).take();
    });
    ChildPipe { setup, inner }
}

impl ChildPipe {
    /// The parent's setup (the fake's clock at the spawn, the servers' parameters).
    pub fn setup(&self) -> &Setup {
        &self.setup
    }

    /// TH6 L2b: the child's HTTPS client over the pipes (what the parts' maker hands the actor), for a child that calls
    /// the parent's directory itself.
    pub fn https(&self) -> PipedHttps {
        PipedHttps(self.inner.clone())
    }

    /// TH6 L2b: ends the child's side: the writer stops, so the parent reads its EOF (as at the child's exit).
    pub fn finish(self) {
        lock(&self.inner.out).take();
    }
}

/// The parts of the child: the piped connector and HTTPS client, the parent's servers' parameters
/// (what `ThreemaNetwork::new` takes in the child, as `mock::pipe::parts(&pipe)`).
pub fn parts(pipe: &ChildPipe) -> ThreemaParts {
    let (ws, https) = (pipe.inner.clone(), pipe.inner.clone());
    ThreemaParts::from_makers(
        move || PipedConnector(ws.clone()),
        move || PipedHttps(https.clone()),
        pipe.setup.params.clone(),
    )
}

/// The child's `WsConnector` over the pipes.
pub struct PipedConnector(Arc<Shared>);

#[async_trait]
impl WsConnector for PipedConnector {
    async fn connect(
        &self,
        request: WsRequest,
    ) -> Result<(Box<dyn WsSender>, async_channel::Receiver<WsEvent>), NetError> {
        let shared = &self.0;
        let id = shared.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        let (events, stream) = async_channel::unbounded();
        lock(&shared.connects).insert(id, tx);
        lock(&shared.streams).insert(id, events);
        lock(&shared.open).insert(id);
        let kind = [u8::from(request.kind != WsKind::Mediator)];
        let sent = shared
            .write(frame(
                tag::CONNECT,
                &[&id.to_le_bytes(), &kind, request.url.as_bytes()],
            ))
            .await;
        let answer = match sent {
            Ok(()) => rx.await.unwrap_or(Err(NetError::Failed("websocket"))),
            Err(error) => Err(error),
        };
        if let Err(error) = answer {
            lock(&shared.streams).remove(&id);
            lock(&shared.open).remove(&id);
            return Err(error);
        }
        Ok((
            Box::new(PipedSender {
                shared: shared.clone(),
                id,
            }),
            stream,
        ))
    }
}

struct PipedSender {
    shared: Arc<Shared>,
    id: u32,
}

#[async_trait]
impl WsSender for PipedSender {
    async fn send(&self, bytes: Vec<u8>) -> Result<(), NetError> {
        if !lock(&self.shared.open).contains(&self.id) {
            return Err(NetError::Failed("websocket send"));
        }
        self.shared
            .write(frame(tag::SEND, &[&self.id.to_le_bytes(), &bytes]))
            .await
            .map_err(|_| NetError::Failed("websocket send"))
    }

    async fn close(&self, code: u16, reason: String) -> Result<(), NetError> {
        if !lock(&self.shared.open).remove(&self.id) {
            return Err(NetError::Failed("websocket close"));
        }
        // The stream ends at once, as the in-process connector's does after a client close.
        lock(&self.shared.streams).remove(&self.id);
        self.shared
            .write(frame(
                tag::CLOSE,
                &[
                    &self.id.to_le_bytes(),
                    &code.to_le_bytes(),
                    reason.as_bytes(),
                ],
            ))
            .await
            .map_err(|_| NetError::Failed("websocket close"))
    }
}

/// The child's `HttpsClient`: every request goes whole to the parent's fake (the directory answers, anything else is
/// refused and recorded there), and its answer comes back.
pub struct PipedHttps(Arc<Shared>);

#[async_trait]
impl HttpsClient for PipedHttps {
    async fn execute(&self, request: HttpsRequest) -> Result<HttpsResponse, NetError> {
        let shared = &self.0;
        let id = shared.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        lock(&shared.https).insert(id, tx);
        let sent = shared
            .write(frame(
                tag::HTTPS,
                &[&id.to_le_bytes(), &encode_request(&request)],
            ))
            .await;
        if let Err(error) = sent {
            lock(&shared.https).remove(&id);
            return Err(error);
        }
        rx.await.unwrap_or(Err(NetError::Failed("https")))
    }
}

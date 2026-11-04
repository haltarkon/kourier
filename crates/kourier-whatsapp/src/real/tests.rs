//! The HTTP side of the real parts (W5b decisions G12, G15). The pure budget is a plain test; what
//! needs a REAL agent and a socket runs ONLY in a child under `unshare -rn` (an empty network
//! namespace) with `lo` brought up there: no 127.0.0.1 fallback outside the child (G15), and a child
//! that cannot bring `lo` up fails.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use wacore::net::{HttpClient, HttpRequest};
use whatsapp_rust::ErrorChainExt as _;

use super::{BODY_BASE, BODY_RATE, EXECUTE_CAP, RealHttp, body_budget};

/// Set in the children of the tests below (each runs in a child under `unshare -rn`).
const CHILD: &str = "WA_REAL_HTTP_TEST_CHILD";

/// Starts this test binary again under `unshare -rn` for the test `child` of this module and checks that it passed.
/// Where user namespaces are unavailable the caller is skipped (a failure under `KOURIER_REQUIRE_UNSHARE=1`): nothing
/// runs outside the namespace. Returns whether the child ran.
fn in_an_empty_network(child: &str) -> bool {
    if !std::process::Command::new("unshare")
        .args(["-rn", "true"])
        .status()
        .is_ok_and(|status| status.success())
    {
        assert_ne!(
            std::env::var("KOURIER_REQUIRE_UNSHARE").as_deref(),
            Ok("1"),
            "`unshare -rn` is not available and KOURIER_REQUIRE_UNSHARE=1"
        );
        eprintln!("SKIPPED: `unshare -rn` is not available; no agent is built outside it");
        return false;
    }
    let module = module_path!()
        .split_once("::")
        .map_or(module_path!(), |(_, rest)| rest);
    let out = std::process::Command::new("unshare")
        .arg("-rn")
        .arg(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            &format!("{module}::{child}"),
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .output()
        .expect("spawn the child under unshare");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("test result: ok. 1 passed;"),
        "the child failed ({:?}):\n{stdout}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    // What a child measured (its `MEASURED` lines) reaches the parent's output: nextest shows it on a failure, and a
    // hand run with `--success-output immediate` shows it always.
    for line in stdout.lines().filter(|line| line.contains(MEASURED)) {
        eprintln!("{line}");
    }
    true
}

/// The mark of a child's line that the parent passes on.
const MEASURED: &str = "MEASURED";

/// In the child: `lo` up (G15: no fallback), or the child fails.
fn loopback_up() {
    let up = std::process::Command::new("ip")
        .args(["link", "set", "lo", "up"])
        .status()
        .is_ok_and(|status| status.success());
    assert!(up, "lo does not come up in the child (G15: no fallback)");
}

/// A peer on 127.0.0.1 that reads a request (the head, then the body the head's `content-length` names) and answers
/// it with the next of `answers` (status and body), one connection each. Returns its port.
fn peer(answers: Vec<(u16, Vec<u8>)>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for (status, answer) in answers {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            read_request(&mut stream);
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                answer.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&answer);
        }
    });
    port
}

/// Reads a request's head and the body its `content-length` names.
fn read_request(stream: &mut TcpStream) {
    let mut seen = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return;
        }
        seen.extend_from_slice(&chunk[..n]);
        if let Some(at) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&seen[..head_end]).to_ascii_lowercase();
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let mut have = seen.len() - head_end;
    while have < length {
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return;
        }
        have += n;
    }
}

/// The upload body's time is 60 s plus the length at 16 KiB/s (a 16 MB file: about 17 minutes).
#[test]
fn the_body_timeout_grows_with_the_length() {
    assert_eq!(BODY_BASE, Duration::from_secs(60));
    assert_eq!(
        body_budget(0, BODY_BASE, BODY_RATE),
        Duration::from_secs(60)
    );
    assert_eq!(
        body_budget(16 * 1024, BODY_BASE, BODY_RATE),
        Duration::from_secs(61)
    );
    assert_eq!(
        body_budget(16_000_000, BODY_BASE, BODY_RATE),
        Duration::from_secs(60) + Duration::from_secs_f64(976.5625)
    );
}

/// The body time of the two upload children below (the time left of it is ureq's write period, at most this).
const BODY_TIME: Duration = Duration::from_secs(2);
/// The body of the two upload children: far more than the socket buffers of both ends hold.
const BODY_LEN: u64 = 64 * 1024 * 1024;
/// What a busy machine may add to a child's end (the measurements of 2026-10-28 ran up to load 33).
const LOAD_SLACK: Duration = Duration::from_secs(5);

/// A listener on 127.0.0.1 whose connections have a tiny receive buffer, so the body fills it at once.
fn small_listener() -> TcpListener {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let small: libc::c_int = 4096;
    // SAFETY: a valid socket descriptor and a pointer to a c_int of the size given.
    let rc = unsafe {
        libc::setsockopt(
            listener.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            (&raw const small).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    assert_eq!(rc, 0, "SO_RCVBUF");
    listener
}

/// Starts `http`'s upload of a synthetic body of `len` bytes to `port` on a thread of its own; returns when it started
/// and where its answer comes.
fn upload_in_the_background(
    http: RealHttp,
    port: u16,
    len: u64,
) -> (
    Instant,
    std::sync::mpsc::Receiver<anyhow::Result<wacore::net::HttpResponse>>,
) {
    let body = Box::new(std::io::repeat(0x5a).take(len));
    let request = HttpRequest::post(format!("http://127.0.0.1:{port}/mms/image"));
    let (done, ended) = std::sync::mpsc::channel();
    let started = Instant::now();
    std::thread::spawn(move || {
        let _ = done.send(http.execute_upload(request, body, len));
    });
    (started, ended)
}

/// Whether a failed upload is the end of the body's time: ureq's send-body timeout (what bounds what: [`body_budget`]).
fn is_body_time_end(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<ureq::Error>(),
        Some(ureq::Error::Timeout(ureq::Timeout::SendBody))
    )
}

/// G12/G15 (settles U5): a CDN socket that takes no more bytes ends the upload with ureq's
/// send-body timeout within the bound below, and the agent carries G12's settings. The parent
/// starts this binary again under `unshare -rn` for [`stalled_upload_child`]; where user namespaces
/// are unavailable it is skipped (a failure under `KOURIER_REQUIRE_UNSHARE=1`), never run outside.
#[test]
fn a_stalled_upload_ends_at_its_body_time() {
    in_an_empty_network("stalled_upload_child");
}

/// How many of ureq's write periods a stalled body runs. ureq 3.4.2 sets the socket's write timeout to the time
/// left of the body before each write of a buffer, and `write_all` writes again under that SAME period after a
/// partial write: a write fails only after a whole period in which it moved no byte. The receiving kernel still
/// takes a few bytes during the first period (it compacts its queue, the window probes find room), so the measured
/// stall is THREE periods (2026-10-28 on the server, 2 s periods, about 75 runs quiet and under 28 busy loops at
/// load 30: 6.10-6.63 s; once two, 4.10 s); the bound allows one more (a late byte under load: the old bound of 7 s
/// failed once in a full ci) plus [`LOAD_SLACK`]. A failure here means the kernel took bytes for longer, not that
/// Kourier waits longer: the attempt's own stall watch (`send::upload`, 60 s) ends a stalled upload long before.
const STALL_PERIODS: u32 = 4;

/// The child: `lo` up, a listener with a tiny receive buffer that accepts and never reads, a
/// 64 MiB synthetic body (far more than both socket buffers hold) under a 2 s body time.
#[test]
fn stalled_upload_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    loopback_up();

    let http = RealHttp::new().with_body_time(BODY_TIME, u64::MAX);
    // G12's agent: connect 30 s, head 120 s, body read 300 s, no proxy, no redirect.
    let config = http.agent.config();
    let timeouts = config.timeouts();
    assert_eq!(timeouts.connect, Some(Duration::from_secs(30)));
    assert_eq!(timeouts.recv_response, Some(Duration::from_secs(120)));
    assert_eq!(timeouts.recv_body, Some(Duration::from_secs(300)));
    assert!(config.proxy().is_none());
    assert_eq!(config.max_redirects(), 0);

    let listener = small_listener();
    let port = listener.local_addr().expect("addr").port();
    let (release, held) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        // Hold the connection open and read nothing until the test ends.
        let _ = held.recv();
        drop(stream);
    });

    let (started, ended) = upload_in_the_background(http, port, BODY_LEN);
    let bound = BODY_TIME * STALL_PERIODS + LOAD_SLACK;
    let answer = ended.recv_timeout(bound).unwrap_or_else(|_| {
        panic!("the stalled upload did not end within {STALL_PERIODS} write periods of {BODY_TIME:?} plus {LOAD_SLACK:?}")
    });
    let elapsed = started.elapsed();
    let _ = release.send(());
    let error = answer.expect_err("a stalled upload cannot succeed");
    println!("{MEASURED} stalled upload: ended after {elapsed:?}");
    assert!(
        is_body_time_end(&error),
        "not the send-body timeout: {error}"
    );
    assert!(
        elapsed >= BODY_TIME,
        "ended before its body time: {elapsed:?}"
    );
    assert!(elapsed < bound, "{elapsed:?}");
}

/// The trickle of [`trickled_upload_child`]'s peer: it reads at most this many bytes...
const TRICKLE_BYTES: usize = 1024;
/// ... this often (20 KiB/s, while the body's time asks for 64 MiB in 2 s).
const TRICKLE_EVERY: Duration = Duration::from_millis(50);

/// The body's time is a CAP, not only a stall timeout: a CDN socket that takes a trickle of bytes, far below the rate
/// the body's time asks for, ends the upload with ureq's send-body timeout soon after that time, after bytes moved.
/// ureq refuses a write that starts past its deadline, and the write in flight then has a period of what was left
/// (see [`body_budget`]); measured 2026-10-28: 2.00-2.06 s at peers of 20 KiB/s to 1.5 GB/s. A ureq upgrade whose
/// timeout only ends a write that moves nothing fails here (with the timeout off: 9.8 s, the peer took 199,794 bytes).
#[test]
fn a_trickled_upload_ends_at_its_body_time() {
    in_an_empty_network("trickled_upload_child");
}

/// The child: `lo` up, a peer with a tiny receive buffer that reads [`TRICKLE_BYTES`] every [`TRICKLE_EVERY`], a 64 MiB
/// synthetic body under a 2 s body time.
///
/// THE BOUND: the upload ends at the body's time, or after the write in flight then. That write has at most
/// `BUFFER_BYTES` to move, which the trickle takes in `BUFFER_BYTES / 20 KiB/s` = 0.8 s, but the writer sees the freed
/// room only at the end of its period (the kernel wakes a blocked writer early only when a third of the send buffer is
/// free, megabytes on loopback), and its period is the time that was left of the body when it started: at most
/// [`BODY_TIME`]. So it ends within `BODY_TIME` (the cap) + `BODY_TIME` (one period) + 0.8 s, plus [`LOAD_SLACK`].
#[test]
fn trickled_upload_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    loopback_up();

    let listener = small_listener();
    let port = listener.local_addr().expect("addr").port();
    let taken = Arc::new(AtomicU64::new(0));
    let over = Arc::new(AtomicBool::new(false));
    let (peer_taken, peer_over) = (Arc::clone(&taken), Arc::clone(&over));
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut chunk = [0u8; TRICKLE_BYTES];
        while !peer_over.load(Ordering::SeqCst) {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => peer_taken.fetch_add(n as u64, Ordering::SeqCst),
            };
            std::thread::sleep(TRICKLE_EVERY);
        }
    });

    let (started, ended) = upload_in_the_background(
        RealHttp::new().with_body_time(BODY_TIME, u64::MAX),
        port,
        BODY_LEN,
    );
    let one_buffer = Duration::from_secs_f64(
        super::BUFFER_BYTES as f64 / TRICKLE_BYTES as f64 * TRICKLE_EVERY.as_secs_f64(),
    );
    let bound = BODY_TIME + BODY_TIME + one_buffer + LOAD_SLACK;
    let answer = ended.recv_timeout(bound).unwrap_or_else(|_| {
        panic!(
            "the trickled upload did not end within {bound:?} (the peer took {} bytes): the body's time is no cap",
            taken.load(Ordering::SeqCst)
        )
    });
    let elapsed = started.elapsed();
    over.store(true, Ordering::SeqCst);
    let took = taken.load(Ordering::SeqCst);
    let error = answer.expect_err("a trickled 64 MiB upload cannot succeed in its body time");
    println!("{MEASURED} trickled upload: ended after {elapsed:?}, the peer took {took} bytes");
    assert!(
        is_body_time_end(&error),
        "not the send-body timeout: {error}"
    );
    assert!(
        elapsed >= BODY_TIME,
        "ended before its body time: {elapsed:?}"
    );
    // Bytes moved all along: far more than the request's head, so this was a slow body, not a stalled one.
    assert!(took >= 8 * 1024, "the peer took only {took} bytes");
    // What the send path makes of it: no HTTP status in the chain, so `upload_failure` says `Incomplete`, which
    // `upload_error` answers `Transient` after the stall floor.
    let chain: &(dyn std::error::Error + Send + Sync + 'static) = error.as_ref();
    assert_eq!(chain.http_status(), None, "no status in the chain");
}

/// Fix F6 (review HIGH, E2 of the interop review): ureq sends a SIZED body without ever asking the reader again after
/// its last byte (no read answers 0), and holds the reader until the answer's head is in. The upload's stall watch
/// (`send::upload`) finds the body's end by position and disarms when the reader drops because of exactly this; the
/// test pins it: a real `execute_upload` to a peer that reads the whole body and answers 200 after a pause sees no
/// 0-byte read, and the reader is still alive when the answer is written. If a ureq bump changes either, this says so.
#[test]
fn a_sized_body_is_not_read_to_its_end_and_its_reader_lives_until_the_answer() {
    in_an_empty_network("sized_body_child");
}

/// A body reader that records what ureq asks of it.
struct Probe {
    left: u64,
    eof_read: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

impl Read for Probe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            if !buf.is_empty() {
                self.eof_read.store(true, Ordering::SeqCst);
            }
            return Ok(0);
        }
        let n = buf
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        buf[..n].fill(0x5a);
        self.left -= n as u64;
        Ok(n)
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

/// The child of [`a_sized_body_is_not_read_to_its_end_and_its_reader_lives_until_the_answer`].
#[test]
fn sized_body_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    loopback_up();
    let (eof_read, dropped) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let len: u64 = 300 * 1024;
    // The peer reads the whole body, waits, and only then answers; the flag is taken at the moment it answers.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let at_answer = Arc::clone(&dropped);
    let (alive, seen_alive) = std::sync::mpsc::channel::<bool>();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        read_request(&mut stream);
        std::thread::sleep(Duration::from_millis(400));
        let _ = alive.send(!at_answer.load(Ordering::SeqCst));
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}");
    });
    let body = Box::new(Probe {
        left: len,
        eof_read: Arc::clone(&eof_read),
        dropped: Arc::clone(&dropped),
    });
    let request = HttpRequest::post(format!("http://127.0.0.1:{port}/mms/document"));
    let answer = RealHttp::new()
        .execute_upload(request, body, len)
        .expect("the upload is answered");
    assert_eq!(answer.status_code, 200);
    assert!(
        !eof_read.load(Ordering::SeqCst),
        "ureq now reads a sized body to its end: the stall watch's tail may start earlier, check `send::upload`"
    );
    assert!(
        seen_alive
            .recv_timeout(Duration::from_secs(5))
            .expect("the peer answered"),
        "ureq now drops the body before the answer's head is in: the tail rule covers less than it says"
    );
    assert!(
        dropped.load(Ordering::SeqCst),
        "the reader goes with the request"
    );
}

/// Fix F7 (privacy LOW 3): a plain request's answer (`execute`: the version file, the upload's resume check) is read
/// through a cap, an answer over it is an error and not a buffer of up to 2 GiB; the default cap is 4 MiB, and a
/// streaming download keeps upstream's own limit. Mutant: `execute` through the client without the cap.
#[test]
fn a_plain_answer_over_the_cap_is_an_error_and_a_download_keeps_its_limit() {
    in_an_empty_network("capped_answer_child");
}

#[test]
fn capped_answer_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    loopback_up();
    assert_eq!(EXECUTE_CAP, 4 * 1024 * 1024);
    let cap: u64 = 4096;
    let http = RealHttp::new().with_execute_cap(cap);
    let small = vec![b'a'; 100];
    let within = vec![b'b'; usize::try_from(cap).expect("cap") / 2];
    let big = vec![b'c'; usize::try_from(cap).expect("cap") * 2];
    let port = peer(vec![
        (200, small.clone()),
        (200, within.clone()),
        (200, big.clone()),
        (200, big.clone()),
    ]);
    let url = format!("http://127.0.0.1:{port}/version");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let one = runtime
        .block_on(http.execute(HttpRequest::get(url.clone())))
        .expect("a small answer");
    assert_eq!((one.status_code, one.body), (200, small));
    let two = runtime
        .block_on(http.execute(HttpRequest::get(url.clone())))
        .expect("an answer within the cap");
    assert_eq!(two.body.len(), within.len());
    let three = runtime.block_on(http.execute(HttpRequest::get(url.clone())));
    assert!(
        three.is_err(),
        "an answer over the cap is an error, not a buffer"
    );
    // A streaming download is upstream's: the cap of `execute` does not reach it.
    let mut streamed = http
        .execute_streaming(HttpRequest::get(url))
        .expect("a download")
        .body;
    let mut got = Vec::new();
    streamed.read_to_end(&mut got).expect("read");
    assert_eq!(got.len(), big.len(), "the download keeps upstream's limit");
}

/// Fix F7, the cap the maker builds in (re-check N1): the test above swaps the cap for a small one, so it cannot see
/// what [`RealHttp::new`] itself sets. This one uses the maker's own client against a peer: an answer of half the
/// cap is read whole, one of [`EXECUTE_CAP`] + 1 bytes is an error. Mutant: `new()` builds `plain` with `u64::MAX`.
#[test]
fn the_makers_client_refuses_a_plain_answer_of_the_cap_and_a_byte() {
    in_an_empty_network("default_cap_child");
}

#[test]
fn default_cap_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    loopback_up();
    let cap = usize::try_from(EXECUTE_CAP).expect("cap");
    let port = peer(vec![(200, vec![b'd'; cap / 2]), (200, vec![b'e'; cap + 1])]);
    let url = format!("http://127.0.0.1:{port}/version");
    let http = RealHttp::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let within = runtime
        .block_on(http.execute(HttpRequest::get(url.clone())))
        .expect("an answer within the cap");
    assert_eq!(within.body.len(), cap / 2);
    let over = runtime.block_on(http.execute(HttpRequest::get(url)));
    assert!(
        over.is_err(),
        "an answer one byte over the maker's cap is an error, not a buffer"
    );
}

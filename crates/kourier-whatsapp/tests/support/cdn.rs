//! THE CDN FAKE of the media tests (W5b chunk B2, brief §2.6, decisions G16, G19, G20): an `HttpClient`
//! the clients of a Wire test get instead of the refusing one (`wire::parts_handing_client_with_http`),
//! so upstream's `upload_stream` runs its whole loop (the `w:m` IQ on the Wire, the resume check, one POST
//! per host, the 401 refetch) against answers the test scripts, and nothing leaves the process.
//!
//! - `execute` is the RESUME CHECK (upstream asks it only from 5 MiB of ciphertext, a body-less POST with
//!   `resume=1`): scripted [`Check`]s, one per call, then 404 ("upload from zero").
//! - `execute_upload` is the POST: it reads the body in 64 KiB pieces as a CDN would, records what it saw
//!   ([`Post`]: host, path, query KEYS, header names, the declared length, the bytes read, how it ended;
//!   never a value of the query), and answers by the next scripted [`Reply`] (then [`Reply::Ok`]). A body
//!   whose reading fails ends the POST with an `Err`, as ureq does when the reader fails mid-send.
//! - What it keeps: every finished upload under its `direct_path` ([`Cdn::blob`]: the PEER downloads it
//!   from there and decrypts it with the received message's key), and the bytes a broken POST got before
//!   it broke under its token, so a later `file_offset` POST of the same token completes it (the resume).
//! - THE LOG ([`Cdn::log_to`], G19): every finished upload is written to a file, one line (the path, the
//!   `direct_path`, the sha256 of the bytes), with an fsync BEFORE the answer leaves, so a killed child
//!   leaves exactly the uploads the client was answered ([`read_log`]).
//! - THE MARKERS (G20): the `w:m` auth ([`AUTH`]), the url's own part ([`URL_MARKER`]), the
//!   `direct_path`'s ([`DIRECT_PATH_MARKER`]) and the body of a failure ([`BODY_MARKER`]) are long
//!   synthetic runs with a unique 8-digit tail, listed by [`secrets`] for the D10 grep of every media
//!   child: a line that holds one leaked what upstream's error chain or a URL carries.
//!
//! Every value is SYNTHETIC; the host names end in `.example.test`, which resolves nowhere.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use wacore::download::{DownloadUtils, Downloadable, MediaType};
use wacore::net::{HttpClient, HttpRequest, HttpResponse, UploadBody};
use waproto::whatsapp as wa;

/// The first and the second media host of the `w:m` answer.
pub const HOST_A: &str = "mmg-a.media.example.test";
pub const HOST_B: &str = "mmg-b.media.example.test";
/// The `w:m` auth (a query value of every POST).
pub const AUTH: &str = "cdnauthmarkerQkVwRtYhUjNmZxCvBnAsDfGhJkLqWeRt61829437";
/// The url's own part (`oh=`), after the `direct_path`.
pub const URL_MARKER: &str = "cdnurlmarkerPlOkIjUhYgTfRdEsWaQzXcVbNmLkJhGf73518264";
/// The start of every `direct_path` (an upload's number follows it).
pub const DIRECT_PATH_MARKER: &str = "cdndirectpathmarkerMnBvCxZaSdFgHjKlPoIuYtRe84927351";
/// In the body of every scripted failure ([`Reply::Status`]): upstream puts the body into its error.
pub const BODY_MARKER: &str = "cdnbodymarkerZqXwCeVrBtNyMuLiKoJpHgFdSa95281746";

/// The markers of this fake, for the D10 grep (`send_secrets()` adds them).
pub fn secrets() -> Vec<&'static str> {
    // The host names too (fix F14): a line that names the media host of the account's `w:m` answer says where the
    // account uploads; neither Kourier's nor upstream's lines may.
    vec![
        AUTH,
        URL_MARKER,
        DIRECT_PATH_MARKER,
        BODY_MARKER,
        HOST_A,
        HOST_B,
    ]
}

/// What the PEER learned of the files it downloaded in this process (fix F14): the media key and both sha256 values of
/// every received message, as hex and as base64 (standard and URL-safe, no padding: upstream's upload path puts
/// `file_enc_sha256` in the URL as the URL-safe form). Random per run, so a grep for the fixture's values is blind to
/// them; once the peer holds them, a line that holds one of them leaked a key or a hash. A child runs one case, so one
/// process-wide list serves ([`learned`]).
static LEARNED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The needles of `bytes` (a key or a hash): hex, base64, URL-safe base64.
pub fn needles_of(bytes: &[u8]) -> [String; 3] {
    const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    fn base64(bytes: &[u8], alphabet: &[u8; 64]) -> String {
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
            // 2, 3 or 4 characters for 1, 2 or 3 bytes (no padding).
            for i in 0..=chunk.len() {
                out.push(char::from(alphabet[((n >> (18 - 6 * i)) & 63) as usize]));
            }
        }
        out
    }
    let hex = bytes.iter().fold(String::new(), |mut hex, b| {
        let _ = std::fmt::Write::write_fmt(&mut hex, format_args!("{b:02x}"));
        hex
    });
    [hex, base64(bytes, STD), base64(bytes, URL)]
}

pub fn learn(bytes: Option<&[u8]>) {
    let Some(bytes) = bytes.filter(|b| !b.is_empty()) else {
        return;
    };
    LEARNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .extend(needles_of(bytes));
}

/// The keys and hashes the peer has learned so far, in every form a log line could print them in (fix F14).
pub fn learned() -> Vec<String> {
    LEARNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// How long a [`Reply::StallAt`] holds at most when nobody releases it (a test that forgets does not
/// leave a thread behind for ever).
const STALL_LIMIT: Duration = Duration::from_secs(120);
/// The piece a POST's body is read in.
const PIECE: usize = 64 * 1024;

/// The answer to one POST.
pub enum Reply {
    /// Read the whole body, keep it, answer 200 with `url` and `direct_path`.
    Ok,
    /// Read the whole body, answer this status; the body holds [`BODY_MARKER`].
    Status(u16),
    /// The connection breaks after `n` bytes of the body: an `Err`; the `n` bytes are kept under the
    /// token for a later resume.
    FailAt(u64),
    /// After `n` bytes the CDN stops reading until [`Cdn::release`] (or [`STALL_LIMIT`]), then reads
    /// on and answers as [`Reply::Ok`].
    StallAt(u64),
    /// After `n` bytes the CDN calls the function once (a test's hook: the stop pressed mid-POST), then
    /// reads on and answers as [`Reply::Ok`] (or `Err` when the body's reading fails).
    CallAt(u64, Box<dyn FnOnce() + Send>),
}

/// The answer to one resume check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// 404: upload from zero.
    NotFound,
    /// `{"resume":"<n>"}`: upload from byte `n`.
    Resume(u64),
    /// `{"resume":"complete", url, direct_path}`: the CDN holds the file already (the one kept under the
    /// same token, or a new path that holds nothing when there is none).
    Complete,
}

/// How a POST's body reading ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyEnd {
    /// Every byte the request declared.
    Whole,
    /// The reader failed (the stop's mechanism), with the kind upstream got.
    Failed(std::io::ErrorKind),
    /// The scripted break of [`Reply::FailAt`].
    Broken,
}

/// What the CDN saw of one POST. Holds no query VALUE (the token and the auth are compared, not kept).
#[derive(Debug, Clone)]
pub struct Post {
    /// [`HOST_A`], [`HOST_B`], or another name the client used.
    pub host: String,
    /// The path without its token segment: `/mms/image`, `/mms/video`, `/mms/audio`, `/mms/document`.
    pub path: String,
    /// The path's last segment equals the query's `token`.
    pub token_in_path: bool,
    /// The query's `auth` equals [`AUTH`].
    pub auth_ok: bool,
    /// The query's keys, in order.
    pub keys: Vec<String>,
    /// The header names, sorted.
    pub headers: Vec<String>,
    /// The `file_offset` of a resumed POST.
    pub file_offset: Option<u64>,
    /// The length the client declared for the body.
    pub content_length: u64,
    /// The bytes read.
    pub received: u64,
    pub read: BodyEnd,
    /// The status answered; `None` = the POST ended with an `Err`.
    pub status: Option<u16>,
    /// The `direct_path` answered with a 200.
    pub direct_path: Option<String>,
}

/// What the CDN saw of one resume check.
#[derive(Debug, Clone)]
pub struct Checked {
    pub host: String,
    pub path: String,
    pub keys: Vec<String>,
    pub answer: Check,
}

/// One line of the upload log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Logged {
    pub path: String,
    pub direct_path: String,
    pub sha256: String,
}

#[derive(Default)]
struct Inner {
    replies: Mutex<VecDeque<Reply>>,
    checks: Mutex<VecDeque<Check>>,
    posts: Mutex<Vec<Post>>,
    checked: Mutex<Vec<Checked>>,
    /// Finished uploads by `direct_path`.
    blobs: Mutex<HashMap<String, Vec<u8>>>,
    /// The `direct_path` of a finished upload, by its token.
    by_token: Mutex<HashMap<String, String>>,
    /// What broken POSTs got, by token (from byte 0).
    partials: Mutex<HashMap<String, Vec<u8>>>,
    next: AtomicUsize,
    /// [`Reply::StallAt`]: `(stalled now, released)`.
    stall: Mutex<(usize, bool)>,
    stall_cv: Condvar,
    log: Mutex<Option<std::fs::File>>,
}

/// The fake; a clone shares everything (the client's maker hands out clones).
#[derive(Clone, Default)]
pub struct Cdn(Arc<Inner>);

/// The parts of an upload URL: `(host, path without the token, token in path, auth ok, query keys,
/// the token, file_offset)`.
struct Url {
    host: String,
    path: String,
    token_in_path: bool,
    auth_ok: bool,
    keys: Vec<String>,
    token: String,
    file_offset: Option<u64>,
}

fn parse(url: &str) -> Url {
    let rest = url.strip_prefix("https://").unwrap_or(url);
    let (host, tail) = rest.split_once('/').unwrap_or((rest, ""));
    let (path, query) = tail.split_once('?').unwrap_or((tail, ""));
    let (base, last) = path.rsplit_once('/').unwrap_or(("", path));
    let mut keys = Vec::new();
    let mut token = String::new();
    let mut auth_ok = false;
    let mut file_offset = None;
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        keys.push(k.to_owned());
        match k {
            "token" => token = v.to_owned(),
            "auth" => auth_ok = v == AUTH,
            "file_offset" => file_offset = v.parse().ok(),
            _ => {}
        }
    }
    Url {
        host: host.to_owned(),
        path: format!("/{base}"),
        token_in_path: !token.is_empty() && last == token,
        auth_ok,
        keys,
        token,
        file_offset,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Cdn {
    /// The answers of the next POSTs, in order (after them: [`Reply::Ok`]).
    pub fn reply(&self, reply: Reply) -> &Self {
        self.0.replies.lock().expect("lock").push_back(reply);
        self
    }

    /// The answers of the next resume checks, in order (after them: [`Check::NotFound`]).
    pub fn check(&self, check: Check) -> &Self {
        self.0.checks.lock().expect("lock").push_back(check);
        self
    }

    /// Every POST so far.
    pub fn posts(&self) -> Vec<Post> {
        self.0.posts.lock().expect("lock").clone()
    }

    /// Every resume check so far.
    pub fn checks(&self) -> Vec<Checked> {
        self.0.checked.lock().expect("lock").clone()
    }

    /// The bytes of the finished upload at `direct_path` (what a download of it gives).
    pub fn blob(&self, direct_path: &str) -> Option<Vec<u8>> {
        self.0.blobs.lock().expect("lock").get(direct_path).cloned()
    }

    /// How many uploads finished.
    pub fn finished(&self) -> usize {
        self.0.blobs.lock().expect("lock").len()
    }

    /// Writes every finished upload to `path`, one line each in one write, synced before the answer; a line a killed
    /// child left unfinished there is cut first (`kill_log::open_append`).
    pub fn log_to(&self, path: &Path) {
        let file = super::kill_log::open_append(path);
        *self.0.log.lock().expect("lock") = Some(file);
    }

    /// Lets a [`Reply::StallAt`] read on (one that comes later does not stall either).
    pub fn release(&self) {
        let mut stall = self.0.stall.lock().expect("lock");
        stall.1 = true;
        self.0.stall_cv.notify_all();
    }

    /// Waits (at most `within`) until a POST stalls.
    pub async fn wait_stalled(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while self.0.stall.lock().expect("lock").0 == 0 {
            if Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        true
    }

    /// Waits (at most `within`) until `n` POSTs ended.
    pub async fn wait_posts(&self, n: usize, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while self.0.posts.lock().expect("lock").len() < n {
            if Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        true
    }

    fn hold(&self) {
        let mut stall = self.0.stall.lock().expect("lock");
        if stall.1 {
            return;
        }
        stall.0 += 1;
        let deadline = Instant::now() + STALL_LIMIT;
        while !stall.1 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            stall = self.0.stall_cv.wait_timeout(stall, left).expect("lock").0;
        }
        stall.0 -= 1;
    }

    /// A new `direct_path` and its url on `host`.
    fn place(&self, host: &str) -> (String, String) {
        let n = self.0.next.fetch_add(1, Ordering::SeqCst);
        let direct_path = format!("/v/t62.7118-24/{DIRECT_PATH_MARKER}{n:04}.enc");
        let url = format!("https://{host}{direct_path}?ccb=11-4&oh={URL_MARKER}&oe=6A1B2C3D");
        (direct_path, url)
    }

    /// Keeps a finished upload: the blob, its token, the synced log line. Returns the answer's body.
    fn finish(&self, url: &Url, bytes: Vec<u8>) -> (String, Vec<u8>) {
        let (direct_path, full_url) = self.place(&url.host);
        if let Some(file) = self.0.log.lock().expect("lock").as_mut() {
            let line = format!(
                "upload path={} direct_path={direct_path} sha256={}\n",
                url.path,
                hex(&Sha256::digest(&bytes))
            );
            file.write_all(line.as_bytes())
                .and_then(|()| file.sync_data())
                .expect("the cdn log is written");
        }
        self.0
            .blobs
            .lock()
            .expect("lock")
            .insert(direct_path.clone(), bytes);
        self.0
            .by_token
            .lock()
            .expect("lock")
            .insert(url.token.clone(), direct_path.clone());
        self.0.partials.lock().expect("lock").remove(&url.token);
        let body = serde_json::json!({ "url": full_url, "direct_path": direct_path });
        (direct_path, body.to_string().into_bytes())
    }
}

/// Reads `body` until `limit` bytes are in `got` (or its end); `Err` when the reader fails.
fn read_up_to(body: &mut UploadBody, got: &mut Vec<u8>, limit: u64) -> Result<(), std::io::Error> {
    let mut piece = vec![0u8; PIECE];
    while (got.len() as u64) < limit {
        let want = PIECE.min((limit - got.len() as u64) as usize);
        match body.read(&mut piece[..want]) {
            Ok(0) => return Ok(()),
            Ok(n) => got.extend_from_slice(&piece[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[async_trait]
impl HttpClient for Cdn {
    async fn execute(&self, request: HttpRequest) -> anyhow::Result<HttpResponse> {
        let url = parse(&request.url);
        let answer = self
            .0
            .checks
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or(Check::NotFound);
        self.0.checked.lock().expect("lock").push(Checked {
            host: url.host.clone(),
            path: url.path.clone(),
            keys: url.keys.clone(),
            answer,
        });
        Ok(match answer {
            Check::NotFound => HttpResponse::new(404, Vec::new()),
            Check::Resume(n) => HttpResponse::new(
                200,
                serde_json::json!({ "resume": n.to_string() })
                    .to_string()
                    .into_bytes(),
            ),
            Check::Complete => {
                let known = self
                    .0
                    .by_token
                    .lock()
                    .expect("lock")
                    .get(&url.token)
                    .cloned();
                let (direct_path, full_url) = match known {
                    Some(direct_path) => {
                        let full_url = format!(
                            "https://{}{direct_path}?ccb=11-4&oh={URL_MARKER}&oe=6A1B2C3D",
                            url.host
                        );
                        (direct_path, full_url)
                    }
                    None => self.place(&url.host),
                };
                HttpResponse::new(
                    200,
                    serde_json::json!({
                        "resume": "complete",
                        "url": full_url,
                        "direct_path": direct_path,
                    })
                    .to_string()
                    .into_bytes(),
                )
            }
        })
    }

    fn supports_upload_streaming(&self) -> bool {
        true
    }

    fn execute_upload(
        &self,
        request: HttpRequest,
        mut body: UploadBody,
        content_length: u64,
    ) -> anyhow::Result<HttpResponse> {
        enum Act {
            Answer(u16),
            Break,
            Stall,
            Call(Box<dyn FnOnce() + Send>),
        }
        let url = parse(&request.url);
        let mut headers: Vec<String> = request.headers.keys().cloned().collect();
        headers.sort();
        let reply = self
            .0
            .replies
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or(Reply::Ok);
        let (point, act) = match reply {
            Reply::Ok => (None, Act::Answer(200)),
            Reply::Status(status) => (None, Act::Answer(status)),
            Reply::FailAt(n) => (Some(n), Act::Break),
            Reply::StallAt(n) => (Some(n), Act::Stall),
            Reply::CallAt(n, call) => (Some(n), Act::Call(call)),
        };
        let mut got = Vec::new();
        let mut result = Ok(());
        let mut broken = false;
        let mut answer = 200;
        if let Some(n) = point {
            result = read_up_to(&mut body, &mut got, n.min(content_length));
        }
        if result.is_ok() {
            match act {
                Act::Answer(status) => answer = status,
                Act::Break => broken = true,
                Act::Stall => self.hold(),
                Act::Call(call) => call(),
            }
        }
        if result.is_ok() && !broken {
            result = read_up_to(&mut body, &mut got, content_length);
        }
        // The file's bytes from byte 0: a resumed POST continues what a broken one of its token got.
        let received = got.len() as u64;
        let whole = match url.file_offset {
            Some(offset) => {
                let mut whole = self
                    .0
                    .partials
                    .lock()
                    .expect("lock")
                    .get(&url.token)
                    .cloned()
                    .unwrap_or_default();
                whole.truncate(usize::try_from(offset).unwrap_or(usize::MAX));
                whole.extend_from_slice(&got);
                whole
            }
            None => got,
        };
        let mut post = Post {
            host: url.host.clone(),
            path: url.path.clone(),
            token_in_path: url.token_in_path,
            auth_ok: url.auth_ok,
            keys: url.keys.clone(),
            headers,
            file_offset: url.file_offset,
            content_length,
            received,
            read: BodyEnd::Whole,
            status: None,
            direct_path: None,
        };
        let failed = match &result {
            Err(e) => Some(BodyEnd::Failed(e.kind())),
            Ok(()) if broken => Some(BodyEnd::Broken),
            Ok(()) => None,
        };
        if let Some(end) = failed {
            post.read = end;
            self.0
                .partials
                .lock()
                .expect("lock")
                .insert(url.token.clone(), whole);
            self.0.posts.lock().expect("lock").push(post);
            return Err(anyhow::anyhow!(
                "cdn fake: the connection broke during the body"
            ));
        }
        if answer != 200 {
            post.status = Some(answer);
            self.0.posts.lock().expect("lock").push(post);
            return Ok(HttpResponse::new(
                answer,
                format!("{{\"error\":\"{BODY_MARKER}\"}}").into_bytes(),
            ));
        }
        let (direct_path, answer_body) = self.finish(&url, whole);
        post.status = Some(200);
        post.direct_path = Some(direct_path);
        self.0.posts.lock().expect("lock").push(post);
        Ok(HttpResponse::new(200, answer_body))
    }
}

impl Cdn {
    /// THE PEER'S DOWNLOAD (decision G18): what a receiver does with a media message it got: the media
    /// part of `message` (an image, video, audio or document message, or the document inside a
    /// `documentWithCaptionMessage`), its blob from this CDN by its `direct_path`, the sha256 of the blob
    /// against `file_enc_sha256`, the decryption under the key type the RECEIVER derives from the
    /// message's shape (`Downloadable::app_info`, not the type the sender uploaded with), the sha256 of
    /// the plaintext against `file_sha256`, its length against `file_length`, and a `mimetype`. Returns
    /// that type and the plaintext; the error is a fixed text (it names no value).
    pub fn download(&self, message: &wa::Message) -> Result<(MediaType, Vec<u8>), &'static str> {
        let caption_wrapped = message
            .document_with_caption_message
            .as_option()
            .and_then(|w| w.message.as_option())
            .and_then(|m| m.document_message.as_option());
        let (item, mimetype): (&dyn Downloadable, Option<&str>) =
            if let Some(m) = message.image_message.as_option() {
                (m, m.mimetype.as_deref())
            } else if let Some(m) = message.video_message.as_option() {
                (m, m.mimetype.as_deref())
            } else if let Some(m) = message.audio_message.as_option() {
                (m, m.mimetype.as_deref())
            } else if let Some(m) = message.document_message.as_option() {
                (m, m.mimetype.as_deref())
            } else if let Some(m) = caption_wrapped {
                (m, m.mimetype.as_deref())
            } else {
                return Err("the message holds no media");
            };
        // The peer now knows this message's key and hashes: the D10 grep of the child looks for them (fix F14).
        learn(item.media_key());
        learn(item.file_sha256());
        learn(item.file_enc_sha256());
        if mimetype.is_none_or(str::is_empty) {
            return Err("the media has no mimetype");
        }
        let direct_path = item.direct_path().ok_or("the media has no direct_path")?;
        let blob = self
            .blob(direct_path)
            .ok_or("the CDN holds nothing at the direct_path")?;
        if Some(Sha256::digest(&blob).as_slice()) != item.file_enc_sha256() {
            return Err("the blob's sha256 is not the file_enc_sha256");
        }
        let key = item.media_key().ok_or("the media has no media_key")?;
        let kind = item.app_info();
        let plain = DownloadUtils::verify_and_decrypt(&blob, key, kind)
            .map_err(|_| "the blob does not decrypt under the message's key and type")?;
        if Some(Sha256::digest(&plain).as_slice()) != item.file_sha256() {
            return Err("the plaintext's sha256 is not the file_sha256");
        }
        if Some(plain.len() as u64) != item.file_length() {
            return Err("the plaintext's length is not the file_length");
        }
        Ok((kind, plain))
    }
}

/// The upload log [`Cdn::log_to`] wrote at `path` (none when it does not exist), its complete lines only: a line a
/// killed child did not finish is no upload (`kill_log::lines`).
pub fn read_log(path: &Path) -> Vec<Logged> {
    super::kill_log::lines(path)
        .iter()
        .filter_map(|line| {
            let mut path = None;
            let mut direct_path = None;
            let mut sha256 = None;
            for word in line.strip_prefix("upload ")?.split(' ') {
                match word.split_once('=')? {
                    ("path", v) => path = Some(v.to_owned()),
                    ("direct_path", v) => direct_path = Some(v.to_owned()),
                    ("sha256", v) => sha256 = Some(v.to_owned()),
                    _ => return None,
                }
            }
            Some(Logged {
                path: path?,
                direct_path: direct_path?,
                sha256: sha256?,
            })
        })
        .collect()
}

/// The sha256 of `bytes` as the log writes it.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

//! THE BLOB MIRROR (TH8): files and pictures of this account, fetched over the one pinned HTTPS client.
//!
//! The steps are Threema Desktop's (`common/dom/network/protocol/fetch-blob.ts`, `common/network/protocol/blob.ts`):
//! a GET of the mirror's URL with `deviceId`, `deviceGroupId` and `scope`, the body opened with XSalsa20-Poly1305 under
//! the key the message or the sync gave, and a POST to mark it done once it is safely kept.
//!
//! THE SCOPES. A file of a message is asked for with `public` (the mirror first, then the public server). A picture of
//! a sync is `local`. A blob is only ever marked done with `local`: that says this device has it and never takes
//! anything off the public server, where another member of a group may still need it. The mark is best effort.
//!
//! A FILE IS HELD WHOLE: a box has its tag first, and the HTTPS seam reads a whole body, so up to [`MAX_FILE`] is in
//! memory for a moment ([`SLOTS`] at once). What reaches the disk is sealed under the key the core chose
//! (`kourier_blob`) and appears at its target only when it is whole.
//!
//! Counts and kinds only in the log: no id, no key, no URL.

use std::io::Write as _;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use crypto_secretbox::aead::Aead as _;
use crypto_secretbox::{KeyInit as _, Nonce, XSalsa20Poly1305};
use kourier_backend_api::{BackendEvent, BlobTarget, Clock, Envelope};
use kourier_blob::{BlobWriter, FileKey};
use kourier_db::ZoneDb;
use kourier_model::{
    AttachmentId, AttachmentMeta, BackendBlob, BackendError, DownloadId, ErrorKind,
};
use serde_json::Value;
use tokio::sync::{Notify, Semaphore, mpsc};

use crate::parts::{HttpsClient, HttpsMethod, HttpsRequest, ServerParams};
use crate::{outbox, store};

/// The length of a blob id.
pub(crate) const ID_LEN: usize = 16;
/// The length of a blob's key.
pub(crate) const KEY_LEN: usize = 32;
/// The length of a blob's nonce.
pub(crate) const NONCE_LEN: usize = 24;
/// The tag in front of every box.
const TAG_LEN: usize = 16;
/// The nonce of a file's and of a picture's blob, when the sync names none (Desktop's `BLOB_FILE_NONCE`).
pub(crate) const FILE_NONCE: [u8; NONCE_LEN] = {
    let mut nonce = [0; NONCE_LEN];
    nonce[NONCE_LEN - 1] = 1;
    nonce
};
/// The nonce of a thumbnail's blob (Desktop's `BLOB_THUMBNAIL_NONCE`).
pub(crate) const THUMB_NONCE: [u8; NONCE_LEN] = {
    let mut nonce = [0; NONCE_LEN];
    nonce[NONCE_LEN - 1] = 2;
    nonce
};
/// The largest file Threema carries: 100 MiB.
pub(crate) const MAX_FILE: usize = 100 * 1024 * 1024;
/// How many files are fetched at once.
pub(crate) const SLOTS: usize = 3;
/// How long one file may take.
const WITHIN: Duration = Duration::from_secs(15 * 60);
/// How long a done mark may take: it is best effort, and a walk waits for it.
const DONE_WITHIN: Duration = Duration::from_secs(30);

/// Where a blob is looked for, and whom its done mark concerns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    /// The device group's mirror, then the public server.
    Public,
    /// The device group's mirror only.
    Local,
}

impl Scope {
    fn word(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Local => "local",
        }
    }
}

/// Why a blob did not come.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The servers no longer have it.
    Gone,
    /// The servers were not reached or did not answer: it may come later.
    Unreachable,
}

/// The mirror as this device talks to it.
#[derive(Clone)]
pub(crate) struct Mirror {
    https: Arc<dyn HttpsClient>,
    clock: Arc<dyn Clock>,
    download: String,
    done: String,
    upload: String,
    /// The public half of the device group's path key.
    group: [u8; 32],
    device: u64,
    user_agent: String,
}

impl Mirror {
    /// The mirror of `identity`. Makes no request.
    pub(crate) fn new(
        https: Arc<dyn HttpsClient>,
        clock: Arc<dyn Clock>,
        server: &ServerParams,
        identity: &store::Identity,
    ) -> Self {
        let group_key = libthreema::common::keys::DeviceGroupKey::from(*identity.device_group_key);
        Self {
            https,
            clock,
            download: server.blob_url.clone(),
            done: server.blob_done_url.clone(),
            upload: server.blob_upload_url.clone(),
            group: group_key.path_public_key(),
            device: identity.d2m_device_id,
            user_agent: crate::conn::user_agent(),
        }
    }

    fn query(&self, scope: Scope) -> String {
        format!(
            "?deviceId={}&deviceGroupId={}&scope={}",
            hex(&self.device.to_le_bytes()),
            hex(&self.group),
            scope.word()
        )
    }

    /// The blob's box, at most `limit` bytes of plaintext.
    pub(crate) async fn fetch(
        &self,
        id: &[u8; ID_LEN],
        scope: Scope,
        limit: usize,
    ) -> Result<Vec<u8>, Failure> {
        let request = HttpsRequest {
            method: HttpsMethod::Get,
            url: format!(
                "{}{}",
                expand(&self.download, &self.group, id),
                self.query(scope)
            ),
            headers: vec![
                ("user-agent".to_owned(), self.user_agent.clone()),
                ("accept".to_owned(), "application/octet-stream".to_owned()),
            ],
            body: Vec::new(),
            max_body: Some(limit.saturating_add(TAG_LEN)),
        };
        let response = tokio::select! {
            biased;
            answer = self.https.execute(request) => answer,
            () = self.clock.sleep(WITHIN) => return Err(Failure::Unreachable),
        }
        .map_err(|_| Failure::Unreachable)?;
        match response.status {
            200 => Ok(response.body),
            404 => Err(Failure::Gone),
            _ => Err(Failure::Unreachable),
        }
    }

    /// Says this device has the blob (module doc). Best effort: a blob not marked expires on the server by itself.
    pub(crate) async fn mark_done(&self, id: &[u8; ID_LEN]) {
        let request = HttpsRequest {
            method: HttpsMethod::Post,
            url: format!(
                "{}{}",
                expand(&self.done, &self.group, id),
                self.query(Scope::Local)
            ),
            headers: vec![("user-agent".to_owned(), self.user_agent.clone())],
            body: Vec::new(),
            max_body: Some(1024),
        };
        let answer = tokio::select! {
            biased;
            answer = self.https.execute(request) => answer,
            () = self.clock.sleep(DONE_WITHIN) => return,
        };
        match answer {
            Ok(response) if response.status == 204 => {}
            Ok(response) => tracing::debug!(status = response.status, "a blob was not marked done"),
            Err(_) => tracing::debug!("a blob could not be marked done"),
        }
    }
}

impl Mirror {
    /// Uploads `boxed` for an outgoing message; answers the blob's id. `persist`: Desktop's `public-persistent` (a group's
    /// blob), otherwise `public-volatile`.
    pub(crate) async fn upload(
        &self,
        boxed: Vec<u8>,
        persist: bool,
    ) -> Result<[u8; ID_LEN], BackendError> {
        let boundary = format!("kourier{}", hex(&rand::random::<[u8; 16]>()));
        let head = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"blob\"; filename=\"blob\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n"
        );
        let tail = format!("\r\n--{boundary}--\r\n");
        let mut body = Vec::with_capacity(head.len() + boxed.len() + tail.len());
        body.extend_from_slice(head.as_bytes());
        body.extend_from_slice(&boxed);
        drop(boxed);
        body.extend_from_slice(tail.as_bytes());
        let request = HttpsRequest {
            method: HttpsMethod::Post,
            url: format!(
                "{}?persist={}{}",
                expand(&self.upload, &self.group, &[0; ID_LEN]),
                u8::from(persist),
                self.query(Scope::Public).replacen('?', "&", 1),
            ),
            headers: vec![
                ("user-agent".to_owned(), self.user_agent.clone()),
                ("accept".to_owned(), "text/plain".to_owned()),
                (
                    "content-type".to_owned(),
                    format!("multipart/form-data; boundary={boundary}"),
                ),
            ],
            body,
            max_body: Some(1024),
        };
        let answer = tokio::select! {
            biased;
            answer = self.https.execute(request) => answer,
            () = self.clock.sleep(WITHIN) => {
                return Err(BackendError::transient("the upload took too long"));
            }
        }
        .map_err(|_| BackendError::transient("the blob server was not reached"))?;
        match answer.status {
            200 => std::str::from_utf8(&answer.body)
                .ok()
                .and_then(|text| unhex(text.trim()))
                .ok_or_else(|| BackendError::internal("the blob server's answer is no blob id")),
            413 => Err(BackendError::new(
                ErrorKind::TooLarge,
                "the blob server refused the file's size",
            )),
            status if status == 408 || status == 429 || status >= 500 => Err(
                BackendError::transient("the blob server is not taking files"),
            ),
            _ => Err(BackendError::internal("the blob server refused the file")),
        }
    }
}

/// `template` with its placeholders filled (libthreema's forms: `common/config.rs`).
fn expand(template: &str, group: &[u8; 32], blob: &[u8; ID_LEN]) -> String {
    template
        .replace("{deviceGroupIdPrefix4}", &format!("{:x}", group[0] >> 4))
        .replace("{deviceGroupIdPrefix8}", &format!("{:02x}", group[0]))
        .replace("{blobIdPrefix}", &format!("{:02x}", blob[0]))
        .replace("{blobId}", &hex(blob))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}

fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 || !text.is_ascii() {
        return None;
    }
    let mut out = [0; N];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// `boxed` opened; `None` when it is not what its key says.
pub(crate) fn open(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN], boxed: &[u8]) -> Option<Vec<u8>> {
    XSalsa20Poly1305::new(key.into())
        .decrypt(Nonce::from_slice(nonce), boxed)
        .ok()
}

/// `plain` boxed; `None` when it cannot be (a length no box takes).
pub(crate) fn seal(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN], plain: &[u8]) -> Option<Vec<u8>> {
    XSalsa20Poly1305::new(key.into())
        .encrypt(Nonce::from_slice(nonce), plain)
        .ok()
}

/// What the actor hands a download.
#[derive(Clone)]
pub(crate) struct Fetcher {
    pub(crate) mirror: Mirror,
    pub(crate) db: Rc<ZoneDb>,
    pub(crate) events: mpsc::Sender<Envelope>,
    pub(crate) wake: Rc<Notify>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) slots: Rc<Semaphore>,
}

/// One requested file: fetched, opened and sealed at `dest`, and answered once, durably.
pub(crate) async fn run(
    fetcher: Fetcher,
    attachment: AttachmentId,
    request: DownloadId,
    remote: BackendBlob,
    dest: BlobTarget,
) {
    let result = fetch(&fetcher, attachment, &remote, dest).await;
    if let Err(error) = &result {
        tracing::warn!(kind = ?error.kind, "a file did not download");
    }
    answer(
        &fetcher.db,
        &*fetcher.clock,
        &fetcher.wake,
        attachment,
        request,
        result,
    );
}

/// The one durable answer to a download request.
pub(crate) fn answer(
    db: &ZoneDb,
    clock: &dyn Clock,
    wake: &Rc<Notify>,
    attachment: AttachmentId,
    request: DownloadId,
    result: Result<AttachmentMeta, BackendError>,
) {
    let answer = BackendEvent::AttachmentDone {
        attachment,
        request: Some(request),
        result,
    };
    let now = i64::try_from(clock.now_ms()).unwrap_or(i64::MAX);
    let appended =
        db.zone(|tx| outbox::append_events(tx, std::slice::from_ref(&answer), now, wake));
    if appended.is_err() {
        tracing::warn!("a download request could not be answered");
    }
}

async fn fetch(
    fetcher: &Fetcher,
    attachment: AttachmentId,
    remote: &BackendBlob,
    dest: BlobTarget,
) -> Result<AttachmentMeta, BackendError> {
    let pointer = pointer_of(remote)
        .ok_or_else(|| BackendError::new(ErrorKind::InvalidInput, "no pointer"))?;
    let _slot = fetcher
        .slots
        .acquire()
        .await
        .map_err(|_| BackendError::transient("the account is stopping"))?;
    report(&fetcher.events, attachment, 0, pointer.size);
    let boxed = fetcher
        .mirror
        .fetch(&pointer.blob, Scope::Public, MAX_FILE)
        .await
        .map_err(|failure| match failure {
            Failure::Gone => BackendError::new(ErrorKind::Expired, "the servers no longer have it"),
            Failure::Unreachable => BackendError::transient("the servers were not reached"),
        })?;
    let key = pointer.key;
    let size = tokio::task::spawn_blocking(move || seal_at(&key, &boxed, dest))
        .await
        .map_err(|_| BackendError::internal("the file could not be kept"))??;
    report(&fetcher.events, attachment, size, size);
    fetcher.mirror.mark_done(&pointer.blob).await;
    Ok(AttachmentMeta {
        size,
        mime: pointer.mime,
        dims: pointer.dims,
        duration_ms: pointer.duration_ms,
    })
}

fn report(events: &mpsc::Sender<Envelope>, attachment: AttachmentId, done: u64, total: u64) {
    let _ = events.try_send(Envelope::ephemeral(BackendEvent::DownloadProgress {
        attachment,
        done,
        total,
    }));
}

/// Opens `boxed` and seals the plaintext at `dest`; answers its size.
fn seal_at(key: &[u8; KEY_LEN], boxed: &[u8], dest: BlobTarget) -> Result<u64, BackendError> {
    let plain = open(key, &FILE_NONCE, boxed)
        .ok_or_else(|| BackendError::new(ErrorKind::InvalidInput, "the file does not open"))?;
    let file_key = FileKey::from_bytes(*dest.key.expose());
    let internal = |what: &'static str| BackendError::internal(what);
    let mut writer = BlobWriter::create(&dest.path, &file_key)
        .map_err(|_| internal("the file could not be opened"))?;
    writer
        .write_all(&plain)
        .map_err(|_| internal("the file could not be written"))?;
    writer
        .finish()
        .map_err(|_| internal("the file could not be finished"))?;
    Ok(plain.len() as u64)
}

/// What a file message's JSON says of its file.
struct Pointer {
    blob: [u8; ID_LEN],
    key: [u8; KEY_LEN],
    size: u64,
    mime: Option<String>,
    dims: Option<(u32, u32)>,
    duration_ms: Option<u32>,
}

/// The pointer of an attachment as `reflect::body::file` kept it.
fn pointer_of(remote: &BackendBlob) -> Option<Pointer> {
    let parsed = crate::reflect::body::file(&remote.bytes, "", false)?;
    let json: Value = serde_json::from_slice(&remote.bytes).ok()?;
    let text = |key: &str| json.get(key).and_then(Value::as_str);
    Some(Pointer {
        blob: unhex(text("b")?)?,
        key: unhex(text("k")?)?,
        size: parsed.attachment.size,
        mime: Some(parsed.attachment.mime),
        dims: parsed.attachment.dims,
        duration_ms: parsed.attachment.duration_ms,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    use super::*;
    use crate::parts::{HttpsResponse, NetError};
    use async_trait::async_trait;

    /// A mirror of the test's making: answers are found by the URL's blob id, every request is kept.
    #[derive(Default)]
    pub(crate) struct FakeMirror {
        pub(crate) blobs: Mutex<HashMap<String, Vec<u8>>>,
        pub(crate) failing: Mutex<VecDeque<u16>>,
        pub(crate) asked: Mutex<Vec<(HttpsMethod, String)>>,
        /// The boxes that went up, out of their multipart bodies.
        pub(crate) up: Mutex<Vec<Vec<u8>>>,
        /// What the next upload is answered, when not a fresh id.
        pub(crate) refuse_upload: Mutex<Option<u16>>,
    }

    impl FakeMirror {
        pub(crate) fn uploaded(&self) -> Vec<Vec<u8>> {
            self.up.lock().expect("lock").clone()
        }

        pub(crate) fn upload_answers(&self, status: u16) {
            *self.refuse_upload.lock().expect("lock") = Some(status);
        }

        pub(crate) fn urls(&self) -> Vec<String> {
            self.asked
                .lock()
                .expect("lock")
                .iter()
                .map(|(_, url)| url.clone())
                .collect()
        }
    }

    #[async_trait]
    impl HttpsClient for FakeMirror {
        async fn execute(&self, request: HttpsRequest) -> Result<HttpsResponse, NetError> {
            self.asked
                .lock()
                .expect("lock")
                .push((request.method, request.url.clone()));
            let answer = |status, body| {
                Ok(HttpsResponse {
                    status,
                    headers: Vec::new(),
                    body,
                })
            };
            if request.method == HttpsMethod::Post && request.url.contains("/upload") {
                if let Some(status) = self.refuse_upload.lock().expect("lock").take() {
                    return answer(status, Vec::new());
                }
                let mut up = self.up.lock().expect("lock");
                up.push(multipart_content(&request.body));
                return answer(200, hex(&[up.len() as u8; ID_LEN]).into_bytes());
            }
            if request.method == HttpsMethod::Post {
                return answer(204, Vec::new());
            }
            if let Some(status) = self.failing.lock().expect("lock").pop_front() {
                return answer(status, Vec::new());
            }
            let blobs = self.blobs.lock().expect("lock");
            match blobs
                .iter()
                .find(|(id, _)| request.url.contains(id.as_str()))
            {
                Some((_, body)) => answer(200, body.clone()),
                None => answer(404, Vec::new()),
            }
        }
    }

    pub(crate) fn sealed(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN], plain: &[u8]) -> Vec<u8> {
        seal(key, nonce, plain).expect("seal")
    }

    pub(crate) fn unhexed<const N: usize>(text: &str) -> [u8; N] {
        unhex(text).expect("hex")
    }

    /// What is between a multipart body's head and its last boundary.
    fn multipart_content(body: &[u8]) -> Vec<u8> {
        let find = |needle: &[u8], from: usize| {
            body[from..]
                .windows(needle.len())
                .position(|w| w == needle)
                .map(|at| at + from)
                .expect("a multipart body")
        };
        let start = find(b"\r\n\r\n", 0) + 4;
        let boundary = &body[..find(b"\r\n", 0)];
        let mut last = Vec::with_capacity(boundary.len() + 2);
        last.extend_from_slice(b"\r\n");
        last.extend_from_slice(boundary);
        body[start..find(&last, start)].to_vec()
    }

    pub(crate) fn mirror_of(https: Arc<FakeMirror>, clock: Arc<dyn Clock>) -> Mirror {
        Mirror {
            https,
            clock,
            download: "https://blob-mirror-{deviceGroupIdPrefix4}.invalid/{deviceGroupIdPrefix8}/{blobId}".to_owned(),
            done: "https://blob-mirror-{deviceGroupIdPrefix4}.invalid/{deviceGroupIdPrefix8}/{blobId}/done".to_owned(),
            upload: "https://blob-mirror-{deviceGroupIdPrefix4}.invalid/{deviceGroupIdPrefix8}/upload".to_owned(),
            group: [0xA7; 32],
            device: 0x0102_0304_0506_0708,
            user_agent: "test".to_owned(),
        }
    }

    #[test]
    fn a_url_is_filled_the_way_desktop_fills_it() {
        let blob = [0x0C; ID_LEN];
        let url = expand(
            "https://blob-mirror-{deviceGroupIdPrefix4}.mirror.test/{deviceGroupIdPrefix8}/{blobId}",
            &[0x0A; 32],
            &blob,
        );
        assert_eq!(
            url,
            format!("https://blob-mirror-0.mirror.test/0a/{}", "0c".repeat(16))
        );
    }

    #[test]
    fn the_device_id_is_little_endian_in_the_query() {
        let clock: Arc<dyn Clock> = Arc::new(kourier_backend_api::FakeClock::new(1));
        let mirror = mirror_of(Arc::new(FakeMirror::default()), clock);
        let query = mirror.query(Scope::Local);
        assert!(query.starts_with("?deviceId=0807060504030201&"), "{query}");
        assert!(query.ends_with("&scope=local"), "{query}");
    }

    #[test]
    fn a_box_opens_only_under_its_key() {
        let key = [3; KEY_LEN];
        let boxed = sealed(&key, &FILE_NONCE, b"a file");
        assert_eq!(
            open(&key, &FILE_NONCE, &boxed).as_deref(),
            Some(&b"a file"[..])
        );
        assert!(open(&[4; KEY_LEN], &FILE_NONCE, &boxed).is_none());
        let mut tampered = boxed;
        tampered[20] ^= 1;
        assert!(open(&key, &FILE_NONCE, &tampered).is_none());
    }

    #[test]
    fn a_pointer_needs_its_blob_and_key() {
        let whole = serde_json::json!({
            "b": "0c".repeat(16), "k": "0d".repeat(32), "m": "image/jpeg", "s": 6, "j": 1,
        })
        .to_string()
        .into_bytes();
        let pointer = pointer_of(&BackendBlob::new(1, whole)).expect("a pointer");
        assert_eq!(pointer.blob, [0x0C; ID_LEN]);
        assert_eq!(pointer.key, [0x0D; KEY_LEN]);
        assert_eq!(pointer.mime.as_deref(), Some("image/jpeg"));
        assert!(pointer_of(&BackendBlob::new(1, b"{}".to_vec())).is_none());
    }
}

//! Attachment uploads (§6.7 step 2; `SD/ts/util/uploadAttachment.preload.ts`,
//! `SD/ts/util/uploads/tusProtocol.node.ts`, `SD/ts/textsecure/WebAPI.preload.ts`
//! `putEncryptedAttachment`).
//!
//! One upload: 64 fresh key bytes and an IV; an upload form for the blob's length through the
//! typed chat API (gRPC where the server offers it); then the blob, produced by a blocking task
//! that reads the plaintext (memory, or a core blob through `kourier-blob`), encrypts it
//! ([`Encryptor`]) and feeds a bounded channel that is the request body. Because key and IV are
//! fixed per upload, a resumed upload replays the same blob from any offset:
//!
//! - CDN 3 (TUS 1.0.0): `POST` creation-with-upload; if the connection drops, up to three times
//!   `HEAD {location}/{key}` for `Upload-Offset` and `PATCH` from there. HTTP errors are final
//!   (Desktop throws on them as well).
//! - CDN 2 (GCS resumable): `POST` with the form's headers for the session `Location`, then
//!   `PUT` with `Content-Range` alone (the session URL is the credential; Desktop sends nothing
//!   else either); on a failure `PUT bytes */{len}` asks how far it got (`308` +
//!   `Range: bytes=0-N`) and the next `PUT` resumes at `N + 1`; at most ten attempts.
//!
//! A request that carries the blob has a whole-request timeout that grows with the blob
//! ([`transfer_timeout`]), so a slow link gets the time a large file needs. That timeout alone
//! would let a dead connection hold an upload for ten minutes and more, so the request is
//! also watched ([`STALL_TIMEOUT`], `Net::https_upload_watched`): when no byte of the blob
//! moves for a minute, the request is dropped and the upload resumes like one whose
//! connection broke (CDN 3: `HEAD` for the offset, then `PATCH`; CDN 2: the status query,
//! then `PUT` from there). What moves is counted where the body is produced, so the last
//! [`QUEUE`] chunks, and an upload no larger than that as a whole, get five minutes
//! (`net::runner::UPLOAD_TAIL_STALLS`): nothing counts them once they are queued.
//!
//! The pointer of every upload is kept in `attachment_uploads` for three days, keyed by what was
//! uploaded, so a retried `Transmit` (or a resend) reuses it instead of uploading again (Desktop
//! reuses by plaintext hash for the same three days). Progress is counted in blob bytes sent.

use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use kourier_db::rusqlite::{Connection, OptionalExtension as _};
use kourier_model::{BackendError, BlobKey, ErrorKind};
use prost::Message as _;
use rand::RngCore as _;
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::attachment::{
    AttachmentError, AttachmentKeys, Encrypted, Encryptor, KEY_LEN, max_blob_len,
    supports_incremental_mac, transfer_timeout,
};
use crate::error::Result;
use crate::net::{
    HttpsResponse, Net, SignalTransport, TransportError, UploadBody, UploadForm, UploadRequest,
};
use crate::proto;

/// Plaintext read per step of the producer.
const CHUNK: usize = 64 * 1024;
/// Blob chunks waiting for the network (the producer pauses when the request is slow).
const QUEUE: usize = 8;
/// Resume attempts of a TUS upload after the first request (Desktop's `DEFAULT_MAX_RETRIES`).
const TUS_RETRIES: u32 = 3;
/// `PUT` attempts of a GCS resumable upload (`putEncryptedAttachment`'s `MAX_RETRIES`).
const GCS_ATTEMPTS: u32 = 10;
/// Timeout of the small requests (offset queries, session creation).
const SMALL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an upload may move no byte of its blob before it counts as stalled. A minute, as
/// for a download that receives nothing (`net::https` `DOWNLOAD_IDLE_TIMEOUT`); the slowest
/// link the transfer timeout plans for (16 KiB/s) still moves a 64 KiB chunk every four
/// seconds.
pub(crate) const STALL_TIMEOUT: Duration = Duration::from_secs(60);
/// How long an uploaded pointer is reused (`MAX_DURATION_TO_REUSE_ATTACHMENT_CDN_POINTER`).
pub(crate) const REUSE_MS: i64 = 3 * 24 * 3600 * 1000;
/// An upload timestamp further in the future than this is not trusted for reuse.
const FUTURE_SLACK_MS: i64 = 12 * 3600 * 1000;

/// Why an upload failed. Log-safe.
#[derive(Debug, thiserror::Error)]
pub(crate) enum UploadError {
    /// The server does not take a blob this large (413 on the upload form).
    #[error("the attachment is larger than the server accepts")]
    TooLarge,
    /// The network failed.
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// The CDN answered with this status.
    #[error("CDN answered {0}")]
    Status(u16),
    /// The upload form names a CDN Kourier cannot upload to.
    #[error("uploads to CDN {0} are not supported")]
    UnsupportedCdn(u32),
    /// The plaintext could not be read or did not have its stated size.
    #[error(transparent)]
    Crypto(#[from] AttachmentError),
    /// Every attempt broke off before the CDN had the whole blob.
    #[error("the upload did not complete")]
    Incomplete,
    /// `backend.db` failed.
    #[error("backend.db: {0}")]
    Db(String),
}

impl From<crate::error::Error> for UploadError {
    fn from(e: crate::error::Error) -> Self {
        Self::Db(e.to_string())
    }
}

impl UploadError {
    /// The contract error of a send that failed on this upload.
    pub(crate) fn to_backend_error(&self) -> BackendError {
        match self {
            Self::TooLarge | Self::Status(413) => {
                BackendError::new(ErrorKind::TooLarge, self.to_string())
            }
            Self::Transport(e) => crate::error::Error::Transport(e.clone()).to_backend_error(),
            Self::Status(429) => BackendError::new(
                ErrorKind::RateLimited { retry_after: None },
                self.to_string(),
            ),
            // A CDN refusal (an expired signature, a lost session) is cured by a new form.
            Self::Status(_) | Self::Incomplete => BackendError::transient(self.to_string()),
            Self::UnsupportedCdn(_) => BackendError::unsupported(self.to_string()),
            Self::Crypto(AttachmentError::Sink(_)) => BackendError::new(
                ErrorKind::InvalidInput,
                "the file to send could not be read",
            ),
            Self::Crypto(_) => BackendError::internal(self.to_string()),
            Self::Db(_) => BackendError::internal(self.to_string()),
        }
    }
}

/// What an upload encrypts.
#[derive(Clone)]
pub(crate) enum Plaintext {
    /// Bytes in memory (a re-encoded image, a long text), zeroized when the last copy goes.
    Memory(Arc<Zeroizing<Vec<u8>>>),
    /// A `kourier-blob` file of `size` plaintext bytes (a core attachment, read in place).
    Blob {
        /// Absolute path.
        path: PathBuf,
        /// Its key.
        key: BlobKey,
        /// Plaintext length (the attachment's `size`; checked while encrypting).
        size: u64,
    },
}

impl Plaintext {
    /// Plaintext length.
    pub(crate) fn len(&self) -> u64 {
        match self {
            Self::Memory(bytes) => bytes.len() as u64,
            Self::Blob { size, .. } => *size,
        }
    }

    fn reader(&self) -> std::result::Result<Box<dyn std::io::Read + '_>, AttachmentError> {
        Ok(match self {
            Self::Memory(bytes) => Box::new(std::io::Cursor::new(bytes.as_slice())),
            Self::Blob { path, key, .. } => Box::new(
                kourier_blob::BlobReader::open(
                    path,
                    &kourier_blob::FileKey::from_bytes(*key.expose()),
                )
                .map_err(|e| AttachmentError::Sink(e.to_string()))?,
            ),
        })
    }
}

impl std::fmt::Debug for Plaintext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            Self::Memory(_) => "memory",
            Self::Blob { .. } => "blob",
        };
        f.debug_struct("Plaintext")
            .field("kind", &kind)
            .field("len", &self.len())
            .finish()
    }
}

/// The secrets of one upload: the pointer's key and the IV. A resumed request replays them.
struct Secret {
    key: Zeroizing<[u8; KEY_LEN]>,
    iv: [u8; 16],
}

impl Secret {
    fn random() -> Self {
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        let mut iv = [0u8; 16];
        let mut rng = rand::rng();
        rng.fill_bytes(&mut key[..]);
        rng.fill_bytes(&mut iv);
        Self { key, iv }
    }
}

/// One blob on its way to the CDN.
struct Upload {
    form: UploadForm,
    plain: Plaintext,
    secret: Arc<Secret>,
    incremental: bool,
    blob_len: u64,
    sent: Arc<AtomicU64>,
    /// Body bytes the requests of this upload have taken (the stall watch looks at it; `sent`
    /// is shared by every upload of the message).
    moved: Arc<AtomicU64>,
    /// How long no byte may move before the request counts as stalled.
    stall: Duration,
}

type Producer = tokio::task::JoinHandle<std::result::Result<Encrypted, AttachmentError>>;

impl Upload {
    /// The blob from `offset` on as a request body, produced by a blocking task, and the
    /// value `moved` has once every byte of that body is handed over (read before the task
    /// starts, which may hand over a small body before the request is even sent).
    fn body(&self, offset: u64) -> (UploadBody, Producer, u64) {
        let goal = self
            .moved
            .load(Ordering::Relaxed)
            .saturating_add(self.blob_len.saturating_sub(offset));
        let (tx, chunks) = mpsc::channel(QUEUE);
        let plain = self.plain.clone();
        let secret = self.secret.clone();
        let incremental = self.incremental;
        let sent = self.sent.clone();
        let moved = self.moved.clone();
        let producer = tokio::task::spawn_blocking(move || {
            produce(
                &plain,
                &secret,
                incremental,
                offset,
                Some(&tx),
                &[&sent, &moved],
            )
        });
        let body = UploadBody {
            len: self.blob_len.saturating_sub(offset),
            chunks,
        };
        (body, producer, goal)
    }

    /// Runs the encryption without sending anything (the CDN already had every byte when a
    /// resumed request found out): its digests are still needed for the pointer.
    async fn digests_only(&self) -> std::result::Result<Encrypted, UploadError> {
        let plain = self.plain.clone();
        let secret = self.secret.clone();
        let incremental = self.incremental;
        let sent = self.sent.clone();
        let blob_len = self.blob_len;
        tokio::task::spawn_blocking(move || {
            produce(&plain, &secret, incremental, blob_len, None, &[&sent])
        })
        .await
        .map_err(|_| AttachmentError::Sink("the encryption task stopped".to_owned()))?
        .map_err(UploadError::from)
    }

    /// Sends a request that carries a [body](Self::body) of the blob, watched for a stall;
    /// `goal` is that body's.
    async fn send_blob<T: SignalTransport>(
        &self,
        net: &Net<T>,
        request: UploadRequest,
        goal: u64,
    ) -> std::result::Result<HttpsResponse, TransportError> {
        net.https_upload_watched(request, self.moved.clone(), goal, self.stall)
            .await
    }

    /// The form's headers plus `extra`.
    fn headers(
        &self,
        extra: &[(HeaderName, String)],
    ) -> std::result::Result<HeaderMap, UploadError> {
        let mut headers = HeaderMap::new();
        for (name, value) in &self.form.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| TransportError::InvalidRequest("upload form header name"))?;
            let mut value = HeaderValue::from_str(value)
                .map_err(|_| TransportError::InvalidRequest("upload form header value"))?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        for (name, value) in extra {
            let value = HeaderValue::from_str(value)
                .map_err(|_| TransportError::InvalidRequest("upload header value"))?;
            headers.insert(name.clone(), value);
        }
        Ok(headers)
    }
}

/// Blocking: encrypts `plain` and sends the blob from `offset` on into `tx` (none: digests
/// only); counts the bytes sent into each of `sent`.
fn produce(
    plain: &Plaintext,
    secret: &Secret,
    incremental: bool,
    offset: u64,
    tx: Option<&mpsc::Sender<std::io::Result<Bytes>>>,
    sent: &[&AtomicU64],
) -> std::result::Result<Encrypted, AttachmentError> {
    let keys = AttachmentKeys::from_slice(&secret.key[..])?;
    let mut encryptor = Encryptor::new(&keys, secret.iv, plain.len(), incremental)?;
    let mut reader = plain.reader()?;
    let mut buf = Zeroizing::new(vec![0u8; CHUNK]);
    let mut out = Vec::with_capacity(CHUNK + 64);
    let mut position = 0u64;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| AttachmentError::Sink(e.kind().to_string()))?;
        if n == 0 {
            break;
        }
        encryptor.update(&buf[..n], &mut out)?;
        position = emit(&mut out, position, offset, tx, sent)?;
    }
    let encrypted = encryptor.finish(&mut out)?;
    emit(&mut out, position, offset, tx, sent)?;
    Ok(encrypted)
}

/// Sends the part of `out` at or after `offset` and clears it; returns the new position.
fn emit(
    out: &mut Vec<u8>,
    position: u64,
    offset: u64,
    tx: Option<&mpsc::Sender<std::io::Result<Bytes>>>,
    sent: &[&AtomicU64],
) -> std::result::Result<u64, AttachmentError> {
    let end = position + out.len() as u64;
    if let Some(tx) = tx
        && end > offset
    {
        let skip = usize::try_from(offset.saturating_sub(position)).unwrap_or(usize::MAX);
        let bytes = Bytes::copy_from_slice(&out[skip.min(out.len())..]);
        let len = bytes.len() as u64;
        tx.blocking_send(Ok(bytes))
            .map_err(|_| AttachmentError::Stopped)?;
        for counter in sent {
            counter.fetch_add(len, Ordering::Relaxed);
        }
    }
    out.clear();
    Ok(end)
}

/// Waits for a producer; its error matters only when the request did not fail on its own.
async fn finished(producer: Producer) -> std::result::Result<Encrypted, UploadError> {
    producer
        .await
        .map_err(|_| AttachmentError::Sink("the encryption task stopped".to_owned()))?
        .map_err(UploadError::from)
}

/// Whether a producer stopped only because its request went away.
fn request_ended(result: &std::result::Result<Encrypted, UploadError>) -> bool {
    matches!(result, Err(UploadError::Crypto(AttachmentError::Stopped)))
}

/// `{location}/{key}`, the TUS resource of the upload.
fn tus_resource(form: &UploadForm) -> String {
    format!(
        "{}/{}",
        form.signed_upload_url.trim_end_matches('/'),
        form.key
    )
}

fn status_error(response: &HttpsResponse) -> UploadError {
    UploadError::Status(response.status.as_u16())
}

/// CDN 3: TUS creation-with-upload, resumed with `HEAD` + `PATCH` when the connection drops.
async fn tus<T: SignalTransport>(
    net: &Net<T>,
    upload: &Upload,
) -> std::result::Result<Encrypted, UploadError> {
    let tus_resumable = HeaderName::from_static("tus-resumable");
    let octets = "application/offset+octet-stream".to_owned();
    let metadata = format!("filename {}", STANDARD.encode(upload.form.key.as_bytes()));
    let headers = upload.headers(&[
        (tus_resumable.clone(), "1.0.0".to_owned()),
        (
            HeaderName::from_static("upload-length"),
            upload.blob_len.to_string(),
        ),
        (HeaderName::from_static("upload-metadata"), metadata),
        (http::header::CONTENT_TYPE, octets.clone()),
    ])?;
    let (body, producer, goal) = upload.body(0);
    let sent = upload
        .send_blob(
            net,
            UploadRequest {
                method: Method::POST,
                url: upload.form.signed_upload_url.clone(),
                headers,
                body: Some(body),
                timeout: transfer_timeout(upload.blob_len),
            },
            goal,
        )
        .await;
    let produced = finished(producer).await;
    match sent {
        Ok(response) if response.status.is_success() => return produced,
        Ok(response) => return Err(status_error(&response)),
        Err(e) if !request_ended(&produced) && produced.is_err() => {
            tracing::warn!(error = %e, "attachment upload stopped by its source");
            return produced;
        }
        Err(e) => tracing::warn!(error = %e, "attachment upload broke off; resuming"),
    }
    let resource = tus_resource(&upload.form);
    for attempt in 1..=TUS_RETRIES {
        tokio::time::sleep(Duration::from_secs(u64::from(attempt))).await;
        let head = net
            .https_upload(UploadRequest {
                method: Method::HEAD,
                url: resource.clone(),
                headers: upload.headers(&[(tus_resumable.clone(), "1.0.0".to_owned())])?,
                body: None,
                timeout: SMALL_TIMEOUT,
            })
            .await?;
        if !head.status.is_success() {
            return Err(status_error(&head));
        }
        let offset = head
            .headers
            .get("upload-offset")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|offset| *offset <= upload.blob_len)
            .ok_or_else(|| TransportError::InvalidResponse("TUS offset".to_owned()))?;
        if offset == upload.blob_len {
            return upload.digests_only().await;
        }
        let headers = upload.headers(&[
            (tus_resumable.clone(), "1.0.0".to_owned()),
            (HeaderName::from_static("upload-offset"), offset.to_string()),
            (http::header::CONTENT_TYPE, octets.clone()),
        ])?;
        let (body, producer, goal) = upload.body(offset);
        let sent = upload
            .send_blob(
                net,
                UploadRequest {
                    method: Method::PATCH,
                    url: resource.clone(),
                    headers,
                    body: Some(body),
                    timeout: transfer_timeout(upload.blob_len),
                },
                goal,
            )
            .await;
        let produced = finished(producer).await;
        match sent {
            Ok(response) if response.status.is_success() => return produced,
            Ok(response) => return Err(status_error(&response)),
            Err(_) if !request_ended(&produced) && produced.is_err() => return produced,
            Err(e) => tracing::warn!(error = %e, attempt, "attachment upload broke off again"),
        }
    }
    Err(UploadError::Incomplete)
}

/// CDN 2: a GCS resumable upload session.
async fn resumable<T: SignalTransport>(
    net: &Net<T>,
    upload: &Upload,
) -> std::result::Result<Encrypted, UploadError> {
    let created = net
        .https_upload(UploadRequest {
            method: Method::POST,
            url: upload.form.signed_upload_url.clone(),
            headers: upload.headers(&[])?,
            body: None,
            timeout: SMALL_TIMEOUT,
        })
        .await?;
    if !created.status.is_success() {
        return Err(status_error(&created));
    }
    let location = created
        .headers
        .get(http::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.starts_with("https://"))
        .ok_or_else(|| TransportError::InvalidResponse("upload session location".to_owned()))?
        .to_owned();
    let len = upload.blob_len;
    // The session's requests carry their range and nothing of the form.
    let content_range = |range: String| -> std::result::Result<HeaderMap, UploadError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_RANGE,
            HeaderValue::from_str(&range)
                .map_err(|_| TransportError::InvalidRequest("upload range"))?,
        );
        Ok(headers)
    };
    let mut start = 0u64;
    for attempt in 0..GCS_ATTEMPTS {
        let range = format!("bytes {start}-{}/{len}", len.saturating_sub(1));
        let (body, producer, goal) = upload.body(start);
        let sent = upload
            .send_blob(
                net,
                UploadRequest {
                    method: Method::PUT,
                    url: location.clone(),
                    headers: content_range(range)?,
                    body: Some(body),
                    timeout: transfer_timeout(upload.blob_len),
                },
                goal,
            )
            .await;
        let produced = finished(producer).await;
        match sent {
            Ok(response) if response.status.is_success() => return produced,
            Err(_) if !request_ended(&produced) && produced.is_err() => return produced,
            Ok(response) => {
                tracing::warn!(
                    status = response.status.as_u16(),
                    attempt,
                    "attachment PUT failed"
                );
            }
            Err(e) => tracing::warn!(error = %e, attempt, "attachment PUT broke off"),
        }
        // How much did the session get?
        let status = net
            .https_upload(UploadRequest {
                method: Method::PUT,
                url: location.clone(),
                headers: content_range(format!("bytes */{len}"))?,
                body: None,
                timeout: SMALL_TIMEOUT,
            })
            .await?;
        if status.status.is_success() {
            return upload.digests_only().await;
        }
        if status.status != StatusCode::PERMANENT_REDIRECT {
            return Err(status_error(&status));
        }
        // `Range: bytes=0-N`: bytes 0..=N arrived. No header: nothing did.
        start = match status
            .headers
            .get(http::header::RANGE)
            .and_then(|v| v.to_str().ok())
        {
            Some(range) => range
                .strip_prefix("bytes=0-")
                .and_then(|n| n.trim().parse::<u64>().ok())
                .map(|n| n.saturating_add(1))
                .filter(|next| *next <= len)
                .ok_or_else(|| TransportError::InvalidResponse("upload range".to_owned()))?,
            None => 0,
        };
    }
    Err(UploadError::Incomplete)
}

/// What one attachment upload needs.
#[derive(Debug)]
pub(crate) struct Spec {
    /// What to encrypt.
    pub(crate) plain: Plaintext,
    /// Content type (decides the incremental MAC).
    pub(crate) mime: String,
    /// Width and height of an image (stored with a reused pointer).
    pub(crate) dims: Option<(u32, u32)>,
    /// The cache key: SHA-256 of what is uploaded and how ([`source_key`]).
    pub(crate) source: [u8; 32],
}

/// The cache key of an upload: a domain tag, how the plaintext was made (`transform`) and what
/// identifies it (a core blob's path and key, or the text itself).
pub(crate) fn source_key(transform: &str, parts: &[&[u8]]) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"kourier-signal upload v1\0");
    hash.update(transform.as_bytes());
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    hash.finalize().into()
}

/// The pointer of a recent upload of `source`, if it may be reused at `now`.
pub(crate) fn cached(
    conn: &Connection,
    source: &[u8; 32],
    now: i64,
) -> Result<Option<proto::AttachmentPointer>> {
    let row: Option<(Vec<u8>, i64)> = conn
        .prepare_cached("SELECT pointer, uploaded_at FROM attachment_uploads WHERE source = ?1")?
        .query_row([&source[..]], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    Ok(row.and_then(|(bytes, at)| {
        let fresh = at > now.saturating_sub(REUSE_MS) && at < now.saturating_add(FUTURE_SLACK_MS);
        fresh
            .then(|| proto::AttachmentPointer::decode(&bytes[..]).ok())
            .flatten()
    }))
}

/// Remembers the pointer of an upload of `source`.
pub(crate) fn remember(
    conn: &Connection,
    source: &[u8; 32],
    pointer: &proto::AttachmentPointer,
    now: i64,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO attachment_uploads(source, pointer, uploaded_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(source) DO UPDATE SET pointer = excluded.pointer,
           uploaded_at = excluded.uploaded_at",
    )?
    .execute((&source[..], pointer.encode_to_vec(), now))?;
    Ok(())
}

/// Forgets pointers too old to reuse (hourly, with the Message Send Log purge).
pub(crate) fn purge(conn: &Connection, now: i64) -> Result<usize> {
    Ok(conn
        .prepare_cached("DELETE FROM attachment_uploads WHERE uploaded_at <= ?1")?
        .execute([now.saturating_sub(REUSE_MS)])?)
}

/// Uploads `spec` (or reuses a pointer of the last three days) and returns the pointer's
/// upload fields: CDN, key, size, digest, incremental MAC, upload time, content type and
/// dimensions. Blob bytes sent are added to `sent`. A request that moves no byte for `stall`
/// is dropped and the upload resumed.
pub(crate) async fn upload<T: SignalTransport>(
    net: &Net<T>,
    socket: &Arc<T::Socket>,
    db: &kourier_db::ZoneDb,
    spec: Spec,
    now: i64,
    sent: &Arc<AtomicU64>,
    stall: Duration,
) -> std::result::Result<proto::AttachmentPointer, UploadError> {
    if let Some(pointer) = db.read(|c| cached(c, &spec.source, now))? {
        tracing::debug!("reusing a recent attachment upload");
        return Ok(pointer);
    }
    // The pointer's size is 32 bits wide; refuse before anything is uploaded.
    let size = u32::try_from(spec.plain.len()).map_err(|_| UploadError::TooLarge)?;
    let incremental = supports_incremental_mac(&spec.mime);
    let blob_len = max_blob_len(spec.plain.len());
    let form = net
        .get_upload_form(socket, blob_len)
        .await?
        .map_err(|_| UploadError::TooLarge)?;
    let upload = Upload {
        form,
        plain: spec.plain,
        secret: Arc::new(Secret::random()),
        incremental,
        blob_len,
        sent: sent.clone(),
        moved: Arc::new(AtomicU64::new(0)),
        stall,
    };
    let encrypted = match upload.form.cdn {
        3 => tus(net, &upload).await?,
        2 => resumable(net, &upload).await?,
        other => return Err(UploadError::UnsupportedCdn(other)),
    };
    let pointer = proto::AttachmentPointer {
        attachment_identifier: Some(proto::attachment_pointer::AttachmentIdentifier::CdnKey(
            upload.form.key.clone(),
        )),
        cdn_number: Some(upload.form.cdn),
        key: Some(upload.secret.key.to_vec()),
        size: Some(size),
        digest: Some(encrypted.digest.to_vec()),
        incremental_mac: encrypted.incremental_mac.clone(),
        chunk_size: encrypted.chunk_size,
        upload_timestamp: Some(u64::try_from(now).unwrap_or(0)),
        content_type: Some(spec.mime),
        width: spec.dims.map(|d| d.0),
        height: spec.dims.map(|d| d.1),
        ..Default::default()
    };
    db.zone(|tx| remember(tx, &spec.source, &pointer, now))?;
    tracing::info!(
        cdn = upload.form.cdn,
        len = encrypted.len,
        incremental = encrypted.incremental_mac.is_some(),
        "attachment uploaded"
    );
    Ok(pointer)
}

#[cfg(test)]
mod tests;

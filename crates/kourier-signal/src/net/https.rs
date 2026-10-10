//! HTTPS to the Storage Service and the CDNs (§6.4): reqwest with a rustls config on the *ring*
//! provider whose root store holds only the environment's CA (Signal's own CA in production, as
//! Desktop's `_ajax` passes `ca` for every call; the mock CA in tests). One rustls backend, no
//! aws-lc-sys, no system roots.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use rustls::pki_types::CertificateDer;

use super::{SignalEnv, TransportError, log_safe_path, user_agent};

/// Connect timeout of storage and CDN requests.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest wait for the next piece of a streamed download: a stalled transfer fails after this
/// long, not only when the whole request's timeout runs out.
const DOWNLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Default limit of a response body (Storage Service answers; downloads set their own).
pub(crate) const DEFAULT_MAX_BODY: u64 = 64 * 1024 * 1024;

/// Which HTTPS service a request goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HttpsHost {
    /// The Storage Service (`storage.signal.org`), also used for groups.
    Storage,
    /// A CDN by number (0, 2 or 3).
    Cdn(u32),
}

/// One HTTPS request. `path` is appended to the host's base URL and must start with `/`.
#[derive(Clone)]
pub(crate) struct HttpsRequest {
    /// Target service.
    pub(crate) host: HttpsHost,
    /// HTTP verb.
    pub(crate) method: Method,
    /// Path and query below the service's base URL.
    pub(crate) path: String,
    /// Request headers (e.g. Basic auth for the Storage Service).
    pub(crate) headers: HeaderMap,
    /// Request body.
    pub(crate) body: Option<Bytes>,
    /// Whole-request timeout.
    pub(crate) timeout: Duration,
    /// Longest response body accepted; a longer one fails the request.
    pub(crate) max_body: u64,
}

impl HttpsRequest {
    /// `GET path` on `host` with a 30 s timeout and [`DEFAULT_MAX_BODY`].
    pub(crate) fn get(host: HttpsHost, path: impl Into<String>) -> Self {
        Self::new(Method::GET, host, path)
    }

    /// `method path` on `host` without a body, with a 30 s timeout and [`DEFAULT_MAX_BODY`].
    pub(crate) fn new(method: Method, host: HttpsHost, path: impl Into<String>) -> Self {
        Self {
            host,
            method,
            path: path.into(),
            headers: HeaderMap::new(),
            body: None,
            timeout: Duration::from_secs(30),
            max_body: DEFAULT_MAX_BODY,
        }
    }
}

impl HttpsRequest {
    /// The path without its query and with every segment that could carry an id, a key or a
    /// token replaced by `…` (the rule of `ChatRequest::log_safe_path`).
    pub(crate) fn log_safe_path(&self) -> String {
        log_safe_path(self.path.split('?').next().unwrap_or(""))
    }
}

/// Log-safe `Debug` (§9.1): no headers (credentials), redacted path.
impl fmt::Debug for HttpsRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpsRequest")
            .field("host", &self.host)
            .field("method", &self.method)
            .field("path", &self.log_safe_path())
            .finish_non_exhaustive()
    }
}

/// An HTTPS response with its whole body.
#[derive(Clone)]
pub(crate) struct HttpsResponse {
    /// HTTP status.
    pub(crate) status: StatusCode,
    /// Response headers.
    pub(crate) headers: HeaderMap,
    /// Response body.
    pub(crate) body: Bytes,
}

impl fmt::Debug for HttpsResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpsResponse")
            .field("status", &self.status)
            .field("body_len", &self.body.len())
            .finish_non_exhaustive()
    }
}

/// The HTTPS client of one environment.
pub(crate) struct HttpsClient {
    client: reqwest::Client,
    storage: reqwest::Url,
    cdns: BTreeMap<u32, reqwest::Url>,
}

impl HttpsClient {
    /// Builds the client for `env`: TLS 1.2+ on ring, the environment's CA as the only root,
    /// HTTP/2 or HTTP/1.1 by ALPN, Kourier's user agent.
    pub(crate) fn new(env: &SignalEnv) -> Result<Self, TransportError> {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from(env.https_ca_der.clone()))
            .map_err(|e| TransportError::Config(format!("https CA: {e}")))?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| TransportError::Config(format!("rustls: {e}")))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .https_only(true)
            .user_agent(user_agent())
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| TransportError::Config(format!("reqwest: {}", e.without_url())))?;

        let parse = |url: &str, what: &'static str| {
            reqwest::Url::parse(url).map_err(|_| TransportError::Config(format!("{what} URL")))
        };
        Ok(Self {
            client,
            storage: parse(&env.storage_url, "storage")?,
            cdns: env
                .cdn_urls
                .iter()
                .map(|(n, url)| Ok((*n, parse(url, "CDN")?)))
                .collect::<Result<_, TransportError>>()?,
        })
    }

    /// The absolute URL of a request: the base URL plus `path`, refused if it would leave the
    /// base's origin (Desktop asserts the same, `WebAPI.preload.ts:2215-2219`).
    pub(crate) fn url(&self, host: HttpsHost, path: &str) -> Result<reqwest::Url, TransportError> {
        let base = match host {
            HttpsHost::Storage => &self.storage,
            HttpsHost::Cdn(n) => self
                .cdns
                .get(&n)
                .ok_or(TransportError::InvalidRequest("unknown CDN"))?,
        };
        if !path.starts_with('/') {
            return Err(TransportError::InvalidRequest("path must start with '/'"));
        }
        let joined = format!("{}{path}", base.as_str().trim_end_matches('/'));
        let url = reqwest::Url::parse(&joined)
            .map_err(|_| TransportError::InvalidRequest("malformed path"))?;
        if url.origin() != base.origin() {
            return Err(TransportError::InvalidRequest(
                "path leaves the service origin",
            ));
        }
        Ok(url)
    }

    /// Sends a request and reads the whole body (at most `request.max_body` bytes). Any HTTP
    /// status is a response; only failures below HTTP, and bodies over the limit, are errors.
    pub(crate) async fn send(
        &self,
        request: HttpsRequest,
    ) -> Result<HttpsResponse, TransportError> {
        let max_body = request.max_body;
        let url = self.url(request.host, &request.path)?;
        let mut builder = self
            .client
            .request(request.method, url)
            .headers(request.headers)
            .timeout(request.timeout);
        if let Some(body) = request.body {
            builder = builder.body(body);
        }
        let mut response = builder.send().await.map_err(https_error)?;
        let status = response.status();
        let headers = response.headers().clone();
        if response.content_length().is_some_and(|len| len > max_body) {
            return Err(TransportError::BodyTooLarge);
        }
        let mut body = bytes::BytesMut::new();
        while let Some(chunk) = response.chunk().await.map_err(https_error)? {
            if (body.len() + chunk.len()) as u64 > max_body {
                return Err(TransportError::BodyTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(HttpsResponse {
            status,
            headers,
            body: body.freeze(),
        })
    }
}

/// Where a streamed download's body goes, chunk by chunk: a bounded channel to a consumer off
/// the network runtime (its full queue holds the download back; a closed one stops it). It
/// counts the bytes it took, so a transfer that broke off can go on where it stopped
/// ([`DownloadSink::resume`]).
#[derive(Clone)]
pub(crate) struct DownloadSink {
    tx: tokio::sync::mpsc::Sender<Bytes>,
    /// Body bytes handed to the consumer so far, over every request of this download.
    taken: Arc<std::sync::atomic::AtomicU64>,
    /// Where the body of this request starts in the whole blob.
    offset: u64,
    /// The length of the whole blob as the server stated it (0: not stated yet).
    total: Arc<std::sync::atomic::AtomicU64>,
}

/// The consumer of a [`DownloadSink`] stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SinkClosed;

impl DownloadSink {
    /// A sink that feeds `tx` from the start of the blob.
    pub(crate) fn new(tx: tokio::sync::mpsc::Sender<Bytes>) -> Self {
        Self {
            tx,
            taken: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            offset: 0,
            total: Arc::default(),
        }
    }

    /// A sink that feeds `tx` from byte `offset` of the blob on: the consumer holds the
    /// bytes before it from an earlier run (a download that goes on after a restart). Its
    /// first request already asks for the rest (`Range: bytes=<offset>-`).
    pub(crate) fn starting_at(tx: tokio::sync::mpsc::Sender<Bytes>, offset: u64) -> Self {
        Self {
            tx,
            taken: Arc::new(std::sync::atomic::AtomicU64::new(offset)),
            offset,
            total: Arc::default(),
        }
    }

    /// The length of the whole blob, once a response stated it (its `Content-Length`, plus
    /// what came before the part it carries).
    pub(crate) fn total(&self) -> Option<u64> {
        match self.total.load(std::sync::atomic::Ordering::Acquire) {
            0 => None,
            total => Some(total),
        }
    }

    /// Blob bytes handed to the consumer so far (every request of the download counted).
    pub(crate) fn taken(&self) -> u64 {
        self.taken.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Where this request's body starts in the blob: 0, or what an earlier request of the
    /// same download delivered. A transport asks the server for the rest
    /// (`Range: bytes=<offset>-`).
    pub(crate) fn offset(&self) -> u64 {
        self.offset
    }

    /// The same consumer for a request that goes on where the last one stopped: its body
    /// starts at [`Self::taken`].
    pub(crate) fn resume(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            taken: self.taken.clone(),
            offset: self.taken(),
            total: self.total.clone(),
        }
    }

    /// Hands the next piece of the body to the consumer, waiting while its queue is full.
    pub(crate) async fn send(&self, chunk: Bytes) -> Result<(), SinkClosed> {
        let len = chunk.len() as u64;
        self.tx.send(chunk).await.map_err(|_| SinkClosed)?;
        self.taken
            .fetch_add(len, std::sync::atomic::Ordering::AcqRel);
        Ok(())
    }
}

/// The first byte position of a `Content-Range: bytes <first>-<last>/<length>` header.
fn content_range_start(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(http::header::CONTENT_RANGE)?
        .to_str()
        .ok()?
        .trim()
        .strip_prefix("bytes ")?
        .split('-')
        .next()?
        .trim()
        .parse()
        .ok()
}

impl HttpsClient {
    /// Sends a request and streams a successful body (at most `request.max_body` bytes for the
    /// whole blob) into `sink`; other statuses return without reading the body. Returns the
    /// status. A body that stops arriving for [`DOWNLOAD_IDLE_TIMEOUT`] is a timeout.
    ///
    /// A sink that [resumes](DownloadSink::resume) a download asks for the rest of the blob
    /// with `Range: bytes=<offset>-`. The server answers 206 with a `Content-Range` that
    /// starts there; one that ignores the header answers 200 with the whole blob, of which the
    /// part the consumer already has is skipped. Either way the consumer sees one unbroken
    /// stream, which it verifies as a whole.
    pub(crate) async fn download(
        &self,
        request: HttpsRequest,
        sink: DownloadSink,
    ) -> Result<StatusCode, TransportError> {
        let max_body = request.max_body;
        let offset = sink.offset();
        let url = self.url(request.host, &request.path)?;
        let mut headers = request.headers;
        if offset > 0 {
            let range = HeaderValue::from_str(&format!("bytes={offset}-"))
                .map_err(|_| TransportError::InvalidRequest("download range"))?;
            headers.insert(http::header::RANGE, range);
        }
        let mut response = self
            .client
            .request(request.method, url)
            .headers(headers)
            .timeout(request.timeout)
            .send()
            .await
            .map_err(https_error)?;
        let status = response.status();
        if !status.is_success() {
            return Ok(status);
        }
        // How much of this body the consumer already has: nothing of a partial answer that
        // starts where we asked, the first `offset` bytes of a whole one.
        let mut skip = match (offset, status) {
            (0, _) => 0,
            (_, StatusCode::PARTIAL_CONTENT) => {
                if content_range_start(response.headers()) != Some(offset) {
                    return Err(TransportError::InvalidResponse(
                        "the download resumed at another position".to_owned(),
                    ));
                }
                0
            }
            _ => offset,
        };
        let already = offset.saturating_sub(skip);
        if response
            .content_length()
            .is_some_and(|len| already.saturating_add(len) > max_body)
        {
            return Err(TransportError::BodyTooLarge);
        }
        if let Some(len) = response.content_length() {
            sink.total.store(
                already.saturating_add(len),
                std::sync::atomic::Ordering::Release,
            );
        }
        let mut received = already;
        loop {
            let chunk = tokio::time::timeout(DOWNLOAD_IDLE_TIMEOUT, response.chunk())
                .await
                .map_err(|_| TransportError::Timeout)?
                .map_err(https_error)?;
            let Some(mut chunk) = chunk else {
                break;
            };
            received = received.saturating_add(chunk.len() as u64);
            if received > max_body {
                return Err(TransportError::BodyTooLarge);
            }
            if skip > 0 {
                let cut = usize::try_from(skip).unwrap_or(usize::MAX).min(chunk.len());
                skip -= cut as u64;
                chunk = chunk.slice(cut..);
                if chunk.is_empty() {
                    continue;
                }
            }
            sink.send(chunk).await.map_err(|_| {
                TransportError::InvalidResponse("the download's consumer stopped".to_owned())
            })?;
        }
        if skip > 0 {
            // A whole answer shorter than what the consumer already has: not the same blob.
            return Err(TransportError::InvalidResponse(
                "the download restarted with a shorter body".to_owned(),
            ));
        }
        Ok(status)
    }
}

/// Longest answer body an upload request reads (CDN answers are tiny).
const UPLOAD_ANSWER_MAX: u64 = 64 * 1024;

/// The body of an upload: its length and its chunks, produced off the network runtime.
pub(crate) struct UploadBody {
    /// Bytes the chunks add up to (sent as `Content-Length`).
    pub(crate) len: u64,
    /// The chunks; an `Err` aborts the request.
    pub(crate) chunks: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
}

/// A request to an absolute URL the server handed out: a signed CDN upload location or the
/// session URL it led to (§6.7 step 2). The URL is a credential and never printed.
pub(crate) struct UploadRequest {
    /// HTTP verb.
    pub(crate) method: Method,
    /// Absolute `https` URL.
    pub(crate) url: String,
    /// Request headers (the upload form's headers, TUS or range headers).
    pub(crate) headers: HeaderMap,
    /// Request body; `None` sends an empty one.
    pub(crate) body: Option<UploadBody>,
    /// Whole-request timeout.
    pub(crate) timeout: Duration,
}

impl fmt::Debug for UploadRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadRequest")
            .field("method", &self.method)
            .field("len", &self.body.as_ref().map(|b| b.len))
            .finish_non_exhaustive()
    }
}

impl HttpsClient {
    /// Sends an upload request to its absolute URL (`https` only) and reads at most 64 KiB of
    /// the answer. Any HTTP status is a response.
    pub(crate) async fn upload(
        &self,
        request: UploadRequest,
    ) -> Result<HttpsResponse, TransportError> {
        let url = reqwest::Url::parse(&request.url)
            .map_err(|_| TransportError::InvalidRequest("malformed upload URL"))?;
        if url.scheme() != "https" || url.host_str().is_none() {
            return Err(TransportError::InvalidRequest("upload URL must be https"));
        }
        let mut builder = self
            .client
            .request(request.method, url)
            .headers(request.headers)
            .timeout(request.timeout);
        builder = match request.body {
            Some(body) => {
                let chunks = futures_util::stream::unfold(body.chunks, |mut rx| async move {
                    rx.recv().await.map(|item| (item, rx))
                });
                builder
                    .header(http::header::CONTENT_LENGTH, body.len)
                    .body(reqwest::Body::wrap_stream(chunks))
            }
            None => builder.body(Bytes::new()),
        };
        let mut response = builder.send().await.map_err(https_error)?;
        let status = response.status();
        let headers = response.headers().clone();
        let mut body = bytes::BytesMut::new();
        while let Some(chunk) = response.chunk().await.map_err(https_error)? {
            if (body.len() + chunk.len()) as u64 > UPLOAD_ANSWER_MAX {
                break;
            }
            body.extend_from_slice(&chunk);
        }
        Ok(HttpsResponse {
            status,
            headers,
            body: body.freeze(),
        })
    }
}

/// reqwest errors print the URL; strip it (paths can carry ids and CDN keys).
fn https_error(error: reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::Timeout
    } else {
        TransportError::Https(error.without_url().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::SignalEnvironment;
    use crate::net::env::tests::dummy_local_env;

    #[test]
    fn urls_join_base_and_path_and_stay_on_origin() {
        let env = SignalEnv::resolve(&SignalEnvironment::LocalTest(dummy_local_env(4433))).unwrap();
        let client = HttpsClient::new(&env).unwrap();
        assert_eq!(
            client
                .url(HttpsHost::Storage, "/v1/storage/manifest")
                .unwrap()
                .as_str(),
            "https://localhost:4433/storageService/v1/storage/manifest"
        );
        assert_eq!(
            client
                .url(HttpsHost::Cdn(3), "/attachments/abc")
                .unwrap()
                .as_str(),
            "https://localhost:4433/cdn3/attachments/abc"
        );
        assert_eq!(
            client.url(HttpsHost::Cdn(2), "/x").unwrap_err(),
            TransportError::InvalidRequest("unknown CDN")
        );
        assert!(client.url(HttpsHost::Cdn(0), "x").is_err());
        assert_eq!(
            client
                .url(HttpsHost::Cdn(0), "@evil.example/x")
                .unwrap_err(),
            TransportError::InvalidRequest("path must start with '/'")
        );

        let prod = SignalEnv::resolve(&SignalEnvironment::Production).unwrap();
        let client = HttpsClient::new(&prod).unwrap();
        assert_eq!(
            client
                .url(HttpsHost::Storage, "/v1/groups/")
                .unwrap()
                .as_str(),
            "https://storage.signal.org/v1/groups/"
        );
        // `//host` after the base is a path segment, not a new authority.
        assert_eq!(
            client
                .url(HttpsHost::Cdn(2), "//evil.example/x")
                .unwrap()
                .host_str(),
            Some("cdn2.signal.org")
        );
    }

    #[test]
    fn request_debug_hides_headers_and_ids() {
        let mut request = HttpsRequest::get(HttpsHost::Storage, "/v1/storage/read?x=secret");
        request.headers.insert(
            http::header::AUTHORIZATION,
            "Basic c2VjcmV0".parse().unwrap(),
        );
        let debug = format!("{request:?}");
        assert!(
            !debug.contains("secret") && !debug.contains("Basic"),
            "{debug}"
        );
        assert!(debug.contains("/v1/storage/read"), "{debug}");
    }
}

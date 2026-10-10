//! Uploads against a scripted CDN: TUS creation, a broken-off TUS upload resumed with
//! `HEAD` + `PATCH`, a GCS resumable session resumed after a failed `PUT`, and the pointer cache.

use std::sync::Mutex;

use http::HeaderMap;

use super::*;
use crate::attachment::{Integrity, decrypt_in_memory};
use crate::net::libsignal::LibsignalSocket;
use crate::net::{
    AuthCredentials, ChatEvent, Connected, DownloadSink, HttpsRequest, ProvisioningEvent,
};

/// What the scripted CDN saw and holds.
#[derive(Default)]
struct CdnState {
    /// `(method, url, headers)` of every request.
    requests: Vec<(Method, String, HeaderMap)>,
    /// The bytes it stored for the upload.
    stored: Vec<u8>,
    /// The next body request breaks off after this many bytes (once).
    break_after: Option<usize>,
    /// The next body request takes this many bytes and then neither reads on nor answers
    /// (once): a connection that went dead without closing.
    stall_after: Option<usize>,
    /// The next body request is answered this long after its body was read (once): a slow
    /// link that is still sending what the body's queue handed over.
    answer_after: Option<Duration>,
}

#[derive(Default)]
struct Cdn {
    state: Mutex<CdnState>,
}

const SESSION: &str = "https://gcs.example/session/1";

impl Cdn {
    async fn take_body(&self, body: Option<UploadBody>) -> (Vec<u8>, bool) {
        let (limit, stall, answer_after) = {
            let mut state = self.state.lock().unwrap();
            (
                state.break_after.take(),
                state.stall_after.take(),
                state.answer_after.take(),
            )
        };
        let mut data = Vec::new();
        let Some(mut body) = body else {
            return (data, false);
        };
        while let Some(chunk) = body.chunks.recv().await {
            data.extend_from_slice(&chunk.unwrap());
            if let Some(limit) = limit
                && data.len() >= limit
            {
                data.truncate(limit);
                return (data, true);
            }
            if let Some(stall) = stall
                && data.len() >= stall
            {
                // The CDN has these bytes; nothing moves any more and no answer comes. The
                // body stays open, as a dead connection would leave it.
                data.truncate(stall);
                self.state.lock().unwrap().stored.extend_from_slice(&data);
                std::future::pending::<()>().await;
            }
        }
        if let Some(wait) = answer_after {
            tokio::time::sleep(wait).await;
        }
        (data, false)
    }
}

fn response(status: u16, headers: &[(&str, String)]) -> HttpsResponse {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        map.insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    HttpsResponse {
        status: StatusCode::from_u16(status).unwrap(),
        headers: map,
        body: Bytes::new(),
    }
}

impl SignalTransport for Cdn {
    type Socket = LibsignalSocket;

    async fn connect_auth(
        &self,
        _credentials: &AuthCredentials,
    ) -> std::result::Result<Connected<LibsignalSocket, ChatEvent>, TransportError> {
        Err(TransportError::Disconnected)
    }

    async fn connect_unauth(
        &self,
    ) -> std::result::Result<Connected<LibsignalSocket, ChatEvent>, TransportError> {
        Err(TransportError::Disconnected)
    }

    async fn connect_provisioning(
        &self,
    ) -> std::result::Result<Connected<LibsignalSocket, ProvisioningEvent>, TransportError> {
        Err(TransportError::Disconnected)
    }

    async fn https(
        &self,
        _request: HttpsRequest,
    ) -> std::result::Result<HttpsResponse, TransportError> {
        Err(TransportError::Disconnected)
    }

    async fn https_download(
        &self,
        _request: HttpsRequest,
        _sink: DownloadSink,
    ) -> std::result::Result<StatusCode, TransportError> {
        Err(TransportError::Disconnected)
    }

    async fn https_upload(
        &self,
        request: UploadRequest,
    ) -> std::result::Result<HttpsResponse, TransportError> {
        let UploadRequest {
            method,
            url,
            headers,
            body,
            ..
        } = request;
        self.state
            .lock()
            .unwrap()
            .requests
            .push((method.clone(), url.clone(), headers.clone()));
        let header = |name: &str| {
            headers
                .get(name)
                .map(|v| v.to_str().unwrap().to_owned())
                .unwrap_or_default()
        };
        let tus = headers.contains_key("tus-resumable");
        match (method.as_str(), tus) {
            ("POST", true) | ("PATCH", true) => {
                if method == Method::PATCH {
                    let offset: usize = header("upload-offset").parse().unwrap();
                    assert_eq!(
                        offset,
                        self.state.lock().unwrap().stored.len(),
                        "resume offset"
                    );
                }
                let (data, broke) = self.take_body(body).await;
                self.state.lock().unwrap().stored.extend_from_slice(&data);
                if broke {
                    return Err(TransportError::Timeout);
                }
                Ok(response(
                    if method == Method::POST { 201 } else { 204 },
                    &[],
                ))
            }
            ("HEAD", true) => {
                let len = self.state.lock().unwrap().stored.len();
                Ok(response(200, &[("upload-offset", len.to_string())]))
            }
            ("POST", false) => Ok(response(201, &[("location", SESSION.to_owned())])),
            ("PUT", false) => {
                assert_eq!(url, SESSION);
                let range = header("content-range");
                if let Some(total) = range.strip_prefix("bytes */") {
                    let total: usize = total.parse().unwrap();
                    let len = self.state.lock().unwrap().stored.len();
                    return Ok(if len == total {
                        response(200, &[])
                    } else if len == 0 {
                        response(308, &[])
                    } else {
                        response(308, &[("range", format!("bytes=0-{}", len - 1))])
                    });
                }
                let start: usize = range
                    .strip_prefix("bytes ")
                    .and_then(|r| r.split('-').next())
                    .unwrap()
                    .parse()
                    .unwrap();
                assert_eq!(start, self.state.lock().unwrap().stored.len(), "PUT start");
                let (data, broke) = self.take_body(body).await;
                self.state.lock().unwrap().stored.extend_from_slice(&data);
                if broke {
                    return Ok(response(503, &[]));
                }
                Ok(response(200, &[]))
            }
            other => panic!("unexpected request {other:?}"),
        }
    }

    fn network_changed(&self) {}
}

fn upload_of(cdn: u32, plaintext: &[u8], incremental: bool) -> Upload {
    Upload {
        form: UploadForm {
            cdn,
            key: "the-cdn-key".to_owned(),
            headers: vec![("x-goog-test".to_owned(), "1".to_owned())],
            signed_upload_url: "https://cdn.example/attachments/".to_owned(),
        },
        plain: Plaintext::Memory(Arc::new(Zeroizing::new(plaintext.to_vec()))),
        secret: Arc::new(Secret::random()),
        incremental,
        blob_len: max_blob_len(plaintext.len() as u64),
        sent: Arc::new(AtomicU64::new(0)),
        moved: Arc::new(AtomicU64::new(0)),
        stall: STALL_TIMEOUT,
    }
}

fn plaintext(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 253) as u8).collect()
}

/// The CDN holds exactly the blob the pointer describes.
fn assert_stored(cdn: &Cdn, upload: &Upload, encrypted: &Encrypted, plain: &[u8]) {
    let stored = cdn.state.lock().unwrap().stored.clone();
    assert_eq!(stored.len() as u64, upload.blob_len);
    assert_eq!(encrypted.len, upload.blob_len);
    let keys = AttachmentKeys::from_slice(&upload.secret.key[..]).unwrap();
    let decrypted = decrypt_in_memory(
        &keys,
        Integrity::Digest(encrypted.digest),
        plain.len() as u64,
        &stored,
        u64::MAX,
    )
    .unwrap();
    assert_eq!(&decrypted[..], plain);
}

#[tokio::test]
async fn tus_creation_with_upload_sends_the_whole_blob_with_desktops_headers() {
    let cdn = Arc::new(Cdn::default());
    let net = Net::new(cdn.clone(), tokio::runtime::Handle::current());
    let plain = plaintext(300_000);
    let upload = upload_of(3, &plain, true);
    let encrypted = tus(&net, &upload).await.unwrap();
    assert_stored(&cdn, &upload, &encrypted, &plain);
    assert!(encrypted.incremental_mac.is_some());
    assert_eq!(upload.sent.load(Ordering::Relaxed), upload.blob_len);
    let state = cdn.state.lock().unwrap();
    assert_eq!(state.requests.len(), 1);
    let (method, url, headers) = &state.requests[0];
    assert_eq!(*method, Method::POST);
    assert_eq!(url, "https://cdn.example/attachments/");
    assert_eq!(headers["tus-resumable"], "1.0.0");
    assert_eq!(
        headers["upload-length"],
        upload.blob_len.to_string().as_str()
    );
    assert_eq!(
        headers["upload-metadata"],
        format!("filename {}", STANDARD.encode("the-cdn-key")).as_str()
    );
    assert_eq!(headers["content-type"], "application/offset+octet-stream");
    assert_eq!(headers["x-goog-test"], "1", "the form's headers");
}

/// An upload whose connection went dead without closing: no byte moves and no error comes.
/// The stall is noticed after the stall time (not after the ten minutes of the request's own
/// timeout), the request is dropped, and the upload goes on from the offset the CDN has, on
/// both CDNs. The CDN ends up with exactly the blob the pointer describes.
#[tokio::test]
async fn a_stalled_upload_is_noticed_and_resumed() {
    for cdn_number in [3u32, 2] {
        let cdn = Arc::new(Cdn::default());
        cdn.state.lock().unwrap().stall_after = Some(70_001);
        let net = Net::new(cdn.clone(), tokio::runtime::Handle::current());
        // Larger than the stalled part plus what the request's queue holds, so the producer
        // is still feeding the body when the connection dies.
        let plain = plaintext(1_500_000);
        let mut upload = upload_of(cdn_number, &plain, false);
        upload.stall = Duration::from_millis(200);
        let started = std::time::Instant::now();
        let encrypted = match cdn_number {
            3 => tus(&net, &upload).await,
            _ => resumable(&net, &upload).await,
        }
        .unwrap_or_else(|e| panic!("CDN {cdn_number}: {e}"));
        let took = started.elapsed();
        assert!(
            took < Duration::from_secs(20),
            "CDN {cdn_number}: noticed by the stall watch, not by the request timeout: {took:?}"
        );
        assert!(took >= Duration::from_millis(200), "{took:?}");
        assert_stored(&cdn, &upload, &encrypted, &plain);
        let state = cdn.state.lock().unwrap();
        let methods: Vec<&str> = state.requests.iter().map(|r| r.0.as_str()).collect();
        match cdn_number {
            3 => {
                assert_eq!(methods, ["POST", "HEAD", "PATCH"], "resumed once");
                assert_eq!(state.requests[2].2["upload-offset"], "70001");
            }
            _ => {
                assert_eq!(
                    methods,
                    ["POST", "PUT", "PUT", "PUT"],
                    "session, blob, query, rest"
                );
                assert_eq!(
                    state.requests[3].2["content-range"],
                    format!("bytes 70001-{}/{}", upload.blob_len - 1, upload.blob_len).as_str()
                );
            }
        }
    }
}

/// A small upload is handed to the request's queue as a whole before a byte is on the wire,
/// so the watch cannot see it move. It gets the tail's time for that, not one stall time: a
/// slow link that is alive is left alone, on both CDNs, and a dead connection is still
/// noticed (after the tail's time) and the upload resumed.
#[tokio::test]
async fn a_small_upload_gets_the_time_of_a_tail() {
    use crate::net::UPLOAD_TAIL_STALLS;
    let stall = Duration::from_millis(200);
    let tail = stall * UPLOAD_TAIL_STALLS;
    // 100 KB: less than the queue holds.
    let plain = plaintext(100_000);
    for cdn_number in [3u32, 2] {
        // Slow: the CDN has the body and answers after three stall times.
        let cdn = Arc::new(Cdn::default());
        cdn.state.lock().unwrap().answer_after = Some(stall * 3);
        let net = Net::new(cdn.clone(), tokio::runtime::Handle::current());
        let mut upload = upload_of(cdn_number, &plain, false);
        upload.stall = stall;
        let encrypted = match cdn_number {
            3 => tus(&net, &upload).await,
            _ => resumable(&net, &upload).await,
        }
        .unwrap_or_else(|e| panic!("CDN {cdn_number}: {e}"));
        assert_stored(&cdn, &upload, &encrypted, &plain);
        let methods: Vec<String> = cdn
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .map(|r| r.0.to_string())
            .collect();
        let expected: &[&str] = if cdn_number == 3 {
            &["POST"]
        } else {
            &["POST", "PUT"]
        };
        assert_eq!(
            methods, expected,
            "one request carried the blob, undisturbed"
        );

        // Dead: the CDN took 70,001 bytes and nothing comes any more.
        let cdn = Arc::new(Cdn::default());
        cdn.state.lock().unwrap().stall_after = Some(70_001);
        let net = Net::new(cdn.clone(), tokio::runtime::Handle::current());
        let mut upload = upload_of(cdn_number, &plain, false);
        upload.stall = stall;
        let started = std::time::Instant::now();
        let encrypted = match cdn_number {
            3 => tus(&net, &upload).await,
            _ => resumable(&net, &upload).await,
        }
        .unwrap_or_else(|e| panic!("CDN {cdn_number}: {e}"));
        let took = started.elapsed();
        assert!(
            took >= tail && took < Duration::from_secs(20),
            "CDN {cdn_number}: noticed after the tail's time: {took:?}"
        );
        assert_stored(&cdn, &upload, &encrypted, &plain);
        let state = cdn.state.lock().unwrap();
        let resumed = state.requests.last().expect("requests");
        match cdn_number {
            3 => assert_eq!(resumed.2["upload-offset"], "70001"),
            _ => assert_eq!(
                resumed.2["content-range"],
                format!("bytes 70001-{}/{}", upload.blob_len - 1, upload.blob_len).as_str()
            ),
        }
    }
}

/// A slow upload is not a stalled one: as long as bytes move, however slowly, the watch
/// leaves the request alone.
#[tokio::test]
async fn a_slow_upload_is_left_alone() {
    use std::sync::atomic::Ordering;
    let cdn = Arc::new(Cdn::default());
    let net = Net::new(cdn.clone(), tokio::runtime::Handle::current());
    let stall = Duration::from_millis(200);
    let moved = Arc::new(AtomicU64::new(0));
    // A body of 10 chunks, one every 100 ms: a second in all, five times the stall time.
    let (tx, chunks) = mpsc::channel(1);
    let feeder_moved = moved.clone();
    let feeder = tokio::spawn(async move {
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            tx.send(Ok(Bytes::from_static(&[7u8; 1000]))).await.unwrap();
            feeder_moved.fetch_add(1000, Ordering::Relaxed);
        }
    });
    let mut headers = HeaderMap::new();
    headers.insert("tus-resumable", HeaderValue::from_static("1.0.0"));
    let answer = net
        .https_upload_watched(
            UploadRequest {
                method: Method::POST,
                url: "https://cdn.example/attachments/".to_owned(),
                headers,
                body: Some(UploadBody {
                    len: 10_000,
                    chunks,
                }),
                timeout: Duration::from_secs(60),
            },
            moved,
            10_000,
            stall,
        )
        .await
        .expect("left alone");
    assert_eq!(answer.status.as_u16(), 201);
    feeder.await.unwrap();
    assert_eq!(cdn.state.lock().unwrap().stored.len(), 10_000);
}

#[tokio::test]
async fn a_broken_tus_upload_resumes_from_the_servers_offset() {
    let cdn = Arc::new(Cdn::default());
    cdn.state.lock().unwrap().break_after = Some(100_003);
    let net = Net::new(cdn.clone(), tokio::runtime::Handle::current());
    let plain = plaintext(250_000);
    let upload = upload_of(3, &plain, false);
    let encrypted = tus(&net, &upload).await.unwrap();
    assert_stored(&cdn, &upload, &encrypted, &plain);
    let state = cdn.state.lock().unwrap();
    let calls: Vec<(&str, &str)> = state
        .requests
        .iter()
        .map(|(m, u, _)| (m.as_str(), u.as_str()))
        .collect();
    assert_eq!(
        calls,
        [
            ("POST", "https://cdn.example/attachments/"),
            ("HEAD", "https://cdn.example/attachments/the-cdn-key"),
            ("PATCH", "https://cdn.example/attachments/the-cdn-key"),
        ]
    );
    assert_eq!(state.requests[2].2["upload-offset"], "100003");
}

#[tokio::test]
async fn a_gcs_session_resumes_after_a_failed_put() {
    let cdn = Arc::new(Cdn::default());
    cdn.state.lock().unwrap().break_after = Some(65_536 + 7);
    let net = Net::new(cdn.clone(), tokio::runtime::Handle::current());
    let plain = plaintext(150_000);
    let upload = upload_of(2, &plain, false);
    let encrypted = resumable(&net, &upload).await.unwrap();
    assert_stored(&cdn, &upload, &encrypted, &plain);
    let state = cdn.state.lock().unwrap();
    let ranges: Vec<String> = state
        .requests
        .iter()
        .filter(|(m, _, _)| *m == Method::PUT)
        .map(|(_, _, h)| h["content-range"].to_str().unwrap().to_owned())
        .collect();
    let len = upload.blob_len;
    assert_eq!(
        ranges,
        [
            format!("bytes 0-{}/{len}", len - 1),
            format!("bytes */{len}"),
            format!("bytes 65543-{}/{len}", len - 1),
        ]
    );
    // The form's headers open the session; its own requests carry only their range.
    for (method, _, headers) in &state.requests {
        assert_eq!(
            headers.contains_key("x-goog-test"),
            *method == Method::POST,
            "{method}"
        );
    }
}

#[test]
fn pointers_are_reused_for_three_days_then_purged() {
    let fixture = crate::receive::tests::fixture();
    let source = source_key("as-is", &[b"core/ab/x", &[7; 32]]);
    assert_ne!(
        source,
        source_key("image-level-3", &[b"core/ab/x", &[7; 32]])
    );
    assert_ne!(source, source_key("as-is", &[b"core/ab/xx", &[7; 32]]));
    let pointer = proto::AttachmentPointer {
        cdn_number: Some(3),
        size: Some(9),
        ..Default::default()
    };
    let day = 24 * 3600 * 1000;
    fixture
        .db
        .zone(|tx| -> Result<()> {
            remember(tx, &source, &pointer, 10 * day)?;
            assert_eq!(cached(tx, &source, 10 * day + 1)?, Some(pointer.clone()));
            assert_eq!(cached(tx, &source, 13 * day - 1)?, Some(pointer.clone()));
            assert_eq!(cached(tx, &source, 13 * day)?, None, "three days at most");
            assert_eq!(
                cached(tx, &source, 9 * day)?,
                None,
                "an upload from the future"
            );
            assert_eq!(cached(tx, &[0; 32], 10 * day)?, None);
            assert_eq!(purge(tx, 12 * day)?, 0);
            assert_eq!(purge(tx, 13 * day)?, 1);
            Ok(())
        })
        .unwrap();
}

#[test]
fn errors_map_to_contract_kinds() {
    assert_eq!(
        UploadError::TooLarge.to_backend_error().kind.name(),
        ErrorKind::TooLarge.name()
    );
    assert!(
        UploadError::Status(403)
            .to_backend_error()
            .kind
            .is_retryable()
    );
    assert!(
        UploadError::Incomplete
            .to_backend_error()
            .kind
            .is_retryable()
    );
    assert!(
        !UploadError::Crypto(AttachmentError::Sink("gone".into()))
            .to_backend_error()
            .kind
            .is_retryable()
    );
}

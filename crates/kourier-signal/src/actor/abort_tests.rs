//! `Command::AbortTransmit` through the real command path of an actor: a `Transmit` of a message
//! whose upload the transport HOLDS, and an abort that arrives before the point of no return
//! (answered `Stopping`: the attempt ends `Failed`, nothing reaches the wire, its requests and
//! files go), after it (`TooLate`: the send goes on), for no attempt at all, and for an edit
//! (never stoppable). The core's side is the fake's (`kourier-core` tests).

use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use kourier_backend_api::{AbortAnswer, OutgoingKind, OutgoingRequest};
use kourier_model::{
    Attachment, AttachmentKind, BlobKey, BlobRef, Body, ConvKey, ErrorKind, MessageId, MsgTarget,
    TargetAuthor, Text,
};
use tokio::sync::{oneshot, watch};

use super::*;
use crate::net::{
    ChatEvent, ChatRequest, ChatResponse, ChatSocket, HttpsRequest, HttpsResponse,
    MultiRecipientOutcome, MultiRecipientSend, ProvisioningEvent, UploadForm, UploadRequest,
    UploadTooLarge,
};

/// A transport whose CDN reads every upload body and then holds the request while `uploads` is
/// closed, and whose chat requests (the first thing a send does once it is built) count
/// themselves and hold while `wire` is closed, then fail.
struct HoldNet {
    uploads: watch::Sender<bool>,
    wire: watch::Sender<bool>,
    /// An upload request has its body and holds.
    arrived: Notify,
    /// Upload requests alive now: a request that is dropped leaves.
    uploading: AtomicI64,
    /// Requests that would reach the server or a recipient.
    touched: AtomicUsize,
    /// Upload requests that were made at all (their bodies are read when one starts).
    upload_calls: AtomicUsize,
    /// A request reached the wire.
    reached_wire: Notify,
}

impl HoldNet {
    fn new(uploads_open: bool, wire_open: bool) -> Arc<Self> {
        Arc::new(Self {
            uploads: watch::channel(uploads_open).0,
            wire: watch::channel(wire_open).0,
            arrived: Notify::new(),
            uploading: AtomicI64::new(0),
            touched: AtomicUsize::new(0),
            upload_calls: AtomicUsize::new(0),
            reached_wire: Notify::new(),
        })
    }

    async fn touch_wire(&self) {
        self.touched.fetch_add(1, Ordering::SeqCst);
        self.reached_wire.notify_one();
        let _ = self.wire.subscribe().wait_for(|open| *open).await;
    }
}

/// Counts an upload request while it lives.
struct Alive<'a>(&'a AtomicI64);

impl<'a> Alive<'a> {
    fn new(count: &'a AtomicI64) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count)
    }
}

impl Drop for Alive<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct HoldSocket(Arc<HoldNet>);

impl ChatSocket for HoldSocket {
    async fn request(
        &self,
        _request: ChatRequest,
    ) -> Result<ChatResponse, crate::net::TransportError> {
        self.0.touch_wire().await;
        Err(crate::net::TransportError::Disconnected)
    }

    async fn send_multi_recipient(
        &self,
        _send: MultiRecipientSend,
    ) -> Result<MultiRecipientOutcome, crate::net::TransportError> {
        self.0.touch_wire().await;
        Err(crate::net::TransportError::Disconnected)
    }

    async fn get_upload_form(
        &self,
        _upload_length: u64,
    ) -> Result<Result<UploadForm, UploadTooLarge>, crate::net::TransportError> {
        Ok(Ok(UploadForm {
            cdn: 3,
            key: "the-cdn-key".to_owned(),
            headers: Vec::new(),
            signed_upload_url: "https://cdn.example/attachments/".to_owned(),
        }))
    }

    async fn disconnect(&self) {}
}

impl SignalTransport for HoldNet {
    type Socket = HoldSocket;

    async fn connect_auth(
        &self,
        _credentials: &crate::net::AuthCredentials,
    ) -> Result<crate::net::Connected<HoldSocket, ChatEvent>, crate::net::TransportError> {
        self.touch_wire().await;
        Err(crate::net::TransportError::Disconnected)
    }

    async fn connect_unauth(
        &self,
    ) -> Result<crate::net::Connected<HoldSocket, ChatEvent>, crate::net::TransportError> {
        self.touch_wire().await;
        Err(crate::net::TransportError::Disconnected)
    }

    async fn connect_provisioning(
        &self,
    ) -> Result<crate::net::Connected<HoldSocket, ProvisioningEvent>, crate::net::TransportError>
    {
        Err(crate::net::TransportError::Disconnected)
    }

    async fn https(
        &self,
        _request: HttpsRequest,
    ) -> Result<HttpsResponse, crate::net::TransportError> {
        self.touch_wire().await;
        Err(crate::net::TransportError::Disconnected)
    }

    async fn https_download(
        &self,
        _request: HttpsRequest,
        _sink: crate::net::DownloadSink,
    ) -> Result<http::StatusCode, crate::net::TransportError> {
        Err(crate::net::TransportError::Disconnected)
    }

    async fn https_upload(
        &self,
        request: UploadRequest,
    ) -> Result<HttpsResponse, crate::net::TransportError> {
        let _alive = Alive::new(&self.uploading);
        self.upload_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(mut body) = request.body {
            while let Some(chunk) = body.chunks.recv().await {
                chunk.map_err(|_| crate::net::TransportError::Disconnected)?;
            }
        }
        self.arrived.notify_one();
        let _ = self.uploads.subscribe().wait_for(|open| *open).await;
        Ok(HttpsResponse {
            status: http::StatusCode::CREATED,
            headers: http::HeaderMap::new(),
            body: bytes::Bytes::new(),
        })
    }

    fn network_changed(&self) {}
}

/// An actor over [`HoldNet`], linked and connected as far as a send needs, and the Note to Self
/// conversation (accepted without a peer's profile).
struct World {
    actor: Actor<HoldNet>,
    net: Arc<HoldNet>,
    dir: tempfile::TempDir,
    own: kourier_model::UserId,
    core: kourier_backend_api::CoreEnds,
}

async fn world(uploads_open: bool, wire_open: bool) -> World {
    let dir = tempfile::tempdir().unwrap();
    let net = HoldNet::new(uploads_open, wire_open);
    let (ctx, core) =
        kourier_backend_api::AccountContext::new(kourier_backend_api::ContextParams {
            account: kourier_model::AccountId(1),
            uuid: kourier_model::AccountUuid::new_random(),
            start: kourier_backend_api::StartMode::Fresh,
            backend_db: dir.path().join("backend.db"),
            db_key: kourier_backend_api::DbKey::new([5; 32]),
            blob_dir: dir.path().join("blobs"),
            net_rt: tokio::runtime::Handle::current(),
            cancel: CancellationToken::new(),
            clock: Arc::new(kourier_backend_api::SystemClock),
            config: kourier_backend_api::BackendConfig::default(),
            ui: Default::default(),
            local_prefs: Default::default(),
        });
    let actor = Actor::start(ctx, net.clone(), Vec::new(), Vec::new(), Tuning::default())
        .expect("the actor starts");
    let account = crate::receive::tests::fixture().account;
    let own = crate::service_id::aci_user_id(account.aci).unwrap();
    *actor.shared.account.borrow_mut() = Some(account);
    *actor.shared.socket.borrow_mut() = Some(Arc::new(HoldSocket(net.clone())));
    World {
        actor,
        net,
        dir,
        own,
        core,
    }
}

impl World {
    /// A file of `len` bytes in the account's blob directory, as the core would have stored it.
    fn file(&self, name: &str, len: usize) -> Attachment {
        let key = kourier_blob::FileKey::generate();
        let path = self.dir.path().join("blobs").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        kourier_blob::write_all(&path, &key, &vec![7u8; len]).unwrap();
        Attachment {
            kind: AttachmentKind::File,
            mime: "application/pdf".to_owned(),
            size: len as u64,
            local: Some(BlobRef {
                path: name.into(),
                key: BlobKey::new(*key.as_bytes()),
            }),
            ..Attachment::default()
        }
    }

    fn request(&self, kind: OutgoingKind) -> OutgoingRequest {
        OutgoingRequest {
            conv: ConvKey::Direct(self.own.clone()),
            local: "1700000000123".into(),
            sent_at: 1_700_000_000_123,
            kind,
            only: None,
            is_recipient_update: false,
        }
    }

    /// A message of one file.
    fn a_file(&self) -> OutgoingRequest {
        self.request(OutgoingKind::Message {
            body: Body::Media {
                attachments: vec![self.file("core/aa/doc", 50_000)],
                caption: None,
            },
            reply_to: None,
            expiry: None,
        })
    }

    /// A message of one big picture: its re-encode (blocking pool) takes long enough for an
    /// abort to find it at work.
    fn a_big_picture(&self) -> OutgoingRequest {
        let bytes = kourier_media::sample_image(5000, 3600, 3, kourier_media::SampleFormat::Jpeg)
            .expect("a picture");
        let key = kourier_blob::FileKey::generate();
        let path = self.dir.path().join("blobs").join("core/bb/picture");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        kourier_blob::write_all(&path, &key, &bytes).unwrap();
        self.request(OutgoingKind::Message {
            body: Body::Media {
                attachments: vec![Attachment {
                    kind: AttachmentKind::Image,
                    mime: "image/jpeg".to_owned(),
                    size: bytes.len() as u64,
                    dims: Some((5000, 3600)),
                    local: Some(BlobRef {
                        path: "core/bb/picture".into(),
                        key: BlobKey::new(*key.as_bytes()),
                    }),
                    ..Attachment::default()
                }],
                caption: None,
            },
            reply_to: None,
            expiry: None,
        })
    }

    /// Waits until the backend says it prepares the files of `local` (the phase `Preparing`).
    async fn preparing(&mut self, local: MessageId) {
        while let Some(envelope) = self.core.events.recv().await {
            for event in envelope.events {
                if let BackendEvent::TransmitProgress {
                    local: l,
                    phase: Some(kourier_model::SendPhase::Preparing),
                    ..
                } = event
                    && l == local
                {
                    return;
                }
            }
        }
        panic!("the events ended before the phase was said");
    }

    /// An edit whose new text is long enough to be sent as a file of its own (an upload).
    fn a_long_edit(&self) -> OutgoingRequest {
        self.request(OutgoingKind::Edit {
            target: MsgTarget {
                conv: None,
                author: TargetAuthor::Own,
                local: "1700000000000".into(),
                alternates: Vec::new(),
            },
            body: Body::Text(Text::plain("word ".repeat(1000))),
        })
    }

    /// `Command::Transmit` as the core sends it; the outcome arrives on the returned receiver.
    async fn transmit(
        &mut self,
        local: MessageId,
        req: OutgoingRequest,
    ) -> oneshot::Receiver<TransmitOutcome> {
        let (reply, outcome) = oneshot::channel();
        let exit = self
            .actor
            .command(Command::Transmit {
                local,
                req: Box::new(req),
                reply,
            })
            .await;
        assert!(exit.is_none());
        outcome
    }

    /// `Command::AbortTransmit` as the dispatcher sends it. The answer must be in the reply
    /// when the command has been handled: nothing is awaited for it.
    async fn abort(&mut self, local: MessageId) -> AbortAnswer {
        let (reply, mut answer) = oneshot::channel();
        let exit = self
            .actor
            .command(Command::AbortTransmit { local, reply })
            .await;
        assert!(exit.is_none());
        answer.try_recv().expect("answered at once")
    }

    fn temp_files(&self) -> usize {
        std::fs::read_dir(&self.actor.shared.upload_tmp).map_or(0, Iterator::count)
    }
}

async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .unwrap_or_else(|_| panic!("timed out: {what}"))
}

/// An abort during the uploads is answered `Stopping` at once, the attempt then ends `Failed`
/// having put nothing on the wire, its upload request is dropped (it does not go on in the
/// background), its entry is forgotten and no temporary file is left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abort_in_the_upload_phase_answers_stopping_and_sends_nothing() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut world = world(false, true).await;
            let local = MessageId(7);
            let req = world.a_file();
            let outcome = world.transmit(local, req).await;
            within("the upload holds", world.net.arrived.notified()).await;
            assert_eq!(world.net.uploading.load(Ordering::SeqCst), 1);

            assert_eq!(world.abort(local).await, AbortAnswer::Stopping);

            let outcome = within("the attempt answers", outcome).await.unwrap();
            let TransmitOutcome::Failed(error) = outcome else {
                panic!("a stopped attempt fails: {outcome:?}");
            };
            assert_eq!(error.kind, ErrorKind::Internal);
            assert!(error.detail.contains("stopped"), "{}", error.detail);
            assert_eq!(
                world.net.touched.load(Ordering::SeqCst),
                0,
                "nothing was sent"
            );
            // The request of the upload ended with its waiter.
            within("the upload request ends", async {
                while world.net.uploading.load(Ordering::SeqCst) != 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            assert_eq!(world.actor.shared.transmits.len(), 0, "forgotten");
            assert_eq!(world.temp_files(), 0);
            // Asking again after the attempt ended: no such attempt.
            assert_eq!(world.abort(local).await, AbortAnswer::TooLate);
        })
        .await;
}

/// Upload progress, fix U3. The phase `Preparing` is said while the pictures are re-encoded,
/// and an abort then is answered `Stopping` at once like one during the uploads: the attempt
/// ends `Failed` having made no upload request and put nothing on the wire, and the temporary
/// picture the re-encode finishes afterwards goes with the attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abort_while_the_pictures_are_re_encoded_answers_stopping_and_uploads_nothing() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut world = world(false, true).await;
            let local = MessageId(10);
            let req = world.a_big_picture();
            let outcome = world.transmit(local, req).await;
            within("the files are being prepared", world.preparing(local)).await;

            assert_eq!(world.abort(local).await, AbortAnswer::Stopping);

            let outcome = within("the attempt answers", outcome).await.unwrap();
            let TransmitOutcome::Failed(error) = outcome else {
                panic!("a stopped attempt fails: {outcome:?}");
            };
            assert_eq!(error.kind, ErrorKind::Internal);
            assert!(error.detail.contains("stopped"), "{}", error.detail);
            assert_eq!(
                world.net.upload_calls.load(Ordering::SeqCst),
                0,
                "not a byte was uploaded: the stop came during the re-encode"
            );
            assert_eq!(world.net.touched.load(Ordering::SeqCst), 0);
            assert_eq!(world.actor.shared.transmits.len(), 0, "forgotten");
            // The blocking task that was re-encoding finishes after the stop; its file goes.
            within("the temporary files go", async {
                while world.temp_files() != 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
        })
        .await;
}

/// An abort that comes after the uploads (the send is built and is on its way to the wire) is
/// answered `TooLate` and changes nothing: the attempt goes on and ends with what the send
/// came to, not with a stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abort_after_the_uploads_answers_too_late_and_the_send_goes_on() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut world = world(true, false).await;
            let local = MessageId(8);
            let req = world.a_file();
            let mut outcome = world.transmit(local, req).await;
            within(
                "the send reaches the wire",
                world.net.reached_wire.notified(),
            )
            .await;

            assert_eq!(world.abort(local).await, AbortAnswer::TooLate);
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(
                outcome.try_recv().is_err(),
                "the attempt goes on, held at the wire"
            );

            world.net.wire.send_replace(true);
            let outcome = within("the attempt answers", outcome).await.unwrap();
            let TransmitOutcome::Failed(error) = outcome else {
                panic!("the held wire fails here: {outcome:?}");
            };
            assert_ne!(
                error.kind,
                ErrorKind::Internal,
                "not a stop: {}",
                error.detail
            );
            assert_eq!(world.actor.shared.transmits.len(), 0);
        })
        .await;
}

/// An abort for a message with no attempt (never sent, already answered) is `TooLate`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abort_for_an_unknown_message_answers_too_late() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut world = world(true, true).await;
            assert_eq!(world.abort(MessageId(99)).await, AbortAnswer::TooLate);
            assert_eq!(world.actor.shared.transmits.len(), 0);
        })
        .await;
}

/// An edit uploads (its long text goes up as a file) but is never stoppable: an edit's message
/// reached its recipients long ago. Its abort is `TooLate` and the upload goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edit_cannot_be_stopped_while_it_uploads() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut world = world(false, false).await;
            let local = MessageId(9);
            let req = world.a_long_edit();
            let mut outcome = world.transmit(local, req).await;
            within("the upload holds", world.net.arrived.notified()).await;

            assert_eq!(world.abort(local).await, AbortAnswer::TooLate);
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(outcome.try_recv().is_err());
            assert_eq!(
                world.net.uploading.load(Ordering::SeqCst),
                1,
                "the upload goes on"
            );

            world.net.uploads.send_replace(true);
            within(
                "the send reaches the wire",
                world.net.reached_wire.notified(),
            )
            .await;
            world.net.wire.send_replace(true);
            let outcome = within("the attempt answers", outcome).await.unwrap();
            let TransmitOutcome::Failed(error) = outcome else {
                panic!("the held wire fails here: {outcome:?}");
            };
            assert_ne!(
                error.kind,
                ErrorKind::Internal,
                "not a stop: {}",
                error.detail
            );
        })
        .await;
}

//! [`FakeSession`]: a [`Session`] that makes no client, opens no socket and records every call
//! (W2, chunk C3). Test code only (`#[cfg(test)]`): the login machine's tests count what it asked
//! of the client (zero pair-code requests for a refused number, exactly the logouts a refusal
//! needs), and the actor's tests in C4a drive it the same way.
//!
//! Results are scripted: each method pops the next scripted answer and falls back to a plain
//! success when the script is empty.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use kourier_model::{BackendError, DeviceLogout};
use wacore::store::traits::DeviceStore;
use whatsapp_rust::GroupMetadataResult;

use super::{
    AskFailure, DownloadFailure, Outgoing, PairCodeFailure, PictureAnswer, ReadKind, Receive,
    Released, SendFailure, Session, SessionFactory, UploadFailure, Uploaded, Wiring,
};
use crate::appstate::ResyncReport;
use crate::send::address::Namespace;
use crate::send::route::Shape;
use crate::send::upload::SealedSource;
use crate::store::KourierStore;
use crate::tap::Tap;
use crate::translate::HoldState;

/// The calls of every session of one factory, in order (`(session id, call)`).
type SharedLog = Arc<Mutex<Vec<(usize, Call)>>>;

/// One call a [`FakeSession`] received, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Call {
    Start,
    Pause,
    Resume,
    /// The digits that were asked for (a test number, never a real one).
    PairWithCode(String),
    CancelPairCode,
    Logout,
    ReconnectNow,
    /// The connection was dropped for an ack that did not come (D5).
    DropConnection,
    FetchHold,
    Stop,
    /// A frame written (or refused): its stanza id. The whole frame is in [`FakeSession::sent`].
    Send(String),
    /// A receipt: the message ids and its kind (the chat is in [`FakeSession::receipts`]).
    MarkRead(Vec<String>, ReadKind),
    /// A typing state.
    Typing(bool),
    LidMigrated,
    GroupMode,
    /// The read-receipts setting was asked for (fix F5).
    PrivacyKnown,
    /// A file was handed to the upload (W5b), under this shape's upload type.
    Upload(Shape),
    /// A file was asked for (W7).
    Download,
    /// A profile picture was looked up (W8), naming this id of the one already held.
    Picture(Option<String>),
    /// A snapshot of these collections was asked for (W6a1, the start-up resync).
    Resync(Vec<String>),
    /// The overview of every group was asked for (W6a2, B3): one IQ.
    Participating,
    /// The metadata of this many groups was asked for in one batch (W6a2, B3); the groups themselves are in
    /// [`FakeSession::fetched`].
    FetchGroups(usize),
    /// ONE group's metadata was asked for in a single query (W6a2, B6); the group is in [`FakeSession::fetched_one`].
    FetchGroup,
    /// The phone's blocklist was asked for (W6a2, B3): one IQ.
    Blocklist,
    /// A group's invite link was asked for, `reset` replacing it first.
    GroupInvite {
        reset: bool,
    },
}

/// Where the double's `upload` stands still while a gate is set ([`FakeSession::hold_uploads`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UploadHold {
    /// Before the first read of the source (the `w:m` IQ, the resume check).
    BeforeRead,
    /// After this many bytes of the body, the reader still open (a POST whose socket stopped taking bytes).
    AfterBytes(u64),
    /// After the source was read to its end, before the answer (the CDN hashing the body, a socket buffer), the reader
    /// still open: ureq holds the body until the answer's head is in (fix F6).
    AfterRead,
    /// After the source was read to its end AND its reader dropped, before the answer (the gap between two hosts: the
    /// resume check, the 401's `w:m` IQ; fix F6).
    AfterDrop,
}

/// What the double's `upload` read: the shape it was asked for and the source's bytes (the ciphertext).
#[derive(Clone)]
pub(crate) struct UploadSeen {
    pub(crate) shape: Shape,
    pub(crate) bytes: Vec<u8>,
}

/// The synthetic CDN pointer the double answers with (`n`: the upload's place among the session's, from 1).
pub(crate) fn fake_direct_path(n: usize) -> String {
    format!("/v/t62.fake-path-{n}-58302719")
}

/// The `mediaKeyTimestamp` the double answers with.
pub(crate) const FAKE_MEDIA_KEY_TIMESTAMP: i64 = 1_790_000_000;

/// A receipt the double was asked for (no `Debug`: it holds JIDs).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Receipt {
    pub(crate) chat: wacore_binary::Jid,
    pub(crate) sender: Option<wacore_binary::Jid>,
    pub(crate) ids: Vec<String>,
    pub(crate) kind: ReadKind,
}

/// The double. Shared by reference; every method takes `&self`.
#[derive(Default)]
pub(crate) struct FakeSession {
    /// The place of this session among the factory's, for the shared log.
    id: usize,
    /// Every call of every session of one factory, in order (`(session id, call)`).
    log: Mutex<Option<SharedLog>>,
    /// The account's store, when a factory attached it: `stop` looks whether it is still open.
    store: Mutex<Option<KourierStore>>,
    /// For each `stop`: whether the store still answered.
    store_open_at_stop: Mutex<Vec<bool>>,
    /// For each `start`: whether the receive hold of a link was closed already (fix F5).
    hold_closed_at_start: Mutex<Vec<bool>>,
    calls: Mutex<Vec<Call>>,
    pair: Mutex<VecDeque<Result<(), PairCodeFailure>>>,
    logouts: Mutex<VecDeque<DeviceLogout>>,
    holds: Mutex<VecDeque<Result<HoldState, BackendError>>>,
    start_error: Mutex<Option<BackendError>>,
    /// While set, `logout` waits for a permit before it answers.
    logout_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// While set, `drop_connection` waits for a permit before it answers (a socket slow to close).
    drop_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// While set, `fetch_hold` waits for a permit before it answers (a reach-out pull that is slow to answer).
    pull_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// Runs inside `stop`, before it answers: the account's traffic that outlives a pause.
    during_stop: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// While set, `stop` times out: the client's tasks still hold the store (fix F9).
    hang_release: AtomicBool,
    /// A `stop` timed out and `let_go` has not come yet.
    release_pending: AtomicBool,
    /// The answers of `send`, one per call (`Ok`, written, when none is left).
    sends: Mutex<VecDeque<Result<(), SendFailure>>>,
    /// Every frame `send` was given, refused ones too, in order.
    sent: Mutex<Vec<Outgoing>>,
    /// Every receipt asked for, in order.
    receipts: Mutex<Vec<Receipt>>,
    /// Every typing state asked for, by chat, in order.
    typed: Mutex<Vec<(wacore_binary::Jid, bool)>>,
    /// The answers of `mark_read`, one per call (`Ok` when none is left).
    receipt_answers: Mutex<VecDeque<Result<(), SendFailure>>>,
    /// The answers of `privacy_known`, one per call (`Ok` when none is left).
    privacy: Mutex<VecDeque<Result<(), SendFailure>>>,
    /// While set, `lid_migrated` waits for a permit before it answers (a slow ask of the account's mode).
    migrated_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// What `lid_migrated` answers.
    migrated: AtomicBool,
    /// The answers of `group_mode`, one per call (`Ok(Lid)` when none is left).
    modes: Mutex<VecDeque<Result<Namespace, SendFailure>>>,
    /// The answers of `resync`, one per call (every collection asked comes back synced when none is left).
    resyncs: Mutex<VecDeque<Result<ResyncReport, BackendError>>>,
    /// While set, `resync` waits for a permit before it answers (a snapshot that takes its time).
    resync_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// How many `resync` calls were dropped before they answered (the task that awaited them was ended).
    resyncs_cut: AtomicUsize,
    /// The answers of `participating`, one per call (no group when none is left).
    overviews: Mutex<VecDeque<Result<Vec<wacore_binary::Jid>, AskFailure>>>,
    /// The answers of `fetch_groups`, one per call (no result when none is left).
    batches: Mutex<VecDeque<Result<Vec<GroupMetadataResult>, AskFailure>>>,
    /// The groups every `fetch_groups` was asked for, in order (no `Debug`: they are JIDs).
    fetched: Mutex<Vec<Vec<wacore_binary::Jid>>>,
    /// The answers of `fetch_group`, one per call (`NotFound` when none is left).
    singles: Mutex<VecDeque<Result<GroupMetadataResult, AskFailure>>>,
    /// The answers of `group_invite_link`, one per call (`NotConnected` when none is left).
    invite_links: Mutex<VecDeque<Result<String, AskFailure>>>,
    /// The groups every `fetch_group` was asked for, in order.
    fetched_single: Mutex<Vec<wacore_binary::Jid>>,
    /// The answers of `blocklist`, one per call (an empty list when none is left).
    blocklists: Mutex<VecDeque<Result<Vec<wacore_binary::Jid>, AskFailure>>>,
    /// The answers of `upload` after its read, one per call (the CDN's pointer when none is left).
    upload_answers: Mutex<VecDeque<Result<(), UploadFailure>>>,
    /// While set, `upload` stands still at the place it names until a permit of the gate.
    /// What the next picture lookups give, in order (after them: [`AskFailure::NotConnected`]).
    pictures: Mutex<std::collections::VecDeque<Result<PictureAnswer, AskFailure>>>,
    /// What the next `download`s give, in order (after them: [`DownloadFailure::Unreachable`]).
    downloads: Mutex<std::collections::VecDeque<Result<Vec<u8>, DownloadFailure>>>,
    upload_gate: Mutex<Option<(Arc<tokio::sync::Notify>, UploadHold)>>,
    /// Every upload whose source read to its end, in order.
    uploads: Mutex<Vec<UploadSeen>>,
    /// While set, the next `upload` leaves its POST thread behind (fix F5): the permit that ends it.
    linger: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl FakeSession {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The next `upload` answers `answer` once it read its source to the end (one entry per call; the CDN's pointer
    /// when none is left). A source whose read fails answers `Incomplete` whatever is scripted.
    pub(crate) fn script_upload(&self, answer: Result<(), UploadFailure>) {
        self.upload_answers.lock().expect("lock").push_back(answer);
    }

    /// What the next picture lookup gives (one entry per call; [`AskFailure::NotConnected`] when none is left).
    pub(crate) fn script_picture(&self, answer: Result<PictureAnswer, AskFailure>) {
        self.pictures.lock().expect("lock").push_back(answer);
    }

    /// What the next `download` gives (one entry per call; [`DownloadFailure::Unreachable`] when none is left).
    pub(crate) fn script_download(&self, answer: Result<Vec<u8>, DownloadFailure>) {
        self.downloads.lock().expect("lock").push_back(answer);
    }

    /// From now on every `upload` stands still at `at` until a permit of the returned gate.
    pub(crate) fn hold_uploads(&self, at: UploadHold) -> Arc<tokio::sync::Notify> {
        let gate = Arc::new(tokio::sync::Notify::new());
        *self.upload_gate.lock().expect("lock") = Some((gate.clone(), at));
        gate
    }

    /// The next `upload` leaves its POST thread behind, as upstream's blocking POST does when the attempt gives up on
    /// it (a socket that stopped taking bytes; fix F5): it reads 1 KiB, parks the OPEN reader on a blocking thread until
    /// the returned sender sends or is dropped, and answers `Incomplete` at once.
    pub(crate) fn linger_next_upload(&self) -> std::sync::mpsc::Sender<()> {
        let (end, parked) = std::sync::mpsc::channel();
        *self.linger.lock().expect("lock") = Some(parked);
        end
    }

    /// Moves the place `upload` stands still at, keeping the gate of [`hold_uploads`](Self::hold_uploads).
    pub(crate) fn move_upload_hold(&self, at: UploadHold) {
        if let Some((_, hold)) = self.upload_gate.lock().expect("lock").as_mut() {
            *hold = at;
        }
    }

    /// Every upload whose source was read to its end.
    pub(crate) fn uploads(&self) -> Vec<UploadSeen> {
        self.uploads.lock().expect("lock").clone()
    }

    /// The next `resync` answers `answer` (one entry per call; every collection asked synced when none is left).
    pub(crate) fn script_resync(&self, answer: Result<ResyncReport, BackendError>) {
        self.resyncs.lock().expect("lock").push_back(answer);
    }

    /// How many `resync` calls were ended before they answered (a quit that ran out of patience).
    pub(crate) fn resyncs_cut(&self) -> usize {
        self.resyncs_cut.load(Ordering::SeqCst)
    }

    /// From now on every `resync` waits for a permit of the returned gate before it answers.
    pub(crate) fn hold_resyncs(&self) -> Arc<tokio::sync::Notify> {
        let gate = Arc::new(tokio::sync::Notify::new());
        *self.resync_gate.lock().expect("lock") = Some(gate.clone());
        gate
    }

    /// The next `participating` answers `answer` (one entry per call; no group when none is left).
    pub(crate) fn script_participating(&self, answer: Result<Vec<wacore_binary::Jid>, AskFailure>) {
        self.overviews.lock().expect("lock").push_back(answer);
    }

    /// The next `fetch_groups` answers `answer` (one entry per call; no result when none is left).
    pub(crate) fn script_batch(&self, answer: Result<Vec<GroupMetadataResult>, AskFailure>) {
        self.batches.lock().expect("lock").push_back(answer);
    }

    /// The groups every `fetch_groups` was asked for, in order.
    pub(crate) fn fetched(&self) -> Vec<Vec<wacore_binary::Jid>> {
        self.fetched.lock().expect("lock").clone()
    }

    /// The next `fetch_group` answers `answer` (one entry per call; `NotFound` when none is left).
    pub(crate) fn script_group(&self, answer: Result<GroupMetadataResult, AskFailure>) {
        self.singles.lock().expect("lock").push_back(answer);
    }

    /// The next `group_invite_link` answers `answer` (one entry per call; `NotConnected` when
    /// none is left).
    pub(crate) fn script_invite_link(&self, answer: Result<String, AskFailure>) {
        self.invite_links.lock().expect("lock").push_back(answer);
    }

    /// The groups every `fetch_group` was asked for, in order.
    pub(crate) fn fetched_one(&self) -> Vec<wacore_binary::Jid> {
        self.fetched_single.lock().expect("lock").clone()
    }

    /// The next `blocklist` answers `answer` (one entry per call; an empty list when none is left).
    pub(crate) fn script_blocklist(&self, answer: Result<Vec<wacore_binary::Jid>, AskFailure>) {
        self.blocklists.lock().expect("lock").push_back(answer);
    }

    /// The next `pair_with_code` answers `answer` (one entry per call).
    pub(crate) fn script_pair(&self, answer: Result<(), PairCodeFailure>) {
        self.pair.lock().expect("lock").push_back(answer);
    }

    /// The next `logout` answers `outcome` (one entry per call; `Removed` when none is left).
    pub(crate) fn script_logout(&self, outcome: DeviceLogout) {
        self.logouts.lock().expect("lock").push_back(outcome);
    }

    /// From now on `logout` is recorded but answers only after `notify_one` on the returned gate (a
    /// logout that is slow to reach the server).
    pub(crate) fn hold_logouts(&self) -> Arc<tokio::sync::Notify> {
        let gate = Arc::new(tokio::sync::Notify::new());
        *self.logout_gate.lock().expect("lock") = Some(gate.clone());
        gate
    }

    /// From now on `drop_connection` is recorded but returns only after `notify_one` on the returned gate (a
    /// socket that takes its time to close: acks can still come meanwhile).
    pub(crate) fn hold_drops(&self) -> Arc<tokio::sync::Notify> {
        let gate = Arc::new(tokio::sync::Notify::new());
        *self.drop_gate.lock().expect("lock") = Some(gate.clone());
        gate
    }

    /// From now on every `fetch_hold` waits for a permit of the returned gate before it answers.
    pub(crate) fn hold_pulls(&self) -> Arc<tokio::sync::Notify> {
        let gate = Arc::new(tokio::sync::Notify::new());
        *self.pull_gate.lock().expect("lock") = Some(gate.clone());
        gate
    }

    /// The next `fetch_hold` answers `answer`.
    pub(crate) fn script_hold(&self, answer: Result<HoldState, BackendError>) {
        self.holds.lock().expect("lock").push_back(answer);
    }

    /// `stop` runs `work` first (something that moves while the client is being stopped).
    pub(crate) fn on_stop(&self, work: impl Fn() + Send + Sync + 'static) {
        *self.during_stop.lock().expect("lock") = Some(Box::new(work));
    }

    /// From now on `stop` times out (the store is still held) until [`let_go`](Self::let_go).
    pub(crate) fn hang_release(&self) {
        self.hang_release.store(true, Ordering::SeqCst);
    }

    /// The client's tasks let go of the store: the release a timed-out `stop` waited for comes.
    pub(crate) fn let_go(&self) {
        self.hang_release.store(false, Ordering::SeqCst);
        self.release_pending.store(false, Ordering::SeqCst);
    }

    /// `start` fails with `error`.
    pub(crate) fn fail_start(&self, error: BackendError) {
        *self.start_error.lock().expect("lock") = Some(error);
    }

    /// The next `send` answers `answer` (one entry per call; written when none is left).
    pub(crate) fn script_send(&self, answer: Result<(), SendFailure>) {
        self.sends.lock().expect("lock").push_back(answer);
    }

    /// Every frame `send` was given, in order (refused ones too).
    pub(crate) fn sent(&self) -> Vec<Outgoing> {
        self.sent.lock().expect("lock").clone()
    }

    /// The next `mark_read` answers `answer` (one entry per call; Ok when none is left). A refused one is recorded too.
    pub(crate) fn script_receipt(&self, answer: Result<(), SendFailure>) {
        self.receipt_answers.lock().expect("lock").push_back(answer);
    }

    /// Every receipt asked for, in order.
    pub(crate) fn receipts(&self) -> Vec<Receipt> {
        self.receipts.lock().expect("lock").clone()
    }

    /// Every typing state asked for, in order.
    pub(crate) fn typed(&self) -> Vec<(wacore_binary::Jid, bool)> {
        self.typed.lock().expect("lock").clone()
    }

    /// The next `privacy_known` answers `answer` (one entry per call; Ok when none is left).
    pub(crate) fn script_privacy(&self, answer: Result<(), SendFailure>) {
        self.privacy.lock().expect("lock").push_back(answer);
    }

    /// From now on every `lid_migrated` waits for a permit of the returned gate before it answers (an attempt or a
    /// typing state stands still between the gate and the frame).
    pub(crate) fn hold_migrated(&self) -> Arc<tokio::sync::Notify> {
        let gate = Arc::new(tokio::sync::Notify::new());
        *self.migrated_gate.lock().expect("lock") = Some(gate.clone());
        gate
    }

    /// From now on `lid_migrated` answers `migrated`.
    pub(crate) fn set_migrated(&self, migrated: bool) {
        self.migrated.store(migrated, Ordering::SeqCst);
    }

    /// The next `group_mode` answers `answer`.
    pub(crate) fn script_group_mode(&self, answer: Result<Namespace, SendFailure>) {
        self.modes.lock().expect("lock").push_back(answer);
    }

    /// Every call so far.
    pub(crate) fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("lock").clone()
    }

    /// How many times `call` happened.
    pub(crate) fn count(&self, call: &Call) -> usize {
        self.calls().iter().filter(|c| *c == call).count()
    }

    /// How many `pair_with_code` requests were made, whatever the number.
    pub(crate) fn pair_requests(&self) -> usize {
        self.calls()
            .iter()
            .filter(|c| matches!(c, Call::PairWithCode(_)))
            .count()
    }

    /// For each `stop` so far: whether the store still answered when it ran.
    pub(crate) fn store_open_at_stop(&self) -> Vec<bool> {
        self.store_open_at_stop.lock().expect("lock").clone()
    }

    /// For each `start` so far: whether the link's receive hold was closed when the client began.
    pub(crate) fn hold_closed_at_start(&self) -> Vec<bool> {
        self.hold_closed_at_start.lock().expect("lock").clone()
    }

    fn record(&self, call: Call) {
        if let Some(log) = self.log.lock().expect("lock").as_ref() {
            log.lock().expect("lock").push((self.id, call.clone()));
        }
        self.calls.lock().expect("lock").push(call);
    }
}

#[async_trait]
impl Session for FakeSession {
    async fn start(&self) -> Result<(), BackendError> {
        let closed = self
            .store
            .lock()
            .expect("lock")
            .as_ref()
            .is_some_and(|store| store.hold().is_closed());
        self.hold_closed_at_start.lock().expect("lock").push(closed);
        self.record(Call::Start);
        match self.start_error.lock().expect("lock").clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn pause(&self) {
        self.record(Call::Pause);
    }

    async fn resume(&self) {
        self.record(Call::Resume);
    }

    async fn pair_with_code(&self, phone_number: &str) -> Result<(), PairCodeFailure> {
        self.record(Call::PairWithCode(phone_number.to_owned()));
        self.pair
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or(Ok(()))
    }

    async fn cancel_pair_code(&self) {
        self.record(Call::CancelPairCode);
    }

    async fn logout(&self) -> DeviceLogout {
        self.record(Call::Logout);
        let gate = self.logout_gate.lock().expect("lock").clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        self.logouts
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or(DeviceLogout::Removed)
    }

    async fn reconnect_now(&self) {
        self.record(Call::ReconnectNow);
    }

    async fn drop_connection(&self) {
        self.record(Call::DropConnection);
        let gate = self.drop_gate.lock().expect("lock").clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
    }

    async fn fetch_hold(&self) -> Result<HoldState, BackendError> {
        self.record(Call::FetchHold);
        let gate = self.pull_gate.lock().expect("lock").clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        self.holds
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or(Ok(HoldState::Lifted))
    }

    async fn resync(&self, collections: Vec<String>) -> Result<ResyncReport, BackendError> {
        self.record(Call::Resync(collections.clone()));
        let gate = self.resync_gate.lock().expect("lock").clone();
        if let Some(gate) = gate {
            // Counts the call as cut when its future is dropped while it still waits.
            struct Cut<'a>(&'a AtomicUsize, bool);
            impl Drop for Cut<'_> {
                fn drop(&mut self) {
                    if self.1 {
                        self.0.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
            let mut cut = Cut(&self.resyncs_cut, true);
            gate.notified().await;
            cut.1 = false;
        }
        self.resyncs
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or_else(|| {
                Ok(ResyncReport {
                    synced: collections,
                    fatal: Vec::new(),
                })
            })
    }

    async fn stop(&self) -> Released {
        if let Some(work) = self.during_stop.lock().expect("lock").as_ref() {
            work();
        }
        let store = self.store.lock().expect("lock").clone();
        let open = match store {
            Some(store) => store.exists().await.is_ok(),
            None => true,
        };
        self.store_open_at_stop.lock().expect("lock").push(open);
        self.record(Call::Stop);
        if self.hang_release.load(Ordering::SeqCst) {
            self.release_pending.store(true, Ordering::SeqCst);
            return Released::TimedOut;
        }
        Released::Yes
    }

    async fn release_wait(&self, _limit: Duration) -> Released {
        if self.release_pending.load(Ordering::SeqCst) {
            Released::TimedOut
        } else {
            Released::Yes
        }
    }

    async fn send(&self, out: Outgoing) -> Result<(), SendFailure> {
        self.record(Call::Send(out.id.clone()));
        self.sent.lock().expect("lock").push(out);
        self.sends
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or(Ok(()))
    }

    async fn profile_picture(
        &self,
        _chat: wacore_binary::Jid,
        _group: bool,
        known: Option<String>,
    ) -> Result<PictureAnswer, AskFailure> {
        self.record(Call::Picture(known));
        self.pictures
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or(Err(AskFailure::NotConnected))
    }

    async fn download(
        &self,
        _params: whatsapp_rust::download::DownloadParams,
        mut sink: crate::download::BlobSink,
    ) -> Result<crate::download::BlobSink, DownloadFailure> {
        self.record(Call::Download);
        let answer = self.downloads.lock().expect("lock").pop_front();
        let bytes = answer.unwrap_or(Err(DownloadFailure::Unreachable))?;
        std::io::Write::write_all(&mut sink, &bytes).map_err(|_| DownloadFailure::Sink)?;
        Ok(sink)
    }

    async fn upload(
        &self,
        source: SealedSource,
        _info: wacore::upload::EncryptedMediaInfo,
        shape: Shape,
    ) -> Result<Uploaded, UploadFailure> {
        use std::io::Read as _;
        use wacore::upload::UploadSource as _;

        self.record(Call::Upload(shape));
        // The place is looked up at each point (a test may move it on: `move_upload_hold`).
        let wait = |at: UploadHold| {
            let gate = self.upload_gate.lock().expect("lock").clone();
            async move {
                if let Some((gate, hold)) = gate
                    && hold == at
                {
                    gate.notified().await;
                }
            }
        };
        wait(UploadHold::BeforeRead).await;
        let parked = self.linger.lock().expect("lock").take();
        if let Some(parked) = parked {
            let opened = tokio::task::spawn_blocking(move || {
                let mut reader = source.reader_from(0)?;
                let mut bytes = Vec::new();
                (&mut reader).take(1024).read_to_end(&mut bytes)?;
                Ok::<_, std::io::Error>(reader)
            })
            .await;
            let Ok(Ok(reader)) = opened else {
                return Err(UploadFailure::Incomplete);
            };
            tokio::task::spawn_blocking(move || {
                let _ = parked.recv();
                drop(reader);
            });
            return Err(UploadFailure::Incomplete);
        }
        let gate = self.upload_gate.lock().expect("lock").clone();
        // Upstream reads the source on the blocking pool (`wacore::runtime::blocking`): so does the double, in
        // two parts when it stands still in the middle of the body (the reader stays open meanwhile). It reads
        // EXACTLY the source's length and never asks again, as ureq does with a sized body (fix F6): a read that
        // answers 0 never reaches the source's reader.
        let len = source.len();
        let first = match gate.as_ref().map(|(_, at)| *at) {
            Some(UploadHold::AfterBytes(n)) => n.min(len),
            _ => len,
        };
        let read = tokio::task::spawn_blocking(move || {
            let mut bytes = Vec::new();
            let mut reader = source.reader_from(0)?;
            (&mut reader).take(first).read_to_end(&mut bytes)?;
            Ok::<_, std::io::Error>((reader, bytes))
        })
        .await;
        let Ok(Ok((mut reader, mut bytes))) = read else {
            return Err(UploadFailure::Incomplete);
        };
        if first != len {
            wait(UploadHold::AfterBytes(first)).await;
            let rest = tokio::task::spawn_blocking(move || {
                (&mut reader).take(len - first).read_to_end(&mut bytes)?;
                Ok::<_, std::io::Error>((reader, bytes))
            })
            .await;
            let Ok(Ok((whole_reader, whole))) = rest else {
                return Err(UploadFailure::Incomplete);
            };
            (reader, bytes) = (whole_reader, whole);
        }
        // The place is looked up again: a test may have moved it on.
        let dropped = self
            .upload_gate
            .lock()
            .expect("lock")
            .as_ref()
            .is_some_and(|(_, at)| *at == UploadHold::AfterDrop);
        if dropped {
            drop(reader);
            wait(UploadHold::AfterDrop).await;
        } else {
            wait(UploadHold::AfterRead).await;
            drop(reader);
        }
        let n = {
            let mut uploads = self.uploads.lock().expect("lock");
            uploads.push(UploadSeen { shape, bytes });
            uploads.len()
        };
        let answer = self
            .upload_answers
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or(Ok(()));
        answer.map(|()| Uploaded {
            url: format!("https://media.example.test{}", fake_direct_path(n)),
            direct_path: fake_direct_path(n),
            media_key_timestamp: FAKE_MEDIA_KEY_TIMESTAMP,
        })
    }

    async fn mark_read(
        &self,
        chat: wacore_binary::Jid,
        sender: Option<wacore_binary::Jid>,
        ids: Vec<String>,
        kind: ReadKind,
    ) -> Result<(), SendFailure> {
        self.record(Call::MarkRead(ids.clone(), kind));
        self.receipts.lock().expect("lock").push(Receipt {
            chat,
            sender,
            ids,
            kind,
        });
        self.receipt_answers
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or(Ok(()))
    }

    async fn typing(&self, chat: wacore_binary::Jid, on: bool) -> Result<(), SendFailure> {
        self.record(Call::Typing(on));
        self.typed.lock().expect("lock").push((chat, on));
        Ok(())
    }

    async fn privacy_known(&self) -> Result<(), SendFailure> {
        self.record(Call::PrivacyKnown);
        self.privacy
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or(Ok(()))
    }

    async fn lid_migrated(&self) -> bool {
        self.record(Call::LidMigrated);
        let gate = self.migrated_gate.lock().expect("lock").clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        self.migrated.load(Ordering::SeqCst)
    }

    async fn group_mode(&self, _group: wacore_binary::Jid) -> Result<Namespace, SendFailure> {
        self.record(Call::GroupMode);
        self.modes
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or(Ok(Namespace::Lid))
    }

    async fn participating(&self) -> Result<Vec<wacore_binary::Jid>, AskFailure> {
        self.record(Call::Participating);
        self.overviews
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or_else(|| Ok(Vec::new()))
    }

    async fn fetch_groups(
        &self,
        groups: Vec<wacore_binary::Jid>,
    ) -> Result<Vec<GroupMetadataResult>, AskFailure> {
        self.record(Call::FetchGroups(groups.len()));
        self.fetched.lock().expect("lock").push(groups);
        self.batches
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or_else(|| Ok(Vec::new()))
    }

    async fn group_invite_link(
        &self,
        _group: wacore_binary::Jid,
        reset: bool,
    ) -> Result<String, AskFailure> {
        self.record(Call::GroupInvite { reset });
        self.invite_links
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or(Err(AskFailure::NotConnected))
    }

    async fn fetch_group(
        &self,
        group: wacore_binary::Jid,
    ) -> Result<GroupMetadataResult, AskFailure> {
        self.record(Call::FetchGroup);
        self.fetched_single
            .lock()
            .expect("lock")
            .push(group.clone());
        self.singles
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or(Ok(GroupMetadataResult::NotFound(group)))
    }

    async fn blocklist(&self) -> Result<Vec<wacore_binary::Jid>, AskFailure> {
        self.record(Call::Blocklist);
        self.blocklists
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or_else(|| Ok(Vec::new()))
    }
}

/// One client the factory made.
#[derive(Clone)]
pub(crate) struct Made {
    pub(crate) session: Arc<FakeSession>,
    /// The client's end of its signals: a test delivers what the real client's handler would.
    pub(crate) tap: Tap,
    /// The receive path the actor wired this client to: a test commits to the inbox the way the
    /// hook does.
    pub(crate) receive: Receive,
    /// The account's store handle (its counters are what rule (b)'s quiet look reads).
    pub(crate) store: KourierStore,
}

/// What a test sees of its [`FakeFactory`] after the factory went into the actor.
#[derive(Clone, Default)]
pub(crate) struct FactoryView {
    made: Arc<Mutex<Vec<Made>>>,
    queue: Arc<Mutex<VecDeque<Arc<FakeSession>>>>,
    log: SharedLog,
}

impl FactoryView {
    /// The next client made is this session (else a plain one).
    pub(crate) fn queue(&self, session: Arc<FakeSession>) {
        self.queue.lock().expect("lock").push_back(session);
    }

    /// The clients made so far.
    pub(crate) fn made(&self) -> Vec<Made> {
        self.made.lock().expect("lock").clone()
    }

    /// Every call of every session, in order.
    pub(crate) fn log(&self) -> Vec<(usize, Call)> {
        self.log.lock().expect("lock").clone()
    }
}

/// The host name the test factory says the application gave.
pub(crate) const FAKE_HOST: &str = "testhost";

/// A [`SessionFactory`] of [`FakeSession`]s.
pub(crate) struct FakeFactory {
    view: FactoryView,
}

impl FakeFactory {
    pub(crate) fn new() -> (Self, FactoryView) {
        let view = FactoryView::default();
        (Self { view: view.clone() }, view)
    }
}

impl SessionFactory for FakeFactory {
    fn device_name(&self) -> String {
        crate::props::device_name(FAKE_HOST)
    }

    fn make(&mut self, wiring: Wiring) -> Arc<dyn Session> {
        let queued = self.view.queue.lock().expect("lock").pop_front();
        let session = queued.unwrap_or_default();
        let mut made = self.view.made.lock().expect("lock");
        let store = wiring.store.clone();
        // `id` is set once, here; a session queued by a test is built with `FakeSession::new`.
        let session = Arc::new(FakeSession::wired(
            &session,
            made.len(),
            self.view.log.clone(),
            wiring.store.clone(),
        ));
        made.push(Made {
            session: session.clone(),
            tap: wiring.tap,
            receive: wiring.receive,
            store,
        });
        session
    }
}

impl FakeSession {
    /// A session that takes over the scripts of `scripted` and is attached to the factory's log
    /// and the account's store.
    fn wired(scripted: &FakeSession, id: usize, log: SharedLog, store: KourierStore) -> Self {
        Self {
            id,
            log: Mutex::new(Some(log)),
            store: Mutex::new(Some(store)),
            store_open_at_stop: Mutex::new(Vec::new()),
            hold_closed_at_start: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            pair: Mutex::new(std::mem::take(&mut *scripted.pair.lock().expect("lock"))),
            logouts: Mutex::new(std::mem::take(&mut *scripted.logouts.lock().expect("lock"))),
            holds: Mutex::new(std::mem::take(&mut *scripted.holds.lock().expect("lock"))),
            start_error: Mutex::new(scripted.start_error.lock().expect("lock").take()),
            logout_gate: Mutex::new(None),
            drop_gate: Mutex::new(scripted.drop_gate.lock().expect("lock").take()),
            pull_gate: Mutex::new(scripted.pull_gate.lock().expect("lock").take()),
            during_stop: Mutex::new(scripted.during_stop.lock().expect("lock").take()),
            hang_release: AtomicBool::new(scripted.hang_release.load(Ordering::SeqCst)),
            release_pending: AtomicBool::new(false),
            sends: Mutex::new(std::mem::take(&mut *scripted.sends.lock().expect("lock"))),
            sent: Mutex::new(Vec::new()),
            downloads: Mutex::new(std::mem::take(
                &mut *scripted.downloads.lock().expect("lock"),
            )),
            pictures: Mutex::new(std::mem::take(
                &mut *scripted.pictures.lock().expect("lock"),
            )),
            receipts: Mutex::new(Vec::new()),
            typed: Mutex::new(Vec::new()),
            receipt_answers: Mutex::new(std::mem::take(
                &mut *scripted.receipt_answers.lock().expect("lock"),
            )),
            privacy: Mutex::new(std::mem::take(&mut *scripted.privacy.lock().expect("lock"))),
            migrated_gate: Mutex::new(scripted.migrated_gate.lock().expect("lock").take()),
            migrated: AtomicBool::new(scripted.migrated.load(Ordering::SeqCst)),
            modes: Mutex::new(std::mem::take(&mut *scripted.modes.lock().expect("lock"))),
            resyncs: Mutex::new(std::mem::take(&mut *scripted.resyncs.lock().expect("lock"))),
            resync_gate: Mutex::new(scripted.resync_gate.lock().expect("lock").take()),
            resyncs_cut: AtomicUsize::new(0),
            overviews: Mutex::new(std::mem::take(
                &mut *scripted.overviews.lock().expect("lock"),
            )),
            batches: Mutex::new(std::mem::take(&mut *scripted.batches.lock().expect("lock"))),
            fetched: Mutex::new(Vec::new()),
            singles: Mutex::new(std::mem::take(&mut *scripted.singles.lock().expect("lock"))),
            invite_links: Mutex::new(std::mem::take(
                &mut *scripted.invite_links.lock().expect("lock"),
            )),
            fetched_single: Mutex::new(Vec::new()),
            blocklists: Mutex::new(std::mem::take(
                &mut *scripted.blocklists.lock().expect("lock"),
            )),
            upload_answers: Mutex::new(std::mem::take(
                &mut *scripted.upload_answers.lock().expect("lock"),
            )),
            upload_gate: Mutex::new(scripted.upload_gate.lock().expect("lock").take()),
            uploads: Mutex::new(Vec::new()),
            linger: Mutex::new(scripted.linger.lock().expect("lock").take()),
        }
    }
}

#[cfg(test)]
mod tests;

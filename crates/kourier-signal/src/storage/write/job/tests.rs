//! The write job (A10c chunk C2) over a scripted Storage Service that applies a PUT as the mock
//! server does (409 unless the version is the next one) and decrypts everything it is sent, so a
//! test sees what the phone would read. The clock is a `FakeClock`: `Done::RepeatAt` is exact.
//! Synthetic ids only.

use std::collections::{HashMap, VecDeque};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, StatusCode};
use kourier_backend_api::{BackendEvent, Clock as _, FakeClock, decode_outbox_row};
use kourier_model::{ConvKey, ConversationChange, ConversationPatch, Patch};
use libsignal_core::{Aci, ServiceId};
use tokio::sync::Notify;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::*;
use crate::crypto::decrypt_gcm;
use crate::net::{
    AuthCredentials, ChatEvent, ChatRequest, ChatResponse, ChatSocket, Connected, DownloadSink,
    MultiRecipientOutcome, MultiRecipientSend, ProvisioningEvent, TransportError, UploadForm,
    UploadRequest, UploadTooLarge,
};
use crate::proto::manifest_record::Identifier;
use crate::proto::manifest_record::identifier::Type as ItemType;
use crate::proto::storage_record::Record;
use crate::receive::tests::{Fixture, fixture};
use crate::service_id::aci_user_id;
use crate::storage::intents::{ARCHIVED, IDENTITY, Intent, MARKED_UNREAD, MUTED_UNTIL};
use crate::storage::{ACCOUNT_TARGET, Decrypted, LastZone, contact_target, merge_zone};

/// What the fixture's account holds as its storage key.
const STORAGE_KEY: [u8; 32] = [9; 32];
const IKM: [u8; 32] = [8; 32];
const START: u64 = 1_900_000_000_000;

const ADA: u128 = 0xada;
const BOB: u128 = 0xb0b;
const CHIARA: u128 = 0xc1a;

fn aci(n: u128) -> Aci {
    Aci::from(Uuid::from_u128(n))
}

fn chat(n: u128) -> ConvKey {
    ConvKey::Direct(aci_user_id(aci(n)).unwrap())
}

fn person(n: u128) -> String {
    contact_target(&ServiceId::from(aci(n)).service_id_string())
}

fn key(n: u8) -> Vec<u8> {
    vec![n; 16]
}

fn item(key: u8, kind: ItemType, record: Record) -> Decrypted {
    let record = proto::StorageRecord {
        record: Some(record),
    };
    Decrypted {
        key: vec![key; 16],
        kind: kind as i32,
        raw: Zeroizing::new(record.encode_to_vec()),
        record,
    }
}

fn contact(key: u8, n: u128, edit: impl FnOnce(&mut proto::ContactRecord)) -> Decrypted {
    let mut record = proto::ContactRecord {
        aci_binary: Uuid::from_u128(n).as_bytes().to_vec(),
        ..Default::default()
    };
    edit(&mut record);
    item(key, ItemType::Contact, Record::Contact(record))
}

fn account_record(key: u8, edit: impl FnOnce(&mut proto::AccountRecord)) -> Decrypted {
    let mut record = proto::AccountRecord::default();
    edit(&mut record);
    item(key, ItemType::Account, Record::Account(record))
}

fn contact_pin(n: u128) -> proto::account_record::PinnedConversation {
    proto::account_record::PinnedConversation::decode(
        &crate::storage::wire::contact_pin_entry(aci(n))[..],
    )
    .unwrap()
}

// ---------------------------------------------------------------------------------------------
// The scripted Storage Service
// ---------------------------------------------------------------------------------------------

/// How the service answers the next PUT (with none left: [`Answer::Apply`]).
enum Answer {
    /// Applied unless the version is not the next one (then 409, as the mock).
    Apply,
    /// Applied, and the answer never arrives.
    ApplyLost,
    /// Applied, and a gateway in front answers this status (a 5xx: R3).
    ApplyThenStatus(u16),
    /// Nothing reaches the server and no answer arrives.
    Lost,
    /// This status, with this `Retry-After`; nothing is applied.
    Status(u16, Option<&'static str>),
    /// The phone's write gets there first, so this PUT meets a newer version.
    PhoneFirst(Box<Decrypted>, Vec<u8>),
}

/// Where the service waits inside a request: it says it got there, then waits to be released.
#[derive(Clone)]
struct Hold {
    reached: Arc<Notify>,
    release: Arc<Notify>,
}

impl Hold {
    fn new() -> Self {
        Self {
            reached: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        }
    }

    async fn wait_reached(&self) {
        tokio::time::timeout(Duration::from_secs(60), self.reached.notified())
            .await
            .expect("the run gets to the request");
    }
}

/// A write the server applied, as the phone reads it.
struct Seen {
    manifest: proto::ManifestRecord,
    records: Vec<(Vec<u8>, Vec<u8>)>,
    deletes: Vec<Vec<u8>>,
}

impl Seen {
    fn record(&self, index: usize) -> proto::StorageRecord {
        proto::StorageRecord::decode(&self.records[index].1[..]).unwrap()
    }

    fn contact(&self, index: usize) -> proto::ContactRecord {
        match self.record(index).record {
            Some(Record::Contact(contact)) => contact,
            _ => panic!("not a contact record"),
        }
    }
}

#[derive(Default)]
struct State {
    version: u64,
    ikm: Vec<u8>,
    identifiers: Vec<Identifier>,
    items: HashMap<Vec<u8>, Vec<u8>>,
    answers: VecDeque<Answer>,
    /// The manifest answers this status instead.
    manifest_status: Option<u16>,
    /// The manifest answers 200 with bytes that do not decode.
    manifest_garbage: bool,
    /// `/v1/storage/auth` answers this status instead.
    auth_status: Option<u16>,
    manifest_paths: Vec<String>,
    auths: usize,
    /// The `Authorization` of each PUT (the credentials differ per `/v1/storage/auth` call).
    put_auths: Vec<Vec<u8>>,
    reads: usize,
    puts: usize,
    written: Vec<Seen>,
    hold_read: Option<Hold>,
    hold_put: Option<Hold>,
    /// A file made at the first PUT (the kill tests' way to tell what reached the server).
    marker: Option<PathBuf>,
    /// The SECOND `/v1/storage/auth` request waits here (the credentials asked for again after a
    /// 401 or 403).
    hold_second_auth: Option<Hold>,
}

fn sealed(plain: &[u8], key: &[u8; 32]) -> Vec<u8> {
    crate::crypto::encrypt_gcm(key, [3; 12], plain)
}

impl State {
    /// The phone writes: `replaces` goes, `new` comes, the version moves on.
    fn phone_writes(&mut self, replaces: &[u8], new: &Decrypted) {
        self.identifiers.retain(|i| i.raw != replaces);
        self.items.remove(replaces);
        self.identifiers.push(Identifier {
            raw: new.key.clone(),
            r#type: new.kind,
        });
        let ikm = self.ikm.clone();
        self.items.insert(
            new.key.clone(),
            sealed(&new.raw, &item_key(&STORAGE_KEY, Some(&ikm), &new.key)),
        );
        self.version += 1;
    }

    fn manifest(&self) -> proto::StorageManifest {
        let record = proto::ManifestRecord {
            version: self.version,
            source_device: 1,
            identifiers: self.identifiers.clone(),
            record_ikm: self.ikm.clone(),
        };
        proto::StorageManifest {
            version: self.version,
            value: sealed(
                &record.encode_to_vec(),
                &manifest_key(&STORAGE_KEY, self.version),
            ),
        }
    }

    /// What the server does with a write: 409 unless it is the next version; every value must
    /// decrypt with the key the phone reads it with.
    fn apply(&mut self, body: &[u8]) -> StatusCode {
        let write = proto::WriteOperation::decode(body).expect("a write operation");
        assert!(!write.clear_all, "a linked device never clears the store");
        let manifest = write.manifest.expect("a manifest");
        if manifest.version != self.version + 1 {
            return StatusCode::CONFLICT;
        }
        let plain = decrypt_gcm(
            &manifest_key(&STORAGE_KEY, manifest.version),
            &manifest.value,
        )
        .expect("the manifest is sealed with the key of its version");
        let manifest_record = proto::ManifestRecord::decode(&plain[..]).expect("a manifest record");
        assert_eq!(manifest_record.version, manifest.version);
        let ikm = manifest_record.record_ikm.clone();
        let mut records = Vec::new();
        for item in &write.insert_item {
            let plain = decrypt_gcm(
                &item_key(
                    &STORAGE_KEY,
                    Some(ikm.as_slice()).filter(|i| !i.is_empty()),
                    &item.key,
                ),
                &item.value,
            )
            .expect("a record the phone can read");
            records.push((item.key.clone(), plain.to_vec()));
            self.items.insert(item.key.clone(), item.value.clone());
        }
        for delete in &write.delete_key {
            self.items.remove(delete);
        }
        self.version = manifest.version;
        self.identifiers = manifest_record.identifiers.clone();
        self.ikm = ikm;
        self.written.push(Seen {
            manifest: manifest_record,
            records,
            deletes: write.delete_key,
        });
        StatusCode::OK
    }
}

#[derive(Default)]
struct Service(Mutex<State>);

impl Service {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn answer(status: StatusCode, body: Vec<u8>) -> crate::net::HttpsResponse {
    crate::net::HttpsResponse {
        status,
        headers: HeaderMap::new(),
        body: Bytes::from(body),
    }
}

struct Socket {
    service: Arc<Service>,
}

impl ChatSocket for Socket {
    async fn request(&self, _request: ChatRequest) -> Result<ChatResponse, TransportError> {
        let (status, auths, hold) = {
            let mut state = self.service.lock();
            state.auths += 1;
            let hold = if state.auths == 2 {
                state.hold_second_auth.take()
            } else {
                None
            };
            (state.auth_status, state.auths, hold)
        };
        if let Some(hold) = hold {
            hold.reached.notify_one();
            hold.release.notified().await;
        }
        Ok(ChatResponse {
            status: status.map_or(StatusCode::OK, |s| StatusCode::from_u16(s).unwrap()),
            message: None,
            headers: HeaderMap::new(),
            // A new password at each call: a PUT shows which credentials it was made with.
            body: Bytes::from(format!(r#"{{"username":"u","password":"p{auths}"}}"#)),
        })
    }

    async fn send_multi_recipient(
        &self,
        _send: MultiRecipientSend,
    ) -> Result<MultiRecipientOutcome, TransportError> {
        Err(TransportError::Disconnected)
    }

    async fn get_upload_form(
        &self,
        _upload_length: u64,
    ) -> Result<Result<UploadForm, UploadTooLarge>, TransportError> {
        Err(TransportError::Disconnected)
    }

    async fn disconnect(&self) {}
}

impl SignalTransport for Service {
    type Socket = Socket;

    async fn connect_auth(
        &self,
        _credentials: &AuthCredentials,
    ) -> Result<Connected<Socket, ChatEvent>, TransportError> {
        Err(TransportError::Disconnected)
    }

    async fn connect_unauth(&self) -> Result<Connected<Socket, ChatEvent>, TransportError> {
        Err(TransportError::Disconnected)
    }

    async fn connect_provisioning(
        &self,
    ) -> Result<Connected<Socket, ProvisioningEvent>, TransportError> {
        Err(TransportError::Disconnected)
    }

    async fn https(
        &self,
        request: HttpsRequest,
    ) -> Result<crate::net::HttpsResponse, TransportError> {
        if request.method == Method::GET && request.path.starts_with("/v1/storage/manifest") {
            let mut state = self.lock();
            state.manifest_paths.push(request.path.clone());
            if let Some(status) = state.manifest_status {
                return Ok(answer(StatusCode::from_u16(status).unwrap(), Vec::new()));
            }
            if state.manifest_garbage {
                return Ok(answer(StatusCode::OK, vec![0xff; 3]));
            }
            return Ok(answer(StatusCode::OK, state.manifest().encode_to_vec()));
        }
        if request.method == Method::PUT && request.path == "/v1/storage/read" {
            let hold = self.lock().hold_read.take();
            if let Some(hold) = hold {
                hold.reached.notify_one();
                hold.release.notified().await;
            }
            let wanted = proto::ReadOperation::decode(&request.body.expect("a body")[..])
                .expect("a read operation");
            let mut state = self.lock();
            state.reads += 1;
            let items = wanted
                .read_key
                .iter()
                .filter_map(|k| {
                    state.items.get(k).map(|v| proto::StorageItem {
                        key: k.clone(),
                        value: v.clone(),
                    })
                })
                .collect();
            return Ok(answer(
                StatusCode::OK,
                proto::StorageItems { items }.encode_to_vec(),
            ));
        }
        if request.method == Method::PUT && request.path == "/v1/storage/" {
            assert_eq!(request.timeout, Duration::from_secs(30));
            assert_eq!(
                request.headers.get(http::header::CONTENT_TYPE),
                Some(&HeaderValue::from_static("application/x-protobuf"))
            );
            assert!(request.headers.contains_key(http::header::AUTHORIZATION));
            let hold = self.lock().hold_put.take();
            if let Some(hold) = hold {
                hold.reached.notify_one();
                hold.release.notified().await;
            }
            let body = request.body.expect("a body");
            let mut state = self.lock();
            state.puts += 1;
            let auth = request.headers.get(http::header::AUTHORIZATION);
            state
                .put_auths
                .push(auth.map(|a| a.as_bytes().to_vec()).unwrap_or_default());
            if let Some(marker) = &state.marker {
                std::fs::write(marker, b"").unwrap();
            }
            return match state.answers.pop_front().unwrap_or(Answer::Apply) {
                Answer::Apply => {
                    let status = state.apply(&body);
                    // The body of a 409 is the mock's manifest; this one would not decode, and
                    // the job must not try (M1).
                    Ok(answer(status, vec![0xff; 3]))
                }
                Answer::ApplyLost => {
                    assert_eq!(state.apply(&body), StatusCode::OK);
                    Err(TransportError::Timeout)
                }
                Answer::ApplyThenStatus(status) => {
                    assert_eq!(state.apply(&body), StatusCode::OK);
                    Ok(answer(StatusCode::from_u16(status).unwrap(), Vec::new()))
                }
                Answer::Lost => Err(TransportError::Disconnected),
                Answer::Status(status, retry_after) => {
                    let mut response = answer(StatusCode::from_u16(status).unwrap(), Vec::new());
                    if let Some(retry_after) = retry_after {
                        response.headers.insert(
                            http::header::RETRY_AFTER,
                            HeaderValue::from_static(retry_after),
                        );
                    }
                    Ok(response)
                }
                Answer::PhoneFirst(new, replaces) => {
                    state.phone_writes(&replaces, &new);
                    let status = state.apply(&body);
                    Ok(answer(status, vec![0xff; 3]))
                }
            };
        }
        Err(TransportError::Disconnected)
    }

    async fn https_download(
        &self,
        _request: HttpsRequest,
        _sink: DownloadSink,
    ) -> Result<StatusCode, TransportError> {
        Err(TransportError::Disconnected)
    }

    async fn https_upload(
        &self,
        _request: UploadRequest,
    ) -> Result<crate::net::HttpsResponse, TransportError> {
        Err(TransportError::Disconnected)
    }

    fn network_changed(&self) {}
}

// ---------------------------------------------------------------------------------------------
// The world
// ---------------------------------------------------------------------------------------------

struct World {
    f: Fixture,
    clock: FakeClock,
    service: Arc<Service>,
    shared: Rc<Shared<Service>>,
    socket: Arc<Socket>,
    _core: Box<dyn std::any::Any>,
}

impl World {
    /// A linked account with the switch off. On a runtime: the transport's futures run there.
    fn new() -> Self {
        let f = fixture();
        let clock = FakeClock::new(START);
        let service = Arc::new(Service::default());
        let test = crate::actor::testing::shared_over_clock(
            service.clone(),
            f.db.clone(),
            f.account.clone(),
            Vec::new(),
            Arc::new(clock.clone()),
        );
        Self {
            socket: Arc::new(Socket {
                service: service.clone(),
            }),
            shared: Rc::new(test.shared),
            _core: Box::new(test.core),
            f,
            clock,
            service,
        }
    }

    fn on() -> Self {
        let w = Self::new();
        w.switch(true);
        w
    }

    fn switch(&self, on: bool) {
        self.f
            .db
            .zone(|tx| -> Result<()> {
                tx.execute("UPDATE account SET roaming_writes = ?1 WHERE id = 1", [on])?;
                Ok(())
            })
            .unwrap();
    }

    /// What `Command::SetRoamingWrites { on: false }` does in its zone.
    fn switch_off_as_the_command_does(&self) {
        let now = self.now();
        self.f
            .db
            .zone(|tx| -> Result<()> {
                tx.execute("UPDATE account SET roaming_writes = 0 WHERE id = 1", [])?;
                intents::drop_all(tx, &self.f.account, now, &self.shared.wake)?;
                Ok(())
            })
            .unwrap();
    }

    fn now(&self) -> i64 {
        i64::try_from(self.clock.now_ms()).unwrap()
    }

    /// Later by `ms`.
    fn later(&self, ms: u64) {
        self.clock.advance(Duration::from_millis(ms));
    }

    /// The phone and this device agree: the service lists `items` at `version`, and they are
    /// merged here at that version. Forgets what the merge told the core.
    fn seed(&self, version: u64, items: Vec<Decrypted>) {
        let now = self.now();
        self.f
            .db
            .zone(|tx| {
                merge_zone(
                    tx,
                    &self.f.account,
                    &items,
                    Some(&LastZone {
                        version,
                        removed: &[],
                    }),
                    false,
                    now,
                    &self.shared.wake,
                )
            })
            .unwrap();
        self.events();
        let mut state = self.service.lock();
        state.version = version;
        state.ikm = IKM.to_vec();
        state.identifiers = items
            .iter()
            .map(|i| Identifier {
                raw: i.key.clone(),
                r#type: i.kind,
            })
            .collect();
        state.items = items
            .iter()
            .map(|i| {
                (
                    i.key.clone(),
                    sealed(&i.raw, &item_key(&STORAGE_KEY, Some(&IKM), &i.key)),
                )
            })
            .collect();
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.service.lock()
    }

    fn click(&self, change: ConversationChange, conv: &ConvKey) {
        let now = self.now();
        self.f
            .db
            .zone(|tx| intents::intend(tx, &self.f.account, &change, conv, now, &self.shared.wake))
            .unwrap()
            .expect("the gate lets it through");
    }

    fn archive(&self, n: u128) {
        self.click(ConversationChange::SetArchived { archived: true }, &chat(n));
    }

    fn mute(&self, n: u128, until: Option<i64>) {
        self.click(ConversationChange::SetMuted { until }, &chat(n));
    }

    fn events(&self) -> Vec<BackendEvent> {
        let events = self
            .f
            .db
            .read(|c| kourier_db::outbox::peek(c, 10_000))
            .unwrap()
            .iter()
            .flat_map(|row| decode_outbox_row(&row.envelope).unwrap())
            .collect();
        self.f
            .db
            .zone(|tx| -> Result<()> {
                tx.execute("DELETE FROM core_outbox", [])?;
                Ok(())
            })
            .unwrap();
        events
    }

    fn job(&self) -> Job {
        let now = self.now();
        self.f
            .db
            .zone(|tx| -> Result<Job> {
                if jobs::count(tx, JobKind::StorageWrite)? == 0 {
                    jobs::delay_to(tx, JobKind::StorageWrite, "", now, now)?;
                }
                Ok(tx.query_row(
                    "SELECT id, key, attempts, expires_at, payload, requests FROM jobs
                     WHERE kind = 'storage_write'",
                    [],
                    |r| {
                        Ok(Job {
                            id: r.get(0)?,
                            kind: JobKind::StorageWrite,
                            key: r.get(1)?,
                            attempts: r.get(2)?,
                            expires_at: r.get(3)?,
                            payload: r.get(4)?,
                            requests: r.get(5)?,
                        })
                    },
                )?)
            })
            .unwrap()
    }

    /// One run as the job runner makes it: `run`, then `jobs::finish`. Only what stops every job
    /// may stop this one (Y3).
    async fn run(&self) -> Done {
        let job = self.job();
        let done = run(&self.shared, &self.socket, &job)
            .await
            .expect("every failure of the write job is a RepeatAt");
        let now = self.now();
        self.f
            .db
            .zone(|tx| jobs::finish(tx, &job, done, now))
            .unwrap();
        done
    }

    fn intent(&self, target: &str, field: &str) -> Option<Intent> {
        self.f
            .db
            .read(intents::all_intents)
            .unwrap()
            .into_iter()
            .find(|i| i.target == target && i.field == field)
    }

    fn intents(&self) -> Vec<Intent> {
        self.f.db.read(intents::all_intents).unwrap()
    }

    fn row(&self) -> Option<InFlight> {
        self.f
            .db
            .zone(|tx| super::super::read_in_flight(tx))
            .unwrap()
    }

    fn version(&self) -> Option<i64> {
        self.f
            .db
            .read(|c| -> Result<_> {
                Ok(c.query_row("SELECT manifest_version FROM account", [], |r| r.get(0))?)
            })
            .unwrap()
    }

    /// Rows of `storage_records` with this key: (record bytes, target, ours).
    fn stored(&self, key: &[u8]) -> Option<(Vec<u8>, Option<String>, bool)> {
        use kourier_db::rusqlite::OptionalExtension as _;
        self.f
            .db
            .read(|c| -> Result<_> {
                Ok(c.query_row(
                    "SELECT record, target, ours FROM storage_records WHERE key = ?1",
                    [key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?)
            })
            .unwrap()
    }

    fn queued(&self, kind: JobKind) -> i64 {
        self.f.db.read(|c| jobs::count(c, kind)).unwrap()
    }

    /// Whether the write job's row is due now (the other kinds a landing queues, such as the
    /// fetch-latest sync, have their own rows and come first in `jobs::next_due`).
    fn due(&self) -> bool {
        let now = self.now();
        self.f
            .db
            .read(|c| -> Result<bool> {
                Ok(c.query_row(
                    "SELECT EXISTS (SELECT 1 FROM jobs WHERE kind = 'storage_write' AND next_at <= ?1)",
                    [now],
                    |r| r.get(0),
                )?)
            })
            .unwrap()
    }

    /// When the write job's row is due, if it is queued.
    fn next_at(&self) -> Option<i64> {
        use kourier_db::rusqlite::OptionalExtension as _;
        self.f
            .db
            .read(|c| -> Result<_> {
                Ok(c.query_row(
                    "SELECT next_at FROM jobs WHERE kind = 'storage_write'",
                    [],
                    |r| r.get(0),
                )
                .optional()?)
            })
            .unwrap()
    }

    /// How many writes the rate counts ([`rate`]).
    fn times(&self) -> usize {
        self.f.db.read(rate::stored).unwrap()
    }

    /// One run, then the clock goes to the time the job asked to run again.
    async fn run_and_wait(&self) -> Done {
        let done = self.run().await;
        if let Done::RepeatAt(at) = done {
            self.later(u64::try_from(at - self.now()).expect("a time in the future"));
        }
        done
    }

    /// The process is killed and started again over the same files and the same service: nothing
    /// but what the database holds is left.
    fn restarted(self) -> Self {
        let Self {
            mut f,
            clock,
            service,
            shared,
            socket,
            _core,
        } = self;
        drop((shared, socket, _core));
        let path = f.backend_path();
        f.db = Rc::new(
            crate::db::open(&path, &kourier_backend_api::DbKey::new([5; 32])).expect("reopen"),
        );
        let test = crate::actor::testing::shared_over_clock(
            service.clone(),
            f.db.clone(),
            f.account.clone(),
            Vec::new(),
            Arc::new(clock.clone()),
        );
        Self {
            socket: Arc::new(Socket {
                service: service.clone(),
            }),
            shared: Rc::new(test.shared),
            _core: Box::new(test.core),
            f,
            clock,
            service,
        }
    }

    /// The job's counts as stored.
    fn tries(&self) -> Tries {
        Tries::of(&self.job())
    }
}

fn patch_of(events: &[BackendEvent], conv: &ConvKey) -> Option<ConversationPatch> {
    events.iter().find_map(|e| match e {
        BackendEvent::ConversationUpsert(p) if &p.key == conv && p.features.is_none() => {
            Some(p.clone())
        }
        _ => None,
    })
}

fn pin_lists(events: &[BackendEvent]) -> Vec<Vec<ConvKey>> {
    events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::PinnedConversations(list) => Some(list.clone()),
            _ => None,
        })
        .collect()
}

/// The world most tests start from: Ada and Bob on the phone, the account record, at version 7.
fn two_people() -> World {
    let w = World::on();
    w.seed(
        7,
        vec![
            contact(1, ADA, |_| {}),
            contact(2, BOB, |_| {}),
            account_record(3, |_| {}),
        ],
    );
    w
}

// ---------------------------------------------------------------------------------------------
// The write
// ---------------------------------------------------------------------------------------------

/// A click is planned, written and landed: the record the phone reads holds the change under a new
/// id, the old id is deleted, every other id stays; here the written bytes are stored as OURS, the
/// version moved, the intent is closed, the row is gone and the phone is told to fetch. A sync of
/// that version then finds nothing new. The manifest is asked for unconditionally (BQ7).
#[tokio::test]
async fn a_click_is_planned_written_and_landed() {
    let w = two_people();
    w.archive(ADA);
    w.events();

    assert_eq!(w.run().await, Done::Finished);

    {
        let state = w.state();
        assert_eq!(state.manifest_paths, ["/v1/storage/manifest"], "BQ7");
        assert_eq!(state.puts, 1);
        assert_eq!(state.reads, 0, "nothing newer to merge");
        let seen = &state.written[0];
        assert_eq!(seen.manifest.version, 8);
        assert_eq!(
            seen.manifest.record_ikm, IKM,
            "the fetched recordIkm is kept"
        );
        assert_eq!(seen.manifest.source_device, 2, "our device id");
        assert_eq!(seen.deletes, [key(1)]);
        assert_eq!(seen.records.len(), 1);
        assert!(seen.contact(0).archived, "the phone reads the change");
        let listed: Vec<&[u8]> = seen
            .manifest
            .identifiers
            .iter()
            .map(|i| i.raw.as_slice())
            .collect();
        assert_eq!(
            listed,
            [
                key(2).as_slice(),
                key(3).as_slice(),
                seen.records[0].0.as_slice()
            ]
        );
    }

    let (new_key, written) = {
        let state = w.state();
        state.written[0].records[0].clone()
    };
    assert_eq!(new_key.len(), 16);
    assert_eq!(w.version(), Some(8), "the version moved with the records");
    assert_eq!(w.stored(&key(1)), None, "the old record is gone");
    let (bytes, target, ours) = w.stored(&new_key).expect("the written record is stored");
    assert_eq!(bytes, written, "the very bytes the phone reads");
    assert_eq!(target, Some(person(ADA)));
    assert!(ours, "a record our write put there");
    assert!(w.intents().is_empty(), "the intent is closed");
    assert!(w.row().is_none(), "the row is gone");
    assert_eq!(w.queued(JobKind::FetchLatestSync), 1, "M8");
    assert_eq!(w.queued(JobKind::StorageWrite), 0, "the job is done");

    // The next sync of that version merges nothing: we hold what it lists.
    let done = crate::storage::sync(&w.shared, &w.socket).await.unwrap();
    assert_eq!(done, Done::Finished);
    assert_eq!(w.state().reads, 0, "a sync finds nothing new");
    assert_eq!(w.version(), Some(8));
    assert!(
        patch_of(&w.events(), &chat(ADA)).is_none_or(|p| p.archived.is_none()),
        "nothing is stated for our own write"
    );
}

/// The maintenance arm runs the job: a queued row writes through the job runner's own dispatch.
#[tokio::test]
async fn the_runner_of_the_kind_is_the_write_job() {
    let w = two_people();
    w.archive(ADA);
    let job = w.job();
    let done = crate::maintenance::run_job(&w.shared, &w.socket, &job)
        .await
        .unwrap();
    assert_eq!(done, Done::Finished);
    assert_eq!(w.state().puts, 1, "the arm is the write");
}

/// The write carries the flags of ONE record per chat and one PUT per run: two chats, one write.
#[tokio::test]
async fn two_clicks_are_one_write() {
    let w = two_people();
    w.archive(ADA);
    w.mute(BOB, Some(9_000));
    assert_eq!(w.run().await, Done::Finished);
    let state = w.state();
    assert_eq!(state.puts, 1);
    let seen = &state.written[0];
    assert_eq!(seen.records.len(), 2);
    assert_eq!(seen.deletes.len(), 2);
    assert_eq!(seen.manifest.identifiers.len(), 3);
}

// ---------------------------------------------------------------------------------------------
// 409 (M1)
// ---------------------------------------------------------------------------------------------

/// M1, the critic's Y4 and §3.4 step 9: a 409 is no wait inside the run. The body is not read (it
/// would not decode, and that would be permanent), nothing was applied, the row goes, the intent
/// stays and the job says when it runs again: 1 s after the first conflict. On the paused clock
/// any `sleep` in the run would move the time. The next run fetches the manifest again, merges
/// what the phone wrote and writes on top of it: both changes are on the server.
#[tokio::test(start_paused = true)]
async fn a_conflict_repeats_later_without_sleeping() {
    let w = two_people();
    w.archive(ADA);
    let phone = contact(0xb2, BOB, |c| c.marked_unread = true);
    w.state()
        .answers
        .push_back(Answer::PhoneFirst(Box::new(phone), key(2)));

    let started = tokio::time::Instant::now();
    let done = w.run().await;
    assert_eq!(
        tokio::time::Instant::now() - started,
        Duration::ZERO,
        "a run never sleeps"
    );
    assert_eq!(done, Done::RepeatAt(w.now() + 1_000), "1 s after the first");
    assert!(w.row().is_none(), "the server applied nothing");
    assert!(
        w.intent(&person(ADA), ARCHIVED).is_some(),
        "the click stays"
    );
    assert_eq!(
        w.state().written.len(),
        0,
        "nothing of ours is on the server"
    );
    assert_eq!(w.tries().conflicts, 1, "counted in the job's payload");
    assert_eq!(w.version(), Some(7), "409's body is not merged");

    w.later(1_000);
    assert_eq!(w.run().await, Done::Finished);

    let state = w.state();
    assert_eq!(state.manifest_paths.len(), 2, "the re-fetch");
    assert_eq!(state.reads, 1, "the phone's record is read");
    let seen = &state.written[0];
    assert_eq!(seen.manifest.version, 9, "on the merged manifest");
    assert!(seen.contact(0).archived);
    assert!(
        seen.manifest.identifiers.iter().any(|i| i.raw == key(0xb2)),
        "the phone's record stays listed"
    );
    drop(state);
    assert_eq!(w.version(), Some(9));
    assert!(w.intents().is_empty());
}

/// The conflict waits: 1 s, 5 s, 30 s, then the normal backoff, never shorter than 30 s and capped
/// at an hour (M1, Y3). Every 409 counts, and a write that lands starts over.
#[test]
fn the_waits_of_conflicts_and_failures() {
    let conflicts: Vec<i64> = (1..=4).map(conflict_wait_ms).collect();
    assert_eq!(conflicts, [1_000, 5_000, 30_000, 30_000]);
    assert!(conflict_wait_ms(12) > 30_000, "the backoff takes over");
    assert_eq!(conflict_wait_ms(u32::MAX), 900_000);
    assert_eq!(failure_wait_ms(1), 1_000, "never a hot loop");
    assert!(failure_wait_ms(9) > 1_000);
    assert!(failure_wait_ms(u32::MAX) <= 3_600_000, "Y3");
}

#[tokio::test]
async fn three_conflicts_in_a_row_count_up_and_a_landing_starts_over() {
    let w = two_people();
    w.archive(ADA);
    for (n, wait) in [(1u8, 1_000), (2, 5_000), (3, 30_000)] {
        let current = key(if n == 1 { 2 } else { 0xb0 + n - 1 });
        let phone = contact(0xb0 + n, BOB, |c| c.marked_unread = n % 2 == 1);
        w.state()
            .answers
            .push_back(Answer::PhoneFirst(Box::new(phone), current));
        assert_eq!(
            w.run().await,
            Done::RepeatAt(w.now() + wait),
            "conflict {n}"
        );
        w.later(u64::try_from(wait).unwrap());
    }
    assert_eq!(w.tries().conflicts, 3);
    assert_eq!(w.run().await, Done::Finished, "the next one lands");
    assert_eq!(w.state().written.len(), 1);
    w.archive(BOB);
    assert_eq!(w.tries(), Tries::default(), "a new row counts from nothing");
}

// ---------------------------------------------------------------------------------------------
// A transport error: the row decides
// ---------------------------------------------------------------------------------------------

/// A timeout or a lost connection says nothing about the server: the row stays, nothing is put
/// back, and the job looks again. The next run judges the row by the fetched manifest. (a) The
/// server never got it: not landed, the write goes again under NEW ids. (b) The server applied it:
/// landed in the merge of that version, no second PUT, and the phone is told to fetch.
#[tokio::test]
async fn a_transport_error_keeps_the_row_and_the_next_run_judges_it() {
    // (a) never reached the server.
    let w = two_people();
    w.archive(ADA);
    w.state().answers.push_back(Answer::Lost);
    let done = w.run().await;
    assert_eq!(done, Done::RepeatAt(w.now() + 1_000));
    let first = w
        .row()
        .expect("the row is kept: the write may be on the server");
    assert_eq!(first.version, 8);
    assert!(w.intent(&person(ADA), ARCHIVED).is_some());
    assert_eq!(w.tries().failures, 1);
    assert_eq!(w.version(), Some(7));

    w.later(1_000);
    assert_eq!(w.run().await, Done::Finished);
    {
        let state = w.state();
        assert_eq!(state.puts, 2, "judged not landed: written again");
        let seen = &state.written[0];
        assert_eq!(seen.manifest.version, 8);
        assert_ne!(seen.records[0].0, first.new_keys[0], "under new ids");
    }
    assert!(w.intents().is_empty());
    assert_eq!(w.queued(JobKind::FetchLatestSync), 1);

    // (b) applied, the answer lost.
    let w = two_people();
    w.archive(ADA);
    w.state().answers.push_back(Answer::ApplyLost);
    assert!(matches!(w.run().await, Done::RepeatAt(_)));
    let row = w.row().expect("the row is kept");
    assert_eq!(w.queued(JobKind::FetchLatestSync), 0, "not landed yet");

    w.later(1_000);
    assert_eq!(w.run().await, Done::Finished);
    assert_eq!(w.state().puts, 1, "nothing is written twice");
    assert_eq!(w.state().reads, 1, "our record is merged like any other");
    assert!(w.row().is_none());
    assert!(w.intents().is_empty(), "the landing closed the click");
    assert_eq!(w.version(), Some(8));
    let (_, _, ours) = w.stored(&row.new_keys[0]).expect("merged");
    assert!(ours, "the merge knows our own record");
    assert_eq!(w.queued(JobKind::FetchLatestSync), 1, "M8: still told");
}

/// D4: a second landing renews the hour of the fetch-latest sync that is still queued (the account
/// is offline or still catching up): `jobs::request` sets the expiry only when it inserts the row,
/// so without the renewal the news of the second write ran out an hour after the FIRST one.
#[tokio::test]
async fn a_later_landing_renews_the_hour_of_the_queued_fetch_latest() {
    let expiry = |w: &World| -> i64 {
        w.f.db
            .read(|c| -> Result<i64> {
                Ok(c.query_row(
                    "SELECT expires_at FROM jobs WHERE kind = 'fetch_latest_sync'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap()
    };
    let w = two_people();
    w.archive(ADA);
    assert_eq!(w.run().await, Done::Finished);
    let first = expiry(&w);
    assert_eq!(first, w.now() + 3_600_000);

    w.later(50 * 60_000);
    w.mute(BOB, Some(5_000));
    assert_eq!(w.run().await, Done::Finished);
    assert_eq!(w.queued(JobKind::FetchLatestSync), 1, "one row for both");
    assert_eq!(expiry(&w), w.now() + 3_600_000, "the second landing's hour");
    assert!(expiry(&w) > first);
}

/// M8 in the shared path: a plain `storage_sync` that runs first after a restart lands the write
/// it finds, and the phone is still told to fetch.
#[tokio::test]
async fn a_sync_that_lands_a_write_queues_the_fetch_latest() {
    let w = two_people();
    w.archive(ADA);
    w.state().answers.push_back(Answer::ApplyLost);
    assert!(matches!(w.run().await, Done::RepeatAt(_)));
    assert!(w.row().is_some());

    let done = crate::storage::sync(&w.shared, &w.socket).await.unwrap();
    assert_eq!(done, Done::Finished);

    assert!(w.row().is_none());
    assert!(w.intents().is_empty());
    assert_eq!(w.queued(JobKind::FetchLatestSync), 1);
}

// ---------------------------------------------------------------------------------------------
// A 5xx: a lost answer (C5b, R3)
// ---------------------------------------------------------------------------------------------

/// R3: a gateway answers 500-599 whether or not the Storage Service applied the PUT, so a 5xx is a
/// lost answer like a timeout. Here the server DID apply it: the row is kept, the time counts, the
/// failure back-off applies, and the next run lands the write by the fetched manifest: no second
/// PUT, our record merged as ours, the phone told to fetch. (Judged "applied nothing" the click
/// would be written again over a write that is there, and a newer click on the same field would
/// lose at the next merge.)
#[tokio::test]
async fn a_5xx_after_the_server_applied_lands_at_the_next_run() {
    let w = two_people();
    w.archive(ADA);
    w.state().answers.push_back(Answer::ApplyThenStatus(504));
    assert_eq!(
        w.run().await,
        Done::RepeatAt(w.now() + 1_000),
        "the failure back-off"
    );
    let row = w
        .row()
        .expect("the row is kept: the write is on the server");
    assert_eq!(row.version, 8);
    assert!(
        w.intent(&person(ADA), ARCHIVED).is_some(),
        "the click stays until the row is judged"
    );
    assert_eq!(w.tries().failures, 1);
    assert_eq!(w.times(), 1, "the time of the write counts");
    assert_eq!(w.version(), Some(7), "nothing is stored on a lost answer");
    assert_eq!(w.queued(JobKind::FetchLatestSync), 0, "not landed yet");

    w.later(1_000);
    assert_eq!(w.run().await, Done::Finished);
    assert_eq!(w.state().puts, 1, "nothing is written twice");
    assert_eq!(w.state().reads, 1, "our record is merged like any other");
    assert!(w.row().is_none());
    assert!(w.intents().is_empty(), "the landing closed the click");
    assert_eq!(w.version(), Some(8));
    let (_, _, ours) = w.stored(&row.new_keys[0]).expect("merged");
    assert!(ours, "the merge knows our own record");
    assert_eq!(w.queued(JobKind::FetchLatestSync), 1, "the phone is told");
    assert_eq!(
        w.tries(),
        Tries::default(),
        "a landing starts the counts over"
    );
}

/// R3, the other half: the 5xx came before the server applied anything. The same handling (the row
/// is kept, the time counts, the back-off), and the next run judges the row NOT landed (the
/// fetched manifest is behind it) and writes the click again under new ids, counted again. Every
/// status from 500 to 599 is the same.
#[tokio::test]
async fn a_5xx_that_applied_nothing_is_written_again() {
    for status in [500u16, 502, 599] {
        let w = two_people();
        w.archive(ADA);
        w.state().answers.push_back(Answer::Status(status, None));
        assert_eq!(w.run().await, Done::RepeatAt(w.now() + 1_000), "{status}");
        let first = w
            .row()
            .expect("the row is kept: a 5xx says nothing of the server's state");
        assert_eq!(first.version, 8, "{status}");
        assert!(w.intent(&person(ADA), ARCHIVED).is_some(), "{status}");
        assert_eq!(w.tries().failures, 1, "{status}");
        assert_eq!(w.times(), 1, "{status}: the time counts");
        assert_eq!(w.version(), Some(7), "{status}");

        w.later(1_000);
        assert_eq!(w.run().await, Done::Finished, "{status}");
        {
            let state = w.state();
            assert_eq!(state.puts, 2, "{status}: judged not landed, written again");
            assert_eq!(state.written.len(), 1, "{status}");
            let seen = &state.written[0];
            assert_eq!(seen.manifest.version, 8, "{status}");
            assert_ne!(seen.records[0].0, first.new_keys[0], "{status}: new ids");
        }
        assert!(w.row().is_none(), "{status}");
        assert!(w.intents().is_empty(), "{status}");
        assert_eq!(w.times(), 2, "{status}: the retry counts again");
        assert_eq!(w.queued(JobKind::FetchLatestSync), 1, "{status}");
        assert_eq!(w.tries(), Tries::default(), "{status}");
    }
}

/// D1: the write never reached the server (a lost answer: a transport error or a 503), and the
/// phone then writes twice for reasons that do not touch Ada. The next run finds the manifest two
/// versions on with none of our ids, but Ada's OLD record (which our write deleted) is still listed:
/// a Storage Service write is atomic, so ours was not applied. It is NOT presumed landed (the click
/// would be closed, shown here and never written): it is written again, on the phone's newer
/// version, and Ada ends up archived on the phone.
#[tokio::test]
async fn a_put_that_never_arrived_is_written_again_although_the_phone_wrote_twice() {
    for lost in [Answer::Lost, Answer::Status(503, None)] {
        let w = two_people();
        w.archive(ADA);
        w.state().answers.push_back(lost);
        assert!(matches!(w.run().await, Done::RepeatAt(_)));
        let first = w.row().expect("the row is kept: the answer is lost");
        assert_eq!(first.version, 8);

        // The phone writes versions 8 and 9: Bob's record twice, nothing of Ada's.
        {
            let mut state = w.state();
            state.phone_writes(&key(2), &contact(0x12, BOB, |_| {}));
            state.phone_writes(&key(0x12), &contact(0x13, BOB, |_| {}));
        }
        w.later(1_000);
        assert_eq!(w.run().await, Done::Finished);

        let state = w.state();
        assert_eq!(state.puts, 2, "written again, not closed as landed");
        assert_eq!(state.written.len(), 1);
        let seen = &state.written[0];
        assert_eq!(seen.manifest.version, 10, "on the phone's newer version");
        assert_eq!(seen.deletes, [key(1)], "Ada's old record goes now");
        assert!(seen.contact(0).archived, "the phone has the click");
        assert_ne!(seen.records[0].0, first.new_keys[0], "under new ids");
        drop(state);
        assert!(w.intents().is_empty());
        assert!(w.row().is_none());
        assert_eq!(w.version(), Some(10));
        assert_eq!(w.queued(JobKind::FetchLatestSync), 1);
    }
}

// ---------------------------------------------------------------------------------------------
// A status that is no 2xx
// ---------------------------------------------------------------------------------------------

/// A refusal below 500 (a 400: the server checks before it applies) says the write is not on the
/// server: the row goes and what it lent a click made during the PUT goes back (review N1 (b),
/// R2-1): the 8 h's base and `shown` are the phone's value again, not the 1 h that was never
/// written. The next run writes the click. (A 5xx says nothing of the kind, R3: its own tests.)
#[tokio::test]
async fn a_status_gives_the_bases_back() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let w = Rc::new(two_people());
            w.mute(BOB, Some(1_000));
            let hold = Hold::new();
            {
                let mut state = w.state();
                state.hold_put = Some(hold.clone());
                state.answers.push_back(Answer::Status(400, None));
            }
            let run = tokio::task::spawn_local({
                let w = w.clone();
                async move { w.run().await }
            });
            hold.wait_reached().await;
            w.mute(BOB, Some(8_000));
            let during = w.intent(&person(BOB), MUTED_UNTIL).unwrap();
            assert_eq!(during.base, Some(1_000), "the click took the written value");
            hold.release.notify_one();
            let done = run.await.unwrap();
            assert_eq!(done, Done::RepeatAt(w.now() + 1_000));

            assert!(w.row().is_none(), "the server applied nothing");
            let kept = w.intent(&person(BOB), MUTED_UNTIL).unwrap();
            assert_eq!(kept.value, Some(8_000));
            assert_eq!(kept.base, None, "the phone's value again");
            assert_eq!(kept.shown, None);
            assert_eq!(w.tries().failures, 1);

            w.later(1_000);
            assert_eq!(w.run().await, Done::Finished);
            let state = w.state();
            assert_eq!(state.written[0].contact(0).muted_until_timestamp, 8_000);
        })
        .await;
}

/// Y3: nothing the server answers drops the job while intents live. A 400, a manifest that does not
/// decode, credentials refused, a manifest status: each is a RepeatAt (a permanent error would
/// delete the row and orphan the intents), capped at an hour.
#[tokio::test]
async fn no_answer_drops_the_job() {
    let w = two_people();
    w.archive(ADA);

    w.state().answers.push_back(Answer::Status(400, None));
    assert!(matches!(w.run().await, Done::RepeatAt(_)));
    assert!(w.row().is_none(), "a 400 is an answer: not landed");

    w.state().manifest_garbage = true;
    assert!(matches!(w.run().await, Done::RepeatAt(_)));
    w.state().manifest_garbage = false;

    w.state().auth_status = Some(401);
    assert!(matches!(w.run().await, Done::RepeatAt(_)));
    w.state().auth_status = None;

    w.state().manifest_status = Some(500);
    let done = w.run().await;
    let Done::RepeatAt(at) = done else {
        panic!("not repeated: {done:?}");
    };
    assert!(at - w.now() <= 3_600_000, "Y3: capped at an hour");
    w.state().manifest_status = None;

    assert_eq!(w.queued(JobKind::StorageWrite), 1, "the job lives");
    assert!(
        w.intent(&person(ADA), ARCHIVED).is_some(),
        "so does the click"
    );
    assert_eq!(w.tries().failures, 4, "each counted");
    assert_eq!(w.state().written.len(), 0);

    assert_eq!(
        w.run().await,
        Done::Finished,
        "and the write goes when it can"
    );
    assert_eq!(w.state().written.len(), 1);
}

/// 401 or 403 from the PUT: the credentials again, once, and the same write again; a second refusal
/// is a status like any other. The row stays through the retry.
#[tokio::test]
async fn a_refused_put_gets_new_credentials_once() {
    let w = two_people();
    w.archive(ADA);
    w.state().answers.push_back(Answer::Status(401, None));
    assert_eq!(w.run().await, Done::Finished);
    {
        let state = w.state();
        assert_eq!(state.puts, 2);
        assert_eq!(state.auths, 2);
        assert_eq!(state.written.len(), 1);
    }

    let w = two_people();
    w.archive(ADA);
    {
        let mut state = w.state();
        state.answers.push_back(Answer::Status(403, None));
        state.answers.push_back(Answer::Status(403, None));
    }
    assert_eq!(w.run().await, Done::RepeatAt(w.now() + 1_000));
    let state = w.state();
    assert_eq!(state.puts, 2, "once");
    assert_eq!(state.auths, 2);
    drop(state);
    assert!(w.row().is_none(), "an answer: not landed");
}

/// V3: the first PUT was refused (a 401: nothing reached the server) and the job asks for new
/// credentials; while it waits the switch goes off. The second PUT must not go out: the first
/// answer is judged, the row goes, what the switch-off kept for the write in flight is dropped and
/// the phone's value stated (Y6's reason to let a write land does not hold for a write that is
/// known not to be on the server).
#[tokio::test]
async fn the_switch_going_off_while_the_credentials_are_fetched_again_sends_no_second_put() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let w = Rc::new(two_people());
            w.archive(ADA);
            let hold = Hold::new();
            {
                let mut state = w.state();
                state.hold_second_auth = Some(hold.clone());
                state.answers.push_back(Answer::Status(401, None));
            }
            let run = tokio::task::spawn_local({
                let w = w.clone();
                async move { w.run().await }
            });
            hold.wait_reached().await;
            w.switch_off_as_the_command_does();
            assert!(
                w.intent(&person(ADA), ARCHIVED).is_some(),
                "the write in flight still carries it"
            );
            w.events();
            hold.release.notify_one();
            assert_eq!(run.await.unwrap(), Done::Finished);

            assert_eq!(w.state().puts, 1, "no second PUT");
            assert_eq!(w.state().written.len(), 0);
            assert!(w.row().is_none());
            assert!(w.intents().is_empty(), "dropped");
            assert_eq!(w.times(), 0, "a write that never left");
            assert_eq!(
                patch_of(&w.events(), &chat(ADA)).and_then(|p| p.archived),
                Some(false),
                "the phone's value, stated"
            );
        })
        .await;
}

/// 429: not landed, the server's `Retry-After` capped at an hour, and remembered as the kind's hold.
#[tokio::test]
async fn a_rate_limit_waits_for_the_retry_after() {
    let w = two_people();
    w.archive(ADA);
    w.state()
        .answers
        .push_back(Answer::Status(429, Some("120")));
    assert_eq!(w.run().await, Done::RepeatAt(w.now() + 120_000));
    assert!(w.row().is_none());
    let floor =
        w.f.db
            .read(|c| jobs::due_floor(c, JobKind::StorageWrite, w.now()))
            .unwrap();
    assert_eq!(floor, w.now() + 120_000, "a click cannot knock earlier");

    let w = two_people();
    w.archive(ADA);
    w.state()
        .answers
        .push_back(Answer::Status(429, Some("999999")));
    assert_eq!(w.run().await, Done::RepeatAt(w.now() + 3_600_000), "Y3");

    // D3: a `Retry-After: 0` waits the shortest wait, as Desktop's `MINIMAL_RETRY_AFTER`: no hot
    // loop of runs with their time refunded.
    let w = two_people();
    w.archive(ADA);
    w.state().answers.push_back(Answer::Status(429, Some("0")));
    assert_eq!(w.run().await, Done::RepeatAt(w.now() + 1_000), "D3");
    let floor =
        w.f.db
            .read(|c| jobs::due_floor(c, JobKind::StorageWrite, w.now()))
            .unwrap();
    assert_eq!(floor, w.now() + 1_000, "and the hold is the same second");
}

/// 404: a linked device never creates a manifest. The intents stay and it looks again in an hour.
#[tokio::test]
async fn no_manifest_keeps_the_intents_for_an_hour() {
    let w = two_people();
    w.archive(ADA);
    w.state().manifest_status = Some(404);
    assert_eq!(w.run().await, Done::RepeatAt(w.now() + 3_600_000));
    assert!(w.intent(&person(ADA), ARCHIVED).is_some());
    assert_eq!(w.state().puts, 0);
}

// ---------------------------------------------------------------------------------------------
// The switch and a row in flight (M2, Y6)
// ---------------------------------------------------------------------------------------------

/// The row is judged BEFORE the switch is looked at: a write that left before the switch went off
/// landed. The run fetches, lands it, states the written value and writes nothing more.
#[tokio::test]
async fn with_the_switch_off_a_landed_row_is_judged_first() {
    let w = two_people();
    w.archive(ADA);
    w.state().answers.push_back(Answer::ApplyLost);
    assert!(matches!(w.run().await, Done::RepeatAt(_)));
    w.switch_off_as_the_command_does();
    w.events();
    assert!(w.row().is_some(), "what is on its way stays (Y6)");
    assert!(w.intent(&person(ADA), ARCHIVED).is_some(), "carried");

    assert_eq!(w.run().await, Done::Finished);

    assert_eq!(w.state().puts, 1, "nothing more is written");
    assert_eq!(
        w.state().manifest_paths.len(),
        2,
        "the row needed the fetch"
    );
    assert!(w.row().is_none());
    assert!(w.intents().is_empty());
    let events = w.events();
    assert_eq!(
        patch_of(&events, &chat(ADA)).and_then(|p| p.archived),
        Some(true),
        "the core is told what the phone now has"
    );
}

/// The same when the row did not land: the phone keeps its value, the core is told it, nothing is
/// written with the switch off (M2).
#[tokio::test]
async fn with_the_switch_off_a_row_that_did_not_land_is_judged_first() {
    let w = two_people();
    w.archive(ADA);
    w.state().answers.push_back(Answer::Lost);
    assert!(matches!(w.run().await, Done::RepeatAt(_)));
    w.switch_off_as_the_command_does();
    w.events();
    assert!(w.row().is_some());

    assert_eq!(w.run().await, Done::Finished);

    assert_eq!(w.state().puts, 1, "only the first try");
    assert_eq!(
        w.state().manifest_paths.len(),
        2,
        "the row needed the fetch"
    );
    assert!(w.row().is_none());
    assert!(w.intents().is_empty(), "nothing will ever write them");
    let events = w.events();
    assert_eq!(
        patch_of(&events, &chat(ADA)).and_then(|p| p.archived),
        Some(false),
        "the phone's value is stated"
    );
}

/// With the switch off and no row the job ends before any request (the default of every account).
#[tokio::test]
async fn with_the_switch_off_and_no_row_nothing_is_requested() {
    let w = World::new();
    w.seed(7, vec![contact(1, ADA, |_| {})]);
    assert_eq!(w.run().await, Done::Finished);
    let state = w.state();
    assert_eq!(state.auths, 0);
    assert!(state.manifest_paths.is_empty());
    assert_eq!(state.puts, 0);
}

/// The switch goes off while the run fetches: nothing goes out (the zone of the row looks again).
#[tokio::test]
async fn the_switch_is_looked_at_again_before_the_row_is_written() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let w = Rc::new(two_people());
            w.archive(ADA);
            // A phone record makes the run read, and hold, before it plans.
            let hold = Hold::new();
            {
                let mut state = w.state();
                state.hold_read = Some(hold.clone());
                let phone = contact(0xb2, BOB, |c| c.marked_unread = true);
                state.phone_writes(&key(2), &phone);
            }
            let run = tokio::task::spawn_local({
                let w = w.clone();
                async move { w.run().await }
            });
            hold.wait_reached().await;
            w.switch_off_as_the_command_does();
            hold.release.notify_one();
            assert_eq!(run.await.unwrap(), Done::Finished);
            assert_eq!(w.state().puts, 0);
            assert!(w.row().is_none());
        })
        .await;
}

/// Y6: the switch goes off during the PUT and the server takes it: what was on its way arrives, the
/// core is told what the phone now has, the phone is told to fetch, and the job ends.
#[tokio::test]
async fn the_switch_going_off_during_a_put_that_lands_states_what_was_written() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let w = Rc::new(two_people());
            w.archive(ADA);
            let hold = Hold::new();
            w.state().hold_put = Some(hold.clone());
            let run = tokio::task::spawn_local({
                let w = w.clone();
                async move { w.run().await }
            });
            hold.wait_reached().await;
            w.switch_off_as_the_command_does();
            w.events();
            hold.release.notify_one();
            assert_eq!(run.await.unwrap(), Done::Finished);

            assert!(w.row().is_none());
            assert!(w.intents().is_empty());
            assert_eq!(w.version(), Some(8), "the records are stored all the same");
            assert_eq!(w.queued(JobKind::FetchLatestSync), 1);
            assert_eq!(
                patch_of(&w.events(), &chat(ADA)).and_then(|p| p.archived),
                Some(true)
            );
        })
        .await;
}

/// M2: the switch goes off during the PUT and the server refuses it: nothing arrives, the phone's
/// value is stated, nothing is left, and the job ends instead of looking again.
#[tokio::test]
async fn the_switch_going_off_during_a_refused_put_ends_the_job() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let w = Rc::new(two_people());
            w.archive(ADA);
            let hold = Hold::new();
            {
                let mut state = w.state();
                state.hold_put = Some(hold.clone());
                state.answers.push_back(Answer::Status(400, None));
            }
            let run = tokio::task::spawn_local({
                let w = w.clone();
                async move { w.run().await }
            });
            hold.wait_reached().await;
            w.switch_off_as_the_command_does();
            w.events();
            hold.release.notify_one();
            assert_eq!(run.await.unwrap(), Done::Finished);

            assert!(w.row().is_none());
            assert!(w.intents().is_empty());
            assert_eq!(w.version(), Some(7));
            assert_eq!(w.queued(JobKind::FetchLatestSync), 0);
            assert_eq!(
                patch_of(&w.events(), &chat(ADA)).and_then(|p| p.archived),
                Some(false)
            );
        })
        .await;
}

/// D2: Bob's mute for 1 h is in flight; he mutes Bob for 8 h (a newer click on the carried field),
/// turns the switch off (`drop_all` deletes the 8 h WITHOUT restating: the landing is to state the
/// written value) and on again before the answer. The promise is in the row, not in the switch:
/// a 200 tells the core the 1 h the phone now has, and a refusal tells it the phone's "not muted"
/// (nothing was written). (Reading the switch at the answer, the core kept the 8 h for good while
/// the phone had the 1 h, or nothing.)
#[tokio::test]
async fn a_switch_off_and_on_during_a_put_still_states_the_answer_to_the_core() {
    for landed in [true, false] {
        tokio::task::LocalSet::new()
            .run_until(async move {
                let w = Rc::new(two_people());
                w.mute(BOB, Some(1_000));
                let hold = Hold::new();
                {
                    let mut state = w.state();
                    state.hold_put = Some(hold.clone());
                    if !landed {
                        state.answers.push_back(Answer::Status(409, None));
                    }
                }
                let run = tokio::task::spawn_local({
                    let w = w.clone();
                    async move { w.run().await }
                });
                hold.wait_reached().await;
                w.mute(BOB, Some(8_000));
                w.switch_off_as_the_command_does();
                assert!(w.intents().is_empty(), "the newer click went, unstated");
                let marked = w.row().expect("the write is in flight").planned;
                assert!(
                    marked.iter().any(|p| p.state_at_landing),
                    "and the row says so"
                );
                w.switch(true);
                w.events();
                hold.release.notify_one();
                run.await.unwrap();

                assert!(w.row().is_none(), "landed={landed}");
                let patch = patch_of(&w.events(), &chat(BOB)).expect("the core is told");
                if landed {
                    assert_eq!(patch.muted_until, Patch::Set(1_000), "what the phone has");
                    assert_eq!(w.version(), Some(8));
                } else {
                    assert_eq!(patch.muted_until, Patch::Clear, "nothing was written");
                    assert_eq!(w.version(), Some(7));
                }
            })
            .await;
    }
}

// ---------------------------------------------------------------------------------------------
// The merge path shared with the sync (review B1, N1 (a))
// ---------------------------------------------------------------------------------------------

/// Review B1 on the job: a write of Bob's mute reached the server and its answer was lost. The next
/// run reads the items of that version; WHILE it waits for them he unmutes Bob. The row is still
/// there, so the Unmute's base is the written mute; the landing runs in the first merge zone,
/// right before the merge of our own record, and the Unmute survives it and is written. (With the
/// landing in a zone of its own before the read, the Unmute takes the replaced record's "not
/// muted" as the phone's value, our own record then wins as "the phone's", and the click is lost.)
#[tokio::test]
async fn a_click_while_the_job_reads_the_items_stays() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let w = Rc::new(two_people());
            w.mute(BOB, Some(9_000));
            w.state().answers.push_back(Answer::ApplyLost);
            assert!(matches!(w.run().await, Done::RepeatAt(_)));
            assert!(w.row().is_some());
            w.later(1_000);

            let hold = Hold::new();
            w.state().hold_read = Some(hold.clone());
            let run = tokio::task::spawn_local({
                let w = w.clone();
                async move { w.run().await }
            });
            hold.wait_reached().await;
            w.mute(BOB, None);
            w.events();
            hold.release.notify_one();
            assert_eq!(run.await.unwrap(), Done::Finished);

            assert!(w.row().is_none());
            assert!(w.intents().is_empty(), "written, not lost");
            let state = w.state();
            assert_eq!(state.written.len(), 2, "his Unmute went out");
            assert_eq!(state.written[1].manifest.version, 9);
            assert_eq!(
                state.written[1].contact(0).muted_until_timestamp,
                0,
                "the phone is unmuted"
            );
            drop(state);
            assert!(
                patch_of(&w.events(), &chat(BOB)).is_none_or(|p| p.muted_until == Patch::Unchanged),
                "our own record is not stated back over the click"
            );
        })
        .await;
}

/// Review N1 (a) on the job: a write that did not land (the phone wrote that very version) is
/// judged before the items are read: a click made while they are read finds no row, and its base
/// is the phone's value. The write then goes out on the merged manifest.
#[tokio::test]
async fn a_row_that_did_not_land_is_judged_before_the_job_reads_the_items() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let w = Rc::new(two_people());
            w.mute(BOB, Some(9_000));
            w.state().answers.push_back(Answer::Lost);
            assert!(matches!(w.run().await, Done::RepeatAt(_)));
            assert!(w.row().is_some());
            w.later(1_000);

            // The phone wrote version 8 itself, on another chat.
            let hold = Hold::new();
            {
                let mut state = w.state();
                let phone = contact(0xa2, ADA, |c| c.marked_unread = true);
                state.phone_writes(&key(1), &phone);
                state.hold_read = Some(hold.clone());
            }
            let run = tokio::task::spawn_local({
                let w = w.clone();
                async move { w.run().await }
            });
            hold.wait_reached().await;
            assert!(w.row().is_none(), "judged before the read");
            hold.release.notify_one();
            assert_eq!(run.await.unwrap(), Done::Finished);

            let state = w.state();
            assert_eq!(state.written.len(), 1);
            assert_eq!(state.written[0].manifest.version, 9);
            assert_eq!(state.written[0].contact(0).muted_until_timestamp, 9_000);
        })
        .await;
}

/// §3.4 step 4: the fetched manifest is merged first, and a merge alone causes no write: the phone
/// did the same thing, the intent is closed, the run ends without a PUT.
#[tokio::test]
async fn a_merge_that_closes_every_intent_writes_nothing() {
    let w = two_people();
    w.archive(ADA);
    {
        let mut state = w.state();
        let phone = contact(0xa2, ADA, |c| c.archived = true);
        state.phone_writes(&key(1), &phone);
    }
    assert_eq!(w.run().await, Done::Finished);
    assert_eq!(w.state().puts, 0);
    assert_eq!(w.version(), Some(8));
    assert!(w.intents().is_empty());
}

/// No intent, no row: no request. The job costs nothing when there is nothing to do.
#[tokio::test]
async fn nothing_to_do_requests_nothing() {
    let w = two_people();
    assert_eq!(w.run().await, Done::Finished);
    assert_eq!(w.state().auths, 0);
    assert!(w.state().manifest_paths.is_empty());
}

/// Y3: an intent older than 30 days goes at the job's start, its value restated, nothing written.
#[tokio::test]
async fn an_old_intent_is_dropped_at_the_start_of_the_job() {
    let w = two_people();
    w.archive(ADA);
    w.events();
    w.later(u64::try_from(intents::STALE_AFTER_MS).unwrap() + 1);
    assert_eq!(w.run().await, Done::Finished);
    assert!(w.intents().is_empty());
    assert_eq!(w.state().puts, 0);
    assert_eq!(w.state().auths, 0, "nothing was left to ask about");
    assert_eq!(
        patch_of(&w.events(), &chat(ADA)).and_then(|p| p.archived),
        Some(false),
        "the phone's value is stated"
    );
}

/// A newer click during the PUT stays when the write lands: its base is the written value, the job
/// comes back a second later and writes it (the debounce's second).
#[tokio::test]
async fn a_click_during_the_put_is_written_next() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let w = Rc::new(two_people());
            w.mute(BOB, Some(1_000));
            let hold = Hold::new();
            w.state().hold_put = Some(hold.clone());
            let run = tokio::task::spawn_local({
                let w = w.clone();
                async move { w.run().await }
            });
            hold.wait_reached().await;
            w.mute(BOB, Some(8_000));
            hold.release.notify_one();
            let done = run.await.unwrap();
            assert_eq!(done, Done::RepeatAt(w.now() + 1_000));
            let kept = w.intent(&person(BOB), MUTED_UNTIL).unwrap();
            assert_eq!((kept.value, kept.base), (Some(8_000), Some(1_000)));
            assert_eq!(w.version(), Some(8));

            w.later(1_000);
            assert_eq!(w.run().await, Done::Finished);
            let state = w.state();
            assert_eq!(state.written[1].manifest.version, 9);
            assert_eq!(state.written[1].contact(0).muted_until_timestamp, 8_000);
        })
        .await;
}

// ---------------------------------------------------------------------------------------------
// A reset (M4)
// ---------------------------------------------------------------------------------------------

/// A new storage key during the PUT: the 200 finds another generation. The write is on the server,
/// so nothing is put back; nothing is stored under the new key's empty store; the intents stay and
/// a full sync is asked for. No fetch-latest: the phone rotated its key anyway.
#[tokio::test]
async fn a_reset_during_the_put_drops_its_result() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let w = Rc::new(two_people());
            w.archive(ADA);
            let hold = Hold::new();
            w.state().hold_put = Some(hold.clone());
            let run = tokio::task::spawn_local({
                let w = w.clone();
                async move { w.run().await }
            });
            hold.wait_reached().await;
            let now = w.now();
            w.f.db.zone(|tx| crate::storage::reset(tx, now)).unwrap();
            hold.release.notify_one();
            let done = run.await.unwrap();

            assert_eq!(done, Done::RepeatAt(w.now() + 1_000));
            assert!(w.row().is_none());
            assert!(
                w.intent(&person(ADA), ARCHIVED).is_some(),
                "the intents stay"
            );
            assert_eq!(w.version(), None, "nothing stored under the new key");
            assert_eq!(w.queued(JobKind::StorageSync), 1, "sync again");
            assert_eq!(w.queued(JobKind::FetchLatestSync), 0);
        })
        .await;
}

/// A new storage key while the run reads: nothing goes out under the old one.
#[tokio::test]
async fn a_reset_during_the_read_sends_nothing() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let w = Rc::new(two_people());
            w.archive(ADA);
            let hold = Hold::new();
            {
                let mut state = w.state();
                state.hold_read = Some(hold.clone());
                let phone = contact(0xb2, BOB, |c| c.marked_unread = true);
                state.phone_writes(&key(2), &phone);
            }
            let run = tokio::task::spawn_local({
                let w = w.clone();
                async move { w.run().await }
            });
            hold.wait_reached().await;
            let now = w.now();
            w.f.db.zone(|tx| crate::storage::reset(tx, now)).unwrap();
            hold.release.notify_one();
            let done = run.await.unwrap();
            assert_eq!(done, Done::RepeatAt(w.now() + 1_000));
            assert_eq!(w.state().puts, 0);
            assert!(w.row().is_none());
            assert!(w.intent(&person(ADA), ARCHIVED).is_some());
        })
        .await;
}

// ---------------------------------------------------------------------------------------------
// `Plan::left`
// ---------------------------------------------------------------------------------------------

impl World {
    /// A row of `storage_intents` as an older build or another path could have left it.
    fn file_raw(&self, target: &str, field: &str, value: Option<i64>, key: Option<&[u8]>) {
        let now = self.now();
        self.f
            .db
            .zone(|tx| -> Result<()> {
                tx.execute(
                    "UPDATE account SET storage_intent_seq = storage_intent_seq + 1",
                    [],
                )?;
                let seq: i64 =
                    tx.query_row("SELECT storage_intent_seq FROM account", [], |r| r.get(0))?;
                tx.execute(
                    "INSERT INTO storage_intents(target, field, value, base, shown, key, seq,
                       automatic, created_at)
                     VALUES (?1, ?2, ?3, NULL, NULL, ?4, ?5, 0, ?6)",
                    (target, field, value, key, seq, now),
                )?;
                Ok(())
            })
            .unwrap();
    }

    /// The manifest forgets the record `key` (the same version: nothing new to merge).
    fn unlist(&self, key: &[u8]) {
        let mut state = self.state();
        state.identifiers.retain(|i| i.raw != key);
        state.items.remove(key);
    }
}

/// `Why::NotListed`: no listed record of the chat: nothing to rotate, the intent stays for a later
/// record, nothing is sent, and it looks again in an hour.
#[tokio::test]
async fn a_chat_the_manifest_does_not_list_keeps_its_intent() {
    let w = two_people();
    w.archive(ADA);
    w.unlist(&key(1));
    assert_eq!(w.run().await, Done::RepeatAt(w.now() + 3_600_000));
    assert_eq!(w.state().puts, 0, "an empty plan is never sent");
    assert!(w.row().is_none());
    assert!(w.intent(&person(ADA), ARCHIVED).is_some());
}

/// `Why::Unreadable`: the record does not decode: never edited, the intent stays.
#[tokio::test]
async fn a_record_we_cannot_read_keeps_its_intent() {
    let w = two_people();
    w.archive(ADA);
    w.f.db
        .zone(|tx| -> Result<()> {
            tx.execute(
                "UPDATE storage_records SET record = x'ffffffff' WHERE key = ?1",
                [key(1)],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(w.run().await, Done::RepeatAt(w.now() + 3_600_000));
    assert_eq!(w.state().puts, 0);
    assert!(w.intent(&person(ADA), ARCHIVED).is_some());
}

/// A kept intent beside a write that goes: the write lands, the kept one waits an hour, and the
/// counts of an earlier failure are over.
#[tokio::test]
async fn a_kept_intent_beside_a_write_looks_again_in_an_hour() {
    let w = two_people();
    w.archive(ADA);
    w.archive(BOB);
    w.unlist(&key(1));
    assert_eq!(w.run().await, Done::RepeatAt(w.now() + 3_600_000));
    assert_eq!(w.state().puts, 1);
    assert!(w.row().is_none());
    assert!(w.intent(&person(ADA), ARCHIVED).is_some(), "kept");
    assert!(w.intent(&person(BOB), ARCHIVED).is_none(), "written");
}

/// `Why::NotWritable`: this record cannot take the field (a mute on Note to Self): deleted, its
/// base stated, nothing sent. The job ends: nothing is left.
#[tokio::test]
async fn an_intent_no_record_can_take_is_deleted() {
    let w = two_people();
    w.file_raw(ACCOUNT_TARGET, MUTED_UNTIL, Some(9_000), None);
    assert_eq!(w.run().await, Done::Finished);
    assert!(w.intents().is_empty());
    assert_eq!(w.state().puts, 0);
}

/// `Why::OtherKey` (M2): a safety number's state for a key the record does not hold: deleted,
/// nothing stated, nothing sent. The same state for the record's own key is written.
#[tokio::test]
async fn an_identity_intent_for_another_key_is_deleted() {
    let w = World::on();
    w.seed(
        7,
        vec![
            contact(1, ADA, |c| c.identity_key = vec![1; 33]),
            contact(2, BOB, |c| c.identity_key = vec![2; 33]),
        ],
    );
    w.file_raw(&person(ADA), IDENTITY, Some(1), Some(&[7; 33]));
    w.events();
    assert_eq!(w.run().await, Done::Finished);
    assert!(w.intents().is_empty());
    assert_eq!(w.state().puts, 0);
    assert!(w.events().is_empty(), "nothing is stated");

    w.file_raw(&person(BOB), IDENTITY, Some(1), Some(&[2; 33]));
    assert_eq!(w.run().await, Done::Finished);
    let state = w.state();
    assert_eq!(state.puts, 1);
    assert_eq!(state.written[0].contact(0).identity_state, 1);
}

/// `Why::OverPinLimit` (M9 b): the phone filled the list meanwhile, so our pin does not fit. Its
/// intent goes, the WHOLE pin list is stated (the chat is not pinned here any more), nothing else
/// changes and nothing is sent.
#[tokio::test]
async fn a_pin_over_the_limit_is_deleted_and_the_whole_list_stated() {
    let w = World::on();
    let pinned = |c: &mut proto::AccountRecord, people: &[u128]| {
        c.pinned_conversations = people.iter().map(|n| contact_pin(*n)).collect();
    };
    w.seed(
        7,
        vec![
            contact(1, ADA, |_| {}),
            contact(2, BOB, |_| {}),
            contact(4, CHIARA, |_| {}),
            contact(5, 0xd0, |_| {}),
            contact(6, 0xe0, |_| {}),
            account_record(3, |c| pinned(c, &[BOB, CHIARA, 0xd0])),
        ],
    );
    w.click(ConversationChange::SetPinned { pinned: true }, &chat(ADA));
    w.events();
    {
        let mut state = w.state();
        let phone = account_record(0x33, |c| pinned(c, &[BOB, CHIARA, 0xd0, 0xe0]));
        state.phone_writes(&key(3), &phone);
    }
    assert_eq!(w.run().await, Done::Finished);
    assert_eq!(w.state().puts, 0);
    assert!(w.intents().is_empty());
    let lists = pin_lists(&w.events());
    assert_eq!(
        lists.last().expect("the whole pin list"),
        &[chat(BOB), chat(CHIARA), chat(0xd0), chat(0xe0)],
        "the phone's four, and not our fifth"
    );
}

/// A refused plan (H6: the manifest lists Ada's record under another type: rotating it would make a
/// mismatch of ours, and the phone force-pushes on that): nothing is sent, every intent stays, the
/// job repeats with the backoff.
#[tokio::test]
async fn a_refused_plan_keeps_every_intent_and_repeats() {
    let w = two_people();
    w.archive(ADA);
    w.archive(BOB);
    {
        let mut state = w.state();
        for identifier in &mut state.identifiers {
            if identifier.raw == key(1) {
                identifier.r#type = ItemType::Account as i32;
            }
        }
    }
    assert_eq!(w.run().await, Done::RepeatAt(w.now() + 1_000));
    assert_eq!(w.state().puts, 0);
    assert!(w.row().is_none());
    assert_eq!(w.intents().len(), 2);
    assert_eq!(w.tries().failures, 1);
}

/// The counts live in the payload, and one this build cannot read counts as none: it decides waits
/// only.
#[test]
fn an_unreadable_payload_counts_as_none() {
    let job = |payload: Option<&[u8]>| Job {
        id: 1,
        kind: JobKind::StorageWrite,
        key: String::new(),
        attempts: 0,
        expires_at: None,
        payload: payload.map(<[u8]>::to_vec),
        requests: 0,
    };
    assert_eq!(Tries::of(&job(None)), Tries::default());
    assert_eq!(Tries::of(&job(Some(&[0xff, 0x00]))), Tries::default());
    assert_eq!(
        Tries::of(&job(Some(br#"{"conflicts":3}"#))),
        Tries {
            conflicts: 3,
            failures: 0
        }
    );
}

// ---------------------------------------------------------------------------------------------
// The failpoints (§3.6): each exists, is hit, and the row says what happened
// ---------------------------------------------------------------------------------------------

const CHILD_ENV: &str = "KOURIER_WRITE_JOB_CHILD";

/// The child of [`the_three_failpoints_are_hit_where_the_row_decides`]: a write that an armed
/// failpoint aborts. Without its environment it does nothing.
#[tokio::test]
async fn child_write_run() {
    let Ok(marker) = std::env::var(CHILD_ENV) else {
        return;
    };
    let w = two_people();
    w.archive(ADA);
    w.state().marker = Some(PathBuf::from(marker));
    w.run().await;
    // Only without a failpoint: the parent looks for the abort.
    std::process::exit(0);
}

fn run_child(failpoint: &str, tmp: &Path) -> std::process::Output {
    let test = format!(
        "{}::child_write_run",
        module_path!().split_once("::").unwrap().1
    );
    std::process::Command::new(std::env::current_exe().unwrap())
        .args([test.as_str(), "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, tmp.join("put-reached"))
        // The fixture's temporary directory (and its backend.db) lands where the parent finds it.
        .env("TMPDIR", tmp)
        .env(kourier_db::failpoint::ENV, format!("{failpoint}:1"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

/// What the kill left: (a PUT reached the server, a row in flight, the version, intents, a
/// fetch-latest queued).
fn left_by_kill(tmp: &Path) -> (bool, bool, Option<i64>, usize, i64) {
    let dir = std::fs::read_dir(tmp)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.join("backend.db").exists())
        .expect("the child's database");
    let db = crate::db::open(
        &dir.join("backend.db"),
        &kourier_backend_api::DbKey::new([5; 32]),
    )
    .unwrap();
    let in_flight = db.read(super::super::in_flight_exists).unwrap();
    let version = db
        .read(|c| -> Result<Option<i64>> {
            Ok(c.query_row("SELECT manifest_version FROM account", [], |r| r.get(0))?)
        })
        .unwrap();
    let live = db.read(intents::all_intents).unwrap().len();
    let fetch = db
        .read(|c| jobs::count(c, JobKind::FetchLatestSync))
        .unwrap();
    (
        tmp.join("put-reached").exists(),
        in_flight,
        version,
        live,
        fetch,
    )
}

/// §3.6's table, in a child process that really aborts: before the PUT the row is committed and
/// nothing reached the server; after the PUT the server has it and the row, the version and the
/// intent are as before (the row decides at the restart); after the commit the row is gone, the
/// version moved, the intent closed and the fetch-latest queued.
#[test]
fn the_three_failpoints_are_hit_where_the_row_decides() {
    if !kourier_db::failpoint::ENABLED {
        return;
    }
    let cases = [
        (
            FAILPOINT_BEFORE_STORAGE_WRITE_PUT,
            (false, true, Some(7), 1, 0),
        ),
        (
            FAILPOINT_AFTER_STORAGE_WRITE_PUT,
            (true, true, Some(7), 1, 0),
        ),
        (
            FAILPOINT_AFTER_STORAGE_WRITE_COMMIT,
            (true, false, Some(8), 0, 1),
        ),
    ];
    for (failpoint, expected) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let output = run_child(failpoint, tmp.path());
        assert_eq!(
            output.status.signal(),
            Some(6),
            "the child aborted at {failpoint}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains(&format!("failpoint {failpoint}:1 hit")),
            "at {failpoint}"
        );
        assert_eq!(left_by_kill(tmp.path()), expected, "at {failpoint}");
    }
    // The names the kill tests of the crate use.
    assert_eq!(
        crate::failpoints::BEFORE_STORAGE_WRITE_PUT,
        "before_storage_write_put"
    );
    assert_eq!(
        crate::failpoints::AFTER_STORAGE_WRITE_PUT,
        "after_storage_write_put"
    );
    assert_eq!(
        crate::failpoints::AFTER_STORAGE_WRITE_COMMIT,
        "after_storage_write_commit"
    );
}

/// A marked chat keeps `MARKED_UNREAD` in the job's rotation like any other field (the automatic
/// clear goes through the same run as a click).
#[tokio::test]
async fn a_mark_is_written_like_the_other_flags() {
    let w = two_people();
    w.click(
        ConversationChange::SetMarkedUnread {
            marked_unread: true,
        },
        &chat(ADA),
    );
    assert!(w.intent(&person(ADA), MARKED_UNREAD).is_some());
    assert_eq!(w.run().await, Done::Finished);
    assert!(w.state().written[0].contact(0).marked_unread);
}

// ---------------------------------------------------------------------------------------------
// The debounce, the rate and the statuses (A10c chunk C3)
// ---------------------------------------------------------------------------------------------

/// A mute of Bob that is its own write when it runs alone (`value` differs from the last).
async fn write_one(w: &World, value: i64) {
    w.mute(BOB, Some(value));
    assert_eq!(w.run().await, Done::Finished);
}

/// BQ4 (the debounce): a click makes the job due ONE SECOND LATER, and every click inside that
/// second moves the due time on (a trailing debounce: `delay_to` only moves later), so a burst is
/// one row, one run and one PUT carrying every change. Mutant: the job is queued at `now`.
#[tokio::test]
async fn clicks_within_a_second_are_one_job_run() {
    let w = two_people();
    let t0 = w.now();
    w.archive(ADA);
    assert_eq!(w.queued(JobKind::StorageWrite), 1);
    assert_eq!(
        w.next_at(),
        Some(t0 + 1_000),
        "a second after the click, not at it"
    );
    assert!(!w.due(), "not due at the click");

    w.later(900);
    w.mute(BOB, Some(9_000));
    assert_eq!(
        w.queued(JobKind::StorageWrite),
        1,
        "one row for both clicks"
    );
    assert_eq!(
        w.next_at(),
        Some(t0 + 900 + 1_000),
        "the second click moved the due time on: a trailing debounce"
    );
    w.later(999);
    assert!(
        !w.due(),
        "the first click's second is over, the last click's is not"
    );
    w.later(1);
    assert!(w.due(), "due a second after the last click");

    assert_eq!(w.run().await, Done::Finished);
    let state = w.state();
    assert_eq!(state.puts, 1, "one run, one PUT");
    assert_eq!(state.written[0].records.len(), 2, "with both changes");
}

/// BQ4 (Y4), the 409 back-off: after three conflicts the job waits 30 s; a click inside the wait
/// makes it due a second after the click, which is LATER than nothing, so the wait stands. No PUT
/// goes out before it ends, and then one write carries both clicks. Mutant: a click pulls `next_at`
/// forward (`schedule_at`, the earlier of the two times).
#[tokio::test]
async fn a_click_during_the_conflict_back_off_sends_no_put_before_it_ends() {
    let w = two_people();
    w.archive(ADA);
    for (n, wait) in [(1u8, 1_000), (2, 5_000), (3, 30_000)] {
        let current = key(if n == 1 { 2 } else { 0xb0 + n - 1 });
        let phone = contact(0xb0 + n, BOB, |c| c.marked_unread = n % 2 == 1);
        w.state()
            .answers
            .push_back(Answer::PhoneFirst(Box::new(phone), current));
        assert_eq!(
            w.run().await,
            Done::RepeatAt(w.now() + wait),
            "conflict {n}"
        );
        if n < 3 {
            w.later(u64::try_from(wait).unwrap());
        }
    }
    let until = w.now() + 30_000;
    assert_eq!(w.next_at(), Some(until));
    let puts = w.state().puts;

    w.later(2_000);
    w.mute(ADA, Some(9_000));
    assert_eq!(
        w.next_at(),
        Some(until),
        "the click does not pull the job forward"
    );
    w.later(8_000);
    assert!(!w.due(), "10 s into the wait");
    w.later(19_999);
    assert!(!w.due(), "a millisecond before it ends");
    assert_eq!(w.state().puts, puts, "no PUT during the wait");
    w.later(1);
    assert!(w.due(), "due when the wait ends");
    assert_eq!(w.run().await, Done::Finished);
    let state = w.state();
    assert_eq!(state.puts, puts + 1);
    assert_eq!(state.written.len(), 1);
    assert_eq!(
        state.written[0].records.len(),
        1,
        "both clicks are on Ada's record"
    );
    let ada = state.written[0].contact(0);
    assert!(ada.archived);
    assert_eq!(ada.muted_until_timestamp, 9_000);
}

/// BQ4: the 7th write in ten minutes waits until the first one leaves the window. The run says so
/// before it makes any request, holds the kind (a remembered hold, so no helper makes the job due
/// earlier), the click stays and a later click batches into the same write. Mutants: the cap is not
/// applied; the early look is gone (a manifest is fetched first).
#[tokio::test]
async fn the_seventh_write_in_ten_minutes_waits_and_the_clicks_batch() {
    let w = two_people();
    let t0 = w.now();
    for i in 0..6 {
        write_one(&w, 10_000 + i).await;
        w.later(1_000);
    }
    assert_eq!(w.times(), 6);
    assert_eq!(w.state().puts, 6);
    let fetched = w.state().manifest_paths.len();

    w.mute(BOB, Some(20_000));
    assert_eq!(
        w.run().await,
        Done::RepeatAt(t0 + 600_000),
        "the first write leaves the window ten minutes after it was made"
    );
    assert_eq!(w.state().puts, 6, "no PUT");
    assert_eq!(
        w.state().manifest_paths.len(),
        fetched,
        "found out without a request"
    );
    assert!(
        w.intent(&person(BOB), MUTED_UNTIL).is_some(),
        "the click waits"
    );
    let floor =
        w.f.db
            .read(|c| jobs::due_floor(c, JobKind::StorageWrite, w.now()))
            .unwrap();
    assert_eq!(floor, t0 + 600_000, "remembered as the kind's hold");

    w.archive(ADA);
    assert_eq!(
        w.next_at(),
        Some(t0 + 600_000),
        "a click does not pull the job forward"
    );
    w.later(593_999);
    assert_eq!(w.now(), t0 + 599_999);
    assert!(!w.due());
    w.later(1);
    assert!(w.due(), "due when a place is free");

    assert_eq!(w.run().await, Done::Finished);
    let state = w.state();
    assert_eq!(state.puts, 7);
    assert_eq!(state.written[6].records.len(), 2, "both clicks, one write");
    drop(state);
    assert_eq!(w.times(), 7);
}

/// BQ4: 30 writes in an hour, two minutes apart (no ten-minute window is full), and the 31st waits
/// for the first to leave the hour.
#[tokio::test]
async fn the_thirty_first_write_in_an_hour_waits() {
    let w = two_people();
    let t0 = w.now();
    for i in 0..30 {
        write_one(&w, 10_000 + i).await;
        if i < 29 {
            w.later(120_000);
        }
    }
    w.later(60_000);
    assert_eq!(w.now(), t0 + 59 * 60_000);

    w.mute(BOB, Some(99_000));
    assert_eq!(w.run().await, Done::RepeatAt(t0 + 3_600_000));
    assert_eq!(w.state().puts, 30);

    w.later(59_999);
    assert!(!w.due());
    w.later(1);
    assert!(w.due());
    assert_eq!(w.run().await, Done::Finished);
    assert_eq!(w.state().puts, 31);
}

/// BQ4: a restart keeps the times and the hold. The job row is gone (a dropped job) and the
/// click makes a new one, which is not due before the hold ends either. Mutants: the time is not
/// stored; the cap is not applied.
#[tokio::test]
async fn a_restart_keeps_the_times_and_the_hold() {
    let w = two_people();
    let t0 = w.now();
    for i in 0..6 {
        write_one(&w, 10_000 + i).await;
        w.later(1_000);
    }
    w.mute(BOB, Some(20_000));
    assert_eq!(w.run().await, Done::RepeatAt(t0 + 600_000));

    let w = w.restarted();
    assert_eq!(w.times(), 6, "the times are in the database");
    assert_eq!(w.next_at(), Some(t0 + 600_000));
    // The job row is lost: a new click makes a new row, and the remembered hold bounds it.
    w.f.db
        .zone(|tx| -> Result<()> {
            tx.execute("DELETE FROM jobs WHERE kind = 'storage_write'", [])?;
            Ok(())
        })
        .unwrap();
    w.archive(ADA);
    assert_eq!(
        w.next_at(),
        Some(t0 + 600_000),
        "not a second after the click: the hold outlives the restart"
    );
    // And a run that ignores the row's time still finds the cap by the stored times.
    assert_eq!(w.run().await, Done::RepeatAt(t0 + 600_000));
    assert_eq!(w.state().puts, 6);

    w.later(600_000 - 6_000);
    assert_eq!(w.run().await, Done::Finished);
    assert_eq!(w.state().puts, 7);
}

/// What the rate counts: a write the server applied or may have applied (a 200, a lost answer, a
/// 5xx: R3) and nothing else. A refusal below 500 (400, 429, 409) says nothing was applied and the
/// time is taken back; a run that sends nothing makes no time. Mutant: a status keeps its time.
#[tokio::test]
async fn only_a_write_the_server_may_have_applied_counts() {
    let w = two_people();
    w.archive(ADA);
    let phone = contact(0xb2, BOB, |c| c.marked_unread = true);
    {
        let mut state = w.state();
        state.answers.push_back(Answer::Status(400, None));
        state.answers.push_back(Answer::Status(429, Some("1")));
        state
            .answers
            .push_back(Answer::PhoneFirst(Box::new(phone), key(2)));
        state.answers.push_back(Answer::Lost);
    }
    for status in ["a 400", "a 429", "a 409"] {
        assert!(
            matches!(w.run_and_wait().await, Done::RepeatAt(_)),
            "{status}"
        );
        assert_eq!(w.times(), 0, "{status} applied nothing");
    }
    assert!(matches!(w.run_and_wait().await, Done::RepeatAt(_)));
    assert_eq!(
        w.times(),
        1,
        "a lost answer: the write may be on the server"
    );
    assert_eq!(w.run().await, Done::Finished);
    assert_eq!(
        w.times(),
        2,
        "judged not landed: written again, counted again"
    );
    assert_eq!(w.state().written.len(), 1);

    // A 5xx counts (R3): the server may have applied it, whatever the scripted one did; the retry
    // counts again.
    let w = two_people();
    w.archive(ADA);
    w.state().answers.push_back(Answer::Status(503, None));
    assert!(matches!(w.run().await, Done::RepeatAt(_)));
    assert_eq!(w.times(), 1, "a 503: the write may be on the server");
    w.later(1_000);
    assert_eq!(w.run().await, Done::Finished);
    assert_eq!(
        w.times(),
        2,
        "judged not landed: written again, counted again"
    );

    // A run that sends nothing.
    let w = two_people();
    w.archive(ADA);
    w.unlist(&key(1));
    assert!(matches!(w.run().await, Done::RepeatAt(_)));
    assert_eq!(w.times(), 0);
}

/// BQ4, the pure part: a window frees a place when the `cap`-th newest write leaves it; the later
/// of two full windows decides; a time after now (the clock stepped back) counts as now.
#[test]
fn the_windows_free_a_place_when_the_oldest_write_leaves() {
    let at = |n: i64| 1_000 * n;
    let five: Vec<i64> = (0..5).map(at).collect();
    assert_eq!(rate::release_at(&five, 5_000, &rate::WINDOWS), None, "room");
    let six: Vec<i64> = (0..6).map(at).collect();
    assert_eq!(rate::release_at(&six, 5_000, &rate::WINDOWS), Some(600_000));
    assert_eq!(
        rate::release_at(&six, 599_999, &rate::WINDOWS),
        Some(600_000)
    );
    assert_eq!(
        rate::release_at(&six, 600_000, &rate::WINDOWS),
        None,
        "the oldest is out at the very end of its window"
    );
    let mut shuffled = six.clone();
    shuffled.reverse();
    assert_eq!(
        rate::release_at(&shuffled, 5_000, &rate::WINDOWS),
        Some(600_000)
    );
    // More than the cap inside (the cap changed, a clock): the cap-th newest decides.
    let eight: Vec<i64> = (0..8).map(at).collect();
    assert_eq!(
        rate::release_at(&eight, 8_000, &rate::WINDOWS),
        Some(602_000)
    );
    // Both windows full: the later wait.
    let thirty: Vec<i64> = (0..30).map(at).collect();
    assert_eq!(
        rate::release_at(&thirty, 30_000, &rate::WINDOWS),
        Some(3_600_000)
    );
    // An hour apart: nothing counts.
    assert_eq!(rate::release_at(&six, 4_000_000, &rate::WINDOWS), None);
    // The future counts as now.
    assert_eq!(rate::release_at(&[10_000], 1_000, &[(100, 1)]), Some(1_100));
    assert_eq!(rate::release_at(&[], 1_000, &rate::WINDOWS), None);
    assert_eq!(rate::WINDOWS, [(600_000, 6), (3_600_000, 30)], "BQ4");
}

/// A kill keeps the time of the write in flight: it is written in the zone of the row.
#[test]
fn a_kill_keeps_the_time_of_the_write_with_its_row() {
    if !kourier_db::failpoint::ENABLED {
        return;
    }
    for failpoint in [
        FAILPOINT_BEFORE_STORAGE_WRITE_PUT,
        FAILPOINT_AFTER_STORAGE_WRITE_PUT,
        FAILPOINT_AFTER_STORAGE_WRITE_COMMIT,
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let output = run_child(failpoint, tmp.path());
        assert_eq!(output.status.signal(), Some(6), "aborted at {failpoint}");
        let dir = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.join("backend.db").exists())
            .expect("the child's database");
        let db = crate::db::open(
            &dir.join("backend.db"),
            &kourier_backend_api::DbKey::new([5; 32]),
        )
        .unwrap();
        assert_eq!(db.read(rate::stored).unwrap(), 1, "at {failpoint}");
    }
}

// ---------------------------------------------------------------------------------------------
// The statuses (C3)
// ---------------------------------------------------------------------------------------------

/// 401 from the PUT: the credentials are asked for again, ONCE, and the very same write goes again
/// with them: two PUTs with different credentials, one write on the server, counted once, no
/// failure on record.
#[tokio::test]
async fn a_401_writes_again_with_new_credentials_and_counts_one_write() {
    let w = two_people();
    w.archive(ADA);
    w.state().answers.push_back(Answer::Status(401, None));
    assert_eq!(w.run().await, Done::Finished);
    {
        let state = w.state();
        assert_eq!(state.puts, 2);
        assert_eq!(state.auths, 2, "one at the start, one after the 401");
        assert_ne!(state.put_auths[0], state.put_auths[1], "new credentials");
        assert_eq!(state.written.len(), 1);
        assert_eq!(state.written[0].manifest.version, 8, "the same write");
    }
    assert_eq!(w.times(), 1, "one write, two PUTs");
    assert_eq!(w.tries(), Tries::default());
    assert_eq!(w.queued(JobKind::FetchLatestSync), 1);
}

/// 403 twice: the second refusal is a status like any other: nothing was applied, the row goes,
/// the click stays, one failure is counted, the time is taken back, the job looks again a second
/// later, and then the write lands.
#[tokio::test]
async fn a_second_403_is_a_failure_and_the_next_run_writes() {
    let w = two_people();
    w.archive(ADA);
    {
        let mut state = w.state();
        state.answers.push_back(Answer::Status(403, None));
        state.answers.push_back(Answer::Status(403, None));
    }
    assert_eq!(w.run().await, Done::RepeatAt(w.now() + 1_000));
    {
        let state = w.state();
        assert_eq!(state.puts, 2, "once more with new credentials, no more");
        assert_eq!(state.auths, 2);
        assert_ne!(state.put_auths[0], state.put_auths[1]);
        assert!(state.written.is_empty());
    }
    assert!(w.row().is_none(), "the server applied nothing");
    assert!(w.intent(&person(ADA), ARCHIVED).is_some());
    assert_eq!(w.tries().failures, 1);
    assert_eq!(w.times(), 0);

    w.later(1_000);
    assert_eq!(w.run().await, Done::Finished);
    assert_eq!(w.state().puts, 3);
    assert_eq!(w.tries(), Tries::default());
}

/// 429 without a `Retry-After` (or one that is not a number of seconds): a minute. The kind is
/// held, so a click during it makes nothing due, and the changes of both clicks go out together
/// when it ends. Mutant: a click pulls `next_at` forward.
#[tokio::test]
async fn a_429_without_retry_after_waits_a_minute_and_a_click_cannot_knock_earlier() {
    let w = two_people();
    w.archive(ADA);
    w.state().answers.push_back(Answer::Status(429, None));
    let t = w.now();
    assert_eq!(w.run().await, Done::RepeatAt(t + 60_000));
    assert!(w.row().is_none(), "nothing was applied");
    assert_eq!(w.times(), 0);
    assert_eq!(w.tries().failures, 0, "a rate limit is no failure");

    w.later(5_000);
    w.mute(BOB, Some(9_000));
    assert_eq!(
        w.next_at(),
        Some(t + 60_000),
        "the click does not knock earlier"
    );
    w.later(54_999);
    assert!(!w.due());
    assert_eq!(w.state().puts, 1, "no PUT during the hold");
    w.later(1);
    assert!(w.due());
    assert_eq!(w.run().await, Done::Finished);
    {
        let state = w.state();
        assert_eq!(state.puts, 2);
        assert_eq!(state.written[0].records.len(), 2, "both clicks, one write");
    }

    // A date is no number of seconds: the same minute.
    let w = two_people();
    w.archive(ADA);
    w.state()
        .answers
        .push_back(Answer::Status(429, Some("Wed, 21 Oct 2026 07:28:00 GMT")));
    assert_eq!(w.run().await, Done::RepeatAt(w.now() + 60_000));
    // Two hours: capped at one (Y3).
    let w = two_people();
    w.archive(ADA);
    w.state()
        .answers
        .push_back(Answer::Status(429, Some("7200")));
    assert_eq!(w.run().await, Done::RepeatAt(w.now() + 3_600_000));
}

/// A server that keeps refusing (400: it applied nothing): the waits follow the normal back-off
/// (`jobs::backoff`: Desktop's ×1.9 steps), never under a second (a run costs two requests),
/// never shorter than the one before, and stop growing at the back-off's own cap (15 minutes,
/// inside Y3's hour). Nothing is dropped, nothing is counted as a write, and the write lands when
/// the server recovers. Mutant: a status keeps its time (the rate would stop the sequence at the
/// seventh failure). (A server that answers 5xx is a lost answer, R3: it counts, and the rate may
/// stop the sequence; `a_5xx_that_applied_nothing_is_written_again`.)
#[tokio::test]
async fn failures_back_off_by_the_normal_steps_and_stop_at_the_cap() {
    let w = two_people();
    w.archive(ADA);
    let mut waits = Vec::new();
    for _ in 0..25 {
        w.state().answers.push_back(Answer::Status(400, None));
        let before = w.now();
        let Done::RepeatAt(at) = w.run_and_wait().await else {
            panic!("every failure is a RepeatAt");
        };
        waits.push(at - before);
    }
    assert_eq!(waits[..3], [1_000, 1_000, 1_000], "the floor");
    assert!(waits.windows(2).all(|pair| pair[0] <= pair[1]), "{waits:?}");
    for pair in waits[3..14].windows(2) {
        let ratio = pair[1] as f64 / pair[0] as f64;
        assert!((1.85..1.95).contains(&ratio), "x1.9 per step: {waits:?}");
    }
    assert!(
        waits[14..].iter().all(|wait| *wait == 900_000),
        "the back-off's cap: {waits:?}"
    );
    assert!(waits.iter().all(|wait| *wait <= 3_600_000), "Y3");
    assert_eq!(w.tries().failures, 25);
    {
        let state = w.state();
        assert_eq!(state.puts, 25);
        assert!(state.written.is_empty());
    }
    assert_eq!(w.times(), 0, "none of them was a write");
    assert!(w.intent(&person(ADA), ARCHIVED).is_some());
    assert_eq!(w.queued(JobKind::StorageWrite), 1);

    assert_eq!(w.run().await, Done::Finished, "the server recovers");
    assert_eq!(w.state().written.len(), 1);
    assert_eq!(w.tries(), Tries::default());
}

// ---------------------------------------------------------------------------------------------
// The fetch-latest job (C3, M8)
// ---------------------------------------------------------------------------------------------

/// The runner of `fetch_latest_sync` sends: over the scripted service, which has no sessions and
/// no sends, the arm fails with an error (the runner's backoff retries it for the hour of the row),
/// and is not a silent `Finished` as before C3. The content is proven in `send::content::tests`
/// and the whole send in `storage_writes::a_kill_after_the_server_applied_still_tells_the_phone_to_fetch`.
#[tokio::test(start_paused = true)]
async fn the_runner_of_the_fetch_latest_sync_tries_to_send_it() {
    let w = two_people();
    let now = w.now();
    w.f.db
        .zone(|tx| {
            jobs::request(
                tx,
                JobKind::FetchLatestSync,
                "",
                now,
                Some(Duration::from_secs(3600)),
            )
        })
        .unwrap();
    let job =
        w.f.db
            .read(|c| jobs::next_due(c, now, true, false))
            .unwrap()
            .expect("the row is due");
    assert_eq!(job.kind, JobKind::FetchLatestSync);
    assert_eq!(
        job.expires_at,
        Some(now + 3_600_000),
        "the news is old after an hour"
    );
    let done = tokio::time::timeout(
        Duration::from_secs(600),
        crate::maintenance::run_job(&w.shared, &w.socket, &job),
    )
    .await
    .expect("the send ends (the clock is paused: waits cost nothing)");
    assert!(done.is_err(), "it tried to send: {done:?}");
}

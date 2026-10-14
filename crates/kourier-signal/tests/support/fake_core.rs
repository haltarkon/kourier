//! A stand-in for the core (S6 replaces it with `kourier-core`): spawns the Signal backend's
//! account actor through `Network::spawn`, sends it commands, and plays the core's side of the
//! boundary. Durable envelopes are committed to a scratch SQLCipher table (the sink) before
//! their `Ack` completes, idempotently on the message id as the real ingest is (§4.9);
//! ephemeral events (login steps) are queued for the test.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, BackendConfig, BackendEnv, BackendEvent, Command, ContextParams, DbKey,
    Envelope, Exit, InitialSync, LocalTestEnv, Network, OutgoingKind, OutgoingRequest, Relink,
    StartMode, SystemClock, TransmitOutcome,
};
use kourier_db::rusqlite::{self, Connection, OptionalExtension};
use kourier_model::{
    AccountId, AccountUuid, Body, ConnState, ConvKey, LoginStep, LoginStepKind, MessageId,
    PersistError, Redacted, UserId,
};
use kourier_signal::SignalNetwork;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::harness::Handshake;

/// The local test environment of a running harness; the CA PEM is written into `dir`.
pub fn local_env(handshake: &Handshake, dir: &Path) -> std::io::Result<BackendEnv> {
    let ca = dir.join("mock-ca.pem");
    std::fs::write(&ca, &handshake.ca_pem)?;
    Ok(BackendEnv::LocalTest(LocalTestEnv {
        host: handshake.host.clone(),
        port: handshake.port,
        ca_cert: Some(ca),
        trust_root: Some(handshake.trust_root.clone()),
        zk_params: Some(handshake.zk_public_params.clone()),
    }))
}

/// One account's files: `backend.db` and its key, as the core keeps them.
#[derive(Clone, Debug)]
pub struct AccountFiles {
    /// `accounts/<uuid>/`.
    pub dir: PathBuf,
    /// The account uuid.
    pub uuid: AccountUuid,
    /// The `backend.db` key.
    pub key: [u8; 32],
}

impl AccountFiles {
    /// A new account directory under `parent`.
    pub fn new(parent: &Path) -> std::io::Result<Self> {
        let uuid = AccountUuid::new_random();
        let dir = parent.join(uuid.to_string());
        std::fs::create_dir_all(dir.join("blobs"))?;
        let mut key = [0u8; 32];
        for (i, b) in key.iter_mut().enumerate() {
            *b = u8::try_from(i).unwrap_or(0) ^ uuid.0.as_bytes()[i % 16];
        }
        Ok(Self { dir, uuid, key })
    }

    /// `backend.db`.
    pub fn backend_db(&self) -> PathBuf {
        self.dir.join("backend.db")
    }

    /// Counts the rows of a `backend.db` table (the actor must not be running).
    pub fn count_rows(&self, table: &str) -> rusqlite::Result<i64> {
        let conn = kourier_db::open(&self.backend_db(), &kourier_db::DbKey::from(self.key))
            .map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))?;
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
    }

    /// Runs `sql` on `backend.db` (the actor must not be running).
    pub fn execute(&self, sql: &str) -> rusqlite::Result<usize> {
        let conn = kourier_db::open(&self.backend_db(), &kourier_db::DbKey::from(self.key))
            .map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))?;
        conn.execute(sql, [])
    }

    /// One integer (or NULL) from `backend.db` through a read-only connection; safe while the
    /// actor runs (WAL readers do not block it).
    pub fn peek_i64(&self, sql: &str) -> rusqlite::Result<Option<i64>> {
        let conn =
            kourier_db::open_read_only(&self.backend_db(), &kourier_db::DbKey::from(self.key))
                .map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))?;
        conn.query_row(sql, [], |r| r.get(0))
            .optional()
            .map(Option::flatten)
    }

    /// Waits until `sql` (one integer) satisfies `pred` while the actor runs.
    pub async fn wait_for_i64(
        &self,
        what: &str,
        sql: &str,
        pred: impl Fn(Option<i64>) -> bool,
    ) -> Option<i64> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let value = self.peek_i64(sql).unwrap_or_else(|e| panic!("{what}: {e}"));
            if pred(value) {
                return value;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what} (last {value:?})"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// The device ids we hold an ACI session with for `aci` (the actor must not be running).
    pub fn session_devices(&self, aci: &str) -> rusqlite::Result<Vec<u32>> {
        let conn = kourier_db::open(&self.backend_db(), &kourier_db::DbKey::from(self.key))
            .map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))?;
        let mut stmt = conn.prepare(
            "SELECT device_id FROM sessions
             WHERE our_kind = 'aci' AND their_service_id = ?1 ORDER BY device_id",
        )?;
        stmt.query_map([aci], |r| r.get(0))?.collect()
    }
}

/// The scratch "core.db": what the fake core committed.
#[derive(Clone)]
pub struct Sink(Arc<Mutex<Connection>>);

/// One message row of the sink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SinkMessage {
    /// `UserId` of the author.
    pub author: String,
    /// Network-local id (Signal: the sent timestamp).
    pub local: String,
    /// Conversation key.
    pub conv: String,
    /// Sender device.
    pub device: Option<u32>,
    /// Text.
    pub body: String,
    /// Arrived before the catch-up boundary.
    pub catch_up: bool,
}

impl Sink {
    /// Opens (creating) the sink at `path`.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = kourier_db::open(path, &kourier_db::DbKey::from([7; 32]))
            .map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages(
               author TEXT NOT NULL, local TEXT NOT NULL, conv TEXT NOT NULL, device INTEGER,
               body TEXT NOT NULL, kind TEXT NOT NULL DEFAULT '', catch_up INTEGER NOT NULL,
               PRIMARY KEY (author, local));
             CREATE TABLE IF NOT EXISTS deliveries(
               seq INTEGER PRIMARY KEY, author TEXT NOT NULL, local TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS caps_flags(seq INTEGER PRIMARY KEY, flags TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS events(seq INTEGER PRIMARY KEY, name TEXT NOT NULL,
               detail TEXT);
             CREATE TABLE IF NOT EXISTS message_json(seq INTEGER PRIMARY KEY,
               author TEXT NOT NULL, local TEXT NOT NULL, json TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS behavior(k TEXT PRIMARY KEY, v TEXT NOT NULL);",
        )?;
        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How this fake core behaves towards imported history (kept in the sink's own file,
    /// so a helper process on the same file behaves alike): `ack_delay_ms` (a pause before
    /// every envelope with a history batch is committed), `refuse` (an envelope with a
    /// history message whose text contains this is refused for good), `stick` (an envelope
    /// with a history message whose text contains this (several, separated by `|`) is
    /// answered as retryable, again and again: a message the real core cannot store is
    /// never refused for good, its ingest fails and the backend ships it again; like the
    /// real core it first commits what stands before the failing batch in the envelope and
    /// skips what stands behind it), `stall` (`1`: no envelope with a history
    /// batch is answered until it is set back), `refuse_wide_chats` (`1`: every envelope that
    /// upserts two or more different chats is refused for good, after
    /// `refuse_wide_chats_delay_ms` if that is set: a core that cannot take a row of several
    /// chats at once but takes each chat alone; `refused_wide` counts the refusals).
    pub fn set_behavior(&self, key: &str, value: Option<&str>) {
        let conn = self.conn();
        match value {
            Some(value) => conn.execute(
                "INSERT INTO behavior(k, v) VALUES (?1, ?2)
                 ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                (key, value),
            ),
            None => conn.execute("DELETE FROM behavior WHERE k = ?1", [key]),
        }
        .expect("behavior");
    }

    pub fn behavior(&self, key: &str) -> Option<String> {
        self.conn()
            .query_row("SELECT v FROM behavior WHERE k = ?1", [key], |r| r.get(0))
            .optional()
            .expect("behavior")
    }

    /// How many history messages the core committed (every delivery counted once per
    /// envelope that carried it).
    pub fn history_deliveries(&self) -> i64 {
        self.conn()
            .query_row(
                "SELECT coalesce(sum(CAST(json_extract(detail, '$.n') AS INTEGER)), 0)
                   FROM events WHERE name = 'HistoryBatch'",
                [],
                |r| r.get(0),
            )
            .expect("history deliveries")
    }

    /// One core transaction for one envelope: idempotent on `(author, local)`, every delivery
    /// counted.
    fn commit(&self, events: &[BackendEvent]) -> rusqlite::Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for event in events {
            match event {
                BackendEvent::HistoryBatch {
                    conv,
                    messages,
                    has_more,
                    progress,
                } => {
                    for m in messages {
                        let author = m.origin.author().map_or("", UserId::as_str).to_owned();
                        let body = match &m.body {
                            Body::Text(text) => text.body.clone(),
                            // A notice keeps its detail, as a live one does (the group tests
                            // read imported group notices; test data only).
                            Body::System(event) => format!(
                                "<System>{}",
                                serde_json::to_string(event).unwrap_or_default()
                            ),
                            other => format!("<{}>", other.kind_name()),
                        };
                        tx.execute(
                            "INSERT INTO deliveries(author, local) VALUES (?1, ?2)",
                            (&author, &*m.local),
                        )?;
                        tx.execute(
                            "INSERT INTO message_json(author, local, json) VALUES (?1, ?2, ?3)",
                            (
                                &author,
                                &*m.local,
                                serde_json::to_string(m).unwrap_or_default(),
                            ),
                        )?;
                        // Idempotent on (author, local), as the core's ingest is.
                        tx.execute(
                            "INSERT OR IGNORE INTO messages(author, local, conv, device, body,
                               kind, catch_up) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                            (
                                &author,
                                &*m.local,
                                m.conv.encode(),
                                m.origin.device(),
                                body,
                                m.body.kind_name(),
                                m.catch_up,
                            ),
                        )?;
                    }
                    let detail = serde_json::json!({
                        "conv": conv.encode(),
                        "n": messages.len(),
                        "has_more": has_more,
                        "progress": progress,
                    });
                    tx.execute(
                        "INSERT INTO events(name, detail) VALUES ('HistoryBatch', ?1)",
                        [detail.to_string()],
                    )?;
                }
                BackendEvent::Message(m) => {
                    let author = m.origin.author().map_or("", UserId::as_str).to_owned();
                    let body = match &m.body {
                        Body::Text(text) => text.body.clone(),
                        // System and placeholder bodies keep their detail for the group and retry
                        // tests (test data only; the real core never logs these).
                        Body::System(event) => format!(
                            "<System>{}",
                            serde_json::to_string(event).unwrap_or_default()
                        ),
                        Body::Placeholder(placeholder) => format!(
                            "<Placeholder>{}",
                            serde_json::to_string(placeholder).unwrap_or_default()
                        ),
                        other => format!("<{}>", other.kind_name()),
                    };
                    tx.execute(
                        "INSERT INTO deliveries(author, local) VALUES (?1, ?2)",
                        (&author, &*m.local),
                    )?;
                    // The whole message for the S13 tests (quotes, timers, placeholders; test
                    // data only).
                    tx.execute(
                        "INSERT INTO message_json(author, local, json) VALUES (?1, ?2, ?3)",
                        (
                            &author,
                            &*m.local,
                            serde_json::to_string(m).unwrap_or_default(),
                        ),
                    )?;
                    // Idempotent on (author, local); a real message replaces an Undecryptable
                    // placeholder with the same net id, the way the core does (§3.9).
                    tx.execute(
                        "INSERT INTO messages(author, local, conv, device, body, kind, catch_up)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                         ON CONFLICT(author, local) DO UPDATE SET
                           conv = excluded.conv, device = excluded.device, body = excluded.body,
                           kind = excluded.kind, catch_up = excluded.catch_up
                         WHERE messages.kind = 'Placeholder' AND excluded.kind != 'Placeholder'",
                        (
                            &author,
                            &*m.local,
                            m.conv.encode(),
                            m.origin.device(),
                            body,
                            m.body.kind_name(),
                            m.catch_up,
                        ),
                    )?;
                }
                BackendEvent::AccountReady {
                    own_ids,
                    device,
                    number,
                    ..
                } => {
                    let detail = format!(
                        "{} device {}",
                        own_ids
                            .iter()
                            .map(UserId::as_str)
                            .collect::<Vec<_>>()
                            .join(","),
                        device.unwrap_or(0)
                    );
                    tx.execute(
                        "INSERT INTO events(name, detail) VALUES ('AccountReady', ?1)",
                        [detail],
                    )?;
                    // The account's number as the backend reported it (mock numbers only).
                    if let Some(number) = number {
                        tx.execute(
                            "INSERT INTO events(name, detail) VALUES ('AccountNumber', ?1)",
                            [number.expose()],
                        )?;
                    }
                }
                BackendEvent::AccountCaps(caps) => {
                    let detail = format!(
                        "max_attachment_bytes={:?} long_text_bytes={:?}",
                        caps.max_attachment_bytes, caps.long_text_bytes
                    );
                    tx.execute(
                        "INSERT INTO events(name, detail) VALUES ('AccountCaps', ?1)",
                        [detail],
                    )?;
                    // The flags beside it, as JSON, in a table of their own (the event list is
                    // compared as it is by other tests).
                    tx.execute(
                        "INSERT INTO caps_flags(flags) VALUES (?1)",
                        [serde_json::to_string(&caps.flags).unwrap_or_default()],
                    )?;
                }
                // Contacts, chats and settings keep their JSON for the S9 tests (test data only;
                // the real core never logs these).
                other @ (BackendEvent::ContactUpsert(_)
                | BackendEvent::ConversationUpsert(_)
                | BackendEvent::PinnedConversations(_)
                | BackendEvent::AccountSettings(_)
                | BackendEvent::AttachmentDone { .. }
                | BackendEvent::IdentityMerged { .. }
                | BackendEvent::Edited { .. }
                | BackendEvent::Deleted { .. }
                | BackendEvent::Reaction { .. }
                | BackendEvent::ConversationCleared { .. }
                | BackendEvent::AttachmentDeleted { .. }
                | BackendEvent::Notice(_)
                | BackendEvent::AttachmentRestore { .. }
                | BackendEvent::CommandPersisted { .. }
                | BackendEvent::Receipts { .. }) => {
                    let detail = serde_json::to_string(other).unwrap_or_default();
                    tx.execute(
                        "INSERT INTO events(name, detail) VALUES (?1, ?2)",
                        (other.name(), detail),
                    )?;
                }
                other => {
                    tx.execute("INSERT INTO events(name) VALUES (?1)", [other.name()])?;
                }
            }
        }
        tx.commit()
    }

    /// The message `(author, local)`, if committed.
    pub fn message(&self, author: &str, local: &str) -> Option<SinkMessage> {
        self.conn()
            .query_row(
                "SELECT author, local, conv, device, body, catch_up FROM messages
                 WHERE author = ?1 AND local = ?2",
                (author, local),
                |r| {
                    Ok(SinkMessage {
                        author: r.get(0)?,
                        local: r.get(1)?,
                        conv: r.get(2)?,
                        device: r.get(3)?,
                        body: r.get(4)?,
                        catch_up: r.get(5)?,
                    })
                },
            )
            .optional()
            .ok()
            .flatten()
    }

    /// How many envelopes carried the message `(author, local)` to the core.
    pub fn deliveries(&self, author: &str, local: &str) -> i64 {
        self.conn()
            .query_row(
                "SELECT count(*) FROM deliveries WHERE author = ?1 AND local = ?2",
                (author, local),
                |r| r.get(0),
            )
            .unwrap_or(-1)
    }

    /// Message rows from `author` whose text starts with `prefix`.
    pub fn messages_with_prefix(&self, author: &str, prefix: &str) -> usize {
        self.conn()
            .query_row(
                "SELECT count(*) FROM messages WHERE author = ?1 AND substr(body, 1, ?3) = ?2",
                (
                    author,
                    prefix,
                    i64::try_from(prefix.len()).unwrap_or(i64::MAX),
                ),
                |r| r.get::<_, i64>(0),
            )
            .map_or(0, |n| usize::try_from(n).unwrap_or(0))
    }

    /// The message rows of conversation `conv` (its encoded key), in commit order.
    pub fn messages_in(&self, conv: &str) -> Vec<SinkMessage> {
        let conn = self.conn();
        let Ok(mut stmt) = conn.prepare(
            "SELECT author, local, conv, device, body, catch_up FROM messages WHERE conv = ?1
             ORDER BY rowid",
        ) else {
            return Vec::new();
        };
        stmt.query_map([conv], |r| {
            Ok(SinkMessage {
                author: r.get(0)?,
                local: r.get(1)?,
                conv: r.get(2)?,
                device: r.get(3)?,
                body: r.get(4)?,
                catch_up: r.get(5)?,
            })
        })
        .and_then(|rows| rows.collect())
        .unwrap_or_default()
    }

    /// Message rows from `author`.
    pub fn messages_from(&self, author: &str) -> i64 {
        self.conn()
            .query_row(
                "SELECT count(*) FROM messages WHERE author = ?1",
                [author],
                |r| r.get(0),
            )
            .unwrap_or(-1)
    }

    /// The committed events named `name` as JSON (contacts, chats, pins, settings, merges).
    pub fn event_values(&self, name: &str) -> Vec<serde_json::Value> {
        self.events(name)
            .into_iter()
            .flatten()
            .filter_map(|detail| serde_json::from_str::<serde_json::Value>(&detail).ok())
            .filter_map(|mut value| value.get_mut(name).map(serde_json::Value::take))
            .collect()
    }

    /// Every delivery of the message `(author, local)` as JSON (`IncomingMessage`), oldest
    /// first.
    pub fn message_values(&self, author: &str, local: &str) -> Vec<serde_json::Value> {
        let conn = self.conn();
        let Ok(mut stmt) = conn
            .prepare("SELECT json FROM message_json WHERE author = ?1 AND local = ?2 ORDER BY seq")
        else {
            return Vec::new();
        };
        stmt.query_map([author, local], |r| r.get::<_, String>(0))
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<String>>>())
            .unwrap_or_default()
            .iter()
            .filter_map(|json| serde_json::from_str(json).ok())
            .collect()
    }

    /// Waits until the message `(author, local)` is committed and returns it as JSON.
    pub async fn wait_message_value(&self, author: &str, local: &str) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if let Some(value) = self.message_values(author, local).into_iter().next() {
                return value;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the message never reached the core"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Waits until an event named `name` whose JSON satisfies `pred` is committed, and returns
    /// it.
    pub async fn wait_event_value(
        &self,
        name: &str,
        pred: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if let Some(value) = self.event_values(name).into_iter().find(|v| pred(v)) {
                return value;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no {name} event matched"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// The names of the committed non-message events, in commit order.
    pub fn event_names(&self) -> Vec<String> {
        let conn = self.conn();
        let Ok(mut stmt) = conn.prepare("SELECT name FROM events ORDER BY seq") else {
            return Vec::new();
        };
        stmt.query_map([], |r| r.get(0))
            .and_then(|rows| rows.collect())
            .unwrap_or_default()
    }

    /// Waits until `n` events named `name` are committed and returns their details.
    pub async fn wait_events(
        &self,
        name: &str,
        n: usize,
        timeout: Duration,
    ) -> Vec<Option<String>> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let events = self.events(name);
            if events.len() >= n {
                return events;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {n} {name} events (have {})",
                events.len()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// The details of every committed `name` event.
    pub fn events(&self, name: &str) -> Vec<Option<String>> {
        let conn = self.conn();
        let Ok(mut stmt) = conn.prepare("SELECT detail FROM events WHERE name = ?1 ORDER BY seq")
        else {
            return Vec::new();
        };
        stmt.query_map([name], |r| r.get(0))
            .and_then(|rows| rows.collect())
            .unwrap_or_default()
    }

    /// The flags of every committed `AccountCaps` event, as JSON, oldest first.
    pub fn caps_flags(&self) -> Vec<String> {
        let conn = self.conn();
        let Ok(mut stmt) = conn.prepare("SELECT flags FROM caps_flags ORDER BY seq") else {
            return Vec::new();
        };
        stmt.query_map([], |r| r.get(0))
            .and_then(|rows| rows.collect())
            .unwrap_or_default()
    }

    /// Waits until `n` `AccountCaps` events are committed; returns their flags.
    pub async fn wait_caps_flags(&self, n: usize, timeout: Duration) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let flags = self.caps_flags();
            if flags.len() >= n {
                return flags;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {n} AccountCaps events (have {})",
                flags.len()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Waits until the message `(author, local)` is committed.
    pub async fn wait_message(
        &self,
        author: &str,
        local: &str,
        timeout: Duration,
    ) -> Option<SinkMessage> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(message) = self.message(author, local) {
                return Some(message);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

/// The system clock plus an offset the test moves: a suspend (the wall clock jumps ahead while
/// no timer ran) or a clock set wrongly. Sleeps wait out the time that was left when they were
/// first polled, as tokio's timers do across a suspend.
#[derive(Clone, Debug, Default)]
pub struct ShiftClock(Arc<std::sync::atomic::AtomicI64>);

impl ShiftClock {
    /// Moves the clock by `by_ms` (negative: back).
    pub fn shift(&self, by_ms: i64) {
        self.0.fetch_add(by_ms, std::sync::atomic::Ordering::SeqCst);
    }
}

impl kourier_backend_api::Clock for ShiftClock {
    fn now_ms(&self) -> u64 {
        let shift = self.0.load(std::sync::atomic::Ordering::SeqCst);
        SystemClock.now_ms().saturating_add_signed(shift)
    }

    fn sleep_until(&self, deadline_ms: u64) -> kourier_backend_api::Sleep {
        let clock = self.clone();
        Box::pin(async move {
            let wait = deadline_ms.saturating_sub(clock.now_ms());
            tokio::time::sleep(Duration::from_millis(wait)).await;
        })
    }
}

/// The fake core of one running account actor.
pub struct FakeCore {
    commands: mpsc::Sender<Command>,
    conn: watch::Receiver<ConnState>,
    ephemeral: mpsc::UnboundedReceiver<BackendEvent>,
    exited: Option<oneshot::Receiver<Exit>>,
    thread: Option<std::thread::JoinHandle<()>>,
    pump: Option<JoinHandle<()>>,
    // Kept so the actor's watch receivers stay connected.
    _ui: watch::Sender<kourier_backend_api::UiPresence>,
    _prefs: watch::Sender<kourier_backend_api::LocalPrefsView>,
    /// The `AccountProgress` reports that waits for a login step passed over.
    pub progress_seen: Vec<kourier_model::AccountProgress>,
}

/// How long a single wait in these helpers may take.
pub const WAIT: Duration = Duration::from_secs(60);

impl FakeCore {
    /// Spawns the Signal account actor for `files` on the current runtime (the network runtime).
    pub fn start(
        env: &BackendEnv,
        files: &AccountFiles,
        start: StartMode,
        sink: &Sink,
    ) -> Result<Self, kourier_model::BackendError> {
        Self::start_with(SignalNetwork::new(), env, files, start, sink)
    }

    /// [`Self::start`] with a configured network (shorter QR rotation, …).
    pub fn start_with(
        network: SignalNetwork,
        env: &BackendEnv,
        files: &AccountFiles,
        start: StartMode,
        sink: &Sink,
    ) -> Result<Self, kourier_model::BackendError> {
        Self::start_full(network, env, files, start, sink, InitialSync::LinkOnly)
    }

    /// [`Self::start_with`] with the link's initial sync (the app uses `ContactsAndStorage`).
    pub fn start_full(
        network: SignalNetwork,
        env: &BackendEnv,
        files: &AccountFiles,
        start: StartMode,
        sink: &Sink,
        initial_sync: InitialSync,
    ) -> Result<Self, kourier_model::BackendError> {
        let clock = Arc::new(SystemClock);
        Self::start_clocked(network, env, files, start, sink, initial_sync, clock)
    }

    /// [`Self::start_full`] with the actor's clock (a [`ShiftClock`] the test moves).
    pub fn start_clocked(
        network: SignalNetwork,
        env: &BackendEnv,
        files: &AccountFiles,
        start: StartMode,
        sink: &Sink,
        initial_sync: InitialSync,
        clock: Arc<dyn kourier_backend_api::Clock>,
    ) -> Result<Self, kourier_model::BackendError> {
        let config = BackendConfig {
            env: env.clone(),
            initial_sync,
            ..BackendConfig::default()
        };
        Self::start_configured(network, config, files, start, sink, clock)
    }

    /// Spawns the actor with the whole backend configuration as given (its environment, what
    /// a first link must find, …).
    pub fn start_configured(
        network: SignalNetwork,
        config: BackendConfig,
        files: &AccountFiles,
        start: StartMode,
        sink: &Sink,
        clock: Arc<dyn kourier_backend_api::Clock>,
    ) -> Result<Self, kourier_model::BackendError> {
        let (ctx, ends) = AccountContext::new(ContextParams {
            account: AccountId(1),
            uuid: files.uuid,
            start,
            backend_db: files.backend_db(),
            db_key: DbKey::new(files.key),
            blob_dir: files.dir.join("blobs"),
            net_rt: tokio::runtime::Handle::current(),
            cancel: CancellationToken::new(),
            clock,
            config,
            ui: Default::default(),
            local_prefs: Default::default(),
        });
        let handle = network.spawn(ctx)?;
        let (ephemeral_tx, ephemeral) = mpsc::unbounded_channel();
        let pump = tokio::spawn(pump(ends.events, sink.clone(), ephemeral_tx));
        Ok(Self {
            commands: ends.commands,
            conn: ends.conn,
            ephemeral,
            exited: Some(handle.exited),
            thread: handle.thread,
            pump: Some(pump),
            _ui: ends.ui,
            _prefs: ends.local_prefs,
            progress_seen: Vec::new(),
        })
    }

    /// Sends a command.
    pub async fn send(&self, command: Command) {
        self.commands
            .send(command)
            .await
            .unwrap_or_else(|_| panic!("the account actor is gone"));
    }

    /// The next login step (other ephemeral events are skipped).
    pub async fn next_login(&mut self) -> LoginStep {
        loop {
            match tokio::time::timeout(WAIT, self.ephemeral.recv()).await {
                Ok(Some(BackendEvent::Login(step))) => return step,
                Ok(Some(BackendEvent::AccountProgress(p))) => self.progress_seen.push(p),
                Ok(Some(_)) => {}
                Ok(None) => panic!("the actor stopped before the next login step"),
                Err(_) => panic!("no login step within {WAIT:?}"),
            }
        }
    }

    /// The next ephemeral event of any kind (a login step, a history transfer state, …);
    /// `None` when none arrives within `timeout`.
    pub async fn next_ephemeral(&mut self, timeout: Duration) -> Option<BackendEvent> {
        match tokio::time::timeout(timeout, self.ephemeral.recv()).await {
            Ok(Some(event)) => Some(event),
            Ok(None) => panic!("the actor stopped before the next event"),
            Err(_) => None,
        }
    }

    /// The next ephemeral `Typing` event as `(conversation key, by, on)` (others are skipped);
    /// `None` when none arrives within `timeout`.
    pub async fn next_typing(&mut self, timeout: Duration) -> Option<(String, String, bool)> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio::time::timeout_at(deadline, self.ephemeral.recv()).await {
                Ok(Some(BackendEvent::Typing { conv, by, on, .. })) => {
                    return Some((conv.encode(), by.as_str().to_owned(), on));
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return None,
            }
        }
    }

    /// Waits until the connection state satisfies `pred` and returns it.
    pub async fn wait_conn(&mut self, what: &str, pred: impl Fn(&ConnState) -> bool) -> ConnState {
        let waited = match tokio::time::timeout(WAIT, self.conn.wait_for(|s| pred(s))).await {
            Ok(Ok(state)) => Ok(state.clone()),
            Ok(Err(_)) => Err("the actor stopped"),
            Err(_) => Err("timed out"),
        };
        waited.unwrap_or_else(|why| {
            panic!(
                "connection not {what}: {why} (now {})",
                self.conn.borrow().name()
            )
        })
    }

    /// Waits for `Online`.
    pub async fn wait_online(&mut self) {
        self.wait_conn("Online", ConnState::is_online).await;
    }

    /// The current connection state.
    pub fn conn_state(&self) -> ConnState {
        self.conn.borrow().clone()
    }

    /// A receiver of connection-state changes.
    pub fn conn_watch(&self) -> watch::Receiver<ConnState> {
        self.conn.clone()
    }

    /// Starts a QR login (a relink when `relink` is set), answers the device-name step and
    /// returns the steps up to the first QR code, and its URL.
    pub async fn begin_login(&mut self, relink: Option<Relink>) -> (String, Vec<LoginStep>) {
        self.send(Command::StartLogin {
            flow: "qr".to_owned(),
            relink,
            taken: Vec::new(),
        })
        .await;
        let mut steps = Vec::new();
        let input = self.next_login().await;
        assert!(
            matches!(&input.kind, LoginStepKind::Input { fields } if fields[0].id == "device_name"),
            "first step asks for the device name: {input:?}"
        );
        self.send(Command::SubmitLoginInput {
            txn: input.txn,
            values: vec![(
                "device_name".to_owned(),
                Redacted::new("Kourier test".to_owned()),
            )],
        })
        .await;
        steps.push(input);
        let qr = self.next_login().await;
        let LoginStepKind::ShowQr { data, .. } = &qr.kind else {
            panic!("expected a QR code, got {qr:?}");
        };
        let url = data.expose().clone();
        steps.push(qr);
        (url, steps)
    }

    /// Links the account by QR: answers the device-name step, hands the QR URL to `scan` (the
    /// phone), then waits for `Done`. Returns the QR URL and every step seen.
    pub async fn link<F, Fut>(&mut self, scan: F) -> (String, Vec<LoginStep>)
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let (url, mut steps) = self.begin_login(None).await;
        scan(url.clone()).await;
        loop {
            let step = self.next_login().await;
            match &step.kind {
                LoginStepKind::Done => {
                    steps.push(step);
                    return (url, steps);
                }
                LoginStepKind::Failed { error } => panic!("login failed: {error:?}"),
                _ => steps.push(step),
            }
        }
    }

    /// Sends a 1:1 text to `to` (allocating the id like the core does) and returns its
    /// `sent_at` and the outcome.
    pub async fn transmit_text(&self, to: &UserId, text: &str) -> (i64, TransmitOutcome) {
        let req = text_request(to, text);
        let sent_at = req.sent_at;
        (sent_at, self.transmit(req).await)
    }

    /// One `Transmit` attempt of `req`.
    pub async fn transmit(&self, req: OutgoingRequest) -> TransmitOutcome {
        let (reply, outcome) = oneshot::channel();
        self.send(Command::Transmit {
            local: MessageId(1),
            req: Box::new(req),
            reply,
        })
        .await;
        tokio::time::timeout(WAIT, outcome)
            .await
            .expect("transmit answered in time")
            .expect("transmit reply sent")
    }

    /// Sends `Shutdown` and waits for the actor's exit and thread.
    pub async fn shutdown(mut self) -> Exit {
        let _ = self.commands.send(Command::Shutdown).await;
        self.join().await
    }

    /// Waits for the actor to exit by itself (an unlinked device, a logout) and for its thread.
    pub async fn wait_exit(mut self) -> Exit {
        self.join().await
    }

    /// Sends `command` (one that makes the actor exit: `Logout`), then [`Self::wait_exit`].
    pub async fn exit_with(mut self, command: Command) -> Exit {
        let _ = self.commands.send(command).await;
        self.join().await
    }

    /// Sends `Logout`, waits for the exit, and returns it with the `Logout` reports the actor
    /// sent before it (exactly one is expected; the other ephemeral events are dropped).
    pub async fn logout(mut self) -> (Exit, Vec<kourier_model::LogoutReport>) {
        let _ = self.commands.send(Command::Logout).await;
        let exit = self.join().await;
        let mut reports = Vec::new();
        while let Ok(event) = self.ephemeral.try_recv() {
            if let BackendEvent::Logout(report) = event {
                reports.push(report);
            }
        }
        (exit, reports)
    }

    async fn join(&mut self) -> Exit {
        let exited = self.exited.take().expect("joined once");
        let exit = tokio::time::timeout(WAIT, exited)
            .await
            .map(Exit::from_channel)
            .expect("the actor exits in time");
        if let Some(thread) = self.thread.take() {
            tokio::task::spawn_blocking(move || thread.join())
                .await
                .expect("join task")
                .expect("actor thread did not panic");
        }
        if let Some(pump) = self.pump.take() {
            pump.await.expect("the fake core saw no contract violation");
        }
        exit
    }
}

/// The fake core's envelope loop: validates every envelope against the contract, commits
/// durable ones to the sink before completing their ack, and queues ephemeral events.
async fn pump(
    mut events: mpsc::Receiver<Envelope>,
    sink: Sink,
    ephemeral: mpsc::UnboundedSender<BackendEvent>,
) {
    while let Some(envelope) = events.recv().await {
        if let Err(violation) = envelope.validate() {
            panic!("contract violation: {violation}");
        }
        match envelope.ack {
            None => {
                for event in envelope.events {
                    let _ = ephemeral.send(event);
                }
            }
            Some(ack) => {
                let history = envelope
                    .events
                    .iter()
                    .any(|event| matches!(event, BackendEvent::HistoryBatch { .. }));
                if history {
                    // A test's slow, stuck or refusing core (`Sink::set_behavior`).
                    while sink.behavior("stall").as_deref() == Some("1") {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    if let Some(ms) = sink.behavior("ack_delay_ms").and_then(|v| v.parse().ok()) {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                    }
                    let holds_in = |event: &BackendEvent, marker: &str| match event {
                        BackendEvent::HistoryBatch { messages, .. } => {
                            messages.iter().any(|m| match &m.body {
                                Body::Text(text) => {
                                    marker.split('|').any(|one| text.body.contains(one))
                                }
                                _ => false,
                            })
                        }
                        _ => false,
                    };
                    let holds = |marker: &str| envelope.events.iter().any(|e| holds_in(e, marker));
                    if let Some(marker) = sink.behavior("refuse")
                        && holds(&marker)
                    {
                        ack.complete(Err(PersistError {
                            retryable: false,
                            detail: "a test refuses this history row".to_owned(),
                        }));
                        continue;
                    }
                    if let Some(marker) = sink.behavior("stick")
                        && let Some(at) = envelope.events.iter().position(|e| holds_in(e, &marker))
                    {
                        // The core applies the jobs in order and stops at the first that
                        // fails: what stands before it is committed (and shipped again,
                        // as the envelope is answered as a whole), what stands behind it
                        // is never tried.
                        if at > 0 {
                            let _ = sink.commit(&envelope.events[..at]);
                        }
                        ack.complete(Err(PersistError {
                            retryable: true,
                            detail: "a test cannot store this history row".to_owned(),
                        }));
                        continue;
                    }
                }
                if sink.behavior("refuse_wide_chats").as_deref() == Some("1") {
                    let mut chats: Vec<&ConvKey> = Vec::new();
                    for event in &envelope.events {
                        if let BackendEvent::ConversationUpsert(patch) = event
                            && !chats.contains(&&patch.key)
                        {
                            chats.push(&patch.key);
                        }
                    }
                    if chats.len() >= 2 {
                        if let Some(ms) = sink
                            .behavior("refuse_wide_chats_delay_ms")
                            .and_then(|v| v.parse().ok())
                        {
                            tokio::time::sleep(Duration::from_millis(ms)).await;
                        }
                        let refused = sink
                            .behavior("refused_wide")
                            .and_then(|v| v.parse::<u32>().ok())
                            .unwrap_or(0);
                        sink.set_behavior("refused_wide", Some(&(refused + 1).to_string()));
                        ack.complete(Err(PersistError {
                            retryable: false,
                            detail: "a test refuses a row of several chats".to_owned(),
                        }));
                        continue;
                    }
                }
                let result = sink.commit(&envelope.events).map_err(|e| PersistError {
                    retryable: true,
                    detail: e.to_string(),
                });
                ack.complete(result);
            }
        }
    }
}

/// A 1:1 text request to `to`, with an id allocated like the core does (the timestamp now).
pub fn text_request(to: &UserId, text: &str) -> OutgoingRequest {
    text_request_in(ConvKey::Direct(to.clone()), text)
}

/// A text request in conversation `conv` (a 1:1 chat or a group), with an id allocated like the
/// core does (the timestamp now).
pub fn text_request_in(conv: ConvKey, text: &str) -> OutgoingRequest {
    let network = SignalNetwork::new();
    let now_ms = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(0);
    let id = network.allocate_message_id(&[], &conv, now_ms);
    OutgoingRequest {
        conv,
        local: id.local,
        sent_at: id.sent_at,
        kind: OutgoingKind::Message {
            body: Body::text(text),
            reply_to: None,
            expiry: None,
        },
        only: None,
        is_recipient_update: false,
    }
}

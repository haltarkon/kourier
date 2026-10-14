//! Test support, public so backend crates and the app's fixture mode can reuse it:
//!
//! - [`DirVault`]: an [`AccountVault`] with plain files. **Never for real accounts**: the
//!   account secrets lie unencrypted next to the data.
//! - [`dump`]: a canonical text form of `core.db` for "same final state" assertions (§10.4).
//! - [`IngestHarness`]: the real ingest on a `core.db` without the runtime, for property tests.

use std::io;
use std::path::{Path, PathBuf};

use kourier_backend_api::{BackendEvent, DbKey as BackendDbKey};
use kourier_db::rusqlite::Connection;
use kourier_model::{AccountId, AccountState, AccountUuid, MsgIdScope, NetworkKind, UserId};

use crate::error::Result;
use crate::facade::{ConversationListQuery, ConversationPage, TimelinePage, TimelineQuery};
use crate::ingest::{self, Changes};
use crate::vault::{AccountVault, VaultEntry, VaultError, VaultFuture};
use crate::{local, query, sql};

const WRAP_MAGIC: &str = "KOURTEST1";

/// A vault of plain files under `root`: `accounts/<uuid>/key.wrap` holds the network and the
/// `backend.db` key XOR the account secret, `secrets/<uuid>` holds the secret. Deleting the
/// secret file is the shred.
#[derive(Clone, Debug)]
pub struct DirVault {
    root: PathBuf,
    /// `false` while [`DirVault::hold_unwraps`] holds the unwrapping of account keys.
    unwrap_open: std::sync::Arc<tokio::sync::watch::Sender<bool>>,
    /// `false` while [`DirVault::hold_dir_removals`] holds the removal of account directories.
    remove_open: std::sync::Arc<tokio::sync::watch::Sender<bool>>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

fn random32() -> [u8; 32] {
    let mut out = [0u8; 32];
    out[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    out[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    out
}

fn remove_if_exists(p: &Path) -> io::Result<()> {
    match std::fs::remove_file(p) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

impl DirVault {
    /// A vault rooted at `root` (created on first use).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            unwrap_open: std::sync::Arc::new(tokio::sync::watch::channel(true).0),
            remove_open: std::sync::Arc::new(tokio::sync::watch::channel(true).0),
        }
    }

    /// Holds every [`AccountVault::unwrap`] (an account's actor cannot start without its key)
    /// until [`Self::release_unwraps`]: a test that needs the time between the core's start
    /// and its supervisors' first backend.
    pub fn hold_unwraps(&self) {
        self.unwrap_open.send_replace(false);
    }

    /// Lets the held unwraps go on.
    pub fn release_unwraps(&self) {
        self.unwrap_open.send_replace(true);
    }

    /// Holds [`AccountVault::remove_dir`] until [`Self::release_dir_removals`]: a test that
    /// needs the time a removal spends on a big account directory (link feedback L5).
    pub fn hold_dir_removals(&self) {
        self.remove_open.send_replace(false);
    }

    /// Lets the held directory removals go on.
    pub fn release_dir_removals(&self) {
        self.remove_open.send_replace(true);
    }

    fn dir(&self, uuid: AccountUuid) -> PathBuf {
        self.root.join("accounts").join(uuid.to_string())
    }

    fn wrap(&self, uuid: AccountUuid) -> PathBuf {
        self.dir(uuid).join("key.wrap")
    }

    fn secret(&self, uuid: AccountUuid) -> PathBuf {
        self.root.join("secrets").join(uuid.to_string())
    }

    fn create_sync(
        &self,
        uuid: AccountUuid,
        network: NetworkKind,
    ) -> Result<BackendDbKey, VaultError> {
        let dir = self.dir(uuid);
        std::fs::create_dir_all(&dir)?;
        std::fs::create_dir_all(self.root.join("secrets"))?;
        if self.wrap(uuid).exists() {
            return Err(VaultError::Exists);
        }
        let (secret, key) = (random32(), random32());
        let wrapped: Vec<u8> = key.iter().zip(secret.iter()).map(|(k, s)| k ^ s).collect();
        std::fs::write(self.secret(uuid), hex(&secret))?;
        std::fs::write(
            self.wrap(uuid),
            format!("{WRAP_MAGIC}\n{}\n{}\n", network.as_str(), hex(&wrapped)),
        )?;
        Ok(BackendDbKey::new(key))
    }

    fn read_wrap(&self, uuid: AccountUuid) -> Result<(NetworkKind, [u8; 32]), VaultError> {
        let text = match std::fs::read_to_string(self.wrap(uuid)) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(VaultError::Missing),
            Err(e) => return Err(e.into()),
        };
        let mut lines = text.lines();
        let (Some(WRAP_MAGIC), Some(net), Some(wrapped)) =
            (lines.next(), lines.next(), lines.next())
        else {
            return Err(VaultError::Other("malformed test key.wrap".into()));
        };
        let network = net
            .parse()
            .map_err(|_| VaultError::Other("unknown network in key.wrap".into()))?;
        let wrapped = unhex32(wrapped).ok_or_else(|| VaultError::Other("malformed key".into()))?;
        Ok((network, wrapped))
    }

    fn unwrap_sync(&self, uuid: AccountUuid) -> Result<BackendDbKey, VaultError> {
        let (_, wrapped) = self.read_wrap(uuid)?;
        let secret = match std::fs::read_to_string(self.secret(uuid)) {
            // A secret that is there and is not a key: the wrap does not open with it.
            Ok(s) => unhex32(&s).ok_or(VaultError::WrongKey)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(VaultError::SecretMissing),
            Err(e) => return Err(e.into()),
        };
        let mut key = [0u8; 32];
        for (i, k) in key.iter_mut().enumerate() {
            *k = wrapped[i] ^ secret[i];
        }
        Ok(BackendDbKey::new(key))
    }

    fn list_sync(&self) -> Result<Vec<VaultEntry>, VaultError> {
        let dir = self.root.join("accounts");
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for e in entries {
            let e = e?;
            let Some(uuid) = e
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<AccountUuid>().ok())
            else {
                continue;
            };
            match self.read_wrap(uuid) {
                Ok((network, _)) => out.push(VaultEntry { uuid, network }),
                Err(VaultError::Missing) => {}
                Err(e) => return Err(e),
            }
        }
        out.sort_by_key(|e| e.uuid);
        Ok(out)
    }

    fn wipe_sync(&self, uuid: AccountUuid) -> Result<(), VaultError> {
        remove_if_exists(&self.secret(uuid))?;
        let db = self.backend_db(uuid);
        for suffix in ["", "-wal", "-shm"] {
            let mut p = db.clone().into_os_string();
            p.push(suffix);
            remove_if_exists(Path::new(&p))?;
        }
        remove_if_exists(&self.wrap(uuid))?;
        Ok(())
    }

    fn remove_dir_sync(&self, uuid: AccountUuid) -> Result<(), VaultError> {
        remove_if_exists(&self.secret(uuid))?;
        match std::fs::remove_dir_all(self.dir(uuid)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, VaultError> + Send + 'static,
) -> VaultFuture<'static, T> {
    Box::pin(async move {
        tokio::task::spawn_blocking(f)
            .await
            .map_err(|_| VaultError::Other("vault task failed".into()))?
    })
}

impl AccountVault for DirVault {
    fn create(&self, uuid: AccountUuid, network: NetworkKind) -> VaultFuture<'_, BackendDbKey> {
        let v = self.clone();
        blocking(move || v.create_sync(uuid, network))
    }

    fn unwrap(&self, uuid: AccountUuid) -> VaultFuture<'_, BackendDbKey> {
        let v = self.clone();
        let mut open = self.unwrap_open.subscribe();
        Box::pin(async move {
            // (the sender lives as long as the vault: the wait cannot fail)
            let _ = open.wait_for(|open| *open).await;
            blocking(move || v.unwrap_sync(uuid)).await
        })
    }

    fn list(&self) -> VaultFuture<'_, Vec<VaultEntry>> {
        let v = self.clone();
        blocking(move || v.list_sync())
    }

    fn wipe_credentials(&self, uuid: AccountUuid) -> VaultFuture<'_, ()> {
        let v = self.clone();
        blocking(move || v.wipe_sync(uuid))
    }

    fn shred(&self, uuid: AccountUuid) -> VaultFuture<'_, ()> {
        let v = self.clone();
        blocking(move || Ok(remove_if_exists(&v.secret(uuid))?))
    }

    fn remove_dir(&self, uuid: AccountUuid) -> VaultFuture<'_, ()> {
        let v = self.clone();
        let mut open = self.remove_open.subscribe();
        Box::pin(async move {
            // (the sender lives as long as the vault: the wait cannot fail)
            let _ = open.wait_for(|open| *open).await;
            blocking(move || v.remove_dir_sync(uuid)).await
        })
    }

    fn backend_db(&self, uuid: AccountUuid) -> PathBuf {
        self.dir(uuid).join("backend.db")
    }

    fn blob_dir(&self, uuid: AccountUuid) -> PathBuf {
        self.dir(uuid).join("blobs")
    }
}

/// What [`dump`] includes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DumpMode {
    /// Everything except rowids, uuids and bookkeeping timestamps.
    Exact,
    /// Also without the arrival-order-dependent columns: live `sort_key`s and what derives from
    /// them (`last_activity`, `read_up_to`), §3.9.
    OrderInsensitive,
}

/// A canonical, sorted text form of the whole `core.db` content, keyed by natural keys (network
/// ids), not rowids, so two databases built by different event orders compare equal when their
/// state is the same.
pub fn dump(conn: &Connection, mode: DumpMode) -> Result<String> {
    let exact = mode == DumpMode::Exact;
    let acc = "(SELECT id, 'a' || ROW_NUMBER() OVER (ORDER BY id) AS l FROM accounts)";
    let mkey = "c.conv_key || '/' || m.net_author || '/' || m.net_local";
    let order_cols_conv = if exact {
        "|| '|' || quote(c.last_activity) || '|' || quote(c.read_up_to)"
    } else {
        ""
    };
    // History sort keys do not depend on arrival order (§3.9); live ones do.
    let order_cols_msg = if exact {
        "|| '|' || quote(m.sort_key)"
    } else {
        "|| '|' || CASE WHEN m.history = 1 THEN quote(m.sort_key) ELSE '-' END"
    };
    let queries = [
        format!(
            "SELECT 'account|' || a.l || '|' || x.network || '|' || x.state || '|' ||
               quote(x.state_reason) || '|' || x.own_ids || '|' || quote(x.own_device) || '|' ||
               quote(x.display_name) || '|' || x.settings || '|' || x.local_prefs || '|' || quote(x.caps)
               || '|' || quote(x.number)
             FROM accounts x JOIN {acc} a ON a.id = x.id"
        ),
        format!(
            "SELECT 'contact|' || a.l || '|' || x.user_id || '|' || quote(x.profile_name) || '|' ||
               quote(x.system_name) || '|' || quote(x.nickname) || '|' || quote(f.path) || '|' ||
               x.trust || '|' || quote(x.fingerprint) || '|' || x.blocked || '|' || x.hidden || '|' ||
               quote(x.unregistered_at) || '|' || x.identifiers || '|' || quote(x.meta)
             FROM contacts x JOIN {acc} a ON a.id = x.account_id LEFT JOIN files f ON f.id = x.avatar_file"
        ),
        format!(
            "SELECT 'alias|' || a.l || '|' || x.alias || '|' || x.user_id
             FROM identity_alias x JOIN {acc} a ON a.id = x.account_id"
        ),
        format!(
            "SELECT 'conv|' || a.l || '|' || c.conv_key || '|' || c.kind || '|' || c.is_self || '|' ||
               quote(c.title) || '|' || quote(f.path) || '|' || quote(c.muted_until) || '|' ||
               c.archived || '|' || quote(c.pinned_order) || '|' || c.blocked || '|' ||
               c.request_pending || '|' || c.marked_unread || '|' || quote(p.conv_key) || '|' ||
               quote(c.timer_secs) || '|' || quote(c.timer_version) || '|' || quote(c.features) ||
               '|' || quote(c.local_muted_until) ||
               '|' || quote(c.draft) || '|' || c.unread || '|' || c.unread_hint || '|' || c.mentions ||
               '|' || quote(c.read_sent_at) || '|' || quote(c.meta) || '|' || c.hidden {order_cols_conv}
             FROM conversations c JOIN {acc} a ON a.id = c.account_id
             LEFT JOIN files f ON f.id = c.avatar_file LEFT JOIN conversations p ON p.id = c.parent_id"
        ),
        "SELECT 'member|' || c.conv_key || '|' || x.user_id || '|' || x.role
         FROM members x JOIN conversations c ON c.id = x.conversation_id"
            .to_owned(),
        format!(
            "SELECT 'msg|' || a.l || '|' || {mkey} || '|' || m.net_scope || '|' || m.origin || '|' ||
               quote(m.device) || '|' ||
               quote((SELECT lc.conv_key || '/' || l.net_author || '/' || l.net_local FROM messages l
                      JOIN conversations lc ON lc.id = l.conversation_id WHERE l.id = m.linked_message_id))
               || '|' || m.sent_at || '|' || quote(m.server_at) || '|' || m.received_at || '|' ||
               quote(m.arrival) || '|' || m.body_kind || '|' || m.body || '|' || quote(m.preview) || '|' ||
               quote(m.reply_to) || '|' || m.revision || '|' || quote(m.edited_at) || '|' ||
               quote(m.deleted) || '|' || quote(m.deleted_by) || '|' || quote(m.out_floor) || '|' ||
               quote(m.out_state) || '|' || quote(m.read_state) || '|' || quote(m.expiry_kind) || '|' ||
               quote(m.expiry_timer_ms) || '|' || quote(m.expiry_started_at) || '|' ||
               quote(m.expires_at) || '|' || m.view_once || '|' || m.history || '|' || m.silent || '|' ||
               m.mentions_me || '|' || quote(m.meta) || '|' || quote(m.raw) || '|' ||
               m.expiry_hidden || '|' || quote(m.call_deadline) {order_cols_msg}
             FROM messages m JOIN conversations c ON c.id = m.conversation_id
             JOIN {acc} a ON a.id = m.account_id"
        ),
        format!(
            "SELECT 'rev|' || {mkey} || '|' || r.revision || '|' || r.net_author || '|' || r.net_local ||
               '|' || quote(r.body) || '|' || quote(r.edited_at)
             FROM message_revisions r JOIN messages m ON m.id = r.message_id
             JOIN conversations c ON c.id = m.conversation_id"
        ),
        format!(
            "SELECT 'react|' || {mkey} || '|' || r.user_id || '|' || r.emoji || '|' || r.at || '|' || r.removed
             FROM reactions r JOIN messages m ON m.id = r.message_id
             JOIN conversations c ON c.id = m.conversation_id"
        ),
        format!(
            "SELECT 'receipt|' || {mkey} || '|' || r.user_id || '|' || r.kind || '|' || r.at
             FROM receipts r JOIN messages m ON m.id = r.message_id
             JOIN conversations c ON c.id = m.conversation_id"
        ),
        format!(
            "SELECT 'send|' || {mkey} || '|' || s.user_id || '|' || s.state || '|' || quote(s.error)
             FROM send_state s JOIN messages m ON m.id = s.message_id
             JOIN conversations c ON c.id = m.conversation_id"
        ),
        format!(
            "SELECT 'att|' || {mkey} || '|' || x.idx || '|' || x.kind || '|' || quote(x.mime) || '|' ||
               quote(x.size) || '|' || quote(x.file_name) || '|' || quote(x.width) || '|' ||
               quote(x.height) || '|' || quote(x.duration_ms) || '|' || quote(x.blurhash) || '|' ||
               quote(x.waveform) || '|' || quote(x.caption) || '|' || x.state || '|' || quote(x.remote)
               || '|' || (x.file_id IS NOT NULL) || '|' || (x.thumb_file_id IS NOT NULL)
               || '|' || x.long_text
             FROM attachments x JOIN messages m ON m.id = x.message_id
             JOIN conversations c ON c.id = m.conversation_id"
        ),
        format!(
            "SELECT 'pending|' || a.l || '|' || quote(x.conv_key) || '|' || quote(x.target_author) || '|' ||
               x.target_local || '|' || x.kind || '|' || x.payload
             FROM pending_modifiers x JOIN {acc} a ON a.id = x.account_id"
        ),
        format!(
            "SELECT 'cmd|' || a.l || '|' || x.command FROM pending_commands x JOIN {acc} a ON a.id = x.account_id"
        ),
        format!(
            "SELECT 'outbox|' || {mkey} || '|' || x.kind || '|' || x.state || '|' || x.request
             FROM outbox x JOIN messages m ON m.id = x.message_id
             JOIN conversations c ON c.id = m.conversation_id"
        ),
        format!(
            "SELECT 'expired|' || a.l || '|' || x.net_scope || '|' || x.conv_key || '|' ||
               x.net_author || '|' || x.net_local || '|' || x.expired_at
             FROM expired_messages x JOIN {acc} a ON a.id = x.account_id"
        ),
        format!(
            // Attachment and thumbnail paths are random blob ids; avatar paths come from the backend.
            "SELECT 'file|' || a.l || '|' || CASE WHEN x.purpose = 'avatar' THEN x.path ELSE '<blob>' END
               || '|' || x.purpose || '|' || quote(x.size) || '|' || quote(x.mime)
             FROM files x JOIN {acc} a ON a.id = x.account_id"
        ),
    ];
    let mut lines = Vec::new();
    for q in &queries {
        let mut stmt = conn.prepare(q)?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for r in rows {
            lines.push(r?);
        }
    }
    lines.sort();
    let mut out = lines.join("\n");
    out.push('\n');
    Ok(out)
}

/// What a write of the [`IngestHarness`] wants announced (§8.7), before the focus rule.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Announced {
    /// The messages to notify, in order.
    pub notify: Vec<kourier_model::MessageId>,
    /// How many rows count towards the catch-up summary instead (missed calls that wait for
    /// it included).
    pub catch_up: usize,
    /// The rows that changed.
    pub changed: Vec<kourier_model::MessageId>,
}

impl Announced {
    fn of(ch: &Changes) -> Self {
        Self {
            notify: ch.notify.iter().map(|n| n.message).collect(),
            catch_up: ch.catch_up.len() + ch.held_calls.len(),
            changed: ch.messages.values().flatten().copied().collect(),
        }
    }
}

/// What a write of the [`IngestHarness`] started for the backend: the durable commands it
/// stored and the downloads it requested.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Started {
    /// The durable commands, in order.
    pub commands: Vec<kourier_backend_api::DurableCommand>,
    /// The downloads: the attachment, the request id `AttachmentDone` names, and the names
    /// handed to the backend.
    pub downloads: Vec<(
        kourier_model::AttachmentId,
        kourier_model::DownloadId,
        Vec<String>,
    )>,
}

impl Started {
    fn of(ch: Changes) -> Self {
        Self {
            commands: ch.commands.into_iter().map(|(_, c)| c).collect(),
            downloads: ch
                .downloads
                .into_iter()
                .map(|d| (d.attachment, d.request, d.net_ids))
                .collect(),
        }
    }
}

/// The real ingest on a `core.db` with one `Active` account, without the runtime (property
/// tests of §4.9 and §10.1).
pub struct IngestHarness {
    conn: Connection,
    account: AccountId,
    scope: MsgIdScope,
    now: i64,
}

impl std::fmt::Debug for IngestHarness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IngestHarness")
            .field("account", &self.account)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl IngestHarness {
    /// Creates (or opens) `path`, migrates it and adds an `Active` account of `network` with
    /// `own_ids` (the state right after `AccountReady`).
    pub fn open(
        path: &Path,
        key: &kourier_db::DbKey,
        network: NetworkKind,
        scope: MsgIdScope,
        own_ids: &[UserId],
    ) -> Result<Self> {
        let mut conn = kourier_db::open(path, key)?;
        kourier_db::migrate(&mut conn, &crate::store::MIGRATIONS)?;
        let account = match local::accounts(&conn)?.first() {
            Some(a) => a.id,
            None => {
                let id = local::insert_account(
                    &conn,
                    AccountUuid::new_random(),
                    network,
                    &AccountState::Active,
                    0,
                )?;
                conn.execute(
                    "UPDATE accounts SET own_ids = ?2 WHERE id = ?1",
                    (id.0, sql::to_json(own_ids)?),
                )?;
                id
            }
        };
        Ok(Self {
            conn,
            account,
            scope,
            now: 1_700_000_000_000,
        })
    }

    /// The account.
    pub fn account(&self) -> AccountId {
        self.account
    }

    /// The harness clock (ms).
    pub fn now(&self) -> i64 {
        self.now
    }

    /// Sets the core clock (pending-modifier expiry, timers, the memory of expired messages).
    pub fn set_now(&mut self, ms: i64) {
        self.now = ms;
    }

    /// Applies one envelope's events as the supervisor does (live events in one transaction,
    /// each history batch in its own).
    pub fn apply(&mut self, events: Vec<BackendEvent>) -> Result<()> {
        let mut segment = Vec::new();
        for e in events {
            if matches!(e, BackendEvent::HistoryBatch { .. }) {
                if !segment.is_empty() {
                    self.apply_one(std::mem::take(&mut segment))?;
                }
                self.apply_one(vec![e])?;
            } else {
                segment.push(e);
            }
        }
        if !segment.is_empty() {
            self.apply_one(segment)?;
        }
        Ok(())
    }

    fn apply_one(&mut self, events: Vec<BackendEvent>) -> Result<Changes> {
        ingest::apply_events(&mut self.conn, self.account, self.scope, self.now, events)
    }

    /// Applies one batch of live events in one transaction and returns what it wants
    /// announced.
    pub fn apply_announcing(&mut self, events: Vec<BackendEvent>) -> Result<Announced> {
        Ok(Announced::of(&self.apply_one(events)?))
    }

    /// Runs the deadline sweep of ringing call lines once at the harness clock; returns what
    /// it changed and wants announced.
    pub fn settle_calls(&mut self) -> Result<Announced> {
        let mut out = Announced::default();
        // (Every account counts as connected and caught up: only a sweep that comes late
        // holds a line back.)
        for (_, ch) in crate::calls::sweep(&mut self.conn, self.now, &|_| true)? {
            let one = Announced::of(&ch);
            out.notify.extend(one.notify);
            out.catch_up += one.catch_up;
            out.changed.extend(one.changed);
        }
        Ok(out)
    }

    /// The nearest deadline of a ringing call line: what the deadline task sleeps towards.
    pub fn next_call_deadline(&self) -> Result<Option<i64>> {
        crate::calls::next_deadline(&self.conn)
    }

    /// `UiCommand::MarkRead` as the core runs it, in its one transaction, at the harness
    /// clock; returns the durable commands it stored for the backend (`None`: nothing was
    /// unread).
    pub fn mark_read(
        &mut self,
        conv: kourier_model::ConversationId,
    ) -> Result<Option<Vec<kourier_backend_api::DurableCommand>>> {
        Ok(local::mark_read(&mut self.conn, conv, self.now)?
            .map(|m| m.commands.into_iter().map(|(_, c)| c).collect()))
    }

    /// Runs the disappearing-message sweeper once at the harness clock (every account is
    /// swept: every clock counts as confirmed); returns how many conversations lost
    /// messages.
    pub fn sweep(&mut self) -> Result<usize> {
        let hold = crate::expiry::Hold::default();
        let (done, _more) = crate::expiry::sweep(&mut self.conn, self.now, &hold)?;
        Ok(done.iter().map(|(_, ch)| ch.timelines.len()).sum())
    }

    /// Runs the sweeper once at the harness clock while no backend has confirmed this
    /// account's clock (§9.6), under `policy`: it hides, keeps or erases what is due.
    /// Returns how many conversations changed for the UI.
    pub fn sweep_unconfirmed(&mut self, policy: crate::facade::ExpiryPolicy) -> Result<usize> {
        let hold = crate::expiry::Hold {
            policy,
            unconfirmed: vec![self.account],
        };
        let (done, _more) = crate::expiry::sweep(&mut self.conn, self.now, &hold)?;
        Ok(done.iter().map(|(_, ch)| ch.timelines.len()).sum())
    }

    /// `UiCommand::SetExpiryPolicy` as the core runs it, in its one transaction, with this
    /// account's clock unconfirmed or not (what it erases is left to the next sweep).
    /// Returns how many conversations changed for the UI.
    pub fn set_expiry_policy(
        &mut self,
        policy: crate::facade::ExpiryPolicy,
        unconfirmed: bool,
    ) -> Result<usize> {
        let held = if unconfirmed {
            vec![self.account]
        } else {
            Vec::new()
        };
        let done = crate::expiry::set_policy(&mut self.conn, policy, self.now, held)?;
        Ok(done.iter().map(|(_, ch)| ch.timelines.len()).sum())
    }

    /// The stored expiry option (what a start reads before its first sweep).
    pub fn expiry_policy(&self) -> Result<crate::facade::ExpiryPolicy> {
        crate::expiry::stored_policy(&self.conn)
    }

    /// [`dump`] of the database.
    pub fn dump(&self, mode: DumpMode) -> Result<String> {
        dump(&self.conn, mode)
    }

    /// The connection (for ad-hoc assertions).
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// The connection, for core-local writes in the crate's own tests.
    #[cfg(test)]
    pub(crate) fn connection_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// A conversation list page, as the facade answers it.
    pub fn conversations(&self, q: &ConversationListQuery) -> Result<ConversationPage> {
        query::conversations(&self.conn, q)
    }

    /// The accounts, as the facade answers `Query::Accounts` (no actor runs: every account
    /// shows the connection state of one that is not running).
    pub fn accounts(&self) -> Result<Vec<crate::facade::AccountRow>> {
        query::accounts(&self.conn, &std::collections::HashMap::new())
    }

    /// The versions of a message, as the facade answers `Query::Revisions`.
    pub fn revisions(
        &self,
        message: kourier_model::MessageId,
    ) -> Result<Option<crate::facade::MessageRevisions>> {
        query::revisions(&self.conn, message)
    }

    /// Applies one batch of live events in one transaction and returns the blob files (relative
    /// to the account's blob directory) the core unlinks after that commit.
    pub fn apply_unlinking(
        &mut self,
        events: Vec<BackendEvent>,
    ) -> Result<Vec<std::path::PathBuf>> {
        Ok(self.apply_one(events)?.unlink)
    }

    /// A timeline page, as the facade answers it (action flags at the harness clock).
    pub fn timeline(&self, q: &TimelineQuery) -> Result<TimelinePage> {
        query::timeline(&self.conn, q, self.now)
    }

    /// One conversation, as the facade answers `Query::Conversation` (a hidden one too).
    pub fn conversation(
        &self,
        conv: kourier_model::ConversationId,
    ) -> Result<Option<crate::facade::ConversationDetail>> {
        query::conversation(&self.conn, conv)
    }

    /// `UiCommand::ClearConversation` (with `remove`: `DeleteConversation`) as the core runs
    /// it, in its one transaction; returns the durable commands it stored for the backend.
    pub fn clear_conversation(
        &mut self,
        conv: kourier_model::ConversationId,
        remove: bool,
    ) -> Result<Vec<kourier_backend_api::DurableCommand>> {
        let done = ingest::clear_conversation(&mut self.conn, self.scope, conv, remove, self.now)?;
        Ok(done.commands.into_iter().map(|(_, c)| c).collect())
    }

    /// `UiCommand::DeleteAttachment` as the core runs it, in its one transaction; returns the
    /// durable commands it stored for the backend.
    pub fn delete_attachment(
        &mut self,
        attachment: kourier_model::AttachmentId,
    ) -> Result<Vec<kourier_backend_api::DurableCommand>> {
        let done = ingest::delete_attachment(&mut self.conn, self.scope, attachment, self.now)?;
        Ok(done.commands.into_iter().map(|(_, c)| c).collect())
    }

    /// `UiCommand::DownloadAttachment` as the core runs it, in its one transaction, at the
    /// harness clock: a download, or a request to the account's other device for a file the
    /// network lost.
    pub fn download_attachment(
        &mut self,
        attachment: kourier_model::AttachmentId,
    ) -> Result<Started> {
        let tx = self
            .conn
            .transaction_with_behavior(kourier_db::rusqlite::TransactionBehavior::Immediate)?;
        let mut apply = ingest::Apply::begin(&tx, self.account, self.scope, self.now)?;
        apply.user_download(attachment)?;
        apply.finish()?;
        let ch = std::mem::take(&mut apply.ch);
        drop(apply);
        tx.commit()?;
        Ok(Started::of(ch))
    }

    /// Applies one batch of live events in one transaction and returns what it started for
    /// the backend.
    pub fn apply_starting(&mut self, events: Vec<BackendEvent>) -> Result<Started> {
        Ok(Started::of(self.apply_one(events)?))
    }

    /// Runs the deadline sweep of the waits for the other device's answer once at the
    /// harness clock; returns how many attachments it changed.
    pub fn settle_restores(&mut self) -> Result<usize> {
        Ok(crate::restore::sweep(&mut self.conn, self.now)?
            .iter()
            .map(|(_, ch)| ch.media.len())
            .sum())
    }

    /// [`Self::settle_restores`] while the waits of the account are held (`hold`: its actor
    /// has only just started, [`crate::restore::START_GRACE_MS`]) or judged; returns how many
    /// attachments it changed and whether it left a due wait alone.
    pub fn settle_restores_held(&mut self, hold: bool) -> Result<(usize, bool)> {
        let (done, held) = crate::restore::sweep_where(&mut self.conn, self.now, &|_| !hold)?;
        Ok((done.iter().map(|(_, ch)| ch.media.len()).sum(), held))
    }

    /// The nearest deadline of a wait for the other device's answer.
    pub fn next_restore_deadline(&self) -> Result<Option<i64>> {
        crate::restore::next_deadline(&self.conn)
    }

    /// The account is seen logged out, as the supervisor records it (the unlink bookkeeping);
    /// returns how many attachments it changed.
    pub fn log_out(&mut self) -> Result<usize> {
        let ended = crate::local::logged_out(&mut self.conn, self.account, "test", self.now)?;
        Ok(ended.media.len())
    }

    /// How many stored message bodies this thread has read and parsed so far (a probe: a loop
    /// over a message's attachments reads its body once).
    pub fn body_reads() -> u64 {
        ingest::body_reads()
    }

    /// How many parked receipts this thread has decoded so far to look at them again after
    /// identity merges (a probe: a transaction of many merges looks at each once).
    pub fn receipt_retry_decodes() -> u64 {
        ingest::receipt_retry_decodes()
    }

    /// What an actor start resends of the downloads an earlier actor left in flight:
    /// `(attachment, names handed to the backend)`.
    pub fn resume_downloads(&mut self) -> Result<Vec<(kourier_model::AttachmentId, Vec<String>)>> {
        let up_to = ingest::newest_download(&self.conn, self.account)?;
        let (jobs, _) =
            ingest::resume_downloads(&mut self.conn, self.account, self.scope, self.now, up_to)?;
        Ok(jobs
            .into_iter()
            .map(|j| (j.attachment, j.net_ids))
            .collect())
    }

    /// The start-up repair of attachments that say they are being asked for although no
    /// request is open; returns how many attachments it changed.
    pub fn end_orphan_asks(&mut self) -> Result<usize> {
        Ok(crate::restore::end_orphan_asks(&mut self.conn, self.now)?
            .iter()
            .map(|(_, ch)| ch.media.len())
            .sum())
    }
}

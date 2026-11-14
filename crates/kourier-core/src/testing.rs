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
    /// The accounts whose unwrapping [`DirVault::hold_unwraps_of`] holds.
    unwrap_held: std::sync::Arc<tokio::sync::watch::Sender<std::collections::HashSet<AccountUuid>>>,
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
            unwrap_held: std::sync::Arc::new(
                tokio::sync::watch::channel(std::collections::HashSet::new()).0,
            ),
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

    /// Holds the [`AccountVault::unwrap`] of ONE account until [`Self::release_unwraps_of`]: its
    /// supervisor cannot get as far as its message loop (a test that needs it busy while the
    /// others run).
    pub fn hold_unwraps_of(&self, uuid: AccountUuid) {
        self.unwrap_held.send_modify(|held| {
            held.insert(uuid);
        });
    }

    /// Lets the held unwrapping of `uuid` go on.
    pub fn release_unwraps_of(&self, uuid: AccountUuid) {
        self.unwrap_held.send_modify(|held| {
            held.remove(&uuid);
        });
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
        let mut held = self.unwrap_held.subscribe();
        Box::pin(async move {
            // (the senders live as long as the vault: the waits cannot fail)
            let _ = open.wait_for(|open| *open).await;
            let _ = held.wait_for(|held| !held.contains(&uuid)).await;
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
               quote(x.unregistered_at) || '|' || x.identifiers || '|' || quote(x.meta) || '|' ||
               quote(x.about) || '|' || quote(x.about_emoji) || '|' || quote(x.number_shown)
             FROM contacts x JOIN {acc} a ON a.id = x.account_id LEFT JOIN files f ON f.id = x.avatar_file"
        ),
        format!(
            "SELECT 'alias|' || a.l || '|' || x.alias || '|' || x.user_id
             FROM identity_alias x JOIN {acc} a ON a.id = x.account_id"
        ),
        format!(
            "SELECT 'conv|' || a.l || '|' || c.conv_key || '|' || c.kind || '|' || c.is_self || '|' ||
               quote(c.title) || '|' || quote(c.description) || '|' || quote(f.path) || '|' || quote(c.muted_until) || '|' ||
               c.archived || '|' || quote(c.pinned_order) || '|' || c.blocked || '|' ||
               c.request_pending || '|' || c.marked_unread || '|' || quote(p.conv_key) || '|' ||
               quote(c.timer_secs) || '|' || quote(c.timer_version) || '|' || quote(c.features) ||
               '|' || quote(c.local_muted_until) ||
               '|' || quote(c.draft) || '|' || c.unread || '|' || c.unread_hint || '|' || c.mentions ||
               '|' || quote(c.read_sent_at) || '|' || quote(c.meta) || '|' || c.hidden ||
               '|' || c.protected || '|' || quote(c.read_seq) || '|' || quote(c.peer_read_seq) ||
               '|' || quote(c.unread_server) || '|' || quote(c.mentions_server) {order_cols_conv}
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
               m.expiry_hidden || '|' || quote(m.call_deadline) || '|' || quote(m.net_seq) {order_cols_msg}
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
            // (No foreign key names the account, so a row whose account is wrong must still show.)
            "SELECT 'alias|' || {mkey} || '|' || quote(a.l) || '|' || x.net_local
             FROM message_aliases x JOIN messages m ON m.id = x.message_id
             JOIN conversations c ON c.id = m.conversation_id
             LEFT JOIN {acc} a ON a.id = x.account_id"
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
            "SELECT 'cut|' || a.l || '|' || x.conv_key || '|' || x.before_ms
             FROM conversation_cuts x JOIN {acc} a ON a.id = x.account_id"
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

/// The statements the core runs for every write and every open chat, and those of the read
/// positions, as `(name, text)`, the text being the one the core runs: what the plan test of a new
/// index holds them against (`core.db` has no planner statistics, so an index that ties with a hot
/// statement's takes its plan).
pub fn hot_statements() -> Vec<(&'static str, String)> {
    vec![
        ("timeline page", query::timeline_older_sql("<")),
        ("timeline page, inclusive", query::timeline_older_sql("<=")),
        ("by_key, account-wide", ingest::BY_KEY_ACCOUNT.to_owned()),
        (
            "by_key, per conversation",
            ingest::BY_KEY_CONVERSATION.to_owned(),
        ),
        ("by_key, per sender", ingest::BY_KEY_PER_SENDER.to_owned()),
        ("by_key, an earlier name", ingest::BY_ALIAS.to_owned()),
        (
            "resolve_target, with an author",
            ingest::RESOLVE_BY_AUTHOR.to_owned(),
        ),
        ("resolve_target, by id", ingest::RESOLVE_BY_LOCAL.to_owned()),
        (
            "messages with an id",
            ingest::MESSAGES_WITH_LOCAL.to_owned(),
        ),
        ("the original of a quote", query::QUOTE_ORIGINAL.to_owned()),
        (
            "the original of a quote, hidden too",
            query::QUOTE_ORIGINAL_ANY.to_owned(),
        ),
        ("recount", ingest::RECOUNT.to_owned()),
        ("the read watermark", ingest::READ_UP_TO.to_owned()),
        ("the read position", ingest::READ_BY_POSITION.to_owned()),
        (
            "the newest row at a read position",
            ingest::NEWEST_AT_POSITION.to_owned(),
        ),
        (
            "what a read here reaches",
            local::NEWEST_READ_POSITION.to_owned(),
        ),
        (
            "what the other side's marker moved",
            ingest::PEER_READ_RANGE.to_owned(),
        ),
    ]
}

/// What a write of the [`IngestHarness`] wants announced (§8.7), before the focus rule.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Announced {
    /// The messages to notify, in order (a muted chat's candidates are in [`Self::muted`]
    /// instead: the runtime drops them while no filter watches the muted chats).
    pub notify: Vec<kourier_model::MessageId>,
    /// How many rows count towards the catch-up summary instead (missed calls that wait for
    /// it included).
    pub catch_up: usize,
    /// The candidates of muted chats (`Notification::muted`): announced only when a filter
    /// with "Also in muted chats" may want them.
    pub muted: Vec<kourier_model::MessageId>,
    /// The caught-up messages by id, in order (what the summary carries while a filter has
    /// Notify).
    pub catch_up_ids: Vec<(kourier_model::ConversationId, kourier_model::MessageId)>,
    /// The rows that changed.
    pub changed: Vec<kourier_model::MessageId>,
    /// The conversation rows that changed (`UiEvent::ConversationsChanged`).
    pub conversations: Vec<kourier_model::ConversationId>,
}

impl Announced {
    fn of(ch: &Changes) -> Self {
        Self {
            // (As the runtime sees it with no filter watching the muted chats: a muted chat's
            // candidate is listed under `muted` and dropped from `notify`, held lines too.)
            notify: ch
                .notify
                .iter()
                .filter(|n| !n.muted)
                .map(|n| n.message)
                .collect(),
            catch_up: ch.catch_up.len() + ch.held_calls.iter().filter(|c| !c.muted).count(),
            muted: ch
                .notify
                .iter()
                .filter(|n| n.muted)
                .map(|n| n.message)
                .collect(),
            catch_up_ids: ch.catch_up_ids.clone(),
            changed: ch.messages.values().flatten().copied().collect(),
            conversations: ch.conversations.iter().copied().collect(),
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
    /// search.db (its writer) and a read-only core.db, as the search task holds them (G1).
    search: crate::search::index::Files,
    search_paths: crate::search::file::Paths,
    /// Why the harness's open built search.db again, if it did.
    search_judged: Option<crate::search::file::Rebuild>,
    account: AccountId,
    scope: MsgIdScope,
    history_read: kourier_model::HistoryReadState,
    server: kourier_model::ServerRules,
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
        // search.db is made now, with the core.db the harness starts from (as the search task
        // makes it at the first `UiReady`): the rows written later go through the log.
        let search_paths = crate::search::file::Paths {
            core: path.to_owned(),
            core_key: key.clone(),
            search: crate::search::file::beside(path),
            search_key: key.clone(),
        };
        let (search, search_judged) = crate::search::index::Files::open(&search_paths)?;
        Ok(Self {
            conn,
            search,
            search_paths,
            search_judged,
            account,
            scope,
            history_read: kourier_model::HistoryReadState::Known,
            server: kourier_model::ServerRules::default(),
            now: 1_700_000_000_000,
        })
    }

    /// States what the network says of the `read` flag of its history rows (W4 fix F1; a real
    /// network's descriptor does it; the harness starts at `Known`).
    #[must_use]
    pub fn with_history_read(mut self, history_read: kourier_model::HistoryReadState) -> Self {
        self.history_read = history_read;
        self
    }

    /// States what the network's server owns (KT-2; a real network's descriptor does it; the
    /// harness starts with everything off).
    #[must_use]
    pub fn with_server(mut self, server: kourier_model::ServerRules) -> Self {
        self.server = server;
        self
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
        ingest::apply_events(
            &mut self.conn,
            self.account,
            self.scope,
            self.history_read,
            self.server,
            self.now,
            events,
        )
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
            out.muted.extend(one.muted);
            out.catch_up += one.catch_up;
            out.changed.extend(one.changed);
            out.conversations.extend(one.conversations);
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
        Ok(
            local::mark_read(&mut self.conn, conv, self.now, self.now, false)?
                .map(|m| m.commands.into_iter().map(|(_, c)| c).collect()),
        )
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

    /// `UiCommand::SetQuietNotices` as the core runs it, in its one transaction: stores the
    /// choice and gives every conversation the sort time its newest listed row says. Returns
    /// the conversations whose sort time changed.
    pub fn set_quiet_notices(&mut self, shown: bool) -> Result<Vec<kourier_model::ConversationId>> {
        crate::notices::set(&mut self.conn, shown)
    }

    /// Whether the user asked to see quiet notices in the list (`Query::QuietNotices`).
    pub fn quiet_notices(&self) -> Result<bool> {
        crate::notices::shown(&self.conn)
    }

    /// `ingest::recount` of one conversation (its counters and its sort time), for the
    /// measurement of what a write pays at the top of a long chat.
    pub fn recount(&self, conv: kourier_model::ConversationId) -> Result<()> {
        ingest::recount(&self.conn, conv)
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
        query::accounts(
            &self.conn,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        )
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
        query::timeline(&self.conn, q, self.now, true, &Default::default(), None)
    }

    /// The catch-up summary's read, as the facade answers `Query::CaughtUpMessages`.
    pub fn caught_up_messages(
        &self,
        ids: &[(kourier_model::ConversationId, kourier_model::MessageId)],
    ) -> Result<Vec<(kourier_model::ConversationId, crate::facade::MessageRow)>> {
        query::caught_up_messages(&self.conn, ids, self.now)
    }

    /// The messages `ids` of `conv`, as the facade answers `Query::MessagesByIds` (action flags
    /// at the harness clock).
    pub fn messages_by_ids(
        &self,
        conv: kourier_model::ConversationId,
        ids: &[kourier_model::MessageId],
    ) -> Result<TimelinePage> {
        query::messages_by_ids(
            &self.conn,
            conv,
            ids,
            self.now,
            true,
            &Default::default(),
            None,
        )
    }

    /// One conversation, as the facade answers `Query::Conversation` (a hidden one too).
    pub fn conversation(
        &self,
        conv: kourier_model::ConversationId,
    ) -> Result<Option<crate::facade::ConversationDetail>> {
        query::conversation(&self.conn, conv)
    }

    /// One person for their page, as the facade answers `Query::Person`.
    pub fn person(&self, user: &UserId) -> Result<Option<crate::facade::PersonDetail>> {
        query::person(&self.conn, self.account, user)
    }

    /// Who a chat or an account stands for, as the facade resolves `Query::PersonOf`.
    pub fn person_of(
        &self,
        target: crate::facade::PersonRef,
    ) -> Result<Option<(AccountId, UserId)>> {
        query::person_of(&self.conn, target)
    }

    /// One group for its page, as the facade answers `Query::Group`.
    pub fn group(
        &self,
        conv: kourier_model::ConversationId,
    ) -> Result<Option<crate::facade::GroupDetail>> {
        query::group(&self.conn, conv)
    }

    /// A page of a chat's gallery, as the facade answers `Query::ChatMedia`.
    pub fn chat_media(
        &self,
        q: &crate::facade::ChatMediaQuery,
    ) -> Result<crate::facade::ChatMediaPage> {
        query::chat_media(&self.conn, q)
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

    /// Flushes the search log into search.db until nothing is left above `applied`, and prunes
    /// the log, as the core's search task does (G1b); returns what it did.
    pub fn search_flush(&mut self) -> Result<search::Flushed> {
        let done = crate::search::index::flush_all(&mut self.search)?;
        crate::search::log::prune_all(&self.conn, done.applied)?;
        Ok(done.into())
    }

    /// The SECOND TRY of a flush that failed (P2): the whole batch again and, when it fails for
    /// a reason of one row, id by id with the failing ids skipped; flushes until the log is
    /// empty, then prunes. The first try is [`Self::search_flush`], which returns the error.
    pub fn search_flush_retry(&mut self) -> Result<search::Flushed> {
        let mut sum = crate::search::index::Flushed::default();
        loop {
            let one = crate::search::index::flush_retry(
                &mut self.search,
                crate::search::index::MAX_BATCH,
            )?;
            sum.taken += one.taken;
            sum.indexed += one.indexed;
            sum.removed += one.removed;
            sum.unreadable += one.unreadable;
            sum.skipped += one.skipped;
            sum.applied = one.applied;
            sum.compact_after = one.compact_after;
            if !one.more {
                break;
            }
        }
        crate::search::log::prune_all(&self.conn, sum.applied)?;
        Ok(sum.into())
    }

    /// ONE flush job of at most `batch` log rows, and its prune (the segment and batch tests);
    /// between its commit and the prune it passes the task's kill point
    /// (`FAILPOINT_SEARCH_AFTER_COMMIT`), as the task does.
    pub fn search_flush_one(&mut self, batch: usize) -> Result<search::Flushed> {
        let done = crate::search::index::flush(&mut self.search, batch)?;
        if done.taken > 0 {
            kourier_db::failpoint::hit(crate::search::index::FAILPOINT_AFTER_COMMIT);
        }
        crate::search::log::prune_all(&self.conn, done.applied)?;
        Ok(done.into())
    }

    /// ONE flush job whose `between` runs on core.db's writer after the flush read its
    /// snapshot and before search.db's commit (a change the snapshot does not see), then the
    /// prune. Without `prune` the log is left as the commit left it.
    pub fn search_flush_between(
        &mut self,
        batch: usize,
        between: impl FnOnce(&Connection) -> Result<()>,
        prune: bool,
    ) -> Result<search::Flushed> {
        let conn = &self.conn;
        let mut between = Some(between);
        let mut failed = None;
        let done = crate::search::index::flush_with(
            &mut self.search,
            batch,
            crate::search::index::Pass::Whole,
            &mut || {
                if let Some(f) = between.take()
                    && let Err(e) = f(conn)
                {
                    failed = Some(e);
                }
            },
        )?;
        if let Some(e) = failed {
            return Err(e);
        }
        if prune {
            crate::search::log::prune_all(&self.conn, done.applied)?;
        }
        Ok(done.into())
    }

    /// THE PRUNE of the log up to `applied` (as the core's Low job runs it); how many rows went.
    pub fn search_prune(&mut self, applied: i64) -> Result<usize> {
        crate::search::log::prune_all(&self.conn, applied)
    }

    /// ONE prune JOB of the log up to `applied` (at most `PRUNE_ROWS` rows, as the core's Low
    /// job runs it): rows gone and whether more remain at or below `applied` (G1f again).
    pub fn search_prune_job(&mut self, applied: i64) -> Result<(usize, bool)> {
        crate::search::log::prune(&self.conn, applied)
    }

    /// ONE flush job like [`Self::search_flush_one`] but with NO prune: the log stays as the
    /// commit left it, and the caller prunes in jobs of its own (G1f again).
    pub fn search_flush_only(&mut self, batch: usize) -> Result<search::Flushed> {
        Ok(crate::search::index::flush(&mut self.search, batch)?.into())
    }

    /// ONE backfill batch of at most `batch` rows (G1c).
    pub fn search_backfill_one(&mut self, batch: usize) -> Result<search::Filled> {
        crate::search::index::backfill(&mut self.search, batch).map(search::Filled::from)
    }

    /// Backfill batches of `batch` rows until every row has been read (G1c).
    pub fn search_backfill_all(&mut self, batch: usize) -> Result<search::Filled> {
        crate::search::index::backfill_all(&mut self.search, batch).map(search::Filled::from)
    }

    /// THE REBUILD of search.db (G1c): a new file from core.db as it is now, every row at or
    /// below `floor`, as a file built again (or the first one of a core.db that has rows).
    pub fn search_rebuild(&mut self) -> Result<()> {
        self.search.rebuild(&self.search_paths)
    }

    /// The search task's open (G1c): search.db closed, opened again and judged against
    /// core.db; the reason it was built again, if it was.
    pub fn search_open(&mut self) -> Result<Option<search::Rebuild>> {
        self.search_open_after(|_| {})
    }

    /// [`Self::search_open`] with `between` run while no connection to search.db is open (it
    /// gets the file's path: a test replaces or damages the file there).
    pub fn search_open_after(
        &mut self,
        between: impl FnOnce(&Path),
    ) -> Result<Option<search::Rebuild>> {
        let placeholder = crate::search::index::Files {
            search: Connection::open_in_memory()?,
            core: Connection::open_in_memory()?,
        };
        drop(std::mem::replace(&mut self.search, placeholder));
        between(&self.search_paths.search);
        let (files, why) = crate::search::index::Files::open(&self.search_paths)?;
        self.search = files;
        Ok(why)
    }

    /// search.db's writer connection: the raw looks ([`search`]) and the steps of a
    /// compaction (G1d).
    pub fn search_db(&self) -> &Connection {
        &self.search.search
    }

    /// The same, mutable (a compaction's steps take it).
    pub fn search_writer(&mut self) -> &mut Connection {
        &mut self.search.search
    }

    /// Why [`Self::open`] built search.db again (`None`: it was new, or it belonged).
    pub fn search_judged(&self) -> Option<search::Rebuild> {
        self.search_judged
    }

    /// Where search.db is.
    pub fn search_path(&self) -> &Path {
        &self.search_paths.search
    }
}

/// A raw look at the search index (G1b, critic H2): what search.db's `messages_fts` holds, read
/// with FTS5's own `MATCH` and its `%_docsize` table, never through `Query::Search` (whose
/// query-time rules would hide a row the index still holds). Every word is cut by the index's
/// own rules first, so a test names a word as a person types it. The functions say which file
/// they read: `search` is search.db (`IngestHarness::search_db`), `core` is core.db.
pub mod search {
    use std::path::{Path, PathBuf};

    use kourier_db::rusqlite::Connection;
    use kourier_model::{ConversationId, MessageId};

    pub use crate::search::file::Rebuild;

    use crate::error::Result;
    use crate::search::normalize::normalize;
    use crate::search::{compact, file, index, log};

    /// What one flush did, counts only.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Flushed {
        /// Log rows taken.
        pub taken: usize,
        /// Rows written into the index.
        pub indexed: usize,
        /// Old entries removed (each named by `indexed`).
        pub removed: usize,
        /// Rows whose body does not parse.
        pub unreadable: usize,
        /// Ids whose write failed and were skipped.
        pub skipped: usize,
        /// `applied` after the job.
        pub applied: i64,
        /// Whether the log may hold more rows.
        pub more: bool,
    }

    impl From<index::Flushed> for Flushed {
        fn from(f: index::Flushed) -> Self {
            Self {
                taken: f.taken,
                indexed: f.indexed,
                removed: f.removed,
                unreadable: f.unreadable,
                skipped: f.skipped,
                applied: f.applied,
                more: f.more,
            }
        }
    }

    /// What one backfill batch did (G1c), counts only.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Filled {
        /// Rows read.
        pub looked: usize,
        /// Rows written into the index.
        pub indexed: usize,
        /// Rows whose body does not parse.
        pub unreadable: usize,
        /// Rows the index would not take (skipped).
        pub skipped: usize,
        /// `floor` after the batch (0: done).
        pub floor: i64,
    }

    impl From<index::Filled> for Filled {
        fn from(f: index::Filled) -> Self {
            Self {
                looked: f.looked,
                indexed: f.indexed,
                unreadable: f.unreadable,
                skipped: f.skipped,
                floor: f.floor,
            }
        }
    }

    /// The search.db beside a core.db ([`crate::search_db_beside`]).
    pub fn beside(core_db: &Path) -> PathBuf {
        file::beside(core_db)
    }

    /// The token of `word` (it must cut to exactly one).
    fn token(word: &str) -> String {
        let mut tokens = normalize(word);
        assert_eq!(tokens.len(), 1, "a test word cuts to one token");
        tokens.remove(0).text
    }

    fn ids(search: &Connection, expr: &str) -> Result<Vec<MessageId>> {
        let mut out: Vec<MessageId> = search
            .prepare("SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?1")?
            .query_map([expr], |r| r.get::<_, i64>(0))?
            .map(|r| r.map(|rowid| MessageId(rowid & index::ID_MASK)))
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        out.sort_unstable_by_key(|m| m.0);
        Ok(out)
    }

    /// The messages whose index entry holds the word exactly (`words:"<token>"`).
    pub fn ids_with(search: &Connection, word: &str) -> Result<Vec<MessageId>> {
        ids(search, &format!("words:\"{}\"", token(word)))
    }

    /// The messages whose index entry holds the word under chat `conv`'s token.
    pub fn ids_with_in(
        search: &Connection,
        word: &str,
        conv: ConversationId,
    ) -> Result<Vec<MessageId>> {
        ids(
            search,
            &format!("words:\"{}\" AND meta:\"c{}\"", token(word), conv.0),
        )
    }

    /// The messages that have an entry in the index (a `%_docsize` row, any hour).
    pub fn indexed(search: &Connection) -> Result<Vec<MessageId>> {
        let mut out: Vec<MessageId> = search
            .prepare("SELECT id FROM messages_fts_docsize")?
            .query_map([], |r| r.get::<_, i64>(0))?
            .map(|r| r.map(|rowid| MessageId(rowid & index::ID_MASK)))
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        out.sort_unstable_by_key(|m| m.0);
        Ok(out)
    }

    /// The `indexed` map: (message, rowid) in message order. THE MAP INVARIANT: it names
    /// exactly the entries `%_docsize` holds, each under its rowid.
    pub fn indexed_map(search: &Connection) -> Result<Vec<(MessageId, i64)>> {
        Ok(search
            .prepare("SELECT message_id, rowid FROM indexed ORDER BY message_id")?
            .query_map([], |r| Ok((MessageId(r.get(0)?), r.get(1)?)))?
            .collect::<kourier_db::rusqlite::Result<_>>()?)
    }

    /// The rowids of `%_docsize`, sorted.
    pub fn docsize_rowids(search: &Connection) -> Result<Vec<i64>> {
        Ok(search
            .prepare("SELECT id FROM messages_fts_docsize ORDER BY id")?
            .query_map([], |r| r.get(0))?
            .collect::<kourier_db::rusqlite::Result<_>>()?)
    }

    /// Asserts THE MAP INVARIANT: `indexed` and `%_docsize` name the same entries.
    pub fn assert_map_holds(search: &Connection) -> Result<()> {
        let mut map: Vec<i64> = indexed_map(search)?.into_iter().map(|(_, r)| r).collect();
        map.sort_unstable();
        let docsize = docsize_rowids(search)?;
        assert_eq!(map, docsize, "`indexed` names exactly what the index holds");
        for (id, rowid) in indexed_map(search)? {
            assert_eq!(
                rowid & index::ID_MASK,
                id.0,
                "an entry's rowid holds its id"
            );
        }
        Ok(())
    }

    /// How many rows wait in core.db's log.
    pub fn queued(core: &Connection) -> Result<usize> {
        let n: i64 = core.query_row("SELECT count(*) FROM search_queue", [], |r| r.get(0))?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    /// The seqs in core.db's log, in order.
    pub fn log_seqs(core: &Connection) -> Result<Vec<i64>> {
        Ok(core
            .prepare("SELECT seq FROM search_queue ORDER BY seq")?
            .query_map([], |r| r.get(0))?
            .collect::<kourier_db::rusqlite::Result<_>>()?)
    }

    /// The log's position (the last seq it handed out, 0 when none).
    pub fn position(core: &Connection) -> Result<i64> {
        log::position(core)
    }

    /// core.db's epoch.
    pub fn epoch(core: &Connection) -> Result<String> {
        log::epoch(core)
    }

    /// Writes a new epoch into core.db (what a wholesale rewrite of the rows would do).
    pub fn new_epoch(core: &Connection) -> Result<()> {
        log::new_epoch(core)
    }

    /// core.db's compaction ask (`compact_after`, a log position).
    pub fn compact_after(core: &Connection) -> Result<i64> {
        log::number(core, "compact_after")
    }

    /// Asks for a compaction as a clear does ([`log::ask_now`]).
    pub fn ask_now(core: &Connection) -> Result<()> {
        log::ask_now(core)
    }

    /// How many segments the index has (each segment has at least one row in `%_idx`).
    pub fn segments(search: &Connection) -> Result<usize> {
        let n: i64 = search.query_row(
            "SELECT count(DISTINCT segid) FROM messages_fts_idx",
            [],
            |r| r.get(0),
        )?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    /// A number of search.db's `meta` (`applied`, `floor`, `top`, `dead`, `dead_since`,
    /// `compacted_for`, `checkpoint_due`, `version`, `norm`); 0 when absent.
    pub fn meta(search: &Connection, key: &str) -> Result<i64> {
        file::meta(search, key)
    }

    /// Sets a number of `meta` (a file of another version, a clock ahead).
    pub fn set_meta(search: &Connection, key: &str, value: i64) -> Result<()> {
        file::set_meta(search, key, value)
    }

    /// The version of search.db's schema in this build.
    pub const SEARCH_FILE_VERSION: i64 = file::SEARCH_FILE_VERSION;

    /// The rowid a message has in the index (`hour << 36 | id`).
    pub fn rowid_of(sort_key: i64, id: MessageId) -> Option<i64> {
        index::rowid_of(sort_key, id.0)
    }

    /// The backfill's progress: `(ready, percent)`.
    pub fn progress(search: &Connection) -> Result<(bool, u8)> {
        file::progress(search)
    }

    /// FTS5's own check of the index (`integrity-check`; a contentless table checks the
    /// index alone).
    pub fn integrity_check(search: &Connection) -> Result<()> {
        search.execute(
            "INSERT INTO messages_fts(messages_fts, rank) VALUES ('integrity-check', 0)",
            [],
        )?;
        Ok(())
    }

    /// The version of the token rules of this build.
    pub const NORMALIZER_VERSION: u32 = crate::search::NORMALIZER_VERSION;

    /// The token of `word` as the index holds it (it must cut to exactly one).
    pub fn token_of(word: &str) -> String {
        token(word)
    }

    /// Whether a compaction is due (G1d).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Due {
        /// Not now; `at`: when the 24 hour rule falls due (the core's clock, ms).
        Not { at: Option<i64> },
        /// Due.
        Now,
    }

    /// [`Due`] at `now_ms`, given core.db's `compact_after` (notes `dead_since` the first time
    /// `dead` is above 0, and moves one ahead of the clock back to it).
    pub fn due(search: &Connection, compact_after: i64, now_ms: i64) -> Result<Due> {
        Ok(match compact::due(search, compact_after, now_ms)? {
            compact::Due::Not { at } => Due::Not { at },
            compact::Due::Now => Due::Now,
        })
    }

    /// The start of a compaction: its snapshot of `dead`.
    pub fn begin(search: &Connection) -> Result<i64> {
        compact::begin(search)
    }

    /// One step of a compaction; whether it did work.
    pub fn step(search: &mut Connection) -> Result<bool> {
        compact::step(search)
    }

    /// The end of a compaction that began with `snapshot`, answering `asked`, at `started_ms`.
    pub fn finish(
        search: &mut Connection,
        snapshot: i64,
        asked: i64,
        started_ms: i64,
    ) -> Result<()> {
        compact::finish(search, snapshot, asked, started_ms)
    }

    /// The truncating checkpoint of search.db; `false` while a reader holds it.
    pub fn checkpoint(search: &Connection) -> Result<bool> {
        compact::checkpoint(search)
    }

    /// What a whole compaction did, counts only.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Compacted {
        /// Steps that did work.
        pub steps: usize,
        /// Whether the truncating checkpoint went through.
        pub checkpointed: bool,
    }

    /// A whole compaction in one go, answering `asked`.
    pub fn compact_all(search: &mut Connection, asked: i64, now_ms: i64) -> Result<Compacted> {
        let c = compact::compact_all(search, asked, now_ms)?;
        Ok(Compacted {
            steps: c.steps,
            checkpointed: c.checkpointed,
        })
    }

    /// The most rows of the index one page of `Query::Search` looks up.
    pub const CAP: usize = crate::search::query::CAP;

    /// One page of `Query::Search` on search.db and core.db connections, as the core runs it but
    /// with at most `cap` rows looked up and no time cap; and how many rows it looked up.
    pub fn page(
        search: &Connection,
        core: &Connection,
        q: &crate::facade::SearchQuery,
        cap: usize,
    ) -> Result<(crate::facade::SearchPage, usize)> {
        let far = std::time::Instant::now() + std::time::Duration::from_secs(3600);
        crate::search::query::search(search, core, q, cap, far).map(|w| (w.page, w.looked))
    }

    /// One page of `Query::Search` that must end by `deadline` (a pattern scan reads the clock
    /// every few hundred rows; a deadline already past gives a `capped` page).
    pub fn page_by(
        search: &Connection,
        core: &Connection,
        q: &crate::facade::SearchQuery,
        deadline: std::time::Instant,
    ) -> Result<crate::facade::SearchPage> {
        crate::search::query::search(search, core, q, crate::search::query::CAP, deadline)
            .map(|w| w.page)
    }

    /// The FTS5 expression a typed text gives (`None`: nothing can match).
    pub fn match_expr(text: &str, chats: Option<&[ConversationId]>) -> Option<String> {
        crate::search::query::match_expr(&crate::search::normalize::query_terms(text), chats)
    }

    /// `EXPLAIN QUERY PLAN` of the search's statements, each on its own file: (name, plan
    /// lines joined by `|`). The word search's two, and the pattern scan's two (R5).
    pub fn plans(search: &Connection, core: &Connection) -> Result<Vec<(&'static str, String)>> {
        use crate::search::pattern::{ID_SCAN, TIMELINE_SCAN};
        use crate::search::query::{HIT, ROWIDS};
        let plan = |conn: &Connection,
                    sql: &str,
                    params: &[&dyn kourier_db::rusqlite::ToSql]|
         -> Result<String> {
            let lines: Vec<String> = conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?
                .query_map(params, |r| r.get::<_, String>(3))?
                .collect::<kourier_db::rusqlite::Result<_>>()?;
            Ok(lines.join("|"))
        };
        Ok(vec![
            (
                "rowids",
                plan(search, ROWIDS, &[&"words:(\"plan\")", &i64::MAX])?,
            ),
            ("hit", plan(core, HIT, &[&1_i64])?),
            ("id_scan", plan(core, ID_SCAN, &[&i64::MAX])?),
            (
                "timeline_scan",
                plan(core, TIMELINE_SCAN, &[&1_i64, &i64::MAX, &i64::MAX])?,
            ),
        ])
    }

    /// The REAL store (its writer thread and its read pools) and the REAL search task
    /// (`search::index::keep`) on a profile's files, driven on a core clock the caller owns
    /// (G1f again: what the core's writer pays while the search writer compacts). The jobs
    /// below go through `Store::write` exactly as the runtime's do (High priority, the account's
    /// lane), so their latency is queue wait plus run time on the one writer thread. Opens the
    /// files at `core` and `search::beside(core)` under one key; nothing else may hold
    /// search.db's writer while it lives.
    pub struct Task {
        store: std::sync::Arc<crate::store::Store>,
        rt: tokio::runtime::Runtime,
        clock: kourier_backend_api::FakeClock,
        stop: std::sync::Arc<tokio::sync::Notify>,
        keeper: Option<std::thread::JoinHandle<()>>,
        account: kourier_model::AccountId,
        scope: kourier_model::MsgIdScope,
    }

    impl Task {
        /// Opens the store and starts the search task (no backfill delay).
        pub fn start(
            core: &Path,
            key: &kourier_db::DbKey,
            account: kourier_model::AccountId,
            scope: kourier_model::MsgIdScope,
            clock_ms: u64,
        ) -> Result<Self> {
            let (store, _) = crate::store::Store::open_with(core, key, &file::beside(core), key)?;
            let store = std::sync::Arc::new(store);
            let clock = kourier_backend_api::FakeClock::new(clock_ms);
            let stop = std::sync::Arc::new(tokio::sync::Notify::new());
            let keeper = {
                let (store, clock, stop) = (store.clone(), clock.clone(), stop.clone());
                std::thread::Builder::new()
                    .name("search-task".into())
                    .spawn(move || {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .expect("keeper runtime");
                        rt.block_on(async {
                            let clock: std::sync::Arc<dyn kourier_backend_api::Clock> =
                                std::sync::Arc::new(clock);
                            tokio::select! {
                                () = index::keep(
                                    &store,
                                    std::time::Duration::ZERO,
                                    clock,
                                    || false,
                                ) => {}
                                () = stop.notified() => {}
                            }
                        });
                    })
                    .expect("spawn the search task")
            };
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("caller runtime");
            Ok(Self {
                store,
                rt,
                clock,
                stop,
                keeper: Some(keeper),
                account,
                scope,
            })
        }

        /// Whether the task has judged (or made) search.db.
        pub fn ready(&self) -> bool {
            self.store.search_link().is_ready()
        }

        /// Moves the core clock forward (the 24 hour rule's time).
        pub fn advance_clock(&self, d: std::time::Duration) {
            self.clock.advance(d);
        }

        /// The `applied` search.db last committed.
        pub fn applied(&self) -> i64 {
            self.store.search_link().applied()
        }

        /// One ingest job (a live envelope's events) on the core's writer; its latency.
        pub fn ingest(
            &self,
            events: Vec<kourier_backend_api::BackendEvent>,
            now: i64,
        ) -> Result<std::time::Duration> {
            let (account, scope) = (self.account, self.scope);
            let t = std::time::Instant::now();
            self.rt.block_on(self.store.write(
                kourier_db::Priority::High,
                crate::store::lane(account),
                move |c| {
                    crate::ingest::apply_events(
                        c,
                        account,
                        scope,
                        kourier_model::HistoryReadState::Known,
                        kourier_model::ServerRules::default(),
                        now,
                        events,
                    )
                    .map(|_| ())
                },
            ))?;
            Ok(t.elapsed())
        }

        /// One send job (a text queued into `conv`, as `UiCommand::SendText` does); its latency.
        pub fn send(
            &self,
            net: std::sync::Arc<dyn kourier_backend_api::Network>,
            conv: ConversationId,
            text: &str,
            now: i64,
        ) -> Result<std::time::Duration> {
            let text = kourier_model::Text::plain(text);
            let t = std::time::Instant::now();
            self.rt.block_on(self.store.write(
                kourier_db::Priority::High,
                crate::store::lane(self.account),
                move |c| {
                    crate::outbox::enqueue_text(c, net.as_ref(), conv, text, None, now).map(|_| ())
                },
            ))?;
            Ok(t.elapsed())
        }

        /// One read-mark job (`UiCommand::MarkRead`); its latency and whether anything was unread.
        pub fn mark_read(
            &self,
            conv: ConversationId,
            now: i64,
        ) -> Result<(std::time::Duration, bool)> {
            let t = std::time::Instant::now();
            let some = self.rt.block_on(self.store.write(
                kourier_db::Priority::High,
                crate::store::lane(self.account),
                move |c| crate::local::mark_read(c, conv, now, now, false).map(|m| m.is_some()),
            ))?;
            Ok((t.elapsed(), some))
        }

        /// One `Query::Search` page through the store's own search reader with the production
        /// time cap; `None` while the index is not ready or a newer search superseded it.
        pub fn query(
            &self,
            q: crate::facade::SearchQuery,
        ) -> Result<Option<(crate::facade::SearchPage, usize, std::time::Duration)>> {
            let t = std::time::Instant::now();
            let got = self.rt.block_on(self.store.search(move |s, c, deadline| {
                crate::search::query::search(s, c, &q, crate::search::query::CAP, deadline)
            }))?;
            Ok(match got {
                crate::store::Searched::Done(w) => Some((w.page, w.looked, t.elapsed())),
                _ => None,
            })
        }

        /// Ends the task and closes the store.
        pub fn shutdown(mut self) {
            self.stop.notify_one();
            if let Some(k) = self.keeper.take() {
                let _ = k.join();
            }
            self.store.close();
        }
    }
}

/// The synthetic filter mix of the perf runs (`cargo xtask perf --filters <n>`, the filter
/// timing tests): `n` specs of literal words, alternations of 5 to 20 words, `\d{3,}`, an
/// address-like pattern and Cyrillic words with `\b`; every 5th also has a sender and a chat
/// pattern, every 7th matches case, half notify, all highlight. The words are made up
/// (8 letters), nobody's.
pub fn filter_mix(n: usize) -> Vec<crate::facade::FilterSpec> {
    use kourier_model::Redacted;
    const SWATCHES: [u32; 8] = [
        0xFFD60A, 0xFF9F0A, 0xFF453A, 0xFF6EB4, 0xBF5AF2, 0x0A84FF, 0x1FC8B4, 0x30D158,
    ];
    let pat = |p: String| Some(Redacted::new(p));
    (0..n)
        .map(|i| {
            let text = match i % 6 {
                0 => filter_mix_word(i, false),
                1 => {
                    let words = 5 + (i * 7) % 16;
                    (0..words)
                        .map(|k| filter_mix_word(i * 31 + k, false))
                        .collect::<Vec<_>>()
                        .join("|")
                }
                2 => r"\d{3,}".to_owned(),
                3 => r"\b\w+@\w+\.\w+\b".to_owned(),
                4 => format!(
                    r"\b{}\w*",
                    filter_mix_word(i, true).chars().take(3).collect::<String>()
                ),
                _ => filter_mix_word(i, true),
            };
            let wide = i % 5 == 4;
            crate::facade::FilterSpec {
                name: Redacted::new(format!("perf filter {i}")),
                text_pattern: pat(text),
                sender_pattern: wide.then(|| Redacted::new(r"^\w+".to_owned())),
                chat_pattern: wide.then(|| Redacted::new(r"\w".to_owned())),
                match_case: i % 7 == 0,
                notify: i % 2 == 0,
                notify_muted: false,
                highlight: true,
                colour: SWATCHES[i % SWATCHES.len()],
            }
        })
        .collect()
}

/// The 8-letter made-up word `k` of [`filter_mix`] (Latin, or Cyrillic when `cyrillic`).
pub fn filter_mix_word(k: usize, cyrillic: bool) -> String {
    const LC: [char; 14] = [
        'b', 'd', 'k', 'l', 'm', 'n', 'p', 'r', 's', 't', 'v', 'z', 'g', 'h',
    ];
    const LV: [char; 5] = ['a', 'e', 'i', 'o', 'u'];
    const CC: [char; 14] = [
        'б', 'д', 'к', 'л', 'м', 'н', 'п', 'р', 'с', 'т', 'в', 'з', 'г', 'х',
    ];
    const CV: [char; 5] = ['а', 'е', 'і', 'о', 'у'];
    let (c, v) = if cyrillic { (&CC, &CV) } else { (&LC, &LV) };
    let mut n = k.wrapping_mul(2_654_435_761) % 70_usize.pow(4);
    let mut out = Vec::with_capacity(8);
    for _ in 0..4 {
        out.push(c[(n % 70) / 5]);
        out.push(v[n % 5]);
        n /= 70;
    }
    out.into_iter().collect()
}

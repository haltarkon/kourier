//! The stage of a history import: the private, encrypted file the archive's frames are mapped
//! into while the archive is read (docs/architecture.md §14.2 item 14; `transfer.rs`, "The
//! import").
//!
//! Frames are not trusted until the reader has checked the archive to its last byte
//! (`archive` module docs), so nothing of an archive may reach `backend.db`, the core or
//! the blob store before that. The stage is where it waits: `transfer/stage.db` in the
//! account's directory, beside the archive file, a SQLCipher database opened through
//! `kourier_db::open` like every other database of Kourier (WAL, a check value on every
//! page). Before the commit (`transfer.rs`) an abort, a quit or a kill costs one file;
//! after it the stage is what is delivered to the core.
//!
//! # The key
//!
//! `HKDF-SHA256(ikm = the transfer's ephemeral backup key, no salt, info = "kourier history
//! stage 1")`, 32 bytes, used as the raw database key. No new secret is stored: the read
//! holds the ephemeral key already, and the write that ends a transfer wipes it from
//! `backend.db` (and from its WAL), after which a stage that is still on disk can be opened
//! by nobody. The info string is fixed; a change of the stage's layout changes
//! [`FORMAT`], not the key.
//!
//! # The tables
//!
//! ```sql
//! meta(k TEXT PRIMARY KEY, v)             -- 'format' at creation; the totals at the commit
//! contacts(n INTEGER PRIMARY KEY, op)     -- one person that something shown uses
//! chats(idx INTEGER PRIMARY KEY, conv UNIQUE, op, pinned, invited,
//!       last_at, n_rows, n_loud)          -- one conversation that has a row, is pinned or
//!                                         -- is a group we are invited to (`pinned`: the
//!                                         -- phone's order); the three counters are
//!                                         -- written at the commit
//! items(chat, n, bytes, msg, PRIMARY KEY(chat, n)) WITHOUT ROWID
//! ```
//!
//! `op` is the JSON of a private type of this crate ([`StagedContact`], [`StagedChat`]);
//! `msg` is one `IncomingMessage` of the contract as JSON, `bytes` its length; `n` is the
//! running number of the frame the row came from times `map::ROWS_PER_FRAME`, plus the
//! row's place among that frame's rows (a group change makes several). One row per
//! MESSAGE: an archive holds the items of all chats in one sequence (Android: by received
//! time over all chats), and the key `(chat, n)` gives each chat's rows back in frame order
//! whatever the interleaving, so the reading thread keeps no buffer per chat.
//!
//! # Bounds
//!
//! The payload (the bytes of every `op` and `msg`) is capped ([`StageLimits::max_bytes`],
//! 8 GiB): over it the import fails as "too large" and the stage is removed. Rows are
//! written in one transaction that is committed every [`StageLimits::commit_every`] bytes of
//! payload, so the stage's own WAL holds one such piece at most, not the whole history.
//!
//! # Secrets
//!
//! The stage holds message bodies, names, profile keys, the master keys of groups and the
//! keys of attachments (in their pointers).
//! Nothing of a row is logged or put into an error: a [`StageError`] is a static reason, and
//! the `Debug` of the staged types prints ids and counts only.

use std::path::{Path, PathBuf};

use hkdf::Hkdf;
use kourier_backend_api::IncomingMessage;
use kourier_db::DbKey;
use kourier_db::rusqlite::{Connection, OptionalExtension as _};
use kourier_model::{ConvKey, UserId};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroizing;

/// The stage's file name inside the transfer directory.
pub(crate) const FILE: &str = "stage.db";

/// The layout of the stage and the meaning of its rows (`meta.format`). A reader refuses
/// any other value: a stage is never migrated, it is read again from the archive. It
/// changes with the tables AND with the mapper's output (a stage of an earlier mapper must
/// not be delivered under rules its rows were not made for).
///
/// 2 (step S3a): `chats` has no `unread` counter (no unread count is delivered: the rows'
/// `read` says it), an own item that was not sent keeps its timer without a start, and an
/// own item the phone calls sent whose timer has no start keeps its timer started at the
/// import's time.
///
/// 3 (step S3b): the mapper maps formatting and mentions, quotes, reactions, the ids of an
/// edited message's later versions, stickers (not available), the placeholders of the kinds
/// Kourier does not show (with their raw item), the notices of 1:1 chats and call lines.
///
/// 4 (step S4a): a chat's `op` has a kind (`StagedKind`) and what its chat frame carried
/// (archived, marked unread, muted until, the 1:1 timer and its version, `ChatFrame`);
/// `chats.pinned` is the phone's pinned order, and a pinned chat is staged and shown even
/// without a loud row (or without any row).
///
/// 5 (step S4b): groups. A chat's kind can be a group (`StagedKind::Group`: master key,
/// title, members, our membership, blocked, accepted), a group's `ChatFrame::timer_secs` is
/// its timer (the chat frame's, else the snapshot's), `chats.invited` lists a group we are
/// invited to, and the mapper maps the items of group chats (authors, quotes, reactions and
/// send states of any staged person, group changes, group calls).
///
/// 6 (step S5): attachment pointers. A message's attachments, its long text and a sticker's
/// image are attachments with a `remote` (`BackendBlob` v 3, the trimmed `FilePointer`)
/// while the server may still have them, else `AttachmentState::Unavailable` without one.
pub(crate) const FORMAT: i64 = 6;

/// The HKDF info of the stage's key.
const KEY_INFO: &[u8] = b"kourier history stage 1";

/// The largest stage, in bytes of payload. The reader lets an archive decompress to 8 GiB
/// (`archive::Limits`), and a staged row is about as large as the frame it came from, so a
/// stage of a real account stays far below this; it bounds what a hostile or broken archive
/// can make this device write. Kourier's own limit (a row of the limits table of
/// architecture §13).
pub(crate) const MAX_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// How much payload one transaction of the writer holds before it is committed: the size
/// the stage's WAL is bounded by.
pub(crate) const COMMIT_EVERY: u64 = 16 * 1024 * 1024;

/// The bounds of a stage; tests shrink them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StageLimits {
    /// The most payload, in bytes.
    pub(crate) max_bytes: u64,
    /// Payload per transaction.
    pub(crate) commit_every: u64,
}

impl Default for StageLimits {
    fn default() -> Self {
        Self {
            max_bytes: MAX_BYTES,
            commit_every: COMMIT_EVERY,
        }
    }
}

/// Why a stage could not be written or read. Log-safe: a reason, never content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum StageError {
    /// The payload passed [`StageLimits::max_bytes`].
    #[error("the stage is larger than its limit")]
    TooLarge,
    /// A file of the stage could not be created or removed (the system's reason, as a kind).
    #[error("a file of the stage failed ({0:?})")]
    Io(std::io::ErrorKind),
    /// The database refused: a full disk, a page that failed its check, a wrong key. The
    /// text says which step, never what was in a row.
    #[error("the stage's database failed while {0}")]
    Db(&'static str),
    /// The stage is of another layout, or is not complete (it was never committed).
    #[error("the stage is not one this version wrote to its end")]
    Unreadable,
}

/// The stage's database key. Wiped on drop; `Debug` prints nothing.
pub(crate) struct StageKey(DbKey);

impl StageKey {
    /// The key of the stage of the transfer whose ephemeral backup key is `ephemeral`
    /// (module docs, "The key").
    pub(crate) fn derive(ephemeral: &[u8; 32]) -> Self {
        let mut key = Zeroizing::new([0u8; 32]);
        // 32 bytes are a valid HKDF-SHA256 output length: `expand` cannot fail.
        let _ = Hkdf::<Sha256>::new(None, ephemeral).expand(KEY_INFO, key.as_mut());
        Self(DbKey::from_bytes(*key))
    }

    /// The raw key, for a test that decrypts the stage's pages itself.
    #[cfg(feature = "testing")]
    pub(crate) fn bytes(&self) -> [u8; 32] {
        *self.0.as_bytes()
    }
}

impl std::fmt::Debug for StageKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StageKey(<redacted>)")
    }
}

/// A person the import will show: the peer of a staged chat or the author of a staged row.
/// What delivery needs to tell the core (`ContactUpsert`) and to write this backend's own
/// rows for the person. `Debug` prints the kind of id only.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StagedContact {
    /// The person's id in Kourier: `aci:`, else `pni:`, else `e164:`.
    pub(crate) user: UserId,
    /// The ACI, when the archive has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) aci: Option<uuid::Uuid>,
    /// The PNI, when the archive has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pni: Option<uuid::Uuid>,
    /// The phone number as `+<digits>`, when the archive has a valid one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) e164: Option<String>,
    /// The profile key, when it is 32 bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) profile_key: Option<[u8; 32]>,
    /// The chat with the person is accepted on the phone (`profileSharing`).
    #[serde(default)]
    pub(crate) profile_sharing: bool,
    /// The person is blocked.
    #[serde(default)]
    pub(crate) blocked: bool,
    /// The person is hidden (removed from the phone's list).
    #[serde(default)]
    pub(crate) hidden: bool,
    /// Since when the person is not registered (ms), when the archive says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) unregistered_at: Option<i64>,
    /// The profile name (given and family name), cut at the mapper's bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) profile_name: Option<String>,
    /// The name in the phone's address book.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) system_name: Option<String>,
    /// The user's own nickname for the person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) nickname: Option<String>,
}

impl std::fmt::Debug for StagedContact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagedContact")
            .field("scheme", &self.user.scheme())
            .field("profile_key", &self.profile_key.is_some())
            .finish_non_exhaustive()
    }
}

/// A conversation the import stages: one that has a row, that the phone pinned, or a group we
/// are invited to. Its kind and what its chat frame carried.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StagedChat {
    /// Whom the conversation is with.
    pub(crate) kind: StagedKind,
    /// What its chat frame carried (the first frame of the conversation).
    #[serde(default)]
    pub(crate) frame: ChatFrame,
}

impl StagedChat {
    /// The chat with the person staged as row `peer` of `contacts`, with a chat frame that
    /// says nothing.
    #[cfg(test)]
    pub(crate) fn direct(peer: u32) -> Self {
        Self {
            kind: StagedKind::Direct { peer },
            frame: ChatFrame::default(),
        }
    }

    /// The chat is Note to Self.
    pub(crate) fn is_self(&self) -> bool {
        matches!(self.kind, StagedKind::NoteToSelf)
    }

    /// The `n` of the peer's row in `contacts`, for a chat with one person.
    pub(crate) fn peer(&self) -> Option<u32> {
        match &self.kind {
            StagedKind::Direct { peer } => Some(*peer),
            StagedKind::NoteToSelf | StagedKind::Group(_) => None,
        }
    }

    /// The group, for a group's chat.
    pub(crate) fn group(&self) -> Option<&StagedGroup> {
        match &self.kind {
            StagedKind::Group(group) => Some(group),
            StagedKind::NoteToSelf | StagedKind::Direct { .. } => None,
        }
    }

    /// A group we are invited to: listed whatever it holds (its one item, the invitation, is
    /// no notice of the contract), with the request to accept, as Android and Desktop list it.
    pub(crate) fn invited(&self) -> bool {
        self.group()
            .is_some_and(|group| group.membership == GroupMembership::Invited)
    }
}

/// The kind of a staged conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum StagedKind {
    /// Note to Self.
    NoteToSelf,
    /// A chat with one person.
    Direct {
        /// The `n` of the person's row in `contacts`.
        peer: u32,
    },
    /// A group's chat.
    Group(Box<StagedGroup>),
}

/// A group of the archive, as the phone's snapshot of it says (step S4b). Delivery writes the
/// `groups` row WITHOUT this snapshot (the server's state comes with the first fetch) and
/// upserts the chat with what is here. `Debug` prints the membership and counts only.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StagedGroup {
    /// The master key (a secret of the group).
    pub(crate) master_key: MasterKey,
    /// The title, when the snapshot has one that is not empty (cut at the mapper's bound).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    /// Full members, invited and requesting people with their roles; `None` for an empty
    /// snapshot (the phone had no state of the group), which must not empty a list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) members: Option<Vec<kourier_model::Member>>,
    /// Our place in the group by the snapshot.
    pub(crate) membership: GroupMembership,
    /// Blocked on the phone.
    #[serde(default)]
    pub(crate) blocked: bool,
    /// Accepted on the phone (the group's message request: `Group.whitelisted`).
    #[serde(default)]
    pub(crate) whitelisted: bool,
}

impl std::fmt::Debug for StagedGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagedGroup")
            .field("membership", &self.membership)
            .field("title", &self.title.is_some())
            .field("members", &self.members.as_ref().map(Vec::len))
            .finish_non_exhaustive()
    }
}

/// Our place in a group by the phone's snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum GroupMembership {
    /// A full member.
    Member,
    /// Invited (by our ACI or our PNI).
    Invited,
    /// We asked to join and wait for an administrator.
    Requesting,
    /// A snapshot without us: we left or were removed.
    Left,
    /// An empty snapshot: the phone had no state of the group.
    Unknown,
}

/// A group's master key. Wiped on drop; `Debug` prints nothing.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MasterKey(pub(crate) [u8; 32]);

impl Drop for MasterKey {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(<redacted>)")
    }
}

/// What a chat frame of the archive says of its conversation, as the import delivers it.
/// The pinned order is not here: it is the `pinned` column of `chats`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChatFrame {
    /// Archived on the phone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) archived: bool,
    /// Marked unread on the phone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) marked_unread: bool,
    /// Muted until (ms; `i64::MAX` is for ever), converted as the Storage Service sync
    /// converts it (`storage::muted_until`). A mute that ran out is a time in the past.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) muted_until: Option<i64>,
    /// The chat's disappearing timer in seconds (at most `i32::MAX`); `None` when it is off.
    /// A group's: the chat frame's when it has one, else the snapshot's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) timer_secs: Option<u32>,
    /// The version of the chat's timer (Signal `expireTimerVersion`), at least 1: the phone
    /// writes it also when the timer is off, and what Kourier sends carries it.
    #[serde(default = "first_version")]
    pub(crate) timer_version: u32,
}

impl Default for ChatFrame {
    /// A chat frame that says nothing: no flag, no mute, no timer, the first version.
    fn default() -> Self {
        Self {
            archived: false,
            marked_unread: false,
            muted_until: None,
            timer_secs: None,
            timer_version: first_version(),
        }
    }
}

fn first_version() -> u32 {
    1
}

/// One chat's counters, known when the last frame was mapped and written at the commit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChatCounters {
    /// The chat's `idx`.
    pub(crate) idx: u32,
    /// Rows staged for it.
    pub(crate) n_rows: u64,
    /// Rows that make a chat show in the list (`map`, "quiet and loud").
    pub(crate) n_loud: u64,
    /// The newest sort time of its rows (ms).
    pub(crate) last_at: i64,
}

/// What a whole stage holds. Counts, no content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StageTotals {
    /// Rows of `items`.
    pub(crate) messages: u64,
    /// Rows of `chats`.
    pub(crate) chats: u64,
    /// Chats with at least one loud row, a pinned order or an invitation: the ones delivery
    /// shows.
    pub(crate) shown_chats: u64,
    /// Rows of `contacts`.
    pub(crate) contacts: u64,
    /// Bytes of payload.
    pub(crate) bytes: u64,
}

/// A chat as the stage gives it back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StagedChatRow {
    /// The conversation.
    pub(crate) conv: ConvKey,
    /// What the chat frame carried.
    pub(crate) op: StagedChat,
    /// `Chat.pinnedOrder` when set and not 0: the place in the phone's list of pinned chats
    /// (ascending; there may be gaps).
    pub(crate) pinned: Option<u32>,
    /// Its counters.
    pub(crate) counters: ChatCounters,
}

impl StagedChatRow {
    /// Whether delivery shows the chat: it has a row that makes a chat show (`map`, "quiet
    /// and loud"), the phone pinned it (Android and Desktop list a pinned chat whatever it
    /// holds), or it is a group we are invited to ([`StagedChat::invited`]). [`SHOWN`] is the
    /// same rule in SQL.
    pub(crate) fn shown(&self) -> bool {
        self.counters.n_loud > 0 || self.pinned.is_some() || self.op.invited()
    }
}

/// [`StagedChatRow::shown`] as a condition on `chats`.
const SHOWN: &str = "n_loud > 0 OR pinned IS NOT NULL OR invited = 1";

/// A row of a chat as the stage gives it back.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StagedItem {
    /// The number of the frame it came from: the order inside its chat.
    pub(crate) n: u64,
    /// The length of its JSON in the stage.
    pub(crate) bytes: u64,
    /// The message.
    pub(crate) msg: IncomingMessage,
}

/// The path of the stage in `dir` (the transfer directory).
pub(crate) fn path_in(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// Removes the stage's files from `dir` (blocking; missing ones are fine): the database, its
/// WAL and its index. The first error is returned after every file was tried.
pub(crate) fn remove(dir: &Path) -> Result<(), StageError> {
    let mut first = Ok(());
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let path = dir.join(format!("{FILE}{suffix}"));
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => first = first.and(Err(StageError::Io(e.kind()))),
        }
    }
    first
}

/// Which file a path names now (device and inode).
fn file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path)
        .ok()
        .map(|meta| (meta.dev(), meta.ino()))
}

fn db(what: &'static str) -> impl Fn(kourier_db::rusqlite::Error) -> StageError {
    move |_| StageError::Db(what)
}

fn to_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn to_u64(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

/// Writes one stage: rows in, then exactly one of [`commit`](Self::commit) and
/// [`abort`](Self::abort). Blocking; used on the thread that reads the archive.
pub(crate) struct StageWriter {
    conn: Connection,
    dir: PathBuf,
    /// The file this writer made: what it may remove ([`Self::abort`]).
    id: Option<(u64, u64)>,
    limits: StageLimits,
    /// Payload written so far.
    bytes: u64,
    /// Payload of the open transaction.
    pending: u64,
    in_tx: bool,
    contacts: u64,
    chats: u64,
    messages: u64,
}

impl StageWriter {
    /// A new, empty stage in `dir`, which must exist (the transfer directory, 0700). A stage
    /// that is there already is removed first: it is what an import left that never reached
    /// its commit.
    pub(crate) fn create(
        dir: &Path,
        key: &StageKey,
        limits: StageLimits,
    ) -> Result<Self, StageError> {
        use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
        remove(dir)?;
        let path = path_in(dir);
        // Made here with its mode, so that the database and the WAL and index files SQLite
        // makes beside it (with the database's mode) are this user's only, whatever the
        // directory and the umask are.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| StageError::Io(e.kind()))?;
        // Which file this call made, asked of its own descriptor: a look at the path could
        // already see the file of another writer that replaced it. Closed before the
        // database is opened (closing any descriptor of a file drops the process's locks
        // on it, also the database's own).
        let id = file.metadata().ok().map(|meta| (meta.dev(), meta.ino()));
        drop(file);
        let conn = match kourier_db::open(&path, &key.0) {
            Ok(conn) => conn,
            Err(_) => {
                remove_own(dir, id);
                return Err(StageError::Db("opening"));
            }
        };
        let writer = Self {
            conn,
            dir: dir.to_owned(),
            id,
            limits,
            bytes: 0,
            pending: 0,
            in_tx: false,
            contacts: 0,
            chats: 0,
            messages: 0,
        };
        if let Err(e) = writer.create_tables() {
            writer.abort();
            return Err(e);
        }
        Ok(writer)
    }

    fn create_tables(&self) -> Result<(), StageError> {
        self.conn
            .execute_batch(
                "CREATE TABLE meta(k TEXT PRIMARY KEY, v) WITHOUT ROWID;
             CREATE TABLE contacts(n INTEGER PRIMARY KEY, op BLOB NOT NULL);
             CREATE TABLE chats(
               idx INTEGER PRIMARY KEY,
               conv TEXT NOT NULL UNIQUE,
               op BLOB NOT NULL,
               pinned INTEGER,
               invited INTEGER NOT NULL DEFAULT 0,
               last_at INTEGER NOT NULL DEFAULT 0,
               n_rows INTEGER NOT NULL DEFAULT 0,
               n_loud INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE items(
               chat INTEGER NOT NULL,
               n INTEGER NOT NULL,
               bytes INTEGER NOT NULL,
               msg BLOB NOT NULL,
               PRIMARY KEY(chat, n)) WITHOUT ROWID;",
            )
            .map_err(db("creating its tables"))?;
        self.conn
            .execute("INSERT INTO meta(k, v) VALUES ('format', ?1)", [FORMAT])
            .map_err(db("creating its tables"))?;
        Ok(())
    }

    /// Counts `len` bytes of payload against the cap and opens the transaction they go into.
    fn account(&mut self, len: usize) -> Result<(), StageError> {
        let len = len as u64;
        if self.bytes.saturating_add(len) > self.limits.max_bytes {
            return Err(StageError::TooLarge);
        }
        if !self.in_tx {
            self.conn
                .execute_batch("BEGIN IMMEDIATE")
                .map_err(db("beginning a transaction"))?;
            self.in_tx = true;
        }
        self.bytes += len;
        self.pending += len;
        Ok(())
    }

    /// Commits the open transaction once it holds [`StageLimits::commit_every`] of payload.
    fn settle(&mut self) -> Result<(), StageError> {
        if self.pending >= self.limits.commit_every {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), StageError> {
        if std::mem::take(&mut self.in_tx) {
            self.conn
                .execute_batch("COMMIT")
                .map_err(db("committing rows"))?;
        }
        self.pending = 0;
        Ok(())
    }

    /// Stages a person as row `n` of `contacts`.
    pub(crate) fn contact(&mut self, n: u32, contact: &StagedContact) -> Result<(), StageError> {
        let op = Zeroizing::new(
            serde_json::to_vec(contact).map_err(|_| StageError::Db("encoding a contact"))?,
        );
        self.account(op.len())?;
        self.conn
            .prepare_cached("INSERT INTO contacts(n, op) VALUES (?1, ?2)")
            .and_then(|mut stmt| stmt.execute((n, op.as_slice())))
            .map_err(db("writing a contact"))?;
        self.contacts += 1;
        self.settle()
    }

    /// Stages a conversation as row `idx` of `chats`.
    pub(crate) fn chat(
        &mut self,
        idx: u32,
        conv: &ConvKey,
        chat: &StagedChat,
        pinned: Option<u32>,
    ) -> Result<(), StageError> {
        // A group's holds its master key.
        let op = Zeroizing::new(
            serde_json::to_vec(chat).map_err(|_| StageError::Db("encoding a chat"))?,
        );
        let conv = conv.encode();
        self.account(op.len() + conv.len())?;
        self.conn
            .prepare_cached(
                "INSERT INTO chats(idx, conv, op, pinned, invited) VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .and_then(|mut stmt| stmt.execute((idx, &conv, op.as_slice(), pinned, chat.invited())))
            .map_err(db("writing a chat"))?;
        self.chats += 1;
        self.settle()
    }

    /// Takes the pin off a staged chat (the pin list is bounded, `MapLimits::max_pins`); the
    /// chat stays, shown when it has a loud row.
    pub(crate) fn unpin(&mut self, idx: u32) -> Result<(), StageError> {
        if !self.in_tx {
            self.conn
                .execute_batch("BEGIN IMMEDIATE")
                .map_err(db("beginning a transaction"))?;
            self.in_tx = true;
        }
        self.conn
            .prepare_cached("UPDATE chats SET pinned = NULL WHERE idx = ?1")
            .and_then(|mut stmt| stmt.execute([idx]))
            .map_err(db("unpinning a chat"))?;
        Ok(())
    }

    /// Stages a message as row `n` of chat `chat`.
    pub(crate) fn item(
        &mut self,
        chat: u32,
        n: u64,
        msg: &IncomingMessage,
    ) -> Result<(), StageError> {
        let json = Zeroizing::new(
            serde_json::to_vec(msg).map_err(|_| StageError::Db("encoding a message"))?,
        );
        self.account(json.len())?;
        self.conn
            .prepare_cached("INSERT INTO items(chat, n, bytes, msg) VALUES (?1, ?2, ?3, ?4)")
            .and_then(|mut stmt| {
                stmt.execute((chat, to_i64(n), to_i64(json.len() as u64), json.as_slice()))
            })
            .map_err(db("writing a message"))?;
        self.messages += 1;
        self.settle()
    }

    /// The archive was authentic to its last byte: writes the chats' counters and the
    /// totals, commits, empties the WAL into the database file and closes it. `frames` is
    /// what the archive held. On an error the stage's files are removed.
    pub(crate) fn commit(
        mut self,
        counters: &[ChatCounters],
        frames: u64,
    ) -> Result<StageTotals, StageError> {
        let outcome = self.finish(counters, frames);
        if outcome.is_err() {
            self.abort();
            return outcome;
        }
        let Self { conn, dir, id, .. } = self;
        if let Err((conn, _)) = conn.close() {
            // Still open: removed before it is dropped, as in `abort`.
            remove_own(&dir, id);
            drop(conn);
            return Err(StageError::Db("closing"));
        }
        outcome
    }

    fn finish(
        &mut self,
        counters: &[ChatCounters],
        frames: u64,
    ) -> Result<StageTotals, StageError> {
        if !self.in_tx {
            self.conn
                .execute_batch("BEGIN IMMEDIATE")
                .map_err(db("beginning a transaction"))?;
            self.in_tx = true;
        }
        {
            let mut stmt = self
                .conn
                .prepare_cached(
                    "UPDATE chats SET last_at = ?2, n_rows = ?3, n_loud = ?4 WHERE idx = ?1",
                )
                .map_err(db("writing the counters"))?;
            for chat in counters {
                let changed = stmt
                    .execute((
                        chat.idx,
                        chat.last_at,
                        to_i64(chat.n_rows),
                        to_i64(chat.n_loud),
                    ))
                    .map_err(db("writing the counters"))?;
                if changed != 1 {
                    return Err(StageError::Db("writing the counters"));
                }
            }
        }
        let shown: i64 = self
            .conn
            .query_row(
                &format!("SELECT count(*) FROM chats WHERE {SHOWN}"),
                [],
                |r| r.get(0),
            )
            .map_err(db("writing the counters"))?;
        let shown = to_u64(shown);
        let totals = StageTotals {
            messages: self.messages,
            chats: self.chats,
            shown_chats: shown,
            contacts: self.contacts,
            bytes: self.bytes,
        };
        {
            let mut stmt = self
                .conn
                .prepare_cached("INSERT INTO meta(k, v) VALUES (?1, ?2)")
                .map_err(db("writing the totals"))?;
            for (k, v) in [
                ("frames", frames),
                ("messages", totals.messages),
                ("chats", totals.chats),
                ("shown_chats", totals.shown_chats),
                ("contacts", totals.contacts),
                ("bytes", totals.bytes),
                // Last: a stage without it was never written to its end.
                ("complete", 1),
            ] {
                stmt.execute((k, to_i64(v)))
                    .map_err(db("writing the totals"))?;
            }
        }
        self.flush()?;
        // Everything into the database file: the stage is one file from here on.
        match kourier_db::try_checkpoint_truncate(&self.conn) {
            Ok(true) => Ok(totals),
            Ok(false) | Err(_) => Err(StageError::Db("its last checkpoint")),
        }
    }

    /// Nothing of this stage may stay: removes its files and closes the database.
    ///
    /// In that order. "The path still names the file I made" ([`remove_own`]) is only
    /// true of that one file while this writer's descriptor is open: a closed, unlinked
    /// file's inode is free, the file system gives it to the next file made, and a stage
    /// the next read made in between would pass for this one and be removed. With the
    /// descriptor open the inode cannot be anybody else's. SQLite, closing a database
    /// whose file is gone or replaced, removes neither the log nor the index by name, so
    /// the close after the removal touches nothing of a stage that took this one's place.
    pub(crate) fn abort(self) {
        let Self { conn, dir, id, .. } = self;
        remove_own(&dir, id);
        drop(conn);
    }
}

/// Removes the stage in `dir` when its database is still the file `id` names: the one a
/// writer made. A read that was stopped and did not end in time can still be running when
/// the next read of the same account has removed its stage and made a new one under the
/// same name; that one is not this writer's to remove.
///
/// Call it while the writer's database is still open ([`StageWriter::abort`]): device and
/// inode name one file only as long as that file is held open. (After a failed open in
/// [`StageWriter::create`] nothing holds it; the file is then some microseconds old and
/// was never written to.) What stays is the instant between the look and the removal,
/// which no call of the system closes; a stage removed in it is read again from the
/// archive in the same run (its record finds no file and is taken back), like any stage
/// that is lost.
fn remove_own(dir: &Path, id: Option<(u64, u64)>) {
    // A test's step at the instant the file's identity is looked at.
    #[cfg(test)]
    tests::at_remove_own();
    if id.is_none() || file_id(&path_in(dir)) != id {
        tracing::debug!("the stage of an aborted import was replaced or removed already");
        return;
    }
    if let Err(e) = remove(dir) {
        tracing::warn!(error = %e, "removing the stage of an aborted import failed");
    }
}

/// Reads a stage that was written to its end. Blocking.
pub(crate) struct StageReader {
    conn: Connection,
}

impl StageReader {
    /// Opens the stage in `dir` read-only. Fails when there is none, when `key` is not the
    /// key it was written with, when its layout is another one, and when it was never
    /// committed.
    pub(crate) fn open(dir: &Path, key: &StageKey) -> Result<Self, StageError> {
        let path = path_in(dir);
        if !path.is_file() {
            return Err(StageError::Io(std::io::ErrorKind::NotFound));
        }
        let conn =
            kourier_db::open_read_only(&path, &key.0).map_err(|_| StageError::Db("opening"))?;
        let reader = Self { conn };
        if reader.meta("format")? != Some(FORMAT) || reader.meta("complete")? != Some(1) {
            return Err(StageError::Unreadable);
        }
        Ok(reader)
    }

    fn meta(&self, k: &str) -> Result<Option<i64>, StageError> {
        self.conn
            .query_row("SELECT v FROM meta WHERE k = ?1", [k], |r| r.get(0))
            .optional()
            .map_err(db("reading its totals"))
    }

    fn total(&self, k: &str) -> Result<u64, StageError> {
        self.meta(k)?.map(to_u64).ok_or(StageError::Unreadable)
    }

    /// The totals the writer recorded at its commit.
    pub(crate) fn totals(&self) -> Result<StageTotals, StageError> {
        Ok(StageTotals {
            messages: self.total("messages")?,
            chats: self.total("chats")?,
            shown_chats: self.total("shown_chats")?,
            contacts: self.total("contacts")?,
            bytes: self.total("bytes")?,
        })
    }

    /// The frames of the archive the stage was read from.
    pub(crate) fn frames(&self) -> Result<u64, StageError> {
        self.total("frames")
    }

    /// The totals counted from the rows themselves, after every page of the file was
    /// checked against its check value (SQLCipher's `cipher_integrity_check`: a row larger
    /// than a page continues on pages that counting alone never reads). A stage that
    /// passes this and whose counts equal its [`totals`](Self::totals) is whole. Delivery
    /// does not run it at a start (`deliver`, "What a start verifies"); tests do.
    #[cfg_attr(not(any(test, feature = "testing")), allow(dead_code))]
    pub(crate) fn count(&self) -> Result<StageTotals, StageError> {
        let damaged = self
            .conn
            .prepare("PRAGMA cipher_integrity_check")
            .and_then(|mut stmt| stmt.query([])?.next().map(|row| row.is_some()))
            .map_err(db("checking its pages"))?;
        if damaged {
            return Err(StageError::Db("checking its pages"));
        }
        let one = |sql: &str| -> Result<u64, StageError> {
            self.conn
                .query_row(sql, [], |r| r.get::<_, i64>(0))
                .map(to_u64)
                .map_err(db("counting its rows"))
        };
        let (messages, item_bytes): (i64, i64) = self
            .conn
            .query_row(
                "SELECT count(*), coalesce(sum(length(msg)), 0) FROM items",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(db("counting its rows"))?;
        let other_bytes = one("SELECT coalesce(sum(length(op)), 0) FROM contacts")?
            + one("SELECT coalesce(sum(length(op) + length(CAST(conv AS BLOB))), 0) FROM chats")?;
        Ok(StageTotals {
            messages: to_u64(messages),
            chats: one("SELECT count(*) FROM chats")?,
            shown_chats: one(&format!("SELECT count(*) FROM chats WHERE {SHOWN}"))?,
            contacts: one("SELECT count(*) FROM contacts")?,
            bytes: to_u64(item_bytes) + other_bytes,
        })
    }

    /// Up to `limit` staged people with `n` above `after`, ascending.
    pub(crate) fn contacts(
        &self,
        after: Option<u32>,
        limit: u32,
    ) -> Result<Vec<(u32, StagedContact)>, StageError> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT n, op FROM contacts WHERE n > ?1 ORDER BY n LIMIT ?2")
            .map_err(db("reading contacts"))?;
        let rows = stmt
            .query_map((after.map_or(-1, i64::from), limit), |r| {
                Ok((r.get::<_, u32>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .map_err(db("reading contacts"))?;
        let mut out = Vec::new();
        for row in rows {
            let (n, op) = row.map_err(db("reading contacts"))?;
            let op = Zeroizing::new(op);
            out.push((
                n,
                serde_json::from_slice(&op).map_err(|_| StageError::Unreadable)?,
            ));
        }
        Ok(out)
    }

    /// Every staged chat, the most recent first (`last_at` descending, then `idx`): the
    /// order delivery shows them in. A chat without a loud row is in the list too; its
    /// counters say so.
    pub(crate) fn chats(&self) -> Result<Vec<StagedChatRow>, StageError> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT idx, conv, op, pinned, last_at, n_rows, n_loud FROM chats
                  ORDER BY last_at DESC, idx",
            )
            .map_err(db("reading chats"))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, u32>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, Option<u32>>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                ))
            })
            .map_err(db("reading chats"))?;
        let mut out = Vec::new();
        for row in rows {
            let (idx, conv, op, pinned, last_at, n_rows, n_loud) =
                row.map_err(db("reading chats"))?;
            let op = Zeroizing::new(op);
            out.push(StagedChatRow {
                conv: ConvKey::parse(&conv).map_err(|_| StageError::Unreadable)?,
                op: serde_json::from_slice(&op).map_err(|_| StageError::Unreadable)?,
                pinned,
                counters: ChatCounters {
                    idx,
                    n_rows: to_u64(n_rows),
                    n_loud: to_u64(n_loud),
                    last_at,
                },
            });
        }
        Ok(out)
    }

    /// The next rows of chat `chat` after row `after` (`None`: from its first), in frame
    /// order: at most `max_rows`, and no more than fit into `max_bytes` of staged JSON,
    /// except that the first row is given whatever its size. An empty answer: the chat has
    /// no row after `after`.
    pub(crate) fn items(
        &self,
        chat: u32,
        after: Option<u64>,
        max_rows: usize,
        max_bytes: u64,
    ) -> Result<Vec<StagedItem>, StageError> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT n, bytes, msg FROM items WHERE chat = ?1 AND n > ?2 ORDER BY n LIMIT ?3",
            )
            .map_err(db("reading messages"))?;
        let mut rows = stmt
            .query((
                chat,
                after.map_or(-1, to_i64),
                i64::try_from(max_rows).unwrap_or(i64::MAX),
            ))
            .map_err(db("reading messages"))?;
        let mut out = Vec::new();
        let mut taken = 0u64;
        while let Some(row) = rows.next().map_err(db("reading messages"))? {
            let bytes = to_u64(row.get::<_, i64>(1).map_err(db("reading messages"))?);
            if !out.is_empty() && taken.saturating_add(bytes) > max_bytes {
                break;
            }
            let json = Zeroizing::new(row.get::<_, Vec<u8>>(2).map_err(db("reading messages"))?);
            out.push(StagedItem {
                n: to_u64(row.get::<_, i64>(0).map_err(db("reading messages"))?),
                bytes,
                msg: serde_json::from_slice(&json).map_err(|_| StageError::Unreadable)?,
            });
            taken += bytes;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests;

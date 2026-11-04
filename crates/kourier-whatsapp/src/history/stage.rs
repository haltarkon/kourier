//! The history stage (W4 C3): the file where a captured chunk waits until its messages are delivered.
//!
//! WhatsApp pushes history as many chunks over minutes or hours, and upstream answers the phone only
//! after the capture hook returned Ok, so a chunk must be DURABLE before Ok: this file is where. It
//! is the only copy (the phone was told the chunk arrived; upstream keeps no replay buffer), which
//! sets every rule below:
//!
//! - ITS OWN FILE, `accounts/<uuid>/history/stage.db` (directory mode 0700), with its own writer
//!   thread. `backend.db`'s writer is the one serial lane of every store trait the receive path waits
//!   for, and a 1-20 MB blob insert there would stall live receiving; and `secure_delete` would make
//!   removing a delivered chunk rewrite its pages. The stage is removed whole instead.
//! - ITS OWN KEY: HKDF-SHA256 of the account's `backend.db` key (no salt, a fixed info string). No new
//!   secret is stored, and a stage never opens with the wrong account's key.
//! - ITS OWN SCHEMA COMPONENT, `whatsapp-history-stage`, migrated through `kourier_db::migrate`. A
//!   stage is MIGRATED for a version and never removed for one: a file of a newer layout is left alone
//!   and reported ([`OpenError::TooNew`]), so a downgrade costs nothing.
//! - REMOVED only on purpose: at a logout or a removal ([`LazyStage::remove`]), or when the file is
//!   of another owner. A stage of another owner is replaced and the chunks it had not delivered are
//!   COUNTED lost ([`Origin::Foreign`]). One that cannot be opened with our key ([`Origin::Unreadable`]) is
//!   NEVER deleted (W4 fix MEDIUM-3): the key is derived from the account's own, so it can only be a
//!   damaged file, and it may hold the only copy of chunks the phone was told arrived. Its three files are
//!   renamed aside (`stage.db.unreadable-<ms>` and its `-wal`/`-shm`), a new stage starts, and the chunks the
//!   books know were not delivered are counted lost; the aside copy goes with the directory at a logout or a
//!   removal.
//! - AN INCARNATION: every file gets 16 random bytes in `meta` when it is made, and the delivery's books in
//!   `backend.db` record which one they point into ([`super::retention::adopt`], W4 fix HIGH-1). Whoever opens
//!   the file, a stage with another id starts the books over before anything is delivered from it.
//! - OPENED WHEN FIRST NEEDED ([`LazyStage`]): a first capture creates it, a start with a stage on disk
//!   resumes it, and an account that never gets a push never makes a file or a thread.
//!
//! Nothing here logs or returns a message id, a JID or a byte of a chunk: errors name an operation.

use std::fs;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hkdf::Hkdf;
use kourier_db::rusqlite::{Connection, OptionalExtension as _, Transaction};
use kourier_db::{DbKey, Migrations, Priority, Writer, WriterHandle, migration};
use sha2::Sha256;
use wacore::store::error::StoreError;
use zeroize::Zeroizing;

use crate::db::{self, Points};
use crate::error::{self, OpenError};

/// The directory inside the account's directory.
pub(crate) const DIR: &str = "history";
/// The stage's file name inside [`DIR`].
pub(crate) const FILE: &str = "stage.db";

/// The HKDF info string of the stage key (a new layout of the KEY is a new string, never an edit).
const KEY_INFO: &[u8] = b"kourier whatsapp history stage 1";

/// The one lane of the stage's writer.
const LANE: u64 = 0;

/// How many prepared statements the connection keeps.
const STATEMENTS: usize = 64;

/// The stage's schema component. Its numbers are its own: they never meet `backend.db`'s.
pub(crate) const MIGRATIONS: Migrations = Migrations::new(
    "whatsapp-history-stage",
    &[migration!(1, "stage/0001_stage")],
);

/// The stage key of the account whose `backend.db` key is `backend`. Wiped on drop.
pub(crate) fn derive_key(backend: &DbKey) -> DbKey {
    let mut key = Zeroizing::new([0u8; 32]);
    // 32 bytes are a valid HKDF-SHA256 output length: `expand` cannot fail.
    let _ = Hkdf::<Sha256>::new(None, backend.as_bytes()).expand(KEY_INFO, key.as_mut());
    DbKey::from_bytes(*key)
}

/// How a stage came to be, at the moment it was opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    /// No file was there: a new, empty stage was made.
    Created,
    /// A stage of this owner was there and is used as it is (after its pending migrations).
    Resumed,
    /// A stage of ANOTHER owner was there: it was replaced by an empty one and `lost` chunks that had
    /// been captured and not delivered went with it (counted, never dropped silently).
    Foreign { lost: u64 },
    /// A file was there that our key does not open: it was set aside (never deleted) and an empty stage was
    /// made. What it held is beyond reach, so only the books can estimate what it lost.
    Unreadable,
}

/// The files of a stage: the database and the two SQLite keeps beside it.
fn files(path: &Path) -> [PathBuf; 3] {
    let beside = |suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    [path.to_owned(), beside("-wal"), beside("-shm")]
}

fn remove_files(path: &Path) {
    for file in files(path) {
        // A file that is not there is the goal already.
        let _ = fs::remove_file(file);
    }
}

/// Sets the three files of a stage that does not open aside, as `<file>.unreadable-<ms>` with the suffix the file
/// kept (`-wal`, `-shm`) after it, so a later look with the right tool finds a database and its journal together.
/// Nothing is deleted; a file that cannot be renamed is an error (the stage is not replaced then).
fn set_aside(path: &Path) -> Result<(), OpenError> {
    let stamp = format!(".unreadable-{}", kourier_db::now_ms());
    let aside = |suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(&stamp);
        name.push(suffix);
        PathBuf::from(name)
    };
    for (from, to) in files(path)
        .into_iter()
        .zip([aside(""), aside("-wal"), aside("-shm")])
    {
        match fs::rename(&from, &to) {
            Ok(()) => {}
            // The journal files are often not there.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(OpenError::Failed(
                    "the unreadable stage could not be set aside".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

/// A handle for the stage's writer: any number of clones, cheap. The jobs run on the stage's own
/// thread, one after the other, each in one transaction.
#[derive(Clone)]
pub(crate) struct StageHandle {
    handle: WriterHandle,
    path: PathBuf,
    key: DbKey,
}

impl StageHandle {
    /// A job that writes: `f` runs in one `BEGIN IMMEDIATE` transaction, with the kill points around
    /// its commit when `points` is given.
    pub(crate) async fn write_at<T, F>(
        &self,
        priority: Priority,
        op: &'static str,
        points: Option<Points>,
        f: F,
    ) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> Result<T, StoreError> + Send + 'static,
    {
        let job = move |conn: &mut Connection| db::zone(conn, op, points, f);
        match self.handle.submit(priority, LANE, job) {
            Err(_) => Err(error::closed()),
            Ok(answer) => answer.await.unwrap_or_else(|_| Err(error::job_failed())),
        }
    }

    /// [`write_at`](Self::write_at) for a thread that is no task (the feeder): waits for the answer.
    pub(crate) fn write_blocking<T, F>(
        &self,
        priority: Priority,
        op: &'static str,
        points: Option<Points>,
        f: F,
    ) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> Result<T, StoreError> + Send + 'static,
    {
        let job = move |conn: &mut Connection| db::zone(conn, op, points, f);
        match self.handle.call_blocking(priority, LANE, job) {
            Err(_) => Err(error::closed()),
            Ok(result) => result,
        }
    }

    /// A second, read-only connection to the stage file with the stage key, for the thread that
    /// inflates and reads a chunk (a chunk is 1-20 MB: it does not pass through the writer's queue).
    /// WAL lets it read while the writer commits.
    pub(crate) fn reader(&self) -> Result<Connection, OpenError> {
        Ok(kourier_db::open_read_only(&self.path, &self.key)?)
    }
}

/// Owns the stage's writer thread. Dropping it finishes the queued jobs and joins the thread.
pub(crate) struct StageOwner {
    writer: Writer,
    handle: StageHandle,
    incarnation: [u8; 16],
}

impl StageOwner {
    /// Opens the stage at `path` (made if missing, its directory with mode 0700) for `owner` with
    /// `key`, migrates it with `migrations`, and says how it came to be. See the module docs for what
    /// is replaced and what never is.
    pub(crate) fn open(
        path: &Path,
        key: &DbKey,
        owner: &str,
        migrations: &Migrations,
    ) -> Result<(Self, Origin), OpenError> {
        if let Some(dir) = path.parent() {
            let new_dir = !dir.exists();
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|_| {
                    OpenError::Failed("the stage directory could not be made".to_owned())
                })?;
            // The directory's own entry is metadata nobody has synced: SQLite fsyncs the WAL and the file, not the
            // entry of their directory in ITS parent, and a power loss could take the directory (and the first
            // chunks, already receipted) with it (LOW-3). Done before the first capture can return Ok.
            if new_dir && let Some(parent) = dir.parent() {
                fs::File::open(parent)
                    .and_then(|parent| parent.sync_all())
                    .map_err(|_| {
                        OpenError::Failed("the stage directory could not be synced".to_owned())
                    })?;
            }
        }
        let existed = path.exists();
        let (conn, origin) = match open_checked(path, key, owner, migrations)? {
            Checked::Ready(conn, origin) => (conn, origin),
            Checked::Replace(origin) => {
                match origin {
                    // A file that does not open is kept, never deleted (MEDIUM-3).
                    Origin::Unreadable => set_aside(path)?,
                    _ => remove_files(path),
                }
                match open_checked(path, key, owner, migrations)? {
                    Checked::Ready(conn, _) => (conn, origin),
                    // A file we just made is of our owner and opens with our key.
                    Checked::Replace(_) => {
                        return Err(OpenError::Failed("the stage could not be made".to_owned()));
                    }
                }
            }
        };
        let origin = match origin {
            Origin::Created if existed => Origin::Resumed,
            other => other,
        };
        conn.set_prepared_statement_cache_capacity(STATEMENTS);
        let incarnation = incarnation_of(&conn)?;
        let writer = Writer::spawn("wa-history-stage", conn)?;
        let handle = StageHandle {
            handle: writer.handle(),
            path: path.to_owned(),
            key: key.clone(),
        };
        Ok((
            Self {
                writer,
                handle,
                incarnation,
            },
            origin,
        ))
    }

    /// The incarnation of the file ([`super::retention::adopt`]).
    pub(crate) fn incarnation(&self) -> [u8; 16] {
        self.incarnation
    }

    /// A handle for the capture and the feeder. Any number of clones.
    pub(crate) fn handle(&self) -> StageHandle {
        self.handle.clone()
    }

    /// Finishes the jobs already queued and stops the thread. Every later call of any handle fails.
    pub(crate) fn close(self) {
        drop(self.writer.shutdown());
    }
}

/// The incarnation id of an open stage (16 bytes made with the file).
fn incarnation_of(conn: &Connection) -> Result<[u8; 16], OpenError> {
    let blob: Vec<u8> = conn
        .query_row("SELECT incarnation FROM meta WHERE id = 1", [], |r| {
            r.get(0)
        })
        .map_err(|_| OpenError::Failed("the stage incarnation could not be read".to_owned()))?;
    <[u8; 16]>::try_from(blob.as_slice())
        .map_err(|_| OpenError::Failed("the stage incarnation is not 16 bytes".to_owned()))
}

/// What looking at an existing file found.
enum Checked {
    /// Open, migrated, owned by us (or new).
    Ready(Connection, Origin),
    /// The file must go and be made anew, for this reason.
    Replace(Origin),
}

/// Opens `path`, migrates it and checks its owner. Never removes anything: it says what must go.
fn open_checked(
    path: &Path,
    key: &DbKey,
    owner: &str,
    migrations: &Migrations,
) -> Result<Checked, OpenError> {
    let mut conn = match kourier_db::open(path, key) {
        Ok(conn) => conn,
        // Not a database of this key: the content is out of reach for good.
        Err(kourier_db::Error::WrongKey) => return Ok(Checked::Replace(Origin::Unreadable)),
        Err(e) => return Err(e.into()),
    };
    // A file of a NEWER layout stops here with `TooNew` and is left as it is; an older one is
    // brought forward with its chunks.
    kourier_db::migrate(&mut conn, migrations)?;
    let found: Option<String> = conn
        .query_row("SELECT owner FROM meta WHERE id = 1", [], |r| r.get(0))
        .optional()
        .map_err(|_| OpenError::Failed("the stage owner could not be read".to_owned()))?;
    match found {
        None => {
            conn.execute(
                "INSERT INTO meta (id, owner, created_at, incarnation) \
                 VALUES (1, ?1, ?2, randomblob(16))",
                (owner, kourier_db::now_ms()),
            )
            .map_err(|_| OpenError::Failed("the stage owner could not be set".to_owned()))?;
            Ok(Checked::Ready(conn, Origin::Created))
        }
        Some(found) if found == owner => Ok(Checked::Ready(conn, Origin::Resumed)),
        Some(_) => {
            let lost: i64 = conn
                .query_row(
                    "SELECT count(*) FROM chunks WHERE state IN ('captured', 'planned')",
                    [],
                    |r| r.get(0),
                )
                .map_err(|_| OpenError::Failed("the stage could not be counted".to_owned()))?;
            drop(conn);
            Ok(Checked::Replace(Origin::Foreign {
                lost: u64::try_from(lost).unwrap_or(0),
            }))
        }
    }
}

/// The stage of one account until it is needed (module docs, "opened when first needed").
pub(crate) struct LazyStage {
    path: PathBuf,
    key: DbKey,
    slot: Mutex<Slot>,
    /// How the stage came to be, until the actor takes it.
    origin: Mutex<Option<Origin>>,
    /// The incarnation of the file this run opened (set with the handle, kept after the origin is taken).
    incarnation: Mutex<Option<[u8; 16]>>,
}

enum Slot {
    /// Nothing opened yet.
    Idle,
    Open(StageOwner),
    /// Closed for good (the account's end): every later request fails.
    Closed,
}

impl LazyStage {
    /// The stage of the account whose `backend.db` is at `backend_db` and has `backend_key`. Opens
    /// nothing and touches no file.
    pub(crate) fn new(backend_db: &Path, backend_key: &DbKey) -> Self {
        let dir = backend_db.parent().unwrap_or_else(|| Path::new("."));
        Self {
            path: dir.join(DIR).join(FILE),
            key: derive_key(backend_key),
            slot: Mutex::new(Slot::Idle),
            origin: Mutex::new(None),
            incarnation: Mutex::new(None),
        }
    }

    /// Whether a stage file is on disk: a start with one resumes its delivery.
    pub(crate) fn on_disk(&self) -> bool {
        self.path.exists()
    }

    /// The handle of the stage for `owner`, opening it first when this is the first need. BLOCKS for
    /// the open and the migration: a task calls it from a blocking context.
    pub(crate) fn handle(&self, owner: &str) -> Result<StageHandle, OpenError> {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        match &*slot {
            Slot::Open(stage) => return Ok(stage.handle()),
            Slot::Closed => return Err(OpenError::Failed("the stage is closed".to_owned())),
            Slot::Idle => {}
        }
        let (stage, origin) = StageOwner::open(&self.path, &self.key, owner, &MIGRATIONS)?;
        let handle = stage.handle();
        *self.incarnation.lock().unwrap_or_else(|e| e.into_inner()) = Some(stage.incarnation());
        *slot = Slot::Open(stage);
        *self.origin.lock().unwrap_or_else(|e| e.into_inner()) = Some(origin);
        Ok(handle)
    }

    /// The incarnation of the stage this run opened: `None` before it was opened.
    pub(crate) fn incarnation(&self) -> Option<[u8; 16]> {
        *self.incarnation.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How the stage came to be, once: `None` when it was not opened yet or the answer was taken.
    pub(crate) fn take_origin(&self) -> Option<Origin> {
        self.origin.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// [`take_origin`](Self::take_origin) without taking it: the driver acts on a replaced stage first and takes the
    /// answer only when that worked, so a failed attempt is made again.
    pub(crate) fn origin(&self) -> Option<Origin> {
        *self.origin.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Finishes the queued jobs and stops the thread; the file stays. Later requests fail.
    pub(crate) fn close(&self) {
        let slot = std::mem::replace(
            &mut *self.slot.lock().unwrap_or_else(|e| e.into_inner()),
            Slot::Closed,
        );
        if let Slot::Open(stage) = slot {
            stage.close();
        }
    }

    /// A fresh link starts with no stage: deletes the directory a logout that never finished removing left behind.
    /// Does nothing once the stage was opened or closed (the actor calls it before anything can open it); unlike
    /// [`remove`](Self::remove) it leaves the stage usable.
    pub(crate) fn clear_leftover(&self) {
        let slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*slot, Slot::Idle)
            && let Some(dir) = self.path.parent()
        {
            // A directory that is not there is the goal already.
            let _ = fs::remove_dir_all(dir);
        }
    }

    /// [`close`](Self::close), then deletes the stage's directory: a logout or a removal (the actor's exit, W4 C18).
    pub(crate) fn remove(&self) {
        self.close();
        if let Some(dir) = self.path.parent() {
            // A directory that is not there is the goal already.
            let _ = fs::remove_dir_all(dir);
        }
    }
}

#[cfg(test)]
mod tests;

//! The per-account blob directory: `accounts/<uuid>/blobs/<2 hex>/<random id>` (§5.1).

use crate::{BlobError, BlobReader, BlobWriter, FileKey};
use rand::RngCore;
use std::fmt;
use std::fs::{DirBuilder, File};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// A random blob id: 32 lowercase hex digits. Ids name files, never content.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlobId(String);

impl BlobId {
    /// A fresh random id (128 bits).
    pub fn new_random() -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut bytes);
        let mut s = String::with_capacity(32);
        for b in bytes {
            s.push(char::from(HEX[usize::from(b >> 4)]));
            s.push(char::from(HEX[usize::from(b & 0x0f)]));
        }
        Self(s)
    }

    /// The id as stored.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `<2 hex>/<id>`, relative to the store root.
    pub fn relative_path(&self) -> PathBuf {
        Path::new(&self.0[..2]).join(&self.0)
    }

    /// Parses a [`relative_path`](Self::relative_path) (`<2 hex>/<id>` with a matching shard);
    /// `None` for anything else.
    pub fn from_relative(path: &Path) -> Option<Self> {
        let mut parts = path.components();
        let (Some(shard), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
            return None;
        };
        let id: BlobId = name.as_os_str().to_str()?.parse().ok()?;
        (shard.as_os_str().to_str()? == &id.0[..2]).then_some(id)
    }
}

impl FromStr for BlobId {
    type Err = BlobError;

    fn from_str(s: &str) -> Result<Self, BlobError> {
        let ok = s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        ok.then(|| Self(s.to_owned())).ok_or(BlobError::NotABlob)
    }
}

impl fmt::Display for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlobId({})", self.0)
    }
}

/// One account's blob directory (0700, sharded by the first two hex digits of the id).
#[derive(Clone, Debug)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// Opens (creating, 0700) the store at `root`.
    pub fn open(root: &Path) -> io::Result<Self> {
        create_private_dir(root)?;
        Ok(Self {
            root: root.to_owned(),
        })
    }

    /// The store root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where blob `id` lives.
    pub fn path(&self, id: &BlobId) -> PathBuf {
        self.root.join(id.relative_path())
    }

    /// Starts writing blob `id` (atomic: it appears only on [`BlobWriter::finish`]).
    pub fn create(&self, id: &BlobId, key: &FileKey) -> Result<BlobWriter, BlobError> {
        let path = self.path(id);
        if let Some(shard) = path.parent() {
            create_private_dir(shard)?;
        }
        BlobWriter::create(&path, key)
    }

    /// Creates the shard directory of blob `id` (0700) and returns where the blob goes: a
    /// destination handed to another writer (a backend's download, `BlobTarget`).
    pub fn prepare(&self, id: &BlobId) -> io::Result<PathBuf> {
        let path = self.path(id);
        if let Some(shard) = path.parent() {
            create_private_dir(shard)?;
        }
        Ok(path)
    }

    /// Encrypts everything `src` yields into a new blob with a fresh id and key (atomic: an error
    /// leaves nothing behind). Returns the id, the key and the plaintext length.
    pub fn import(&self, src: &mut impl io::Read) -> Result<(BlobId, FileKey, u64), BlobError> {
        let (id, key) = (BlobId::new_random(), FileKey::generate());
        let mut w = self.create(&id, &key)?;
        io::copy(src, &mut w).map_err(BlobError::from_io)?;
        let len = w.finish()?;
        Ok((id, key, len))
    }

    /// [`import`](Self::import) without fsyncs ([`BlobWriter::finish_volatile`]): for a blob
    /// that can be made again from durable data.
    pub fn import_volatile(
        &self,
        src: &mut impl io::Read,
    ) -> Result<(BlobId, FileKey, u64), BlobError> {
        let (id, key) = (BlobId::new_random(), FileKey::generate());
        let mut w = self.create(&id, &key)?;
        io::copy(src, &mut w).map_err(BlobError::from_io)?;
        let len = w.finish_volatile()?;
        Ok((id, key, len))
    }

    /// Every complete blob in the store (`*.tmp` leftovers and foreign files are skipped).
    pub fn list(&self) -> io::Result<Vec<BlobId>> {
        let mut out = Vec::new();
        for shard in std::fs::read_dir(&self.root)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(shard.path())? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let rel = Path::new(&shard.file_name()).join(entry.file_name());
                if let Some(id) = BlobId::from_relative(&rel) {
                    out.push(id);
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Opens blob `id` for streaming decryption.
    pub fn open_blob(&self, id: &BlobId, key: &FileKey) -> Result<BlobReader<File>, BlobError> {
        BlobReader::open(&self.path(id), key)
    }

    /// Deletes blob `id` (after the transaction that dropped its row committed). A missing file
    /// is not an error.
    pub fn remove(&self, id: &BlobId) -> io::Result<()> {
        match std::fs::remove_file(self.path(id)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// Removes `*.tmp` leftovers of writes a crash interrupted; returns how many. Run at startup.
    pub fn sweep_tmp(&self) -> io::Result<usize> {
        let mut removed = 0;
        for shard in std::fs::read_dir(&self.root)? {
            let shard = shard?.path();
            if !shard.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(&shard)? {
                let path = entry?.path();
                if path.extension().is_some_and(|e| e == "tmp") {
                    std::fs::remove_file(&path)?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

/// Creates `dir` and missing parents with mode 0700, and tightens an existing `dir` to 0700.
pub(crate) fn create_private_dir(dir: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let mut perms = std::fs::metadata(dir)?.permissions();
    if perms.mode() & 0o777 != 0o700 {
        perms.set_mode(0o700);
        std::fs::set_permissions(dir, perms)?;
    }
    Ok(())
}

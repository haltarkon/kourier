//! Contact avatars in the account's blob directory (§5.8, §6.10, §6.11): profile avatars
//! (CDN0, decrypted with the profile key) and contact-sync avatars (inline in the sync blob).
//!
//! An avatar is written as a new `kourier-blob` file *before* the zone that hands it to the core
//! (`ContactUpsert{avatar}`); the zone records it in `peers` and deletes the file it replaced
//! only after the commit. A crash in between leaves at most an unreferenced file, never a core
//! row whose file is missing or under another key.

use std::path::{Path, PathBuf};

use kourier_blob::{BlobId, BlobStore, FileKey};
use kourier_db::Tx;
use kourier_db::rusqlite::OptionalExtension;
use kourier_model::{BlobKey, BlobRef, Patch};

use crate::error::{Error, Result};
use crate::peers;

/// Largest avatar accepted (Desktop's profile avatars are re-encoded far below this).
pub(crate) const MAX_AVATAR_LEN: u64 = 10 * 1024 * 1024;

/// Where an avatar came from (`peers.avatar_source`). A contact has one avatar here: the profile
/// picture wins over the address-book photo, or the other way round when our phone's setting
/// says so (`AccountRecord.preferContactAvatars`, [`crate::storage::prefer_contact_avatars`]);
/// when the setting changes, the source that now wins is fetched again (`storage`
/// `merge_account`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// The user's own profile avatar.
    Profile,
    /// The phone's address-book photo from contact sync.
    ContactSync,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::ContactSync => "contact_sync",
        }
    }
}

/// A written avatar file the core does not know yet.
pub(crate) struct Written {
    /// What the core stores.
    pub(crate) blob: BlobRef,
}

impl std::fmt::Debug for Written {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Written(<avatar>)")
    }
}

/// Encrypts `image` into a new blob below `blob_dir` (atomic, 0600).
pub(crate) fn write(blob_dir: &Path, image: &[u8]) -> Result<Written> {
    let store = BlobStore::open(blob_dir).map_err(|_| Error::Internal("blob directory"))?;
    let id = BlobId::new_random();
    let key = FileKey::generate();
    let mut writer = store
        .create(&id, &key)
        .map_err(|_| Error::Internal("creating an avatar file"))?;
    std::io::Write::write_all(&mut writer, image)
        .map_err(|_| Error::Internal("writing an avatar file"))?;
    writer
        .finish()
        .map_err(|_| Error::Internal("writing an avatar file"))?;
    Ok(Written {
        blob: BlobRef {
            path: id.relative_path(),
            key: BlobKey::new(*key.as_bytes()),
        },
    })
}

/// Removes an avatar file that no zone recorded (its zone failed).
pub(crate) fn discard(blob_dir: &Path, written: &Written) {
    let _ = std::fs::remove_file(blob_dir.join(&written.blob.path));
}

/// The avatar `peers` records for `service_id`: (blob path, source).
pub(crate) fn current(tx: &Tx<'_>, service_id: &str) -> Result<(Option<String>, Option<Source>)> {
    let row: Option<(Option<String>, Option<String>)> = tx
        .prepare_cached("SELECT avatar_blob, avatar_source FROM peers WHERE service_id = ?1")?
        .query_row([service_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let (blob, source) = row.unwrap_or((None, None));
    let source = match source.as_deref() {
        Some("profile") => Some(Source::Profile),
        Some("contact_sync") => Some(Source::ContactSync),
        _ => None,
    };
    Ok((blob, source))
}

/// A change of one contact's avatar.
pub(crate) struct Change<'a> {
    /// The new file, or `None` for "no avatar".
    pub(crate) written: Option<&'a Written>,
    /// Where it came from.
    pub(crate) source: Source,
    /// The CDN0 path of a profile avatar.
    pub(crate) profile_path: Option<&'a str>,
    /// The SHA-256 of a contact-sync avatar.
    pub(crate) hash: Option<&'a [u8]>,
}

/// Records `change` as the avatar of `service_id` in this zone and returns the
/// `ContactPatch.avatar` for the core. The file it replaces is deleted after the commit.
pub(crate) fn record(
    tx: &mut Tx<'_>,
    blob_dir: &Path,
    service_id: &str,
    change: &Change<'_>,
    now: i64,
) -> Result<Patch<BlobRef>> {
    let (old, _) = current(tx, service_id)?;
    peers::ensure(tx, service_id, now)?;
    let new_path = change
        .written
        .map(|w| w.blob.path.to_string_lossy().into_owned());
    tx.prepare_cached(
        "UPDATE peers SET avatar_blob = ?2, avatar_source = ?3, profile_avatar = ?4,
           avatar_hash = ?5, updated_at = ?6 WHERE service_id = ?1",
    )?
    .execute((
        service_id,
        &new_path,
        new_path.as_ref().map(|_| change.source.as_str()),
        change.profile_path,
        change.hash,
        now,
    ))?;
    if let Some(old) = old.filter(|old| Some(old) != new_path.as_ref()) {
        let file: PathBuf = blob_dir.join(old);
        tx.on_commit(move || {
            let _ = std::fs::remove_file(&file);
        });
    }
    Ok(match change.written {
        Some(written) => Patch::Set(written.blob.clone()),
        None => Patch::Clear,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile<'a>(written: Option<&'a Written>, path: Option<&'a str>) -> Change<'a> {
        Change {
            written,
            source: Source::Profile,
            profile_path: path,
            hash: None,
        }
    }

    #[test]
    fn avatars_replace_their_file_after_the_commit() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = dir.path().join("blobs");
        let db = crate::db::open(
            &dir.path().join("backend.db"),
            &kourier_backend_api::DbKey::new([2; 32]),
        )
        .unwrap();
        let first = write(&blobs, b"first image").unwrap();
        let first_path = blobs.join(&first.blob.path);
        let key = kourier_blob::FileKey::from_bytes(*first.blob.key.expose());
        assert_eq!(
            &kourier_blob::read_all(&first_path, &key).unwrap()[..],
            b"first image"
        );
        let patch = db
            .zone(|tx| record(tx, &blobs, "a", &profile(Some(&first), Some("p/1")), 1))
            .unwrap();
        assert_eq!(patch, Patch::Set(first.blob.clone()));
        let second = write(&blobs, b"second image").unwrap();
        // A failed zone keeps the old file and the new one can be discarded.
        let failed: Result<()> = db.zone(|tx| {
            record(tx, &blobs, "a", &profile(Some(&second), Some("p/2")), 2)?;
            Err(Error::Internal("abort"))
        });
        assert!(failed.is_err());
        assert!(first_path.exists(), "nothing is deleted before a commit");
        db.zone(|tx| record(tx, &blobs, "a", &profile(Some(&second), Some("p/2")), 3))
            .unwrap();
        assert!(
            !first_path.exists(),
            "the replaced file goes after the commit"
        );
        assert!(blobs.join(&second.blob.path).exists());
        let (path, source) = db.zone(|tx| current(tx, "a")).unwrap();
        assert_eq!(path.as_deref(), second.blob.path.to_str());
        assert_eq!(source, Some(Source::Profile));
        let cleared = db
            .zone(|tx| record(tx, &blobs, "a", &profile(None, None), 4))
            .unwrap();
        assert_eq!(cleared, Patch::Clear);
        assert!(!blobs.join(&second.blob.path).exists());
        assert_eq!(db.zone(|tx| current(tx, "a")).unwrap(), (None, None));
    }
}

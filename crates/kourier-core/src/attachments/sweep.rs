//! Startup sweep of an account's blob directory (§5.8 retention), before its actor starts:
//!
//! - `*.tmp` leftovers of interrupted writes anywhere in it (no writer runs yet);
//! - blobs in the core's own subtree ([`CORE_BLOBS`](super::CORE_BLOBS)) that no `core.db` row
//!   references. Their keys are gone, so they can never be read again: an import or a thumbnail
//!   a kill cut off before its transaction, a download target a backend finished after the core
//!   dropped its row, every core blob after `core.db` was rebuilt.
//!
//! Blobs outside the subtree belong to backends (avatars, recorded in their own databases) and
//! are never removed here.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use kourier_blob::BlobStore;
use kourier_db::rusqlite::Connection;
use kourier_model::AccountId;

use super::{core_path, core_store};
use crate::error::Result;

/// Every blob path (relative) `core.db` references for `account`: files rows and download
/// targets.
pub(crate) fn referenced(conn: &Connection, account: AccountId) -> Result<HashSet<PathBuf>> {
    let mut out = HashSet::new();
    for sql in [
        "SELECT path FROM files WHERE account_id = ?1",
        "SELECT path FROM attachment_downloads WHERE account_id = ?1",
    ] {
        let rows: Vec<String> = conn
            .prepare_cached(sql)?
            .query_map([account.0], |r| r.get(0))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        out.extend(rows.into_iter().map(PathBuf::from));
    }
    Ok(out)
}

/// Sweeps `blob_dir` (blocking; only while no actor of the account runs). Returns how many files
/// went.
pub(crate) fn sweep(blob_dir: &Path, referenced: &HashSet<PathBuf>) -> usize {
    if !blob_dir.is_dir() {
        return 0;
    }
    let tmp = |store: &BlobStore| {
        store.sweep_tmp().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "removing interrupted blob writes failed");
            0
        })
    };
    let mut removed = BlobStore::open(blob_dir).map_or(0, |s| tmp(&s));
    let store = match core_store(blob_dir) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "opening a blob directory for the sweep failed");
            return removed;
        }
    };
    removed += tmp(&store);
    let ids = match store.list() {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error = %e, "listing a blob directory failed");
            return removed;
        }
    };
    for id in ids {
        if !referenced.contains(&core_path(&id)) && store.remove(&id).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        tracing::info!(
            removed,
            "removed unreadable leftovers from a blob directory"
        );
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use kourier_blob::{BlobId, FileKey};
    use std::io::Write;

    #[test]
    fn unreferenced_core_blobs_and_leftovers_go_backend_blobs_stay() {
        let dir = tempfile::tempdir().expect("tempdir");
        let core = core_store(dir.path()).expect("core store");
        let key = FileKey::generate();
        let (kept, _, _) = core.import(&mut &b"kept"[..]).expect("blob");
        let (orphan, _, _) = core.import(&mut &b"orphan"[..]).expect("blob");
        let mut w = core.create(&BlobId::new_random(), &key).expect("tmp");
        w.write_all(b"cut off").expect("write");
        std::mem::forget(w);
        // A backend's avatar beside the core subtree, and its interrupted write.
        let backend = BlobStore::open(dir.path()).expect("store");
        let (avatar, _, _) = backend.import(&mut &b"avatar"[..]).expect("blob");
        let mut w = backend.create(&BlobId::new_random(), &key).expect("tmp");
        w.write_all(b"cut off").expect("write");
        std::mem::forget(w);
        let refs: HashSet<PathBuf> = [core_path(&kept)].into();

        assert_eq!(sweep(dir.path(), &refs), 3, "two .tmp files and the orphan");
        assert!(!core.path(&orphan).exists());
        assert!(core.path(&kept).exists());
        assert!(backend.path(&avatar).exists(), "backend blobs are not ours");
        assert_eq!(sweep(dir.path(), &refs), 0);
        assert_eq!(sweep(&dir.path().join("missing"), &refs), 0);
    }
}

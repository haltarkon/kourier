//! kourier-blob v1: round trips, tamper and truncation detection, atomic writes, the store and
//! the media cache (§5.8).

use kourier_blob::{
    BlobError, BlobId, BlobReader, BlobStore, BlobWriter, CHUNK_SIZE, FileKey, HEADER_LEN, MAGIC,
    MediaCache, read_all, write_all,
};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const TAG: usize = 16;

fn data(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn blob(dir: &Path, key: &FileKey, plain: &[u8]) -> PathBuf {
    let path = dir.join("b");
    write_all(&path, key, plain).unwrap();
    path
}

fn chunks(len: usize) -> usize {
    len.div_ceil(CHUNK_SIZE).max(1)
}

#[test]
fn round_trips_at_chunk_boundaries_with_odd_writes() {
    let dir = tempfile::tempdir().unwrap();
    let key = FileKey::generate();
    for len in [
        0,
        1,
        CHUNK_SIZE - 1,
        CHUNK_SIZE,
        CHUNK_SIZE + 1,
        3 * CHUNK_SIZE + 17,
    ] {
        let plain = data(len);
        let path = dir.path().join(format!("b{len}"));
        let mut w = BlobWriter::create(&path, &key).unwrap();
        for piece in plain.chunks(7919) {
            w.write_all(piece).unwrap();
        }
        assert!(!path.exists(), "invisible until finished");
        assert_eq!(w.finish().unwrap(), len as u64);

        let raw = std::fs::read(&path).unwrap();
        assert_eq!(&raw[..8], &MAGIC);
        assert_eq!(raw.len(), HEADER_LEN + len + chunks(len) * TAG, "len {len}");
        assert_eq!(mode(&path), 0o600);
        assert!(!path.with_file_name(format!("b{len}.tmp")).exists());

        // Streaming with a small buffer gives the same bytes.
        let mut r = BlobReader::open(&path, &key).unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 1000];
        loop {
            let n = r.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, plain, "len {len}");
        assert_eq!(*read_all(&path, &key).unwrap(), plain);
    }
}

#[test]
fn wrong_key_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let path = blob(dir.path(), &FileKey::generate(), &data(100));
    assert!(matches!(
        read_all(&path, &FileKey::generate()),
        Err(BlobError::Corrupt)
    ));
}

#[test]
fn every_tampered_byte_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let key = FileKey::generate();
    let plain = data(2 * CHUNK_SIZE + 100);
    let path = blob(dir.path(), &key, &plain);
    let original = std::fs::read(&path).unwrap();
    let last_tag = original.len() - 1;
    // Version, chunk size, nonce prefix, first chunk, chunk 2, last tag.
    for (offset, expected) in [
        (0, "magic"),
        (8, "version"),
        (9, "chunk size"),
        (13, "nonce"),
        (HEADER_LEN, "chunk"),
        (HEADER_LEN + CHUNK_SIZE + TAG + 5, "chunk"),
        (last_tag, "chunk"),
    ] {
        let mut bad = original.clone();
        bad[offset] ^= 0x01;
        std::fs::write(&path, &bad).unwrap();
        let err = read_all(&path, &key).unwrap_err();
        let ok = match expected {
            "magic" => matches!(err, BlobError::NotABlob),
            "version" => matches!(err, BlobError::UnsupportedVersion(_)),
            // A changed chunk size either is out of range or breaks the header authentication.
            "chunk size" => matches!(err, BlobError::BadHeader | BlobError::Corrupt),
            _ => matches!(err, BlobError::Corrupt),
        };
        assert!(ok, "offset {offset}: {err:?}");
    }
}

#[test]
fn reordered_or_extended_chunks_are_detected() {
    let dir = tempfile::tempdir().unwrap();
    let key = FileKey::generate();
    let path = blob(dir.path(), &key, &data(3 * CHUNK_SIZE));
    let original = std::fs::read(&path).unwrap();
    let ct = CHUNK_SIZE + TAG;

    let mut swapped = original.clone();
    let (a, b) = (HEADER_LEN, HEADER_LEN + ct);
    let first: Vec<u8> = swapped[a..a + ct].to_vec();
    swapped.copy_within(b..b + ct, a);
    swapped[b..b + ct].copy_from_slice(&first);
    std::fs::write(&path, &swapped).unwrap();
    assert!(matches!(read_all(&path, &key), Err(BlobError::Corrupt)));

    let mut extended = original;
    extended.extend_from_slice(b"appended");
    std::fs::write(&path, &extended).unwrap();
    assert!(matches!(read_all(&path, &key), Err(BlobError::Corrupt)));
}

#[test]
fn truncation_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let key = FileKey::generate();
    let path = blob(dir.path(), &key, &data(2 * CHUNK_SIZE + 10));
    let original = std::fs::read(&path).unwrap();
    let ct = CHUNK_SIZE + TAG;
    for (cut, boundary) in [
        (HEADER_LEN + 2 * ct, true), // the short last chunk is gone
        (HEADER_LEN + ct, true),     // two chunks gone
        (HEADER_LEN, true),          // header only
        (HEADER_LEN + ct + 100, false),
        (original.len() - 1, false),
        (12, true), // mid-header
    ] {
        std::fs::write(&path, &original[..cut]).unwrap();
        let err = read_all(&path, &key).unwrap_err();
        if boundary {
            assert!(matches!(err, BlobError::Truncated), "cut {cut}: {err:?}");
        } else {
            assert!(
                matches!(err, BlobError::Truncated | BlobError::Corrupt),
                "cut {cut}: {err:?}"
            );
        }
    }
}

#[test]
fn a_full_size_last_chunk_cut_after_is_truncation() {
    // Exactly two full chunks: the second is the last one. Drop it and the first chunk only
    // authenticates as "not last".
    let dir = tempfile::tempdir().unwrap();
    let key = FileKey::generate();
    let path = blob(dir.path(), &key, &data(2 * CHUNK_SIZE));
    let original = std::fs::read(&path).unwrap();
    std::fs::write(&path, &original[..HEADER_LEN + CHUNK_SIZE + TAG]).unwrap();
    assert!(matches!(read_all(&path, &key), Err(BlobError::Truncated)));
}

#[test]
fn unfinished_writer_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b");
    let mut w = BlobWriter::create(&path, &FileKey::generate()).unwrap();
    w.write_all(&data(3 * CHUNK_SIZE)).unwrap();
    drop(w);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn store_layout_permissions_sweep_and_remove() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("accounts/u/blobs");
    let store = BlobStore::open(&root).unwrap();
    assert_eq!(mode(&root), 0o700);

    let id = BlobId::new_random();
    assert_eq!(id.as_str().len(), 32);
    assert_eq!(id.as_str().parse::<BlobId>().unwrap(), id);
    assert!("../etc/passwd".parse::<BlobId>().is_err());

    let key = FileKey::generate();
    let mut w = store.create(&id, &key).unwrap();
    w.write_all(b"attachment").unwrap();
    w.finish().unwrap();
    let path = store.path(&id);
    assert_eq!(path, root.join(&id.as_str()[..2]).join(id.as_str()));
    assert_eq!(mode(path.parent().unwrap()), 0o700);
    let mut out = Vec::new();
    store
        .open_blob(&id, &key)
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    assert_eq!(out, b"attachment");

    // A crash mid-write leaves `<id>.tmp`; the startup sweep removes it.
    let crashed = BlobId::new_random();
    let mut w = store.create(&crashed, &key).unwrap();
    w.write_all(b"partial").unwrap();
    std::mem::forget(w);
    assert_eq!(store.sweep_tmp().unwrap(), 1);
    assert!(store.path(&id).exists(), "complete blobs stay");

    store.remove(&id).unwrap();
    assert!(!store.path(&id).exists());
    store.remove(&id).unwrap();
}

#[test]
fn store_import_prepare_list_and_relative_paths() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::open(&dir.path().join("blobs")).unwrap();
    let plain = data(3 * CHUNK_SIZE + 17);
    let (id, key, len) = store.import(&mut plain.as_slice()).unwrap();
    assert_eq!(len, plain.len() as u64);
    assert_eq!(*read_all(&store.path(&id), &key).unwrap(), plain);

    // A destination for another writer: the shard exists, the file does not yet.
    let target = BlobId::new_random();
    let path = store.prepare(&target).unwrap();
    assert!(path.parent().unwrap().is_dir() && !path.exists());
    assert_eq!(mode(path.parent().unwrap()), 0o700);
    write_all(&path, &key, b"downloaded").unwrap();

    // Leftovers and foreign files are not blobs.
    let mut w = store.create(&BlobId::new_random(), &key).unwrap();
    w.write_all(b"partial").unwrap();
    std::mem::forget(w);
    std::fs::write(store.root().join("README"), b"x").unwrap();
    std::fs::write(path.parent().unwrap().join("not-a-blob"), b"x").unwrap();
    let mut want = vec![id.clone(), target.clone()];
    want.sort();
    assert_eq!(store.list().unwrap(), want);

    assert_eq!(BlobId::from_relative(&id.relative_path()), Some(id.clone()));
    let wrong_shard = Path::new("zz").join(id.as_str());
    assert_eq!(BlobId::from_relative(&wrong_shard), None);
    assert_eq!(BlobId::from_relative(Path::new(id.as_str())), None);
    assert_eq!(
        BlobId::from_relative(&Path::new("..").join(id.relative_path())),
        None
    );
}

#[test]
fn media_cache_wipes_materializes_and_cleans_up() {
    let dir = tempfile::tempdir().unwrap();
    let media_dir = dir.path().join("run/kourier/media");
    std::fs::create_dir_all(&media_dir).unwrap();
    std::fs::write(media_dir.join("stale.mp4"), b"left by a crash").unwrap();

    let cache = MediaCache::open(&media_dir).unwrap();
    assert_eq!(
        std::fs::read_dir(&media_dir).unwrap().count(),
        0,
        "startup wipe"
    );
    assert_eq!(mode(&media_dir), 0o700);

    let key = FileKey::generate();
    let plain = data(CHUNK_SIZE + 5);
    let blob_path = blob(dir.path(), &key, &plain);
    let file = cache.materialize(&blob_path, &key, "gif").unwrap();
    assert_eq!(file.path().extension().unwrap(), "gif");
    assert_eq!(std::fs::read(file.path()).unwrap(), plain);
    assert_eq!(mode(file.path()), 0o600);
    let path = file.path().to_owned();
    drop(file);
    assert!(!path.exists(), "deleted when the view closes");

    // A tampered blob never leaves plaintext behind.
    let mut raw = std::fs::read(&blob_path).unwrap();
    let n = raw.len();
    raw[n - 3] ^= 1;
    std::fs::write(&blob_path, raw).unwrap();
    assert!(matches!(
        cache.materialize(&blob_path, &key, "gif"),
        Err(BlobError::Corrupt)
    ));
    assert_eq!(std::fs::read_dir(&media_dir).unwrap().count(), 0);
    assert!(cache.materialize(&blob_path, &key, "../x").is_err());

    let _kept = cache
        .materialize(&blob(dir.path(), &key, b"x"), &key, "")
        .unwrap();
    cache.clear().unwrap();
    assert_eq!(
        std::fs::read_dir(&media_dir).unwrap().count(),
        0,
        "exit wipe"
    );
}

#[test]
fn media_cache_reports_tmpfs() {
    let shm = Path::new("/dev/shm");
    if !shm.is_dir() {
        eprintln!("skipped: no /dev/shm");
        return;
    }
    let dir = tempfile::tempdir_in(shm).unwrap();
    assert!(
        MediaCache::open(&dir.path().join("media"))
            .unwrap()
            .is_tmpfs()
    );
}

/// A blob written without fsyncs is the same blob: it reads back, its file has the length
/// `sealed_len` states for its plaintext (at every chunk boundary), and a file cut short has
/// another length.
#[test]
fn a_volatile_blob_reads_back_and_has_its_sealed_length() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::open(dir.path()).unwrap();
    for len in [
        0,
        1,
        CHUNK_SIZE - 1,
        CHUNK_SIZE,
        CHUNK_SIZE + 1,
        3 * CHUNK_SIZE,
    ] {
        let plain = data(len);
        let (id, key, n) = store.import_volatile(&mut plain.as_slice()).unwrap();
        assert_eq!(n, len as u64);
        let path = store.path(&id);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(read_all(&path, &key).unwrap().as_slice(), plain.as_slice());
        let on_disk = std::fs::metadata(&path).unwrap().len();
        assert_eq!(on_disk, kourier_blob::sealed_len(len as u64), "{len}");
        assert_eq!(
            on_disk,
            (HEADER_LEN + len + TAG * chunks(len)) as u64,
            "{len}"
        );
        // The durable writer makes the same file length.
        let (id2, _, _) = store.import(&mut plain.as_slice()).unwrap();
        assert_eq!(std::fs::metadata(store.path(&id2)).unwrap().len(), on_disk);
    }
    // No temporary file stays behind.
    assert_eq!(store.sweep_tmp().unwrap(), 0);
    // An unfinished volatile write leaves nothing either.
    let key = FileKey::generate();
    let id = BlobId::new_random();
    let mut w = store.create(&id, &key).unwrap();
    w.write_all(b"cut off").unwrap();
    drop(w);
    assert!(!store.path(&id).exists());
}

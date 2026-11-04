//! kourier-blob v1: round trips, tamper and truncation detection, atomic writes, the store and
//! the media cache (§5.8).

use kourier_blob::{
    BlobError, BlobId, BlobReader, BlobStore, BlobWriter, CHUNK_SIZE, FileKey, HEADER_LEN, MAGIC,
    MediaCache, SaveError, SaveMode, Staging, read_all, save_copy, save_staged, save_to, write_all,
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

// ---------- save_copy / save_to: a plaintext copy in a folder of the user's (§5.8.2) ----------

const BOTH: [Staging; 2] = [Staging::Auto, Staging::Named];

/// The names in a folder.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn save(
    dir: &Path,
    name: &str,
    mode: SaveMode,
    staging: Staging,
    plain: &[u8],
) -> Result<u64, SaveError> {
    save_staged(dir, name, mode, staging, &mut &plain[..])
}

#[test]
fn a_copy_has_the_bytes_the_name_and_no_executable_bit() {
    let store = tempfile::tempdir().unwrap();
    let key = FileKey::generate();
    for staging in BOTH {
        for len in [0, 1, CHUNK_SIZE - 1, CHUNK_SIZE, 3 * CHUNK_SIZE + 17] {
            let plain = data(len);
            let sealed = store.path().join(format!("b{len}"));
            write_all(&sealed, &key, &plain).unwrap();
            let out = tempfile::tempdir().unwrap();
            // The blob wrapper is the writer over a BlobReader; both routes give the same file.
            let mut reader = BlobReader::open(&sealed, &key).unwrap();
            let n = save_staged(
                out.path(),
                "photo.jpg",
                SaveMode::Replace,
                staging,
                &mut reader,
            )
            .unwrap();
            assert_eq!(n, len as u64, "{staging:?} {len}");
            assert_eq!(std::fs::read(out.path().join("photo.jpg")).unwrap(), plain);
            // Nothing else stays in the folder, no hidden part either.
            assert_eq!(names(out.path()), ["photo.jpg"], "{staging:?} {len}");
            let m = mode(&out.path().join("photo.jpg"));
            assert_eq!(m & 0o111, 0, "no executable bit: {m:o}");
            assert_eq!(m & !0o666, 0, "no more than a plain file asked for: {m:o}");
        }
    }
    // The thin wrapper over a blob.
    let sealed = blob(store.path(), &key, b"saved bytes");
    let out = tempfile::tempdir().unwrap();
    let n = save_copy(&sealed, &key, out.path(), "kept.bin", SaveMode::NoReplace).unwrap();
    assert_eq!(n, 11);
    assert_eq!(
        std::fs::read(out.path().join("kept.bin")).unwrap(),
        b"saved bytes"
    );
}

/// A reader that looks into the folder on its first read: while the data is being copied the
/// anonymous file has no name (a half-written copy can show under none), and the named route
/// has the hidden part.
struct Peek<'a> {
    dir: &'a Path,
    seen: std::cell::RefCell<Option<Vec<String>>>,
    rest: &'a [u8],
}

impl Read for Peek<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.seen.borrow().is_none() {
            *self.seen.borrow_mut() = Some(names(self.dir));
        }
        let n = out.len().min(self.rest.len());
        out[..n].copy_from_slice(&self.rest[..n]);
        self.rest = &self.rest[n..];
        Ok(n)
    }
}

#[test]
fn the_data_waits_nameless_in_a_tmpfile_and_hidden_on_the_fallback_route() {
    let out = tempfile::tempdir().unwrap();
    for (staging, name) in [(Staging::Auto, "a"), (Staging::Named, "b")] {
        let mut peek = Peek {
            dir: out.path(),
            seen: Default::default(),
            rest: b"data",
        };
        save_staged(out.path(), name, SaveMode::NoReplace, staging, &mut peek).unwrap();
        let seen = peek.seen.take().unwrap();
        match staging {
            // O_TMPFILE: the folder holds nothing while the data is written. (tmpfs and ext4,
            // where the tests run, have it.)
            Staging::Auto => assert!(seen.is_empty(), "{seen:?}"),
            Staging::Named => {
                assert_eq!(
                    seen.len(),
                    2,
                    "the first copy and the hidden part: {seen:?}"
                );
                let part = seen.iter().find(|n| n.as_str() != "a").unwrap();
                assert!(
                    part.starts_with(".kourier-") && part.ends_with(".part"),
                    "{part}"
                );
            }
        }
    }
    assert_eq!(names(out.path()), ["a", "b"]);
}

#[test]
fn no_replace_answers_exists_and_replace_replaces() {
    for staging in BOTH {
        let out = tempfile::tempdir().unwrap();
        let target = out.path().join("report.pdf");
        std::fs::write(&target, b"old").unwrap();
        let err = save(
            out.path(),
            "report.pdf",
            SaveMode::NoReplace,
            staging,
            b"new",
        )
        .unwrap_err();
        assert!(matches!(err, SaveError::Exists), "{staging:?}: {err:?}");
        assert_eq!(std::fs::read(&target).unwrap(), b"old", "untouched");
        assert_eq!(names(out.path()), ["report.pdf"], "no part is left behind");
        // A second name is free.
        save(
            out.path(),
            "report 2.pdf",
            SaveMode::NoReplace,
            staging,
            b"two",
        )
        .unwrap();
        // Replace puts the copy over it.
        save(out.path(), "report.pdf", SaveMode::Replace, staging, b"new").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(names(out.path()), ["report 2.pdf", "report.pdf"]);
    }
}

/// A reader that counts how often it is asked.
struct Counting<'a> {
    reads: usize,
    rest: &'a [u8],
}

impl Read for Counting<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reads += 1;
        self.rest.read(buf)
    }
}

#[test]
fn a_taken_name_is_answered_before_a_byte_is_read() {
    use std::os::unix::fs::symlink;
    for staging in BOTH {
        let out = tempfile::tempdir().unwrap();
        std::fs::write(out.path().join("report.pdf"), b"old").unwrap();
        // A dangling link counts as taken too.
        symlink("nowhere", out.path().join("link.pdf")).unwrap();
        for name in ["report.pdf", "link.pdf"] {
            let mut src = Counting {
                reads: 0,
                rest: b"new data",
            };
            let err =
                save_staged(out.path(), name, SaveMode::NoReplace, staging, &mut src).unwrap_err();
            assert!(matches!(err, SaveError::Exists), "{staging:?}: {err:?}");
            assert_eq!(src.reads, 0, "{staging:?} {name}: nothing was read");
        }
        assert_eq!(
            std::fs::read(out.path().join("report.pdf")).unwrap(),
            b"old"
        );
        assert_eq!(
            names(out.path()),
            ["link.pdf", "report.pdf"],
            "no part is left"
        );
        // A free name still reads the source to its end; Replace never asks early.
        let mut src = Counting {
            reads: 0,
            rest: b"new data",
        };
        save_staged(
            out.path(),
            "free.pdf",
            SaveMode::NoReplace,
            staging,
            &mut src,
        )
        .unwrap();
        assert!(src.reads >= 1);
        let mut src = Counting {
            reads: 0,
            rest: b"new data",
        };
        save_staged(
            out.path(),
            "report.pdf",
            SaveMode::Replace,
            staging,
            &mut src,
        )
        .unwrap();
        assert!(src.reads >= 1);
        assert_eq!(
            std::fs::read(out.path().join("report.pdf")).unwrap(),
            b"new data"
        );
    }
}

#[test]
fn a_link_of_that_name_is_replaced_or_counts_as_taken_and_never_followed() {
    use std::os::unix::fs::symlink;
    for staging in BOTH {
        let out = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let victim = elsewhere.path().join("victim");
        std::fs::write(&victim, b"precious").unwrap();
        // Replace: the link goes, the file it pointed at stays as it was.
        symlink(&victim, out.path().join("a")).unwrap();
        save(out.path(), "a", SaveMode::Replace, staging, b"copy").unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious", "{staging:?}");
        let meta = std::fs::symlink_metadata(out.path().join("a")).unwrap();
        assert!(meta.is_file(), "the link was replaced by a plain file");
        assert_eq!(std::fs::read(out.path().join("a")).unwrap(), b"copy");
        // No replace: a link counts as a file that exists, one to a file, to a folder, or to
        // nothing at all (a dangling link must not look free: creating through it would make
        // the file where the link points).
        let dangling = elsewhere.path().join("not-there");
        symlink(&victim, out.path().join("b")).unwrap();
        symlink(elsewhere.path(), out.path().join("c")).unwrap();
        symlink(&dangling, out.path().join("d")).unwrap();
        for name in ["b", "c", "d"] {
            let err = save(out.path(), name, SaveMode::NoReplace, staging, b"copy").unwrap_err();
            assert!(
                matches!(err, SaveError::Exists),
                "{staging:?} {name}: {err:?}"
            );
            assert!(
                std::fs::symlink_metadata(out.path().join(name))
                    .unwrap()
                    .is_symlink()
            );
        }
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
        assert!(!dangling.exists(), "nothing was made where the link points");
        // Replace over a link to a folder replaces the link, not the folder's content.
        save(out.path(), "c", SaveMode::Replace, staging, b"copy").unwrap();
        assert!(
            std::fs::symlink_metadata(out.path().join("c"))
                .unwrap()
                .is_file()
        );
        assert_eq!(names(elsewhere.path()), ["victim"]);
    }
}

#[test]
fn the_name_is_one_path_component_and_nothing_is_written_outside_the_folder() {
    let outer = tempfile::tempdir().unwrap();
    let out = outer.path().join("chosen");
    std::fs::create_dir(&out).unwrap();
    let long = "x".repeat(256);
    for name in [
        "",
        ".",
        "..",
        "../escape",
        "a/b",
        "/abs",
        "x/",
        "ok\0bad",
        "dir/../x",
        long.as_str(),
    ] {
        for staging in BOTH {
            let err = save(&out, name, SaveMode::Replace, staging, b"copy").unwrap_err();
            assert!(matches!(err, SaveError::BadName), "{name:?}: {err:?}");
        }
    }
    assert!(names(&out).is_empty(), "{:?}", names(&out));
    assert_eq!(names(outer.path()), ["chosen"], "nothing outside either");
    // A name at the limit and one that merely looks like a path are fine.
    save(
        &out,
        &"y".repeat(255),
        SaveMode::Replace,
        Staging::Auto,
        b"1",
    )
    .unwrap();
    save(&out, "..a", SaveMode::Replace, Staging::Auto, b"2").unwrap();
    save(&out, "a..", SaveMode::Replace, Staging::Auto, b"3").unwrap();
    // The check comes before the blob is opened.
    let missing = outer.path().join("no-such-blob");
    let err = save_copy(
        &missing,
        &FileKey::generate(),
        &out,
        "../x",
        SaveMode::Replace,
    )
    .unwrap_err();
    assert!(matches!(err, SaveError::BadName), "{err:?}");
}

/// Fails after some bytes; the folder is then as it was, whichever route.
struct Breaks {
    left: usize,
}

impl Read for Breaks {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            return Err(std::io::Error::other("the source broke"));
        }
        let n = out.len().min(self.left);
        out[..n].fill(7);
        self.left -= n;
        Ok(n)
    }
}

#[test]
fn a_failing_reader_leaves_nothing_under_the_final_name_and_keeps_an_old_file() {
    for staging in BOTH {
        for mode in [SaveMode::Replace, SaveMode::NoReplace] {
            let out = tempfile::tempdir().unwrap();
            let err = save_staged(
                out.path(),
                "new",
                mode,
                staging,
                &mut Breaks { left: 70_000 },
            )
            .unwrap_err();
            assert!(matches!(err, SaveError::Source(_)), "{err:?}");
            assert!(
                names(out.path()).is_empty(),
                "{staging:?} {mode:?}: {:?}",
                names(out.path())
            );
            // An old file at the name stays as it was.
            std::fs::write(out.path().join("old"), b"keep me").unwrap();
            save_staged(out.path(), "old", mode, staging, &mut Breaks { left: 5 }).unwrap_err();
            assert_eq!(std::fs::read(out.path().join("old")).unwrap(), b"keep me");
            assert_eq!(names(out.path()), ["old"]);
        }
    }
}

#[test]
fn a_tampered_or_truncated_blob_saves_nothing_and_the_key_is_checked() {
    let store = tempfile::tempdir().unwrap();
    let key = FileKey::generate();
    let plain = data(2 * CHUNK_SIZE + 100);
    let path = blob(store.path(), &key, &plain);
    let original = std::fs::read(&path).unwrap();
    let out = tempfile::tempdir().unwrap();
    // A byte flipped in the last chunk comes up only at the end: after more than a chunk of
    // plaintext went to the tmpfile.
    let mut bad = original.clone();
    let last = bad.len() - 1;
    bad[last] ^= 1;
    std::fs::write(&path, &bad).unwrap();
    let err = save_copy(&path, &key, out.path(), "x", SaveMode::Replace).unwrap_err();
    assert!(
        matches!(err, SaveError::Source(BlobError::Corrupt)),
        "{err:?}"
    );
    // Cut after a whole chunk.
    std::fs::write(&path, &original[..HEADER_LEN + CHUNK_SIZE + TAG]).unwrap();
    let err = save_copy(&path, &key, out.path(), "x", SaveMode::Replace).unwrap_err();
    assert!(
        matches!(
            err,
            SaveError::Source(BlobError::Truncated | BlobError::Corrupt)
        ),
        "{err:?}"
    );
    // The wrong key.
    std::fs::write(&path, &original).unwrap();
    let err = save_copy(
        &path,
        &FileKey::generate(),
        out.path(),
        "x",
        SaveMode::Replace,
    )
    .unwrap_err();
    assert!(matches!(err, SaveError::Source(_)), "{err:?}");
    assert!(names(out.path()).is_empty(), "{:?}", names(out.path()));
    // Not a blob, and no such file.
    std::fs::write(&path, b"plain text, not a blob at all, just bytes").unwrap();
    assert!(save_copy(&path, &key, out.path(), "x", SaveMode::Replace).is_err());
    assert!(
        save_copy(
            &store.path().join("gone"),
            &key,
            out.path(),
            "x",
            SaveMode::Replace
        )
        .is_err()
    );
    assert!(names(out.path()).is_empty());
}

#[test]
fn a_folder_that_is_missing_or_not_a_folder_is_a_typed_answer() {
    let out = tempfile::tempdir().unwrap();
    let file = out.path().join("file");
    std::fs::write(&file, b"x").unwrap();
    for dir in [out.path().join("missing"), file] {
        let err = save_to(&dir, "x", SaveMode::Replace, &mut &b"y"[..]).unwrap_err();
        assert!(matches!(err, SaveError::NoFolder), "{err:?}");
    }
    // A folder reached through a link is the user's choice and works.
    let link = out.path().join("link");
    std::os::unix::fs::symlink(out.path(), &link).unwrap();
    save_to(&link, "via-link", SaveMode::Replace, &mut &b"y"[..]).unwrap();
    assert_eq!(std::fs::read(out.path().join("via-link")).unwrap(), b"y");
    // Read-only: skipped for root, which writes anywhere.
    let ro = out.path().join("ro");
    std::fs::create_dir(&ro).unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        for staging in BOTH {
            let err = save(&ro, "x", SaveMode::Replace, staging, b"y").unwrap_err();
            assert!(
                matches!(err, SaveError::NotWritable),
                "{staging:?}: {err:?}"
            );
        }
        assert!(names(&ro).is_empty());
    }
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o700)).unwrap();
}

//! The files of an upload (W5b, G3): the sealed temporary file and its end, the start's sweep, the core's file path.
//! Synthetic bytes only.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;

use wacore::download::MediaType;
use wacore::upload::encrypt_media_streaming_with_key;

use super::*;

fn scratch() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

/// 1 MiB of synthetic file bytes with a marker in it (no real file).
fn plain() -> Vec<u8> {
    let mut plain: Vec<u8> = (0..1u32 << 20)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    plain[300_000..300_032].copy_from_slice(b"synthetic-marker-58301947-of-fil");
    plain
}

/// Whether some 64-byte window of `needle` is anywhere in `hay` (candidates by their first 16 bytes, then compared
/// whole).
fn shares_a_window(hay: &[u8], needle: &[u8]) -> bool {
    const W: usize = 64;
    let mut starts: HashMap<[u8; 16], Vec<usize>> = HashMap::new();
    for (at, window) in hay.windows(W).enumerate() {
        let head: [u8; 16] = window[..16].try_into().expect("16 bytes");
        starts.entry(head).or_default().push(at);
    }
    needle.windows(W).any(|window| {
        let head: [u8; 16] = window[..16].try_into().expect("16 bytes");
        starts
            .get(&head)
            .is_some_and(|ats| ats.iter().any(|&at| &hay[at..at + W] == window))
    })
}

/// G3: the temporary file holds WhatsApp's ciphertext SEALED once more: neither the file's bytes (the marker) nor any
/// 64-byte window of the ciphertext is on disk; it reads back to exactly the ciphertext with its own key and with no
/// other; 0600 in a 0700 directory beside `backend.db`. Mutant: the file sealed under a key that is not the guard's.
#[test]
fn the_temporary_file_holds_neither_the_file_nor_its_ciphertext() {
    let root = scratch();
    let dir = dir_beside(&root.path().join("backend.db"));
    assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some("upload-tmp"));
    let plain = plain();
    let media_key = [0x5A; 32];
    let (file, mut writer) = TempFile::create(&dir).expect("create");
    let info = encrypt_media_streaming_with_key(
        &plain[..],
        &mut writer,
        MediaType::Document,
        Some(&media_key),
        None,
    )
    .expect("encrypt into the sealed file");
    let len = TempFile::finish(writer).expect("finish");
    let mut ciphertext = Vec::new();
    let again = encrypt_media_streaming_with_key(
        &plain[..],
        &mut ciphertext,
        MediaType::Document,
        Some(&media_key),
        None,
    )
    .expect("encrypt in memory");
    assert_eq!(
        info.file_enc_sha256, again.file_enc_sha256,
        "the same bytes"
    );
    assert_eq!(ciphertext.len(), wacore::upload::encrypted_len(plain.len()));
    assert_eq!(len, ciphertext.len() as u64);

    let disk = std::fs::read(file.path()).expect("read");
    assert_eq!(&disk[..8], &kourier_blob::MAGIC[..], "a kourier-blob file");
    assert!(
        !disk
            .windows(32)
            .any(|w| w == b"synthetic-marker-58301947-of-fil"),
        "the file's bytes are not on disk"
    );
    assert!(
        !shares_a_window(&disk, &ciphertext),
        "WhatsApp's ciphertext is not on disk"
    );
    let mut back = Vec::new();
    file.reader()
        .expect("opens with its key")
        .read_to_end(&mut back)
        .expect("reads");
    assert!(back == ciphertext, "it reads back to the ciphertext");
    assert!(
        kourier_blob::read_all(file.path(), &FileKey::generate()).is_err(),
        "no other key opens it"
    );
    assert_eq!(
        std::fs::metadata(file.path()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

/// G3: no temporary file survives its attempt: a guard dropped after a finished write, one dropped while its writer
/// had not finished, one shared with a blocking reader (it stays until that reader lets go, then goes), one dropped
/// with an aborted task. The directory stays for the next attempt. Mutant: the guard's `Drop` removes nothing.
#[tokio::test]
async fn no_temporary_file_survives_its_attempt() {
    let root = scratch();
    let dir = dir_beside(&root.path().join("backend.db"));

    let (file, mut writer) = TempFile::create(&dir).expect("create");
    writer.write_all(&[7; 100_000]).expect("write");
    assert_eq!(TempFile::finish(writer).expect("finish"), 100_000);
    let path = file.path().to_owned();
    assert!(path.exists());
    drop(file);
    assert!(!path.exists(), "a finished file goes with its guard");

    let (file, mut writer) = TempFile::create(&dir).expect("create");
    writer.write_all(&[8; 70_000]).expect("write");
    let path = file.path().to_owned();
    drop(file);
    drop(writer);
    assert!(!path.exists(), "an unfinished file goes too");

    let (file, writer) = TempFile::create(&dir).expect("create");
    TempFile::finish(writer).expect("finish");
    let file = Arc::new(file);
    let path = file.path().to_owned();
    let (go, wait) = std::sync::mpsc::channel::<()>();
    let reader = {
        let file = file.clone();
        std::thread::spawn(move || {
            let mut r = file.reader().expect("reader");
            let _ = wait.recv();
            let mut sink = Vec::new();
            r.read_to_end(&mut sink)
                .expect("reads after its attempt ended");
        })
    };
    drop(file);
    assert!(path.exists(), "the reader still holds it");
    go.send(()).expect("go");
    reader.join().expect("no panic");
    assert!(!path.exists(), "the last holder removed it");

    let (made, taken) = tokio::sync::oneshot::channel();
    let task = tokio::spawn({
        let dir = dir.clone();
        async move {
            let (file, writer) = TempFile::create(&dir).expect("create");
            TempFile::finish(writer).expect("finish");
            let _ = made.send(file.path().to_owned());
            let _kept = file;
            std::future::pending::<()>().await;
        }
    });
    let path = taken.await.expect("made");
    assert!(path.exists());
    task.abort();
    let _ = task.await;
    assert!(!path.exists(), "a dropped task's file goes with it");

    assert!(dir.is_dir(), "the directory stays for the next attempt");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
}

/// G3: a process killed in the middle of an upload ran no destructor: its file is removed when the account's actor
/// starts, and a missing directory is fine. Mutant: the sweep removes nothing.
#[test]
fn the_directory_is_emptied_at_start() {
    let root = scratch();
    let dir = dir_beside(&root.path().join("backend.db"));
    clean_at_start(&dir);
    let (file, writer) = TempFile::create(&dir).expect("create");
    TempFile::finish(writer).expect("finish");
    let path = file.path().to_owned();
    // The kill: nothing runs the guard.
    std::mem::forget(file);
    assert!(path.exists());
    clean_at_start(&dir);
    assert!(!dir.exists(), "the directory and its file are gone");
    clean_at_start(&dir);
}

/// The core's file of an attachment is read only inside the blob directory: an empty path, an absolute one and any
/// `..` are refused (`InvalidInput`), a plain relative one is joined. Mutant: the check removed.
#[test]
fn a_core_file_outside_the_blob_directory_is_refused() {
    let blobs = Path::new("/synthetic/blobs");
    assert_eq!(
        core_file(blobs, Path::new("ab/cd0123")).expect("inside"),
        blobs.join("ab/cd0123")
    );
    assert_eq!(
        core_file(blobs, Path::new("./ab/cd0123")).expect("inside"),
        blobs.join("./ab/cd0123")
    );
    for outside in ["", "/etc/passwd", "../outside", "ab/../../outside", "ab/.."] {
        let refused = core_file(blobs, Path::new(outside)).expect_err(outside);
        assert_eq!(refused.kind, ErrorKind::InvalidInput, "{outside}");
    }
}

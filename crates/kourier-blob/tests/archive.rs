//! The export archive writer (architecture §5.8.3): entries, order, hashes, ZIP64, a corrupt
//! source, cancel, errors, the name rules, the mode. Synthetic bytes only.

use kourier_blob::{
    ArchiveItem, ArchiveOptions, CHUNK_SIZE, FileKey, SaveError, SaveMode, Source, Staging,
    check_entry_name, write_all, write_archive,
};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

const BOTH: [Staging; 2] = [Staging::Auto, Staging::Named];

fn data(len: usize, seed: usize) -> Vec<u8> {
    (0..len).map(|i| ((i + seed) * 31 % 251) as u8).collect()
}

/// A recognisable stream no other generated data repeats in a window of 64 bytes.
fn unique(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn names(dir: &Path) -> Vec<String> {
    let mut n: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    n.sort();
    n
}

fn blob(dir: &Path, name: &str, plain: &[u8]) -> (PathBuf, FileKey) {
    let key = FileKey::generate();
    let path = dir.join(name);
    write_all(&path, &key, plain).unwrap();
    (path, key)
}

fn file_item(entry: &str, path: &Path, key: &FileKey, size: usize) -> ArchiveItem {
    ArchiveItem {
        name: entry.to_owned(),
        source: Source::Blob {
            path: path.to_owned(),
            key: key.clone(),
            size: size as u64,
        },
    }
}

fn doc(entry: &str, bytes: &[u8]) -> ArchiveItem {
    ArchiveItem {
        name: entry.to_owned(),
        source: Source::Bytes(bytes.to_vec()),
    }
}

fn manifest(_: &[kourier_blob::EntryInfo], failed: &[String]) -> (String, Vec<u8>) {
    (
        "manifest.json".to_owned(),
        format!("{{\"f\":{}}}", failed.len()).into_bytes(),
    )
}

fn read_back(path: &Path) -> Vec<(String, Vec<u8>, zip::CompressionMethod, Option<u32>)> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    (0..zip.len())
        .map(|i| {
            let mut f = zip.by_index(i).unwrap();
            let mut bytes = Vec::new();
            f.read_to_end(&mut bytes).unwrap();
            (
                f.name().unwrap().into_owned(),
                bytes,
                f.compression(),
                f.unix_mode(),
            )
        })
        .collect()
}

#[test]
fn entries_read_back_in_order_with_their_hashes_and_the_manifest_last() {
    for staging in BOTH {
        let tmp = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let a = data(CHUNK_SIZE * 2 + 17, 1);
        let b = data(5, 2);
        let (pa, ka) = blob(tmp.path(), "a", &a);
        let (pb, kb) = blob(tmp.path(), "b", &b);
        let items = vec![
            file_item("files/a.bin", &pa, &ka, a.len()),
            file_item("files/b.bin", &pb, &kb, b.len()),
            doc("index.html", b"<html></html>"),
            doc("messages.jsonl", b"{}\n"),
        ];
        let cancel = AtomicBool::new(false);
        let mut options = ArchiveOptions::new(&cancel);
        options.staging = staging;
        let mut last = 0;
        let report = write_archive(
            out.path(),
            "x.zip",
            items,
            &options,
            &mut |done| {
                assert!(done >= last, "progress never goes back");
                last = done;
            },
            &mut manifest,
        )
        .unwrap();
        assert_eq!(last, (a.len() + b.len() + 13 + 3) as u64);
        assert!(report.failed.is_empty());
        assert_eq!(
            names(out.path()),
            ["x.zip"],
            "{staging:?}: no .part is left"
        );
        let path = out.path().join("x.zip");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(report.bytes, std::fs::metadata(&path).unwrap().len());
        let entries = read_back(&path);
        let listed: Vec<&str> = entries.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(
            listed,
            [
                "files/a.bin",
                "files/b.bin",
                "index.html",
                "messages.jsonl",
                "manifest.json"
            ]
        );
        assert_eq!(report.entries.len(), 5);
        for ((name, bytes, method, mode), info) in entries.iter().zip(&report.entries) {
            assert_eq!(&info.name, name);
            assert_eq!(info.size, bytes.len() as u64);
            assert_eq!(info.sha256, hex(Sha256::digest(bytes).as_slice()), "{name}");
            assert_eq!(mode.unwrap() & 0o777, 0o644, "never an executable bit");
            let stored = name.starts_with("files/");
            assert_eq!(*method == zip::CompressionMethod::Stored, stored, "{name}");
        }
        assert_eq!(entries[0].1, a);
        assert_eq!(entries[1].1, b);
    }
}

#[test]
fn zip64_entries_read_back() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let a = data(100_000, 3);
    let (pa, ka) = blob(tmp.path(), "a", &a);
    let cancel = AtomicBool::new(false);
    let mut options = ArchiveOptions::new(&cancel);
    // Every entry is written as ZIP64.
    options.zip64_from = 0;
    write_archive(
        out.path(),
        "z.zip",
        vec![
            file_item("files/a", &pa, &ka, a.len()),
            doc("index.html", b"hi"),
        ],
        &options,
        &mut |_| {},
        &mut manifest,
    )
    .unwrap();
    let entries = read_back(&out.path().join("z.zip"));
    assert_eq!(entries[0].1, a);
    assert_eq!(entries[1].1, b"hi");
    // The local header of the first entry carries the ZIP64 extra field (id 0x0001).
    let raw = std::fs::read(out.path().join("z.zip")).unwrap();
    assert!(
        raw.windows(4).any(|w| w == [0x01, 0x00, 0x10, 0x00]),
        "a ZIP64 extra field"
    );
}

#[test]
fn a_blob_corrupt_in_its_second_chunk_fails_that_file_only() {
    for staging in BOTH {
        for position in 0..3 {
            let tmp = tempfile::tempdir().unwrap();
            let out = tempfile::tempdir().unwrap();
            let bad = unique(CHUNK_SIZE * 3, 9);
            let (pbad, kbad) = blob(tmp.path(), "bad", &bad);
            let mut raw = std::fs::read(&pbad).unwrap();
            let second = kourier_blob::HEADER_LEN + CHUNK_SIZE + 16 + 10;
            raw[second] ^= 1;
            std::fs::write(&pbad, raw).unwrap();
            let good = data(70_000, 4);
            let (pg, kg) = blob(tmp.path(), "good", &good);
            let mut items = vec![
                file_item("files/good1", &pg, &kg, good.len()),
                file_item("files/good2", &pg, &kg, good.len()),
            ];
            items.insert(position, file_item("files/bad", &pbad, &kbad, bad.len()));
            items.push(doc("index.html", b"<p>after</p>"));
            let cancel = AtomicBool::new(false);
            let mut options = ArchiveOptions::new(&cancel);
            options.staging = staging;
            let report = write_archive(
                out.path(),
                "c.zip",
                items,
                &options,
                &mut |_| {},
                &mut manifest,
            )
            .unwrap();
            assert_eq!(report.failed, ["files/bad"]);
            let entries = read_back(&out.path().join("c.zip"));
            let listed: Vec<&str> = entries.iter().map(|e| e.0.as_str()).collect();
            assert!(!listed.contains(&"files/bad"), "{listed:?}");
            assert_eq!(listed.len(), 4, "{listed:?}");
            assert_eq!(listed.last(), Some(&"manifest.json"));
            for (name, bytes, ..) in &entries {
                match name.as_str() {
                    "files/good1" | "files/good2" => assert_eq!(bytes, &good),
                    "index.html" => assert_eq!(bytes, b"<p>after</p>"),
                    _ => {}
                }
            }
            assert_eq!(names(out.path()), ["c.zip"]);
            // H1: none of the failed file's plaintext (its first chunk was written before the
            // second failed to authenticate) is left in the archive, and the file ends at the end
            // of the end-of-central-directory record.
            let raw = std::fs::read(out.path().join("c.zip")).unwrap();
            let mut at = 0;
            while at + 64 <= CHUNK_SIZE {
                assert!(
                    !raw.windows(64).any(|w| w == &bad[at..at + 64]),
                    "plaintext of the failed file at {at} ({staging:?}, {position})"
                );
                at += 997;
            }
            assert!(
                !raw.windows(64)
                    .any(|w| w == &bad[CHUNK_SIZE - 64..CHUNK_SIZE])
            );
            let eocd = raw.len() - 22;
            assert_eq!(&raw[eocd..eocd + 4], [0x50, 0x4b, 0x05, 0x06]);
            assert_eq!(
                &raw[eocd + 20..],
                [0, 0],
                "no comment: the record ends the file"
            );
            assert_eq!(report.bytes, raw.len() as u64);
        }
    }
}

#[test]
fn a_file_that_does_not_open_is_reported_and_the_archive_goes_on() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let report = write_archive(
        out.path(),
        "m.zip",
        vec![
            file_item(
                "files/gone",
                &tmp.path().join("nope"),
                &FileKey::generate(),
                3,
            ),
            doc("index.html", b"x"),
        ],
        &ArchiveOptions::new(&AtomicBool::new(false)),
        &mut |_| {},
        &mut manifest,
    )
    .unwrap();
    assert_eq!(report.failed, ["files/gone"]);
    assert_eq!(read_back(&out.path().join("m.zip")).len(), 2);
}

#[test]
fn a_cancel_leaves_nothing_on_both_routes() {
    for staging in BOTH {
        let tmp = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let a = data(CHUNK_SIZE * 3, 5);
        let (pa, ka) = blob(tmp.path(), "a", &a);
        let cancel = AtomicBool::new(false);
        let mut options = ArchiveOptions::new(&cancel);
        options.staging = staging;
        let mut chunks = 0;
        let err = write_archive(
            out.path(),
            "x.zip",
            vec![
                file_item("files/a", &pa, &ka, a.len()),
                doc("index.html", b"x"),
            ],
            &options,
            &mut |_| {
                chunks += 1;
                if chunks == 2 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &mut manifest,
        )
        .unwrap_err();
        assert!(matches!(err, SaveError::Cancelled), "{err:?}");
        assert_eq!(names(out.path()), Vec::<String>::new(), "{staging:?}");
    }
}

#[test]
fn a_pause_before_each_file_entry_still_sees_a_cancel() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let a = data(100, 5);
    let (pa, ka) = blob(tmp.path(), "a", &a);
    let (pb, kb) = blob(tmp.path(), "b", &a);
    let cancel = AtomicBool::new(false);
    let mut options = ArchiveOptions::new(&cancel);
    options.entry_pause = std::time::Duration::from_secs(30);
    // A cancel from another thread lands while the writer waits before the first file.
    let started = std::time::Instant::now();
    let err = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(200));
            cancel.store(true, Ordering::Relaxed);
        });
        write_archive(
            out.path(),
            "p.zip",
            vec![
                file_item("files/a", &pa, &ka, a.len()),
                file_item("files/b", &pb, &kb, a.len()),
                doc("index.html", b"x"),
            ],
            &options,
            &mut |_| {},
            &mut manifest,
        )
        .unwrap_err()
    });
    assert!(matches!(err, SaveError::Cancelled), "{err:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(names(out.path()), Vec::<String>::new());
}

#[test]
fn an_error_leaves_nothing_and_a_taken_name_is_exists() {
    for staging in BOTH {
        let out = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        let mut options = ArchiveOptions::new(&cancel);
        options.staging = staging;
        // The manifest's own name is checked too: a bad one fails after everything was written.
        let err = write_archive(
            out.path(),
            "x.zip",
            vec![doc("index.html", b"x")],
            &options,
            &mut |_| {},
            &mut |_, _| ("../manifest.json".to_owned(), Vec::new()),
        )
        .unwrap_err();
        assert!(matches!(err, SaveError::BadName), "{err:?}");
        assert_eq!(names(out.path()), Vec::<String>::new(), "{staging:?}");
        // A taken name.
        std::fs::write(out.path().join("t.zip"), b"old").unwrap();
        let err = write_archive(
            out.path(),
            "t.zip",
            vec![doc("index.html", b"x")],
            &options,
            &mut |_| {},
            &mut manifest,
        )
        .unwrap_err();
        assert!(matches!(err, SaveError::Exists), "{err:?}");
        assert_eq!(std::fs::read(out.path().join("t.zip")).unwrap(), b"old");
        assert_eq!(names(out.path()), ["t.zip"]);
        // Replace takes its place.
        options.mode = SaveMode::Replace;
        write_archive(
            out.path(),
            "t.zip",
            vec![doc("index.html", b"x")],
            &options,
            &mut |_| {},
            &mut manifest,
        )
        .unwrap();
        assert_eq!(read_back(&out.path().join("t.zip")).len(), 2);
        assert_eq!(names(out.path()), ["t.zip"]);
    }
}

#[test]
fn bad_entry_names_refuse_the_whole_archive_before_anything_is_written() {
    let long = "a".repeat(300);
    let ok_long = "a".repeat(255);
    let bad: Vec<String> = vec![
        "files/../x".into(),
        "../x".into(),
        "..".into(),
        ".".into(),
        "files/..".into(),
        "files/a/b".into(),
        "other/a".into(),
        "files\\a".into(),
        "a\\b".into(),
        "a\0b".into(),
        "a\u{1}b".into(),
        "a\u{7f}b".into(),
        "a\u{85}b".into(),
        "/etc/passwd".into(),
        "/files/a".into(),
        format!("files/{long}"),
        long,
        String::new(),
        "files/".into(),
        "files//a".into(),
        // M2: invisible format characters (a joiner is NOT among them), the characters Windows
        // refuses, a trailing dot or space, the reserved device names.
        "files/a\u{202E}b".into(),
        "files/a\u{2066}b".into(),
        "files/a\u{2069}b".into(),
        "files/a\u{061C}b".into(),
        "files/a\u{200B}b".into(),
        "files/a\u{200E}b".into(),
        "files/a\u{200F}b".into(),
        "files/a\u{2060}b".into(),
        "files/a\u{2064}b".into(),
        "files/a\u{FEFF}b".into(),
        "files/a:b".into(),
        "files/a*b".into(),
        "files/a?b".into(),
        "files/a\"b".into(),
        "files/a<b".into(),
        "files/a>b".into(),
        "files/a|b".into(),
        "files/a.".into(),
        "files/a ".into(),
        "files/CON".into(),
        "files/con.txt".into(),
        "files/Prn".into(),
        "files/aux.png".into(),
        "files/NUL.tar.gz".into(),
        "files/COM1".into(),
        "files/com9.txt".into(),
        "files/LPT1".into(),
        "files/lpt9.x".into(),
        "NUL".into(),
    ];
    for name in &bad {
        assert!(
            matches!(check_entry_name(name), Err(SaveError::BadName)),
            "{name:?}"
        );
        let out = tempfile::tempdir().unwrap();
        let err = write_archive(
            out.path(),
            "x.zip",
            vec![doc("index.html", b"x"), doc(name, b"y")],
            &ArchiveOptions::new(&AtomicBool::new(false)),
            &mut |_| {},
            &mut manifest,
        )
        .unwrap_err();
        assert!(matches!(err, SaveError::BadName), "{name:?}: {err:?}");
        assert_eq!(names(out.path()), Vec::<String>::new());
    }
    for name in [
        "index.html",
        "files/a b.png",
        "files/ä.txt",
        "files/CONSOLE.txt",
        "files/COM10",
        "files/COM0",
        "files/می\u{200C}خواهم.txt",
        "files/\u{1F468}\u{200D}\u{1F469}.png",
        &format!("files/{ok_long}"),
    ] {
        assert!(check_entry_name(name).is_ok(), "{name:?}");
    }
    // M3: the place follows the kind: a file entry under `files/`, a document at the top.
    let tmp = tempfile::tempdir().unwrap();
    let (p, k) = blob(tmp.path(), "b", b"abc");
    for items in [
        vec![file_item("page.html", &p, &k, 3)],
        vec![doc("files/x", b"1")],
    ] {
        let out = tempfile::tempdir().unwrap();
        let err = write_archive(
            out.path(),
            "x.zip",
            items,
            &ArchiveOptions::new(&AtomicBool::new(false)),
            &mut |_| {},
            &mut manifest,
        )
        .unwrap_err();
        assert!(matches!(err, SaveError::BadName), "{err:?}");
        assert_eq!(names(out.path()), Vec::<String>::new());
    }
    // Duplicates, also against the manifest.
    let out = tempfile::tempdir().unwrap();
    for items in [
        vec![doc("a", b"1"), doc("a", b"2")],
        vec![doc("manifest.json", b"1")],
    ] {
        let err = write_archive(
            out.path(),
            "x.zip",
            items,
            &ArchiveOptions::new(&AtomicBool::new(false)),
            &mut |_| {},
            &mut manifest,
        )
        .unwrap_err();
        assert!(matches!(err, SaveError::BadName), "{err:?}");
        assert_eq!(names(out.path()), Vec::<String>::new());
    }
}

#[test]
fn the_staged_file_is_private_while_the_write_runs_and_one_large_file_cancels_mid_way() {
    use std::os::unix::fs::MetadataExt;
    // (M5 f) On the named route the `.part` is 0o600 for the whole write, not only at the end.
    let tmp = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let a = data(CHUNK_SIZE * 4, 5);
    let (pa, ka) = blob(tmp.path(), "a", &a);
    let cancel = AtomicBool::new(false);
    let mut options = ArchiveOptions::new(&cancel);
    options.staging = Staging::Named;
    let mut modes = Vec::new();
    let dir = out.path().to_owned();
    write_archive(
        out.path(),
        "m.zip",
        vec![
            file_item("files/a", &pa, &ka, a.len()),
            doc("index.html", b"x"),
        ],
        &options,
        &mut |_| {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let entry = entry.unwrap();
                if entry.file_name().to_string_lossy().starts_with(".kourier-") {
                    modes.push(entry.metadata().unwrap().mode() & 0o777);
                }
            }
        },
        &mut manifest,
    )
    .unwrap();
    assert!(!modes.is_empty(), "a `.part` was there while the write ran");
    assert!(modes.iter().all(|m| *m == 0o600), "{modes:?}");

    // (M5 g) One large item, cancelled from the progress callback in the middle of the file:
    // no later item gives the per-item check a chance, only the check inside the chunks does.
    for staging in BOTH {
        let out = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        let mut options = ArchiveOptions::new(&cancel);
        options.staging = staging;
        let mut calls = 0;
        let err = write_archive(
            out.path(),
            "x.zip",
            vec![file_item("files/a", &pa, &ka, a.len())],
            &options,
            &mut |_| {
                calls += 1;
                if calls == 1 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &mut manifest,
        )
        .unwrap_err();
        assert!(matches!(err, SaveError::Cancelled), "{err:?}");
        assert_eq!(calls, 1, "it stopped inside the file, not after it");
        assert_eq!(names(out.path()), Vec::<String>::new(), "{staging:?}");
    }
}

#[test]
fn the_debug_of_the_report_and_the_entries_shows_no_name() {
    let info = kourier_blob::EntryInfo {
        name: "files/secret-name.pdf".into(),
        size: 5,
        sha256: "00".into(),
    };
    let report = kourier_blob::ArchiveReport {
        entries: vec![info.clone()],
        failed: vec!["files/other-secret.png".into()],
        bytes: 9,
    };
    for text in [format!("{info:?}"), format!("{report:?}")] {
        assert!(!text.contains("secret"), "{text}");
    }
}

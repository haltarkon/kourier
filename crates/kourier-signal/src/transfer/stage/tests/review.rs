//! Review probes of the stage file (step S1's independent review): what a stage found on
//! disk can be made to do. Synthetic data; counts only are printed.

use super::*;

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// The per-page check is on for ordinary reads too, not only inside `count()`: with one
/// bit flipped in a page, reading every row back the way delivery will (no `count()`
/// first) either fails or gives exactly the rows that were written. Never other rows.
#[test]
fn review_a_changed_page_never_reads_back_as_other_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = writer(dir.path(), 2, StageLimits::default());
    for n in 1..=45u64 {
        let chat = u32::try_from(n % 2).unwrap();
        // Short rows, rows over a quarter of a page, and rows of several pages.
        let len = match n % 3 {
            0 => 200,
            1 => 1_800,
            _ => 9_000,
        };
        let text = format!("{n}:{}", "t".repeat(len));
        writer.item(chat, n, &message(chat, n, &text)).unwrap();
    }
    writer
        .commit(&[counters(0, 22, 1_044), counters(1, 23, 1_045)], 50)
        .unwrap();
    let path = path_in(dir.path());
    let whole = std::fs::read(&path).unwrap();
    let pages = whole.len() / 4096;
    type Everything = (
        Vec<(u32, StagedContact)>,
        Vec<StagedChatRow>,
        Vec<StagedItem>,
    );
    let read_all = || -> Result<Everything, StageError> {
        let reader = StageReader::open(dir.path(), &key())?;
        let people = reader.contacts(None, 1_000)?;
        let chats = reader.chats()?;
        let mut items = Vec::new();
        for chat in 0..2 {
            items.extend(reader.items(chat, None, 1_000, u64::MAX)?);
        }
        Ok((people, chats, items))
    };
    let honest = read_all().unwrap();
    assert_eq!(honest.2.len(), 45);
    let (mut failed, mut same, mut other) = (0, 0, 0);
    for page in 0..pages {
        for offset in [16, 4_000] {
            let mut damaged = whole.clone();
            damaged[page * 4096 + offset] ^= 0x01;
            std::fs::write(&path, &damaged).unwrap();
            match read_all() {
                Err(_) => failed += 1,
                Ok(read) if read == honest => same += 1,
                Ok(_) => other += 1,
            }
        }
    }
    std::fs::write(&path, &whole).unwrap();
    println!(
        "{pages} pages, 2 flipped bits each, read back without count(): {failed} reads \
         failed, {same} gave the written rows, {other} gave other rows"
    );
    assert_eq!(other, 0, "a changed page was read as other rows");
    // A page no row read touches (the index of `chats.conv`) reads back the same; only
    // `count()` looks at it.
    assert!(failed > same * 50, "{failed} failed, {same} the same");
}

/// A commit that fails must leave no file: `Importer::commit` consumes the importer and
/// nobody calls `abort` after it. (The builder's departure 3; it had no test.)
#[test]
fn review_a_commit_that_fails_leaves_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = writer(dir.path(), 1, StageLimits::default());
    writer.item(0, 1, &message(0, 1, "never whole")).unwrap();
    // Counters for a chat the stage does not have: the commit refuses.
    let end = writer.commit(&[counters(7, 1, 1_001)], 3);
    assert!(end.is_err());
    assert_eq!(names(dir.path()), Vec::<String>::new());
}

/// `create` in a directory where the stage's names are symbolic links (planted, or left by
/// anything): the links are removed, never followed, and the files they pointed at are
/// untouched.
#[test]
fn review_create_never_writes_through_a_planted_link() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mut victims = Vec::new();
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let victim = outside.path().join(format!("victim{suffix}"));
        std::fs::write(&victim, b"not yours to write").unwrap();
        std::os::unix::fs::symlink(&victim, dir.path().join(format!("{FILE}{suffix}"))).unwrap();
        victims.push(victim);
    }
    let mut writer = writer(dir.path(), 1, StageLimits::default());
    for n in 1..=50u64 {
        writer.item(0, n, &message(0, n, &"q".repeat(900))).unwrap();
    }
    writer.commit(&[counters(0, 50, 1_050)], 60).unwrap();
    for victim in &victims {
        assert_eq!(std::fs::read(victim).unwrap(), b"not yours to write");
    }
    let meta = std::fs::symlink_metadata(path_in(dir.path())).unwrap();
    assert!(meta.file_type().is_file(), "the stage is a file of its own");
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
}

/// A read that did not stop in time and then COMMITS (it was past its last frame) while the
/// next read of the account has made a new stage under the same name: the late commit
/// neither damages nor removes the new stage, also not its log and index, which SQLite
/// removes by name when a database is closed.
#[test]
fn review_a_late_commit_leaves_the_stage_that_replaced_it_alone() {
    let dir = tempfile::tempdir().unwrap();
    let mut late = writer(dir.path(), 1, StageLimits::default());
    late.item(0, 1, &message(0, 1, "late")).unwrap();
    let mut next = writer(dir.path(), 1, StageLimits::default());
    next.item(0, 1, &message(0, 1, "kept")).unwrap();
    let before = names(dir.path());
    assert!(before.contains(&format!("{FILE}-wal")), "{before:?}");
    // The late writer ends whole; nobody takes its summary.
    late.commit(&[counters(0, 1, 1_001)], 3).unwrap();
    assert_eq!(
        names(dir.path()),
        before,
        "the new stage's files are all there"
    );
    next.item(0, 2, &message(0, 2, "kept too")).unwrap();
    next.commit(&[counters(0, 2, 1_002)], 4).unwrap();
    let reader = StageReader::open(dir.path(), &key()).unwrap();
    assert_eq!(reader.count().unwrap(), reader.totals().unwrap());
    let rows = rows_of(&reader, 0, 10);
    assert_eq!(
        rows.iter().map(|row| body_of(&row.msg)).collect::<Vec<_>>(),
        ["kept", "kept too"]
    );
}

/// The files SQLite makes beside the stage while it is written are this user's only.
#[test]
fn review_the_wal_and_its_index_are_private_while_the_stage_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = writer(dir.path(), 1, StageLimits::default());
    writer.item(0, 1, &message(0, 1, "x")).unwrap();
    for suffix in ["-wal", "-shm"] {
        let path = dir.path().join(format!("{FILE}{suffix}"));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{suffix}");
    }
    writer.abort();
    assert_eq!(names(dir.path()), Vec::<String>::new());
}

/// A recorded stage with a write-ahead log planted beside it. The writer leaves no log
/// (its last checkpoint truncates it and closing removes it), so a log found later is
/// foreign. An earlier log of the SAME stage holds pages that pass their check value (same
/// key, same page numbers): does it roll rows back unnoticed?
#[test]
fn review_an_earlier_log_planted_beside_a_committed_stage_changes_nothing_unnoticed() {
    let dir = tempfile::tempdir().unwrap();
    let limits = StageLimits {
        max_bytes: MAX_BYTES,
        commit_every: 6_000,
    };
    let mut writer = writer(dir.path(), 3, limits);
    let wal = dir.path().join(format!("{FILE}-wal"));
    let mut captured: Vec<(u64, Vec<u8>)> = Vec::new();
    for n in 1..=400u64 {
        let chat = u32::try_from(n % 3).unwrap();
        writer
            .item(
                chat,
                n,
                &message(chat, n, &format!("row {n} {}", "r".repeat(200))),
            )
            .unwrap();
        if n % 50 == 0 {
            captured.push((n, std::fs::read(&wal).unwrap()));
        }
    }
    let per = |chat: u64| (1..=400u64).filter(|n| n % 3 == chat).count() as u64;
    let all = [
        counters(0, per(0), 1_399),
        counters(1, per(1), 1_400),
        counters(2, per(2), 1_398),
    ];
    let totals = writer.commit(&all, 410).unwrap();
    let whole = std::fs::read(path_in(dir.path())).unwrap();
    let rows = |reader: &StageReader| -> Vec<(u32, u64, String)> {
        (0..3)
            .flat_map(|chat| {
                rows_of(reader, chat, 500)
                    .into_iter()
                    .map(move |row| (chat, row.n, body_of(&row.msg).to_owned()))
            })
            .collect()
    };
    let honest = rows(&StageReader::open(dir.path(), &key()).unwrap());
    assert_eq!(honest.len(), 400);

    let mut accepted_same = 0;
    let mut refused = 0;
    let mut accepted_changed = 0;
    for (at, log) in &captured {
        // The stage as the commit left it, and the earlier log beside it.
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(dir.path().join(format!("{FILE}{suffix}")));
        }
        std::fs::write(path_in(dir.path()), &whole).unwrap();
        std::fs::write(&wal, log).unwrap();
        let outcome = StageReader::open(dir.path(), &key()).and_then(|reader| {
            let counted = reader.count()?;
            Ok((counted == reader.totals()?, reader))
        });
        match outcome {
            Ok((true, reader)) => {
                let all_rows: Result<Vec<_>, _> = (0..3)
                    .map(|chat| reader.items(chat, None, 1_000, u64::MAX))
                    .collect();
                match all_rows {
                    Ok(_) if rows(&reader) == honest => accepted_same += 1,
                    _ => {
                        accepted_changed += 1;
                        println!("a log captured at row {at}: accepted, and the rows differ");
                    }
                }
            }
            Ok((false, _)) | Err(_) => refused += 1,
        }
    }
    println!(
        "{} earlier logs planted: {refused} refused, {accepted_same} accepted with the same \
         rows, {accepted_changed} accepted with other rows (totals {totals:?})",
        captured.len()
    );
    assert_eq!(accepted_changed, 0);
}

/// A log of garbage beside a committed stage is no reason to lose the stage, and no way
/// to change it.
#[test]
fn review_a_garbage_log_beside_a_committed_stage_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = writer(dir.path(), 1, StageLimits::default());
    writer.item(0, 1, &message(0, 1, "kept")).unwrap();
    let totals = writer.commit(&[counters(0, 1, 1_001)], 3).unwrap();
    std::fs::write(dir.path().join(format!("{FILE}-wal")), vec![0xa5; 8_192]).unwrap();
    let reader = StageReader::open(dir.path(), &key()).unwrap();
    assert_eq!(reader.count().unwrap(), totals);
    assert_eq!(body_of(&rows_of(&reader, 0, 10)[0].msg), "kept");
}

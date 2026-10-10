//! The stage file on its own: what goes in comes back per chat in frame order, only its own
//! key opens it, a changed page is refused, its bounds hold. Synthetic data.

use std::os::unix::fs::PermissionsExt as _;

use kourier_model::{Body, Origin};

use super::*;

const EPHEMERAL: [u8; 32] = [0x42; 32];

thread_local! {
    /// What the next `remove_own` of this thread runs before it looks at the file.
    static AT_REMOVE_OWN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Called by `remove_own`: runs, once, the step a test put there.
pub(super) fn at_remove_own() {
    let step = AT_REMOVE_OWN.with(|step| step.borrow_mut().take());
    if let Some(step) = step {
        step();
    }
}

fn key() -> StageKey {
    StageKey::derive(&EPHEMERAL)
}

fn peer(i: u32) -> UserId {
    UserId::parse(&format!("aci:00000000-0000-4000-8000-{i:012x}")).unwrap()
}

fn conv(i: u32) -> ConvKey {
    ConvKey::Direct(peer(i))
}

fn contact(i: u32) -> StagedContact {
    StagedContact {
        user: peer(i),
        aci: None,
        pni: None,
        e164: None,
        profile_key: Some([7; 32]),
        profile_sharing: true,
        blocked: false,
        hidden: false,
        unregistered_at: None,
        profile_name: Some(format!("Person {i}")),
        system_name: None,
        nickname: None,
    }
}

fn message(chat: u32, n: u64, body: &str) -> IncomingMessage {
    let mut msg = IncomingMessage::new(
        conv(chat),
        (1_000 + n).to_string(),
        Origin::Remote {
            author: peer(chat),
            device: None,
        },
        Body::text(body),
        i64::try_from(1_000 + n).unwrap(),
        i64::try_from(1_001 + n).unwrap(),
    );
    msg.silent = true;
    msg
}

fn body_of(msg: &IncomingMessage) -> &str {
    match &msg.body {
        Body::Text(text) => &text.body,
        _ => panic!("a text"),
    }
}

/// A writer with `chats` chats and their contacts staged.
fn writer(dir: &Path, chats: u32, limits: StageLimits) -> StageWriter {
    let mut writer = StageWriter::create(dir, &key(), limits).unwrap();
    for i in 0..chats {
        writer.contact(i, &contact(i)).unwrap();
        writer
            .chat(
                i,
                &conv(i),
                &StagedChat {
                    is_self: false,
                    peer: Some(i),
                },
                None,
            )
            .unwrap();
    }
    writer
}

fn counters(idx: u32, rows: u64, last_at: i64) -> ChatCounters {
    ChatCounters {
        idx,
        n_rows: rows,
        n_loud: rows,
        last_at,
    }
}

/// Every row of `chat`, read in pages of `page` rows.
fn rows_of(reader: &StageReader, chat: u32, page: usize) -> Vec<StagedItem> {
    let mut all = Vec::new();
    let mut after = None;
    loop {
        let rows = reader.items(chat, after, page, u64::MAX).unwrap();
        let Some(last) = rows.last() else {
            return all;
        };
        after = Some(last.n);
        all.extend(rows);
    }
}

#[test]
fn the_stage_gives_a_chat_back_in_frame_order_whatever_the_interleaving() {
    let dir = tempfile::tempdir().unwrap();
    // A transaction every few rows: the rows of one chat are spread over many.
    let limits = StageLimits {
        max_bytes: MAX_BYTES,
        commit_every: 2_000,
    };
    let mut writer = writer(dir.path(), 3, limits);
    // The archive's order: the items of all chats in one sequence. Chat 2 gets its rows in
    // bursts, chat 0 and 1 alternate; chat 1's last frames are written before earlier ones
    // of chat 0.
    let order: Vec<(u32, u64)> = (1..=60u64)
        .map(|n| {
            let chat = match n % 7 {
                0..=2 => 2,
                3 | 5 => 0,
                _ => 1,
            };
            (chat, n)
        })
        .collect();
    for (chat, n) in &order {
        writer
            .item(
                *chat,
                *n,
                &message(*chat, *n, &format!("chat {chat} frame {n}")),
            )
            .unwrap();
    }
    let per_chat = |chat: u32| -> Vec<u64> {
        order
            .iter()
            .filter(|(c, _)| *c == chat)
            .map(|(_, n)| *n)
            .collect()
    };
    let all: Vec<ChatCounters> = (0..3)
        .map(|chat| {
            let ns = per_chat(chat);
            counters(
                chat,
                ns.len() as u64,
                i64::try_from(1_000 + ns.last().unwrap()).unwrap(),
            )
        })
        .collect();
    let totals = writer.commit(&all, 99).unwrap();
    assert_eq!(totals.messages, 60);
    assert_eq!(totals.chats, 3);
    assert_eq!(totals.shown_chats, 3);
    assert_eq!(totals.contacts, 3);

    let reader = StageReader::open(dir.path(), &key()).unwrap();
    assert_eq!(reader.totals().unwrap(), totals);
    assert_eq!(
        reader.count().unwrap(),
        totals,
        "the rows are what the totals say"
    );
    assert_eq!(reader.frames().unwrap(), 99);
    for chat in 0..3 {
        for page in [1, 4, 500] {
            let rows = rows_of(&reader, chat, page);
            let ns: Vec<u64> = rows.iter().map(|row| row.n).collect();
            assert_eq!(ns, per_chat(chat), "chat {chat} in pages of {page}");
            for row in &rows {
                assert_eq!(row.msg.conv, conv(chat));
                assert_eq!(body_of(&row.msg), format!("chat {chat} frame {}", row.n));
                assert_eq!(
                    row.bytes,
                    serde_json::to_vec(&row.msg).unwrap().len() as u64,
                    "the stored length is the length of the row's JSON"
                );
            }
        }
    }
    // The chats come most recent first, with their counters; the people in staging order.
    let chats = reader.chats().unwrap();
    let mut by_recency: Vec<&ChatCounters> = all.iter().collect();
    by_recency.sort_by_key(|chat| std::cmp::Reverse(chat.last_at));
    assert_eq!(
        chats.iter().map(|chat| chat.counters).collect::<Vec<_>>(),
        by_recency.into_iter().copied().collect::<Vec<_>>()
    );
    assert!(
        chats
            .iter()
            .all(|chat| chat.op.peer == Some(chat.counters.idx))
    );
    let people = reader.contacts(None, 2).unwrap();
    assert_eq!(people.len(), 2);
    assert_eq!(people[1].1, contact(1));
    let rest = reader.contacts(Some(people[1].0), 2).unwrap();
    assert_eq!(rest.iter().map(|(n, _)| *n).collect::<Vec<_>>(), [2]);

    // A byte budget cuts a page short, and never to nothing.
    let first = &rows_of(&reader, 0, 500)[0];
    let two = reader.items(0, None, 500, first.bytes + 1).unwrap();
    assert_eq!(two.len(), 1, "the second row does not fit");
    let tiny = reader.items(0, None, 500, 1).unwrap();
    assert_eq!(tiny.len(), 1, "one row is given whatever its size");
}

#[test]
fn a_stage_opens_only_with_the_key_of_its_transfer() {
    let dir = tempfile::tempdir().unwrap();
    let marker = "a text nobody else may read 7f3a";
    let mut writer = writer(dir.path(), 1, StageLimits::default());
    writer.item(0, 1, &message(0, 1, marker)).unwrap();
    writer.commit(&[counters(0, 1, 1_001)], 4).unwrap();

    // The key is derived, the same every time, and is not the ephemeral key itself.
    assert_eq!(
        key().0.as_bytes(),
        StageKey::derive(&EPHEMERAL).0.as_bytes()
    );
    assert_ne!(key().0.as_bytes(), &EPHEMERAL);
    let mut other = EPHEMERAL;
    other[31] ^= 1;
    assert_ne!(key().0.as_bytes(), StageKey::derive(&other).0.as_bytes());

    // The file is this user's only, holds no plain text, and no other transfer's key opens it.
    let path = path_in(dir.path());
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.len() >= 4096);
    assert!(!bytes.windows(marker.len()).any(|w| w == marker.as_bytes()));
    assert!(!bytes.windows(6).any(|w| w == b"SQLite"));
    assert!(StageReader::open(dir.path(), &StageKey::derive(&other)).is_err());
    assert!(StageReader::open(dir.path(), &StageKey::derive(&[0; 32])).is_err());
    // With its own key the text is there.
    let reader = StageReader::open(dir.path(), &key()).unwrap();
    assert_eq!(body_of(&rows_of(&reader, 0, 10)[0].msg), marker);
}

#[test]
fn a_changed_stage_page_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = writer(dir.path(), 2, StageLimits::default());
    // Rows of a few hundred bytes, and one far larger than a page: it continues on pages
    // that counting rows never reads.
    for n in 1..=200u64 {
        let chat = u32::try_from(n % 2).unwrap();
        writer
            .item(chat, n, &message(chat, n, &"x".repeat(300)))
            .unwrap();
    }
    writer
        .item(0, 201, &message(0, 201, &"y".repeat(40_000)))
        .unwrap();
    writer
        .commit(&[counters(0, 101, 1_201), counters(1, 100, 1_200)], 205)
        .unwrap();
    let path = path_in(dir.path());
    let whole = std::fs::read(&path).unwrap();
    let pages = whole.len() / 4096;
    assert!(pages > 20, "a stage of many pages: {pages}");
    let check = || StageReader::open(dir.path(), &key()).and_then(|reader| reader.count());
    assert!(check().is_ok(), "the untouched stage is whole");

    // One flipped bit in any page, the last one too: refused.
    let mut refused = 0;
    for page in 0..pages {
        let mut damaged = whole.clone();
        damaged[page * 4096 + 2_000] ^= 0x01;
        std::fs::write(&path, &damaged).unwrap();
        assert!(check().is_err(), "a changed page {page} was accepted");
        refused += 1;
    }
    println!("{refused} of {pages} pages: every changed page was refused");
    // A file cut short is refused too.
    std::fs::write(&path, &whole[..whole.len() - 4096]).unwrap();
    assert!(check().is_err());
    std::fs::write(&path, &whole).unwrap();
    assert!(check().is_ok());
}

#[test]
fn a_stage_that_was_never_committed_is_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = writer(dir.path(), 1, StageLimits::default());
    writer.item(0, 1, &message(0, 1, "half")).unwrap();
    // The writer goes away without a commit and without an abort, as with a kill (its
    // open transaction is rolled back; a kill leaves the same or less).
    drop(writer);
    assert!(path_in(dir.path()).exists());
    assert!(StageReader::open(dir.path(), &key()).is_err());

    // A new stage takes its place: the old file is removed first.
    let mut again = StageWriter::create(dir.path(), &key(), StageLimits::default()).unwrap();
    again.contact(0, &contact(0)).unwrap();
    let totals = again.commit(&[], 1).unwrap();
    assert_eq!((totals.messages, totals.contacts), (0, 1));
    assert!(StageReader::open(dir.path(), &key()).is_ok());
    // No directory at all: nothing to open.
    assert_eq!(
        StageReader::open(&dir.path().join("none"), &key()).err(),
        Some(StageError::Io(std::io::ErrorKind::NotFound))
    );
}

#[test]
fn an_aborted_stage_leaves_no_file_and_never_removes_the_one_that_replaced_it() {
    let dir = tempfile::tempdir().unwrap();
    let files = |dir: &Path| -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    };
    let mut first = writer(dir.path(), 1, StageLimits::default());
    first.item(0, 1, &message(0, 1, "gone")).unwrap();
    assert!(files(dir.path()).contains(&FILE.to_owned()));
    first.abort();
    assert_eq!(files(dir.path()), Vec::<String>::new());

    // A writer whose read did not stop in time, and the next read's stage under the same
    // name: the late abort leaves the new stage alone.
    let late = writer(dir.path(), 1, StageLimits::default());
    let mut next = writer(dir.path(), 1, StageLimits::default());
    next.item(0, 1, &message(0, 1, "kept")).unwrap();
    late.abort();
    assert!(files(dir.path()).contains(&FILE.to_owned()));
    next.commit(&[counters(0, 1, 1_001)], 3).unwrap();
    let reader = StageReader::open(dir.path(), &key()).unwrap();
    assert_eq!(body_of(&rows_of(&reader, 0, 10)[0].msg), "kept");
}

/// A directory on the disk the build is on, not in `/tmp`: a file system that gives a freed
/// inode to the next file (ext4 does, tmpfs counts up) is what the test below needs to
/// show the fault it is about.
fn disk_dir() -> tempfile::TempDir {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp");
    std::fs::create_dir_all(&base).unwrap();
    tempfile::tempdir_in(base).unwrap()
}

/// Whether the file system of `dir` gives the inode of a file that was just removed to the
/// next file made there.
fn reuses_inodes(dir: &Path) -> bool {
    let probe = dir.join("probe");
    std::fs::write(&probe, b"").unwrap();
    let first = file_id(&probe);
    std::fs::remove_file(&probe).unwrap();
    std::fs::write(&probe, b"").unwrap();
    let second = file_id(&probe);
    std::fs::remove_file(&probe).unwrap();
    first == second
}

/// F5 of the review of step S1. A read that outlived its stop aborts at the very moment the
/// next read of the account removes its stage and makes a new one. The late abort must know
/// its own file from the new one, and device and inode say that only while the old file is
/// held open: closed first, its inode is free, the new stage gets it (on a file system that
/// reuses inodes) and is removed as "mine".
#[test]
fn a_late_abort_knows_its_file_from_one_made_in_the_same_instant() {
    let dir = disk_dir();
    let reuse = reuses_inodes(dir.path());
    let late = writer(dir.path(), 1, StageLimits::default());
    let next: std::rc::Rc<std::cell::RefCell<Option<StageWriter>>> = Default::default();
    {
        let next = next.clone();
        let dir = dir.path().to_owned();
        AT_REMOVE_OWN.with(|step| {
            *step.borrow_mut() = Some(Box::new(move || {
                // The next read: `create` removes what is there and makes its own stage.
                *next.borrow_mut() = Some(writer(&dir, 1, StageLimits::default()));
            }));
        });
    }
    late.abort();
    let mut next = next.borrow_mut().take().expect("the next read began");
    println!(
        "this file system gives a freed inode to the next file: {reuse} (the fault shows \
         only where it does)"
    );
    assert!(
        path_in(dir.path()).exists(),
        "the late abort removed the stage of the next read"
    );
    next.item(0, 1, &message(0, 1, "kept")).unwrap();
    next.commit(&[counters(0, 1, 1_001)], 3).unwrap();
    let reader = StageReader::open(dir.path(), &key()).unwrap();
    assert_eq!(reader.count().unwrap(), reader.totals().unwrap());
    assert_eq!(body_of(&rows_of(&reader, 0, 10)[0].msg), "kept");
}

#[test]
fn a_stage_over_its_cap_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let limits = StageLimits {
        max_bytes: 20_000,
        commit_every: 4_000,
    };
    let mut writer = writer(dir.path(), 1, limits);
    let mut written = 0;
    let end = loop {
        match writer.item(0, written + 1, &message(0, written + 1, &"z".repeat(500))) {
            Ok(()) => written += 1,
            Err(e) => break e,
        }
        assert!(written < 1_000, "the cap never came");
    };
    assert_eq!(end, StageError::TooLarge);
    assert!(written > 10, "{written} rows fit under the cap");
    writer.abort();
    assert!(!path_in(dir.path()).exists());
}

/// The rows go in transactions of a bounded size, so the stage's own write-ahead log holds
/// a piece of the history, never all of it.
#[test]
fn the_wal_stays_bounded_while_a_large_stage_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let limits = StageLimits {
        max_bytes: MAX_BYTES,
        commit_every: 256 * 1024,
    };
    let mut writer = writer(dir.path(), 20, limits);
    let wal = dir.path().join(format!("{FILE}-wal"));
    let body = "w".repeat(1_500);
    let mut largest = 0;
    let mut payload = 0u64;
    let mut n = 0u64;
    while payload < 24 * 1024 * 1024 {
        n += 1;
        let chat = u32::try_from(n % 20).unwrap();
        let msg = message(chat, n, &body);
        payload += serde_json::to_vec(&msg).unwrap().len() as u64;
        writer.item(chat, n, &msg).unwrap();
        if n.is_multiple_of(64) {
            largest = largest.max(std::fs::metadata(&wal).map_or(0, |meta| meta.len()));
        }
    }
    let all: Vec<ChatCounters> = (0..20).map(|chat| counters(chat, n / 20, 1)).collect();
    let totals = writer.commit(&all, n).unwrap();
    let file = std::fs::metadata(path_in(dir.path())).unwrap().len();
    println!(
        "{n} rows, {} MiB of payload: the stage file is {} MiB, its WAL was at most {} KiB",
        payload / (1024 * 1024),
        file / (1024 * 1024),
        largest / 1024
    );
    assert_eq!(totals.messages, n);
    assert!(totals.bytes >= payload);
    // SQLite moves the WAL into the database once a commit finds it over 1000 pages
    // (4 MiB): the log is that plus one transaction at most, whatever the history's size.
    assert!(
        largest < 8 * 1024 * 1024,
        "the WAL grew to {largest} bytes for {payload} bytes of payload"
    );
    assert!(file > payload, "the rows are in the database file");
    assert_eq!(
        std::fs::metadata(&wal).map_or(0, |meta| meta.len()),
        0,
        "the commit leaves nothing in the WAL"
    );
}

/// Review of step S3a (which made `FORMAT` 2): a committed stage whose `meta.format` is
/// another one is refused by the reader, so a stage an earlier mapper wrote is never
/// delivered under rules its rows were not made for (the delivery then forgets it and the
/// archive is read again). No test held the check: with it removed every test passed.
#[test]
fn review_a_stage_of_another_format_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = writer(dir.path(), 1, StageLimits::default());
    writer
        .item(0, 1, &message(0, 1, "of an earlier mapper"))
        .unwrap();
    writer.commit(&[counters(0, 1, 1_001)], 4).unwrap();
    assert!(StageReader::open(dir.path(), &key()).is_ok());
    {
        let conn = kourier_db::open(&path_in(dir.path()), &key().0).unwrap();
        let changed = conn
            .execute("UPDATE meta SET v = ?1 WHERE k = 'format'", [FORMAT - 1])
            .unwrap();
        assert_eq!(changed, 1);
    }
    assert_eq!(
        StageReader::open(dir.path(), &key()).err(),
        Some(StageError::Unreadable)
    );
}

/// Probes of the independent review of step S1.
mod review;

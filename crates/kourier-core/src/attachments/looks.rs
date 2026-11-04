//! The looks of outgoing files, kept by file (LOC, critic F26): the composer's plan is asked
//! again on every settled caption, reply and "send as file" toggle, none of which changes
//! whether a file carries where it was taken. A look is kept under the file's identity and
//! version as the file system states it (device, inode, size, modification and change time),
//! never under its path or its plan, so a file written again, renamed over or touched is
//! looked at anew. `Unreadable` is never kept: a look cut by its time bound on a cold disk is
//! tried again. The entry also holds whether a clean of the file failed (critic F13), which
//! makes the verdict ask.
//!
//! Bounded: at most [`CAPACITY`] files, the oldest dropped first. Nothing of a file's content
//! is kept, only its look (which names a format and a kind of proof, never a value).

use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, Metadata};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use kourier_media::{Look, ReadSeek};

/// How many files the cache holds.
pub(crate) const CAPACITY: usize = 4096;

/// A file's identity and version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FileKey {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl FileKey {
    /// The key of a file from its metadata; `None` where the platform states no inode (the
    /// look is then not kept).
    #[cfg(unix)]
    pub(crate) fn of(meta: &Metadata) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        Some(Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.len(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        })
    }

    #[cfg(not(unix))]
    pub(crate) fn of(_: &Metadata) -> Option<Self> {
        None
    }
}

/// [`kourier_media::look`] of `r` (`len` bytes, blocking), and one log line when the answer is
/// `Unreadable` (LOC P4): a file the look could not judge within its bounds (2 s, 2 MiB read,
/// a damaged or cut file; a slow disk or a network mount makes it the common answer) leaves
/// unasked (BQ6), so how often it happens must be seen. The line names the reason (`time` or
/// `bound`) and the format; never a path, a name or a value.
pub(crate) fn look_noted(r: &mut dyn ReadSeek, len: u64) -> Look {
    let (look, why) = kourier_media::look_why(r, len);
    if let Some(why) = why {
        tracing::info!(
            reason = why.reason(),
            format = why.format_name(),
            "a file could not be judged for where it was taken; it is treated as not located"
        );
    }
    look
}

/// What is kept of one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Looked {
    /// Its look.
    pub look: Look,
    /// A clean of it failed: the verdict asks as if the network cleaned nothing (critic F13).
    pub clean_failed: bool,
}

#[derive(Default)]
struct Inner {
    map: BTreeMap<FileKey, Looked>,
    order: VecDeque<FileKey>,
}

/// The cache ([`LOOKS`] is the core's one).
pub(crate) struct LookCache {
    inner: Mutex<Inner>,
    hits: AtomicU64,
}

/// The core's cache: a look is a property of the file, whichever core instance asks.
pub(crate) static LOOKS: LookCache = LookCache::new();

impl LookCache {
    pub(crate) const fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                map: BTreeMap::new(),
                order: VecDeque::new(),
            }),
            hits: AtomicU64::new(0),
        }
    }

    /// The look of the opened `file` whose metadata (of the same descriptor) is `meta`: kept,
    /// or looked at now (blocking) and kept. The file's position afterwards is unspecified.
    pub(crate) fn look(&self, file: &mut File, meta: &Metadata) -> Looked {
        let key = FileKey::of(meta);
        if let Some(key) = key
            && let Some(kept) = self.lock().map.get(&key).copied()
        {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return kept;
        }
        let looked = Looked {
            look: look_noted(file, meta.len()),
            clean_failed: false,
        };
        if let Some(key) = key
            && looked.look != Look::Unreadable
        {
            self.insert(key, looked);
        }
        looked
    }

    /// The look of the opened `file` looked at NOW, never answered from the cache and never kept
    /// in it (LOC P6: the import judges the bytes it is about to read, and the key cannot see a
    /// rewrite in place within a file system's time step). When this look is `Unreadable`, the
    /// kept look of the same key, if there is one, decides (it was made of the same file as far
    /// as the file system can tell); else the answer stays `Unreadable`. A failed clean is
    /// carried over only when the kept look is the one found now.
    pub(crate) fn look_fresh(&self, file: &mut File, meta: &Metadata) -> Looked {
        let kept = FileKey::of(meta).and_then(|key| self.lock().map.get(&key).copied());
        let look = look_noted(file, meta.len());
        match (look, kept) {
            (Look::Unreadable, Some(kept)) => kept,
            (look, kept) => Looked {
                look,
                clean_failed: kept.is_some_and(|k| k.clean_failed && k.look == look),
            },
        }
    }

    /// Remembers that a clean of the file failed, so the next verdict asks (critic F13; M1
    /// calls it when its clean at import fails).
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "M1 marks a failed clean (critic F13)")
    )]
    pub(crate) fn mark_clean_failed(&self, meta: &Metadata, look: Look) {
        if let Some(key) = FileKey::of(meta) {
            self.insert(
                key,
                Looked {
                    look,
                    clean_failed: true,
                },
            );
        }
    }

    /// How many looks were answered from the cache.
    #[cfg_attr(not(test), expect(dead_code, reason = "read by the tests"))]
    pub(crate) fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    fn insert(&self, key: FileKey, looked: Looked) {
        let mut inner = self.lock();
        if inner.map.insert(key, looked).is_none() {
            inner.order.push_back(key);
            while inner.order.len() > CAPACITY {
                if let Some(old) = inner.order.pop_front() {
                    inner.map.remove(&old);
                }
            }
        }
    }

    /// Puts `look` under the key of `meta`, as if it had been looked at (tests plant a stale one).
    #[cfg(test)]
    pub(crate) fn plant(&self, meta: &Metadata, look: Look) {
        if let Some(key) = FileKey::of(meta) {
            self.insert(
                key,
                Looked {
                    look,
                    clean_failed: false,
                },
            );
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kourier_media::{SampleLocated, sample_located};

    fn opened(path: &std::path::Path) -> (File, Metadata) {
        let f = File::open(path).expect("open");
        let m = f.metadata().expect("meta");
        (f, m)
    }

    #[test]
    fn an_unchanged_file_is_answered_from_the_cache_and_a_changed_one_is_looked_at_anew() {
        let cache = LookCache::new();
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("clip.mp4");
        std::fs::write(&p, sample_located(SampleLocated::Mp4)).expect("write");
        let (mut f, m) = opened(&p);
        assert!(matches!(cache.look(&mut f, &m).look, Look::Found(_)));
        assert_eq!(cache.hits(), 0);
        let (mut f, m) = opened(&p);
        assert!(matches!(cache.look(&mut f, &m).look, Look::Found(_)));
        assert_eq!(cache.hits(), 1, "the same file, unchanged: a hit");
        // Written again with other content: a new version, looked at anew.
        std::fs::write(&p, b"no location here").expect("write");
        let (mut f, m) = opened(&p);
        assert_eq!(cache.look(&mut f, &m).look, Look::None);
        assert_eq!(cache.hits(), 1);
        // A failed clean is kept with the look.
        cache.mark_clean_failed(&m, Look::None);
        let (mut f, m) = opened(&p);
        assert!(cache.look(&mut f, &m).clean_failed);
    }

    #[test]
    fn the_cache_is_bounded() {
        let cache = LookCache::new();
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("x");
        std::fs::write(&p, b"x").expect("write");
        let m = std::fs::metadata(&p).expect("meta");
        let key = FileKey::of(&m).expect("unix");
        for n in 0..(CAPACITY as u64 + 10) {
            cache.insert(
                FileKey { ino: n, ..key },
                Looked {
                    look: Look::None,
                    clean_failed: false,
                },
            );
        }
        let inner = cache.lock();
        assert_eq!(inner.map.len(), CAPACITY);
        assert_eq!(inner.order.len(), CAPACITY);
        assert!(!inner.map.contains_key(&FileKey { ino: 0, ..key }));
    }
    /// A log writer into a shared buffer.
    #[derive(Clone, Default)]
    struct Sink(std::sync::Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("sink").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// LOC P4: an `Unreadable` look leaves one log line with the reason and the format, and
    /// nothing of the file (no path, no name); an answered look leaves none.
    #[test]
    fn an_unreadable_look_leaves_one_log_safe_line() {
        let dir = tempfile::tempdir().expect("dir");
        let secret = dir.path().join("private-name-of-a-file.png");
        let mut cut = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        cut.extend([0x00, 0x10, 0x00, 0x00, b'I', b'D', b'A', b'T', 1, 2, 3]);
        std::fs::write(&secret, &cut).expect("write");
        let plain = dir.path().join("plain.txt");
        std::fs::write(&plain, b"words").expect("write");
        let sink = Sink::default();
        let out = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || out.clone())
            .with_ansi(false)
            .finish();
        let cache = LookCache::new();
        let (a, b) = tracing::subscriber::with_default(subscriber, || {
            let (mut f, m) = opened(&secret);
            let a = cache.look(&mut f, &m).look;
            let (mut f, m) = opened(&plain);
            let b = cache.look(&mut f, &m).look;
            (a, b)
        });
        assert_eq!((a, b), (Look::Unreadable, Look::None));
        let log = String::from_utf8(sink.0.lock().expect("sink").clone()).expect("utf8");
        assert_eq!(log.lines().count(), 1, "{log}");
        assert!(
            log.contains("reason=\"bound\"") && log.contains("format=\"png\""),
            "{log}"
        );
        assert!(
            !log.contains("private-name") && !log.contains("plain.txt"),
            "{log}"
        );
        assert!(!log.contains(dir.path().to_str().expect("utf8")), "{log}");
    }

    /// LOC P6: a fresh look is never answered from the cache nor kept in it; an unreadable one
    /// defers to the kept look of the same key.
    #[test]
    fn a_fresh_look_skips_the_cache_and_defers_when_unreadable() {
        let cache = LookCache::new();
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("clip.mp4");
        std::fs::write(&p, sample_located(SampleLocated::Mp4)).expect("write");
        let (mut f, m) = opened(&p);
        cache.plant(&m, Look::None);
        assert!(matches!(cache.look_fresh(&mut f, &m).look, Look::Found(_)));
        assert_eq!(cache.hits(), 0, "the cache was not asked");
        // Not kept either: a plain file stays unknown to the cache.
        let q = dir.path().join("plain");
        std::fs::write(&q, b"words").expect("write");
        let (mut f, m) = opened(&q);
        assert_eq!(cache.look_fresh(&mut f, &m).look, Look::None);
        let key = FileKey::of(&m).expect("unix");
        assert!(!cache.lock().map.contains_key(&key));
    }
}

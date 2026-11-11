//! BQ11's counts line (A10d D8; the lead's calls H3 and M1 on the A10d critic): what THIS PROCESS
//! saw of the Storage Service, for the lead's later real check on a test profile with one Signal
//! account (`cargo xtask smoke state` prints the line; the dev control's `state.storage` carries
//! it, debug builds only). Counts only: no name, identifier, key or byte, ever.
//!
//! - `version`, `identifiers`: the version and the identifier count of the newest manifest this
//!   process SAW, set where the shared merge path (`storage::merge_manifest`, the job's and the
//!   sync's one place) has decrypted a fetched manifest AND at a 200 of our own write (the written
//!   version and the written count, M1: a 200 queues no fetch, so without it the line would stay one
//!   version behind the server after every write of ours).
//! - `unknown`: how many identifiers of that manifest had no record in `backend.db` when it was
//!   seen (H3): the merge's own "not stored yet" number for a manifest newer than the merged
//!   version (the first read of a fresh profile has `unknown == identifiers`: the baseline), the
//!   identifiers not stored for one that is not newer, and 0 at a 200 (`land_written` stored what
//!   was written). A phone's force push (every identifier new, the version one higher, the count
//!   unchanged) shows as `unknown` near `identifiers`: the stop rule of the real check.
//! - `writes`: how many of our PUTs were answered 200 (any 2xx: the job's success path).
//!
//! Process-wide on purpose (one Signal account on the check's test profile), like
//! `kourier_whatsapp::wam_sent`. [`storage_counts`] is `None` until a manifest was read in this
//! process: "not measured" is never a row of zeros. The four values are written together under a
//! sequence number (a seqlock), so a reader never mixes the version of one manifest with the count
//! of another; a reader waits for nothing (it retries a few times and is never blocked).

use std::sync::atomic::{AtomicU64, Ordering};

/// Even while no update is under way, odd during one; 0 = nothing seen yet in this process.
static SEQ: AtomicU64 = AtomicU64::new(0);
/// The newest manifest's version.
static VERSION: AtomicU64 = AtomicU64::new(0);
/// The identifiers it lists.
static IDENTIFIERS: AtomicU64 = AtomicU64::new(0);
/// Its identifiers with no record in `backend.db` when it was seen.
static UNKNOWN: AtomicU64 = AtomicU64::new(0);
/// PUTs of ours answered 200.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// How often a reader tries again while an update is under way before it takes what it read.
const READ_TRIES: usize = 16;

/// The counts of [`storage_counts`]: numbers only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageCounts {
    /// The version of the newest manifest this process saw (fetched or written).
    pub version: u64,
    /// How many identifiers that manifest lists.
    pub identifiers: u64,
    /// How many of them had no record in `backend.db` when it was seen (0 after our own write).
    pub unknown: u64,
    /// How many of our writes were answered 200 in this process.
    pub writes: u64,
}

/// What this process saw of the Storage Service (BQ11's debug-only counts line; the module
/// `storage::counts` says what each number is): `None` until a manifest was read (decrypted) or
/// written in this process. Never blocks.
pub fn storage_counts() -> Option<StorageCounts> {
    let mut read = None;
    for _ in 0..READ_TRIES {
        let before = SEQ.load(Ordering::SeqCst);
        if before == 0 {
            return None;
        }
        let counts = StorageCounts {
            version: VERSION.load(Ordering::SeqCst),
            identifiers: IDENTIFIERS.load(Ordering::SeqCst),
            unknown: UNKNOWN.load(Ordering::SeqCst),
            writes: WRITES.load(Ordering::SeqCst),
        };
        read = Some(counts);
        if before.is_multiple_of(2) && SEQ.load(Ordering::SeqCst) == before {
            break;
        }
        std::hint::spin_loop();
    }
    read
}

/// A fetched manifest was decrypted: its version, its identifier count and how many of its
/// identifiers have no record here.
pub(crate) fn seen(version: u64, identifiers: usize, unknown: usize) {
    update(version, identifiers, unknown, false);
}

/// Our write of `version` listing `identifiers` was answered 200: the newest manifest is the
/// written one, every record it names that we wrote is stored, and one more write counts.
pub(crate) fn written(version: u64, identifiers: usize) {
    update(version, identifiers, 0, true);
}

fn update(version: u64, identifiers: usize, unknown: usize, write: bool) {
    SEQ.fetch_add(1, Ordering::SeqCst);
    VERSION.store(version, Ordering::SeqCst);
    IDENTIFIERS.store(as_count(identifiers), Ordering::SeqCst);
    UNKNOWN.store(as_count(unknown), Ordering::SeqCst);
    if write {
        WRITES.fetch_add(1, Ordering::SeqCst);
    }
    SEQ.fetch_add(1, Ordering::SeqCst);
}

fn as_count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

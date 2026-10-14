//! `cargo xtask perf`: measures the quality targets of architecture §1.3 on this machine and
//! prints one table against their budgets.
//!
//! What runs:
//!
//! 1. `cargo build --release -p kourier --features perf` (a measuring build: dumpable, with
//!    the reference profile and a scripted session; copied to `target/perf/kourier-perf`), then
//!    the normal release build, whose size is reported and which must be non-dumpable. A
//!    `--quick` or `--start` run builds the measuring binary only (two rows of the table need
//!    the plain one and are "not measured") and removes `target/release/kourier` after the
//!    copy, because what is there is then the measuring build, which must not stay under the
//!    normal binary's name.
//! 2. The reference profile (`target/perf/profile`): about 200 conversations and 50,000
//!    messages on the fake network, a share of them pictures and files, seeded through the real
//!    core. Seeding is the "catch-up ingest" measurement on the fake network. It is seeded
//!    again only when the code that fills it changed ([`profile_digest`]), not whenever the
//!    binary was relinked.
//! 3. A private D-Bus session with a mock notification server and tray host
//!    (`kourier-desktop`'s `mock_session` example): the app runs with its desktop services on,
//!    and nothing reaches the real session. Every child process gets configuration, cache, data
//!    and state directories below `target/perf/env`: nothing is read from or written to the
//!    user's own.
//! 4. Offscreen runs of the measuring build on copies of the profile: several starts as a user
//!    starts it (no scripted session in them), a few more with Kourier's own files evicted from
//!    the page cache, several starts with one live message (receive → notification), one
//!    session (open conversations, flick through an image-heavy chat, idle with the window
//!    shown, hidden to the tray and released; each idle time sampled in parts).
//! 5. Optionally the Signal mock catch-up test of `kourier-e2e` in release mode, several times.
//!
//! `--start` measures steps 1, 2 and the first two lines of step 4 only (the warm starts and
//! one start with the files out of the page cache), to answer "did my change move the start-up?".
//! Every run ends with the wall time of its parts ([`Parts`]).
//!
//! Every number in the table says how it was taken and from how many runs.
//!
//! The budgets are those of architecture §1.3 as the user confirmed them on 2026-10-07. Each
//! line of the table is one number against one limit and its budget cell says what is
//! enforced; a line marked "for information" never fails a run.
//!
//! How a run ends: status 0 when every budget is met (or `--report-only` was given), status
//! [`EXIT_MISSED`] when the measurements worked and a budget is missed, status 1 when a
//! measurement itself failed (an app run that crashed or never reached a mark, a test that
//! failed): then there is no verdict, and `--report-only` does not hide it.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use super::{cargo, qml_problems, root, run, target_dir, theme_home};

const USAGE: &str = "usage: cargo xtask perf [--out <file>] [--quick] [--start] [--report-only] \
[--idle <seconds>] [--runs <n>] [--scale <factor>] [--no-mock] [--no-build] [--keep-profile]

  --out <file>     write the report there too (default: target/perf/report.md; a --start
                   run: target/perf/report-start.md)
  --quick          3 starts instead of 10, 10 s of idle instead of 300, the window released
                   after 3 s instead of 60, no Signal mock test. Builds the measuring binary
                   only: \"Binary size\" and \"Release build is non-dumpable\" are not measured
                   (a full run builds and checks the plain release binary), and
                   target/release/kourier is removed (it would be the measuring build)
  --start          only the starts: warm ones and one with the files out of the page cache (5
                   starts unless --runs says otherwise); no notification starts, no session,
                   no mock test, and the build rule of --quick. For \"did my change move the
                   start-up?\"; every other row is \"not measured\"
  --report-only    exit 0 even when a budget is missed
  --idle <s>       idle time measured with the window shown and again hidden (default 300;
                   below 300 the idle CPU lines are for information: their budgets are
                   per 5 min, so --quick does not judge them); not with --start
  --runs <n>       starts measured (default 10)
  --scale <x>      the display scale of every app start (QT_SCALE_FACTOR; default 1, 0.5 to 4):
                   --scale 1.25 is the user's 125 % display, where tall pictures are drawn from
                   the file (P1); the report says which scale it measured
  --no-mock        skip the catch-up test on the Signal mock server
  --no-build       measure the binaries as they are in target/
  --keep-profile   reuse target/perf/profile even when the code that fills it changed

The reference profile is seeded again (about 45 s) only when the sources that decide what it
holds changed (kourier-core, -db, -blob, -media, -fake, -model, -backend-api, kourier-app,
Cargo.toml, Cargo.lock, rust-toolchain.toml), not when the binary was relinked; a seeding
by a binary the run did not build (--no-build) is redone by the next run that builds. The
measuring build never stays as target/release/kourier: after a --quick or --start run that
file is gone until the next release build. A run ends by saying where its wall time went.

exit status: 0 every budget met (or --report-only); 3 a budget missed; 1 a measurement failed
(a run that crashed or never reached a mark, a failed test: no verdict, also with --report-only)";

/// The exit status of a run whose measurements worked and that missed a budget (a failed
/// measurement is status 1, like every other error of `cargo xtask`).
pub const EXIT_MISSED: i32 = 3;

/// What starts every trace line of the app (`kourier_app::trace::LINE_PREFIX`).
const TRACE_PREFIX: &str = "kourier-trace: ";

/// Free disk space below which nothing is built (the workspace rule after 2026-10-06).
const MIN_FREE_GB: u64 = 25;

/// The window size of every run.
const WINDOW: &str = "1200x800";

/// The length of the sleep before the live message (ms): an odd number no other timer uses.
const LIVE_AFTER_MS: u64 = 1777;

/// What precedes the fields of a timing event of the core and the backends in the app's log
/// (`kourier_core::PERF_TARGET`, `kourier_signal::PERF_TARGET`).
const PERF_EVENT: &str = " kourier::perf: ";

/// The log filter that lets those events through (`KOURIER_LOG`).
const PERF_LOG_FILTER: &str = "warn,kourier::perf=trace";

/// Parts each idle time is sampled in at least: a burst shows as one busy part, a periodic
/// wake-up in every part.
const IDLE_PARTS: u32 = 3;

/// The idle time §1.3's idle CPU budgets are about (s): 5 min. A shorter idle time cannot
/// show them.
pub const IDLE_BUDGET_S: f64 = 300.0;

/// The length of a part that §1.3's "no 100 s part above 0.5 wake-ups per second" names (s).
const IDLE_PART_S: u64 = 100;

/// Parts an idle time of `idle_s` seconds is sampled in: [`IDLE_PARTS`], and more for a
/// longer time, so that no part is much longer than the 100 s the budget names (300 s: 3
/// parts of 100 s; 600 s: 6).
pub fn idle_parts(idle_s: u64) -> u32 {
    u32::try_from(idle_s / IDLE_PART_S)
        .unwrap_or(u32::MAX)
        .max(IDLE_PARTS)
}

/// Starts measured with Kourier's own files evicted from the page cache (`--quick`: 1).
const COLD_STARTS: usize = 3;

/// Runs of the Signal mock catch-up test.
const MOCK_RUNS: usize = 3;

/// Rows a conversation must have loaded for its `timeline.open` to count (§1.3: "first 50
/// rows visible").
const OPEN_ROWS: f64 = 50.0;

/// The command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// Where the report is written besides stdout.
    pub out: Option<PathBuf>,
    /// The short run.
    pub quick: bool,
    /// Only the starts (warm, and one with the files out of the page cache).
    pub start: bool,
    /// Never fail on a missed budget.
    pub report_only: bool,
    /// Idle seconds per state.
    pub idle_s: u64,
    /// Measured starts.
    pub runs: usize,
    /// Run the Signal mock catch-up test.
    pub mock: bool,
    /// Build first.
    pub build: bool,
    /// Reuse the profile even when the code that fills it changed.
    pub keep_profile: bool,
    /// The display scale of every app start (`QT_SCALE_FACTOR`), as the user wrote it.
    pub scale: String,
}

impl Options {
    /// Whether the run builds, measures and judges the plain release binary ("Binary size",
    /// "Release build is non-dumpable"): only a full run does. A quick or start-only run
    /// builds the measuring binary alone.
    pub fn measures_plain_binary(&self) -> bool {
        !self.quick && !self.start
    }
}

/// Starts of a `--start` run when `--runs` does not say.
const START_RUNS: usize = 5;

/// Parses the arguments of `cargo xtask perf`.
pub fn parse_options(args: &[String]) -> Result<Options> {
    let mut o = Options {
        out: None,
        quick: false,
        start: false,
        report_only: false,
        idle_s: 300,
        runs: 10,
        mock: true,
        build: true,
        keep_profile: false,
        scale: "1".to_owned(),
    };
    let mut idle = None;
    let mut runs = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .with_context(|| format!("{name} needs a value\n{USAGE}"))
        };
        match arg.as_str() {
            "--out" => o.out = Some(PathBuf::from(value("--out")?)),
            "--quick" => o.quick = true,
            "--start" => o.start = true,
            "--report-only" => o.report_only = true,
            "--no-mock" => o.mock = false,
            "--no-build" => o.build = false,
            "--keep-profile" => o.keep_profile = true,
            "--scale" => {
                let v = value("--scale")?;
                v.parse::<f64>()
                    .ok()
                    .filter(|x| (0.5..=4.0).contains(x))
                    .with_context(|| format!("bad --scale {v:?} (0.5 to 4)"))?;
                o.scale = v;
            }
            "--idle" => {
                let v = value("--idle")?;
                idle = Some(
                    v.parse::<u64>()
                        .ok()
                        .filter(|s| (2..=3600).contains(s))
                        .with_context(|| format!("bad --idle {v:?} (2 to 3600 seconds)"))?,
                );
            }
            "--runs" => {
                let v = value("--runs")?;
                runs = Some(
                    v.parse::<usize>()
                        .ok()
                        .filter(|n| (1..=100).contains(n))
                        .with_context(|| format!("bad --runs {v:?} (1 to 100)"))?,
                );
            }
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    if o.start {
        if idle.is_some() {
            bail!("--start measures no idle time, so --idle does not apply\n{USAGE}");
        }
        o.runs = START_RUNS;
        o.mock = false;
    }
    if o.quick {
        o.idle_s = 10;
        o.runs = 3;
        o.mock = false;
    }
    // An explicit value wins over --quick and --start.
    o.idle_s = idle.unwrap_or(o.idle_s);
    o.runs = runs.unwrap_or(o.runs);
    Ok(o)
}

// ---------------------------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------------------------

/// One timing mark of the app.
#[derive(Clone, Debug, PartialEq)]
pub struct Mark {
    /// The mark's name.
    pub name: String,
    /// Its fields (`at_ms` among them).
    pub fields: BTreeMap<String, f64>,
}

impl Mark {
    /// A field's value.
    pub fn get(&self, key: &str) -> Option<f64> {
        self.fields.get(key).copied()
    }
}

/// Parses `kourier-trace: <name> key=number …`; `None` for any other line.
pub fn parse_trace_line(line: &str) -> Option<Mark> {
    let rest = line.trim_end().strip_prefix(TRACE_PREFIX)?;
    let mut words = rest.split_whitespace();
    let name = words.next()?.to_owned();
    let mut fields = BTreeMap::new();
    for word in words {
        let (key, value) = word.split_once('=')?;
        fields.insert(key.to_owned(), value.parse::<f64>().ok()?);
    }
    Some(Mark { name, fields })
}

/// Parses a timing event of the core or a backend from the app's log:
/// `<time> TRACE kourier::perf: name="recv.commit" account=1 events=1 took_us=4321`. The
/// name is the `name` field; every other field must be a number (a mark never carries text).
/// `None` for any other line.
pub fn parse_perf_event(line: &str) -> Option<Mark> {
    let rest = line.split_once(PERF_EVENT)?.1;
    let mut name = None;
    let mut fields = BTreeMap::new();
    for word in rest.split_whitespace() {
        let (key, value) = word.split_once('=')?;
        if key == "name" {
            name = Some(value.trim_matches('"').to_owned());
        } else {
            fields.insert(key.to_owned(), value.parse::<f64>().ok()?);
        }
    }
    Some(Mark {
        name: name.filter(|n| !n.is_empty())?,
        fields,
    })
}

/// `/proc/<pid>/smaps_rollup`, in kB.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rollup {
    /// Resident set size.
    pub rss_kb: u64,
    /// Proportional set size: what the budgets are about.
    pub pss_kb: u64,
    /// Anonymous part of the PSS (heaps, stacks).
    pub pss_anon_kb: u64,
    /// File-backed part (code and data of the binary and its libraries, mapped files).
    pub pss_file_kb: u64,
    /// Shared-memory part.
    pub pss_shmem_kb: u64,
    /// Swapped-out part (not in `pss_kb`).
    pub swap_pss_kb: u64,
}

fn kb_value(line: &str, key: &str) -> Option<u64> {
    let rest = line.strip_prefix(key)?.strip_prefix(':')?;
    rest.trim().strip_suffix("kB")?.trim().parse().ok()
}

/// Parses `smaps_rollup`; `None` when it has no `Pss:` line.
pub fn parse_rollup(text: &str) -> Option<Rollup> {
    let mut r = Rollup::default();
    let mut seen = false;
    for line in text.lines() {
        if let Some(v) = kb_value(line, "Rss") {
            r.rss_kb = v;
        } else if let Some(v) = kb_value(line, "Pss") {
            r.pss_kb = v;
            seen = true;
        } else if let Some(v) = kb_value(line, "Pss_Anon") {
            r.pss_anon_kb = v;
        } else if let Some(v) = kb_value(line, "Pss_File") {
            r.pss_file_kb = v;
        } else if let Some(v) = kb_value(line, "Pss_Shmem") {
            r.pss_shmem_kb = v;
        } else if let Some(v) = kb_value(line, "SwapPss") {
            r.swap_pss_kb = v;
        }
    }
    seen.then_some(r)
}

/// Whether `line` starts a mapping in `/proc/<pid>/smaps` (`<from>-<to> perms offset dev inode
/// [path]`); returns the path, or a name for an anonymous mapping.
fn mapping_header(line: &str) -> Option<String> {
    let mut words = line.split_whitespace();
    let range = words.next()?;
    let (from, to) = range.split_once('-')?;
    if from.is_empty()
        || !from.bytes().all(|b| b.is_ascii_hexdigit())
        || !to.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    let perms = words.next()?;
    if perms.len() != 4 {
        return None;
    }
    let path = words.nth(3).map(|first| {
        // A path may hold spaces: the rest of the line belongs to it.
        std::iter::once(first)
            .chain(words)
            .collect::<Vec<_>>()
            .join(" ")
    });
    Some(match path {
        Some(p) => p,
        None => "[anonymous]".to_owned(),
    })
}

/// Groups a mapping by what it is: one library, the binary, the heap, anonymous memory.
fn mapping_group(path: &str) -> String {
    if path.starts_with('[') {
        return path.to_owned();
    }
    let file = path.rsplit('/').next().unwrap_or(path);
    let file = file.strip_suffix(" (deleted)").unwrap_or(file);
    // libQt6Quick.so.6.11.0 → libQt6Quick
    match file.split_once(".so") {
        Some((stem, _)) => stem.to_owned(),
        None => file.to_owned(),
    }
}

/// The PSS (kB) of `/proc/<pid>/smaps` per group of mappings, largest first.
pub fn parse_smaps(text: &str) -> Vec<(String, u64)> {
    let mut groups: BTreeMap<String, u64> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(path) = mapping_header(line) {
            current = Some(mapping_group(&path));
        } else if let (Some(group), Some(kb)) = (&current, kb_value(line, "Pss")) {
            *groups.entry(group.clone()).or_default() += kb;
        }
    }
    let mut out: Vec<(String, u64)> = groups.into_iter().filter(|(_, kb)| *kb > 0).collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// What a process has resident of some libraries and how much of that it shares with other
/// processes, from `/proc/<pid>/smaps` (kB).
///
/// PSS divides a page among the processes that map it, so a program's PSS depends on what
/// else runs: a library nobody else maps counts in full.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LibraryShare {
    /// Resident pages.
    pub resident_kb: u64,
    /// Of those, the pages another process maps too.
    pub shared_kb: u64,
    /// What the resident pages count for this process: its share of each page.
    pub pss_kb: u64,
}

impl LibraryShare {
    /// The share (0 … 1) of the resident pages that other processes carry: what the PSS
    /// leaves out. `None` when nothing is resident.
    pub fn carried_by_others(&self) -> Option<f64> {
        if self.resident_kb == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        let share = self.resident_kb.saturating_sub(self.pss_kb) as f64 / self.resident_kb as f64;
        Some(share)
    }
}

/// Adds up the mappings of `/proc/<pid>/smaps` whose group `wanted` accepts (a group is a
/// name as [`parse_smaps`] gives it: `libQt6Core`). `None` when there is no such mapping.
pub fn parse_smaps_sharing(text: &str, wanted: impl Fn(&str) -> bool) -> Option<LibraryShare> {
    let mut inside = false;
    let mut seen = false;
    let mut out = LibraryShare::default();
    for line in text.lines() {
        if let Some(path) = mapping_header(line) {
            inside = wanted(&mapping_group(&path));
            seen |= inside;
        } else if inside {
            if let Some(kb) = kb_value(line, "Rss") {
                out.resident_kb += kb;
            } else if let Some(kb) = kb_value(line, "Pss") {
                out.pss_kb += kb;
            } else if let Some(kb) =
                kb_value(line, "Shared_Clean").or_else(|| kb_value(line, "Shared_Dirty"))
            {
                out.shared_kb += kb;
            }
        }
    }
    seen.then_some(out)
}

/// What the groups of Qt's own libraries begin with (`libQt6Core`, `libQt6Quick`, …): how
/// much of them other processes carry says which PSS limit a session is held to
/// ([`Sharing`]).
const QT_LIBRARIES: &str = "libQt6";

/// The one Qt library the report also names alone.
const QT_CORE: &str = "libQt6Core";

/// Which of §1.3's two PSS limits an idle time is held to: what the session it ran in does
/// with the Qt libraries.
///
/// The line is a half: the lower limit applies when other processes carry half or more of
/// the Qt 6 libraries' resident pages (their PSS is at most half of their RSS), the higher
/// one when they carry less.
///
/// Why this measure and this number. The two limits are 20 MB (shown) and 35 MB (hidden)
/// apart: the lower one is for a session whose programs map the same Qt files, the higher
/// one for a session that shares no library page. PSS divides a page among the processes
/// that map it, so what tells the two apart is how much of the resident pages the others
/// carry, not whether somebody else maps them, and of all Qt libraries, not of one. On
/// 2026-10-07 the desktop still ran on Qt files an update had replaced and only a few
/// programs, a browser's helpers among them, mapped the current ones. A start of the
/// measuring build on the fixture then had 95 % of its Qt6Core mapped by somebody else, yet
/// its Qt libraries still counted with 54 % of their 34.9 MB and the file-backed part of
/// its PSS was 48 MB, between the 70 MB of phase I's unshared session and the 36 MB beside
/// three Kirigami programs: about 23 MB less than unshared, 16 MB of that on the Qt
/// libraries. The lower limit of the hidden state needs just those 23 MB with phase I's
/// anonymous memory (132.8 MB unshared less 23 MB is 110 MB), so that session sat on the
/// line, where a miss would be the session's doing and not Kourier's. A half is the round
/// number just beyond it: there the lower limits are met with a little to spare. The
/// anonymous number is judged on every session whatever this says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sharing {
    /// Other processes carry half or more of the resident Qt pages: the lower limit.
    Shared,
    /// They carry less than half: the higher limit.
    Unshared,
    /// Not read. The lower limit: the higher one is an allowance that needs its evidence.
    Unknown,
}

impl Sharing {
    /// What a session with these Qt pages counts as.
    pub fn of(qt: Option<LibraryShare>) -> Self {
        match qt {
            None => Sharing::Unknown,
            Some(qt) if qt.resident_kb == 0 => Sharing::Unknown,
            // Others carry less than half: the PSS is more than half of the RSS.
            Some(qt) if qt.pss_kb * 2 > qt.resident_kb => Sharing::Unshared,
            Some(_) => Sharing::Shared,
        }
    }

    /// The PSS limit of `budget` that applies (MB).
    pub fn pss_limit(self, budget: MemoryBudget) -> f64 {
        match self {
            Sharing::Unshared => budget.unshared,
            Sharing::Shared | Sharing::Unknown => budget.shared,
        }
    }

    /// The words after the limit in the budget cell.
    fn label(self) -> &'static str {
        match self {
            Sharing::Shared => "Qt shared",
            Sharing::Unshared => "Qt not shared",
            Sharing::Unknown => "Qt sharing not read",
        }
    }
}

/// §1.3's memory budgets of one idle state (MB).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MemoryBudget {
    /// Anonymous memory (`Pss_Anon`), on any session.
    pub anonymous: f64,
    /// PSS on a session whose programs map the same Qt files.
    pub shared: f64,
    /// PSS on a session that shares no library page.
    pub unshared: f64,
}

/// Idle with the window shown.
pub const MEMORY_SHOWN: MemoryBudget = MemoryBudget {
    anonymous: 100.0,
    shared: 150.0,
    unshared: 170.0,
};

/// Idle and hidden to the tray, the window released. (§1.3: the anonymous limit is to become
/// 60 MB once the SQLite caches are released with the window and a measurement shows it.)
pub const MEMORY_HIDDEN: MemoryBudget = MemoryBudget {
    anonymous: 70.0,
    shared: 110.0,
    unshared: 145.0,
};

/// One thread's counters at an instant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadSample {
    /// Thread id.
    pub tid: u32,
    /// Thread name.
    pub name: String,
    /// Times the thread gave up the CPU to wait: one per wake-up.
    pub voluntary: u64,
    /// Times it was preempted.
    pub involuntary: u64,
    /// CPU time in clock ticks (user + system).
    pub cpu_ticks: u64,
}

/// Parses `/proc/<pid>/task/<tid>/status` (name and context switches).
pub fn parse_thread_status(text: &str) -> Option<(String, u64, u64)> {
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
            .map(str::trim)
    };
    Some((
        field("Name")?.to_owned(),
        field("voluntary_ctxt_switches")?.parse().ok()?,
        field("nonvoluntary_ctxt_switches")?.parse().ok()?,
    ))
}

/// CPU ticks (utime + stime) from `/proc/<pid>/task/<tid>/stat`. The name in parentheses may
/// hold spaces and parentheses: fields are counted after the last `)`.
pub fn parse_stat_ticks(text: &str) -> Option<u64> {
    let rest = &text[text.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    // After the name: state (3), then fields 4 … ; utime is field 14, stime 15.
    let utime: u64 = fields.nth(11)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some(utime + stime)
}

/// What a thread name did between two samples.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadActivity {
    /// Thread name (threads of one name are added up).
    pub name: String,
    /// Threads of that name.
    pub threads: usize,
    /// Wake-ups in the interval.
    pub wakeups: u64,
    /// CPU time in the interval (ms).
    pub cpu_ms: f64,
}

/// The activity between two samples, per thread name, busiest first. Threads that started or
/// ended in between count with what is known of them. `ticks_per_s` is `CLK_TCK`.
pub fn thread_activity(
    before: &[ThreadSample],
    after: &[ThreadSample],
    ticks_per_s: u64,
) -> Vec<ThreadActivity> {
    let earlier: BTreeMap<u32, &ThreadSample> = before.iter().map(|t| (t.tid, t)).collect();
    let mut by_name: BTreeMap<&str, ThreadActivity> = BTreeMap::new();
    for t in after {
        let (v0, c0) = earlier
            .get(&t.tid)
            .map_or((0, 0), |e| (e.voluntary, e.cpu_ticks));
        let entry = by_name.entry(&t.name).or_insert_with(|| ThreadActivity {
            name: t.name.clone(),
            threads: 0,
            wakeups: 0,
            cpu_ms: 0.0,
        });
        entry.threads += 1;
        entry.wakeups += t.voluntary.saturating_sub(v0);
        #[allow(clippy::cast_precision_loss)]
        {
            entry.cpu_ms +=
                t.cpu_ticks.saturating_sub(c0) as f64 * 1000.0 / ticks_per_s.max(1) as f64;
        }
    }
    let mut out: Vec<ThreadActivity> = by_name.into_values().collect();
    out.sort_by(|a, b| b.wakeups.cmp(&a.wakeups).then_with(|| a.name.cmp(&b.name)));
    out
}

/// The app's threads and memory at one instant of an idle time.
#[derive(Clone, Debug, PartialEq)]
pub struct IdleSample {
    /// Seconds since the idle time's first sample.
    pub at_s: f64,
    /// Every thread's counters.
    pub threads: Vec<ThreadSample>,
    /// CPU ticks of the whole process (`/proc/<pid>/stat`): threads that have ended since
    /// the process began count too.
    pub process_ticks: u64,
    /// Memory.
    pub rollup: Rollup,
}

/// What one idle time measured, from its samples.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Idle {
    /// Seconds between the first and the last sample.
    pub seconds: f64,
    /// Activity per thread name over the whole time, busiest first (the sum of the parts, so
    /// a thread that ended counts with what the samples before its end saw).
    pub threads: Vec<ThreadActivity>,
    /// Wake-ups per second of all threads in each part.
    pub part_rates: Vec<f64>,
    /// CPU time of the process over the whole time (ms), ended threads included.
    pub cpu_ms: f64,
    /// Threads that started inside the time.
    pub started: usize,
    /// Threads that ended inside it.
    pub ended: usize,
    /// PSS at each sample (MB).
    pub pss_mb: Vec<f64>,
    /// Memory at the last sample: the value the budgets are about.
    pub rollup: Rollup,
    /// Memory by mapping at the last sample (kB, largest first).
    pub maps: Vec<(String, u64)>,
    /// The Qt 6 libraries at the last sample: what is resident and how it is shared. Says
    /// which PSS limit the idle time is held to.
    pub qt: Option<LibraryShare>,
    /// Qt6Core alone at the last sample.
    pub qt_core: Option<LibraryShare>,
}

impl Idle {
    /// Which of §1.3's two PSS limits this idle time is held to.
    pub fn sharing(&self) -> Sharing {
        Sharing::of(self.qt)
    }

    /// CPU time of the process per 5 min of the idle time (ms): what §1.3's budget is in.
    /// The idle time counts in whole seconds, as it was asked for. Its last sample comes a
    /// moment late, and the CPU time comes in clock ticks of 10 ms: 50 ms in 300.0004 s
    /// would read as 49.99993 ms and pass a budget of "below 50 ms".
    pub fn cpu_ms_per_5_min(&self) -> Option<f64> {
        let seconds = self.seconds.round();
        (seconds > 0.0).then(|| self.cpu_ms * IDLE_BUDGET_S / seconds)
    }

    /// Wake-ups per second of all threads over the whole time.
    pub fn wakeups_per_s(&self) -> Option<f64> {
        if self.seconds <= 0.0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        let total = self.threads.iter().map(|t| t.wakeups).sum::<u64>() as f64;
        Some(total / self.seconds)
    }

    /// The busiest part's wake-ups per second.
    pub fn worst_part(&self) -> Option<f64> {
        max(&self.part_rates)
    }
}

/// Puts the samples of an idle time together (`None` with fewer than two samples).
/// `ticks_per_s` is `CLK_TCK`. The memory by mapping is not in the samples: the caller adds
/// it.
pub fn idle_from_samples(samples: &[IdleSample], ticks_per_s: u64) -> Option<Idle> {
    let (first, last) = (samples.first()?, samples.last()?);
    if samples.len() < 2 {
        return None;
    }
    let mut by_name: BTreeMap<String, ThreadActivity> = BTreeMap::new();
    let mut part_rates = Vec::with_capacity(samples.len() - 1);
    for pair in samples.windows(2) {
        let [before, after] = pair else { continue };
        let part = thread_activity(&before.threads, &after.threads, ticks_per_s);
        #[allow(clippy::cast_precision_loss)]
        let wakeups = part.iter().map(|t| t.wakeups).sum::<u64>() as f64;
        part_rates.push(wakeups / (after.at_s - before.at_s).max(f64::EPSILON));
        for t in part {
            let entry = by_name
                .entry(t.name.clone())
                .or_insert_with(|| ThreadActivity {
                    name: t.name.clone(),
                    threads: 0,
                    wakeups: 0,
                    cpu_ms: 0.0,
                });
            entry.threads = entry.threads.max(t.threads);
            entry.wakeups += t.wakeups;
            entry.cpu_ms += t.cpu_ms;
        }
    }
    let mut threads: Vec<ThreadActivity> = by_name.into_values().collect();
    threads.sort_by(|a, b| b.wakeups.cmp(&a.wakeups).then_with(|| a.name.cmp(&b.name)));
    let tids = |s: &IdleSample| -> std::collections::BTreeSet<u32> {
        s.threads.iter().map(|t| t.tid).collect()
    };
    // Every thread ever seen, so one that came and went between two samples of the middle
    // still counts as started and ended.
    let (at_first, at_last) = (tids(first), tids(last));
    let seen: std::collections::BTreeSet<u32> = samples.iter().flat_map(tids).collect();
    #[allow(clippy::cast_precision_loss)]
    let cpu_ms = last.process_ticks.saturating_sub(first.process_ticks) as f64 * 1000.0
        / ticks_per_s.max(1) as f64;
    Some(Idle {
        seconds: last.at_s - first.at_s,
        threads,
        part_rates,
        cpu_ms,
        started: seen.difference(&at_first).count(),
        ended: seen.difference(&at_last).count(),
        pss_mb: samples.iter().map(|s| mb(s.rollup.pss_kb)).collect(),
        rollup: last.rollup,
        maps: Vec::new(),
        qt: None,
        qt_core: None,
    })
}

/// The rate in the line the Signal mock catch-up test prints:
/// `S7 throughput: 5000 queued messages committed in core.db 1.23 s after connect (4065
/// messages/s; app start to connect 0.40 s; release build)`. Also says whether it was a
/// release build.
pub fn parse_s7_line(output: &str) -> Option<(f64, bool)> {
    let line = output.lines().find(|l| l.contains("S7 throughput:"))?;
    let inside = line.split_once('(')?.1;
    let rate = inside.split_once(" messages/s")?.0.trim().parse().ok()?;
    Some((rate, line.contains("release build")))
}

/// Available kB of the file system holding a path, from `df -Pk <path>`.
pub fn parse_df_available_kb(output: &str) -> Option<u64> {
    output
        .lines()
        .nth(1)?
        .split_whitespace()
        .nth(3)?
        .parse()
        .ok()
}

// ---------------------------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------------------------

/// The value at quantile `p` (0 … 1) of `values` (nearest rank); `None` when empty.
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let rank = ((p.clamp(0.0, 1.0) * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted.get(rank - 1).copied()
}

/// The median (nearest rank).
pub fn median(values: &[f64]) -> Option<f64> {
    percentile(values, 0.5)
}

fn max(values: &[f64]) -> Option<f64> {
    values.iter().copied().max_by(f64::total_cmp)
}

fn min(values: &[f64]) -> Option<f64> {
    values.iter().copied().min_by(f64::total_cmp)
}

// ---------------------------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------------------------

/// A budget of §1.3.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Budget {
    /// The measured value must not be above this.
    AtMost(f64),
    /// The measured value must be below this.
    Below(f64),
    /// The measured value must not be below this.
    AtLeast(f64),
    /// A check that must hold: the value is 1 for yes and 0 for no.
    Yes,
}

impl Budget {
    /// The number of the budget, where it has one.
    fn bound(self) -> Option<f64> {
        match self {
            Budget::AtMost(b) | Budget::Below(b) | Budget::AtLeast(b) => Some(b),
            Budget::Yes => None,
        }
    }
}

/// How a measured value stands against its budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Inside the budget with room.
    Ok,
    /// Inside the budget, but within 20 % of it (or a single run was outside).
    Close,
    /// Outside the budget.
    Miss,
    /// No budget, or one that is not enforced for this row: reported for information.
    Info,
    /// Not measured in this run (left out on purpose: `--quick`, `--no-mock`).
    NotMeasured,
    /// The measurement was tried and gave no number: the run has no verdict.
    Failed,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Close => "close",
            Status::Miss => "MISS",
            Status::Info => "info",
            Status::NotMeasured => "not measured",
            Status::Failed => "FAILED",
        }
    }
}

/// The share of a budget that counts as "close".
const CLOSE: f64 = 0.8;

/// Judges `value` against `budget`. `worst` is the worst single run where there were several:
/// a value inside the budget whose worst run is outside counts as close.
pub fn judge(budget: Option<Budget>, value: Option<f64>, worst: Option<f64>) -> Status {
    let Some(value) = value else {
        return Status::NotMeasured;
    };
    let Some(budget) = budget else {
        return Status::Info;
    };
    let (miss, close) = match budget {
        Budget::AtMost(b) => (value > b, value > b * CLOSE || worst.is_some_and(|w| w > b)),
        Budget::Below(b) => (
            value >= b,
            value > b * CLOSE || worst.is_some_and(|w| w >= b),
        ),
        Budget::AtLeast(b) => (value < b, value < b / CLOSE || worst.is_some_and(|w| w < b)),
        Budget::Yes => (value < 1.0, false),
    };
    if miss {
        Status::Miss
    } else if close {
        Status::Close
    } else {
        Status::Ok
    }
}

/// One line of the table.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// The quality target.
    pub target: String,
    /// Its budget.
    pub budget: Option<Budget>,
    /// Which limit that is, where §1.3 has several for one number (`Qt shared`): shown after
    /// the budget. Empty for a row with one limit.
    pub limit: &'static str,
    /// The measured value (the median where there were several runs).
    pub value: Option<f64>,
    /// The worst single run, where there were several.
    pub worst: Option<f64>,
    /// The unit of budget and value.
    pub unit: &'static str,
    /// The budget is shown but not enforced: §1.3 states it for information, or what was
    /// measured is not what the budget is about (another renderer, a shorter idle time). The
    /// budget cell says "for information" and the row never fails the run.
    pub advisory: bool,
    /// The measurement was tried and failed (a test that did not pass, a run without its
    /// number): the row's verdict is [`Status::Failed`] whatever value it shows.
    pub failed: bool,
    /// How the value was taken and from how many runs.
    pub how: String,
}

impl Row {
    /// The row's verdict.
    pub fn status(&self) -> Status {
        if self.failed {
            return Status::Failed;
        }
        match judge(self.budget, self.value, self.worst) {
            Status::Ok | Status::Close | Status::Miss if self.advisory => Status::Info,
            status => status,
        }
    }
}

fn number(v: f64) -> String {
    if v.abs() >= 100.0 || v.fract() == 0.0 {
        format!("{v:.0}")
    } else if v.abs() >= 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    }
}

/// The budget cell: exactly what the row is held to.
fn budget_text(row: &Row) -> String {
    let mut text = match row.budget {
        Some(Budget::AtMost(b)) => format!("≤ {} {}", number(b), row.unit),
        Some(Budget::Below(b)) => format!("< {} {}", number(b), row.unit),
        Some(Budget::AtLeast(b)) => format!("≥ {} {}", number(b), row.unit),
        Some(Budget::Yes) => "yes".to_owned(),
        None => return "—".to_owned(),
    };
    if !row.limit.is_empty() {
        let _ = write!(text, " ({})", row.limit);
    }
    if row.advisory {
        text.push_str(", for information");
    }
    text
}

/// A measured number beside the number of its budget: as [`number`] writes it, and with
/// three decimals where rounding would print the budget's number although the value is not
/// on it (150.3 MB against 150 MB is a miss and must not read "150", nor 70.04 MB against
/// 70 MB "70.0").
fn beside(v: f64, bound: Option<f64>) -> String {
    let text = number(v);
    let same = |a: f64, b: f64| (a - b).abs() <= f64::EPSILON;
    match bound {
        // Compared as numbers: the budget is written "70" and a value beside it "70.0".
        Some(b) if !same(v, b) && text.parse().is_ok_and(|shown: f64| same(shown, b)) => {
            format!("{v:.3}")
        }
        _ => text,
    }
}

fn value_text(row: &Row) -> String {
    if row.budget == Some(Budget::Yes) {
        return match row.value {
            None => "—".to_owned(),
            Some(v) if v >= 1.0 => "yes".to_owned(),
            Some(_) => "no".to_owned(),
        };
    }
    let bound = row.budget.and_then(Budget::bound);
    match (row.value, row.worst) {
        (None, _) => "—".to_owned(),
        (Some(v), Some(w)) if (w - v).abs() > f64::EPSILON => {
            format!(
                "{} {} (worst {})",
                beside(v, bound),
                row.unit,
                beside(w, bound)
            )
        }
        (Some(v), _) => format!("{} {}", beside(v, bound), row.unit),
    }
}

/// The table as Markdown: target, budget, measured, status, how.
pub fn render_table(rows: &[Row]) -> String {
    let header = ["Target", "Budget", "Measured", "Status", "How measured"];
    let cells: Vec<[String; 5]> = rows
        .iter()
        .map(|r| {
            [
                r.target.clone(),
                budget_text(r),
                value_text(r),
                r.status().label().to_owned(),
                r.how.clone(),
            ]
        })
        .collect();
    let width = |col: usize| {
        cells
            .iter()
            .map(|c| c[col].chars().count())
            .chain(std::iter::once(header[col].chars().count()))
            .max()
            .unwrap_or(0)
    };
    let widths: Vec<usize> = (0..5).map(width).collect();
    let line = |cols: &[String; 5]| {
        let mut out = String::from("|");
        for (text, w) in cols.iter().zip(&widths) {
            let pad = w.saturating_sub(text.chars().count());
            let _ = write!(out, " {text}{} |", " ".repeat(pad));
        }
        out
    };
    let mut out = line(&header.map(str::to_owned));
    out.push('\n');
    out.push('|');
    for w in &widths {
        let _ = write!(out, "{}|", "-".repeat(w + 2));
    }
    out.push('\n');
    for c in &cells {
        out.push_str(&line(c));
        out.push('\n');
    }
    out
}

/// The rows whose budget is missed.
pub fn missed(rows: &[Row]) -> Vec<&Row> {
    rows.iter().filter(|r| r.status() == Status::Miss).collect()
}

/// The rows whose measurement failed.
pub fn failed(rows: &[Row]) -> Vec<&Row> {
    rows.iter()
        .filter(|r| r.status() == Status::Failed)
        .collect()
}

/// The exit status for a table: 1 when a measurement failed (no verdict; `--report-only`
/// does not hide that), [`EXIT_MISSED`] when a budget is missed and `--report-only` was not
/// given, else 0.
pub fn exit_status(rows: &[Row], report_only: bool) -> i32 {
    if !failed(rows).is_empty() {
        1
    } else if !report_only && !missed(rows).is_empty() {
        EXIT_MISSED
    } else {
        0
    }
}

// ---------------------------------------------------------------------------------------------
// Measurements
// ---------------------------------------------------------------------------------------------

/// What the starts measured (ms).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Startup {
    /// `main` → first `frameSwapped`, per run.
    pub first_frame: Vec<f64>,
    /// First frame → the frame that shows conversation rows, per run.
    pub list_after_frame: Vec<f64>,
    /// `main` → `App::start` returned, per run.
    pub app_started: Vec<f64>,
    /// Process spawn → the first-frame line read by xtask, per run.
    pub spawn_to_frame: Vec<f64>,
    /// `main` → `Main.qml` loaded and the window created, per run.
    pub qml_loaded: Vec<f64>,
    /// Time the conversation list's refreshes took on the GUI thread until it was shown, per
    /// run.
    pub list_refresh: Vec<f64>,
    /// Heap in use at the first frame (MB), per run.
    pub heap_at_frame: Vec<f64>,
    /// Heap in use and free-but-held 1.5 s after the first frame (MB), from the one start
    /// that ran the scripted driver.
    pub heap_settled: Option<(f64, f64)>,
    /// Conversation rows and the delegates the list holds 1.5 s after the first frame (the
    /// same start).
    pub rows_and_delegates: Option<(f64, f64)>,
    /// `main` → first frame of the starts with Kourier's own files evicted from the page
    /// cache, per run.
    pub cold_first_frame: Vec<f64>,
    /// Spawn → first-frame line of those starts.
    pub cold_spawn_to_frame: Vec<f64>,
    /// What was evicted before each of them: files and MB.
    pub cold_evicted: Option<(f64, f64)>,
}

/// What the session run measured.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Session {
    /// `timeline.open` per opened conversation (ms).
    pub opens: Vec<f64>,
    /// The query part of each open (ms).
    pub open_queries: Vec<f64>,
    /// Rows each opened conversation had loaded when its frame was shown.
    pub open_rows: Vec<f64>,
    /// The scroll mark's fields.
    pub scroll: Option<Mark>,
    /// The idle time with the window shown.
    pub shown: Option<Idle>,
    /// The idle time hidden, the window released.
    pub hidden: Option<Idle>,
    /// Heap in use and free-but-held when the idle time began (MB).
    pub heap_shown: Option<(f64, f64)>,
    /// The same right after the hidden window was released and the heap trimmed.
    pub heap_released: Option<(f64, f64)>,
    /// The same two seconds later.
    pub heap_released_settled: Option<(f64, f64)>,
    /// The idle seconds asked for per state.
    pub idle_s: f64,
    /// QML warnings the run printed.
    pub qml_warnings: Vec<String>,
}

impl Session {
    /// The `timeline.open` times that count: conversations with at least [`OPEN_ROWS`] rows
    /// loaded.
    pub fn counted_opens(&self) -> Vec<f64> {
        self.opens
            .iter()
            .zip(&self.open_rows)
            .filter(|(_, rows)| **rows >= OPEN_ROWS)
            .map(|(ms, _)| *ms)
            .collect()
    }
}

/// What seeding the reference profile measured.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Seed {
    /// Messages ingested.
    pub messages: f64,
    /// Events ingested.
    pub events: f64,
    /// Messages per second from the link to the last ack.
    pub messages_per_s: f64,
    /// Attachment files written.
    pub files: f64,
    /// Seconds the downloads took after the last ack.
    pub downloads_s: f64,
}

/// Everything one `cargo xtask perf` measured.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Measured {
    /// Starts.
    pub startup: Startup,
    /// The session.
    pub session: Session,
    /// Envelope → notification accepted (`notify.shown` `recv_ms`), per run.
    pub notify_recv: Vec<f64>,
    /// The core's notification event → accepted (`notify.shown` `ms`), per run.
    pub notify_event: Vec<f64>,
    /// The core's `recv.commit` of the live message → accepted, per run (§1.3's span).
    pub notify_commit: Vec<f64>,
    /// That commit's `took_us` in ms (the envelope reaching the core → commit and ack), per
    /// run.
    pub commit_took: Vec<f64>,
    /// Memory a few seconds after each of those starts, no conversation opened.
    pub fresh: Vec<Rollup>,
    /// The seeding of the profile (by this run or an earlier one).
    pub seed: Option<Seed>,
    /// An earlier run seeded the profile: the seed numbers are that run's.
    pub seed_reused: bool,
    /// The Signal mock catch-up rates (messages/s), one per run of the test.
    pub mock_ingest: Vec<f64>,
    /// Whether the test ran as a release build.
    pub mock_release: bool,
    /// Why the Signal mock test failed, when it did.
    pub mock_failure: Option<String>,
    /// Size of the release binary (bytes).
    pub binary_bytes: Option<u64>,
    /// Whether the normal release binary hides its memory from its user (§9.4).
    pub release_non_dumpable: Option<bool>,
    /// QML warnings of the starts and the notification runs.
    pub warnings: Vec<String>,
    /// The load average when the run ended.
    pub load_end: Option<String>,
}

fn mb(kb: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let v = kb as f64 / 1024.0;
    v
}

/// §1.3, idle CPU on the fake network: the wake-ups per second of all threads over the idle
/// time must not be above this.
pub const IDLE_WAKEUPS_PER_S: f64 = 0.2;

/// §1.3: no part of the idle time may have more wake-ups per second than this.
pub const IDLE_PART_WAKEUPS_PER_S: f64 = 0.5;

/// §1.3: the CPU time of the process per 5 min of idle must stay below this (ms).
pub const IDLE_CPU_MS_PER_5_MIN: f64 = 50.0;

fn or_dash(value: Option<f64>) -> String {
    value.map_or("—".into(), number)
}

fn arrows(values: &[f64]) -> String {
    values
        .iter()
        .map(|v| format!("{v:.1}"))
        .collect::<Vec<_>>()
        .join(" → ")
}

fn rates(values: &[f64]) -> String {
    values
        .iter()
        .map(|r| format!("{r:.2}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Which of §1.3's two PSS limits an idle time is held to and why, for its row.
fn pss_limit_how(idle: &Idle, budget: MemoryBudget) -> String {
    let (lower, higher) = (number(budget.shared), number(budget.unshared));
    let Some((qt, carried)) = idle.qt.and_then(|qt| Some((qt, qt.carried_by_others()?))) else {
        return format!(
            "Limit: §1.3's lower one, {lower} MB ({higher} MB is for a session that is shown \
             not to share its Qt): what other processes carry of the Qt 6 libraries could not \
             be read"
        );
    };
    let facts = format!(
        "{:.0} % of the {:.1} MB of Qt 6 libraries resident here (they count with {:.1} MB)",
        carried * 100.0,
        mb(qt.resident_kb),
        mb(qt.pss_kb)
    );
    if idle.sharing() == Sharing::Unshared {
        format!(
            "Limit: §1.3's higher one, {higher} MB ({lower} MB on a session that shares its \
             Qt): other processes carry only {facts}, less than half"
        )
    } else {
        format!(
            "Limit: §1.3's lower one, {lower} MB ({higher} MB on a session that does not share \
             its Qt): other processes carry {facts}, half or more"
        )
    }
}

/// The samples behind an idle time's PSS and the limit it is held to, for its row.
fn pss_samples_how(idle: Option<&Idle>, budget: MemoryBudget) -> String {
    idle.map_or_else(String::new, |i| {
        format!(
            " (at the samples: {} MB; anonymous {:.1} MB, file-backed {:.1} MB). {}",
            arrows(&i.pss_mb),
            mb(i.rollup.pss_anon_kb),
            mb(i.rollup.pss_file_kb),
            pss_limit_how(i, budget)
        )
    })
}

/// The row of the anonymous part of an idle time's memory: what Kourier alone causes,
/// whatever else maps the libraries. Judged on any session.
fn anonymous_row(target: &str, idle: Option<&Idle>, budget: MemoryBudget, how: String) -> Row {
    Row {
        target: target.into(),
        budget: Some(Budget::AtMost(budget.anonymous)),
        limit: "",
        value: idle.map(|i| mb(i.rollup.pss_anon_kb)),
        worst: None,
        unit: "MB",
        advisory: false,
        failed: false,
        how,
    }
}

/// The row of an idle time's PSS, held to the limit of the session it ran in ([`Sharing`]).
fn pss_row(target: &str, idle: Option<&Idle>, budget: MemoryBudget, how: String) -> Row {
    let sharing = idle.map_or(Sharing::Unknown, Idle::sharing);
    Row {
        target: target.into(),
        budget: Some(Budget::AtMost(sharing.pss_limit(budget))),
        limit: sharing.label(),
        value: idle.map(|i| mb(i.rollup.pss_kb)),
        worst: None,
        unit: "MB",
        advisory: false,
        failed: false,
        how,
    }
}

/// The three rows of an idle time's CPU use: §1.3 has a limit for the wake-ups over the
/// whole time, one for its busiest part and one for the CPU time. They are per 5 min of
/// idle, so a run that idled for less (`idle_s`: the seconds asked for) shows them for
/// information.
fn idle_cpu_rows(state: &str, when: &str, idle: Option<&Idle>, idle_s: f64) -> [Row; 3] {
    let advisory = idle_s < IDLE_BUDGET_S;
    let short = if advisory {
        format!(
            ". §1.3's budget is per {IDLE_BUDGET_S:.0} s of idle and this run idled for \
             {idle_s:.0} s: for information"
        )
    } else {
        String::new()
    };
    let row =
        |what: &str, budget: Budget, value: Option<f64>, unit: &'static str, how: String| Row {
            target: format!("Idle CPU, {state}: {what}"),
            budget: Some(budget),
            limit: "",
            value,
            worst: None,
            unit,
            advisory,
            failed: false,
            how: format!("{how}{short}"),
        };
    [
        row(
            "wake-ups",
            Budget::AtMost(IDLE_WAKEUPS_PER_S),
            idle.and_then(Idle::wakeups_per_s),
            "wakeups/s",
            idle.map_or_else(
                || "voluntary context switches of all threads over the idle time".to_owned(),
                |i| {
                    format!(
                        "voluntary context switches of all threads \
                         (`/proc/<pid>/task/*/status`) over {:.0} s{when}, on the fake network \
                         (no keepalive; with a Signal connection §1.3 has no number yet); {} \
                         threads started and {} ended meanwhile; 1 run",
                        i.seconds, i.started, i.ended
                    )
                },
            ),
        ),
        row(
            "busiest part",
            Budget::AtMost(IDLE_PART_WAKEUPS_PER_S),
            idle.and_then(Idle::worst_part),
            "wakeups/s",
            idle.map_or_else(
                || "the busiest part of the same idle time".to_owned(),
                |i| {
                    #[allow(clippy::cast_precision_loss)]
                    let part_s = i.seconds / i.part_rates.len().max(1) as f64;
                    format!(
                        "the same idle time in {} parts of {part_s:.0} s: the wake-ups per \
                         second of the busiest one (each part: {})",
                        i.part_rates.len(),
                        rates(&i.part_rates)
                    )
                },
            ),
        ),
        row(
            "process CPU time",
            Budget::Below(IDLE_CPU_MS_PER_5_MIN),
            idle.and_then(Idle::cpu_ms_per_5_min),
            "ms per 5 min",
            idle.map_or_else(
                || "CPU time of the process over the same idle time".to_owned(),
                |i| {
                    format!(
                        "CPU time of the process over the same idle time (`/proc/<pid>/stat`, \
                         in clock ticks; threads that ended count too): {:.0} ms in {:.0} s",
                        i.cpu_ms, i.seconds
                    )
                },
            ),
        ),
    ]
}

/// The row of the warm starts.
const COLD_START: &str = "Cold start to first frame";
/// The row of the start with Kourier's own files out of the page cache.
const COLD_FILES_START: &str = "Start with Kourier's own files out of the page cache";
/// The row of the list's first populated frame.
const LIST_AFTER_FRAME: &str = "First frame to populated conversation list";
/// The row of the fake network's catch-up: the seeding's number, taken from the profile's
/// stamp when the profile was reused.
const FAKE_CATCH_UP: &str = "Catch-up ingest, fake network (history with media)";
/// The two rows that need the plain release binary.
const BINARY_SIZE: &str = "Binary size";
const NON_DUMPABLE: &str = "Release build is non-dumpable (§9.4)";

/// Rows a `--start` run has a number for: the starts, and the seed numbers of the profile.
const START_TARGETS: [&str; 4] = [
    COLD_START,
    COLD_FILES_START,
    LIST_AFTER_FRAME,
    FAKE_CATCH_UP,
];

/// Why a row has no number when the run's options left its measurement out, `None` for a row
/// that this run measures (or whose measurement failed for another reason). A row marked this
/// way is "not measured": it counts as neither passed nor missed nor failed.
fn left_out(o: &Options, target: &str) -> Option<&'static str> {
    if !o.measures_plain_binary() && (target == BINARY_SIZE || target == NON_DUMPABLE) {
        return Some(if o.start {
            "not measured: a `--start` run does not build the plain release binary (a full run \
             does)"
        } else {
            "not measured: a quick run does not build the plain release binary (a full run \
             does)"
        });
    }
    if o.start && !START_TARGETS.contains(&target) {
        return Some("not measured: a `--start` run measures only the starts");
    }
    None
}

/// The rows of the table for what was measured: one number against one limit per row, the
/// budgets as §1.3 has them.
pub fn rows(m: &Measured, o: &Options) -> Vec<Row> {
    let runs = m.startup.first_frame.len();
    let s = &m.session;
    let scroll = |key: &str| s.scroll.as_ref().and_then(|mark| mark.get(key));
    let opens = s.counted_opens();
    let fresh: Vec<f64> = m.fresh.iter().map(|r| mb(r.pss_kb)).collect();
    let (shown, hidden) = (s.shown.as_ref(), s.hidden.as_ref());
    let mut rows = vec![
        Row {
            target: COLD_START.into(),
            budget: Some(Budget::AtMost(400.0)),
            limit: "",
            value: median(&m.startup.first_frame),
            worst: max(&m.startup.first_frame),
            unit: "ms",
            advisory: false,
            failed: false,
            how: format!(
                "trace `main` → first `frameSwapped`; median of {runs} starts of the release \
                 build on the reference profile as a user starts it (no scripted driver), \
                 offscreen, warm page cache, after one start that is not counted (spawn → \
                 frame line: {} ms)",
                or_dash(median(&m.startup.spawn_to_frame))
            ),
        },
        Row {
            target: COLD_FILES_START.into(),
            budget: Some(Budget::AtMost(600.0)),
            limit: "",
            value: median(&m.startup.cold_first_frame),
            worst: max(&m.startup.cold_first_frame),
            unit: "ms",
            advisory: false,
            failed: false,
            how: format!(
                "same trace; before each start the binary, the profile and Kourier's cache \
                 directory are written out and evicted from the page cache ({}); the system's \
                 libraries stay cached, as on a running desktop (a start after boot with them \
                 cold is a manual check); median of {} starts (spawn → frame line: {} ms)",
                m.startup
                    .cold_evicted
                    .map_or("—".into(), |(files, mb)| format!(
                        "{files:.0} files, {mb:.0} MB"
                    )),
                m.startup.cold_first_frame.len(),
                or_dash(median(&m.startup.cold_spawn_to_frame))
            ),
        },
        Row {
            target: LIST_AFTER_FRAME.into(),
            budget: Some(Budget::AtMost(50.0)),
            limit: "",
            value: median(&m.startup.list_after_frame),
            worst: max(&m.startup.list_after_frame),
            unit: "ms",
            advisory: false,
            failed: false,
            how: format!(
                "trace first frame → the frame that shows the rows from `core.db`; median of \
                 {runs} starts"
            ),
        },
        Row {
            target: "Open a conversation (50 rows visible)".into(),
            budget: Some(Budget::AtMost(80.0)),
            limit: "",
            value: median(&opens),
            worst: max(&opens),
            unit: "ms",
            advisory: false,
            // Opens were measured, but none of a conversation with enough rows.
            failed: opens.is_empty() && !s.opens.is_empty(),
            how: format!(
                "trace span `timeline.open` (`App.openConversation` → frame with the rows); \
                 median of {} conversations in 1 session, each with {} or more rows loaded \
                 ({} with fewer than {OPEN_ROWS:.0} left out); the query alone: {} ms; the \
                 first open, which creates the chat page: {} ms",
                opens.len(),
                or_dash(min(&s.open_rows)),
                s.opens.len() - opens.len(),
                or_dash(median(&s.open_queries)),
                or_dash(s.opens.first().copied())
            ),
        },
        anonymous_row(
            "Idle memory, window shown: anonymous",
            shown,
            MEMORY_SHOWN,
            format!(
                "`Pss_Anon` of `/proc/<pid>/smaps_rollup` of the `dumpable` build after the \
                 session and {:.0} s idle: heaps, stacks and written data, what Kourier alone \
                 causes whatever else maps the libraries, so it is judged on any session; 1 run",
                s.idle_s
            ),
        ),
        pss_row(
            "Idle memory, window shown: PSS",
            shown,
            MEMORY_SHOWN,
            format!(
                "`Pss` of the same sample{}. A few seconds after a start, nothing opened: {} \
                 MB (median of {} starts, worst {})",
                pss_samples_how(shown, MEMORY_SHOWN),
                or_dash(median(&fresh)),
                fresh.len(),
                or_dash(max(&fresh))
            ),
        ),
        anonymous_row(
            "Idle memory, hidden to tray: anonymous",
            hidden,
            MEMORY_HIDDEN,
            format!(
                "`Pss_Anon` of the same build {:.0} s after the hidden window was released and \
                 the heap trimmed; 1 run. §1.3 tightens this limit to 60 MB once the SQLite \
                 caches are released with the window and a measurement shows it: 60 MB is not \
                 enforced yet",
                s.idle_s
            ),
        ),
        pss_row(
            "Idle memory, hidden to tray: PSS",
            hidden,
            MEMORY_HIDDEN,
            format!(
                "`Pss` of the same sample{}",
                pss_samples_how(hidden, MEMORY_HIDDEN)
            ),
        ),
    ];
    rows.extend(idle_cpu_rows("window shown", "", shown, s.idle_s));
    rows.extend(idle_cpu_rows(
        "hidden to tray",
        " after the window was released",
        hidden,
        s.idle_s,
    ));
    rows.extend([
        Row {
            target: "Receive → notification".into(),
            budget: Some(Budget::AtMost(150.0)),
            limit: "",
            value: median(&m.notify_recv),
            worst: max(&m.notify_recv),
            unit: "ms",
            advisory: false,
            failed: false,
            how: format!(
                "fake network: envelope handed to the backend's stage 1 → `Notify` accepted by \
                 a mock notification server on a private bus; median of {} starts with one \
                 live message each. Inside that: the envelope reaching the core → its commit \
                 and ack (`took_us`): {} ms; the core's notification event → accepted: {} ms",
                m.notify_recv.len(),
                or_dash(median(&m.commit_took)),
                or_dash(median(&m.notify_event))
            ),
        },
        Row {
            target: "Receive → notification: `recv.commit` → `notify.shown`".into(),
            budget: Some(Budget::AtMost(50.0)),
            limit: "",
            value: median(&m.notify_commit),
            worst: max(&m.notify_commit),
            unit: "ms",
            advisory: false,
            failed: false,
            how: format!(
                "the same starts: the core's `recv.commit` of the live message → `Notify` \
                 accepted, as the time between reading the two lines from the app's output; \
                 median of {} starts",
                m.notify_commit.len()
            ),
        },
        Row {
            target: "Catch-up ingest, Signal mock".into(),
            budget: Some(Budget::AtLeast(3000.0)),
            limit: "release build",
            value: median(&m.mock_ingest),
            worst: min(&m.mock_ingest),
            unit: "messages/s",
            advisory: false,
            failed: m.mock_failure.is_some(),
            how: match (&m.mock_failure, m.mock_ingest.len()) {
                (Some(why), _) => format!("the test failed: {why}"),
                (None, 0) => "skipped (`--no-mock` or `--quick`)".to_owned(),
                (None, n) => format!(
                    "`kourier-e2e` test `five_thousand_queued_messages_ingest_at_500_per_second` \
                     ({} build): 5,000 queued 1:1 texts of one sender, connect → all committed \
                     in `core.db`; median of {n} runs of the test. The test's own assert of \
                     500 messages/s is the floor for debug builds",
                    if m.mock_release { "release" } else { "debug" }
                ),
            },
        },
        Row {
            target: FAKE_CATCH_UP.into(),
            budget: Some(Budget::AtLeast(800.0)),
            limit: "",
            value: m.seed.map(|s| s.messages_per_s),
            worst: None,
            unit: "messages/s",
            advisory: true,
            failed: false,
            how: m.seed.map_or_else(
                || "seeding the reference profile".to_owned(),
                |s| {
                    format!(
                        "seeding the reference profile{}: {:.0} messages ({:.0} events) from \
                         the backend's stage 1 to the core's commit, attachment downloads \
                         running beside it; 1 run. §1.3 has this number for information",
                        if m.seed_reused {
                            " (by an earlier run; this run reused the profile)"
                        } else {
                            ""
                        },
                        s.messages,
                        s.events
                    )
                },
            ),
        },
        Row {
            target: "Timeline scroll, slowest frame (software renderer)".into(),
            budget: Some(Budget::AtMost(16.0)),
            limit: "",
            value: scroll("max_ms"),
            worst: None,
            unit: "ms",
            advisory: true,
            failed: false,
            how: format!(
                "time between animation ticks while flicking through the image-heavy chat for \
                 8 s; software renderer on the GUI thread, offscreen. The budget is for the GPU \
                 scene graph on Wayland (manual check): for information ({} frames, median {} \
                 ms, 95th {} ms, {} over 16.7 ms)",
                or_dash(scroll("frames")),
                or_dash(scroll("p50_ms")),
                or_dash(scroll("p95_ms")),
                or_dash(scroll("over16")),
            ),
        },
        Row {
            target: BINARY_SIZE.into(),
            budget: Some(Budget::AtMost(60.0)),
            limit: "",
            #[allow(clippy::cast_precision_loss)]
            value: m.binary_bytes.map(|b| b as f64 / (1024.0 * 1024.0)),
            worst: None,
            unit: "MiB",
            advisory: false,
            failed: false,
            how: "`target/release/kourier` as built (default features, the default release \
                  profile); the limit makes debug information coming back fail the table \
                  (260 MB once)"
                .into(),
        },
        Row {
            target: NON_DUMPABLE.into(),
            budget: Some(Budget::Yes),
            limit: "",
            value: m.release_non_dumpable.map(|ok| if ok { 1.0 } else { 0.0 }),
            worst: None,
            unit: "",
            advisory: false,
            failed: false,
            how: "the normal release binary runs and its `/proc/<pid>/smaps_rollup` cannot be \
                  read by the same user"
                .into(),
        },
    ]);
    for row in &mut rows {
        if row.value.is_none()
            && !row.failed
            && let Some(why) = left_out(o, &row.target)
        {
            row.how = why.to_owned();
        }
    }
    if o.quick {
        for row in &mut rows {
            row.how.push_str(" [quick run]");
        }
    }
    rows
}

fn activity_table(title: &str, idle: Option<&Idle>) -> String {
    let mut out = format!("\n{title}\n\n");
    let Some(idle) = idle else {
        out.push_str("Not measured.\n");
        return out;
    };
    let _ = writeln!(
        out,
        "{:.0} s; wake-ups per second in each part: {}; process CPU time {:.0} ms; {} threads \
         started, {} ended.\n",
        idle.seconds,
        rates(&idle.part_rates),
        idle.cpu_ms,
        idle.started,
        idle.ended
    );
    out.push_str("| Thread | Threads | Wakeups | per s | CPU ms |\n|---|---|---|---|---|\n");
    for t in idle
        .threads
        .iter()
        .filter(|t| t.wakeups > 0 || t.cpu_ms > 0.0)
    {
        #[allow(clippy::cast_precision_loss)]
        let per_s = t.wakeups as f64 / idle.seconds.max(f64::EPSILON);
        let _ = writeln!(
            out,
            "| {} | {} | {} | {:.2} | {:.0} |",
            t.name, t.threads, t.wakeups, per_s, t.cpu_ms
        );
    }
    out
}

fn memory_table(title: &str, idle: Option<&Idle>) -> String {
    let mut out = format!("\n{title}\n\n");
    let Some(idle) = idle else {
        out.push_str("Not measured.\n");
        return out;
    };
    let r = idle.rollup;
    let _ = writeln!(
        out,
        "PSS {:.1} MB (anonymous {:.1}, file {:.1}, shared memory {:.1}), RSS {:.1} MB, swapped {:.1} MB.\n",
        mb(r.pss_kb),
        mb(r.pss_anon_kb),
        mb(r.pss_file_kb),
        mb(r.pss_shmem_kb),
        mb(r.rss_kb),
        mb(r.swap_pss_kb)
    );
    if let Some((qt, carried)) = idle.qt.and_then(|qt| Some((qt, qt.carried_by_others()?))) {
        let _ = write!(
            out,
            "Of {:.1} MB of Qt 6 libraries resident, {:.1} MB are mapped by other processes too \
             and {:.1} MB count for this process: the others carry {:.0} %.",
            mb(qt.resident_kb),
            mb(qt.shared_kb),
            mb(qt.pss_kb),
            carried * 100.0
        );
        if let Some(core) = idle.qt_core {
            let _ = write!(
                out,
                " Of {:.1} MB of Qt6Core resident, {:.1} MB are mapped by other processes too.",
                mb(core.resident_kb),
                mb(core.shared_kb)
            );
        }
        out.push_str(match idle.sharing() {
            Sharing::Shared => {
                " Half or more: a session that shares its Qt, held to the lower PSS limit."
            }
            Sharing::Unshared => {
                " Less than half: the library pages count in full or nearly so (a desktop that \
                 still runs on library files an update has replaced does that), the file-backed \
                 part of the PSS is near its upper bound and the higher PSS limit applies. The \
                 anonymous part is not affected."
            }
            Sharing::Unknown => "",
        });
        out.push_str("\n\n");
    }
    out.push_str("| Mapping | PSS MB |\n|---|---|\n");
    let shown = 14;
    for (name, kb) in idle.maps.iter().take(shown) {
        let _ = writeln!(out, "| {name} | {:.1} |", mb(*kb));
    }
    let rest: u64 = idle.maps.iter().skip(shown).map(|(_, kb)| kb).sum();
    if rest > 0 {
        let _ = writeln!(
            out,
            "| {} smaller mappings | {:.1} |",
            idle.maps.len().saturating_sub(shown),
            mb(rest)
        );
    }
    out
}

fn list(values: &[f64]) -> String {
    values
        .iter()
        .map(|v| format!("{v:.0}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn names(rows: &[&Row]) -> String {
    rows.iter()
        .map(|r| r.target.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

/// The whole report: the table and what stands behind its numbers.
pub fn render_report(m: &Measured, o: &Options, context: &str) -> String {
    let table = rows(m, o);
    let mut out = String::from("# Kourier: quality targets (architecture §1.3)\n\n");
    out.push_str(context);
    out.push_str("\n\n");
    out.push_str(&render_table(&table));
    out.push_str(
        "\nBudgets: architecture §1.3, confirmed by the user on 2026-10-07. `ok`: inside the \
         budget. `close`: inside, but within 20 % of it or with a single run outside. `MISS`: \
         outside. `info`: for information, never fails the run.\n",
    );
    let (missed, failed) = (missed(&table), failed(&table));
    if !failed.is_empty() {
        let _ = writeln!(
            out,
            "\nFailed measurements (no verdict for this run): {}.",
            names(&failed)
        );
    }
    if missed.is_empty() {
        out.push_str("\nNo budget is missed.\n");
    } else {
        let _ = writeln!(out, "\nMissed: {}.", names(&missed));
    }
    out.push_str("\n## Behind the numbers\n\n");
    let _ = writeln!(
        out,
        "- Starts, `main` → first frame (ms): {}; min {}.",
        list(&m.startup.first_frame),
        or_dash(min(&m.startup.first_frame))
    );
    let _ = writeln!(
        out,
        "- Starts with Kourier's own files out of the page cache (ms): {}; spawn → frame \
         line: {}.",
        list(&m.startup.cold_first_frame),
        list(&m.startup.cold_spawn_to_frame)
    );
    let _ = writeln!(
        out,
        "- Starts, `main` → `App::start` returned (keys, `core.db`, core) (ms): {}.",
        list(&m.startup.app_started)
    );
    let _ = writeln!(
        out,
        "- Starts, first frame → list rows shown (ms): {}.",
        list(&m.startup.list_after_frame)
    );
    let _ = writeln!(
        out,
        "- Starts, `main` → `Main.qml` loaded and the window created (ms): {}; the \
         conversation list's refreshes on the GUI thread until it was shown (ms): {}.",
        list(&m.startup.qml_loaded),
        list(&m.startup.list_refresh)
    );
    let _ = writeln!(
        out,
        "- Heap in use at the first frame (MB): {}.",
        list(&m.startup.heap_at_frame)
    );
    if let (Some((rows, delegates)), Some((used, free))) =
        (m.startup.rows_and_delegates, m.startup.heap_settled)
    {
        let _ = writeln!(
            out,
            "- 1.5 s after the first frame (the one start with the scripted driver): {used:.0} \
             MB in use and {free:.0} MB free but still held by the allocator ({rows:.0} \
             conversations, {delegates:.0} list delegates alive)."
        );
    }
    if !m.fresh.is_empty() {
        let _ = writeln!(
            out,
            "- Memory a few seconds after a start, nothing opened (MB PSS): {}.",
            list(&m.fresh.iter().map(|r| mb(r.pss_kb)).collect::<Vec<_>>())
        );
    }
    if let Some((used, free)) = m.session.heap_shown {
        let _ = writeln!(
            out,
            "- Heap after the session: {used:.0} MB in use, {free:.0} MB free but held."
        );
    }
    if let Some((used, free)) = m.session.heap_released {
        let _ = writeln!(
            out,
            "- Heap when the hidden window was released and the heap trimmed: {used:.0} MB in \
             use, {free:.0} MB free in the allocator's arenas (a trim hands the pages of free \
             chunks back to the system and leaves this number as it is: the PSS says what \
             stayed)."
        );
    }
    if let Some((used, free)) = m.session.heap_released_settled {
        let _ = writeln!(
            out,
            "- Heap two seconds later: {used:.0} MB in use, {free:.0} MB free in the arenas."
        );
    }
    if !m.session.opens.is_empty() {
        let _ = writeln!(
            out,
            "- `timeline.open` (ms): {}; 95th {}; rows loaded: {} to {}.",
            list(&m.session.opens),
            or_dash(percentile(&m.session.counted_opens(), 0.95)),
            or_dash(min(&m.session.open_rows)),
            or_dash(max(&m.session.open_rows))
        );
    }
    if let Some(scroll) = &m.session.scroll {
        let fields: Vec<String> = scroll
            .fields
            .iter()
            .filter(|(k, _)| k.as_str() != "at_ms")
            .map(|(k, v)| format!("{k}={}", number(*v)))
            .collect();
        let _ = writeln!(out, "- Scroll: {}.", fields.join(", "));
    }
    if !m.notify_recv.is_empty() {
        let _ = writeln!(
            out,
            "- Receive → notification (ms): {}; `recv.commit` → accepted (ms): {}; envelope at \
             the core → commit and ack (ms): {}; core event → accepted (ms): {}.",
            list(&m.notify_recv),
            list(&m.notify_commit),
            m.commit_took
                .iter()
                .map(|v| format!("{v:.1}"))
                .collect::<Vec<_>>()
                .join(", "),
            list(&m.notify_event)
        );
    }
    if !m.mock_ingest.is_empty() {
        let _ = writeln!(
            out,
            "- Signal mock catch-up (messages/s): {}.",
            list(&m.mock_ingest)
        );
    }
    if let Some(seed) = m.seed {
        let _ = writeln!(
            out,
            "- Reference profile: {:.0} messages, {:.0} events, {:.0} attachment files; \
             downloads settled {:.1} s after the last ack.",
            seed.messages, seed.events, seed.files, seed.downloads_s
        );
    }
    if let Some(load) = &m.load_end {
        let _ = writeln!(out, "- Load average when the run ended: {load}.");
    }
    out.push_str(&memory_table(
        "### Memory, window shown (after the session)",
        m.session.shown.as_ref(),
    ));
    out.push_str(&memory_table(
        "### Memory, hidden to tray (window released)",
        m.session.hidden.as_ref(),
    ));
    out.push_str(&activity_table(
        "### Idle activity, window shown",
        m.session.shown.as_ref(),
    ));
    out.push_str(&activity_table(
        "### Idle activity, hidden to tray",
        m.session.hidden.as_ref(),
    ));
    let warnings: Vec<&String> = m.warnings.iter().chain(&m.session.qml_warnings).collect();
    if !warnings.is_empty() {
        out.push_str("\n### QML warnings during the runs\n\n");
        for w in warnings {
            let _ = writeln!(out, "- `{w}`");
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Running things
// ---------------------------------------------------------------------------------------------

fn perf_dir() -> PathBuf {
    target_dir().join("perf")
}

fn free_gb(path: &Path) -> Result<u64> {
    let out = Command::new("df")
        .arg("-Pk")
        .arg(path)
        .output()
        .context("run df")?;
    let kb = parse_df_available_kb(&String::from_utf8_lossy(&out.stdout))
        .context("df printed no available space")?;
    Ok(kb / (1024 * 1024))
}

fn check_disk() -> Result<()> {
    let free = free_gb(&root())?;
    if free < MIN_FREE_GB {
        bail!(
            "only {free} GB free on the disk of the workspace; {MIN_FREE_GB} GB are needed to build"
        );
    }
    eprintln!("perf: {free} GB free");
    Ok(())
}

fn remove_dir(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", dir.display())),
    }
}

/// Copies the profile directory (files keep their modes) and has the copy written to the
/// disk before it is used. Without that the run would pay for it: on ext4 an `fsync` of the
/// app waits for the data of files created in the same journal transaction, so the first
/// commits after a fresh copy of 200 MB took 30 ms instead of 6.
fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    remove_dir(to)?;
    let status = Command::new("cp")
        .arg("-a")
        .arg(from)
        .arg(to)
        .status()
        .context("run cp")?;
    if !status.success() {
        bail!("copying {} failed", from.display());
    }
    let status = Command::new("sync")
        .arg("-f")
        .arg(to)
        .status()
        .context("run sync")?;
    if !status.success() {
        bail!("writing {} out failed", to.display());
    }
    Ok(())
}

/// Writes a file's pages out and asks the kernel to drop them from the page cache, so the
/// next read comes from the disk. Returns the file's size.
fn evict_file(path: &Path) -> Result<u64> {
    use std::os::fd::AsRawFd as _;
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let len = file.metadata()?.len();
    // Pages that are not written yet cannot be dropped.
    file.sync_all()
        .with_context(|| format!("sync {}", path.display()))?;
    // SAFETY: posix_fadvise gets the descriptor of the file that is open here and plain
    // numbers; it reads and writes no memory of this process.
    let rc = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    if rc != 0 {
        bail!(
            "posix_fadvise on {}: {}",
            path.display(),
            std::io::Error::from_raw_os_error(rc)
        );
    }
    Ok(len)
}

/// Evicts `path` (a file, or every file below a directory; links are not followed) from the
/// page cache. Adds the files and bytes to `count`. A path that does not exist counts nothing.
fn evict(path: &Path, count: &mut (u64, u64)) -> Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
    };
    if meta.is_dir() {
        for entry in std::fs::read_dir(path).with_context(|| format!("read {}", path.display()))? {
            evict(&entry?.path(), count)?;
        }
    } else if meta.is_file() {
        count.0 += 1;
        count.1 += evict_file(path)?;
    }
    Ok(())
}

/// The session bus address of a child that needs no bus at all: a socket that does not exist.
/// (Without the variable the D-Bus libraries fall back to the user's real session bus.)
fn no_bus() -> String {
    format!("unix:path={}", perf_dir().join("env/no-such-bus").display())
}

/// The display scale of the run (`--scale`), set once by [`run_perf`]'s caller before any child
/// starts.
static DISPLAY_SCALE: OnceLock<String> = OnceLock::new();

/// `QT_SCALE_FACTOR` of every child: the run's `--scale`, 1 without it.
fn display_scale() -> &'static str {
    DISPLAY_SCALE.get().map_or("1", String::as_str)
}

/// Gives a child directories of its own below `target/perf/env` (configuration with the
/// Breeze light scheme, cache, data, state) and names its session bus: no run reads or
/// changes anything of the user's, and the start that is not counted fills the caches.
fn private_env(
    cmd: &mut Command,
    config_home: &Path,
    session_bus: &str,
    scale: &str,
) -> Result<()> {
    for (var, name) in [
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
    ] {
        let dir = config_home.with_file_name(name);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        cmd.env(var, dir);
    }
    cmd.env("XDG_CONFIG_HOME", config_home)
        .env("DBUS_SESSION_BUS_ADDRESS", session_bus)
        // Kourier takes resume and network hints only from a bus named by this variable
        // when its desktop services are forced on (`KOURIER_DESKTOP=1`): none here.
        .env_remove("DBUS_SYSTEM_BUS_ADDRESS")
        .env_remove("XDG_ACTIVATION_TOKEN")
        .env("QT_QPA_PLATFORM", "offscreen")
        .env("QT_QUICK_BACKEND", "software")
        .env("QT_QPA_PLATFORMTHEME", "kde")
        .env("QT_QUICK_CONTROLS_STYLE", "org.kde.desktop")
        .env("QT_SCALE_FACTOR", scale)
        .env("QT_FORCE_STDERR_LOGGING", "1")
        .env("LANG", "en_US.UTF-8")
        .env("LANGUAGE", "en");
    // Whatever the developer's shell has set for a debug build has no say here.
    for var in [
        "KOURIER_HOME",
        "KOURIER_DEV_KEYRING",
        "KOURIER_DEV_DEMO_STATE",
        "KOURIER_DEV_RELEASE_MS",
        "KOURIER_SIGNAL_ENV",
    ] {
        cmd.env_remove(var);
    }
    Ok(())
}

/// The measuring profile shows a tray icon: the tray is off for a profile that never chose
/// (KDE HIG), and "hidden to the tray" is one of the states that are measured. A run with
/// `--perf-profile=<profile>` keeps its preferences inside the profile
/// (`<profile>/kourier/config/kourier.toml`), not under `XDG_CONFIG_HOME`.
fn tray_on(profile: &Path) -> Result<()> {
    let prefs = profile.join("kourier/config/kourier.toml");
    let old = std::fs::read_to_string(&prefs).unwrap_or_default();
    if old.lines().any(|l| l.trim() == "tray = true") {
        return Ok(());
    }
    // First in the file: a key after a `[table]` would belong to that table.
    let mut new = String::from("tray = true\n");
    for line in old.lines().filter(|l| !l.trim_start().starts_with("tray")) {
        new.push_str(line);
        new.push('\n');
    }
    let dir = prefs.parent().expect("a file below the profile");
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(&prefs, new).with_context(|| format!("write {}", prefs.display()))
}

/// The command of an app run: offscreen with the software renderer, directories of its own,
/// desktop services on the private bus `bus`, traces on.
fn app_command(bin: &Path, config_home: &Path, bus: &str) -> Result<Command> {
    let mut cmd = Command::new(bin);
    private_env(&mut cmd, config_home, bus, display_scale())?;
    cmd.env("KOURIER_TRACE_STARTUP", "1")
        .env("KOURIER_LOG", "warn")
        // Desktop services on, but only on the private bus: no real notification, no icon in
        // the real tray, no logind or NetworkManager of this machine.
        .env("KOURIER_DESKTOP", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    Ok(cmd)
}

/// A line of a child's output with the instant it was read.
type Line = (Instant, String);

fn read_lines(reader: impl std::io::Read + Send + 'static) -> Receiver<Line> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            let Ok(line) = line else { break };
            if tx.send((Instant::now(), line)).is_err() {
                break;
            }
        }
    });
    rx
}

/// A running app with its trace.
struct App {
    child: Child,
    lines: Receiver<Line>,
    spawned: Instant,
    /// Every mark read so far, with the instant it was read.
    marks: Vec<(Instant, Mark)>,
    /// Every timing event of the core and the backends read so far (when the log filter lets
    /// them through), with the instant it was read.
    events: Vec<(Instant, Mark)>,
    /// Everything else the app printed.
    log: Vec<String>,
}

impl App {
    fn spawn(mut cmd: Command) -> Result<Self> {
        let spawned = Instant::now();
        let mut child = cmd.spawn().context("start the app")?;
        let stderr = child.stderr.take().context("the app's stderr")?;
        Ok(Self {
            child,
            lines: read_lines(stderr),
            spawned,
            marks: Vec::new(),
            events: Vec::new(),
            log: Vec::new(),
        })
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn take_line(&mut self, at: Instant, line: String) {
        if let Some(mark) = parse_trace_line(&line) {
            self.marks.push((at, mark));
        } else if let Some(event) = parse_perf_event(&line) {
            self.events.push((at, event));
        } else {
            self.log.push(line);
        }
    }

    /// Waits for the next mark named `name` that was not returned before; fails when the app
    /// exits, reports a failure or `timeout` passes.
    fn wait_mark(&mut self, name: &str, from: usize, timeout: Duration) -> Result<usize> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(i) = (from..self.marks.len()).find(|i| self.marks[*i].1.name == name) {
                return Ok(i);
            }
            if let Some((_, failed)) = self.marks.iter().find(|(_, m)| m.name == "perf.failed") {
                bail!(
                    "the scripted session failed at step {}\n{}",
                    failed.get("step").unwrap_or(0.0),
                    self.log_tail()
                );
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok((at, line)) => self.take_line(at, line),
                Err(RecvTimeoutError::Timeout) => {
                    bail!("no `{name}` mark within {timeout:?}\n{}", self.log_tail())
                }
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("the app exited before `{name}`\n{}", self.log_tail())
                }
            }
        }
    }

    fn mark(&self, name: &str) -> Option<&Mark> {
        self.marks
            .iter()
            .find(|(_, m)| m.name == name)
            .map(|(_, m)| m)
    }

    fn field(&self, name: &str, key: &str) -> Option<f64> {
        self.mark(name).and_then(|m| m.get(key))
    }

    fn log_tail(&self) -> String {
        let from = self.log.len().saturating_sub(15);
        self.log[from..].join("\n")
    }

    /// Reads what is left of the output and waits for the exit (killing the app after
    /// `timeout`); returns the QML warnings among what it printed besides its marks.
    fn finish(mut self, timeout: Duration) -> Result<Vec<String>> {
        let deadline = Instant::now() + timeout;
        loop {
            match self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok((at, line)) => self.take_line(at, line),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    bail!(
                        "the app did not exit within {timeout:?}\n{}",
                        self.log_tail()
                    );
                }
            }
        }
        let status = self.child.wait().context("wait for the app")?;
        if !status.success() {
            bail!("the app exited with {status}\n{}", self.log_tail());
        }
        Ok(qml_problems(&self.log.join("\n")))
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // A run that failed half way must not leave the app behind.
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// The private desktop session: a bus of its own with a mock notification server and tray
/// host (`kourier-desktop`'s example `mock_session`). Each app run gets a session of its own:
/// the mock's tray commands go to the first tray item it ever saw.
struct MockSession {
    child: Child,
    stdin: ChildStdin,
    address: String,
    lines: Receiver<Line>,
}

impl MockSession {
    fn start(bin: &Path) -> Result<Self> {
        let mut child = Command::new(bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // The mock starts a bus of its own and must not find the real one.
            .env("DBUS_SESSION_BUS_ADDRESS", no_bus())
            .spawn()
            .with_context(|| format!("start {}", bin.display()))?;
        let stdin = child.stdin.take().context("mock session stdin")?;
        let lines = read_lines(child.stdout.take().context("mock session stdout")?);
        let address = loop {
            match lines.recv_timeout(Duration::from_secs(20)) {
                Ok((_, line)) => {
                    if let Some(a) = line.strip_prefix("DBUS_SESSION_BUS_ADDRESS=") {
                        break a.to_owned();
                    }
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("the mock session printed no bus address");
                }
            }
        };
        Ok(Self {
            child,
            stdin,
            address,
            lines,
        })
    }

    fn send(&mut self, command: &str) -> Result<()> {
        writeln!(self.stdin, "{command}").context("write to the mock session")?;
        self.stdin.flush().context("flush the mock session")
    }

    /// Waits until the session prints a line starting with `prefix` (lines before it are
    /// dropped).
    fn wait_line(&self, prefix: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            match self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok((_, line)) if line.starts_with(prefix) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    }
}

impl Drop for MockSession {
    fn drop(&mut self) {
        let _ = self.send("quit");
        std::thread::sleep(Duration::from_millis(200));
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn read_rollup(pid: u32) -> Result<Rollup> {
    let path = format!("/proc/{pid}/smaps_rollup");
    let text = std::fs::read_to_string(&path).with_context(|| format!("read {path}"))?;
    parse_rollup(&text).with_context(|| format!("{path} has no Pss line"))
}

fn sample_threads(pid: u32) -> Result<Vec<ThreadSample>> {
    let dir = format!("/proc/{pid}/task");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("read {dir}"))? {
        let path = entry?.path();
        let Some(tid) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        // A thread may end between the listing and the read.
        let (Ok(status), Ok(stat)) = (
            std::fs::read_to_string(path.join("status")),
            std::fs::read_to_string(path.join("stat")),
        ) else {
            continue;
        };
        if let (Some((name, voluntary, involuntary)), Some(cpu_ticks)) =
            (parse_thread_status(&status), parse_stat_ticks(&stat))
        {
            out.push(ThreadSample {
                tid,
                name,
                voluntary,
                involuntary,
                cpu_ticks,
            });
        }
    }
    Ok(out)
}

fn clock_ticks() -> u64 {
    Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(100)
}

fn idle_sample(pid: u32, began: Instant) -> Result<IdleSample> {
    let stat = format!("/proc/{pid}/stat");
    let process_ticks = std::fs::read_to_string(&stat)
        .ok()
        .as_deref()
        .and_then(parse_stat_ticks)
        .with_context(|| format!("read {stat}"))?;
    Ok(IdleSample {
        at_s: began.elapsed().as_secs_f64(),
        threads: sample_threads(pid)?,
        process_ticks,
        rollup: read_rollup(pid)?,
    })
}

/// Watches the app for `idle`: samples its threads and memory now, after each of its equal
/// parts ([`idle_parts`]), and its memory by mapping at the end. Returns what the time
/// measured and the instant of the last sample. Reading `/proc` does not wake the app.
/// (Another process of the same binary running meanwhile would share its pages and lower the
/// PSS: nothing else of Kourier runs during a session.)
fn idle_period(pid: u32, idle: Duration) -> Result<(Idle, Instant)> {
    let began = Instant::now();
    let mut samples = vec![idle_sample(pid, began)?];
    let parts = idle_parts(idle.as_secs());
    for part in 1..=parts {
        // Towards fixed instants, so the time the samples take does not add up.
        let due = began + idle * part / parts;
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
        samples.push(idle_sample(pid, began)?);
    }
    let ended = Instant::now();
    let mut out = idle_from_samples(&samples, clock_ticks()).context("no idle samples")?;
    let smaps = std::fs::read_to_string(format!("/proc/{pid}/smaps")).unwrap_or_default();
    out.maps = parse_smaps(&smaps);
    out.qt = parse_smaps_sharing(&smaps, |group| group.starts_with(QT_LIBRARIES));
    out.qt_core = parse_smaps_sharing(&smaps, |group| group == QT_CORE);
    Ok((out, ended))
}

/// What one start measured (ms).
struct Start {
    first_frame: f64,
    list_after_frame: f64,
    spawn_to_frame: f64,
    app_started: Option<f64>,
    qml_loaded: Option<f64>,
    list_refresh: Option<f64>,
    heap_at_frame: Option<f64>,
    heap_settled: Option<(f64, f64)>,
    rows_and_delegates: Option<(f64, f64)>,
    warnings: Vec<String>,
}

/// What one start with a live message measured.
struct NotifyRun {
    /// Envelope → notification accepted (ms).
    recv: f64,
    /// The core's notification event → accepted (ms).
    event: Option<f64>,
    /// The core's `recv.commit` → accepted (ms).
    commit: f64,
    /// The commit's own `took_us` (ms).
    took: Option<f64>,
    /// Memory a few seconds after the start.
    rollup: Rollup,
    warnings: Vec<String>,
}

/// Everything a run needs to start the app.
struct Runner {
    bin: PathBuf,
    mock_bin: PathBuf,
    config_home: PathBuf,
    profile: PathBuf,
    quick: bool,
}

impl Runner {
    /// The app on `profile`; with a `scenario` its scripted driver runs, without one the app
    /// starts exactly as a user starts it.
    fn command(
        &self,
        session: &MockSession,
        profile: &Path,
        scenario: Option<&str>,
    ) -> Result<Command> {
        tray_on(profile)?;
        let mut cmd = app_command(&self.bin, &self.config_home, &session.address)?;
        cmd.arg(format!("--perf-profile={}", profile.display()))
            .arg(format!("--size={WINDOW}"));
        if let Some(scenario) = scenario {
            cmd.arg(format!("--perf-scenario={scenario}"));
        }
        if self.quick {
            cmd.env("KOURIER_DEV_RELEASE_MS", "3000");
        }
        Ok(cmd)
    }

    fn run_copy(&self, name: &str) -> Result<PathBuf> {
        let dir = perf_dir().join(name);
        copy_dir(&self.profile, &dir)?;
        Ok(dir)
    }

    /// One start up to the frame that shows the conversation list, then a clean exit: with
    /// the `startup` scenario the scripted driver waits 1.5 s, counts the list's delegates
    /// and quits; without a scenario nothing but the app runs, and it is quit through its
    /// tray menu.
    fn one_start(&self, profile: &Path, scenario: Option<&str>) -> Result<Start> {
        let mut session = MockSession::start(&self.mock_bin)?;
        let mut app = App::spawn(self.command(&session, profile, scenario)?)?;
        let frame = app.wait_mark("startup.first_frame", 0, Duration::from_secs(60))?;
        let spawn_to_frame = app.marks[frame].0.duration_since(app.spawned).as_secs_f64() * 1000.0;
        app.wait_mark("startup.list_populated", 0, Duration::from_secs(60))?;
        if scenario.is_some() {
            app.wait_mark("perf.done", 0, Duration::from_secs(60))?;
        } else {
            if !session.wait_line("tray item registered", Duration::from_secs(20)) {
                bail!("the app registered no tray item\n{}", app.log_tail());
            }
            session.send("tray-quit")?;
        }
        let first = app
            .field("startup.first_frame", "at_ms")
            .context("first frame without a time")?;
        let list = app
            .field("startup.list_populated", "at_ms")
            .context("list without a time")?;
        let mut start = Start {
            first_frame: first,
            // A list that was on the first frame already took no extra time.
            list_after_frame: (list - first).max(0.0),
            spawn_to_frame,
            app_started: app.field("startup.app_started", "at_ms"),
            qml_loaded: app.field("startup.qml_loaded", "at_ms"),
            list_refresh: app.field("startup.list_populated", "refresh_ms"),
            heap_at_frame: app.field("startup.first_frame", "heap_used_mb"),
            heap_settled: app
                .field("perf.list", "heap_used_mb")
                .zip(app.field("perf.list", "heap_free_mb")),
            rows_and_delegates: app
                .field("perf.list", "rows")
                .zip(app.field("perf.list", "delegates")),
            warnings: Vec::new(),
        };
        start.warnings = app.finish(Duration::from_secs(30))?;
        Ok(start)
    }

    /// `runs` starts on a warm page cache and `cold_runs` with Kourier's own files evicted
    /// before each; returns the QML warnings of all of them too.
    fn starts(&self, runs: usize, cold_runs: usize) -> Result<(Startup, Vec<String>)> {
        let profile = self.run_copy("run-startup")?;
        let mut out = Startup::default();
        // One start that is not counted: it pages the binary and the profile in and fills
        // the caches. It runs the scripted driver, which says what the list holds.
        let mut first = self.one_start(&profile, Some("startup"))?;
        out.heap_settled = first.heap_settled;
        out.rows_and_delegates = first.rows_and_delegates;
        let mut warnings = std::mem::take(&mut first.warnings);
        for run in 1..=runs {
            let mut start = self.one_start(&profile, None)?;
            eprintln!(
                "perf: start {run}/{runs}: first frame {:.0} ms, list {:.0} ms later",
                start.first_frame, start.list_after_frame
            );
            out.first_frame.push(start.first_frame);
            out.list_after_frame.push(start.list_after_frame);
            out.spawn_to_frame.push(start.spawn_to_frame);
            out.app_started.extend(start.app_started);
            out.qml_loaded.extend(start.qml_loaded);
            out.list_refresh.extend(start.list_refresh);
            out.heap_at_frame.extend(start.heap_at_frame);
            warnings.append(&mut start.warnings);
        }
        let cache_home = self.config_home.with_file_name("cache");
        for run in 1..=cold_runs {
            let mut evicted = (0, 0);
            for path in [&self.bin, &profile, &cache_home] {
                evict(path, &mut evicted)?;
            }
            let mut start = self.one_start(&profile, None)?;
            eprintln!(
                "perf: start {run}/{cold_runs} with its own files out of the page cache: first \
                 frame {:.0} ms",
                start.first_frame
            );
            out.cold_first_frame.push(start.first_frame);
            out.cold_spawn_to_frame.push(start.spawn_to_frame);
            #[allow(clippy::cast_precision_loss)]
            {
                out.cold_evicted = Some((evicted.0 as f64, evicted.1 as f64 / (1024.0 * 1024.0)));
            }
            warnings.append(&mut start.warnings);
        }
        remove_dir(&profile)?;
        Ok((out, warnings))
    }

    fn session(&self, idle_s: u64) -> Result<Session> {
        let profile = self.run_copy("run-session")?;
        let mut session = MockSession::start(&self.mock_bin)?;
        let idle = Duration::from_secs(idle_s);
        let scenario = format!("session:{idle_s}");
        let mut app = App::spawn(self.command(&session, &profile, Some(&scenario))?)?;
        let pid = app.pid();
        eprintln!("perf: session: opening conversations and scrolling");
        let idle_mark = app.wait_mark("perf.idle", 0, Duration::from_secs(240))?;
        let idle_began = app.marks[idle_mark].0;
        let mut out = Session {
            idle_s: idle.as_secs_f64(),
            ..Session::default()
        };
        for (_, mark) in &app.marks {
            if mark.name == "timeline.open" {
                // An open without its time or its rows is no measurement.
                let (Some(ms), Some(rows)) = (mark.get("ms"), mark.get("rows")) else {
                    bail!("a `timeline.open` mark without `ms` or `rows`");
                };
                out.opens.push(ms);
                out.open_rows.push(rows);
                out.open_queries.extend(mark.get("query_ms"));
            }
        }
        // The last open is the image-heavy chat of the scroll measurement.
        out.scroll = app.mark("scroll.frames").cloned();
        let heap = |mark: Option<&Mark>| {
            mark.and_then(|m| m.get("heap_used_mb").zip(m.get("heap_free_mb")))
        };
        out.heap_shown = heap(app.mark("perf.idle"));

        // Idle with the window shown: the driver hides the window `idle_s` + 4 s after its
        // mark; the samples begin 2 s after it and end 2 s before the hide.
        eprintln!("perf: session: idle with the window shown for {idle_s} s");
        std::thread::sleep(Duration::from_secs(2).saturating_sub(idle_began.elapsed()));
        let (shown, sampled_until) = idle_period(pid, idle)?;
        out.shown = Some(shown);

        // Hidden: the shell releases the window after its delay and trims the heap.
        let hidden = app.wait_mark("window.hidden", 0, Duration::from_secs(30))?;
        if app.marks[hidden].0 <= sampled_until {
            bail!(
                "the window was hidden before the idle samples ended: the idle time with the \
                 window shown was not measured"
            );
        }
        eprintln!("perf: session: hidden to the tray; waiting for the window's release");
        let released = app.wait_mark("window.released", 0, Duration::from_secs(120))?;
        let released_at = app.marks[released].0;
        out.heap_released = heap(app.mark("window.released"));
        app.wait_mark("window.released_settled", 0, Duration::from_secs(30))?;
        out.heap_released_settled = heap(app.mark("window.released_settled"));
        // (That mark is written two seconds after the release; one more for it to be over.)
        std::thread::sleep(Duration::from_secs(3).saturating_sub(released_at.elapsed()));
        eprintln!("perf: session: idle and hidden for {idle_s} s");
        let (hidden, _) = idle_period(pid, idle)?;
        out.hidden = Some(hidden);

        // Quit through the tray menu, as a user would.
        session.send("tray-quit")?;
        out.qml_warnings = app.finish(Duration::from_secs(30))?;
        remove_dir(&profile)?;
        Ok(out)
    }

    /// One start with a live message.
    fn notify(&self, run: usize) -> Result<NotifyRun> {
        let profile = self.run_copy("run-notify")?;
        let mut session = MockSession::start(&self.mock_bin)?;
        let mut cmd = self.command(&session, &profile, Some("notify"))?;
        cmd.arg(format!("--perf-live-ms={LIVE_AFTER_MS}"))
            // The core's own marks (`recv.commit`) come through its log.
            .env("KOURIER_LOG", PERF_LOG_FILTER);
        let mut app = App::spawn(cmd)?;
        let shown = app.wait_mark("notify.shown", 0, Duration::from_secs(60))?;
        let (shown_at, mark) = app.marks[shown].clone();
        let recv = mark
            .get("recv_ms")
            .context("the notification has no arrival time")?;
        // The commit of the live message: the newest `recv.commit` that asked for a
        // notification and was printed before the notification was shown. Both lines came
        // through the same pipe, so the time between reading them is the time between them.
        let (commit_at, commit) = app
            .events
            .iter()
            .rev()
            .find(|(at, e)| {
                e.name == "recv.commit"
                    && e.get("notify").is_some_and(|n| n >= 1.0)
                    && *at <= shown_at
            })
            .context("the core printed no `recv.commit` for the live message")?;
        let commit_to_shown = shown_at.duration_since(*commit_at).as_secs_f64() * 1000.0;
        let took = commit.get("took_us").map(|us| us / 1000.0);
        if !session.wait_line("notify id=", Duration::from_secs(5)) {
            bail!("the mock notification server saw no notification");
        }
        eprintln!(
            "perf: notification {run}: {recv:.0} ms after the envelope, {commit_to_shown:.0} ms \
             after the core's commit"
        );
        // Let the start settle, then look at the memory of an app nobody touched.
        app.wait_mark("perf.idle", 0, Duration::from_secs(30))?;
        std::thread::sleep(Duration::from_secs(3));
        let rollup = read_rollup(app.pid())?;
        session.send("tray-quit")?;
        let warnings = app.finish(Duration::from_secs(30))?;
        remove_dir(&profile)?;
        Ok(NotifyRun {
            recv,
            event: mark.get("ms"),
            commit: commit_to_shown,
            took,
            rollup,
            warnings,
        })
    }
}

/// What decides the content of the reference profile, relative to the workspace root: the
/// crates that write what the seeding stores (the core, the databases, the attachment files,
/// the media thumbnails, the fake network and the contract between them), the composition
/// root (the seeder, and what it tells the core: keys, paths, configuration), and what
/// decides how they are built (dependency versions and features, the toolchain). Nothing of
/// the UI (`kourier-qt`, `kourier-viewmodel`), the desktop services, the binary crate or
/// this tool can change stored data.
const PROFILE_SOURCES: [&str; 11] = [
    "crates/kourier-core",
    "crates/kourier-db",
    "crates/kourier-blob",
    "crates/kourier-media",
    "crates/kourier-fake",
    "crates/kourier-model",
    "crates/kourier-backend-api",
    "crates/kourier-app",
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
];

/// The files below `rel` of `root`, as paths relative to `root`. Directories named `tests`
/// (integration tests store nothing in the profile) and `target` are left out.
fn profile_files(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let path = root.join(rel);
    if path.is_dir() {
        let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if name == "tests" || name == "target" {
            return Ok(());
        }
        for entry in std::fs::read_dir(&path).with_context(|| format!("read {}", path.display()))? {
            profile_files(root, &rel.join(entry?.file_name()), out)?;
        }
    } else if path.is_file() {
        out.push(rel.to_owned());
    }
    Ok(())
}

/// A hash of every file of [`PROFILE_SOURCES`] below `root` (path and content). A relinked
/// binary does not change it; a changed, new, removed or renamed file of the core does.
fn profile_digest(root: &Path) -> Result<u64> {
    let mut files = Vec::new();
    for rel in PROFILE_SOURCES {
        profile_files(root, Path::new(rel), &mut files)?;
    }
    files.sort();
    let mut hasher = DefaultHasher::new();
    for rel in &files {
        rel.hash(&mut hasher);
        std::fs::read(root.join(rel))
            .with_context(|| format!("read {}", rel.display()))?
            .hash(&mut hasher);
    }
    Ok(hasher.finish())
}

/// The profile's stamp: the digest of the sources that filled it and what seeding it
/// measured.
fn stamp_text(digest: Option<u64>, seed: Option<Seed>) -> String {
    let seed = seed.unwrap_or_default();
    serde_json::json!({
        // `null`: the binary that seeded was not built by that run (`--no-build`), so no
        // sources can be named; the next run that builds seeds again.
        "source_digest": digest.map(|d| format!("{d:016x}")),
        "messages": seed.messages,
        "events": seed.events,
        "messages_per_s": seed.messages_per_s,
        "files": seed.files,
        "downloads_s": seed.downloads_s,
    })
    .to_string()
}

/// Whether `stamp` says the profile was filled by the sources of `digest` (a stamp of the
/// older kind, which named the binary, has no digest and matches nothing).
fn stamp_matches(stamp: &serde_json::Value, digest: u64) -> bool {
    stamp["source_digest"].as_str() == Some(format!("{digest:016x}").as_str())
        && stamp_seed(stamp).is_some()
}

fn stamp_seed(stamp: &serde_json::Value) -> Option<Seed> {
    Some(Seed {
        messages: stamp["messages"].as_f64()?,
        events: stamp["events"].as_f64()?,
        messages_per_s: stamp["messages_per_s"].as_f64()?,
        files: stamp["files"].as_f64()?,
        downloads_s: stamp["downloads_s"].as_f64()?,
    })
}

/// The seed numbers of a `perf.seed` mark.
pub fn seed_from_mark(mark: &Mark) -> Option<Seed> {
    Some(Seed {
        messages: mark.get("messages")?,
        events: mark.get("events")?,
        messages_per_s: mark.get("messages_per_s")?,
        files: mark.get("files")?,
        downloads_s: mark.get("downloads_ms")? / 1000.0,
    })
}

/// Why the profile has to be seeded: `old` is its stamp, if there is one.
fn seed_reason(old: Option<&serde_json::Value>, profile_exists: bool) -> &'static str {
    match old {
        None => "no stamp says what filled it",
        Some(_) if !profile_exists => "there is no profile",
        Some(old)
            if old
                .get("source_digest")
                .is_none_or(serde_json::Value::is_null) =>
        {
            "its stamp does not say what filled it"
        }
        Some(_) => "the code that fills it changed",
    }
}

/// Makes sure `profile` holds the reference data seeded by the sources of `digest`
/// ([`profile_digest`], taken before the build, so a source edited meanwhile seeds again at
/// the next run); `bin` is the measuring binary that seeds it when it does not. Returns the
/// seed numbers and whether they are an earlier run's (the profile was reused).
/// `built`: this run built `bin` from the sources `digest` is of. A seeding by a binary that
/// was only found there (`--no-build`) is stamped without sources: that binary can be older
/// than they are, and a later run with a fresh build must not take its profile for current.
fn ensure_profile(
    bin: &Path,
    profile: &Path,
    config_home: &Path,
    keep: bool,
    digest: u64,
    built: bool,
) -> Result<(Option<Seed>, bool)> {
    let stamp_path = perf_dir().join("profile.stamp.json");
    let old: Option<serde_json::Value> = std::fs::read_to_string(&stamp_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    if let Some(old) = &old
        && profile.is_dir()
    {
        if stamp_matches(old, digest) {
            eprintln!("perf: reusing the reference profile (the code that fills it is unchanged)");
            return Ok((stamp_seed(old), true));
        }
        if keep {
            eprintln!(
                "perf: reusing the reference profile (--keep-profile; {})",
                if old
                    .get("source_digest")
                    .is_none_or(serde_json::Value::is_null)
                {
                    "its stamp does not say what filled it"
                } else {
                    "the code that fills it changed since"
                }
            );
            return Ok((stamp_seed(old), true));
        }
    }
    eprintln!(
        "perf: seeding the reference profile (about 45 s): {}",
        seed_reason(old.as_ref(), profile.is_dir())
    );
    let _ = std::fs::remove_file(&stamp_path);
    remove_dir(profile)?;
    let mut cmd = Command::new(bin);
    // (The seeding ends before Qt starts and uses no bus.)
    private_env(&mut cmd, config_home, &no_bus(), display_scale())?;
    let out = cmd
        .arg(format!("--perf-profile={}", profile.display()))
        .arg("--perf-seed")
        .env("KOURIER_LOG", "warn")
        .env("KOURIER_DESKTOP", "0")
        .stdin(Stdio::null())
        .output()
        .context("run the seeding")?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        bail!("seeding failed ({}):\n{stderr}", out.status);
    }
    let seed = stderr
        .lines()
        .filter_map(parse_trace_line)
        .find(|m| m.name == "perf.seed")
        .as_ref()
        .and_then(seed_from_mark)
        .context("the seeding printed no `perf.seed` mark")?;
    std::fs::write(&stamp_path, stamp_text(built.then_some(digest), Some(seed)))?;
    Ok((Some(seed), false))
}

/// Whether the normal release binary hides its memory from this user (§9.4): starts it on the
/// fixture, offscreen, and tries to read its `smaps_rollup`.
fn release_non_dumpable(bin: &Path, config_home: &Path) -> Result<bool> {
    let mut cmd = Command::new(bin);
    private_env(&mut cmd, config_home, &no_bus(), display_scale())?;
    let mut child = cmd
        .args(["--fixture", "--page=list", &format!("--size={WINDOW}")])
        .env("KOURIER_DESKTOP", "0")
        .env("KOURIER_LOG", "error")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("start {}", bin.display()))?;
    std::thread::sleep(Duration::from_millis(1500));
    let alive = matches!(child.try_wait(), Ok(None));
    let read = std::fs::read_to_string(format!("/proc/{}/smaps_rollup", child.id()));
    let _ = child.kill();
    let _ = child.wait();
    if !alive {
        bail!(
            "{} exited at once; cannot check its dump flag",
            bin.display()
        );
    }
    Ok(matches!(read, Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied))
}

/// Runs the Signal mock catch-up test of `kourier-e2e` once as a release build and reads its
/// rate. A test that fails gives no number, whatever it printed before.
fn mock_ingest() -> Result<(f64, bool)> {
    let out = cargo()
        .current_dir(root())
        .args([
            "test",
            "--release",
            "-p",
            "kourier-e2e",
            "--test",
            "signal_core",
            "five_thousand_queued_messages_ingest_at_500_per_second",
            "--",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env("KOURIER_REQUIRE_MOCK_HARNESS", "1")
        .stdin(Stdio::null())
        .output()
        .context("run the Signal mock catch-up test")?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let tail = || {
        let tail: Vec<&str> = text.lines().rev().take(12).collect();
        tail.into_iter().rev().collect::<Vec<_>>().join("\n")
    };
    let rate = parse_s7_line(&text);
    if !out.status.success() {
        bail!(
            "the test did not pass ({}){}:\n{}",
            out.status,
            rate.map_or_else(String::new, |(r, _)| format!(
                " after it printed {r:.0} messages/s"
            )),
            tail()
        );
    }
    rate.with_context(|| format!("the test passed and printed no rate:\n{}", tail()))
}

fn load_average() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|t| t.split_whitespace().next().map(str::to_owned))
        .unwrap_or_default()
}

fn machine() -> String {
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix("model name"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown CPU".into());
    let cores = std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get);
    format!(
        "{cpu}, {cores} threads, load average {} when the run began",
        load_average()
    )
}

/// Where the wall time of a run went.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Parts {
    /// The cargo builds.
    builds: Duration,
    /// Making sure the reference profile is there (reusing or seeding it).
    profile: Duration,
    /// Whether the profile was reused (`Some(true)`) or seeded (`Some(false)`); `None` when
    /// the run ended before it got there.
    reused: Option<bool>,
}

/// A whole number of seconds, rounded.
fn secs(d: Duration) -> u64 {
    (d + Duration::from_millis(500)).as_secs()
}

/// "3 min 02 s", or "47 s" below a minute.
fn long_time(d: Duration) -> String {
    let s = secs(d);
    if s < 60 {
        format!("{s} s")
    } else {
        format!("{} min {:02} s", s / 60, s % 60)
    }
}

impl Parts {
    /// The line that ends a run: its wall time and where it went. The measurements are
    /// everything else (the starts, the session, the Signal mock test with its own build, the
    /// report).
    fn line(&self, total: Duration) -> String {
        let measurements = total.saturating_sub(self.builds + self.profile);
        format!(
            "perf: {} in all: builds {} s, profile {} s{}, measurements {} s",
            long_time(total),
            secs(self.builds),
            secs(self.profile),
            match self.reused {
                Some(true) => " (reused)",
                Some(false) => " (seeded)",
                None => "",
            },
            secs(measurements)
        )
    }
}

/// `cargo xtask perf`. Returns the exit status: 0, or [`EXIT_MISSED`] when a budget is
/// missed; a measurement that failed is an error. Before it returns, also with an error, it
/// prints the wall time of its parts.
pub fn perf(args: &[String]) -> Result<i32> {
    let opts = parse_options(args)?;
    let began = Instant::now();
    let mut parts = Parts::default();
    let result = run_perf(&opts, &mut parts);
    eprintln!("{}", parts.line(began.elapsed()));
    result
}

/// Builds what a run measures: the measuring binary (kept as `bin`), the mock desktop
/// session and, with `plain_binary`, the normal release binary last.
///
/// The measuring build is linked as `plain` (`target/release/kourier`) and has the cargo
/// feature `dumpable`: its memory can be read by other processes of the user. It is removed
/// from that name whether or not keeping the copy worked, so that no error leaves it there
/// to be taken for the normal build; a full run links the normal one there again.
fn build_binaries(plain: &Path, bin: &Path, plain_binary: bool) -> Result<()> {
    run(
        "build the measuring binary (release, feature perf)",
        cargo().args(["build", "--release", "-p", "kourier", "--features", "perf"]),
    )?;
    let kept = std::fs::copy(plain, bin)
        .map(|_| ())
        .context("keep the measuring binary");
    let removed = match std::fs::remove_file(plain) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("remove the measuring build {}", plain.display()))
        }
        _ => Ok(()),
    };
    kept?;
    removed?;
    run(
        "build the mock desktop session",
        cargo().args([
            "build",
            "--release",
            "-p",
            "kourier-desktop",
            "--features",
            "testing",
            "--example",
            "mock_session",
        ]),
    )?;
    if plain_binary {
        // Last, so `target/release/kourier` is the normal build again.
        run(
            "build the release binary",
            cargo().args(["build", "--release", "-p", "kourier"]),
        )?;
    }
    Ok(())
}

fn run_perf(opts: &Options, parts: &mut Parts) -> Result<i32> {
    let dir = perf_dir();
    std::fs::create_dir_all(&dir)?;
    let release = target_dir().join("release");
    let bin = dir.join("kourier-perf");
    let plain = release.join("kourier");
    let mock_bin = release.join("examples/mock_session");
    let plain_binary = opts.measures_plain_binary();
    // Before the build: a source edited while it runs makes the next run seed again.
    let digest = profile_digest(&root())?;
    if opts.build {
        check_disk()?;
        let built = Instant::now();
        let done = build_binaries(&plain, &bin, plain_binary);
        parts.builds = built.elapsed();
        done?;
    }
    let mut needed = vec![&bin, &mock_bin];
    if plain_binary {
        needed.push(&plain);
    }
    for needed in needed {
        if !needed.is_file() {
            bail!("{} not found; run without --no-build", needed.display());
        }
    }

    let config_home = theme_home(&dir.join("env"), "light", "BreezeLight", "breeze", false)?;
    let mut m = Measured::default();
    if plain_binary {
        m.binary_bytes = Some(std::fs::metadata(&plain)?.len());
        m.release_non_dumpable = Some(release_non_dumpable(&plain, &config_home)?);
    }
    let profile = dir.join("profile");
    let profiled = Instant::now();
    let ensured = ensure_profile(
        &bin,
        &profile,
        &config_home,
        opts.keep_profile,
        digest,
        opts.build,
    );
    parts.profile = profiled.elapsed();
    (m.seed, m.seed_reused) = ensured?;
    parts.reused = Some(m.seed_reused);

    let runner = Runner {
        bin: bin.clone(),
        mock_bin: mock_bin.clone(),
        config_home,
        profile,
        quick: opts.quick,
    };
    let context = format!(
        "Measured {} on {}. Release build of this tree, offscreen (`QT_QPA_PLATFORM=offscreen`, \
         software renderer, window {WINDOW}), the reference profile on the fake network, desktop \
         services on a private bus with a mock notification server and tray.{}",
        date(),
        machine(),
        if opts.start {
            " Start-up only (`--start`): the notification starts, the session and the mock test \
             were left out."
        } else {
            ""
        }
    ) + &scale_note(&opts.scale);
    let cold_runs = if opts.quick || opts.start {
        1
    } else {
        COLD_STARTS
    };
    (m.startup, m.warnings) = runner.starts(opts.runs, cold_runs)?;
    if !opts.start {
        let notify_runs = if opts.quick { 2 } else { 5 };
        for run in 1..=notify_runs {
            let mut one = runner.notify(run)?;
            m.notify_recv.push(one.recv);
            m.notify_event.extend(one.event);
            m.notify_commit.push(one.commit);
            m.commit_took.extend(one.took);
            m.fresh.push(one.rollup);
            m.warnings.append(&mut one.warnings);
        }
        m.session = runner.session(opts.idle_s)?;
    }
    m.load_end = Some(load_average());

    if opts.mock {
        eprintln!(
            "perf: the Signal mock catch-up test (release build of kourier-e2e), {MOCK_RUNS} runs"
        );
        for run in 1..=MOCK_RUNS {
            match mock_ingest() {
                Ok((rate, release)) => {
                    eprintln!("perf: catch-up {run}/{MOCK_RUNS}: {rate:.0} messages/s");
                    m.mock_ingest.push(rate);
                    m.mock_release = release;
                }
                Err(e) => {
                    let first = e.to_string();
                    let first = first.lines().next().unwrap_or("failed").to_owned();
                    eprintln!("perf: the Signal mock test failed: {e:#}");
                    m.mock_failure = Some(first);
                    break;
                }
            }
        }
    }

    let report = render_report(&m, opts, &context);
    println!("{report}");
    // (A start-only table is mostly "not measured": it does not replace the last report.)
    let default_out = if opts.start {
        "report-start.md"
    } else {
        "report.md"
    };
    let out = opts.out.clone().unwrap_or_else(|| dir.join(default_out));
    std::fs::write(&out, &report).with_context(|| format!("write {}", out.display()))?;
    eprintln!("perf: report written to {}", out.display());
    let table = rows(&m, opts);
    let failed = failed(&table);
    if !failed.is_empty() {
        bail!(
            "{} measurement(s) failed, so this run has no verdict: {}",
            failed.len(),
            names(&failed)
        );
    }
    let status = exit_status(&table, opts.report_only);
    if status != 0 {
        eprintln!(
            "perf: {} budget(s) missed (exit status {status}; --report-only reports without \
             failing)",
            missed(&table).len()
        );
    }
    Ok(status)
}

/// The sentence of the report that names a display scale other than 1.
fn scale_note(scale: &str) -> String {
    if scale.parse::<f64>().is_ok_and(|x| (x - 1.0).abs() < 1e-9) {
        String::new()
    } else {
        format!(" Display scale {scale} (`QT_SCALE_FACTOR={scale}`, `--scale`).")
    }
}

fn date() -> String {
    Command::new("date")
        .arg("+%Y-%m-%d %H:%M")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Whether the command would end with a status other than 0.
    fn fails(rows: &[Row], report_only: bool) -> bool {
        exit_status(rows, report_only) != 0
    }

    #[test]
    fn options() {
        let o = parse_options(&[]).expect("defaults");
        assert_eq!((o.idle_s, o.runs), (300, 10));
        assert!(o.mock && o.build && !o.quick && !o.report_only && !o.keep_profile);
        assert_eq!(o.out, None);
        let o = parse_options(&args(&["--quick", "--report-only", "--out", "/tmp/r.md"]))
            .expect("quick");
        assert_eq!((o.idle_s, o.runs, o.mock), (10, 3, false));
        assert!(o.report_only);
        assert_eq!(o.out.as_deref(), Some(Path::new("/tmp/r.md")));
        // Explicit values win over --quick, in any order.
        let o = parse_options(&args(&["--idle", "60", "--quick", "--runs", "5"])).expect("mix");
        assert_eq!((o.idle_s, o.runs), (60, 5));
        let o = parse_options(&args(&["--no-mock", "--no-build", "--keep-profile"])).expect("no");
        assert!(!o.mock && !o.build && o.keep_profile);
        // Only a full run builds and judges the plain release binary.
        assert!(parse_options(&[]).expect("full").measures_plain_binary());
        assert!(
            !parse_options(&args(&["--quick"]))
                .expect("quick")
                .measures_plain_binary()
        );
        assert!(
            !parse_options(&args(&["--start"]))
                .expect("start")
                .measures_plain_binary()
        );
        for bad in [
            &["--idle"][..],
            &["--start", "--idle", "5"],
            &["--idle", "5", "--start"],
            &["--idle", "0"],
            &["--idle", "x"],
            &["--runs", "0"],
            &["--out"],
            &["--bogus"],
        ] {
            assert!(parse_options(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_display_scale_is_an_option_and_reaches_the_children() {
        // (F8, 2026-10-14: the run measured scale 1 only, where no picture comes from its file.)
        assert_eq!(parse_options(&args(&[])).unwrap().scale, "1");
        assert_eq!(
            parse_options(&args(&["--quick", "--scale", "1.25"]))
                .unwrap()
                .scale,
            "1.25"
        );
        for bad in [
            &["--scale"][..],
            &["--scale", "0"],
            &["--scale", "x"],
            &["--scale", "9"],
        ] {
            assert!(parse_options(&args(bad)).is_err(), "{bad:?}");
        }
        let home =
            std::env::temp_dir().join(format!("xtask-perf-scale-{}/config", std::process::id()));
        for scale in ["1", "1.25"] {
            let mut cmd = Command::new("true");
            private_env(&mut cmd, &home, "unix:path=/nonexistent", scale).unwrap();
            let got = cmd
                .get_envs()
                .find(|(k, _)| *k == "QT_SCALE_FACTOR")
                .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()));
            assert_eq!(got.as_deref(), Some(scale));
        }
        let _ = std::fs::remove_dir_all(home.parent().unwrap());
        // The report says so only when the scale is not 1.
        assert_eq!(scale_note("1"), "");
        assert_eq!(scale_note("1.0"), "");
        assert!(scale_note("1.25").contains("Display scale 1.25"));
    }

    #[test]
    fn trace_lines() {
        let m = parse_trace_line("kourier-trace: timeline.open at_ms=1803.9 ms=41.250 rows=50\n")
            .expect("a mark");
        assert_eq!(m.name, "timeline.open");
        assert_eq!(m.get("ms"), Some(41.25));
        assert_eq!(m.get("rows"), Some(50.0));
        assert_eq!(m.get("at_ms"), Some(1803.9));
        assert_eq!(m.get("nope"), None);
        let m = parse_trace_line("kourier-trace: startup.first_frame at_ms=212.4").expect("mark");
        assert_eq!(m.fields.len(), 1);
        // Log lines and damaged lines are not marks.
        for not in [
            "2026-10-07T05:00:00Z  INFO kourier_app: core started",
            "kourier-trace:",
            "kourier-trace: x at_ms",
            "kourier-trace: x at_ms=abc",
            " kourier-trace: x at_ms=1",
        ] {
            assert_eq!(parse_trace_line(not), None, "{not}");
        }
    }

    #[test]
    fn perf_events_of_the_core() {
        let line = "2026-10-07T08:00:53.956284Z TRACE kourier::perf: name=\"recv.commit\" account=1 events=100 messages=34 notify=3 catch_up=0 took_us=6818";
        let e = parse_perf_event(line).expect("an event");
        assert_eq!(e.name, "recv.commit");
        assert_eq!(e.get("notify"), Some(3.0));
        assert_eq!(e.get("took_us"), Some(6818.0));
        assert_eq!(e.get("name"), None, "the name is no field");
        let e = parse_perf_event(
            "2026-10-07T08:00:54Z TRACE kourier::perf: name=\"recv.catchup\" account=1 messages=5000 conversations=1 took_ms=477 per_second=10479",
        )
        .expect("an event");
        assert_eq!(
            (e.name.as_str(), e.get("per_second")),
            ("recv.catchup", Some(10479.0))
        );
        // Other targets, events without a name and events with a text field are no marks;
        // a trace line is not an event and an event is not a trace line.
        for not in [
            "2026-10-07T08:00:53Z  WARN kourier_core::supervisor: name=\"recv.commit\" account=1",
            "2026-10-07T08:00:53Z TRACE kourier::perf: account=1 events=3",
            "2026-10-07T08:00:53Z TRACE kourier::perf: name=\"\" events=3",
            "2026-10-07T08:00:53Z TRACE kourier::perf: name=\"recv.commit\" who=alice",
            "2026-10-07T08:00:53Z TRACE kourier::perf: name=\"recv.commit\" free text",
            "kourier-trace: notify.shown at_ms=1 ms=2",
        ] {
            assert_eq!(parse_perf_event(not), None, "{not}");
        }
        assert_eq!(parse_trace_line(line), None);
    }

    const ROLLUP: &str = "\
5650a8e00000-7ffc5a3fe000 ---p 00000000 00:00 0                          [rollup]
Rss:              201536 kB
Pss:              118272 kB
Pss_Dirty:         70000 kB
Pss_Anon:          69632 kB
Pss_File:          47616 kB
Pss_Shmem:          1024 kB
Shared_Clean:      90000 kB
Swap:                  0 kB
SwapPss:               0 kB
Locked:                0 kB
";

    #[test]
    fn smaps_rollup() {
        let r = parse_rollup(ROLLUP).expect("rollup");
        assert_eq!(
            r,
            Rollup {
                rss_kb: 201_536,
                pss_kb: 118_272,
                pss_anon_kb: 69_632,
                pss_file_kb: 47_616,
                pss_shmem_kb: 1024,
                swap_pss_kb: 0,
            }
        );
        assert!((mb(r.pss_kb) - 115.5).abs() < 0.01);
        assert_eq!(parse_rollup("Rss: 5 kB\n"), None, "no Pss line");
        assert_eq!(parse_rollup(""), None);
    }

    const SMAPS: &str = "\
55d0c0a00000-55d0c2000000 r-xp 00000000 fd:01 123 /home/u/k/target/perf/kourier-perf
Size:              22528 kB
Rss:               12000 kB
Pss:               12000 kB
Pss_Dirty:             0 kB
55d0c2000000-55d0c2100000 rw-p 01600000 fd:01 123 /home/u/k/target/perf/kourier-perf
Rss:                 400 kB
Pss:                 400 kB
55d0c3000000-55d0c9000000 rw-p 00000000 00:00 0                          [heap]
Rss:               50000 kB
Pss:               50000 kB
7f0000000000-7f0000400000 rw-p 00000000 00:00 0
Rss:                3000 kB
Pss:                3000 kB
7f0001000000-7f0001800000 r-xp 00000000 fd:01 77 /usr/lib/x86_64-linux-gnu/libQt6Quick.so.6.11.0
Rss:                6000 kB
Pss:                1500 kB
7f0001800000-7f0001900000 r--p 00800000 fd:01 77 /usr/lib/x86_64-linux-gnu/libQt6Quick.so.6.11.0
Rss:                 500 kB
Pss:                 250 kB
7f0002000000-7f0002100000 r--s 00000000 fd:01 99 /home/u/.cache/a file with spaces.cache
Rss:                 100 kB
Pss:                 100 kB
7f0003000000-7f0003001000 ---p 00000000 00:00 0
Rss:                   0 kB
Pss:                   0 kB
VmFlags: rd wr mr mw me ac
";

    #[test]
    fn smaps_by_mapping() {
        assert_eq!(
            parse_smaps(SMAPS),
            [
                ("[heap]".to_owned(), 50_000),
                ("kourier-perf".to_owned(), 12_400),
                ("[anonymous]".to_owned(), 3000),
                ("libQt6Quick".to_owned(), 1750),
                ("a file with spaces.cache".to_owned(), 100),
            ]
        );
        assert!(parse_smaps("").is_empty());
        // Sharing of one library: resident, what others map too and what counts for this
        // process, over its mappings.
        let lib = "\
7f0001000000-7f0001800000 r-xp 00000000 fd:01 77 /usr/lib/libQt6Core.so.6.11.1
Rss:                6000 kB
Pss:                1500 kB
Shared_Clean:       5600 kB
Shared_Dirty:          0 kB
Private_Clean:       400 kB
7f0001800000-7f0001900000 rw-p 00800000 fd:01 77 /usr/lib/libQt6Core.so.6.11.1
Rss:                 500 kB
Pss:                 500 kB
Shared_Clean:          0 kB
Shared_Dirty:         20 kB
Private_Dirty:       480 kB
7f0002000000-7f0002100000 r-xp 00000000 fd:01 78 /usr/lib/libQt6CoreX.so.6
Rss:                 900 kB
Shared_Clean:        900 kB
";
        assert_eq!(
            parse_smaps_sharing(lib, |group| group == QT_CORE),
            Some(LibraryShare {
                resident_kb: 6500,
                shared_kb: 5620,
                pss_kb: 2000,
            })
        );
        // Every Qt library together: the second one too (its mapping has no `Pss` line).
        assert_eq!(
            parse_smaps_sharing(lib, |group| group.starts_with(QT_LIBRARIES)),
            Some(LibraryShare {
                resident_kb: 7400,
                shared_kb: 6520,
                pss_kb: 2000,
            })
        );
        assert_eq!(parse_smaps_sharing(lib, |group| group == "libQt6Gui"), None);
        assert_eq!(
            parse_smaps_sharing(SMAPS, |group| group.starts_with(QT_LIBRARIES)),
            Some(LibraryShare {
                resident_kb: 6500,
                shared_kb: 0,
                pss_kb: 1750,
            })
        );
        // Attribute lines are no mapping headers.
        assert_eq!(mapping_header("Pss:                 250 kB"), None);
        assert_eq!(mapping_header("VmFlags: rd wr mr mw me ac"), None);
    }

    #[test]
    fn thread_counters() {
        let status = "Name:\tkourier-rt\nUmask:\t0077\nState:\tS (sleeping)\nvoluntary_ctxt_switches:\t1234\nnonvoluntary_ctxt_switches:\t7\n";
        assert_eq!(
            parse_thread_status(status),
            Some(("kourier-rt".to_owned(), 1234, 7))
        );
        assert_eq!(parse_thread_status("Name:\tx\n"), None);
        // comm may hold spaces and parentheses.
        let stat = "4242 (Qt (bearer) th) S 1 4242 4242 0 -1 4194304 100 0 0 0 250 50 0 0 20 0 9 0 12345 1 2";
        assert_eq!(parse_stat_ticks(stat), Some(300));
        assert_eq!(parse_stat_ticks("garbage"), None);
    }

    fn sample(tid: u32, name: &str, voluntary: u64, cpu_ticks: u64) -> ThreadSample {
        ThreadSample {
            tid,
            name: name.to_owned(),
            voluntary,
            involuntary: 0,
            cpu_ticks,
        }
    }

    #[test]
    fn activity_is_the_difference_per_thread_name() {
        let before = [
            sample(1, "kourier", 100, 50),
            sample(2, "kourier-rt", 10, 5),
            sample(3, "kourier-rt", 20, 5),
            sample(4, "gone", 1, 1),
        ];
        let after = [
            sample(1, "kourier", 160, 53),
            sample(2, "kourier-rt", 10, 5),
            sample(3, "kourier-rt", 25, 6),
            sample(9, "new", 3, 2),
        ];
        let activity = thread_activity(&before, &after, 100);
        assert_eq!(
            activity,
            [
                ThreadActivity {
                    name: "kourier".into(),
                    threads: 1,
                    wakeups: 60,
                    cpu_ms: 30.0
                },
                ThreadActivity {
                    name: "kourier-rt".into(),
                    threads: 2,
                    wakeups: 5,
                    cpu_ms: 10.0
                },
                ThreadActivity {
                    name: "new".into(),
                    threads: 1,
                    wakeups: 3,
                    cpu_ms: 20.0
                },
            ]
        );
    }

    fn idle_sample(at_s: f64, threads: &[ThreadSample], ticks: u64, pss_mb: u64) -> IdleSample {
        IdleSample {
            at_s,
            threads: threads.to_vec(),
            process_ticks: ticks,
            rollup: Rollup {
                pss_kb: pss_mb * 1024,
                ..Rollup::default()
            },
        }
    }

    #[test]
    fn an_idle_time_is_the_sum_of_its_parts() {
        // Three parts of 100 s. Thread 2 wakes six times in the second part only (a burst),
        // thread 3 ends after the first part, thread 9 starts in the second and thread 7
        // lives only between two samples of the middle.
        let samples = [
            idle_sample(
                0.0,
                &[
                    sample(1, "kourier", 100, 50),
                    sample(2, "kourier-rt", 10, 5),
                    sample(3, "pool", 4, 1),
                ],
                500,
                160,
            ),
            idle_sample(
                100.0,
                &[
                    sample(1, "kourier", 102, 50),
                    sample(2, "kourier-rt", 10, 5),
                    sample(3, "pool", 5, 1),
                    sample(7, "short", 2, 0),
                ],
                501,
                158,
            ),
            idle_sample(
                200.0,
                &[
                    sample(1, "kourier", 104, 51),
                    sample(2, "kourier-rt", 16, 5),
                    sample(9, "new", 1, 0),
                ],
                503,
                158,
            ),
            idle_sample(
                300.0,
                &[
                    sample(1, "kourier", 106, 51),
                    sample(2, "kourier-rt", 16, 5),
                    sample(9, "new", 1, 0),
                ],
                504,
                157,
            ),
        ];
        let idle = idle_from_samples(&samples, 100).expect("idle");
        assert_eq!(idle.seconds, 300.0);
        // 2 + 1 + 2 (thread 7, seen once) in the first part, 2 + 6 + 1 in the second, 2 in
        // the third.
        assert_eq!(idle.part_rates, [0.05, 0.09, 0.02]);
        assert_eq!(idle.worst_part(), Some(0.09));
        assert_eq!(idle.wakeups_per_s(), Some(16.0 / 300.0));
        let by_name: Vec<(&str, usize, u64)> = idle
            .threads
            .iter()
            .map(|t| (t.name.as_str(), t.threads, t.wakeups))
            .collect();
        assert_eq!(
            by_name,
            [
                ("kourier", 1, 6),
                ("kourier-rt", 1, 6),
                ("short", 1, 2),
                ("new", 1, 1),
                ("pool", 1, 1)
            ]
        );
        // The process's CPU time counts the threads that are gone too.
        assert_eq!(idle.cpu_ms, 40.0);
        assert_eq!((idle.started, idle.ended), (2, 2));
        assert_eq!(idle.pss_mb, [160.0, 158.0, 158.0, 157.0]);
        assert_eq!(
            idle.rollup.pss_kb,
            157 * 1024,
            "the last sample is the value"
        );
        assert_eq!(idle_from_samples(&samples[..1], 100), None);
        assert_eq!(idle_from_samples(&[], 100), None);
        assert_eq!(Idle::default().wakeups_per_s(), None);
        // The budget for the CPU time is per 5 min: this idle time's own length.
        assert_eq!(idle.cpu_ms_per_5_min(), Some(40.0));
        assert_eq!(Idle::default().cpu_ms_per_5_min(), None);
        // The samples do not say how the Qt libraries are shared: not known until the
        // mappings were read.
        assert_eq!(idle.sharing(), Sharing::Unknown);
    }

    #[test]
    fn the_mock_test_line() {
        let out = "running 1 test\nS7 throughput: 5000 queued messages committed in core.db 1.23 s after connect (4065 messages/s; app start to connect 0.40 s; release build)\ntest ok\n";
        assert_eq!(parse_s7_line(out), Some((4065.0, true)));
        let debug = "S7 throughput: 5000 queued messages committed in core.db 2.04 s after connect (2450 messages/s; app start to connect 0.61 s; debug build)";
        assert_eq!(parse_s7_line(debug), Some((2450.0, false)));
        assert_eq!(parse_s7_line("test result: FAILED"), None);
    }

    /// The bytes of `path` that are in the page cache, by `fincore` (util-linux); `None` when
    /// the tool is not installed.
    fn cached_bytes(path: &Path) -> Option<u64> {
        let out = Command::new("fincore")
            .args(["--bytes", "--noheadings", "--output", "RES"])
            .arg(path)
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }

    #[test]
    fn eviction_takes_files_out_of_the_page_cache() {
        // Below the target directory: a real file system (tmpfs has no pages to drop).
        let dir = target_dir().join(format!("tmp/xtask-perf-evict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).expect("dir");
        let (a, b) = (dir.join("a.bin"), dir.join("sub/b.bin"));
        std::fs::write(&a, vec![7u8; 1024 * 1024]).expect("write");
        std::fs::write(&b, vec![9u8; 300_000]).expect("write");
        std::os::unix::fs::symlink(&a, dir.join("link")).expect("symlink");
        // Freshly written and read back: in the cache.
        assert_eq!(std::fs::read(&a).expect("read").len(), 1024 * 1024);
        let before = cached_bytes(&a);
        let mut count = (0, 0);
        evict(&dir, &mut count).expect("evict");
        assert_eq!(
            count,
            (2, 1024 * 1024 + 300_000),
            "two files; the link is not followed"
        );
        // A path that is not there counts nothing and is no error.
        evict(&dir.join("nope"), &mut count).expect("missing");
        assert_eq!(count.0, 2);
        match (before, cached_bytes(&a), cached_bytes(&b)) {
            (Some(before), Some(a_after), Some(b_after)) => {
                assert!(before >= 1024 * 1024, "the file was cached: {before}");
                assert_eq!((a_after, b_after), (0, 0), "nothing of them is cached now");
            }
            _ => eprintln!("fincore is not installed; the page cache was not looked at"),
        }
        // The contents are what they were.
        assert!(
            std::fs::read(&b)
                .expect("read")
                .iter()
                .all(|byte| *byte == 9)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn df_output() {
        let out = "Filesystem     1024-blocks      Used Available Capacity Mounted on\n/dev/mapper/x    462000000 303000000 136314880      70% /\n";
        assert_eq!(parse_df_available_kb(out), Some(136_314_880));
        assert_eq!(parse_df_available_kb("Filesystem\n"), None);
    }

    #[test]
    fn statistics() {
        let v = [30.0, 10.0, 20.0, 40.0];
        assert_eq!(median(&v), Some(20.0));
        assert_eq!(percentile(&v, 0.95), Some(40.0));
        assert_eq!(percentile(&v, 0.0), Some(10.0));
        assert_eq!(percentile(&v, 1.0), Some(40.0));
        assert_eq!(median(&[7.0]), Some(7.0));
        assert_eq!(median(&[]), None);
        assert_eq!(max(&v), Some(40.0));
        assert_eq!(min(&v), Some(10.0));
    }

    #[test]
    fn verdicts() {
        let at_most = Some(Budget::AtMost(100.0));
        assert_eq!(judge(at_most, Some(50.0), None), Status::Ok);
        assert_eq!(judge(at_most, Some(80.0), None), Status::Ok);
        assert_eq!(judge(at_most, Some(81.0), None), Status::Close);
        assert_eq!(judge(at_most, Some(100.0), None), Status::Close);
        assert_eq!(judge(at_most, Some(100.1), None), Status::Miss);
        // A good median with one run outside the budget is close, not ok.
        assert_eq!(judge(at_most, Some(50.0), Some(120.0)), Status::Close);
        assert_eq!(judge(at_most, Some(50.0), Some(90.0)), Status::Ok);
        let at_least = Some(Budget::AtLeast(500.0));
        assert_eq!(judge(at_least, Some(2000.0), None), Status::Ok);
        assert_eq!(judge(at_least, Some(600.0), None), Status::Close);
        assert_eq!(judge(at_least, Some(499.0), None), Status::Miss);
        assert_eq!(judge(at_least, None, None), Status::NotMeasured);
        assert_eq!(judge(None, Some(1.0), None), Status::Info);
        // "Below" leaves the limit itself outside.
        let below = Some(Budget::Below(50.0));
        assert_eq!(judge(below, Some(40.0), None), Status::Ok);
        assert_eq!(judge(below, Some(40.1), None), Status::Close);
        assert_eq!(judge(below, Some(49.9), None), Status::Close);
        assert_eq!(judge(below, Some(50.0), None), Status::Miss);
        assert_eq!(judge(below, Some(10.0), Some(50.0)), Status::Close);
        assert_eq!(judge(Some(Budget::Yes), Some(1.0), None), Status::Ok);
        assert_eq!(judge(Some(Budget::Yes), Some(0.0), None), Status::Miss);
        assert_eq!(judge(None, None, None), Status::NotMeasured);
    }

    fn row(target: &str, budget: Option<Budget>, value: Option<f64>, worst: Option<f64>) -> Row {
        Row {
            target: target.into(),
            budget,
            limit: "",
            value,
            worst,
            unit: "ms",
            advisory: false,
            failed: false,
            how: "trace; median of 3 runs".into(),
        }
    }

    #[test]
    fn an_advisory_row_shows_its_budget_but_never_fails() {
        let mut scroll = row("Scroll", Some(Budget::AtMost(16.0)), Some(48.5), None);
        assert_eq!(scroll.status(), Status::Miss);
        scroll.advisory = true;
        assert_eq!(scroll.status(), Status::Info);
        assert!(!fails(std::slice::from_ref(&scroll), false));
        assert!(
            render_table(std::slice::from_ref(&scroll))
                .contains("| ≤ 16 ms, for information | 48.5 ms  | info")
        );
        // Not measured stays not measured.
        scroll.value = None;
        assert_eq!(scroll.status(), Status::NotMeasured);
    }

    #[test]
    fn the_table_is_aligned_markdown() {
        let rows = [
            row(
                "Start",
                Some(Budget::AtMost(400.0)),
                Some(212.4),
                Some(250.0),
            ),
            row("Ingest", Some(Budget::AtLeast(500.0)), Some(123.0), None),
            row("Size", None, Some(48.25), None),
            row("Skipped", Some(Budget::AtMost(1.0)), None, None),
            row("Hardened", Some(Budget::Yes), Some(1.0), None),
            row("Open", Some(Budget::Yes), Some(0.0), None),
            // One of several limits is named; "below" leaves the limit out; a row for
            // information says so where its budget stands.
            Row {
                limit: "Qt not shared",
                ..row("Memory", Some(Budget::AtMost(170.0)), Some(157.8), None)
            },
            row("CPU", Some(Budget::Below(50.0)), Some(50.0), None),
            Row {
                advisory: true,
                ..row("History", Some(Budget::AtLeast(800.0)), Some(123.0), None)
            },
        ];
        let table = render_table(&rows);
        let expected = "\
| Target   | Budget                    | Measured           | Status       | How measured            |
|----------|---------------------------|--------------------|--------------|-------------------------|
| Start    | ≤ 400 ms                  | 212 ms (worst 250) | ok           | trace; median of 3 runs |
| Ingest   | ≥ 500 ms                  | 123 ms             | MISS         | trace; median of 3 runs |
| Size     | —                         | 48.2 ms            | info         | trace; median of 3 runs |
| Skipped  | ≤ 1 ms                    | —                  | not measured | trace; median of 3 runs |
| Hardened | yes                       | yes                | ok           | trace; median of 3 runs |
| Open     | yes                       | no                 | MISS         | trace; median of 3 runs |
| Memory   | ≤ 170 ms (Qt not shared)  | 158 ms             | close        | trace; median of 3 runs |
| CPU      | < 50 ms                   | 50 ms              | MISS         | trace; median of 3 runs |
| History  | ≥ 800 ms, for information | 123 ms             | info         | trace; median of 3 runs |
";
        assert_eq!(table, expected);
        // Every line is as wide as the header (in characters).
        let width = table.lines().next().map_or(0, |l| l.chars().count());
        assert!(table.lines().all(|l| l.chars().count() == width), "{table}");
    }

    #[test]
    fn a_value_next_to_its_budget_keeps_the_digits_that_tell_them_apart() {
        let cell = |budget: Budget, value: f64, worst: Option<f64>| {
            value_text(&row("Memory", Some(budget), Some(value), worst))
        };
        // Rounded, a miss by 0.3 would read like the budget.
        assert_eq!(cell(Budget::AtMost(150.0), 150.3, None), "150.300 ms");
        assert_eq!(cell(Budget::AtMost(150.0), 149.8, None), "149.800 ms");
        assert_eq!(cell(Budget::AtMost(0.2), 0.2033, None), "0.203 ms");
        assert_eq!(cell(Budget::AtLeast(3000.0), 2999.6, None), "2999.600 ms");
        // The worst run too.
        assert_eq!(
            cell(Budget::AtMost(600.0), 520.0, Some(600.4)),
            "520 ms (worst 600.400)"
        );
        // A budget below 100 is written without decimals and a value beside it with one or
        // two: a miss by 0.04 must not read "70.0" against "≤ 70" either.
        assert_eq!(cell(Budget::AtMost(70.0), 70.04, None), "70.040 ms");
        assert_eq!(cell(Budget::AtMost(80.0), 79.96, None), "79.960 ms");
        assert_eq!(cell(Budget::AtMost(60.0), 60.004, None), "60.004 ms");
        assert_eq!(cell(Budget::Below(50.0), 49.99, None), "49.990 ms");
        assert_eq!(
            cell(Budget::AtMost(50.0), 21.0, Some(50.04)),
            "21 ms (worst 50.040)"
        );
        // On the budget, or clearly off it, nothing changes.
        assert_eq!(cell(Budget::AtMost(70.0), 70.0, None), "70 ms");
        assert_eq!(cell(Budget::AtMost(70.0), 69.9, None), "69.9 ms");
        assert_eq!(cell(Budget::AtMost(150.0), 150.0, None), "150 ms");
        assert_eq!(cell(Budget::AtMost(150.0), 157.8, None), "158 ms");
        assert_eq!(cell(Budget::Below(50.0), 12.0, None), "12 ms");
        assert_eq!(value_text(&row("Size", None, Some(48.25), None)), "48.2 ms");
    }

    #[test]
    fn a_missed_budget_fails_unless_report_only() {
        let ok = [row("Start", Some(Budget::AtMost(400.0)), Some(212.0), None)];
        let miss = [
            row("Start", Some(Budget::AtMost(400.0)), Some(212.0), None),
            row("Open", Some(Budget::AtMost(100.0)), Some(140.0), None),
        ];
        assert!(!fails(&ok, false));
        assert!(fails(&miss, false));
        assert!(!fails(&miss, true), "--report-only never fails");
        assert_eq!(missed(&miss).len(), 1);
        assert_eq!(missed(&miss)[0].target, "Open");
        // Nothing measured and nothing with a budget never fail the run.
        let none = [
            row("Skipped", Some(Budget::AtMost(1.0)), None, None),
            row("Size", None, Some(48.0), None),
        ];
        assert!(!fails(&none, false));
        assert_eq!(
            (
                exit_status(&ok, false),
                exit_status(&miss, false),
                exit_status(&miss, true)
            ),
            (0, EXIT_MISSED, 0)
        );
    }

    #[test]
    fn a_failed_measurement_is_not_a_slow_one() {
        // The test ran, printed a fine rate and then failed: no verdict, whatever the rate.
        let mut mock = row("Mock", Some(Budget::AtLeast(500.0)), Some(9000.0), None);
        assert_eq!(mock.status(), Status::Ok);
        mock.failed = true;
        assert_eq!(mock.status(), Status::Failed);
        let rows = [
            row("Start", Some(Budget::AtMost(400.0)), Some(212.0), None),
            mock.clone(),
        ];
        assert_eq!(failed(&rows).len(), 1);
        assert!(missed(&rows).is_empty(), "failed is not missed");
        // It fails the run with the status of an error, also with --report-only, and before
        // a missed budget is looked at.
        assert_eq!(exit_status(&rows, false), 1);
        assert_eq!(exit_status(&rows, true), 1);
        let both = [
            row("Open", Some(Budget::AtMost(100.0)), Some(140.0), None),
            mock.clone(),
        ];
        assert_eq!(exit_status(&both, false), 1);
        assert_ne!(EXIT_MISSED, 1);
        assert!(
            render_table(&rows).contains("| FAILED "),
            "{}",
            render_table(&rows)
        );
        // An advisory row that failed is still failed; a failed row without a number too.
        mock.advisory = true;
        assert_eq!(mock.status(), Status::Failed);
        mock.value = None;
        assert_eq!(mock.status(), Status::Failed);
    }

    /// An idle time of 5 min with all its wake-ups in the middle part, on a session whose
    /// other programs carry four fifths of the Qt libraries.
    fn idle(pss_kb: u64, name: &str, threads: usize, wakeups: u64, cpu_ms: f64) -> Idle {
        Idle {
            seconds: 300.0,
            threads: vec![ThreadActivity {
                name: name.into(),
                threads,
                wakeups,
                cpu_ms,
            }],
            #[allow(clippy::cast_precision_loss)]
            part_rates: vec![0.0, wakeups as f64 / 100.0, 0.0],
            cpu_ms,
            started: 1,
            ended: 2,
            pss_mb: vec![mb(pss_kb) + 2.0, mb(pss_kb) + 1.0, mb(pss_kb), mb(pss_kb)],
            rollup: Rollup {
                pss_kb,
                ..Rollup::default()
            },
            maps: Vec::new(),
            qt: Some(LibraryShare {
                resident_kb: 35_000,
                shared_kb: 33_000,
                pss_kb: 7000,
            }),
            qt_core: Some(LibraryShare {
                resident_kb: 5000,
                shared_kb: 4000,
                pss_kb: 1250,
            }),
        }
    }

    /// The idle time with the window shown of [`measured`], with `wakeups` in its 5 min.
    fn shown_idle(wakeups: u64) -> Idle {
        let mut shown = idle(0, "kourier", 1, wakeups, 12.0);
        shown.rollup = parse_rollup(ROLLUP).expect("rollup");
        shown.pss_mb = vec![117.0, 116.0, 115.5, 115.5];
        shown.maps = parse_smaps(SMAPS);
        // Nobody else carries this Qt.
        shown.qt = Some(LibraryShare {
            resident_kb: 35_000,
            shared_kb: 2000,
            pss_kb: 34_000,
        });
        shown.qt_core = Some(LibraryShare {
            resident_kb: 5200,
            shared_kb: 100,
            pss_kb: 5150,
        });
        shown
    }

    /// A full run in which no budget is missed.
    fn measured() -> Measured {
        let scroll =
            parse_trace_line("kourier-trace: scroll.frames at_ms=9 frames=420 p50_ms=16.4 p95_ms=21 max_ms=48.5 over16=30 over33=2")
                .expect("mark");
        Measured {
            startup: Startup {
                first_frame: vec![300.0, 280.0, 310.0],
                list_after_frame: vec![0.0, 12.0, 20.0],
                app_started: vec![80.0, 75.0, 90.0],
                spawn_to_frame: vec![340.0, 320.0, 350.0],
                qml_loaded: vec![200.0, 190.0, 210.0],
                list_refresh: vec![5.0, 6.0, 7.0],
                heap_at_frame: vec![40.0, 41.0, 42.0],
                heap_settled: Some((38.0, 4.0)),
                rows_and_delegates: Some((199.0, 16.0)),
                cold_first_frame: vec![520.0, 480.0, 610.0],
                cold_spawn_to_frame: vec![560.0, 515.0, 650.0],
                cold_evicted: Some((7400.0, 265.0)),
            },
            session: Session {
                // The second conversation is too short to count.
                opens: vec![140.0, 9.0, 40.0, 60.0],
                open_queries: vec![5.0, 1.0, 6.0, 9.0],
                open_rows: vec![50.0, 12.0, 50.0, 50.0],
                scroll: Some(scroll),
                shown: Some(shown_idle(30)),
                hidden: Some(idle(60 * 1024, "kourier-rt", 4, 3, 1.0)),
                heap_shown: Some((60.0, 20.0)),
                heap_released: Some((35.0, 3.0)),
                heap_released_settled: Some((20.0, 18.0)),
                idle_s: 300.0,
                qml_warnings: vec!["qrc:/x/Session.qml:3: TypeError".into()],
            },
            notify_recv: vec![62.0, 70.0],
            notify_event: vec![5.0, 6.0],
            notify_commit: vec![21.0, 25.0],
            commit_took: vec![11.5, 12.5],
            fresh: vec![
                Rollup {
                    pss_kb: 150 * 1024,
                    ..Rollup::default()
                },
                Rollup {
                    pss_kb: 152 * 1024,
                    ..Rollup::default()
                },
            ],
            seed: Some(Seed {
                messages: 50_000.0,
                events: 140_000.0,
                messages_per_s: 4000.0,
                files: 4100.0,
                downloads_s: 12.0,
            }),
            seed_reused: false,
            mock_ingest: Vec::new(),
            mock_release: false,
            mock_failure: None,
            binary_bytes: Some(48 * 1024 * 1024),
            release_non_dumpable: Some(true),
            warnings: vec!["qrc:/x/Start.qml:1: ReferenceError".into()],
            load_end: Some("0.42".into()),
        }
    }

    /// The table of a full run of `m`.
    fn table_of(m: &Measured) -> Vec<Row> {
        rows(m, &parse_options(&[]).expect("options"))
    }

    /// The row of `m` whose target begins with `target`.
    fn row_of(m: &Measured, target: &str) -> Row {
        table_of(m)
            .into_iter()
            .find(|r| r.target.starts_with(target))
            .unwrap_or_else(|| panic!("row {target}"))
    }

    /// The targets `m` misses; a run that misses any ends with [`EXIT_MISSED`].
    fn missed_targets(m: &Measured) -> Vec<String> {
        let table = table_of(m);
        let targets: Vec<String> = missed(&table).iter().map(|r| r.target.clone()).collect();
        assert_eq!(
            exit_status(&table, false),
            if targets.is_empty() { 0 } else { EXIT_MISSED }
        );
        assert_eq!(exit_status(&table, true), 0, "--report-only");
        targets
    }

    #[test]
    fn the_rows_carry_the_budgets_of_the_architecture() {
        let o = parse_options(&[]).expect("options");
        let mut m = measured();
        let rows = rows(&m, &o);
        let by_target = |t: &str| {
            rows.iter()
                .find(|r| r.target.starts_with(t))
                .unwrap_or_else(|| panic!("row {t}"))
        };
        let start = by_target("Cold start");
        assert_eq!(start.budget, Some(Budget::AtMost(400.0)));
        assert_eq!((start.value, start.worst), (Some(300.0), Some(310.0)));
        assert_eq!(start.status(), Status::Ok);
        assert!(start.how.contains("median of 3 starts"), "{}", start.how);
        assert!(start.how.contains("no scripted driver"), "{}", start.how);
        // The start with cold files has a budget of its own: the median is inside it, one
        // start was not.
        let cold = by_target("Start with Kourier's own files");
        assert_eq!((cold.value, cold.worst), (Some(520.0), Some(610.0)));
        assert_eq!(cold.budget, Some(Budget::AtMost(600.0)));
        assert_eq!((cold.advisory, cold.status()), (false, Status::Close));
        assert!(cold.how.contains("7400 files, 265 MB"), "{}", cold.how);
        assert_eq!(by_target("First frame").budget, Some(Budget::AtMost(50.0)));
        assert_eq!(by_target("First frame").status(), Status::Ok);
        let open = by_target("Open a conversation");
        assert_eq!(open.budget, Some(Budget::AtMost(80.0)));
        assert_eq!(
            (open.value, open.worst),
            (Some(60.0), Some(140.0)),
            "the open of the short conversation does not count"
        );
        assert_eq!(open.status(), Status::Close, "one open was over the budget");
        assert!(
            open.how.contains("median of 3 conversations")
                && open.how.contains("(1 with fewer than 50 left out)")
                && open.how.contains("creates the chat page: 140 ms"),
            "{}",
            open.how
        );
        // Memory with the window shown: the anonymous part, and the PSS against the higher
        // limit, because nobody else carries this session's Qt. The row says so.
        let anon = by_target("Idle memory, window shown: anonymous");
        assert_eq!(
            (anon.value, anon.budget, anon.limit, anon.status()),
            (Some(68.0), Some(Budget::AtMost(100.0)), "", Status::Ok)
        );
        assert!(
            anon.how.contains("`Pss_Anon`") && anon.how.contains("300 s idle"),
            "{}",
            anon.how
        );
        let shown = by_target("Idle memory, window shown: PSS");
        assert_eq!(
            (shown.budget, shown.limit),
            (Some(Budget::AtMost(170.0)), "Qt not shared")
        );
        assert_eq!((shown.value, shown.status()), (Some(115.5), Status::Ok));
        assert!(
            shown.how.contains(
                "(at the samples: 117.0 → 116.0 → 115.5 → 115.5 MB; anonymous 68.0 MB, \
                 file-backed 46.5 MB). Limit: §1.3's higher one, 170 MB (150 MB on a session \
                 that shares its Qt): other processes carry only 3 % of the 34.2 MB of Qt 6 \
                 libraries resident here (they count with 33.2 MB), less than half."
            ) && shown
                .how
                .contains("nothing opened: 150 MB (median of 2 starts, worst 152)"),
            "{}",
            shown.how
        );
        // Hidden: the same two numbers; this idle time ran beside programs on the same Qt.
        let anon = by_target("Idle memory, hidden to tray: anonymous");
        assert_eq!(anon.budget, Some(Budget::AtMost(70.0)));
        assert!(
            anon.how.contains("60 MB is not enforced yet"),
            "{}",
            anon.how
        );
        let hidden = by_target("Idle memory, hidden to tray: PSS");
        assert_eq!(
            (hidden.budget, hidden.limit),
            (Some(Budget::AtMost(110.0)), "Qt shared")
        );
        assert_eq!((hidden.value, hidden.status()), (Some(60.0), Status::Ok));
        assert!(
            hidden.how.ends_with(
                "Limit: §1.3's lower one, 110 MB (145 MB on a session that does not share its \
                 Qt): other processes carry 80 % of the 34.2 MB of Qt 6 libraries resident \
                 here (they count with 6.8 MB), half or more"
            ),
            "{}",
            hidden.how
        );
        // Idle CPU: the wake-ups of the whole time, of its busiest part, and the CPU time.
        let cpu = by_target("Idle CPU, window shown: wake-ups");
        assert_eq!(
            (cpu.budget, cpu.value, cpu.worst),
            (Some(Budget::AtMost(0.2)), Some(0.1), None)
        );
        assert_eq!(cpu.status(), Status::Ok);
        assert!(
            cpu.how.contains("over 300 s, on the fake network")
                && cpu.how.contains("1 threads started and 2 ended"),
            "{}",
            cpu.how
        );
        let part = by_target("Idle CPU, window shown: busiest part");
        assert_eq!(
            (part.budget, part.value, part.status()),
            (Some(Budget::AtMost(0.5)), Some(0.3), Status::Ok)
        );
        assert!(
            part.how
                .contains("in 3 parts of 100 s: the wake-ups per second of the busiest one")
                && part.how.contains("(each part: 0.00, 0.30, 0.00)"),
            "{}",
            part.how
        );
        let time = by_target("Idle CPU, window shown: process CPU time");
        assert_eq!(
            (time.budget, time.value, time.unit, time.status()),
            (
                Some(Budget::Below(50.0)),
                Some(12.0),
                "ms per 5 min",
                Status::Ok
            )
        );
        assert!(time.how.contains("12 ms in 300 s"), "{}", time.how);
        let hidden = by_target("Idle CPU, hidden to tray: wake-ups");
        assert_eq!((hidden.value, hidden.status()), (Some(0.01), Status::Ok));
        assert!(
            hidden
                .how
                .contains("over 300 s after the window was released, on the fake network"),
            "{}",
            hidden.how
        );
        assert_eq!(
            by_target("Idle CPU, hidden to tray: busiest part").value,
            Some(0.03)
        );
        // A notification: the whole way, and from the core's commit.
        let notify = by_target("Receive → notification");
        assert_eq!(notify.budget, Some(Budget::AtMost(150.0)));
        assert_eq!((notify.value, notify.worst), (Some(62.0), Some(70.0)));
        assert_eq!(notify.status(), Status::Ok);
        assert!(
            notify.how.contains("(`took_us`): 11.5 ms")
                && notify.how.contains("event → accepted: 5 ms"),
            "{}",
            notify.how
        );
        let commit = by_target("Receive → notification: `recv.commit` → `notify.shown`");
        assert_eq!(
            (commit.budget, commit.value, commit.worst),
            (Some(Budget::AtMost(50.0)), Some(21.0), Some(25.0))
        );
        assert_eq!(commit.status(), Status::Ok);
        assert!(commit.how.contains("median of 2 starts"), "{}", commit.how);
        let fake = by_target("Catch-up ingest, fake");
        assert_eq!(fake.budget, Some(Budget::AtLeast(800.0)));
        assert_eq!(fake.status(), Status::Info, "§1.3 has it for information");
        assert!(!fake.how.contains("earlier run"), "{}", fake.how);
        let mock = by_target("Catch-up ingest, Signal");
        assert_eq!(
            (mock.budget, mock.limit),
            (Some(Budget::AtLeast(3000.0)), "release build")
        );
        assert_eq!(mock.status(), Status::NotMeasured);
        assert!(mock.how.starts_with("skipped"), "{}", mock.how);
        let scroll = by_target("Timeline scroll");
        assert_eq!(
            (scroll.budget, scroll.value, scroll.status()),
            (Some(Budget::AtMost(16.0)), Some(48.5), Status::Info)
        );
        let size = by_target("Binary size");
        assert_eq!(
            (size.budget, size.value, size.status()),
            (Some(Budget::AtMost(60.0)), Some(48.0), Status::Ok)
        );
        assert_eq!(
            by_target("Release build is non-dumpable").status(),
            Status::Ok
        );
        assert_eq!(rows.len(), 21);
        assert!(missed(&rows).is_empty(), "{:?}", missed(&rows));
        assert_eq!(exit_status(&rows, false), 0);

        // The mock test: the median of its runs, the slowest as the worst; a profile that
        // was reused says so.
        m.mock_ingest = vec![9000.0, 450.0, 10_000.0];
        m.mock_release = true;
        m.seed_reused = true;
        let again = super::rows(&m, &o);
        let mock = again
            .iter()
            .find(|r| r.target.starts_with("Catch-up ingest, Signal"))
            .expect("row");
        assert_eq!((mock.value, mock.worst), (Some(9000.0), Some(450.0)));
        assert_eq!(mock.status(), Status::Close, "one run was below the budget");
        assert!(
            mock.how.contains("(release build)") && mock.how.contains("median of 3 runs"),
            "{}",
            mock.how
        );
        assert!(
            again.iter().any(|r| r.how.contains("by an earlier run")),
            "the reused profile"
        );
        // The test failed after two runs: no verdict, although both rates were fine.
        m.mock_ingest = vec![9000.0, 9500.0];
        m.mock_failure = Some("the test did not pass (exit status: 101)".into());
        let again = super::rows(&m, &o);
        let mock = again
            .iter()
            .find(|r| r.target.starts_with("Catch-up ingest, Signal"))
            .expect("row");
        assert_eq!(mock.status(), Status::Failed);
        assert_eq!(
            mock.how,
            "the test failed: the test did not pass (exit status: 101)"
        );
        assert_eq!(exit_status(&again, true), 1);
        // Opens were measured, but only of conversations that are too short: failed.
        m.mock_failure = None;
        m.session.open_rows = vec![3.0, 12.0, 20.0, 49.0];
        let again = super::rows(&m, &o);
        let open = again
            .iter()
            .find(|r| r.target.starts_with("Open a conversation"))
            .expect("row");
        assert_eq!((open.value, open.status()), (None, Status::Failed));
        // No session at all (nothing opened, no idle time) is "not measured", and the PSS
        // rows then show the lower limit: nothing says the session does not share.
        m.session = Session::default();
        let again = super::rows(&m, &o);
        let open = again
            .iter()
            .find(|r| r.target.starts_with("Open a conversation"))
            .expect("row");
        assert_eq!(open.status(), Status::NotMeasured);
        let pss = again
            .iter()
            .find(|r| r.target == "Idle memory, hidden to tray: PSS")
            .expect("row");
        assert_eq!(
            (pss.budget, pss.limit, pss.status()),
            (
                Some(Budget::AtMost(110.0)),
                "Qt sharing not read",
                Status::NotMeasured
            )
        );
        assert_eq!(exit_status(&again, false), 0);
    }

    #[test]
    fn starts_and_opens_are_held_to_the_confirmed_budgets() {
        assert!(missed_targets(&measured()).is_empty());
        // A start with Kourier's own files out of the page cache: 600 ms, enforced.
        let cold = "Start with Kourier's own files out of the page cache";
        let mut m = measured();
        m.startup.cold_first_frame = vec![480.0];
        assert_eq!(row_of(&m, cold).status(), Status::Ok);
        m.startup.cold_first_frame = vec![600.0];
        assert_eq!(row_of(&m, cold).status(), Status::Close);
        assert!(missed_targets(&m).is_empty());
        m.startup.cold_first_frame = vec![600.5];
        assert_eq!(missed_targets(&m), [cold]);
        // The start on a warm page cache keeps its 400 ms.
        let mut m = measured();
        m.startup.first_frame = vec![400.0];
        assert!(missed_targets(&m).is_empty());
        m.startup.first_frame = vec![400.5];
        assert_eq!(missed_targets(&m), ["Cold start to first frame"]);
        // The list: 50 ms after the first frame (it was 300).
        let mut m = measured();
        m.startup.list_after_frame = vec![50.0];
        assert_eq!(row_of(&m, "First frame").status(), Status::Close);
        assert!(missed_targets(&m).is_empty());
        m.startup.list_after_frame = vec![50.5];
        assert_eq!(
            missed_targets(&m),
            ["First frame to populated conversation list"]
        );
        // Opening a conversation: 80 ms (it was 100).
        let mut m = measured();
        m.session.opens = vec![80.0];
        m.session.open_rows = vec![50.0];
        assert_eq!(row_of(&m, "Open a conversation").status(), Status::Close);
        assert!(missed_targets(&m).is_empty());
        m.session.opens = vec![80.5];
        assert_eq!(
            missed_targets(&m),
            ["Open a conversation (50 rows visible)"]
        );
        let table = render_table(&table_of(&measured()));
        for cell in ["| ≤ 400 ms ", "| ≤ 600 ms ", "| ≤ 50 ms ", "| ≤ 80 ms "] {
            assert!(table.contains(cell), "{cell}\n{table}");
        }
    }

    #[test]
    fn a_session_shares_its_qt_when_others_carry_half_of_it() {
        let qt = |resident_kb, pss_kb| {
            Some(LibraryShare {
                resident_kb,
                shared_kb: 0,
                pss_kb,
            })
        };
        // Nobody else maps these files: every page counts in full.
        assert_eq!(Sharing::of(qt(30_000, 30_000)), Sharing::Unshared);
        // The line is half of the resident pages, to the kB.
        assert_eq!(Sharing::of(qt(30_000, 15_001)), Sharing::Unshared);
        assert_eq!(Sharing::of(qt(30_000, 15_000)), Sharing::Shared);
        assert_eq!(Sharing::of(qt(30_000, 4000)), Sharing::Shared);
        // That other processes map the pages does not decide it, what the pages still
        // count does: the session of 2026-10-07 (most pages mapped by a few programs).
        let few_sharers = LibraryShare {
            resident_kb: 35_756,
            shared_kb: 21_948,
            pss_kb: 19_469,
        };
        assert_eq!(Sharing::of(Some(few_sharers)), Sharing::Unshared);
        let carried = few_sharers.carried_by_others().expect("resident");
        assert!((carried - 0.4555).abs() < 0.0001, "{carried}");
        assert_eq!(
            qt(30_000, 15_000).and_then(|qt| qt.carried_by_others()),
            Some(0.5)
        );
        // Not read, or nothing resident: not known, and then the lower limit applies.
        assert_eq!(Sharing::of(None), Sharing::Unknown);
        assert_eq!(Sharing::of(qt(0, 0)), Sharing::Unknown);
        assert_eq!(qt(0, 0).and_then(|qt| qt.carried_by_others()), None);
        for (budget, lower, higher) in [(MEMORY_SHOWN, 150.0, 170.0), (MEMORY_HIDDEN, 110.0, 145.0)]
        {
            assert_eq!(Sharing::Shared.pss_limit(budget), lower);
            assert_eq!(Sharing::Unknown.pss_limit(budget), lower);
            assert_eq!(Sharing::Unshared.pss_limit(budget), higher);
        }
        assert_eq!(
            (MEMORY_SHOWN.anonymous, MEMORY_HIDDEN.anonymous),
            (100.0, 70.0)
        );
    }

    #[test]
    fn idle_memory_is_judged_by_its_anonymous_part_and_by_the_pss_limit_of_its_session() {
        let share = |pss_kb| {
            Some(LibraryShare {
                resident_kb: 30_000,
                shared_kb: 29_000,
                pss_kb,
            })
        };
        // Others carry exactly half of the Qt libraries, and one kB less than half.
        let (shared, unshared) = (share(15_000), share(15_001));
        // The state, and its limits in MB: anonymous, PSS shared, PSS unshared.
        for (hidden, anon_mb, lower_mb, higher_mb) in [(false, 100, 150, 170), (true, 70, 110, 145)]
        {
            let state = if hidden {
                "Idle memory, hidden to tray"
            } else {
                "Idle memory, window shown"
            };
            let (anon_row, pss_row) = (format!("{state}: anonymous"), format!("{state}: PSS"));
            let with = |anon_kb: u64, pss_kb: u64, qt: Option<LibraryShare>| {
                let mut m = measured();
                let idle = if hidden {
                    m.session.hidden.as_mut()
                } else {
                    m.session.shown.as_mut()
                }
                .expect("idle");
                idle.rollup.pss_anon_kb = anon_kb;
                idle.rollup.pss_kb = pss_kb;
                idle.qt = qt;
                m
            };
            let (anon, lower, higher) = (anon_mb * 1024, lower_mb * 1024, higher_mb * 1024);
            let small = 40 * 1024;

            // The anonymous part: its limit on any session, to the kB.
            for qt in [shared, unshared, None] {
                let m = with(anon, small, qt);
                assert_eq!(row_of(&m, &anon_row).status(), Status::Close, "{state}");
                assert!(missed_targets(&m).is_empty(), "{state}");
                assert_eq!(
                    missed_targets(&with(anon + 1, small, qt)),
                    [anon_row.as_str()]
                );
            }
            assert_eq!(
                row_of(&with(small, small, shared), &anon_row).status(),
                Status::Ok
            );

            // The PSS on a session that shares its Qt: the lower limit.
            let m = with(small, lower, shared);
            let row = row_of(&m, &pss_row);
            assert_eq!((row.limit, row.status()), ("Qt shared", Status::Close));
            assert!(row.how.contains("Limit: §1.3's lower one"), "{}", row.how);
            assert!(missed_targets(&m).is_empty(), "{state}");
            assert!(
                render_table(&table_of(&m)).contains(&format!("| ≤ {lower_mb} MB (Qt shared) ")),
                "{state}"
            );
            assert_eq!(
                missed_targets(&with(small, lower + 1, shared)),
                [pss_row.as_str()]
            );

            // The same PSS where the others carry less than half: the higher limit, to the
            // kB too.
            let m = with(small, lower + 1, unshared);
            let row = row_of(&m, &pss_row);
            assert_eq!(row.limit, "Qt not shared");
            assert_ne!(row.status(), Status::Miss, "{state}");
            assert!(row.how.contains("Limit: §1.3's higher one"), "{}", row.how);
            assert!(missed_targets(&m).is_empty(), "{state}");
            let m = with(small, higher, unshared);
            assert!(missed_targets(&m).is_empty(), "{state}");
            assert!(
                render_table(&table_of(&m))
                    .contains(&format!("| ≤ {higher_mb} MB (Qt not shared) ")),
                "{state}"
            );
            assert_eq!(
                missed_targets(&with(small, higher + 1, unshared)),
                [pss_row.as_str()]
            );
            // Above the higher limit is a miss on a sharing session all the more.
            assert_eq!(
                missed_targets(&with(small, higher + 1, shared)),
                [pss_row.as_str()]
            );

            // Sharing that was not read gets the lower limit, and the row says so.
            let m = with(small, lower + 1, None);
            let row = row_of(&m, &pss_row);
            assert_eq!(row.limit, "Qt sharing not read");
            assert!(row.how.contains("could not be read"), "{}", row.how);
            assert_eq!(missed_targets(&m), [pss_row.as_str()]);

            // Both numbers are judged: a small PSS does not excuse the anonymous part, nor
            // the other way round.
            assert_eq!(
                missed_targets(&with(anon + 1, higher + 1, unshared)),
                [anon_row.as_str(), pss_row.as_str()]
            );
            assert!(
                render_table(&table_of(&with(small, small, shared)))
                    .contains(&format!("| ≤ {anon_mb} MB ")),
                "{state}"
            );
        }
        // Hidden, the anonymous limit is 70 MB: the 60 MB §1.3 names for later are not
        // enforced.
        let mut m = measured();
        m.session.hidden.as_mut().expect("idle").rollup.pss_anon_kb = 65 * 1024;
        assert!(missed_targets(&m).is_empty());
        assert_eq!(
            row_of(&m, "Idle memory, hidden to tray: anonymous").status(),
            Status::Close
        );
    }

    #[test]
    fn idle_cpu_has_three_limits_and_they_need_five_minutes() {
        // An idle time of `seconds` (asked for: the same) with `wakeups` in all, the busiest
        // part at `part` wake-ups per second and `cpu_ms` of CPU time; `hidden`: which one.
        let with = |hidden: bool, wakeups: u64, part: f64, cpu_ms: f64, seconds: f64| {
            let mut m = measured();
            let idle = if hidden {
                m.session.hidden.as_mut()
            } else {
                m.session.shown.as_mut()
            }
            .expect("idle");
            idle.seconds = seconds;
            idle.threads[0].wakeups = wakeups;
            idle.part_rates = vec![0.0, part, 0.0];
            idle.cpu_ms = cpu_ms;
            m.session.idle_s = seconds;
            m
        };
        for (hidden, state) in [(false, "window shown"), (true, "hidden to tray")] {
            let rate = format!("Idle CPU, {state}: wake-ups");
            let part = format!("Idle CPU, {state}: busiest part");
            let time = format!("Idle CPU, {state}: process CPU time");
            // What phase I measured: far inside all three.
            let m = with(hidden, 12, 0.09, 10.0, 300.0);
            for target in [&rate, &part, &time] {
                assert_eq!(row_of(&m, target).status(), Status::Ok, "{target}");
            }
            assert!(missed_targets(&m).is_empty());
            // 0.2 wake-ups per second over the whole time: 60 in 5 min pass, 61 do not.
            let m = with(hidden, 60, 0.3, 10.0, 300.0);
            assert_eq!(row_of(&m, &rate).value, Some(0.2));
            assert_eq!(row_of(&m, &rate).status(), Status::Close);
            assert!(missed_targets(&m).is_empty());
            assert_eq!(
                missed_targets(&with(hidden, 61, 0.3, 10.0, 300.0)),
                [rate.as_str()]
            );
            // No part above 0.5 per second, however quiet the whole time was: a miss now,
            // where the tool said "close" before.
            let m = with(hidden, 12, 0.5, 10.0, 300.0);
            assert_eq!(row_of(&m, &part).status(), Status::Close);
            assert!(missed_targets(&m).is_empty());
            assert_eq!(
                missed_targets(&with(hidden, 12, 0.51, 10.0, 300.0)),
                [part.as_str()]
            );
            // CPU time below 50 ms per 5 min: 40 ms pass, 50 ms do not.
            assert_eq!(
                row_of(&with(hidden, 12, 0.09, 40.0, 300.0), &time).status(),
                Status::Ok
            );
            let m = with(hidden, 12, 0.09, 49.0, 300.0);
            assert_eq!(row_of(&m, &time).status(), Status::Close);
            assert!(missed_targets(&m).is_empty());
            assert_eq!(
                missed_targets(&with(hidden, 12, 0.09, 50.0, 300.0)),
                [time.as_str()]
            );
            // The last sample of a real idle time comes a moment late. The CPU time comes in
            // clock ticks of 10 ms: five of them stay 50 ms and a miss, not 49.99993 ms.
            let mut late = with(hidden, 12, 0.09, 50.0, 300.0);
            let idle = if hidden {
                late.session.hidden.as_mut()
            } else {
                late.session.shown.as_mut()
            }
            .expect("idle");
            idle.seconds = 300.0004;
            assert_eq!(row_of(&late, &time).value, Some(50.0));
            assert_eq!(missed_targets(&late), [time.as_str()]);
            // A longer idle time is judged per 5 min of it.
            let m = with(hidden, 24, 0.09, 90.0, 600.0);
            assert_eq!(row_of(&m, &time).value, Some(45.0));
            assert_eq!(row_of(&m, &rate).value, Some(0.04));
            assert!(missed_targets(&m).is_empty());
            assert_eq!(
                missed_targets(&with(hidden, 24, 0.09, 100.0, 600.0)),
                [time.as_str()]
            );
            // All three at once are three misses.
            assert_eq!(
                missed_targets(&with(hidden, 900, 9.0, 500.0, 300.0)),
                [rate.as_str(), part.as_str(), time.as_str()]
            );
            // A run that idled for less than 5 min cannot show these budgets: its rows are
            // for information and never fail, whatever they read.
            for seconds in [10.0, 299.0] {
                let m = with(hidden, 900, 9.0, 500.0, seconds);
                for target in [&rate, &part, &time] {
                    let row = row_of(&m, target);
                    assert_eq!((row.advisory, row.status()), (true, Status::Info));
                    assert!(
                        row.how.contains(&format!(
                            "§1.3's budget is per 300 s of idle and this run idled for \
                             {seconds:.0} s: for information"
                        )),
                        "{}",
                        row.how
                    );
                }
                assert!(missed_targets(&m).is_empty());
            }
        }
        // The budget cells say what is enforced, and when nothing is.
        let table = render_table(&table_of(&with(false, 12, 0.09, 10.0, 300.0)));
        for cell in [
            "| ≤ 0.20 wakeups/s ",
            "| ≤ 0.50 wakeups/s ",
            "| < 50 ms per 5 min ",
        ] {
            assert!(table.contains(cell), "{cell}\n{table}");
        }
        let table = render_table(&table_of(&with(false, 12, 0.09, 10.0, 10.0)));
        for cell in [
            "| ≤ 0.20 wakeups/s, for information ",
            "| ≤ 0.50 wakeups/s, for information ",
            "| < 50 ms per 5 min, for information ",
        ] {
            assert!(table.contains(cell), "{cell}\n{table}");
        }
    }

    #[test]
    fn a_longer_idle_time_is_sampled_in_more_parts() {
        // Three parts at least; 100 s each for the 5 min of the budget, and never much
        // longer than that.
        assert_eq!(
            [2, 10, 299, 300, 399, 450, 600, 3600].map(idle_parts),
            [3, 3, 3, 3, 3, 4, 6, 36]
        );
    }

    #[test]
    fn a_notification_has_a_limit_for_the_whole_way_and_one_from_the_commit() {
        let (whole, commit) = (
            "Receive → notification",
            "Receive → notification: `recv.commit` → `notify.shown`",
        );
        // Both at their limits: 150 ms from the envelope (it was 300), 50 ms from the
        // core's commit.
        let mut m = measured();
        m.notify_recv = vec![150.0];
        m.notify_commit = vec![50.0];
        assert_eq!(row_of(&m, whole).status(), Status::Close);
        assert_eq!(row_of(&m, commit).status(), Status::Close);
        assert!(missed_targets(&m).is_empty());
        let table = render_table(&table_of(&m));
        assert!(
            table.contains("| ≤ 150 ms ") && table.contains("| ≤ 50 ms "),
            "{table}"
        );
        // Each is judged alone.
        m.notify_recv = vec![150.5];
        assert_eq!(missed_targets(&m), [whole]);
        m.notify_recv = vec![62.0];
        m.notify_commit = vec![50.5];
        assert_eq!(missed_targets(&m), [commit]);
        // A good median with one slow run is close, as everywhere.
        m.notify_recv = vec![37.0, 38.0, 192.0];
        m.notify_commit = vec![9.0, 10.0, 11.0];
        assert_eq!(row_of(&m, whole).status(), Status::Close);
        assert_eq!(row_of(&m, commit).status(), Status::Ok);
    }

    #[test]
    fn catch_up_and_binary_size_have_budgets_and_the_fake_history_only_informs() {
        let mock = "Catch-up ingest, Signal mock";
        let mut m = measured();
        m.mock_release = true;
        // 3,000 messages/s for the release build on the Signal mock (it was 500).
        m.mock_ingest = vec![9000.0];
        assert_eq!(row_of(&m, mock).status(), Status::Ok);
        m.mock_ingest = vec![3000.0];
        assert_eq!(row_of(&m, mock).status(), Status::Close);
        assert!(missed_targets(&m).is_empty());
        m.mock_ingest = vec![2999.0];
        assert_eq!(missed_targets(&m), [mock]);
        assert!(
            render_table(&table_of(&m)).contains("| ≥ 3000 messages/s (release build) "),
            "{}",
            render_table(&table_of(&m))
        );
        // The history with media on the fake network has a number, for information: it
        // never fails a run, however slow.
        let mut m = measured();
        let fake = "Catch-up ingest, fake network";
        for rate in [4000.0, 800.0, 799.0, 1.0] {
            m.seed.as_mut().expect("seed").messages_per_s = rate;
            let row = row_of(&m, fake);
            assert_eq!((row.advisory, row.status()), (true, Status::Info));
            assert!(missed_targets(&m).is_empty());
        }
        assert!(
            render_table(&table_of(&m)).contains("| ≥ 800 messages/s, for information "),
            "{}",
            render_table(&table_of(&m))
        );
        // The scroll on the software renderer too.
        assert!(
            render_table(&table_of(&m)).contains("| ≤ 16 ms, for information "),
            "{}",
            render_table(&table_of(&m))
        );
        // The release binary: 60 MiB, to the byte.
        let mut m = measured();
        m.binary_bytes = Some(60 * 1024 * 1024);
        assert_eq!(row_of(&m, "Binary size").status(), Status::Close);
        assert!(missed_targets(&m).is_empty());
        m.binary_bytes = Some(60 * 1024 * 1024 + 1);
        assert_eq!(missed_targets(&m), ["Binary size"]);
        assert!(
            render_table(&table_of(&m)).contains("| ≤ 60 MiB "),
            "{}",
            render_table(&table_of(&m))
        );
    }

    #[test]
    fn the_report_names_what_was_missed_and_shows_the_breakdowns() {
        let o = parse_options(&args(&["--quick"])).expect("options");
        let report = render_report(&measured(), &o, "Measured here.");
        assert!(report.starts_with("# Kourier: quality targets"));
        assert!(report.contains("Measured here."));
        assert!(report.contains("\nNo budget is missed.\n"), "{report}");
        assert!(
            report.contains(
                "\nBudgets: architecture §1.3, confirmed by the user on 2026-10-07. `ok`: \
                 inside the budget."
            ),
            "{report}"
        );
        // Too many wake-ups with the window shown, most of them in one part: two rows.
        let mut busy = measured();
        busy.session.shown = Some(shown_idle(90));
        let report = render_report(&busy, &o, "Measured here.");
        assert!(
            report.contains(
                "Missed: Idle CPU, window shown: wake-ups; Idle CPU, window shown: busiest part."
            ),
            "{report}"
        );
        assert!(!report.contains("No budget is missed"), "{report}");
        assert!(!report.contains("Failed measurements"), "{report}");
        assert!(report.contains(
            "the one start with the scripted driver): 38 MB in use and 4 MB free but still held"
        ));
        assert!(report.contains("(199 conversations, 16 list delegates alive)"));
        assert!(report.contains("own files out of the page cache (ms): 520, 480, 610; spawn → frame line: 560, 515, 650."));
        assert!(report.contains("nothing opened (MB PSS): 150, 152."));
        assert!(report.contains("Heap after the session: 60 MB in use, 20 MB free but held."));
        assert!(report.contains("released and the heap trimmed: 35 MB in use, 3 MB free"));
        assert!(report.contains("Heap two seconds later: 20 MB in use, 18 MB free in the arenas"));
        assert!(
            report
                .contains("`timeline.open` (ms): 140, 9, 40, 60; 95th 140; rows loaded: 12 to 50.")
        );
        assert!(report.contains(
            "Receive → notification (ms): 62, 70; `recv.commit` → accepted (ms): 21, 25; envelope at the core → commit and ack (ms): 11.5, 12.5; core event → accepted (ms): 5, 6."
        ), "{report}");
        assert!(report.contains("Load average when the run ended: 0.42."));
        assert!(report.contains("[quick run]"));
        assert!(report.contains("| [heap] | 48.8 |"), "{report}");
        assert!(
            report.contains("300 s; wake-ups per second in each part: 0.00, 0.90, 0.00; process CPU time 12 ms; 1 threads started, 2 ended."),
            "{report}"
        );
        assert!(
            report.contains("| kourier | 1 | 90 | 0.30 | 12 |"),
            "{report}"
        );
        assert!(
            report.contains("PSS 115.5 MB (anonymous 68.0, file 46.5"),
            "{report}"
        );
        // How the Qt libraries are shared, and which PSS limit follows from it.
        assert!(
            report.contains(
                "Of 34.2 MB of Qt 6 libraries resident, 2.0 MB are mapped by other processes \
                 too and 33.2 MB count for this process: the others carry 3 %. Of 5.1 MB of \
                 Qt6Core resident, 0.1 MB are mapped by other processes too. Less than half: \
                 the library pages count in full or nearly so"
            ) && report.contains(
                "Of 34.2 MB of Qt 6 libraries resident, 32.2 MB are mapped by other processes \
                 too and 6.8 MB count for this process: the others carry 80 %. Of 4.9 MB of \
                 Qt6Core resident, 3.9 MB are mapped by other processes too. Half or more: a \
                 session that shares its Qt, held to the lower PSS limit.\n"
            ),
            "{report}"
        );
        // The warnings of every run, not only the session's.
        assert!(
            report.contains("- `qrc:/x/Start.qml:1: ReferenceError`")
                && report.contains("- `qrc:/x/Session.qml:3: TypeError`"),
            "{report}"
        );
        // A non-dumpable release build that can be read is a miss.
        let mut m = measured();
        m.release_non_dumpable = Some(false);
        let report = render_report(&m, &o, "");
        assert!(report.contains("Missed: Release build is non-dumpable (§9.4)."));
        let rows = rows(&m, &o);
        assert_eq!(rows.last().map(Row::status), Some(Status::Miss));
        // A failed measurement is named apart from the missed budgets.
        m.mock_failure = Some("the test did not pass".into());
        let report = render_report(&m, &o, "");
        assert!(
            report.contains(
                "Failed measurements (no verdict for this run): Catch-up ingest, Signal mock."
            ),
            "{report}"
        );
        // An idle time that was not measured says so.
        m.session.hidden = None;
        let report = render_report(&m, &o, "");
        assert!(
            report.contains("### Memory, hidden to tray (window released)\n\nNot measured."),
            "{report}"
        );
    }

    #[test]
    fn seed_marks() {
        let mark = parse_trace_line(
            "kourier-trace: perf.seed at_ms=14000 messages=50000 events=141234 ingest_ms=12500 messages_per_s=4000 events_per_s=11298.720 files=4100 downloads_ms=9000",
        )
        .expect("mark");
        let seed = seed_from_mark(&mark).expect("seed");
        assert_eq!(seed.messages, 50_000.0);
        assert_eq!(seed.messages_per_s, 4000.0);
        assert_eq!(seed.downloads_s, 9.0);
        let stamp: serde_json::Value = serde_json::json!({
            "messages": 50000.0, "events": 141234.0, "messages_per_s": 4000.0,
            "files": 4100.0, "downloads_s": 9.0,
        });
        assert_eq!(stamp_seed(&stamp), Some(seed));
        assert_eq!(stamp_seed(&serde_json::json!({})), None);
    }

    #[test]
    fn a_start_only_run_parses_and_takes_runs() {
        let o = parse_options(&[]).expect("defaults");
        assert!(!o.start);
        let o = parse_options(&args(&["--start"])).expect("start");
        assert!(o.start && o.build && !o.quick && !o.mock && !o.keep_profile);
        assert_eq!(o.runs, START_RUNS);
        // --runs wins, in any order; --quick makes it short as it does every run.
        let o = parse_options(&args(&["--runs", "7", "--start"])).expect("runs");
        assert_eq!(o.runs, 7);
        let o = parse_options(&args(&["--start", "--runs", "2", "--report-only"])).expect("runs");
        assert_eq!((o.runs, o.report_only), (2, true));
        let o = parse_options(&args(&["--start", "--quick"])).expect("both");
        assert!(o.start && o.quick);
        assert_eq!(o.runs, 3);
        let o = parse_options(&args(&["--start", "--quick", "--runs", "4"])).expect("both");
        assert_eq!(o.runs, 4);
        // A start-only run measures no idle time.
        assert!(parse_options(&args(&["--start", "--idle", "10"])).is_err());
        assert!(USAGE.contains("--start"));
    }

    /// A directory below `target/` for a test, removed again.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = target_dir()
                .join("xtask-test")
                .join(format!("perf-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create the scratch directory");
            Self(dir)
        }

        fn write(&self, rel: &str, content: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("create parents");
            std::fs::write(path, content).expect("write the file");
        }

        fn remove(&self, rel: &str) {
            std::fs::remove_file(self.0.join(rel)).expect("remove the file");
        }

        fn rename(&self, from: &str, to: &str) {
            std::fs::rename(self.0.join(from), self.0.join(to)).expect("rename the file");
        }

        fn digest(&self) -> u64 {
            profile_digest(&self.0).expect("digest")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A tree with a file in every place the profile's digest reads, and in the places it
    /// must not.
    fn profile_tree(name: &str) -> Scratch {
        let tree = Scratch::new(name);
        for krate in [
            "kourier-core",
            "kourier-db",
            "kourier-blob",
            "kourier-media",
            "kourier-fake",
            "kourier-model",
            "kourier-backend-api",
        ] {
            tree.write(&format!("crates/{krate}/src/lib.rs"), "// code");
            tree.write(&format!("crates/{krate}/src/deep/more.rs"), "// code");
            tree.write(&format!("crates/{krate}/Cargo.toml"), "[package]");
            tree.write(&format!("crates/{krate}/tests/it.rs"), "// test");
        }
        tree.write("crates/kourier-core/target/junk", "junk");
        tree.write("crates/kourier-app/src/perf.rs", "// seeder");
        tree.write("crates/kourier-app/src/app.rs", "// what the core is told");
        tree.write("crates/kourier-app/Cargo.toml", "[features]");
        tree.write("crates/kourier-qt/qml/Main.qml", "Item {}");
        tree.write("crates/kourier-qt/src/lib.rs", "// ui");
        tree.write("crates/kourier-viewmodel/src/lib.rs", "// viewmodel");
        tree.write("crates/kourier-desktop/src/lib.rs", "// desktop");
        tree.write("crates/kourier-signal/src/lib.rs", "// signal");
        tree.write("crates/kourier/src/main.rs", "// binary");
        tree.write("xtask/src/perf.rs", "// tool");
        tree.write("docs/architecture.md", "# docs");
        tree.write("Cargo.lock", "# lock");
        tree.write("Cargo.toml", "[workspace]");
        tree.write("rust-toolchain.toml", "[toolchain]");
        tree
    }

    #[test]
    fn the_profile_digest_follows_the_code_that_fills_the_profile() {
        let base = profile_tree("digest-base").digest();
        assert_eq!(base, profile_tree("digest-same").digest(), "the same tree");
        let mut seen = vec![base];
        for (rel, content) in [
            ("crates/kourier-core/src/lib.rs", "// code 2"),
            ("crates/kourier-core/src/deep/more.rs", "// code 2"),
            ("crates/kourier-core/src/new.rs", "// a new file"),
            ("crates/kourier-db/src/lib.rs", "// code 2"),
            ("crates/kourier-blob/src/lib.rs", "// code 2"),
            ("crates/kourier-media/src/lib.rs", "// code 2"),
            ("crates/kourier-fake/src/lib.rs", "// code 2"),
            ("crates/kourier-model/src/lib.rs", "// code 2"),
            ("crates/kourier-backend-api/src/lib.rs", "// code 2"),
            ("crates/kourier-core/Cargo.toml", "[package]\nname = \"x\""),
            ("crates/kourier-app/src/perf.rs", "// seeder 2"),
            (
                "crates/kourier-app/src/app.rs",
                "// the core is told something else",
            ),
            ("crates/kourier-app/Cargo.toml", "[features]\nperf = []"),
            ("Cargo.lock", "# lock 2"),
            ("Cargo.toml", "[workspace]\n[profile.release]"),
            ("rust-toolchain.toml", "[toolchain]\nchannel = \"1\""),
        ] {
            let tree = profile_tree("digest-each");
            tree.write(rel, content);
            let digest = tree.digest();
            assert_ne!(digest, base, "{rel}");
            seen.push(digest);
        }
        // A file that is gone, and one that only has another name (the paths are hashed,
        // not only what the files hold).
        let tree = profile_tree("digest-removed");
        tree.remove("crates/kourier-core/src/deep/more.rs");
        assert_ne!(tree.digest(), base, "a removed file");
        seen.push(tree.digest());
        let tree = profile_tree("digest-renamed");
        tree.rename(
            "crates/kourier-db/src/deep/more.rs",
            "crates/kourier-db/src/deep/other.rs",
        );
        assert_ne!(tree.digest(), base, "a renamed file");
        seen.push(tree.digest());
        // Each change is its own digest, not only "another than before".
        let changes = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), changes, "every change gives its own digest");
    }

    #[test]
    fn the_profile_digest_ignores_what_cannot_change_stored_data() {
        let tree = profile_tree("digest-ignores");
        let base = tree.digest();
        for (rel, content) in [
            ("crates/kourier-qt/qml/Main.qml", "Item { x: 1 }"),
            ("crates/kourier-qt/src/lib.rs", "// ui 2"),
            ("crates/kourier-qt/src/new.rs", "// a new file"),
            ("crates/kourier-viewmodel/src/lib.rs", "// viewmodel 2"),
            ("crates/kourier-desktop/src/lib.rs", "// desktop 2"),
            ("crates/kourier-signal/src/lib.rs", "// signal 2"),
            ("crates/kourier/src/main.rs", "// binary 2"),
            ("xtask/src/perf.rs", "// tool 2"),
            ("docs/architecture.md", "# docs 2"),
            // What is not part of the built code of a listed crate.
            ("crates/kourier-core/tests/it.rs", "// test 2"),
            ("crates/kourier-core/tests/new/more.rs", "// a new test"),
            ("crates/kourier-db/tests/it.rs", "// test 2"),
            ("crates/kourier-core/target/junk", "more junk"),
        ] {
            tree.write(rel, content);
            assert_eq!(tree.digest(), base, "{rel}");
        }
    }

    #[test]
    fn the_listed_sources_exist_in_the_workspace() {
        // A path that is gone (a renamed crate) would be skipped by the digest without a word.
        for rel in PROFILE_SOURCES {
            assert!(root().join(rel).exists(), "{rel}");
        }
        // And the real tree is read: files of every listed place, the same twice.
        let mut files = Vec::new();
        for rel in PROFILE_SOURCES {
            let before = files.len();
            profile_files(&root(), Path::new(rel), &mut files).expect("read the tree");
            assert!(files.len() > before, "no file below {rel}");
        }
        for known in [
            "crates/kourier-core/src/lib.rs",
            "crates/kourier-app/src/perf.rs",
            "Cargo.lock",
        ] {
            assert!(files.contains(&PathBuf::from(known)), "{known}");
        }
        assert!(
            files.iter().all(|f| !f.starts_with("crates/kourier-qt")),
            "the UI is not part of it"
        );
        let digest = profile_digest(&root()).expect("the real tree can be read");
        assert_eq!(digest, profile_digest(&root()).expect("again"), "stable");
    }

    #[test]
    fn the_stamp_names_the_sources_not_the_binary() {
        let seed = Seed {
            messages: 50_000.0,
            events: 141_234.0,
            messages_per_s: 4000.0,
            files: 4100.0,
            downloads_s: 9.0,
        };
        let text = stamp_text(Some(0xdead_beef_0123_4567), Some(seed));
        let stamp: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(stamp["source_digest"], "deadbeef01234567");
        assert!(stamp.get("binary_bytes").is_none() && stamp.get("binary_modified").is_none());
        assert_eq!(stamp_seed(&stamp), Some(seed));
        // The profile is reused for the same sources and for no other.
        assert!(stamp_matches(&stamp, 0xdead_beef_0123_4567));
        assert!(!stamp_matches(&stamp, 0xdead_beef_0123_4568));
        assert!(!stamp_matches(&stamp, 0));
        // A stamp of the older kind (it named the binary) means: seed again.
        let old = serde_json::json!({
            "binary_bytes": 55_817_432, "binary_modified": 1_791_470_319,
            "messages": 50000.0, "events": 101_166.0, "messages_per_s": 1322.0,
            "files": 7308.0, "downloads_s": 0.29,
        });
        assert_eq!(
            stamp_seed(&old),
            Some(Seed {
                messages: 50_000.0,
                events: 101_166.0,
                messages_per_s: 1322.0,
                files: 7308.0,
                downloads_s: 0.29,
            })
        );
        for digest in [0, 1, 0xdead_beef_0123_4567, u64::MAX] {
            assert!(!stamp_matches(&old, digest), "{digest:x}");
        }
        // A stamp without its seed numbers cannot be reused either.
        assert!(!stamp_matches(
            &serde_json::json!({"source_digest": "0000000000000007"}),
            7
        ));
        // What is said when the profile is seeded.
        assert_eq!(seed_reason(None, true), "no stamp says what filled it");
        assert_eq!(seed_reason(Some(&stamp), false), "there is no profile");
        assert_eq!(
            seed_reason(Some(&old), true),
            "its stamp does not say what filled it"
        );
        // A seeding by a binary the run did not build names no sources: no digest matches
        // it, so the next run that builds seeds again.
        let unbuilt: serde_json::Value =
            serde_json::from_str(&stamp_text(None, Some(seed))).expect("json");
        assert!(unbuilt["source_digest"].is_null());
        assert_eq!(stamp_seed(&unbuilt), Some(seed));
        for digest in [0, 0xdead_beef_0123_4567, u64::MAX] {
            assert!(!stamp_matches(&unbuilt, digest), "{digest:x}");
        }
        assert_eq!(
            seed_reason(Some(&unbuilt), true),
            "its stamp does not say what filled it"
        );
        assert_eq!(
            seed_reason(Some(&stamp), true),
            "the code that fills it changed"
        );
    }

    #[test]
    fn a_quick_report_marks_the_plain_binary_rows_as_not_measured() {
        let o = parse_options(&args(&["--quick"])).expect("quick");
        // What a quick run measures: no plain binary, so no size and no dump check.
        let mut m = measured();
        m.binary_bytes = None;
        m.release_non_dumpable = None;
        let table = rows(&m, &o);
        for target in [BINARY_SIZE, NON_DUMPABLE] {
            let row = table.iter().find(|r| r.target == target).expect("the row");
            assert_eq!(row.status(), Status::NotMeasured, "{target}");
            assert_eq!(row.value, None, "{target}");
            assert!(
                row.how.starts_with(
                    "not measured: a quick run does not build the plain release binary"
                ),
                "{}",
                row.how
            );
        }
        // Not passed, not missed, not failed: nothing about them reaches the verdict.
        assert!(missed(&table).is_empty() && failed(&table).is_empty());
        assert_eq!(exit_status(&table, false), 0);
        let rendered = render_table(&table);
        assert!(
            rendered.contains("| Binary size ") && rendered.contains("| not measured |"),
            "{rendered}"
        );
        let report = render_report(&m, &o, "");
        assert!(report.contains("\nNo budget is missed.\n"), "{report}");
        assert!(!report.contains("Failed measurements"), "{report}");
        // The rows around them are judged as before, and the Signal mock row says it too.
        assert_eq!(
            table
                .iter()
                .filter(|r| r.status() == Status::NotMeasured)
                .count(),
            3
        );
        let mock = table
            .iter()
            .find(|r| r.target.starts_with("Catch-up ingest, Signal"))
            .expect("row");
        assert_eq!(mock.status(), Status::NotMeasured);
        // A full run of the same data has both rows with their numbers.
        let full = rows(&measured(), &parse_options(&[]).expect("full"));
        let size = full.iter().find(|r| r.target == BINARY_SIZE).expect("row");
        assert_eq!(size.status(), Status::Ok);
        assert!(!size.how.contains("not measured"), "{}", size.how);
    }

    #[test]
    fn a_start_only_report_judges_the_starts_and_leaves_the_rest_not_measured() {
        let o = parse_options(&args(&["--start"])).expect("start");
        let m = Measured {
            startup: measured().startup,
            seed: measured().seed,
            load_end: Some("0.5".into()),
            ..Measured::default()
        };
        let table = rows(&m, &o);
        let status = |target: &str| {
            table
                .iter()
                .find(|r| r.target == target)
                .map(Row::status)
                .expect("the row")
        };
        assert_eq!(status(COLD_START), Status::Ok);
        assert_eq!(status(COLD_FILES_START), Status::Close);
        assert_eq!(status(LIST_AFTER_FRAME), Status::Ok);
        // What this run did not measure says so and is neither a miss nor a failure.
        for row in &table {
            if START_TARGETS.contains(&row.target.as_str()) {
                continue;
            }
            assert_eq!(row.status(), Status::NotMeasured, "{}", row.target);
            assert!(
                row.how.starts_with("not measured: "),
                "{}: {}",
                row.target,
                row.how
            );
        }
        assert!(
            table
                .iter()
                .find(|r| r.target == BINARY_SIZE)
                .is_some_and(|r| r.how.contains("`--start` run does not build the plain"))
        );
        assert!(missed(&table).is_empty() && failed(&table).is_empty());
        assert_eq!(exit_status(&table, false), 0);
        let report = render_report(&m, &o, "");
        assert!(report.contains("\nNo budget is missed.\n"), "{report}");
        // Nothing prints an empty list for what was not measured.
        for empty in [
            "`timeline.open` (ms)",
            "Receive → notification (ms)",
            "nothing opened",
        ] {
            assert!(!report.contains(empty), "{empty}: {report}");
        }
        // A slow start is a miss in a start-only run too.
        let mut slow = m.clone();
        slow.startup.first_frame = vec![450.0, 460.0, 440.0];
        assert_eq!(missed(&rows(&slow, &o)).len(), 1);
        assert_eq!(exit_status(&rows(&slow, &o), false), EXIT_MISSED);
    }

    #[test]
    fn a_run_says_where_its_wall_time_went() {
        let s = Duration::from_secs;
        let parts = Parts {
            builds: s(104),
            profile: s(0),
            reused: Some(true),
        };
        assert_eq!(
            parts.line(s(182)),
            "perf: 3 min 02 s in all: builds 104 s, profile 0 s (reused), measurements 78 s"
        );
        let parts = Parts {
            builds: Duration::from_millis(103_600),
            profile: s(45),
            reused: Some(false),
        };
        assert_eq!(
            parts.line(s(329)),
            "perf: 5 min 29 s in all: builds 104 s, profile 45 s (seeded), measurements 180 s"
        );
        // Below a minute, no build, a run that ended before its profile.
        assert_eq!(
            Parts::default().line(Duration::from_millis(46_700)),
            "perf: 47 s in all: builds 0 s, profile 0 s, measurements 47 s"
        );
        assert_eq!(long_time(s(60)), "1 min 00 s");
        assert_eq!(long_time(s(59)), "59 s");
        assert_eq!(long_time(s(3601)), "60 min 01 s");
        // The parts can never be longer than the whole.
        let over = Parts {
            builds: s(10),
            profile: s(10),
            reused: None,
        };
        assert_eq!(
            over.line(s(5)),
            "perf: 5 s in all: builds 10 s, profile 10 s, measurements 0 s"
        );
    }
}

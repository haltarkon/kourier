//! Pruning of `target/debug` at the end of `cargo xtask ci` (T3, 2026-10-14).
//!
//! Why: a day of work leaves several generations of dependencies and test binaries behind
//! (cargo never removes the files of a unit whose hash changed: another feature set, a `-p`
//! subset, a profile change, a changed dependency). On 2026-10-14 the target directory reached
//! 159 GB and the disk 6.7 GB free. `scripts/trim-target.sh` was the manual cure; this is the
//! same logic in the tool that already builds everything, run after a green `ci`.
//!
//! How: while `ci` runs, [`record`] notes every cargo command that builds ([`query_for`] turns
//! it into the matching `--no-run`/`--message-format=json` query). At the end [`after_ci`]
//! replays them: after the run's own steps every unit is fresh, so each query is a no-op that
//! prints one `compiler-artifact` line per unit with the files it owns ([`Graph`]). What the
//! graph names stays; every other file of `deps/`, `examples/` and `.fingerprint/`, every
//! directory of `build/` and the incremental caches of crates that no unit uses are deleted
//! ([`plan`], [`apply`]).
//!
//! The two tiers differ in what they build (the quick tier builds the unit tests of every
//! package and the integration tests of the packages a change reaches, never the Qt tests
//! unless a Qt crate is reached). The full tier's graph is therefore written to
//! `target/debug/.kourier-prune-full.json`, and the quick tier keeps it as well as its own: a
//! quick run never deletes what the last full run built, and without that record it deletes
//! nothing.
//!
//! Never deleted: a file some process runs (`/proc/*/exe`, by device and inode); anything
//! changed in the last [`YOUNG`] minutes (another session's build); anything while another
//! cargo holds the build directory's lock; anything at all when the graph names fewer than
//! [`MIN_UNITS`] hashes (the query did not cover the workspace). `--keep-target` skips the
//! whole step. A failing prune never fails the run.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

/// Fewer hashes than this and the query cannot have covered the workspace (the guard of
/// `scripts/trim-target.sh`): nothing is deleted.
pub const MIN_UNITS: usize = 300;

/// Anything modified more recently than this is left alone: it may be another session's build.
pub const YOUNG: Duration = Duration::from_secs(15 * 60);

/// The full tier's graph, in `target/debug/`.
const KEEP_FILE: &str = ".kourier-prune-full.json";

static RECORDED: Mutex<Vec<Vec<String>>> = Mutex::new(Vec::new());
static FULL_TIER: AtomicBool = AtomicBool::new(false);

/// The full tier is running (the quick tier turns into it when a change can alter any build).
pub fn mark_full() {
    FULL_TIER.store(true, Ordering::Relaxed);
}

/// Called for every command `crate::run` starts: the cargo commands that build are kept as
/// queries for [`after_ci`].
pub fn record(cmd: &Command) {
    let program = Path::new(cmd.get_program());
    let is_cargo = program.file_name() == Some(OsStr::new("cargo"))
        || std::env::var_os("CARGO").is_some_and(|c| program == Path::new(&c));
    if !is_cargo {
        return;
    }
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    if let Some(query) = query_for(&args) {
        let mut recorded = RECORDED.lock().unwrap_or_else(|e| e.into_inner());
        if !recorded.contains(&query) {
            recorded.push(query);
        }
    }
}

/// The cargo arguments that list the units a cargo command built, without building: the same
/// package selection and flags, `--no-run` for tests, JSON messages. `None` for a command that
/// builds nothing worth naming (`fmt`, `metadata`, `test --doc`).
pub fn query_for(args: &[String]) -> Option<Vec<String>> {
    let (sub, rest) = args.split_first()?;
    // Everything after `--` goes to the tool (clippy's lints, a test filter), not to cargo. A
    // test filter changes no build; clippy's lints do: clippy-driver tracks `CLIPPY_ARGS`, so a
    // replay without `-D warnings` would check the workspace crates again and leave them
    // fingerprinted for the lints of the replay, which the next `ci` then checks once more.
    let split = rest
        .iter()
        .position(|a| a.as_str() == "--")
        .unwrap_or(rest.len());
    let (flags, tool_args) = rest.split_at(split);
    let mut query = match sub.as_str() {
        "build" | "clippy" => vec![sub.clone()],
        "test" if !flags.iter().any(|a| a.as_str() == "--doc") => {
            vec!["test".to_owned(), "--no-run".to_owned()]
        }
        "nextest" if flags.first().map(|a| a.as_str()) == Some("run") => {
            let mut q = vec!["test".to_owned(), "--no-run".to_owned()];
            // A filter set picks the tests that RUN out of what was built (the quick tier's
            // `-E 'kind(test) & …'`): nextest's own flag, no cargo flag, no change to the build.
            let mut rest = flags[1..].iter();
            while let Some(arg) = rest.next() {
                match arg.as_str() {
                    "-E" | "--filterset" => {
                        rest.next();
                    }
                    a if a.starts_with("--filterset=") || a.starts_with("-E") => {}
                    _ => q.push(arg.clone()),
                }
            }
            q.push("--message-format=json".to_owned());
            return Some(q);
        }
        _ => return None,
    };
    query.extend(flags.iter().cloned());
    query.push("--message-format=json".to_owned());
    if sub == "clippy" {
        query.extend(tool_args.iter().cloned());
    }
    Some(query)
}

/// The first 16-hex-digit group a cargo output name carries: `libserde-0123456789abcdef.rlib`,
/// `it-0123456789abcdef`, `serde-0123456789abcdef.d`, `serde-0123456789abcdef` (a directory).
pub fn unit_hash(name: &str) -> Option<&str> {
    let bytes = name.as_bytes();
    let mut found = None;
    for (i, &b) in bytes.iter().enumerate() {
        if b != b'-' {
            continue;
        }
        let Some(hash) = bytes.get(i + 1..i + 17) else {
            continue;
        };
        let hex = hash
            .iter()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c));
        if hex && matches!(bytes.get(i + 17), None | Some(b'.')) {
            found = Some(&name[i + 1..i + 17]);
        }
    }
    found
}

/// `<crate>` of an incremental directory `<crate>-<base 36 hash>`.
pub fn incremental_crate(name: &str) -> Option<&str> {
    let (krate, hash) = name.rsplit_once('-')?;
    let plausible = hash.len() >= 8
        && hash
            .bytes()
            .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase());
    (plausible && !krate.is_empty()).then_some(krate)
}

/// The first path component below `<...>/debug/<dir>/` of an absolute path, if it lies there.
fn below(path: &Path, dir: &str) -> Option<String> {
    let parts: Vec<&OsStr> = path.components().map(|c| c.as_os_str()).collect();
    let at = parts
        .windows(2)
        .position(|w| w[0] == OsStr::new("debug") && w[1] == OsStr::new(dir))?;
    parts.get(at + 2).map(|p| p.to_string_lossy().into_owned())
}

/// What the cargo commands of a run name: the units that exist for its build graph.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Graph {
    /// The hashes in the names of the files and directories the units own.
    pub hashes: BTreeSet<String>,
    /// The directories of `build/` a build script's program or output lives in.
    pub build_dirs: BTreeSet<String>,
    /// Per crate name (`-` written as `_`, as rustc names its incremental directory): the
    /// hashes of its units from path packages, the only ones that have incremental caches.
    pub workspace_units: BTreeMap<String, BTreeSet<String>>,
    /// Units a query had to build (not fresh): the run's own steps did not cover them.
    pub rebuilt: usize,
    /// The targets of those units (`name` or `name (kind)` for a test), in the order they came.
    pub rebuilt_units: Vec<String>,
}

/// A unit as a person reads it: its target's name, and the kind when it is not the library.
fn unit_label(artifact: &Value) -> String {
    let name = artifact["target"]["name"].as_str().unwrap_or("?");
    match artifact["target"]["kind"][0].as_str() {
        Some("lib") | None => name.to_owned(),
        Some(kind) => format!("{name} ({kind})"),
    }
}

/// What a query that had to build says on stderr, so that a prune that costs time can be
/// traced to the command whose steps did not cover it. `None` when it built nothing.
fn rebuilt_note(query: &[String], units: &[String]) -> Option<String> {
    const SHOWN: usize = 8;
    if units.is_empty() {
        return None;
    }
    let more = units.len().saturating_sub(SHOWN);
    Some(format!(
        "prune: `cargo {}` had to build {} units: {}{}",
        query.join(" "),
        units.len(),
        units[..units.len().min(SHOWN)].join(", "),
        if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        }
    ))
}

impl Graph {
    /// One line of a cargo JSON stream (lines of other kinds and text are ignored).
    pub fn add_line(&mut self, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return;
        };
        match message["reason"].as_str() {
            Some("compiler-artifact") => {
                let mut files: Vec<&str> = message["filenames"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                files.extend(message["executable"].as_str());
                let mut unit = BTreeSet::new();
                for file in files {
                    let path = Path::new(file);
                    let hash = path.file_name().and_then(OsStr::to_str).and_then(unit_hash);
                    if let Some(hash) = hash {
                        self.hashes.insert(hash.to_owned());
                        unit.insert(hash.to_owned());
                    }
                    self.name_build_dir(path);
                }
                if message["fresh"].as_bool() == Some(false) {
                    self.rebuilt += 1;
                    self.rebuilt_units.push(unit_label(&message));
                }
                let in_workspace = message["package_id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("path+"));
                if let (true, Some(name)) = (in_workspace, message["target"]["name"].as_str()) {
                    self.workspace_units
                        .entry(name.replace('-', "_"))
                        .or_default()
                        .extend(unit);
                }
            }
            Some("build-script-executed") => {
                if let Some(out_dir) = message["out_dir"].as_str() {
                    self.name_build_dir(Path::new(out_dir));
                }
            }
            _ => {}
        }
    }

    /// A file or directory below `build/` names the directory that holds it.
    fn name_build_dir(&mut self, path: &Path) {
        if let Some(dir) = below(path, "build") {
            if let Some(hash) = unit_hash(&dir) {
                self.hashes.insert(hash.to_owned());
            }
            self.build_dirs.insert(dir);
        }
    }

    /// Everything `other` names stays as well.
    pub fn merge(&mut self, other: &Graph) {
        self.hashes.extend(other.hashes.iter().cloned());
        self.build_dirs.extend(other.build_dirs.iter().cloned());
        for (name, units) in &other.workspace_units {
            self.workspace_units
                .entry(name.clone())
                .or_default()
                .extend(units.iter().cloned());
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "hashes": self.hashes,
            "build_dirs": self.build_dirs,
            "workspace_units": self.workspace_units,
        })
    }

    pub fn from_json(text: &str) -> Result<Graph> {
        let v: Value = serde_json::from_str(text).context("the record is not JSON")?;
        let set = |key: &str| -> Result<BTreeSet<String>> {
            v[key]
                .as_array()
                .with_context(|| format!("the record has no `{key}`"))
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
        };
        let mut units = BTreeMap::new();
        for (name, hashes) in v["workspace_units"]
            .as_object()
            .context("the record has no `workspace_units`")?
        {
            let hashes = hashes
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            units.insert(name.clone(), hashes);
        }
        Ok(Graph {
            hashes: set("hashes")?,
            build_dirs: set("build_dirs")?,
            workspace_units: units,
            rebuilt: 0,
            rebuilt_units: Vec::new(),
        })
    }
}

/// The part of `target/debug` an entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Part {
    Deps,
    Build,
    Examples,
    Fingerprint,
    Incremental,
}

impl Part {
    fn label(self) -> &'static str {
        match self {
            Part::Deps => "deps",
            Part::Build => "build",
            Part::Examples => "examples",
            Part::Fingerprint => ".fingerprint",
            Part::Incremental => "incremental",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removal {
    pub path: PathBuf,
    pub bytes: u64,
    pub part: Part,
}

/// What a prune would delete and what it left because it must.
#[derive(Debug, Default)]
pub struct Plan {
    pub remove: Vec<Removal>,
    /// Entries a process runs: kept.
    pub running: Vec<PathBuf>,
    /// Entries changed within [`YOUNG`]: kept.
    pub young: usize,
}

/// The device and inode of every executable some process runs (`/proc/*/exe` follows to the
/// inode even when the file was deleted, so a replaced binary is still protected).
pub fn running_files() -> HashSet<(u64, u64)> {
    let mut files = HashSet::new();
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return files;
    };
    for entry in procs.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(meta) = std::fs::metadata(entry.path().join("exe")) {
            files.insert((meta.dev(), meta.ino()));
        }
    }
    files
}

/// Whether `args` (a process's command line) is a test run: `cargo-nextest ...`, `cargo
/// nextest ...` or `cargo test ...` (also `cargo +toolchain test`, `cargo t`).
fn is_test_run(args: &[String]) -> bool {
    let program = args
        .first()
        .and_then(|a| Path::new(a).file_name())
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    match program {
        "cargo-nextest" => true,
        "cargo" => args
            .iter()
            .skip(1)
            .find(|a| !a.starts_with('-') && !a.starts_with('+'))
            .is_some_and(|sub| matches!(sub.as_str(), "test" | "t" | "nextest")),
        _ => false,
    }
}

/// A `cargo nextest` / `cargo test` process that works against this checkout (its working
/// directory is below `root`, or its command line names `root`): `proc_dir` is `/proc`. The
/// tests of a run are started one by one for minutes after its build ended, so no executable of
/// it may be running at the moment a prune looks (`running_files` sees only the instant); a
/// binary of another selection that is queued would be deleted under it (T3 review F5). The
/// process is described by its pid and the start of its command line; `me` is not counted.
pub fn test_run_here(proc_dir: &Path, root: &Path, me: u32) -> Option<String> {
    let root_text = root.to_string_lossy();
    let mut found: Vec<(u32, String)> = Vec::new();
    for entry in std::fs::read_dir(proc_dir).ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
            .filter(|pid| *pid != me)
        else {
            continue;
        };
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        if !is_test_run(&args) {
            continue;
        }
        let in_cwd =
            std::fs::read_link(entry.path().join("cwd")).is_ok_and(|cwd| cwd.starts_with(root));
        let in_cmdline = args.iter().any(|a| a.contains(root_text.as_ref()));
        if in_cwd || in_cmdline {
            let line: String = args.join(" ").chars().take(100).collect();
            found.push((pid, line));
        }
    }
    found.sort();
    found
        .first()
        .map(|(pid, line)| format!("pid {pid}: {line}"))
}

/// Bytes of a file, or of everything below a directory (links are not followed).
fn bytes_of(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path)
        .map(|rd| rd.flatten().map(|e| bytes_of(&e.path())).sum())
        .unwrap_or(0)
}

/// Whether a file at or below `path` is one of `running`.
fn holds_running(path: &Path, running: &HashSet<(u64, u64)>) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if meta.is_dir() {
        return std::fs::read_dir(path)
            .map(|rd| rd.flatten().any(|e| holds_running(&e.path(), running)))
            .unwrap_or(false);
    }
    running.contains(&(meta.dev(), meta.ino()))
}

/// The newest modification time at or below `path`.
fn newest(path: &Path) -> SystemTime {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return SystemTime::UNIX_EPOCH;
    };
    let mut newest = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    if meta.is_dir()
        && let Ok(rd) = std::fs::read_dir(path)
    {
        for e in rd.flatten() {
            newest = newest.max(self::newest(&e.path()));
        }
    }
    newest
}

/// What to delete from `debug` (`target/debug`) so that only what `graph` names, and the
/// caches of crates with units, stays. `min_units` is the guard ([`MIN_UNITS`]).
pub fn plan(
    debug: &Path,
    graph: &Graph,
    running: &HashSet<(u64, u64)>,
    now: SystemTime,
    min_units: usize,
) -> Result<Plan> {
    if graph.hashes.len() < min_units {
        bail!(
            "the build graph names {} units, fewer than {min_units}: it does not cover the workspace",
            graph.hashes.len()
        );
    }
    let mut plan = Plan::default();
    let consider = |plan: &mut Plan, path: PathBuf, part: Part| {
        if holds_running(&path, running) {
            plan.running.push(path);
        } else if now
            .duration_since(newest(&path))
            .is_ok_and(|age| age < YOUNG)
        {
            plan.young += 1;
        } else {
            let bytes = bytes_of(&path);
            plan.remove.push(Removal { path, bytes, part });
        }
    };

    // Files of a unit: `deps/` and `examples/` hold them flat, `.fingerprint/` a directory per unit.
    for (dir, part) in [
        ("deps", Part::Deps),
        ("examples", Part::Examples),
        (".fingerprint", Part::Fingerprint),
    ] {
        let Ok(entries) = std::fs::read_dir(debug.join(dir)) else {
            continue;
        };
        let mut names: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        names.sort();
        for path in names {
            let hash = path.file_name().and_then(OsStr::to_str).and_then(unit_hash);
            if hash.is_some_and(|h| !graph.hashes.contains(h)) {
                consider(&mut plan, path, part);
            }
        }
    }

    // `build/`: a directory per build script (its program) and per run (its output); both end
    // in a hash, both are named when in use.
    if let Ok(entries) = std::fs::read_dir(debug.join("build")) {
        let mut names: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        names.sort();
        for path in names {
            let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
            if path.is_dir() && unit_hash(name).is_some() && !graph.build_dirs.contains(name) {
                consider(&mut plan, path, Part::Build);
            }
        }
    }

    // `incremental/`: the directory name carries rustc's own hash (of its StableCrateId), not
    // the unit's, and nothing in the directory or in cargo's JSON ties it to a package. Per
    // crate NAME keep as many of the newest directories as the graph has units of that name.
    // Not per (package, target): that is the same number, since every unit has its own hash
    // and the six test targets called `it` are six units of the name `it`. What the name
    // cannot tell: a newer stale directory (a hand-run variant, a unit the graph dropped)
    // pushes out an older current one of the same name. The worst case is one non-incremental
    // compile of that crate, after which the next prune drops the stale one: a cache lost,
    // never a wrong build (accepted and documented, T3 fix D6).
    if let Ok(entries) = std::fs::read_dir(debug.join("incremental")) {
        let mut by_crate: BTreeMap<String, Vec<(SystemTime, PathBuf)>> = BTreeMap::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(krate) = incremental_crate(&name).filter(|_| path.is_dir()) {
                by_crate
                    .entry(krate.to_owned())
                    .or_default()
                    .push((newest(&path), path));
            }
        }
        for (krate, mut dirs) in by_crate {
            let keep = graph.workspace_units.get(&krate).map_or(0, BTreeSet::len);
            dirs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            for (_, path) in dirs.into_iter().skip(keep) {
                consider(&mut plan, path, Part::Incremental);
            }
        }
    }
    Ok(plan)
}

/// Deletes what `plan` lists; the bytes freed and the entries that could not go.
pub fn apply(plan: &Plan) -> (u64, Vec<String>) {
    let mut freed = 0;
    let mut failed = Vec::new();
    for removal in &plan.remove {
        let result = match std::fs::symlink_metadata(&removal.path) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&removal.path),
            Ok(_) => std::fs::remove_file(&removal.path),
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => freed += removal.bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => failed.push(format!("{}: {e}", removal.path.display())),
        }
    }
    (freed, failed)
}

/// Bytes a build can still use on the file system that holds `path`.
fn free_bytes(path: &Path) -> Option<u64> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: `c_path` is a NUL-terminated path and `stat` a valid place for the answer.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    // The fields are `u64` on x86-64 and narrower on other targets.
    #[allow(clippy::useless_conversion)]
    (rc == 0).then(|| u64::from(stat.f_bavail) * u64::from(stat.f_frsize))
}

/// Under this much free space the line says so: a build can need 20 GB or more.
const LOW_DISK: u64 = 20 << 30;

/// How much disk is left, for the end of the line.
pub fn disk_note(free: Option<u64>) -> String {
    match free {
        Some(f) if f < LOW_DISK => format!(
            "; only {} free on the disk: free more before a build (`cargo xtask prune`, `cargo clean` of an old lane)",
            gb(f)
        ),
        Some(f) => format!("; {} free on the disk", gb(f)),
        None => String::new(),
    }
}

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / (1u64 << 30) as f64)
}

/// The line the run prints: what was freed, by part, and what stayed because it had to.
pub fn summary(plan: &Plan, freed: u64, failed: &[String]) -> String {
    let mut parts: BTreeMap<Part, (usize, u64)> = BTreeMap::new();
    for r in &plan.remove {
        let e = parts.entry(r.part).or_default();
        e.0 += 1;
        e.1 += r.bytes;
    }
    let by_part: Vec<String> = parts
        .iter()
        .map(|(p, (n, b))| format!("{} {} ({n})", p.label(), gb(*b)))
        .collect();
    let mut line = format!(
        "pruned target/debug: freed {}{}",
        gb(freed),
        if by_part.is_empty() {
            String::new()
        } else {
            format!(" = {}", by_part.join(", "))
        }
    );
    if !plan.running.is_empty() {
        line += &format!("; kept {} that a process runs", plan.running.len());
    }
    if plan.young > 0 {
        line += &format!("; kept {} changed in the last 15 minutes", plan.young);
    }
    if !failed.is_empty() {
        line += &format!("; could not delete {}: {}", failed.len(), failed[0]);
    }
    line
}

/// The `n` biggest removals of a plan, one per line (the dry run's look at what would go).
fn largest(plan: &Plan, n: usize) -> String {
    let mut all: Vec<&Removal> = plan.remove.iter().collect();
    all.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    all.iter()
        .take(n)
        .map(|r| {
            format!(
                "  {:>8} {} {}",
                gb(r.bytes),
                r.part.label(),
                r.path.file_name().unwrap_or_default().to_string_lossy()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Lists the units of one query as a [`Graph`]: runs `cargo <query>` (a no-op after the run's
/// own steps) and reads its JSON lines.
fn graph_of(queries: &[Vec<String>]) -> Result<Graph> {
    let mut graph = Graph::default();
    for query in queries {
        let out = crate::cargo()
            .args(query)
            .current_dir(crate::root())
            .stderr(Stdio::null())
            .output()
            .with_context(|| format!("cargo {}", query.join(" ")))?;
        if !out.status.success() {
            bail!("`cargo {}` failed ({})", query.join(" "), out.status);
        }
        let before = graph.rebuilt_units.len();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            graph.add_line(line);
        }
        if let Some(note) = rebuilt_note(query, &graph.rebuilt_units[before..]) {
            eprintln!("{note}");
        }
    }
    Ok(graph)
}

/// The end of a green `ci`: prune `target/debug` unless `keep_target`. Never fails the run.
pub fn after_ci(keep_target: bool) {
    if keep_target {
        eprintln!("\n==> prune target/debug: skipped (--keep-target)");
        return;
    }
    eprintln!("\n==> prune target/debug");
    let recorded = RECORDED.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let full = FULL_TIER.load(Ordering::Relaxed);
    match prune(end_queries(recorded, full), full, false) {
        Ok(line) => eprintln!("{line}"),
        Err(e) => eprintln!("prune: nothing deleted: {e:#}"),
    }
}

/// The queries the end of a run replays: what it ran, the tool itself (`cargo xtask` builds it
/// by this command, whatever the run was) and, in the full tier, the five of
/// [`canonical_queries`] whether or not the step that runs them happened: a full run that skips
/// the screenshots never builds the application (`build -p kourier`), and its variant would be
/// deleted and built again by the next `shots`, `smoke` or `demo` (T3 review F2).
pub fn end_queries(recorded: Vec<Vec<String>>, full: bool) -> Vec<Vec<String>> {
    let mut queries = recorded;
    let mut wanted = vec![query(&["build", "-p", "xtask"])];
    if full {
        wanted.extend(canonical_queries());
    }
    for q in wanted {
        if !queries.contains(&q) {
            queries.push(q);
        }
    }
    queries
}

fn query(args: &[&str]) -> Vec<String> {
    let mut q: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    q.push("--message-format=json".to_owned());
    q
}

/// The five commands of `scripts/trim-target.sh`, as queries: the whole build graph of the
/// full tier (and of the manual cure), whatever a `ci` run built. The clippy one is the lint
/// step's own command ([`crate::CLIPPY_ARGS`], lint arguments included).
pub fn canonical_queries() -> Vec<Vec<String>> {
    let clippy: Vec<String> = crate::CLIPPY_ARGS.iter().map(|a| (*a).to_owned()).collect();
    vec![
        query_for(&clippy).expect("clippy is a query"),
        query(&["test", "--no-run", "--workspace", "--exclude", "kourier-qt"]),
        query(&["test", "--no-run", "-p", "kourier-qt"]),
        query(&["build", "-p", "kourier"]),
        query(&["build", "-p", "xtask"]),
    ]
}

/// `cargo xtask prune [--dry-run]`: the prune of the end of `ci` on its own, for the full
/// graph. It builds what that graph needs and is missing (the same five commands as
/// `scripts/trim-target.sh`), so run it with nothing else building. `--dry-run` lists what
/// would go and deletes nothing.
pub fn command(args: &[String]) -> Result<()> {
    let mut dry_run = false;
    for arg in args {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            _ => bail!("prune: unknown argument {arg:?} (usage: prune [--dry-run])"),
        }
    }
    eprintln!(
        "\n==> prune target/debug{}",
        if dry_run { " (dry run)" } else { "" }
    );
    eprintln!("{}", prune(canonical_queries(), true, dry_run)?);
    Ok(())
}

/// Writes the full tier's record: to a temporary file beside it, then renamed into place, so a
/// kill in the middle leaves the old record (a truncated one made every quick prune say "the
/// record is not JSON" and delete nothing until the next full run; T3 review F7).
fn write_record(path: &Path, graph: &Graph) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, graph.to_json().to_string())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Prunes `target/debug` by the graph the `queries` list (plus, in the quick tier, the last
/// full run's record) and returns the line to print. `full` is the full tier: it writes the
/// record. `dry_run`: plan and report only.
fn prune(queries: Vec<Vec<String>>, full: bool, dry_run: bool) -> Result<String> {
    let debug = crate::target_dir().join("debug");
    if !debug.is_dir() {
        bail!("{} does not exist", debug.display());
    }
    let keep_path = debug.join(KEEP_FILE);
    // The quick tier needs the last full run's record; look before the queries cost seconds.
    let kept = if full {
        None
    } else {
        let text = std::fs::read_to_string(&keep_path).context(
            "no record of a full run yet (`cargo xtask ci` writes it; \
             `scripts/trim-target.sh` is the manual way)",
        )?;
        Some(Graph::from_json(&text)?)
    };
    // A test run beside this one (another session's) starts its test binaries for minutes after
    // its build ended: nothing is deleted while one runs against this checkout.
    let test_run = test_run_here(Path::new("/proc"), &crate::root(), std::process::id());
    if let (Some(run), false) = (&test_run, dry_run) {
        bail!("a test run works against this checkout ({run}); run the prune when it has ended");
    }
    let mut graph = graph_of(&queries)?;
    // The full tier writes its record below, once the delete went well; the quick tier keeps it.
    if let Some(kept) = &kept {
        graph.merge(kept);
    }
    let rebuilt = if graph.rebuilt > 0 {
        format!(
            "; listing the graph had to build {} units the run's steps had not",
            graph.rebuilt
        )
    } else {
        String::new()
    };
    if dry_run {
        let plan = plan(
            &debug,
            &graph,
            &running_files(),
            SystemTime::now(),
            MIN_UNITS,
        )?;
        let line = summary(&plan, plan.remove.iter().map(|r| r.bytes).sum(), &[])
            .replacen("pruned", "would prune", 1)
            .replacen("freed", "free", 1);
        let blocked = test_run.map_or(String::new(), |run| {
            format!("; a test run works against this checkout ({run}): a real prune would delete nothing")
        });
        return Ok(format!(
            "{line}{rebuilt}{blocked}{}\n{}",
            disk_note(free_bytes(&debug)),
            largest(&plan, 12)
        ));
    }
    // cargo's own lock: nothing is deleted while another cargo builds here, and none starts.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(debug.join(".cargo-lock"))
        .context("open the build directory's lock")?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!("another cargo is building in this target directory")
        }
        Err(std::fs::TryLockError::Error(e)) => return Err(e).context("lock the build directory"),
    }
    let plan = plan(
        &debug,
        &graph,
        &running_files(),
        SystemTime::now(),
        MIN_UNITS,
    )?;
    let (freed, failed) = apply(&plan);
    if full {
        write_record(&keep_path, &graph).context("write the record of this full run")?;
    }
    Ok(format!(
        "{}{rebuilt}{}",
        summary(&plan, freed, &failed),
        disk_note(free_bytes(&debug))
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    /// A scratch directory below the target directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = crate::target_dir()
                .join("tmp/xtask-prune")
                .join(format!("{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch directory");
            Self(dir)
        }

        /// A file of `len` bytes, made `age` ago.
        fn file(&self, rel: &str, len: usize, age: Duration) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().expect("a directory")).expect("directory");
            std::fs::write(&path, vec![b'x'; len]).expect("file");
            // The directories made on the way are as old as the file: a directory counts as
            // changed when anything in it did.
            let when = SystemTime::now() - age;
            let mut at = Some(path.as_path());
            while let Some(p) = at.filter(|p| *p != self.0) {
                File::open(p)
                    .expect("open")
                    .set_modified(when)
                    .expect("mtime");
                at = p.parent();
            }
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const OLD: Duration = Duration::from_secs(3 * 3600);
    const H1: &str = "0123456789abcdef";
    const H2: &str = "fedcba9876543210";

    fn graph_with(hashes: &[&str]) -> Graph {
        Graph {
            hashes: hashes.iter().map(|h| (*h).to_owned()).collect(),
            ..Graph::default()
        }
    }

    fn removed(plan: &Plan) -> Vec<String> {
        let mut names: Vec<String> = plan
            .remove
            .iter()
            .map(|r| r.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_unit_hash_is_the_sixteen_hex_digits_after_the_last_dash() {
        assert_eq!(unit_hash("libserde-0123456789abcdef.rlib"), Some(H1));
        assert_eq!(unit_hash("it-fedcba9876543210"), Some(H2));
        assert_eq!(unit_hash("serde-0123456789abcdef.d"), Some(H1));
        assert_eq!(unit_hash("kourier-qt-0123456789abcdef"), Some(H1));
        assert_eq!(unit_hash("a-0123456789abcdef-b-fedcba9876543210"), Some(H2));
        // Not a hash: too short, too long, upper case, a word, a suffix that is not an extension.
        assert_eq!(unit_hash("it-0123456789abcde.d"), None);
        assert_eq!(unit_hash("it-0123456789abcdef0"), None);
        assert_eq!(unit_hash("it-0123456789ABCDEF"), None);
        assert_eq!(unit_hash("build-script-build"), None);
        assert_eq!(unit_hash("libfoo.rlib"), None);
        assert_eq!(unit_hash("é-0123456789abcdef"), Some(H1));
        assert_eq!(unit_hash(""), None);
    }

    #[test]
    fn an_incremental_directory_names_its_crate() {
        assert_eq!(
            incremental_crate("kourier_core-1abcdefghijkl"),
            Some("kourier_core")
        );
        assert_eq!(incremental_crate("it-0a2137u4sxtwu"), Some("it"));
        assert_eq!(incremental_crate("noslash"), None);
        assert_eq!(incremental_crate("it-short"), None);
        assert_eq!(incremental_crate("-1abcdefghijkl"), None);
    }

    #[test]
    fn a_compiler_artifact_names_its_files_and_a_build_script_its_directory() {
        let mut g = Graph::default();
        g.add_line(&format!(
            r#"{{"reason":"compiler-artifact","package_id":"registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0","target":{{"name":"serde"}},"filenames":["/t/debug/deps/libserde-{H1}.rlib","/t/debug/deps/libserde-{H1}.rmeta"],"fresh":true}}"#
        ));
        assert_eq!(g.hashes, BTreeSet::from([H1.to_owned()]));
        assert!(
            g.workspace_units.is_empty(),
            "a registry crate has no incremental cache"
        );
        g.add_line(&format!(
            r#"{{"reason":"compiler-artifact","package_id":"path+file:///w/crates/kourier-core#0.1.0","target":{{"name":"kourier-core"}},"filenames":[],"executable":"/t/debug/deps/kourier_core-{H2}","fresh":false}}"#
        ));
        assert!(g.hashes.contains(H2));
        assert_eq!(
            g.workspace_units["kourier_core"],
            BTreeSet::from([H2.to_owned()])
        );
        assert_eq!(g.rebuilt, 1);
        assert_eq!(g.rebuilt_units, ["kourier-core"]);
        g.add_line(&format!(
            r#"{{"reason":"compiler-artifact","package_id":"path+file:///w/crates/kourier-qt#0.1.0","target":{{"name":"build-script-build"}},"filenames":["/t/debug/build/kourier-qt-{H1}/build_script_build-{H1}"],"fresh":true}}"#
        ));
        assert!(g.build_dirs.contains(&format!("kourier-qt-{H1}")));
        g.add_line(&format!(
            r#"{{"reason":"build-script-executed","package_id":"x","out_dir":"/t/debug/build/zvariant-{H2}/out"}}"#
        ));
        assert!(g.build_dirs.contains(&format!("zvariant-{H2}")));
        // Noise is ignored.
        g.add_line("   Compiling foo v0.1.0");
        g.add_line(r#"{"reason":"build-finished","success":true}"#);
        g.add_line("");
        assert_eq!(g.build_dirs.len(), 2);
    }

    #[test]
    fn a_query_that_had_to_build_names_its_units() {
        let mut g = Graph::default();
        g.add_line(&format!(
            r#"{{"reason":"compiler-artifact","package_id":"path+file:///w/crates/kourier-core#0.1.0","target":{{"name":"it","kind":["test"]}},"filenames":[],"executable":"/t/debug/deps/it-{H1}","fresh":false}}"#
        ));
        g.add_line(&format!(
            r#"{{"reason":"compiler-artifact","package_id":"path+file:///w/crates/kourier-core#0.1.0","target":{{"name":"kourier-core","kind":["lib"]}},"filenames":["/t/debug/deps/libkourier_core-{H2}.rmeta"],"fresh":false}}"#
        ));
        g.add_line(&format!(
            r#"{{"reason":"compiler-artifact","package_id":"registry+x#serde@1.0.0","target":{{"name":"serde","kind":["lib"]}},"filenames":["/t/debug/deps/libserde-{H1}.rlib"],"fresh":true}}"#
        ));
        assert_eq!(g.rebuilt_units, ["it (test)", "kourier-core"]);
        let query = args(&["test", "--no-run", "-p", "kourier-core"]);
        assert_eq!(
            rebuilt_note(&query, &g.rebuilt_units).as_deref(),
            Some(
                "prune: `cargo test --no-run -p kourier-core` had to build 2 units: \
                 it (test), kourier-core"
            )
        );
        assert_eq!(rebuilt_note(&query, &[]), None);
        // Many units: the first few and a count.
        let many: Vec<String> = (0..11).map(|i| format!("u{i}")).collect();
        let note = rebuilt_note(&query, &many).unwrap();
        assert!(note.contains("11 units: u0, u1, u2, u3, u4, u5, u6, u7 and 3 more"));
    }

    #[test]
    fn a_graph_survives_its_record() {
        let mut g = graph_with(&[H1, H2]);
        g.build_dirs.insert(format!("a-{H1}"));
        g.workspace_units
            .insert("it".into(), BTreeSet::from([H1.to_owned()]));
        g.rebuilt = 7;
        let back = Graph::from_json(&g.to_json().to_string()).unwrap();
        assert_eq!(back.hashes, g.hashes);
        assert_eq!(back.build_dirs, g.build_dirs);
        assert_eq!(back.workspace_units, g.workspace_units);
        assert!(Graph::from_json("{").is_err());
        assert!(Graph::from_json("{}").is_err());
        let mut both = graph_with(&[H1]);
        both.merge(&graph_with(&[H2]));
        assert_eq!(both.hashes.len(), 2);
    }

    #[test]
    fn the_commands_that_build_become_queries_that_list() {
        assert_eq!(
            query_for(&args(&[
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings"
            ])),
            Some(args(&[
                "clippy",
                "--workspace",
                "--all-targets",
                "--message-format=json",
                "--",
                "-D",
                "warnings"
            ]))
        );
        // A test filter after `--` changes no build and is not passed on.
        assert_eq!(
            query_for(&args(&["test", "-p", "kourier-core", "--", "--exact", "x"])),
            Some(args(&[
                "test",
                "--no-run",
                "-p",
                "kourier-core",
                "--message-format=json"
            ]))
        );
        assert_eq!(
            query_for(&args(&[
                "nextest",
                "run",
                "--workspace",
                "--exclude",
                "kourier-qt"
            ])),
            Some(args(&[
                "test",
                "--no-run",
                "--workspace",
                "--exclude",
                "kourier-qt",
                "--message-format=json"
            ]))
        );
        assert_eq!(
            query_for(&args(&[
                "nextest",
                "run",
                "-p",
                "kourier-app",
                "--test",
                "*"
            ])),
            Some(args(&[
                "test",
                "--no-run",
                "-p",
                "kourier-app",
                "--test",
                "*",
                "--message-format=json"
            ]))
        );
        assert_eq!(
            query_for(&args(&["test", "-p", "kourier-core", "--test", "crash"])),
            Some(args(&[
                "test",
                "--no-run",
                "-p",
                "kourier-core",
                "--test",
                "crash",
                "--message-format=json"
            ]))
        );
        assert_eq!(
            query_for(&args(&["build", "-p", "kourier"])),
            Some(args(&["build", "-p", "kourier", "--message-format=json"]))
        );
        // Builds nothing a prune must name, or is not a build.
        assert_eq!(query_for(&args(&["test", "--workspace", "--doc"])), None);
        assert_eq!(query_for(&args(&["fmt", "--all", "--check"])), None);
        assert_eq!(query_for(&args(&["metadata"])), None);
        assert_eq!(query_for(&args(&["nextest", "list"])), None);
        assert_eq!(query_for(&[]), None);
    }

    #[test]
    fn a_nextest_filter_set_is_not_part_of_the_replay() {
        // The quick tier's `-E 'kind(test) & (…)'` picks what runs; `cargo test --no-run`
        // would refuse the flag, and the build is the workspace's whatever it says.
        let full = query_for(&args(&[
            "nextest",
            "run",
            "--workspace",
            "--exclude",
            "kourier-qt",
        ]));
        for filter in [
            args(&["-E", "kind(test) & (package(=a) | package(=b))"]),
            args(&["--filterset", "kind(test)"]),
            args(&["--filterset=kind(test)"]),
            args(&["-Ekind(test)"]),
        ] {
            let mut cmd = args(&["nextest", "run", "--workspace"]);
            cmd.extend(filter.clone());
            cmd.extend(args(&["--exclude", "kourier-qt"]));
            assert_eq!(query_for(&cmd), full, "{filter:?}");
        }
    }

    #[test]
    fn the_full_tier_always_prunes_by_the_whole_graph() {
        // A full run that skipped the screenshots ran the lint step and the two test steps.
        let clippy: Vec<String> = crate::CLIPPY_ARGS.iter().map(|a| (*a).to_owned()).collect();
        let nextest = |select: &[&str]| {
            let mut cmd = args(&["nextest", "run"]);
            cmd.extend(args(select));
            query_for(&cmd).expect("a query")
        };
        let recorded = vec![
            query_for(&clippy).expect("a query"),
            nextest(&crate::runner::NON_QT),
            nextest(&crate::runner::QT),
        ];
        let build_kourier = query(&["build", "-p", "kourier"]);
        let full = end_queries(recorded.clone(), true);
        assert!(full.contains(&build_kourier), "{full:?}");
        // The recorded commands ARE the canonical ones (no second clippy with other lints, no
        // second test query): five in all.
        assert_eq!(full.len(), 5, "{full:?}");
        for q in canonical_queries() {
            assert!(full.contains(&q), "{q:?} missing from {full:?}");
        }
        // The quick tier adds the tool and nothing else.
        let quick = end_queries(recorded, false);
        assert!(!quick.contains(&build_kourier));
        assert!(quick.contains(&query(&["build", "-p", "xtask"])));
        assert_eq!(quick.len(), 4, "{quick:?}");
    }

    #[test]
    fn the_replay_of_clippy_passes_the_lints_of_the_lint_step() {
        // clippy-driver tracks its lint arguments: a replay without `-D warnings` checks the
        // workspace crates again and leaves them fingerprinted for other lints.
        let clippy = &canonical_queries()[0];
        assert_eq!(clippy[0], "clippy");
        let tail = clippy
            .iter()
            .position(|a| a == "--")
            .expect("lint arguments");
        assert_eq!(&clippy[tail..], ["--", "-D", "warnings"]);
        assert!(clippy[..tail].contains(&"--message-format=json".to_owned()));
    }

    /// A fake `/proc`: one directory per pid with a `cmdline` and a `cwd` link.
    fn fake_proc(s: &Scratch, pid: u32, cmdline: &[&str], cwd: &Path) {
        let dir = s.0.join(format!("proc/{pid}"));
        std::fs::create_dir_all(&dir).expect("pid dir");
        let mut raw = Vec::new();
        for a in cmdline {
            raw.extend_from_slice(a.as_bytes());
            raw.push(0);
        }
        std::fs::write(dir.join("cmdline"), raw).expect("cmdline");
        std::os::unix::fs::symlink(cwd, dir.join("cwd")).expect("cwd");
    }

    #[test]
    fn a_prune_waits_for_a_test_run_against_this_checkout() {
        let s = Scratch::new("procs");
        let root = s.0.join("checkout");
        let elsewhere = s.0.join("elsewhere");
        std::fs::create_dir_all(root.join("crates")).expect("root");
        std::fs::create_dir_all(&elsewhere).expect("elsewhere");
        let proc_dir = s.0.join("proc");
        std::fs::create_dir_all(&proc_dir).expect("proc");
        let found = || test_run_here(&proc_dir, &root, 1);
        // Not tests, or not here: nothing.
        fake_proc(&s, 10, &["cargo", "build", "-p", "kourier"], &root);
        fake_proc(&s, 11, &["vim", &root.join("x").to_string_lossy()], &root);
        fake_proc(&s, 12, &["cargo", "nextest", "run"], &elsewhere);
        fake_proc(&s, 13, &["/usr/bin/cargo", "fmt", "--all"], &root);
        assert_eq!(found(), None);
        // `me` is not counted.
        fake_proc(&s, 1, &["cargo-nextest", "nextest", "run"], &root);
        assert_eq!(found(), None);
        // By working directory: nextest, its two spellings, `cargo test`, `cargo t`, a toolchain.
        for (pid, cmd) in [
            (20, vec!["cargo-nextest", "nextest", "run", "--workspace"]),
            (21, vec!["/home/u/.cargo/bin/cargo", "nextest", "run"]),
            (22, vec!["cargo", "test", "-p", "kourier-core"]),
            (23, vec!["cargo", "t"]),
            (24, vec!["cargo", "+nightly", "test"]),
        ] {
            let at = root.join("crates");
            fake_proc(&s, pid, &cmd, &at);
            let line = found().unwrap_or_else(|| panic!("{cmd:?} is a test run"));
            assert!(line.starts_with(&format!("pid {pid}: ")), "{line}");
            std::fs::remove_dir_all(proc_dir.join(pid.to_string())).expect("gone");
            assert_eq!(found(), None, "{cmd:?}");
        }
        // By command line: another working directory, this checkout named in the arguments.
        let manifest = root.join("Cargo.toml");
        fake_proc(
            &s,
            30,
            &[
                "cargo",
                "test",
                "--manifest-path",
                &manifest.to_string_lossy(),
            ],
            &elsewhere,
        );
        assert!(found().expect("by command line").starts_with("pid 30: "));
    }

    #[test]
    fn the_record_is_replaced_never_written_in_place() {
        let s = Scratch::new("record");
        let path = s.0.join(KEEP_FILE);
        let other = s.0.join("hard-link-to-the-old-record");
        std::fs::write(&other, "OLD").expect("old");
        std::fs::hard_link(&other, &path).expect("link");
        let mut graph = graph_with(&[H1]);
        graph.build_dirs.insert("x-0123456789abcdef".to_owned());
        write_record(&path, &graph).expect("written");
        // A write in place would have changed the old record through the other name: a kill in
        // the middle of it leaves a truncated file.
        assert_eq!(std::fs::read_to_string(&other).expect("old"), "OLD");
        let back = Graph::from_json(&std::fs::read_to_string(&path).expect("new")).expect("json");
        assert_eq!(back.hashes, graph.hashes);
        assert_eq!(back.build_dirs, graph.build_dirs);
        let left: Vec<_> = std::fs::read_dir(&s.0)
            .expect("dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(left.is_empty(), "a temporary file stays: {left:?}");
    }

    #[test]
    fn what_the_graph_does_not_name_goes_and_what_it_names_stays() {
        let s = Scratch::new("plan");
        // deps: a unit in the graph (kept), one not (removed), a file without a hash (kept).
        s.file(&format!("deps/libold-{H2}.rlib"), 100, OLD);
        s.file(&format!("deps/old-{H2}.d"), 10, OLD);
        s.file(&format!("deps/libnew-{H1}.rlib"), 100, OLD);
        s.file("deps/libfoo.rlib", 100, OLD);
        s.file(&format!("examples/mock_session-{H2}"), 50, OLD);
        s.file(&format!(".fingerprint/old-{H2}/lib-old"), 5, OLD);
        s.file(&format!(".fingerprint/new-{H1}/lib-new"), 5, OLD);
        // build/: a used directory and a stale one.
        s.file(&format!("build/zvariant-{H2}/out/x"), 20, OLD);
        s.file(&format!("build/qt-{H1}/out/x"), 20, OLD);
        let mut graph = graph_with(&[H1]);
        graph.build_dirs.insert(format!("qt-{H1}"));
        let plan = plan(&s.0, &graph, &HashSet::new(), SystemTime::now(), 1).unwrap();
        assert_eq!(
            removed(&plan),
            [
                format!("libold-{H2}.rlib"),
                format!("mock_session-{H2}"),
                format!("old-{H2}"),
                format!("old-{H2}.d"),
                format!("zvariant-{H2}"),
            ]
        );
        let bytes: u64 = plan.remove.iter().map(|r| r.bytes).sum();
        assert_eq!(bytes, 100 + 50 + 5 + 10 + 20);
        let (freed, failed) = apply(&plan);
        assert_eq!((freed, failed.len()), (bytes, 0));
        assert!(!s.0.join(format!("deps/libold-{H2}.rlib")).exists());
        assert!(!s.0.join(format!("build/zvariant-{H2}")).exists());
        for kept in [
            format!("deps/libnew-{H1}.rlib"),
            "deps/libfoo.rlib".to_owned(),
            format!(".fingerprint/new-{H1}/lib-new"),
            format!("build/qt-{H1}/out/x"),
        ] {
            assert!(s.0.join(&kept).exists(), "{kept} must stay");
        }
        // Applying twice is harmless.
        assert_eq!(apply(&plan).1.len(), 0);
    }

    #[test]
    fn a_graph_too_small_to_be_the_workspace_deletes_nothing() {
        let s = Scratch::new("guard");
        s.file(&format!("deps/libold-{H2}.rlib"), 100, OLD);
        let err = plan(
            &s.0,
            &graph_with(&[H1]),
            &HashSet::new(),
            SystemTime::now(),
            2,
        )
        .expect_err("one hash is too few");
        assert!(
            err.to_string().contains("does not cover the workspace"),
            "{err}"
        );
        assert!(s.0.join(format!("deps/libold-{H2}.rlib")).exists());
        let none = plan(
            &s.0,
            &Graph::default(),
            &HashSet::new(),
            SystemTime::now(),
            MIN_UNITS,
        );
        assert!(none.is_err());
    }

    #[test]
    fn an_executable_some_process_runs_is_never_deleted() {
        let s = Scratch::new("running");
        let exe = s.file(&format!("deps/it-{H2}"), 100, OLD);
        let other = s.file(&format!("deps/other-{H2}"), 100, OLD);
        // The test process itself runs a binary: its inode is in the set, a copy's is not.
        let meta = std::fs::metadata(std::env::current_exe().unwrap()).unwrap();
        let running = running_files();
        assert!(running.contains(&(meta.dev(), meta.ino())));
        // A hard link to a running executable is the same file (cargo links deps/x to debug/x).
        std::fs::remove_file(&exe).unwrap();
        std::fs::hard_link(std::env::current_exe().unwrap(), &exe).unwrap();
        let plan = plan(&s.0, &graph_with(&[H1]), &running, SystemTime::now(), 1).unwrap();
        assert_eq!(removed(&plan), [format!("other-{H2}")]);
        assert_eq!(plan.running, std::slice::from_ref(&exe));
        let (_, failed) = apply(&plan);
        assert!(failed.is_empty());
        assert!(exe.exists() && !other.exists());
    }

    #[test]
    fn what_changed_a_moment_ago_is_left_to_the_session_that_made_it() {
        let s = Scratch::new("young");
        s.file(
            &format!("deps/libfresh-{H2}.rlib"),
            100,
            Duration::from_secs(60),
        );
        s.file(&format!("deps/libold-{H2}.rlib"), 100, OLD);
        let now = SystemTime::now();
        let plan = plan(&s.0, &graph_with(&[H1]), &HashSet::new(), now, 1).unwrap();
        assert_eq!(removed(&plan), [format!("libold-{H2}.rlib")]);
        assert_eq!(plan.young, 1);
        // Later, the same file is old enough.
        let later = now + 2 * YOUNG;
        let plan = super::plan(&s.0, &graph_with(&[H1]), &HashSet::new(), later, 1).unwrap();
        assert_eq!(plan.young, 0);
        assert_eq!(plan.remove.len(), 2);
    }

    #[test]
    fn an_incremental_cache_keeps_the_newest_directories_of_a_crate_with_units() {
        let s = Scratch::new("incremental");
        let ages = [
            ("kourier_core-1aaaaaaaaaaaa", 5 * 3600),
            ("kourier_core-1bbbbbbbbbbbb", 4 * 3600),
            ("kourier_core-1cccccccccccc", 3 * 3600),
            ("gone_crate-1dddddddddddd", 3 * 3600),
        ];
        for (dir, secs) in ages {
            s.file(
                &format!("incremental/{dir}/s-x-y/work"),
                10,
                Duration::from_secs(secs),
            );
        }
        let mut graph = graph_with(&[H1]);
        // Two units of kourier_core (a lib and its test build): two directories stay.
        graph.workspace_units.insert(
            "kourier_core".into(),
            BTreeSet::from([H1.to_owned(), H2.to_owned()]),
        );
        let plan = plan(&s.0, &graph, &HashSet::new(), SystemTime::now(), 1).unwrap();
        assert_eq!(
            removed(&plan),
            ["gone_crate-1dddddddddddd", "kourier_core-1aaaaaaaaaaaa"]
        );
        assert!(plan.remove.iter().all(|r| r.part == Part::Incremental));
    }

    #[test]
    fn the_six_test_targets_called_it_share_the_count_of_their_name() {
        // T3 fix D6: the count is per crate name. Six packages have an `it` test target, so six
        // units of the name `it` keep the six newest directories, whichever package made them;
        // a newer stale one (a hand-run variant) pushes out the oldest current one. A cache
        // lost, never a wrong build; this pins what the doc comment of `plan` says.
        let s = Scratch::new("incremental-it");
        let ages = [
            ("it-1aaaaaaaaaaaa", 6 * 3600),
            ("it-1bbbbbbbbbbbb", 5 * 3600),
            ("it-1cccccccccccc", 4 * 3600),
            ("it-1dddddddddddd", 3 * 3600),
            ("it-1eeeeeeeeeeee", 2 * 3600),
            ("it-1ffffffffffff", 3600),
            ("it-1gggggggggggg", 1800), // stale: no unit of the graph made it
        ];
        for (dir, secs) in ages {
            s.file(
                &format!("incremental/{dir}/s-x-y/work"),
                10,
                Duration::from_secs(secs),
            );
        }
        let mut graph = graph_with(&[H1]);
        let units: BTreeSet<String> = (0..6).map(|i| format!("{i:016x}")).collect();
        graph.workspace_units.insert("it".into(), units);
        let plan = plan(&s.0, &graph, &HashSet::new(), SystemTime::now(), 1).unwrap();
        assert_eq!(removed(&plan), ["it-1aaaaaaaaaaaa"]);
    }

    #[test]
    fn the_canonical_graph_is_the_trim_scripts() {
        // `cargo xtask prune` and the manual cure must name the same build graph.
        let script = include_str!("../../scripts/trim-target.sh");
        let mut from_script: Vec<Vec<String>> = script
            .lines()
            .filter_map(|l| l.strip_prefix("run cargo "))
            .map(|l| {
                let mut a: Vec<String> = l.split_whitespace().map(str::to_owned).collect();
                a.sort();
                a
            })
            .collect();
        let mut ours: Vec<Vec<String>> = canonical_queries()
            .into_iter()
            .map(|mut q| {
                q.sort();
                q
            })
            .collect();
        // The script writes `--no-run` after the package selection; the order is not the point.
        from_script.sort();
        ours.sort();
        assert_eq!(ours, from_script);
        assert_eq!(ours.len(), 5);
    }

    #[test]
    fn the_end_of_the_line_says_how_much_disk_is_left() {
        assert_eq!(disk_note(None), "");
        assert_eq!(disk_note(Some(128 << 30)), "; 128.0 GB free on the disk");
        let low = disk_note(Some(6 << 30));
        assert!(
            low.contains("only 6.0 GB free") && low.contains("cargo xtask prune"),
            "{low}"
        );
        // The real file system answers for a path that exists, and for none that does not.
        assert!(free_bytes(&crate::root()).is_some_and(|f| f > 0));
        assert_eq!(free_bytes(Path::new("/nonexistent-dir-for-the-test")), None);
    }

    #[test]
    fn the_biggest_removals_are_listed_biggest_first() {
        let plan = Plan {
            remove: vec![
                Removal {
                    path: "d/small".into(),
                    bytes: 1 << 20,
                    part: Part::Deps,
                },
                Removal {
                    path: "d/big".into(),
                    bytes: 3 << 30,
                    part: Part::Incremental,
                },
                Removal {
                    path: "d/mid".into(),
                    bytes: 1 << 30,
                    part: Part::Build,
                },
            ],
            ..Plan::default()
        };
        let text = largest(&plan, 2);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].contains("3.0 GB") && lines[0].contains("big"),
            "{text}"
        );
        assert!(lines[1].contains("mid"), "{text}");
    }

    #[test]
    fn the_summary_says_what_was_freed_and_what_stayed() {
        let plan = Plan {
            remove: vec![
                Removal {
                    path: "a".into(),
                    bytes: 3 << 30,
                    part: Part::Deps,
                },
                Removal {
                    path: "b".into(),
                    bytes: 1 << 30,
                    part: Part::Incremental,
                },
            ],
            running: vec!["r".into()],
            young: 2,
        };
        let line = summary(&plan, 4 << 30, &[]);
        assert!(line.contains("freed 4.0 GB"), "{line}");
        assert!(
            line.contains("deps 3.0 GB (1)") && line.contains("incremental 1.0 GB (1)"),
            "{line}"
        );
        assert!(line.contains("kept 1 that a process runs"), "{line}");
        assert!(
            line.contains("kept 2 changed in the last 15 minutes"),
            "{line}"
        );
        assert!(summary(&Plan::default(), 0, &[]).contains("freed 0.0 GB"));
        assert!(
            summary(&Plan::default(), 0, &["x: denied".into()])
                .contains("could not delete 1: x: denied")
        );
    }
}

//! `cargo xtask tdlib <build|check|path>`: the pinned, patched TDLib that the Telegram helper
//! (`kourier-tdlib`) opens at run time (TG-1 plan §3; decisions D3 H1, M6, LC-5, L5).
//!
//! - `build [--jobs N] [--fetch-gperf|--system-gperf] [--detach|--foreground] [--unpatched]`: the
//!   pin, then the cache key; a cache hit verifies the cached library and copies it to
//!   `target/tdlib/`; a miss fetches the source BY HASH from the pin's repository, verifies the
//!   tree (and that the checkout is clean), applies the patches of the pin, finds gperf, configures
//!   (Unix Makefiles, the pin's build type), builds the `tdjson` target, proves the patches are in
//!   the built tree, strips, checks and writes the manifest into
//!   `~/.cache/kourier/tdlib/<key>/out/`. `--detach` starts that miss as a job of its own (`setsid
//!   nohup nice -n 10`, a log and marker files in the cache directory) and returns at once, so a
//!   long build never holds `srv`'s lock; the next plain `build` is then the cache hit. A miss
//!   without `--detach` refuses unless `--foreground` is given (a foreground build inside `srv`
//!   holds its lock for the whole 8 minutes and a `kcap -t 600` kills it half way). `--unpatched`
//!   builds the same commit WITHOUT the patches (another key, `target/tdlib-unpatched/`): only the
//!   trap test's mutant uses it (TG-1b).
//! - One cache entry, one `flock` on `<key>/lock`: the job holds it from its start to its end (the
//!   detached job inherits the descriptor), a cache hit holds it shared while it checks and copies.
//!   The markers say what happened ([`JobState`]): a job that died without `job.done` (the time
//!   limit, the OOM killer, a reboot) is reported, not mistaken for a running one.
//! - `check [--lib <path>] [--unpatched]`: only the checks, on the cached library or any file.
//! - `path [--unpatched]`: prints the verified library under `target/` or exits 1.
//! - `fakedc <build|lib|env|record>`: the lane's TEST-ONLY fake DC (TD-4a, [`fakedc`]).
//!
//! "Pinned and licence-checked" means: `rev-parse HEAD` is the pin's commit, git's tree id and a
//! sha256 over `git ls-tree -r HEAD` are the pin's, the licence files carry the pinned sha256 (and
//! are copied to `out/notices/`), the dynamic symbols are EXACTLY the pin's exports, NEEDED is
//! exactly the pin's list, the SONAME is the pin's, there is no RPATH or RUNPATH, the size is in
//! the pin's range, the manifest's sha256 is the file's and its `patch` lines are the pin's patches
//! with each file's current sha256, in order (a library built before patch 0002 fails `check` and
//! `path`). A failed check deletes nothing.
//!
//! gperf is the pinned `.deb` (`--fetch-gperf` downloads and unpacks it once); a gperf on PATH is
//! taken only with `--system-gperf` and only at the pin's version (the re-check's N4).
//!
//! Nothing here contacts anything but the pin's source host (and, with `--fetch-gperf`, the
//! distribution's package mirror through `apt-get download`); never `sudo`.

mod fakedc;

use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The pin file, relative to the workspace root.
const PIN_FILE: &str = "third_party/tdlib/tdlib.pin";
/// The patch directory, relative to the workspace root.
const PATCH_DIR: &str = "third_party/tdlib/patches";
/// Default parallel compile jobs of the C++ build (decisions D3 M6).
const DEFAULT_JOBS: u32 = 16;
/// The detached job's own time limit (seconds), for the whole fetch, configure and build.
const JOB_TIMEOUT_S: u32 = 5400;
/// The descriptor number the detached job inherits the entry's lock on.
const LOCK_FD: i32 = 9;
/// How long a lock that looks held is asked for again (a program another thread just started holds
/// a copy of the descriptor until its `exec`; `.claude/rules/testing.md`, the `File::try_lock` trap).
const LOCK_SETTLE: Duration = Duration::from_millis(400);
/// Variables the compiler, the linker or cmake read at configure time that would change the library
/// without changing the key (decisions F8; the re-check's N4 added the search paths).
const BUILD_ENV_REMOVED: [&str; 9] = [
    "CFLAGS",
    "CXXFLAGS",
    "CPPFLAGS",
    "LDFLAGS",
    "CPATH",
    "C_INCLUDE_PATH",
    "CPLUS_INCLUDE_PATH",
    "LIBRARY_PATH",
    "PKG_CONFIG_PATH",
];
/// Every variable with this prefix is removed too: cmake reads `CMAKE_*` from the environment
/// (`CMAKE_PREFIX_PATH`, `CMAKE_BUILD_TYPE`, `CMAKE_C_COMPILER_LAUNCHER`, `CMAKE_TOOLCHAIN_FILE`, ...).
const BUILD_ENV_PREFIX_REMOVED: &str = "CMAKE_";
/// Git variables that would point a git call at another repository or object store.
const GIT_ENV_REMOVED: [&str; 5] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

/// The parsed pin file: every field is required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pin {
    pub repo: String,
    pub commit: String,
    pub version: String,
    pub layer: u32,
    pub tree: String,
    pub ls_tree_sha256: String,
    /// `(path in the tree, sha256)`.
    pub licenses: Vec<(String, String)>,
    pub patches: Vec<String>,
    pub build_type: String,
    pub openssl: String,
    pub gperf: String,
    /// The sha256 of the gperf `.deb` that `--fetch-gperf` downloads (decisions F8).
    pub gperf_deb_sha256: String,
    pub soname: String,
    pub exports: Vec<String>,
    pub needed: Vec<String>,
    pub size_mib: (u64, u64),
    /// The sha256 of TDLib's two compiled handshake keys (the PKCS#1 PEM blocks of
    /// `td/telegram/net/PublicRsaKeySharedMain.cpp`, BEGIN line to END line): the genuine-keys check
    /// row of `check`/`path` and the needles of the test-only library (TD-4a, brief §3.4).
    pub rsa_test_pem_sha256: String,
    pub rsa_main_pem_sha256: String,
}

/// The fields a pin file must hold, in the order a reader expects them.
const PIN_FIELDS: [&str; 18] = [
    "repo",
    "commit",
    "version",
    "layer",
    "tree",
    "ls_tree_sha256",
    "licenses",
    "patches",
    "build_type",
    "openssl",
    "gperf",
    "gperf_deb_sha256",
    "soname",
    "exports",
    "needed",
    "size_mib",
    "rsa_test_pem_sha256",
    "rsa_main_pem_sha256",
];

/// One value of the pin's tiny `key = value` grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PinValue {
    Str(String),
    Int(u64),
    List(Vec<PinValue>),
}

fn parse_value(text: &str) -> Result<PinValue> {
    let text = text.trim();
    if let Some(inner) = text.strip_prefix('"') {
        let Some(s) = inner.strip_suffix('"') else {
            bail!("an unterminated string");
        };
        if s.contains('"') || s.contains('\\') {
            bail!("a string with a quote or a backslash");
        }
        return Ok(PinValue::Str(s.to_owned()));
    }
    if let Some(inner) = text.strip_prefix('[') {
        let Some(inner) = inner.strip_suffix(']') else {
            bail!("an unterminated list");
        };
        let mut items = Vec::new();
        let mut rest = inner.trim();
        while !rest.is_empty() {
            let (item, after) = if let Some(quoted) = rest.strip_prefix('"') {
                let end = quoted
                    .find('"')
                    .context("an unterminated string in a list")?
                    + 2;
                (&rest[..end], &rest[end..])
            } else {
                match rest.find(',') {
                    Some(comma) => (&rest[..comma], &rest[comma..]),
                    None => (rest, ""),
                }
            };
            items.push(parse_value(item)?);
            rest = after.trim_start();
            if let Some(after_comma) = rest.strip_prefix(',') {
                rest = after_comma.trim_start();
            } else if !rest.is_empty() {
                bail!("a list item not followed by a comma");
            }
        }
        return Ok(PinValue::List(items));
    }
    text.parse::<u64>()
        .map(PinValue::Int)
        .map_err(|_| anyhow::anyhow!("not a string, a number or a list: {text}"))
}

impl Pin {
    pub(crate) fn parse(text: &str) -> Result<Pin> {
        let mut fields: std::collections::BTreeMap<String, PinValue> = Default::default();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .with_context(|| format!("pin line {}: no `=`", n + 1))?;
            let key = key.trim();
            if !PIN_FIELDS.contains(&key) {
                bail!("pin line {}: unknown field `{key}`", n + 1);
            }
            let value =
                parse_value(value).with_context(|| format!("pin line {}: `{key}`", n + 1))?;
            if fields.insert(key.to_owned(), value).is_some() {
                bail!("pin line {}: `{key}` given twice", n + 1);
            }
        }
        let mut take = |key: &str| {
            fields
                .remove(key)
                .with_context(|| format!("the pin has no `{key}`"))
        };
        fn string(key: &str, v: PinValue) -> Result<String> {
            match v {
                PinValue::Str(s) if !s.is_empty() => Ok(s),
                _ => bail!("pin `{key}`: a non-empty string is wanted"),
            }
        }
        fn strings(key: &str, v: PinValue) -> Result<Vec<String>> {
            match v {
                PinValue::List(items) => items.into_iter().map(|i| string(key, i)).collect(),
                _ => bail!("pin `{key}`: a list of strings is wanted"),
            }
        }
        fn hex(key: &str, s: String, len: usize) -> Result<String> {
            if s.len() != len
                || !s
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            {
                bail!("pin `{key}`: {len} lower-case hex digits are wanted");
            }
            Ok(s)
        }
        let repo = string("repo", take("repo")?)?;
        let commit = hex("commit", string("commit", take("commit")?)?, 40)?;
        let version = string("version", take("version")?)?;
        let layer = match take("layer")? {
            PinValue::Int(n) => u32::try_from(n).context("pin `layer` is too large")?,
            _ => bail!("pin `layer`: a number is wanted"),
        };
        let tree = hex("tree", string("tree", take("tree")?)?, 40)?;
        let ls_tree_sha256 = hex(
            "ls_tree_sha256",
            string("ls_tree_sha256", take("ls_tree_sha256")?)?,
            64,
        )?;
        let licenses = strings("licenses", take("licenses")?)?
            .into_iter()
            .map(|l| {
                let (path, sum) = l
                    .split_once(' ')
                    .context("pin `licenses`: `<path> <sha256>` is wanted")?;
                Ok((path.to_owned(), hex("licenses", sum.to_owned(), 64)?))
            })
            .collect::<Result<Vec<_>>>()?;
        if licenses.is_empty() {
            bail!("pin `licenses`: at least one licence file is wanted");
        }
        let patches = strings("patches", take("patches")?)?;
        let build_type = string("build_type", take("build_type")?)?;
        let openssl = string("openssl", take("openssl")?)?;
        let gperf = string("gperf", take("gperf")?)?;
        let gperf_deb_sha256 = hex(
            "gperf_deb_sha256",
            string("gperf_deb_sha256", take("gperf_deb_sha256")?)?,
            64,
        )?;
        let soname = string("soname", take("soname")?)?;
        let exports = strings("exports", take("exports")?)?;
        let needed = strings("needed", take("needed")?)?;
        if exports.is_empty() || needed.is_empty() {
            bail!("pin `exports` and `needed` must not be empty");
        }
        let size_mib = match take("size_mib")? {
            PinValue::List(v) => match v.as_slice() {
                [PinValue::Int(lo), PinValue::Int(hi)] if lo <= hi => (*lo, *hi),
                _ => bail!("pin `size_mib`: [low, high] is wanted"),
            },
            _ => bail!("pin `size_mib`: [low, high] is wanted"),
        };
        let rsa_test_pem_sha256 = hex(
            "rsa_test_pem_sha256",
            string("rsa_test_pem_sha256", take("rsa_test_pem_sha256")?)?,
            64,
        )?;
        let rsa_main_pem_sha256 = hex(
            "rsa_main_pem_sha256",
            string("rsa_main_pem_sha256", take("rsa_main_pem_sha256")?)?,
            64,
        )?;
        if rsa_test_pem_sha256 == rsa_main_pem_sha256 {
            bail!("pin: the two handshake keys must differ");
        }
        Ok(Pin {
            repo,
            commit,
            version,
            layer,
            tree,
            ls_tree_sha256,
            licenses,
            patches,
            build_type,
            openssl,
            gperf,
            gperf_deb_sha256,
            soname,
            exports,
            needed,
            size_mib,
            rsa_test_pem_sha256,
            rsa_main_pem_sha256,
        })
    }

    fn load(root: &Path) -> Result<Pin> {
        let path = root.join(PIN_FILE);
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        Pin::parse(&text)
    }

    /// The cmake configure flags, without the source, build and gperf paths.
    fn cmake_flags(&self) -> Vec<String> {
        vec![
            format!("-DCMAKE_BUILD_TYPE={}", self.build_type),
            "-DCMAKE_C_COMPILER=/usr/bin/gcc".to_owned(),
            "-DCMAKE_CXX_COMPILER=/usr/bin/g++".to_owned(),
        ]
    }
}

/// Everything the cache key is made of. A change of any field is another key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyInputs {
    pub commit: String,
    pub tree: String,
    pub ls_tree_sha256: String,
    pub build_type: String,
    pub cmake_flags: Vec<String>,
    /// `(file name, sha256 of its content)`, in the order applied; empty for `--unpatched`.
    pub patches: Vec<(String, String)>,
    pub gcc: String,
    pub gxx: String,
    /// `ld --version`'s first line (binutils: the linker and `strip` come from one package).
    pub ld: String,
    pub cmake: String,
    pub gperf: String,
    pub openssl: String,
    /// The installed `libssl-dev` and `zlib1g-dev` (`<package> <version>`, `; `-joined).
    pub dev_packages: String,
}

impl KeyInputs {
    /// The text the key hashes (also written into the manifest).
    pub(crate) fn text(&self) -> String {
        let mut t = String::new();
        let mut line = |k: &str, v: &str| {
            t.push_str(k);
            t.push_str(" = ");
            t.push_str(v);
            t.push('\n');
        };
        line("commit", &self.commit);
        line("tree", &self.tree);
        line("ls_tree_sha256", &self.ls_tree_sha256);
        line("build_type", &self.build_type);
        line("cmake_flags", &self.cmake_flags.join(" "));
        for (name, sum) in &self.patches {
            line("patch", &format!("{name} {sum}"));
        }
        if self.patches.is_empty() {
            line("patch", "none");
        }
        line("gcc", &self.gcc);
        line("g++", &self.gxx);
        line("ld", &self.ld);
        line("cmake", &self.cmake);
        line("gperf", &self.gperf);
        line("openssl", &self.openssl);
        line("dev_packages", &self.dev_packages);
        t
    }

    /// The first 16 hex digits of the sha256 over [`KeyInputs::text`].
    pub(crate) fn key(&self) -> Result<String> {
        Ok(sha256_bytes(self.text().as_bytes())?[..16].to_owned())
    }
}

/// The result of one library check: each line names one failed rule.
#[derive(Debug, Default)]
pub(crate) struct CheckReport {
    pub problems: Vec<String>,
}

/// Defined dynamic symbols from `nm -D --defined-only` output (`<value> <type> <name>`), without
/// the linker's own `_init`/`_fini`.
pub(crate) fn defined_symbols(nm: &str) -> BTreeSet<String> {
    nm.lines()
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let (_, ty, name) = (parts.next()?, parts.next()?, parts.next()?);
            // Version node symbols (`A`) are no exports.
            (ty != "A" && name != "_init" && name != "_fini")
                .then(|| name.split('@').next().unwrap_or(name).to_owned())
        })
        .collect()
}

/// What `readelf -d` says: the NEEDED entries, the SONAME, and whether an RPATH or RUNPATH is set.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Dynamic {
    pub needed: BTreeSet<String>,
    pub soname: Option<String>,
    pub search_path: bool,
}

pub(crate) fn parse_dynamic(readelf: &str) -> Dynamic {
    let mut d = Dynamic::default();
    for l in readelf.lines() {
        let bracketed = || {
            let start = l.find('[')? + 1;
            let end = l[start..].find(']')? + start;
            Some(l[start..end].to_owned())
        };
        if l.contains("(NEEDED)") {
            if let Some(v) = bracketed() {
                d.needed.insert(v);
            }
        } else if l.contains("(SONAME)") {
            d.soname = bracketed();
        } else if l.contains("(RPATH)") || l.contains("(RUNPATH)") {
            d.search_path = true;
        }
    }
    d
}

/// The rules on a library's dynamic section and symbols (set equality, never "at least").
pub(crate) fn check_elf(pin: &Pin, nm: &str, readelf: &str, size: u64) -> CheckReport {
    let mut r = CheckReport::default();
    let want: BTreeSet<String> = pin.exports.iter().cloned().collect();
    let have = defined_symbols(nm);
    if have != want {
        let missing: Vec<_> = want.difference(&have).collect();
        let extra: Vec<_> = have.difference(&want).collect();
        r.problems.push(format!(
            "exports differ from the pin: missing {missing:?}, extra {extra:?}"
        ));
    }
    let d = parse_dynamic(readelf);
    let want: BTreeSet<String> = pin.needed.iter().cloned().collect();
    if d.needed != want {
        let missing: Vec<_> = want.difference(&d.needed).collect();
        let extra: Vec<_> = d.needed.difference(&want).collect();
        r.problems.push(format!(
            "NEEDED differs from the pin: missing {missing:?}, extra {extra:?}"
        ));
    }
    if d.soname.as_deref() != Some(pin.soname.as_str()) {
        r.problems.push(format!(
            "SONAME is {:?}, the pin says {}",
            d.soname, pin.soname
        ));
    }
    if d.search_path {
        r.problems
            .push("the library has an RPATH or RUNPATH".to_owned());
    }
    let mib = 1024 * 1024;
    let (lo, hi) = pin.size_mib;
    if size < lo * mib || size > hi * mib {
        r.problems.push(format!(
            "size {size} bytes is outside the pin's {lo}-{hi} MiB"
        ));
    }
    r
}

/// The first line of every compiled RSA public key block, and its last.
pub(crate) const PEM_BEGIN: &[u8] = b"-----BEGIN RSA PUBLIC KEY-----";
pub(crate) const PEM_END: &[u8] = b"-----END RSA PUBLIC KEY-----";

/// Every offset of `needle` in `hay` (overlaps counted).
pub(crate) fn find_all(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return Vec::new();
    }
    hay.windows(needle.len())
        .enumerate()
        .filter_map(|(i, w)| (w == needle).then_some(i))
        .collect()
}

/// Every RSA public key block in `bytes` as `(offset, length)`, BEGIN line to the end of the END
/// line; a BEGIN without an END within 4 KiB counts with length 0 (no key of ours looks so).
pub(crate) fn pem_blocks(bytes: &[u8]) -> Vec<(usize, usize)> {
    find_all(bytes, PEM_BEGIN)
        .into_iter()
        .map(|at| {
            let window = &bytes[at..bytes.len().min(at + 4096)];
            let len = find_all(window, PEM_END)
                .first()
                .map_or(0, |end| end + PEM_END.len());
            (at, len)
        })
        .collect()
}

/// The genuine-keys row (TD-4a, brief §3.4): a REAL library holds TDLib's two handshake keys (by
/// the pin's sha256, each exactly once) and the pin's commit exactly once. The test-only library
/// (both keys ours, the commit zeros) fails it, so `check`, `path` and everything that takes the
/// library through them (`KOURIER_TDJSON=$(cargo xtask tdlib path)`) refuse it. `dist` ships no
/// TDLib yet (it names no tdjson): the shipping guard is the row of `check`/`path` alone until
/// T9, which must call `check_file` with the manifest and the pin's patches (S-L5).
pub(crate) fn genuine_keys_problems(pin: &Pin, bytes: &[u8]) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    let mut hashes = Vec::new();
    for (at, len) in pem_blocks(bytes) {
        hashes.push(sha256_bytes(&bytes[at..at + len])?);
    }
    for (what, want) in [
        ("test", &pin.rsa_test_pem_sha256),
        ("production", &pin.rsa_main_pem_sha256),
    ] {
        let n = hashes.iter().filter(|h| *h == want).count();
        if n != 1 {
            problems.push(format!(
                "the library holds TDLib's genuine {what} handshake key {n} times, not once (a test-only library?)"
            ));
        }
    }
    let n = find_all(bytes, pin.commit.as_bytes()).len();
    if n != 1 {
        problems.push(format!(
            "the library names the pin's commit {n} times, not once (a test-only library?)"
        ));
    }
    Ok(problems)
}

/// The value of `key = "…"` in a manifest written by [`write_manifest`].
fn manifest_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(" = ")?;
        (k.trim() == key).then(|| v.trim().trim_matches('"').to_owned())
    })
}

/// Every value of `key = "…"` in a manifest, in order (the `patch` lines repeat).
fn manifest_values(text: &str, key: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(" = ")?;
            (k.trim() == key).then(|| v.trim().trim_matches('"').to_owned())
        })
        .collect()
}

/// The manifest's `patch` lines must be exactly `want` (`<name> <sha256>`, in order; `none` when
/// `want` is empty): a library built before a patch joined the pin, or with an older patch file,
/// must not pass `check`/`path` (patch 0002: such a library answers incoming calls).
fn manifest_patch_problem(text: &str, want: &[(String, String)]) -> Option<String> {
    let have = manifest_values(text, "patch");
    let want: Vec<String> = if want.is_empty() {
        vec!["none".to_owned()]
    } else {
        want.iter().map(|(n, s)| format!("{n} {s}")).collect()
    };
    (have != want).then(|| {
        format!(
            "the library was built with other patches (manifest {have:?}, pin {want:?}): run `cargo xtask tdlib build`"
        )
    })
}

// ------------------------------------------------------------------------------------------------
// Commands

struct Opts {
    jobs: u32,
    fetch_gperf: bool,
    /// A gperf on PATH is taken although it is not the pinned package (its version must still be
    /// the pin's).
    system_gperf: bool,
    detach: bool,
    /// A miss is built in this process (inside `srv` that holds its lock for the whole build).
    foreground: bool,
    unpatched: bool,
    lib: Option<PathBuf>,
    /// The detached job (internal): the inputs directory it reads the pin and patches from.
    job: Option<PathBuf>,
}

fn parse_opts(args: &[String]) -> Result<Opts> {
    let mut o = Opts {
        jobs: DEFAULT_JOBS,
        fetch_gperf: false,
        system_gperf: false,
        detach: false,
        foreground: false,
        unpatched: false,
        lib: None,
        job: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--jobs" => {
                o.jobs = it
                    .next()
                    .context("--jobs needs a number")?
                    .parse()
                    .context("--jobs needs a number")?;
            }
            "--fetch-gperf" => o.fetch_gperf = true,
            "--system-gperf" => o.system_gperf = true,
            "--detach" => o.detach = true,
            "--foreground" => o.foreground = true,
            "--unpatched" => o.unpatched = true,
            "--lib" => o.lib = Some(PathBuf::from(it.next().context("--lib needs a path")?)),
            "--job" => o.job = Some(PathBuf::from(it.next().context("--job needs a path")?)),
            other => bail!("tdlib: unknown argument {other}"),
        }
    }
    if o.detach && o.foreground {
        bail!("tdlib: --detach and --foreground exclude each other");
    }
    Ok(o)
}

pub(crate) fn tdlib(args: &[String]) -> Result<i32> {
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => ("", &[][..]),
    };
    if cmd == "fakedc" {
        return fakedc::fakedc(rest);
    }
    let opts = parse_opts(rest)?;
    match cmd {
        "build" => build(&opts),
        "check" => check(&opts),
        "path" => path(&opts),
        _ => {
            eprintln!(
                "usage: cargo xtask tdlib <build [--jobs N] [--fetch-gperf|--system-gperf] [--detach|--foreground] [--unpatched]|check [--lib <path>] [--unpatched]|path [--unpatched]|fakedc <build|lib|env|record>>"
            );
            Ok(2)
        }
    }
}

/// `~/.cache/kourier/tdlib` (the builder container's own disk; never synced, never fetched).
fn cache_root() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".cache/kourier/tdlib"))
}

fn tools_root() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".cache/kourier/tools"))
}

/// Where a verified copy goes: `target/tdlib/` (or `target/tdlib-unpatched/`).
fn target_copy(unpatched: bool) -> PathBuf {
    crate::target_dir().join(if unpatched {
        "tdlib-unpatched"
    } else {
        "tdlib"
    })
}

fn output(cmd: &mut Command) -> Result<String> {
    let desc = format!("{cmd:?}");
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("spawn {desc}"))?;
    if !out.status.success() {
        bail!(
            "{desc} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn first_line(cmd: &mut Command) -> Result<String> {
    Ok(output(cmd)?
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned())
}

fn sha256_bytes(bytes: &[u8]) -> Result<String> {
    use std::io::Write;
    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("spawn sha256sum")?;
    child
        .stdin
        .take()
        .context("sha256sum stdin")?
        .write_all(bytes)?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("sha256sum failed");
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned())
}

fn sha256_file(path: &Path) -> Result<String> {
    let text = output(Command::new("sha256sum").arg(path))?;
    Ok(text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned())
}

/// The fetched `.deb` must be the pinned one (decisions F8): a package the archive changed since is
/// a decision for the pin, not something a build takes silently.
fn check_deb_sum(have: &str, pin: &Pin) -> Result<()> {
    if have == pin.gperf_deb_sha256 {
        return Ok(());
    }
    bail!(
        "the gperf package's sha256 is {have}, the pin says {}: if the archive moved on, look at the new package and change `gperf_deb_sha256` in {PIN_FILE} on purpose",
        pin.gperf_deb_sha256
    )
}

/// Where gperf comes from ([`gperf_source`]).
#[derive(Debug, PartialEq, Eq)]
enum GperfSource {
    /// The pinned package, already unpacked into the tools cache.
    Unpacked,
    /// The one on PATH (`--system-gperf`).
    Path,
    /// The pinned package, downloaded and unpacked now (`--fetch-gperf`).
    Fetch,
}

/// The re-check's N4: a gperf on PATH is not the pinned package, so it is taken only when asked
/// for (`--system-gperf`); otherwise the pinned package (unpacked, or fetched with `--fetch-gperf`).
fn gperf_source(unpacked: bool, on_path: bool, fetch: bool, system: bool) -> Result<GperfSource> {
    if unpacked {
        return Ok(GperfSource::Unpacked);
    }
    if system && on_path {
        return Ok(GperfSource::Path);
    }
    if fetch {
        return Ok(GperfSource::Fetch);
    }
    if on_path {
        bail!(
            "the gperf on PATH is not the pinned package: run `cargo xtask tdlib build --fetch-gperf` (the pinned .deb, no root), or add --system-gperf to take it (its version must be the pin's)"
        );
    }
    if system {
        bail!("--system-gperf: there is no gperf on PATH");
    }
    bail!(
        "gperf is missing: install it (the builder recipe) or run `cargo xtask tdlib build --fetch-gperf` (apt-get download + dpkg -x into ~/.cache/kourier/tools, no root)"
    )
}

/// `gperf --version`'s first line must name the pin's version as its last word ("GNU gperf 3.1").
fn check_gperf_version(first_line: &str, pin: &Pin) -> Result<()> {
    if first_line.split_whitespace().last() == Some(pin.gperf.as_str()) {
        return Ok(());
    }
    bail!(
        "the gperf on PATH is not version {} of the pin (`gperf --version` says {first_line:?})",
        pin.gperf
    )
}

/// gperf: the pinned package unpacked into the tools cache (fetched with `--fetch-gperf`), or with
/// `--system-gperf` the one on PATH, whose version must be the pin's ([`gperf_source`]). The
/// unpacked package must be the pinned `.deb` ([`check_deb_sum`]). Returns the binary and the
/// .deb's sha256 when it is the package.
fn find_gperf(fetch: bool, system: bool, pin: &Pin) -> Result<(PathBuf, Option<String>)> {
    let dir = tools_root()?.join("gperf");
    let local = dir.join("root/usr/bin/gperf");
    let on_path = output(Command::new("sh").args(["-c", "command -v gperf"]))
        .map(|p| p.trim().to_owned())
        .ok()
        .filter(|p| !p.is_empty());
    let source = gperf_source(local.is_file(), on_path.is_some(), fetch, system)?;
    if source == GperfSource::Unpacked {
        let deb = std::fs::read_to_string(dir.join("deb.sha256"))
            .map(|s| s.trim().to_owned())
            .with_context(|| {
                format!(
                    "{} has no deb.sha256: delete the directory and fetch again",
                    dir.display()
                )
            })?;
        check_deb_sum(&deb, pin).with_context(|| {
            format!(
                "the unpacked gperf in {} (delete it to fetch again)",
                dir.display()
            )
        })?;
        return Ok((local, Some(deb)));
    }
    if let (GperfSource::Path, Some(path)) = (&source, on_path) {
        let path = PathBuf::from(path);
        check_gperf_version(&first_line(Command::new(&path).arg("--version"))?, pin)?;
        return Ok((path, None));
    }
    std::fs::create_dir_all(&dir)?;
    // No older .deb may stay beside the new one: the one that is there afterwards is the one
    // downloaded now, not whichever `read_dir` lists first.
    for e in std::fs::read_dir(&dir)? {
        let p = e?.path();
        if p.extension().is_some_and(|e| e == "deb") {
            std::fs::remove_file(&p)?;
        }
    }
    // apt verifies the package against its signed lists; no root is needed for a download.
    output(
        Command::new("apt-get")
            .args(["download", "gperf"])
            .current_dir(&dir),
    )?;
    let deb = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|e| e == "deb"))
        .context("apt-get download left no .deb")?;
    let sum = sha256_file(&deb)?;
    if let Err(e) = check_deb_sum(&sum, pin) {
        let _ = std::fs::remove_file(&deb);
        return Err(e);
    }
    output(
        Command::new("dpkg")
            .arg("-x")
            .arg(&deb)
            .arg(dir.join("root")),
    )?;
    std::fs::write(dir.join("deb.sha256"), format!("{sum}\n"))?;
    if !local.is_file() {
        bail!("the gperf package has no usr/bin/gperf");
    }
    Ok((local, Some(sum)))
}

/// `dpkg-query -W -f '${Package} ${Version}\n' libssl-dev zlib1g-dev`'s output as the key's one
/// line: both packages must be there (the C++ build links against them), in the order asked.
fn dev_packages_line(dpkg: &str) -> Result<String> {
    let mut found = Vec::new();
    for want in ["libssl-dev", "zlib1g-dev"] {
        let line = dpkg
            .lines()
            .find(|l| l.split_whitespace().next() == Some(want))
            .with_context(|| format!("the package {want} is not installed"))?;
        let mut parts = line.split_whitespace();
        let (name, version) = (parts.next(), parts.next());
        match (name, version, parts.next()) {
            (Some(name), Some(version), None) => found.push(format!("{name} {version}")),
            _ => bail!("dpkg-query printed an unexpected line for {want}"),
        }
    }
    Ok(found.join("; "))
}

/// The pin's patches as `(file name, sha256 of the file in patch_dir)`, in order; empty for
/// `--unpatched`.
fn pin_patches(pin: &Pin, patch_dir: &Path, unpatched: bool) -> Result<Vec<(String, String)>> {
    if unpatched {
        return Ok(Vec::new());
    }
    pin.patches
        .iter()
        .map(|p| Ok((p.clone(), sha256_file(&patch_dir.join(p))?)))
        .collect()
}

/// The key inputs on this machine for `pin`, with the patch files read from `patch_dir`.
fn key_inputs(pin: &Pin, patch_dir: &Path, unpatched: bool, gperf: &Path) -> Result<KeyInputs> {
    let dpkg = output(Command::new("dpkg-query").args([
        "-W",
        "-f",
        "${Package} ${Version}\\n",
        "libssl-dev",
        "zlib1g-dev",
    ]))?;
    let patches = pin_patches(pin, patch_dir, unpatched)?;
    Ok(KeyInputs {
        commit: pin.commit.clone(),
        tree: pin.tree.clone(),
        ls_tree_sha256: pin.ls_tree_sha256.clone(),
        build_type: pin.build_type.clone(),
        cmake_flags: pin.cmake_flags(),
        patches,
        gcc: first_line(Command::new("/usr/bin/gcc").arg("--version"))?,
        gxx: first_line(Command::new("/usr/bin/g++").arg("--version"))?,
        ld: first_line(Command::new("ld").arg("--version"))?,
        cmake: first_line(Command::new("cmake").arg("--version"))?,
        gperf: first_line(Command::new(gperf).arg("--version"))?,
        openssl: pin.openssl.clone(),
        dev_packages: dev_packages_line(&dpkg)?,
    })
}

/// A git call that reads no config of the builder's (`core.autocrlf`, `filter.*`, sparse checkout,
/// hooks and templates could change the files on disk without changing the commit) and no
/// repository named by the environment (decisions F8).
fn git_command(repo: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C")
        .arg(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    for var in GIT_ENV_REMOVED {
        c.env_remove(var);
    }
    c
}

/// `git status --porcelain --untracked-files=all --ignored` of the fresh checkout: nothing at all.
fn ensure_clean_tree(status: &str) -> Result<()> {
    let dirty: Vec<&str> = status.lines().take(5).collect();
    if dirty.is_empty() {
        return Ok(());
    }
    bail!(
        "the checked-out tree is not clean before the patches (`git status` lists {} line(s), the first: {dirty:?})",
        status.lines().count()
    )
}

/// After the build: the tree that was built holds EXACTLY the pin's patches. Patched: each one
/// reverses cleanly (`git apply --reverse --check`); unpatched: each still applies forward.
fn verify_patches_in_tree(src: &Path, patch_dir: &Path, pin: &Pin, unpatched: bool) -> Result<()> {
    for name in &pin.patches {
        let patch = patch_dir.join(name).display().to_string();
        let mut git = git_command(src);
        git.args(["apply", "--check"]);
        if !unpatched {
            git.arg("--reverse");
        }
        git.arg(&patch);
        output(&mut git).with_context(|| {
            if unpatched {
                format!("the unpatched tree already holds {name}")
            } else {
                format!("the built tree does not hold {name}")
            }
        })?;
    }
    Ok(())
}

/// The environment of the C++ build: no flag, search-path or `CMAKE_*` variable of the builder's
/// (the compiler and cmake read them, and they would change the library without changing its key).
trait BuildEnv {
    fn without_build_flags(&mut self) -> &mut Self;
}

impl BuildEnv for Command {
    fn without_build_flags(&mut self) -> &mut Self {
        for var in build_vars_removed(std::env::vars_os().map(|(k, _)| k)) {
            self.env_remove(var);
        }
        self
    }
}

/// What [`BuildEnv::without_build_flags`] removes from an environment holding `names`: every name of
/// [`BUILD_ENV_REMOVED`] (present or not) and every present name that starts with
/// [`BUILD_ENV_PREFIX_REMOVED`].
fn build_vars_removed(
    names: impl IntoIterator<Item = std::ffi::OsString>,
) -> Vec<std::ffi::OsString> {
    let mut out: Vec<std::ffi::OsString> = BUILD_ENV_REMOVED.iter().map(Into::into).collect();
    out.extend(names.into_iter().filter(|n| {
        n.to_str()
            .is_some_and(|n| n.starts_with(BUILD_ENV_PREFIX_REMOVED))
    }));
    out
}

/// The checks of a library file against the pin (and, when given, against a manifest's sha256,
/// commit and patch list; `patches` is what the manifest must list, see [`pin_patches`]).
fn check_file(
    pin: &Pin,
    lib: &Path,
    manifest: Option<&Path>,
    patches: &[(String, String)],
) -> Result<CheckReport> {
    check_file_read(pin, lib, manifest, patches).map(|(report, _)| report)
}

/// [`check_file`] and the bytes it read: the one read every rule (and the manifest's sha256) judged.
fn check_file_read(
    pin: &Pin,
    lib: &Path,
    manifest: Option<&Path>,
    patches: &[(String, String)],
) -> Result<(CheckReport, Vec<u8>)> {
    let nm = output(Command::new("nm").args(["-D", "--defined-only"]).arg(lib))?;
    let readelf = output(Command::new("readelf").arg("-d").arg(lib))?;
    let bytes = std::fs::read(lib).with_context(|| format!("read {}", lib.display()))?;
    let r = check_bytes(pin, &nm, &readelf, &bytes)?;
    let r = check_manifest(pin, r, &bytes, manifest, patches)?;
    Ok((r, bytes))
}

/// The rules on a library's bytes: [`check_elf`] and the genuine-keys row.
pub(crate) fn check_bytes(pin: &Pin, nm: &str, readelf: &str, bytes: &[u8]) -> Result<CheckReport> {
    let mut r = check_elf(pin, nm, readelf, bytes.len() as u64);
    r.problems.extend(genuine_keys_problems(pin, bytes)?);
    Ok(r)
}

/// The manifest rows of [`check_file`]: sha256, commit and patch list.
fn check_manifest(
    pin: &Pin,
    mut r: CheckReport,
    bytes: &[u8],
    manifest: Option<&Path>,
    patches: &[(String, String)],
) -> Result<CheckReport> {
    if let Some(m) = manifest {
        let text = std::fs::read_to_string(m).with_context(|| format!("read {}", m.display()))?;
        let want = manifest_value(&text, "sha256").unwrap_or_default();
        let have = sha256_bytes(bytes)?;
        if want != have {
            r.problems
                .push("the library's sha256 differs from its manifest".to_owned());
        }
        if manifest_value(&text, "commit").as_deref() != Some(pin.commit.as_str()) {
            r.problems
                .push("the manifest names another commit".to_owned());
        }
        if let Some(p) = manifest_patch_problem(&text, patches) {
            r.problems.push(p);
        }
    }
    Ok(r)
}

fn report(r: &CheckReport, what: &Path) -> Result<()> {
    if r.problems.is_empty() {
        eprintln!("tdlib: {} passes every check", what.display());
        Ok(())
    } else {
        for p in &r.problems {
            eprintln!("tdlib: check failed: {p}");
        }
        bail!("{} failed {} check(s)", what.display(), r.problems.len())
    }
}

/// The paths of one cache entry.
struct Entry {
    dir: PathBuf,
}

impl Entry {
    fn src(&self) -> PathBuf {
        self.dir.join("src")
    }
    fn build(&self) -> PathBuf {
        self.dir.join("build")
    }
    fn out(&self) -> PathBuf {
        self.dir.join("out")
    }
    fn lib(&self, pin: &Pin) -> PathBuf {
        self.out().join(&pin.soname)
    }
    fn manifest(&self) -> PathBuf {
        self.out().join("manifest.toml")
    }
    fn log(&self) -> PathBuf {
        self.dir.join("job.log")
    }
    fn pid(&self) -> PathBuf {
        self.dir.join("job.pid")
    }
    fn done(&self) -> PathBuf {
        self.dir.join("job.done")
    }
    fn lock_path(&self) -> PathBuf {
        self.dir.join("lock")
    }
    /// The lock file of the entry (created when it is missing).
    fn open_lock(&self) -> Result<File> {
        std::fs::create_dir_all(&self.dir)?;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.lock_path())
            .with_context(|| format!("open {}", self.lock_path().display()))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LockKind {
    Exclusive,
    Shared,
}

/// Takes the lock of `file`, asking again for up to `wait` while it looks held (the
/// `File::try_lock` trap in `.claude/rules/testing.md`). `false`: someone else holds it.
fn lock_for(file: &File, kind: LockKind, wait: Duration) -> Result<bool> {
    let deadline = Instant::now() + wait;
    loop {
        let r = match kind {
            LockKind::Exclusive => file.try_lock(),
            LockKind::Shared => file.try_lock_shared(),
        };
        match r {
            Ok(()) => return Ok(true),
            Err(std::fs::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).context("lock the cache entry");
            }
        }
    }
}

/// What the markers and the lock of one cache entry say about its job (decisions F8).
#[derive(Debug, PartialEq, Eq)]
enum JobState {
    /// No job left a mark here.
    Idle,
    /// Someone holds the entry's lock: a job (its pid, when it wrote one) or a cache hit copying.
    Running { pid: Option<String> },
    /// The job ended and wrote `job.done` (its exit status).
    Done(String),
    /// `job.pid` exists, `job.done` does not and nobody holds the lock: the job died (its time
    /// limit, the OOM killer, a reboot) without telling.
    Died { pid: String },
}

fn read_marker(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
}

fn job_state(entry: &Entry) -> Result<JobState> {
    let pid = read_marker(&entry.pid());
    let lock = entry.open_lock()?;
    if lock_for(&lock, LockKind::Shared, LOCK_SETTLE)? {
        // Nobody holds it exclusively; the markers decide.
        drop(lock);
        if let Some(done) = read_marker(&entry.done()) {
            return Ok(JobState::Done(done));
        }
        return Ok(match pid {
            Some(pid) => JobState::Died { pid },
            None => JobState::Idle,
        });
    }
    Ok(JobState::Running { pid })
}

fn build(o: &Opts) -> Result<i32> {
    if let Some(inputs) = &o.job {
        // The detached job: its parent took the entry's lock and handed the descriptor on.
        return job(o, inputs);
    }
    let root = crate::root();
    let pin = Pin::load(&root)?;
    let (gperf, _) = find_gperf(o.fetch_gperf, o.system_gperf, &pin)?;
    let inputs = key_inputs(&pin, &root.join(PATCH_DIR), o.unpatched, &gperf)?;
    let key = inputs.key()?;
    let entry = Entry {
        dir: cache_root()?.join(&key),
    };
    eprintln!(
        "tdlib: key {key} ({}{})",
        pin.commit,
        if o.unpatched { ", unpatched" } else { "" }
    );
    let lock = entry.open_lock()?;
    if entry.manifest().is_file() && entry.lib(&pin).is_file() {
        // A job that still writes holds the lock exclusively; a hit holds it shared while it
        // checks and copies.
        if !lock_for(&lock, LockKind::Shared, LOCK_SETTLE)? {
            eprintln!(
                "tdlib: a job holds the lock of key {key}; its output is not final yet (log: {})",
                entry.log().display()
            );
            return Ok(1);
        }
        let started = Instant::now();
        report(
            &check_file(
                &pin,
                &entry.lib(&pin),
                Some(&entry.manifest()),
                &inputs.patches,
            )?,
            &entry.lib(&pin),
        )?;
        let dest = target_copy(o.unpatched);
        copy_out(&entry.out(), &dest)?;
        eprintln!(
            "tdlib: cache hit, verified and copied to {} in {:.1} s",
            dest.display(),
            started.elapsed().as_secs_f64()
        );
        return Ok(0);
    }
    drop(lock);
    match job_state(&entry)? {
        JobState::Running { pid } => {
            eprintln!(
                "tdlib: a detached build runs (pid {}); its log: {}",
                pid.as_deref().unwrap_or("not written yet"),
                entry.log().display()
            );
            return Ok(1);
        }
        JobState::Died { pid } => eprintln!(
            "tdlib: the detached job (pid {pid}) ended without a marker (killed by its time limit, the OOM killer or a reboot?); its log: {}",
            entry.log().display()
        ),
        JobState::Done(status) if status != "0" => eprintln!(
            "tdlib: the last detached build failed ({status}); its log: {}",
            entry.log().display()
        ),
        JobState::Done(_) | JobState::Idle => {}
    }
    if !o.detach && !o.foreground {
        eprintln!(
            "tdlib: no cached build for key {key}. Start one outside srv's lock with `cargo xtask tdlib build --detach --fetch-gperf` (and finish with this command, the cache hit), or build here with --foreground"
        );
        return Ok(1);
    }
    // The entry is ours from here to the end of the job (the detached job inherits the descriptor).
    let lock = entry.open_lock()?;
    if !lock_for(&lock, LockKind::Exclusive, LOCK_SETTLE)? {
        eprintln!(
            "tdlib: another run holds the lock of key {key} (log: {})",
            entry.log().display()
        );
        return Ok(1);
    }
    // Inputs of the job: a copy of the pin and the patches, so a later sync cannot change them
    // under it, and a copy of this program, so a rebuild or prune of target/ cannot either.
    let job_inputs = entry.dir.join("inputs");
    if job_inputs.exists() {
        std::fs::remove_dir_all(&job_inputs)?;
    }
    std::fs::create_dir_all(job_inputs.join("patches"))?;
    std::fs::copy(root.join(PIN_FILE), job_inputs.join("tdlib.pin"))?;
    for p in &pin.patches {
        std::fs::copy(
            root.join(PATCH_DIR).join(p),
            job_inputs.join("patches").join(p),
        )?;
    }
    let mut args = vec![
        "tdlib".to_owned(),
        "build".to_owned(),
        "--jobs".to_owned(),
        o.jobs.to_string(),
        "--job".to_owned(),
        job_inputs.display().to_string(),
    ];
    if o.fetch_gperf {
        args.push("--fetch-gperf".to_owned());
    }
    if o.system_gperf {
        args.push("--system-gperf".to_owned());
    }
    if o.unpatched {
        args.push("--unpatched".to_owned());
    }
    let _ = std::fs::remove_file(entry.done());
    let _ = std::fs::remove_file(entry.pid());
    if o.detach {
        let exe = entry.dir.join("xtask-job");
        std::fs::copy(std::env::current_exe()?, &exe)?;
        // setsid + nohup: the job survives the end of the srv run that started it (probed
        // 2026-10-25); nice 10 and a time limit of its own. The lock descriptor goes on as fd
        // LOCK_FD (dup2 clears close-on-exec), so the lock lives as long as the job does.
        use std::os::fd::AsRawFd;
        let lock_fd = lock.as_raw_fd();
        let mut start = Command::new("sh");
        start
            .arg("-c")
            .arg(
                "cd / && setsid nohup nice -n 10 timeout \"$JOB_TIMEOUT\" \"$@\" > \"$JOB_LOG\" 2>&1 < /dev/null & echo $!",
            )
            .arg("sh")
            .arg(&exe)
            .args(&args)
            .env("JOB_LOG", entry.log())
            .env("JOB_TIMEOUT", JOB_TIMEOUT_S.to_string())
            .stdin(Stdio::null());
        // SAFETY: only async-signal-safe calls (dup2, fcntl) between fork and exec.
        unsafe {
            start.pre_exec(move || {
                if lock_fd == LOCK_FD {
                    let flags = libc::fcntl(lock_fd, libc::F_GETFD);
                    if flags < 0
                        || libc::fcntl(lock_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                } else if libc::dup2(lock_fd, LOCK_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let out = start.output().context("start the detached build")?;
        let pid = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        eprintln!(
            "tdlib: detached build started (pid {pid}); log {}, marker {} (holds the exit status when it ends)",
            entry.log().display(),
            entry.done().display()
        );
        return Ok(0);
    }
    // In the foreground: the same job, in this process, under the lock taken above.
    let status = job(o, &job_inputs)?;
    if status != 0 {
        return Ok(status);
    }
    let dest = target_copy(o.unpatched);
    copy_out(&entry.out(), &dest)?;
    eprintln!("tdlib: copied to {}", dest.display());
    drop(lock);
    Ok(0)
}

/// Copies `out/` (library, manifest, notices) to `dest`, replacing what was there.
fn copy_out(out: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    output(Command::new("cp").arg("-r").arg(out).arg(dest))?;
    Ok(())
}

/// The fetch, verify, patch, configure, build, strip, check and manifest of one cache entry.
/// Writes `job.pid` at its start and `job.done` (the status) at its end.
fn job(o: &Opts, inputs: &Path) -> Result<i32> {
    let pin = Pin::parse(&std::fs::read_to_string(inputs.join("tdlib.pin"))?)?;
    let patch_dir = inputs.join("patches");
    let dir = inputs.parent().context("job inputs")?.to_path_buf();
    let entry = Entry { dir };
    std::fs::write(entry.pid(), format!("{}\n", std::process::id()))?;
    let result = job_steps(o, &pin, &patch_dir, &entry);
    let status = match &result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("tdlib: the build failed: {e:?}");
            1
        }
    };
    std::fs::write(entry.done(), format!("{status}\n"))?;
    Ok(status)
}

fn job_steps(o: &Opts, pin: &Pin, patch_dir: &Path, entry: &Entry) -> Result<()> {
    let t0 = Instant::now();
    let (gperf, gperf_deb) = find_gperf(o.fetch_gperf, o.system_gperf, pin)?;
    let inputs = key_inputs(pin, patch_dir, o.unpatched, &gperf)?;
    let key = inputs.key()?;
    if entry.dir.file_name().and_then(|n| n.to_str()) != Some(key.as_str()) {
        bail!("the job's directory is not its key {key}: the inputs changed");
    }
    // Fetch BY HASH into a fresh private copy (the build writes generated sources into it).
    let src = entry.src();
    if src.exists() {
        std::fs::remove_dir_all(&src)?;
    }
    std::fs::create_dir_all(&src)?;
    let git = |args: &[&str]| output(git_command(&src).args(args));
    git(&["init", "-q", "--template="])?;
    git(&["fetch", "-q", "--depth", "1", &pin.repo, &pin.commit])?;
    git(&["checkout", "-q", "--detach", "FETCH_HEAD"])?;
    let fetched = t0.elapsed().as_secs_f64();
    let head = git(&["rev-parse", "HEAD"])?;
    if head.trim() != pin.commit {
        bail!("fetched {} instead of the pinned commit", head.trim());
    }
    let tree = git(&["rev-parse", "HEAD^{tree}"])?;
    if tree.trim() != pin.tree {
        bail!("the tree id differs from the pin");
    }
    let ls = git(&["ls-tree", "-r", "HEAD"])?;
    if sha256_bytes(ls.as_bytes())? != pin.ls_tree_sha256 {
        bail!("the sha256 over `git ls-tree -r HEAD` differs from the pin");
    }
    // The three checks above describe the commit; this one the files on disk (a filter or an
    // autocrlf of some config would show here, not there).
    ensure_clean_tree(&git(&[
        "status",
        "--porcelain",
        "--untracked-files=all",
        "--ignored",
    ])?)?;
    let notices = entry.out().join("notices");
    if entry.out().exists() {
        std::fs::remove_dir_all(entry.out())?;
    }
    std::fs::create_dir_all(&notices)?;
    for (path, sum) in &pin.licenses {
        let file = src.join(path);
        if sha256_file(&file)? != *sum {
            bail!("the licence file {path} differs from the pin");
        }
        let name = path.replace('/', "_");
        std::fs::copy(&file, notices.join(name))?;
    }
    for (name, _) in &inputs.patches {
        let patch = patch_dir.join(name);
        git(&["apply", "--check", &patch.display().to_string()])?;
        git(&["apply", &patch.display().to_string()])?;
        eprintln!("tdlib: applied {name}");
    }

    // Configure and build (Unix Makefiles; ccache off: TDLib's CMake uses it when it finds it).
    let build = entry.build();
    let t1 = Instant::now();
    let status = Command::new("cmake")
        .arg("-S")
        .arg(&src)
        .arg("-B")
        .arg(&build)
        .args(pin.cmake_flags())
        .arg(format!("-DGPERF_EXECUTABLE={}", gperf.display()))
        .env("CCACHE_DISABLE", "1")
        .stdin(Stdio::null())
        .without_build_flags()
        .status()
        .context("cmake configure")?;
    if !status.success() {
        bail!("cmake configure failed ({status})");
    }
    let configured = t1.elapsed().as_secs_f64();
    let t2 = Instant::now();
    let status = Command::new("cmake")
        .arg("--build")
        .arg(&build)
        .args(["--target", "tdjson", "-j", &o.jobs.to_string()])
        .env("CCACHE_DISABLE", "1")
        .stdin(Stdio::null())
        .without_build_flags()
        .status()
        .context("cmake build")?;
    if !status.success() {
        bail!("cmake build failed ({status})");
    }
    let built = t2.elapsed().as_secs_f64();
    // The tree that was built holds exactly the patches (or, unpatched, none of them).
    verify_patches_in_tree(&src, patch_dir, pin, o.unpatched)?;
    let unstripped = build.join(&pin.soname);
    let lib = entry.lib(pin);
    output(
        Command::new("strip")
            .arg("--strip-all")
            .arg("-o")
            .arg(&lib)
            .arg(&unstripped),
    )?;
    let r = check_file(pin, &lib, None, &[])?;
    report(&r, &lib)?;
    let sum = sha256_file(&lib)?;
    let size = std::fs::metadata(&lib)?.len();
    let unstripped_size = std::fs::metadata(&unstripped)?.len();
    write_manifest(
        &entry.manifest(),
        &key,
        &inputs,
        &sum,
        size,
        unstripped_size,
        gperf_deb.as_deref(),
        [fetched, configured, built, t0.elapsed().as_secs_f64()],
    )?;
    eprintln!(
        "tdlib: built {} ({size} bytes stripped) in {:.0} s (fetch {fetched:.0} s, configure {configured:.0} s, build {built:.0} s)",
        lib.display(),
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_manifest(
    path: &Path,
    key: &str,
    inputs: &KeyInputs,
    sha256: &str,
    size: u64,
    unstripped: u64,
    gperf_deb: Option<&str>,
    [fetch, configure, build, total]: [f64; 4],
) -> Result<()> {
    let mut t = String::new();
    t.push_str("# Written by `cargo xtask tdlib build`.\n");
    t.push_str(&format!("key = \"{key}\"\n"));
    t.push_str(&format!("sha256 = \"{sha256}\"\n"));
    t.push_str(&format!("size = {size}\n"));
    t.push_str(&format!("size_unstripped = {unstripped}\n"));
    t.push_str(&format!(
        "gperf_deb_sha256 = \"{}\"\n",
        gperf_deb.unwrap_or("none (gperf from PATH)")
    ));
    t.push_str(&format!(
        "seconds = \"fetch {fetch:.0}, configure {configure:.0}, build {build:.0}, total {total:.0}\"\n"
    ));
    t.push_str("# The key's inputs:\n");
    for l in inputs.text().lines() {
        let (k, v) = l.split_once(" = ").unwrap_or((l, ""));
        t.push_str(&format!("{k} = \"{v}\"\n"));
    }
    std::fs::write(path, t)?;
    Ok(())
}

fn check(o: &Opts) -> Result<i32> {
    let root = crate::root();
    let pin = Pin::load(&root)?;
    let (lib, manifest) = match &o.lib {
        Some(lib) => (lib.clone(), None),
        None => {
            let dir = target_copy(o.unpatched);
            (dir.join(&pin.soname), Some(dir.join("manifest.toml")))
        }
    };
    let patches = pin_patches(&pin, &root.join(PATCH_DIR), o.unpatched)?;
    report(
        &check_file(&pin, &lib, manifest.as_deref(), &patches)?,
        &lib,
    )?;
    Ok(0)
}

fn path(o: &Opts) -> Result<i32> {
    let root = crate::root();
    let pin = Pin::load(&root)?;
    let dir = target_copy(o.unpatched);
    let lib = dir.join(&pin.soname);
    if !lib.is_file() {
        eprintln!(
            "tdlib: no library at {} (run `cargo xtask tdlib build`)",
            lib.display()
        );
        return Ok(1);
    }
    let patches = pin_patches(&pin, &root.join(PATCH_DIR), o.unpatched)?;
    let r = check_file(&pin, &lib, Some(&dir.join("manifest.toml")), &patches)?;
    if !r.problems.is_empty() {
        for p in &r.problems {
            eprintln!("tdlib: check failed: {p}");
        }
        return Ok(1);
    }
    println!("{}", lib.display());
    Ok(0)
}

/// The pinned, patched library under `target/tdlib/`, checked as `path` checks it (TR R0a, the run
/// tool's preflight; critic 24): the pin's ELF rules, the genuine-keys row and the manifest's sha256,
/// commit and `patch` lines, which are read from the PIN (`tdlib.pin`'s patch list with each file's
/// sha256), never a hard-coded `0001 + 0002`. Returns the library's path and its sha256, the sum of
/// the very bytes the checks read (and the manifest's sha256 was compared with), so the tool can tell
/// later whether the library it stages is the one that was checked (TR fix round 2, H-3); or says why
/// not, with the fetch line for a library that is not on this machine.
pub(crate) fn checked_library(root: &Path) -> Result<(PathBuf, String)> {
    checked_library_in(root, &target_copy(false))
}

/// [`checked_library`] over the directory `dir` that holds the library and its `manifest.toml`.
pub(crate) fn checked_library_in(root: &Path, dir: &Path) -> Result<(PathBuf, String)> {
    let pin = Pin::load(root)?;
    let lib = dir.join(&pin.soname);
    if !lib.is_file() {
        bail!(
            "no TDLib library is on this machine: the build runs on the server, so fetch it with `srv -f target/tdlib 'kcap -t 900 cargo xtask tdlib build'` from the checkout"
        );
    }
    let patches = pin_patches(&pin, &root.join(PATCH_DIR), false)?;
    // `nm` or `readelf` failing on a file that is no library names its path: say it in fixed words.
    let (r, bytes) = check_file_read(&pin, &lib, Some(&dir.join("manifest.toml")), &patches)
        .map_err(|_| {
            anyhow::anyhow!("the TDLib library could not be examined (nm or readelf failed on it)")
        })?;
    if !r.problems.is_empty() {
        bail!(
            "the TDLib library fails {} check(s): {}",
            r.problems.len(),
            r.problems.join("; ")
        );
    }
    // The manifest named this sum and the bytes hashed to it, or there would be a problem above.
    let sum = sha256_bytes(&bytes)?;
    Ok((lib, sum))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin_text() -> String {
        std::fs::read_to_string(crate::root().join(PIN_FILE)).expect("pin file")
    }

    #[test]
    fn the_pin_file_parses_and_every_field_is_required() {
        let text = pin_text();
        let pin = Pin::parse(&text).expect("the committed pin parses");
        assert_eq!(pin.commit.len(), 40);
        assert_eq!(pin.exports.len(), 14);
        assert_eq!(pin.needed.len(), 7);
        assert_eq!(pin.gperf_deb_sha256.len(), 64);
        assert_eq!(
            pin.patches,
            [
                "0001-no-config-fallback.patch",
                "0002-no-incoming-calls.patch"
            ]
        );
        for p in &pin.patches {
            assert!(crate::root().join(PATCH_DIR).join(p).is_file(), "{p}");
        }
        for field in PIN_FIELDS {
            let without: String = text
                .lines()
                .filter(|l| !l.starts_with(&format!("{field} =")))
                .map(|l| format!("{l}\n"))
                .collect();
            assert_ne!(without, text, "{field} is not in the pin file");
            let err = Pin::parse(&without).expect_err(field);
            assert!(
                format!("{err:#}").contains(field),
                "{field}: the error names another field: {err:#}"
            );
        }
        // A wrong shape is refused too.
        // The needles come from the parsed pin, never a literal: a literal commit stops matching at the
        // next bump, `replace` then changes nothing, `bad == text` parses and the assert fails (LC-20).
        let full = format!("commit = \"{}\"", pin.commit);
        assert!(
            text.contains(&full),
            "the pin file does not hold its own commit line"
        );
        let bad = text.replace(&full, &format!("commit = \"{}\"", &pin.commit[..7]));
        assert_ne!(bad, text, "the short commit was not put in");
        assert!(Pin::parse(&bad).is_err(), "a short commit was taken");
        let twice = format!("{text}version = \"{}\"\n", pin.version);
        assert!(Pin::parse(&twice).is_err(), "a field given twice was taken");
    }

    fn sample_nm(names: &[&str]) -> String {
        let mut s = String::from("                 w __cxa_finalize\n0000000000001000 T _init\n");
        for n in names {
            s.push_str(&format!("00000000000a1b2c T {n}\n"));
        }
        s
    }

    fn sample_readelf(needed: &[&str], soname: &str, runpath: bool) -> String {
        let mut s = String::from(
            "\nDynamic section at offset 0x16ea2c8 contains 30 entries:\n  Tag        Type                         Name/Value\n",
        );
        for n in needed {
            s.push_str(&format!(
                " 0x0000000000000001 (NEEDED)             Shared library: [{n}]\n"
            ));
        }
        s.push_str(&format!(
            " 0x000000000000000e (SONAME)             Library soname: [{soname}]\n"
        ));
        if runpath {
            s.push_str(" 0x000000000000001d (RUNPATH)            Library runpath: [$ORIGIN]\n");
        }
        s.push_str(" 0x000000000000000c (INIT)               0x2f000\n");
        s
    }

    #[test]
    fn the_export_and_needed_checks_want_set_equality() {
        let pin = Pin::parse(&pin_text()).unwrap();
        let exports: Vec<&str> = pin.exports.iter().map(String::as_str).collect();
        let needed: Vec<&str> = pin.needed.iter().map(String::as_str).collect();
        let size = 24_810_792;
        let good = check_elf(
            &pin,
            &sample_nm(&exports),
            &sample_readelf(&needed, &pin.soname, false),
            size,
        );
        assert!(good.problems.is_empty(), "{:?}", good.problems);
        let refused = |nm: &str, readelf: &str, size: u64, why: &str| {
            let r = check_elf(&pin, nm, readelf, size);
            assert!(!r.problems.is_empty(), "{why}: accepted");
        };
        let elf = sample_readelf(&needed, &pin.soname, false);
        refused(&sample_nm(&exports[..13]), &elf, size, "13 exports");
        let mut more = exports.clone();
        more.push("td_extra");
        refused(&sample_nm(&more), &elf, size, "15 exports");
        let mut renamed = exports.clone();
        renamed[3] = "td_execute2";
        refused(&sample_nm(&renamed), &elf, size, "a renamed export");
        let nm = sample_nm(&exports);
        let no_z: Vec<&str> = needed
            .iter()
            .copied()
            .filter(|n| *n != "libz.so.1")
            .collect();
        refused(
            &nm,
            &sample_readelf(&no_z, &pin.soname, false),
            size,
            "NEEDED without libz",
        );
        let mut crypt = needed.clone();
        crypt.push("libcrypt.so.1");
        refused(
            &nm,
            &sample_readelf(&crypt, &pin.soname, false),
            size,
            "NEEDED with libcrypt",
        );
        refused(
            &nm,
            &sample_readelf(&needed, &pin.soname, true),
            size,
            "a RUNPATH",
        );
        refused(
            &nm,
            &sample_readelf(&needed, "libtdjson.so", false),
            size,
            "another SONAME",
        );
        refused(&nm, &elf, 19 * 1024 * 1024, "too small");
        refused(&nm, &elf, 29 * 1024 * 1024, "too large");
    }

    fn inputs() -> KeyInputs {
        KeyInputs {
            commit: "a".repeat(40),
            tree: "b".repeat(40),
            ls_tree_sha256: "c".repeat(64),
            build_type: "MinSizeRel".to_owned(),
            cmake_flags: vec!["-DX=1".to_owned()],
            patches: vec![("0001.patch".to_owned(), "d".repeat(64))],
            gcc: "gcc (Ubuntu 13.3.0-6ubuntu2~24.04) 13.3.0".to_owned(),
            gxx: "g++ (Ubuntu 13.3.0-6ubuntu2~24.04) 13.3.0".to_owned(),
            ld: "GNU ld (GNU Binutils for Ubuntu) 2.42".to_owned(),
            cmake: "cmake version 3.30.5".to_owned(),
            gperf: "GNU gperf 3.1".to_owned(),
            openssl: "system".to_owned(),
            dev_packages: "libssl-dev 3.0.13-0ubuntu3; zlib1g-dev 1:1.3.dfsg-3.1".to_owned(),
        }
    }

    /// B1 (patch 0002): the manifest's patch list must be the pin's, in order, with the current
    /// sha256 of each file. A library built before 0002 (or from an older 0002) fails.
    #[test]
    fn the_manifest_must_list_exactly_the_pins_patches() {
        let p = |n: &str, c: char| (n.to_owned(), c.to_string().repeat(64));
        let both = [p("0001-a.patch", 'a'), p("0002-b.patch", 'b')];
        let manifest = |lines: &[&str]| {
            let mut t = "key = \"k\"\ncommit = \"c\"\n".to_owned();
            for l in lines {
                t.push_str(&format!("patch = \"{l}\"\n"));
            }
            t.push_str("gcc = \"g\"\n");
            t
        };
        let a = format!("0001-a.patch {}", "a".repeat(64));
        let b = format!("0002-b.patch {}", "b".repeat(64));
        assert_eq!(manifest_patch_problem(&manifest(&[&a, &b]), &both), None);
        // Built before 0002 joined the pin.
        let only_first = manifest_patch_problem(&manifest(&[&a]), &both).expect("0001 only");
        assert!(only_first.contains("other patches"), "{only_first}");
        // Another order, another sha256 of 0002, a patch too many, unpatched.
        for lines in [
            vec![b.as_str(), a.as_str()],
            vec![a.as_str(), &format!("0002-b.patch {}", "c".repeat(64))],
            vec![a.as_str(), b.as_str(), a.as_str()],
            vec!["none"],
            vec![],
        ] {
            assert!(
                manifest_patch_problem(&manifest(&lines), &both).is_some(),
                "{lines:?} was taken"
            );
        }
        // The unpatched library's manifest says `none`, and only that passes for `--unpatched`.
        assert_eq!(manifest_patch_problem(&manifest(&["none"]), &[]), None);
        assert!(manifest_patch_problem(&manifest(&[&a]), &[]).is_some());
        // The manifest that `write_manifest` writes from the key's inputs lists the patches so.
        let inputs = inputs();
        let dir = TempDir::new("manifest-patches");
        let path = dir.0.join("manifest.toml");
        write_manifest(&path, "k", &inputs, "s", 1, 2, None, [0.0; 4]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(manifest_patch_problem(&text, &inputs.patches), None);
        assert!(manifest_patch_problem(&text, &both).is_some());
    }

    #[test]
    fn the_cache_key_changes_with_every_input() {
        let base = inputs();
        let key = base.key().unwrap();
        assert_eq!(key.len(), 16);
        assert_eq!(key, inputs().key().unwrap(), "the key is stable");
        type Change = (&'static str, fn(&mut KeyInputs));
        let changes: [Change; 15] = [
            ("commit", |i| i.commit = "e".repeat(40)),
            ("tree", |i| i.tree = "e".repeat(40)),
            ("ls_tree_sha256", |i| i.ls_tree_sha256 = "e".repeat(64)),
            ("build_type", |i| i.build_type = "Release".to_owned()),
            ("cmake_flags", |i| i.cmake_flags.push("-DY=2".to_owned())),
            ("patch content", |i| i.patches[0].1 = "e".repeat(64)),
            ("unpatched", |i| i.patches.clear()),
            ("gcc", |i| i.gcc = "gcc 14.2.0".to_owned()),
            ("g++", |i| i.gxx = "g++ 14.2.0".to_owned()),
            ("ld", |i| i.ld = "GNU ld 2.43".to_owned()),
            ("cmake", |i| i.cmake = "cmake version 4.4.4".to_owned()),
            ("gperf", |i| i.gperf = "GNU gperf 3.2".to_owned()),
            ("openssl", |i| i.openssl = "static".to_owned()),
            ("libssl-dev", |i| {
                i.dev_packages = i.dev_packages.replace("3.0.13", "3.0.14")
            }),
            ("zlib1g-dev", |i| {
                i.dev_packages = i.dev_packages.replace("1.3.dfsg", "1.3.dfsg+1")
            }),
        ];
        for (what, change) in changes {
            let mut i = inputs();
            change(&mut i);
            assert_ne!(i.key().unwrap(), key, "the key ignores {what}");
        }
    }

    #[test]
    fn the_dev_packages_come_from_dpkg_query_in_a_fixed_order() {
        let dpkg = "zlib1g-dev 1:1.3.dfsg-3.1ubuntu2\nlibssl-dev 3.0.13-0ubuntu3.5\n";
        assert_eq!(
            dev_packages_line(dpkg).unwrap(),
            "libssl-dev 3.0.13-0ubuntu3.5; zlib1g-dev 1:1.3.dfsg-3.1ubuntu2"
        );
        let err = dev_packages_line("libssl-dev 3.0.13\n").unwrap_err();
        assert!(format!("{err}").contains("zlib1g-dev"), "{err}");
        assert!(dev_packages_line("libssl-dev\nzlib1g-dev 1\n").is_err());
        assert!(dev_packages_line("libssl-dev 3 extra\nzlib1g-dev 1\n").is_err());
        // The key text carries the line, so the manifest names the versions it was built with.
        assert!(inputs().text().contains("dev_packages = libssl-dev"));
    }

    #[test]
    fn a_gperf_package_other_than_the_pinned_one_is_refused() {
        let pin = Pin::parse(&pin_text()).unwrap();
        check_deb_sum(&pin.gperf_deb_sha256, &pin).expect("the pinned package");
        let err = check_deb_sum(&"0".repeat(64), &pin).expect_err("another package");
        assert!(format!("{err}").contains("gperf_deb_sha256"), "{err}");
        assert!(check_deb_sum("", &pin).is_err());
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("xtask-tdlib-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_markers_and_the_lock_tell_what_became_of_a_job() {
        let tmp = TempDir::new("states");
        let entry = Entry {
            dir: tmp.0.join("k"),
        };
        assert_eq!(job_state(&entry).unwrap(), JobState::Idle);
        // A pid but no marker and nobody holding the lock: the job died.
        std::fs::write(entry.pid(), "4242\n").unwrap();
        assert_eq!(
            job_state(&entry).unwrap(),
            JobState::Died {
                pid: "4242".to_owned()
            }
        );
        // The same files while a holder has the lock: it runs.
        let holder = entry.open_lock().unwrap();
        assert!(lock_for(&holder, LockKind::Exclusive, Duration::ZERO).unwrap());
        assert_eq!(
            job_state(&entry).unwrap(),
            JobState::Running {
                pid: Some("4242".to_owned())
            }
        );
        // A marker written while it still runs does not make it done.
        std::fs::write(entry.done(), "0\n").unwrap();
        assert!(matches!(
            job_state(&entry).unwrap(),
            JobState::Running { .. }
        ));
        drop(holder);
        assert_eq!(job_state(&entry).unwrap(), JobState::Done("0".to_owned()));
        std::fs::write(entry.done(), "1\n").unwrap();
        assert_eq!(job_state(&entry).unwrap(), JobState::Done("1".to_owned()));
        // A recycled pid cannot make a finished or dead entry look alive: only the lock does.
        std::fs::remove_file(entry.done()).unwrap();
        std::fs::write(entry.pid(), format!("{}\n", std::process::id())).unwrap();
        assert!(matches!(job_state(&entry).unwrap(), JobState::Died { .. }));
    }

    #[test]
    fn one_cache_entry_is_locked_for_one_writer_and_many_readers() {
        let tmp = TempDir::new("lock");
        let entry = Entry {
            dir: tmp.0.join("k"),
        };
        let writer = entry.open_lock().unwrap();
        let other = entry.open_lock().unwrap();
        assert!(lock_for(&writer, LockKind::Exclusive, Duration::ZERO).unwrap());
        assert!(!lock_for(&other, LockKind::Exclusive, Duration::from_millis(30)).unwrap());
        assert!(!lock_for(&other, LockKind::Shared, Duration::from_millis(30)).unwrap());
        drop(writer);
        assert!(lock_for(&other, LockKind::Shared, Duration::ZERO).unwrap());
        let third = entry.open_lock().unwrap();
        assert!(lock_for(&third, LockKind::Shared, Duration::ZERO).unwrap());
        assert!(!lock_for(&third, LockKind::Exclusive, Duration::from_millis(30)).unwrap());
    }

    #[test]
    fn every_git_call_ignores_the_builders_config_and_repository_variables() {
        let c = git_command(Path::new("/some/src"));
        let envs: std::collections::BTreeMap<_, _> = c
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(envs["GIT_CONFIG_GLOBAL"].as_deref(), Some("/dev/null"));
        assert_eq!(envs["GIT_CONFIG_NOSYSTEM"].as_deref(), Some("1"));
        for var in GIT_ENV_REMOVED {
            assert_eq!(
                envs[var], None,
                "{var} must be removed from the environment"
            );
        }
        let mut b = Command::new("cmake");
        b.without_build_flags();
        let envs: std::collections::BTreeMap<_, _> = b
            .get_envs()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.is_none()))
            .collect();
        for var in BUILD_ENV_REMOVED {
            assert_eq!(
                envs.get(var),
                Some(&true),
                "{var} must be removed from the build"
            );
        }
    }

    /// N4: the search paths and every `CMAKE_*` variable leave the build's environment; other
    /// variables (PATH, HOME, a name that only starts with CMAKE) stay.
    #[test]
    fn the_build_environment_loses_search_paths_and_every_cmake_variable() {
        let have = [
            "PATH",
            "HOME",
            "CMAKE_PREFIX_PATH",
            "CMAKE_C_COMPILER_LAUNCHER",
            "CMAKE_TOOLCHAIN_FILE",
            "CMAKEX",
            "CPATH",
        ];
        let removed: Vec<String> = build_vars_removed(have.iter().map(Into::into))
            .into_iter()
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        for var in [
            "CPATH",
            "C_INCLUDE_PATH",
            "CPLUS_INCLUDE_PATH",
            "LIBRARY_PATH",
            "PKG_CONFIG_PATH",
            "CFLAGS",
            "CMAKE_PREFIX_PATH",
            "CMAKE_C_COMPILER_LAUNCHER",
            "CMAKE_TOOLCHAIN_FILE",
        ] {
            assert!(removed.iter().any(|r| r == var), "{var} stays in the build");
        }
        for var in ["PATH", "HOME", "CMAKEX"] {
            assert!(!removed.iter().any(|r| r == var), "{var} is removed");
        }
    }

    /// N4: a gperf on PATH is taken only with --system-gperf; the pinned package wins where it is
    /// unpacked; --fetch-gperf fetches it rather than taking PATH's.
    #[test]
    fn a_gperf_on_path_is_taken_only_when_asked_for() {
        use GperfSource::{Fetch, Path, Unpacked};
        // (unpacked, on PATH, --fetch-gperf, --system-gperf) -> source, or an error.
        let cases = [
            ((true, true, false, false), Some(Unpacked)),
            ((true, false, true, true), Some(Unpacked)),
            ((false, true, false, false), None),
            ((false, true, false, true), Some(Path)),
            ((false, true, true, false), Some(Fetch)),
            ((false, true, true, true), Some(Path)),
            ((false, false, true, false), Some(Fetch)),
            ((false, false, true, true), Some(Fetch)),
            ((false, false, false, true), None),
            ((false, false, false, false), None),
        ];
        for ((unpacked, on_path, fetch, system), want) in cases {
            let got = gperf_source(unpacked, on_path, fetch, system).ok();
            assert_eq!(got, want, "{unpacked} {on_path} {fetch} {system}");
        }
        let err = gperf_source(false, true, false, false).unwrap_err();
        assert!(format!("{err}").contains("--system-gperf"), "{err}");
    }

    /// N4: the PATH's gperf must be the pin's version, by its last word.
    #[test]
    fn a_gperf_of_another_version_is_refused() {
        let pin = Pin::parse(&pin_text()).expect("pin");
        let want = format!("GNU gperf {}", pin.gperf);
        check_gperf_version(&want, &pin).expect("the pinned version");
        for line in [
            "GNU gperf 3.2".to_owned(),
            format!("GNU gperf {}.1", pin.gperf),
            format!("GNU gperf {} extra", pin.gperf),
            String::new(),
        ] {
            assert!(
                check_gperf_version(&line, &pin).is_err(),
                "{line:?} was taken"
            );
        }
    }

    #[test]
    fn a_checkout_with_any_line_in_git_status_is_not_clean() {
        ensure_clean_tree("").expect("clean");
        for status in [
            " M td/telegram/ConfigManager.cpp\n",
            "?? stray.txt\n",
            "!! ignored.o\n",
        ] {
            assert!(
                ensure_clean_tree(status).is_err(),
                "{status:?} was taken as clean"
            );
        }
    }

    /// A real git repository with one file and a patch that changes it: the proof after the build
    /// wants the patch in the tree (reversible) when patched and out of it when unpatched.
    #[test]
    fn the_built_tree_must_hold_exactly_the_patches() {
        let tmp = TempDir::new("patches");
        let src = tmp.0.join("src");
        let patches = tmp.0.join("patches");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&patches).unwrap();
        let git = |args: &[&str]| output(git_command(&src).args(args)).expect("git");
        git(&["init", "-q", "--template="]);
        std::fs::write(src.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        git(&["add", "a.txt"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "x",
        ]);
        std::fs::write(
            patches.join("0001-x.patch"),
            "--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+2\n three\n",
        )
        .unwrap();
        let mut pin = Pin::parse(&pin_text()).unwrap();
        pin.patches = vec!["0001-x.patch".to_owned()];
        // Not applied: patched build is refused, unpatched build is fine.
        assert!(verify_patches_in_tree(&src, &patches, &pin, false).is_err());
        verify_patches_in_tree(&src, &patches, &pin, true).expect("unpatched tree");
        // Applied: the other way round.
        git(&["apply", &patches.join("0001-x.patch").display().to_string()]);
        verify_patches_in_tree(&src, &patches, &pin, false).expect("patched tree");
        assert!(verify_patches_in_tree(&src, &patches, &pin, true).is_err());
        // The checkout is clean before the patch, not after; ignored files count too.
        git(&["checkout", "-q", "--", "a.txt"]);
        ensure_clean_tree(&git(&[
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignored",
        ]))
        .expect("clean again");
        std::fs::write(src.join(".gitignore"), "").unwrap();
        assert!(
            ensure_clean_tree(&git(&[
                "status",
                "--porcelain",
                "--untracked-files=all",
                "--ignored"
            ]))
            .is_err()
        );
    }

    /// F5: the patch's header says what the full config does and what the patch costs; its bytes are
    /// a key input, so a change of the text is another cache entry (the key test above).
    #[test]
    fn the_patch_header_says_what_the_full_config_does_and_what_the_patch_costs() {
        let pin = Pin::parse(&pin_text()).unwrap();
        let text = std::fs::read_to_string(crate::root().join(PATCH_DIR).join(&pin.patches[0]))
            .expect("the patch");
        let (header, hunk) = text.split_once("\n--- a/").expect("a header and a hunk");
        for needed in [
            // The full config dials the stored data-centre addresses directly, past any proxy.
            "request_raw_connection_by_ip",
            "EMPTY proxy",
            "DIRECTLY",
            // And the price of turning it off.
            "What it costs",
            "unreachable or blocked",
            "help.getConfig",
        ] {
            assert!(header.contains(needed), "the patch header lacks `{needed}`");
        }
        assert!(
            !header.contains("from the data centres of that answer"),
            "the old, wrong description of the full config is back"
        );
        assert!(hunk.contains("bool has_connecting_problem = false;"));
    }

    #[test]
    fn a_manifest_value_is_read_back() {
        let text = "key = \"0123\"\nsha256 = \"abcd\"\nsize = 5\n";
        assert_eq!(manifest_value(text, "sha256").as_deref(), Some("abcd"));
        assert_eq!(manifest_value(text, "size").as_deref(), Some("5"));
        assert_eq!(manifest_value(text, "commit"), None);
    }
}

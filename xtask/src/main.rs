//! Developer tasks: `cargo xtask <ci|deps-check|symbols-check|shots|demo|smoke|wa-run|tdlib|perf|install|uninstall>`.
//!
//! `ci` is the local CI: every step must pass before a commit. `ci --quick [--since <rev>]` is
//! its tier for use while working: the unit tests of every package and the integration tests of
//! the packages a change reaches ([`quick`]). `deps-check` enforces the crate layering from
//! docs/architecture.md §2.2-2.3 and the front-end seam of §8.10 ([`seam`]). `symbols-check` fails when a binary exports
//! OpenSSL-named symbols (BoringSSL's), which Qt's libssl.so.3 could bind to (§2.4, §5.3).
//! `shots` renders every UI page in both sizes and color schemes, several at a time, and
//! compares the pictures with the stored ones ([`shots`]). `demo` runs the real app against Signal's mock server and photographs it ([`demo`]).
//! `smoke` runs the real app step by step against a Signal server on a profile that keeps its
//! login, and rehearses that against the mock ([`smoke`], `docs/smoke-run.md`).
//! `wa-run` is its twin for the WhatsApp backend: the real run step by step on a profile that keeps
//! its login, and a rehearsal against the fake playing WhatsApp ([`wa_run`], `docs/whatsapp-run.md`).
//! `tdlib` builds, checks and locates the pinned, patched TDLib the Telegram helper opens ([`tdlib`]).
//! `perf` measures the quality targets of §1.3 and prints them against their budgets
//! ([`perf`]). `install` and `uninstall` put the release build under a prefix in the XDG
//! layout and take it away again ([`install`]). `dist` makes the archives of a release on this
//! machine and checks that they are fit to be made public ([`dist`]); it publishes nothing.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

mod cleaned;
mod demo;
mod dist;
#[cfg(test)]
mod filter_model;
mod green;
mod i18n;
mod install;
mod lock;
mod manifest;
mod names;
mod perf;
mod prune;
mod quick;
mod runner;
mod seam;
mod shots;
mod slow;
mod smoke;
mod tdlib;
mod tg_run;
mod wa_run;
mod workspace;

fn main() {
    if let Err(err) = real_main() {
        // What `fn main() -> Result` prints; a run that did not wait for the test lock
        // (`--no-wait`) ends with status 75 and its own sentence.
        if err.downcast_ref::<lock::Busy>().is_some() {
            eprintln!("{err}");
        } else {
            eprintln!("Error: {err:?}");
        }
        std::process::exit(lock::exit_code(&err));
    }
}

fn real_main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("ci") => ci(&args[1..]),
        Some("lock") => lock::command(&args[1..]),
        Some("prune") => prune::command(&args[1..]),
        Some("deps-check") => deps_check(),
        Some("symbols-check") => symbols_check(args.get(1).map(PathBuf::from)),
        Some("shots") => shots::shots(&args[1..]),
        Some("demo") => demo::demo(&args[1..]),
        Some("perf") => {
            // 0, or the status of a missed budget; a failed measurement is an error (1).
            let status = perf::perf(&args[1..])?;
            if status != 0 {
                std::process::exit(status);
            }
            Ok(())
        }
        Some("smoke") => {
            // 0, or 1 when a step failed; a command that could not run is an error (1).
            let status = smoke::smoke(&args[1..])?;
            if status != 0 {
                std::process::exit(status);
            }
            Ok(())
        }
        Some("wa-run") => {
            // 0, 2 for the usage, or 1 when a step failed; a command that could not run is an error (1).
            let status = wa_run::wa_run(&args[1..])?;
            if status != 0 {
                std::process::exit(status);
            }
            Ok(())
        }
        Some("tg-run") => {
            // 0, 2 for the usage, or 1 when a check failed; a command that could not run is an error (1).
            let status = tg_run::tg_run(&args[1..])?;
            if status != 0 {
                std::process::exit(status);
            }
            Ok(())
        }
        Some("tdlib") => {
            // 0, 1 when a detached build still runs or a check failed, 2 for the usage.
            let status = tdlib::tdlib(&args[1..])?;
            if status != 0 {
                std::process::exit(status);
            }
            Ok(())
        }
        Some("i18n") => i18n::i18n(&args[1..]),
        Some("dist") => dist::dist(&args[1..]),
        Some("install") => install::install(&args[1..]),
        Some("uninstall") => install::uninstall(&args[1..]),
        _ => {
            eprintln!(
                "usage: cargo xtask <ci [--quick [--since <rev>]] [--keep-target] [--no-wait] [--force]|lock [--no-wait] -- <command>|prune [--dry-run]|deps-check|symbols-check [binary]|shots [dir] [--page a,b] [--theme light|dark] [--size wide|narrow] [--jobs N] [--no-build] [--accept] [--no-compare] [--no-wait] [--matrix [--variant a,b]]|demo [--out <dir>] [--interactive]|smoke <start|step|steps|state|report|stop|backup|wipe|rehearse> …|wa-run <start|tail|step|steps|state|report|stop|backup|wipe|rehearse> …|tg-run <connect|rehearse|wipe|ranges> …|perf [--out <file>] [--quick] [--report-only]|tdlib <build [--jobs N] [--fetch-gperf] [--detach|--foreground] [--unpatched]|check [--lib <path>] [--unpatched]|path [--unpatched]>|i18n <extract|check|build [--out <dir>]>|install (--prefix <dir>|--user)|uninstall (--prefix <dir>|--user)|dist [--out <dir>] [--release] [--allow-dirty] [--allow-paths] [--no-build]>"
            );
            std::process::exit(2);
        }
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn run(desc: &str, cmd: &mut Command) -> Result<()> {
    eprintln!("\n==> {desc}");
    // `ci` prunes target/debug at its end by the graph of the cargo commands it ran.
    prune::record(cmd);
    // What runs under the test lock says so (a nested `cargo xtask` does not wait for it).
    lock::mark_children(cmd);
    let status = cmd
        .current_dir(root())
        .status()
        .with_context(|| format!("spawn: {desc}"))?;
    if !status.success() {
        bail!("step failed: {desc} ({status})");
    }
    Ok(())
}

/// The Telegram API pair `kourier-telegram` reads with `option_env!` (`api.rs`): no build, test or
/// lint the xtask starts may compile one in from the caller's environment (TG-2 S-5). A release
/// build that is meant to carry a pair must set it on its own command after `cargo()`; none does
/// today.
const TELEGRAM_API_VARS: [&str; 2] = ["KOURIER_TG_API_ID", "KOURIER_TG_API_HASH"];

fn cargo() -> Command {
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    lock::mark_children(&mut cmd);
    for var in TELEGRAM_API_VARS {
        cmd.env_remove(var);
    }
    cmd
}

/// The colour scheme files the screenshots read (a part of the green-run digest, T5b).
fn scheme_files() -> Vec<PathBuf> {
    SHOT_THEMES
        .iter()
        .map(|(_, scheme, _)| color_scheme_file(scheme))
        .collect()
}

/// What a `ci` run knows about the tree it stands on when it starts (T5b, [`green`]): the
/// digest of the tree and its surroundings, and the records earlier green runs left.
pub struct Start {
    sys: Option<green::System>,
    pub digest: Option<green::Digest>,
    pub records: Vec<(green::Tier, green::Record)>,
    /// `--force`: run although a record covers this run.
    pub force: bool,
}

impl Start {
    /// Takes the digest (after the test lock: the tree must not change under a run that waited).
    /// A digest that cannot be taken is told and means no skip and no record, never a failure.
    fn take(force: bool) -> Start {
        let (sys, digest) = match green::System::query(&root(), &scheme_files()) {
            Ok(sys) => match green::digest(&root(), &sys) {
                Ok(digest) => (Some(sys), Some(digest)),
                Err(e) => {
                    eprintln!("ci: cannot take the digest of the tree ({e:#}): no skip, no record");
                    (Some(sys), None)
                }
            },
            Err(e) => {
                eprintln!("ci: cannot ask the machine for the digest ({e:#}): no skip, no record");
                (None, None)
            }
        };
        Start {
            sys,
            digest,
            records: green::load(&target_dir()),
            force,
        }
    }

    /// Whether a record of this very tree covers `want`; says so (the skip), or says which parts
    /// of the tree moved since the last green run. `--force` never skips.
    pub fn covered(&self, want: &green::Ran) -> bool {
        let Some(digest) = &self.digest else {
            return false;
        };
        if self.force {
            eprintln!("\nci: --force: running although a green run may be on record");
            return false;
        }
        match green::covering(&self.records, digest, want) {
            Some(skip) => {
                eprintln!("\n{}", green::skip_message(&skip));
                true
            }
            None => {
                if let Some(why) = green::why_not_same(&self.records, digest) {
                    eprintln!("\n{why}");
                }
                false
            }
        }
    }

    /// Writes the record of a green run, unless the tree moved while it ran. Never fails the run.
    fn record(&self, tier: green::Tier, ran: green::Ran, engine: runner::Engine) {
        let (Some(before), Some(sys)) = (&self.digest, &self.sys) else {
            return;
        };
        let Ok(after) = green::digest(&root(), sys) else {
            return;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        match green::record_green(
            &target_dir(),
            tier,
            before,
            &after,
            runner::engine_name(engine),
            ran,
            now,
        ) {
            Ok(Some(_)) => eprintln!(
                "ci: this tree is on record as green ({}, digest {}): a repeat on it ends at once",
                tier.name(),
                after.hex()
            ),
            Ok(None) => eprintln!("ci: the tree changed while the run went on: no record written"),
            Err(e) => {
                eprintln!("ci: could not write the record ({e:#}): the next run will not skip")
            }
        }
    }
}

/// How a `ci` run that did not fail ended.
pub enum Outcome {
    /// A record of this very tree covered it (T5b): nothing ran.
    Skipped,
    /// It ran green: what it covered, for the record.
    Ran {
        tier: green::Tier,
        ran: green::Ran,
        engine: runner::Engine,
    },
}

/// `ci`: everything, or with `--quick` the tier for use while working ([`quick`]).
fn ci(args: &[String]) -> Result<()> {
    let args = quick::parse_args(args)?;
    // One test run at a time on this PC, both checkouts: held to the end, the prune included.
    lock::take(args.no_wait)?;
    let start = Start::take(args.force);
    let outcome = if args.quick {
        quick::run(args.since.as_deref(), &start)
    } else {
        ci_full(&start)
    };
    match outcome {
        Err(err) => {
            // The tree is no longer known green.
            if let Some(digest) = &start.digest {
                for tier in green::forget(&target_dir(), digest) {
                    eprintln!("ci: the {} record of this tree is removed", tier.name());
                }
            }
            Err(err)
        }
        // Nothing was built: nothing to prune.
        Ok(Outcome::Skipped) => Ok(()),
        Ok(Outcome::Ran { tier, ran, engine }) => {
            start.record(tier, ran, engine);
            // Only a green run gets here: a failed one may not have built the whole graph.
            prune::after_ci(args.keep_target);
            Ok(())
        }
    }
}

/// The lint step's clippy run. The prune replays exactly this (`prune::canonical_queries`):
/// clippy-driver tracks its lint arguments, so a replay with other ones would check the
/// workspace crates again and leave them fingerprinted for its own lints.
const CLIPPY_ARGS: [&str; 6] = [
    "clippy",
    "--workspace",
    "--all-targets",
    "--",
    "-D",
    "warnings",
];

/// The steps both tiers start with: rustfmt, clippy and deps-check.
fn lint_steps() -> Result<()> {
    run("rustfmt", cargo().args(["fmt", "--all", "--check"]))?;
    run("clippy", cargo().args(CLIPPY_ARGS))?;
    eprintln!("\n==> deps-check");
    deps_check()?;
    eprintln!("\n==> i18n check");
    i18n::i18n(&["check".to_owned()])
}

fn ci_full(start: &Start) -> Result<Outcome> {
    prune::mark_full();
    if start.covered(&green::Ran::everything()) {
        return Ok(Outcome::Skipped);
    }
    let engine = runner::engine();
    // After a green quick run on this very tree only what it did not cover runs (T5b).
    let remainder = match (&start.digest, start.force) {
        (Some(digest), false) => green::remainder(
            &start.records,
            digest,
            runner::engine_name(engine),
            &slow::integration_filter,
        ),
        _ => None,
    };
    match &remainder {
        Some(rest) => {
            eprintln!("\n{}", rest.message());
            runner::full_remainder(rest)?;
            if rest.qmllint {
                qmllint()?;
            }
        }
        None => {
            lint_steps()?;
            runner::full(engine)?;
            qmllint()?;
        }
    }
    shots::ci_step()?;
    eprintln!("\nci: all steps passed");
    Ok(Outcome::Ran {
        tier: green::Tier::Full,
        ran: green::Ran::everything(),
        engine,
    })
}

/// Allowed internal dependencies per crate (docs/architecture.md §2.2). Anything else is an error.
fn allowed_internal() -> BTreeMap<&'static str, &'static [&'static str]> {
    BTreeMap::from([
        ("kourier-model", &[][..]),
        ("kourier-backend-api", &["kourier-model"][..]),
        ("kourier-db", &[][..]),
        ("kourier-blob", &[][..]),
        ("kourier-media", &[][..]),
        (
            "kourier-core",
            &[
                "kourier-model",
                "kourier-backend-api",
                "kourier-db",
                "kourier-blob",
                "kourier-media",
            ][..],
        ),
        (
            "kourier-fake",
            &[
                "kourier-model",
                "kourier-backend-api",
                "kourier-db",
                // Synthetic attachments: blobs written into the core's targets (§10.2).
                "kourier-blob",
                "kourier-media",
            ][..],
        ),
        (
            "kourier-signal",
            &[
                "kourier-model",
                "kourier-backend-api",
                "kourier-db",
                "kourier-blob",
                "kourier-media",
            ][..],
        ),
        // The WhatsApp backend (architecture §7): from W5b (media send) the encrypted files and the pictures too.
        (
            "kourier-whatsapp",
            &[
                "kourier-model",
                "kourier-backend-api",
                "kourier-db",
                "kourier-blob",
                "kourier-media",
            ][..],
        ),
        // The Telegram backend (architecture §7.5) and its TDLib helper. The helper crate stands
        // alone (the frame protocol, the serve loop, the binary that dlopens libtdjson); only the
        // Telegram backend uses it.
        (
            "kourier-telegram",
            &[
                "kourier-model",
                "kourier-backend-api",
                "kourier-db",
                "kourier-tdlib",
            ][..],
        ),
        ("kourier-tdlib", &[][..]),
        // The Threema backend (architecture §7.4): the same edges as WhatsApp's.
        (
            "kourier-threema",
            &["kourier-model", "kourier-backend-api", "kourier-db"][..],
        ),
        ("kourier-desktop", &[][..]),
        (
            "kourier-app",
            &[
                "kourier-core",
                "kourier-backend-api",
                "kourier-model",
                "kourier-desktop",
                "kourier-blob",
                "kourier-signal",
                // Behind the cargo feature `whatsapp` (W2: the feature and the registration).
                "kourier-whatsapp",
                // Behind the cargo feature `telegram` (TG-1a: debug builds only, behind `networks-on`).
                "kourier-telegram",
                // Behind the cargo feature `threema` (TH1: debug builds only).
                "kourier-threema",
                "kourier-fake",
                // `CoreConfig::core_key` is a `kourier_db::DbKey` (only the key type is used).
                "kourier-db",
            ][..],
        ),
        ("kourier-viewmodel", &["kourier-core", "kourier-model"][..]),
        (
            "kourier-qt",
            &["kourier-app", "kourier-viewmodel", "kourier-model"][..],
        ),
        ("kourier", &["kourier-qt", "kourier-app"][..]),
        // Test-only: BoringSSL + SQLCipher + Qt TLS in one process (§5.3).
        ("kourier-coexist", &["kourier-db"][..]),
        // Test-only: the real core with real backends against local test servers (§10.1).
        (
            "kourier-e2e",
            &[
                "kourier-core",
                "kourier-signal",
                "kourier-fake",
                "kourier-db",
                "kourier-backend-api",
                "kourier-model",
                // Sample pictures for the attachment tests.
                "kourier-media",
            ][..],
        ),
        ("xtask", &[][..]),
    ])
}

/// External crates that only specific internal crates may use (prefix match on the crate name).
/// Toolkit crates (cxx-qt and the rest) are not here: [`seam::toolkit_links`] holds them for
/// every kind of dependency.
fn restricted_external() -> Vec<(&'static str, &'static [&'static str])> {
    vec![
        ("libsignal", &["kourier-signal", "kourier-coexist"][..]),
        ("zkgroup", &["kourier-signal"][..]),
        ("signal-", &["kourier-signal"][..]),
        // whatsapp-rust and its crates (one git revision, architecture §7): only the WhatsApp
        // backend, and the test-only crate that runs real backends against local servers. The
        // prefixes `wacore` and `waproto` also cover `wacore-binary`, `wacore-appstate` …;
        // `buffa` is the protobuf runtime `waproto` uses (it must not leak into other crates).
        ("whatsapp-rust", &["kourier-whatsapp", "kourier-e2e"][..]),
        ("wacore", &["kourier-whatsapp", "kourier-e2e"][..]),
        ("waproto", &["kourier-whatsapp", "kourier-e2e"][..]),
        ("buffa", &["kourier-whatsapp", "kourier-e2e"][..]),
        // libthreema, vendored (third_party/libthreema): only the Threema backend. The prefix also
        // covers `libthreema-macros`, `libthreema-dependencies` and `libthreema-blake2`.
        ("libthreema", &["kourier-threema"][..]),
        (
            "rusqlite",
            &[
                "kourier-db",
                "kourier-core",
                "kourier-signal",
                "kourier-whatsapp",
                "kourier-telegram",
                "kourier-fake",
            ][..],
        ),
        ("libsqlite3-sys", &["kourier-db"][..]),
        ("zbus", &["kourier-desktop"][..]),
        (
            "tokio",
            &[
                "kourier-backend-api",
                "kourier-core",
                "kourier-fake",
                "kourier-signal",
                "kourier-whatsapp",
                "kourier-telegram",
                "kourier-threema",
                "kourier-desktop",
                "kourier-app",
                "kourier-db",
                "kourier-blob",
                "kourier-qt",
                "kourier-coexist",
                "kourier-e2e",
            ][..],
        ),
    ]
}

fn deps_check() -> Result<()> {
    let out = cargo()
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(root())
        .output()
        .context("cargo metadata")?;
    if !out.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let meta: Value = serde_json::from_slice(&out.stdout)?;
    let errors = check_metadata(&meta)?;
    let mut errors = errors;
    errors.extend(check_facade_only()?);
    errors.extend(seam::check(&root(), &workspace::Workspace::load(&root())?)?);
    errors.extend(cleaned::check(&root())?);
    if errors.is_empty() {
        eprintln!("deps-check: layering OK");
        Ok(())
    } else {
        for e in &errors {
            eprintln!("deps-check: {e}");
        }
        bail!("{} layering violation(s)", errors.len())
    }
}

fn check_metadata(meta: &Value) -> Result<Vec<String>> {
    let allowed = allowed_internal();
    let restricted = restricted_external();
    let packages = meta["packages"].as_array().context("packages")?;
    let internal: BTreeSet<&str> = packages.iter().filter_map(|p| p["name"].as_str()).collect();
    let mut errors = Vec::new();
    for p in packages {
        let name = p["name"].as_str().unwrap_or_default();
        let Some(ok) = allowed.get(name) else {
            errors.push(format!(
                "{name}: not in the layering table (xtask/src/main.rs)"
            ));
            continue;
        };
        for d in p["dependencies"].as_array().into_iter().flatten() {
            // Build- and dev-dependencies of internal crates are checked too: tests must not
            // smuggle in forbidden edges either, except dev-dependencies on kourier-fake.
            let dep = d["name"].as_str().unwrap_or_default();
            let kind = d["kind"].as_str().unwrap_or("normal");
            if internal.contains(dep) {
                let dev_fake = kind == "dev" && dep == "kourier-fake";
                // A crate's dev-dependency on itself is no edge: it is cargo's way to
                // switch one of the crate's own features on for its own tests
                // (`kourier-signal`'s `testing`).
                let own_tests = kind == "dev" && dep == name;
                if !ok.contains(&dep) && !dev_fake && !own_tests {
                    errors.push(format!("{name} -> {dep} ({kind}) is forbidden"));
                }
            } else if kind != "build" {
                for (prefix, users) in &restricted {
                    if dep.starts_with(prefix) && !users.contains(&name) {
                        errors.push(format!(
                            "{name} -> {dep} ({kind}): only {users:?} may use it"
                        ));
                    }
                }
            }
        }
    }
    Ok(errors)
}

/// kourier-viewmodel and kourier-qt may only use `kourier_core::facade`.
fn check_facade_only() -> Result<Vec<String>> {
    let mut errors = Vec::new();
    for krate in ["kourier-viewmodel", "kourier-qt"] {
        let dir = root().join("crates").join(krate);
        for file in rust_files(&dir)? {
            let text = std::fs::read_to_string(&file)?;
            for (i, line) in text.lines().enumerate() {
                let mut rest = line;
                while let Some(pos) = rest.find("kourier_core::") {
                    let after = &rest[pos + "kourier_core::".len()..];
                    if !after.starts_with("facade") {
                        errors.push(format!(
                            "{}:{}: uses kourier_core internals (only kourier_core::facade is allowed)",
                            file.display(),
                            i + 1
                        ));
                    }
                    rest = after;
                }
            }
        }
    }
    Ok(errors)
}

/// Symbol prefixes BoringSSL shares with OpenSSL; none may be in a binary's dynamic symbol table.
const OPENSSL_PREFIXES: [&str; 3] = ["SSL_", "EVP_", "CRYPTO_"];

/// `nm -D --defined-only <bin>` must list no OpenSSL-named symbol (default bin:
/// target/release/kourier).
fn symbols_check(bin: Option<PathBuf>) -> Result<()> {
    let bin = bin.unwrap_or_else(|| root().join("target/release/kourier"));
    if !bin.is_file() {
        bail!(
            "{} not found; build it first (cargo build --release -p kourier)",
            bin.display()
        );
    }
    let out = Command::new("nm")
        .args(["-D", "--defined-only"])
        .arg(&bin)
        .output()
        .context("run nm")?;
    if !out.status.success() {
        bail!("nm failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let bad = exported_openssl_symbols(&String::from_utf8_lossy(&out.stdout));
    if bad.is_empty() {
        eprintln!(
            "symbols-check: {} exports no SSL_/EVP_/CRYPTO_ symbols",
            bin.display()
        );
        return Ok(());
    }
    for name in &bad {
        eprintln!("symbols-check: exported {name}");
    }
    bail!(
        "{} exports {} OpenSSL-named symbol(s); never link with -rdynamic",
        bin.display(),
        bad.len()
    )
}

/// Names in `nm -D --defined-only` output (`<address> <type> <name>[@[@]<version>]`) that start
/// with an OpenSSL prefix.
pub(crate) fn exported_openssl_symbols(nm_output: &str) -> Vec<String> {
    nm_output
        .lines()
        .filter_map(|line| line.split_whitespace().nth(2))
        .map(|name| name.split('@').next().unwrap_or(name))
        .filter(|name| OPENSSL_PREFIXES.iter().any(|p| name.starts_with(p)))
        .map(str::to_owned)
        .collect()
}

fn rust_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.file_name().is_some_and(|n| n == "target") {
            continue;
        }
        if path.is_dir() {
            out.extend(rust_files(&path)?);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(out)
}

fn qml_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            out.extend(qml_files(&path)?);
        } else if path.extension().is_some_and(|e| e == "qml") {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn qmllint() -> Result<()> {
    let files = qml_files(&root().join("crates/kourier-qt/qml"))?;
    let mut cmd = Command::new("/usr/lib/qt6/bin/qmllint");
    // cxx-qt writes the generated qmldir/qmltypes of our module below <target>/cxxqt.
    cmd.arg("-I").arg(target_dir().join("cxxqt/qml_modules"));
    cmd.args(&files);
    run("qmllint", &mut cmd)
}

/// Cargo's target directory: `$CARGO_TARGET_DIR` (relative to the workspace root) or `target/`.
fn target_dir() -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(dir) if !dir.is_empty() => {
            let dir = PathBuf::from(dir);
            if dir.is_absolute() {
                dir
            } else {
                root().join(dir)
            }
        }
        _ => root().join("target"),
    }
}

/// Pages, window sizes and color schemes of the screenshot matrix (§10.1). Every page renders
/// through the real core on a temporary profile: `--fixture-link` (no account yet, the fake
/// network's linking walk) for the first three, `--fixture` (the fake account's demo data in
/// `core.db`) for the rest. Every `--page` value is here (`PAGES` in `crates/kourier-qt/src/lib.rs`;
/// a test compares the two).
const SHOT_PAGES: [&str; 82] = [
    "welcome",
    "link",
    "qr",
    "list",
    "chat",
    "history",
    "settings",
    "drawer",
    "media",
    "viewer",
    "compose",
    "files",
    "album",
    "albumviewer",
    "revisions",
    "compact",
    "quotes",
    "menu",
    "react",
    "reactions",
    "edit",
    "reply",
    "delete",
    "timer",
    "placeholders",
    "offline",
    "loggedout",
    "challenge",
    "clock",
    "originals",
    "timercustom",
    "clearchat",
    "mediamenu",
    "deleteattachment",
    "folder",
    "folderfiles",
    "folderzip",
    "expiry",
    "readmore",
    "about",
    "calls",
    "unavailable",
    "transferwait",
    "transferdownload",
    "transferread",
    "transferupdate",
    "transferbanner",
    "transferfailed",
    "transferask",
    "networks",
    "warisk",
    "waqr",
    "waphone",
    "wacode",
    "thqr",
    "themoji",
    "tgwarn",
    "tgqr",
    "tgcode",
    "tgpassword",
    "wahistory",
    "waban",
    "walock",
    "waoff",
    "wafailed",
    "walost",
    "uploading",
    "uploadfile",
    "archive",
    "person",
    "group",
    "chatmedia",
    "safety",
    "safetychanged",
    "share",
    "timerpending",
    "save",
    "location",
    "locationmany",
    "locationnever",
    "accounts",
    "accountsdrawer",
];

/// Pages that look the same in both color schemes: the image viewer covers the window with a
/// dark overlay and a picture whatever the scheme, so its light rendering is not brighter.
const SHOT_PAGES_SAME_IN_BOTH_THEMES: [&str; 2] = ["viewer", "albumviewer"];

/// How much brighter (mean luminance, 0-255) the light rendering of a page must be than the dark
/// one for the color scheme to count as applied.
const SHOT_LUMA_GAP: u32 = 40;

/// Why a page's light and dark renderings do not look like two color schemes, if they do not.
fn theme_problem(page: &str, light: u32, dark: u32) -> Option<String> {
    if SHOT_PAGES_SAME_IN_BOTH_THEMES.contains(&page) || light > dark + SHOT_LUMA_GAP {
        return None;
    }
    Some(format!(
        "light luma {light} is not clearly above dark {dark}: the color scheme did not apply"
    ))
}

/// The fixture flag a page renders with.
fn shot_mode(page: &str) -> &'static str {
    match page {
        "welcome" | "link" | "qr" => "--fixture-link",
        _ => "--fixture",
    }
}

const SHOT_SIZES: [(&str, u32, u32); 2] = [("wide", 1200, 800), ("narrow", 420, 800)];
const SHOT_THEMES: [(&str, &str, &str); 2] = [
    ("light", "BreezeLight", "breeze"),
    ("dark", "BreezeDark", "breeze-dark"),
];

/// The file of a KDE color scheme.
fn color_scheme_file(scheme: &str) -> PathBuf {
    Path::new("/usr/share/color-schemes").join(format!("{scheme}.colors"))
}

/// The `[KDE]` line that stops the style's animations: `org.kde.desktop` reads Plasma's
/// "animation speed" from it (0 = instant), and every duration of `Kirigami.Units` (the fade of
/// a menu, a dialog, a tip and of the scroll bar, the slide of a page) becomes 0 to 2 ms
/// (measured: short 0, long 1, very long 2, against 100, 200 and 400 ms). A picture for
/// comparison is taken when nothing moves (`shotTimer`, two equal frames 50 ms apart); a
/// transition that waits before it starts (QTBUG-106118: a `Binding` with `delayed: true`
/// ahead of a fade) fits between those two frames, so it is removed at its source.
const STILL_STYLE: &str = "AnimationDurationFactor=0\n";

/// The text of a render's `kdeglobals`: the color scheme file `colors`, the icon theme, a text
/// cursor that does not blink (`CursorBlinkRate=0`, so two pictures of a page with a focused
/// text field are the same), the animations stopped when `still`, then `other_groups` (more
/// groups, e.g. fonts).
fn render_kdeglobals(colors: &str, icons: &str, still: bool, other_groups: &str) -> String {
    format!(
        "{colors}\n[Icons]\nTheme={icons}\n\n[KDE]\nCursorBlinkRate=0\n{}\n{other_groups}",
        if still { STILL_STYLE } else { "" }
    )
}

/// A config home whose `kdeglobals` selects one Breeze color scheme and its icon theme, so the
/// KDE platform theme renders that scheme regardless of the desktop's own settings. The text
/// cursor does not blink (see [`render_kdeglobals`]); with `still` the style's animations stop
/// too ([`STILL_STYLE`]: the pictures for comparison, not the demo or the measurements, which
/// show and time the animations).
fn theme_home(base: &Path, theme: &str, scheme: &str, icons: &str, still: bool) -> Result<PathBuf> {
    let home = base.join(theme);
    std::fs::create_dir_all(&home)?;
    let colors = color_scheme_file(scheme);
    let globals =
        std::fs::read_to_string(&colors).with_context(|| format!("read {}", colors.display()))?;
    std::fs::write(
        home.join("kdeglobals"),
        render_kdeglobals(&globals, icons, still, ""),
    )?;
    Ok(home)
}

/// Stderr lines that mean a QML problem (warnings carry the QML file URL).
fn qml_problems(stderr: &str) -> Vec<String> {
    let lines: Vec<&str> = stderr.lines().collect();
    lines
        .iter()
        .enumerate()
        // Not ours and not a fault of the page: Kirigami's own drawer handle reports a
        // binding loop on its `bottomMargin` in some renders of the narrow layout when
        // several run at once (two lines: the loop, then where). Everything else counts.
        .filter(|(i, l)| {
            let here = l.contains(KIRIGAMI_DRAWER_HANDLE);
            let next = lines
                .get(i + 1)
                .is_some_and(|n| n.contains(KIRIGAMI_DRAWER_HANDLE));
            !(here || (next && l.contains("Binding loop detected")))
        })
        .map(|(_, l)| *l)
        .filter(|l| {
            l.contains(".qml:")
                || l.contains("qrc:/")
                || l.contains("QML ")
                || l.contains("TypeError")
                || l.contains("ReferenceError")
                || l.contains("Binding loop")
                || l.contains("QTextCursor")
        })
        .map(str::to_owned)
        .collect()
}

/// Where Kirigami's drawer handle lives (see [`qml_problems`]).
const KIRIGAMI_DRAWER_HANDLE: &str = "org/kde/kirigami/templates/private/DrawerHandle.qml";

/// Mean luminance from the app's `shot: <path> luma=<n> settle_ms=<n>` line.
fn shot_luma(stdout: &str) -> Option<u32> {
    shot_field(stdout, "luma")
}

/// Milliseconds from the page's ready moment to the grab, from the same line.
fn shot_settle_ms(stdout: &str) -> Option<u32> {
    shot_field(stdout, "settle_ms")
}

/// The number after ` <name>=` on the app's `shot: <path> luma=<n> settle_ms=<n>` line (the
/// path may hold spaces: the words are read from the end).
fn shot_field(stdout: &str, name: &str) -> Option<u32> {
    let line = stdout.lines().find_map(|l| l.strip_prefix("shot: "))?;
    line.rsplit(' ')
        .find_map(|w| w.strip_prefix(name)?.strip_prefix('=')?.trim().parse().ok())
}

/// What kept a picture from being final, from the app's `shot: unsettled <what>` line on stderr
/// (it saved the frame as it was after 30 s).
fn shot_unsettled(stderr: &str) -> Option<String> {
    stderr
        .lines()
        .find_map(|l| l.strip_prefix("shot: unsettled"))
        .map(|what| what.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every cargo the xtask starts has the Telegram API pair removed from its environment, so a
    /// pair in the caller's shell never reaches `option_env!` (S-5).
    #[test]
    fn cargo_never_passes_the_telegram_api_pair() {
        let cmd = cargo();
        let removed: Vec<_> = cmd
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_owned())
            .collect();
        for var in ["KOURIER_TG_API_ID", "KOURIER_TG_API_HASH"] {
            assert!(
                removed.iter().any(|k| k == var),
                "{var} is removed from cargo's environment"
            );
        }
    }

    #[test]
    fn a_record_of_the_same_tree_ends_the_run_unless_it_is_forced() {
        let record = green::Record {
            digest: 7,
            parts: BTreeMap::new(),
            at: 100,
            engine: "nextest".to_owned(),
            ran: green::Ran::everything(),
        };
        let start = |force: bool, total: u64| Start {
            sys: None,
            digest: Some(green::Digest {
                parts: BTreeMap::new(),
                total,
            }),
            records: vec![(green::Tier::Full, record.clone())],
            force,
        };
        let want = green::Ran::everything();
        assert!(start(false, 7).covered(&want));
        assert!(!start(true, 7).covered(&want), "--force runs anyway");
        assert!(!start(false, 8).covered(&want), "another tree");
        let blind = Start {
            sys: None,
            digest: None,
            records: Vec::new(),
            force: false,
        };
        assert!(!blind.covered(&want), "no digest, no skip");
    }

    fn meta(edges: &[(&str, &str, &str)]) -> Value {
        let mut pkgs: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
        for name in allowed_internal().keys() {
            pkgs.entry(name).or_default();
        }
        for (from, to, kind) in edges {
            pkgs.entry(from)
                .or_default()
                .push(json!({"name": to, "kind": kind}));
        }
        json!({"packages": pkgs.into_iter().map(|(n, d)| json!({"name": n, "dependencies": d})).collect::<Vec<_>>()})
    }

    #[test]
    fn allowed_edges_pass() {
        let m = meta(&[
            ("kourier-core", "kourier-model", "normal"),
            ("kourier-qt", "cxx-qt", "normal"),
        ]);
        assert!(check_metadata(&m).unwrap().is_empty());
    }

    #[test]
    fn core_to_signal_is_rejected() {
        let m = meta(&[("kourier-core", "kourier-signal", "normal")]);
        let errors = check_metadata(&m).unwrap();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("kourier-core -> kourier-signal"));
    }

    /// A crate may name itself as a dev-dependency (cargo's way to switch one of its own
    /// features on for its own tests); as a normal dependency, or of another crate, the rule
    /// stands.
    #[test]
    fn a_crates_dev_dependency_on_itself_is_no_edge() {
        let m = meta(&[("kourier-signal", "kourier-signal", "dev")]);
        assert!(check_metadata(&m).unwrap().is_empty());
        let m = meta(&[("kourier-signal", "kourier-signal", "normal")]);
        assert_eq!(check_metadata(&m).unwrap().len(), 1);
        let m = meta(&[("kourier-core", "kourier-signal", "dev")]);
        assert_eq!(check_metadata(&m).unwrap().len(), 1);
    }

    #[test]
    fn restricted_external_is_rejected() {
        let m = meta(&[
            ("kourier-core", "libsignal-protocol", "normal"),
            ("kourier-model", "tokio", "normal"),
        ]);
        assert_eq!(check_metadata(&m).unwrap().len(), 2);
    }

    /// The WhatsApp backend's edges (architecture §2.2, §7): the contract crates, the database, the
    /// encrypted files and the pictures (W5b), whatsapp-rust's crates and tokio; nothing else of Kourier, and nobody else gets its crates.
    #[test]
    fn whatsapp_may_use_the_contract_the_database_and_whatsapp_rust() {
        let m = meta(&[
            ("kourier-whatsapp", "kourier-model", "normal"),
            ("kourier-whatsapp", "kourier-backend-api", "normal"),
            ("kourier-whatsapp", "kourier-db", "normal"),
            // W5b (media send).
            ("kourier-whatsapp", "kourier-blob", "normal"),
            ("kourier-whatsapp", "kourier-media", "normal"),
            ("kourier-whatsapp", "whatsapp-rust", "normal"),
            (
                "kourier-whatsapp",
                "whatsapp-rust-tokio-transport",
                "normal",
            ),
            (
                "kourier-whatsapp",
                "whatsapp-rust-ureq-http-client",
                "normal",
            ),
            ("kourier-whatsapp", "wacore", "normal"),
            ("kourier-whatsapp", "wacore-binary", "normal"),
            ("kourier-whatsapp", "waproto", "normal"),
            ("kourier-whatsapp", "buffa", "normal"),
            ("kourier-whatsapp", "tokio", "normal"),
            ("kourier-whatsapp", "rusqlite", "dev"),
            ("kourier-e2e", "wacore", "dev"),
            ("kourier-app", "kourier-whatsapp", "normal"),
        ]);
        assert!(check_metadata(&m).unwrap().is_empty());
    }

    #[test]
    fn whatsapp_stays_below_the_core_and_alone_with_its_crates() {
        // The WhatsApp backend does not reach into the core, the app or another backend.
        for to in [
            "kourier-core",
            "kourier-app",
            "kourier-signal",
            "kourier-fake",
        ] {
            let m = meta(&[("kourier-whatsapp", to, "normal")]);
            let errors = check_metadata(&m).unwrap();
            assert_eq!(errors.len(), 1, "{to}: {errors:?}");
            assert!(errors[0].contains(&format!("kourier-whatsapp -> {to}")));
        }
        // Nothing reaches into it but the app (behind a feature).
        for from in [
            "kourier-core",
            "kourier-signal",
            "kourier-viewmodel",
            "kourier-qt",
        ] {
            let m = meta(&[(from, "kourier-whatsapp", "normal")]);
            assert_eq!(check_metadata(&m).unwrap().len(), 1, "{from}");
        }
        // whatsapp-rust's crates belong to the WhatsApp backend (and the e2e crate's tests).
        for from in [
            "kourier-core",
            "kourier-signal",
            "kourier-app",
            "kourier-model",
        ] {
            for dep in [
                "whatsapp-rust",
                // The real transport and HTTP client: the builder's slots are filled by the
                // WhatsApp backend alone (W1 fix F1).
                "whatsapp-rust-tokio-transport",
                "whatsapp-rust-ureq-http-client",
                "wacore",
                "wacore-binary",
                "wacore-appstate",
                "waproto",
                "buffa",
            ] {
                let m = meta(&[(from, dep, "normal")]);
                let errors = check_metadata(&m).unwrap();
                assert_eq!(errors.len(), 1, "{from} -> {dep}: {errors:?}");
                assert!(errors[0].contains(&format!("{from} -> {dep}")));
            }
        }
    }

    /// The Telegram backend's edges (architecture §2.2, §7.5): the contract crates, the database and
    /// the TDLib helper crate, tokio and rusqlite; nothing else of Kourier.
    #[test]
    fn telegram_may_use_the_contract_the_database_and_the_helper_crate() {
        let m = meta(&[
            ("kourier-telegram", "kourier-model", "normal"),
            ("kourier-telegram", "kourier-backend-api", "normal"),
            ("kourier-telegram", "kourier-db", "normal"),
            ("kourier-telegram", "kourier-tdlib", "normal"),
            ("kourier-telegram", "kourier-tdlib", "dev"),
            ("kourier-telegram", "kourier-fake", "dev"),
            ("kourier-telegram", "tokio", "normal"),
            ("kourier-telegram", "rusqlite", "dev"),
            ("kourier-tdlib", "libc", "normal"),
            ("kourier-tdlib", "serde_json", "normal"),
            ("kourier-tdlib", "kourier-tdlib", "dev"),
            ("kourier-app", "kourier-telegram", "normal"),
        ]);
        assert!(check_metadata(&m).unwrap().is_empty());
    }

    #[test]
    fn telegram_stays_below_the_core_and_the_helper_crate_alone() {
        // The Telegram backend does not reach into the core, the app or another backend.
        for to in [
            "kourier-core",
            "kourier-app",
            "kourier-signal",
            "kourier-whatsapp",
            "kourier-blob",
        ] {
            let m = meta(&[("kourier-telegram", to, "normal")]);
            let errors = check_metadata(&m).unwrap();
            assert_eq!(errors.len(), 1, "{to}: {errors:?}");
            assert!(errors[0].contains(&format!("kourier-telegram -> {to}")));
        }
        // Nothing reaches into it but the app (behind a feature); the core never does.
        for from in [
            "kourier-core",
            "kourier-signal",
            "kourier-whatsapp",
            "kourier-viewmodel",
            "kourier-qt",
        ] {
            let m = meta(&[(from, "kourier-telegram", "normal")]);
            assert_eq!(check_metadata(&m).unwrap().len(), 1, "{from}");
        }
        // The helper crate depends on no other Kourier crate, and only the Telegram backend uses
        // it (the app reaches the helper through the backend's seam).
        for to in ["kourier-model", "kourier-backend-api", "kourier-db"] {
            let m = meta(&[("kourier-tdlib", to, "normal")]);
            assert_eq!(
                check_metadata(&m).unwrap().len(),
                1,
                "kourier-tdlib -> {to}"
            );
        }
        for from in ["kourier-app", "kourier-core", "kourier-whatsapp", "kourier"] {
            let m = meta(&[(from, "kourier-tdlib", "normal")]);
            assert_eq!(
                check_metadata(&m).unwrap().len(),
                1,
                "{from} -> kourier-tdlib"
            );
        }
        // No tokio and no rusqlite in the helper: it is a small std program.
        for dep in ["tokio", "rusqlite"] {
            let m = meta(&[("kourier-tdlib", dep, "normal")]);
            assert_eq!(
                check_metadata(&m).unwrap().len(),
                1,
                "kourier-tdlib -> {dep}"
            );
        }
    }

    /// The Threema backend's edges (architecture §2.2, §7.4): the contract crates and the database,
    /// libthreema and tokio; nothing else of Kourier, and nobody else gets libthreema.
    #[test]
    fn threema_may_use_the_contract_the_database_and_libthreema() {
        let m = meta(&[
            ("kourier-threema", "kourier-model", "normal"),
            ("kourier-threema", "kourier-backend-api", "normal"),
            ("kourier-threema", "kourier-db", "normal"),
            ("kourier-threema", "libthreema", "normal"),
            ("kourier-threema", "tokio", "normal"),
            ("kourier-app", "kourier-threema", "normal"),
        ]);
        assert!(check_metadata(&m).unwrap().is_empty());
    }

    #[test]
    fn threema_stays_below_the_core_and_alone_with_its_crates() {
        // The Threema backend does not reach into the core, the app or another backend.
        for to in [
            "kourier-core",
            "kourier-app",
            "kourier-signal",
            "kourier-whatsapp",
            "kourier-telegram",
            "kourier-tdlib",
            "kourier-fake",
        ] {
            let m = meta(&[("kourier-threema", to, "normal")]);
            let errors = check_metadata(&m).unwrap();
            assert_eq!(errors.len(), 1, "{to}: {errors:?}");
            assert!(errors[0].contains(&format!("kourier-threema -> {to}")));
        }
        // Nothing reaches into it but the app (behind a feature).
        for from in [
            "kourier-core",
            "kourier-signal",
            "kourier-whatsapp",
            "kourier-telegram",
            "kourier-viewmodel",
            "kourier-qt",
        ] {
            let m = meta(&[(from, "kourier-threema", "normal")]);
            assert_eq!(check_metadata(&m).unwrap().len(), 1, "{from}");
        }
        // libthreema's crates belong to the Threema backend.
        for from in [
            "kourier-core",
            "kourier-signal",
            "kourier-whatsapp",
            "kourier-app",
            "kourier-model",
        ] {
            for dep in [
                "libthreema",
                "libthreema-macros",
                "libthreema-dependencies",
                "libthreema-blake2",
            ] {
                let m = meta(&[(from, dep, "normal")]);
                let errors = check_metadata(&m).unwrap();
                assert_eq!(errors.len(), 1, "{from} -> {dep}: {errors:?}");
                assert!(errors[0].contains(&format!("{from} -> {dep}")));
            }
        }
    }

    #[test]
    fn coexist_may_use_libsignal_and_db() {
        let m = meta(&[
            ("kourier-coexist", "kourier-db", "normal"),
            ("kourier-coexist", "libsignal-net-infra", "normal"),
            ("kourier-coexist", "tokio", "normal"),
            ("kourier-coexist", "cxx-qt-build", "build"),
        ]);
        assert!(check_metadata(&m).unwrap().is_empty());
        let m = meta(&[("kourier-coexist", "kourier-core", "normal")]);
        assert_eq!(check_metadata(&m).unwrap().len(), 1);
    }

    #[test]
    fn nm_output_parsing() {
        let clean = "\
0000000000c1a2b0 T kourier_main
                 w __gmon_start__
00000000017f0010 B stdout@GLIBC_2.2.5
0000000000a00000 T SSLeay_like_but_not_prefixed
0000000000a00010 T my_SSL_helper
";
        assert!(exported_openssl_symbols(clean).is_empty());
        let dirty = "\
0000000000a1b2c3 T SSL_CTX_new
0000000000a1b2d0 T EVP_EncryptInit_ex@@OPENSSL_3.0.0
0000000000a1b2e0 W CRYPTO_free
0000000000a1b2f0 T kourier_ok
";
        assert_eq!(
            exported_openssl_symbols(dirty),
            ["SSL_CTX_new", "EVP_EncryptInit_ex", "CRYPTO_free"]
        );
    }

    #[test]
    fn shot_output_parsing() {
        assert_eq!(shot_luma("noise\nshot: /a b/c.png luma=187\n"), Some(187));
        assert_eq!(shot_luma("shot: x"), None);
        let line = "shot: /a luma=9/c.png luma=187 settle_ms=2450\n";
        assert_eq!(
            (shot_luma(line), shot_settle_ms(line)),
            (Some(187), Some(2450))
        );
        assert_eq!(shot_settle_ms("shot: /a.png luma=187\n"), None);
        assert_eq!(
            shot_unsettled("x\nshot: unsettled images 2, moving\n").as_deref(),
            Some("images 2, moving")
        );
        assert_eq!(shot_unsettled("shot: /a.png luma=1\n"), None);
        let stderr = "kf.config.core: whatever\nqrc:/qt/qml/x/Main.qml:3:5: Unable to assign\n";
        assert_eq!(qml_problems(stderr).len(), 1);
        // Kirigami's own drawer handle: its two lines are passed over, a loop of ours next
        // to them is not.
        let kirigami = "WARN qt: <Unknown File>: QML QQuickAnchors*: Binding loop detected for property \"bottomMargin\":\nqrc:/qt/qml/org/kde/kirigami/templates/private/DrawerHandle.qml:139:9 category=\"default\"\n";
        assert!(qml_problems(kirigami).is_empty());
        let ours = format!(
            "{kirigami}WARN qt: qrc:/qt/qml/app/kourier/Main.qml:3:1: QML Item: Binding loop detected for property \"width\"\n"
        );
        assert_eq!(qml_problems(&ours).len(), 1);
    }

    /// The quoted strings of `pub const PAGES: [&str; N] = [ … ];` in the UI crate.
    fn ui_pages() -> Vec<String> {
        let path = root().join("crates/kourier-qt/src/lib.rs");
        let source = std::fs::read_to_string(&path).expect("read the UI crate's lib.rs");
        let list = source
            .split_once("pub const PAGES:")
            .and_then(|(_, rest)| rest.split_once("= ["))
            .and_then(|(_, rest)| rest.split_once("];"))
            .map(|(list, _)| list)
            .expect("PAGES in the UI crate");
        list.split('"')
            .skip(1)
            .step_by(2)
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn every_page_is_in_the_screenshot_matrix() {
        let pages = ui_pages();
        assert!(pages.len() >= 12, "{pages:?}");
        assert_eq!(pages, SHOT_PAGES, "SHOT_PAGES must list every --page value");
        for page in SHOT_PAGES_SAME_IN_BOTH_THEMES {
            assert!(SHOT_PAGES.contains(&page), "{page}");
        }
        assert_eq!(shot_mode("qr"), "--fixture-link");
        for page in [
            "media",
            "viewer",
            "compose",
            "files",
            "album",
            "albumviewer",
            "revisions",
            "compact",
            "quotes",
            "menu",
            "timer",
            "loggedout",
        ] {
            assert_eq!(shot_mode(page), "--fixture", "{page}");
        }
    }

    #[test]
    fn the_theme_check_spares_only_pages_dark_in_both_schemes() {
        // A page whose dark rendering is as bright as the light one: the scheme did not apply.
        assert!(theme_problem("media", 120, 110).is_some());
        assert!(
            theme_problem("compact", 150, 110).is_some(),
            "the gap is 40"
        );
        assert_eq!(theme_problem("compact", 151, 110), None);
        assert_eq!(theme_problem("chat", 215, 52), None);
        // The viewer's dark overlay hides the scheme (measured 115 vs 111 and 54 vs 46).
        assert_eq!(theme_problem("viewer", 115, 111), None);
        assert_eq!(theme_problem("viewer", 46, 54), None);
        assert_eq!(theme_problem("albumviewer", 60, 58), None);
        // The other new pages are ordinary pages: the scheme must show.
        assert!(theme_problem("files", 120, 110).is_some());
        assert!(theme_problem("album", 120, 110).is_some());
        assert!(theme_problem("revisions", 120, 110).is_some());
    }

    #[test]
    fn dev_dependency_on_fake_is_allowed() {
        let m = meta(&[("kourier-core", "kourier-fake", "dev")]);
        assert!(check_metadata(&m).unwrap().is_empty());
    }
}

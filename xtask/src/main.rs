//! Developer tasks: `cargo xtask <ci|deps-check|symbols-check|shots|demo|smoke|perf|install|uninstall>`.
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
//! `perf` measures the quality targets of §1.3 and prints them against their budgets
//! ([`perf`]). `install` and `uninstall` put the release build under a prefix in the XDG
//! layout and take it away again ([`install`]). `dist` makes the archives of a release on this
//! machine and checks that they are fit to be made public ([`dist`]); it publishes nothing.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

mod demo;
mod dist;
mod i18n;
mod install;
mod perf;
mod quick;
mod seam;
mod shots;
mod smoke;
mod workspace;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("ci") => ci(&args[1..]),
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
        Some("i18n") => i18n::i18n(&args[1..]),
        Some("dist") => dist::dist(&args[1..]),
        Some("install") => install::install(&args[1..]),
        Some("uninstall") => install::uninstall(&args[1..]),
        _ => {
            eprintln!(
                "usage: cargo xtask <ci [--quick [--since <rev>]]|deps-check|symbols-check [binary]|shots [dir] [--page a,b] [--theme light|dark] [--size wide|narrow] [--jobs N] [--no-build] [--accept] [--no-compare] [--matrix [--variant a,b]]|demo [--out <dir>] [--interactive]|smoke <start|step|steps|state|report|stop|backup|wipe|rehearse> …|perf [--out <file>] [--quick] [--report-only]|i18n <extract|check|build [--out <dir>]>|install (--prefix <dir>|--user)|uninstall (--prefix <dir>|--user)|dist [--out <dir>] [--release] [--allow-dirty] [--allow-paths] [--no-build]>"
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
    let status = cmd
        .current_dir(root())
        .status()
        .with_context(|| format!("spawn: {desc}"))?;
    if !status.success() {
        bail!("step failed: {desc} ({status})");
    }
    Ok(())
}

fn cargo() -> Command {
    Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
}

/// `ci`: everything, or with `--quick` the tier for use while working ([`quick`]).
fn ci(args: &[String]) -> Result<()> {
    let args = quick::parse_args(args)?;
    if args.quick {
        quick::run(args.since.as_deref())
    } else {
        ci_full()
    }
}

/// The steps both tiers start with: rustfmt, clippy and deps-check.
fn lint_steps() -> Result<()> {
    run("rustfmt", cargo().args(["fmt", "--all", "--check"]))?;
    run(
        "clippy",
        cargo().args([
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ]),
    )?;
    eprintln!("\n==> deps-check");
    deps_check()?;
    eprintln!("\n==> i18n check");
    i18n::i18n(&["check".to_owned()])
}

fn ci_full() -> Result<()> {
    lint_steps()?;
    run(
        "tests (non-Qt)",
        cargo().args(["test", "--workspace", "--exclude", "kourier-qt"]),
    )?;
    run(
        "tests (Qt, offscreen)",
        cargo()
            .args(["test", "-p", "kourier-qt"])
            .env("QT_QPA_PLATFORM", "offscreen"),
    )?;
    qmllint()?;
    shots::ci_step()?;
    eprintln!("\nci: all steps passed");
    Ok(())
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
        (
            "rusqlite",
            &[
                "kourier-db",
                "kourier-core",
                "kourier-signal",
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
fn exported_openssl_symbols(nm_output: &str) -> Vec<String> {
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
const SHOT_PAGES: [&str; 47] = [
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
    "transferwait",
    "transferdownload",
    "transferread",
    "transferbanner",
    "transferfailed",
    "transferask",
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

/// A config home whose `kdeglobals` selects one Breeze color scheme and its icon theme, so the
/// KDE platform theme renders that scheme regardless of the desktop's own settings. The text
/// cursor does not blink (`CursorBlinkRate=0`), so two pictures of a page with a focused text
/// field are the same.
fn theme_home(base: &Path, theme: &str, scheme: &str, icons: &str) -> Result<PathBuf> {
    let home = base.join(theme);
    std::fs::create_dir_all(&home)?;
    let colors = color_scheme_file(scheme);
    let mut globals =
        std::fs::read_to_string(&colors).with_context(|| format!("read {}", colors.display()))?;
    globals.push_str(&format!(
        "\n[Icons]\nTheme={icons}\n\n[KDE]\nCursorBlinkRate=0\n"
    ));
    std::fs::write(home.join("kdeglobals"), globals)?;
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

/// Mean luminance from the app's `shot: <path> luma=<n>` line.
fn shot_luma(stdout: &str) -> Option<u32> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("shot: "))
        .and_then(|l| l.rsplit_once(" luma="))
        .and_then(|(_, n)| n.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

//! `cargo xtask tdlib fakedc <build>`: the lane's fake Telegram DC (TD-4a, brief §3.3; decisions
//! P-11, L-f), a TEST-ONLY workspace of its own at `crates/kourier-telegram/fakedc/` with its own
//! `Cargo.lock`, never a member of Kourier's workspace and never in a shipped binary.
//!
//! - `record [<name>...]`: the recording cases with `KOURIER_TG_FAKEDC_RECORD`, then the scrub
//!   (C7, brief §3.8) of each raw recording into `target/tg-fakedc/scrubbed/`.
//! - `build`: the layer coupling (the schema's layer is the pin's, LC-20), then `cargo fmt --check`, `clippy --locked --all-targets -D warnings`, `test --locked`,
//!   `build --release --locked` on its manifest with the ABSOLUTE target directory
//!   `target/tg-fakedc/` (so no `fakedc/target/` appears inside the tree), then `cargo deny check
//!   licenses bans sources` on its lock with the ROOT `deny.toml` (the test-only crates must carry
//!   licences the project accepts), then writes the digest of `crates/kourier-telegram/fakedc/**` as
//!   it was built beside the binary (`release/tg-fakedc.tree-sha256`): a later `env` refuses a
//!   binary whose tree changed since (M6). Every cargo goes through xtask's `cargo()` (no Telegram
//!   API variable reaches it). The first build fetches its crates from crates.io (not a Telegram
//!   host).
//! - The layer coupling (LC-20): the fake DC writes the TL types of the vendored `grammers-tl-types`
//!   (`vendor/grammers-tl-types/`, grammers' `api.tl` at the pin's layer), and TDLib cannot parse an
//!   object of another layer's constructor id. `build`, `env` and `record` (both through
//!   `fakedc_vars`) refuse when the `// LAYER` line of that `api.tl` is not the pin's `layer`, so a
//!   pin bump that forgets the fake DC stops there and not at the re-record.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The workspace's directory, relative to the repository root.
pub(crate) const FAKEDC_DIR: &str = "crates/kourier-telegram/fakedc";
/// The binary's name.
const BIN: &str = "tg-fakedc";
/// The digest file's name beside the binary.
pub(crate) const DIGEST_FILE: &str = "tg-fakedc.tree-sha256";

/// `target/tg-fakedc` as an absolute path.
pub(crate) fn target_dir() -> PathBuf {
    let t = crate::target_dir();
    let t = if t.is_absolute() {
        t
    } else {
        crate::root().join(t)
    };
    t.join("tg-fakedc")
}

/// The release binary.
pub(crate) fn binary() -> PathBuf {
    target_dir().join("release").join(BIN)
}

fn manifest() -> PathBuf {
    crate::root().join(FAKEDC_DIR).join("Cargo.toml")
}

/// The fake DC's TL schema, relative to the repository root: grammers' `api.tl`, vendored
/// (`vendor/grammers-tl-types/VENDORED.md`).
const SCHEMA: &str = "crates/kourier-telegram/fakedc/vendor/grammers-tl-types/tl/api.tl";

/// The layer a schema's first line names: `// LAYER 232` (the line grammers' `build.rs` reads).
pub(crate) fn parse_layer(first_line: &str) -> Option<u32> {
    first_line
        .trim()
        .strip_prefix("//")?
        .trim()
        .strip_prefix("LAYER")?
        .trim()
        .parse()
        .ok()
}

/// The layer of the fake DC's schema, read from the first line of the vendored `api.tl`.
pub(crate) fn schema_layer(root: &Path) -> Result<u32> {
    use std::io::BufRead;
    let path = root.join(SCHEMA);
    let mut first = String::new();
    std::io::BufReader::new(
        std::fs::File::open(&path).with_context(|| format!("open {}", path.display()))?,
    )
    .read_line(&mut first)
    .with_context(|| format!("read {}", path.display()))?;
    parse_layer(&first)
        .with_context(|| format!("{}: the first line is not `// LAYER <n>`", path.display()))
}

/// The refusal when the fake DC's schema is not the pin's layer (LC-20, lead call L10): TDLib parses
/// only its own layer's constructor ids, so a fake DC at another layer fails at the first answer that
/// holds a renamed constructor (layer 232 renamed `channel` and `channelFull`).
pub(crate) fn layer_problem(schema: u32, pin: u32) -> Option<String> {
    (schema != pin).then(|| {
        format!(
            "the fake DC's schema is layer {schema} but the pin's TDLib is layer {pin}: vendor grammers' layer-{pin} `api.tl` (crates/kourier-telegram/fakedc/vendor/grammers-tl-types/VENDORED.md), then run `cargo xtask tdlib fakedc build`"
        )
    })
}

/// [`layer_problem`] for the tree at `root`; an unreadable schema is a problem too.
pub(crate) fn layer_line(root: &Path, pin: &super::Pin) -> Option<String> {
    match schema_layer(root) {
        Ok(schema) => layer_problem(schema, pin.layer),
        Err(e) => Some(format!("the fake DC's schema: {e:#}")),
    }
}

/// Every file under `dir` (relative to it, sorted), directories walked, symbolic links refused.
fn tree_files(dir: &Path) -> Result<Vec<PathBuf>> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_symlink() {
                bail!("{} is a symbolic link", path.display());
            } else if kind.is_dir() {
                walk(base, &path, out)?;
            } else {
                out.push(
                    path.strip_prefix(base)
                        .expect("under the base")
                        .to_path_buf(),
                );
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    out.sort();
    Ok(out)
}

/// The digest of a tree: sha256 over, per file in path order, its relative path, a NUL, its length
/// in decimal, a NUL and its bytes.
pub(crate) fn tree_digest(dir: &Path) -> Result<String> {
    let mut all = Vec::new();
    for rel in tree_files(dir)? {
        let bytes =
            std::fs::read(dir.join(&rel)).with_context(|| format!("read {}", rel.display()))?;
        all.extend_from_slice(rel.to_string_lossy().as_bytes());
        all.push(0);
        all.extend_from_slice(bytes.len().to_string().as_bytes());
        all.push(0);
        all.extend_from_slice(&bytes);
    }
    super::sha256_bytes(&all)
}

fn step(desc: &str, cmd: &mut Command) -> Result<()> {
    eprintln!("\n==> fakedc: {desc}");
    let status = cmd
        .current_dir(crate::root())
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("spawn: {desc}"))?;
    if !status.success() {
        bail!("fakedc: {desc} failed ({status})");
    }
    Ok(())
}

/// A cargo command on the fake DC's manifest with its own absolute target directory.
fn fakedc_cargo(args: &[&str]) -> Command {
    let mut cmd = crate::cargo();
    cmd.args(args)
        .arg("--manifest-path")
        .arg(manifest())
        .env("CARGO_TARGET_DIR", target_dir());
    cmd
}

fn build() -> Result<i32> {
    let root = crate::root();
    let pin = super::Pin::load(&root)?;
    if let Some(p) = layer_line(&root, &pin) {
        bail!("fakedc build: {p}");
    }
    let dir = root.join(FAKEDC_DIR);
    let digest = tree_digest(&dir)?;
    step("rustfmt", fakedc_cargo(&["fmt", "--check"]).arg("--all"))?;
    step(
        "clippy",
        fakedc_cargo(&["clippy", "--locked", "--all-targets"]).args(["--", "-D", "warnings"]),
    )?;
    step("tests", &mut fakedc_cargo(&["test", "--locked"]))?;
    step(
        "release build",
        &mut fakedc_cargo(&["build", "--release", "--locked"]),
    )?;
    let mut deny = crate::cargo();
    // cargo-deny 0.20: `--config` is an option of `cargo deny` itself, before `check`.
    deny.args(["deny", "--locked", "--manifest-path"])
        .arg(manifest())
        .arg("--config")
        .arg(crate::root().join("deny.toml"))
        .args(["check", "licenses", "bans", "sources"])
        .env("CARGO_TARGET_DIR", target_dir());
    step(
        "cargo deny (licences, bans, sources; the root deny.toml)",
        &mut deny,
    )?;
    if tree_digest(&dir)? != digest {
        bail!("fakedc: the tree changed while it was built; build again");
    }
    let bin = binary();
    if !bin.is_file() {
        bail!("fakedc: no binary at {}", bin.display());
    }
    std::fs::write(bin.with_file_name(DIGEST_FILE), format!("{digest}\n"))
        .context("write the tree digest")?;
    println!("fakedc: built {} (tree {digest})", bin.display());
    Ok(0)
}

// ------------------------------------------------------------------------------------------------
// The test-only library (C2, brief §3.4, decisions P-9/M3)

/// The patcher's version: an input of the test-only library's own cache key.
const PATCHER: &str = "tdlib-fakedc 1";
/// The commit the test-only library reports instead of the pin's (refused by every real parts).
pub(crate) const MARKER_COMMIT: &str = "0000000000000000000000000000000000000000";
/// RSA public key blocks the stripped 0001+0002 library holds: TDLib's two handshake keys. The
/// simple-config key of `ConfigManager.cpp` (dead since 0001) is NOT in it (measured 2026-10-27; the
/// brief expected three): any other count stops the tool.
pub(crate) const PEM_BLOCKS: usize = 2;
/// The patch the mutant base lacks (decisions P-9: the one pre-0002 run).
const CALLS_PATCH: &str = "0002-no-incoming-calls.patch";
/// The first line of every test-only manifest, and the mutant's second.
const TEST_ONLY: &str =
    "# TEST ONLY: never shipped, refused by every real parts and by `tdlib check`/`path`.";
const ANSWERS_CALLS: &str =
    "# TEST ONLY: ANSWERS CALLS (built on a library without patch 0002; delete after its one run).";

/// Where the test-only library goes: `target/tdlib-fakedc/` (or `-pre0002/` for the mutant base).
pub(crate) fn lib_dir(mutant: bool) -> PathBuf {
    crate::target_dir().join(if mutant {
        "tdlib-fakedc-pre0002"
    } else {
        "tdlib-fakedc"
    })
}

/// The test-only library's file name: `libtdjson-fakedc.so.<version>`.
pub(crate) fn lib_name(pin: &super::Pin) -> String {
    format!("libtdjson-fakedc.so.{}", pin.version)
}

/// The two public keys `tg-fakedc keygen` wrote, as the library will hold them.
pub(crate) struct NewKeys {
    pub test: Vec<u8>,
    pub second: Vec<u8>,
}

/// Where each needle was and what replaced it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Patched {
    pub bytes: Vec<u8>,
    /// Offsets of the test key, the production key and the commit.
    pub offsets: [usize; 3],
}

/// The same-length byte patch: TDLib's test handshake key -> ours, its production key -> the second
/// key (whose private half was never written), the commit -> [`MARKER_COMMIT`]. Exactly
/// [`PEM_BLOCKS`] blocks, the two named by the pin's sha256, each replacement of its needle's exact
/// length, the commit exactly once; afterwards no needle is left and each replacement is there once.
pub(crate) fn patch_keys(pin: &super::Pin, base: &[u8], keys: &NewKeys) -> Result<Patched> {
    let blocks = super::pem_blocks(base);
    if blocks.len() != PEM_BLOCKS {
        bail!(
            "fakedc lib: the base holds {} RSA key blocks, not {PEM_BLOCKS}: refused (the relink is the fallback, lead call L9)",
            blocks.len()
        );
    }
    let mut test_at = None;
    let mut main_at = None;
    for (at, len) in blocks {
        let sum = super::sha256_bytes(&base[at..at + len])?;
        let slot = if sum == pin.rsa_test_pem_sha256 {
            &mut test_at
        } else if sum == pin.rsa_main_pem_sha256 {
            &mut main_at
        } else {
            bail!("fakedc lib: a key block at {at} is neither of the pin's two handshake keys");
        };
        if slot.replace((at, len)).is_some() {
            bail!("fakedc lib: a handshake key appears twice");
        }
    }
    let (Some(test_at), Some(main_at)) = (test_at, main_at) else {
        bail!("fakedc lib: the base lacks one of the pin's handshake keys");
    };
    let commits = super::find_all(base, pin.commit.as_bytes());
    let [commit_at] = commits.as_slice() else {
        bail!(
            "fakedc lib: the base names the pin's commit {} times, not once",
            commits.len()
        );
    };
    let mut bytes = base.to_vec();
    for ((at, len), new, what) in [
        (test_at, &keys.test, "test"),
        (main_at, &keys.second, "production"),
        (
            (*commit_at, pin.commit.len()),
            &MARKER_COMMIT.as_bytes().to_vec(),
            "commit",
        ),
    ] {
        if new.len() != len {
            bail!(
                "fakedc lib: the {what} replacement has {} bytes, its needle {len}",
                new.len()
            );
        }
        bytes[at..at + len].copy_from_slice(new);
    }
    for (needle, what) in [
        (&base[test_at.0..test_at.0 + test_at.1], "test key"),
        (&base[main_at.0..main_at.0 + main_at.1], "production key"),
        (pin.commit.as_bytes(), "commit"),
    ] {
        if !super::find_all(&bytes, needle).is_empty() {
            bail!("fakedc lib: TDLib's {what} is still in the result");
        }
    }
    for (new, what) in [(&keys.test, "test key"), (&keys.second, "second key")] {
        if super::find_all(&bytes, new).len() != 1 {
            bail!("fakedc lib: our {what} is not in the result exactly once");
        }
    }
    if super::pem_blocks(&bytes).len() != PEM_BLOCKS {
        bail!("fakedc lib: the result's key blocks changed in number");
    }
    Ok(Patched {
        bytes,
        offsets: [test_at.0, main_at.0, *commit_at],
    })
}

/// The test-only library's cache key: the first 16 hex digits of sha256 over the patcher's version,
/// the base's key and sha256, the marker commit and whether it is the mutant base.
pub(crate) fn fkey(base_key: &str, base_sha256: &str, mutant: bool) -> Result<String> {
    let text = format!(
        "{PATCHER}\nbase_key {base_key}\nbase_sha256 {base_sha256}\nmarker {MARKER_COMMIT}\nmutant {mutant}\n"
    );
    Ok(super::sha256_bytes(text.as_bytes())?[..16].to_owned())
}

/// The problem with the fake DC's binary, if any: missing, no digest, or built from another tree (M6).
pub(crate) fn binary_problem(
    bin_exists: bool,
    digest_file: Option<&str>,
    tree: &str,
) -> Option<String> {
    if !bin_exists {
        return Some("no fake DC binary: run `cargo xtask tdlib fakedc build`".into());
    }
    match digest_file.map(str::trim) {
        None => Some("the fake DC binary has no tree digest: run `cargo xtask tdlib fakedc build`".into()),
        Some(d) if d != tree => Some(
            "the fake DC binary is STALE (its tree changed since): run `cargo xtask tdlib fakedc build`".into(),
        ),
        Some(_) => None,
    }
}

/// What `env` refuses in the test-only library's manifest: the mutant base (M3), a manifest of
/// another base than `tdlib path`'s library, or a library whose bytes moved.
pub(crate) fn manifest_problems(
    text: &str,
    base_key: &str,
    base_sha256: &str,
    lib_sha256: &str,
) -> Result<Vec<String>> {
    let mut p = Vec::new();
    let v = |k: &str| super::manifest_value(text, k).unwrap_or_default();
    if text.contains("ANSWERS CALLS") || v("mutant") != "false" {
        p.push(
            "the test-only library is the MUTANT base (answers calls): never listed by `env`"
                .into(),
        );
    }
    if v("base_sha256") != base_sha256 || v("base_key") != base_key {
        p.push("the test-only library was made from another base than `tdlib path`'s library: run `cargo xtask tdlib fakedc lib`".into());
    }
    if v("fkey") != fkey(base_key, base_sha256, false)? {
        p.push("the test-only library's key does not match its base: run `cargo xtask tdlib fakedc lib`".into());
    }
    if v("sha256") != lib_sha256 {
        p.push("the test-only library's sha256 differs from its manifest".into());
    }
    Ok(p)
}

fn read_trim(path: &Path) -> Result<Vec<u8>> {
    let mut b = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    while b.last() == Some(&b'\n') {
        b.pop();
    }
    Ok(b)
}

/// The base library and its manifest after the checks `path` makes (for the mutant base: the same
/// with the pin's patches minus 0002).
fn checked_base(pin: &super::Pin, dir: &Path, mutant: bool) -> Result<(PathBuf, String)> {
    let root = crate::root();
    let lib = dir.join(&pin.soname);
    let manifest = dir.join("manifest.toml");
    let mut patches = super::pin_patches(pin, &root.join(super::PATCH_DIR), false)?;
    if mutant {
        patches.retain(|(name, _)| name != CALLS_PATCH);
    }
    super::report(
        &super::check_file(pin, &lib, Some(&manifest), &patches)?,
        &lib,
    )?;
    let text = std::fs::read_to_string(&manifest)?;
    Ok((lib, text))
}

/// Removes a half-made `fakedc lib` directory when the function ends any way but the rename
/// (after the rename the path is gone and the removal does nothing).
struct TmpGuard(PathBuf);

impl Drop for TmpGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn lib(args: &[String]) -> Result<i32> {
    let mut base = None;
    let mut mutant = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--base" => {
                base = Some(PathBuf::from(
                    it.next().context("--base needs a directory")?,
                ))
            }
            "--mutant-base" => mutant = true,
            _ => bail!("usage: cargo xtask tdlib fakedc lib [--base <dir>] [--mutant-base]"),
        }
    }
    if mutant && base.is_none() {
        bail!("fakedc lib: --mutant-base needs --base <the pre-0002 library's directory>");
    }
    let root = crate::root();
    let pin = super::Pin::load(&root)?;
    let base_dir = base.unwrap_or_else(|| super::target_copy(false));
    let (base_lib, base_manifest) = checked_base(&pin, &base_dir, mutant)?;
    let base_key =
        super::manifest_value(&base_manifest, "key").context("the base manifest has no key")?;
    let base_bytes = std::fs::read(&base_lib)?;
    let base_sha = super::sha256_bytes(&base_bytes)?;
    let key = fkey(&base_key, &base_sha, mutant)?;
    let out = lib_dir(mutant);
    let out_lib = out.join(lib_name(&pin));
    if let Ok(text) = std::fs::read_to_string(out.join("manifest.toml"))
        && super::manifest_value(&text, "fkey").as_deref() == Some(key.as_str())
        && out.join("test-key.pem").is_file()
        && out_lib.is_file()
        && super::manifest_value(&text, "sha256") == Some(super::sha256_file(&out_lib)?)
    {
        println!(
            "fakedc lib: {} is up to date (key {key})",
            out_lib.display()
        );
        return Ok(0);
    }
    let bin = binary();
    let dir = root.join(FAKEDC_DIR);
    let bin_digest = std::fs::read_to_string(bin.with_file_name(DIGEST_FILE)).ok();
    if let Some(p) = binary_problem(bin.is_file(), bin_digest.as_deref(), &tree_digest(&dir)?) {
        bail!("fakedc lib: {p}");
    }
    let tmp = out.with_extension("tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    // A failure after the write (the ELF rows, the genuine-keys refusal, the rename) must not
    // leave a patched library and its private key in `*.tmp` (S-L3).
    let _tmp_guard = TmpGuard(tmp.clone());
    let keys_dir = tmp.join("keys");
    step(
        "keygen (two 2048-bit keys; the second's private half is never written)",
        Command::new(&bin).arg("keygen").arg("--out").arg(&keys_dir),
    )?;
    let keys = NewKeys {
        test: read_trim(&keys_dir.join("test-public.pem"))?,
        second: read_trim(&keys_dir.join("second-public.pem"))?,
    };
    let fingerprints = std::fs::read_to_string(keys_dir.join("fingerprints.txt"))?;
    let patched = patch_keys(&pin, &base_bytes, &keys)?;
    let lib_path = tmp.join(lib_name(&pin));
    std::fs::write(&lib_path, &patched.bytes)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&lib_path, std::fs::Permissions::from_mode(0o755))?;
    }
    let nm = super::output(
        Command::new("nm")
            .args(["-D", "--defined-only"])
            .arg(&lib_path),
    )?;
    let readelf = super::output(Command::new("readelf").arg("-d").arg(&lib_path))?;
    let elf = super::check_elf(&pin, &nm, &readelf, patched.bytes.len() as u64);
    super::report(&elf, &lib_path)?;
    if super::genuine_keys_problems(&pin, &patched.bytes)?.is_empty() {
        bail!("fakedc lib: the result passes the genuine-keys row: refused");
    }
    std::fs::rename(keys_dir.join("test-key.pem"), tmp.join("test-key.pem"))?;
    std::fs::remove_dir_all(&keys_dir)?;
    let sha = super::sha256_bytes(&patched.bytes)?;
    let mut m = String::new();
    m.push_str(TEST_ONLY);
    m.push('\n');
    if mutant {
        m.push_str(ANSWERS_CALLS);
        m.push('\n');
    }
    m.push_str(&format!("fkey = \"{key}\"\n"));
    m.push_str(&format!("mutant = \"{mutant}\"\n"));
    m.push_str(&format!("base_key = \"{base_key}\"\n"));
    m.push_str(&format!("base_sha256 = \"{base_sha}\"\n"));
    m.push_str(&format!("commit = \"{MARKER_COMMIT}\"\n"));
    for l in fingerprints.lines() {
        if let Some((k, v)) = l.split_once(' ') {
            m.push_str(&format!("{k}_fingerprint = \"{v}\"\n"));
        }
    }
    let [a, b, c] = patched.offsets;
    m.push_str(&format!(
        "offsets = \"test key {a}, production key {b}, commit {c}\"\n"
    ));
    m.push_str(&format!("sha256 = \"{sha}\"\n"));
    std::fs::write(tmp.join("manifest.toml"), m)?;
    let _ = std::fs::remove_dir_all(&out);
    std::fs::rename(&tmp, &out)?;
    println!(
        "fakedc lib: wrote {} (key {key}{})",
        out.join(lib_name(&pin)).display(),
        if mutant {
            ", THE MUTANT BASE: answers calls"
        } else {
            ""
        }
    );
    Ok(0)
}

/// What `env` and `record` hand to the test run: the test-only library, the fake DC's binary and
/// key, and `tdlib path`'s library (`KOURIER_TDJSON`).
pub(crate) struct Vars {
    pub(crate) lib: PathBuf,
    pub(crate) bin: PathBuf,
    pub(crate) key: PathBuf,
    pub(crate) real: PathBuf,
}

/// THE ONE check of the test-only side (S-M3), used by `env` (prints it) and `record` (runs the
/// cases with it): the fake DC binary is built from the current tree, `tdlib path`'s library
/// passes its checks, the fake DC's schema is the pin's layer (LC-20), and the test-only library's
/// manifest vouches for exactly these bytes (not the mutant base, made from the base that stands
/// now, its fkey, its sha256) and its key file exists. `Err` holds every problem, one line each.
pub(crate) fn fakedc_vars(pin: &super::Pin) -> Result<std::result::Result<Vars, Vec<String>>> {
    let root = crate::root();
    let mut problems = Vec::new();
    let bin = binary();
    let bin_digest = std::fs::read_to_string(bin.with_file_name(DIGEST_FILE)).ok();
    problems.extend(binary_problem(
        bin.is_file(),
        bin_digest.as_deref(),
        &tree_digest(&root.join(FAKEDC_DIR))?,
    ));
    problems.extend(layer_line(&root, pin));
    let dir = lib_dir(false);
    let lib = dir.join(lib_name(pin));
    let key = dir.join("test-key.pem");
    let mut real = None;
    match (
        checked_base(pin, &super::target_copy(false), false),
        std::fs::read_to_string(dir.join("manifest.toml")),
    ) {
        (Ok((base_lib, base_manifest)), Ok(text)) if lib.is_file() && key.is_file() => {
            let base_key = super::manifest_value(&base_manifest, "key").unwrap_or_default();
            problems.extend(manifest_problems(
                &text,
                &base_key,
                &super::sha256_file(&base_lib)?,
                &super::sha256_file(&lib)?,
            )?);
            real = Some(base_lib);
        }
        (Err(e), _) => problems.push(format!("`tdlib path`'s library: {e}")),
        _ => problems.push("no test-only library: run `cargo xtask tdlib fakedc lib`".into()),
    }
    match real {
        Some(real) if problems.is_empty() => Ok(Ok(Vars {
            lib,
            bin,
            key,
            real,
        })),
        _ => Ok(Err(problems)),
    }
}

/// `env`: three `export` lines for `eval`, or exit 1 with the reason (never the mutant base).
fn env() -> Result<i32> {
    let pin = super::Pin::load(&crate::root())?;
    match fakedc_vars(&pin)? {
        Ok(v) => {
            println!("export KOURIER_TDJSON_FAKEDC={}", v.lib.display());
            println!("export KOURIER_TG_FAKEDC_BIN={}", v.bin.display());
            println!("export KOURIER_TG_FAKEDC_KEY={}", v.key.display());
            Ok(0)
        }
        Err(problems) => {
            for p in &problems {
                eprintln!("fakedc env: {p}");
            }
            Ok(1)
        }
    }
}

/// The recording cases (`fakedc::rec::<test>`) and the recording each writes (C7, brief §3.7).
pub(crate) const RECORDINGS: &[(&str, &str)] = &[
    ("rc1_fresh_start", "fresh-start"),
    ("rc2_restart_saved_pts", "restart-saved-pts"),
    ("rc3_kill_at_once", "kill-at-once"),
    ("rc3_kill_after_quiet", "kill-after-quiet"),
    ("rc4_reconnect", "reconnect"),
    ("rc5a_gap_known_sender", "gap-known-sender"),
    ("rc5b_gap_filled_by_push", "gap-filled-by-push"),
    ("rc5c_gap_channel", "gap-channel"),
    ("rc5d_gap_unknown_sender", "gap-unknown-sender"),
    ("rc6_channel_difference", "channel-difference"),
    ("rc7_edit", "edit"),
    ("rc8_delete", "delete"),
    ("rc9_read_elsewhere", "read-elsewhere"),
    ("rc10_incoming_call", "incoming-call"),
    ("rc11_basic_group_upgrade", "basic-group-upgrade"),
    ("rc13_read_act", "read-act"),
    ("rc12a_first_live_after_load", "first-live-after-load"),
    ("rc12b_first_live_before_load", "first-live-before-load"),
    ("rc14_difference_too_long", "difference-too-long"),
];

/// The recordings `record` makes: every one, or those named (by recording or test name).
pub(crate) fn chosen(names: &[String]) -> Result<Vec<(&'static str, &'static str)>> {
    if names.is_empty() {
        return Ok(RECORDINGS.to_vec());
    }
    names
        .iter()
        .map(|n| {
            RECORDINGS
                .iter()
                .find(|(t, r)| n == t || n == r)
                .copied()
                .with_context(|| format!("no recording named {n}"))
        })
        .collect()
}

/// `record [<name>...]`: runs the recording cases with `KOURIER_TG_FAKEDC_RECORD` set to
/// `target/tg-fakedc/raw/` (fresh), in the lane's environment (`tdlib path`'s library, the fake-DC
/// variables of `env`, every REQUIRE flag), then scrubs each raw recording with
/// `scripts/tg-fixture-scrub.py --recording` into `target/tg-fakedc/scrubbed/`. The raw files
/// never leave `target/`; what may go into `testdata/` is decided by a person reading the scrubbed
/// file (the README's rule). The cases themselves run TDLib and the fake DC only inside their
/// `unshare -rn` child.
fn record(names: &[String]) -> Result<i32> {
    let chosen = chosen(names)?;
    let root = crate::root();
    let pin = super::Pin::load(&root)?;
    // The same whole check as `env` (S-M3): a stale fake DC, another base, a moved library, the
    // mutant base: `record` refuses with `env`'s lines.
    let vars = match fakedc_vars(&pin)? {
        Ok(v) => v,
        Err(problems) => {
            for p in &problems {
                eprintln!("fakedc record: {p}");
            }
            bail!("fakedc record: the test-only side is not in order (the lines above)");
        }
    };
    let raw = target_dir().join("raw");
    let scrubbed = target_dir().join("scrubbed");
    for d in [&raw, &scrubbed] {
        if d.exists() {
            std::fs::remove_dir_all(d).with_context(|| format!("remove {}", d.display()))?;
        }
    }
    std::fs::create_dir_all(&raw)?;
    let filter = chosen
        .iter()
        .map(|(t, _)| format!("test(/fakedc::rec::{t}$/)"))
        .collect::<Vec<_>>()
        .join(" | ");
    let status = crate::cargo()
        .args([
            "nextest",
            "run",
            "--workspace",
            "--exclude",
            "kourier-qt",
            "-E",
        ])
        .arg(format!("package(kourier-telegram) & ({filter})"))
        .env("KOURIER_TDJSON", &vars.real)
        .env("KOURIER_TDJSON_FAKEDC", &vars.lib)
        .env("KOURIER_TG_FAKEDC_BIN", &vars.bin)
        .env("KOURIER_TG_FAKEDC_KEY", &vars.key)
        .env("KOURIER_TG_FAKEDC_RECORD", &raw)
        .env_remove("KOURIER_TDJSON_FAKEDC_PRE0002")
        .envs(
            ["TDLIB", "UNSHARE", "LANDLOCK", "FAKEDC"]
                .map(|r| (format!("KOURIER_REQUIRE_{r}"), "1")),
        )
        .status()
        .context("cargo nextest")?;
    if !status.success() {
        eprintln!("fakedc record: the recording cases failed; nothing scrubbed");
        return Ok(1);
    }
    let mut failed = 0;
    for (_, name) in &chosen {
        let recording = raw.join(format!("{name}.raw.jsonl"));
        let world = raw.join(format!("{name}.world.json"));
        let ok = Command::new("python3")
            .arg("-I")
            .arg(root.join("scripts/tg-fixture-scrub.py"))
            .arg("--recording")
            .arg(&recording)
            .arg("--world")
            .arg(&world)
            .arg(&scrubbed)
            .stdin(Stdio::null())
            .status()
            .context("python3")?
            .success();
        if !ok {
            eprintln!("fakedc record: {name}: SCRUB FAILED");
            failed += 1;
        }
    }
    println!(
        "fakedc record: {} recording(s), {} scrubbed into {}",
        chosen.len(),
        chosen.len() - failed,
        scrubbed.display()
    );
    Ok(i32::from(failed > 0))
}

/// `perf [<test>...]`: TP's measuring cases (`fakedc::tp::`, ignored in every other run), all or those
/// named (`tp_one`), one at a time, with the same whole check of the test-only side as `record`.
/// Prints their `FAKEDC tp-*` lines.
fn perf(names: &[String]) -> Result<i32> {
    let pin = super::Pin::load(&crate::root())?;
    let vars = match fakedc_vars(&pin)? {
        Ok(v) => v,
        Err(problems) => {
            for p in &problems {
                eprintln!("fakedc perf: {p}");
            }
            bail!("fakedc perf: the test-only side is not in order (the lines above)");
        }
    };
    let status = crate::cargo()
        .args([
            "nextest",
            "run",
            "--workspace",
            "--exclude",
            "kourier-qt",
            "--run-ignored",
            "only",
            "--no-capture",
            "-E",
        ])
        .arg(format!(
            "package(kourier-telegram) & test(/fakedc::tp::({})$/)",
            if names.is_empty() {
                ".*".to_owned()
            } else {
                names.join("|")
            }
        ))
        .env("KOURIER_TDJSON", &vars.real)
        .env("KOURIER_TDJSON_FAKEDC", &vars.lib)
        .env("KOURIER_TG_FAKEDC_BIN", &vars.bin)
        .env("KOURIER_TG_FAKEDC_KEY", &vars.key)
        .env_remove("KOURIER_TDJSON_FAKEDC_PRE0002")
        .envs(
            ["TDLIB", "UNSHARE", "LANDLOCK", "FAKEDC"]
                .map(|r| (format!("KOURIER_REQUIRE_{r}"), "1")),
        )
        .status()
        .context("cargo nextest")?;
    Ok(status.code().unwrap_or(1))
}

pub(crate) fn fakedc(args: &[String]) -> Result<i32> {
    match args.first().map(String::as_str) {
        Some("build") if args.len() == 1 => build(),
        Some("lib") => lib(&args[1..]),
        Some("env") if args.len() == 1 => env(),
        Some("record") => record(&args[1..]),
        Some("perf") => perf(&args[1..]),
        _ => {
            eprintln!(
                "usage: cargo xtask tdlib fakedc <build|lib [--base <dir>] [--mutant-base]|env|record [<name>...]|perf [<test>...]>"
            );
            Ok(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_half_made_library_directory_is_removed_when_the_function_leaves() {
        let base = std::env::temp_dir().join(format!("xtask-fakedc-tmp-{}", std::process::id()));
        let _cleanup = TempDir(base.clone());
        let tmp = base.join("tdlib-fakedc.tmp");
        std::fs::create_dir_all(tmp.join("keys")).unwrap();
        std::fs::write(tmp.join("test-key.pem"), "not a key").unwrap();
        {
            let _guard = TmpGuard(tmp.clone());
            assert!(tmp.join("test-key.pem").is_file());
        }
        assert!(!tmp.exists(), "the half-made directory is left");
    }

    #[test]
    fn the_tree_digest_follows_every_file_name_and_byte() {
        let base = std::env::temp_dir().join(format!("xtask-fakedc-{}", std::process::id()));
        let _guard = TempDir(base.clone());
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("Cargo.toml"), "a").unwrap();
        std::fs::write(base.join("src/main.rs"), "fn main() {}").unwrap();
        let first = tree_digest(&base).unwrap();
        assert_eq!(first.len(), 64);
        assert_eq!(tree_digest(&base).unwrap(), first);
        std::fs::write(base.join("src/main.rs"), "fn main() { }").unwrap();
        let changed = tree_digest(&base).unwrap();
        assert_ne!(changed, first);
        std::fs::write(base.join("src/new.rs"), "").unwrap();
        assert_ne!(tree_digest(&base).unwrap(), changed);
        std::fs::remove_file(base.join("src/new.rs")).unwrap();
        std::fs::rename(base.join("src/main.rs"), base.join("src/lib.rs")).unwrap();
        assert_ne!(tree_digest(&base).unwrap(), changed);
    }

    /// A key-shaped block of exactly 425 bytes whose body is `fill`.
    fn block(fill: u8) -> Vec<u8> {
        let mut b = super::super::PEM_BEGIN.to_vec();
        b.push(b'\n');
        b.extend(std::iter::repeat_n(fill, 425 - 31 - 28));
        b.extend_from_slice(super::super::PEM_END);
        assert_eq!(b.len(), 425);
        b
    }

    /// The committed pin with its two key hashes set to the synthetic blocks 'T' and 'M'.
    fn synthetic_pin() -> super::super::Pin {
        let text = std::fs::read_to_string(crate::root().join(super::super::PIN_FILE)).unwrap();
        let mut pin = super::super::Pin::parse(&text).unwrap();
        pin.rsa_test_pem_sha256 = super::super::sha256_bytes(&block(b'T')).unwrap();
        pin.rsa_main_pem_sha256 = super::super::sha256_bytes(&block(b'M')).unwrap();
        pin
    }

    /// A library-shaped buffer: filler, the test key, filler, the production key, the commit.
    fn base(pin: &super::super::Pin, blocks: &[Vec<u8>]) -> Vec<u8> {
        let mut b = vec![0u8; 100];
        for bl in blocks {
            b.extend_from_slice(bl);
            b.extend([0u8; 37]);
        }
        b.extend_from_slice(pin.commit.as_bytes());
        b.extend([0u8; 9]);
        b
    }

    fn new_keys() -> NewKeys {
        NewKeys {
            test: block(b'a'),
            second: block(b'b'),
        }
    }

    #[test]
    fn both_handshake_keys_and_the_commit_are_replaced_in_place() {
        let pin = synthetic_pin();
        let b = base(&pin, &[block(b'T'), block(b'M')]);
        let p = patch_keys(&pin, &b, &new_keys()).unwrap();
        assert_eq!(p.bytes.len(), b.len());
        assert_eq!(p.offsets, [100, 100 + 425 + 37, 100 + 2 * (425 + 37)]);
        let find = |n: &[u8]| super::super::find_all(&p.bytes, n).len();
        assert_eq!(find(&block(b'T')), 0, "TDLib's test key left");
        assert_eq!(find(&block(b'M')), 0, "TDLib's production key left");
        assert_eq!(find(pin.commit.as_bytes()), 0, "the commit left");
        assert_eq!(find(&block(b'a')), 1);
        assert_eq!(find(&block(b'b')), 1);
        assert_eq!(find(MARKER_COMMIT.as_bytes()), 1);
        // The order of the two keys in the library does not matter: each goes by its hash.
        let b = base(&pin, &[block(b'M'), block(b'T')]);
        let p = patch_keys(&pin, &b, &new_keys()).unwrap();
        assert_eq!(&p.bytes[100..525], &block(b'b')[..]);
    }

    #[test]
    fn another_count_of_key_blocks_or_of_the_commit_is_refused() {
        let pin = synthetic_pin();
        for blocks in [
            vec![block(b'T')],
            vec![block(b'T'), block(b'M'), block(b'X')],
            vec![block(b'T'), block(b'X')],
            vec![block(b'T'), block(b'T')],
        ] {
            assert!(patch_keys(&pin, &base(&pin, &blocks), &new_keys()).is_err());
        }
        let mut twice = base(&pin, &[block(b'T'), block(b'M')]);
        twice.extend_from_slice(pin.commit.as_bytes());
        assert!(patch_keys(&pin, &twice, &new_keys()).is_err());
    }

    #[test]
    fn a_replacement_of_another_length_is_refused() {
        let pin = synthetic_pin();
        let b = base(&pin, &[block(b'T'), block(b'M')]);
        let mut short = new_keys();
        short.second.pop();
        assert!(patch_keys(&pin, &b, &short).is_err());
    }

    /// The genuine-keys row of `check`/`path`: the base passes it, the patched library fails it.
    #[test]
    fn the_check_refuses_a_patched_library_and_keeps_a_genuine_one() {
        let pin = synthetic_pin();
        let b = base(&pin, &[block(b'T'), block(b'M')]);
        let row = |r: &super::super::CheckReport| {
            r.problems
                .iter()
                .filter(|p| p.contains("a test-only library?"))
                .count()
        };
        let genuine = super::super::check_bytes(&pin, "", "", &b).unwrap();
        assert_eq!(row(&genuine), 0, "{:?}", genuine.problems);
        let patched = patch_keys(&pin, &b, &new_keys()).unwrap();
        let r = super::super::check_bytes(&pin, "", "", &patched.bytes).unwrap();
        assert_eq!(row(&r), 3, "{:?}", r.problems);
    }

    #[test]
    fn the_library_key_follows_every_input() {
        let k = fkey("a", "b", false).unwrap();
        assert_eq!(k.len(), 16);
        assert_eq!(fkey("a", "b", false).unwrap(), k);
        for other in [
            fkey("a2", "b", false),
            fkey("a", "b2", false),
            fkey("a", "b", true),
        ] {
            assert_ne!(other.unwrap(), k);
        }
    }

    #[test]
    fn a_stale_or_missing_binary_is_refused() {
        assert!(binary_problem(true, Some("abc\n"), "abc").is_none());
        assert!(
            binary_problem(true, Some("abc\n"), "abd")
                .unwrap()
                .contains("STALE")
        );
        assert!(binary_problem(true, None, "abc").is_some());
        assert!(binary_problem(false, Some("abc"), "abc").is_some());
    }

    #[test]
    fn env_never_lists_the_mutant_base_nor_a_library_of_another_base() {
        let manifest = |mutant: bool, base_sha: &str| {
            format!(
                "{TEST_ONLY}\n{}fkey = \"{}\"\nmutant = \"{mutant}\"\nbase_key = \"k\"\nbase_sha256 = \"{base_sha}\"\nsha256 = \"l\"\n",
                if mutant {
                    format!("{ANSWERS_CALLS}\n")
                } else {
                    String::new()
                },
                fkey("k", base_sha, mutant).unwrap()
            )
        };
        assert!(
            manifest_problems(&manifest(false, "s"), "k", "s", "l")
                .unwrap()
                .is_empty()
        );
        let m = manifest_problems(&manifest(true, "s"), "k", "s", "l").unwrap();
        assert!(m.iter().any(|p| p.contains("MUTANT")), "{m:?}");
        assert!(
            !manifest_problems(&manifest(false, "s"), "k", "s2", "l")
                .unwrap()
                .is_empty()
        );
        assert!(
            !manifest_problems(&manifest(false, "s"), "k", "s", "l2")
                .unwrap()
                .is_empty()
        );
        assert_eq!(lib_dir(false).file_name().unwrap(), "tdlib-fakedc");
    }

    #[test]
    fn the_layer_is_read_from_the_first_line_of_the_schema() {
        assert_eq!(parse_layer("// LAYER 232\n"), Some(232));
        assert_eq!(parse_layer("//LAYER 9"), Some(9));
        for bad in [
            "",
            "\n",
            "LAYER 232",
            "// LAYER",
            "// LAYER x",
            "// layer 232",
            "# LAYER 232",
        ] {
            assert_eq!(parse_layer(bad), None, "{bad:?}");
        }
    }

    /// The tree's own schema is the committed pin's layer: the test that fails in the bump's first
    /// unit-test run when the fake DC was forgotten.
    #[test]
    fn the_committed_schema_is_the_pins_layer() {
        let root = crate::root();
        let pin = super::super::Pin::load(&root).unwrap();
        assert_eq!(schema_layer(&root).unwrap(), pin.layer);
        assert_eq!(layer_line(&root, &pin), None);
    }

    /// L10, widened: a fake DC at another layer than the pin is refused by `build` (it calls
    /// `layer_line` first), by `env` and by `record` (both through `fakedc_vars`).
    #[test]
    fn a_fake_dc_at_another_layer_than_the_pin_is_refused() {
        assert_eq!(layer_problem(232, 232), None);
        for (schema, pin) in [(230, 232), (232, 230), (233, 232)] {
            let p = layer_problem(schema, pin).unwrap();
            assert!(
                p.contains(&format!("layer {schema}")) && p.contains(&format!("layer {pin}")),
                "{p}"
            );
            assert!(
                p.contains("VENDORED.md") && p.contains("fakedc build"),
                "{p}"
            );
        }
        let root = crate::root();
        let mut pin = synthetic_pin();
        let schema = schema_layer(&root).unwrap();
        pin.layer = schema - 2;
        assert!(layer_line(&root, &pin).unwrap().contains("layer"));
        // `build` stops on that line before any cargo runs; `env` and `record` print it with the
        // others: the pin of another layer lists it among the problems, the right one does not.
        let problems = fakedc_vars(&pin).unwrap().err().expect("refused");
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("the fake DC's schema is layer")),
            "{problems:?}"
        );
        pin.layer = schema;
        if let Err(problems) = fakedc_vars(&pin).unwrap() {
            assert!(
                !problems
                    .iter()
                    .any(|p| p.starts_with("the fake DC's schema")),
                "{problems:?}"
            );
        }
        // An unreadable schema is a problem, never a pass.
        let nowhere = std::env::temp_dir().join(format!("xtask-no-schema-{}", std::process::id()));
        assert!(
            layer_line(&nowhere, &pin)
                .unwrap()
                .contains("the fake DC's schema")
        );
    }

    #[test]
    fn the_target_directory_is_absolute_and_outside_the_tree() {
        let t = target_dir();
        assert!(t.is_absolute());
        assert!(!t.starts_with(crate::root().join(FAKEDC_DIR)));
        assert!(binary().starts_with(&t));
    }
}

//! The names file: the symbol table of the release binary, split off after the build (SIZE step
//! S-names, 2026-10-27).
//!
//! `cargo xtask dist` builds the binary with its `.symtab` (the release profile keeps
//! `strip = "debuginfo"`), then [`split`] derives two files from those very bytes:
//! `objcopy --only-keep-debug` writes the names file, `objcopy --strip-all
//! --add-gnu-debuglink=<names file>` writes the shipped file. Both keep the GNU build-id of the
//! input, and the shipped file stores the names file's basename and the zlib CRC-32 of its bytes
//! in `.gnu_debuglink`. [`check_pair`] proves the tie on three files: the build output, the shipped
//! file and the names file.
//!
//! The names file is called `kourier-<build-id hex>.debug`, not `kourier.debug`: std's backtrace
//! code finds a separate file by that basename and does not check the CRC, so a fixed name would
//! let release N's names answer for release N+1's binary after a plain `cp -r bin share` upgrade
//! (critic MEDIUM-1). A stale file is simply not found.
//!
//! Why the DWARF refusal: after the split, debug information could not reach the shipped file
//! any more (it would go into the names file, which is what Flatpak's `.Debug` extension holds).
//! The refusal is a POLICY, not a size need: it keeps the build time and the 260 MB target
//! directory the profile's `debug = false` / `strip = "debuginfo"` were set for. A later decision
//! to keep line tables changes [`refuse_debug_info`] (critic LOW-11).
//!
//! The functions are used by `dist.rs` and `perf.rs`.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The shipped file's name inside the split directory.
pub(crate) const SHIPPED_NAME: &str = "kourier";

/// The result of [`split`].
#[derive(Debug, Clone)]
pub(crate) struct Split {
    /// The binary without its symbol table, `<dir>/kourier`.
    pub shipped: PathBuf,
    /// The names file, `<dir>/kourier-<build-id>.debug`.
    pub names: PathBuf,
    pub shipped_bytes: u64,
    pub names_bytes: u64,
    /// The GNU build-id of the input, lowercase hex.
    pub build_id: String,
    /// The CRC-32 stored in the shipped file's `.gnu_debuglink` (and of the names file).
    pub crc: u32,
}

/// The names file's basename for a build-id.
pub(crate) fn names_file_name(build_id: &str) -> String {
    format!("kourier-{build_id}.debug")
}

/// Splits `input` into `<dir>/kourier` (no symbol table, debuglink added) and
/// `<dir>/kourier-<build-id>.debug`. Refuses an input with debug information or without a
/// build-id. `input` is not touched. `dir` is created; files in it are overwritten.
pub(crate) fn split(input: &Path, dir: &Path) -> Result<Split> {
    refuse_debug_info(input)?;
    refuse_no_symbol_table(input)?;
    let id = build_id(input)?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let names = dir.join(names_file_name(&id));
    let shipped = dir.join(SHIPPED_NAME);
    run(Command::new("objcopy")
        .arg("--only-keep-debug")
        .arg(input)
        .arg(&names))?;
    // --add-gnu-debuglink stores the BASENAME of the file it is given and its CRC.
    run(Command::new("objcopy")
        .arg("--strip-all")
        .arg(format!("--add-gnu-debuglink={}", names.display()))
        .arg(input)
        .arg(&shipped))?;
    let (_, crc) = debuglink(&shipped)?;
    Ok(Split {
        shipped_bytes: len(&shipped)?,
        names_bytes: len(&names)?,
        shipped,
        names,
        build_id: id,
        crc,
    })
}

/// Refuses a file with a `.debug_*`/`.zdebug_*` section (policy, see the module comment).
fn refuse_debug_info(path: &Path) -> Result<()> {
    let sections = section_names(&readelf(path, "-SW")?);
    if let Some(s) = sections.iter().find(|s| is_debug_section(s)) {
        bail!(
            "{} has debug information ({s}): the release build must not carry it, so \
             `[profile.release]` `debug = false` / `strip = \"debuginfo\"` in Cargo.toml was \
             changed. This is a policy (build time and target size), not a size need of the \
             shipped file; to keep line tables, change names::refuse_debug_info",
            path.display()
        );
    }
    Ok(())
}

/// Refuses a file without a `.symtab` (review MEDIUM-1): the names file would be empty, and
/// dist would publish a release whose names never existed.
fn refuse_no_symbol_table(path: &Path) -> Result<()> {
    let sections = section_names(&readelf(path, "-SW")?);
    if !sections.iter().any(|s| s == ".symtab") {
        bail!(
            "{} has no symbol table (.symtab): the names file would be empty. The release \
             profile must keep `strip = \"debuginfo\"` (not \"symbols\"), and no linker flag, \
             `-C strip` or RUSTFLAGS may drop the table",
            path.display()
        );
    }
    Ok(())
}

fn is_debug_section(name: &str) -> bool {
    name.starts_with(".debug_") || name.starts_with(".zdebug_")
}

/// The GNU build-id of an ELF file (lowercase hex), from `readelf -n`.
pub(crate) fn build_id(path: &Path) -> Result<String> {
    parse_build_id(&readelf(path, "-n")?)
        .with_context(|| format!("{} has no GNU build-id", path.display()))
}

/// The id in `readelf -n` output (`    Build ID: <hex>`).
pub(crate) fn parse_build_id(readelf_n: &str) -> Option<String> {
    readelf_n.lines().find_map(|l| {
        let id = l.trim().strip_prefix("Build ID:")?.trim();
        (!id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| id.to_ascii_lowercase())
    })
}

/// The name and CRC stored in `.gnu_debuglink`, from `readelf --debug-dump=links` (writes
/// nothing, unlike a section dump through objcopy: critic LOW-4).
pub(crate) fn debuglink(path: &Path) -> Result<(String, u32)> {
    parse_debuglink(&readelf(path, "-wk")?)
        .with_context(|| format!("{} has no .gnu_debuglink", path.display()))
}

/// `Separate debug info file: <name>` and `CRC value: 0x<hex>` in `readelf -wk` output.
pub(crate) fn parse_debuglink(text: &str) -> Option<(String, u32)> {
    let mut name = None;
    let mut crc = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(n) = line.strip_prefix("Separate debug info file:") {
            name = Some(n.trim().to_owned());
        } else if let Some(c) = line.strip_prefix("CRC value:") {
            crc = u32::from_str_radix(c.trim().trim_start_matches("0x"), 16).ok();
        }
    }
    Some((name.filter(|n| !n.is_empty())?, crc?))
}

/// The section names in `readelf -SW` output, in order.
pub(crate) fn section_names(readelf_s: &str) -> Vec<String> {
    sections(readelf_s).into_iter().map(|s| s.0).collect()
}

/// `(name, file offset, size)` of every section in `readelf -SW` output.
pub(crate) fn sections(readelf_s: &str) -> Vec<(String, u64, u64)> {
    readelf_s
        .lines()
        .filter_map(|l| {
            let rest = l.trim_start().strip_prefix('[')?;
            let rest = rest.split_once(']')?.1;
            let mut f = rest.split_whitespace();
            let name = f.next()?.to_owned();
            let _type = f.next()?;
            let _addr = f.next()?;
            let off = u64::from_str_radix(f.next()?, 16).ok()?;
            let size = u64::from_str_radix(f.next()?, 16).ok()?;
            Some((name, off, size))
        })
        .collect()
}

/// zlib's CRC-32 (the one `.gnu_debuglink` stores), with a 256-entry table (critic LOW-4).
pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut t = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            t[i] = c;
            i += 1;
        }
        t
    };
    !bytes.iter().fold(!0u32, |c, &b| {
        TABLE[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8)
    })
}

/// Proves that `shipped` and `names` are the two halves of `built` (the build output):
/// (1) the three build-ids are equal; (2) the shipped file's `.gnu_debuglink` names
/// `kourier-<build-id>.debug`, that is the names file's basename, and its CRC is the CRC-32 of the
/// names file's bytes; (3) the shipped file has no `.symtab`, `.strtab` and no `.debug_*`;
/// (4) `nm -D --defined-only` of the shipped file equals the build output's and lists no
/// OpenSSL-named symbol; (5) the build output has symbols and the names file holds as many (nothing was
/// lost). The first failed check is the error.
pub(crate) fn check_pair(built: &Path, shipped: &Path, names: &Path) -> Result<()> {
    // (1)
    let id = build_id(built)?;
    let shipped_id = build_id(shipped)?;
    let names_id = build_id(names)?;
    if shipped_id != id || names_id != id {
        bail!(
            "the build-ids differ: build output {id}, shipped file {shipped_id}, names file {names_id}"
        );
    }
    // (2)
    let (link_name, link_crc) = debuglink(shipped)?;
    let expected = names_file_name(&id);
    let names_base = names
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if link_name != expected || names_base != expected {
        bail!(
            "the debuglink names `{link_name}` and the names file is `{names_base}`; both must be `{expected}`"
        );
    }
    let bytes = std::fs::read(names).with_context(|| format!("read {}", names.display()))?;
    let crc = crc32(&bytes);
    if link_crc != crc {
        bail!("the debuglink CRC {link_crc:#010x} is not the names file's CRC {crc:#010x}");
    }
    // (3)
    let kept = section_names(&readelf(shipped, "-SW")?);
    if let Some(s) = kept
        .iter()
        .find(|s| *s == ".symtab" || *s == ".strtab" || is_debug_section(s))
    {
        bail!("the shipped file still has the section {s}");
    }
    // (4)
    let dyn_built = nm(built, &["-D", "--defined-only"])?;
    let dyn_shipped = nm(shipped, &["-D", "--defined-only"])?;
    if dyn_built != dyn_shipped {
        bail!("the dynamic symbol table of the shipped file differs from the build output's");
    }
    let bad = crate::exported_openssl_symbols(&dyn_shipped);
    if !bad.is_empty() {
        bail!(
            "the shipped file exports OpenSSL-named symbols: {}",
            bad.join(", ")
        );
    }
    // (5)
    let (n_built, n_names) = (
        nm(built, &["--defined-only"])?.lines().count(),
        nm(names, &["--defined-only"])?.lines().count(),
    );
    if n_built == 0 {
        bail!("the build output has no symbols (no symbol table): the names file would be empty");
    }
    if n_built != n_names {
        bail!("the names file holds {n_names} symbols, the build output {n_built}");
    }
    Ok(())
}

/// The first line of `objcopy --version`, for the run's output (critic LOW-2).
pub(crate) fn objcopy_version() -> Result<String> {
    let out = run(Command::new("objcopy").arg("--version"))?;
    Ok(out.lines().next().unwrap_or_default().trim().to_owned())
}

fn readelf(path: &Path, flag: &str) -> Result<String> {
    run(Command::new("readelf").arg(flag).arg(path))
}

fn nm(path: &Path, flags: &[&str]) -> Result<String> {
    run(Command::new("nm").args(flags).arg(path))
}

fn len(path: &Path) -> Result<u64> {
    Ok(std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .len())
}

/// Runs a binutils command; its stdout, or an error with its stderr.
fn run(cmd: &mut Command) -> Result<String> {
    let name = cmd.get_program().to_string_lossy().into_owned();
    let out = cmd.output().with_context(|| format!("run {name}"))?;
    if !out.status.success() {
        bail!(
            "{name} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_is_zlibs() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn the_build_id_is_read_from_readelf_notes() {
        let text = "Displaying notes found in: .note.gnu.build-id\n  Owner  Data size\tDescription\n  GNU   0x00000014\tNT_GNU_BUILD_ID (unique build ID bitstring)\n    Build ID: 37989D9de15c02fda4cb137b139c8a3ca9ac4e9d\n";
        assert_eq!(
            parse_build_id(text).as_deref(),
            Some("37989d9de15c02fda4cb137b139c8a3ca9ac4e9d")
        );
        assert_eq!(parse_build_id("Build ID: \nnothing"), None);
        assert_eq!(parse_build_id("Build ID: xyz"), None);
        assert_eq!(parse_build_id(""), None);
    }

    #[test]
    fn the_debuglink_is_read_from_readelf_links() {
        let text = "b: Found separate debug info file: a.debug\nContents of the .gnu_debuglink section (loaded from b):\n\n  Separate debug info file: kourier-ab12.debug\n  CRC value: 0x8cbf1699\n";
        assert_eq!(
            parse_debuglink(text),
            Some(("kourier-ab12.debug".to_owned(), 0x8cbf_1699))
        );
        assert_eq!(parse_debuglink("nothing"), None);
        assert_eq!(parse_debuglink("  Separate debug info file: x\n"), None);
    }

    #[test]
    fn the_sections_are_read_from_readelf_s() {
        let text = "Section Headers:\n  [Nr] Name              Type            Address          Off    Size   ES Flg Lk Inf Al\n  [ 0]                   NULL            0000000000000000 000000 000000 00      0   0  0\n  [ 3] .note.gnu.build-id NOTE            0000000000000368 000368 000024 00   A  0   0  4\n  [26] .symtab           SYMTAB          0000000000000000 003040 000618 18     27  44  8\n  [27] .debug_info       PROGBITS        0000000000000000 004000 000010 00      0   0  1\n";
        let s = sections(text);
        assert!(s.contains(&(".symtab".to_owned(), 0x3040, 0x618)));
        let names = section_names(text);
        assert!(names.contains(&".note.gnu.build-id".to_owned()));
        assert!(names.iter().any(|n| is_debug_section(n)));
        assert!(!is_debug_section(".symtab"));
        assert!(is_debug_section(".zdebug_info"));
    }

    #[test]
    fn the_names_file_is_named_by_the_build() {
        assert_eq!(names_file_name("ab12"), "kourier-ab12.debug");
    }

    // ---- the real tools on a compiled toy program ----

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = crate::target_dir()
                .join("tmp/xtask-names")
                .join(format!("{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch directory");
            Self(dir)
        }
        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Compiles `int main(void){return X;}` with `extra` flags; the program's value is `x`.
    fn compile(dir: &Scratch, out: &str, x: u32, extra: &[&str]) -> PathBuf {
        let src = dir.path("t.c");
        std::fs::write(&src, "int main(void){return X;}\n").expect("source");
        let bin = dir.path(out);
        run(Command::new("cc")
            .args(extra)
            .arg(format!("-DX={x}"))
            .arg("-Wl,--build-id")
            .arg(&src)
            .arg("-o")
            .arg(&bin))
        .expect("cc");
        bin
    }

    #[test]
    fn a_split_keeps_the_build_id_and_the_tie_checks_out() {
        let s = Scratch::new("tie");
        let built = compile(&s, "built", 0, &["-g0"]);
        let a = split(&built, &s.path("a")).expect("split");
        let b = split(&built, &s.path("b")).expect("split again");
        // Deterministic: the same input gives the same bytes.
        assert_eq!(
            std::fs::read(&a.shipped).unwrap(),
            std::fs::read(&b.shipped).unwrap()
        );
        assert_eq!(
            std::fs::read(&a.names).unwrap(),
            std::fs::read(&b.names).unwrap()
        );
        let id = build_id(&built).unwrap();
        assert_eq!(a.build_id, id);
        assert_eq!(build_id(&a.shipped).unwrap(), id);
        assert_eq!(build_id(&a.names).unwrap(), id);
        assert_eq!(
            a.names.file_name().unwrap().to_string_lossy(),
            names_file_name(&id)
        );
        assert_eq!(
            debuglink(&a.shipped).unwrap(),
            (names_file_name(&id), a.crc)
        );
        assert_eq!(a.crc, crc32(&std::fs::read(&a.names).unwrap()));
        assert!(a.shipped_bytes < std::fs::metadata(&built).unwrap().len());
        assert_eq!(
            a.shipped_bytes,
            std::fs::metadata(&a.shipped).unwrap().len()
        );
        assert!(a.names_bytes > 0);
        check_pair(&built, &a.shipped, &a.names).expect("a good pair");
        // The input is untouched.
        assert!(section_names(&readelf(&built, "-SW").unwrap()).contains(&".symtab".to_owned()));
    }

    #[test]
    fn an_input_with_debug_information_is_refused() {
        let s = Scratch::new("dwarf");
        let built = compile(&s, "built", 0, &["-g"]);
        let err = split(&built, &s.path("out")).expect_err("DWARF refused");
        assert!(err.to_string().contains("debug information"), "{err}");
        assert!(!s.path("out").exists(), "nothing was written");
    }

    #[test]
    fn an_input_without_a_build_id_is_refused() {
        let s = Scratch::new("noid");
        let src = s.path("t.c");
        std::fs::write(&src, "int main(void){return 0;}\n").unwrap();
        let bin = s.path("built");
        run(Command::new("cc")
            .args(["-g0", "-Wl,--build-id=none"])
            .arg(&src)
            .arg("-o")
            .arg(&bin))
        .expect("cc");
        let err = split(&bin, &s.path("out")).expect_err("no build-id");
        assert!(err.to_string().contains("build-id"), "{err}");
    }

    #[test]
    fn a_names_file_with_one_byte_flipped_fails_on_the_crc() {
        let s = Scratch::new("flip");
        let built = compile(&s, "built", 0, &["-g0"]);
        let a = split(&built, &s.path("a")).expect("split");
        // One byte inside .strtab: the build-id, the section table and the symbol count stay, so
        // only the CRC can tell.
        let (_, off, size) = sections(&readelf(&a.names, "-SW").unwrap())
            .into_iter()
            .find(|s| s.0 == ".strtab")
            .expect(".strtab");
        assert!(size > 2);
        let mut bytes = std::fs::read(&a.names).unwrap();
        bytes[off as usize + 1] ^= 0x01;
        std::fs::write(&a.names, bytes).unwrap();
        let err = check_pair(&built, &a.shipped, &a.names).expect_err("CRC mismatch");
        assert!(err.to_string().contains("CRC"), "{err}");
    }

    #[test]
    fn a_pair_from_another_build_fails_on_the_build_id() {
        let s = Scratch::new("other");
        let one = compile(&s, "one", 0, &["-g0"]);
        let a = split(&one, &s.path("a")).expect("split");
        let two = compile(&s, "two", 1, &["-g0"]);
        let b = split(&two, &s.path("b")).expect("split other");
        // The two builds really differ, or the failures below would prove nothing.
        assert_ne!(a.build_id, b.build_id);
        check_pair(&one, &a.shipped, &a.names).expect("pair one");
        check_pair(&two, &b.shipped, &b.names).expect("pair two");
        // Names from another build.
        let err = check_pair(&one, &a.shipped, &b.names).expect_err("other names");
        assert!(err.to_string().contains("build-id"), "{err}");
        // A shipped file from another build.
        let err = check_pair(&two, &a.shipped, &b.names).expect_err("other shipped");
        assert!(err.to_string().contains("build-id"), "{err}");
        // A build output from another build.
        let err = check_pair(&two, &b.shipped, &a.names).expect_err("other names, same shipped");
        assert!(err.to_string().contains("build-id"), "{err}");
    }

    #[test]
    fn a_shipped_file_that_kept_its_symbols_fails() {
        let s = Scratch::new("kept");
        let built = compile(&s, "built", 0, &["-g0"]);
        let a = split(&built, &s.path("a")).expect("split");
        // The unstripped build output in place of the shipped file: same id, no debuglink.
        let err = check_pair(&built, &built, &a.names).expect_err("not stripped");
        assert!(err.to_string().contains("debuglink"), "{err}");
    }

    #[test]
    fn a_renamed_names_file_fails_on_the_debuglink_name() {
        let s = Scratch::new("renamed");
        let built = compile(&s, "built", 0, &["-g0"]);
        let a = split(&built, &s.path("a")).expect("split");
        let moved = s.path("a/kourier.debug");
        std::fs::rename(&a.names, &moved).unwrap();
        let err = check_pair(&built, &a.shipped, &moved).expect_err("wrong name");
        assert!(err.to_string().contains("debuglink names"), "{err}");
    }

    /// Like [`compile`] with a source text of its own.
    fn compile_src(dir: &Scratch, out: &str, source: &str, extra: &[&str]) -> PathBuf {
        let src = dir.path(&format!("{out}.c"));
        std::fs::write(&src, source).expect("source");
        let bin = dir.path(out);
        run(Command::new("cc")
            .args(extra)
            .arg("-Wl,--build-id")
            .arg(&src)
            .arg("-o")
            .arg(&bin))
        .expect("cc");
        bin
    }

    #[test]
    fn an_input_without_a_symbol_table_is_refused() {
        let s = Scratch::new("nosym");
        let built = compile(&s, "built", 0, &["-g0", "-s"]);
        let err = split(&built, &s.path("out")).expect_err("no .symtab");
        assert!(err.to_string().contains("symbol table"), "{err}");
        assert!(err.to_string().contains("debuginfo"), "{err}");
        assert!(!s.path("out").exists(), "nothing was written");
    }

    #[test]
    fn a_pair_of_a_build_without_symbols_fails_on_the_count() {
        let s = Scratch::new("zero");
        let built = compile(&s, "built", 0, &["-g0", "-s"]);
        // What an unguarded split would have made: both halves of an empty table.
        let names = s.path(&names_file_name(&build_id(&built).unwrap()));
        run(Command::new("objcopy")
            .arg("--only-keep-debug")
            .arg(&built)
            .arg(&names))
        .unwrap();
        let shipped = s.path("shipped");
        run(Command::new("objcopy")
            .arg("--strip-all")
            .arg(format!("--add-gnu-debuglink={}", names.display()))
            .arg(&built)
            .arg(&shipped))
        .unwrap();
        let err = check_pair(&built, &shipped, &names).expect_err("0 == 0 must not pass");
        assert!(err.to_string().contains("no symbols"), "{err}");
    }

    #[test]
    fn a_shipped_file_with_its_symtab_but_a_right_debuglink_fails_on_the_section() {
        let s = Scratch::new("symtab");
        let built = compile(&s, "built", 0, &["-g0"]);
        let a = split(&built, &s.path("a")).expect("split");
        // Debuglink and CRC right, build-id equal, but no --strip-all.
        let kept = s.path("kept");
        run(Command::new("objcopy")
            .arg(format!("--add-gnu-debuglink={}", a.names.display()))
            .arg(&built)
            .arg(&kept))
        .unwrap();
        let err = check_pair(&built, &kept, &a.names).expect_err("symtab kept");
        assert!(
            err.to_string().contains("still has the section .symtab"),
            "{err}"
        );
    }

    #[test]
    fn a_shipped_file_that_exports_an_openssl_name_fails() {
        let s = Scratch::new("ssl");
        let built = compile_src(
            &s,
            "built",
            "int SSL_CTX_new(void){return 1;}\nint main(void){return SSL_CTX_new();}\n",
            &["-g0", "-rdynamic"],
        );
        let a = split(&built, &s.path("a")).expect("split");
        let err = check_pair(&built, &a.shipped, &a.names).expect_err("OpenSSL-named");
        assert!(err.to_string().contains("OpenSSL-named"), "{err}");
        assert!(err.to_string().contains("SSL_CTX_new"), "{err}");
    }

    #[test]
    fn a_names_file_that_lost_a_symbol_fails_on_the_count() {
        let s = Scratch::new("lost");
        let built = compile(&s, "built", 0, &["-g0"]);
        let id = build_id(&built).unwrap();
        // A names file without `main`, a shipped file linked to it (so the CRC is right).
        let names = s.path(&names_file_name(&id));
        run(Command::new("objcopy")
            .arg("--only-keep-debug")
            .arg("--strip-symbol=main")
            .arg(&built)
            .arg(&names))
        .unwrap();
        let shipped = s.path("shipped");
        run(Command::new("objcopy")
            .arg("--strip-all")
            .arg(format!("--add-gnu-debuglink={}", names.display()))
            .arg(&built)
            .arg(&shipped))
        .unwrap();
        let err = check_pair(&built, &shipped, &names).expect_err("a symbol lost");
        assert!(err.to_string().contains("holds"), "{err}");
        assert!(err.to_string().contains("symbols"), "{err}");
    }

    #[test]
    fn objcopy_names_itself() {
        assert!(objcopy_version().unwrap().contains("objcopy"));
    }
}

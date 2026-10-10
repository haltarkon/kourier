//! `cargo xtask install` and `uninstall`: a local install under a prefix in the XDG layout
//! (architecture §11 S16).
//!
//! ```text
//! <prefix>/bin/kourier
//! <prefix>/share/applications/<id>.desktop        Exec and TryExec name the installed binary
//! <prefix>/share/icons/hicolor/scalable/apps/<id>.svg
//! <prefix>/share/metainfo/<id>.metainfo.xml
//! <prefix>/share/locale/<lang>/LC_MESSAGES/kourier.mo     one per language of po/LINGUAS
//! ```
//!
//! `--user` is the prefix `~/.local`. The sources are `packaging/<id>.{desktop,svg,metainfo.xml}`
//! and `target/release/kourier`. The application id is [`APP_ID`] and nothing else here names
//! it: a rename changes that constant, the three file names in `packaging/` and the ids inside
//! them, and a test says which of them was forgotten.
//!
//! The desktop file declares Kourier as a handler of `sgnl://` and `signalcaptcha://` links
//! (`MimeType=`). That line reaches beyond the prefix: next to an installed Signal Desktop it
//! makes two candidates for those links, and which one the desktop then starts is not ours to
//! decide. So the line is installed only when no other application's desktop entry claims
//! one of the two schemes ([`other_handler`]); `--scheme-handlers` installs it regardless and
//! `--no-scheme-handlers` never.
//!
//! What an install may touch: the four files above, written through a temporary file beside
//! each, and the directories that lead to them. A `bin/kourier` that is already there and
//! that no desktop entry of ours in the same prefix starts is somebody else's program: an
//! install refuses to replace it and an uninstall leaves it alone. An uninstall removes the
//! four files and the directories that are empty afterwards, never the prefix itself.
//! `--update-caches` runs the desktop's cache tools on the prefix; the cache files they write
//! (`mimeinfo.cache`) are theirs and stay.

use anyhow::{Context, Result, bail};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{cargo, root, run, target_dir};

/// The application id: desktop file name, icon name, AppStream id (the same string as
/// `kourier_desktop::APP_ID`; a test compares them).
pub const APP_ID: &str = "app.kourier.Kourier";

/// The installed program's file name.
pub const BINARY: &str = "kourier";

/// A text only a measuring build (`cargo xtask perf`, cargo features `dumpable` and `perf`)
/// holds: the start of what it prints when it starts
/// (`kourier_app::process::MEASURING_BUILD_NOTE`; a test compares the two). Such a build stays
/// dumpable (architecture §9.4) and must never be installed.
const PERF_BUILD_MARKER: &[u8] = b"kourier: measuring build (cargo feature dumpable)";

const USAGE: &str = "usage: cargo xtask install (--prefix <dir> | --user) [--no-build] \
[--scheme-handlers | --no-scheme-handlers] [--update-caches]
       cargo xtask uninstall (--prefix <dir> | --user) [--update-caches]

  --prefix <dir>         install below <dir> (bin/, share/applications/, share/icons/, share/metainfo/)
  --user                 the prefix ~/.local
  --no-build             install target/release/kourier as it is (default: build it first)
  --scheme-handlers      offer Kourier for sgnl:// and signalcaptcha:// links (the MimeType line
                         of the desktop file) even when another application, such as Signal
                         Desktop, already does
  --no-scheme-handlers   never offer Kourier for those links
                         (default: offer it unless another application's desktop entry does)
  --update-caches        run update-desktop-database on the prefix afterwards, and
                         gtk-update-icon-cache when the prefix has an icon cache already; the
                         cache files they write stay after an uninstall";

/// The URI schemes the desktop entry claims (`MimeType=x-scheme-handler/<scheme>`).
pub const SCHEMES: [&str; 2] = ["sgnl", "signalcaptcha"];

/// Whether the installed desktop entry offers Kourier for the [`SCHEMES`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemeHandlers {
    /// Unless another application's desktop entry already claims one of them.
    Auto,
    /// Always.
    On,
    /// Never.
    Off,
}

/// What `install` and `uninstall` were asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// Where to install.
    pub prefix: PathBuf,
    /// Build the release binary first.
    pub build: bool,
    /// The `MimeType=` line (the `sgnl://` and `signalcaptcha://` handlers).
    pub scheme_handlers: SchemeHandlers,
    /// Refresh the desktop and icon caches of the prefix afterwards.
    pub update_caches: bool,
}

/// Parses the arguments of `install` and `uninstall`. `home` is `$HOME` (for `--user`).
pub fn parse_options(args: &[String], home: Option<&Path>) -> Result<Options> {
    let mut prefix: Option<PathBuf> = None;
    let mut opts = Options {
        prefix: PathBuf::new(),
        build: true,
        scheme_handlers: SchemeHandlers::Auto,
        update_caches: false,
    };
    let mut handlers: Option<SchemeHandlers> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let chosen = match arg.as_str() {
            "--user" => Some(
                home.filter(|h| h.is_absolute())
                    .context("--user needs HOME")?
                    .join(".local"),
            ),
            "--prefix" => Some(PathBuf::from(
                it.next().context("--prefix needs a directory")?,
            )),
            "--no-build" => {
                opts.build = false;
                None
            }
            "--scheme-handlers" | "--no-scheme-handlers" => {
                let asked = if arg == "--scheme-handlers" {
                    SchemeHandlers::On
                } else {
                    SchemeHandlers::Off
                };
                if handlers.is_some_and(|h| h != asked) {
                    bail!("give one of --scheme-handlers and --no-scheme-handlers\n{USAGE}");
                }
                handlers = Some(asked);
                None
            }
            "--update-caches" => {
                opts.update_caches = true;
                None
            }
            other => match other.strip_prefix("--prefix=") {
                Some(dir) => Some(PathBuf::from(dir)),
                None => bail!("unknown argument {other:?}\n{USAGE}"),
            },
        };
        if let Some(dir) = chosen {
            if prefix.is_some() {
                bail!("give one of --prefix and --user, once\n{USAGE}");
            }
            if dir.as_os_str().is_empty() {
                bail!("--prefix needs a directory\n{USAGE}");
            }
            prefix = Some(dir);
        }
    }
    opts.scheme_handlers = handlers.unwrap_or(SchemeHandlers::Auto);
    let prefix =
        prefix.with_context(|| format!("where to? Give --prefix <dir> or --user\n{USAGE}"))?;
    opts.prefix = std::path::absolute(&prefix)
        .with_context(|| format!("make {} absolute", prefix.display()))?;
    Ok(opts)
}

/// The installed files of a prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// `bin/kourier`.
    pub binary: PathBuf,
    /// `share/applications/<id>.desktop`.
    pub desktop: PathBuf,
    /// `share/icons/hicolor/scalable/apps/<id>.svg`.
    pub icon: PathBuf,
    /// `share/metainfo/<id>.metainfo.xml`.
    pub metainfo: PathBuf,
}

impl Layout {
    /// The layout below `prefix`.
    pub fn new(prefix: &Path) -> Self {
        let share = prefix.join("share");
        Self {
            binary: prefix.join("bin").join(BINARY),
            desktop: share.join("applications").join(format!("{APP_ID}.desktop")),
            icon: share
                .join("icons/hicolor/scalable/apps")
                .join(format!("{APP_ID}.svg")),
            metainfo: share
                .join("metainfo")
                .join(format!("{APP_ID}.metainfo.xml")),
        }
    }

    /// Every installed file, the binary first.
    pub fn files(&self) -> [&Path; 4] {
        [&self.binary, &self.desktop, &self.icon, &self.metainfo]
    }
}

/// The files an install copies from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sources {
    /// The release binary.
    pub binary: PathBuf,
    /// The desktop entry.
    pub desktop: PathBuf,
    /// The scalable icon.
    pub icon: PathBuf,
    /// The AppStream metainfo.
    pub metainfo: PathBuf,
}

impl Sources {
    /// `packaging/` of the repository at `root` and the release binary in `target`.
    pub fn in_repo(root: &Path, target: &Path) -> Self {
        let packaging = root.join("packaging");
        Self {
            binary: target.join("release").join(BINARY),
            desktop: packaging.join(format!("{APP_ID}.desktop")),
            icon: packaging.join(format!("{APP_ID}.svg")),
            metainfo: packaging.join(format!("{APP_ID}.metainfo.xml")),
        }
    }
}

/// `path` as the program of an `Exec=` value (Desktop Entry Specification, "The Exec key"):
/// quoted when it holds a reserved character, with `"`, `` ` ``, `$` and `\` escaped inside the
/// quotes, the backslashes doubled once more for the string value, and `%` doubled because a
/// single one starts a field code.
pub fn exec_program(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .with_context(|| format!("{} is not UTF-8", path.display()))?;
    if text.chars().any(|c| c.is_control()) {
        bail!("{} has a control character in it", path.display());
    }
    const RESERVED: &str = " \t\"'\\><~|&;$*?#()`";
    let quote = text.chars().any(|c| RESERVED.contains(c));
    let mut out = String::new();
    if quote {
        out.push('"');
    }
    for c in text.chars() {
        match c {
            '"' | '`' | '$' if quote => {
                out.push_str("\\\\");
                out.push(c);
            }
            // Inside quotes a backslash is escaped for the shell-like quoting rule and both
            // backslashes again for the desktop file's string escapes.
            '\\' if quote => out.push_str("\\\\\\\\"),
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
    }
    if quote {
        out.push('"');
    }
    Ok(out)
}

/// `path` as a desktop file string value (`TryExec=`): backslashes doubled.
fn string_value(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .with_context(|| format!("{} is not UTF-8", path.display()))?;
    if text.chars().any(|c| c.is_control()) {
        bail!("{} has a control character in it", path.display());
    }
    Ok(text.replace('\\', "\\\\"))
}

/// The desktop entry to install: `source` with `Exec=` and `TryExec=` naming `binary` (a
/// desktop's launcher has no `~/.local/bin` on its `PATH` on every system), and without the
/// `MimeType=` line when `scheme_handlers` is off. Fails when `source` has no `Exec=` line.
pub fn desktop_entry(source: &str, binary: &Path, scheme_handlers: bool) -> Result<String> {
    let program = exec_program(binary)?;
    let try_exec = string_value(binary)?;
    let mut out = String::with_capacity(source.len() + 2 * program.len());
    let mut exec_lines = 0;
    for line in source.lines() {
        if let Some(value) = line.strip_prefix("Exec=") {
            // Keep the arguments (`%U`): only the program changes.
            let args = value.split_once(char::is_whitespace).map_or("", |(_, a)| a);
            out.push_str("Exec=");
            out.push_str(&program);
            if !args.trim().is_empty() {
                out.push(' ');
                out.push_str(args.trim());
            }
            exec_lines += 1;
        } else if line.starts_with("TryExec=") {
            out.push_str("TryExec=");
            out.push_str(&try_exec);
        } else if line.starts_with("MimeType=") && !scheme_handlers {
            continue;
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if exec_lines != 1 {
        bail!("the desktop entry must have exactly one Exec line (found {exec_lines})");
    }
    Ok(out)
}

fn create_dirs(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))
}

/// Writes `bytes` to `path` with `mode` through a temporary file next to it, so a running
/// program is replaced in one step and never seen half written.
fn write_file(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let dir = path.parent().context("a file has a directory")?;
    create_dirs(dir)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("a file has a name")?;
    let tmp = dir.join(format!(".{name}.xtask-install"));
    // What an interrupted install left there goes first, and the file is made anew: a link
    // with this name would otherwise be written through, to wherever it points.
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("remove {}", tmp.display())),
    }
    let written = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("write {}", tmp.display()))?;
        drop(file);
        // The mode asked for, whatever the umask made of it.
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("set the mode of {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

fn read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("read {}", path.display()))
}

/// Whether `binary` is a measuring build (it knows the perf flags).
pub fn is_perf_build(binary: &[u8]) -> bool {
    binary
        .windows(PERF_BUILD_MARKER.len())
        .any(|w| w == PERF_BUILD_MARKER)
}

/// Whether the files below a prefix are an install of ours: its desktop entry is there and
/// starts the binary of this layout.
pub fn installed_here(layout: &Layout) -> bool {
    let (Ok(entry), Ok(binary)) = (
        std::fs::read_to_string(&layout.desktop),
        string_value(&layout.binary),
    ) else {
        return false;
    };
    let try_exec = format!("TryExec={binary}");
    entry.lines().any(|l| l == try_exec)
}

/// The desktop entry of another application that claims one of the [`SCHEMES`], looked for
/// in the `applications` directories `dirs` and one level below them (`applications/kde/`).
/// Kourier's own entry does not count.
pub fn other_handler(dirs: &[PathBuf]) -> Option<PathBuf> {
    fn claims(path: &Path) -> bool {
        let Ok(text) = std::fs::read_to_string(path) else {
            return false;
        };
        text.lines()
            .filter_map(|l| l.strip_prefix("MimeType="))
            .flat_map(|types| types.split(';'))
            .filter_map(|t| t.trim().strip_prefix("x-scheme-handler/"))
            .any(|scheme| SCHEMES.contains(&scheme))
    }
    fn scan(dir: &Path, depth: u8) -> Option<PathBuf> {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .collect();
        entries.sort();
        entries.into_iter().find_map(|path| {
            let name = path.file_name()?.to_str()?;
            if path.is_dir() {
                (depth > 0).then(|| scan(&path, depth - 1)).flatten()
            } else if name.ends_with(".desktop")
                && name != format!("{APP_ID}.desktop")
                && claims(&path)
            {
                Some(path)
            } else {
                None
            }
        })
    }
    dirs.iter().find_map(|dir| scan(dir, 1))
}

/// The `applications` directories a desktop reads its entries from: the prefix's own, the
/// user's (`data_home`: `$XDG_DATA_HOME`, else `~/.local/share`), the system's (`data_dirs`:
/// `$XDG_DATA_DIRS`, else `/usr/local/share:/usr/share`) and where Flatpak and Snap export
/// theirs. They are only read.
pub fn application_dirs(
    prefix: &Path,
    home: Option<&Path>,
    data_home: Option<&std::ffi::OsStr>,
    data_dirs: Option<&std::ffi::OsStr>,
) -> Vec<PathBuf> {
    let mut bases = vec![prefix.join("share")];
    let data_home = data_home
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".local/share")));
    if let Some(dir) = &data_home {
        bases.push(dir.clone());
        bases.push(dir.join("flatpak/exports/share"));
    }
    match data_dirs.filter(|d| !d.is_empty()) {
        Some(dirs) => bases.extend(std::env::split_paths(dirs)),
        None => bases.extend(["/usr/local/share", "/usr/share"].map(PathBuf::from)),
    }
    bases.push(PathBuf::from("/var/lib/flatpak/exports/share"));
    let mut out: Vec<PathBuf> = bases.into_iter().map(|b| b.join("applications")).collect();
    out.push(PathBuf::from("/var/lib/snapd/desktop/applications"));
    let mut seen = std::collections::BTreeSet::new();
    out.retain(|dir| dir.is_absolute() && seen.insert(dir.clone()));
    out
}

/// Copies the desktop entry, the icon and the metainfo (0644) and the binary (0755) from
/// `src` into the layout below `prefix`. Returns the layout. Nothing is written when a source
/// is missing, when the binary is a measuring build, or when `bin/kourier` is there already
/// and is not ours ([`installed_here`]).
pub fn install_files(src: &Sources, prefix: &Path, scheme_handlers: bool) -> Result<Layout> {
    let layout = Layout::new(prefix);
    let binary = read(&src.binary)?;
    if is_perf_build(&binary) {
        bail!(
            "{} is a measuring build (cargo feature `perf`): it stays dumpable and is not for \
             daily use. Build the release binary again: cargo build --release -p kourier",
            src.binary.display()
        );
    }
    let desktop = String::from_utf8(read(&src.desktop)?)
        .with_context(|| format!("{} is not UTF-8", src.desktop.display()))?;
    let desktop = desktop_entry(&desktop, &layout.binary, scheme_handlers)?;
    let icon = read(&src.icon)?;
    let metainfo = read(&src.metainfo)?;
    if std::fs::symlink_metadata(&layout.binary).is_ok() && !installed_here(&layout) {
        bail!(
            "{} is there already and no desktop entry of Kourier in this prefix starts it: it \
             is not from `cargo xtask install`. Move it away or choose another prefix",
            layout.binary.display()
        );
    }
    // The desktop entry first and the binary last: an install that stops half way leaves an
    // entry whose `TryExec` hides it, and the next install or uninstall knows the files as
    // ours.
    write_file(&layout.desktop, desktop.as_bytes(), 0o644)?;
    write_file(&layout.icon, &icon, 0o644)?;
    write_file(&layout.metainfo, &metainfo, 0o644)?;
    write_file(&layout.binary, &binary, 0o755)?;
    Ok(layout)
}

/// Copies the compiled catalog of each language of `langs` from `built`
/// (`<built>/<lang>/LC_MESSAGES/kourier.mo`, made by `cargo xtask i18n build`) to
/// `<prefix>/share/locale/<lang>/LC_MESSAGES/kourier.mo` (0644). The pseudo-language is not
/// installed. Returns the installed files.
pub fn install_catalogs(built: &Path, langs: &[String], prefix: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for lang in langs {
        let dest = catalog_path(prefix, lang);
        write_file(&dest, &read(&catalog_path_in(built, lang))?, 0o644)?;
        out.push(dest);
    }
    Ok(out)
}

/// `<root>/<lang>/LC_MESSAGES/kourier.mo`.
fn catalog_path_in(root: &Path, lang: &str) -> PathBuf {
    root.join(lang).join("LC_MESSAGES").join("kourier.mo")
}

/// `<prefix>/share/locale/<lang>/LC_MESSAGES/kourier.mo`.
fn catalog_path(prefix: &Path, lang: &str) -> PathBuf {
    catalog_path_in(&prefix.join("share/locale"), lang)
}

/// Removes every `kourier.mo` below `<prefix>/share/locale` and the directories that are
/// empty afterwards, up to but not including the prefix. Returns the removed files and
/// directories.
pub fn uninstall_catalogs(prefix: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let locale = prefix.join("share/locale");
    let (mut files, mut directories) = (Vec::new(), Vec::new());
    let Ok(langs) = std::fs::read_dir(&locale) else {
        return Ok((files, directories));
    };
    let mut langs: Vec<PathBuf> = langs.filter_map(|e| e.ok().map(|e| e.path())).collect();
    langs.sort();
    for lang in langs {
        let file = lang.join("LC_MESSAGES").join("kourier.mo");
        match std::fs::remove_file(&file) {
            Ok(()) => files.push(file.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("remove {}", file.display())),
        }
        // Only empty directories go: `remove_dir` refuses any other.
        let mut dir = file.parent();
        while let Some(d) = dir.filter(|d| *d != prefix && d.starts_with(prefix)) {
            if std::fs::remove_dir(d).is_err() {
                break;
            }
            directories.push(d.to_path_buf());
            dir = d.parent();
        }
    }
    Ok((files, directories))
}

/// What an uninstall did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Uninstalled {
    /// The files that were removed.
    pub removed: Vec<PathBuf>,
    /// The directories that were empty afterwards and were removed.
    pub directories: Vec<PathBuf>,
    /// `bin/kourier`, when it is there and was left alone because nothing says it is ours.
    pub foreign: Option<PathBuf>,
}

/// Removes the installed files below `prefix` and every directory that is empty afterwards,
/// up to but not including the prefix. `bin/kourier` goes only when the prefix's desktop
/// entry of Kourier starts it ([`installed_here`]).
pub fn uninstall_files(prefix: &Path) -> Result<Uninstalled> {
    let layout = Layout::new(prefix);
    let mut out = Uninstalled::default();
    // Asked before the desktop entry goes.
    let ours = installed_here(&layout);
    for file in layout.files() {
        if file == layout.binary && !ours {
            if std::fs::symlink_metadata(file).is_ok() {
                out.foreign = Some(file.to_path_buf());
            }
            continue;
        }
        match std::fs::remove_file(file) {
            Ok(()) => out.removed.push(file.to_path_buf()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("remove {}", file.display())),
        }
        // Only empty directories go: `remove_dir` refuses any other.
        let mut dir = file.parent();
        while let Some(d) = dir.filter(|d| *d != prefix && d.starts_with(prefix)) {
            if std::fs::remove_dir(d).is_err() {
                break;
            }
            out.directories.push(d.to_path_buf());
            dir = d.parent();
        }
    }
    Ok(out)
}

/// The cache refreshes `--update-caches` runs for `prefix`: program and arguments.
pub fn cache_commands(prefix: &Path) -> Vec<(&'static str, Vec<PathBuf>)> {
    let share = prefix.join("share");
    vec![
        ("update-desktop-database", vec![share.join("applications")]),
        (
            "gtk-update-icon-cache",
            vec![
                PathBuf::from("-q"),
                PathBuf::from("-t"),
                PathBuf::from("-f"),
                share.join("icons/hicolor"),
            ],
        ),
    ]
}

/// Whether a cache refresh of `--update-caches` runs: its directory exists (after an
/// uninstall it may be gone), and for the icon cache the prefix has one already. A new
/// `icon-theme.cache` in a directory other programs put icons into without refreshing it
/// would hide their icons from the toolkits that trust the cache.
pub fn cache_refresh_wanted(program: &str, dir: &Path) -> bool {
    dir.is_dir() && (program != "gtk-update-icon-cache" || dir.join("icon-theme.cache").is_file())
}

fn update_caches(prefix: &Path) {
    for (program, args) in cache_commands(prefix) {
        if !args
            .last()
            .is_some_and(|dir| cache_refresh_wanted(program, dir))
        {
            continue;
        }
        match Command::new(program).args(&args).status() {
            Ok(status) if status.success() => eprintln!("install: ran {program}"),
            Ok(status) => eprintln!("install: {program} failed ({status}); ignored"),
            Err(_) => eprintln!("install: {program} is not installed; skipped"),
        }
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// `cargo xtask install`.
pub fn install(args: &[String]) -> Result<()> {
    let opts = parse_options(args, home().as_deref())?;
    if opts.build {
        run(
            "build the release binary",
            cargo().args(["build", "--release", "-p", "kourier"]),
        )?;
    }
    let src = Sources::in_repo(&root(), &target_dir());
    if !src.binary.is_file() {
        bail!(
            "{} not found; build it first (cargo build --release -p kourier)",
            src.binary.display()
        );
    }
    let other = other_handler(&application_dirs(
        &opts.prefix,
        home().as_deref(),
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("XDG_DATA_DIRS").as_deref(),
    ));
    let scheme_handlers = match opts.scheme_handlers {
        SchemeHandlers::On => true,
        SchemeHandlers::Off => false,
        SchemeHandlers::Auto => other.is_none(),
    };
    // The translations of po/LINGUAS, compiled from the sources, before anything is written.
    let locale = target_dir().join("locale");
    crate::i18n::build(&locale)?;
    let languages = crate::i18n::linguas()?;
    let layout = install_files(&src, &opts.prefix, scheme_handlers)?;
    for file in layout.files() {
        eprintln!("install: {}", file.display());
    }
    for file in install_catalogs(&locale, &languages, &opts.prefix)? {
        eprintln!("install: {}", file.display());
    }
    if opts.update_caches {
        update_caches(&opts.prefix);
    }
    match (scheme_handlers, &other) {
        (true, Some(other)) => eprintln!(
            "install: the desktop entry offers Kourier for sgnl:// and signalcaptcha:// links, \
             and so does {}. Check which one opens them (xdg-mime query default \
             x-scheme-handler/sgnl).",
            other.display()
        ),
        (true, None) => eprintln!(
            "install: the desktop entry offers Kourier for sgnl:// and signalcaptcha:// links."
        ),
        (false, Some(other)) if opts.scheme_handlers == SchemeHandlers::Auto => eprintln!(
            "install: {} already opens sgnl:// or signalcaptcha:// links, so Kourier is \
             installed without offering itself for them (a captcha challenge will not come \
             back to Kourier by itself). Install again with --scheme-handlers to offer it too.",
            other.display()
        ),
        (false, _) => eprintln!(
            "install: Kourier does not offer itself for sgnl:// and signalcaptcha:// links."
        ),
    }
    let bin_dir = layout.binary.parent().unwrap_or(&opts.prefix);
    let on_path =
        std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d == bin_dir));
    if !on_path {
        eprintln!(
            "install: {} is not on PATH; the desktop entry starts Kourier by its full path",
            bin_dir.display()
        );
    }
    Ok(())
}

/// `cargo xtask uninstall`.
pub fn uninstall(args: &[String]) -> Result<()> {
    let opts = parse_options(args, home().as_deref())?;
    let mut done = uninstall_files(&opts.prefix)?;
    let (catalogs, directories) = uninstall_catalogs(&opts.prefix)?;
    done.removed.extend(catalogs);
    done.directories.extend(directories);
    if done.removed.is_empty() {
        eprintln!(
            "uninstall: nothing of Kourier below {}",
            opts.prefix.display()
        );
    }
    for file in &done.removed {
        eprintln!("uninstall: removed {}", file.display());
    }
    for dir in &done.directories {
        eprintln!("uninstall: removed the empty directory {}", dir.display());
    }
    if let Some(file) = &done.foreign {
        eprintln!(
            "uninstall: left {} alone: no desktop entry of Kourier in this prefix starts it, \
             so it is not from `cargo xtask install`",
            file.display()
        );
    }
    if opts.update_caches {
        update_caches(&opts.prefix);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    /// A scratch directory below the target directory (never the user's home), removed on
    /// drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = target_dir()
                .join("tmp/xtask-install")
                .join(format!("{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch directory");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o7777
    }

    /// The repository's packaging files with a stand-in for the release binary.
    fn sources(scratch: &Path, binary: &[u8]) -> Sources {
        let mut src = Sources::in_repo(&root(), scratch);
        std::fs::create_dir_all(scratch.join("release")).expect("dir");
        std::fs::write(&src.binary, binary).expect("binary");
        src.binary = scratch.join("release").join(BINARY);
        src
    }

    /// Every file and directory below `dir`, relative to it.
    fn tree(dir: &Path) -> Vec<String> {
        fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("read_dir").flatten() {
                let path = entry.path();
                out.push(
                    path.strip_prefix(base)
                        .expect("below the base")
                        .display()
                        .to_string(),
                );
                if path.is_dir() {
                    walk(base, &path, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    #[test]
    fn options() {
        let home = Path::new("/home/u");
        let o = parse_options(&args(&["--user"]), Some(home)).expect("user");
        assert_eq!(o.prefix, Path::new("/home/u/.local"));
        assert!(o.build && !o.update_caches);
        assert_eq!(o.scheme_handlers, SchemeHandlers::Auto);
        let o = parse_options(
            &args(&[
                "--prefix",
                "/tmp/p",
                "--no-build",
                "--no-scheme-handlers",
                "--update-caches",
            ]),
            None,
        )
        .expect("prefix");
        assert_eq!(o.prefix, Path::new("/tmp/p"));
        assert!(!o.build && o.update_caches);
        assert_eq!(o.scheme_handlers, SchemeHandlers::Off);
        let on = parse_options(&args(&["--prefix", "/tmp/p", "--scheme-handlers"]), None)
            .expect("handlers on");
        assert_eq!(on.scheme_handlers, SchemeHandlers::On);
        assert_eq!(
            parse_options(&args(&["--prefix=/tmp/q"]), None)
                .expect("prefix=")
                .prefix,
            Path::new("/tmp/q")
        );
        // A relative prefix is taken from the current directory.
        assert!(
            parse_options(&args(&["--prefix", "rel"]), None)
                .expect("relative")
                .prefix
                .is_absolute()
        );
        for bad in [
            &[][..],
            &["--user"],
            &["--prefix"],
            &["--prefix="],
            &["--prefix", "/a", "--prefix", "/b"],
            &[
                "--prefix",
                "/a",
                "--scheme-handlers",
                "--no-scheme-handlers",
            ],
            &["--bogus"],
        ] {
            assert!(parse_options(&args(bad), None).is_err(), "{bad:?}");
        }
        // Never both, even when HOME is known.
        assert!(parse_options(&args(&["--user", "--prefix", "/a"]), Some(home)).is_err());
    }

    #[test]
    fn the_exec_line_names_the_installed_binary() {
        let source = std::fs::read_to_string(Sources::in_repo(&root(), &target_dir()).desktop)
            .expect("the packaged desktop entry");
        let entry = desktop_entry(&source, Path::new("/opt/k/bin/kourier"), true).expect("entry");
        assert!(entry.contains("\nExec=/opt/k/bin/kourier %U\n"), "{entry}");
        assert!(entry.contains("\nTryExec=/opt/k/bin/kourier\n"), "{entry}");
        assert!(
            entry.contains("\nMimeType=x-scheme-handler/sgnl;x-scheme-handler/signalcaptcha;\n")
        );
        // Nothing else changes.
        let changed: Vec<(&str, &str)> = source
            .lines()
            .zip(entry.lines())
            .filter(|(a, b)| a != b)
            .collect();
        assert_eq!(
            changed,
            [
                ("Exec=kourier %U", "Exec=/opt/k/bin/kourier %U"),
                ("TryExec=kourier", "TryExec=/opt/k/bin/kourier"),
            ]
        );
        assert_eq!(source.lines().count(), entry.lines().count());

        let without =
            desktop_entry(&source, Path::new("/opt/k/bin/kourier"), false).expect("entry");
        assert!(!without.contains("MimeType"), "{without}");
        assert!(!without.contains("x-scheme-handler"));
        assert_eq!(without.lines().count() + 1, source.lines().count());

        assert!(desktop_entry("[Desktop Entry]\nName=x\n", Path::new("/b"), true).is_err());
        assert!(desktop_entry("Exec=a\nExec=b\n", Path::new("/b"), true).is_err());
    }

    #[test]
    fn exec_quoting_follows_the_desktop_entry_specification() {
        let q = |p: &str| exec_program(Path::new(p)).expect("quotable");
        assert_eq!(q("/usr/bin/kourier"), "/usr/bin/kourier");
        assert_eq!(
            q("/home/a b/.local/bin/kourier"),
            "\"/home/a b/.local/bin/kourier\""
        );
        assert_eq!(q("/x/$y/k"), "\"/x/\\\\$y/k\"");
        assert_eq!(q("/x/a\\b/k"), "\"/x/a\\\\\\\\b/k\"");
        assert_eq!(q("/x/100%/k"), "/x/100%%/k");
        assert!(exec_program(Path::new("/x/a\nb")).is_err());
        let entry = desktop_entry(
            "Exec=kourier %U\nTryExec=kourier\n",
            Path::new("/a b/k"),
            true,
        )
        .expect("entry");
        assert_eq!(entry, "Exec=\"/a b/k\" %U\nTryExec=/a b/k\n");
    }

    #[test]
    fn catalogs_go_under_share_locale_and_uninstall_takes_them_away() {
        let scratch = Scratch::new("catalogs");
        let built = scratch.0.join("built");
        for lang in ["de", "pt_BR", "x-test"] {
            let file = catalog_path_in(&built, lang);
            std::fs::create_dir_all(file.parent().expect("dir")).expect("dirs");
            std::fs::write(&file, format!("catalog {lang}")).expect("catalog");
        }
        let prefix = scratch.0.join("prefix");
        std::fs::create_dir_all(&prefix).expect("prefix");
        let langs = vec!["de".to_owned(), "pt_BR".to_owned()];
        let files = install_catalogs(&built, &langs, &prefix).expect("install");
        assert_eq!(files.len(), 2);
        assert_eq!(mode(&files[0]), 0o644);
        assert_eq!(
            std::fs::read_to_string(prefix.join("share/locale/pt_BR/LC_MESSAGES/kourier.mo"))
                .expect("pt_BR"),
            "catalog pt_BR"
        );
        // The pseudo-language is not in the list, so it is not installed.
        assert!(!prefix.join("share/locale/x-test").exists());
        // A language with no compiled catalog is an error, and nothing is half written.
        assert!(install_catalogs(&built, &["fr".to_owned()], &prefix).is_err());
        let (removed, dirs) = uninstall_catalogs(&prefix).expect("uninstall");
        assert_eq!(removed.len(), 2);
        assert!(dirs.contains(&prefix.join("share/locale")));
        assert!(tree(&prefix).is_empty(), "{:?}", tree(&prefix));
        // Nothing to remove is not an error.
        assert_eq!(
            uninstall_catalogs(&prefix).expect("again"),
            (vec![], vec![])
        );
    }

    #[test]
    fn install_puts_four_files_with_their_modes_and_uninstall_leaves_nothing() {
        let scratch = Scratch::new("roundtrip");
        let prefix = scratch.0.join("prefix with space");
        std::fs::create_dir_all(&prefix).expect("prefix");
        let src = sources(&scratch.0.join("src"), b"\x7fELF not really");
        let layout = install_files(&src, &prefix, true).expect("install");
        assert_eq!(
            tree(&prefix),
            [
                "bin",
                "bin/kourier",
                "share",
                "share/applications",
                "share/applications/app.kourier.Kourier.desktop",
                "share/icons",
                "share/icons/hicolor",
                "share/icons/hicolor/scalable",
                "share/icons/hicolor/scalable/apps",
                "share/icons/hicolor/scalable/apps/app.kourier.Kourier.svg",
                "share/metainfo",
                "share/metainfo/app.kourier.Kourier.metainfo.xml",
            ]
        );
        assert_eq!(mode(&layout.binary), 0o755);
        for file in [&layout.desktop, &layout.icon, &layout.metainfo] {
            assert_eq!(mode(file), 0o644, "{}", file.display());
        }
        assert_eq!(
            std::fs::read(&layout.binary).expect("binary"),
            b"\x7fELF not really"
        );
        assert_eq!(
            std::fs::read(&layout.icon).expect("icon"),
            std::fs::read(&src.icon).expect("source icon")
        );
        assert_eq!(
            std::fs::read(&layout.metainfo).expect("metainfo"),
            std::fs::read(&src.metainfo).expect("source metainfo")
        );
        let desktop = std::fs::read_to_string(&layout.desktop).expect("desktop");
        let exec = format!("Exec=\"{}\" %U", layout.binary.display());
        assert!(desktop.lines().any(|l| l == exec), "{desktop}");
        let try_exec = format!("TryExec={}", layout.binary.display());
        assert!(desktop.lines().any(|l| l == try_exec), "{desktop}");

        // A second install replaces the files (an update).
        let newer = sources(&scratch.0.join("src2"), b"newer");
        install_files(&newer, &prefix, false).expect("update");
        assert_eq!(std::fs::read(&layout.binary).expect("binary"), b"newer");
        assert_eq!(mode(&layout.binary), 0o755);
        assert!(
            !std::fs::read_to_string(&layout.desktop)
                .expect("desktop")
                .contains("MimeType")
        );
        assert_eq!(tree(&prefix).len(), 12, "no temporary file stays");

        let done = uninstall_files(&prefix).expect("uninstall");
        assert_eq!(done.removed.len(), 4);
        assert_eq!(done.directories.len(), 8, "{:?}", done.directories);
        assert_eq!(done.foreign, None);
        assert!(prefix.is_dir(), "the prefix itself stays");
        assert_eq!(tree(&prefix), Vec::<String>::new(), "nothing is left");
        // Again: nothing to remove, no error.
        assert_eq!(
            uninstall_files(&prefix).expect("again"),
            Uninstalled::default()
        );
    }

    #[test]
    fn a_program_of_the_same_name_is_not_ours() {
        let scratch = Scratch::new("foreign");
        let prefix = scratch.0.join("prefix");
        let src = sources(&scratch.0.join("src"), b"ours");
        // Somebody else's `kourier` in the prefix's bin.
        std::fs::create_dir_all(prefix.join("bin")).expect("bin");
        std::fs::write(prefix.join("bin/kourier"), b"theirs").expect("write");
        let before = tree(&prefix);
        let err = install_files(&src, &prefix, true).expect_err("refused");
        assert!(err.to_string().contains("is there already"), "{err}");
        assert_eq!(tree(&prefix), before, "nothing was written");
        assert_eq!(
            std::fs::read(prefix.join("bin/kourier")).expect("read"),
            b"theirs"
        );
        // A desktop entry of Kourier that starts another binary does not make it ours.
        let entry = desktop_entry(
            "Exec=kourier %U\nTryExec=kourier\n",
            Path::new("/somewhere/else/bin/kourier"),
            true,
        )
        .expect("entry");
        let layout = Layout::new(&prefix);
        std::fs::create_dir_all(layout.desktop.parent().expect("parent")).expect("dir");
        std::fs::write(&layout.desktop, entry).expect("write");
        assert!(!installed_here(&layout));
        assert!(install_files(&src, &prefix, true).is_err());
        // An uninstall takes the entry and leaves the program, and says so.
        let done = uninstall_files(&prefix).expect("uninstall");
        assert_eq!(done.removed, std::slice::from_ref(&layout.desktop));
        assert_eq!(done.foreign.as_deref(), Some(layout.binary.as_path()));
        assert_eq!(tree(&prefix), ["bin", "bin/kourier"]);
        assert_eq!(
            std::fs::read(prefix.join("bin/kourier")).expect("read"),
            b"theirs"
        );
    }

    #[test]
    fn an_install_that_stopped_half_way_can_be_repeated_or_removed() {
        let scratch = Scratch::new("half");
        let prefix = scratch.0.join("prefix");
        let src = sources(&scratch.0.join("src"), b"bin");
        // `bin` is a file: the binary, written last, cannot be written.
        std::fs::create_dir_all(&prefix).expect("prefix");
        std::fs::write(prefix.join("bin"), b"in the way").expect("write");
        assert!(install_files(&src, &prefix, true).is_err());
        let layout = Layout::new(&prefix);
        assert!(layout.desktop.is_file() && layout.icon.is_file() && layout.metainfo.is_file());
        // The obstacle gone, the same install goes through: the entry that is there is ours.
        std::fs::remove_file(prefix.join("bin")).expect("remove");
        install_files(&src, &prefix, true).expect("again");
        assert!(installed_here(&layout));
        // And the half install alone can be removed.
        std::fs::remove_file(&layout.binary).expect("remove the binary");
        std::fs::remove_dir(prefix.join("bin")).expect("remove bin");
        let done = uninstall_files(&prefix).expect("uninstall");
        assert_eq!(done.removed.len(), 3);
        assert_eq!(tree(&prefix), Vec::<String>::new());
    }

    #[test]
    fn a_link_left_as_the_temporary_file_is_not_written_through() {
        let scratch = Scratch::new("link");
        let prefix = scratch.0.join("prefix");
        let src = sources(&scratch.0.join("src"), b"the new binary");
        let outside = scratch.0.join("outside");
        std::fs::write(&outside, b"untouched").expect("write");
        std::fs::create_dir_all(prefix.join("bin")).expect("bin");
        std::os::unix::fs::symlink(&outside, prefix.join("bin/.kourier.xtask-install"))
            .expect("symlink");
        let layout = install_files(&src, &prefix, true).expect("install");
        assert_eq!(std::fs::read(&outside).expect("read"), b"untouched");
        assert_eq!(
            std::fs::read(&layout.binary).expect("read"),
            b"the new binary"
        );
        assert!(
            !std::fs::symlink_metadata(&layout.binary)
                .expect("metadata")
                .file_type()
                .is_symlink()
        );
        assert_eq!(mode(&layout.binary), 0o755);
        assert_eq!(
            tree(&prefix).len(),
            12,
            "the link is gone, no temporary file stays"
        );
    }

    #[test]
    fn another_handler_of_the_schemes_is_found() {
        let scratch = Scratch::new("handlers");
        let (user, system) = (
            scratch.0.join("user/applications"),
            scratch.0.join("sys/applications"),
        );
        std::fs::create_dir_all(user.join("kde")).expect("dirs");
        std::fs::create_dir_all(&system).expect("dirs");
        let write = |path: PathBuf, mime: &str| {
            std::fs::write(path, format!("[Desktop Entry]\nName=x\nMimeType={mime}\n"))
                .expect("write");
        };
        let dirs = [
            user.clone(),
            system.clone(),
            scratch.0.join("missing/applications"),
        ];
        // Nobody: a browser, a mail program, and Kourier's own entry do not count.
        write(
            system.join("browser.desktop"),
            "x-scheme-handler/https;text/html;",
        );
        write(user.join("mail.desktop"), "x-scheme-handler/mailto;");
        write(
            user.join(format!("{APP_ID}.desktop")),
            "x-scheme-handler/sgnl;x-scheme-handler/signalcaptcha;",
        );
        // A scheme that only begins like ours, and a file that is no desktop entry.
        write(system.join("other.desktop"), "x-scheme-handler/sgnlx;");
        write(system.join("notes.txt"), "x-scheme-handler/sgnl;");
        assert_eq!(other_handler(&dirs), None);
        // Signal Desktop in the system's directory.
        write(
            system.join("signal-desktop.desktop"),
            "x-scheme-handler/sgnl;x-scheme-handler/signalcaptcha;",
        );
        assert_eq!(
            other_handler(&dirs),
            Some(system.join("signal-desktop.desktop"))
        );
        // One that claims only the captcha scheme, one directory below the user's.
        write(
            user.join("kde/org.example.Fork.desktop"),
            "text/plain; x-scheme-handler/signalcaptcha ;",
        );
        assert_eq!(
            other_handler(&dirs),
            Some(user.join("kde/org.example.Fork.desktop")),
            "the first directory with one wins"
        );
        assert_eq!(other_handler(&[]), None);
    }

    #[test]
    fn the_directories_a_desktop_reads_entries_from() {
        let os = |s: &'static str| Some(std::ffi::OsStr::new(s));
        let dirs = application_dirs(Path::new("/p"), Some(Path::new("/home/u")), None, None);
        assert_eq!(
            dirs,
            [
                "/p/share/applications",
                "/home/u/.local/share/applications",
                "/home/u/.local/share/flatpak/exports/share/applications",
                "/usr/local/share/applications",
                "/usr/share/applications",
                "/var/lib/flatpak/exports/share/applications",
                "/var/lib/snapd/desktop/applications",
            ]
            .map(PathBuf::from)
        );
        // The environment's directories, each once (`--user`: the prefix is the user's own),
        // relative ones left out.
        let dirs = application_dirs(
            Path::new("/home/u/.local"),
            Some(Path::new("/home/u")),
            os("/home/u/.local/share"),
            os("/usr/share:rel:/usr/share:/opt/share"),
        );
        assert_eq!(
            dirs,
            [
                "/home/u/.local/share/applications",
                "/home/u/.local/share/flatpak/exports/share/applications",
                "/usr/share/applications",
                "/opt/share/applications",
                "/var/lib/flatpak/exports/share/applications",
                "/var/lib/snapd/desktop/applications",
            ]
            .map(PathBuf::from)
        );
        // Without a home only the prefix's and the system's.
        let dirs = application_dirs(Path::new("/p"), None, os(""), os(""));
        assert_eq!(dirs.len(), 5);
        assert_eq!(dirs[0], Path::new("/p/share/applications"));
    }

    #[test]
    fn an_icon_cache_is_refreshed_only_where_there_is_one() {
        let scratch = Scratch::new("caches");
        let hicolor = scratch.0.join("share/icons/hicolor");
        let applications = scratch.0.join("share/applications");
        assert!(!cache_refresh_wanted(
            "update-desktop-database",
            &applications
        ));
        std::fs::create_dir_all(&hicolor).expect("dir");
        std::fs::create_dir_all(&applications).expect("dir");
        assert!(cache_refresh_wanted(
            "update-desktop-database",
            &applications
        ));
        assert!(
            !cache_refresh_wanted("gtk-update-icon-cache", &hicolor),
            "no cache is made where there was none"
        );
        std::fs::write(hicolor.join("icon-theme.cache"), b"cache").expect("write");
        assert!(cache_refresh_wanted("gtk-update-icon-cache", &hicolor));
    }

    #[test]
    fn uninstall_keeps_what_others_put_there() {
        let scratch = Scratch::new("shared");
        let prefix = scratch.0.join("prefix");
        let src = sources(&scratch.0.join("src"), b"bin");
        // Another application's files in the same directories, as in ~/.local.
        for other in [
            "bin/other",
            "share/applications/org.example.Other.desktop",
            "share/icons/hicolor/scalable/apps/org.example.Other.svg",
        ] {
            let path = prefix.join(other);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(path, b"theirs").expect("write");
        }
        install_files(&src, &prefix, true).expect("install");
        uninstall_files(&prefix).expect("uninstall");
        assert_eq!(
            tree(&prefix),
            [
                "bin",
                "bin/other",
                "share",
                "share/applications",
                "share/applications/org.example.Other.desktop",
                "share/icons",
                "share/icons/hicolor",
                "share/icons/hicolor/scalable",
                "share/icons/hicolor/scalable/apps",
                "share/icons/hicolor/scalable/apps/org.example.Other.svg",
            ],
            "only Kourier's files and the directory only it used are gone"
        );
    }

    #[test]
    fn a_measuring_build_is_refused() {
        let scratch = Scratch::new("perf-build");
        let prefix = scratch.0.join("prefix");
        let mut binary = b"\x7fELF....".to_vec();
        binary.extend_from_slice(PERF_BUILD_MARKER);
        binary.extend_from_slice(b": its memory can be read....");
        let src = sources(&scratch.0.join("src"), &binary);
        let err = install_files(&src, &prefix, true).expect_err("refused");
        assert!(err.to_string().contains("measuring build"), "{err}");
        assert!(!prefix.exists(), "nothing was written");
    }

    #[test]
    fn the_marker_is_what_a_measuring_build_prints() {
        let source = std::fs::read_to_string(root().join("crates/kourier-app/src/process.rs"))
            .expect("read the source");
        let marker = std::str::from_utf8(PERF_BUILD_MARKER).expect("UTF-8");
        assert!(
            source.contains(&format!("MEASURING_BUILD_NOTE: &str = \"{marker}")),
            "PERF_BUILD_MARKER must start kourier_app::process::MEASURING_BUILD_NOTE"
        );
        assert!(!is_perf_build(b"an ordinary binary"));
    }

    #[test]
    fn a_missing_source_installs_nothing() {
        let scratch = Scratch::new("missing");
        let prefix = scratch.0.join("prefix");
        let mut src = sources(&scratch.0.join("src"), b"bin");
        src.metainfo = scratch.0.join("nope.xml");
        assert!(install_files(&src, &prefix, true).is_err());
        assert!(
            !prefix.exists(),
            "every source is read before anything is written"
        );
    }

    #[test]
    fn cache_refreshes_stay_inside_the_prefix() {
        let prefix = Path::new("/tmp/p");
        for (program, args) in cache_commands(prefix) {
            let dir = args.last().expect("a directory");
            assert!(dir.starts_with(prefix), "{program} {dir:?}");
        }
    }

    /// The quoted value of `pub const APP_ID: &str = "…";` in a source file.
    fn app_id_in(path: &Path) -> String {
        let source = std::fs::read_to_string(path).expect("read the source");
        source
            .split_once("pub const APP_ID: &str = \"")
            .and_then(|(_, rest)| rest.split_once('"'))
            .map(|(id, _)| id.to_owned())
            .expect("APP_ID in the source")
    }

    #[test]
    fn the_application_id_is_the_same_everywhere() {
        let root = root();
        // The crates define it once, in kourier-desktop (architecture §8.10, R6); the copy
        // here must be that one, and no other crate may spell an id of its own.
        assert_eq!(
            app_id_in(&root.join("crates/kourier-desktop/src/lib.rs")),
            APP_ID
        );
        for other in [
            "kourier-app/src/keys.rs",
            "kourier-app/src/desktop/mod.rs",
            "kourier-qt/src/lib.rs",
        ] {
            let text = std::fs::read_to_string(root.join("crates").join(other)).expect(other);
            assert!(
                !text.contains("const APP_ID"),
                "{other} defines an application id of its own"
            );
        }
        let src = Sources::in_repo(&root, &target_dir());
        for file in [&src.desktop, &src.icon, &src.metainfo] {
            assert!(file.is_file(), "{} is missing", file.display());
        }
        let desktop = std::fs::read_to_string(&src.desktop).expect("desktop");
        assert!(
            desktop.lines().any(|l| l == format!("Icon={APP_ID}")),
            "{desktop}"
        );
        assert!(desktop.lines().any(|l| l == format!("Exec={BINARY} %U")));
        assert!(desktop.lines().any(|l| l == format!("TryExec={BINARY}")));
        let metainfo = std::fs::read_to_string(&src.metainfo).expect("metainfo");
        assert!(
            metainfo.contains(&format!("<id>{APP_ID}</id>")),
            "{metainfo}"
        );
        assert!(metainfo.contains(&format!(
            "<launchable type=\"desktop-id\">{APP_ID}.desktop</launchable>"
        )));
        assert!(metainfo.contains(&format!("<binary>{BINARY}</binary>")));
    }

    /// `desktop-file-validate` and `appstreamcli validate` on the packaged files, when the
    /// tools are installed (they are on the development machine; elsewhere the test says so
    /// and passes).
    #[test]
    fn packaged_files_pass_their_validators() {
        let scratch = Scratch::new("validate");
        let prefix = scratch.0.join("prefix");
        let src = sources(&scratch.0.join("src"), b"bin");
        let layout = install_files(&src, &prefix, true).expect("install");
        let checks: [(&str, Vec<&std::ffi::OsStr>); 2] = [
            ("desktop-file-validate", vec![layout.desktop.as_os_str()]),
            (
                "appstreamcli",
                vec![
                    "validate".as_ref(),
                    "--no-net".as_ref(),
                    // The project has no site or domain of its own yet, so the metainfo names
                    // no homepage; every other finding still fails.
                    "--override=url-homepage-missing=info".as_ref(),
                    layout.metainfo.as_os_str(),
                ],
            ),
        ];
        for (program, args) in checks {
            match Command::new(program).args(&args).output() {
                Ok(out) => assert!(
                    out.status.success(),
                    "{program}: {}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                ),
                Err(_) => eprintln!("{program} is not installed; not checked"),
            }
        }
    }
}

//! "Open in Slicer" below the seam (architecture §9.4): the fixed table of four slicers, the
//! search for the ones that are installed in the applications directories of the XDG
//! base-directory spec, and the command that starts one with a copy of a 3D model.
//!
//! No toolkit type here (§8.10). Everything that looks at the file system ([`scan`]) is meant
//! for a worker thread: the front-end's thread never calls it.
//!
//! - THE TABLE ([`SLICERS`]) is fixed and in his order: OrcaSlicer (the default), Bambu Studio,
//!   PrusaSlicer, UltiMaker Cura. Names are product names and are not translated.
//! - FINDING ONE: a desktop file named after one of the slicer's desktop ids (table order), else
//!   any `*.desktop` whose plain `Name=` is the slicer's and whose `MimeType=` lists a 3D model
//!   type (AppImage integrations). A desktop id found first wins (spec precedence); `Hidden=true`,
//!   a `Type` other than `Application`, no `Exec`, a `TryExec` or an Exec program that is not
//!   there, an oversized file all mean "not installed" ([`DesktopEntry`], bounds below).
//! - THE COMMAND ([`command`]): the Desktop Entry spec's `Exec` rules. The file is handed as an
//!   ABSOLUTE PATH for `%f %F %u %U` (the spec allows a path for `%u`; Cura turns a `file://`
//!   argument into a bogus path), `@@u` becomes `@@` so a Flatpak's `--file-forwarding` hands the
//!   path through the document portal. Started like [`crate::opener::start`]: detached, reaped by
//!   a thread, never through a shell, the cleaned environment.
//! - NO PROTOCOL LINKS and no web server: no slicer's URL protocol takes a local file (research,
//!   `target/next/v6/research.md`), so every slicer is started with the file.
//!
//! Log safety (§9.1): a path or a desktop entry's text is never logged here.

use std::ffi::{OsStr, OsString};
use std::io::Read as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The slicer a person gets until they have started another one.
pub const DEFAULT_SLICER: &str = "orcaslicer";
/// The id of "Other Application…" (the portal's chooser); never remembered.
pub const OTHER: &str = "other";

/// A desktop file over this size is skipped.
pub const MAX_ENTRY_BYTES: u64 = 64 * 1024;
/// At most this many files of one applications directory are looked at.
pub const MAX_FILES_PER_DIR: usize = 4096;
/// An `Exec` longer than this is not used.
pub const MAX_EXEC_BYTES: usize = 4096;
/// An `Exec` with more arguments than this is not used.
pub const MAX_EXEC_ARGS: usize = 64;

/// The environment variable that names the only applications directories a FIXTURE run reads
/// (`:`-separated, debug builds only; absent = none), so pictures and tests do not depend on the
/// machine's slicers.
pub const DEV_APPLICATIONS_ENV: &str = "KOURIER_DEV_APPLICATIONS";

/// The MIME types a desktop file must list for the Name match.
const MODEL_MIMES: [&str; 5] = [
    "model/stl",
    "model/3mf",
    "application/vnd.ms-3mfdocument",
    "model/x.stl-binary",
    "model/x.stl-ascii",
];

/// One slicer of the fixed table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slicer {
    /// The id the preference and QML use.
    pub id: &'static str,
    /// The product name (not translated).
    pub name: &'static str,
    /// The desktop ids to look for, in order (without `.desktop`).
    pub desktop_ids: &'static [&'static str],
}

/// The table, in his order: "The big four ... with OrcaSlicer first as default".
pub static SLICERS: [Slicer; 4] = [
    Slicer {
        id: "orcaslicer",
        name: "OrcaSlicer",
        desktop_ids: &[
            "com.orcaslicer.OrcaSlicer",
            "io.github.softfever.OrcaSlicer",
            "OrcaSlicer",
            "orca-slicer",
        ],
    },
    Slicer {
        id: "bambustudio",
        name: "Bambu Studio",
        desktop_ids: &["com.bambulab.BambuStudio", "BambuStudio", "bambu-studio"],
    },
    Slicer {
        id: "prusaslicer",
        name: "PrusaSlicer",
        desktop_ids: &["com.prusa3d.PrusaSlicer", "PrusaSlicer", "prusa-slicer"],
    },
    Slicer {
        id: "cura",
        name: "UltiMaker Cura",
        desktop_ids: &[
            "com.ultimaker.cura",
            "UltiMaker-Cura",
            "ultimaker-cura",
            "cura",
        ],
    },
];

/// The slicer with this id.
pub fn find(id: &str) -> Option<&'static Slicer> {
    SLICERS.iter().find(|s| s.id == id)
}

/// The id a preference value means: a table id, else [`DEFAULT_SLICER`] (empty or unknown).
pub fn resolve_id(pref: &str) -> &'static str {
    find(pref).map_or(DEFAULT_SLICER, |s| s.id)
}

/// A desktop file that passed the checks of [`parse_entry`] (the `[Desktop Entry]` group only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopEntry {
    /// The file it was read from (`%k`).
    pub path: PathBuf,
    /// The plain `Name=` (`%c`).
    pub name: String,
    /// `Exec=`, string escapes resolved, argument quoting still in place.
    pub exec: String,
    /// `TryExec=`.
    pub try_exec: Option<String>,
    /// `Icon=` (`%i`).
    pub icon: Option<String>,
    /// `Path=`: the working directory.
    pub work_dir: Option<PathBuf>,
    /// `MimeType=`, split.
    pub mime_types: Vec<String>,
}

/// The string escapes of the Desktop Entry spec: `\s`, `\n`, `\t`, `\r`, `\\`.
fn unescape_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// The entry in `text` (a desktop file's content), or `None` when it is hidden, is not an
/// `Application`, has no `Exec`, or the `Exec` is over the bounds. Only the `[Desktop Entry]`
/// group counts; localized keys (`Name[de]=`) are not read.
pub fn parse_entry(text: &str, path: &Path) -> Option<DesktopEntry> {
    let mut in_group = false;
    let (mut name, mut exec, mut try_exec, mut icon, mut work_dir) = (None, None, None, None, None);
    let (mut kind, mut hidden, mut mime) = (None, false, String::new());
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_group || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim_end(), value.trim_start());
        match key {
            "Type" => kind = Some(value.to_owned()),
            "Hidden" => hidden = value == "true",
            "Name" => name = Some(unescape_value(value)),
            "Exec" => exec = Some(unescape_value(value)),
            "TryExec" => try_exec = Some(unescape_value(value)),
            "Icon" => icon = Some(unescape_value(value)),
            "Path" => work_dir = Some(PathBuf::from(unescape_value(value))),
            "MimeType" => mime = value.to_owned(),
            _ => {}
        }
    }
    if hidden || kind.as_deref() != Some("Application") {
        return None;
    }
    let exec = exec.filter(|e| !e.trim().is_empty() && e.len() <= MAX_EXEC_BYTES)?;
    Some(DesktopEntry {
        path: path.to_path_buf(),
        name: name.unwrap_or_default(),
        exec,
        try_exec: try_exec.filter(|t| !t.is_empty()),
        icon: icon.filter(|i| !i.is_empty()),
        work_dir: work_dir.filter(|p| p.is_absolute()),
        mime_types: mime
            .split(';')
            .filter(|m| !m.is_empty())
            .map(str::to_owned)
            .collect(),
    })
}

/// One piece of an unquoted or quoted argument of an `Exec`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Piece {
    Lit(char),
    Code(char),
}

/// The arguments of `exec` as pieces (the spec's quoting: `"` around an argument, and inside
/// it a backslash before `"`, `` ` ``, `$` and `\`). A field code inside a quoted part makes the
/// entry unusable (`None`), as an unclosed quote does; `%%` is a literal percent.
fn split_exec(exec: &str) -> Option<Vec<Vec<Piece>>> {
    let mut args: Vec<Vec<Piece>> = Vec::new();
    let mut current: Option<Vec<Piece>> = None;
    let mut chars = exec.chars().peekable();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' => quoted = false,
                '\\' => match chars.next() {
                    Some(n @ ('"' | '`' | '$' | '\\')) => {
                        current.get_or_insert_with(Vec::new).push(Piece::Lit(n));
                    }
                    Some(n) => {
                        let arg = current.get_or_insert_with(Vec::new);
                        arg.push(Piece::Lit('\\'));
                        arg.push(Piece::Lit(n));
                    }
                    None => return None,
                },
                '%' => match chars.next() {
                    Some('%') => current.get_or_insert_with(Vec::new).push(Piece::Lit('%')),
                    _ => return None,
                },
                other => current.get_or_insert_with(Vec::new).push(Piece::Lit(other)),
            }
            continue;
        }
        match c {
            ' ' | '\t' | '\n' => {
                if let Some(done) = current.take() {
                    args.push(done);
                }
            }
            '"' => {
                quoted = true;
                current.get_or_insert_with(Vec::new);
            }
            '%' => match chars.next() {
                Some('%') => current.get_or_insert_with(Vec::new).push(Piece::Lit('%')),
                Some(code) => current.get_or_insert_with(Vec::new).push(Piece::Code(code)),
                None => return None,
            },
            other => current.get_or_insert_with(Vec::new).push(Piece::Lit(other)),
        }
        if args.len() > MAX_EXEC_ARGS {
            return None;
        }
    }
    if quoted {
        return None;
    }
    if let Some(done) = current.take() {
        args.push(done);
    }
    (args.len() <= MAX_EXEC_ARGS).then_some(args)
}

/// The argument vector of `entry` for the copy at `file` (an absolute path), or `None` when the
/// `Exec` cannot be used. Field codes: `%f %F %u %U` the file's path, `%i` `--icon <Icon>` (nothing
/// without an icon), `%c` the name, `%k` the desktop file's path, `%%` a percent; the deprecated
/// `%d %D %n %N %v %m` are removed; any other code makes the entry unusable. A standalone `@@u`
/// (Flatpak's file forwarding marker) becomes `@@`: flatpak then forwards the PATH through the
/// document portal. The slicer's own flags (`--single-instance-on-url`, ...) stay as written.
pub fn exec_argv(entry: &DesktopEntry, file: &Path) -> Option<Vec<OsString>> {
    let mut out: Vec<OsString> = Vec::new();
    for arg in split_exec(&entry.exec)? {
        if arg == [Piece::Lit('@'), Piece::Lit('@'), Piece::Lit('u')] {
            out.push(OsString::from("@@"));
            continue;
        }
        if arg == [Piece::Code('i')] {
            if let Some(icon) = &entry.icon {
                out.push(OsString::from("--icon"));
                out.push(OsString::from(icon));
            }
            continue;
        }
        let mut text = OsString::new();
        let mut produced = false;
        for piece in &arg {
            match *piece {
                Piece::Lit(c) => {
                    text.push(c.to_string());
                    produced = true;
                }
                Piece::Code('f' | 'F' | 'u' | 'U') => {
                    text.push(file.as_os_str());
                    produced = true;
                }
                Piece::Code('c') => {
                    text.push(&entry.name);
                    produced = true;
                }
                Piece::Code('k') => {
                    text.push(entry.path.as_os_str());
                    produced = true;
                }
                Piece::Code('i' | 'd' | 'D' | 'n' | 'N' | 'v' | 'm') => {}
                Piece::Code(_) => return None,
            }
        }
        if produced || arg.is_empty() {
            out.push(text);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Whether `program` (a path, or a name looked up in `path_var`) is an executable file.
fn program_exists(program: &OsStr, path_var: &OsStr) -> bool {
    let executable = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.as_bytes().contains(&b'/') {
        let p = Path::new(program);
        return p.is_absolute() && executable(p);
    }
    !program.is_empty()
        && std::env::split_paths(path_var)
            .any(|dir| dir.is_absolute() && executable(&dir.join(program)))
}

/// Whether `entry` can be started: its `TryExec`, and the program of its `Exec`, exist.
pub fn is_usable(entry: &DesktopEntry, path_var: &OsStr) -> bool {
    if let Some(t) = &entry.try_exec
        && !program_exists(OsStr::new(t), path_var)
    {
        return false;
    }
    // Any file stands in for the copy: only the program is judged here.
    exec_argv(entry, Path::new("/file")).is_some_and(|argv| program_exists(&argv[0], path_var))
}

/// What the search found for one slicer of the table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// The table's slicer.
    pub slicer: &'static Slicer,
    /// Its desktop entry when it is installed.
    pub entry: Option<DesktopEntry>,
}

/// The desktop file at `path` read within the bounds (`None`: too big, unreadable, not UTF-8,
/// or [`parse_entry`] refused it).
fn read_entry(path: &Path) -> Option<DesktopEntry> {
    let file = std::fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_ENTRY_BYTES {
        return None;
    }
    let mut text = String::new();
    file.take(MAX_ENTRY_BYTES).read_to_string(&mut text).ok()?;
    parse_entry(&text, path)
}

/// Looks for the four slicers in `dirs` (applications directories, most important first). File
/// I/O: call it from a worker thread. `path_var` is the `PATH` an `Exec` program is looked up in.
pub fn scan(dirs: &[PathBuf], path_var: &OsStr) -> Vec<Found> {
    // Pass 2's candidates: every `*.desktop` of every directory, read once, in order.
    let mut by_name: Option<Vec<DesktopEntry>> = None;
    SLICERS
        .iter()
        .map(|slicer| {
            let by_id = slicer.desktop_ids.iter().find_map(|id| {
                let file = format!("{id}.desktop");
                // Spec precedence: the first directory that has the file decides.
                let first = dirs.iter().map(|d| d.join(&file)).find(|p| p.is_file())?;
                read_entry(&first).filter(|e| is_usable(e, path_var))
            });
            let entry = by_id.or_else(|| {
                let all = by_name.get_or_insert_with(|| all_entries(dirs));
                all.iter()
                    .find(|e| {
                        e.name.eq_ignore_ascii_case(slicer.name)
                            && e.mime_types
                                .iter()
                                .any(|m| MODEL_MIMES.contains(&m.as_str()))
                            && is_usable(e, path_var)
                    })
                    .cloned()
            });
            Found { slicer, entry }
        })
        .collect()
}

/// Every readable entry of `dirs` (at most [`MAX_FILES_PER_DIR`] files of each, sorted by name).
fn all_entries(dirs: &[PathBuf]) -> Vec<DesktopEntry> {
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(read) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = read
            .filter_map(Result::ok)
            .take(MAX_FILES_PER_DIR)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "desktop"))
            .collect();
        files.sort();
        out.extend(files.iter().filter_map(|p| read_entry(p)));
    }
    out
}

/// The applications directories to search. A fixture run (`--fixture`, shots, the Qt tests) in a
/// debug build reads ONLY the directories named in [`DEV_APPLICATIONS_ENV`] (none when it is
/// absent) and a fixture run of a release build none, so pictures do not depend on the machine;
/// a normal run reads `$XDG_DATA_HOME/applications` (default `~/.local/share/applications`),
/// each `$XDG_DATA_DIRS` entry's `applications` (default `/usr/local/share:/usr/share`), and the
/// Flatpak exports (`~/.local/share/flatpak/exports/share/applications`,
/// `/var/lib/flatpak/exports/share/applications`) when not already listed.
pub fn application_dirs(fixture: bool, env: &dyn Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    if fixture {
        if !cfg!(debug_assertions) {
            return Vec::new();
        }
        return env(DEV_APPLICATIONS_ENV)
            .map(|v| {
                std::env::split_paths(&v)
                    .filter(|p| p.is_absolute())
                    .collect()
            })
            .unwrap_or_default();
    }
    let home = env("HOME").map(PathBuf::from).filter(|p| p.is_absolute());
    let data_home = env("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut add = |p: PathBuf| {
        if !dirs.contains(&p) {
            dirs.push(p);
        }
    };
    if let Some(d) = &data_home {
        add(d.join("applications"));
    }
    let data_dirs = env("XDG_DATA_DIRS")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| OsString::from("/usr/local/share:/usr/share"));
    for d in std::env::split_paths(&data_dirs).filter(|p| p.is_absolute()) {
        add(d.join("applications"));
    }
    if let Some(h) = &home {
        add(h.join(".local/share/flatpak/exports/share/applications"));
    }
    add(PathBuf::from("/var/lib/flatpak/exports/share/applications"));
    dirs
}

/// [`application_dirs`] for this process's environment.
pub fn application_dirs_now(fixture: bool) -> Vec<PathBuf> {
    application_dirs(fixture, &|name| std::env::var_os(name))
}

/// The command that starts `entry` with the file at `file` (an absolute path): the `Exec`'s
/// arguments, no standard streams, a process group of its own, every name of `drop_env`, every
/// development variable and what the AppImage runtime set taken out of the CHILD's environment
/// ([`crate::opener`], [`crate::process::child_env_drop`]), the working
/// directory `Path=` when it is a folder, else the home folder. Nothing is started. `None` when the
/// file path is not absolute or the `Exec` cannot be used.
pub fn command(entry: &DesktopEntry, file: &Path, drop_env: &[&str]) -> Option<Command> {
    if !file.is_absolute() {
        return None;
    }
    let argv = exec_argv(entry, file)?;
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    for name in drop_env {
        command.env_remove(name);
    }
    for name in crate::process::child_env_drop_now() {
        command.env_remove(name);
    }
    let dir = entry.work_dir.clone().filter(|p| p.is_dir()).or_else(|| {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
    });
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    Some(command)
}

/// Whether `path` ends in `.stl` or `.3mf` (slicers pick their reader by the extension; the
/// core's copy of a model is named so, `kourier-core` `attachments/playback.rs`).
pub fn has_model_extension(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|e| e.eq_ignore_ascii_case("stl") || e.eq_ignore_ascii_case("3mf"))
}

/// What [`ask_for_application`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chooser {
    /// The portal took the request: its chooser is the person's now.
    Asked,
    /// No portal answered: the caller opens the file with the desktop's default application
    /// (`xdg-open`, [`crate::opener`]) and says so in its own words.
    NoPortal,
}

/// "Other Application…": `org.freedesktop.portal.OpenURI.OpenFile` for the copy at `file` with
/// `ask: true` (KDE then shows its own application chooser). Blocks on the session bus: a worker
/// thread only. `Err`: the file could not be opened.
pub fn ask_for_application(file: &Path) -> std::io::Result<Chooser> {
    let opened = std::fs::File::open(file)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    Ok(runtime.block_on(async {
        let Ok(bus) = kourier_desktop::Bus::session().await else {
            return Chooser::NoPortal;
        };
        match kourier_desktop::portal::open_file_asking(&bus, &opened).await {
            Ok(()) => Chooser::Asked,
            Err(_) => Chooser::NoPortal,
        }
    }))
}

#[cfg(test)]
mod tests;

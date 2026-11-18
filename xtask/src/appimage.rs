//! The AppImage that `cargo xtask dist` makes beside the archives.
//!
//! ```text
//! target/dist/kourier-<version>-linux-<arch>[-suffix].AppImage
//! ```
//!
//! What it does, in order ([`make`]):
//!
//! 1. Fetches the three pinned upstream tools into `<target>/appimage-tools/` (linuxdeploy,
//!    appimagetool and the type2 runtime), checks each against its recorded SHA-256 on every
//!    run, and extracts the two AppImages once, so that nothing needs FUSE ([`tools`]).
//! 2. Builds an AppDir below `<out>/.dist-appimage/AppDir`: the shipped binary and the stage's
//!    `share/`, the Qt plugins of a reviewed list ([`PLUGINS`]), the QML modules the program
//!    and the Kirigami desktop style import ([`pick_qml_modules`]) and a `qt.conf` that points Qt
//!    at them.
//! 3. Lets linuxdeploy bundle the shared libraries all of that needs (`usr/lib`) and set the
//!    run paths. linuxdeploy-plugin-qt is not used: it copies far more than is wanted.
//! 4. Writes the licence texts of every bundled file's Debian package and the list of what is
//!    bundled from which source package ([`render_bundled`]): the AppImage redistributes Qt,
//!    KDE Frameworks, FFmpeg and more under the LGPL and the GPL.
//! 5. Packs it with appimagetool and the pinned runtime. There is no update information.
//!
//! [`verify`] then unpacks the file and checks the tree, searches every file for private names
//! and starts the program from the unpacked tree and from the packed file.

use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use super::dist::{
    Needle, OWN_QML_MODULE, Scan, VERSION_TIMEOUT, check_fixture_refused, check_release_networks,
    check_version_output, check_version_output_with, dpkg_owner, human, library_dirs, qmake_query,
    run_limited, scan_bytes, sha256_of, stdout_of,
};
use super::install::{self, APP_ID, BINARY};
use super::{root, target_dir};

/// One upstream tool: an official GitHub release asset and the digest GitHub records for it.
struct Tool {
    /// What it is called in messages and in the extraction directory.
    name: &'static str,
    version: &'static str,
    url: &'static str,
    sha256: &'static str,
    /// The cached file's name.
    file: &'static str,
}

const LINUXDEPLOY: Tool = Tool {
    name: "linuxdeploy",
    version: "1-alpha-20251107-1",
    url: "https://github.com/linuxdeploy/linuxdeploy/releases/download/1-alpha-20251107-1/linuxdeploy-x86_64.AppImage",
    sha256: "c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d",
    file: "linuxdeploy-1-alpha-20251107-1-x86_64.AppImage",
};

const APPIMAGETOOL: Tool = Tool {
    name: "appimagetool",
    version: "1.9.1",
    url: "https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage",
    sha256: "ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0",
    file: "appimagetool-1.9.1-x86_64.AppImage",
};

const RUNTIME: Tool = Tool {
    name: "type2-runtime",
    version: "20251108",
    url: "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64",
    sha256: "2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d",
    file: "runtime-20251108-x86_64",
};

/// The only architecture the tools are pinned for.
const PINNED_ARCH: &str = "x86_64";

/// The Qt plugins that are bundled, relative to `QT_INSTALL_PLUGINS`; one that is missing on
/// the builder is an error. The list is reviewed by hand: linuxdeploy-plugin-qt would take
/// every plugin of the Qt packages.
pub const PLUGINS: &[&str] = &[
    // The platforms: Wayland, X11 and the offscreen one (the tests and a start without a
    // display).
    "platforms/libqwayland.so",
    "platforms/libqxcb.so",
    "platforms/libqoffscreen.so",
    // The portal theme is the only platform theme ("Portal theme only", the maintainer's
    // choice): no desktop-specific theme is bundled.
    "platformthemes/libqxdgdesktopportal.so",
    // Compose keys and IBus, the two input methods that need no further library on the system.
    "platforminputcontexts/libcomposeplatforminputcontextplugin.so",
    "platforminputcontexts/libibusplatforminputcontextplugin.so",
    // What the Wayland platform loads: the shell, the client-side decorations and EGL.
    "wayland-shell-integration/libxdg-shell.so",
    "wayland-decoration-client/libadwaita.so",
    "wayland-decoration-client/libbradient.so",
    "wayland-graphics-integration-client/libqt-plugin-wayland-egl.so",
    // What the X11 platform loads: EGL and GLX.
    "xcbglintegrations/libqxcb-egl-integration.so",
    "xcbglintegrations/libqxcb-glx-integration.so",
    // SVG icons (the icon theme's and the application's own).
    "iconengines/libqsvgicon.so",
    // The picture formats of attachments and avatars.
    "imageformats/libqgif.so",
    "imageformats/libqico.so",
    "imageformats/libqjpeg.so",
    "imageformats/libqsvg.so",
    "imageformats/libqwebp.so",
    "imageformats/libqtiff.so",
    "imageformats/kimg_heif.so",
    "imageformats/kimg_avif.so",
    // Audio and video playback.
    "multimedia/libffmpegmediaplugin.so",
    // The Kirigami desktop style (org.kde.desktop) and the icon engine of the KDE icon themes.
    "kf6/kirigami/platform/org.kde.desktop.so",
    "kiconthemes6/iconengines/KIconEnginePlugin.so",
    // The text field menu of org.kde.desktop uses Sonnet's spelling check.
    "kf6/sonnet/sonnet_hunspell.so",
];

/// The `qt.conf` beside the program: the prefix is the AppDir's `usr`, so that the Qt of the
/// AppImage looks for plugins and QML modules there and nowhere else.
pub const QT_CONF: &str = "[Paths]\nPrefix = ..\nPlugins = plugins\nQmlImports = qml\n";

/// The Controls styles that are not bundled: the program uses the Kirigami desktop style.
const SKIPPED_STYLES: [&str; 4] = [
    "QtQuick.Controls.FluentWinUI3",
    "QtQuick.Controls.Imagine",
    "QtQuick.Controls.Material",
    "QtQuick.Controls.Universal",
];

/// Where the licence material goes, below `usr/share/licenses/kourier`.
const BUNDLED_LIST: &str = "BUNDLED.txt";
const BUNDLED_COPYRIGHTS: &str = "bundled";

/// How long linuxdeploy and appimagetool may take.
const TOOL_TIMEOUT: Duration = Duration::from_secs(1800);

/// How long the packed file may take to extract itself and print its version.
const PACKED_TIMEOUT: Duration = Duration::from_secs(180);

// ---------------------------------------------------------------------------------------
// The tools

/// `<target>/appimage-tools/`.
fn cache_dir() -> PathBuf {
    target_dir().join("appimage-tools")
}

/// The cached file of `tool`, downloaded when it is not there; its digest is checked on every
/// run, and a file that does not match is deleted and an error.
fn fetch(tool: &Tool, cache: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(cache).with_context(|| format!("create {}", cache.display()))?;
    let file = cache.join(tool.file);
    if !file.exists() {
        eprintln!(
            "appimage: downloading {} {} from {}",
            tool.name, tool.version, tool.url
        );
        let part = cache.join(format!("{}.part", tool.file));
        let status = Command::new("curl")
            .args(["-fsSL", "--proto", "=https", "--tlsv1.2", "-o"])
            .arg(&part)
            .arg(tool.url)
            .status()
            .context("run curl")?;
        if !status.success() {
            let _ = std::fs::remove_file(&part);
            bail!("curl could not download {}", tool.url);
        }
        std::fs::rename(&part, &file)?;
    }
    let found = sha256_of(&file)?;
    if found != tool.sha256 {
        let _ = std::fs::remove_file(&file);
        bail!(
            "{} has the SHA-256 {found}, expected {}; the file was deleted",
            tool.file,
            tool.sha256
        );
    }
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))?;
    Ok(file)
}

/// The name of the file that says an extraction is complete.
const EXTRACTED_STAMP: &str = ".extracted-sha256";

/// The directory with the extracted AppImage `file` of `tool` (`<cache>/<name>-<version>/`),
/// extracted when it is not there. The stamp, written last, holds the AppImage's digest, so
/// that a half extraction is redone.
fn extract(tool: &Tool, file: &Path, cache: &Path) -> Result<PathBuf> {
    let dir = cache.join(format!("{}-{}", tool.name, tool.version));
    let stamp = dir.join(EXTRACTED_STAMP);
    if std::fs::read_to_string(&stamp).is_ok_and(|s| s.trim() == tool.sha256) {
        return Ok(dir);
    }
    let _ = std::fs::remove_dir_all(&dir);
    let scratch = cache.join(format!("extract-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch)?;
    let result = (|| -> Result<()> {
        let out = run_limited(
            {
                let mut cmd = Command::new(file);
                cmd.arg("--appimage-extract").current_dir(&scratch);
                cmd
            },
            TOOL_TIMEOUT,
        )
        .with_context(|| format!("extract {}", file.display()))?;
        if !out.status.success() {
            bail!(
                "{} --appimage-extract exited with {}: {}",
                file.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        std::fs::rename(scratch.join("squashfs-root"), &dir)
            .context("the extraction left no squashfs-root")?;
        std::fs::write(&stamp, format!("{}\n", tool.sha256))?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    result?;
    Ok(dir)
}

/// The three tools, ready: the extracted linuxdeploy, the extracted appimagetool and the
/// runtime file.
struct Tools {
    linuxdeploy: PathBuf,
    appimagetool: PathBuf,
    runtime: PathBuf,
}

fn tools(arch: &str) -> Result<Tools> {
    if arch != PINNED_ARCH {
        bail!(
            "the AppImage tools are pinned for {PINNED_ARCH} only, this machine is {arch}; give --no-appimage"
        );
    }
    let cache = cache_dir();
    let linuxdeploy = extract(&LINUXDEPLOY, &fetch(&LINUXDEPLOY, &cache)?, &cache)?;
    let appimagetool = extract(&APPIMAGETOOL, &fetch(&APPIMAGETOOL, &cache)?, &cache)?;
    let runtime = fetch(&RUNTIME, &cache)?;
    eprintln!(
        "appimage: tools: linuxdeploy {}, appimagetool {}, type2-runtime {} (digests checked)",
        LINUXDEPLOY.version, APPIMAGETOOL.version, RUNTIME.version
    );
    Ok(Tools {
        linuxdeploy,
        appimagetool,
        runtime,
    })
}

// ---------------------------------------------------------------------------------------
// Copying

/// Copies the file `src` (a link is followed) to `dst`, and makes sure the owner can write it:
/// linuxdeploy patches the run paths of what it finds in the AppDir.
fn copy_file(src: &Path, dst: &Path) -> Result<()> {
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::copy(src, dst)
        .with_context(|| format!("copy {} to {}", src.display(), dst.display()))?;
    let mode = std::fs::metadata(dst)?.permissions().mode() & 0o7777;
    std::fs::set_permissions(dst, std::fs::Permissions::from_mode(mode | 0o200))?;
    Ok(())
}

/// Copies the directory `src` and everything below it to `dst`; links are followed.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if std::fs::metadata(entry.path())?.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else {
            copy_file(&entry.path(), &to)?;
        }
    }
    Ok(())
}

/// Copies one QML module: the files of its directory, and the subdirectories that have no
/// `qmldir` of their own (a subdirectory with one is a module the scanner lists itself).
fn copy_module(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    let mut entries: Vec<_> = std::fs::read_dir(src)
        .with_context(|| format!("read {}", src.display()))?
        .collect::<std::io::Result<_>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let to = dst.join(entry.file_name());
        if std::fs::metadata(entry.path())?.is_dir() {
            if !entry.path().join("qmldir").exists() {
                copy_tree(&entry.path(), &to)?;
            }
        } else {
            copy_file(&entry.path(), &to)?;
        }
    }
    Ok(())
}

/// Every regular file below `dir` (links are neither followed nor listed), sorted.
fn files_below(dir: &Path) -> Result<Vec<PathBuf>> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                walk(&entry.path(), out)?;
            } else if kind.is_file() {
                out.push(entry.path());
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, &mut out)?;
    out.sort();
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// The QML modules

/// Whether `name` is one of the Controls styles that are not bundled (the name itself or a
/// module below it).
fn is_skipped_style(name: &str) -> bool {
    SKIPPED_STYLES
        .iter()
        .any(|s| name == *s || name.strip_prefix(s).is_some_and(|r| r.starts_with('.')))
}

/// The modules to bundle from the JSON `qmlimportscanner` printed: the entries of type
/// `module` that have a path, without the unused Controls styles, as (name, directory) sorted by
/// directory. The program's own module (compiled into the binary) is skipped; any other module
/// outside `qml_root` is an error, because it would be left out without a word.
pub fn pick_qml_modules(json: &str, qml_root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let entries: serde_json::Value =
        serde_json::from_str(json).context("qmlimportscanner printed no JSON")?;
    let entries = entries
        .as_array()
        .context("qmlimportscanner printed no JSON array")?;
    let mut picked: BTreeMap<PathBuf, String> = BTreeMap::new();
    for entry in entries {
        if entry["type"].as_str() != Some("module") {
            continue;
        }
        let Some(path) = entry["path"].as_str().filter(|p| !p.is_empty()) else {
            continue;
        };
        let name = entry["name"].as_str().unwrap_or_default();
        if is_skipped_style(name) {
            continue;
        }
        let path = PathBuf::from(path);
        if path == qml_root {
            bail!(
                "the QML module {name} has the QML root {} as its path",
                path.display()
            );
        }
        if !path.starts_with(qml_root) {
            if name == OWN_QML_MODULE {
                continue;
            }
            bail!(
                "the QML module {name} is at {}, not below {}: it would not be bundled",
                path.display(),
                qml_root.display()
            );
        }
        picked.entry(path).or_insert_with(|| name.to_owned());
    }
    Ok(picked.into_iter().map(|(p, n)| (n, p)).collect())
}

// ---------------------------------------------------------------------------------------
// The AppDir

/// The directories below `usr/plugins` and `usr/qml` that directly hold a `.so` file, sorted:
/// linuxdeploy's `--deploy-deps-only` does not look below the directory it is given.
pub fn deploy_dirs(appdir: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = BTreeSet::new();
    for top in ["usr/plugins", "usr/qml"] {
        let top = appdir.join(top);
        if !top.is_dir() {
            continue;
        }
        for file in files_below(&top)? {
            if file.extension().is_some_and(|e| e == "so") {
                dirs.insert(file.parent().context("a file has a directory")?.to_owned());
            }
        }
    }
    Ok(dirs.into_iter().collect())
}

/// The last `count` lines of `text`.
fn tail_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(count)..].join("\n")
}

/// Runs `cmd` with its output in `log`; when it fails, the last lines of the log are in the
/// error.
fn run_logged(what: &str, mut cmd: Command, log: &Path) -> Result<()> {
    let file = std::fs::File::create(log).with_context(|| format!("create {}", log.display()))?;
    cmd.stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file);
    let status = cmd.status().with_context(|| format!("run {what}"))?;
    if !status.success() {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        bail!(
            "{what} exited with {status} (log: {}); its last 60 lines:\n{}",
            log.display(),
            tail_lines(&text, 60)
        );
    }
    Ok(())
}

/// The Qt plugins of [`PLUGINS`] from `plugin_root` to `<appdir>/usr/plugins`.
fn copy_plugins(plugin_root: &Path, appdir: &Path) -> Result<()> {
    for rel in PLUGINS {
        let src = plugin_root.join(rel);
        if !src.is_file() {
            bail!(
                "the Qt plugin {rel} is not at {}: the builder lacks the package that has it",
                src.display()
            );
        }
        copy_file(&src, &appdir.join("usr/plugins").join(rel))?;
    }
    Ok(())
}

/// The QML modules `qmlimportscanner` finds for the program's QML and the Kirigami desktop
/// style, from `qml_root` to `<appdir>/usr/qml`. Returns how many.
fn copy_qml(qml_root: &Path, appdir: &Path) -> Result<usize> {
    let libexecs =
        qmake_query("QT_INSTALL_LIBEXECS").context("qmake6 -query QT_INSTALL_LIBEXECS")?;
    let scanner = Path::new(&libexecs).join("qmlimportscanner");
    let mut cmd = Command::new(&scanner);
    cmd.arg("-rootPath")
        .arg(root().join("crates/kourier-qt/qml"))
        .arg("-rootPath")
        .arg(qml_root.join("org/kde/desktop"))
        .arg("-importPath")
        .arg(qml_root);
    let json = stdout_of(&mut cmd, "qmlimportscanner")?;
    let modules = pick_qml_modules(&json, qml_root)?;
    for (_, path) in &modules {
        let rel = path.strip_prefix(qml_root)?;
        copy_module(path, &appdir.join("usr/qml").join(rel))?;
    }
    Ok(modules.len())
}

/// The icon file `<APP_ID>.svg` below `<appdir>/usr/share/icons`.
fn find_icon(appdir: &Path) -> Result<PathBuf> {
    let name = format!("{APP_ID}.svg");
    files_below(&appdir.join("usr/share/icons"))?
        .into_iter()
        .find(|f| f.file_name().is_some_and(|n| *n == *name))
        .with_context(|| format!("no {name} below usr/share/icons of the AppDir"))
}

/// Runs linuxdeploy on `appdir`, which bundles the libraries into `usr/lib`; the output goes to
/// `log`.
fn run_linuxdeploy(tools: &Tools, appdir: &Path, log: &Path) -> Result<()> {
    let mut cmd = Command::new(tools.linuxdeploy.join("AppRun"));
    // linuxdeploy's own strip is too old for the system's libraries.
    cmd.env("NO_STRIP", "1")
        .arg("--appdir")
        .arg(appdir)
        .arg("--executable")
        .arg(appdir.join("usr/bin").join(BINARY))
        .arg("--desktop-file")
        .arg(
            appdir
                .join("usr/share/applications")
                .join(format!("{APP_ID}.desktop")),
        )
        .arg("--icon-file")
        .arg(find_icon(appdir)?);
    for dir in deploy_dirs(appdir)? {
        cmd.arg("--deploy-deps-only").arg(dir);
    }
    run_logged("linuxdeploy", cmd, log)
}

// ---------------------------------------------------------------------------------------
// What is bundled, and under which licences

/// The package a bundled file belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageInfo {
    pub version: String,
    pub source: String,
    pub source_version: String,
}

/// Reads a line of `dpkg-query -W -f='${Version}\t${source:Package}\t${source:Version}\n'`.
pub fn parse_package_info(line: &str) -> Result<PackageInfo> {
    let fields: Vec<&str> = line.trim_end_matches('\n').split('\t').collect();
    match fields[..] {
        [version, source, source_version]
            if !version.is_empty() && !source.is_empty() && !source_version.is_empty() =>
        {
            Ok(PackageInfo {
                version: version.to_owned(),
                source: source.to_owned(),
                source_version: source_version.to_owned(),
            })
        }
        _ => bail!("expected \"version<TAB>source<TAB>source version\", got {line:?}"),
    }
}

/// One bundled file and the Debian package it is a copy from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bundled {
    /// The path inside the AppImage, from its root.
    pub path: String,
    pub package: String,
    pub info: PackageInfo,
}

/// `BUNDLED.txt`: what the AppImage bundles and where the sources are, one line per file,
/// sorted by path.
pub fn render_bundled(files: &[Bundled]) -> String {
    let mut lines: Vec<String> = files
        .iter()
        .map(|f| {
            format!(
                "{}  {} {}  source {} {}",
                f.path, f.package, f.info.version, f.info.source, f.info.source_version
            )
        })
        .collect();
    lines.sort();
    let mut text = format!(
        "Files of other projects that this AppImage bundles\n\
         \n\
         These files are copies of the Ubuntu 24.04 and KDE neon packages of the machine that\n\
         built the AppImage. The corresponding sources are the source packages named on each\n\
         line, at the version named there. The copyright file of every package is in\n\
         {BUNDLED_COPYRIGHTS}/ next to this file.\n\
         \n\
         <path inside the AppImage>  <package> <version>  source <source package> <source version>\n\
         \n"
    );
    for line in lines {
        text.push_str(&line);
        text.push('\n');
    }
    text
}

/// Whether `path` is an ELF file.
fn is_elf(path: &Path) -> Result<bool> {
    let mut magic = [0u8; 4];
    let mut file = std::fs::File::open(path)?;
    Ok(file.read_exact(&mut magic).is_ok() && magic == *b"\x7fELF")
}

/// What linuxdeploy copied into `usr/lib`, from its log: the file name there and the system
/// file it is a copy of (`Copying file <source> to <appdir>/usr/lib/<name>`). Some libraries
/// come from a folder below the library directory (`pulseaudio/`, `libproxy/`).
pub fn copied_libraries(log: &str) -> BTreeMap<String, PathBuf> {
    let mut copied = BTreeMap::new();
    for line in log.lines() {
        let Some(rest) = line.trim_end().strip_prefix("Copying file ") else {
            continue;
        };
        let Some((source, dest)) = rest.rsplit_once(" to ") else {
            continue;
        };
        let dest = Path::new(dest);
        let in_usr_lib = dest
            .parent()
            .is_some_and(|parent| parent.ends_with("usr/lib"));
        if let (true, Some(name)) = (in_usr_lib, dest.file_name()) {
            copied.insert(name.to_string_lossy().into_owned(), PathBuf::from(source));
        }
    }
    copied
}

/// The system file that the bundled file `rel` below `usr/<kind>` is a copy of: for `usr/lib`
/// the one linuxdeploy's log names (`copied`), else the first library directory that has it.
fn system_source(
    kind: &str,
    rel: &Path,
    arch: &str,
    plugin_root: &Path,
    qml_root: &Path,
    copied: &BTreeMap<String, PathBuf>,
) -> Result<PathBuf> {
    match kind {
        "plugins" => Ok(plugin_root.join(rel)),
        "qml" => Ok(qml_root.join(rel)),
        _ if copied.contains_key(rel.to_string_lossy().as_ref()) => {
            Ok(copied[rel.to_string_lossy().as_ref()].clone())
        }
        _ => {
            let mut dirs: Vec<PathBuf> = library_dirs(arch).to_vec();
            dirs.extend([PathBuf::from("/usr/lib"), PathBuf::from("/lib")]);
            dirs.iter()
                .map(|d| d.join(rel))
                .find(|p| p.exists())
                .with_context(|| {
                    format!(
                        "usr/lib/{} is bundled, but no system copy of it is in {dirs:?}",
                        rel.display()
                    )
                })
        }
    }
}

fn package_info(package: &str) -> Result<PackageInfo> {
    let out = stdout_of(
        Command::new("dpkg-query").args([
            "-W",
            "-f=${Version}\t${source:Package}\t${source:Version}\n",
            package,
        ]),
        "dpkg-query",
    )?;
    parse_package_info(&out).with_context(|| format!("the version of package {package}"))
}

/// Finds, for every ELF file below `usr/lib`, `usr/plugins` and `usr/qml`, the system file it
/// is a copy of and the package that owns that. Returns the files and the packages.
fn bundled_files(
    appdir: &Path,
    arch: &str,
    plugin_root: &Path,
    qml_root: &Path,
    copied: &BTreeMap<String, PathBuf>,
) -> Result<(Vec<Bundled>, BTreeSet<String>)> {
    let mut infos: BTreeMap<String, PackageInfo> = BTreeMap::new();
    let mut files = Vec::new();
    for kind in ["lib", "plugins", "qml"] {
        let top = appdir.join("usr").join(kind);
        if !top.is_dir() {
            continue;
        }
        for file in files_below(&top)? {
            if !is_elf(&file)? {
                continue;
            }
            let rel = file.strip_prefix(&top)?;
            let source = system_source(kind, rel, arch, plugin_root, qml_root, copied)?;
            let mut owners = dpkg_owner(&source);
            if owners.is_empty()
                && let Ok(real) = std::fs::canonicalize(&source)
            {
                owners = dpkg_owner(&real);
            }
            let Some(package) = owners.into_iter().next() else {
                bail!(
                    "usr/{kind}/{} is a copy of {}, which no installed package owns",
                    rel.display(),
                    source.display()
                );
            };
            if !infos.contains_key(&package) {
                infos.insert(package.clone(), package_info(&package)?);
            }
            files.push(Bundled {
                path: format!("usr/{kind}/{}", rel.display()),
                info: infos[&package].clone(),
                package,
            });
        }
    }
    let packages = infos.keys().cloned().collect();
    Ok((files, packages))
}

/// Writes `BUNDLED.txt` and the copyright file of every package into the AppDir.
fn write_bundled(
    appdir: &Path,
    arch: &str,
    plugin_root: &Path,
    qml_root: &Path,
    copied: &BTreeMap<String, PathBuf>,
) -> Result<(usize, usize)> {
    let (files, packages) = bundled_files(appdir, arch, plugin_root, qml_root, copied)?;
    let dir = appdir.join("usr/share/licenses/kourier");
    for package in &packages {
        let src = Path::new("/usr/share/doc").join(package).join("copyright");
        if !src.is_file() {
            bail!(
                "the package {package} has no {}: its licence text cannot be bundled",
                src.display()
            );
        }
        copy_file(
            &src,
            &dir.join(BUNDLED_COPYRIGHTS)
                .join(format!("{package}.copyright")),
        )?;
    }
    std::fs::write(dir.join(BUNDLED_LIST), render_bundled(&files))?;
    Ok((files.len(), packages.len()))
}

// ---------------------------------------------------------------------------------------
// Making it

/// What [`make`] needs from `dist`.
pub struct Request<'a> {
    /// The output directory.
    pub out: &'a Path,
    /// The staged tree (`bin/kourier`, `share/`, `README.txt`).
    pub stage: &'a Path,
    /// The AppImage's file name.
    pub name: &'a str,
    pub version: &'a str,
    /// The commit's time, for the squashfs.
    pub commit_time: i64,
    pub arch: &'a str,
}

/// Variables that make appimagetool guess update information (it does so only with `-g`, but a
/// build must not depend on the environment either way).
pub fn update_information_vars(names: impl IntoIterator<Item = String>) -> Vec<String> {
    names
        .into_iter()
        .filter(|n| {
            n.starts_with("GITHUB_")
                || n.starts_with("GITLAB_")
                || n.starts_with("TRAVIS")
                || n == "CI"
        })
        .collect()
}

/// Makes `<out>/<name>`: the AppDir, linuxdeploy, the licence files, appimagetool. The half
/// written file is `<name>.partial`.
pub fn make(req: &Request) -> Result<()> {
    let tools = tools(req.arch)?;
    let work = req.out.join(super::dist::APPIMAGE_DIR);
    let _ = std::fs::remove_dir_all(&work);
    let appdir = work.join("AppDir");
    std::fs::create_dir_all(&appdir)?;

    // The program and the installed tree (not README.txt: it lists what a system must have,
    // and the AppImage brings it).
    let bin = appdir.join("usr/bin").join(BINARY);
    copy_file(&req.stage.join("bin").join(BINARY), &bin)?;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
    copy_tree(&req.stage.join("share"), &appdir.join("usr/share"))?;

    let plugin_root = PathBuf::from(
        qmake_query("QT_INSTALL_PLUGINS").context("qmake6 -query QT_INSTALL_PLUGINS")?,
    );
    let qml_root =
        PathBuf::from(qmake_query("QT_INSTALL_QML").context("qmake6 -query QT_INSTALL_QML")?);
    copy_plugins(&plugin_root, &appdir)?;
    let modules = copy_qml(&qml_root, &appdir)?;
    std::fs::write(appdir.join("usr/bin/qt.conf"), QT_CONF)?;
    eprintln!(
        "appimage: AppDir: {} Qt plugins and {modules} QML modules from {} and {}",
        PLUGINS.len(),
        plugin_root.display(),
        qml_root.display()
    );

    let log = work.join("linuxdeploy.log");
    eprintln!("appimage: linuxdeploy (log: {})", log.display());
    run_linuxdeploy(&tools, &appdir, &log)?;
    let app_run = appdir.join("AppRun");
    if std::fs::read_link(&app_run).ok().as_deref() != Some(Path::new("usr/bin/kourier")) {
        bail!("linuxdeploy did not make AppRun a link to usr/bin/{BINARY}");
    }
    // linuxdeploy puts the desktop file and the icon at the top; appimagetool makes `.DirIcon`
    // of the icon, and `verify` looks for it in the packed file.
    for rel in [format!("{APP_ID}.desktop"), format!("{APP_ID}.svg")] {
        if !appdir.join(&rel).exists() {
            bail!("linuxdeploy left no {rel} in the AppDir");
        }
    }
    let libs = files_below(&appdir.join("usr/lib"))?;
    let lib_bytes: u64 = libs
        .iter()
        .map(|f| std::fs::metadata(f).map(|m| m.len()))
        .sum::<std::io::Result<u64>>()?;
    eprintln!(
        "appimage: linuxdeploy bundled {} files into usr/lib ({})",
        libs.len(),
        human(lib_bytes)
    );

    let copied = copied_libraries(
        &std::fs::read_to_string(&log).with_context(|| format!("read {}", log.display()))?,
    );
    let (files, packages) = write_bundled(&appdir, req.arch, &plugin_root, &qml_root, &copied)?;
    eprintln!(
        "appimage: {files} bundled files from {packages} packages listed in usr/share/licenses/kourier/{BUNDLED_LIST}"
    );

    let partial = req.out.join(format!("{}.partial", req.name));
    let log = work.join("appimagetool.log");
    let mut cmd = Command::new(tools.appimagetool.join("AppRun"));
    // No update information: the file is never passed `-u` or `-g`.
    for var in
        update_information_vars(std::env::vars_os().filter_map(|(k, _)| k.into_string().ok()))
    {
        cmd.env_remove(var);
    }
    cmd.env("ARCH", PINNED_ARCH)
        .env("VERSION", req.version)
        .env("SOURCE_DATE_EPOCH", req.commit_time.to_string())
        // The metainfo is checked by `dist` itself (appstreamcli, with the project's own
        // override); appimagetool only looks for `<id>.appdata.xml` and warns that the
        // upstream metadata is missing when it finds the `.metainfo.xml` of the installed tree.
        .arg("--no-appstream")
        .arg("--runtime-file")
        .arg(&tools.runtime)
        .arg(&appdir)
        .arg(&partial);
    eprintln!("appimage: appimagetool (log: {})", log.display());
    run_logged("appimagetool", cmd, &log)?;
    let file = req.out.join(req.name);
    std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&partial, &file)?;
    eprintln!(
        "appimage: {} ({})",
        req.name,
        human(std::fs::metadata(&file)?.len())
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Verifying it

/// What [`verify`] needs from `dist`.
pub struct Check<'a> {
    /// A fresh directory for the extraction (`squashfs-root/` appears in it).
    pub work: &'a Path,
    /// The packed AppImage.
    pub file: &'a Path,
    /// The shipped binary (the symbol table split off) the AppImage's program is tied to.
    pub shipped: &'a Path,
    pub version: &'a str,
    pub needles: &'a [Needle],
}

/// Unpacks the AppImage and checks it; returns what the search for private names found, one
/// entry per file, named `AppImage:<path>`.
pub fn verify(check: &Check) -> Result<Vec<(String, Scan)>> {
    let _ = std::fs::remove_dir_all(check.work);
    std::fs::create_dir_all(check.work)?;
    let mut cmd = Command::new(check.file);
    cmd.arg("--appimage-extract").current_dir(check.work);
    let out = run_limited(cmd, PACKED_TIMEOUT)
        .with_context(|| format!("extract {}", check.file.display()))?;
    if !out.status.success() {
        bail!(
            "{} --appimage-extract exited with {}: {}",
            check.file.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let root_dir = check.work.join("squashfs-root");

    // The tree.
    let program = root_dir.join("usr/bin").join(BINARY);
    let bytes = std::fs::read(&program).with_context(|| format!("read {}", program.display()))?;
    if install::is_perf_build(&bytes) {
        bail!("the AppImage's usr/bin/{BINARY} is a measuring build");
    }
    let mode = std::fs::metadata(&program)?.permissions().mode() & 0o7777;
    if mode != 0o755 {
        bail!("usr/bin/{BINARY} in the AppImage has mode {mode:o}, expected 755");
    }
    // linuxdeploy's patchelf changes the run path, so the bytes differ from the shipped
    // binary's; the build-id is what ties them.
    let (packed_id, shipped_id) = (
        crate::names::build_id(&program)?,
        crate::names::build_id(check.shipped)?,
    );
    if packed_id != shipped_id {
        bail!(
            "usr/bin/{BINARY} in the AppImage has the build-id {packed_id}, the shipped binary {shipped_id}"
        );
    }
    let qt_conf = std::fs::read_to_string(root_dir.join("usr/bin/qt.conf"))?;
    if qt_conf != QT_CONF {
        bail!("usr/bin/qt.conf in the AppImage says {qt_conf:?}, expected {QT_CONF:?}");
    }
    let app_run = root_dir.join("AppRun");
    if std::fs::read_link(&app_run).ok().as_deref() != Some(Path::new("usr/bin/kourier")) {
        bail!("AppRun in the AppImage is not a link to usr/bin/{BINARY}");
    }
    // The icon a file manager shows for the AppImage (appimagetool links it to the top icon).
    if !root_dir.join(".DirIcon").is_file() {
        bail!("the AppImage has no .DirIcon, or it does not lead to a file");
    }
    for rel in [
        format!("{APP_ID}.desktop"),
        format!("usr/share/applications/{APP_ID}.desktop"),
    ] {
        let desktop = std::fs::read_to_string(root_dir.join(&rel))
            .with_context(|| format!("read {rel} of the AppImage"))?;
        for scheme in install::SCHEMES {
            if desktop.contains(&format!("x-scheme-handler/{scheme}")) {
                bail!("{rel} of the AppImage claims x-scheme-handler/{scheme}: it must not");
            }
        }
    }
    for rel in ["LICENSE", "THIRD-PARTY.html", BUNDLED_LIST] {
        let file = root_dir.join("usr/share/licenses/kourier").join(rel);
        if !file.is_file() {
            bail!("the AppImage has no usr/share/licenses/kourier/{rel}");
        }
    }
    for rel in PLUGINS {
        if !root_dir.join("usr/plugins").join(rel).is_file() {
            bail!("the AppImage has no usr/plugins/{rel}");
        }
    }

    // No private name in any file.
    let mut scans = Vec::new();
    let (mut count, mut size) = (0usize, 0u64);
    for file in files_below(&root_dir)? {
        let bytes = std::fs::read(&file)?;
        count += 1;
        size += bytes.len() as u64;
        let scan = scan_bytes(&bytes, check.needles, 5);
        if scan.total() > 0 {
            let rel = file.strip_prefix(&root_dir)?;
            scans.push((format!("AppImage:{}", rel.display()), scan));
        }
    }
    eprintln!(
        "appimage: searched {count} files ({}) of the AppImage for private names: {} files with a match",
        human(size),
        scans.len()
    );

    // It starts: from the unpacked tree, and then the packed file itself (the runtime,
    // extracting itself because the builder has no FUSE).
    check_version_output(&app_run, check.version, check.work, VERSION_TIMEOUT)?;
    check_fixture_refused(&app_run, check.work, VERSION_TIMEOUT)?;
    check_release_networks(&app_run, check.work, VERSION_TIMEOUT)?;
    let tmp = check.work.join("tmp");
    std::fs::create_dir_all(&tmp)?;
    let one = Path::new("1");
    check_version_output_with(
        check.file,
        check.version,
        check.work,
        PACKED_TIMEOUT,
        &[("APPIMAGE_EXTRACT_AND_RUN", one), ("TMPDIR", &tmp)],
    )
    .context("the packed file itself")?;
    eprintln!(
        "appimage: verified: AppRun --version prints \"Kourier {}\", --fixture is refused, WhatsApp registered only while switched on; the packed file prints it too",
        check.version
    );
    Ok(scans)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory below the target directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = target_dir()
                .join("tmp/xtask-appimage")
                .join(format!("{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch directory");
            Self(dir)
        }

        fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().expect("a directory")).expect("directory");
            std::fs::write(&path, bytes).expect("file");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_pinned_tools_are_https_release_assets_with_full_digests() {
        for tool in [&LINUXDEPLOY, &APPIMAGETOOL, &RUNTIME] {
            assert!(tool.url.starts_with("https://github.com/"), "{}", tool.name);
            assert!(tool.url.contains(tool.version), "{}", tool.name);
            assert_eq!(tool.sha256.len(), 64, "{}", tool.name);
            assert!(tool.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(tool.file.contains(tool.version), "{}", tool.name);
        }
    }

    #[test]
    fn the_plugin_list_has_no_duplicate_and_only_relative_so_paths() {
        let unique: BTreeSet<&str> = PLUGINS.iter().copied().collect();
        assert_eq!(unique.len(), PLUGINS.len());
        for p in PLUGINS {
            assert!(!p.starts_with('/') && !p.contains(".."), "{p}");
            assert!(p.ends_with(".so"), "{p}");
        }
    }

    const SCAN: &str = r#"[
        {"type": "module", "name": "QtQuick", "path": "/q/QtQuick"},
        {"type": "module", "name": "QtQuick.Controls", "path": "/q/QtQuick/Controls"},
        {"type": "module", "name": "QtQuick.Controls.Material", "path": "/q/QtQuick/Controls/Material"},
        {"type": "module", "name": "QtQuick.Controls.Material.impl", "path": "/q/QtQuick/Controls/Material/impl"},
        {"type": "module", "name": "QtQuick.Controls.Imagine", "path": "/q/QtQuick/Controls/Imagine"},
        {"type": "module", "name": "QtQuick.Controls.Universal", "path": "/q/QtQuick/Controls/Universal"},
        {"type": "module", "name": "QtQuick.Controls.FluentWinUI3", "path": "/q/QtQuick/Controls/FluentWinUI3"},
        {"type": "module", "name": "QtQuick.Controls.MaterialStyle", "path": "/q/QtQuick/Controls/MaterialStyle"},
        {"type": "module", "name": "org.kde.kirigami", "path": "/q/org/kde/kirigami"},
        {"type": "module", "name": "app.kourier", "path": "/repo/crates/qml/app/kourier"},
        {"type": "module", "name": "QtQml"},
        {"type": "module", "name": "QtCore", "path": ""},
        {"type": "file", "name": "Main.qml", "path": "/repo/Main.qml"},
        {"type": "module", "name": "QtQuick", "path": "/q/QtQuick"}
    ]"#;

    #[test]
    fn the_qml_modules_are_the_scanners_modules_below_the_qml_root_minus_the_styles() {
        let picked = pick_qml_modules(SCAN, Path::new("/q")).expect("picked");
        let names: Vec<&str> = picked.iter().map(|(n, _)| n.as_str()).collect();
        // Sorted by directory, once each; the unused styles (and what is below them) and the
        // program's own module are gone, a name that only begins like a style stays.
        assert_eq!(
            names,
            [
                "QtQuick",
                "QtQuick.Controls",
                "QtQuick.Controls.MaterialStyle",
                "org.kde.kirigami"
            ]
        );
        assert_eq!(picked[0].1, Path::new("/q/QtQuick"));
    }

    #[test]
    fn a_foreign_module_outside_the_qml_root_is_refused() {
        let json = r#"[{"type": "module", "name": "Foreign.Thing", "path": "/elsewhere/Foreign"}]"#;
        let err = pick_qml_modules(json, Path::new("/q")).expect_err("refused");
        assert!(err.to_string().contains("Foreign.Thing"), "{err}");
        assert!(pick_qml_modules("{}", Path::new("/q")).is_err());
        assert!(pick_qml_modules("not json", Path::new("/q")).is_err());
        let root = r#"[{"type": "module", "name": "X", "path": "/q"}]"#;
        assert!(pick_qml_modules(root, Path::new("/q")).is_err());
    }

    #[test]
    fn a_module_is_copied_with_its_files_and_the_subdirectories_that_are_no_modules() {
        let s = Scratch::new("module");
        s.write("m/qmldir", b"module M\n");
        s.write("m/libplugin.so", b"so");
        s.write("m/data/x.txt", b"x");
        s.write("m/data/deep/y.txt", b"y");
        // A subdirectory with its own qmldir is a module the scanner lists itself.
        s.write("m/sub/qmldir", b"module M.sub\n");
        s.write("m/sub/z.qml", b"z");
        // A link is followed.
        s.write("elsewhere/real.qml", b"real");
        std::os::unix::fs::symlink(s.0.join("elsewhere/real.qml"), s.0.join("m/link.qml"))
            .expect("link");
        copy_module(&s.0.join("m"), &s.0.join("out/m")).expect("copied");
        let copied: Vec<String> = files_below(&s.0.join("out"))
            .expect("files")
            .iter()
            .map(|f| {
                f.strip_prefix(s.0.join("out"))
                    .expect("rel")
                    .display()
                    .to_string()
            })
            .collect();
        assert_eq!(
            copied,
            [
                "m/data/deep/y.txt",
                "m/data/x.txt",
                "m/libplugin.so",
                "m/link.qml",
                "m/qmldir"
            ]
        );
        assert!(!s.0.join("out/m/link.qml").is_symlink());
    }

    #[test]
    fn linuxdeploy_gets_every_directory_that_directly_holds_a_shared_object() {
        let s = Scratch::new("deploy");
        s.write("usr/plugins/platforms/libqxcb.so", b"");
        s.write("usr/plugins/platforms/readme.txt", b"");
        s.write("usr/plugins/kf6/kirigami/platform/org.kde.desktop.so", b"");
        s.write("usr/plugins/empty/only.txt", b"");
        s.write("usr/qml/QtQuick/libqtquickplugin.so", b"");
        s.write("usr/qml/QtQuick/Controls/libcontrols.so", b"");
        s.write("usr/qml/QtQuick/Controls/qmldir", b"");
        s.write("usr/lib/libignored.so", b"");
        let dirs = deploy_dirs(&s.0).expect("dirs");
        let rel: Vec<String> = dirs
            .iter()
            .map(|d| d.strip_prefix(&s.0).expect("rel").display().to_string())
            .collect();
        assert_eq!(
            rel,
            [
                "usr/plugins/kf6/kirigami/platform",
                "usr/plugins/platforms",
                "usr/qml/QtQuick",
                "usr/qml/QtQuick/Controls"
            ]
        );
        assert!(deploy_dirs(&s.0.join("nothing")).expect("none").is_empty());
    }

    #[test]
    fn bundled_txt_lists_every_file_sorted_with_its_package_and_source() {
        let info = |v: &str, s: &str, sv: &str| PackageInfo {
            version: v.into(),
            source: s.into(),
            source_version: sv.into(),
        };
        let files = [
            Bundled {
                path: "usr/qml/QtQuick/libqtquickplugin.so".into(),
                package: "qml6-module-qtquick".into(),
                info: info("6.11.1-1", "qtdeclarative-opensource-src", "6.11.1-1"),
            },
            Bundled {
                path: "usr/lib/libavcodec.so.60".into(),
                package: "libavcodec60".into(),
                info: info("7:6.1.1-3ubuntu5", "ffmpeg", "7:6.1.1-3ubuntu5"),
            },
        ];
        let text = render_bundled(&files);
        assert!(text.contains("Ubuntu 24.04 and KDE neon"), "{text}");
        assert!(text.contains("corresponding sources"), "{text}");
        assert!(text.contains("bundled/"), "{text}");
        let lines: Vec<&str> = text.lines().filter(|l| l.starts_with("usr/")).collect();
        assert_eq!(
            lines,
            [
                "usr/lib/libavcodec.so.60  libavcodec60 7:6.1.1-3ubuntu5  source ffmpeg 7:6.1.1-3ubuntu5",
                "usr/qml/QtQuick/libqtquickplugin.so  qml6-module-qtquick 6.11.1-1  source qtdeclarative-opensource-src 6.11.1-1"
            ]
        );
    }

    /// The source of a library in `usr/lib` is the file linuxdeploy says it copied, also from a
    /// folder below the library directory; copies elsewhere and other lines are not taken.
    #[test]
    fn the_libraries_linuxdeploy_copied_come_from_its_log() {
        let log = "\
Deploying shared library /usr/lib/x86_64-linux-gnu/pulseaudio/libpulsecommon-16.1.so
Copying file /usr/lib/x86_64-linux-gnu/pulseaudio/libpulsecommon-16.1.so to /b/AppDir/usr/lib/libpulsecommon-16.1.so
Copying file /lib/x86_64-linux-gnu/libKF6I18n.so.6 to /b/AppDir/usr/lib/libKF6I18n.so.6
Copying file /b/kourier.svg to /b/AppDir/usr/share/icons/hicolor/scalable/apps/kourier.svg
Copying file /lib/x86_64-linux-gnu/libz.so.1 to /b/AppDir/usr/lib/sub/libz.so.1
Setting rpath in ELF file /b/AppDir/usr/lib/libpulsecommon-16.1.so to $ORIGIN
";
        let copied = copied_libraries(log);
        assert_eq!(
            copied.into_iter().collect::<Vec<_>>(),
            vec![
                (
                    "libKF6I18n.so.6".to_owned(),
                    PathBuf::from("/lib/x86_64-linux-gnu/libKF6I18n.so.6")
                ),
                (
                    "libpulsecommon-16.1.so".to_owned(),
                    PathBuf::from("/usr/lib/x86_64-linux-gnu/pulseaudio/libpulsecommon-16.1.so")
                ),
            ]
        );
    }

    #[test]
    fn a_package_line_is_three_tab_separated_fields() {
        assert_eq!(
            parse_package_info("1:2.3-4\tsrc\t1:2.3-5\n").expect("info"),
            PackageInfo {
                version: "1:2.3-4".into(),
                source: "src".into(),
                source_version: "1:2.3-5".into()
            }
        );
        for bad in ["", "a\tb\n", "a\t\tc", "a\tb\tc\td"] {
            assert!(parse_package_info(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_log_tail_and_the_guessing_variables() {
        assert_eq!(tail_lines("a\nb\nc\nd\n", 2), "c\nd");
        assert_eq!(tail_lines("a\n", 60), "a");
        let vars = [
            "PATH",
            "GITHUB_REPOSITORY",
            "GITLAB_CI",
            "TRAVIS_REPO_SLUG",
            "CI",
            "HOME",
        ]
        .map(String::from);
        assert_eq!(
            update_information_vars(vars),
            ["GITHUB_REPOSITORY", "GITLAB_CI", "TRAVIS_REPO_SLUG", "CI"]
        );
    }

    #[test]
    fn the_styles_are_skipped_by_name_and_by_prefix_with_a_dot() {
        assert!(is_skipped_style("QtQuick.Controls.Material"));
        assert!(is_skipped_style("QtQuick.Controls.Material.impl"));
        assert!(!is_skipped_style("QtQuick.Controls.MaterialStyle"));
        assert!(!is_skipped_style("QtQuick.Controls"));
    }

    #[test]
    fn an_elf_file_is_told_by_its_first_bytes() {
        let s = Scratch::new("elf");
        assert!(is_elf(&s.write("a", b"\x7fELF\x02\x01")).expect("elf"));
        assert!(!is_elf(&s.write("b", b"#!/bin/sh\n")).expect("script"));
        assert!(!is_elf(&s.write("c", b"")).expect("empty"));
    }

    #[test]
    fn the_qt_conf_points_qt_at_the_appdirs_plugins_and_qml() {
        assert_eq!(
            QT_CONF,
            "[Paths]\nPrefix = ..\nPlugins = plugins\nQmlImports = qml\n"
        );
    }
}

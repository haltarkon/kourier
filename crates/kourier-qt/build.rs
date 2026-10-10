use std::path::{Path, PathBuf};

use cxx_qt_build::{CxxQtBuilder, QmlFile, QmlModule};

/// Every `.qml` file below `dir`, sorted, as paths relative to the crate.
fn qml_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            println!("cargo::rerun-if-changed={}", path.display());
            qml_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "qml") {
            out.push(path);
        }
    }
}

/// A KDE library the shim (`cpp/kde.cpp`, `cpp/i18n.cpp`) is compiled and linked against.
struct KdeLibrary {
    /// A header the shim includes, below the include root.
    header: &'static str,
    /// The include directories its CMake target names, below the include root.
    include_dirs: &'static [&'static str],
    /// The libraries to link (`lib<name>.so`).
    link: &'static [&'static str],
    /// The Debian and KDE neon package with the header and the `.so` link.
    package: &'static str,
}

/// KDE Frameworks 6 and Kirigami Add-ons ship CMake config files and (mostly) no pkg-config
/// files, so what their targets say is written down here: `KF6::CoreAddons`, `KF6::I18n` and
/// `KF6::I18nQml`, `KF6::Crash`, and Kirigami Add-ons' `KirigamiApp`.
const KDE_LIBRARIES: [KdeLibrary; 4] = [
    KdeLibrary {
        header: "KF6/KCoreAddons/KAboutData",
        include_dirs: &["KF6/KCoreAddons"],
        link: &["KF6CoreAddons"],
        package: "kf6-kcoreaddons-dev",
    },
    KdeLibrary {
        header: "KF6/KI18n/KLocalizedQmlContext",
        include_dirs: &["KF6/KI18n"],
        link: &["KF6I18n", "KF6I18nQml"],
        package: "kf6-ki18n-dev",
    },
    KdeLibrary {
        header: "KF6/KCrash/KCrash",
        include_dirs: &["KF6/KCrash"],
        link: &["KF6Crash"],
        package: "kf6-kcrash-dev",
    },
    KdeLibrary {
        header: "KirigamiAddons/App/KirigamiAppDefaults",
        include_dirs: &["KirigamiAddons/App"],
        link: &["KirigamiApp"],
        package: "kf6-kirigami-addons-dev",
    },
];

/// The include directories of [`KDE_LIBRARIES`], and their link lines for cargo. The headers
/// are looked for below `<prefix>/include`, where the prefix is `KOURIER_KDE_PREFIX` or
/// `/usr`; the libraries are found on the linker's own search path (a prefix that is not the
/// system's also gets `<prefix>/lib` and `<prefix>/lib64` as search paths). Stops the build
/// with the package to install when a header is missing.
fn kde_libraries() -> Vec<PathBuf> {
    println!("cargo::rerun-if-env-changed=KOURIER_KDE_PREFIX");
    let prefix =
        std::env::var_os("KOURIER_KDE_PREFIX").map_or_else(|| "/usr".into(), PathBuf::from);
    let root = prefix.join("include");
    let mut missing = Vec::new();
    let mut dirs = Vec::new();
    for library in &KDE_LIBRARIES {
        let header = root.join(library.header);
        println!("cargo::rerun-if-changed={}", header.display());
        if !header.is_file() {
            missing.push(format!(
                "  {} (package {})",
                header.display(),
                library.package
            ));
            continue;
        }
        dirs.extend(library.include_dirs.iter().map(|d| root.join(d)));
        for name in library.link {
            println!("cargo::rustc-link-lib=dylib={name}");
        }
    }
    assert!(
        missing.is_empty(),
        "kourier-qt needs the development files of KDE Frameworks 6 and Kirigami Add-ons; \
         these headers were not found:\n{}\nOn Debian, Ubuntu and KDE neon: sudo apt install {}\n\
         (another install prefix: set KOURIER_KDE_PREFIX)",
        missing.join("\n"),
        KDE_LIBRARIES.map(|l| l.package).join(" "),
    );
    // KI18n's QML header includes `<qqmlintegration.h>`: Qt's header-only module
    // QtQmlIntegration (package qt6-declarative-dev), which cxx-qt-build cannot be asked for
    // as a module because it has no library.
    println!("cargo::rerun-if-env-changed=QMAKE");
    let qmake = std::env::var_os("QMAKE").unwrap_or_else(|| "qmake6".into());
    let qt_headers = std::process::Command::new(&qmake)
        .args(["-query", "QT_INSTALL_HEADERS"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()));
    let integration = qt_headers.map(|dir| dir.join("QtQmlIntegration"));
    match integration {
        Some(dir) if dir.join("qqmlintegration.h").is_file() => dirs.push(dir),
        _ => panic!(
            "kourier-qt needs Qt's header qqmlintegration.h (QtQmlIntegration; on Debian, \
             Ubuntu and KDE neon: sudo apt install qt6-declarative-dev); `{} -query \
             QT_INSTALL_HEADERS` did not lead to it",
            qmake.to_string_lossy()
        ),
    }
    if prefix != Path::new("/usr") {
        for lib in ["lib", "lib64"] {
            println!(
                "cargo::rustc-link-search=native={}",
                prefix.join(lib).display()
            );
        }
    }
    dirs
}

fn main() {
    println!("cargo::rerun-if-changed=qml");
    println!("cargo::rerun-if-changed=icons");
    let mut files = Vec::new();
    qml_files(Path::new("qml"), &mut files);
    files.sort();
    // The scripted session of a measuring build (cargo feature `perf`, `cargo xtask perf`):
    // outside `qml/`, so no other build carries it.
    println!("cargo::rerun-if-changed=perf");
    if std::env::var_os("CARGO_FEATURE_PERF").is_some() {
        files.push(PathBuf::from("perf/PerfDriver.qml"));
    }
    let qml: Vec<QmlFile> = files
        .iter()
        .map(|p| {
            // Shared constants (`Style`), translations (`L10n`), the media actions
            // (`MediaActions`), the message actions (`TimelineActions`) and the emoji lists
            // (`Emoji`) are QML singletons.
            const SINGLETONS: [&str; 5] = [
                "Style.qml",
                "L10n.qml",
                "MediaActions.qml",
                "TimelineActions.qml",
                "Emoji.qml",
            ];
            let singleton = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| SINGLETONS.contains(&n));
            QmlFile::from(p.as_path()).singleton(singleton)
        })
        .collect();

    let mut icons: Vec<String> = std::fs::read_dir("icons")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "svg"))
        .map(|p| p.display().to_string())
        .collect();
    icons.sort();

    // The development control channel (src/dev/, `--dev-control`) exists only with debug
    // assertions; release builds compile neither its bridge nor its C++.
    println!("cargo::rerun-if-env-changed=CARGO_CFG_DEBUG_ASSERTIONS");
    let dev = std::env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_some();
    let mut bridges = vec!["src/bridge.rs", "src/kde_ffi.rs"];
    if dev {
        bridges.push("src/dev_ffi.rs");
    }
    let mut builder = CxxQtBuilder::new_qml_module(QmlModule::new("app.kourier").qml_files(qml))
        .qt_module("Widgets")
        .qt_module("Quick")
        .files(bridges)
        .cpp_file("cpp/devtools.cpp")
        .cpp_file("cpp/desktop.cpp")
        .cpp_file("cpp/imageprovider.cpp")
        .cpp_file("cpp/kde.cpp")
        .cpp_file("cpp/i18n.cpp");
    let kde_include_dirs = kde_libraries();
    // SAFETY: only include directories are added; no flag that changes how cxx-qt's own
    // sources are compiled.
    builder = unsafe {
        builder.cc_builder(|cc| {
            cc.includes(&kde_include_dirs);
        })
    };
    if dev {
        builder = builder.cpp_file("cpp/devcontrol.cpp");
    }
    builder.qrc_resources(icons).build().export();
}

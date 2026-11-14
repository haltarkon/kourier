use super::*;
use std::fs;

const FILE: &str = "/run/user/1000/kourier/media/attachments/ab12.3mf";

fn entry(exec: &str) -> DesktopEntry {
    DesktopEntry {
        path: PathBuf::from("/usr/share/applications/x.desktop"),
        name: "Some Slicer".to_owned(),
        exec: exec.to_owned(),
        try_exec: None,
        icon: Some("slicer-icon".to_owned()),
        work_dir: None,
        mime_types: Vec::new(),
    }
}

fn argv(exec: &str) -> Option<Vec<String>> {
    exec_argv(&entry(exec), Path::new(FILE)).map(|v| {
        v.into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    })
}

fn strs(items: &[&str]) -> Option<Vec<String>> {
    Some(items.iter().map(|s| (*s).to_owned()).collect())
}

/// The four Exec lines of the research (`target/next/v6/research.md`) give the exact argument
/// vectors: the path for `%U`/`%F`/`%u`, `@@u` as `@@`, the slicer's own flags as written.
#[test]
fn the_four_research_exec_lines_give_their_argv() {
    assert_eq!(argv("bambu-studio %U"), strs(&["bambu-studio", FILE]));
    assert_eq!(
        argv(
            "/usr/bin/flatpak run --branch=master --arch=x86_64 --command=entrypoint --file-forwarding com.orcaslicer.OrcaSlicer @@u %U @@"
        ),
        strs(&[
            "/usr/bin/flatpak",
            "run",
            "--branch=master",
            "--arch=x86_64",
            "--command=entrypoint",
            "--file-forwarding",
            "com.orcaslicer.OrcaSlicer",
            "@@",
            FILE,
            "@@"
        ])
    );
    assert_eq!(argv("prusa-slicer %F"), strs(&["prusa-slicer", FILE]));
    assert_eq!(
        argv(
            "/usr/bin/flatpak run --branch=stable --arch=x86_64 --command=/app/bin/slic3r-app-launcher --file-forwarding com.prusa3d.PrusaSlicer --single-instance-on-url @@u %u @@"
        ),
        strs(&[
            "/usr/bin/flatpak",
            "run",
            "--branch=stable",
            "--arch=x86_64",
            "--command=/app/bin/slic3r-app-launcher",
            "--file-forwarding",
            "com.prusa3d.PrusaSlicer",
            "--single-instance-on-url",
            "@@",
            FILE,
            "@@"
        ])
    );
    assert_eq!(argv("UltiMaker-Cura %U"), strs(&["UltiMaker-Cura", FILE]));
}

#[test]
fn every_field_code_is_handled() {
    assert_eq!(argv("p %f %F %u %U"), strs(&["p", FILE, FILE, FILE, FILE]));
    assert_eq!(argv("p --file=%f"), strs(&["p", &format!("--file={FILE}")]));
    assert_eq!(
        argv("p %i %c %k"),
        strs(&[
            "p",
            "--icon",
            "slicer-icon",
            "Some Slicer",
            "/usr/share/applications/x.desktop"
        ])
    );
    assert_eq!(argv("p 100%% %U"), strs(&["p", "100%", FILE]));
    // The deprecated codes are removed (their argument goes with them).
    assert_eq!(argv("p %d %D %n %N %v %m %U"), strs(&["p", FILE]));
    // %i without an icon is nothing.
    let mut bare = entry("p %i %U");
    bare.icon = None;
    let got: Vec<String> = exec_argv(&bare, Path::new(FILE))
        .expect("usable")
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(got, ["p", FILE]);
    // An unknown code makes the entry unusable.
    assert_eq!(argv("p %z"), None);
    assert_eq!(argv("p %"), None);
}

#[test]
fn quotes_and_escapes_follow_the_spec() {
    assert_eq!(
        argv(r#""/opt/my slicer/bin" --name "a \"b\" \$c \\d""#),
        strs(&["/opt/my slicer/bin", "--name", r#"a "b" $c \d"#])
    );
    // A literal percent inside quotes is `%%`.
    assert_eq!(argv(r#"p "50%%" %U"#), strs(&["p", "50%", FILE]));
    // A field code inside a quoted argument makes the entry unusable.
    assert_eq!(argv(r#"p "%U""#), None);
    assert_eq!(argv(r#"p "--file=%f""#), None);
    // An unclosed quote too.
    assert_eq!(argv(r#"p "oops %U"#), None);
    // The string escapes of the value come first (`\s` is a space, `\\` a backslash).
    let parsed = parse_entry(
        "[Desktop Entry]\nType=Application\nExec=p\\sq %U\n",
        Path::new("/a.desktop"),
    )
    .expect("entry");
    assert_eq!(parsed.exec, "p q %U");
}

#[test]
fn the_exec_bounds_are_enforced() {
    let many = format!("p {}", "x ".repeat(MAX_EXEC_ARGS));
    assert_eq!(argv(&many), None, "65 arguments");
    let fits = format!("p {}", "x ".repeat(MAX_EXEC_ARGS - 1));
    assert!(argv(&fits).is_some(), "64 arguments");
    let long = format!(
        "[Desktop Entry]\nType=Application\nExec=p {}\n",
        "y".repeat(MAX_EXEC_BYTES)
    );
    assert!(parse_entry(&long, Path::new("/a.desktop")).is_none());
}

#[test]
fn a_desktop_entry_is_read_from_its_group_only() {
    let text = "[Desktop Action new]\nExec=other %U\nName=Act\n\n[Desktop Entry]\nType=Application\nName=OrcaSlicer\nName[de]=Orca\nExec=orca-slicer %U\nTryExec=orca-slicer\nIcon=orca\nPath=/work\nMimeType=model/stl;model/3mf;\n";
    let e = parse_entry(text, Path::new("/a.desktop")).expect("entry");
    assert_eq!(e.name, "OrcaSlicer");
    assert_eq!(e.exec, "orca-slicer %U");
    assert_eq!(e.try_exec.as_deref(), Some("orca-slicer"));
    assert_eq!(e.icon.as_deref(), Some("orca"));
    assert_eq!(e.work_dir, Some(PathBuf::from("/work")));
    assert_eq!(e.mime_types, ["model/stl", "model/3mf"]);
    for bad in [
        "[Desktop Entry]\nType=Application\nHidden=true\nExec=p\n",
        "[Desktop Entry]\nType=Link\nExec=p\n",
        "[Desktop Entry]\nType=Application\n",
        "[Desktop Entry]\nType=Application\nExec=\n",
    ] {
        assert!(parse_entry(bad, Path::new("/a.desktop")).is_none(), "{bad}");
    }
}

fn write(dir: &Path, name: &str, text: &str) {
    fs::create_dir_all(dir).expect("dir");
    fs::write(dir.join(name), text).expect("write");
}

fn desktop(name: &str, exec: &str, extra: &str) -> String {
    format!("[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\n{extra}")
}

fn found_ids(found: &[Found]) -> Vec<(&'static str, bool)> {
    found
        .iter()
        .map(|f| (f.slicer.id, f.entry.is_some()))
        .collect()
}

const PATH: &str = "/usr/bin:/bin";

#[test]
fn the_table_is_fixed_and_in_his_order() {
    let ids: Vec<&str> = SLICERS.iter().map(|s| s.id).collect();
    assert_eq!(ids, ["orcaslicer", "bambustudio", "prusaslicer", "cura"]);
    let names: Vec<&str> = SLICERS.iter().map(|s| s.name).collect();
    assert_eq!(
        names,
        [
            "OrcaSlicer",
            "Bambu Studio",
            "PrusaSlicer",
            "UltiMaker Cura"
        ]
    );
    assert_eq!(resolve_id(""), "orcaslicer");
    assert_eq!(resolve_id("nonsense"), "orcaslicer");
    assert_eq!(resolve_id("cura"), "cura");
}

#[test]
fn the_scan_matches_by_desktop_id_and_by_name_and_mime() {
    let dir = tempfile::tempdir().expect("dir");
    let apps = dir.path().join("applications");
    // By id (the second id of the table).
    write(
        &apps,
        "io.github.softfever.OrcaSlicer.desktop",
        &desktop("Orca", "/bin/true %U", ""),
    );
    // By Name + MIME (an AppImage integration with an odd file name).
    write(
        &apps,
        "appimagekit-bambu.desktop",
        &desktop("bambu studio", "/bin/true %U", "MimeType=model/3mf;\n"),
    );
    // The right name but no model MIME type: not it.
    write(
        &apps,
        "appimagekit-prusa.desktop",
        &desktop("PrusaSlicer", "/bin/true %F", "MimeType=text/plain;\n"),
    );
    let found = scan(&[apps], OsStr::new(PATH));
    assert_eq!(
        found_ids(&found),
        [
            ("orcaslicer", true),
            ("bambustudio", true),
            ("prusaslicer", false),
            ("cura", false)
        ]
    );
    assert_eq!(
        found[0]
            .entry
            .as_ref()
            .expect("orca")
            .path
            .file_name()
            .expect("name"),
        "io.github.softfever.OrcaSlicer.desktop"
    );
}

#[test]
fn a_hidden_entry_or_a_missing_program_means_not_installed() {
    let dir = tempfile::tempdir().expect("dir");
    let apps = dir.path().join("applications");
    write(
        &apps,
        "OrcaSlicer.desktop",
        &desktop("OrcaSlicer", "/bin/true %U", "Hidden=true\n"),
    );
    // The stale AppImage entry of his PC: the program sat in a mount that is gone.
    write(
        &apps,
        "BambuStudio.desktop",
        &desktop("Bambu Studio", "/tmp/.mount_gone/AppRun %U", ""),
    );
    // A TryExec that is not there.
    write(
        &apps,
        "PrusaSlicer.desktop",
        &desktop(
            "PrusaSlicer",
            "/bin/true %F",
            "TryExec=/nonexistent/prusa\n",
        ),
    );
    // A program looked up on PATH that is not there, then one that is.
    write(
        &apps,
        "cura.desktop",
        &desktop("Cura", "no-such-program-here %U", ""),
    );
    let found = scan(std::slice::from_ref(&apps), OsStr::new(PATH));
    assert!(found.iter().all(|f| f.entry.is_none()), "{found:?}");
    write(&apps, "cura.desktop", &desktop("Cura", "true %U", ""));
    let found = scan(&[apps], OsStr::new(PATH));
    assert_eq!(
        found_ids(&found),
        [
            ("orcaslicer", false),
            ("bambustudio", false),
            ("prusaslicer", false),
            ("cura", true)
        ]
    );
}

#[test]
fn the_first_directory_with_a_desktop_id_wins() {
    let dir = tempfile::tempdir().expect("dir");
    let (first, second) = (dir.path().join("a"), dir.path().join("b"));
    write(
        &first,
        "com.orcaslicer.OrcaSlicer.desktop",
        &desktop("OrcaSlicer", "/bin/true --first %U", ""),
    );
    write(
        &second,
        "com.orcaslicer.OrcaSlicer.desktop",
        &desktop("OrcaSlicer", "/bin/true --second %U", ""),
    );
    let found = scan(&[first.clone(), second.clone()], OsStr::new(PATH));
    assert!(
        found[0]
            .entry
            .as_ref()
            .expect("orca")
            .exec
            .contains("--first")
    );
    let found = scan(&[second, first], OsStr::new(PATH));
    assert!(
        found[0]
            .entry
            .as_ref()
            .expect("orca")
            .exec
            .contains("--second")
    );
}

#[test]
fn an_oversized_desktop_file_is_skipped() {
    let dir = tempfile::tempdir().expect("dir");
    let apps = dir.path().join("applications");
    let padding = "# x\n".repeat((MAX_ENTRY_BYTES as usize / 4) + 8);
    write(
        &apps,
        "OrcaSlicer.desktop",
        &desktop("OrcaSlicer", "/bin/true %U", &padding),
    );
    let found = scan(&[apps], OsStr::new(PATH));
    assert!(found[0].entry.is_none());
}

#[test]
fn a_fixture_run_reads_only_the_named_directories() {
    let env = |name: &str| -> Option<OsString> {
        match name {
            "HOME" => Some("/home/x".into()),
            DEV_APPLICATIONS_ENV => Some("/tmp/a:/tmp/b:relative".into()),
            _ => None,
        }
    };
    if cfg!(debug_assertions) {
        assert_eq!(
            application_dirs(true, &env),
            [PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")]
        );
    }
    assert!(application_dirs(true, &|_| None).is_empty());
    let dirs = application_dirs(false, &env);
    assert_eq!(dirs[0], PathBuf::from("/home/x/.local/share/applications"));
    assert!(dirs.contains(&PathBuf::from("/usr/share/applications")));
    assert!(dirs.contains(&PathBuf::from(
        "/var/lib/flatpak/exports/share/applications"
    )));
    // Nothing twice.
    let mut sorted = dirs.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), dirs.len());
}

#[test]
fn the_command_hands_the_path_and_never_a_shell() {
    let e = entry("/bin/true --flag %U");
    let command = command(&e, Path::new(FILE), &["QT_X"]).expect("command");
    assert_eq!(command.get_program(), "/bin/true");
    let args: Vec<_> = command.get_args().collect();
    assert_eq!(args, ["--flag", FILE]);
    assert!(
        command.get_envs().any(|(n, v)| n == "QT_X" && v.is_none()),
        "the names Kourier set for Qt are taken out of the child's copy"
    );
    // A relative path is refused: a slicer would read it against its own folder.
    assert!(super::command(&e, Path::new("rel/a.stl"), &[]).is_none());
}

#[test]
fn a_model_copy_is_named_by_its_extension() {
    assert!(has_model_extension(Path::new("/x/ab.stl")));
    assert!(has_model_extension(Path::new("/x/ab.3MF")));
    assert!(!has_model_extension(Path::new("/x/ab")));
    assert!(!has_model_extension(Path::new("/x/ab.png")));
}

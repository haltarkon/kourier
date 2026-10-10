//! The XDG autostart entry (architecture §8.8, research qt-kirigami §7.5): a desktop file in
//! `$XDG_CONFIG_HOME/autostart/` that starts Kourier with the session, optionally hidden in the
//! tray (`--background`). The caller picks the directory, so test profiles never write into the
//! user's real autostart folder.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

/// What the autostart entry runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutostartEntry {
    /// The program (an absolute path, or a name on `PATH`).
    pub program: PathBuf,
    /// Extra arguments (`--profile=work`).
    pub args: Vec<String>,
    /// Start hidden in the tray (`--background`).
    pub background: bool,
    /// The `Name=` line.
    pub name: String,
    /// The `Icon=` line (the app id).
    pub icon: String,
}

/// Quotes one argument for an `Exec=` line (Desktop Entry spec, "The Exec key").
fn quote_arg(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@+,".contains(c));
    if plain {
        return arg.to_owned();
    }
    let mut out = String::from("\"");
    for c in arg.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    // `%` is a field code everywhere in the line, quoted or not.
    out.replace('%', "%%")
}

/// Escapes a string value (`\n`, `\t`, `\\`) for a desktop entry.
fn escape_value(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
        .replace('\r', "\\r")
}

impl AutostartEntry {
    /// The `Exec=` value.
    pub fn exec(&self) -> String {
        let mut parts = vec![quote_arg(&self.program.to_string_lossy())];
        parts.extend(self.args.iter().map(|a| quote_arg(a)));
        if self.background {
            parts.push("--background".into());
        }
        escape_value(&parts.join(" "))
    }

    /// The desktop file's text.
    pub fn render(&self) -> String {
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name={name}\n\
             Exec={exec}\n\
             Icon={icon}\n\
             Terminal=false\n\
             NoDisplay=true\n\
             X-GNOME-Autostart-enabled=true\n\
             X-KDE-autostart-after=panel\n",
            name = escape_value(&self.name),
            exec = self.exec(),
            icon = escape_value(&self.icon),
        )
    }
}

/// `<config_home>/autostart/<file_name>.desktop`.
pub fn entry_path(config_home: &Path, file_name: &str) -> PathBuf {
    config_home
        .join("autostart")
        .join(format!("{file_name}.desktop"))
}

/// Writes the entry atomically (mode 0644: the session reads it; it holds no secrets).
pub fn enable(path: &Path, entry: &AutostartEntry) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("autostart path without a directory"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("desktop.tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(&tmp)?;
        f.write_all(entry.render().as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Removes the entry (no error when it does not exist).
pub fn disable(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Whether the entry exists.
pub fn is_enabled(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(program: &str, background: bool) -> AutostartEntry {
        AutostartEntry {
            program: PathBuf::from(program),
            args: vec!["--profile=work".into()],
            background,
            name: "Kourier".into(),
            icon: "app.kourier.Kourier".into(),
        }
    }

    #[test]
    fn exec_line_quotes_and_escapes() {
        assert_eq!(
            entry("/usr/bin/kourier", true).exec(),
            "/usr/bin/kourier --profile=work --background"
        );
        assert_eq!(
            entry("/home/u/my apps/kourier", false).exec(),
            "\"/home/u/my apps/kourier\" --profile=work"
        );
        assert_eq!(
            entry("/opt/100%/kourier", false).exec(),
            "\"/opt/100%%/kourier\" --profile=work"
        );
        assert_eq!(
            entry("/a/\"b\"$c", false).exec(),
            "\"/a/\\\\\"b\\\\\"\\\\$c\" --profile=work"
        );
    }

    #[test]
    fn enable_disable_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = entry_path(dir.path(), "app.kourier.Kourier");
        assert!(!is_enabled(&path));
        enable(&path, &entry("/usr/bin/kourier", true)).expect("enable");
        assert!(is_enabled(&path));
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.starts_with("[Desktop Entry]\n"));
        assert!(text.contains("\nExec=/usr/bin/kourier --profile=work --background\n"));
        assert!(text.contains("\nX-KDE-autostart-after=panel\n"));
        enable(&path, &entry("/usr/bin/kourier", false)).expect("rewrite");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(!text.contains("--background"));
        disable(&path).expect("disable");
        assert!(!is_enabled(&path));
        disable(&path).expect("disable twice");
    }
}

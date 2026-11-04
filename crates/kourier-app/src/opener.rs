//! Programs Kourier starts for the person: their browser, the application a file opens in
//! ("Open in Another Application", a link, Kirigami's About page). Logic every front-end needs,
//! without a toolkit type (architecture §8.10).
//!
//! The program is `xdg-open` (the desktop's own dispatcher: it asks KDE's `kde-open`, GNOME's
//! `gio` or the configured default), started detached with a PER-CHILD environment: the
//! variables Kourier itself set for its toolkit at start (the front-end names them, see
//! `kourier_qt::prepare_environment`) are taken out of the child's copy, so the program gets the
//! environment the person gave Kourier. The process's own environment is never changed after
//! start (`std::env::set_var` is a data race with every thread that reads it). Every
//! `KOURIER_DEV_*` variable is taken out as well, whoever set it: a development run may hold the
//! last digits of a phone number there (`KOURIER_DEV_EXPECT_NUMBER_TAIL`), and a child's
//! environment is readable by every process of the same user. So is the WhatsApp call test switch
//! (`KOURIER_WA_CALL_BUILTIN`, [`crate::process::WA_CALL_BUILTIN_ENV`]).
//!
//! Log safety (§9.1): a URL or a path is never logged here.

use std::ffi::{OsStr, OsString};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

/// The program that opens a URL or a file.
pub const PROGRAM: &str = "xdg-open";

/// Whether `url` starts with a scheme (RFC 3986: a letter, then letters, digits, `+`, `-`, `.`,
/// and a colon). `xdg-open` takes options from the same place as the URL, so a string that does
/// not start with a scheme (`--help`, `-x`, a relative path) is never handed to it.
pub fn has_scheme(url: &str) -> bool {
    let mut chars = url.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    for c in chars {
        match c {
            ':' => return true,
            c if c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.') => {}
            _ => return false,
        }
    }
    false
}

/// The command that opens `url` for the person: `program url` with every name of `drop_env`
/// removed from the child's environment, no standard streams, in a process group of its own
/// (Ctrl-C at Kourier's terminal does not reach it). `None` for a string without a scheme.
/// Nothing is started.
pub fn command_for(program: impl AsRef<OsStr>, url: &str, drop_env: &[&str]) -> Option<Command> {
    command_for_in(program, url, drop_env, crate::process::dev_env_names_now())
}

/// [`command_for`] with the development variables to take out named by the caller. EVERY
/// `KOURIER_DEV_*` variable of the process goes ([`crate::process::DEV_ENV_PREFIX`]): one of them
/// holds the last digits of a phone number, and the environment of a child is readable by every
/// process of the same user (W2c, C0).
fn command_for_in(
    program: impl AsRef<OsStr>,
    url: &str,
    drop_env: &[&str],
    dev_env: Vec<OsString>,
) -> Option<Command> {
    if !has_scheme(url) {
        return None;
    }
    let mut command = Command::new(program);
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    for name in drop_env {
        command.env_remove(name);
    }
    for name in dev_env {
        command.env_remove(name);
    }
    Some(command)
}

/// [`command_for`] [`PROGRAM`].
pub fn command(url: &str, drop_env: &[&str]) -> Option<Command> {
    command_for(PROGRAM, url, drop_env)
}

/// Starts `command` and returns at once; a thread of its own waits for the child so it does not
/// stay a zombie (`xdg-open` may run for as long as the program it starts). Whether it started.
pub fn start(mut command: Command) -> bool {
    match command.spawn() {
        Ok(mut child) => {
            let reaper = std::thread::Builder::new()
                .name("kourier-opener".to_owned())
                .spawn(move || {
                    let _ = child.wait();
                });
            if let Err(e) = reaper {
                tracing::warn!(error = %e, "no thread to wait for the program that opens a link");
            }
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, kind = ?e.kind(), "the program that opens a link did not start");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the child will NOT inherit: the names `env_remove` took out of its copy.
    fn removed(command: &Command) -> Vec<String> {
        let mut names: Vec<String> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_child_gets_the_url_and_loses_the_names_kourier_set() {
        let command = command_for_in(
            PROGRAM,
            "https://example.org/a b",
            &["Z_ONE", "A_TWO"],
            Vec::new(),
        )
        .expect("a URL");
        assert_eq!(command.get_program(), PROGRAM);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["https://example.org/a b"]
        );
        assert_eq!(removed(&command), ["A_TWO", "Z_ONE"]);
        // Nothing is added to the child's environment: only taken out.
        assert_eq!(command.get_envs().count(), 2);
    }

    /// W2c C0: no `KOURIER_DEV_*` variable reaches the program a link opens in (one holds the last
    /// digits of a phone number), whoever set it, nor does the WhatsApp call test switch; a name that
    /// only looks like one stays.
    #[test]
    fn a_link_command_takes_out_every_development_variable() {
        let environ = [
            "PATH",
            "KOURIER_DEV_EXPECT_NUMBER_TAIL",
            "KOURIER_DEV_KEYRING",
            "KOURIER_DEVX",
            "XKOURIER_DEV_A",
            "KOURIER_WA_CALL_BUILTIN",
            "KOURIER_WA_CALL_BUILTIN_X",
            "XKOURIER_WA_CALL_BUILTIN",
        ];
        let dev = crate::process::dev_env_names(environ.map(OsString::from));
        let command =
            command_for_in(PROGRAM, "https://example.org/", &["QT_X"], dev).expect("a URL");
        assert_eq!(
            removed(&command),
            [
                "KOURIER_DEV_EXPECT_NUMBER_TAIL",
                "KOURIER_DEV_KEYRING",
                "KOURIER_WA_CALL_BUILTIN",
                "QT_X"
            ]
        );
    }

    /// The same through the process's real environment: a child of this test binary, whose
    /// environment holds the variable, builds the command for a link.
    #[test]
    fn the_command_for_a_link_reads_the_real_environment() {
        let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "opener::tests::the_child_link_command",
                "--nocapture",
            ])
            .env("KOURIER_OPENER_CHILD", "1")
            .env("KOURIER_DEV_EXPECT_NUMBER_TAIL", "47110815")
            .output()
            .expect("the child starts");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("test opener::tests::the_child_link_command ... ok"),
            "{stdout}"
        );
    }

    #[test]
    fn the_child_link_command() {
        if std::env::var_os("KOURIER_OPENER_CHILD").is_none() {
            return;
        }
        let command = command("https://example.org/", &[]).expect("a URL");
        assert!(
            removed(&command).contains(&"KOURIER_DEV_EXPECT_NUMBER_TAIL".to_owned()),
            "the child's environment held the variable, the command did not remove it"
        );
    }

    #[test]
    fn no_name_to_drop_leaves_the_environment_alone() {
        let command =
            command_for_in(PROGRAM, "mailto:someone@example.org", &[], Vec::new()).expect("a URL");
        assert_eq!(command.get_envs().count(), 0);
    }

    #[test]
    fn only_a_string_with_a_scheme_is_handed_to_the_program() {
        for url in [
            "http://example.org",
            "https://example.org/x",
            "mailto:a@b.example",
            "file:///tmp/a%20b.png",
            "a+b-c.d:rest",
        ] {
            assert!(has_scheme(url), "{url}");
            assert!(command(url, &[]).is_some(), "{url}");
        }
        // Options of xdg-open, a path, a scheme with a bad character, nothing at all.
        for url in [
            "--help",
            "-x",
            "--manual",
            "/etc/passwd",
            "relative/file",
            "1http://example.org",
            "ht tp://example.org",
            "-h:ttp://x",
            "noscheme",
            "",
            ":",
        ] {
            assert!(!has_scheme(url), "{url:?}");
            assert!(command(url, &[]).is_none(), "{url:?}");
        }
    }

    /// A program that does not exist is a `false`, not a panic.
    #[test]
    fn a_missing_program_is_reported() {
        let command =
            command_for("/nonexistent/kourier-opener", "https://example.org", &[]).expect("a URL");
        assert!(!start(command));
    }

    /// The child really runs, with the names taken out of what it inherits (a shell script that
    /// writes whether one variable of this process is in its environment, using shell built-ins
    /// only; the control runs it without a name to drop).
    #[test]
    fn the_started_child_has_the_environment_without_the_names() {
        // A variable this process has and `sh` does not make up for itself (dash sets `PATH`).
        let name = [
            "CARGO_MANIFEST_DIR",
            "HOME",
            "USER",
            "LANG",
            "XDG_RUNTIME_DIR",
        ]
        .into_iter()
        .find(|name| std::env::var_os(name).is_some())
        .expect("a variable to look for");
        let dir = std::env::temp_dir().join(format!("kourier-opener-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let script = dir.join("fake-open");
        let out = dir.join("said.txt");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"url=$1 seen=${{{name}+set}}\" > '{out}'\n",
                out = out.display()
            ),
        )
        .expect("write the script");
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("mode");
        let said = |drop_env: &[&str]| {
            let _ = std::fs::remove_file(&out);
            let command = command_for(&script, "https://example.org/x", drop_env).expect("a URL");
            assert!(start(command));
            let began = std::time::Instant::now();
            loop {
                // The file is written by one `echo`: a line, or nothing yet.
                if let Ok(text) = std::fs::read_to_string(&out)
                    && text.ends_with('\n')
                {
                    return text;
                }
                assert!(
                    began.elapsed() < std::time::Duration::from_secs(10),
                    "the child wrote nothing"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        assert_eq!(
            said(&[]),
            "url=https://example.org/x seen=set\n",
            "the control: a child inherits {name}"
        );
        assert_eq!(said(&[name]), "url=https://example.org/x seen=\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

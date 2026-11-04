//! Starting Kourier again ("Restart Now").
//!
//! A setting that is read when Kourier starts (an experimental network that is turned on or
//! off, `networks-on` in `kourier.toml`) takes effect at the next start. The window offers to
//! make that start itself: the UI asks the app ([`AppHandle::request_restart`](crate::AppHandle)),
//! quits its event loop, the app shuts down in order (the core stopped, the profile's lock and
//! the single-instance name free), and only then the binary runs this module's [`Restart`], which
//! replaces the process by a fresh Kourier of the same profile. Nothing starts before the old
//! one is gone, so the new one never finds the old one's lock. The shutdown of such a run also
//! writes the preferences it holds in memory when the file differs ("Turn On and Restart" sets
//! them and quits at once; a desktop task that was busy is abandoned with them unsaved), so the
//! new process never starts on the old file.
//!
//! A restart carries over the profile and NOTHING else of the command line: not `--background`
//! (the user pressed a button in the window, so the new one shows it), not a link given on the
//! command line (`sgnl://`, `signalcaptcha://` would run a second time), and not a fixture,
//! screenshot, dev-control or measuring option: those runs only record the request, they never
//! start another process.
//!
//! The environment goes with it, except the variables Kourier itself set for its toolkit and the
//! development variables that hold a secret (`KOURIER_DEV_EXPECT_NUMBER_TAIL`, the last digits
//! of a phone number, and, in a build with Telegram, the run tool's nonce `KOURIER_DEV_TELEGRAM_DIAL`):
//! the other `KOURIER_DEV_*` variables stay, so a development profile restarts in its own mode
//! (W2c, L3).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Command-line options of a run that is not a person's session (tests, pictures, a measuring
/// run, the debug control channel). A run with one of them never restarts itself.
const NOT_A_SESSION: [&str; 6] = [
    "--fixture",
    "--screenshot",
    "--page",
    "--size",
    "--dev-control",
    "--perf",
];

/// The development variables that hold a secret and never reach the new process: the last digits
/// of a phone number and (TR R0a fix G-4, review F4) the nonce of the Telegram run tool's
/// confirmation, which would otherwise stay in the environment of every child. The new process
/// would find no confirmation file anyway; the nonce has no business in its environment.
const SECRETS: &[&str] = &[
    crate::process::EXPECT_NUMBER_TAIL_ENV,
    #[cfg(feature = "telegram")]
    crate::telegram::DIAL_ENV,
];

/// How Kourier starts itself again: the program and its arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restart {
    /// The program to run (this binary).
    pub program: PathBuf,
    /// Its arguments: the profile of this run, if it named one.
    pub args: Vec<OsString>,
}

impl Restart {
    /// The restart of this run: `exe` with the arguments `args` (the process's, without the
    /// program name) asks for, or `None` for a run that never restarts itself (see the module
    /// documentation).
    pub fn of_run(exe: &Path, args: &[OsString]) -> Option<Self> {
        let text: Vec<std::borrow::Cow<'_, str>> =
            args.iter().map(|a| a.to_string_lossy()).collect();
        if text
            .iter()
            .any(|a| NOT_A_SESSION.iter().any(|flag| a.starts_with(flag)))
        {
            return None;
        }
        let args = args
            .iter()
            .zip(&text)
            .filter(|(_, t)| t.starts_with("--profile="))
            .map(|(a, _)| a.clone())
            .collect();
        Some(Self {
            program: still_there(exe),
            args,
        })
    }

    /// Replaces this process by the new one. `unset` are the environment variables Kourier
    /// itself set for its own use (`kourier_qt::env_set_by_kourier`): the new process starts
    /// without them, so it prepares its environment as a first start does. Returns only when
    /// the program could not be started, with the reason.
    pub fn exec(self, unset: &[&str]) -> std::io::Error {
        use std::os::unix::process::CommandExt as _;
        self.command_now(unset).exec()
    }

    /// The command [`exec`](Self::exec) runs: without the variables Kourier set and without the
    /// development variables that hold a secret ([`SECRETS`]: `KOURIER_DEV_EXPECT_NUMBER_TAIL`, the
    /// last digits of a phone number; the new process reads its own environment at its start, and
    /// the window before it makes itself non-dumpable is a window in which `/proc/<pid>/environ` is
    /// readable, W2c C0; and the Telegram run tool's nonce). The other `KOURIER_DEV_*` variables
    /// (the development keyring, no dumps, the QR url) stay, so a development profile restarts in
    /// its own mode. A run with `--dev-control` (smoke, T1) never restarts (`NOT_A_SESSION`), so a
    /// tail is never lost in such a run.
    fn command_now(&self, unset: &[&str]) -> std::process::Command {
        self.command_in(unset, SECRETS)
    }

    fn command_in(&self, unset: &[&str], secrets: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new(&self.program);
        command.args(&self.args);
        for name in unset.iter().chain(secrets) {
            command.env_remove(name);
        }
        command
    }
}

/// `exe` as a path that can be started: after a package update replaced the binary of a running
/// Kourier, Linux reports its path with " (deleted)" appended, and the new file stands under
/// the plain path.
fn still_there(exe: &Path) -> PathBuf {
    const DELETED: &str = " (deleted)";
    let text = exe.to_string_lossy();
    match text.strip_suffix(DELETED) {
        Some(plain) if !exe.exists() => PathBuf::from(plain),
        _ => exe.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    /// A session restarts with its profile and nothing else: not `--background`, not a link
    /// given on the command line.
    #[test]
    fn a_restart_keeps_the_profile_and_nothing_else() {
        let exe = Path::new("/usr/bin/kourier");
        let plain = Restart::of_run(exe, &[]).expect("a session");
        assert_eq!(plain.program, exe);
        assert!(plain.args.is_empty());
        let named = Restart::of_run(
            exe,
            &args(&[
                "--background",
                "--profile=work",
                "sgnl://linkdevice?uuid=1&pub_key=2",
                "signalcaptcha://x",
            ]),
        )
        .expect("a session");
        assert_eq!(named.args, args(&["--profile=work"]));
    }

    /// Fixture, picture, debug-control and measuring runs never start another process.
    #[test]
    fn a_run_that_is_not_a_session_never_restarts() {
        let exe = Path::new("/usr/bin/kourier");
        for flag in [
            "--fixture",
            "--fixture-link",
            "--fixture-network=whatsapp",
            "--screenshot=/tmp/a.png",
            "--page=chat",
            "--dev-control",
            "--perf-profile=/tmp/p",
        ] {
            assert_eq!(
                Restart::of_run(exe, &args(&["--profile=work", flag])),
                None,
                "{flag}"
            );
        }
    }

    /// The path of a binary a package update replaced is the plain one when that exists.
    #[test]
    fn a_replaced_binary_is_started_by_its_plain_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("kourier");
        std::fs::write(&real, b"#!/bin/sh\n").expect("write");
        let gone = PathBuf::from(format!("{} (deleted)", real.display()));
        assert_eq!(still_there(&gone), real);
        // A file that really has that name, or a path that never had the suffix: untouched.
        assert_eq!(still_there(&real), real);
        let odd = dir.path().join("a (deleted)");
        std::fs::write(&odd, b"x").expect("write");
        assert_eq!(still_there(&odd), odd);
    }

    /// What the new process does NOT inherit: the names `env_remove` took out of its copy.
    fn removed(command: &std::process::Command) -> Vec<String> {
        let mut names: Vec<String> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The secrets [`command_now`](Restart::command_now) takes out of the new process's
    /// environment in this build, sorted: the number tail, and the run tool's nonce in a build with
    /// Telegram (G-4).
    fn secret_names() -> Vec<String> {
        let mut names = vec!["KOURIER_DEV_EXPECT_NUMBER_TAIL".to_owned()];
        if cfg!(feature = "telegram") {
            names.push("KOURIER_DEV_TELEGRAM_DIAL".to_owned());
        }
        names.sort();
        names
    }

    /// W2c C0 / L3, TR R0a G-4: the secrets among the development variables (the tail, the Telegram
    /// run tool's nonce) do not reach the new process; the names Kourier set for Qt go as before; the
    /// other `KOURIER_DEV_*` variables are NOT taken out. Mutants: the tail not removed; the nonce
    /// not removed; every `KOURIER_DEV_*` removed again.
    #[test]
    fn a_restart_takes_out_the_secrets_and_keeps_the_other_development_variables() {
        let restart = Restart {
            program: PathBuf::from("/usr/bin/kourier"),
            args: args(&["--profile=work"]),
        };
        let command = restart.command_in(&["QT_X"], SECRETS);
        let mut expected = secret_names();
        expected.push("QT_X".to_owned());
        assert_eq!(removed(&command), expected);
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["--profile=work"]);
    }

    /// The same through the process's real environment: a child of this test binary, whose
    /// environment holds the tail, the run tool's nonce (a build with Telegram) and the development
    /// keyring, asks for the command `exec` would run: it removes the tail and the nonce and leaves
    /// the keyring variable alone (and `KOURIER_DEV_NO_DUMP` and the QR url with it).
    #[test]
    fn exec_takes_the_secrets_from_the_real_environment_and_keeps_the_keyring_variable() {
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"));
        child
            .args([
                "--exact",
                "restart::tests::the_child_restart_command",
                "--nocapture",
            ])
            .env("KOURIER_RESTART_CHILD", "1")
            .env("KOURIER_DEV_EXPECT_NUMBER_TAIL", "47110815")
            .env("KOURIER_DEV_KEYRING", "passphrase")
            .env("KOURIER_DEV_NO_DUMP", "1");
        #[cfg(feature = "telegram")]
        child.env(crate::telegram::DIAL_ENV, "ab".repeat(32));
        let out = child.output().expect("the child starts");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("test restart::tests::the_child_restart_command ... ok"),
            "{stdout}"
        );
    }

    #[test]
    fn the_child_restart_command() {
        if std::env::var_os("KOURIER_RESTART_CHILD").is_none() {
            return;
        }
        let restart = Restart {
            program: PathBuf::from("/usr/bin/kourier"),
            args: Vec::new(),
        };
        let command = restart.command_now(&[]);
        assert_eq!(
            removed(&command),
            secret_names(),
            "only the secrets leave the new process's environment"
        );
        for kept in ["KOURIER_DEV_KEYRING", "KOURIER_DEV_NO_DUMP"] {
            assert!(
                std::env::var_os(kept).is_some(),
                "the child's own environment holds {kept}, so the new process inherits it"
            );
        }
    }

    /// A program that cannot be started is an error, not a panic or a silent exit: the binary
    /// reports it and ends as an ordinary quit would.
    #[test]
    fn a_program_that_cannot_start_is_reported() {
        let restart = Restart {
            program: PathBuf::from("/nonexistent/kourier-restart-test"),
            args: Vec::new(),
        };
        let error = restart.exec(&[]);
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
}

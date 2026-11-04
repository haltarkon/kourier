//! The command line (X1): no secret on it, and the known flags are read before the secret words.

use std::path::PathBuf;

use super::{args, error_of};
use crate::tg_run::tg_run;

/// X1: no secret is a command line. Every spelling that could carry a number, a tail, a code, a
/// password, an API value, a hash, a nonce or a token is refused, as a flag or in the command's
/// place, and the error names neither the flag nor what followed it. Mutant: `api` dropped from the
/// words (`--api-id` is the one flag that holds no other word).
#[test]
fn no_secret_values_on_a_command_line() {
    let value = "58301947";
    // The spelling the lane's source check refuses in a file is made at run time.
    let api_id_form = format!("--api{}={value}", "_id");
    for line in [
        vec!["connect", "--confirmed-by-user", "--phone", value],
        vec!["connect", "--tail", value],
        vec!["connect", "--api-id", value],
        vec!["connect", api_id_form.as_str()],
        vec!["connect", "--apiid", value],
        vec!["connect", "--hash", value],
        vec!["connect", "--api-hash", value],
        vec!["connect", "--code", value],
        vec!["connect", "--password=58301947"],
        vec!["connect", "--password", value],
        vec!["connect", "--nonce", value],
        vec!["connect", "--token", value],
        vec!["connect", "--digits", value],
        vec!["connect", "--number", value],
        vec!["connect", "--phone-number=58301947"],
        vec!["connect", "--msisdn", value],
        vec!["connect", "--expect-tail", value],
        vec!["connect", "--TAIL", value],
        vec!["wipe", "--code", value],
        vec!["--phone", value],
        vec!["--api-id=58301947"],
    ] {
        let error = error_of(args(&line));
        assert!(
            error.contains("takes no number, tail, code, password"),
            "{line:?}: {error}"
        );
        assert!(
            !error.contains(value) && !error.contains("5830"),
            "{line:?} echoed the value: {error}"
        );
        for word in [
            "phone", "tail", "api", "hash", "password", "nonce", "token", "digit",
        ] {
            assert!(
                !error.contains(&format!("--{word}")),
                "{line:?} echoed the flag: {error}"
            );
        }
    }
}

/// Whatever else the parser does not know is not echoed either, except a flag's NAME.
#[test]
fn an_unknown_argument_is_not_echoed_except_a_flag_name() {
    let value = "58301947";
    for line in [
        vec!["connect", value],
        vec!["connect", "--58301947"],
        vec!["connect", "--profile=58301947"],
        vec![value],
        vec!["tail"],
        vec!["connect", "--enable-x=58301947"],
    ] {
        let error = error_of(args(&line));
        assert!(!error.contains(value), "{line:?} echoed: {error}");
    }
    let error = error_of(args(&["connect", "--no-such-flag"]));
    assert!(error.contains("--no-such-flag"), "{error}");
    let error = error_of(args(&["connect", "--no-such-flag=58301947"]));
    assert!(
        error.contains("--no-such-flag") && !error.contains(value),
        "{error}"
    );
}

/// Critic 13: the known flags are matched FIRST, so a flag that holds a secret word as a part of
/// another word (`--allow-unverified-ranges`, `--allow-unencrypted-disk`) is read, not refused.
#[test]
fn the_parser_reads_every_documented_command_and_flag() {
    let a = args(&[
        "connect",
        "--confirmed-by-user",
        "--env",
        "test",
        "--profile",
        "r0",
        "--no-build",
        "--allow-unencrypted-disk",
        "--allow-unverified-ranges",
    ])
    .expect("parses");
    assert_eq!(a.command, "connect");
    assert!(a.confirmed && a.no_build && a.allow_unencrypted && a.allow_unverified_ranges);
    assert_eq!(a.env.as_deref(), Some("test"));
    assert_eq!(a.place.profile.as_deref(), Some("r0"));
    let a = args(&["wipe", "--profile", "x", "--yes-delete-the-login"]).expect("parses");
    assert!(a.yes_delete);
    assert_eq!(a.place.profile.as_deref(), Some("x"));
    let a = args(&["connect", "--rehearsal-place", "/somewhere/p"]).expect("parses");
    assert_eq!(a.place.rehearsal_place, Some(PathBuf::from("/somewhere/p")));
    for command in ["connect", "rehearse", "wipe", "ranges", "help"] {
        assert_eq!(args(&[command]).expect("parses").command, command);
    }
    assert_eq!(args(&["--help"]).expect("parses").command, "help");
    // A flag that needs a value says so; the default profile is not in the parser's hands.
    assert!(error_of(args(&["connect", "--env"])).contains("--env needs"));
    assert_eq!(args(&["connect"]).expect("parses").place.profile, None);
    assert!(args(&[]).is_err());
}

/// No flag of R0a is a send, a number or a step: the command list is closed.
#[test]
fn the_commands_of_the_other_run_tools_are_not_commands_here() {
    for command in [
        "start", "tail", "step", "steps", "state", "report", "stop", "backup",
    ] {
        let error = error_of(args(&[command]));
        assert!(
            error.contains("unknown tg-run command"),
            "{command}: {error}"
        );
        assert!(!error.contains(&format!("command {command}")), "{error}");
    }
}

#[test]
fn the_usage_status_is_2_and_help_is_0() {
    assert_eq!(tg_run(&[]).expect("usage"), 2);
    assert_eq!(tg_run(&["help".to_owned()]).expect("help"), 0);
}

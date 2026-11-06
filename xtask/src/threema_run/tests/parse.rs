//! The command line: no ID, no digest of one and no flag that names it is ever a command line, and
//! the known flags are read before the secret words.

use std::path::PathBuf;

use super::{args, digest_shape, error_of, id_shape, strings};
use crate::threema_run::{
    NO_SECRETS_ON_A_COMMAND_LINE, SECRET_WORDS, holds_a_digest, is_a_secret_flag, looks_like_an_id,
    parse_args, threema_run,
};

/// The documented flags: each is read, none of them is mistaken for a secret one.
const FLAGS: [&str; 15] = [
    "--profile",
    "--rehearsal-place",
    "--confirmed-by-user",
    "--for-link",
    "--after-halt",
    "--allow-schema-change",
    "--allow-unencrypted-disk",
    "--no-build",
    "--force",
    "--keep",
    "--to",
    "--yes-delete-the-login",
    "--seconds",
    "-h",
    "--help",
];

/// M2 (the mutant): no ID, no digest of one and nothing that names the ID is a command line. Every
/// spelling is refused, as a flag, as a value, as a positional word or in the command's place, with
/// the one fixed sentence, and the error echoes neither the value nor the flag. Mutant:
/// `is_a_secret_flag` -> `false` (the flag rows are then "unknown argument", the sentence is gone).
#[test]
fn no_id_or_digest_is_a_command_line() {
    let id = id_shape();
    let lower = id.to_lowercase();
    let gateway = ["*", "SYNTH01"].concat();
    let digest = digest_shape();
    let prefixed = ["sha", "256:", digest.as_str()].concat();
    let with_flag = format!("--id={id}");
    let in_a_path = format!("--to=/x/{digest}/y");
    let mut lines: Vec<Vec<String>> = Vec::new();
    // The flags that name the ID or a value derived from it, with and without a value.
    for flag in [
        "--id",
        "--ID",
        "--threema",
        "--threema-id",
        "--test-id",
        "--digest",
        "--hash",
        "--sha256",
        "--key",
        "--code",
        "--token",
        "--number",
        "--nonce",
        "--password",
        "--secret",
        "--phone",
        "--tail",
        "--digits",
    ] {
        let valued = format!("{flag}={id}");
        lines.push(strings(&["start", flag]));
        lines.push(strings(&["start", flag, id.as_str()]));
        lines.push(strings(&["wipe", valued.as_str()]));
    }
    // The value itself, in every place a value or a word can stand.
    for line in [
        vec!["start", with_flag.as_str()],
        vec!["id", id.as_str()],
        vec!["id", lower.as_str()],
        vec!["id", gateway.as_str()],
        vec!["start", "--profile", id.as_str()],
        vec!["start", "--profile", lower.as_str()],
        vec!["start", "--profile", digest.as_str()],
        vec!["start", "--to", prefixed.as_str()],
        vec!["start", "--rehearsal-place", digest.as_str()],
        vec!["start", digest.as_str()],
        vec!["start", prefixed.as_str()],
        vec!["backup", in_a_path.as_str()],
        vec!["step", id.as_str()],
        vec!["step", "link", id.as_str()],
        vec!["step", "passive", "--seconds", id.as_str()],
        // In the command's place.
        vec![id.as_str()],
        vec![digest.as_str()],
        vec![prefixed.as_str()],
        vec!["--id"],
        vec!["--threema-id", id.as_str()],
    ] {
        lines.push(strings(&line));
    }
    for line in lines {
        let line_text: Vec<&str> = line.iter().map(String::as_str).collect();
        let error = error_of(parse_args(&line));
        assert!(
            error.contains("takes no Threema ID"),
            "{line_text:?}: not the fixed sentence: {error}"
        );
        assert_eq!(
            error, NO_SECRETS_ON_A_COMMAND_LINE,
            "{line_text:?}: more than the fixed sentence"
        );
        for secret in [&id, &lower, &digest, &gateway] {
            assert!(
                !error.contains(secret.as_str()),
                "{line_text:?} echoed a value: {error}"
            );
        }
        for word in SECRET_WORDS {
            assert!(
                !error.contains(&format!("--{word}")),
                "{line_text:?} echoed a flag: {error}"
            );
        }
    }
}

/// The shapes, case by case: eight letters or digits (a `*` first for a Gateway ID) is an ID; a run
/// of 64 hexadecimal digits anywhere is a digest, and 63 is not.
#[test]
fn the_shapes_of_an_id_and_of_a_digest() {
    for yes in [
        id_shape().as_str(),
        id_shape().to_lowercase().as_str(),
        "12345678",
        "*ABCDEFG",
        "abcdefgh",
    ] {
        assert!(looks_like_an_id(yes), "{yes}");
    }
    for no in [
        "",
        "rt1",
        "abcdefg",
        "abcdefghi",
        "ABCD-123",
        "ABCD 123",
        "A*BCDEFG",
        "\u{e9}bcdefgh",
        "--no-build",
    ] {
        assert!(!looks_like_an_id(no), "{no}");
    }
    let digest = digest_shape();
    assert!(holds_a_digest(&digest));
    assert!(holds_a_digest(&format!("sha256:{digest}")));
    assert!(holds_a_digest(&format!("--to=/x/{digest}/y")));
    assert!(!holds_a_digest(&digest[..63]));
    // One non-hex character inside breaks the run.
    assert!(!holds_a_digest(&format!(
        "{}-{}",
        &digest[..32],
        &digest[..32]
    )));
    assert!(!holds_a_digest("sha256:"));
}

/// Whatever else the parser does not know is not echoed either, except a flag's NAME.
#[test]
fn an_unknown_argument_is_not_echoed_except_a_flag_name() {
    let value = "58301947";
    for line in [
        vec!["start", "word"],
        vec!["start", "--58301947"],
        vec!["start", "--profile=58301947"],
        vec!["start", "--enable-x=58301947"],
        vec!["start", "/some/place/58301947"],
    ] {
        let error = error_of(args(&line));
        assert!(!error.contains(value), "{line:?} echoed: {error}");
    }
    let error = error_of(args(&["start", "--no-such-flag"]));
    assert!(error.contains("--no-such-flag"), "{error}");
    let error = error_of(args(&["start", "--no-such-flag=somevalue"]));
    assert!(
        error.contains("--no-such-flag") && !error.contains("somevalue"),
        "{error}"
    );
    let error = error_of(args(&["start", "alien"]));
    assert!(
        error.contains("not shown") && !error.contains("alien"),
        "{error}"
    );
}

/// The known flags are matched FIRST: none of the documented ones is a secret flag, whatever letters
/// it holds, and each is read into its field.
#[test]
fn the_parser_reads_every_documented_command_and_flag() {
    for flag in FLAGS {
        assert!(!is_a_secret_flag(flag), "{flag}");
    }
    let a = args(&[
        "start",
        "--confirmed-by-user",
        "--for-link",
        "--after-halt",
        "--allow-schema-change",
        "--allow-unencrypted-disk",
        "--no-build",
        "--profile",
        "rt1",
    ])
    .expect("parses");
    assert_eq!(a.command, "start");
    assert!(a.confirmed && a.for_link && a.after_halt && a.no_build);
    assert!(a.allow_schema_change && a.allow_unencrypted);
    assert_eq!(a.place.profile.as_deref(), Some("rt1"));
    let a = args(&["wipe", "--profile", "x", "--yes-delete-the-login"]).expect("parses");
    assert!(a.yes_delete);
    assert_eq!(a.place.profile.as_deref(), Some("x"));
    let a = args(&["rehearse", "--keep", "--no-build"]).expect("parses");
    assert!(a.keep && a.no_build);
    let a = args(&["stop", "--force"]).expect("parses");
    assert!(a.force);
    let a = args(&["backup", "--to", "/somewhere/else"]).expect("parses");
    assert_eq!(a.to, Some(PathBuf::from("/somewhere/else")));
    let a = args(&["start", "--rehearsal-place", "/somewhere/p"]).expect("parses");
    assert_eq!(a.place.rehearsal_place, Some(PathBuf::from("/somewhere/p")));
    // A step has a name of lower-case letters and dashes, and takes seconds.
    let a = args(&["step", "phone-sleep", "--seconds", "600"]).expect("parses");
    assert_eq!(a.step.as_deref(), Some("phone-sleep"));
    assert_eq!(a.seconds, Some(600));
    // A step name of eight lower-case letters is a step name, not an ID.
    assert_eq!(
        args(&["step", "abcdefgh"]).expect("parses").step.as_deref(),
        Some("abcdefgh")
    );
    assert!(error_of(args(&["step"])).contains("step needs a name"));
    assert!(error_of(args(&["step", "--force"])).contains("step needs a name"));
    assert!(error_of(args(&["step", "Link"])).contains("lower-case letters and dashes"));
    assert!(error_of(args(&["step", "x", "--seconds", "soon"])).contains("--seconds needs"));
    for command in [
        "start",
        "id",
        "answer",
        "steps",
        "state",
        "report",
        "stop",
        "backup",
        "wipe",
        "rehearse",
        "help",
        "__supervise",
    ] {
        assert_eq!(args(&[command]).expect("parses").command, command);
    }
    assert_eq!(args(&["--help"]).expect("parses").command, "help");
    // A flag that needs a value says so; the default profile is not in the parser's hands.
    assert!(error_of(args(&["start", "--profile"])).contains("--profile needs"));
    assert_eq!(args(&["start"]).expect("parses").place.profile, None);
    assert!(args(&[]).is_err());
}

/// The command list is closed: the other run tools' commands are not commands here, and the word that
/// was typed is not echoed.
#[test]
fn the_commands_of_the_other_run_tools_are_not_commands_here() {
    for command in ["tail", "connect", "ranges", "allow", "link", "send"] {
        let error = error_of(args(&[command]));
        assert!(
            error.contains("unknown threema-run command"),
            "{command}: {error}"
        );
        assert!(!error.contains(&format!("command {command}")), "{error}");
    }
}

/// `id` takes no value: the digest of the ID is made in that process, and nothing is given.
#[test]
fn id_takes_no_value() {
    assert_eq!(args(&["id"]).expect("parses").command, "id");
    assert_eq!(
        args(&["id", "--profile", "rt1"]).expect("parses").command,
        "id"
    );
    assert!(args(&["id", "value"]).is_err());
}

#[test]
fn the_usage_status_is_2_and_help_is_0() {
    assert_eq!(threema_run(&[]).expect("usage"), 2);
    assert_eq!(threema_run(&["help".to_owned()]).expect("help"), 0);
}

#[test]
fn the_test_phone_flags_parse_and_the_secret_rules_still_hold() {
    let a = args(&[
        "start",
        "--confirmed-by-user",
        "--test-phone",
        "192.0.2.19:5555",
    ])
    .unwrap();
    assert_eq!(a.test_phone.as_deref(), Some("192.0.2.19:5555"));
    let a = args(&["step", "unlink", "--user-said-yes"]).unwrap();
    assert!(a.user_said_yes);
    let a = args(&["id", "--from-test-phone"]).unwrap();
    assert!(a.from_test_phone);
    assert_eq!(
        args(&["answer", "--relayed", "y"]).unwrap().relayed,
        Some(true)
    );
    assert_eq!(
        args(&["answer", "--relayed", "n"]).unwrap().relayed,
        Some(false)
    );
    let a = args(&["answer", "--relayed", "y", "--question", "pp-delivered"]).unwrap();
    assert_eq!(a.question.as_deref(), Some("pp-delivered"));
    assert!(args(&["answer", "--relayed", "y", "--question"]).is_err());
    assert!(args(&["answer", "--relayed", "maybe"]).is_err());
    assert!(args(&["answer", "--relayed"]).is_err());
    // a serial with the shape of an ID is still refused, without echoing it
    let e = error_of(args(&["start", "--test-phone", &id_shape()]));
    assert!(!e.contains(&id_shape()), "{e}");
    // and the flags a phone's value could hide behind are not opened by the new ones
    assert!(args(&["start", "--phone-number", "1"]).is_err());
    assert!(args(&["start", "--digest", "1"]).is_err());
}

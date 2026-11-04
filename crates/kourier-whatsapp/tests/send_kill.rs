//! W5a chunk 15b (brief §5, decisions D4, D5, D21; critic M6): the KILL LOOP of the send path over the
//! scripted server. A child plays the core: for each of three messages (two texts the server takes, one it
//! refuses with a 463) it notes the attempt (an fsynced line of `core.log`), sends the `Transmit`, and notes
//! the outcome it was answered. The process is ABORTED at a failpoint, `point:1`, `point:2`, ... until the
//! child ends normally: the backend's outcome zone (`wa_inside_send_outcome` / `wa_after_send_outcome`) and
//! the core's two points around the backend's answer (`outbox_after_attempt_commit`: the attempt noted, no
//! `Transmit` yet; `outbox_before_outcome_commit`: answered, the outcome not yet noted). A second child plays
//! the next start: the core sends again every message whose outcome it did not note, as the core's outbox
//! does at a start. The Wire logs (and fsyncs) every frame BEFORE it answers it, one log per child.
//!
//! The invariant, exactly as D21 states it (critic M6; amended by fix F1: "the backend writes at most one frame of a
//! local per connection while its outcome is unknown; after a nack at most one more per floor; upstream's own same-id
//! resends, its phash repair to uncovered devices and its answers to retry receipts, are the protocol's and keep the id"
//! (neither can happen in this loop: one device, no retry receipt, no nack; `send_wire` has the nack cases)), checked
//! after the kill and after the recovery:
//!  - every `<message>` frame carries one of the core's locals as its stanza id (no fresh id);
//!  - at most one frame per local per connection (each child has one connection: it asserts so);
//!  - no frame after `acked` or `refused`: a local the ledger holds at the kill gets no frame at the next
//!    start (it is answered from the ledger);
//!  - an answer the core noted was durable before it was given: every outcome in `core.log` at the kill is
//!    in the ledger, the same outcome;
//!  - a SECOND frame of a local (on the later connection) only at the kill point between the logged frame and
//!    the ledger's commit (`wa_inside_send_outcome`). "Once" there rests on the server's dedupe of the same
//!    id (U1, accepted by his answer "Resend, same id"); a 463 is then met once more (D21);
//!  - in the end every local has its outcome in the core and in the ledger: the texts `acked`, the 463
//!    `refused`.
//!
//! W5b chunk B15 (brief §5 `send_kill::`, decisions G4, G19): a FOURTH message carries a file (a document the
//! core sealed into the account's blob directory), uploaded to the CDN fake (`support::cdn`) whose upload log is
//! fsynced before each answer, one log per child; the upload record's zone adds two kill points
//! (`wa_inside_media_upload` / `wa_after_media_upload`). Checked besides D21: no frame of the file before its
//! record committed (a frame of it at the kill means the record is there), no upload of it after its record
//! committed (a record at the kill means the next start uploads nothing: it sends the recorded file), and in
//! the end the record names an upload the CDN answered (its `direct_path` and the sha256 of the bytes).
//!
//! The whole loop is in the slow set (`xtask/src/slow.rs`); the regular set keeps a sample of ONE kill per
//! point. Every child runs under `unshare -rn` over the scripted server; nothing here touches a network.
//! The children's output and log lines are grepped for the fixture's secrets (D10).

use std::collections::BTreeMap;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};

use kourier_backend_api::TransmitOutcome;
use kourier_model::ErrorKind;
use kourier_whatsapp::failpoints as fp;
use kourier_whatsapp::fault;

use crate::media_wire::{chat, core_attachment, core_file, file_request, media_server_of};
use crate::receive::{account_dir, count, raw};
use crate::send_wire::{Rig, Script, send_secrets, server_of, text_request};
use crate::support::capture::Capture;
use crate::support::cdn::{self, Cdn};
use crate::support::peer::Peer;
use crate::support::wire::Ack;
use crate::support::{self};
use crate::wire_kill::copy_dir;

const ROLE: &str = "WA_SK_ROLE";
const DIR: &str = "WA_SK_DIR";

/// The core's two kill points around a backend's answer (`kourier-core/src/outbox.rs`
/// `FAILPOINT_AFTER_ATTEMPT`, `FAILPOINT_BEFORE_OUTCOME_COMMIT`; this crate may not name the core).
const CORE_AFTER_ATTEMPT: &str = "outbox_after_attempt_commit";
const CORE_BEFORE_OUTCOME: &str = "outbox_before_outcome_commit";

/// The core's locals, sent in this order: a text, a text the server refuses (463), a text.
const FIRST_TEXT: &str = "3EB0C0DE5C1A4B27";
const REFUSED: &str = "3EB0C0DE5C2B5C38";
const LAST_TEXT: &str = "3EB0C0DE5C3C6D49";
/// W5b: a message with one file (a document), the last one sent.
const FILE_MESSAGE: &str = "3EB0C0DE5C4D7E5A";
const LOCALS: [&str; 4] = [FIRST_TEXT, REFUSED, LAST_TEXT, FILE_MESSAGE];
/// The document's sealed copy in the account's blob directory (made in the template) and its name there.
const FILE_BLOB: &str = "kill-document-61728394";
/// The most hits of one point the three messages make. A loop that goes past it is a bug.
const MAX_HITS: u32 = 8;

/// The outcome the core noted, as its word in `core.log` and in the ledger.
fn expected(local: &str) -> &'static str {
    if local == REFUSED { "refused" } else { "acked" }
}

// --- what the files say ---

/// The core's notes: `attempt <local>` and `outcome <local> <acked|refused>`; the outcomes by local.
fn core_outcomes(dir: &Path) -> BTreeMap<String, String> {
    support::kill_log::lines(&dir.join("core.log"))
        .iter()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some("outcome"))
                .then(|| Some((words.next()?.to_owned(), words.next()?.to_owned())))
                .flatten()
        })
        .collect()
}

/// One fsynced line of `core.log`, never glued to a line a killed child left unfinished (`kill_log::note`).
fn note(dir: &Path, line: &str) {
    support::kill_log::note(&dir.join("core.log"), line);
}

/// The `<message>` frames of one child's connection, counted by stanza id (the Wire's log line:
/// `message type=.. class=.. xmlns=.. id=<id> items=..`).
fn frames(dir: &Path, role: &str) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for line in support::kill_log::lines(&dir.join(format!("wire-{role}.log"))) {
        let mut words = line.split_whitespace();
        if words.next() != Some("message") {
            continue;
        }
        let id = words
            .find_map(|w| w.strip_prefix("id="))
            .unwrap_or("-")
            .to_owned();
        *counts.entry(id).or_insert(0) += 1;
    }
    counts
}

/// The ledger's outcomes by local.
fn ledger(dir: &Path) -> BTreeMap<String, String> {
    let conn = raw(dir);
    let mut stmt = conn
        .prepare("SELECT local, outcome FROM send_outcomes")
        .expect("prepare");
    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect()
}

/// The file message's upload record at this moment: its pointer (JSON), if any.
fn record(dir: &Path) -> Option<String> {
    let conn = raw(dir);
    if count(
        &conn,
        &format!("SELECT count(*) FROM media_uploads WHERE local = '{FILE_MESSAGE}'"),
    ) == 0
    {
        return None;
    }
    let pointer: Vec<u8> = conn
        .query_row(
            "SELECT pointer FROM media_uploads WHERE local = ?1",
            [FILE_MESSAGE],
            |r| r.get(0),
        )
        .expect("the record reads");
    Some(String::from_utf8(pointer).expect("a JSON pointer"))
}

/// The uploads the CDN fake answered to one child (its fsynced log).
fn uploads(dir: &Path, role: &str) -> Vec<cdn::Logged> {
    cdn::read_log(&dir.join(format!("cdn-{role}.log")))
}

/// At the kill (W5b): a frame of the file only after its record.
fn assert_file_at_kill(dir: &Path, what: &str) {
    if frames(dir, "first").contains_key(FILE_MESSAGE) {
        assert!(
            record(dir).is_some(),
            "{what}: a frame of the file was written, but its upload record was not committed"
        );
    }
}

/// After the recovery (W5b): no upload after the record, and the record names an upload the CDN answered.
fn assert_file_final(dir: &Path, what: &str, recorded_at_kill: bool) {
    if recorded_at_kill {
        assert_eq!(
            uploads(dir, "recover").len(),
            0,
            "{what}: the file was recorded at the kill, yet the next start uploaded it again"
        );
    }
    let pointer = record(dir).unwrap_or_else(|| panic!("{what}: the file has no upload record"));
    let all: Vec<cdn::Logged> = uploads(dir, "first")
        .into_iter()
        .chain(uploads(dir, "recover"))
        .collect();
    assert!(
        all.iter().any(|u| u.path == "/mms/document"
            && pointer.contains(&format!("\"{}\"", u.direct_path))
            && pointer.contains(&u.sha256)),
        "{what}: the record names no upload the CDN answered"
    );
}

/// The frames of one connection: each carries a local of the core as its id, at most once.
fn assert_one_connection(frames: &BTreeMap<String, usize>, what: &str, role: &str) {
    for (id, n) in frames {
        assert!(
            LOCALS.contains(&id.as_str()),
            "{what}: the {role} connection wrote a frame whose id is none of the core's locals"
        );
        assert!(
            *n <= 1,
            "{what}: the {role} connection wrote {n} frames of one local (at most one per connection)"
        );
    }
}

/// At the kill: what the core noted was durable before it was answered, and every ledger row has its frame.
fn assert_at_kill(dir: &Path, what: &str) {
    let first = frames(dir, "first");
    assert_one_connection(&first, what, "first");
    let kept = ledger(dir);
    for (local, outcome) in core_outcomes(dir) {
        assert_eq!(
            kept.get(&local),
            Some(&outcome),
            "{what}: the core was answered {outcome} for {local}, but the ledger does not hold it"
        );
    }
    for local in kept.keys() {
        assert_eq!(
            first.get(local),
            Some(&1),
            "{what}: the ledger holds {local}, but no frame of it was written"
        );
    }
}

/// After the recovery (or a run without a kill): D21's invariant over both connections and the end state.
fn assert_final(dir: &Path, what: &str, point: &str, at_kill: &BTreeMap<String, String>) {
    let first = frames(dir, "first");
    let again = frames(dir, "recover");
    assert_one_connection(&again, what, "recover");
    for local in at_kill.keys() {
        assert_eq!(
            again.get(local),
            None,
            "{what}: {local} was {} in the ledger at the kill, yet the next start wrote a frame of it",
            at_kill[local]
        );
    }
    for local in LOCALS {
        let total = first.get(local).copied().unwrap_or(0) + again.get(local).copied().unwrap_or(0);
        assert!(
            (1..=2).contains(&total),
            "{what}: {local} has {total} frames in all"
        );
        assert!(
            total == 1 || point == fp::INSIDE_SEND_OUTCOME,
            "{what}: {local} was written twice, but the kill did not fall between its frame and the ledger"
        );
    }
    let expect: BTreeMap<String, String> = LOCALS
        .iter()
        .map(|l| ((*l).to_owned(), expected(l).to_owned()))
        .collect();
    assert_eq!(
        core_outcomes(dir),
        expect,
        "{what}: the core did not end with every outcome"
    );
    assert_eq!(
        ledger(dir),
        expect,
        "{what}: the ledger did not end with every outcome"
    );
}

// --- the child ---

/// One start of the account: the core sends every message whose outcome it has not noted, one after the
/// other, behind its own two kill points.
async fn play_the_core(dir: &Path, role: &str) {
    let script = Script::acked();
    let server = media_server_of(&script);
    server.ack_id(REFUSED, Ack::Error(463));
    server.log_to(&dir.join(format!("wire-{role}.log")));
    let fake = Cdn::default();
    fake.log_to(&dir.join(format!("cdn-{role}.log")));
    let rig = Rig::start_with_cdn(dir, server, &fake).await;
    let noted = core_outcomes(dir);
    for local in LOCALS.iter().filter(|l| !noted.contains_key(**l)) {
        note(dir, &format!("attempt {local}"));
        kourier_db::failpoint::hit(CORE_AFTER_ATTEMPT);
        let req = if *local == FILE_MESSAGE {
            file_request(chat(), local, the_file(), None)
        } else {
            text_request(local)
        };
        let outcome = rig.transmit(&req).await;
        kourier_db::failpoint::hit(CORE_BEFORE_OUTCOME);
        let word = match &outcome {
            TransmitOutcome::Sent { .. } => "acked",
            TransmitOutcome::Failed(e) if e.kind == ErrorKind::ReachOutRefused => "refused",
            TransmitOutcome::Failed(e) => {
                panic!("a send was answered neither Sent nor refused: {:?}", e.kind)
            }
            _ => panic!("a send was answered neither Sent nor refused"),
        };
        note(dir, &format!("outcome {local} {word}"));
    }
    assert_eq!(rig.server.dials(), 1, "a child's run is not ONE connection");
    rig.finish().await;
}

/// The document's bytes (a pattern).
fn file_bytes() -> Vec<u8> {
    (0..150_000u32)
        .map(|i| (i as u8).wrapping_mul(13).wrapping_add(0x29))
        .collect()
}

/// The core's attachment of the file message: the sealed copy the template made (the same key and path).
fn the_file() -> kourier_model::Attachment {
    core_attachment(
        FILE_BLOB,
        file_bytes().len() as u64,
        kourier_model::AttachmentKind::File,
        "application/pdf",
    )
}

/// The template: a resumed account whose chat with the peer was started by the peer (D7), its session
/// with the peer warm, copied to the directory the parent names.
async fn make_template(to: &Path) {
    let (dir, device) = account_dir().await;
    let rig = Rig::start(dir.path(), server_of(&Script::acked())).await;
    let mut peer = Peer::new(&device, None).await;
    rig.peer_writes_first(&mut peer).await;
    rig.finish().await;
    core_file(
        dir.path(),
        FILE_BLOB,
        &file_bytes(),
        kourier_model::AttachmentKind::File,
        "application/pdf",
    );
    copy_dir(dir.path(), to);
}

/// The child (a no-op as an ordinary test): one role of the loop; every log line is recorded, and none
/// may hold a secret.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_kill_child() {
    if !support::offline::in_child() {
        return;
    }
    let log = Capture::default();
    log.install();
    let dir = PathBuf::from(std::env::var(DIR).expect("the parent names the directory"));
    let role = std::env::var(ROLE).expect("the parent names the role");
    match role.as_str() {
        "template" => make_template(&dir).await,
        "first" | "recover" => play_the_core(&dir, &role).await,
        other => panic!("unknown role {other}"),
    }
    assert_eq!(
        log.leaks(&send_secrets()),
        Vec::<String>::new(),
        "a log line holds a secret of the fixture (the targets are named)"
    );
}

// --- the parent ---

/// Runs one child; `point` arms the failpoint. Returns how it ended and what it said, which must hold no
/// secret of the fixture.
fn run_child(dir: &Path, role: &str, point: Option<&str>) -> (std::process::ExitStatus, String) {
    let mut command = support::offline::child_command(module_path!(), "send_kill_child");
    command
        .env(ROLE, role)
        .env(DIR, dir)
        .env_remove("KOURIER_FAILPOINT")
        .env_remove(fault::ENV);
    if let Some(point) = point {
        command.env("KOURIER_FAILPOINT", point);
    }
    let out = command.output().expect("spawn the child");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    for secret in send_secrets() {
        assert!(
            !said.contains(secret),
            "a child printed one of the fixture's secrets"
        );
    }
    (out.status, said)
}

/// The template account, made once per test by a child.
fn template() -> tempfile::TempDir {
    let template = tempfile::tempdir().expect("temp dir");
    let (status, said) = run_child(template.path(), "template", None);
    assert!(
        status.success() && said.contains("test result: ok. 1 passed;"),
        "the template child failed: {said}"
    );
    template
}

/// Kills the account at `point:1`, `point:2`, ... (at most `most` kills) until the child ends normally;
/// each kill is followed by the next start. Returns how many kills there were.
fn kill_at_hits(template: &Path, point: &str, most: u32) -> u32 {
    let mut kills = 0;
    for n in 1..=MAX_HITS + 1 {
        assert!(n <= MAX_HITS, "{point} is hit more than {MAX_HITS} times");
        let dir = tempfile::tempdir().expect("temp dir");
        copy_dir(template, dir.path());
        let what = format!("{point}:{n}");
        let (status, said) = run_child(dir.path(), "first", Some(&what));
        if status.success() {
            assert_final(dir.path(), &what, point, &BTreeMap::new());
            assert_file_final(dir.path(), &what, false);
            break;
        }
        assert_eq!(
            status.signal(),
            Some(6),
            "{what} must abort: {status:?} {said}"
        );
        assert!(said.contains("hit: aborting"), "{what}: {said}");
        assert_at_kill(dir.path(), &what);
        assert_file_at_kill(dir.path(), &what);
        let at_kill = ledger(dir.path());
        let recorded = record(dir.path()).is_some();
        let (status, said) = run_child(dir.path(), "recover", None);
        assert!(status.success(), "{what}: the next start failed: {said}");
        assert_final(dir.path(), &what, point, &at_kill);
        assert_file_final(dir.path(), &what, recorded);
        kills += 1;
        if kills == most {
            break;
        }
    }
    assert!(
        kills >= 1,
        "{point} was never hit: the kill test saw nothing"
    );
    kills
}

const POINTS: [&str; 6] = [
    fp::INSIDE_SEND_OUTCOME,
    fp::AFTER_SEND_OUTCOME,
    fp::INSIDE_MEDIA_UPLOAD,
    fp::AFTER_MEDIA_UPLOAD,
    CORE_AFTER_ATTEMPT,
    CORE_BEFORE_OUTCOME,
];

/// In the slow set (`xtask/src/slow.rs`): every hit of every point.
#[test]
fn every_send_once_after_a_kill_at_every_point() {
    if !support::offline::unshare_or_skip("this test never runs an actor on the real network") {
        return;
    }
    let template = template();
    for point in POINTS {
        let kills = kill_at_hits(template.path(), point, u32::MAX);
        eprintln!("{point}: {kills} kills");
    }
}

/// The regular set's sample of the loop above: the first hit of each point.
#[test]
fn every_send_once_after_one_kill_at_each_point() {
    if !support::offline::unshare_or_skip("this test never runs an actor on the real network") {
        return;
    }
    let template = template();
    for point in POINTS {
        kill_at_hits(template.path(), point, 1);
    }
}

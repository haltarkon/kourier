//! What the user types in his own terminal, and the list of test IDs' whole life (T4; `briefs/tr.md`
//! §2.1, §2.4, §2.10 S1b, the critic's M1 and M4; wa_run.rs:2168-2485 `Tty`, `type_tail`,
//! `type_answer`, wa_run.rs:5051-5103 the terminal rule).
//!
//! * `threema-run id` (HIS terminal, the built binary, never through `kcap` or an agent): makes itself
//!   non-dumpable, refuses unless it runs in the foreground of a terminal, while the question of a
//!   failed link stands in the keeper root, and unless the supervisor of a `--for-link` start waits
//!   for the ID; then asks for the TEST ID twice with the terminal's echo off, takes exactly eight of
//!   `[A-Z0-9]` (letters upper-cased, spaces dropped; a Gateway ID's `*` and any other key are
//!   refused loudly), hashes `kourier-threema-allow-v1:` + the eight characters through
//!   [`super::launch::sha256_bytes`] (what `kourier_threema::AllowList::digest` computes), wipes every
//!   buffer that held the ID, and sends ONLY the digest to the supervisor over its control socket
//!   (`{"op": "allow", "digest": ...}`). It prints `taken (8 characters)`, never a character.
//! * The supervisor's side ([`write_the_list`], [`remove_the_list`]): the list is written only on
//!   that operation, only in a `--for-link` start that waits, under the KEEPER LOCK
//!   (`<keeper root>/.threema-run.lock`, held from the write until the file is gone), 0600, in the app's
//!   form (`sha256:` and 64 lower-case hex digits, one line), and it goes (unlink + an fsync of the
//!   directory) at the app's FIRST dev-control answer: the app reads the list once, in
//!   `kourier-app` `start_networks` inside `App::start`, before the binary hands the context to the
//!   window and the window starts the dev control (`kourier/src/main.rs` `App::start` then
//!   `set_context`, `kourier-qt/src/app.rs` `initialize` -> `dev::start`), so any answer on the dev
//!   channel comes after the read. At every other end (the app's exit, a halt, `stop`, the time
//!   limit, the supervisor's end) it goes too; a leftover is removed by the next `start` or `wipe`.
//! * `threema-run answer` (HIS terminal): ONE key, y or n, read from `/dev/tty` and nowhere else.
//!   It answers the question of a failed link (stop rule 1b) first, when one stands in the keeper
//!   root, else the question a step put ([`QUESTIONS`]). It works on the keeper-root file ALONE when
//!   no supervisor answers (a start that died before its supervisor came up leaves the question
//!   with nobody to serve it): it then takes the profile's lock, clears `link_at` in its state and
//!   removes the file.
//! * The keeper root keeps `linking` DURABLY (critic M4): the supervisor writes the question file
//!   with the kind `linking` the moment the app's login reaches `linking` (fsynced, before the event
//!   is passed on), removes it at `done` or at a refusal of the ID, and turns it into the question at
//!   any other end; a supervisor killed at `linking` leaves it, and every link start, `wipe` and `id`
//!   of every profile of the keeper root refuses while it stands, until he answers.
//!
//! The supervisor takes `allow` and `answer` only from a process in the foreground of a terminal
//! ([`peer_is_on_a_terminal`]); the last guard is procedural: only he runs `id` and `answer`.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::launch::{QUESTION_KIND, sha256_bytes, socket_for_client};
use super::profile::{
    ProfileDir, State, open_lock_file, private_dir, remove_synced, try_lock_for, write_private,
};
use super::supervise::{call_at, make_non_dumpable};
use super::{Args, var};
use crate::target_dir;

// ---- The words and the shapes ---------------------------------------------------------------------

/// The bytes in front of the ID in every digest (`kourier-threema` `src/allow.rs` `DOMAIN`; a test
/// hashes that file's own test vector through this path).
const DOMAIN: &[u8] = b"kourier-threema-allow-v1:";
/// A Threema ID has eight characters.
pub(super) const ID_LEN: usize = 8;
/// The most characters an entry keeps before it is refused (a longer one is no ID).
const ENTRY_MAX: usize = 16;
/// The prefix of a line of the list (the app's `LINE_PREFIX`).
const LINE_PREFIX: &str = "sha256:";
/// The kind of the keeper-root file while a login stands at `linking` (its end not yet seen). The
/// question's own kind is [`QUESTION_KIND`].
pub(super) const LINKING_KIND: &str = "linking";

/// The questions a step may put to the user and the words `answer` shows him; the question of a
/// failed link comes from the keeper-root file, never from a step. T6 and T7 add their rows here.
pub(super) const QUESTIONS: &[(&str, &str)] = &[
    (
        QUESTION_KIND,
        "A login stood at linking and its end was not seen: a device MAY be registered. On the TEST phone open Threema's linked devices (the personal phone never opens that list): was there a Kourier entry? Remove it there if so",
    ),
    (
        "test-phone-in-hand",
        "Is the phone in your hand the TEST phone, the one whose ID you typed (never your personal phone)?",
    ),
    // T6: the three the link step ends with, the emoji check, and the mirror's read.
    (
        "emoji-equal",
        "Are the three emoji the same on the Kourier window and on the TEST phone? (No: cancel on the TEST phone now)",
    ),
    (
        "list-shows-kourier",
        "On the TEST phone, does Threema's linked devices list show an entry named Kourier with this PC's host name in brackets?",
    ),
    (
        "menu-path-as-runbook",
        "Was the menu path you used on the TEST phone to add a device the one the runbook spells?",
    ),
    (
        "phone-warning",
        "Does either phone show a warning about an unofficial client, a security warning about the device, or a request to verify the account? (A line about forward security in a chat is none of these)",
    ),
    (
        "pp-read-it",
        "Did the PERSONAL phone open the 1:1 chat and show the text from the TEST phone, with read receipts on there?",
    ),
    // The media and send steps (TH8). Each is a y or n about a phone or the window.
    (
        "files-open",
        "In Kourier, did the picture show and open, did the file open, and did the voice message play?",
    ),
    (
        "avatar-shows",
        "Does Kourier show the PERSONAL phone's profile picture on that chat? (Answer no too if that phone has none or does not share it)",
    ),
    (
        "pp-got-send",
        "Did the PERSONAL phone show that text exactly once, from the test ID, in the 1:1 chat?",
    ),
    (
        "tp-mirrors-send",
        "Does the TEST phone show that text once, as its own message, in the 1:1 chat?",
    ),
    (
        "pp-got-group-send",
        "Did the PERSONAL phone show that text exactly once, from the test ID, in the group?",
    ),
    (
        "tp-mirrors-group-send",
        "Does the TEST phone show that text once, as its own message, in the group?",
    ),
    (
        "pp-got-file",
        "Did the PERSONAL phone show that picture with its caption exactly once, and does it open?",
    ),
    (
        "tp-mirrors-file",
        "Does the TEST phone show that picture once, as its own message, in the 1:1 chat?",
    ),
    // T7: `kinds`, `phone-sleep`, `flight-mode` and the two ends. Each is a y or n about a phone.
    (
        "kinds-sent",
        "From the PERSONAL phone did you send all six (a quote, a mention in the group, bold text, an edit, a reaction and a delete) and wait about 20 seconds after the last? (n: the step records what came)",
    ),
    (
        "phone-locked",
        "Is the TEST phone locked now (screen off, Threema not in front), and the PERSONAL phone face down?",
    ),
    (
        "tp-shows-once",
        "The TEST phone is unlocked with Threema open: does it show the text from the PERSONAL phone exactly once in that chat? (n: missing or twice)",
    ),
    (
        "push-own",
        "On the TEST phone, Threema's notification setting: does Threema use its OWN push service (y), rather than the system's push (n)?",
    ),
    (
        "flight-on",
        "Is the TEST phone in flight mode now (every radio off, Wi-Fi too), Threema still open on it?",
    ),
    (
        "pp-delivered",
        "Does the PERSONAL phone show its 1:1 text as delivered?",
    ),
    (
        "flight-off-both",
        "Flight mode is OFF and Threema is open on the TEST phone: are both texts there (the 1:1 one and the group one), once each? (n: missing or twice)",
    ),
    (
        "tp-shows-later",
        "The TEST phone has followed again: does it show the later text from the PERSONAL phone exactly once? (n: missing or twice)",
    ),
    (
        "fs-line",
        "Does the PERSONAL phone's chat with the test ID show a line about forward security (for example that it is off or changed)? It is expected after the link: it is recorded and never stops the run",
    ),
    (
        "end-go",
        "This ENDS the link: the TEST phone's Kourier entry goes and the account is removed. Do you want that now?",
    ),
    (
        "tp-list-clean",
        "On the TEST phone, does Threema's linked devices list show NO Kourier entry any more?",
    ),
];

/// The first prompt of `id` (critic M1: it names the TEST ID). Nothing in it is a character of an ID.
pub(super) const PROMPT_FIRST: &str = "Type the TEST ID, as the TEST phone shows it under My ID. Never your personal ID. 8 characters, nothing is shown while you type, then Enter: ";
pub(super) const PROMPT_AGAIN: &str = "Once more, the TEST ID: ";
const CANCELLED: &str = "cancelled: nothing was sent";
/// Every refused entry says that the command takes no value (critic M1 (4)).
const NO_VALUE: &str = " (`threema-run id` takes no value on its command line: the ID is typed only at its prompt; one typed on a command line stays in the shell's history, delete that line there)";
const ONLY_ID_KEYS: &str = "only letters and digits: another key was typed, so nothing was sent. Run `threema-run id` again and type the TEST ID once more";
const GATEWAY: &str = "an ID that starts with * is a Gateway ID, never a test ID: nothing was sent";
const BAD_ID: &str = "a Threema ID is exactly 8 letters and digits: nothing was sent";
const DIFFER: &str = "the two entries differ: nothing was sent";
const QUESTION_STANDS: &str = "the question of a failed link is open in the keeper root (a device MAY be registered on the TEST phone): answer it first with `threema-run answer` in your own terminal. Nothing was asked.";
const NO_QUESTION: &str = "no question is open right now: run the step that asks and answer while it waits. Nothing was asked.";
const WRONG_QUESTION: &str = "the question that is open is not the one named with --question: nothing was taken and the question stays open. Use the name in the `ASK-USER [<name>]` line of the step that is waiting.";
const NO_QUESTION_NAMED: &str = "`answer --relayed y|n` needs `--question <name>` (the name in the `ASK-USER [<name>]` line): nothing was taken.";
const ONLY_Y_OR_N: &str =
    "only y or n: another key was typed, so no answer was given. Run the command again";
/// What the supervisor says to an `allow` or an `answer` from a process that is not in the
/// foreground of a terminal (wa_run.rs:5051 `NOT_FROM_A_TERMINAL`).
pub(super) const NOT_FROM_A_TERMINAL: &str = "his y or n and the digest of the test ID are taken only from `threema-run answer` or `threema-run id` running in the foreground of a terminal of his own: nothing was taken, a question stays open";

/// What a command that needs his terminal says when it has none.
fn no_terminal(command: &str) -> String {
    format!(
        "threema-run {command} needs a terminal of its own: run it in a tab of your own terminal as `target/debug/xtask threema-run {command} --profile <name>`, not through an agent, a pipe or a script. Nothing was asked."
    )
}

/// The same for a process in a background group of the terminal (a read would stop it).
fn background_group(command: &str) -> String {
    format!(
        "threema-run {command} runs in a background process group of the terminal, where a read would stop it (kcap and `timeout` put their command there): run the built binary directly in your terminal, `target/debug/xtask threema-run {command} --profile <name>`, not through kcap or cargo. Nothing was asked."
    )
}

/// Whether `id` is a Threema ID the tool takes: exactly eight of `[A-Z0-9]` (the entry is
/// upper-cased before; a Gateway ID's `*` is not among them).
pub(super) fn valid_id(id: &[u8]) -> bool {
    id.len() == ID_LEN
        && id
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// Whether `text` is a digest as the list holds it: 64 lower-case hex digits, nothing else.
pub(super) fn is_digest_hex(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The list's text for one digest: one line `sha256:<64 lower-case hex digits>` (the app's form,
/// `kourier-app` `threema.rs` `parse_allow_file`).
pub(super) fn list_text(digest: &str) -> String {
    format!("{LINE_PREFIX}{digest}\n")
}

/// Overwrites `buf` with zeros in a way the compiler keeps (no crate for it: `Cargo.lock` stays).
pub(super) fn wipe(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        // SAFETY: `b` is a valid, aligned, exclusive reference to one byte.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

/// A string that held the ID or its digest, wiped and dropped.
fn wipe_string(text: String) {
    let mut bytes = text.into_bytes();
    wipe(&mut bytes);
}

/// The digest of an ID as the list holds it: SHA-256 of `kourier-threema-allow-v1:` + the
/// characters, by `sha256sum` on stdin (`launch::sha256_bytes`; nothing on a command line). The
/// copy of the ID made here is wiped.
pub(super) fn digest_of_id(id: &[u8]) -> Result<String> {
    if !valid_id(id) {
        bail!("{BAD_ID}");
    }
    let mut input = [0u8; DOMAIN.len() + ID_LEN];
    input[..DOMAIN.len()].copy_from_slice(DOMAIN);
    input[DOMAIN.len()..].copy_from_slice(id);
    let digest = sha256_bytes(&input);
    wipe(&mut input);
    digest
}

/// What he typed at one prompt: kept in a fixed buffer that is wiped when it goes.
struct Entry {
    bytes: [u8; ENTRY_MAX],
    len: usize,
}

impl Entry {
    fn new() -> Self {
        Self {
            bytes: [0; ENTRY_MAX],
            len: 0,
        }
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        wipe(&mut self.bytes);
    }
}

// ---- The terminal (wa_run.rs:2256 `Tty`) -----------------------------------------------------------

/// The user's terminal, opened as `/dev/tty`. While it is `quiet` nothing he types is shown and no
/// key raises a signal; dropping it puts the terminal back the way it was, whatever way the prompt
/// ends.
struct Tty {
    file: std::fs::File,
    saved: Option<libc::termios>,
    /// The command that opened it (`id` or `answer`): the words of a refusal name it.
    command: &'static str,
}

impl Tty {
    /// The process's controlling terminal, when this process may read it: a process in a background
    /// group of the terminal would be stopped by the read.
    fn open(command: &'static str) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map_err(|_| anyhow!(no_terminal(command)))?;
        // SAFETY: both calls take no pointer.
        let (foreground, mine) = unsafe { (libc::tcgetpgrp(file.as_raw_fd()), libc::getpgrp()) };
        if foreground != mine {
            bail!(background_group(command));
        }
        Ok(Self {
            file,
            saved: None,
            command,
        })
    }

    /// Echo, line editing by the terminal and signals off: bytes arrive one by one, shown nowhere.
    fn quiet(&mut self) -> Result<()> {
        let fd = self.file.as_raw_fd();
        // SAFETY: an all-zero `termios` is a valid buffer for `tcgetattr` to fill.
        let mut attrs: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is open and `attrs` is a valid `termios`.
        if unsafe { libc::tcgetattr(fd, &mut attrs) } != 0 {
            bail!(no_terminal(self.command));
        }
        let saved = attrs;
        attrs.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG);
        attrs.c_cc[libc::VMIN] = 1;
        attrs.c_cc[libc::VTIME] = 0;
        // SAFETY: `fd` is open and `attrs` is a valid `termios`.
        if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &attrs) } != 0 {
            bail!("cannot switch the terminal's echo off: nothing was asked");
        }
        self.saved = Some(saved);
        Ok(())
    }

    /// Puts the terminal back and throws away what was typed ahead (the shell would show it).
    fn restore(&mut self) {
        if let Some(saved) = self.saved.take() {
            // SAFETY: `fd` is open and `saved` is the `termios` it had.
            unsafe { libc::tcsetattr(self.file.as_raw_fd(), libc::TCSAFLUSH, &saved) };
        }
    }

    /// One byte from the terminal; the end of it or an error cancels.
    fn key(&mut self) -> Result<u8> {
        let mut byte = [0u8; 1];
        loop {
            match self.file.read(&mut byte) {
                Ok(0) => bail!(CANCELLED),
                Ok(_) => return Ok(byte[0]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => bail!(CANCELLED),
            }
        }
    }

    /// A refusal after a key: the newline the terminal did not echo, then the words.
    fn refuse(&mut self, words: &str) -> anyhow::Error {
        let _ = self.file.write_all(b"\r\n");
        anyhow!("{words}{NO_VALUE}")
    }

    /// One entry of the ID: the prompt, then keys until Enter. Letters (upper-cased) and digits are
    /// kept and shown nowhere; spaces are dropped; Backspace takes one back; ^C, ^D on an empty entry
    /// and the end of the terminal cancel; a `*` (a Gateway ID) and any other key refuse LOUDLY (a
    /// key dropped in silence would leave a shorter entry). The key is never named.
    fn ask_id(&mut self, prompt: &str) -> Result<Entry> {
        self.file.write_all(prompt.as_bytes())?;
        let mut entry = Entry::new();
        loop {
            let key = self.key()?;
            match key {
                b'\n' | b'\r' => break,
                0x03 => {
                    let _ = self.file.write_all(b"\r\n");
                    bail!(CANCELLED);
                }
                0x04 if entry.len == 0 => {
                    let _ = self.file.write_all(b"\r\n");
                    bail!(CANCELLED);
                }
                0x7f | 0x08 => {
                    if entry.len > 0 {
                        entry.len -= 1;
                        entry.bytes[entry.len] = 0;
                    }
                }
                b' ' => {}
                b'*' => return Err(self.refuse(GATEWAY)),
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => {
                    if entry.len == ENTRY_MAX {
                        return Err(self.refuse(BAD_ID));
                    }
                    entry.bytes[entry.len] = key.to_ascii_uppercase();
                    entry.len += 1;
                }
                _ => return Err(self.refuse(ONLY_ID_KEYS)),
            }
        }
        // The newline the terminal did not echo.
        let _ = self.file.write_all(b"\r\n");
        if !valid_id(entry.as_slice()) {
            bail!("{BAD_ID}{NO_VALUE}");
        }
        Ok(entry)
    }

    /// One question and ONE key: `y` or `n` in either case (nothing else answers; there is no
    /// default). The key is shown back (it is no secret); ^C, ^D and the end of the terminal cancel.
    fn ask_yes_no(&mut self, prompt: &str) -> Result<bool> {
        self.file.write_all(prompt.as_bytes())?;
        let key = self.key()?;
        let yes = match key {
            b'y' | b'Y' => true,
            b'n' | b'N' => false,
            0x03 | 0x04 => {
                let _ = self.file.write_all(b"\r\n");
                bail!(CANCELLED);
            }
            _ => {
                let _ = self.file.write_all(b"\r\n");
                bail!(ONLY_Y_OR_N);
            }
        };
        let _ = self.file.write_all(if yes { b"y\r\n" } else { b"n\r\n" });
        Ok(yes)
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        self.restore();
    }
}

// ---- The terminal rule of the supervisor (wa_run.rs:5054-5100) ---------------------------------------

/// The pid of the process at the other end of a Unix stream (`SO_PEERCRED`: the process that
/// connected).
pub(super) fn peer_pid(stream: &UnixStream) -> Option<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>()).ok()?;
    // SAFETY: `getsockopt` writes at most `len` bytes into `cred`, which has room for a `ucred`.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut len,
        )
    };
    if rc == 0 {
        u32::try_from(cred.pid).ok().filter(|pid| *pid > 0)
    } else {
        None
    }
}

/// Whether a `/proc/<pid>/stat` line says the process has a controlling terminal and is the
/// foreground process group of it: `tty_nr` (field 7) is not 0 and `tpgid` (field 8) is its own
/// `pgrp` (field 5). The command name (field 2) may hold spaces and brackets, so the fields are
/// counted after the LAST `)`.
pub(super) fn foreground_of_a_terminal(stat: &str) -> bool {
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return false;
    };
    // After the name: state, ppid, pgrp, session, tty_nr, tpgid.
    let fields: Vec<i64> = rest
        .split_whitespace()
        .skip(1)
        .take(5)
        .filter_map(|field| field.parse().ok())
        .collect();
    matches!(fields[..], [_ppid, pgrp, _session, tty_nr, tpgid] if tty_nr != 0 && tpgid > 0 && tpgid == pgrp)
}

/// Whether the process at the other end of the connection is in the foreground of a terminal. A
/// process that is gone before it is looked at, or whose `stat` cannot be read, is not.
pub(super) fn peer_is_on_a_terminal(stream: &UnixStream) -> bool {
    peer_pid(stream)
        .and_then(|pid| std::fs::read_to_string(format!("/proc/{pid}/stat")).ok())
        .is_some_and(|stat| foreground_of_a_terminal(&stat))
}

/// The operations of the control socket that only a process on a terminal may send.
pub(super) fn needs_a_terminal(op: Option<&str>) -> bool {
    matches!(op, Some("allow" | "answer"))
}

// ---- The list of test IDs: the supervisor's side ----------------------------------------------------

/// Writes the list of test IDs for one digest: the keeper root made 0700, the KEEPER LOCK taken
/// (refused when another run holds it), then the file through a temporary 0600 file, an fsync, a
/// rename and an fsync of the directory, in the app's form. Returns the held lock: the caller keeps
/// it until [`remove_the_list`] has removed the file.
pub(super) fn write_the_list(dir: &ProfileDir, digest: &str) -> Result<std::fs::File> {
    if !is_digest_hex(digest) {
        bail!("a digest is 64 lower-case hex digits");
    }
    private_dir(dir.keeper())?;
    let lock = open_lock_file(&dir.keeper_lock_file())?;
    if !try_lock_for(&lock, Duration::ZERO)? {
        bail!("another run holds the keeper lock");
    }
    // LOW-1: the rename needs a plain file (or nothing) at the final name: a directory, a link or
    // anything else there would fail it AFTER the temporary file held the digest. Refused before
    // anything is written.
    if std::fs::symlink_metadata(dir.allow_file()).is_ok_and(|m| !m.is_file()) {
        bail!("the list's name in the keeper root is not a plain file: nothing was written");
    }
    let mut text = list_text(digest);
    let written = write_private(&dir.allow_file(), &text);
    // Not the ID, but as good as it (36^8 tries): no copy is left behind in this process.
    wipe_string(std::mem::take(&mut text));
    if let Err(e) = written {
        // A write or rename that failed leaves neither the list nor its temporary file (LOW-1).
        let _ = remove_synced(&dir.allow_tmp());
        let _ = remove_synced(&dir.allow_file());
        return Err(e);
    }
    Ok(lock)
}

/// Removes the list of test IDs (unlink + an fsync of the directory), THEN lets the keeper lock go.
/// `held` is the lock the writer kept; without it the lock is taken first, and a list under another
/// run's live lock is that run's and stays. `true` when a list was removed.
pub(super) fn remove_the_list(dir: &ProfileDir, held: Option<std::fs::File>) -> Result<bool> {
    let lock = match held {
        Some(lock) => lock,
        None => {
            if std::fs::symlink_metadata(dir.allow_file()).is_err()
                && std::fs::symlink_metadata(dir.allow_tmp()).is_err()
            {
                return Ok(false);
            }
            let lock = open_lock_file(&dir.keeper_lock_file())?;
            if !try_lock_for(&lock, Duration::ZERO)? {
                return Ok(false);
            }
            lock
        }
    };
    // The temporary file first (LOW-1: it holds the digest until the rename; a kill between the
    // create and the rename leaves it), then the list; the lock goes after both.
    let tmp = remove_synced(&dir.allow_tmp());
    let list = remove_synced(&dir.allow_file());
    drop(lock);
    Ok(tmp? | list?)
}

// ---- The keeper root's question file -----------------------------------------------------------------

/// The word of RP-b's drop outcome the keeper root's question file keeps (`drop`, written by the supervisor at the
/// app's exit): `removed` or `not-reached`; `None` when there is no file or it names none.
pub(super) fn question_drop(dir: &ProfileDir) -> Option<String> {
    std::fs::read_to_string(dir.open_question_file())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| v["drop"].as_str().map(str::to_owned))
}

/// The words before the question of a failed link: what RP-b's drop line said (the question file's
/// `drop`, written by the supervisor at the app's exit), or that no outcome was seen.
pub(super) fn drop_preface(dir: &ProfileDir) -> &'static str {
    let drop = question_drop(dir);
    match drop.as_deref() {
        Some("removed") => {
            "Kourier dropped the device itself: the mediator said Removed. Check anyway. "
        }
        Some("not-reached") => {
            "Kourier tried to drop the device and did NOT reach the mediator: an entry is likely. "
        }
        _ => "Kourier's own drop of the device was not confirmed. ",
    }
}

/// The kind word of the keeper-root question file: `None` when there is none; a file that cannot be
/// read or has no kind is the question (fail closed: it stands until he answers).
pub(super) fn question_kind(dir: &ProfileDir) -> Option<String> {
    let path = dir.open_question_file();
    std::fs::symlink_metadata(&path).ok()?;
    let kind = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| v["kind"].as_str().map(str::to_owned))
        .filter(|k| k == LINKING_KIND || k == QUESTION_KIND);
    Some(kind.unwrap_or_else(|| QUESTION_KIND.to_owned()))
}

/// A login reached `linking`: the keeper root keeps it (critic M4), fsynced, unless a file of the
/// question stands already (it is never overwritten by the lesser word).
pub(super) fn mark_linking(dir: &ProfileDir, now: u64) -> Result<()> {
    if std::fs::symlink_metadata(dir.open_question_file()).is_ok() {
        return Ok(());
    }
    let text = format!("{:#}\n", json!({"kind": LINKING_KIND, "at_ms": now}));
    write_private(&dir.open_question_file(), &text)
}

/// The login's end at `linking` was seen (`done`, or the ID refused before anything was stored): the
/// keeper root's `linking` goes. A question stays.
pub(super) fn linking_ended(dir: &ProfileDir) -> Result<()> {
    if question_kind(dir).as_deref() == Some(LINKING_KIND) {
        remove_synced(&dir.open_question_file())?;
    }
    Ok(())
}

/// His answer to the question of a failed link, as the state keeps it: a word and a time, and the
/// login's `linking` is spent.
pub(super) fn record_link_answer(state: &mut State, yes: bool, now: u64) {
    state.set("link_at", Value::Null);
    state.set(
        "last_link_question",
        json!({"answer": if yes { "yes" } else { "no" }, "at_ms": now}),
    );
}

// ---- `threema-run id` ---------------------------------------------------------------------------------

/// Sends the digest over the socket as one hand-built line, which is wiped after the write (a
/// `serde_json::Value` would keep copies nobody can wipe), and reads the one answer.
fn send_digest(socket: &Path, digest: &str) -> Result<()> {
    send_digest_op(socket, "allow", digest)
}

/// [`send_digest`] with the operation named: `allow` (typed, needs his terminal) or `allow_phone`
/// (read from the TEST phone, a run with a test phone only).
fn send_digest_op(socket: &Path, op: &str, digest: &str) -> Result<()> {
    let mut stream = UnixStream::connect(socket).context(
        "the supervisor's socket is gone: start again with --for-link. Nothing was sent",
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut line = Vec::with_capacity(128);
    line.extend_from_slice(br#"{"op":""#);
    line.extend_from_slice(op.as_bytes());
    line.extend_from_slice(br#"","digest":""#);
    line.extend_from_slice(digest.as_bytes());
    line.extend_from_slice(b"\"}\n");
    let written = stream.write_all(&line);
    wipe(&mut line);
    written.context("the supervisor did not take the line. Nothing was taken")?;
    let mut answer = String::new();
    BufReader::new(stream)
        .read_line(&mut answer)
        .context("the supervisor did not answer")?;
    let answer: Value = serde_json::from_str(&answer).context("the supervisor's answer")?;
    if answer["ok"] == json!(true) {
        return Ok(());
    }
    bail!(
        "{} ({})",
        answer["error"]["message"].as_str().unwrap_or("refused"),
        answer["error"]["code"].as_str().unwrap_or("failed")
    )
}

/// `id`: the checks, the prompt twice, the digest to the supervisor. Returns what to print
/// (`taken (8 characters)`, never a character). The process makes itself non-dumpable first and asks
/// for nothing before it knows that a supervisor waits for the ID.
pub(super) fn type_id(dir: &ProfileDir, socket: &Path) -> Result<String> {
    make_non_dumpable()?;
    let mut tty = Tty::open("id")?;
    if question_kind(dir).is_some() {
        bail!(QUESTION_STANDS);
    }
    let info = call_at(socket, &json!({"op": "info"}), Duration::from_secs(5)).context(
        "no run is up on this profile: start one with `start --for-link` first. Nothing was asked.",
    )?;
    if info["id"] != json!(super::launch::INFO_ID_WAITING) {
        bail!(
            "this start does not wait for the ID (it was started without --for-link, the ID was taken already, or it gave up waiting): stop, and start again with --for-link. Nothing was asked."
        );
    }
    tty.quiet()?;
    let first = tty.ask_id(PROMPT_FIRST)?;
    let again = tty.ask_id(PROMPT_AGAIN)?;
    // The terminal is the user's again before anything else happens.
    drop(tty);
    if first.as_slice() != again.as_slice() {
        bail!("{DIFFER}{NO_VALUE}");
    }
    drop(again);
    let digest = digest_of_id(first.as_slice());
    drop(first);
    let digest = digest?;
    let sent = send_digest(socket, &digest);
    wipe_string(digest);
    sent?;
    Ok(format!("taken ({ID_LEN} characters)"))
}

/// `id --from-test-phone` (B1): asks the SUPERVISOR to read the TEST ID from the TEST phone it was
/// started with and to keep only its digest. This process reads no phone and sends no digest; the
/// supervisor refuses a request that carries one. The same checks as [`type_id`] come first (no
/// terminal is needed). Returns what to print.
pub(super) fn id_from_test_phone(dir: &ProfileDir, socket: &Path) -> Result<String> {
    if question_kind(dir).is_some() {
        bail!(QUESTION_STANDS);
    }
    let info = call_at(socket, &json!({"op": "info"}), Duration::from_secs(5)).context(
        "no run is up on this profile: start one with `start --for-link --test-phone <serial>` first. Nothing was read.",
    )?;
    if info["test_phone"] != json!(true) {
        bail!(
            "this run has no test phone: the ID is typed in `threema-run id` in the user's terminal. Nothing was read."
        );
    }
    if info["id"] != json!(super::launch::INFO_ID_WAITING) {
        bail!(
            "this start does not wait for the ID (it was started without --for-link, the ID was taken already, or it gave up waiting): stop, and start again with --for-link. Nothing was read."
        );
    }
    call_at(
        socket,
        &json!({"op": "allow_phone"}),
        Duration::from_secs(60),
    )?;
    Ok(format!(
        "taken ({ID_LEN} characters, read from the TEST phone by the supervisor)"
    ))
}

/// `answer --relayed y|n --question <name>`: the lead relays the user's word from chat. Only a run with a test phone
/// takes it (the supervisor refuses otherwise); recorded as `relayed by the lead`. With no supervisor
/// it closes the question of a failed link alone, when the profile has a test phone.
pub(super) fn relayed_answer(
    dir: &ProfileDir,
    socket: Option<&Path>,
    yes: bool,
    question: &str,
) -> Result<String> {
    let info = socket.and_then(|s| call_at(s, &json!({"op": "info"}), Duration::from_secs(5)).ok());
    let kind = question_kind(dir);
    let own_linking = kind.as_deref() == Some(LINKING_KIND)
        && info.as_ref().is_some_and(|i| i["link_at"] == json!(true));
    let name = match (&kind, &info) {
        (Some(_), _) if !own_linking => QUESTION_KIND.to_owned(),
        (_, Some(info)) => info["question"]
            .as_str()
            .filter(|n| question_text(n).is_some())
            .ok_or_else(|| anyhow!(NO_QUESTION))?
            .to_owned(),
        (_, None) => bail!(
            "no run is up on this profile and no question of a failed link is open: there is nothing to answer."
        ),
    };
    // RT1: a relay answers the question it NAMES, never whatever is open. The name is not echoed.
    if name != question {
        bail!("{WRONG_QUESTION}");
    }
    let word = if yes { "yes" } else { "no" };
    match (&info, socket) {
        (Some(_), Some(socket)) => {
            call_at(
                socket,
                &json!({"op": "relay_answer", "name": name, "yes": yes}),
                Duration::from_secs(10),
            )?;
        }
        _ => {
            let has_phone = State::load(dir)?.is_some_and(|s| s.test_phone().is_some());
            if !has_phone {
                bail!(
                    "this profile has no test phone: his answer is typed in `threema-run answer` in his terminal. Nothing was taken."
                );
            }
            let lock = take_for_the_answer(dir)?;
            answer_alone(dir, yes)?;
            drop(lock);
        }
    }
    Ok(format!("taken ({word}, relayed by the lead)"))
}

pub(super) fn id_command(a: &Args) -> Result<i32> {
    let dir = a.place.resolve(&var, &target_dir())?;
    let socket = socket_for_client(&dir, &var)?;
    if a.from_test_phone {
        println!("{}", id_from_test_phone(&dir, &socket)?);
        return Ok(0);
    }
    println!("{}", type_id(&dir, &socket)?);
    Ok(0)
}

// ---- `threema-run answer` -----------------------------------------------------------------------------

/// The words of a question by its name.
fn question_text(name: &str) -> Option<&'static str> {
    QUESTIONS
        .iter()
        .find(|(q, _)| *q == name)
        .map(|(_, text)| *text)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// `answer`: the question that is open, then ONE key, then the answer: to the supervisor when one
/// answers on the profile's socket, else (the question of a failed link only) ALONE on the keeper
/// root's file. This is the ONLY place a y or n is read: from the controlling terminal, never from
/// standard input, an argument or an environment variable. Returns what to print.
pub(super) fn type_answer(dir: &ProfileDir, socket: Option<&Path>) -> Result<String> {
    let mut tty = Tty::open("answer")?;
    let info = socket.and_then(|s| call_at(s, &json!({"op": "info"}), Duration::from_secs(5)).ok());
    let kind = question_kind(dir);
    // A live run whose own login stands at `linking` keeps the keeper root's `linking` as its own
    // mark: that is no question yet.
    let own_linking = kind.as_deref() == Some(LINKING_KIND)
        && info.as_ref().is_some_and(|i| i["link_at"] == json!(true));
    let name = match (&kind, &info) {
        (Some(_), _) if !own_linking => QUESTION_KIND.to_owned(),
        (_, Some(info)) => info["question"]
            .as_str()
            .filter(|n| question_text(n).is_some())
            .ok_or_else(|| anyhow!(NO_QUESTION))?
            .to_owned(),
        (_, None) => bail!(
            "no run is up on this profile and no question of a failed link is open: there is nothing to answer. Nothing was asked."
        ),
    };
    let text = question_text(&name).ok_or_else(|| anyhow!(NO_QUESTION))?;
    // What Kourier's own drop of the device came to, when the run saw it (critic M2): said first,
    // the question stays (it is the backstop).
    let preface = if name == QUESTION_KIND {
        drop_preface(dir)
    } else {
        ""
    };
    // Alone (no supervisor answers): the profile's lock is taken BEFORE he is asked, and kept.
    let alone = if info.is_none() {
        Some(take_for_the_answer(dir)?)
    } else {
        None
    };
    tty.quiet()?;
    let yes = tty.ask_yes_no(&format!("{preface}{text} [y/n]: "))?;
    drop(tty);
    let word = if yes { "yes" } else { "no" };
    match (alone, socket) {
        (Some(lock), _) => {
            answer_alone(dir, yes)?;
            drop(lock);
        }
        (None, Some(socket)) => {
            call_at(
                socket,
                &json!({"op": "answer", "name": name, "yes": yes}),
                Duration::from_secs(10),
            )?;
        }
        (None, None) => unreachable!("an answer with no supervisor is answered alone"),
    }
    if name == QUESTION_KIND {
        Ok(format!(
            "taken ({word}): the question of the failed link is closed"
        ))
    } else {
        Ok(format!("taken ({word})"))
    }
}

/// The profile's lock for an answer given alone: free, or a supervisor lives that does not answer.
/// A profile that does not exist (any more) has nothing to lock and no state.
fn take_for_the_answer(dir: &ProfileDir) -> Result<Option<std::fs::File>> {
    if std::fs::symlink_metadata(dir.tool()).is_err() {
        return Ok(None);
    }
    let lock = open_lock_file(&dir.lock_file())?;
    if !try_lock_for(&lock, Duration::from_millis(200))? {
        bail!(
            "a supervisor holds this profile's lock but does not answer: run `threema-run stop` first, then answer. Nothing was asked."
        );
    }
    Ok(Some(lock))
}

/// The question of a failed link answered with no supervisor: the profile's state first (`link_at`
/// spent, the answer recorded; a start that died before it saved the state left `link_at`), THEN the
/// keeper root's file goes. A failure on the way leaves the question standing (fail closed).
fn answer_alone(dir: &ProfileDir, yes: bool) -> Result<()> {
    if std::fs::symlink_metadata(dir.tool()).is_ok()
        && let Some(mut state) = State::load(dir)?
    {
        record_link_answer(&mut state, yes, now_ms());
        state.save(dir)?;
    }
    remove_synced(&dir.open_question_file())?;
    Ok(())
}

pub(super) fn answer_command(a: &Args) -> Result<i32> {
    let dir = a.place.resolve(&var, &target_dir())?;
    let socket = socket_for_client(&dir, &var).ok();
    if let Some(yes) = a.relayed {
        let question = a
            .question
            .as_deref()
            .ok_or_else(|| anyhow!(NO_QUESTION_NAMED))?;
        println!(
            "{}",
            relayed_answer(&dir, socket.as_deref(), yes, question)?
        );
        return Ok(0);
    }
    println!("{}", type_answer(&dir, socket.as_deref())?);
    Ok(0)
}

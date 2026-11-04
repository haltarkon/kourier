//! The events file of a kill test's child (`history::kill`, `receive_kill`): one line per fact, written and synced,
//! so a killed child's lines survive it. The integration tests include this very file (`tests/support/mod.rs`, by
//! path) for their children's logs: the core's notes of `tests/send_kill.rs` and the logs of the Wire and the Cdn fake
//! (`open_append` at `log_to`, read by `send_kill`, `wire_kill` and `cdn::read_log`); its own tests run in both test
//! binaries. The kill is an `abort()` on ANOTHER thread (a writer's failpoint), so it can
//! land in the middle of a line: `writeln!` makes two writes (the text, then the newline), and the kernel may stop a
//! write that crosses a page at the boundary. A line is a fact only once its newline is written: [`lines`] reads the
//! complete lines only, and [`note`] cuts a torn tail off before it appends. (Strike of 2026-10-27: the kill at
//! `wa_after_history_row:9` left `msg 6 .. L598` without its newline, and the recovery's first line `msg 7 .. L500`
//! continued it: the parent read one message `L598msg | 7 | .. | L500` that no run gave the core.)

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::Path;

/// Appends `line` and its newline in ONE write and syncs it; first cuts off what a killed writer left of a line
/// ([`open_append`]), so the new line never continues a torn one.
pub(crate) fn note(path: &Path, line: &str) {
    let mut file = open_append(path);
    file.write_all(format!("{line}\n").as_bytes())
        .expect("write the line");
    file.sync_all().expect("sync the line");
}

/// `path` opened to append (made when missing), with what a killed writer left of a line cut off first. A writer that
/// keeps the file open (the Wire's and the Cdn fake's logs) writes each line, newline included, in ONE write: its own
/// lines are whole until it is killed itself, and the next process to open the file cuts what that left.
pub(crate) fn open_append(path: &Path) -> File {
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .expect("events file");
    cut_torn_tail(&mut file);
    file
}

/// Everything after the file's last newline goes (a writer died there); a file that ends in one is left alone.
fn cut_torn_tail(file: &mut File) {
    let len = file.metadata().expect("events file size").len();
    if len == 0 {
        return;
    }
    let mut last = [0u8];
    file.seek(SeekFrom::Start(len - 1)).expect("seek");
    file.read_exact(&mut last).expect("read the last byte");
    if last[0] == b'\n' {
        return;
    }
    let mut all = Vec::new();
    file.seek(SeekFrom::Start(0)).expect("seek");
    file.read_to_end(&mut all).expect("read the events file");
    let keep = all.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    file.set_len(u64::try_from(keep).expect("small"))
        .expect("cut the torn line");
}

/// The complete lines of the events file (a missing file has none); a last line without its newline is no fact.
pub(crate) fn lines(path: &Path) -> Vec<String> {
    let bytes = std::fs::read(path).unwrap_or_default();
    let mut lines: Vec<String> = String::from_utf8_lossy(&bytes)
        .split('\n')
        .map(str::to_owned)
        .collect();
    // What follows the last newline: nothing, or the line a killed writer did not finish.
    lines.pop();
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_a_killed_writer_did_not_finish_is_no_fact_and_the_next_line_stands_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("events");
        std::fs::write(&path, "msg\t6\tA\tL598\nmsg\t6\tA\tL5").expect("a torn file");
        assert_eq!(
            lines(&path),
            ["msg\t6\tA\tL598"],
            "the torn line is not read"
        );
        note(&path, "msg\t7\tB\tL500");
        assert_eq!(
            lines(&path),
            ["msg\t6\tA\tL598", "msg\t7\tB\tL500"],
            "the next line does not continue the torn one"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "msg\t6\tA\tL598\nmsg\t7\tB\tL500\n"
        );
    }

    #[test]
    fn a_file_of_one_torn_line_starts_over_and_a_whole_file_is_kept() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("events");
        assert!(lines(&path).is_empty(), "no file, no lines");
        std::fs::write(&path, "capt").expect("a torn file");
        assert!(lines(&path).is_empty());
        note(&path, "captured 0");
        note(&path, "captured 1");
        assert_eq!(lines(&path), ["captured 0", "captured 1"]);
    }
}

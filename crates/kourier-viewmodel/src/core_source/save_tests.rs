//! `CoreSource`: the save calls (F3b, "Save As…" and "Save All…") against the simulated core of
//! [`super::tests`]: the facts query, the name offered, the command, a clash in a batch (the next
//! name, never a replaced file), the answer, and what each failure is called.

use std::path::PathBuf;
use std::sync::Arc;

use kourier_core::facade::{
    ActionSubject, CoreError, Query, QueryData, SaveError, SaveFacts, SaveMode, UiCommand, UiEvent,
};
use kourier_model::{AccountId, AttachmentId};

use super::tests::{Sent, result, source, take_queries, user};
use super::*;
use crate::source::{SaveAnswer, SaveFailure};

fn facts(file_name: Option<&str>, position: u32, sniffed: Option<&str>) -> SaveFacts {
    SaveFacts {
        account: AccountId(1),
        author: Some(user(7)),
        sent_at: 1_760_000_000_000,
        position,
        file_name: file_name.map(str::to_owned),
        mime: Some("image/jpeg".into()),
        sniffed: sniffed.map(str::to_owned),
    }
}

/// The save commands sent so far, taken: `(attachment, folder, name, replaces)`.
fn saves(sent: &Sent) -> Vec<(i64, PathBuf, String, bool)> {
    let mut out = Vec::new();
    sent.borrow_mut().retain(|c| match c {
        UiCommand::SaveAttachment {
            attachment,
            target,
            mode,
        } => {
            out.push((
                attachment.0,
                target.dir.clone(),
                target.name.clone(),
                *mode == SaveMode::Replace,
            ));
            false
        }
        _ => true,
    });
    out
}

fn saved(src: &mut CoreSource, n: i64) -> Changes {
    src.on_event(&UiEvent::AttachmentSaved {
        attachment: AttachmentId(n),
        bytes: 10,
    })
}

fn refused(src: &mut CoreSource, n: i64, error: CoreError) -> Changes {
    src.on_event(&UiEvent::ActionFailed {
        account: None,
        action: "SaveAttachment",
        error: Arc::new(error),
        subject: ActionSubject::Attachment(AttachmentId(n)),
    })
}

/// Answers the facts query of attachment `n` with `facts`.
fn answer(src: &mut CoreSource, sent: &Sent, facts: SaveFacts) -> Changes {
    let queries = take_queries(sent);
    assert_eq!(queries.len(), 1, "one facts query per file: {queries:?}");
    let (token, query) = &queries[0];
    assert!(matches!(query, Query::SaveFacts(_)), "{query:?}");
    assert_eq!(token.model, MODEL_SAVE);
    src.on_event(&result(*token, QueryData::SaveFacts(Some(facts))))
}

#[test]
fn the_dialog_is_offered_the_name_the_facts_make() {
    let (mut src, sent) = source();
    assert!(src.ask_save(AttachmentId(5)));
    let ch = answer(
        &mut src,
        &sent,
        facts(Some("Flight plan.pdf"), 1, Some("application/pdf")),
    );
    assert!(ch.saves);
    assert_eq!(
        src.take_saves(),
        [SaveAnswer::Suggested {
            attachment: AttachmentId(5),
            name: "Flight plan.pdf".into()
        }]
    );
    assert!(src.take_saves().is_empty(), "taken once");
    // A file with no name of its own: the sender, the day, the time (local time, digits), the
    // number of its place in an album, and the extension of what it is.
    assert!(src.ask_save(AttachmentId(6)));
    answer(&mut src, &sent, facts(None, 2, Some("image/png")));
    let [SaveAnswer::Suggested { name, .. }] = &src.take_saves()[..] else {
        panic!("one name");
    };
    assert!(
        name.ends_with(" 2.png") && !name.contains(':') && name.contains("2025-"),
        "{name}"
    );
    // An attachment the core no longer knows ends the ask: a failure the notice words.
    assert!(src.ask_save(AttachmentId(7)));
    let (token, _) = take_queries(&sent).remove(0);
    let ch = src.on_event(&result(token, QueryData::SaveFacts(None)));
    assert!(ch.saves);
    let [SaveAnswer::Done(done)] = &src.take_saves()[..] else {
        panic!("a failed answer");
    };
    assert_eq!(
        (done.saved, done.failed, done.failure),
        (0, 1, Some(SaveFailure::Gone))
    );
}

#[test]
fn the_dialogs_answer_replaces_and_says_what_it_wrote() {
    let (mut src, sent) = source();
    let dir = PathBuf::from("/home/ari/out");
    assert!(src.save_to(AttachmentId(5), dir.clone(), "Flight plan.pdf".into()));
    // At once, no query, replacing: the dialog asked about a file of that name.
    assert!(take_queries(&sent).is_empty());
    assert_eq!(
        saves(&sent),
        [(5, dir.clone(), "Flight plan.pdf".to_owned(), true)]
    );
    assert!(saved(&mut src, 5).saves);
    let [SaveAnswer::Done(done)] = &src.take_saves()[..] else {
        panic!("one answer");
    };
    assert_eq!((done.saved, done.failed), (1, 0));
    assert_eq!(done.name, "Flight plan.pdf");
    assert_eq!(done.dir, dir);
    // A failure is the class of the reason.
    assert!(src.save_to(AttachmentId(6), dir, "x.pdf".into()));
    saves(&sent);
    let ch = refused(&mut src, 6, CoreError::Save(SaveError::NoSpace));
    assert!(ch.saves);
    let [SaveAnswer::Done(done)] = &src.take_saves()[..] else {
        panic!("one answer");
    };
    assert_eq!((done.saved, done.failed), (0, 1));
    assert_eq!(done.failure, Some(SaveFailure::NoSpace));
}

/// The dialog asked about the name the user typed, so that very name is sent, and replaces: a
/// cleaned name written with `Replace` could overwrite another file (`report_v2.pdf` for a typed
/// `report:v2.pdf`) that the dialog never asked about.
#[test]
fn the_name_the_user_typed_is_saved_as_it_is_and_never_cleaned() {
    let (mut src, sent) = source();
    let dir = PathBuf::from("/home/ari/out");
    for typed in [
        "report:v2.pdf",
        "-notes.txt",
        "a  b.pdf",
        ".config.txt",
        "CON.txt",
    ] {
        assert!(src.save_to(AttachmentId(5), dir.clone(), typed.into()));
        assert_eq!(
            saves(&sent),
            [(5, dir.clone(), typed.to_owned(), true)],
            "{typed}: the name as typed, replacing the file the dialog asked about"
        );
        assert!(saved(&mut src, 5).saves);
        let [SaveAnswer::Done(done)] = &src.take_saves()[..] else {
            panic!("one answer");
        };
        assert_eq!(
            done.name, typed,
            "the notice names the file that was written"
        );
    }
}

#[test]
fn save_all_gives_each_file_its_name_never_replaces_and_answers_once() {
    let (mut src, sent) = source();
    let dir = PathBuf::from("/home/ari/out");
    assert!(src.save_all(
        vec![AttachmentId(1), AttachmentId(2), AttachmentId(3)],
        dir.clone()
    ));
    // Three facts queries, nothing written yet.
    let queries = take_queries(&sent);
    assert_eq!(queries.len(), 3);
    assert!(saves(&sent).is_empty());
    // All three come with the same name (three files called report.pdf).
    for (token, _) in &queries {
        src.on_event(&result(
            *token,
            QueryData::SaveFacts(Some(facts(Some("report.pdf"), 1, Some("application/pdf")))),
        ));
    }
    let first = saves(&sent);
    assert_eq!(first.len(), 3);
    assert!(
        first
            .iter()
            .all(|(_, d, name, replaces)| d == &dir && name == "report.pdf" && !replaces),
        "a batch never replaces: {first:?}"
    );
    // The first is written; the second and third meet its name.
    assert!(
        !saved(&mut src, 1).saves,
        "the batch answers with its last file"
    );
    let exists = || CoreError::Save(SaveError::Exists);
    refused(&mut src, 2, exists());
    refused(&mut src, 3, exists());
    let retry = saves(&sent);
    assert_eq!(
        retry
            .iter()
            .map(|r| (r.0, r.2.as_str()))
            .collect::<Vec<_>>(),
        [(2, "report (1).pdf"), (3, "report (1).pdf")],
        "the next name, asked again with no replace"
    );
    assert!(retry.iter().all(|r| !r.3));
    assert!(!saved(&mut src, 2).saves);
    // The third meets the second's file: one more number.
    refused(&mut src, 3, exists());
    assert_eq!(
        saves(&sent).iter().map(|r| r.2.clone()).collect::<Vec<_>>(),
        ["report (2).pdf"]
    );
    assert!(
        src.take_saves().is_empty(),
        "nothing is told before the last file"
    );
    let ch = saved(&mut src, 3);
    assert!(ch.saves);
    let [SaveAnswer::Done(done)] = &src.take_saves()[..] else {
        panic!("one answer for the batch");
    };
    assert_eq!((done.saved, done.failed, done.failure), (3, 0, None));
    assert_eq!(done.name, "report (2).pdf", "the last file written");
    assert_eq!(done.dir, dir);
}

/// The core's answers name the attachment only, so a second save of an attachment whose first is
/// in flight waits for that answer: nothing is sent for it before, and the two answers end the
/// two calls, each in its own folder.
#[test]
fn a_second_save_of_a_file_in_flight_waits_for_the_first_answer() {
    for second_fails in [false, true] {
        let (mut src, sent) = source();
        let (x, y) = (PathBuf::from("/home/ari/x"), PathBuf::from("/home/ari/y"));
        // Save All of an album (a, b) into X; both files are on their way to the core.
        assert!(src.save_all(vec![AttachmentId(1), AttachmentId(2)], x.clone()));
        for (token, _) in take_queries(&sent) {
            let facts = facts(Some("p.jpg"), 1, Some("image/jpeg"));
            src.on_event(&result(token, QueryData::SaveFacts(Some(facts))));
        }
        let first = saves(&sent);
        assert_eq!(first.len(), 2, "{first:?}");
        // The viewer's Ctrl+S on a, into Y, before any answer.
        assert!(src.save_to(AttachmentId(1), y.clone(), "y.jpg".into()));
        assert!(
            saves(&sent).is_empty(),
            "one save of a file at a time: the second waits"
        );
        // The first answer for a ends the album's file and starts the one for Y, replacing.
        assert!(!saved(&mut src, 1).saves, "the album still waits for b");
        assert!(src.take_saves().is_empty(), "no notice for the album yet");
        assert_eq!(saves(&sent), [(1, y.clone(), "y.jpg".to_owned(), true)]);
        // b ends the album: ONE answer, two files in X, not the single save's name.
        assert!(saved(&mut src, 2).saves);
        let [SaveAnswer::Done(album)] = &src.take_saves()[..] else {
            panic!("the album's answer");
        };
        assert_eq!((album.saved, album.failed, &album.dir), (2, 0, &x));
        // The second answer for a ends the single save, in Y.
        let ch = if second_fails {
            refused(&mut src, 1, CoreError::Save(SaveError::NotWritable))
        } else {
            saved(&mut src, 1)
        };
        assert!(ch.saves);
        let [SaveAnswer::Done(single)] = &src.take_saves()[..] else {
            panic!("the single save's answer");
        };
        assert_eq!(&single.dir, &y);
        if second_fails {
            assert_eq!((single.saved, single.failed), (0, 1));
            assert_eq!(single.failure, Some(SaveFailure::NotWritable));
        } else {
            assert_eq!((single.saved, single.failed), (1, 0));
            assert_eq!(single.name, "y.jpg");
        }
        assert!(saves(&sent).is_empty());
        assert!(src.save_waiting.is_empty() && src.save_runs.is_empty());
    }
}

/// A clash of the album's file asks for the next name with the ALBUM's folder, and the waiting
/// save of the same file starts only after the retries ended.
#[test]
fn a_retry_of_the_first_run_keeps_its_folder_and_the_waiting_run_goes_after_it() {
    let (mut src, sent) = source();
    let (x, y) = (PathBuf::from("/home/ari/x"), PathBuf::from("/home/ari/y"));
    assert!(src.save_all(vec![AttachmentId(1)], x.clone()));
    let facts = facts(Some("p.jpg"), 1, Some("image/jpeg"));
    answer(&mut src, &sent, facts);
    assert_eq!(saves(&sent).len(), 1);
    assert!(src.save_to(AttachmentId(1), y.clone(), "y.jpg".into()));
    assert!(saves(&sent).is_empty());
    refused(&mut src, 1, CoreError::Save(SaveError::Exists));
    let retry = saves(&sent);
    assert_eq!(
        retry,
        [(1, x.clone(), "p (1).jpg".to_owned(), false)],
        "the album's retry, into the album's folder, without replacing"
    );
    assert!(saved(&mut src, 1).saves);
    let [SaveAnswer::Done(album)] = &src.take_saves()[..] else {
        panic!("the album's answer");
    };
    assert_eq!(
        (album.saved, &album.dir, album.name.as_str()),
        (1, &x, "p (1).jpg")
    );
    assert_eq!(saves(&sent), [(1, y, "y.jpg".to_owned(), true)]);
}

#[test]
fn a_batch_that_loses_files_says_how_many_and_why() {
    let (mut src, sent) = source();
    let dir = PathBuf::from("/home/ari/out");
    assert!(src.save_all(vec![AttachmentId(1), AttachmentId(2), AttachmentId(3)], dir));
    let queries = take_queries(&sent);
    for (n, (token, _)) in queries.iter().enumerate() {
        let facts = facts(Some(&format!("f{n}.pdf")), 1, None);
        src.on_event(&result(*token, QueryData::SaveFacts(Some(facts))));
    }
    saves(&sent);
    saved(&mut src, 1);
    refused(&mut src, 2, CoreError::Save(SaveError::NotWritable));
    assert!(src.take_saves().is_empty());
    let ch = refused(
        &mut src,
        3,
        CoreError::Refused {
            reason: kourier_core::facade::Refusal::Gone,
            detail: "gone",
        },
    );
    assert!(ch.saves);
    let [SaveAnswer::Done(done)] = &src.take_saves()[..] else {
        panic!("one answer");
    };
    assert_eq!((done.saved, done.failed), (1, 2));
    assert_eq!(
        done.failure,
        Some(SaveFailure::Gone),
        "the last failure's class"
    );
    // An answer for a file nobody saves is none.
    assert!(!saved(&mut src, 99).saves);
    assert!(!refused(&mut src, 99, CoreError::Save(SaveError::NoSpace)).saves);
}

#[test]
fn the_classes_of_a_failed_save() {
    use kourier_core::facade::Refusal;
    for (error, class) in [
        (CoreError::Save(SaveError::NoSpace), SaveFailure::NoSpace),
        (
            CoreError::Save(SaveError::NotWritable),
            SaveFailure::NotWritable,
        ),
        (CoreError::Save(SaveError::NoFolder), SaveFailure::NoFolder),
        (CoreError::Save(SaveError::BadName), SaveFailure::Other),
        (
            CoreError::Refused {
                reason: Refusal::Deleted,
                detail: "x",
            },
            SaveFailure::Gone,
        ),
        (
            CoreError::Refused {
                reason: Refusal::WrongKind,
                detail: "x",
            },
            SaveFailure::Gone,
        ),
        (CoreError::InvalidInput("not here"), SaveFailure::NotHere),
    ] {
        assert_eq!(save_failure(&error), class, "{error:?}");
    }
}

//! `CoreSource`: the export flow (Export Selected, EXP-ui 2) against the simulated core of
//! [`super::tests`]: the plan's numbers, the ticks in the command's lists, each end and its
//! notice, Cancel, and one export at a time.

use std::path::PathBuf;

use kourier_core::facade::{
    ExportFailure, ExportOutcome, ExportPlan, ExportStamp, ExportVerdict, LeftOut, Progress,
    Refusal, RequestId, SaveMode, UiCommand, UiEvent,
};
use kourier_model::{AttachmentId, AttachmentKind, ConversationId};

use super::tests::{Sent, Store, message, settle, source, take_queries};
use super::*;
use crate::export::Ticks;
use crate::export::flow::{ExportEnd, ExportUpdate};
use crate::source::UiSource;

fn planned(
    conv: i64,
    n: i64,
    verdict: ExportVerdict,
    files: Vec<(i64, AttachmentKind)>,
) -> kourier_core::facade::PlannedMessage {
    let row = message(conv, n, false);
    kourier_core::facade::PlannedMessage {
        stamp: ExportStamp {
            id: row.id,
            deleted: false,
            revision: 0,
        },
        row,
        quote: None,
        verdict,
        files: files
            .into_iter()
            .map(|(id, kind)| kourier_core::facade::PlannedFile {
                attachment: AttachmentId(id),
                kind,
                mime: "application/octet-stream".into(),
                size: 1000,
                file_name: Some(format!("f{id}.bin")),
                sniffed: None,
                accepted: Ok(()),
            })
            .collect(),
        revisions: Vec::new(),
    }
}

/// Four messages: a plain one with a picture and a video, a plain one with a file, a
/// disappearing one with a picture, a view-once one.
fn plan() -> ExportPlan {
    ExportPlan {
        conv: ConversationId(1),
        chat: Ok(()),
        members: 2,
        messages: vec![
            planned(
                1,
                1,
                ExportVerdict::Included,
                vec![(11, AttachmentKind::Image), (12, AttachmentKind::Video)],
            ),
            planned(
                1,
                2,
                ExportVerdict::Included,
                vec![(13, AttachmentKind::File)],
            ),
            planned(
                1,
                3,
                ExportVerdict::NeedsTick,
                vec![(14, AttachmentKind::Image)],
            ),
            planned(1, 4, ExportVerdict::LeftOut(LeftOut::ViewOnce), Vec::new()),
        ],
        now: super::tests::NOW,
    }
}

fn opened(plan: ExportPlan) -> (CoreSource, Sent, Store) {
    let (mut src, sent) = source();
    let mut store = Store::new(&[(1, 5)]);
    store.features.export = kourier_model::ChatExport::Allowed;
    store.plan = Some(plan);
    src.reload();
    src.open_conversation(Some(ConversationId(1)));
    settle(&mut src, &sent, &store);
    (src, sent, store)
}

fn ids() -> Vec<kourier_model::MessageId> {
    (1..=4)
        .map(|n| kourier_model::MessageId(100_000 + n))
        .collect()
}

fn all_on() -> Ticks {
    Ticks {
        pictures: true,
        videos: true,
        other_files: true,
        disappearing: false,
    }
}

/// Click -> plan -> dialog: the JSON of the dialog.
fn dialog(src: &mut CoreSource, sent: &Sent, store: &Store) -> serde_json::Value {
    let ch = src.export_plan(ConversationId(1), ids());
    assert!(ch.export);
    let ch = settle(src, sent, store);
    assert!(ch.export);
    let updates = src.take_export();
    let [ExportUpdate::Dialog(json)] = &updates[..] else {
        panic!("{updates:?}");
    };
    serde_json::from_str(json).unwrap()
}

/// Everything up to the command: returns it.
fn running(src: &mut CoreSource, sent: &Sent, store: &Store, ticks: Ticks) -> UiCommand {
    dialog(src, sent, store);
    assert!(src.export_choose(ticks));
    let updates = src.take_export();
    assert!(
        matches!(&updates[..], [ExportUpdate::SaveName(n)] if n.ends_with(".zip")),
        "{updates:?}"
    );
    let job = src
        .export_target(PathBuf::from("/nonexistent-export-dir"), "x.zip".into())
        .expect("a job");
    let built = job.run();
    assert_eq!(
        built.built().counts.messages as usize,
        built.built().messages.len()
    );
    src.export_built(built);
    let mut cmds: Vec<UiCommand> = sent
        .borrow_mut()
        .drain(..)
        .filter(|c| matches!(c, UiCommand::ExportArchive { .. }))
        .collect();
    assert_eq!(cmds.len(), 1);
    cmds.remove(0)
}

fn request_of(c: &UiCommand) -> RequestId {
    match c {
        UiCommand::ExportArchive { request, .. } => *request,
        other => panic!("{other:?}"),
    }
}

fn ended(src: &mut CoreSource, request: RequestId, outcome: ExportOutcome) -> ExportEnd {
    let ch = src.on_event(&UiEvent::ExportEnded { request, outcome });
    assert!(ch.export);
    let mut updates = src.take_export();
    match updates.pop() {
        Some(ExportUpdate::Ended(e)) => e,
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_plan_answer_opens_the_dialog_with_its_numbers() {
    let (mut src, sent, store) = opened(plan());
    let v = dialog(&mut src, &sent, &store);
    assert_eq!(v["messages"], 2);
    assert_eq!(v["disappearing"], 1);
    assert_eq!(v["viewOnce"], 1);
    assert_eq!(v["pictures"]["count"], 1);
    assert_eq!(v["videos"]["count"], 1);
    assert_eq!(v["other"]["count"], 1);
    assert_eq!(v["pictures"]["size"], "1.0 KB");
    assert!(
        !src.export_busy(),
        "the dialog stands and nothing runs: the bar behind it shows no progress"
    );
}

#[test]
fn the_ticks_choose_the_messages_and_files_of_the_command() {
    let (mut src, sent, store) = opened(plan());
    let ticks = Ticks {
        videos: false,
        ..all_on()
    };
    let UiCommand::ExportArchive {
        messages,
        files,
        with_disappearing,
        documents,
        mode,
        ..
    } = running(&mut src, &sent, &store, ticks)
    else {
        unreachable!()
    };
    // The view-once and (without the tick) the disappearing message are not listed.
    assert_eq!(messages.len(), 2);
    let mut got: Vec<i64> = files.iter().map(|f| f.attachment.0).collect();
    got.sort_unstable();
    assert_eq!(got, [11, 13], "no video");
    assert!(!with_disappearing);
    assert_eq!(mode, SaveMode::NoReplace, "the name was free");
    let names: Vec<&str> = documents.iter().map(|d| d.entry.as_str()).collect();
    assert_eq!(names, ["index.html", "messages.jsonl"]);

    // With the disappearing tick that message and its picture go in too.
    let (mut src, sent, store) = opened(plan());
    let ticks = Ticks {
        disappearing: true,
        ..all_on()
    };
    let UiCommand::ExportArchive {
        messages,
        files,
        with_disappearing,
        ..
    } = running(&mut src, &sent, &store, ticks)
    else {
        unreachable!()
    };
    assert_eq!(messages.len(), 3);
    assert_eq!(files.len(), 4);
    assert!(with_disappearing);
}

#[test]
fn each_end_has_its_notice_and_the_selection_ends_only_after_done() {
    let cases: Vec<(ExportOutcome, bool, &str)> = vec![
        (
            ExportOutcome::Done {
                messages: 3,
                files: 2,
                failed: vec![AttachmentId(13)],
                bytes: 5,
            },
            true,
            "Exported 3 messages. 1 file could not be read and is not in the archive. Not saved: f13.bin",
        ),
        (
            ExportOutcome::Failed(ExportFailure::Changed),
            false,
            "Messages changed while exporting. Try again.",
        ),
        (
            ExportOutcome::Cancelled,
            false,
            "Export cancelled. Nothing was saved.",
        ),
        (
            ExportOutcome::Failed(ExportFailure::NoSpace),
            false,
            "Could not export the messages: there is no space left on the disk.",
        ),
        (
            ExportOutcome::Failed(ExportFailure::NotWritable),
            false,
            "Could not export the messages: the folder is not writable.",
        ),
        (
            ExportOutcome::Failed(ExportFailure::NoFolder),
            false,
            "Could not export the messages: the folder is gone.",
        ),
        (
            ExportOutcome::Failed(ExportFailure::Io),
            false,
            "Could not export the messages: the file could not be written.",
        ),
        (
            ExportOutcome::Failed(ExportFailure::ChatRefused(Refusal::ExportRestricted)),
            false,
            "This chat's network does not allow saving its messages.",
        ),
    ];
    for (outcome, ends, text) in cases {
        let (mut src, sent, store) = opened(plan());
        let request = request_of(&running(&mut src, &sent, &store, all_on()));
        let end = ended(&mut src, request, outcome);
        assert_eq!(end.text, text);
        assert_eq!(end.selection_ends, ends, "{text}");
        assert_eq!(end.conv, Some(ConversationId(1)), "{text}");
        assert_eq!(end.saved.is_some(), ends);
        assert!(!src.export_busy(), "ready for the next export");
    }
}

#[test]
fn a_taken_name_gets_the_next_number() {
    let (mut src, sent, store) = opened(plan());
    let request = request_of(&running(&mut src, &sent, &store, all_on()));
    let ch = src.on_event(&UiEvent::ExportEnded {
        request,
        outcome: ExportOutcome::Failed(ExportFailure::Exists),
    });
    assert!(!ch.export, "not the end");
    let again: Vec<UiCommand> = sent.borrow_mut().drain(..).collect();
    let [
        UiCommand::ExportArchive {
            request: r2,
            target,
            mode,
            ..
        },
    ] = &again[..]
    else {
        panic!("{again:?}");
    };
    assert_ne!(*r2, request);
    assert_eq!(target.name, "x (1).zip");
    assert_eq!(*mode, SaveMode::NoReplace);
    assert!(src.export_busy());
}

#[test]
fn cancel_sends_cancel_export_and_the_end_comes_with_the_core() {
    let (mut src, sent, store) = opened(plan());
    let request = request_of(&running(&mut src, &sent, &store, all_on()));
    src.take_export();
    let ch = src.on_event(&UiEvent::Progress(Progress::Export {
        request,
        done: 10,
        total: 40,
    }));
    assert!(ch.export);
    assert_eq!(
        src.take_export(),
        [ExportUpdate::Progress {
            done: 10,
            total: 40
        }]
    );
    src.export_cancel();
    let cmds: Vec<UiCommand> = sent.borrow_mut().drain(..).collect();
    assert!(
        matches!(&cmds[..], [UiCommand::CancelExport { request: r }] if *r == request),
        "{cmds:?}"
    );
    assert!(src.export_busy(), "until the core says it ended");
    let end = ended(&mut src, request, ExportOutcome::Cancelled);
    assert!(!end.selection_ends);
    assert!(!src.export_busy());
}

#[test]
fn a_second_click_while_one_runs_is_refused() {
    let (mut src, sent, store) = opened(plan());
    dialog(&mut src, &sent, &store);
    // The notice, and nothing else: no plan query, the stage stays.
    let refused = |src: &mut CoreSource, sent: &Sent| {
        let ch = src.export_plan(ConversationId(1), ids());
        assert!(ch.export, "the notice is handed over");
        assert!(take_queries(sent).is_empty(), "no second plan query");
        assert_eq!(
            src.take_export(),
            [ExportUpdate::Refused("An export is running.".to_owned())]
        );
    };
    refused(&mut src, &sent);
    assert!(
        src.export_numbers(all_on()).is_some(),
        "still in the dialog"
    );
    // Still refused while the save dialog stands, the documents are made and the command runs.
    assert!(src.export_choose(all_on()));
    src.take_export();
    refused(&mut src, &sent);
    let job = src
        .export_target(PathBuf::from("/nonexistent-export-dir"), "x.zip".into())
        .unwrap();
    src.take_export();
    refused(&mut src, &sent);
    src.export_built(job.run());
    refused(&mut src, &sent);
    assert!(src.export_busy(), "the running export is untouched");
}

#[test]
fn cancel_before_the_command_drops_the_job_and_the_late_documents() {
    let (mut src, sent, store) = opened(plan());
    dialog(&mut src, &sent, &store);
    assert!(src.export_choose(all_on()));
    src.take_export();
    let job = src
        .export_target(PathBuf::from("/nonexistent-export-dir"), "x.zip".into())
        .unwrap();
    src.take_export();
    let ch = src.export_cancel();
    assert!(ch.export);
    assert!(!src.export_busy());
    src.export_built(job.run());
    assert!(
        sent.borrow()
            .iter()
            .all(|c| !matches!(c, UiCommand::ExportArchive { .. })),
        "nothing is sent for a cancelled export"
    );
}

#[test]
fn a_refused_chat_shows_a_notice_and_no_dialog() {
    let mut p = plan();
    p.chat = Err(Refusal::ExportUnstated);
    p.messages.clear();
    let (mut src, sent, store) = opened(p);
    src.export_plan(ConversationId(1), ids());
    settle(&mut src, &sent, &store);
    let updates = src.take_export();
    let [ExportUpdate::Ended(e)] = &updates[..] else {
        panic!("{updates:?}");
    };
    assert!(e.text.contains("cannot tell yet"), "{}", e.text);
    assert!(!e.selection_ends);
    assert!(!src.export_busy());
}

#[test]
fn the_dialog_closed_with_no_answer_drops_the_plan() {
    let (mut src, sent, store) = opened(plan());
    dialog(&mut src, &sent, &store);
    src.export_abandon();
    assert!(!src.export_busy());
    assert!(!src.export_choose(all_on()), "no dialog stands");
}

#[test]
fn a_chat_the_header_refuses_never_asks_for_a_plan() {
    let (mut src, sent) = source();
    let mut store = Store::new(&[(1, 5)]);
    store.features.export = kourier_model::ChatExport::Restricted;
    src.reload();
    src.open_conversation(Some(ConversationId(1)));
    settle(&mut src, &sent, &store);
    let ch = src.export_plan(ConversationId(1), ids());
    assert!(ch.export);
    assert!(
        take_queries(&sent).is_empty(),
        "no plan query for a refused chat"
    );
    let updates = src.take_export();
    let [ExportUpdate::Ended(e)] = &updates[..] else {
        panic!("{updates:?}");
    };
    assert!(e.text.contains("does not allow"), "{}", e.text);
    assert!(!src.export_busy());
}

#[test]
fn only_a_done_export_ends_the_selection_and_only_in_its_own_chat() {
    let (mut src, sent, store) = opened(plan());
    let request = request_of(&running(&mut src, &sent, &store, all_on()));
    let done = ended(
        &mut src,
        request,
        ExportOutcome::Done {
            messages: 2,
            files: 1,
            failed: Vec::new(),
            bytes: 5,
        },
    );
    assert!(
        done.ends_selection_in(ConversationId(1)),
        "Done in A, A open"
    );
    assert!(
        !done.ends_selection_in(ConversationId(2)),
        "Done in A, B open"
    );
    for outcome in [
        ExportOutcome::Cancelled,
        ExportOutcome::Failed(ExportFailure::Changed),
        ExportOutcome::Failed(ExportFailure::Io),
    ] {
        let (mut src, sent, store) = opened(plan());
        let request = request_of(&running(&mut src, &sent, &store, all_on()));
        let end = ended(&mut src, request, outcome);
        assert!(!end.ends_selection_in(ConversationId(1)));
        assert!(!end.ends_selection_in(ConversationId(2)));
    }
}

#[test]
fn a_cancel_that_crosses_an_exists_is_not_retried() {
    let (mut src, sent, store) = opened(plan());
    let request = request_of(&running(&mut src, &sent, &store, all_on()));
    src.export_cancel();
    sent.borrow_mut().clear();
    let end = ended(
        &mut src,
        request,
        ExportOutcome::Failed(ExportFailure::Exists),
    );
    assert_eq!(end.text, "Export cancelled. Nothing was saved.");
    assert!(!end.selection_ends);
    assert!(
        sent.borrow()
            .iter()
            .all(|c| !matches!(c, UiCommand::ExportArchive { .. })),
        "no second ExportArchive after a cancel"
    );
    assert!(!src.export_busy());
    // A Done that was too late is a Done.
    let (mut src, sent, store) = opened(plan());
    let request = request_of(&running(&mut src, &sent, &store, all_on()));
    src.export_cancel();
    let end = ended(
        &mut src,
        request,
        ExportOutcome::Done {
            messages: 2,
            files: 1,
            failed: Vec::new(),
            bytes: 5,
        },
    );
    assert!(end.selection_ends);
}

#[test]
fn the_archive_name_never_carries_an_unnamed_peers_number() {
    let name_of = |direct: bool, title: &str| {
        let (mut src, sent, store) = opened(plan());
        dialog(&mut src, &sent, &store);
        // (After the dialog: the plan's round trip reloads the detail from the store.)
        let d = src.detail.as_mut().expect("the open chat");
        if direct {
            // A peer the names source has no name for (a phone-number id), and a title that
            // is only the number.
            d.row.kind = kourier_model::ConvKind::Direct;
            d.row.peer = Some(UserId::parse("e164:+447700900123").expect("id"));
        } else {
            d.row.kind = kourier_model::ConvKind::Group;
            d.row.peer = None;
        }
        d.row.title = Some(title.to_owned());
        assert!(src.export_choose(all_on()));
        match src.take_export().pop() {
            Some(ExportUpdate::SaveName(n)) => n,
            other => panic!("{other:?}"),
        }
    };
    // A direct chat that is only a number: the fallback word, none of the digits.
    let number = "447700900123";
    let n = name_of(true, number);
    assert!(n.starts_with("Chat "), "{n}");
    assert!(!n.contains("44770") && !n.contains("00123"), "{n}");
    // A named chat keeps its name.
    let n = name_of(false, "Cycling");
    assert!(n.starts_with("Cycling "), "{n}");
}

#[test]
fn a_protected_chat_s_header_says_why_export_is_off() {
    let (mut src, sent) = source();
    let mut store = Store::new(&[(1, 5)]);
    store.features.export = kourier_model::ChatExport::Allowed;
    store.protected = vec![1];
    src.reload();
    src.open_conversation(Some(ConversationId(1)));
    settle(&mut src, &sent, &store);
    let header = src.chat(ConversationId(1)).expect("the header");
    assert_eq!(
        header.export_off,
        "This chat is protected, so it cannot be exported."
    );
}

#[test]
fn the_dialog_numbers_follow_the_ticks_and_only_a_dialog_has_them() {
    let (mut src, sent, store) = opened(plan());
    assert!(
        src.export_numbers(all_on()).is_none(),
        "no dialog, no numbers"
    );
    dialog(&mut src, &sent, &store);
    let v: serde_json::Value =
        serde_json::from_str(&src.export_numbers(all_on()).expect("numbers")).unwrap();
    assert_eq!(v["messages"], 2);
    let with = Ticks {
        disappearing: true,
        ..all_on()
    };
    let v: serde_json::Value =
        serde_json::from_str(&src.export_numbers(with).expect("numbers")).unwrap();
    assert_eq!(v["messages"], 3);
    assert_eq!(
        v["pictures"]["count"], 2,
        "the timed picture joins the line"
    );
    let none = Ticks {
        pictures: false,
        videos: false,
        other_files: false,
        disappearing: true,
    };
    let v: serde_json::Value =
        serde_json::from_str(&src.export_numbers(none).expect("numbers")).unwrap();
    assert_eq!(v["totalSize"], "0 B");
}

#[test]
fn failed_file_names_are_cleaned_and_cut_and_nothing_prints_text() {
    use crate::export::flow::{end_of, shown_file_name};
    let long = "a".repeat(200);
    let cut = shown_file_name(&long);
    assert_eq!(cut.chars().count(), 61, "60 characters and the ellipsis");
    assert!(cut.ends_with('…'));
    // Characters, not bytes.
    let wide = "é".repeat(100);
    assert_eq!(shown_file_name(&wide).chars().count(), 61);
    let odd = shown_file_name("line\nbreak\u{202e}gpj.png");
    assert!(!odd.contains('\n') && !odd.contains('\u{202e}'), "{odd:?}");
    let end = end_of(
        &ExportOutcome::Done {
            messages: 1,
            files: 0,
            failed: vec![AttachmentId(1)],
            bytes: 0,
        },
        std::slice::from_ref(&long),
        PathBuf::from("/secret/dir/x.zip"),
        Some(ConversationId(1)),
    );
    assert!(end.text.contains('…') && !end.text.contains(&long));
    // Debug prints lengths and `is_some` only.
    let printed = format!(
        "{end:?} {:?} {:?}",
        ExportUpdate::Dialog("{\"secret\":1}".into()),
        ExportUpdate::SaveName("Secret Chat.zip".into())
    );
    assert!(
        !printed.contains("secret") && !printed.contains("Secret") && !printed.contains("aaaa"),
        "{printed}"
    );
}

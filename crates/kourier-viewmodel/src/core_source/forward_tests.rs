//! `CoreSource`: the forward flow (forward line F5) against the simulated core of
//! [`super::tests`]: the dialog's plan and ticks, a job that downloads first, lost files and the
//! question, the look Picture, which targets get headers, the end notice, and the staged forward.

use std::path::PathBuf;

use kourier_core::facade::{
    ForwardChange, ForwardLook, ForwardPlan, ForwardRefusal, ForwardVerdict, PlannedForward,
    PlannedTarget, Query, UiCommand, UiEvent,
};
use kourier_model::{AccountId, AttachmentId, ConversationId, MessageId};

use super::tests::{Sent, Store, message, settle, source, take_queries};
use super::*;
use crate::forward::ForwardUpdate;
use crate::source::UiSource;

fn conv(n: i64) -> ConversationId {
    ConversationId(n)
}

/// Three chats: the source 1 (messages 1 and 2 are a person's, 3 is ours: `message`'s rule) and the targets 2, 3.
fn setup() -> (CoreSource, Sent, Store) {
    let (mut src, sent) = source();
    let mut store = Store::new(&[(1, 3), (2, 1), (3, 1)]);
    for m in store.convs.get_mut(&1).expect("source").iter_mut() {
        m.actions.forward = true;
    }
    src.reload();
    settle(&mut src, &sent, &store);
    (src, sent, store)
}

fn ids() -> Vec<MessageId> {
    vec![MessageId(100_001), MessageId(100_003)]
}

fn planned_messages() -> Vec<PlannedForward> {
    // 100_001 is a person's, 100_003 is ours (`message` makes every third one ours).
    [1, 3]
        .into_iter()
        .map(|n| {
            let row = message(1, n, false);
            PlannedForward {
                id: row.id,
                row: Some(row),
                verdict: ForwardVerdict::Goes,
            }
        })
        .collect()
}

fn target(c: i64, native: bool) -> PlannedTarget {
    PlannedTarget {
        conv: conv(c),
        account: Some(AccountId(1)),
        native,
        refused: None,
        changes: Vec::new(),
    }
}

fn plan(targets: Vec<PlannedTarget>) -> ForwardPlan {
    ForwardPlan {
        source: conv(1),
        now: super::tests::NOW,
        messages: planned_messages(),
        targets,
        to_download: Vec::new(),
        lost: Vec::new(),
        limits: Vec::new(),
        many: false,
    }
}

/// The forward plan queries sent so far: `(token, asked targets)`.
fn plan_queries(sent: &Sent) -> Vec<(QueryToken, Vec<ConversationId>)> {
    take_queries(sent)
        .into_iter()
        .map(|(t, q)| match q {
            Query::ForwardPlan { targets, .. } => {
                assert_eq!(t.model, MODEL_FORWARD);
                (t, targets)
            }
            other => panic!("not a forward plan: {other:?}"),
        })
        .collect()
}

fn answer(src: &mut CoreSource, token: QueryToken, plan: ForwardPlan) -> Changes {
    src.on_event(&UiEvent::QueryResult {
        token,
        result: Ok(kourier_core::facade::QueryData::ForwardPlan(Box::new(plan))),
    })
}

/// Opens the dialog on `plan` with `ticked` ticked, returns the updates taken.
fn opened(src: &mut CoreSource, sent: &Sent, ticked: Vec<ConversationId>, p: &ForwardPlan) {
    src.forward_open(conv(1), ids());
    let [(t, targets)] = &plan_queries(sent)[..] else {
        panic!("one plan query");
    };
    assert!(targets.is_empty());
    answer(src, *t, p.clone());
    src.forward_tick(ticked.clone());
    let [(t, targets)] = &plan_queries(sent)[..] else {
        panic!("one plan query");
    };
    assert_eq!(targets, &ticked);
    let ch = answer(src, *t, p.clone());
    assert!(ch.forward);
}

fn commands(sent: &Sent) -> Vec<UiCommand> {
    let all = std::mem::take(&mut *sent.borrow_mut());
    let (queries, other): (Vec<_>, Vec<_>) = all
        .into_iter()
        .partition(|c| matches!(c, UiCommand::Query { .. }));
    *sent.borrow_mut() = queries;
    other
}

fn forwards(cmds: &[UiCommand]) -> Vec<&UiCommand> {
    cmds.iter()
        .filter(|c| matches!(c, UiCommand::Forward { .. }))
        .collect()
}

#[test]
fn the_dialog_opens_from_the_plan_answer() {
    let (mut src, sent, _store) = setup();
    assert!(!src.forward_open(conv(1), ids()).forward);
    let [(t, targets)] = &plan_queries(&sent)[..] else {
        panic!("one plan query");
    };
    assert!(targets.is_empty());
    assert!(src.take_forward().is_empty());
    let ch = answer(&mut src, *t, plan(Vec::new()));
    assert!(ch.forward);
    let updates = src.take_forward();
    let [ForwardUpdate::Dialog(json)] = &updates[..] else {
        panic!("{updates:?}");
    };
    let v: serde_json::Value = serde_json::from_str(json).expect("json");
    assert_eq!(v["count"], 2);
    assert_eq!(v["title"], "Forward 2 Messages");
    // Nothing ticked: nothing to send.
    assert_eq!(v["canForward"], false);
    assert_eq!(src.forward_send(ForwardLook::Header, ""), None);
    // The targets are the list's chats.
    assert_eq!(src.forward_targets().len(), 3);
}

#[test]
fn ticks_plan_again_and_the_newest_win() {
    let (mut src, sent, _store) = setup();
    let p = plan(vec![target(2, true)]);
    src.forward_open(conv(1), ids());
    let [(t, _)] = &plan_queries(&sent)[..] else {
        panic!();
    };
    answer(&mut src, *t, p.clone());
    src.take_forward();
    src.forward_tick(vec![conv(2)]);
    let [(first, asked)] = &plan_queries(&sent)[..] else {
        panic!("one query");
    };
    assert_eq!(asked, &vec![conv(2)]);
    // Two more ticks while it is in flight: no new query yet.
    src.forward_tick(vec![conv(2), conv(3)]);
    src.forward_tick(vec![conv(3)]);
    assert!(plan_queries(&sent).is_empty());
    // Its answer is not shown; the newest ticks are asked.
    answer(&mut src, *first, p.clone());
    assert!(src.take_forward().is_empty());
    let [(second, asked)] = &plan_queries(&sent)[..] else {
        panic!("one query");
    };
    assert_eq!(asked, &vec![conv(3)]);
    let ch = answer(&mut src, *second, plan(vec![target(3, true)]));
    assert!(ch.forward);
    assert_eq!(src.take_forward().len(), 1);
}

/// Opens, ticks chat 2 and sends: the job's first plan query is out and answered by the caller.
fn started(
    src: &mut CoreSource,
    sent: &Sent,
    look: ForwardLook,
    comment: &str,
    p: &ForwardPlan,
) -> u64 {
    opened(src, sent, vec![conv(2)], p);
    src.take_forward();
    let request = src.forward_send(look, comment).expect("a job");
    assert!(commands(sent).is_empty());
    request
}

#[test]
fn a_file_to_download_is_asked_once_and_the_forward_waits_for_it() {
    let (mut src, sent, _store) = setup();
    let mut p = plan(vec![target(2, true)]);
    let request = started(&mut src, &sent, ForwardLook::Header, "  look at this  ", &p);
    let [(t, targets)] = &plan_queries(&sent)[..] else {
        panic!("one query");
    };
    assert_eq!(targets, &vec![conv(2)]);
    p.to_download = vec![AttachmentId(7)];
    answer(&mut src, *t, p.clone());
    let cmds = commands(&sent);
    assert!(
        matches!(&cmds[..], [UiCommand::DownloadAttachment { attachment }] if *attachment == AttachmentId(7)),
        "{cmds:?}"
    );
    assert_eq!(
        src.take_forward(),
        vec![ForwardUpdate::Progress {
            request,
            done: 0,
            total: 1
        }]
    );
    // The file is still coming: the source changed, the plan is asked again, nothing is asked
    // of the core twice, and a second event while the plan is in flight makes one more query only.
    src.on_event(&UiEvent::TimelineInvalidated { conv: conv(1) });
    src.on_event(&UiEvent::TimelineInvalidated { conv: conv(1) });
    let [(t, _)] = &plan_queries(&sent)[..] else {
        panic!("one query in flight");
    };
    answer(&mut src, *t, p.clone());
    assert!(commands(&sent).is_empty());
    // The replan flag set by the second event asks once more.
    let [(t, _)] = &plan_queries(&sent)[..] else {
        panic!("the follow-up query");
    };
    // The file is here.
    p.to_download.clear();
    answer(&mut src, *t, p.clone());
    let cmds = commands(&sent);
    let [
        UiCommand::Forward {
            request: r,
            source,
            messages,
            targets,
            look,
            headers,
            comment,
            without,
            ..
        },
    ] = &cmds[..]
    else {
        panic!("{cmds:?}");
    };
    assert_eq!(r.0, request);
    assert_eq!(*source, conv(1));
    assert_eq!(messages, &ids());
    assert_eq!(targets, &vec![conv(2)]);
    assert_eq!(*look, ForwardLook::Header);
    // A native target gets no header, and the comment is trimmed.
    assert!(headers.is_empty());
    assert_eq!(
        comment.as_ref().map(|c| c.body.as_str()),
        Some("  look at this")
    );
    assert!(without.is_empty());
    assert_eq!(
        src.take_forward(),
        vec![ForwardUpdate::Progress {
            request,
            done: 1,
            total: 1
        }]
    );
    // The end.
    src.on_event(&UiEvent::ForwardEnded {
        request: RequestId(request),
        sent: vec![conv(2)],
        refused: Vec::new(),
    });
    let updates = src.take_forward();
    let [ForwardUpdate::Ended { request: r, notice }] = &updates[..] else {
        panic!("{updates:?}");
    };
    assert_eq!(*r, request);
    assert!(notice.starts_with("Forwarded to "), "{notice}");
}

#[test]
fn a_lost_file_asks_and_the_answer_goes_on_or_ends() {
    for send in [true, false] {
        let (mut src, sent, _store) = setup();
        let mut p = plan(vec![target(2, true)]);
        let request = started(&mut src, &sent, ForwardLook::Plain, "", &p);
        p.lost = vec![AttachmentId(9)];
        let [(t, _)] = &plan_queries(&sent)[..] else {
            panic!();
        };
        answer(&mut src, *t, p.clone());
        assert_eq!(
            src.take_forward(),
            vec![ForwardUpdate::AskWithout { request, count: 1 }]
        );
        assert!(forwards(&commands(&sent)).is_empty());
        // While the question stands the source's events ask nothing.
        src.on_event(&UiEvent::TimelineInvalidated { conv: conv(1) });
        assert!(plan_queries(&sent).is_empty());
        src.forward_without(request, send);
        if send {
            let [(t, _)] = &plan_queries(&sent)[..] else {
                panic!("a new plan");
            };
            answer(&mut src, *t, p.clone());
            let cmds = commands(&sent);
            let [
                UiCommand::Forward {
                    without, comment, ..
                },
            ] = &cmds[..]
            else {
                panic!("{cmds:?}");
            };
            assert_eq!(without, &vec![AttachmentId(9)]);
            assert!(comment.is_none());
        } else {
            let updates = src.take_forward();
            assert_eq!(
                updates,
                vec![ForwardUpdate::Ended {
                    request,
                    notice: String::new()
                }]
            );
            assert!(plan_queries(&sent).is_empty());
            assert!(forwards(&commands(&sent)).is_empty());
        }
    }
}

#[test]
fn the_look_picture_waits_for_the_pictures_and_sends_them() {
    let (mut src, sent, _store) = setup();
    // One converted target.
    let p = plan(vec![target(2, false)]);
    let request = started(&mut src, &sent, ForwardLook::Picture, "", &p);
    let [(t, _)] = &plan_queries(&sent)[..] else {
        panic!();
    };
    answer(&mut src, *t, p.clone());
    assert!(forwards(&commands(&sent)).is_empty());
    assert_eq!(
        src.take_forward(),
        vec![ForwardUpdate::RenderPictures {
            request,
            source: conv(1),
            messages: ids(),
        }]
    );
    // A later plan does not ask for the pictures again.
    src.on_event(&UiEvent::TimelineInvalidated { conv: conv(1) });
    assert!(plan_queries(&sent).is_empty());
    src.forward_pictures(request, vec![PathBuf::from("/nonexistent/forward-0.png")]);
    let cmds = commands(&sent);
    let [
        UiCommand::Forward {
            pictures, headers, ..
        },
    ] = &cmds[..]
    else {
        panic!("{cmds:?}");
    };
    assert_eq!(pictures.expose().len(), 1);
    assert!(headers.is_empty());
}

#[test]
fn a_native_target_with_the_look_picture_asks_for_no_picture() {
    let (mut src, sent, _store) = setup();
    let p = plan(vec![target(2, true)]);
    started(&mut src, &sent, ForwardLook::Picture, "", &p);
    let [(t, _)] = &plan_queries(&sent)[..] else {
        panic!();
    };
    answer(&mut src, *t, p);
    assert_eq!(forwards(&commands(&sent)).len(), 1);
    assert!(src.take_forward().is_empty());
}

#[test]
fn headers_go_only_for_converted_targets_with_header_or_quoted() {
    for (look, native, headers) in [
        (ForwardLook::Header, false, true),
        (ForwardLook::Quoted, false, true),
        (ForwardLook::Plain, false, false),
        (ForwardLook::Header, true, false),
    ] {
        let (mut src, sent, _store) = setup();
        let p = plan(vec![target(2, native)]);
        started(&mut src, &sent, look, "", &p);
        let [(t, _)] = &plan_queries(&sent)[..] else {
            panic!();
        };
        answer(&mut src, *t, p);
        let cmds = commands(&sent);
        let [UiCommand::Forward { headers: h, .. }] = &cmds[..] else {
            panic!("{cmds:?}");
        };
        if headers {
            // Two senders: two runs, each led by its own header; ours is named by the account.
            assert_eq!(h.len(), 2, "{look:?}");
            assert_eq!(h[0].0, MessageId(100_001));
            assert!(
                h[0].1.body.starts_with("Forwarded from "),
                "{}",
                h[0].1.body
            );
            assert_eq!(h[1].0, MessageId(100_003));
            assert!(
                h[1].1.body.starts_with("Forwarded from Me · "),
                "{}",
                h[1].1.body
            );
        } else {
            assert!(h.is_empty(), "{look:?} native {native}");
        }
    }
}

#[test]
fn a_refusal_before_the_command_ends_the_job() {
    let (mut src, sent, _store) = setup();
    let mut p = plan(vec![target(2, true)]);
    let request = started(&mut src, &sent, ForwardLook::Header, "", &p);
    p.targets[0].refused = Some(ForwardRefusal::AccountNotLinked);
    let [(t, _)] = &plan_queries(&sent)[..] else {
        panic!();
    };
    answer(&mut src, *t, p);
    assert!(forwards(&commands(&sent)).is_empty());
    let updates = src.take_forward();
    let [ForwardUpdate::Ended { request: r, notice }] = &updates[..] else {
        panic!("{updates:?}");
    };
    assert_eq!(*r, request);
    assert!(notice.contains("Not forwarded to "), "{notice}");
}

#[test]
fn the_end_names_the_refused_chats() {
    let (mut src, sent, _store) = setup();
    let p = plan(vec![target(2, true), target(3, true)]);
    src.forward_open(conv(1), ids());
    let [(t, _)] = &plan_queries(&sent)[..] else {
        panic!();
    };
    answer(&mut src, *t, p.clone());
    src.forward_tick(vec![conv(2), conv(3)]);
    let [(t, _)] = &plan_queries(&sent)[..] else {
        panic!();
    };
    answer(&mut src, *t, p.clone());
    src.take_forward();
    let request = src.forward_send(ForwardLook::Header, "").expect("job");
    let [(t, _)] = &plan_queries(&sent)[..] else {
        panic!();
    };
    answer(&mut src, *t, p);
    assert_eq!(forwards(&commands(&sent)).len(), 1);
    src.on_event(&UiEvent::ForwardEnded {
        request: RequestId(request),
        sent: vec![conv(2)],
        refused: vec![(conv(3), ForwardRefusal::TooManyChats)],
    });
    let updates = src.take_forward();
    let [ForwardUpdate::Ended { notice, .. }] = &updates[..] else {
        panic!("{updates:?}");
    };
    assert!(notice.starts_with("Forwarded to "), "{notice}");
    assert!(
        notice.contains("Not forwarded to ") && notice.ends_with("Too many chats for one forward."),
        "{notice}"
    );
}

#[test]
fn cancel_ends_a_job_and_asks_nothing_more() {
    let (mut src, sent, _store) = setup();
    let mut p = plan(vec![target(2, true)]);
    let request = started(&mut src, &sent, ForwardLook::Header, "", &p);
    p.to_download = vec![AttachmentId(7)];
    let [(t, _)] = &plan_queries(&sent)[..] else {
        panic!();
    };
    answer(&mut src, *t, p);
    commands(&sent);
    src.take_forward();
    src.forward_cancel(request);
    assert_eq!(
        src.take_forward(),
        vec![ForwardUpdate::Ended {
            request,
            notice: String::new()
        }]
    );
    src.on_event(&UiEvent::TimelineInvalidated { conv: conv(1) });
    assert!(plan_queries(&sent).is_empty());
}

// ---- staging ----

#[test]
fn staging_holds_one_forward_per_target_and_a_new_one_replaces_it() {
    let (mut src, _sent, _store) = setup();
    assert!(src.forward_of(conv(2)).is_none());
    src.set_forward_default(ForwardLook::Quoted);
    assert_eq!(src.forward_default(), ForwardLook::Quoted);
    assert!(src.stage_forward(conv(2), conv(1), ids()));
    assert_eq!(
        src.take_forward(),
        vec![ForwardUpdate::StagedChanged(conv(2))]
    );
    let s = src.forward_of(conv(2)).expect("staged");
    assert_eq!(s.count, 2);
    assert_eq!(s.look, ForwardLook::Quoted);
    assert!(s.native, "all test chats are on one account");
    assert!(!s.waiting);
    assert_eq!(s.from, "c1 v0");
    // Replaced by one message; another target keeps its own.
    assert!(src.stage_forward(conv(2), conv(1), vec![MessageId(100_001)]));
    assert!(src.stage_forward(conv(3), conv(1), ids()));
    assert_eq!(src.forward_of(conv(2)).expect("staged").count, 1);
    assert_eq!(src.forward_of(conv(3)).expect("staged").count, 2);
    // The look changes on the card.
    src.take_forward();
    assert!(src.set_forward_look(conv(2), ForwardLook::Plain));
    assert_eq!(
        src.forward_of(conv(2)).expect("staged").look,
        ForwardLook::Plain
    );
    assert_eq!(
        src.take_forward(),
        vec![ForwardUpdate::StagedChanged(conv(2))]
    );
    assert!(!src.set_forward_look(conv(9), ForwardLook::Plain));
    // Dropped.
    assert!(src.drop_forward(conv(2)));
    assert!(src.forward_of(conv(2)).is_none());
    assert!(!src.drop_forward(conv(2)));
    assert!(src.forward_of(conv(3)).is_some());
}

#[test]
fn staging_refuses_a_chat_that_cannot_send_and_rows_none_of_which_may_go() {
    let (mut src, _sent, _store) = setup();
    // A target that cannot send.
    for c in src.list.iter_mut().filter(|c| c.id == conv(3)) {
        c.can_send = false;
    }
    assert!(!src.stage_forward(conv(3), conv(1), ids()));
    // Not a chat of the list.
    assert!(!src.stage_forward(conv(99), conv(1), ids()));
    // No message at all.
    assert!(!src.stage_forward(conv(2), conv(1), Vec::new()));
    assert!(src.take_forward().is_empty());

    // The source is open and its rows are known: none may be forwarded.
    let (mut src, sent, mut store) = setup();
    for m in store.convs.get_mut(&1).expect("source").iter_mut() {
        m.actions.forward = false;
    }
    src.open_conversation(Some(conv(1)));
    settle(&mut src, &sent, &store);
    assert!(!src.stage_forward(conv(2), conv(1), ids()));
    // One of them may: it goes.
    store.convs.get_mut(&1).expect("source")[0].actions.forward = true;
    src.reload();
    src.open_conversation(Some(conv(1)));
    settle(&mut src, &sent, &store);
    src.on_event(&UiEvent::TimelineInvalidated { conv: conv(1) });
    settle(&mut src, &sent, &store);
    assert!(src.stage_forward(conv(2), conv(1), ids()));
}

#[test]
fn a_staged_forward_is_sent_with_the_comment_and_goes_when_its_job_ends() {
    let (mut src, sent, _store) = setup();
    src.set_forward_default(ForwardLook::Header);
    assert!(src.stage_forward(conv(2), conv(1), ids()));
    src.take_forward();
    assert!(src.send_staged_forward(conv(2), "see this"));
    assert_eq!(
        src.take_forward(),
        vec![ForwardUpdate::StagedChanged(conv(2))]
    );
    assert!(src.forward_of(conv(2)).expect("staged").waiting);
    // One job at a time per stage.
    assert!(!src.send_staged_forward(conv(2), "again"));
    let [(t, targets)] = &plan_queries(&sent)[..] else {
        panic!("one query");
    };
    assert_eq!(targets, &vec![conv(2)]);
    answer(&mut src, *t, plan(vec![target(2, true)]));
    let cmds = commands(&sent);
    let [
        UiCommand::Forward {
            request,
            targets,
            comment,
            source,
            ..
        },
    ] = &cmds[..]
    else {
        panic!("{cmds:?}");
    };
    assert_eq!(targets, &vec![conv(2)]);
    assert_eq!(*source, conv(1));
    assert_eq!(comment.as_ref().map(|c| c.body.as_str()), Some("see this"));
    // The stage stays, waiting, until the end; then it is gone.
    assert!(src.forward_of(conv(2)).is_some());
    src.take_forward();
    src.on_event(&UiEvent::ForwardEnded {
        request: *request,
        sent: vec![conv(2)],
        refused: Vec::new(),
    });
    let updates = src.take_forward();
    assert!(
        updates.contains(&ForwardUpdate::StagedChanged(conv(2))),
        "{updates:?}"
    );
    assert!(src.forward_of(conv(2)).is_none());
}

#[test]
fn a_refused_staged_forward_stays_for_another_try() {
    let (mut src, sent, _store) = setup();
    assert!(src.stage_forward(conv(2), conv(1), ids()));
    assert!(src.send_staged_forward(conv(2), ""));
    let [(t, _)] = &plan_queries(&sent)[..] else {
        panic!();
    };
    answer(&mut src, *t, plan(vec![target(2, true)]));
    let cmds = commands(&sent);
    let [UiCommand::Forward { request, .. }] = &cmds[..] else {
        panic!("{cmds:?}");
    };
    src.on_event(&UiEvent::ForwardEnded {
        request: *request,
        sent: Vec::new(),
        refused: vec![(conv(2), ForwardRefusal::Failed)],
    });
    let s = src.forward_of(conv(2)).expect("still staged");
    assert!(!s.waiting);
}

#[test]
fn dropping_a_staged_forward_cancels_its_job() {
    let (mut src, sent, _store) = setup();
    assert!(src.stage_forward(conv(2), conv(1), ids()));
    assert!(src.send_staged_forward(conv(2), ""));
    plan_queries(&sent);
    src.take_forward();
    assert!(src.drop_forward(conv(2)));
    let updates = src.take_forward();
    assert!(
        updates
            .iter()
            .any(|u| matches!(u, ForwardUpdate::Ended { notice, .. } if notice.is_empty())),
        "{updates:?}"
    );
    assert!(updates.contains(&ForwardUpdate::StagedChanged(conv(2))));
    // Nothing is sent when the plan of the dropped job comes.
    src.on_event(&UiEvent::TimelineInvalidated { conv: conv(1) });
    assert!(forwards(&commands(&sent)).is_empty());
}

#[test]
fn a_change_line_for_a_converted_target_reaches_the_dialog() {
    let (mut src, sent, _store) = setup();
    let mut t = target(2, false);
    t.changes = vec![ForwardChange::MentionsAsText(1)];
    let p = plan(vec![t]);
    src.forward_open(conv(1), ids());
    let [(tok, _)] = &plan_queries(&sent)[..] else {
        panic!();
    };
    answer(&mut src, *tok, p);
    let updates = src.take_forward();
    let [ForwardUpdate::Dialog(json)] = &updates[..] else {
        panic!("{updates:?}");
    };
    let v: serde_json::Value = serde_json::from_str(json).expect("json");
    assert_eq!(v["changes"][0], "1 mention becomes plain text.");
    assert_eq!(v["lookShown"], true);
}

#[test]
fn the_render_rows_are_the_going_messages_built_as_the_timeline_builds_them() {
    let (mut src, sent, _store) = setup();
    let p = plan(vec![target(2, false)]);
    let request = started(&mut src, &sent, ForwardLook::Picture, "", &p);
    // No plan yet: nothing; an unknown job: nothing.
    assert!(src.forward_render_rows(request).is_empty());
    let [(t, _)] = &plan_queries(&sent)[..] else {
        panic!();
    };
    answer(&mut src, *t, p);
    let rows = src.forward_render_rows(request);
    let messages: Vec<&crate::timeline::MessageRow> = rows
        .iter()
        .filter(|r| r.kind == crate::timeline::RowKind::Message)
        .collect();
    assert_eq!(messages.len(), 2);
    // Newest first, as the timeline's model takes them.
    assert_eq!(messages[0].message_id(), Some(MessageId(100_003)));
    assert_eq!(messages[1].message_id(), Some(MessageId(100_001)));
    assert!(messages.iter().all(|r| !r.selected));
    // No chat is open: the SOURCE chat (a group) decides the author line, not the open one.
    assert!(src.detail.is_none());
    assert!(messages[1].show_author && !messages[1].own);
    assert!(!messages[0].show_author && messages[0].own);
    assert!(src.forward_render_rows(request + 99).is_empty());
}

//! The media and send steps (TH8): `files` (what the PERSONAL phone sends arrives, opens and plays),
//! `send`, `group-send` and `send-file` (what is sent from the Kourier window arrives once on the
//! PERSONAL phone and shows on the TEST phone as its own).
//!
//! The send is the user's own act in the window, or, with `--lead-sends` (the user's word in chat),
//! the step's own: one marker text into the only chat of its kind, `send-file` with the tool's own
//! picture ([`crate::threema_run::supervise`]'s `step_send`, which takes it once per step and start).
//! Each send step opens the judge's [`SendWindow`](crate::threema_run::judge::SendWindow) BEFORE
//! anything is sent, so the frames of that one send pass within bounds; any other frame of origin
//! `transmit`, in a step or outside one, still halts the run (`transmit`). A paused call keeps its
//! window for the call that goes on; a call that ends closes it.

use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::json;

use super::{Chat, Snap, StepRun, chat_fact, counts_of, poll, settled_baseline};
use crate::threema_run::judge::Said;
use crate::threema_run::supervise::word;

/// How long a send step waits for the chat to show the user's send, in one call.
const SEND_SHOWN_WITHIN_SECS: u64 = 300;

/// What a file step counts: one picture, one file and one voice message.
const FILES_THAT_ARRIVE: i64 = 3;

fn newest<'a>(chats: &'a [Chat], kind: &str) -> Option<&'a Chat> {
    chats
        .iter()
        .filter(|c| c.kind == kind && !c.is_self)
        .max_by_key(|c| c.last_activity)
}

/// The chat a step's own send goes to: the only one of its kind. With two, counts and words cannot
/// tell which of them the phones share, and the tool sends into neither.
fn the_only<'a>(chats: &'a [Chat], kind: &str) -> Result<Option<&'a Chat>> {
    let mut of_kind = chats.iter().filter(|c| c.kind == kind && !c.is_self);
    let first = of_kind.next();
    if of_kind.next().is_some() {
        bail!(
            "more than one {kind} chat is listed: the tool sends only into the one chat of its kind, nothing was sent (without --lead-sends the send is the user's in the window)"
        );
    }
    Ok(first)
}

/// Step `files`: three messages from the PERSONAL phone raise the 1:1 chat's unread counter, then the
/// user says they opened and played in the window. The step opens no chat (opening one marks it
/// read), so the chat must be closed at the start.
pub(super) fn step_files(run: &mut StepRun<'_>) -> Result<()> {
    let base = settled_baseline(run)?;
    let carried = run
        .carried_u64("chat")
        .and_then(|id| i64::try_from(id).ok());
    let chat = match carried {
        Some(id) => base.chats.iter().find(|c| c.id == id),
        None => newest(&base.chats, "direct"),
    };
    let Some(chat) = chat.cloned() else {
        bail!("no 1:1 chat is listed: nothing can arrive (Kourier starts no chat)");
    };
    if carried.is_none() {
        if base.open == chat.id {
            bail!(
                "that 1:1 chat is open in the Kourier window: close it (the step counts its unread messages), then run the step again"
            );
        }
        run.carry_set("chat", json!(chat.id));
        run.carry_set("unread", json!(chat.unread));
    }
    let before = run
        .carry
        .get("unread")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(chat.unread);
    if !run.carried_bool("arrived") {
        run.personal(
            "from the PERSONAL phone, send ONE picture, ONE file (a PDF, say) and ONE voice message to the test ID in your existing 1:1 chat with it",
        );
        run.waiting("for the three messages");
        let bound = run.wait_for(super::STEP_WAIT_MAX_SECS).min(run.left());
        let seen = poll(bound, run.timing.poll.max(Duration::from_millis(5)), || {
            run.halted_now()?;
            let now = Snap::take(run.client)?;
            Ok(now
                .chats
                .iter()
                .any(|c| c.id == chat.id && c.unread >= before + FILES_THAT_ARRIVE)
                .then_some(()))
        })?;
        if seen.is_none() {
            return Err(run.pause(
                "the three messages have not all arrived: run the step again, it keeps the count it started from",
            ));
        }
        run.carry_set("arrived", json!(true));
    }
    run.window("open that chat, open the picture, open the file, and play the voice message");
    if !run.asked_once("files-open", false)? {
        bail!("the picture, the file or the voice message did not open: a finding, read the log");
    }
    if !run.asked_once("avatar-shows", false)? {
        run.note(
            "no profile picture shows: a finding only if the PERSONAL phone has one and shares it",
        );
    }
    let end = Snap::take(run.client)?;
    let facts = end
        .chats
        .iter()
        .find(|c| c.id == chat.id)
        .map(chat_fact)
        .into_iter()
        .collect::<Vec<_>>();
    run.facts = Some(json!({"chats": facts, "arrived": FILES_THAT_ARRIVE}));
    Ok(())
}

struct SendPlan {
    /// The step's name, which is also its window's and its marker's.
    slot: &'static str,
    kind: &'static str,
    instruction: &'static str,
    /// A `--lead-sends` send takes the tool's own picture.
    picture: bool,
    personal: &'static str,
    mirror: &'static str,
}

/// The transmit frames the socket that stands now has written, as its last count line said.
fn transmit_frames(run: &StepRun<'_>) -> Result<(u64, u64)> {
    let (lines, _) = run.client.lines(0)?;
    Ok(counts_of(&lines)
        .last()
        .filter(|c| !c.end)
        .map_or((0, 0), |c| {
            (c.get("out_reflect_transmit"), c.get("out_box_transmit"))
        }))
}

fn send_step(run: &mut StepRun<'_>, plan: &SendPlan) -> Result<()> {
    let result = send_inner(run, plan);
    if run.resume.is_none() {
        let over = run.client.send_window_over(plan.slot);
        result?;
        return over;
    }
    result
}

fn send_inner(run: &mut StepRun<'_>, plan: &SendPlan) -> Result<()> {
    let base = settled_baseline(run)?;
    let carried = run
        .carried_u64("chat")
        .and_then(|id| i64::try_from(id).ok());
    let chat = match carried {
        Some(id) => base.chats.iter().find(|c| c.id == id),
        None if run.args.lead_sends => the_only(&base.chats, plan.kind)?,
        None => newest(&base.chats, plan.kind),
    };
    let Some(chat) = chat.cloned() else {
        bail!(
            "no {} chat is listed: nothing can be sent (Kourier starts no chat)",
            plan.kind
        );
    };
    if carried.is_none() {
        let (_, boxes) = transmit_frames(run)?;
        run.carry_set("chat", json!(chat.id));
        run.carry_set(
            "activity",
            json!(u64::try_from(chat.last_activity).unwrap_or(0)),
        );
        run.carry_set("boxes", json!(boxes));
    }
    let before = run
        .carried_u64("activity")
        .and_then(|v| i64::try_from(v).ok())
        .unwrap_or(chat.last_activity);
    let boxes_before = run.carried_u64("boxes").unwrap_or(0);
    // Before anything is sent: the first frame of the send must find the window open.
    run.client.send_window(plan.slot)?;
    let marker = run.marker(plan.slot);
    if run.carried_bool("told") {
        run.waiting(&format!(
            "for the send asked for before (do NOT send it twice if it went). The text, if it did not:\n{marker}"
        ));
    } else if run.args.lead_sends {
        // Marked first: a call that goes on after this one never sends again.
        run.carry_set("told", json!(true));
        run.client
            .step_send(plan.slot, chat.id, &marker, plan.picture)?;
        run.note("the tool made this step's send itself (--lead-sends)");
        run.waiting(&format!(
            "for the tool's send to show in the chat:\n{marker}"
        ));
    } else {
        run.carry_set("told", json!(true));
        run.window(&format!(
            "{}. The text must be exactly:\n{marker}",
            plan.instruction
        ));
        run.waiting("for your send to show in the chat");
    }
    // The PERSONAL phone's yes stands for a send that showed: `newest_own` reads the chat's preview,
    // and the next message to arrive takes the preview over, so a later call must not wait for it.
    let confirmed = run.carried_answer(plan.personal) == Some(true);
    let shown = if confirmed {
        run.note("the send was seen by an earlier call; the chat's preview is not read again");
        Some(chat.clone())
    } else {
        let bound = run.wait_for(SEND_SHOWN_WITHIN_SECS).min(run.left());
        poll(bound, run.timing.poll.max(Duration::from_millis(5)), || {
            run.halted_now()?;
            let now = Snap::take(run.client)?;
            Ok(now
                .chats
                .iter()
                .find(|c| c.id == chat.id && c.last_activity > before)
                .filter(|c| {
                    matches!(
                        c.newest_own.as_str(),
                        "sent" | "delivered" | "read" | "failed"
                    )
                })
                .cloned())
        })?
    };
    let Some(shown) = shown else {
        return Err(run.pause(
            "the chat does not show the send yet: run the step again, it keeps the text and never asks for a second send",
        ));
    };
    if !confirmed && shown.newest_own == "failed" {
        bail!("the send shows as failed in the chat: a finding, read the log");
    }
    if run.dir.rehearsal {
        run.note("frame counts: not measured (rehearsal)");
    } else {
        let frames = poll(
            run.timing.count_wait.min(run.left()),
            run.timing.poll.max(Duration::from_millis(5)),
            || {
                run.halted_now()?;
                let (reflects, boxes) = transmit_frames(run)?;
                Ok((boxes > boxes_before).then_some((reflects, boxes)))
            },
        )?;
        // The window closes at the step's end, on the counts it has seen: the frames must be among them.
        let Some((reflects, boxes)) = frames else {
            return Err(run.pause(
                "no count line shows the send's frames yet: run the step again, it never asks for a second send",
            ));
        };
        run.note(format!(
            "frame counts of this socket: reflects {reflects}, boxes {boxes}"
        ));
    }
    if !run.asked_once(plan.personal, false)? {
        let _ = run.client.said(Said::Lost)?;
        bail!(
            "the PERSONAL phone did not show the send exactly once: the run is halted (lost). Read the log"
        );
    }
    if !run.asked_once(plan.mirror, false)? {
        bail!("the TEST phone does not show the send as its own: a finding, read the log");
    }
    run.ask_the_warning()?;
    run.see("receipt", json!(word(&shown.newest_own)));
    run.facts = Some(json!({"chats": [chat_fact(&shown)], "receipt": word(&shown.newest_own)}));
    Ok(())
}

pub(super) fn step_send(run: &mut StepRun<'_>) -> Result<()> {
    send_step(
        run,
        &SendPlan {
            slot: "send",
            kind: "direct",
            instruction: "in the Kourier window, open the 1:1 chat with the personal ID and send ONE text",
            picture: false,
            personal: "pp-got-send",
            mirror: "tp-mirrors-send",
        },
    )
}

pub(super) fn step_group_send(run: &mut StepRun<'_>) -> Result<()> {
    send_step(
        run,
        &SendPlan {
            slot: "group-send",
            kind: "group",
            instruction: "in the Kourier window, open the group both phones are in and send ONE text",
            picture: false,
            personal: "pp-got-group-send",
            mirror: "tp-mirrors-group-send",
        },
    )
}

pub(super) fn step_send_file(run: &mut StepRun<'_>) -> Result<()> {
    send_step(
        run,
        &SendPlan {
            slot: "send-file",
            kind: "direct",
            instruction: "in the Kourier window, open the 1:1 chat with the personal ID, attach ONE small picture, type the caption and send it",
            picture: true,
            personal: "pp-got-file",
            mirror: "tp-mirrors-file",
        },
    )
}

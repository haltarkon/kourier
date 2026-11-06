//! The steps, part two (T7; `briefs/tr.md` §2.11 and §5 rows 12-14 and 18, the critic's M5 and M6,
//! the line contract of `target/threema/th6-lines.md`): `kinds`, `phone-sleep`, `flight-mode`,
//! `removed-on-phone` and `unlink`. They run on part one's runner and client ([`super::run_step`],
//! [`super::Client`]) and keep its rules: a step sends nothing from Kourier, opens no chat, prints
//! counts, words and times only, asks his phone questions through `answer` and ends them with S9's
//! question (a).
//!
//! These steps are longer than one call (the phone sleeps 10 minutes, a flight mode waits up to 10
//! for the promotion), so each ends a call that runs out of time FAIL with a resume
//! ([`super::StepRun::pause`]): the phases done, the baselines, his answers and what the report will
//! read (`obs`). Time is the wall clock (`now_ms`): "minute 5" is five minutes from his "locked",
//! however many calls pass.
//!
//! THE LEADER RULE in these steps: Kourier only RECEIVES while the TEST phone leads (`kinds`), and as
//! a leader in `phone-sleep` and `flight-mode` it receives, reflects, acknowledges and sends
//! receipts and (for a message wrapped in forward security) one `Reject` box, all of it within the
//! judge's S5b bounds; the steps count and ask, the judge halts. The S9 question is split (critic
//! M6): (a) a warning about an unofficial client, the device or a request to verify the account
//! halts; (b) a line about forward security in the chat with the test ID is EXPECTED after the link
//! and is only recorded (`fs-line`).
//!
//! `flight-mode` reads the socket's role FIRST (critic M5): when the phone's sleep already made
//! Kourier the leader, no second D1 can come on that socket, so it says so and goes on to the texts.
//! `removed-on-phone` and `unlink` tell the judge a removal is asked BEFORE he acts (R-L2: 4113,
//! `logged_out` and the drop frame are then the removal's own); both are the end of the run and ask
//! him once more whether he wants it (`end-go`).

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use super::{
    Arrivals, Snap, StepRun, arrivals, chat_fact, counts_note, counts_of, newest_message, now_ms,
    one_arrives_in, poll, settled_baseline,
};
use crate::threema_run::judge::Said;
use crate::threema_run::lines::Body;
use crate::threema_run::profile::State;
use crate::threema_run::supervise::word;

// ---- Shared by the steps of part two ------------------------------------------------------------------------

/// A step of part two starts: the run is not halted, the profile is linked and the account is
/// online. Returns the supervisor's `info`.
fn ready(run: &mut StepRun<'_>) -> Result<Value> {
    run.halted_now()?;
    if !State::load(run.dir)?.is_some_and(|s| s.linked()) {
        bail!("this profile is not linked: run `step link` first");
    }
    let online = poll(
        run.timing.online_wait.min(run.left()),
        run.timing.poll,
        || {
            run.halted_now()?;
            let info = run.client.info()?;
            Ok((info["online"] == json!(true)).then_some(info))
        },
    )?;
    match online {
        Some(info) => Ok(info),
        None => bail!(
            "the account is not online after {} s (account: {}): read threema-run/app.log",
            run.timing.online_wait.as_secs(),
            run.client.info()?["account"]
        ),
    }
}

/// How many D1 lines (a socket unblocked incoming messages as the leader) the run has seen.
fn d1_count(info: &Value) -> u64 {
    info["lines"]["D1_UNBLOCKED"].as_u64().unwrap_or(0)
}

/// Whether the live socket leads (a D1 seen on it since its last end).
fn leads(info: &Value) -> bool {
    info["leader"] == json!(true)
}

fn ms_of(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The times (ms since the epoch, the supervisor's receipt) of the D1 lines it still keeps.
fn d1_times(run: &StepRun<'_>) -> Result<Vec<u64>> {
    let (lines, _) = run.client.lines(0)?;
    Ok(lines
        .iter()
        .filter(|l| l.parsed.row == "D1_UNBLOCKED")
        .map(|l| l.at_ms)
        .collect())
}

/// Waits for the wall clock to reach `at_ms`, never past this call's time; a halt ends it. `true`
/// when the time is reached.
fn wait_until(run: &mut StepRun<'_>, at_ms: u64) -> Result<bool> {
    let wait = Duration::from_millis(at_ms.saturating_sub(now_ms()));
    poll(wait.min(run.left()), run.timing.poll, || {
        run.halted_now()?;
        Ok((now_ms() >= at_ms).then_some(()))
    })?;
    Ok(now_ms() >= at_ms)
}

/// Seconds from `from_ms` to now.
fn secs_since(from_ms: u64) -> u64 {
    now_ms().saturating_sub(from_ms) / 1000
}

/// The chats a step wants recorded for `reconnect` (`facts.chats`): the facts of one chat replace
/// an earlier fact of the same chat in what the step carries.
fn carry_chats(run: &mut StepRun<'_>, facts: Vec<Value>) {
    let mut all: Vec<Value> = run
        .carry
        .get("chats")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for fact in facts {
        all.retain(|f| f["chat"] != fact["chat"]);
        all.push(fact);
    }
    run.carry_set("chats", Value::Array(all));
}

/// Writes the carried chats into the record's facts.
fn record_chats(run: &mut StepRun<'_>) {
    let chats = run.carry.get("chats").cloned().unwrap_or_else(|| json!([]));
    run.facts = Some(json!({"chats": chats}));
    run.resume = None;
}

/// A text the TEST phone lacks or shows twice, by his answer: S7, the run halts.
fn lost_or_doubled(run: &mut StepRun<'_>, what: &str) -> anyhow::Error {
    let _ = run.client.said(Said::Lost);
    anyhow!(
        "you said the TEST phone does not show {what} exactly once (missing or doubled): the text counts as lost or doubled and the run is halted (lost). Read threema-run/app.log; a new start needs --after-halt and his word"
    )
}

/// The phone-question (b) of the split S9 (critic M6): a forward-security line on the personal
/// phone is expected after the link; it is only recorded.
fn forward_security_line(run: &mut StepRun<'_>) -> Result<()> {
    if run.has_phone() {
        // A personal-phone look no check can make: ASK-USER, bounded; no answer is "not looked at"
        // and never stops the run.
        run.client.ask("fs-line")?;
        run.tell_question("fs-line");
        let bound = run.left().min(run.timing.answer_wait);
        return match run.person_answer("fs-line", bound)? {
            Some(line) => {
                run.see("fs_line", json!(line));
                run.note(if line {
                    "the PERSONAL phone's chat with the test ID shows a forward-security line (expected after the link: recorded, no stop)"
                } else {
                    "the PERSONAL phone's chat with the test ID shows no forward-security line"
                });
                Ok(())
            }
            None => {
                run.record_origin("fs-line", crate::threema_run::tphone::Origin::NotLooked);
                run.see("fs_line", Value::Null);
                run.note("the forward-security line was not looked at (no answer in its window; it never stops the run)");
                Ok(())
            }
        };
    }
    run.say("PHONE: look at the PERSONAL phone's chat with the test ID: a line about forward security there is expected after the link (it is no stop); answer what you see".to_owned());
    let line = run.asked_once("fs-line", true)?;
    run.see("fs_line", json!(line));
    run.note(if line {
        "the PERSONAL phone's chat with the test ID shows a forward-security line (expected after the link: recorded, no stop)"
    } else {
        "the PERSONAL phone's chat with the test ID shows no forward-security line"
    });
    Ok(())
}

// ---- Step `kinds` ----------------------------------------------------------------------------------------------

/// What the rows of an open chat say about the kinds of message: edits, tombstones, reactions and
/// quotes (counts only; mentions and bold text have no field).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RowCounts {
    edited: i64,
    deleted: i64,
    reactions: i64,
    quoted: i64,
}

impl RowCounts {
    fn of(rows: &[Value]) -> Self {
        let mut c = Self::default();
        for r in rows {
            c.edited += i64::from(r["edited"] == json!(true));
            c.deleted += i64::from(r["deleted"] == json!(true));
            c.quoted += i64::from(!r["quote"].is_null());
            c.reactions += r["reactions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| x["count"].as_i64())
                .sum::<i64>();
        }
        c
    }

    fn to_value(self) -> Value {
        json!({"edited": self.edited, "deleted": self.deleted, "reactions": self.reactions, "quoted": self.quoted})
    }

    fn from_value(v: &Value) -> Self {
        let n = |k: &str| v[k].as_i64().unwrap_or(0);
        Self {
            edited: n("edited"),
            deleted: n("deleted"),
            reactions: n("reactions"),
            quoted: n("quoted"),
        }
    }

    fn minus(self, other: Self) -> Self {
        Self {
            edited: self.edited - other.edited,
            deleted: self.deleted - other.deleted,
            reactions: self.reactions - other.reactions,
            quoted: self.quoted - other.quoted,
        }
    }
}

fn arrived(a: &Arrivals) -> i64 {
    a.known.iter().map(|(_, n)| *n).sum()
}

/// How many chats of `kind` (not the note-to-self chat) changed their activity since the baseline.
fn activity_moved(base: &Snap, now: &Snap, kind: &str) -> i64 {
    let moved = now
        .chats
        .iter()
        .filter(|c| c.kind == kind && !c.is_self)
        .filter(|c| {
            base.chats
                .iter()
                .find(|b| b.id == c.id)
                .is_none_or(|b| b.last_activity != c.last_activity)
        })
        .count();
    i64::try_from(moved).unwrap_or(i64::MAX)
}

/// Step `kinds` (optional, his TR-Q1 yes): the PERSONAL phone sends a quote, a mention in the group,
/// bold text, an edit, a reaction and a delete while the TEST phone leads; Kourier only receives and
/// counts. Counts only, no verdict per kind: the new messages the list shows (a chat HE opened gives
/// its rows: edits, tombstones, reactions, quotes), the chats whose activity moved. The step opens no
/// chat; nothing at all arriving is the one FAIL.
pub(super) fn step_kinds(run: &mut StepRun<'_>) -> Result<()> {
    let info = ready(run)?;
    let resumed = run.carry.get("base").and_then(Snap::from_value);
    let (base, base_newest, rows_base) = match resumed {
        Some((snap, newest)) => {
            run.note(
                "the baseline of the earlier call is used (what came between the calls counts)",
            );
            let rows = run
                .carry
                .get("rows_base")
                .map_or_else(RowCounts::default, RowCounts::from_value);
            (snap, newest, rows)
        }
        None => {
            let snap = settled_baseline(run)?;
            let newest = newest_message(&snap.rows);
            let rows = RowCounts::of(&snap.rows);
            let mut value = snap.to_value();
            value["newest"] = json!(newest);
            run.carry_set("base", value);
            run.carry_set("rows_base", rows.to_value());
            run.carry_set("started_ms", json!(now_ms()));
            (snap, newest, rows)
        }
    };
    let started = run.carried_u64("started_ms").unwrap_or_else(now_ms);
    run.see("leader_at_start", json!(leads(&info)));
    if leads(&info) {
        run.note("Kourier already leads at the start: the six are received by the leader (its counts stand inside S5b)");
    }
    run.personal(
        "PERSONAL phone, in this order: (1) in the existing 1:1 chat with the test ID send a text that QUOTES an older message; (2) send a text with *bold* in it; (3) edit that bold text; (4) react with an emoji to any message of that chat; (5) delete one of your own texts in it; (6) in the group send a text that MENTIONS the test ID. Then wait about 20 seconds and answer here. Kourier only receives and counts; the step opens no chat",
    );
    run.waiting("for the six, then for your answer");
    let sent_all = if run.has_phone() {
        // No question: the step waits (bounded) until what Kourier counts says the six are in (two
        // new 1:1 messages, the quote and the bold text, and one in the group, the mention; an edit,
        // a reaction and a delete show in no count) and then stays quiet for a while counted from the
        // LAST change, or until the bound. What came is recorded, not judged.
        let quiet = run.timing.settle * 6;
        let bound = run.wait_for(240).min(run.left());
        let mut last = Snap::take(run.client)?.list();
        let mut since = std::time::Instant::now();
        let end = std::time::Instant::now() + bound;
        let mut all_in;
        loop {
            run.halted_now()?;
            std::thread::sleep(run.timing.poll);
            let snap = Snap::take(run.client)?;
            let now = snap.list();
            if now != last {
                last = now;
                since = std::time::Instant::now();
            }
            all_in = arrived(&arrivals(
                &base.chats,
                base.open,
                base_newest,
                &snap,
                "direct",
            )) >= 2
                && arrived(&arrivals(
                    &base.chats,
                    base.open,
                    base_newest,
                    &snap,
                    "group",
                )) >= 1;
            if (all_in && since.elapsed() >= quiet) || std::time::Instant::now() >= end {
                break;
            }
        }
        run.note(if all_in {
            "the three new messages the six acts make (two in the 1:1 chat, one in the group) were counted, then it stayed quiet"
        } else {
            "the bound ended the wait before the three new messages the six acts make were all counted"
        });
        run.record_origin("kinds-sent", crate::threema_run::tphone::Origin::NotLooked);
        run.note("whether all six were sent was not asked: what Kourier counted is recorded below");
        true
    } else {
        run.asked_once("kinds-sent", true)?
    };
    std::thread::sleep(run.timing.settle);
    let fin = Snap::take(run.client)?;
    let direct = arrivals(&base.chats, base.open, base_newest, &fin, "direct");
    let group = arrivals(&base.chats, base.open, base_newest, &fin, "group");
    if !direct.fresh.is_empty() || !group.fresh.is_empty() {
        bail!(
            "a message opened a conversation that did not exist before ({} new): Kourier starts no chat, so the sender may not be the one the step asked for or the same person is filed twice. Stop and tell the user",
            direct.fresh.len() + group.fresh.len()
        );
    }
    let (new_direct, new_group) = (arrived(&direct), arrived(&group));
    let (moved_direct, moved_group) = (
        activity_moved(&base, &fin, "direct"),
        activity_moved(&base, &fin, "group"),
    );
    run.see("new_direct", json!(new_direct));
    run.see("new_group", json!(new_group));
    run.see("moved_direct", json!(moved_direct));
    run.see("moved_group", json!(moved_group));
    run.see(
        "sent_all",
        if run.has_phone() {
            Value::Null
        } else {
            json!(sent_all)
        },
    );
    run.see("watch_s", json!(secs_since(started)));
    run.note(format!(
        "the watch lasted {} s from the baseline to the answer",
        secs_since(started)
    ));
    run.note(format!(
        "new messages: {new_direct} in 1:1 chats, {new_group} in groups; chats whose activity moved: {moved_direct} 1:1, {moved_group} group (counts only, no verdict per kind: the six acts were a quote, a mention in the group, bold text, an edit, a reaction and a delete)"
    ));
    if fin.open != 0 && fin.open == base.open {
        let delta = RowCounts::of(&fin.rows).minus(rows_base);
        run.see("rows", delta.to_value());
        run.note(format!(
            "in the chat HE had open (his act; the step opened none): quoted rows {:+}, edited rows {:+}, deleted rows (tombstones) {:+}, reactions {:+}",
            delta.quoted, delta.edited, delta.deleted, delta.reactions
        ));
    } else {
        run.see("rows", Value::Null);
        run.note("no chat was open at the start and at the end: edits, reactions and deletes are not counted from rows (the step opens none: opening an unread chat marks it read); they show only as the list's activity moving");
    }
    if !sent_all {
        run.note("he said not all six were sent: the counts above are of what he did send");
    }
    // The chats that moved, for `reconnect`.
    let moved: Vec<Value> = fin
        .chats
        .iter()
        .filter(|c| !c.is_self && matches!(c.kind.as_str(), "direct" | "group"))
        .filter(|c| {
            base.chats
                .iter()
                .find(|b| b.id == c.id)
                .is_none_or(|b| (b.unread, b.last_activity) != (c.unread, c.last_activity))
        })
        .map(chat_fact)
        .collect();
    carry_chats(run, moved);
    run.asked_the_warning_once()?;
    if new_direct + new_group == 0 && fin.open == 0 {
        bail!(
            "nothing new arrived in any chat while the personal phone sent: the six were not received (or he sent none). Look at the TEST phone and the window; the step counts, it halts nothing"
        );
    }
    record_chats(run);
    Ok(())
}

// ---- Step `phone-sleep` ----------------------------------------------------------------------------------

/// Step `phone-sleep`: the TEST phone locked for 10 minutes; at minute 5 the PERSONAL phone sends ONE
/// text; PASS is that text once in Kourier and once on the TEST phone after the unlock. RECORDS whether
/// D1 came (Kourier took the lead in the sleep: yes or no, and how soon) and the push mode he gives.
/// Not observing the lead is a finding of the report, not a failure of the step.
pub(super) fn step_phone_sleep(run: &mut StepRun<'_>) -> Result<()> {
    let info = ready(run)?;
    if run.carried_u64("locked_ms").is_none() {
        run.see("leader_at_start", json!(leads(&info)));
        run.carry_set("d1_at_start", json!(d1_count(&info)));
        run.test_phone_instruction(
            "TEST phone: LOCK it now (screen off, Threema not in front) and leave it alone; the PERSONAL phone stays face down until the step tells you. Then answer that the TEST phone is locked",
        );
        if !run.asked_once("phone-locked", false)? {
            bail!(
                "you said the TEST phone is not locked: nothing was started; run `step phone-sleep` again when it is locked"
            );
        }
        run.carry_set("locked_ms", json!(now_ms()));
    }
    let locked = run.carried_u64("locked_ms").unwrap_or_default();
    let text_at = locked + ms_of(run.timing.sleep_text_at);
    let end_at = locked + ms_of(run.timing.sleep_total);
    run.waiting(&format!(
        "the TEST phone sleeps: the personal phone's text is asked at minute {} and the sleep ends at minute {}",
        run.timing.sleep_text_at.as_secs() / 60,
        run.timing.sleep_total.as_secs() / 60
    ));
    if !wait_until(run, text_at)? {
        return Err(run.pause(format!(
            "the TEST phone has slept {} s of {}: run `step phone-sleep` again, it goes on from the clock of the lock",
            secs_since(locked),
            run.timing.sleep_total.as_secs()
        )));
    }
    if !run.carried_bool("text_done") {
        let instruction = run.with_text(
            "PERSONAL phone: send ONE short text in the existing 1:1 chat with the test ID now (the TEST phone stays locked)",
            "sleep",
        );
        let (_, facts) = one_arrives_in(run, "direct", &instruction, Some("text"))?;
        carry_chats(run, facts);
        run.carry_set("text_done", json!(true));
        run.see("text_once", json!(true));
        run.note("the text came into Kourier once");
    }
    run.waiting("the rest of the sleep");
    if !wait_until(run, end_at)? {
        return Err(run.pause(format!(
            "the TEST phone has slept {} s of {}: run `step phone-sleep` again, it goes on from the clock of the lock",
            secs_since(locked),
            run.timing.sleep_total.as_secs()
        )));
    }
    // THE LEADER RULE in sleep: did Kourier take the lead?
    let info = run.client.info()?;
    let before = run.carried_u64("d1_at_start").unwrap_or(0);
    let was_leader = run.obs.get("leader_at_start") == Some(&json!(true));
    if d1_count(&info) > before {
        let first = d1_times(run)?.into_iter().filter(|t| *t >= locked).min();
        let after = first.map(|t| t.saturating_sub(locked) / 1000);
        run.see("leader_in_sleep", json!(true));
        run.see("d1_after_lock_s", json!(after));
        run.note(format!(
            "leader observed in sleep: YES (D1 {} s after the lock): Kourier took the lead while the TEST phone slept",
            after.map_or_else(|| "? ".to_owned(), |s| s.to_string())
        ));
    } else if was_leader {
        run.see("leader_in_sleep", json!(false));
        run.note("leader observed in sleep: Kourier led already before the step (no second D1 can come on its socket), so the sleep did not decide it");
    } else {
        run.see("leader_in_sleep", json!(false));
        run.note(format!(
            "leader observed in sleep: NO (no D1 in {} s): the phone kept the lead, or its push mode kept it connected (a finding for the leader rule, not a failure)",
            run.timing.sleep_total.as_secs()
        ));
    }
    run.see("leader_at_end", json!(leads(&info)));
    run.test_phone_instruction("unlock the TEST phone now and open Threema, the chat with the personal ID. Then answer here");
    if !run.asked_once("tp-shows-once", true)? {
        return Err(lost_or_doubled(run, "the text"));
    }
    run.see("tp_shows_once", json!(true));
    run.note("the TEST phone shows the text exactly once after the unlock");
    // The push mode is decided ONCE per step: a call that resumes keeps what an earlier call recorded
    // (`null` too, "not looked at") with its origin, and asks only when it is absent (RT1: a carried
    // "not looked at" came back as an answer he gave).
    if !run.obs.contains_key("push_own") {
        let push_own = run.asked_once("push-own", true)?;
        let origin = run
            .origins
            .iter()
            .find(|o| o["question"] == json!("push-own"))
            .and_then(|o| o["origin"].as_str().map(str::to_owned))
            .unwrap_or_else(|| {
                // A run without a test phone records no origin: the answer is typed in his terminal.
                let origin = if run.has_phone() {
                    crate::threema_run::tphone::Origin::NotLooked
                } else {
                    crate::threema_run::tphone::Origin::Typed
                };
                origin.word().to_owned()
            });
        run.see("push_origin", json!(origin));
        if origin == crate::threema_run::tphone::Origin::NotLooked.word() {
            run.see("push_own", Value::Null);
            run.note("push mode: not looked at (the notification setting could not be read on the TEST phone)");
        } else {
            run.see("push_own", json!(push_own));
            run.note(format!(
                "push mode ({origin}): {}",
                if push_own {
                    "Threema's own push service"
                } else {
                    "the system's push"
                }
            ));
        }
    }
    run.asked_the_warning_once()?;
    record_chats(run);
    Ok(())
}

// ---- Step `flight-mode` -----------------------------------------------------------------------------------

/// The last count line of a leader socket (`out_unblock` >= 1) that holds at least `in_min` incoming
/// messages, among the lines since `from`.
fn leader_count_line(run: &StepRun<'_>, from: u64, in_min: u64) -> Result<Option<String>> {
    let (lines, _) = run.client.lines(from)?;
    Ok(counts_of(&lines)
        .into_iter()
        .rev()
        .find(|c| c.get("out_unblock") >= 1 && c.get("in_csp_message") >= in_min)
        .map(|c| {
            format!(
                "{} out_reflect_inbound={} in_message_ack_out={} in_csp_message={}",
                counts_note(c),
                c.get("out_reflect_inbound"),
                c.get("out_message_ack"),
                c.get("in_csp_message")
            )
        }))
}

/// Step `flight-mode`: THE LEADER TEST. The role of the live socket is read FIRST (critic M5): when it
/// leads already (the phone's sleep made it so) the wait is skipped; otherwise the TEST phone goes to
/// flight mode and Kourier gets up to 10 minutes to take the lead (D1; none is a FAIL "leader not
/// observed", no halt). Then the PERSONAL phone sends one text 1:1 and one in the group, each seen once
/// by Kourier (S7); the leader's count line is read; the personal phone is asked whether the 1:1 text
/// shows as delivered; flight mode off and both texts once on the TEST phone; two minutes later one more
/// text, once in Kourier and once on the phone; the forward-security line (b) is recorded; S9 (a) last.
pub(super) fn step_flight_mode(run: &mut StepRun<'_>) -> Result<()> {
    let result = flight_mode(run);
    // The cut stands only while the step goes on. A step that ENDS (a failure, a halt, a lost text)
    // without a resume takes it off before it reports; a pause (the next call goes on) leaves it, and
    // the phone's own timer ends it in the worst case.
    if result.is_err() && run.resume.is_none() {
        end_the_cut(run);
    }
    result
}

/// Removes a cut this step made, when one is on record and not removed yet; says so in the notes.
fn end_the_cut(run: &mut StepRun<'_>) {
    let Some(cut) = run.carried_cut() else {
        return;
    };
    if run.carried_bool("cut_removed") {
        return;
    }
    let Some(mut phone) = run.tp.take() else {
        return;
    };
    let gone = crate::threema_run::tphone::uncut_threema(phone.as_mut(), &cut);
    run.tp = Some(phone);
    match gone {
        Ok(_) => {
            run.carry_set("cut_removed", json!(true));
            run.note("the step ended: the cut was removed from both tables and checked gone");
        }
        Err(_) => run.note(
            "the step ended and the cut could NOT be verified gone: remove the DROP rules for Threema's uid by hand (iptables/ip6tables -w -D OUTPUT); the phone's own timer removes them within the hour",
        ),
    }
}

fn flight_mode(run: &mut StepRun<'_>) -> Result<()> {
    let info = ready(run)?;
    // THE ROLE FIRST (critic M5).
    if !run.carried_bool("role_seen") {
        let lead = leads(&info);
        run.say("WRITES: as the leader Kourier sends the unblock once, per incoming message a Reflect and a message-ack, per 1:1 text the delivery receipt (a Reflect and ONE box), and per message wrapped in forward security one Reject box; the judge holds the counts to S5b's bounds and halts on a burst".to_owned());
        run.see("leader_before", json!(lead));
        run.carry_set("d1_at_start", json!(d1_count(&info)));
        run.carry_set("lines_from", json!(info["next_line"].as_u64().unwrap_or(0)));
        run.carry_set("events_from", json!(info["next_seq"].as_u64().unwrap_or(0)));
        run.carry_set("role_seen", json!(true));
        if lead {
            let since = d1_times(run)?.into_iter().max().map(secs_since);
            run.note(format!(
                "Kourier already leads before flight mode (D1 seen{}): the wait for D1 is skipped and the texts are asked (critic M5)",
                since.map_or_else(String::new, |s| format!(" {s} s ago"))
            ));
            run.carry_set("promoted", json!(true));
        }
    }
    // H1: with a test phone the TEST phone's network is cut WHATEVER Kourier's role at the start (a
    // leader already: the texts are still sent to a phone that is offline); without one the old rule
    // stands (a leader already needs no flight mode).
    if (run.has_phone() || !run.carried_bool("promoted")) && run.carried_u64("on_ms").is_none() {
        {
            run.test_phone_instruction(
                "TEST phone: switch FLIGHT MODE on (every radio off, Wi-Fi too) with Threema still open; the PERSONAL phone stays face down. Then answer here",
            );
            if !run.asked_once("flight-on", false)? {
                bail!(
                    "you said the TEST phone is not in flight mode: nothing was started; run `step flight-mode` again when it is"
                );
            }
            run.carry_set("on_ms", json!(now_ms()));
        }
    }
    run.ensure_cut()?;
    if !run.carried_bool("promoted") {
        let on = run.carried_u64("on_ms").unwrap_or_default();
        let limit = on + ms_of(run.timing.d1_wait);
        let at_start = run.carried_u64("d1_at_start").unwrap_or(0);
        run.waiting("for Kourier to take the lead (D1) while the TEST phone is offline");
        let wait = Duration::from_millis(limit.saturating_sub(now_ms()));
        let got = poll(wait.min(run.left()), run.timing.poll, || {
            run.halted_now()?;
            let info = run.client.info()?;
            Ok((leads(&info) || d1_count(&info) > at_start).then_some(()))
        })?;
        if got.is_none() {
            if now_ms() >= limit {
                run.see("promoted", json!(false));
                run.see("d1_after_on_s", Value::Null);
                bail!(
                    "leader not observed: Kourier did not take the lead in {} s with the TEST phone in flight mode. Turn flight mode OFF on the TEST phone now (no halt; the texts of this step were not asked and nothing was sent)",
                    run.timing.d1_wait.as_secs()
                );
            }
            return Err(run.pause(format!(
                "no D1 yet, {} s of {} since flight mode went on: run `step flight-mode` again, it goes on from the clock",
                secs_since(on),
                run.timing.d1_wait.as_secs()
            )));
        }
        let after = secs_since(on);
        run.see("promoted", json!(true));
        run.see("d1_after_on_s", json!(after));
        run.carry_set("promoted", json!(true));
        run.note(format!(
            "Kourier took the lead {after} s after flight mode went on (D1)"
        ));
        // The promotion publishes Syncing again (TH6 C4): online, syncing, online, timed.
        let seen_at = std::time::Instant::now();
        let back = poll(
            run.timing.online_wait.min(run.left()),
            run.timing.poll,
            || {
                run.halted_now()?;
                Ok((run.client.info()?["online"] == json!(true)).then_some(()))
            },
        )?;
        let from = run.carried_u64("events_from").unwrap_or(0);
        let (events, _) = run.client.events(from)?;
        let conns: Vec<String> = events
            .iter()
            .filter(|e| e["event"] == "account_state")
            .filter_map(|e| e["data"]["conn"].as_str())
            .map(word)
            .collect();
        run.note(format!(
            "account states since the step began: {}; online {} {} s after the promotion was seen",
            if conns.is_empty() {
                "(none)".to_owned()
            } else {
                conns.join(" > ")
            },
            if back.is_some() { "again" } else { "NOT again" },
            seen_at.elapsed().as_secs()
        ));
    }
    // The texts.
    if !run.carried_bool("text_1to1_done") {
        let instruction = run.with_text(
            "PERSONAL phone: send ONE short text in the existing 1:1 chat with the test ID now (the TEST phone is offline)",
            "flight-direct",
        );
        let (_, facts) = one_arrives_in(run, "direct", &instruction, Some("text-1to1"))?;
        carry_chats(run, facts);
        run.carry_set("text_1to1_done", json!(true));
        run.note("the 1:1 text came into Kourier once");
        run.ensure_cut()?;
    }
    if !run.carried_bool("text_group_done") {
        let instruction = run.with_text(
            "PERSONAL phone: send ONE short text in the group both phones are in now (the TEST phone is still offline)",
            "flight-group",
        );
        let (_, facts) = one_arrives_in(run, "group", &instruction, Some("text-group"))?;
        carry_chats(run, facts);
        run.carry_set("text_group_done", json!(true));
        run.note("the group text came into Kourier once");
        run.ensure_cut()?;
    }
    // The leader's count line with both texts in it.
    if !run.carried_bool("counts_done") {
        let from = run.carried_u64("lines_from").unwrap_or(0);
        let line = poll(
            run.timing.count_wait.min(run.left()),
            run.timing.poll,
            || {
                run.halted_now()?;
                leader_count_line(run, from, 2)
            },
        )?;
        match line {
            Some(l) => {
                run.see("leader_counts", json!(true));
                run.note(format!(
                    "the leader's count line: {l} (the judge holds it to S5b's bounds)"
                ));
            }
            None => {
                run.see("leader_counts", json!(false));
                run.note(format!(
                    "the leader's frame counts: not measured (no leader count line with both texts in {} s; the line comes every 60 s while the socket stands)",
                    run.timing.count_wait.as_secs()
                ));
            }
        }
        run.carry_set("counts_done", json!(true));
    }
    let delivered = run.asked_once("pp-delivered", true)?;
    run.see("pp_delivered", json!(delivered));
    run.note(format!(
        "the PERSONAL phone shows the 1:1 text as delivered: {}",
        if delivered { "yes" } else { "NO" }
    ));
    // Flight mode off: both texts once on the TEST phone.
    if run.carried_u64("off_ms").is_none() {
        run.test_phone_instruction(
            "TEST phone: switch FLIGHT MODE off and open Threema. Then answer here",
        );
        if !run.asked_once("flight-off-both", false)? {
            return Err(lost_or_doubled(run, "both texts"));
        }
        run.carry_set("off_ms", json!(now_ms()));
        run.see("both_once_after_off", json!(true));
        run.note("the TEST phone shows both texts once each after flight mode went off");
    }
    // Two minutes later: one more text (Kourier still leads, the phone follows).
    let off = run.carried_u64("off_ms").unwrap_or_default();
    run.waiting("two minutes with the TEST phone back online, then one more text");
    if !wait_until(run, off + ms_of(run.timing.later_text_after))? {
        return Err(run.pause(format!(
            "{} s of {} since flight mode went off: run `step flight-mode` again, it goes on from the clock",
            secs_since(off),
            run.timing.later_text_after.as_secs()
        )));
    }
    if !run.carried_bool("text_later_done") {
        let instruction = run.with_text(
            "PERSONAL phone: send ONE more short text in the 1:1 chat now (Kourier still leads, the TEST phone follows)",
            "later",
        );
        let (_, facts) = one_arrives_in(run, "direct", &instruction, Some("text-later"))?;
        carry_chats(run, facts);
        run.carry_set("text_later_done", json!(true));
        run.note("the later text came into Kourier once");
        run.ensure_cut()?;
    }
    run.test_phone_instruction(
        "TEST phone: Threema open on the chat with the personal ID. Then answer here",
    );
    if !run.asked_once("tp-shows-later", true)? {
        return Err(lost_or_doubled(run, "the later text"));
    }
    run.see("tp_shows_later", json!(true));
    run.note("the TEST phone shows the later text exactly once");
    // The role at the end, and whether the socket lived (no demotion, no new socket).
    let info = run.client.info()?;
    let from = run.carried_u64("lines_from").unwrap_or(0);
    let (lines, _) = run.client.lines(from)?;
    let ended = lines.iter().any(|l| {
        matches!(&l.parsed.body, Body::Closed { .. })
            || matches!(&l.parsed.body, Body::Count(c) if c.end)
    });
    run.see("leader_at_end", json!(leads(&info)));
    run.see("socket_ended", json!(ended));
    run.note(format!(
        "Kourier {} at the end; the socket {} during the step",
        if leads(&info) {
            "still leads"
        } else {
            "does not lead"
        },
        if ended { "ENDED" } else { "lived" }
    ));
    forward_security_line(run)?;
    run.asked_the_warning_once()?;
    if !delivered {
        bail!(
            "the PERSONAL phone did not show the 1:1 text as delivered although Kourier led: a finding (the receipt the leader sends); nothing is retried"
        );
    }
    record_chats(run);
    Ok(())
}

// ---- The two ends -----------------------------------------------------------------------------------------

/// The end's own question (`end-go`) and the removal told to the judge, BEFORE he acts. A no ends
/// the step with nothing changed.
fn begin_the_end(run: &mut StepRun<'_>, what: &str) -> Result<Value> {
    let info = ready(run)?;
    if run.has_phone() {
        // CONSENT (D4): the lead passes `--user-said-yes` only after the user's word in chat.
        if !run.args.user_said_yes {
            bail!(
                "this step ENDS the link and needs the user's word: the lead runs it again with --user-said-yes only after the user said yes in chat. Nothing was changed and nothing was asked of the judge"
            );
        }
        run.record_origin("end-go", crate::threema_run::tphone::Origin::Relayed);
    } else if !run.asked_once("end-go", false)? {
        bail!(
            "you said you do not want the link ended now: nothing was changed and nothing was asked of the judge"
        );
    }
    run.client.removal(what)?;
    run.see("removal_asked", json!(what));
    Ok(info)
}

/// Step `removed-on-phone` (END A, only on his word): he removes the Kourier entry on the TEST phone
/// while the run lives; the mediator closes the socket with 4113 and the account reads `logged_out`
/// (RemovedOnPhone; the account's words may be generic). `wipe` follows on his word, not here.
pub(super) fn step_removed_on_phone(run: &mut StepRun<'_>) -> Result<()> {
    let info = begin_the_end(run, "removed-on-phone")?;
    let result = removed_on_phone_after_the_ask(run, &info);
    give_the_removal_back(run, "removed-on-phone", &result);
    result
}

/// An end step that fails after the judge was told a removal is asked gives it back (LOW-9): from then
/// on a 4113, a `logged_out` and a drop frame halt again, so a flag set for a removal that never came
/// does not excuse them for the rest of the start. Best effort: a supervisor that is gone has no flag.
fn give_the_removal_back(run: &mut StepRun<'_>, what: &str, result: &Result<()>) {
    if result.is_err() && run.client.removal_over(what).is_ok() {
        run.see("removal_given_back", json!(true));
    }
}

fn removed_on_phone_after_the_ask(run: &mut StepRun<'_>, info: &Value) -> Result<()> {
    let from = info["next_line"].as_u64().unwrap_or(0);
    run.personal(
        "TEST phone: Threema, the linked devices list (of the TEST phone), and REMOVE the entry named Kourier, now",
    );
    run.waiting("for the mediator to close the socket (4113) and the account to read logged out");
    let look = |run: &StepRun<'_>| -> Result<(bool, bool, Value)> {
        let (lines, _) = run.client.lines(from)?;
        let close = lines
            .iter()
            .any(|l| matches!(l.parsed.body, Body::Closed { code: 4113 }));
        let info = run.client.info()?;
        let out = info["account"]["state"] == json!("logged_out")
            || info["account"]["conn"] == json!("logged_out");
        Ok((close, out, info["account"].clone()))
    };
    let got = poll(
        run.timing.removal_wait.min(run.left()),
        run.timing.poll,
        || {
            run.halted_now()?;
            let (close, out, account) = look(run)?;
            Ok((close && out).then_some(account))
        },
    )?;
    let (close, out, account) = look(run)?;
    run.see("close_4113", json!(close));
    run.see("logged_out", json!(out));
    run.note(format!(
        "close 4113 seen: {}; account logged out: {} (account words: state {}, conn {})",
        if close { "yes" } else { "NO" },
        if out { "yes" } else { "NO" },
        word(account["state"].as_str().unwrap_or("none")),
        word(account["conn"].as_str().unwrap_or("none")),
    ));
    if got.is_none() {
        bail!(
            "the removal on the TEST phone did not show in {} s (4113 {}, logged out {}): read threema-run/app.log; nothing is retried",
            run.timing.removal_wait.as_secs(),
            if close { "seen" } else { "NOT seen" },
            if out { "yes" } else { "no" }
        );
    }
    run.terminal(
        "next: `cargo xtask threema-run stop`, then `wipe --yes-delete-the-login` only on his word (the TEST phone's list is clean: you removed the entry there)",
    );
    run.resume = None;
    Ok(())
}

/// Step `unlink` (END B, only on his word): he removes the account in Kourier's own Settings; Kourier
/// drops the device on the mediator (RP-b: the ack, or 4113/4114 after the ask), the account is gone,
/// and the TEST phone's list is clean. `wipe` follows on his word, not here.
pub(super) fn step_unlink(run: &mut StepRun<'_>) -> Result<()> {
    let info = begin_the_end(run, "unlink")?;
    let result = unlink_after_the_ask(run, &info);
    give_the_removal_back(run, "unlink", &result);
    result
}

fn unlink_after_the_ask(run: &mut StepRun<'_>, info: &Value) -> Result<()> {
    let acked_before = info["lines"]["D4_DROP_ACKED"].as_u64().unwrap_or(0);
    run.window(
        "in the Kourier window: Settings, Accounts, the Threema account, Remove, and confirm. The TEST phone is not touched",
    );
    run.waiting("for the drop of the device and for the account to go");
    let look = |run: &StepRun<'_>| -> Result<(Option<String>, bool)> {
        let info = run.client.info()?;
        let drop = info["drop"].as_str().map(str::to_owned);
        let gone = run
            .client
            .state()?
            .get("accounts")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty);
        Ok((drop, gone))
    };
    let got = poll(
        run.timing.removal_wait.min(run.left()),
        run.timing.poll,
        || {
            run.halted_now()?;
            let (drop, gone) = look(run)?;
            // The drop's outcome is final once it is told: a NotReached will not turn into Removed.
            Ok(
                (drop.as_deref() == Some("not-reached") || (drop.is_some() && gone))
                    .then_some((drop, gone)),
            )
        },
    )?;
    let (drop, gone) = got.map_or_else(|| look(run), Ok)?;
    let acked = run.client.info()?["lines"]["D4_DROP_ACKED"]
        .as_u64()
        .unwrap_or(0)
        .saturating_sub(acked_before);
    run.see("drop", json!(drop));
    run.see("account_gone", json!(gone));
    run.see("d4_acked", json!(acked));
    run.note(format!(
        "the device's drop on the mediator: {} (D4 'acked the own device's drop' lines: {acked}); the account is gone from the window: {}",
        drop.as_deref().map_or("no outcome", |d| match d {
            "removed" => "Removed",
            "not-reached" => "NOT reached",
            _ => "unknown",
        }),
        if gone { "yes" } else { "NO" }
    ));
    if drop.as_deref() != Some("removed") || !gone {
        bail!(
            "the removal did not finish as expected (drop: {}, account gone: {}): check the TEST phone's linked devices list and remove any Kourier entry there by hand; nothing is retried",
            drop.as_deref().unwrap_or("no outcome"),
            if gone { "yes" } else { "no" }
        );
    }
    run.test_phone_instruction(
        "TEST phone: open Threema's linked devices list again. Then answer here",
    );
    let clean = run.asked_once("tp-list-clean", true)?;
    run.see("list_clean", json!(clean));
    run.note(format!(
        "the TEST phone's linked devices list is clean: {}",
        if clean { "yes" } else { "NO" }
    ));
    if !clean {
        bail!(
            "the TEST phone's list still shows a Kourier entry although Kourier dropped the device: a finding (the drop's effect on the phone); remove the entry there by hand"
        );
    }
    run.terminal(
        "next: `cargo xtask threema-run stop`, then `wipe --yes-delete-the-login` only on his word",
    );
    run.resume = None;
    Ok(())
}

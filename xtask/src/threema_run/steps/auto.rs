//! The automated path of the steps (`docs/threema-run.md` "With a test phone"): a run started with
//! `--test-phone` answers the questions about the TEST phone by CHECKS the step makes itself on the
//! phone, and does the TEST phone's acts itself. Every answer names its ORIGIN
//! ([`Origin`]); a screen that cannot be read is no answer (the step pauses with "run `step <name>`
//! again" and KEEPS its carry; the run is not halted). Texts of the phone are never printed, logged
//! or written: the checks in `tphone` compare in memory, and the words here are fixed sentences.

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use super::{Duration, StepRun, poll};
use crate::threema_run::tphone::{self, ChatKind, Cut, Origin, Verdict};

/// The firewall cut's phone-side safety timer. A call that finds less than [`REARM_BELOW_MS`] of it
/// left puts a new tagged pair with a fresh timer in front of the old one, so the cut always covers
/// the step's remaining time; the old timer then deletes nothing (it deletes only its own tag).
pub(super) const CUT_TIMER_SECS: u32 = 3600;
const REARM_BELOW_MS: u64 = 30 * 60 * 1000;

/// A verdict as a yes or no, or the step's own pause.
fn yes(v: Verdict, step: &str) -> Result<bool> {
    match v {
        Verdict::Yes => Ok(true),
        Verdict::No => Ok(false),
        Verdict::Unknown(why) => bail!("{why}: run `step {step}` again (nothing was decided)"),
        Verdict::Ask => bail!("this check needs a person: run `step {step}` again"),
    }
}

impl StepRun<'_> {
    /// The step has a TEST phone.
    pub(super) fn has_phone(&self) -> bool {
        self.tp.is_some()
    }

    /// Writes down where an answer came from: a note, and the record's `facts.origins`.
    pub(super) fn record_origin(&mut self, question: &str, origin: Origin) {
        self.note(format!("answer to \"{question}\": {}", origin.word()));
        self.origins.retain(|o| o["question"] != json!(question));
        self.origins
            .push(json!({"question": question, "origin": origin.word()}));
    }

    /// An act of the TEST phone, told as a FIXED sentence (never a node's text).
    pub(super) fn act(&mut self, fixed: &str) {
        self.say(format!("ACT: {fixed}"));
        self.note(format!("act on the TEST phone: {fixed}"));
    }

    /// What the user does on the PERSONAL phone: `DO-USER:` in a run with a test phone, else the
    /// old `PHONE:` line.
    pub(super) fn personal(&mut self, what: &str) {
        if self.has_phone() {
            self.say(format!("DO-USER: {what}"));
        } else {
            self.phone(what);
        }
    }

    /// What the user does on the TEST phone when the step does not do it itself: told in a run
    /// without a test phone; in a run with one the step's check performs the act (and says so).
    pub(super) fn test_phone_instruction(&mut self, what: &str) {
        if !self.has_phone() {
            self.phone(what);
        }
    }

    /// The text of `slot` for THIS attempt of the step: `kourier-rt1-<slot>-<six random letters>`,
    /// chosen at the first call and kept in the carry (so a resumed call counts the same word). An
    /// older attempt's copy can never match it.
    pub(super) fn marker(&mut self, slot: &str) -> String {
        let key = format!("marker_{slot}");
        if let Some(m) = self.carry.get(&key).and_then(Value::as_str) {
            return m.to_owned();
        }
        let m = tphone::marker_for(slot, &tphone::random_letters());
        self.carry_set(&key, json!(m));
        self.register_marker(&m);
        m
    }

    /// Writes the marker of this step into `obs.markers` (step name, marker, how often the step recorded
    /// it in a chat: 0), so a later step knows it is a text of this run (RT1: a mix-up of markers).
    fn register_marker(&mut self, marker: &str) {
        let mut all = self.obs_markers();
        if !all.iter().any(|m| m["marker"] == json!(marker)) {
            all.push(json!({"step": self.step, "marker": marker, "seen": 0}));
            self.see("markers", Value::Array(all));
        }
    }

    fn obs_markers(&self) -> Vec<Value> {
        self.obs
            .get("markers")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    fn set_marker_seen(&mut self, marker: &str, seen: usize) {
        let mut all = self.obs_markers();
        match all.iter_mut().find(|m| m["marker"] == json!(marker)) {
            Some(m) => m["seen"] = json!(seen),
            None => all.push(json!({"step": self.step, "marker": marker, "seen": seen})),
        }
        self.see("markers", Value::Array(all));
    }

    /// The markers of this run's steps (the records and this call's own), but `except`.
    fn other_markers(&self, except: &str) -> Vec<tphone::OtherMarker> {
        let mut all: Vec<Value> = Vec::new();
        let records = super::recorded_steps(self.dir);
        let lists = records
            .iter()
            .map(|r| {
                r["facts"]["obs"]["markers"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .chain(std::iter::once(self.obs_markers()));
        for list in lists {
            for m in list {
                match all.iter_mut().find(|k| k["marker"] == m["marker"]) {
                    Some(k) => *k = m,
                    None => all.push(m),
                }
            }
        }
        all.iter()
            .filter(|m| m["marker"].as_str().is_some_and(|k| k != except))
            .filter_map(|m| {
                Some(tphone::OtherMarker {
                    step: m["step"].as_str()?.to_owned(),
                    marker: m["marker"].as_str()?.to_owned(),
                    seen: usize::try_from(m["seen"].as_u64().unwrap_or(0)).unwrap_or(0),
                })
            })
            .collect()
    }

    /// [`tphone::chat_shows_once_among`] with the markers of this run's other steps: a marker that shows
    /// once is recorded as seen once; a MIX-UP (the own marker missing, another step's marker shows
    /// more often than recorded) is no `No`: the note names that step, its count is taken as recorded
    /// (so the next call does not meet the same extra text again) and nothing is decided.
    pub(super) fn shows_once(
        &mut self,
        phone: &mut dyn crate::test_phone::Phone,
        kind: ChatKind,
        marker: &str,
    ) -> Verdict {
        self.register_marker(marker);
        let others = self.other_markers(marker);
        let shown = tphone::chat_shows_once_among(phone, kind, marker, &others);
        if shown.verdict == Verdict::Yes {
            self.set_marker_seen(marker, 1);
        }
        if let Some((i, now)) = shown.mixup {
            let step = others[i].step.clone();
            self.note(format!(
                "not decided: a marker of another step was sent (a mix-up): the marker of step `{step}` shows more often than that step recorded it; run the step again"
            ));
            self.set_marker_seen(&others[i].marker, now);
        }
        shown.verdict
    }

    /// The text the personal phone is asked to send: in a run with a test phone the step's own
    /// unique word for this attempt, which it counts on the TEST phone.
    pub(super) fn with_text(&mut self, base: &str, slot: &str) -> String {
        if self.has_phone() {
            let m = self.marker(slot);
            // The marker stands on a line of its own: easy to copy on a phone (RT1).
            format!("{base}. The text must be exactly:\n{m}")
        } else {
            base.to_owned()
        }
    }

    fn host(&self) -> Result<String> {
        tphone::host_name().ok_or_else(|| {
            anyhow!(
                "this computer's host name cannot be read, so no list row can be called this PC's: run `step {}` again after fixing it",
                self.step
            )
        })
    }

    /// The cut this step made, from the carry.
    pub(super) fn carried_cut(&self) -> Option<Cut> {
        let uid = u32::try_from(self.carried_u64("cut_uid")?).ok()?;
        let nonce = self.carry.get("cut_nonce")?.as_str()?.to_owned();
        Some(Cut { uid, nonce })
    }

    fn carry_cut(&mut self, cut: &Cut, deadline_ms: u64) {
        self.carry_set("cut_uid", json!(cut.uid));
        self.carry_set("cut_nonce", json!(cut.nonce));
        self.carry_set("cut_deadline_ms", json!(deadline_ms));
    }

    /// H2: the cut must still stand, and its phone-side timer must cover the step's remaining time.
    /// `Err` with the words of a FAIL when the cut ended early (the TEST phone was online during the
    /// step: no text can be judged); a pause when the phone cannot be read.
    pub(super) fn ensure_cut(&mut self) -> Result<()> {
        let Some(cut) = self.carried_cut() else {
            return Ok(());
        };
        if self.carried_bool("cut_removed") {
            return Ok(());
        }
        let Some(mut phone) = self.tp.take() else {
            return Ok(());
        };
        let stands = tphone::cut_stands(phone.as_mut(), &cut);
        let result = match stands {
            Err(_) => Err(None),
            Ok(false) => Err(Some(())),
            Ok(true) => {
                let deadline = self.carried_u64("cut_deadline_ms").unwrap_or(0);
                if deadline.saturating_sub(super::now_ms()) < REARM_BELOW_MS {
                    match tphone::rearm_cut(phone.as_mut(), &cut, CUT_TIMER_SECS) {
                        Ok(new) => Ok(Some(new)),
                        Err(_) => Err(None),
                    }
                } else {
                    Ok(None)
                }
            }
        };
        self.tp = Some(phone);
        match result {
            Ok(Some(new)) => {
                self.carry_cut(&new, super::now_ms() + u64::from(CUT_TIMER_SECS) * 1000);
                self.note("the cut was re-armed: a new tagged pair with a fresh timer, the old pair removed");
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(Some(())) => bail!(
                "the cut ENDED before the step did (the phone's timer or something else removed it): the TEST phone was online while the texts were sent, so nothing they show proves anything. The step FAILS; run `step {}` again from the start (it makes a new cut)",
                self.step
            ),
            Err(None) => Err(self.pause(format!(
                "the TEST phone's firewall could not be read: run `step {}` again, it goes on from here (the cut stays until the step or the phone's timer removes it)",
                self.step
            ))),
        }
    }

    /// The automated answer to `name`, if the question has a check and the step has a phone:
    /// `Ok(Some(answer))` is recorded with its origin; `Ok(None)` leaves the question to a person
    /// (typed or relayed); an error is a screen that could not be read.
    pub(super) fn auto_answer(&mut self, name: &str) -> Result<Option<bool>> {
        let Some(mut phone) = self.tp.take() else {
            return Ok(None);
        };
        let got = self.auto_with(phone.as_mut(), name);
        self.tp = Some(phone);
        match got? {
            Some((answer, origin)) => {
                self.record_origin(name, origin);
                Ok(Some(answer))
            }
            None => Ok(None),
        }
    }

    fn auto_with(
        &mut self,
        phone: &mut dyn crate::test_phone::Phone,
        name: &str,
    ) -> Result<Option<(bool, Origin)>> {
        let step = self.step;
        let on_phone = |b: bool| Some((b, Origin::TestPhone));
        Ok(match name {
            "list-shows-kourier" => {
                let host = self.host()?;
                let (verdict, list) = tphone::list_shows_kourier(phone, &host);
                if verdict == Verdict::Ask {
                    self.note("the TEST phone's linked-devices list shows no device row: an empty list proves nothing, a person is asked");
                    return Ok(None);
                }
                if let Some(l) = list {
                    self.note(format!(
                        "the TEST phone's linked-devices list: {} row(s), {} labelled Kourier with this host, {} unlabelled{}",
                        l.rows,
                        l.mine,
                        l.unlabelled,
                        if l.unlabelled > 0 {
                            " (a row is unlabelled)"
                        } else {
                            ""
                        }
                    ));
                }
                on_phone(yes(verdict, step)?)
            }
            "menu-path-as-runbook" => on_phone(yes(tphone::menu_path_as_runbook(phone), step)?),
            "phone-warning" => {
                // H4: the TEST phone's half is looked at here; the PERSONAL phone's half stays a
                // question for a person (ASK-USER), each half with its own origin. A warning on the
                // TEST phone answers at once (yes); otherwise the person's answer is the answer.
                let none = yes(tphone::no_warning_on_test_phone(phone), step)?;
                self.record_origin("phone-warning (TEST phone half)", Origin::TestPhone);
                if none { None } else { on_phone(true) }
            }
            "push-own" => match tphone::push_is_own(phone) {
                Verdict::Yes => on_phone(true),
                Verdict::No => on_phone(false),
                // M2: a fact the step only records; unreadable is "not looked at", never a pause.
                Verdict::Unknown(_) | Verdict::Ask => Some((false, Origin::NotLooked)),
            },
            "tp-list-clean" => {
                let host = self.host()?;
                let verdict = tphone::list_is_clean(phone, &host);
                if verdict == Verdict::Ask {
                    self.note("the TEST phone's linked-devices list shows no device row: an empty list proves nothing, a person is asked");
                    return Ok(None);
                }
                on_phone(yes(verdict, step)?)
            }
            "phone-locked" => {
                self.act("the TEST phone is put to sleep (screen off)");
                on_phone(yes(tphone::lock_test_phone(phone), step)?)
            }
            "tp-shows-once" => {
                self.act("the TEST phone is woken and Threema's 1:1 chat is opened to count the step's own text");
                let m = self.marker("sleep");
                on_phone(yes(self.shows_once(phone, ChatKind::Direct, &m), step)?)
            }
            "tp-shows-later" => {
                self.act("the TEST phone's 1:1 chat is opened to count the step's later text");
                let m = self.marker("later");
                on_phone(yes(self.shows_once(phone, ChatKind::Direct, &m), step)?)
            }
            "flight-on" => {
                self.act("Threema is cut off the network on the TEST phone (tagged firewall rules on its user id, with a timer on the phone that removes only them)");
                let cut = tphone::cut_threema(phone, CUT_TIMER_SECS).map_err(|_| {
                    anyhow!("the cut could not be made or verified on the TEST phone: no rule is trusted; run `step {step}` again (it removes any older rule of this tool first)")
                })?;
                self.carry_cut(&cut, super::now_ms() + u64::from(CUT_TIMER_SECS) * 1000);
                self.note(format!(
                    "the cut stands in both tables; the phone's own timer removes it after {} minutes",
                    CUT_TIMER_SECS / 60
                ));
                on_phone(true)
            }
            "flight-off-both" => {
                // The cut is taken off first (once: a pause after it keeps `cut_removed`); a cut
                // that had ended on its own FAILS the step. Then the two texts are counted.
                if let Some(cut) = self.carried_cut()
                    && !self.carried_bool("cut_removed")
                {
                    self.act("the cut is removed from both tables and checked gone");
                    let was = tphone::uncut_threema(phone, &cut).map_err(|_| {
                        anyhow!("a rule of the cut could not be removed or verified gone on the TEST phone: do it by hand (iptables/ip6tables -w -D OUTPUT for Threema's uid); run `step {step}` again")
                    })?;
                    self.carry_set("cut_removed", json!(true));
                    if !was {
                        bail!(
                            "the cut had ENDED before the step removed it: the TEST phone was online while the texts were sent, so they prove nothing. The step FAILS; run it again from the start"
                        );
                    }
                    self.note("the cut was in place until the step removed it");
                    self.see("cut_in_place_at_end", json!(true));
                    // The phone must be back on the network before its chats are read.
                    phone.sleep(Duration::from_secs(20));
                }
                self.act("the TEST phone's chats are opened to count the two texts");
                let m1 = self.marker("flight-direct");
                let m2 = self.marker("flight-group");
                let one = yes(self.shows_once(phone, ChatKind::Direct, &m1), step)?;
                let group = yes(self.shows_once(phone, ChatKind::Group, &m2), step)?;
                on_phone(one && group)
            }
            // B3: `pp-delivered` has NO automatic answer. The line contract carries no receipt for
            // THAT message (its box counters are cumulative per socket and also count an FS reject
            // and a creator's answer), so it is a question for a person.
            _ => None,
        })
    }

    /// The step's wait for an answer from a person: `ASK-USER:` and how the lead relays it.
    /// The line names its question in brackets and the `ANSWER:` line gives the exact command with that
    /// name: a relay is taken only for the question it names (RT1).
    pub(super) fn ask_user_lines(&mut self, name: &str, text: &str) {
        self.say(format!("ASK-USER [{name}]: {text}"));
        self.say(format!(
            "ANSWER: after the user's word in chat the lead runs `target/debug/xtask threema-run answer --relayed y|n --question {name} --profile {}` (or the user types `answer` in his own terminal)",
            self.dir.label()
        ));
    }

    /// Waits for a person's answer to `name` (already asked), records its origin. `None` when none
    /// came in time.
    pub(super) fn person_answer(&mut self, name: &str, bound: Duration) -> Result<Option<bool>> {
        let got = poll(bound, self.timing.poll, || {
            self.halted_now()?;
            self.client.answer_with_origin(name)
        })?;
        Ok(got.map(|(answer, origin)| {
            self.record_origin(name, origin);
            answer
        }))
    }
}

//! Forward in [`CoreSource`] (forward line F5, design D5): the dialog's plan, jobs that download
//! first, and the staged forward of a chat's composer (THE CONTRACT with the split-view line:
//! `stage_forward`, `forward_of`, `set_forward_look`, `drop_forward`, `send_staged_forward`).
//!
//! The dialog holds ONE plan at a time (`Dialog`); every Send starts a [`Job`] with its own
//! request id. A job asks a fresh plan, downloads the files the plan says are not here (each
//! asked ONCE), waits, asks the plan again whenever the source chat changes, asks "send without
//! them" for files the network no longer has, waits for the front end's pictures when the look is
//! `Picture` and a target is converted, then sends `UiCommand::Forward` and waits for
//! `UiEvent::ForwardEnded`. The front end takes what happened with
//! [`UiSource::take_forward`]. Nothing here logs a title, a name, a text or a path.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use kourier_core::facade::{
    ForwardLook, ForwardPlan, ForwardRefusal, Query, QueryData, RequestId, UiCommand,
};
use kourier_model::{AttachmentId, ConversationId, MessageId, Redacted, Text};

use super::{Changes, CoreSource, QueryToken, conversation_title};
use crate::conversations::{Hidden, network_name};
use crate::forward::{
    ForwardDialogView, ForwardTarget, ForwardUpdate, StagedForward, ViewFacts, ended_notice,
    going_ids, headers_for, nothing_sent_notice,
};
use crate::i18n::trc;
use crate::names::Names;

/// Query model id of the forward plan (after `MODEL_EXPORT`).
pub const MODEL_FORWARD: u32 = 23;

#[derive(Default)]
pub(super) struct Forward {
    next_generation: u64,
    /// What each plan query in flight is for.
    asked: HashMap<u64, Asked>,
    dialog: Option<Dialog>,
    jobs: Vec<Job>,
    staged: HashMap<ConversationId, Staged>,
    default_look: Option<ForwardLook>,
    updates: Vec<ForwardUpdate>,
}

#[derive(Clone, Copy)]
enum Asked {
    Dialog,
    Job(u64),
}

/// The open dialog.
struct Dialog {
    source: ConversationId,
    messages: Vec<MessageId>,
    /// The ticked chats, newest ticks.
    ticked: Vec<ConversationId>,
    /// The plan query in flight (its generation).
    in_flight: Option<u64>,
    /// The ticks changed while a query was in flight: ask again when it answers.
    pending: bool,
    /// The last answered view said the forward may go (valid while nothing is in flight).
    can_forward: bool,
    plan_known: bool,
}

/// A forward in progress.
struct Job {
    request: u64,
    source: ConversationId,
    messages: Vec<MessageId>,
    targets: Vec<ConversationId>,
    look: ForwardLook,
    comment: Option<Text>,
    /// The chat whose staged forward started it.
    staged_for: Option<ConversationId>,
    /// A plan query is in flight.
    planning: bool,
    /// An event came while it was: ask again.
    replan: bool,
    /// The files already asked of the core (never asked twice).
    asked: HashSet<AttachmentId>,
    /// The files the user agreed to leave out.
    without: Vec<AttachmentId>,
    /// The question is out for these lost files (the count, to ask again when it grows).
    asking: Option<Vec<AttachmentId>>,
    last_progress: Option<(u32, u32)>,
    /// The pictures were asked of the front end.
    render_asked: bool,
    pictures: Option<Vec<PathBuf>>,
    /// The newest plan.
    plan: Option<ForwardPlan>,
    /// `UiCommand::Forward` is out.
    sent: bool,
}

/// A forward staged into a chat's composer.
struct Staged {
    source: ConversationId,
    messages: Vec<MessageId>,
    look: ForwardLook,
    /// The request of its running job.
    job: Option<u64>,
}

impl CoreSource {
    fn forward_note(&mut self, update: ForwardUpdate) -> Changes {
        self.forward.updates.push(update);
        Changes {
            forward: true,
            ..Changes::default()
        }
    }

    fn forward_title(&self, conv: ConversationId) -> String {
        self.list
            .iter()
            .find(|c| c.id == conv)
            .map(conversation_title)
            .unwrap_or_else(|| {
                trc("@item:intext a chat that is not listed", "Unknown chat").into_owned()
            })
    }

    fn forward_plan_query(
        &mut self,
        asked: Asked,
        source: ConversationId,
        messages: Vec<MessageId>,
        targets: Vec<ConversationId>,
    ) -> Option<u64> {
        self.forward.next_generation += 1;
        let generation = self.forward.next_generation;
        if !self.send(UiCommand::Query {
            token: QueryToken {
                model: MODEL_FORWARD,
                generation,
            },
            query: Query::ForwardPlan {
                source,
                messages,
                targets,
            },
        }) {
            return None;
        }
        self.forward.asked.insert(generation, asked);
        Some(generation)
    }

    // ---- the dialog ----

    /// Every chat of the list, before the shown accounts and the search narrow them
    /// ([`crate::forward::listed_forward_targets`]).
    pub(super) fn forward_targets_now(&self) -> Vec<ForwardTarget> {
        let table = self.account_table(&Hidden::Unknown, false);
        self.list
            .iter()
            .map(|c| {
                let (account_tag, tint) = table.chip_and_tint(c.account).unwrap_or_else(|| {
                    (
                        String::new(),
                        crate::conversations::network_tint(self.account_network(c.account))
                            .to_owned(),
                    )
                });
                ForwardTarget {
                    id: c.id,
                    account: c.account,
                    title: conversation_title(c),
                    avatar_url: self.avatar_url(c.account, c.avatar.as_ref()),
                    is_self: c.is_self,
                    can_send: c.can_send,
                    archived: super::archived_row(c),
                    pinned_order: c.pinned_order,
                    last_activity: c.last_activity,
                    account_tag,
                    tint,
                    native: false,
                }
            })
            .collect()
    }

    pub(super) fn forward_open_now(
        &mut self,
        source: ConversationId,
        messages: Vec<MessageId>,
    ) -> Changes {
        if messages.is_empty() {
            return Changes::default();
        }
        self.forward.dialog = None;
        let in_flight =
            self.forward_plan_query(Asked::Dialog, source, messages.clone(), Vec::new());
        if in_flight.is_none() {
            return Changes::default();
        }
        self.forward.dialog = Some(Dialog {
            source,
            messages,
            ticked: Vec::new(),
            in_flight,
            pending: false,
            can_forward: false,
            plan_known: false,
        });
        Changes::default()
    }

    pub(super) fn forward_tick_now(&mut self, targets: Vec<ConversationId>) -> Changes {
        let Some(d) = self.forward.dialog.as_mut() else {
            return Changes::default();
        };
        d.ticked = targets;
        if d.in_flight.is_some() {
            // The newest ticks win: ask again when the answer in flight is in.
            d.pending = true;
            return Changes::default();
        }
        self.forward_ask_dialog();
        Changes::default()
    }

    fn forward_ask_dialog(&mut self) {
        let Some(d) = self.forward.dialog.as_ref() else {
            return;
        };
        let (source, messages, ticked) = (d.source, d.messages.clone(), d.ticked.clone());
        let g = self.forward_plan_query(Asked::Dialog, source, messages, ticked);
        if let Some(d) = self.forward.dialog.as_mut() {
            d.in_flight = g;
            d.pending = false;
        }
    }

    pub(super) fn forward_abandon_now(&mut self) {
        self.forward.dialog = None;
    }

    /// The dialog's plan came.
    fn on_forward_dialog_plan(&mut self, generation: u64, plan: Option<&ForwardPlan>) -> Changes {
        let Some(d) = self.forward.dialog.as_mut() else {
            return Changes::default();
        };
        if d.in_flight != Some(generation) {
            return Changes::default();
        }
        d.in_flight = None;
        if d.pending {
            self.forward_ask_dialog();
            return Changes::default();
        }
        let Some(plan) = plan else {
            self.forward.dialog = None;
            return self.forward_note(ForwardUpdate::Refused(
                trc("@info:status", "Could not read the messages to forward.").into_owned(),
            ));
        };
        let view = {
            let title = |c: ConversationId| self.forward_title(c);
            let network = |a| network_name(self.account_network(a)).to_owned();
            ForwardDialogView::new(
                plan,
                &ViewFacts {
                    title: &title,
                    network: &network,
                },
            )
        };
        if let Some(d) = self.forward.dialog.as_mut() {
            d.can_forward = view.can_forward;
            d.plan_known = true;
        }
        self.forward_note(ForwardUpdate::Dialog(view.to_json()))
    }

    /// The dialog's Forward: starts a job from its source, messages and ticked chats. `None`
    /// when the forward may not go (no plan yet or a newer one under way, nothing ticked,
    /// every chat refused, a limit passed).
    pub(super) fn forward_send_now(&mut self, look: ForwardLook, comment: &str) -> Option<u64> {
        let d = self.forward.dialog.as_ref()?;
        if !d.plan_known || d.in_flight.is_some() || d.pending || !d.can_forward {
            return None;
        }
        let (source, messages, targets) = (d.source, d.messages.clone(), d.ticked.clone());
        let comment = crate::composer::outgoing_text(comment);
        let request = self.forward_start_job(source, messages, targets, look, comment, None)?;
        self.forward.dialog = None;
        Some(request)
    }

    // ---- jobs ----

    fn forward_start_job(
        &mut self,
        source: ConversationId,
        messages: Vec<MessageId>,
        targets: Vec<ConversationId>,
        look: ForwardLook,
        comment: Option<Text>,
        staged_for: Option<ConversationId>,
    ) -> Option<u64> {
        self.next_request += 1;
        let request = self.next_request;
        self.forward_plan_query(
            Asked::Job(request),
            source,
            messages.clone(),
            targets.clone(),
        )?;
        self.forward.jobs.push(Job {
            request,
            source,
            messages,
            targets,
            look,
            comment,
            staged_for,
            planning: true,
            replan: false,
            asked: HashSet::new(),
            without: Vec::new(),
            asking: None,
            last_progress: None,
            render_asked: false,
            pictures: None,
            plan: None,
            sent: false,
        });
        Some(request)
    }

    fn forward_job_mut(&mut self, request: u64) -> Option<&mut Job> {
        self.forward.jobs.iter_mut().find(|j| j.request == request)
    }

    /// Asks the job's plan again (one query in flight per job).
    fn forward_replan(&mut self, request: u64) {
        let Some(j) = self.forward_job_mut(request) else {
            return;
        };
        if j.planning {
            j.replan = true;
            return;
        }
        j.planning = true;
        let (source, messages, targets) = (j.source, j.messages.clone(), j.targets.clone());
        if self
            .forward_plan_query(Asked::Job(request), source, messages, targets)
            .is_none()
            && let Some(j) = self.forward_job_mut(request)
        {
            j.planning = false;
        }
    }

    /// An event touched `conv`: the jobs that wait on it ask their plan again.
    pub(super) fn forward_source_changed(&mut self, conv: ConversationId) -> Changes {
        let due: Vec<u64> = self
            .forward
            .jobs
            .iter()
            .filter(|j| j.source == conv && !j.sent && j.asking.is_none() && !j.render_asked)
            .map(|j| j.request)
            .collect();
        for r in due {
            self.forward_replan(r);
        }
        Changes::default()
    }

    /// Takes a job out of the list and tells the front end it is over.
    fn forward_end_job(
        &mut self,
        request: u64,
        notice: String,
        sent: &[ConversationId],
    ) -> Changes {
        let Some(at) = self.forward.jobs.iter().position(|j| j.request == request) else {
            return Changes::default();
        };
        let job = self.forward.jobs.remove(at);
        let mut ch = self.forward_note(ForwardUpdate::Ended { request, notice });
        if let Some(c) = job.staged_for {
            let gone = sent.contains(&c);
            if gone {
                self.forward.staged.remove(&c);
            } else if let Some(s) = self.forward.staged.get_mut(&c)
                && s.job == Some(request)
            {
                s.job = None;
            }
            self.forward.updates.push(ForwardUpdate::StagedChanged(c));
            ch.forward = true;
        }
        ch
    }

    /// A job's plan came.
    fn on_forward_job_plan(&mut self, request: u64, plan: Option<&ForwardPlan>) -> Changes {
        let Some(j) = self.forward_job_mut(request) else {
            return Changes::default();
        };
        j.planning = false;
        let again = std::mem::take(&mut j.replan);
        let Some(plan) = plan else {
            let notice = trc("@info:status", "Could not read the messages to forward.");
            return self.forward_end_job(request, notice.into_owned(), &[]);
        };
        if again {
            // A newer plan is wanted anyway.
            self.forward_replan(request);
            return Changes::default();
        }
        self.forward_advance(request, plan.clone())
    }

    /// Goes one step on with a fresh plan.
    fn forward_advance(&mut self, request: u64, plan: ForwardPlan) -> Changes {
        let mut ch = Changes::default();
        let going = going_ids(&plan);
        // A refusal that comes before the command ends the job the way the end does.
        if going.is_empty() || plan.targets.iter().all(|t| t.refused.is_some()) {
            let notice = {
                let title = |c: ConversationId| self.forward_title(c);
                let n = nothing_sent_notice(&plan, &title);
                if n.is_empty() {
                    trc("@info:status", "None of the messages can be forwarded.").into_owned()
                } else {
                    n
                }
            };
            return self.forward_end_job(request, notice, &[]);
        }
        // Download first: every file the plan says is not here, once.
        let mut to_ask: Vec<AttachmentId> = Vec::new();
        let Some(j) = self.forward_job_mut(request) else {
            return ch;
        };
        for id in &plan.to_download {
            if j.asked.insert(*id) {
                to_ask.push(*id);
            }
        }
        let total = u32::try_from(j.asked.len()).unwrap_or(u32::MAX);
        let here = j
            .asked
            .iter()
            .filter(|a| !plan.to_download.contains(a) && !plan.lost.contains(a))
            .count();
        let done = u32::try_from(here).unwrap_or(u32::MAX);
        let progress = (total > 0 && j.last_progress != Some((done, total))).then(|| {
            j.last_progress = Some((done, total));
            ForwardUpdate::Progress {
                request,
                done,
                total,
            }
        });
        let unresolved: Vec<AttachmentId> = plan
            .lost
            .iter()
            .filter(|a| !j.without.contains(a))
            .copied()
            .collect();
        let ask_without = !unresolved.is_empty() && j.asking.as_ref() != Some(&unresolved);
        if !unresolved.is_empty() {
            j.asking = Some(unresolved.clone());
        }
        let waiting_for_files = !plan.to_download.is_empty() || !unresolved.is_empty();
        let converted = plan
            .targets
            .iter()
            .any(|t| t.refused.is_none() && !t.native);
        let wants_pictures = j.look == ForwardLook::Picture && converted;
        let mut render = false;
        if !waiting_for_files && wants_pictures && j.pictures.is_none() && !j.render_asked {
            j.render_asked = true;
            render = true;
        }
        let ready = !waiting_for_files && (!wants_pictures || j.pictures.is_some());
        let source = j.source;
        j.plan = Some(plan);
        for id in to_ask {
            if !self.send(UiCommand::DownloadAttachment { attachment: id }) {
                return self.forward_end_job(
                    request,
                    trc("@info:status", "Could not start the forward.").into_owned(),
                    &[],
                );
            }
        }
        if let Some(p) = progress {
            ch.merge(self.forward_note(p));
        }
        if ask_without {
            ch.merge(self.forward_note(ForwardUpdate::AskWithout {
                request,
                count: u32::try_from(unresolved.len()).unwrap_or(u32::MAX),
            }));
        }
        if render {
            ch.merge(self.forward_note(ForwardUpdate::RenderPictures {
                request,
                source,
                messages: going,
            }));
        }
        if ready {
            ch.merge(self.forward_send_command(request));
        }
        ch
    }

    /// The name of the sender of `row` for a header: our own profile name, a person's name as the
    /// timeline words it.
    fn forward_sender_name(
        &self,
        source: ConversationId,
        row: &kourier_core::facade::MessageRow,
    ) -> String {
        use kourier_core::facade::OriginKind;
        if row.origin != OriginKind::Remote {
            let account = self.list.iter().find(|c| c.id == source).map(|c| c.account);
            return account
                .and_then(|a| self.accounts.iter().find(|x| x.id == a))
                .and_then(|a| a.display_name.as_deref())
                .map(crate::names::single_line)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| {
                    trc("@item:intext the author of a message, yourself", "You").into_owned()
                });
        }
        row.author.as_ref().map_or_else(
            || trc("@item:intext a person without a name", "Unknown").into_owned(),
            |u| self.names.display_name(u),
        )
    }

    /// `UiCommand::Forward` of the job, from its newest plan.
    fn forward_send_command(&mut self, request: u64) -> Changes {
        let Some(j) = self.forward.jobs.iter().find(|j| j.request == request) else {
            return Changes::default();
        };
        let Some(plan) = j.plan.clone() else {
            return Changes::default();
        };
        if j.sent {
            return Changes::default();
        }
        let (source, targets, look, comment, without) = (
            j.source,
            j.targets.clone(),
            j.look,
            j.comment.clone(),
            j.without.clone(),
        );
        let pictures = j.pictures.clone().unwrap_or_default();
        let converted = plan
            .targets
            .iter()
            .any(|t| t.refused.is_none() && !t.native);
        let headers = if converted && matches!(look, ForwardLook::Header | ForwardLook::Quoted) {
            let time = crate::time::TimeContext::new(self.now_ms(), self.calendar.as_ref());
            let name_of =
                |r: &kourier_core::facade::MessageRow| self.forward_sender_name(source, r);
            let when = |ts: i64| format!("{} {}", time.short_date(ts), time.clock(ts));
            headers_for(&plan, &name_of, &when)
        } else {
            Vec::new()
        };
        let command = UiCommand::Forward {
            request: RequestId(request),
            source,
            messages: going_ids(&plan),
            targets,
            look,
            headers,
            comment,
            pictures: Redacted::new(pictures),
            without,
        };
        if !self.send(command) {
            return self.forward_end_job(
                request,
                trc("@info:status", "Could not start the forward.").into_owned(),
                &[],
            );
        }
        if let Some(j) = self.forward_job_mut(request) {
            j.sent = true;
        }
        Changes::default()
    }

    /// "Send Without Them" (`send`) or Cancel at the question about lost files.
    pub(super) fn forward_without_now(&mut self, request: u64, send: bool) -> Changes {
        let Some(j) = self.forward_job_mut(request) else {
            return Changes::default();
        };
        let Some(lost) = j.asking.take() else {
            return Changes::default();
        };
        if !send {
            return self.forward_end_job(request, String::new(), &[]);
        }
        for a in lost {
            if !j.without.contains(&a) {
                j.without.push(a);
            }
        }
        // The newest plan decides what is left to wait for (or to send).
        self.forward_replan(request);
        Changes::default()
    }

    /// Ends the job (the downloads it asked stay).
    pub(super) fn forward_cancel_now(&mut self, request: u64) -> Changes {
        self.forward_end_job(request, String::new(), &[])
    }

    /// The front end's pictures for a job that asked for them.
    pub(super) fn forward_pictures_now(&mut self, request: u64, paths: Vec<PathBuf>) -> Changes {
        let Some(j) = self.forward_job_mut(request) else {
            return Changes::default();
        };
        if !j.render_asked || j.pictures.is_some() {
            return Changes::default();
        }
        j.pictures = Some(paths);
        self.forward_send_command(request)
    }

    /// The rows the picture look draws for the job's going messages (F7): the core's rows of the
    /// newest plan, built by the mapping the open timeline uses (`rows_of_kind`, with the source
    /// chat's kind), newest first like the timeline's. Empty for an unknown job or before a plan
    /// came.
    pub(super) fn forward_render_rows_now(&self, request: u64) -> Vec<crate::timeline::MessageRow> {
        let Some(plan) = self
            .forward
            .jobs
            .iter()
            .find(|j| j.request == request)
            .and_then(|j| j.plan.as_ref())
        else {
            return Vec::new();
        };
        let rows: Vec<kourier_core::facade::MessageRow> = plan
            .messages
            .iter()
            .filter(|m| m.verdict == kourier_core::facade::ForwardVerdict::Goes)
            .filter_map(|m| m.row.clone())
            .collect();
        // The author lines follow the SOURCE chat, which need not be open any more when a
        // download-first job draws (the open chat's kind would give a group's messages no names).
        let source = plan.source;
        let is_group = self
            .list
            .iter()
            .chain(self.list_partial.iter())
            .chain(self.detail.as_ref().map(|d| &d.row))
            .find(|c| c.id == source)
            .is_some_and(|c| c.kind != kourier_model::ConvKind::Direct);
        let time = crate::time::TimeContext::new(self.now_ms(), self.calendar.as_ref());
        self.rows_of_kind(
            &rows,
            None,
            false,
            &crate::readmore::ReadMore::default(),
            &time,
            is_group,
        )
    }

    /// `UiEvent::ForwardEnded`.
    pub(super) fn on_forward_ended(
        &mut self,
        request: RequestId,
        sent: &[ConversationId],
        refused: &[(ConversationId, ForwardRefusal)],
    ) -> Changes {
        if !self.forward.jobs.iter().any(|j| j.request == request.0) {
            return Changes::default();
        }
        let notice = {
            let title = |c: ConversationId| self.forward_title(c);
            ended_notice(sent, refused, &title)
        };
        self.forward_end_job(request.0, notice, sent)
    }

    /// A plan answer (`MODEL_FORWARD`).
    pub(super) fn on_forward_plan(
        &mut self,
        token: QueryToken,
        result: Option<&QueryData>,
    ) -> Changes {
        let Some(asked) = self.forward.asked.remove(&token.generation) else {
            return Changes::default();
        };
        let plan = match result {
            Some(QueryData::ForwardPlan(p)) => Some(p.as_ref()),
            _ => None,
        };
        match asked {
            Asked::Dialog => self.on_forward_dialog_plan(token.generation, plan),
            Asked::Job(request) => self.on_forward_job_plan(request, plan),
        }
    }

    // ---- staging (the contract) ----

    fn forward_account_of(&self, conv: ConversationId) -> Option<kourier_model::AccountId> {
        self.list
            .iter()
            .find(|c| c.id == conv)
            .map(|c| c.account)
            .or_else(|| self.known_details.get(&conv).map(|d| d.row.account))
    }

    pub(super) fn stage_forward_now(
        &mut self,
        target: ConversationId,
        source: ConversationId,
        messages: Vec<MessageId>,
    ) -> bool {
        if messages.is_empty() || !self.list.iter().any(|c| c.id == target && c.can_send) {
            return false;
        }
        // Rows known (the open timeline is the source's): at least one must offer Forward.
        if self.open == Some(source) && self.window.loaded {
            let any = self
                .window
                .rows
                .iter()
                .any(|r| messages.contains(&r.id) && r.actions.forward);
            if !any {
                return false;
            }
        }
        if let Some(old) = self.forward.staged.remove(&target)
            && let Some(request) = old.job
        {
            self.forward_end_job_quiet(request);
        }
        let look = self.forward_default_now();
        self.forward.staged.insert(
            target,
            Staged {
                source,
                messages,
                look,
                job: None,
            },
        );
        self.forward
            .updates
            .push(ForwardUpdate::StagedChanged(target));
        true
    }

    /// Ends a job without a notice and without touching the staged forward (it is being
    /// replaced or dropped by the caller).
    fn forward_end_job_quiet(&mut self, request: u64) {
        if let Some(at) = self.forward.jobs.iter().position(|j| j.request == request) {
            self.forward.jobs.remove(at);
            self.forward.updates.push(ForwardUpdate::Ended {
                request,
                notice: String::new(),
            });
        }
    }

    pub(super) fn forward_of_now(&self, conv: ConversationId) -> Option<StagedForward> {
        let s = self.forward.staged.get(&conv)?;
        let native = self
            .forward_account_of(conv)
            .zip(self.forward_account_of(s.source))
            .is_some_and(|(a, b)| a == b);
        Some(StagedForward {
            count: s.messages.len(),
            from: self.forward_title(s.source),
            look: s.look,
            native,
            waiting: s.job.is_some(),
        })
    }

    pub(super) fn set_forward_look_now(&mut self, conv: ConversationId, look: ForwardLook) -> bool {
        let Some(s) = self.forward.staged.get_mut(&conv) else {
            return false;
        };
        s.look = look;
        self.forward
            .updates
            .push(ForwardUpdate::StagedChanged(conv));
        true
    }

    pub(super) fn drop_forward_now(&mut self, conv: ConversationId) -> bool {
        let Some(s) = self.forward.staged.remove(&conv) else {
            return false;
        };
        if let Some(request) = s.job {
            self.forward_end_job_quiet(request);
        }
        self.forward
            .updates
            .push(ForwardUpdate::StagedChanged(conv));
        true
    }

    pub(super) fn send_staged_forward_now(&mut self, conv: ConversationId, comment: &str) -> bool {
        let Some(s) = self.forward.staged.get(&conv) else {
            return false;
        };
        if s.job.is_some() {
            return false;
        }
        let (source, messages, look) = (s.source, s.messages.clone(), s.look);
        let comment = crate::composer::outgoing_text(comment);
        let Some(request) =
            self.forward_start_job(source, messages, vec![conv], look, comment, Some(conv))
        else {
            return false;
        };
        if let Some(s) = self.forward.staged.get_mut(&conv) {
            s.job = Some(request);
        }
        self.forward
            .updates
            .push(ForwardUpdate::StagedChanged(conv));
        true
    }

    pub(super) fn set_forward_default_now(&mut self, look: ForwardLook) {
        self.forward.default_look = Some(look);
    }

    pub(super) fn forward_default_now(&self) -> ForwardLook {
        self.forward.default_look.unwrap_or(ForwardLook::Header)
    }

    pub(super) fn take_forward_now(&mut self) -> Vec<ForwardUpdate> {
        std::mem::take(&mut self.forward.updates)
    }
}

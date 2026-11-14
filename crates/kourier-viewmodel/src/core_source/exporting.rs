//! Export Selected in [`CoreSource`] (EXP-ui 2): the plan, the dialog, the save name, the
//! documents (built by a worker the front-end runs), the command, progress, Cancel and the end.
//!
//! One export at a time per window. The stages: `Idle` -> `Planning` (the plan query is out) ->
//! `Dialog` (the plan is kept here, the dialog is open) -> `Picking` (the save dialog is open) ->
//! `Building` (a worker makes the documents; the plan went with the job) -> `Running` (the
//! command is out) -> `Idle`. The front-end takes what changed with
//! [`UiSource::take_export`]. Nothing here logs a name, a number or a path.

use std::collections::HashMap;
use std::path::PathBuf;

use kourier_core::facade::{
    ExportDocument, ExportFailure, ExportFile, ExportHead, ExportOutcome, ExportPlan, ExportStamp,
    Progress, Query, RequestId, SaveMode, SaveTarget, UiCommand,
};
use kourier_model::{AttachmentId, ConvKind, ConversationId, MessageId, UserId};

use super::{Changes, CoreSource, QueryToken, conversation_title};
use crate::conversations::network_name;
use crate::export::flow::{
    DialogView, ExportBuilt, ExportJob, ExportUpdate, end_of, file_names, offset_zone,
    page_language, plain_end, refusal_text, suggested_archive_name,
};
use crate::export::{BuildInput, NameSnapshot, OffsetTable, Ticks, archive_name_title, numbers};
use crate::i18n::trc;

/// Query model id of the export plan (after `MODEL_SEARCH`).
pub const MODEL_EXPORT: u32 = 22;

/// The most numbered names tried after an `Exists` of a name that was free when it was asked.
const EXISTS_TRIES: u32 = 99;

#[derive(Default)]
pub(super) struct Export {
    stage: Stage,
    next_generation: u64,
    updates: Vec<ExportUpdate>,
}

#[derive(Default)]
enum Stage {
    #[default]
    Idle,
    Planning {
        generation: u64,
        conv: ConversationId,
    },
    Dialog(Box<ExportPlan>),
    Picking {
        plan: Box<ExportPlan>,
        ticks: Ticks,
    },
    Building(Box<Active>),
    Running(Box<Active>),
}

/// An export that has a target.
struct Active {
    request: RequestId,
    conv: ConversationId,
    dir: PathBuf,
    /// The name the dialog gave, and the name asked now (` (1)` … after an `Exists`).
    base: String,
    name: String,
    tries: u32,
    mode: SaveMode,
    with_disappearing: bool,
    /// For the end notice only: file names by attachment.
    failed_names: HashMap<AttachmentId, String>,
    /// What a numbered name sends again.
    retry: Option<Retry>,
    /// Cancel was pressed while the command ran: a name taken meanwhile is no reason to write
    /// the archive the person cancelled.
    cancelled: bool,
}

struct Retry {
    messages: Vec<ExportStamp>,
    files: Vec<ExportFile>,
    documents: Vec<ExportDocument>,
    manifest: ExportHead,
}

/// See [`CoreSource::export_chat_facts`].
struct ChatFacts {
    names: NameSnapshot,
    own_name: String,
    own: Vec<UserId>,
    network: String,
    chat_title: String,
    chat_kind: ConvKind,
    peer: Option<UserId>,
}

impl Export {
    /// Takes the stage out (leaving `Idle`) only when `want` says it is the one asked for; any
    /// other stage stays where it is.
    fn take_if(&mut self, want: impl Fn(&Stage) -> bool) -> Option<Stage> {
        want(&self.stage).then(|| std::mem::take(&mut self.stage))
    }
}

impl CoreSource {
    fn export_note(&mut self, update: ExportUpdate) -> Changes {
        self.export.updates.push(update);
        Changes {
            export: true,
            ..Changes::default()
        }
    }

    /// The chat's list row, as the header or the list has it.
    fn export_row(&self, conv: ConversationId) -> Option<&super::CoreConversation> {
        self.detail
            .as_ref()
            .map(|d| &d.row)
            .filter(|r| r.id == conv)
            .or_else(|| self.known_details.get(&conv).map(|d| &d.row))
            .or_else(|| self.list.iter().find(|c| c.id == conv))
    }

    /// What the name of the archive and the documents are made of, gathered ONCE so the two
    /// cannot differ: the chat's row, the account, the names snapshot, the kind, the peer and
    /// the title.
    fn export_chat_facts(&self, plan: &ExportPlan) -> ChatFacts {
        let row = self.export_row(plan.conv).cloned();
        let account = row.as_ref().map(|r| r.account);
        let acct = account.and_then(|a| self.accounts.iter().find(|x| x.id == a));
        let peer = row.as_ref().and_then(|r| r.peer.clone());
        let mut names = NameSnapshot::from_plan(plan, &self.names);
        if let Some(p) = peer.as_ref() {
            names.add(p, &self.names);
        }
        ChatFacts {
            names,
            own_name: acct
                .and_then(|a| a.display_name.clone())
                .unwrap_or_default(),
            own: acct.map(|a| a.own_ids.clone()).unwrap_or_default(),
            network: network_name(account.map_or(kourier_model::NetworkKind::Fake, |a| {
                self.account_network(a)
            }))
            .to_owned(),
            chat_title: row.as_ref().map(conversation_title).unwrap_or_default(),
            chat_kind: row.as_ref().map_or(ConvKind::Group, |r| r.kind),
            peer,
        }
    }

    pub(super) fn export_busy_now(&self) -> bool {
        !matches!(self.export.stage, Stage::Idle)
    }

    /// Something runs that the bar shows as progress: the plan is asked for, the documents are
    /// made or the archive is written. While the dialog or the save dialog stands the plan is
    /// kept but nothing runs, so the bar behind them stays as it is (a second plan is still
    /// refused: [`Self::export_busy_now`]).
    pub(super) fn export_running_now(&self) -> bool {
        matches!(
            self.export.stage,
            Stage::Planning { .. } | Stage::Building(_) | Stage::Running(_)
        )
    }

    /// The bar's Export Selected…: asks the core for the plan of `ids`.
    pub(super) fn export_plan_now(&mut self, conv: ConversationId, ids: Vec<MessageId>) -> Changes {
        if ids.is_empty() {
            return Changes::default();
        }
        if self.export_busy_now() {
            return self.export_note(ExportUpdate::Refused(
                trc("@info:status", "An export is running.").into_owned(),
            ));
        }
        if let Some(d) = self.detail.as_ref().filter(|d| d.row.id == conv)
            && let Err(r) = d.export()
        {
            return self.export_note(ExportUpdate::Ended(plain_end(refusal_text(r), Some(conv))));
        }
        self.export.next_generation += 1;
        let generation = self.export.next_generation;
        if !self.send(UiCommand::Query {
            token: QueryToken {
                model: MODEL_EXPORT,
                generation,
            },
            query: Query::ExportPlan { conv, ids },
        }) {
            return Changes::default();
        }
        self.export.stage = Stage::Planning { generation, conv };
        Changes {
            export: true,
            ..Changes::default()
        }
    }

    /// The plan's answer.
    pub(super) fn on_export_plan(
        &mut self,
        token: QueryToken,
        result: Option<&ExportPlan>,
    ) -> Changes {
        let Stage::Planning { generation, conv } = self.export.stage else {
            return Changes::default();
        };
        if generation != token.generation {
            return Changes::default();
        }
        self.export.stage = Stage::Idle;
        let Some(plan) = result else {
            return self.export_note(ExportUpdate::Ended(plain_end(
                trc("@info:status", "Could not read the selected messages.").into_owned(),
                Some(conv),
            )));
        };
        if let Err(r) = plan.chat {
            return self.export_note(ExportUpdate::Ended(plain_end(refusal_text(r), Some(conv))));
        }
        let n = numbers(plan);
        if n.included == 0 && n.disappearing == 0 {
            return self.export_note(ExportUpdate::Ended(plain_end(
                trc(
                    "@info:status",
                    "None of the selected messages can be exported.",
                )
                .into_owned(),
                Some(conv),
            )));
        }
        let json = DialogView::new(n).to_json();
        self.export.stage = Stage::Dialog(Box::new(plan.clone()));
        self.export_note(ExportUpdate::Dialog(json))
    }

    /// The dialog's Export…: the save dialog is next.
    pub(super) fn export_choose_now(&mut self, ticks: Ticks) -> bool {
        let Some(Stage::Dialog(plan)) = self.export.take_if(|s| matches!(s, Stage::Dialog(_)))
        else {
            return false;
        };
        // The title the documents use (never the raw title: a direct chat without a name has the
        // number there).
        let facts = self.export_chat_facts(&plan);
        let title = archive_name_title(
            &facts.names,
            &facts.own_name,
            &facts.own,
            facts.chat_kind,
            plan.members,
            facts.peer.as_ref(),
            &facts.chat_title,
        );
        let (date, time) = crate::save_name::stamp(plan.now, self.calendar.as_ref());
        let name = suggested_archive_name(&title, &date, &time);
        self.export.stage = Stage::Picking { plan, ticks };
        self.export.updates.push(ExportUpdate::SaveName(name));
        true
    }

    /// The dialog or the save dialog closed without an answer: the plan is dropped.
    pub(super) fn export_abandon_now(&mut self) {
        if matches!(self.export.stage, Stage::Dialog(_) | Stage::Picking { .. }) {
            self.export.stage = Stage::Idle;
        }
    }

    /// The file dialog gave the folder and the name: the job for a worker.
    pub(super) fn export_target_now(&mut self, dir: PathBuf, name: String) -> Option<ExportJob> {
        let Some(Stage::Picking { plan, ticks }) =
            self.export.take_if(|s| matches!(s, Stage::Picking { .. }))
        else {
            return None;
        };
        let plan = *plan;
        let conv = plan.conv;
        let facts = self.export_chat_facts(&plan);
        let input = BuildInput {
            names: facts.names,
            offsets: OffsetTable::from_plan(&plan, self.calendar.as_ref()),
            time_zone: offset_zone(self.calendar.utc_offset_s(plan.now)),
            ticks,
            own_name: facts.own_name,
            chat_title: facts.chat_title,
            chat_kind: facts.chat_kind,
            peer: facts.peer,
            own: facts.own,
            network: facts.network,
            exporter: format!("Kourier {}", env!("CARGO_PKG_VERSION")),
            salt: [0; 32],
            language: page_language(),
            plan,
        };
        let failed_names = file_names(&input.plan);
        self.next_request += 1;
        let request = RequestId(self.next_request);
        self.export.stage = Stage::Building(Box::new(Active {
            request,
            conv,
            dir: dir.clone(),
            base: name.clone(),
            name: name.clone(),
            tries: 0,
            mode: SaveMode::NoReplace,
            with_disappearing: ticks.disappearing,
            failed_names,
            retry: None,
            cancelled: false,
        }));
        self.export
            .updates
            .push(ExportUpdate::Progress { done: 0, total: 0 });
        Some(ExportJob {
            request,
            conv,
            input,
            dir,
            name,
        })
    }

    /// The worker is done: the command goes out (here, because the command sink is the Qt
    /// thread's).
    pub(super) fn export_built_now(&mut self, built: ExportBuilt) -> Changes {
        // (Not building any more: cancelled meanwhile, the documents are dropped.)
        let Some(Stage::Building(mut active)) = self
            .export
            .take_if(|s| matches!(s, Stage::Building(a) if a.request == built.request))
        else {
            return Changes::default();
        };
        let crate::export::Built {
            messages,
            files,
            documents,
            head,
            ..
        } = built.built;
        active.mode = built.mode;
        active.conv = built.conv;
        active.retry = Some(Retry {
            messages: messages.clone(),
            files: files.clone(),
            documents: documents.clone(),
            manifest: head.clone(),
        });
        let sent = self.send(UiCommand::ExportArchive {
            request: active.request,
            conv: active.conv,
            target: built.target,
            mode: built.mode,
            with_disappearing: built.with_disappearing,
            messages,
            files,
            documents,
            manifest: head,
        });
        if !sent {
            return self.export_note(ExportUpdate::Ended(plain_end(
                failed_words(),
                Some(active.conv),
            )));
        }
        self.export.stage = Stage::Running(active);
        Changes::default()
    }

    /// Cancel: before the target nothing is out; while the documents are made the worker's result
    /// is dropped; while it writes the core is told.
    pub(super) fn export_cancel_now(&mut self) -> Changes {
        let cancelled = || trc("@info:status", "Export cancelled. Nothing was saved.").into_owned();
        match std::mem::take(&mut self.export.stage) {
            Stage::Idle => Changes::default(),
            Stage::Planning { conv, .. } => {
                self.export_note(ExportUpdate::Ended(plain_end(cancelled(), Some(conv))))
            }
            Stage::Dialog(plan) | Stage::Picking { plan, .. } => {
                self.export_note(ExportUpdate::Ended(plain_end(cancelled(), Some(plan.conv))))
            }
            Stage::Building(active) => self.export_note(ExportUpdate::Ended(plain_end(
                cancelled(),
                Some(active.conv),
            ))),
            Stage::Running(mut active) => {
                let request = active.request;
                // Remembered: an `Exists` that crosses this cancel must not be retried.
                active.cancelled = true;
                self.export.stage = Stage::Running(active);
                self.send(UiCommand::CancelExport { request });
                Changes::default()
            }
        }
    }

    pub(super) fn on_export_progress(&mut self, p: &Progress) -> Changes {
        let Progress::Export {
            request,
            done,
            total,
        } = p
        else {
            return Changes::default();
        };
        match &self.export.stage {
            Stage::Running(a) if a.request == *request => {
                let (done, total) = (*done, *total);
                self.export_note(ExportUpdate::Progress { done, total })
            }
            _ => Changes::default(),
        }
    }

    pub(super) fn on_export_ended(
        &mut self,
        request: RequestId,
        outcome: &ExportOutcome,
    ) -> Changes {
        let Some(Stage::Running(mut active)) = self
            .export
            .take_if(|s| matches!(s, Stage::Running(a) if a.request == request))
        else {
            return Changes::default();
        };
        // A name that was free when the worker looked and is taken now (a no-replace path):
        // the next numbered one, like a batch of saves.
        if active.cancelled && matches!(outcome, ExportOutcome::Failed(ExportFailure::Exists)) {
            // The person cancelled: the name taken meanwhile is no reason to write the archive.
            return self.export_note(ExportUpdate::Ended(end_of(
                &ExportOutcome::Cancelled,
                &[],
                active.dir.join(&active.name),
                Some(active.conv),
            )));
        }
        if matches!(outcome, ExportOutcome::Failed(ExportFailure::Exists))
            && active.mode == SaveMode::NoReplace
            && active.tries < EXISTS_TRIES
            && let Some(retry) = active.retry.as_ref()
        {
            active.tries += 1;
            active.name = crate::save_name::with_number(&active.base, active.tries);
            self.next_request += 1;
            active.request = RequestId(self.next_request);
            let command = UiCommand::ExportArchive {
                request: active.request,
                conv: active.conv,
                target: SaveTarget {
                    dir: active.dir.clone(),
                    name: active.name.clone(),
                },
                mode: SaveMode::NoReplace,
                with_disappearing: active.with_disappearing,
                messages: retry.messages.clone(),
                files: retry.files.clone(),
                documents: retry.documents.clone(),
                manifest: retry.manifest.clone(),
            };
            if self.send(command) {
                self.export.stage = Stage::Running(active);
                return Changes::default();
            }
            return self.export_note(ExportUpdate::Ended(plain_end(
                failed_words(),
                Some(active.conv),
            )));
        }
        let failed: Vec<String> = match outcome {
            ExportOutcome::Done { failed, .. } => failed
                .iter()
                .filter_map(|id| active.failed_names.get(id).cloned())
                .collect(),
            _ => Vec::new(),
        };
        let end = end_of(
            outcome,
            &failed,
            active.dir.join(&active.name),
            Some(active.conv),
        );
        self.export_note(ExportUpdate::Ended(end))
    }

    /// The dialog's numbers for `ticks` (JSON), from the plan the `Dialog` stage holds; `None` in
    /// any other stage.
    pub(super) fn export_numbers_now(&self, ticks: Ticks) -> Option<String> {
        match &self.export.stage {
            Stage::Dialog(plan) => Some(DialogView::new(numbers(plan)).to_json_for(ticks)),
            _ => None,
        }
    }

    pub(super) fn take_export_now(&mut self) -> Vec<ExportUpdate> {
        std::mem::take(&mut self.export.updates)
    }
}

fn failed_words() -> String {
    trc("@info:status", "The export could not be started.").into_owned()
}

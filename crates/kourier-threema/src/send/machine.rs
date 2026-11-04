//! THE MACHINE (TH6a S5a): ONE row on ONE socket, sans-I/O. It holds no socket, no file and no clock: the socket's pass
//! hands it what the servers said and does what it answers ([`Do`]), in that order, and a zone's end comes back as its
//! next input. So THE ORDER of the send steps is this file's alone:
//!
//! ```text
//!  prepared:  Reflect -> its ack -> ZONE REFLECTED -> the boxes
//!  reflected: the boxes (every one owed, in one pass) -> their acks -> ZONE ACKS, once per pass
//!               some pending:             the row stays, the rest of the acks are awaited
//!               none pending, update owed: the update's seal is written by that zone -> Reflect -> its ack -> ZONE DONE
//!               none pending, none owed:  finished
//!  acked:     Reflect (the stored update) -> its ack -> ZONE DONE -> finished
//! ```
//!
//! A row starts where its state says, so a new socket resumes it: a sealed reflect not yet acked is written again, the
//! pending boxes are written again, a durable ack is never asked for twice. A machine lives as long as its socket: it
//! writes each frame once, and a frame whose ack does not come waits for a later socket.
//!
//! WHAT IS NOT HERE: a frame's id (the socket gives each `Reflect` a new one, unique per connection, and says which:
//! [`Machine::reflect_written`]), the time (a zone's `now` is the socket's), and what happens when an awaited ack does
//! not come: the machine just goes on waiting, says that frames stand unanswered ([`Machine::unanswered`]), and the
//! socket's deadline and the row's cooling decide (`crate::send::shared`).

use std::collections::BTreeSet;
use std::fmt;

use super::queue::{ReceiverState, SendEnd, SendReceiver, SendState, SendTask};

/// Which of a row's two reflects a frame is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReflectOf {
    /// The `OutgoingMessage`.
    Message,
    /// The `Sent` update.
    Update,
}

/// A zone the socket runs and whose end it hands back.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Zone {
    /// ZONE REFLECTED; then [`Machine::reflected`].
    Reflected,
    /// ZONE ACKS of one pass; then [`Machine::acks_end`].
    Acks {
        /// The receivers this pass acked (none: a row that has nobody to write to).
        acked: Vec<String>,
        /// This pass leaves nobody pending and the row owes an update: the socket seals it for this zone.
        seal: bool,
    },
    /// ZONE DONE; then [`Machine::done`].
    Done {
        /// The time of the update's reflect-ack (ms): the core's `server_at`.
        sent_at: i64,
    },
}

impl fmt::Debug for Zone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reflected => f.write_str("Zone::Reflected"),
            Self::Acks { acked, seal } => f
                .debug_struct("Zone::Acks")
                .field("acked", &acked.len())
                .field("seal", seal)
                .finish(),
            Self::Done { sent_at } => f
                .debug_struct("Zone::Done")
                .field("sent_at", sent_at)
                .finish(),
        }
    }
}

/// What the socket does next for the row, in the order given.
#[derive(PartialEq, Eq)]
pub(crate) enum Do {
    /// Write this sealed envelope as one `Reflect` under a new reflect-id, and tell [`Machine::reflect_written`].
    Reflect {
        /// The stored bytes, as they are.
        envelope: Vec<u8>,
        /// Which reflect it is.
        of: ReflectOf,
    },
    /// Write this `message-with-metadata-box`, as it is stored.
    Box(Vec<u8>),
    /// Run this zone.
    Zone(Zone),
    /// The row is over: the next row may run.
    Finished,
    /// The row cannot go on, and nothing is awaited of it: it cools.
    Cool,
}

/// Lengths only: a box holds a receiver's ID, in the clear, at its offset 8.
impl fmt::Debug for Do {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reflect { envelope, of } => f
                .debug_struct("Do::Reflect")
                .field("len", &envelope.len())
                .field("of", of)
                .finish(),
            Self::Box(bytes) => f.debug_tuple("Do::Box").field(&bytes.len()).finish(),
            Self::Zone(zone) => zone.fmt(f),
            Self::Finished => f.write_str("Do::Finished"),
            Self::Cool => f.write_str("Do::Cool"),
        }
    }
}

/// Where the row stands on this socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// The `OutgoingMessage`'s reflect-ack is awaited.
    Reflect,
    /// The boxes' acks are awaited (or their zone runs).
    Boxes,
    /// The update's reflect-ack is awaited.
    Update,
}

/// ONE ROW on ONE socket (module doc). `Debug` prints counts.
pub(crate) struct Machine {
    task: SendTask,
    phase: Phase,
    /// The boxes owed and not written yet, by receiver.
    owed: Vec<(String, Vec<u8>)>,
    /// How many pending receivers this machine leaves alone: the attempt's `only` does not name them. They hold the update
    /// back: nothing is sealed while a receiver is pending.
    others: usize,
    /// The receivers whose box this socket wrote and whose ack is not durable.
    written: BTreeSet<String>,
    /// How many of [`Machine::written`] never went: the socket was going when their turn came.
    unsent: usize,
    /// The reflect-id whose ack is awaited.
    awaited: Option<u32>,
}

impl fmt::Debug for Machine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Machine")
            .field("task", &self.task.id)
            .field("phase", &self.phase)
            .field("owed", &self.owed.len())
            .field("written", &self.written.len())
            .finish_non_exhaustive()
    }
}

impl Machine {
    /// The machine of a row and what to do first, by the row's state. `only`: the attempt's receivers, and nobody else gets
    /// a box. `None`: the row has nothing to run, or lacks the bytes its state says it has (the schema's CHECKs refuse that).
    pub(crate) fn start(
        mut task: SendTask,
        receivers: Vec<SendReceiver>,
        only: Option<&[String]>,
    ) -> Option<(Self, Vec<Do>)> {
        let mut owed = Vec::new();
        let mut others = 0;
        for r in receivers {
            if r.state != ReceiverState::Pending {
                continue;
            }
            let payload = r.payload?;
            if only.is_none_or(|names| names.contains(&r.identity)) {
                owed.push((r.identity, payload));
            } else {
                others += 1;
            }
        }
        let (phase, first) = match task.state {
            SendState::Prepared => (
                Phase::Reflect,
                task.reflect_env.take().map(ReflectOf::message),
            ),
            SendState::Reflected => (Phase::Boxes, None),
            SendState::Acked => (Phase::Update, task.update_env.take().map(ReflectOf::update)),
            SendState::Done => return None,
        };
        let mut machine = Self {
            task,
            phase,
            owed,
            others,
            written: BTreeSet::new(),
            unsent: 0,
            awaited: None,
        };
        let first = match (phase, first) {
            (Phase::Boxes, _) => machine.boxes(),
            (_, Some(reflect)) => vec![reflect],
            (_, None) => return None,
        };
        Some((machine, first))
    }

    /// The row.
    pub(crate) fn task(&self) -> &SendTask {
        &self.task
    }

    /// A box just handed out did not go: the socket was going. No ack can come for it, and it is no frame unanswered.
    pub(crate) fn box_not_written(&mut self) {
        self.unsent += 1;
    }

    /// Whether a frame of the row went on this socket and its ack did not come (what makes the socket's end a failed run
    /// of the row, and what its deadline counts for).
    pub(crate) fn unanswered(&self) -> bool {
        self.awaited.is_some() || self.written.len() > self.unsent
    }

    /// The reflect just handed out goes under `id`: its ack is awaited.
    pub(crate) fn reflect_written(&mut self, id: u32) {
        self.awaited = Some(id);
    }

    /// A reflect-ack came. The zone it allows; `None` when it is no ack this machine awaits.
    pub(crate) fn reflect_ack(&mut self, id: u32, at_ms: u64) -> Option<Zone> {
        if self.awaited != Some(id) {
            return None;
        }
        self.awaited = None;
        match self.phase {
            Phase::Reflect => Some(Zone::Reflected),
            Phase::Update => Some(Zone::Done {
                sent_at: i64::try_from(at_ms).unwrap_or(i64::MAX),
            }),
            Phase::Boxes => None,
        }
    }

    /// ZONE REFLECTED committed: the boxes may go.
    pub(crate) fn reflected(&mut self) -> Vec<Do> {
        self.phase = Phase::Boxes;
        self.boxes()
    }

    /// Every box owed, in one pass; a row with none goes on to its zone (a notes group, every receiver left out, or a
    /// kill between ZONE REFLECTED and the zone of no acks).
    fn boxes(&mut self) -> Vec<Do> {
        if self.owed.is_empty() && self.written.is_empty() {
            return vec![Do::Zone(self.acks_zone(Vec::new()))];
        }
        std::mem::take(&mut self.owed)
            .into_iter()
            .map(|(identity, payload)| {
                self.written.insert(identity);
                Do::Box(payload)
            })
            .collect()
    }

    /// The message-acks of one pass (the receiver's ID and the message id). The zone they allow, and how many of them
    /// this machine awaits none of.
    pub(crate) fn message_acks(&mut self, acks: Vec<(String, u64)>) -> (Option<Zone>, usize) {
        let mut acked = Vec::new();
        let mut unknown = 0;
        for (identity, message_id) in acks {
            if message_id == self.task.message_id && self.written.remove(&identity) {
                acked.push(identity);
            } else {
                unknown += 1;
            }
        }
        let zone = (!acked.is_empty()).then(|| self.acks_zone(acked));
        (zone, unknown)
    }

    /// ZONE ACKS of `acked`; the pass that leaves nobody pending seals the update the row owes.
    fn acks_zone(&self, acked: Vec<String>) -> Zone {
        let seal = self.task.has_update
            && self.owed.is_empty()
            && self.written.is_empty()
            && self.others == 0;
        Zone::Acks { acked, seal }
    }

    /// ZONE ACKS committed and left the row at `end`; `update` is the update it wrote. No update goes before the zone
    /// that holds it, and that zone is the one that left nobody pending.
    pub(crate) fn acks_end(&mut self, end: SendEnd, update: Option<Vec<u8>>) -> Vec<Do> {
        match (end, update) {
            // The attempt's receivers are through; the others stay pending, and nothing else is owed on this run.
            (SendEnd::Open { .. }, _) if self.written.is_empty() && self.others > 0 => {
                vec![Do::Finished]
            }
            (SendEnd::Open { .. }, _) if self.written.is_empty() => vec![Do::Cool],
            (SendEnd::Open { .. }, _) => Vec::new(),
            (SendEnd::Acked, Some(envelope)) => {
                self.phase = Phase::Update;
                vec![ReflectOf::update(envelope)]
            }
            (SendEnd::Acked, None) => vec![Do::Cool],
            (SendEnd::Done | SendEnd::Deleted, _) => vec![Do::Finished],
        }
    }

    /// ZONE DONE committed: the row is over.
    pub(crate) fn done(&self) -> Vec<Do> {
        vec![Do::Finished]
    }
}

impl ReflectOf {
    fn message(envelope: Vec<u8>) -> Do {
        Do::Reflect {
            envelope,
            of: Self::Message,
        }
    }

    fn update(envelope: Vec<u8>) -> Do {
        Do::Reflect {
            envelope,
            of: Self::Update,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::send::queue::SendOrigin;

    const ANN: &str = "PEER0001";
    const BOB: &str = "PEER0002";
    const CRE: &str = "PEER0003";
    const ID: u64 = 0x0807_0605_0403_0201;

    fn task(state: SendState, has_update: bool) -> SendTask {
        SendTask {
            id: 7,
            origin: SendOrigin::Job,
            kind: "test-job".to_owned(),
            local: None,
            message_id: ID,
            conversation: vec![1],
            created_at: 1,
            state,
            has_update,
            sent_at: None,
            bytes: 0,
            attempts: 0,
            queued_at: 1,
            finished_at: None,
            reflect_env: (state == SendState::Prepared).then(|| b"the reflect".to_vec()),
            update_env: (state == SendState::Acked).then(|| b"the update".to_vec()),
        }
    }

    fn pending(identity: &str) -> SendReceiver {
        SendReceiver {
            identity: identity.to_owned(),
            state: ReceiverState::Pending,
            payload: Some(format!("box to {identity}").into_bytes()),
        }
    }

    fn other(identity: &str, state: ReceiverState) -> SendReceiver {
        SendReceiver {
            identity: identity.to_owned(),
            state,
            payload: None,
        }
    }

    fn the_reflect() -> Do {
        Do::Reflect {
            envelope: b"the reflect".to_vec(),
            of: ReflectOf::Message,
        }
    }

    fn the_update() -> Do {
        Do::Reflect {
            envelope: b"the update".to_vec(),
            of: ReflectOf::Update,
        }
    }

    fn a_box(identity: &str) -> Do {
        Do::Box(format!("box to {identity}").into_bytes())
    }

    fn ack(identity: &str) -> (String, u64) {
        (identity.to_owned(), ID)
    }

    fn acks_zone(acked: &[&str], seal: bool) -> Zone {
        Zone::Acks {
            acked: acked.iter().map(|id| (*id).to_owned()).collect(),
            seal,
        }
    }

    /// The row's three receivers, its reflect not yet acked: past the reflect's ack and its zone.
    fn at_the_boxes(has_update: bool) -> Machine {
        let (mut machine, first) = Machine::start(
            task(SendState::Prepared, has_update),
            vec![pending(ANN), pending(BOB), pending(CRE)],
            None,
        )
        .expect("a row that runs");
        assert_eq!(first, [the_reflect()]);
        machine.reflect_written(41);
        assert_eq!(machine.reflect_ack(41, 1000), Some(Zone::Reflected));
        assert_eq!(
            machine.reflected(),
            [a_box(ANN), a_box(BOB), a_box(CRE)],
            "every box, in one pass"
        );
        machine
    }

    /// A row at `prepared` writes its reflect and not one box until the reflect-ack's zone committed.
    #[test]
    fn a_prepared_row_writes_its_reflect_and_no_box_before_the_zone_of_the_ack() {
        let (mut machine, first) = Machine::start(
            task(SendState::Prepared, true),
            vec![pending(ANN), pending(BOB)],
            None,
        )
        .expect("a row that runs");
        assert_eq!(first, [the_reflect()], "the reflect alone");
        machine.reflect_written(5);
        assert_eq!(
            machine.reflect_ack(5, 1000),
            Some(Zone::Reflected),
            "the ack allows the zone and no frame"
        );
        assert_eq!(machine.reflected(), [a_box(ANN), a_box(BOB)]);
    }

    /// The ack of another id, or a second ack of the same, moves nothing.
    #[test]
    fn a_reflect_ack_of_another_id_moves_nothing() {
        let (mut machine, _) =
            Machine::start(task(SendState::Prepared, true), vec![pending(ANN)], None)
                .expect("a row");
        assert_eq!(
            machine.reflect_ack(5, 1000),
            None,
            "nothing was written yet"
        );
        machine.reflect_written(5);
        assert_eq!(machine.reflect_ack(6, 1000), None, "another id");
        assert_eq!(machine.reflect_ack(5, 1000), Some(Zone::Reflected));
        assert_eq!(machine.reflect_ack(5, 1000), None, "the same id again");
    }

    /// A row at `reflected` resumes at the boxes it still owes: an acked receiver gets none.
    #[test]
    fn a_reflected_row_resumes_at_the_boxes_it_still_owes() {
        let (machine, first) = Machine::start(
            task(SendState::Reflected, true),
            vec![
                other(ANN, ReceiverState::Acked),
                pending(BOB),
                other(CRE, ReceiverState::Blocked),
            ],
            None,
        )
        .expect("a row that runs");
        assert_eq!(first, [a_box(BOB)], "the one box owed");
        assert_eq!(machine.written.len(), 1);
    }

    /// A row at `acked` writes the stored update and nothing else: every box is durable.
    #[test]
    fn an_acked_row_writes_its_stored_update_and_the_ack_gives_the_time() {
        let (mut machine, first) = Machine::start(
            task(SendState::Acked, true),
            vec![other(ANN, ReceiverState::Acked)],
            None,
        )
        .expect("a row that runs");
        assert_eq!(first, [the_update()]);
        machine.reflect_written(3);
        assert_eq!(
            machine.reflect_ack(3, 1_761_500_000_123),
            Some(Zone::Done {
                sent_at: 1_761_500_000_123
            })
        );
        assert_eq!(machine.done(), [Do::Finished]);
    }

    /// The acks of a pass make one zone; only the pass that leaves nobody pending seals the update, and no frame goes
    /// until that zone wrote it.
    #[test]
    fn only_the_last_ack_seals_the_update_and_nothing_goes_before_its_zone() {
        let mut machine = at_the_boxes(true);
        let (zone, unknown) = machine.message_acks(vec![ack(BOB), ack(ANN)]);
        assert_eq!(
            zone,
            Some(acks_zone(&[BOB, ANN], false)),
            "one zone, no seal"
        );
        assert_eq!(unknown, 0);
        assert_eq!(
            machine.acks_end(SendEnd::Open { pending: 1 }, None),
            [],
            "a receiver is pending: no update, nothing to do"
        );
        let (zone, _) = machine.message_acks(vec![ack(CRE)]);
        assert_eq!(zone, Some(acks_zone(&[CRE], true)), "the last one seals");
        assert_eq!(
            machine.acks_end(SendEnd::Acked, Some(b"the update".to_vec())),
            [the_update()]
        );
        machine.reflect_written(42);
        assert_eq!(
            machine.reflect_ack(42, 2000),
            Some(Zone::Done { sent_at: 2000 })
        );
        assert_eq!(machine.done(), [Do::Finished]);
    }

    /// No update owed: the last zone ends the row.
    #[test]
    fn a_row_that_owes_no_update_ends_with_its_last_zone() {
        for end in [SendEnd::Done, SendEnd::Deleted] {
            let mut machine = at_the_boxes(false);
            let (zone, _) = machine.message_acks(vec![ack(ANN), ack(BOB), ack(CRE)]);
            assert_eq!(zone, Some(acks_zone(&[ANN, BOB, CRE], false)));
            assert_eq!(machine.acks_end(end, None), [Do::Finished], "{end:?}");
        }
    }

    /// An ack of a receiver not written to, of another message, or a second one, is no ack: no zone, and counted.
    #[test]
    fn an_ack_nothing_waits_for_makes_no_zone_and_is_counted() {
        let mut machine = at_the_boxes(true);
        let (zone, unknown) = machine.message_acks(vec![
            ack("PEER0009"),
            (ANN.to_owned(), ID + 1),
            ack(BOB),
            ack(BOB),
        ]);
        assert_eq!(zone, Some(acks_zone(&[BOB], false)), "the one that counts");
        assert_eq!(unknown, 3);
        assert_eq!(
            machine.message_acks(vec![ack("PEER0009")]),
            (None, 1),
            "nothing to write for it"
        );
    }

    /// No box was written before the reflect's zone, so an ack before it is no ack.
    #[test]
    fn a_message_ack_before_the_boxes_is_no_ack() {
        let (mut machine, _) =
            Machine::start(task(SendState::Prepared, true), vec![pending(ANN)], None)
                .expect("a row");
        assert_eq!(machine.message_acks(vec![ack(ANN)]), (None, 1));
    }

    /// A row with nobody to write to (a notes group, or a kill before the zone of no acks) goes on with a zone of no
    /// acks, sealed when it owes an update; at `reflected` as a fresh row or a resumed one alike.
    #[test]
    fn a_row_with_nobody_to_write_to_goes_on_with_a_zone_of_no_acks() {
        let (mut machine, first) =
            Machine::start(task(SendState::Prepared, true), Vec::new(), None).expect("a row");
        assert_eq!(first, [the_reflect()]);
        machine.reflect_written(1);
        assert_eq!(machine.reflect_ack(1, 1000), Some(Zone::Reflected));
        assert_eq!(machine.reflected(), [Do::Zone(acks_zone(&[], true))]);

        let (_, first) = Machine::start(
            task(SendState::Reflected, false),
            vec![other(ANN, ReceiverState::Blocked)],
            None,
        )
        .expect("a row");
        assert_eq!(first, [Do::Zone(acks_zone(&[], false))], "no update owed");
    }

    /// A row the machine cannot go on with: nothing is awaited, so the socket cools it.
    #[test]
    fn a_row_that_contradicts_the_machine_cools() {
        let mut machine = at_the_boxes(true);
        let (zone, _) = machine.message_acks(vec![ack(ANN), ack(BOB), ack(CRE)]);
        assert_eq!(zone, Some(acks_zone(&[ANN, BOB, CRE], true)));
        assert_eq!(
            machine.acks_end(SendEnd::Open { pending: 1 }, None),
            [Do::Cool],
            "the row says pending and this socket awaits nobody"
        );
        assert_eq!(
            machine.acks_end(SendEnd::Acked, None),
            [Do::Cool],
            "no update"
        );
    }

    /// What stands unanswered, step by step: a reflect until its ack, the boxes until the last ack; nothing between the
    /// zones. A box that never went (the socket was going) is no frame unanswered, one written beside it still is.
    #[test]
    fn a_frame_stands_unanswered_until_its_ack_and_a_box_that_never_went_is_none() {
        let (mut machine, _) = Machine::start(
            task(SendState::Prepared, true),
            vec![pending(ANN), pending(BOB)],
            None,
        )
        .expect("a row");
        assert!(!machine.unanswered(), "nothing written yet");
        machine.reflect_written(1);
        assert!(machine.unanswered(), "the reflect");
        assert_eq!(machine.reflect_ack(1, 1000), Some(Zone::Reflected));
        assert!(!machine.unanswered(), "the reflect was answered");
        assert_eq!(machine.reflected(), [a_box(ANN), a_box(BOB)]);
        assert!(machine.unanswered(), "two boxes");
        let (zone, _) = machine.message_acks(vec![ack(ANN)]);
        assert_eq!(zone, Some(acks_zone(&[ANN], false)));
        assert!(machine.unanswered(), "one box");
        machine.box_not_written();
        assert!(!machine.unanswered(), "the other never went");

        let mut machine = at_the_boxes(true);
        machine.box_not_written();
        machine.box_not_written();
        assert!(machine.unanswered(), "one of the three went");
        machine.box_not_written();
        assert!(!machine.unanswered(), "none of the three went");
    }

    /// A row with nothing to run, or without the bytes its state says it has, makes no machine.
    #[test]
    fn a_row_with_nothing_to_run_makes_no_machine() {
        assert!(Machine::start(task(SendState::Done, false), Vec::new(), None).is_none());
        let mut no_reflect = task(SendState::Prepared, true);
        no_reflect.reflect_env = None;
        assert!(Machine::start(no_reflect, Vec::new(), None).is_none());
        let mut no_update = task(SendState::Acked, true);
        no_update.update_env = None;
        assert!(Machine::start(no_update, Vec::new(), None).is_none());
        let empty = SendReceiver {
            identity: ANN.to_owned(),
            state: ReceiverState::Pending,
            payload: None,
        };
        assert!(Machine::start(task(SendState::Reflected, true), vec![empty], None).is_none());
    }

    /// Past `i64::MAX` the time saturates; the row still ends.
    #[test]
    fn a_time_past_the_range_saturates() {
        let (mut machine, _) =
            Machine::start(task(SendState::Acked, true), Vec::new(), None).expect("a row");
        machine.reflect_written(1);
        assert_eq!(
            machine.reflect_ack(1, u64::MAX),
            Some(Zone::Done { sent_at: i64::MAX })
        );
    }

    /// `Debug` of everything a machine holds prints lengths and counts: a box holds an ID, a row an ID and sealed bytes.
    #[test]
    fn debug_prints_counts_and_no_id() {
        let mut machine = at_the_boxes(true);
        let (zone, _) = machine.message_acks(vec![ack(ANN)]);
        let printed = format!(
            "{machine:?} {zone:?} {:?} {:?} {:?}",
            a_box(BOB),
            the_reflect(),
            Do::Zone(acks_zone(&[CRE], true))
        );
        assert!(!printed.contains("PEER"), "an ID in {printed}");
        assert!(!printed.contains("the reflect"), "bytes in {printed}");
        assert!(printed.contains("written: 2"), "the counts in {printed}");
    }

    /// R4, `only`: the boxes of those it names and nobody else's; the update stays sealed while another receiver is pending,
    /// and the run ends with the last of them acked. The row stands for a later attempt.
    #[test]
    fn under_only_the_others_get_no_box_and_the_run_ends_with_no_update() {
        let only = [BOB.to_owned()];
        let (mut machine, first) = Machine::start(
            task(SendState::Reflected, true),
            vec![pending(ANN), pending(BOB), pending(CRE)],
            Some(&only),
        )
        .expect("a row that runs");
        assert_eq!(first, [a_box(BOB)], "the box of the one it names");
        let (zone, unknown) = machine.message_acks(vec![ack(BOB)]);
        assert_eq!(zone, Some(acks_zone(&[BOB], false)), "no seal");
        assert_eq!(unknown, 0);
        assert_eq!(
            machine.acks_end(SendEnd::Open { pending: 2 }, None),
            [Do::Finished],
            "the attempt is through"
        );
    }

    /// `only` that names everyone still pending: the last ack seals, as a plain run does.
    #[test]
    fn under_only_the_last_pending_receiver_seals_the_update() {
        let only = [BOB.to_owned()];
        let (mut machine, first) = Machine::start(
            task(SendState::Reflected, true),
            vec![other(ANN, ReceiverState::Acked), pending(BOB)],
            Some(&only),
        )
        .expect("a row that runs");
        assert_eq!(first, [a_box(BOB)]);
        let (zone, _) = machine.message_acks(vec![ack(BOB)]);
        assert_eq!(zone, Some(acks_zone(&[BOB], true)), "the seal");
    }

    /// `only` that names nobody pending: no box, and the empty zone tells the row is open for the others: the run ends.
    #[test]
    fn under_only_with_nobody_of_it_pending_the_run_writes_nothing() {
        let only = [ANN.to_owned()];
        let (mut machine, first) = Machine::start(
            task(SendState::Reflected, true),
            vec![other(ANN, ReceiverState::Acked), pending(BOB)],
            Some(&only),
        )
        .expect("a row that runs");
        assert_eq!(first, [Do::Zone(acks_zone(&[], false))], "no box, no seal");
        assert_eq!(
            machine.acks_end(SendEnd::Open { pending: 1 }, None),
            [Do::Finished]
        );
    }
}

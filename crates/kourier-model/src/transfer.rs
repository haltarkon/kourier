//! Where the history transfer of a newly linked account stands (architecture §14.2 item 14).
//!
//! Some networks hand a new device the account's history once, right after linking (Signal:
//! the phone uploads one encrypted archive, the new device downloads and reads it). While that
//! runs the backend holds back live receiving, so the account shows nothing new until the
//! transfer is done, skipped or failed. A backend reports every change of state, and the
//! progress of the download, with `BackendEvent::HistoryTransfer`; the user can end the wait
//! or the download with `Command::SkipHistoryTransfer`.
//!
//! A state says where the transfer stands now; it is not a step of a sequence. The backend
//! repeats the stored state at every start and at every connect, also one that is over,
//! until `Command::DismissHistoryTransfer` (the user has seen the end). So the same state
//! can arrive many times, an end can arrive at a start long after the transfer ended, and
//! the bytes of a download can be lower than the last ones seen (a restart counts from the
//! file on disk). Every failure is final in this version: nothing is tried again by itself.

use serde::{Deserialize, Serialize};

/// The state of a history transfer. Numbers only: nothing of the history itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HistoryTransfer {
    /// Waiting for the phone to prepare and upload the history. Can be skipped.
    Waiting,
    /// Downloading what the phone uploaded. Can be skipped.
    Downloading {
        /// Bytes on disk so far.
        done: u64,
        /// The size of the download, once the server stated it.
        #[serde(default)]
        total: Option<u64>,
    },
    /// The download is complete; it is being verified and read. Cannot be skipped.
    Reading,
    /// The history was read to its end. Live receiving starts.
    Done,
    /// No history was transferred, and that is not an error: the phone's user declined, the
    /// phone did not answer in time, the server has no transfer for this device (it refused
    /// the question, or the archive is not there), or the user of this device skipped. Live
    /// receiving starts.
    Skipped,
    /// The transfer failed for good. Live receiving starts all the same.
    Failed(HistoryTransferFailure),
}

impl HistoryTransfer {
    /// Whether the transfer has ended (done, skipped or failed): nothing more follows, and
    /// receiving is no longer held back.
    pub fn is_over(&self) -> bool {
        matches!(self, Self::Done | Self::Skipped | Self::Failed(_))
    }

    /// Whether `Command::SkipHistoryTransfer` can end the transfer in this state.
    pub fn can_skip(&self) -> bool {
        matches!(self, Self::Waiting | Self::Downloading { .. })
    }

    /// The variant name (log-safe).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Waiting => "Waiting",
            Self::Downloading { .. } => "Downloading",
            Self::Reading => "Reading",
            Self::Done => "Done",
            Self::Skipped => "Skipped",
            Self::Failed(_) => "Failed",
        }
    }
}

/// Why a history transfer failed. Each kind is a different thing for the user to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HistoryTransferFailure {
    /// The phone asks for the device to be linked again (it could not make or upload the
    /// history). The UI offers to link again.
    LinkAgain,
    /// The history is in a format this version does not read, or the server answered with
    /// an error this version does not know: update the application.
    UpdateApp,
    /// What was downloaded is not the archive the phone made: damaged, cut short or changed.
    Corrupt,
    /// The history is larger than this device accepts.
    TooLarge,
    /// The history could not be fetched: the server's answer about where it is could not be
    /// read, the download was refused, no byte arrived for too long, or the download as a
    /// whole took too long or stayed too slow.
    Download,
    /// Something on this device failed (the disk, the database).
    Internal,
    /// Only a part of the history was transferred: the import had begun to fill the chats
    /// and could not be finished (what the phone sent could no longer be read, or the rest
    /// could not be stored). What was transferred stays; the UI offers to transfer again.
    Incomplete,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_wait_and_a_download_can_be_skipped_and_three_states_are_final() {
        let states = [
            HistoryTransfer::Waiting,
            HistoryTransfer::Downloading {
                done: 1,
                total: Some(2),
            },
            HistoryTransfer::Reading,
            HistoryTransfer::Done,
            HistoryTransfer::Skipped,
            HistoryTransfer::Failed(HistoryTransferFailure::Corrupt),
        ];
        let skippable: Vec<_> = states.iter().map(HistoryTransfer::can_skip).collect();
        assert_eq!(skippable, [true, true, false, false, false, false]);
        let over: Vec<_> = states.iter().map(HistoryTransfer::is_over).collect();
        assert_eq!(over, [false, false, false, true, true, true]);
    }

    #[test]
    fn a_download_without_a_stated_size_reads_back() {
        let json = r#"{"Downloading":{"done":5}}"#;
        let state: HistoryTransfer = serde_json::from_str(json).unwrap();
        assert_eq!(
            state,
            HistoryTransfer::Downloading {
                done: 5,
                total: None
            }
        );
    }
}

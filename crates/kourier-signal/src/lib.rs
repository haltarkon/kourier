//! The Signal backend: Kourier's own service layer on libsignal v0.104.0 (docs/architecture.md §6).
//!
//! Behavior is ported from Signal Desktop, which is the spec; every module names the Desktop file
//! it follows. The crate's public items are [`SignalNetwork`] (the `Network` implementation the
//! composition root registers) and environment configuration ([`SignalEnvironment`],
//! [`LocalTestEnv`]); protobuf and libsignal types never appear in a public item (§2.3).
//!
//! Modules (§6.2):
//! - `actor`: the per-account thread (current-thread runtime + `LocalSet`) and command loop.
//! - `connection`: the authenticated socket, the receive loop, `Syncing`/`Online`, post-connect.
//! - `provisioning` (+ `provisioning::link`): linking crypto and the link flow (§6.5).
//! - `keys`: prekey generation, upload and the bundles of other users.
//! - `store`: libsignal's protocol stores over `backend.db`; `db`: open, migrate, the account.
//! - `receive`: stage 1 (decrypt + inbox, ack after commit, backpressure), stage 2 (lanes per
//!   conversation, poison rules, events into `core_outbox`) and the content mapping of §6.13;
//!   `outbox`: event rows and the forwarder to the core (with the poison quarantine).
//! - `send`: the 1:1 send path, serialized per recipient: content builders, sealed sender with
//!   sender certificates, the unauthenticated socket, 409/410 repair, sync transcripts;
//!   `msl`: the Message Send Log and per-recipient send progress; `peers`: what we know about
//!   other users (access keys, sealed-sender state, profile sharing); `features`: 1:1
//!   conversation features and the "start this chat on your phone" rule; `commands`: receipts,
//!   read syncs (durable commands) and typing; `padding`, `service_id`, `proto`, `net` as before.
//! - `jobs`: the durable job queue of `backend.db` (§6.12); `maintenance`: remote config,
//!   capabilities, sync requests and the MSL purge, run as jobs.
//! - S9 (contacts and account data, §6.9-§6.11): `attachment` (receive-side `AttachmentPointer`
//!   crypto and CDN downloads), `contact_sync` (the phone's contact list), `storage` (Storage
//!   Service read and merge), `profile` (versioned profile fetches, names, sealed-sender state),
//!   `avatar` (avatar files in the blob store), `crypto` (the AES-GCM of profiles and storage).
//! - S10 (§6.8, §6.6): `groups` (credentials, state and log fetches, decryption, the apply zone
//!   with its system messages, the group lane's network step and failure policy), the sender
//!   key store and SKDM processing (`store`, `receive::stage1`), group content
//!   (`receive::group_dispatch`), and `retry` (retry requests both ways: placeholders, the
//!   `DecryptionErrorMessage`, MSL resends, null messages, session resets).
//! - S15 (robustness, §4.3, §6.4, §6.12): `connection` keeps the authenticated socket alive
//!   (reconnect with backoff and jitter, keepalive, stale sockets after a suspend, 4409, clock
//!   skew, the unlink procedure with `actor`); `keys` also rotates the signed prekey and the
//!   Kyber last-resort key and cleans old keys (the `key_maintenance` job); `challenge` answers
//!   the server's send challenge (428); `number_change` applies our phone's change of number.
//! - S12 (§5.8, §6.7 step 2, §6.11): `attachment` also encrypts (Desktop's padding, digest,
//!   incremental MAC) and downloads into blob files off the network runtime; `upload` (upload
//!   forms, CDN 3 TUS and CDN 2 resumable uploads, the three-day pointer cache); `send::media`
//!   (outgoing bodies: re-encoded images, long texts, pointers, progress); `receive::media`
//!   (pointers → core attachments, stickers, long-text downloads in the lane step).
//! - History transfer (§14.2 item 14): `archive` (the reader of the file the phone uploads)
//!   and `transfer` (the link-time protocol: the stored state, the long-poll, the resumable
//!   download, the read into an importer, and the hold on live receiving until it is over;
//!   and the import up to its commit: `transfer::map` turns frames into rows of the
//!   contract, `transfer::stage` is the private encrypted file they wait in until the
//!   archive was checked to its last byte, `transfer::import` is the importer that joins
//!   the two), and `transfer::deliver` hands the committed stage to the core in small
//!   rows with a durable cursor.

mod actor;
mod archive;
mod attachment;
mod avatar;
mod backfill;
mod challenge;
mod commands;
mod connection;
mod contact_sync;
mod crypto;
mod db;
mod delete_sync;
mod emoji;
mod error;
mod features;
mod groups;
mod jobs;
mod keys;
mod links;
mod maintenance;
mod msl;
mod net;
mod number_change;
mod outbox;
mod padding;
mod peers;
mod profile;
mod progress;
mod proto;
mod provisioning;
mod receive;
mod retry;
mod send;
mod service_id;
mod storage;
mod store;
mod timer;
mod transfer;
mod upload;
mod wire_b64;

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, AccountHandle, AllocatedId, Network, spawn_account_thread,
};
use kourier_model::{
    BackendError, ConvKey, HistorySource, LoginFlow, MsgIdScope, NetworkDescriptor, NetworkKind,
    TypingPolicy, UserId,
};

pub use net::{LocalTestEnv, SignalEnvironment};

/// Archives of Signal's history transfer for other crates' tests (feature `testing`). Only
/// plain data crosses this boundary: no protobuf or libsignal type (§2.3).
#[cfg(feature = "testing")]
pub mod archive_testing {
    use std::io;
    use std::path::Path;

    use uuid::Uuid;

    /// Writes a small sample backup (an account, the note-to-self, one contact, one chat and
    /// three messages) to `path`, encrypted the way the phone encrypts the archive it uploads at
    /// link time, for a device linked as account `own_aci` with `ephemeral_backup_key` in its
    /// provisioning message.
    pub fn write_sample_archive(
        path: &Path,
        ephemeral_backup_key: &[u8; 32],
        own_aci: Uuid,
    ) -> io::Result<()> {
        crate::archive::writer::write_sample(path, ephemeral_backup_key, own_aci.into())
    }

    /// What [`write_archive`] writes beyond the sample backup.
    #[derive(Clone, Copy, Debug)]
    pub struct ArchiveSpec {
        /// More text messages in the sample chat, each its own frame (thousands make an
        /// archive of some megabytes: their texts do not compress).
        pub extra_messages: u32,
        /// The format version the header states (the reader knows only 1).
        pub version: u64,
        /// After this many frames, one that is not a protobuf message, then the rest: the
        /// archive is authentic and fails half-way through its read.
        pub broken_after: Option<u32>,
    }

    impl Default for ArchiveSpec {
        fn default() -> Self {
            Self {
                extra_messages: 0,
                version: 1,
                broken_after: None,
            }
        }
    }

    /// The frames a reader gets out of the archive [`write_archive`] writes for
    /// `extra_messages` (the header is not counted).
    pub fn frames_of(extra_messages: u32) -> u64 {
        crate::archive::writer::sample_frames().len() as u64 + u64::from(extra_messages)
    }

    /// Writes the sample backup shaped by `spec` to `path`, encrypted as
    /// [`write_sample_archive`] does.
    pub fn write_archive(
        path: &Path,
        ephemeral_backup_key: &[u8; 32],
        own_aci: Uuid,
        spec: &ArchiveSpec,
    ) -> io::Result<()> {
        crate::archive::writer::write_shaped(path, ephemeral_backup_key, own_aci.into(), spec)
    }

    pub use crate::archive::writer::{
        AttachmentSpec, EncryptedAttachment, IntegritySpec, LocatorSpec, PointerSpec,
        encrypt_attachment,
    };
    pub use crate::archive::writer::{
        CallSpec, ChatSpec, ChatWith, ContactSpec, GeneratedSpec, GroupCallSpec, GroupChangeSpec,
        GroupSpec, HistorySpec, IndividualCallSpec, ItemSpec, NoticeSpec, Party, QuoteSpec,
        RangeKindSpec, RangeSpec, ReactionSpec, SendStatusSpec, SimpleSpec, SnapshotSpec, TextSpec,
        Who, generated_aci,
    };

    /// Writes the history `spec` describes (people, chats, text messages) to `path`,
    /// encrypted as [`write_sample_archive`] does. Returns how many frames a reader gets
    /// from the archive when it is whole (the header is not counted).
    pub fn write_history_archive(
        path: &Path,
        ephemeral_backup_key: &[u8; 32],
        own_aci: Uuid,
        spec: &HistorySpec,
    ) -> io::Result<u64> {
        crate::archive::writer::write_history(path, ephemeral_backup_key, own_aci.into(), spec)
    }

    /// The name of the import's stage file inside an account's `transfer` directory; its
    /// write-ahead log is this name with `-wal`.
    pub const STAGE_FILE: &str = crate::transfer::stage::FILE;

    /// The database key of the stage of the transfer whose ephemeral backup key is given:
    /// for a test that decrypts the stage's pages itself.
    pub fn stage_key(ephemeral_backup_key: &[u8; 32]) -> [u8; 32] {
        crate::transfer::stage::StageKey::derive(ephemeral_backup_key).bytes()
    }

    /// What a committed stage holds, read back the way delivery will read it.
    #[derive(Clone, Debug)]
    pub struct StageDump {
        /// The frames of the archive it was read from.
        pub frames: u64,
        /// Its messages, over all chats.
        pub messages: u64,
        /// Its chats that delivery shows: with at least one row that makes a chat show, or
        /// pinned.
        pub shown_chats: u64,
        /// The user ids of the people it holds, in the order they were staged.
        pub contacts: Vec<String>,
        /// Its chats, the most recent first.
        pub chats: Vec<StagedChatDump>,
    }

    /// One chat of a [`StageDump`].
    #[derive(Clone, Debug)]
    pub struct StagedChatDump {
        /// The conversation key in its text form (`direct:aci:…`).
        pub conv: String,
        /// The chat is Note to Self.
        pub is_self: bool,
        /// Rows counted for it at the commit.
        pub rows: u64,
        /// Of those, rows that make a chat show.
        pub loud: u64,
        /// Its rows that say "unread" (`read: false` on a message somebody else wrote, or
        /// on a call line of a missed or a ringing call): what the core will count for the
        /// chat. Counted here from the rows; the stage keeps no such counter.
        pub unread: u64,
        /// The newest sort time of its rows (ms).
        pub last_at: i64,
        /// The phone's pinned order (`Chat.pinnedOrder`, not 0).
        pub pinned: Option<u32>,
        /// Archived on the phone.
        pub archived: bool,
        /// Marked unread on the phone.
        pub marked_unread: bool,
        /// Muted until (ms; `i64::MAX`: for ever).
        pub muted_until: Option<i64>,
        /// The 1:1 disappearing timer in seconds.
        pub timer_secs: Option<u32>,
        /// The version of that timer (at least 1).
        pub timer_version: u32,
        /// Delivery shows the chat (a loud row, pinned, or a group we are invited to).
        pub shown: bool,
        /// For a group's chat, what the stage holds of the group.
        pub group: Option<StagedGroupDump>,
        /// Its rows, in the order of their frames.
        pub messages: Vec<kourier_backend_api::IncomingMessage>,
    }

    /// The group of a [`StagedChatDump`] (no master key).
    #[derive(Clone, Debug)]
    pub struct StagedGroupDump {
        /// The title.
        pub title: Option<String>,
        /// The members with their roles (`None`: an empty snapshot).
        pub members: Option<Vec<kourier_model::Member>>,
        /// Our place: "member", "invited", "requesting", "left" or "unknown".
        pub membership: &'static str,
        /// Blocked on the phone.
        pub blocked: bool,
        /// Accepted on the phone.
        pub whitelisted: bool,
    }

    /// Whether the row says "unread", the way the core will count it: a message somebody
    /// else wrote, or a call line of a missed call (or of a group call somebody started)
    /// that the phone had not seen. A line that still RINGS in the archive counts too: it
    /// has no read state, and the core settles it at once as an unread missed call.
    fn says_unread(m: &kourier_backend_api::IncomingMessage) -> bool {
        use kourier_model::{Body, CallState, SystemEvent};
        !m.read
            && match &m.body {
                Body::System(SystemEvent::Call(line)) => {
                    line.counts_unread() || matches!(line.state, CallState::Ringing { .. })
                }
                Body::System(_) => false,
                _ => !m.origin.is_own(),
            }
    }

    /// Reads the committed stage in `transfer_dir` with the key derived from
    /// `ephemeral_backup_key`. Fails when there is none, when it was never committed, when
    /// the key is another transfer's, and when its own counts differ from its totals.
    pub fn read_stage(
        transfer_dir: &Path,
        ephemeral_backup_key: &[u8; 32],
    ) -> io::Result<StageDump> {
        use crate::transfer::stage::{StageKey, StageReader};
        let other = |e: crate::transfer::stage::StageError| io::Error::other(e.to_string());
        let reader = StageReader::open(transfer_dir, &StageKey::derive(ephemeral_backup_key))
            .map_err(other)?;
        let totals = reader.totals().map_err(other)?;
        if reader.count().map_err(other)? != totals {
            return Err(io::Error::other("the stage's rows differ from its totals"));
        }
        let mut contacts = Vec::new();
        let mut after = None;
        loop {
            let page = reader.contacts(after, 500).map_err(other)?;
            let Some((last, _)) = page.last() else {
                break;
            };
            after = Some(*last);
            contacts.extend(page.into_iter().map(|(_, c)| c.user.as_str().to_owned()));
        }
        let mut chats = Vec::new();
        for chat in reader.chats().map_err(other)? {
            let mut messages = Vec::new();
            let mut after = None;
            loop {
                let rows = reader
                    .items(chat.counters.idx, after, 500, 1024 * 1024)
                    .map_err(other)?;
                let Some(last) = rows.last() else {
                    break;
                };
                after = Some(last.n);
                messages.extend(rows.into_iter().map(|row| row.msg));
            }
            chats.push(StagedChatDump {
                conv: chat.conv.encode(),
                is_self: chat.op.is_self(),
                rows: chat.counters.n_rows,
                loud: chat.counters.n_loud,
                unread: messages.iter().filter(|m| says_unread(m)).count() as u64,
                last_at: chat.counters.last_at,
                pinned: chat.pinned,
                archived: chat.op.frame.archived,
                marked_unread: chat.op.frame.marked_unread,
                muted_until: chat.op.frame.muted_until,
                timer_secs: chat.op.frame.timer_secs,
                timer_version: chat.op.frame.timer_version,
                shown: chat.shown(),
                group: chat.op.group().map(|group| {
                    use crate::transfer::stage::GroupMembership;
                    StagedGroupDump {
                        title: group.title.clone(),
                        members: group.members.clone(),
                        membership: match group.membership {
                            GroupMembership::Member => "member",
                            GroupMembership::Invited => "invited",
                            GroupMembership::Requesting => "requesting",
                            GroupMembership::Left => "left",
                            GroupMembership::Unknown => "unknown",
                        },
                        blocked: group.blocked,
                        whitelisted: group.whitelisted,
                    }
                }),
                messages,
            });
        }
        Ok(StageDump {
            frames: reader.frames().map_err(other)?,
            messages: totals.messages,
            shown_chats: totals.shown_chats,
            contacts,
            chats,
        })
    }

    /// The delivery rows of the history archive at `archive`, as a delivery appends them
    /// (people, the chats that are shown most recent first, then each chat's batches),
    /// made without an account: archive → mapper
    /// → stage (in `work_dir`, which must exist) → feeder. No features are put in front of
    /// a chat and nothing is dropped for being due. For tests that feed a core directly.
    pub fn delivery_rows(
        archive: &Path,
        ephemeral_backup_key: &[u8; 32],
        own_aci: Uuid,
        own_pni: Uuid,
        now_ms: i64,
        work_dir: &Path,
    ) -> io::Result<Vec<Vec<kourier_backend_api::BackendEvent>>> {
        use crate::transfer::stage::StageKey;
        let key = zeroize::Zeroizing::new(*ephemeral_backup_key);
        let context = crate::transfer::ImportContext {
            dir: work_dir.to_owned(),
            key: key.clone(),
            aci: own_aci.into(),
            pni: own_pni.into(),
            now: now_ms,
            queue_time: None,
            receive_limit: crate::attachment::DEFAULT_MAX_RECEIVE_BYTES,
        };
        let archive_key = crate::archive::ArchiveKey::for_transfer(&key, own_aci.into());
        crate::transfer::import_archive(
            archive,
            &archive_key,
            crate::archive::Limits::default(),
            crate::transfer::history_importer(context),
            &std::sync::atomic::AtomicBool::new(false),
            std::time::Duration::ZERO,
        )
        .map_err(|_| io::Error::other("the archive could not be read into a stage"))?;
        crate::transfer::deliver::plain_rows(
            work_dir,
            &StageKey::derive(ephemeral_backup_key),
            crate::transfer::deliver::DeliveryBounds::DEFAULT,
        )
        .map_err(|e| io::Error::other(e.to_string()))
    }

    #[cfg(test)]
    mod tests {
        use kourier_backend_api::IncomingMessage;
        use kourier_model::{Body, CallDirection, CallLine, CallState, ConvKey, Origin, UserId};

        use super::says_unread;

        const AT: i64 = 1_700_000_000_000;

        fn row(body: Body, origin: Origin, read: bool) -> IncomingMessage {
            let conv =
                ConvKey::Direct(UserId::parse("aci:21212121-2121-2121-2121-212121212121").unwrap());
            let mut msg = IncomingMessage::new(conv, "1", origin, body, AT, AT);
            msg.read = read;
            msg
        }

        fn call(
            group: bool,
            direction: CallDirection,
            state: CallState,
            read: bool,
        ) -> IncomingMessage {
            let conv = if group {
                ConvKey::Group("gv2:AAAA".into())
            } else {
                ConvKey::Direct(UserId::parse("aci:21212121-2121-2121-2121-212121212121").unwrap())
            };
            let line = CallLine::signal(direction, group, state, AT);
            let mut msg = IncomingMessage::call(
                conv,
                "5",
                Origin::OwnOtherDevice { device: None },
                line,
                AT,
                AT,
            );
            msg.read = read;
            msg
        }

        /// What the dump counts as unread is what the core will count: a ring that is
        /// still on in the archive is settled at once as an unread missed call (S3a), so
        /// it is counted, if the archive says it was not read.
        #[test]
        fn the_dump_counts_the_rows_the_core_counts_unread() {
            use CallDirection::Incoming;
            let peer = Origin::Remote {
                author: UserId::parse("aci:21212121-2121-2121-2121-212121212121").unwrap(),
                device: None,
            };
            let ringing = CallState::Ringing { deadline: AT };
            let cases = [
                (
                    "ringing, unread",
                    call(true, Incoming, ringing, false),
                    true,
                ),
                ("ringing, read", call(true, Incoming, ringing, true), false),
                (
                    "missed, unread",
                    call(false, Incoming, CallState::Missed, false),
                    true,
                ),
                (
                    "missed, read",
                    call(false, Incoming, CallState::Missed, true),
                    false,
                ),
                (
                    "started, unread",
                    call(true, Incoming, CallState::Started, false),
                    true,
                ),
                (
                    "answered",
                    call(false, Incoming, CallState::Answered, false),
                    false,
                ),
                (
                    "a text of the peer, unread",
                    row(Body::text("hi"), peer.clone(), false),
                    true,
                ),
                (
                    "a text of the peer, read",
                    row(Body::text("hi"), peer, true),
                    false,
                ),
                (
                    "a text of ours",
                    row(
                        Body::text("hi"),
                        Origin::OwnOtherDevice { device: None },
                        false,
                    ),
                    false,
                ),
            ];
            for (what, msg, unread) in cases {
                assert_eq!(says_unread(&msg), unread, "{what}");
            }
        }
    }
}

use crate::net::SignalEnv;
use crate::net::libsignal::LibsignalTransport;

/// The `tracing` target of the measuring marks (architecture §1.3). A mark is an event at
/// `TRACE` level with a `name` field (one of the `PERF_*` names) and unsigned numbers, never a
/// text of a message or a contact, so a measuring build can turn the target on
/// (`kourier::perf=trace`) and feed the marks to `kourier_app::trace` without any risk for
/// what the log may hold (§9.1). Nothing is formatted or sent while the target is off.
pub const PERF_TARGET: &str = "kourier::perf";

/// The mark `recv.stored`: a stage-1 batch is durable in the inbox, before its acks (§6.6).
/// Fields: `envelopes` (how many the batch held), `waited_us` (from the arrival of its first
/// envelope at the account actor to this point: the batching wait of up to 75 ms, the
/// decryption and the commit), `backlog` (1 while the account still reads the server's queue,
/// 0 for a live message). The start of "receive → notification" is `waited_us` before the
/// mark.
pub const PERF_STAGE1_STORED: &str = "recv.stored";

/// The mark `link.registered`: a link is stored (the account, its keys and, when the phone
/// sent a key for it, the history transfer). Fields, in microseconds: `taken_us` (from the
/// phone's provisioning message taken off its socket to the start of the link's own work:
/// its decryption, the check of the account, the "Linking" step), `keys_us` (making the
/// signed and the post-quantum prekeys of both identities and the link request),
/// `connect_us` (opening the unauthenticated socket), `request_us` (`PUT /v1/devices/link`
/// until its answer), `store_us` (closing that socket and the transaction that stores the
/// account), `linked_us` (from the phone's provisioning message to the answer of the link
/// request: what the phone waits for, 60 s at most on Android; the sum of the four before
/// `store_us`) and `total_us` (the same up to the stored account). The QR sockets that are
/// still open are closed beside all this, and their closing is in none of the numbers.
pub const PERF_LINK_REGISTERED: &str = "link.registered";

/// Failpoint names of this backend (`KOURIER_FAILPOINT=<name>[:<n>]`, §10.4), for tests.
pub mod failpoints {
    /// After a stage-1 commit, before any server ack.
    pub const AFTER_INBOX_COMMIT: &str = crate::receive::FAILPOINT_AFTER_INBOX_COMMIT;
    /// Before each server ack.
    pub const BEFORE_ACK: &str = crate::receive::FAILPOINT_BEFORE_ACK;
    /// After a stage-2 (or job) zone committed, its events still in `core_outbox`.
    pub const AFTER_BACKEND_EFFECTS_COMMIT: &str =
        crate::receive::FAILPOINT_AFTER_BACKEND_EFFECTS_COMMIT;
    /// After the core acked a forwarded batch, before the forwarder deletes its rows (the
    /// forwarder of `kourier-db`).
    pub const AFTER_CORE_COMMIT: &str = kourier_db::outbox::FAILPOINT_AFTER_CORE_COMMIT;
    /// After an encrypt zone committed, before the ciphertext leaves.
    pub const AFTER_ENCRYPT_COMMIT: &str = crate::send::FAILPOINT_AFTER_ENCRYPT_COMMIT;
    /// After the encrypt zone of an edit, a delete for everyone or a reaction committed, before
    /// its ciphertext leaves (hit by nothing else).
    pub const AFTER_MODIFIER_ENCRYPT_COMMIT: &str =
        crate::send::FAILPOINT_AFTER_MODIFIER_ENCRYPT_COMMIT;
    /// After a durable command's job and its `CommandPersisted` committed, before the job runs.
    pub const AFTER_COMMAND_PERSISTED: &str = crate::commands::FAILPOINT_AFTER_COMMAND_PERSISTED;
    /// After one Storage Service merge zone committed, before the next one (§6.9).
    pub const AFTER_STORAGE_MERGE_COMMIT: &str =
        crate::storage::FAILPOINT_AFTER_STORAGE_MERGE_COMMIT;
    /// History import: a read of the archive is about to begin. A stage that no commit
    /// recorded is removed, the read's start is not counted yet.
    pub const BEFORE_HISTORY_IMPORT_READ: &str = crate::transfer::FAILPOINT_BEFORE_IMPORT_READ;
    /// History import: the importer left a whole stage, the commit zone has not recorded it.
    pub const BEFORE_HISTORY_IMPORT_COMMIT: &str = crate::transfer::FAILPOINT_BEFORE_IMPORT_COMMIT;
    /// History import: right after the commit zone recorded the stage.
    pub const AFTER_HISTORY_IMPORT_COMMIT: &str = crate::transfer::FAILPOINT_AFTER_IMPORT_COMMIT;
    /// History delivery: a chunk of the stage is about to be appended; nothing of it is
    /// written.
    pub const BEFORE_HISTORY_DELIVERY_CHUNK: &str =
        crate::transfer::deliver::FAILPOINT_BEFORE_CHUNK;
    /// History delivery: right after a chunk's zone (its `core_outbox` row, its entry and
    /// the cursor are durable).
    pub const AFTER_HISTORY_DELIVERY_CHUNK: &str = crate::transfer::deliver::FAILPOINT_AFTER_CHUNK;
    /// History delivery: INSIDE a chunk's zone, after its row and its entry are written and
    /// before the cursor is. Nothing is durable yet: a kill here leaves nothing of the
    /// chunk. Hit once per chunk that has a row.
    pub const INSIDE_HISTORY_DELIVERY_CHUNK: &str =
        crate::transfer::deliver::FAILPOINT_INSIDE_CHUNK;
    /// History delivery: at the commit of a chunk's zone, from the commit's own callback and
    /// before the zone returns. The row, its entry and the cursor are durable together; a
    /// cursor written in a second zone would not be. Hit once per chunk that has a row.
    pub const AT_HISTORY_DELIVERY_COMMIT: &str = crate::transfer::deliver::FAILPOINT_AT_COMMIT;
    /// History delivery: right after the zone that settled acknowledged rows.
    pub const AFTER_HISTORY_DELIVERY_SETTLE: &str =
        crate::transfer::deliver::FAILPOINT_AFTER_SETTLE;
    /// History delivery: every row is acknowledged, `done` is not written yet.
    pub const BEFORE_HISTORY_DELIVERY_DONE: &str = crate::transfer::deliver::FAILPOINT_BEFORE_DONE;
}

/// Timings of the account actor that tests may shorten (production uses Desktop's).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tuning {
    /// How often linking shows a new QR code (Desktop: 45 s).
    pub(crate) qr_rotation: Duration,
    /// How long after an answered keepalive the next one goes out (Desktop: 30 s).
    pub(crate) keepalive_interval: Duration,
    /// How often a clock found too far from the server's is looked at again (Desktop: 10 s).
    pub(crate) clock_recheck: Duration,
    /// How a download goes on after its transfer broke off (§6.11).
    pub(crate) download_resume: attachment::ResumePolicy,
    /// How long an upload may move no byte before it counts as stalled and is resumed
    /// (§6.7 step 2).
    pub(crate) upload_stall: Duration,
    /// How long the phone's upload of a history transfer is waited for, from the link
    /// (Desktop: an hour).
    pub(crate) transfer_wait: Duration,
    /// The longest one long-poll for the transfer archive asks the server to hold the
    /// request (Desktop: five minutes).
    pub(crate) transfer_poll: Duration,
    /// The largest transfer archive that is downloaded.
    pub(crate) transfer_max_bytes: u64,
    /// How long the archive's download may go without a new byte before it fails for good.
    pub(crate) transfer_stall: Duration,
    /// A pause after every frame of the archive's read (zero in the app; a test that stops
    /// the account while it reads needs the read to last).
    pub(crate) transfer_frame_delay: Duration,
    /// A pause between the importer's commit and the end of the read's task (zero in the
    /// app; the test of a stop that reaches a read past its last frame needs that moment
    /// to last).
    pub(crate) transfer_commit_pause: Duration,
    /// Whether a fresh link offers the history transfer (`backup5` in the linking URL).
    /// OFF in the library (`SignalNetwork::new()`), because tests build networks directly;
    /// the application turns it ON where it builds its network
    /// (`with_history_transfer_offer(true)`, `kourier_app::signal_network`), as Signal
    /// Desktop offers it (architecture §14.2 item 14). With it off the URL carries no
    /// capability and a key the phone sends all the same is ignored.
    pub(crate) transfer_offer: bool,
    /// The bounds of the archive's download as a whole (its deadline, its slowest rate).
    pub(crate) transfer_download: transfer::DownloadBounds,
    /// A pause after the downloaded archive was synced to disk (zero in the app; a test of
    /// a skip that arrives during the sync needs the sync to last).
    pub(crate) transfer_sync_delay: Duration,
    /// Whether the write that ends a transfer is followed by the checkpoint that removes the
    /// key from the WAL (always in the app; off in the test of a kill between the two).
    pub(crate) transfer_end_checkpoint: bool,
    /// What makes the importer of a read of the transfer archive: the one that maps the
    /// frames into the stage. A seam for this crate's own tests; no public method sets it.
    pub(crate) transfer_importer: transfer::ImporterFactory,
    /// The bounds of the delivery of a committed import to the core (row sizes, the window,
    /// when an unacknowledged row is taken back).
    pub(crate) transfer_delivery: transfer::deliver::DeliveryBounds,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            qr_rotation: provisioning::link::ROTATION_INTERVAL,
            keepalive_interval: connection::KEEPALIVE_INTERVAL,
            clock_recheck: connection::CLOCK_RECHECK,
            download_resume: attachment::ResumePolicy::DEFAULT,
            upload_stall: upload::STALL_TIMEOUT,
            transfer_wait: transfer::WAIT,
            transfer_poll: transfer::POLL,
            transfer_max_bytes: transfer::MAX_ARCHIVE_BYTES,
            transfer_stall: transfer::STALL,
            transfer_frame_delay: Duration::ZERO,
            transfer_commit_pause: Duration::ZERO,
            transfer_offer: false,
            transfer_download: transfer::DownloadBounds::DEFAULT,
            transfer_sync_delay: Duration::ZERO,
            transfer_end_checkpoint: true,
            transfer_importer: transfer::history_importer,
            transfer_delivery: transfer::deliver::DeliveryBounds::DEFAULT,
        }
    }
}

/// The Signal network: descriptor, message ids and the account actor factory (§4.2).
pub struct SignalNetwork {
    descriptor: NetworkDescriptor,
    tuning: Tuning,
}

impl SignalNetwork {
    /// The Signal backend. Its environment (production, staging, a local test server) comes
    /// from each account's `BackendConfig::env`.
    pub fn new() -> Self {
        Self {
            descriptor: NetworkDescriptor {
                display_name: "Signal".to_owned(),
                icon_name: "org.signal.Signal".to_owned(),
                login_flows: vec![LoginFlow {
                    id: "qr".to_owned(),
                    label: "Link with a QR code".to_owned(),
                }],
                // Phone-number and username lookup (CDSI, usernames) arrive after S2.
                start_chat: Vec::new(),
                history: HistorySource::None,
                msg_id_scope: MsgIdScope::AccountAuthor,
                typing: TypingPolicy {
                    refresh: Duration::from_secs(10),
                    stop_after: Duration::from_secs(3),
                },
                risk_notice: None,
            },
            tuning: Tuning::default(),
        }
    }

    /// Shows a new QR code every `every` instead of Desktop's 45 s. For tests that exercise QR
    /// rotation against a local server; the app keeps the default.
    #[must_use]
    pub fn with_qr_rotation(mut self, every: Duration) -> Self {
        self.tuning.qr_rotation = every;
        self
    }

    /// Sends the connection keepalive every `every` instead of Desktop's 30 s. For tests that
    /// watch keepalives against a local server; the app keeps the default.
    #[must_use]
    pub fn with_keepalive_interval(mut self, every: Duration) -> Self {
        self.tuning.keepalive_interval = every;
        self
    }

    /// Looks at a wrong system clock again every `every` instead of Desktop's 10 s. For tests
    /// of the clock check against a local server; the app keeps the default.
    #[must_use]
    pub fn with_clock_recheck(mut self, every: Duration) -> Self {
        self.tuning.clock_recheck = every;
        self
    }

    /// Waits `pause` instead of a second before a download that broke off and made no
    /// progress is asked for again (doubled per fruitless request). For tests that break
    /// transfers against a local server; the app keeps the default.
    #[must_use]
    pub fn with_download_resume_pause(mut self, pause: Duration) -> Self {
        self.tuning.download_resume.pause = pause;
        self
    }

    /// Counts an upload as stalled after `after` without a byte moving, instead of a minute.
    /// For tests that stall transfers against a local server; the app keeps the default.
    #[must_use]
    pub fn with_upload_stall(mut self, after: Duration) -> Self {
        self.tuning.upload_stall = after;
        self
    }

    /// Waits `wait` in all for the phone's upload of a history transfer instead of an hour,
    /// asking the server to hold each long-poll for `poll` (whole seconds, at least one)
    /// instead of five minutes. For tests against a local server; the app keeps the defaults.
    #[must_use]
    pub fn with_history_transfer_wait(mut self, wait: Duration, poll: Duration) -> Self {
        self.tuning.transfer_wait = wait;
        self.tuning.transfer_poll = poll;
        self
    }

    /// Downloads a history transfer archive of at most `max_bytes` instead of 8 GiB, and
    /// gives a download up after `stall` without a new byte instead of an hour. For tests
    /// against a local server; the app keeps the defaults.
    #[must_use]
    pub fn with_history_transfer_limits(mut self, max_bytes: u64, stall: Duration) -> Self {
        self.tuning.transfer_max_bytes = max_bytes;
        self.tuning.transfer_stall = stall;
        self
    }

    /// Pauses for `delay` after every frame while a history transfer archive is read. For
    /// tests that stop the account in the middle of the read; the app reads without a pause.
    #[must_use]
    pub fn with_history_transfer_frame_delay(mut self, delay: Duration) -> Self {
        self.tuning.transfer_frame_delay = delay;
        self
    }

    /// Whether a fresh link offers the phone the history transfer (`backup5` in the linking
    /// URL). Off in the library, which has no way to know who builds it: the APP switches it
    /// on (`kourier_app` does, for every new link, unless the run's environment says
    /// `KOURIER_HISTORY_TRANSFER=0`; architecture §14.2 item 14), and the tests that build a
    /// network directly (the mock harness of `kourier-e2e`, this crate's unit tests) keep
    /// their links free of a transfer unless they are about it. With it off the linking URL
    /// carries no capability, and a key a phone sends all the same is ignored. Every test of
    /// the transfer switches it on.
    ///
    /// The descriptor follows the switch: with the offer on it says that linking brings the
    /// account's history (`HistorySource::PushedOnLink`), which is what makes a UI offer
    /// "Transfer History from Phone…" for an account that is linked already.
    #[must_use]
    pub fn with_history_transfer_offer(mut self, offer: bool) -> Self {
        self.tuning.transfer_offer = offer;
        self.descriptor.history = if offer {
            HistorySource::PushedOnLink
        } else {
            HistorySource::None
        };
        self
    }

    /// Bounds the download of a history transfer archive as a whole: it fails when it took
    /// longer than `deadline` in all (instead of two hours), or when, once it is older than
    /// `rate_grace` (a minute), its average rate since it began stayed under `min_rate`
    /// bytes per second (4 KiB/s) for `slow_for` (ten minutes). For tests against a local
    /// server; the app keeps the defaults.
    #[must_use]
    pub fn with_history_transfer_download_bounds(
        mut self,
        deadline: Duration,
        min_rate: u64,
        rate_grace: Duration,
        slow_for: Duration,
    ) -> Self {
        self.tuning.transfer_download = transfer::DownloadBounds {
            deadline,
            min_rate,
            rate_grace,
            slow_for,
        };
        self
    }

    /// Pauses for `delay` after a downloaded history transfer archive was synced to disk,
    /// as a slow disk would. For the test of a skip that arrives during the sync; the app
    /// does not pause.
    #[must_use]
    pub fn with_history_transfer_sync_delay(mut self, delay: Duration) -> Self {
        self.tuning.transfer_sync_delay = delay;
        self
    }

    /// Delivers a history import in rows of at most `max_messages` messages (never more
    /// than the contract's 500) and `max_bytes` staged bytes instead of 500 and 1 MiB. For
    /// tests against a local server; the app keeps the defaults, and a build without the
    /// `testing` feature does not have this switch.
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn with_history_delivery_rows(mut self, max_messages: usize, max_bytes: u64) -> Self {
        self.tuning.transfer_delivery.max_messages = max_messages;
        self.tuning.transfer_delivery.max_bytes = max_bytes;
        self
    }

    /// Bounds the delivery of a history import: a row found unacknowledged at more than
    /// `max_starts` delivery starts (3), and every unacknowledged row after `idle` of
    /// running time without any acknowledgement (ten minutes; stored every `stored_every`),
    /// is taken back and appended again as single messages; `max_strikes` takings back in a
    /// row (3) end the transfer as incomplete. For tests against a local server; the app
    /// keeps the defaults, and a build without the `testing` feature does not have this
    /// switch (it could switch the bounds off).
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn with_history_delivery_bounds(
        mut self,
        max_starts: u32,
        idle: Duration,
        stored_every: Duration,
        max_strikes: u32,
    ) -> Self {
        self.tuning.transfer_delivery.max_starts = max_starts;
        self.tuning.transfer_delivery.idle = idle;
        self.tuning.transfer_delivery.idle_stored_every = stored_every;
        self.tuning.transfer_delivery.max_strikes = max_strikes;
        self
    }

    /// The two bounds of a whole delivery that nothing resets: `max_refused` single units
    /// the core refused in a row (20) and `stuck`, the total running time the delivery may
    /// spend waiting for acknowledgements that came no more (an hour), after which the
    /// transfer ends as incomplete. For tests against a local server; the app keeps the
    /// defaults, and a build without the `testing` feature does not have this switch.
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn with_history_delivery_totals(mut self, max_refused: u32, stuck: Duration) -> Self {
        self.tuning.transfer_delivery.max_refused = max_refused;
        self.tuning.transfer_delivery.stuck = stuck;
        self
    }

    /// Pauses for `pause` after the importer of a history transfer committed its stage,
    /// before the read's task ends: the moment in which the stage is whole and nothing has
    /// recorded it. Only for the test of a stop that falls into that moment; the app does
    /// not pause, and a build without the `testing` feature does not have this switch.
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn with_history_transfer_commit_pause(mut self, pause: Duration) -> Self {
        self.tuning.transfer_commit_pause = pause;
        self
    }

    /// Leaves out the checkpoint that follows the end of a history transfer, as a process
    /// does that is killed between the two. Only for the test that proves the next start
    /// removes the key from the write-ahead log; the app always checkpoints, and a build
    /// without the `testing` feature (every release) does not have this switch.
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn without_history_transfer_end_checkpoint(mut self) -> Self {
        self.tuning.transfer_end_checkpoint = false;
        self
    }
}

impl Default for SignalNetwork {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SignalNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SignalNetwork")
    }
}

impl Network for SignalNetwork {
    fn kind(&self) -> NetworkKind {
        NetworkKind::Signal
    }

    fn descriptor(&self) -> &NetworkDescriptor {
        &self.descriptor
    }

    /// Signal's message identity is author + sent timestamp (§3.1): the local part is the
    /// timestamp in ms, which is also `sent_at`.
    fn allocate_message_id(&self, _own: &[UserId], _conv: &ConvKey, now_ms: u64) -> AllocatedId {
        AllocatedId {
            local: now_ms.to_string().into(),
            sent_at: i64::try_from(now_ms).unwrap_or(i64::MAX),
        }
    }

    /// A Signal attachment is named by the `clientUuid` of its pointer, which Kourier derives
    /// from the message id and the attachment's position (`send::media::client_uuid`), so it is
    /// known before the upload.
    fn attachment_net_ids(&self, local: &str, index: usize) -> Vec<String> {
        send::media::attachment_net_ids(local, index)
    }

    fn spawn(&self, ctx: AccountContext) -> Result<AccountHandle, BackendError> {
        let environment = SignalEnvironment::from_backend_env(&ctx.config.env).map_err(|e| {
            BackendError::new(kourier_model::ErrorKind::InvalidInput, e.to_string())
        })?;
        let env = SignalEnv::resolve(&environment).map_err(|e| {
            BackendError::new(kourier_model::ErrorKind::InvalidInput, e.to_string())
        })?;
        let trust_roots = env.trust_roots.clone();
        let zk_params = env.zk_params.clone();
        // libsignal-net's resolver and connectors belong to the network runtime.
        let transport = {
            let _runtime = ctx.net_rt.enter();
            LibsignalTransport::new(env, ctx.net_rt.clone())
                .map_err(|e| BackendError::internal(e.to_string()))?
        };
        let name = format!("signal-{}", ctx.uuid.short());
        let tuning = self.tuning;
        spawn_account_thread(name, move || {
            actor::run(ctx, Arc::new(transport), trust_roots, zk_params, tuning)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_ids_are_the_sent_timestamp() {
        let network = SignalNetwork::new();
        let conv =
            ConvKey::Direct(UserId::parse("aci:9d0652a3-dcc3-4d11-975f-74d61598733f").unwrap());
        let id = network.allocate_message_id(&[], &conv, 1_712_345_678_901);
        assert_eq!(&*id.local, "1712345678901");
        assert_eq!(id.sent_at, 1_712_345_678_901);
        assert_eq!(network.kind(), NetworkKind::Signal);
        assert_eq!(network.descriptor().login_flows[0].id, "qr");
        assert_eq!(network.descriptor().msg_id_scope, MsgIdScope::AccountAuthor);
        // Linking brings no history unless the offer is switched on, and the descriptor
        // says what the linking URL will offer: a UI shows "Transfer History from Phone…"
        // by it.
        assert_eq!(network.descriptor().history, HistorySource::None);
        assert!(!network.tuning.transfer_offer);
        let offering = SignalNetwork::new().with_history_transfer_offer(true);
        assert_eq!(offering.descriptor().history, HistorySource::PushedOnLink);
        assert!(offering.tuning.transfer_offer);
        let off_again = offering.with_history_transfer_offer(false);
        assert_eq!(off_again.descriptor().history, HistorySource::None);
        assert!(!off_again.tuning.transfer_offer);
        // Own attachments have a name before they are sent: stable, one per position.
        let names = network.attachment_net_ids(&id.local, 0);
        assert_eq!(names.len(), 1);
        assert_eq!(names, network.attachment_net_ids("1712345678901", 0));
        assert_ne!(names, network.attachment_net_ids(&id.local, 1));
    }
}

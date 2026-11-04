//! Core → backend commands (architecture §4.2) and the outgoing-message request the core
//! persists in its outbox (§4.8).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kourier_model::{
    AccountId, AccountSettingsPatch, AttachmentId, BackendBlob, BackendError, BlobKey, Body, CmdId,
    ContactChange, ConvKey, ConversationChange, DownloadId, Identifier, IdentityFingerprint,
    LocalMsgId, MsgTarget, NetMsgId, OwnDevice, ProfilePatch, ReceiptKind, RecipientState,
    Redacted, ReplyTo, SafetyAnswer, UserId,
};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::network::AllocatedId;

/// The reply channel of a request-style command. Dropping it only means "the caller stopped
/// waiting"; it never aborts work already on the wire (§4.7).
pub type Reply<T> = oneshot::Sender<Result<T, BackendError>>;

/// A command from the core to one account actor. Commands with a `reply` are answered on it;
/// durable ones carry a [`CmdId`] and are confirmed with `BackendEvent::CommandPersisted`.
#[derive(Debug)]
pub enum Command {
    /// Connect (sent after the first UI frame).
    Connect,
    /// Disconnect, keep the actor.
    Disconnect,
    /// Stop accepting work, finish the current durable step, exit with `Exit::Shutdown`.
    Shutdown,
    /// Unlink this device on the network, then exit with `Exit::LoggedOut`.
    ///
    /// Also sent to an actor the core has not accepted: right behind
    /// [`CancelLogin`](Self::CancelLogin), and when the core refused a link (a relink that
    /// turned out to be another account). The link may have finished on the network a moment
    /// before, with `AccountReady` still on its way to the core, and nobody else would ever
    /// remove that device from the user's account. A backend that has a device (credentials
    /// it could connect with) tells the network to forget it; one that has none just exits.
    Logout,
    /// NetworkManager reported a connectivity change: reconnect now if offline.
    NetworkChanged,
    /// logind reported resume from suspend: check the connection now.
    Resumed,
    /// Start a login flow (`"qr"`; later `"pair-code"`, `"id-backup"`).
    StartLogin {
        /// A flow id from `NetworkDescriptor::login_flows`.
        flow: String,
        /// Set when relinking an existing account (§4.3 step 6).
        relink: Option<Relink>,
        /// EVERY own id of the profile's OTHER accounts on this network, one entry per id (not
        /// the accounts being relinked or removed): a WhatsApp account has its LID and its phone
        /// number, and a login that is typed a number must see it. A backend that learns whose account
        /// the phone links BEFORE it registers a device refuses a link that brings one of
        /// them with `ErrorKind::AlreadyAdded`, so the phone never gets a second device of
        /// one account (L1, `buttons.md` F2). The core checks the same again when
        /// `AccountReady` arrives; a backend that cannot know early may ignore this.
        taken: Vec<TakenIdentity>,
    },
    /// Answer an `Input` login step.
    SubmitLoginInput {
        /// The `LoginStep::txn` being answered.
        txn: u64,
        /// `(field id, value)`; values may be passwords.
        values: Vec<(String, Redacted<String>)>,
    },
    /// Abort the login flow. A [`Logout`](Self::Logout) follows at once, for the case that
    /// the link had already finished on the network.
    CancelLogin,
    /// One send attempt of an outbox item (§4.8); the backend's own repair (device mismatch,
    /// unsealed fallback) happens inside it.
    ///
    /// A transmit of an id the network already has (`req.local`, the network-local id the
    /// core allocated: a resend after a kill between the attempt and its outcome, a retry
    /// after a lost answer) makes no second message: the recipients see one, and the answer
    /// is the message as it was sent (or the first attempt's failure), never a second send.
    /// The core relies on it and resends with the same id after a restart (§4.8). A backend
    /// whose library keeps its own send queue keeps a map from `req.local` to what it sent.
    /// A retry that names only some recipients (`only`) sends to those and to nobody else.
    Transmit {
        /// The message row.
        local: LocalMsgId,
        /// What to send (boxed: it dwarfs every other command).
        req: Box<OutgoingRequest>,
        /// The outcome.
        reply: oneshot::Sender<TransmitOutcome>,
    },
    /// Stop the attempt of `local` before anything of it can reach anyone (the X on an upload;
    /// not durable, no repeat). The backend answers at once, from its stop state, never by
    /// waiting for the attempt:
    /// - [`AbortAnswer::Stopping`]: nothing of the message can go out any more. The attempt
    ///   ends and answers its `Transmit` with `Failed(..)`, having sent nothing to anyone.
    /// - [`AbortAnswer::TooLate`]: the attempt is past the point of no return (a request is on
    ///   its way, or it is over) or is not known. It goes on as it would have; the core keeps
    ///   the message and the user is told that it was sent.
    ///
    /// A backend that sends `AttachmentProgress` MUST answer this (the UI draws an X from those
    /// reports). A backend that drops the reply, or has no upload to stop, is read as `TooLate`.
    AbortTransmit {
        /// The message row of the attempt (`Transmit.local`).
        local: LocalMsgId,
        /// The answer.
        reply: oneshot::Sender<AbortAnswer>,
    },
    /// Durable: send read or viewed receipts. The core never names a message of a history
    /// import here: its sender is not told that it was read on this device.
    SendReceipts {
        /// The `pending_commands` row.
        cmd: CmdId,
        /// The conversation.
        conv: ConvKey,
        /// The messages.
        targets: Vec<MsgTarget>,
        /// `Read` or `Viewed`.
        kind: ReceiptKind,
    },
    /// Durable: tell our other devices what we read.
    ///
    /// `up_to` is always a MESSAGE somebody else wrote: never a system notice and never a
    /// call line (neither has an id a network could name). `read` lists every message that
    /// became read, those of a history import included: a message the other device had
    /// not read and that was read here is told like a live one.
    SyncReadMarker {
        /// The `pending_commands` row.
        cmd: CmdId,
        /// The conversation.
        conv: ConvKey,
        /// Newest message read.
        up_to: MsgTarget,
        /// The messages that became read.
        read: Vec<MsgTarget>,
    },
    /// Durable: the user cleared a conversation's history, or deleted the conversation, on
    /// this device. The core has done it already (rows and files are gone); the backend
    /// tells the account's other devices where the network syncs that (Signal:
    /// `SyncMessage.deleteForMe.conversationDeletes`, or `localOnlyConversationDeletes` when
    /// both lists are empty) and confirms with `CommandPersisted` like every durable command,
    /// also when its network has nothing to tell. Nobody but our own devices learns of it.
    ///
    /// It is the outgoing form of `BackendEvent::ConversationCleared`: `newest` followed by
    /// `newest_non_expiring` is that event's `up_to`. A backend never answers it with that
    /// event.
    ClearConversation {
        /// The `pending_commands` row.
        cmd: CmdId,
        /// The conversation.
        conv: ConvKey,
        /// The newest messages a person sent that the conversation held when it was cleared,
        /// newest first, at most [`CLEAR_ANCHORS`]: the other devices delete up to the
        /// newest of them that they have. Messages still waiting in this device's outbox are
        /// not among them (no other device has them, and they were not deleted). Empty when
        /// the conversation held none.
        newest: Vec<MsgTarget>,
        /// The same, counting only messages without a disappearing timer (Signal
        /// `mostRecentNonExpiringMessages`: an anchor that has not expired on the other
        /// device by the time the sync arrives).
        newest_non_expiring: Vec<MsgTarget>,
        /// The conversation itself was deleted, not only emptied (Signal `isFullDelete`). The
        /// core sends a clear without `remove` only when `newest` names a message (there is
        /// nothing else another device could delete up to); a deleted conversation is sent
        /// also with both lists empty.
        remove: bool,
        /// The newest `sent_at` (ms) among the messages the clear removed that a person sent
        /// (not a notice, not a stand-in: the rows that can be anchors, so the time of
        /// `newest[0]` unless a newer row was hidden), in the clock of the network's
        /// `sent_at`; `None` when it removed none. A network whose clear names a time instead of
        /// messages (WhatsApp: the range's `lastMessageTimestamp`, with `newest[0]` as its key)
        /// sends this as that timestamp; Signal ignores it. At most the core's clock plus
        /// [`CLEAR_CLOCK_TOLERANCE`] (a later time is read as that bound), so a message that
        /// claims a later time never makes the range reach past it.
        before_ms: Option<i64>,
        /// The same for the rows that are NOT messages of a person (a group notice, a call
        /// line, a stand-in for a message that could not be read): the newest `sent_at` among
        /// the removed rows that are `System` or `Placeholder`, `None` when it removed none. A
        /// network whose range keeps the two apart (WhatsApp: `lastSystemMessageTimestamp`
        /// beside `lastMessageTimestamp`) sends it there; a network that has one time sends the
        /// larger of the two. The cut the core keeps for refusing later history is the larger
        /// of both.
        system_before_ms: Option<i64>,
    },
    /// Durable: the user deleted one attachment of a message on this device (for this
    /// account only). The core has removed it already; the backend tells the account's other
    /// devices where the network syncs that (Signal:
    /// `SyncMessage.deleteForMe.attachmentDeletes`) and confirms with `CommandPersisted`.
    /// The outgoing form of `BackendEvent::AttachmentDeleted`.
    DeleteAttachment {
        /// The `pending_commands` row.
        cmd: CmdId,
        /// The conversation.
        conv: ConvKey,
        /// The message (its original id).
        target: MsgTarget,
        /// Where the attachment was among the message's attachments (0-based) before it
        /// went, for a network that names attachments by position.
        index: u16,
        /// The names the attachment had (`Attachment::net_ids`: what the backend gave with
        /// the attachment it emitted, or [`Network::attachment_net_ids`](crate::Network) for
        /// one this device sent). They name content, hence redacted in `Debug`. The core
        /// sends no command for an attachment without names on a network that has no other
        /// way to name one; a backend that cannot name it confirms and tells nobody.
        net_ids: Redacted<Vec<String>>,
    },
    /// Durable: ask the account's other device to upload the files of one message again,
    /// because the network lost them (Signal: `SyncMessage.attachmentBackfillRequest` to our
    /// own devices, which only the primary phone answers). One request names a whole
    /// message: the other device answers for all its files. The core sends it only for a
    /// download the user started. The backend confirms the command with `CommandPersisted`,
    /// then reports `BackendEvent::AttachmentRestore` with [`RestoreUpdate::Sent`] once the
    /// request went out (the core's wait starts there) or [`RestoreUpdate::NotSent`] when it
    /// gave up sending it. Answers arrive as `AttachmentRestore` too.
    ///
    /// [`RestoreUpdate::Sent`]: crate::RestoreUpdate::Sent
    /// [`RestoreUpdate::NotSent`]: crate::RestoreUpdate::NotSent
    RequestAttachments {
        /// The `pending_commands` row.
        cmd: CmdId,
        /// The conversation.
        conv: ConvKey,
        /// The message (its original id).
        target: MsgTarget,
    },
    /// Ephemeral: we started or stopped typing.
    SetTyping {
        /// The conversation.
        conv: ConvKey,
        /// Typing.
        on: bool,
    },
    /// Download an attachment into `dest`; answered with `AttachmentDone`, which echoes
    /// `request`. The answer is wanted also when the core has deleted the attachment
    /// meanwhile: the core removes the file the backend left at `dest` when the answer
    /// arrives, so a backend answers every request it wrote a file for.
    DownloadAttachment {
        /// The attachment row.
        attachment: AttachmentId,
        /// This request (never reused; a resend after a restart repeats it).
        request: DownloadId,
        /// The network pointer (`Attachment::remote`).
        remote: BackendBlob,
        /// Where the plaintext goes.
        dest: BlobTarget,
        /// The attachment's names (`Attachment::net_ids`). A backend may check the download
        /// against a name that names content (Signal: `plaintext:`, the plaintext hash an
        /// import kept), which binds a pointer another device gave later to the file that
        /// was asked for. Redacted in `Debug`.
        net_ids: Redacted<Vec<String>>,
    },
    /// Roaming conversation change; confirmed by `ConversationUpsert` after the network
    /// transaction (§4.1 principle 4).
    ///
    /// `ConversationChange::SetTimer` (the disappearing-message timer; the core sends it only
    /// for a timer the conversation's features take, `ConversationFeatures::takes_timer`: one
    /// of the offered values or a value of the stated range, in whole seconds; or `None`,
    /// off): the backend tells the network (Signal
    /// 1:1: an expiration-timer update with the next timer version and its sync transcript; a
    /// group: the group change), answers `reply`, and confirms with one delivery of
    /// `ConversationUpsert { timer, timer_version }` and a `Message` whose body is
    /// `SystemEvent::TimerChanged { by: None, timer }`, with `Origin::OwnThisDevice` and the
    /// network id of the update it sent (`id`, where the network carries the change as a
    /// message). The backend writes that timeline row for our own changes exactly as it does
    /// for a change by a peer, a group member or another own device (`by` then names who): only
    /// the backend knows what the network made of the change, and the row's network id makes a
    /// redelivered or echoed change the same row. The core never writes a timer row itself and
    /// changes `timer` only from the patch. The timer version is the backend's to count: it
    /// drops a change older than the one it has, states the version in
    /// `ConversationPatch::timer_version` and attaches the current one to its sends.
    ChangeConversation {
        /// The conversation.
        conv: ConvKey,
        /// The change.
        change: ConversationChange,
        /// A message id from the core's allocator (`Network::allocate_message_id`, as for an
        /// own message: never the id of another own message, queued ones included), set for a
        /// change a network may carry as a message of its own (`SetTimer`). A backend that
        /// sends such a message uses it as the message's id and as `local`/`sent_at` of the
        /// timeline row it emits; an id it made up itself could be that of a message waiting
        /// in the core's outbox, and the row would be taken for that message's echo.
        id: Option<AllocatedId>,
        /// Accepted or not.
        reply: Reply<()>,
    },
    /// Durable: a roaming conversation change the core makes on its own, with no click behind
    /// it and no one waiting for an answer (today only the clearing of a "marked unread" flag
    /// when the user reads the chat, `ConversationChange::SetMarkedUnread { marked_unread:
    /// false }`). It is [`Command::ChangeConversation`] without a `reply`: the core stores it
    /// in the transaction that reads the chat (`pending_commands`) and sends it again at every
    /// start until the backend answers `CommandPersisted { cmd }`, so the backend answers it
    /// after its OWN durable write (a zone of its `backend.db` that holds the intent and the
    /// confirmation together), never through a oneshot. A backend that has nothing to write
    /// for it (a network that states no roaming settings, an account whose roaming switch is
    /// off) still answers `CommandPersisted` and drops it; one that never answers would be
    /// sent the same row at every start for ever. The core queues it only for an account whose
    /// caps state [`AccountCaps::roaming_writes`](kourier_model::AccountCaps::roaming_writes)
    /// (`Some(_)`); a stale row of an account that no longer states it is dropped by the core,
    /// not resent.
    ///
    /// A backend applies (or files) it in the order it receives it relative to
    /// [`Command::ChangeConversation`] of the same chat: a later click on the same setting wins.
    /// The core keeps its side of that order (a reading holds the chat's turn until the durable
    /// command is queued, and a later click goes through the same channel behind it); a backend
    /// that files the durable one as a job that runs later must not let a plain click that came
    /// after it overtake it, or the user who reads a marked chat and marks it again at once ends
    /// with the clear landing after the mark.
    ///
    /// `asked_at` is the core's clock (ms) at the moment the user read the chat: a backend that
    /// keeps what another device wrote since compares it with the time it merged that (the
    /// phone's newer mark stays and is stated back to the core).
    ChangeConversationDurable {
        /// The `pending_commands` row.
        cmd: CmdId,
        /// The conversation.
        conv: ConvKey,
        /// The change.
        change: ConversationChange,
        /// The core's clock (ms) when the change was asked.
        asked_at: i64,
    },
    /// Roaming contact change; confirmed by `ContactUpsert`.
    ChangeContact {
        /// The contact.
        user: UserId,
        /// The change.
        change: ContactChange,
        /// Accepted or not.
        reply: Reply<()>,
    },
    /// Change network-synced account settings; confirmed by `AccountSettings`.
    ChangeAccountSettings {
        /// The change.
        patch: AccountSettingsPatch,
        /// Accepted or not.
        reply: Reply<()>,
    },
    /// Change our profile.
    ChangeProfile {
        /// The change.
        patch: ProfilePatch,
        /// Accepted or not.
        reply: Reply<()>,
    },
    /// The user turned the account's switch for WRITING roaming settings (settings the network
    /// keeps for the account's other devices: archive, pin, mute, marked unread) on or off.
    /// Not durable: the backend keeps the switch in its own store, answers `Ok` once it is
    /// stored, and states the new value in [`AccountCaps::FLAG_ROAMING_WRITES`] with a
    /// `BackendEvent::AccountCaps` (the core changes nothing before that event). A network that
    /// cannot write roaming settings answers `ErrorKind::Unsupported` and states no flag.
    /// Turning it off drops what the backend still had queued to write.
    ///
    /// [`AccountCaps::FLAG_ROAMING_WRITES`]: kourier_model::AccountCaps::FLAG_ROAMING_WRITES
    SetRoamingWrites {
        /// On (`true`) or off.
        on: bool,
        /// Done, or refused.
        reply: Reply<()>,
    },
    /// The user accepted a safety-number change ("Confirm New Safety Number", BQ-2b: Desktop's
    /// "Send anyway"); paused sends may continue. `key` is the key of the safety number the
    /// user was SHOWN ([`SafetyNumber::key`](kourier_model::SafetyNumber), the lead's RC2),
    /// as for [`Command::SetVerified`]: for a person whose key changed after it was verified
    /// the backend refuses with `ErrorKind::InvalidInput` (nothing changes) when its stored key
    /// is another one, so a key the user never saw is not approved; for any other person it is
    /// an approval only and `key` is ignored. The core requeues the sends paused on `user`
    /// only after `Ok` (the lead's RC6): a refusal leaves them paused.
    ConfirmIdentity {
        /// Whose identity.
        user: UserId,
        /// The key the user was shown.
        key: IdentityFingerprint,
        /// Confirmed (or nothing to confirm), or refused.
        reply: Reply<()>,
    },
    /// The safety number of a person (P5a), answered at once from the backend's own store:
    /// computed from our identity key and the person's STORED key. A network without safety
    /// numbers answers `SafetyUnavailable::Unsupported`.
    SafetyNumber {
        /// Whose.
        user: UserId,
        /// The number, or why there is none.
        reply: Reply<SafetyAnswer>,
    },
    /// Mark (`verified`) or clear (`!verified`) the verification of a person's identity key
    /// (P5a: "Mark as Verified" and "Clear Verification" ONLY; "Confirm New Safety Number" is
    /// [`Command::ConfirmIdentity`], never a clear: a clear always queues a Verified sync,
    /// BQ-2b). `key` is the key of the safety number the user was SHOWN
    /// ([`SafetyNumber::key`](kourier_model::SafetyNumber)): the backend refuses with
    /// `ErrorKind::InvalidInput` when its stored key is another one, so a state is never
    /// applied to a key the user did not compare. The core releases the sends paused on
    /// `user` after every `Ok` (P5a, critic F2), whatever the state was.
    SetVerified {
        /// Whose identity.
        user: UserId,
        /// Mark (`true`) or clear (`false`).
        verified: bool,
        /// The key the user was shown.
        key: IdentityFingerprint,
        /// Done, or refused.
        reply: Reply<()>,
    },
    /// Fetch older history (only for `HistorySource::OnDemand`).
    FetchHistory {
        /// The conversation.
        conv: ConvKey,
        /// Older than this message; `None` = newest.
        before: Option<MsgTarget>,
        /// At most this many messages.
        limit: u16,
    },
    /// Resolve a phone number or username to a user ("start chat").
    Resolve {
        /// What the user typed.
        id: Identifier,
        /// The user.
        reply: Reply<UserId>,
    },
    /// List our devices.
    ListOwnDevices {
        /// The devices.
        reply: Reply<Vec<OwnDevice>>,
    },
    /// Refresh a contact's profile.
    RefreshContact {
        /// The contact.
        user: UserId,
    },
    /// The user answered a challenge of the network (Signal: the captcha behind a 428). The
    /// backend submits the answer; `Ok` means the network accepted it and refused sends may be
    /// tried again (the core then lifts its pause and transmits the waiting rows), an error
    /// leaves the challenge standing.
    SubmitChallenge {
        /// The token of the challenge the core's outbox waits on (`ErrorKind::Challenge`),
        /// handed back so a backend needs no memory of it; `None` when the core holds none
        /// (the backend announced the challenge through its connection state or a notice).
        token: Option<Redacted<String>>,
        /// What the browser handed back: the solution token, or the whole redirect link it
        /// came in (Signal: `signalcaptcha://…`); the backend strips what it does not need.
        answer: Redacted<String>,
        /// Accepted or not.
        reply: Reply<()>,
    },
    /// The user does not want to wait for the history transfer of a new link (or for its
    /// download) any longer: the backend ends it as `HistoryTransfer::Skipped`, removes what
    /// it downloaded and starts receiving. Ignored when no transfer is waiting or
    /// downloading (none was started, it is being read already, or it is over); the answer
    /// is the `BackendEvent::HistoryTransfer` that follows a skip.
    SkipHistoryTransfer,
    /// The user has seen how the history transfer ended (done, skipped, failed with its
    /// kind): the backend forgets the ended transfer, and from then on reports nothing
    /// about it, neither when it starts nor at `Connect`. Ignored while a transfer is not
    /// over, and when there is none. No answer follows.
    DismissHistoryTransfer,
}

impl Command {
    /// The variant name (log-safe).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Connect => "Connect",
            Self::Disconnect => "Disconnect",
            Self::Shutdown => "Shutdown",
            Self::Logout => "Logout",
            Self::NetworkChanged => "NetworkChanged",
            Self::Resumed => "Resumed",
            Self::StartLogin { .. } => "StartLogin",
            Self::SubmitLoginInput { .. } => "SubmitLoginInput",
            Self::CancelLogin => "CancelLogin",
            Self::Transmit { .. } => "Transmit",
            Self::AbortTransmit { .. } => "AbortTransmit",
            Self::SendReceipts { .. } => "SendReceipts",
            Self::SyncReadMarker { .. } => "SyncReadMarker",
            Self::ClearConversation { .. } => "ClearConversation",
            Self::DeleteAttachment { .. } => "DeleteAttachment",
            Self::RequestAttachments { .. } => "RequestAttachments",
            Self::SetTyping { .. } => "SetTyping",
            Self::DownloadAttachment { .. } => "DownloadAttachment",
            Self::ChangeConversation { .. } => "ChangeConversation",
            Self::ChangeConversationDurable { .. } => "ChangeConversationDurable",
            Self::ChangeContact { .. } => "ChangeContact",
            Self::ChangeAccountSettings { .. } => "ChangeAccountSettings",
            Self::ChangeProfile { .. } => "ChangeProfile",
            Self::SetRoamingWrites { .. } => "SetRoamingWrites",
            Self::ConfirmIdentity { .. } => "ConfirmIdentity",
            Self::SafetyNumber { .. } => "SafetyNumber",
            Self::SetVerified { .. } => "SetVerified",
            Self::FetchHistory { .. } => "FetchHistory",
            Self::Resolve { .. } => "Resolve",
            Self::ListOwnDevices { .. } => "ListOwnDevices",
            Self::RefreshContact { .. } => "RefreshContact",
            Self::SubmitChallenge { .. } => "SubmitChallenge",
            Self::SkipHistoryTransfer => "SkipHistoryTransfer",
            Self::DismissHistoryTransfer => "DismissHistoryTransfer",
        }
    }

    /// The `pending_commands` id of a durable command.
    pub fn durable_id(&self) -> Option<CmdId> {
        match self {
            Self::SendReceipts { cmd, .. }
            | Self::SyncReadMarker { cmd, .. }
            | Self::ClearConversation { cmd, .. }
            | Self::DeleteAttachment { cmd, .. }
            | Self::RequestAttachments { cmd, .. }
            | Self::ChangeConversationDurable { cmd, .. } => Some(*cmd),
            _ => None,
        }
    }
}

/// How many of a conversation's newest messages [`Command::ClearConversation`] names (each
/// of its two lists). The number is the protocol's: every Signal client sends its five newest
/// addressable messages and deletes up to the newest of those it finds
/// (`SD/ts/sql/Server.node.ts` `getMostRecentAddressableMessages`, `limit = 5`), so more than
/// one anchor makes the delete land although the devices' histories differ at the edge.
pub const CLEAR_ANCHORS: usize = 5;

/// How far after the core's clock a time of a clear by time may stand before the core reads it as
/// that bound (one hour). A clear names the newest message of its range, and that is a time of the
/// NETWORK's clock (the server's, the sender's): this computer's clock may run minutes behind it
/// (no time sync, a resume before the sync), and the exact local "now" would then leave the last
/// minutes before the clear in the chat. A time further ahead than this is a wrong clock or a
/// hostile claim: it is read as `now + CLEAR_CLOCK_TOLERANCE`, because a cut only rises and one
/// wrong time would delete live messages and refuse the chat's history for good. It applies to
/// [`ConversationCleared::before_ms`](crate::BackendEvent::ConversationCleared) and to the times
/// of [`Command::ClearConversation`] alike.
pub const CLEAR_CLOCK_TOLERANCE: Duration = Duration::from_secs(60 * 60);

/// The persisted form of a durable command (`pending_commands.command`, JSON). The core
/// stores it in the transaction that does the command's local part (marks messages read,
/// clears a conversation, removes an attachment) and resends it at startup until
/// `CommandPersisted{cmd}` arrives (§4.8).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DurableCommand {
    /// See [`Command::SendReceipts`].
    SendReceipts {
        /// The conversation.
        conv: ConvKey,
        /// The messages.
        targets: Vec<MsgTarget>,
        /// `Read` or `Viewed`.
        kind: ReceiptKind,
    },
    /// See [`Command::SyncReadMarker`].
    SyncReadMarker {
        /// The conversation.
        conv: ConvKey,
        /// Newest message read.
        up_to: MsgTarget,
        /// The messages that became read.
        #[serde(default)]
        read: Vec<MsgTarget>,
    },
    /// See [`Command::ClearConversation`].
    ClearConversation {
        /// The conversation.
        conv: ConvKey,
        /// Its newest messages, newest first.
        #[serde(default)]
        newest: Vec<MsgTarget>,
        /// Its newest messages without a disappearing timer, newest first.
        #[serde(default)]
        newest_non_expiring: Vec<MsgTarget>,
        /// The conversation itself was deleted.
        #[serde(default)]
        remove: bool,
        /// The newest `sent_at` (ms) among the removed rows that a person sent (not `System`,
        /// not `Placeholder`: the filter of the anchors); `None` when it removed none (and for a
        /// command stored before this field existed). See [`Command::ClearConversation`].
        #[serde(default)]
        before_ms: Option<i64>,
        /// The newest `sent_at` (ms) among the other removed rows (`System`, `Placeholder`);
        /// `None` when it removed none (and for a command stored before this field existed).
        #[serde(default)]
        system_before_ms: Option<i64>,
    },
    /// See [`Command::DeleteAttachment`].
    DeleteAttachment {
        /// The conversation.
        conv: ConvKey,
        /// The message.
        target: MsgTarget,
        /// The attachment's position before it went.
        #[serde(default)]
        index: u16,
        /// The attachment's names.
        #[serde(default)]
        net_ids: Redacted<Vec<String>>,
    },
    /// See [`Command::ChangeConversationDurable`].
    ChangeConversation {
        /// The conversation.
        conv: ConvKey,
        /// The change.
        change: ConversationChange,
        /// The core's clock (ms) when the change was asked.
        asked_at: i64,
    },
    /// See [`Command::RequestAttachments`].
    RequestAttachments {
        /// The conversation.
        conv: ConvKey,
        /// The message.
        target: MsgTarget,
    },
}

impl DurableCommand {
    /// The command to send for the `pending_commands` row `cmd`.
    pub fn into_command(self, cmd: CmdId) -> Command {
        match self {
            Self::SendReceipts {
                conv,
                targets,
                kind,
            } => Command::SendReceipts {
                cmd,
                conv,
                targets,
                kind,
            },
            Self::SyncReadMarker { conv, up_to, read } => Command::SyncReadMarker {
                cmd,
                conv,
                up_to,
                read,
            },
            Self::ClearConversation {
                conv,
                newest,
                newest_non_expiring,
                remove,
                before_ms,
                system_before_ms,
            } => Command::ClearConversation {
                cmd,
                conv,
                newest,
                newest_non_expiring,
                remove,
                before_ms,
                system_before_ms,
            },
            Self::DeleteAttachment {
                conv,
                target,
                index,
                net_ids,
            } => Command::DeleteAttachment {
                cmd,
                conv,
                target,
                index,
                net_ids,
            },
            Self::ChangeConversation {
                conv,
                change,
                asked_at,
            } => Command::ChangeConversationDurable {
                cmd,
                conv,
                change,
                asked_at,
            },
            Self::RequestAttachments { conv, target } => {
                Command::RequestAttachments { cmd, conv, target }
            }
        }
    }
}

/// One own id of another account of the profile, and whether that account is logged out
/// (`Command::StartLogin::taken`; an account with several ids has one entry for each): the
/// answer of a refused link says which.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakenIdentity {
    /// One of the account's own ids (all of them are listed, the primary first).
    pub user: UserId,
    /// That account is logged out: the user points it out with "Link Again".
    pub logged_out: bool,
}

/// Refuse to link another account than the one being relinked (§4.3 step 6).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relink {
    /// The account being relinked.
    pub account: AccountId,
    /// Its own ids; the provisioned identity must be one of them.
    pub expect_own: Vec<UserId>,
}

/// Where a download goes: an absolute path inside the account's blob directory and the file
/// key, both chosen by the core. The backend streams plaintext into `kourier-blob`'s writer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobTarget {
    /// Absolute destination path (under `AccountContext::blob_dir`).
    pub path: PathBuf,
    /// The file key.
    pub key: BlobKey,
}

/// An outbox item (`outbox.request`, JSON): one own message or modifier to transmit. `local`
/// and `sent_at` are allocated once at enqueue by `Network::allocate_message_id`; every retry
/// reuses them, so receivers dedup by id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutgoingRequest {
    /// The conversation.
    pub conv: ConvKey,
    /// Network-local part of our message id.
    pub local: Arc<str>,
    /// The sent timestamp that goes with `local`.
    pub sent_at: i64,
    /// What to send.
    pub kind: OutgoingKind,
    /// `None` = all recipients; `Some` = resend to these members only.
    #[serde(default)]
    pub only: Option<Vec<UserId>>,
    /// A follow-up send to more recipients (Signal's sync-transcript flag).
    #[serde(default)]
    pub is_recipient_update: bool,
}

impl OutgoingRequest {
    /// The id of the message this request sends (own: no author).
    pub fn net_id(&self) -> NetMsgId {
        NetMsgId::own(self.local.clone())
    }

    /// The same request, resent to `members` only (§4.8 `Partial` and "Retry").
    pub fn resend_to(&self, members: Vec<UserId>) -> Self {
        Self {
            only: Some(members),
            is_recipient_update: true,
            ..self.clone()
        }
    }
}

/// What an [`OutgoingRequest`] sends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum OutgoingKind {
    /// A new message.
    Message {
        /// Content.
        body: Body,
        /// Quoted message.
        #[serde(default)]
        reply_to: Option<ReplyTo>,
        /// Disappearing timer of the conversation at send time.
        #[serde(default)]
        expiry: Option<Duration>,
    },
    /// An edit of an own message.
    Edit {
        /// The message: under [`EditTarget::LatestRevision`](kourier_model::EditTarget) its
        /// latest revision that the recipients have, under
        /// [`EditTarget::Original`](kourier_model::EditTarget) the message itself, never a
        /// revision (the network says which in
        /// [`NetworkDescriptor::edit_target`](kourier_model::NetworkDescriptor::edit_target)).
        target: MsgTarget,
        /// The new content: `Body::Text` for a text message; for a message with attachments
        /// `Body::Media` with every attachment of the message, unchanged and in order, and the
        /// new caption (`None`: the caption was removed). Each attachment carries what the core
        /// has of it: `remote` (the pointer it arrived with, when it came from the network) and
        /// `local`/`thumb` (the core's blobs, when the file is here), so a backend re-sends the
        /// pointer it has or uploads again. An edit never adds, removes or replaces a file.
        body: Body,
    },
    /// A delete.
    Delete {
        /// The message.
        target: MsgTarget,
        /// For everyone (otherwise a delete-for-me sync).
        for_everyone: bool,
    },
    /// A reaction.
    React {
        /// The message.
        target: MsgTarget,
        /// The emoji.
        emoji: String,
        /// Remove instead of add.
        #[serde(default)]
        remove: bool,
    },
}

impl OutgoingKind {
    /// The variant name (log-safe; also the core's `outbox.kind`).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Message { .. } => "Message",
            Self::Edit { .. } => "Edit",
            Self::Delete { .. } => "Delete",
            Self::React { .. } => "React",
        }
    }
}

/// A backend's answer to [`Command::AbortTransmit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AbortAnswer {
    /// The attempt stops and nothing of it goes out to anyone; its `Transmit` answers `Failed`.
    Stopping,
    /// Too late, or no such attempt: it goes on as it would have.
    TooLate,
}

/// The outcome of one [`Command::Transmit`] attempt (§4.8 step 3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TransmitOutcome {
    /// Accepted for every recipient.
    Sent {
        /// Server timestamp.
        #[serde(default)]
        server_at: Option<i64>,
        /// Per-recipient result.
        #[serde(default)]
        per_recipient: Vec<(UserId, RecipientState)>,
    },
    /// Accepted for every recipient, and the network's server gave the message an id and a
    /// position of its own (KT-2: Telegram). Read as [`Sent`](Self::Sent) in every respect,
    /// plus `assigned`: the core takes the message from its allocated id
    /// ([`Network::allocate_message_id`](crate::Network::allocate_message_id)) to the assigned
    /// one, and keeps the allocated id as another name for it. A network that answers so
    /// promises:
    ///
    /// - Only a [`OutgoingKind::Message`] is answered so. An edit, a delete or a reaction
    ///   targets a message that has its id already, and answers `Sent`; `SentAs` to one is a
    ///   backend fault, which the core reads as `Sent` and says so in one warn line.
    /// - A resend under the allocated id (the core keeps the id when it sends again) answers
    ///   the SAME `assigned`, whether or not an earlier attempt reached the server: the
    ///   network's own store holds the map from the allocated id, and holds it durably BEFORE
    ///   the request leaves, or a kill between the send and the answer would lose it.
    /// - The ids it allocates never equal an id its server gives (the fake: `k<hex>` against
    ///   decimal numbers), so an allocated id never meets a message the server named.
    /// - `assigned.local` is an id like any other (it passes the grammar every id of an
    ///   envelope does, [`Envelope::validate`](crate::Envelope::validate), and the core never
    ///   parses it) that no other message of the conversation has; `assigned.seq` is the
    ///   position the echo of the message states as
    ///   [`IncomingMessage::net_seq`](crate::IncomingMessage::net_seq), in
    ///   `0..=`[`MAX_NET_SEQ`](crate::MAX_NET_SEQ). An own row of the conversation that is
    ///   already under `assigned.local` is taken to be the echo of this message that knew no
    ///   binding, and is merged into it, unless it has a send of its own queued. An id the core
    ///   cannot take (it fails the grammar, or another message has it: a row of somebody else,
    ///   a call line, a row of another conversation, an own message with its own send queued)
    ///   is not an error: the send counts and the message keeps its allocated id. A position
    ///   out of range is left out: the message takes the id, and its position comes with the
    ///   echo.
    /// - Behind an unsent message the core queues its edits, deletes and reactions, and they
    ///   go out naming the assigned id once this answer is in and the allocated one before it:
    ///   a request may name either, and the network resolves both to the same message. That
    ///   includes a DELETE that names the allocated id of a message whose send was never
    ///   answered (the attempt ended with no outcome: the server may hold the message), which
    ///   the network maps through its own store.
    /// - A QUOTE may name the allocated id of a message whose send FAILED, which has no
    ///   assigned id and never will. A network that assigns ids sends such a reply WITHOUT the
    ///   quote.
    ///
    /// The echo of the message
    /// ([`IncomingMessage::allocated_as`](crate::IncomingMessage::allocated_as)) and this answer
    /// state one `server_at`, so the order they arrive in changes nothing.
    SentAs {
        /// The id and the position the server gave the message.
        assigned: AssignedId,
        /// Server timestamp.
        #[serde(default)]
        server_at: Option<i64>,
        /// Per-recipient result.
        #[serde(default)]
        per_recipient: Vec<(UserId, RecipientState)>,
    },
    /// Some recipients failed.
    Partial {
        /// Recipients the server accepted.
        delivered: Vec<(UserId, RecipientState)>,
        /// Recipients that failed, and why.
        failed: Vec<(UserId, BackendError)>,
    },
    /// The attempt failed. After a transient failure (a timeout, a lost connection) the server
    /// may have the message already: the core resends under the same id and the network makes
    /// no second message. An attempt that answered [`AbortAnswer::Stopping`] to an
    /// [`Command::AbortTransmit`] sent nothing to anyone.
    Failed(BackendError),
}

/// What the server called a message of ours, and where it put it ([`TransmitOutcome::SentAs`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AssignedId {
    /// Network-local part of the id the server gave the message (the stored author of own
    /// messages is always `None`).
    pub local: Arc<str>,
    /// The message's position in its conversation: the `net_seq` its echo states, in
    /// `0..=`[`MAX_NET_SEQ`](crate::MAX_NET_SEQ).
    pub seq: i64,
}

impl TransmitOutcome {
    /// Failed members a resend may still reach (`only` of the next attempt).
    pub fn retryable_members(&self) -> Vec<UserId> {
        match self {
            Self::Partial { failed, .. } => failed
                .iter()
                .filter(|(_, e)| e.is_retryable())
                .map(|(u, _)| u.clone())
                .collect(),
            Self::Sent { .. } | Self::SentAs { .. } | Self::Failed(_) => Vec::new(),
        }
    }

    /// Members that failed for good (they make the message `PartiallyFailed`).
    pub fn permanently_failed(&self) -> Vec<UserId> {
        match self {
            Self::Partial { failed, .. } => failed
                .iter()
                .filter(|(_, e)| !e.is_retryable())
                .map(|(u, _)| u.clone())
                .collect(),
            Self::Sent { .. } | Self::SentAs { .. } | Self::Failed(_) => Vec::new(),
        }
    }
}

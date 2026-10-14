//! Core → backend commands (architecture §4.2) and the outgoing-message request the core
//! persists in its outbox (§4.8).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kourier_model::{
    AccountId, AccountSettingsPatch, AttachmentId, BackendBlob, BackendError, BlobKey, Body, CmdId,
    ContactChange, ConvKey, ConversationChange, DownloadId, Identifier, LocalMsgId, MsgTarget,
    NetMsgId, OwnDevice, ProfilePatch, ReceiptKind, RecipientState, Redacted, ReplyTo, UserId,
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
        /// The primary own ids of the profile's OTHER accounts on this network (not the one
        /// being relinked, none that is being removed). A backend that learns whose account
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
    Transmit {
        /// The message row.
        local: LocalMsgId,
        /// What to send (boxed: it dwarfs every other command).
        req: Box<OutgoingRequest>,
        /// The outcome.
        reply: oneshot::Sender<TransmitOutcome>,
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
    /// The user accepted a safety-number change; paused sends may continue.
    ConfirmIdentity {
        /// Whose identity.
        user: UserId,
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
            Self::SendReceipts { .. } => "SendReceipts",
            Self::SyncReadMarker { .. } => "SyncReadMarker",
            Self::ClearConversation { .. } => "ClearConversation",
            Self::DeleteAttachment { .. } => "DeleteAttachment",
            Self::RequestAttachments { .. } => "RequestAttachments",
            Self::SetTyping { .. } => "SetTyping",
            Self::DownloadAttachment { .. } => "DownloadAttachment",
            Self::ChangeConversation { .. } => "ChangeConversation",
            Self::ChangeContact { .. } => "ChangeContact",
            Self::ChangeAccountSettings { .. } => "ChangeAccountSettings",
            Self::ChangeProfile { .. } => "ChangeProfile",
            Self::ConfirmIdentity { .. } => "ConfirmIdentity",
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
            | Self::RequestAttachments { cmd, .. } => Some(*cmd),
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
            } => Command::ClearConversation {
                cmd,
                conv,
                newest,
                newest_non_expiring,
                remove,
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
            Self::RequestAttachments { conv, target } => {
                Command::RequestAttachments { cmd, conv, target }
            }
        }
    }
}

/// The primary own id of another account of the profile, and whether that account is logged
/// out (`Command::StartLogin::taken`): the answer of a refused link says which.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakenIdentity {
    /// The account's primary own id (the first of its `own_ids`).
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
        /// The message (its latest revision).
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
    /// Some recipients failed.
    Partial {
        /// Recipients the server accepted.
        delivered: Vec<(UserId, RecipientState)>,
        /// Recipients that failed, and why.
        failed: Vec<(UserId, BackendError)>,
    },
    /// Nothing was sent.
    Failed(BackendError),
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
            Self::Sent { .. } | Self::Failed(_) => Vec::new(),
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
            Self::Sent { .. } | Self::Failed(_) => Vec::new(),
        }
    }
}

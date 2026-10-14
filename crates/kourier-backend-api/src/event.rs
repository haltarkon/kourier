//! Backend → core events, envelopes and acks (architecture §4.2, §5.6).
//!
//! Durable events reach the core only from the backend's `core_outbox` forwarder, in an
//! [`Envelope`] that carries an [`Ack`]; the core applies the envelope in one `core.db`
//! transaction and completes the ack after the commit. Ephemeral events (login steps, typing,
//! progress) bypass the outbox with `ack: None`.

use std::sync::Arc;
use std::time::Duration;

use kourier_model::ids::validate_local;
use kourier_model::{
    AccountCaps, AccountProgress, AccountSettingsPatch, AttachmentId, AttachmentMeta, BackendBlob,
    BackendError, Body, CmdId, ContactPatch, ConvKey, ConversationPatch, DownloadId, Expiry,
    HistoryTransfer, IdError, LocalMsgId, LoginStep, LogoutReport, MsgTarget, NetMsgId, Origin,
    OutgoingState, PersistError, ReceiptKind, RecipientState, Redacted, ReplyTo, UserId,
};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// Most messages in one [`BackendEvent::HistoryBatch`] (one bounded core transaction, §4.10).
pub const MAX_HISTORY_BATCH: usize = 500;

/// An event from a backend. Serde by variant name: durable events are stored in the
/// backend's `core_outbox` (see [`encode_outbox_row`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum BackendEvent {
    /// Ephemeral: a login step to show (§4.4).
    Login(LoginStep),
    /// The account is linked and usable; sets `own_ids`, device and display name.
    ///
    /// Contract rule: during a link (fresh or relink, until the core acked it) `AccountReady` is
    /// the first durable event the backend ships. A durable envelope that does not start with it
    /// is refused for good ([`ContractViolation::DurableBeforeAccountReady`], see
    /// [`Envelope::validate_linking`]): the core checks the linked identity only on that event
    /// (§4.3 step 6). Ephemeral `Login` steps may come before it.
    AccountReady {
        /// Our ids (Signal: `[aci, pni]`).
        own_ids: Vec<UserId>,
        /// Our device id.
        #[serde(default)]
        device: Option<u32>,
        /// Own profile name (personal data, hence redacted in `Debug`).
        #[serde(default)]
        display_name: Option<Redacted<String>>,
        /// The account's own number or address as the network shows it to people (Signal:
        /// the E.164 phone number). `None` leaves what the core has: a backend sets it at
        /// the link and again, with the new `own_ids`, after the number changed on another
        /// device. Personal data, hence redacted in `Debug`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        number: Option<Redacted<String>>,
    },
    /// Network-synced account settings changed (read receipts, typing, link previews, …).
    AccountSettings(AccountSettingsPatch),
    /// Server limits and feature flags.
    AccountCaps(AccountCaps),
    /// A contact was created or changed.
    ContactUpsert(ContactPatch),
    /// `from` is an alias of `into` (PNI→ACI, E164→ACI, PN→LID); the core rewrites and merges
    /// (§3.3).
    IdentityMerged {
        /// The alias.
        from: UserId,
        /// The canonical id.
        into: UserId,
    },
    /// A conversation was created or changed (created if unknown).
    ConversationUpsert(ConversationPatch),
    /// The full ordered list of pinned conversations.
    PinnedConversations(Vec<ConvKey>),
    /// A conversation is gone on the network (left and deleted elsewhere).
    ConversationRemoved {
        /// The conversation.
        conv: ConvKey,
    },
    /// Our account deleted a conversation's messages on another own device (a delete for me of
    /// a whole chat: Signal `SyncMessage.deleteForMe.conversationDeletes` and
    /// `localOnlyConversationDeletes`). Nobody else is told, and nobody else can send it: a
    /// backend emits it only for a sync from an own device.
    ///
    /// The core deletes for good (rows, reactions, receipts, attachments; the files are
    /// unlinked after the commit) every message of the conversation up to the newest of
    /// `up_to` that it has, in timeline order; what arrived here later stays, and so does an
    /// own message that still waits in the outbox (the other device cannot have had it) and a
    /// message whose own delete still waits there (that delete must still be sent). With an
    /// empty `up_to` no message is deleted by that rule. The event may overtake its anchors
    /// (messages a backend still holds back): the core keeps it for each message of `up_to`
    /// that is not here yet, two days at most, and deletes that message when it arrives;
    /// when none of them was here, what the conversation held when the event came goes with
    /// the first that arrives. What the event deleted is remembered, so a later copy of such
    /// a message (a redelivery, a history import) does not bring it back. Applying the event
    /// twice changes nothing more. For "everything, and the conversation with it" there is
    /// [`ConversationRemoved`](Self::ConversationRemoved).
    ConversationCleared {
        /// The conversation.
        conv: ConvKey,
        /// The newest messages the deleting device had (Signal: `mostRecentMessages`
        /// followed by `mostRecentNonExpiringMessages`; any order).
        #[serde(default)]
        up_to: Vec<MsgTarget>,
        /// The chat itself was deleted, not only emptied (Signal `isFullDelete`, and every
        /// `localOnlyConversationDeletes`): when no message a person sent is left after the
        /// rule above, the notices that remain go too and the conversation leaves the
        /// conversation list. Its row, title, members and features stay, so the backend need
        /// not send them again: the next message, or the user writing there, brings the
        /// conversation back. While a message is left the conversation stays listed (Signal
        /// Desktop's `deleteLocalOnlyConversation`).
        #[serde(default)]
        remove: bool,
        /// When (ms).
        at: i64,
    },
    /// Our account deleted one attachment of a message on another own device (Signal
    /// `SyncMessage.deleteForMe.attachmentDeletes`). The attachment's row and files go (the
    /// files are unlinked after the commit), the rest of the message stays; a message left
    /// with neither an attachment nor a text is hidden like a delete for me. The event waits
    /// for a message that is not here yet like other modifiers do (two days); on a message
    /// that is here, an unknown name changes nothing. It also finds the attachments of
    /// messages this device sent, by the names of
    /// [`Network::attachment_net_ids`](crate::Network::attachment_net_ids).
    AttachmentDeleted {
        /// The message.
        target: MsgTarget,
        /// Names of the attachment, in order of priority (Signal: client UUID, digest,
        /// plaintext hash): the first name that any attachment of the message has in its
        /// `Attachment::net_ids` decides, and that attachment goes (the order over ALL
        /// attachments, so a name two attachments share, like a plaintext hash, never beats a
        /// name only one has). They name content, hence redacted in `Debug`.
        net_ids: Redacted<Vec<String>>,
        /// When (ms).
        at: i64,
    },
    /// News about a request for the files of one message that the network lost
    /// (`Command::RequestAttachments`; Signal: attachment backfill): our request went out or
    /// could not be sent, or the account's other device answered. An answer comes also for
    /// a request another device of the account made (the phone answers all of them), and
    /// several answers come for one request (a first one at once, more as the other device
    /// uploads). The backend reports an answer only from the device that can give one
    /// (Signal: the primary, device 1) and only for a message it could name; the core
    /// applies it only to a message it has and never adds an attachment.
    AttachmentRestore {
        /// The message (its original id).
        target: MsgTarget,
        /// What happened.
        update: RestoreUpdate,
        /// When (ms).
        at: i64,
        /// The command the news is about: `Some` for [`RestoreUpdate::Sent`] and
        /// [`RestoreUpdate::NotSent`] (the `cmd` of `Command::RequestAttachments`), which the
        /// core applies only to the request that command made, never to a newer wait for the
        /// same message; `None` for an answer, which names no command on any network (the
        /// other device answers about the message). Old stored events lack it.
        #[serde(default)]
        cmd: Option<CmdId>,
    },
    /// A live message.
    ///
    /// Also how a backend reports a signal of a call: a message whose body is a call line
    /// (`Body::System(SystemEvent::Call(..))`, built with [`IncomingMessage::call`]). The
    /// first signal of a call makes the line; a later message with the same `conv` and
    /// `local` whose body is a call line MERGES into that row by
    /// [`CallLine::join`](kourier_model::CallLine::join) instead of being a duplicate, in any
    /// arrival order. A line is found by its conversation and its `local` on every network,
    /// never account-wide: the same call id in two chats is two lines. See
    /// [`IncomingMessage::call`] for the ids, the fields, who may say what and the table of
    /// the signals. The phone deleting the call entry is [`BackendEvent::Deleted`] with
    /// `for_everyone: false`, `by: None` and [`IncomingMessage::call_target`] as its target;
    /// it may arrive before the line (it waits) and a deleted line never comes back.
    Message(IncomingMessage),
    /// Imported history: at most [`MAX_HISTORY_BATCH`] messages of one conversation.
    ///
    /// What a backend may rely on for a message of a batch (a HISTORY ROW), beyond what holds
    /// for a live [`Message`](Self::Message):
    ///
    /// - It is stored once: its id is `(author, local)` like a live one, a second delivery
    ///   changes nothing, and a copy that arrives live (before or after) is the same row.
    ///   It sorts by its `sent_at` among the history rows, whatever order batches and
    ///   messages come in; a live row sorts behind what is there when it ARRIVES (§3.9), so
    ///   one that came before the history row may sort in front of it.
    /// - It NEVER NOTIFIES and never enters the catch-up summary, whatever its `silent` and
    ///   `catch_up` say; its attachments wait for a click. (A row of a batch is meant; a LIVE
    ///   message that later fills the placeholder of an undecryptable history row is a live
    ///   message and is announced like one, while the row stays a history row and its
    ///   sender still gets no receipt.)
    /// - UNREAD: THE ROWS ARE THE TRUTH. [`IncomingMessage::read`] `false` on a message
    ///   somebody else wrote means the backend KNOWS it is unread: it counts for the
    ///   conversation's unread count and mentions exactly like a live unread row, until
    ///   the chat is read here or a read marker of another device covers it. A backend
    ///   whose history does not say which messages are unread says `read: true` for all of
    ///   them and states the count as [`ConversationPatch::unread_hint`], which is for
    ///   unread messages it has NO rows for and for nothing else: never send both for the
    ///   same messages (they would count twice).
    /// - When the chat is read here, our other devices are told about the history rows that
    ///   were unread like about live ones (`Command::SyncReadMarker`, whose `up_to` is
    ///   always a message, never a notice or a call line). Their SENDERS are not told: no
    ///   `Command::SendReceipts` names a history row, also with read receipts on.
    /// - Own messages: [`IncomingMessage::initial_out_state`] is their state; edited ones:
    ///   [`IncomingMessage::edits`]; timers: [`IncomingMessage::expiry`]; reactions:
    ///   [`IncomingMessage::reactions`]. Of these only `edits` is read for a history row
    ///   alone: the core reads the other three of a live message too.
    /// - A call line of the past ([`IncomingMessage::call`]) honours `read` and `expiry` as
    ///   a message does: a missed call the phone had not seen is unread and counts.
    ///
    /// An empty batch is allowed: it says where the conversation's import stands
    /// (`has_more`, `progress`) and nothing else.
    HistoryBatch {
        /// The conversation.
        conv: ConvKey,
        /// The messages.
        messages: Vec<IncomingMessage>,
        /// More batches follow for this conversation.
        #[serde(default)]
        has_more: bool,
        /// Overall import progress 0.0 … 1.0.
        #[serde(default)]
        progress: Option<f32>,
    },
    /// A message was edited.
    ///
    /// `body` is the new revision. For a message with attachments the edit is its new caption:
    /// the core keeps the message's attachment rows and files and takes only the text of
    /// `body` (`Body::Text`, or the `caption` of a `Body::Media`; an empty text or no caption
    /// removes the caption). Attachments listed in an edit's body are ignored: an edit never
    /// adds, removes or replaces a file. Every revision's body is kept, so earlier captions
    /// stay readable.
    ///
    /// Who checks what: the backend checks what only it sees, the wire form of the edit (it
    /// drops one its network's clients would not accept as an edit, and never emits `body`
    /// as anything but `Text` or `Media`). The core checks everything about the target,
    /// which only it sees: that `by` wrote it, that it is a message of the conversation
    /// `target.conv` names, the receive window, and that it can be edited at all. Only a
    /// text message, the caption of a message with attachments (where the conversation's
    /// `files` say every one of its kinds carries a caption; a caption can always be
    /// removed) and a message that is still an undecryptable placeholder take an edit; a
    /// sticker, a contact card, a location, a poll, a system notice, view-once content and
    /// unsupported content never do, and the core drops such an edit without a trace.
    Edited {
        /// The edited message (any of its revision keys).
        target: MsgTarget,
        /// Who edited; `None` = own account.
        #[serde(default)]
        by: Option<UserId>,
        /// The `local` of the new revision (Signal: the edit's timestamp).
        #[serde(default)]
        new_local: Option<Arc<str>>,
        /// The new content.
        body: Body,
        /// When (ms): the server's time for the edit where the network gives one (an edit
        /// synced from another own device may carry its own timestamp). The core measures
        /// `edit_receive` from the original's `server_at` (its `sent_at` when it has none)
        /// to this, so set `IncomingMessage.server_at` on messages wherever the network
        /// dates them.
        edited_at: i64,
        /// Whether the edit still carries the quote of the message it edits. `Some(false)`:
        /// it has none, and a quote the message had goes with this revision: an edit can
        /// remove a quote, it cannot change one (Signal Desktop,
        /// `SD/ts/util/handleEditMessage.preload.ts`: "quote can be removed but not
        /// modified"). `Some(true)` and `None` (the network's edits say nothing about
        /// quotes) leave the quote as it is. Once removed, a later edit does not bring it
        /// back.
        #[serde(default)]
        keeps_quote: Option<bool>,
    },
    /// A message was deleted.
    ///
    /// For everyone: the core applies it when `by` wrote the message, or when the
    /// conversation's features have `admin_delete` and `by` is an admin of it in the core's
    /// member list. `by: None` with a target another member wrote is our own account's admin
    /// delete, synced from another own device: it counts when one of our own ids is an admin.
    /// `delete_receive` is measured like `edit_receive`, from the target's `server_at` to
    /// `at`. The target must be a message of the conversation `target.conv` names.
    Deleted {
        /// The message.
        target: MsgTarget,
        /// Who deleted; `None` = own account.
        #[serde(default)]
        by: Option<UserId>,
        /// For everyone (else a delete-for-me sync from another own device).
        for_everyone: bool,
        /// When (ms): the server's time for the delete where the network gives one.
        at: i64,
    },
    /// A reaction was added or removed.
    Reaction {
        /// The message.
        target: MsgTarget,
        /// Who reacted; `None` = own account.
        #[serde(default)]
        by: Option<UserId>,
        /// The emoji.
        emoji: String,
        /// Removed instead of added.
        #[serde(default)]
        remove: bool,
        /// When (ms).
        at: i64,
    },
    /// Delivery, read or viewed receipts from one user.
    ///
    /// A receipt counts only for a message of our own that went to `by`: `by` is the peer of
    /// the message's direct chat, a member of its group, or has a send state on it. From
    /// anyone else it changes nothing (Signal Desktop: `MessageReceipts.getTargetMessage`
    /// wants the sender among the message's recipients); the core keeps such a receipt for
    /// two days and looks again when `by` turns out to be a recipient after all: another id
    /// of one (`IdentityMerged`), a member the conversation's next update names
    /// (`ConversationUpsert`), or someone the message's send state names (`OwnSendState`,
    /// the answer of the transmit). A receipt whose `by` is one of our own ids is our own read or
    /// view of an incoming message on another device.
    ///
    /// A backend that knows which conversation the receipt was sent for states it in each
    /// target's `conv` (WhatsApp and Threema receipts name the chat): the receipt then only
    /// ever lands on a message of that conversation, like every modifier that states one.
    /// `conv: None` (Signal: a receipt carries timestamps and nothing else) is looked up
    /// account-wide.
    Receipts {
        /// The messages; each `conv` is the conversation the receipt was sent for, when the
        /// network says.
        targets: Vec<MsgTarget>,
        /// Who.
        by: UserId,
        /// What.
        kind: ReceiptKind,
        /// When (ms).
        at: i64,
    },
    /// We read up to here on another own device.
    ReadMarker {
        /// The conversation, when the network says.
        #[serde(default)]
        conv: Option<ConvKey>,
        /// Newest message read.
        up_to: MsgTarget,
    },
    /// Per-recipient send state of an own message (sync transcripts, late outcomes).
    OwnSendState {
        /// Our message row, when the backend knows it.
        #[serde(default)]
        local: Option<LocalMsgId>,
        /// The message.
        target: MsgTarget,
        /// Per-recipient state.
        per_recipient: Vec<(UserId, RecipientState)>,
    },
    /// A durable command is in the backend's own job table (§4.8).
    CommandPersisted {
        /// The `pending_commands` row.
        cmd: CmdId,
    },
    /// Ephemeral: someone is typing (or stopped).
    Typing {
        /// The conversation.
        conv: ConvKey,
        /// Who.
        by: UserId,
        /// From which device.
        #[serde(default)]
        device: Option<u32>,
        /// Typing or stopped.
        on: bool,
        /// Clear after this long without a refresh.
        ttl: Duration,
    },
    /// Ephemeral: upload progress of attachment `index` of an own message.
    AttachmentProgress {
        /// The message row.
        local: LocalMsgId,
        /// Attachment index.
        index: u16,
        /// Bytes done.
        done: u64,
        /// Total bytes.
        total: u64,
    },
    /// Ephemeral: download progress of a `DownloadAttachment`.
    DownloadProgress {
        /// The attachment row.
        attachment: AttachmentId,
        /// Bytes done.
        done: u64,
        /// Total bytes.
        total: u64,
    },
    /// A `DownloadAttachment` finished. Durable: sent with an ack, so the core records the
    /// file before the backend forgets the job.
    AttachmentDone {
        /// The attachment row.
        attachment: AttachmentId,
        /// The `DownloadAttachment.request` this answers. `None` only in rows written before
        /// the field existed: the core then applies it to whatever download is in flight.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request: Option<DownloadId>,
        /// What was written, or why not.
        result: Result<AttachmentMeta, BackendError>,
    },
    /// Ephemeral: transmit progress of an own message. The backend reports it while an
    /// attempt runs, at most a few times a second; a restart starts the attempt over.
    TransmitProgress {
        /// The message row.
        local: LocalMsgId,
        /// The upload share of its attachments, 0.0 … 1.0 (0.0 for a message without
        /// attachments).
        progress: f32,
        /// What the send is doing now (`None`: no step, only the upload share is known; it
        /// ends the step shown, so the uploads' end reports `None`). The UI keeps the last
        /// one until the next report or until the row's state changes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase: Option<kourier_model::SendPhase>,
    },
    /// A banner for the user.
    Notice(BackendNotice),
    /// Ephemeral: where the history transfer of a new link stands (waiting for the phone,
    /// downloading with its bytes, reading, done, skipped, failed with a kind). Sent at every
    /// change of state, a few times a second while a download moves, and again, whatever
    /// the state, every time the actor starts and at every `Command::Connect`: also a state
    /// that is over (done, skipped, failed) is said again, until
    /// `Command::DismissHistoryTransfer` makes the backend forget it, so a UI that came up
    /// later, or was not listening when the transfer ended, still learns how it ended. A
    /// receiver therefore takes a state as "this is where it stands now", never as a step
    /// in a sequence: the same state can come any number of times, and the bytes of a
    /// download can be lower than the last ones reported (after a restart the count starts
    /// from what is on disk). While the state is not over the backend delivers no message;
    /// `Command::SkipHistoryTransfer` ends a wait or a download.
    HistoryTransfer(HistoryTransfer),
    /// Ephemeral: how far the work of a newly linked account is that the user waits for
    /// while the chats are already usable: the delivery of an imported history (messages
    /// delivered and in all), then the profiles and the groups the link still fetches. Sent
    /// while that work runs, when the numbers change, and once more with `finished` set when
    /// nothing is left. Never stored: after a restart the backend reports again if work is
    /// left. A backend that has no such work never sends it.
    AccountProgress(AccountProgress),
    /// Ephemeral: what became of this device on the network after `Command::Logout` (removed,
    /// none to remove, the network not reached) and the name it was linked with. Sent once,
    /// right before the actor exits `LoggedOut` for that command; the core tells the user,
    /// once, whether the phone still lists the device. Never sent for an unlink by the phone.
    Logout(LogoutReport),
}

impl BackendEvent {
    /// The variant name (log-safe).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Login(_) => "Login",
            Self::AccountReady { .. } => "AccountReady",
            Self::AccountSettings(_) => "AccountSettings",
            Self::AccountCaps(_) => "AccountCaps",
            Self::ContactUpsert(_) => "ContactUpsert",
            Self::IdentityMerged { .. } => "IdentityMerged",
            Self::ConversationUpsert(_) => "ConversationUpsert",
            Self::PinnedConversations(_) => "PinnedConversations",
            Self::ConversationRemoved { .. } => "ConversationRemoved",
            Self::ConversationCleared { .. } => "ConversationCleared",
            Self::AttachmentDeleted { .. } => "AttachmentDeleted",
            Self::AttachmentRestore { .. } => "AttachmentRestore",
            Self::Message(_) => "Message",
            Self::HistoryBatch { .. } => "HistoryBatch",
            Self::Edited { .. } => "Edited",
            Self::Deleted { .. } => "Deleted",
            Self::Reaction { .. } => "Reaction",
            Self::Receipts { .. } => "Receipts",
            Self::ReadMarker { .. } => "ReadMarker",
            Self::OwnSendState { .. } => "OwnSendState",
            Self::CommandPersisted { .. } => "CommandPersisted",
            Self::Typing { .. } => "Typing",
            Self::AttachmentProgress { .. } => "AttachmentProgress",
            Self::DownloadProgress { .. } => "DownloadProgress",
            Self::AttachmentDone { .. } => "AttachmentDone",
            Self::TransmitProgress { .. } => "TransmitProgress",
            Self::Notice(_) => "Notice",
            Self::HistoryTransfer(_) => "HistoryTransfer",
            Self::AccountProgress(_) => "AccountProgress",
            Self::Logout(_) => "Logout",
        }
    }

    /// Ephemeral events are never stored and travel with `ack: None`: login steps, typing and
    /// progress. Every other event is durable.
    pub fn is_ephemeral(&self) -> bool {
        matches!(
            self,
            Self::Login(_)
                | Self::Typing { .. }
                | Self::AttachmentProgress { .. }
                | Self::DownloadProgress { .. }
                | Self::TransmitProgress { .. }
                | Self::HistoryTransfer(_)
                | Self::AccountProgress(_)
                | Self::Logout(_)
        )
    }

    /// Checks the grammar of the conversation keys and message ids inside (their fields are
    /// public, so they can be built unchecked).
    pub fn validate_ids(&self) -> Result<(), IdError> {
        let target = validate_target;
        match self {
            Self::ConversationUpsert(p) => p.key.validate(),
            Self::PinnedConversations(keys) => keys.iter().try_for_each(ConvKey::validate),
            Self::ConversationRemoved { conv } | Self::Typing { conv, .. } => conv.validate(),
            Self::Message(m) => m.validate_ids(),
            Self::HistoryBatch { conv, messages, .. } => {
                conv.validate()?;
                messages.iter().try_for_each(IncomingMessage::validate_ids)
            }
            Self::Edited {
                target: t,
                new_local,
                ..
            } => {
                target(t)?;
                new_local.as_deref().map_or(Ok(()), validate_local)
            }
            Self::Deleted { target: t, .. }
            | Self::Reaction { target: t, .. }
            | Self::AttachmentDeleted { target: t, .. }
            | Self::AttachmentRestore { target: t, .. }
            | Self::OwnSendState { target: t, .. } => target(t),
            Self::ConversationCleared { conv, up_to, .. } => {
                conv.validate()?;
                up_to.iter().try_for_each(target)
            }
            Self::Receipts { targets, .. } => targets.iter().try_for_each(target),
            Self::ReadMarker { conv, up_to } => {
                if let Some(c) = conv {
                    c.validate()?;
                }
                target(up_to)
            }
            Self::Login(_)
            | Self::AccountReady { .. }
            | Self::AccountSettings(_)
            | Self::AccountCaps(_)
            | Self::ContactUpsert(_)
            | Self::IdentityMerged { .. }
            | Self::CommandPersisted { .. }
            | Self::AttachmentProgress { .. }
            | Self::DownloadProgress { .. }
            | Self::AttachmentDone { .. }
            | Self::TransmitProgress { .. }
            | Self::HistoryTransfer(_)
            | Self::AccountProgress(_)
            | Self::Logout(_)
            | Self::Notice(_) => Ok(()),
        }
    }
}

/// What [`BackendEvent::AttachmentRestore`] reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestoreUpdate {
    /// This device's request went out: the wait for an answer starts now.
    Sent,
    /// This device's request was not sent and will not be (it was given up): the wait ends
    /// as if the other device had not answered.
    NotSent,
    /// The other device's answer: one slot per file of the message in the other device's
    /// order (the files a network can restore: no quote thumbnail, link preview picture or
    /// contact avatar; Signal puts a sticker's image first), and the rest of a long text in
    /// a slot of its own.
    Answer {
        /// The files, in the other device's order.
        attachments: Vec<RestoreSlot>,
        /// The rest of the message's long text, when the answer has a slot for it.
        long_text: Option<RestoreSlot>,
    },
    /// The other device does not have the message (or no file of it).
    MessageNotFound,
}

/// One file of a [`RestoreUpdate::Answer`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestoreSlot {
    /// The file was uploaded again: its new pointer.
    File {
        /// The new network pointer (becomes `Attachment::remote`).
        remote: BackendBlob,
        /// The names the pointer gives the file (Signal: `uuid:` of its client UUID, the new
        /// `digest:`), in order of priority. The core matches a slot to an attachment by
        /// these first and adds the ones the attachment lacks. Redacted in `Debug`.
        net_ids: Redacted<Vec<String>>,
        /// The plaintext size the pointer states.
        size: u64,
    },
    /// The other device is still working on it (uploading): wait for a later answer.
    Pending,
    /// The other device does not have this file.
    Gone,
    /// Nothing usable (a pointer this build cannot fetch, an empty slot): the slot keeps
    /// its place and changes nothing.
    Skip,
}

/// Checks a message reference: its conversation key, `local` and alternates.
fn validate_target(t: &MsgTarget) -> Result<(), IdError> {
    if let Some(c) = &t.conv {
        c.validate()?;
    }
    validate_local(&t.local)?;
    t.alternates.iter().try_for_each(|a| validate_local(a))
}

/// A reaction carried by a history import.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryReaction {
    /// Who reacted; `None` = own account.
    #[serde(default)]
    pub by: Option<UserId>,
    /// The emoji.
    pub emoji: String,
    /// When (ms).
    pub at: i64,
}

/// A later version of an edited message of a history import ([`IncomingMessage::edits`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEdit {
    /// The `local` of this version (Signal: the edit's timestamp): what a later reaction,
    /// receipt, quote or edit names when it means the message as it stood after this edit.
    pub local: Arc<str>,
    /// When the edit was made (ms).
    pub at: i64,
}

/// Most [`IncomingMessage::edits`] the core reads of one message (the edit cap of the limits
/// table): of a longer list the NEWEST this many, so the newest version's id always resolves.
pub const MAX_HISTORY_EDITS: usize = 100;

/// A message from the network (live or history). Its id is `(origin author, local)`; own
/// messages (`Origin::Own*`) have no author.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IncomingMessage {
    /// The conversation.
    pub conv: ConvKey,
    /// Network-local part of the id (Signal: sent timestamp).
    pub local: Arc<str>,
    /// Author and device.
    pub origin: Origin,
    /// Content; the core drops invalid text ranges on ingest.
    pub body: Body,
    /// Quoted message.
    #[serde(default)]
    pub reply_to: Option<ReplyTo>,
    /// Sender's timestamp (ms).
    pub sent_at: i64,
    /// Server timestamp (ms).
    #[serde(default)]
    pub server_at: Option<i64>,
    /// Local wall clock when the backend's stage 1 stored it (ms).
    pub received_at: i64,
    /// The backend's stage-1 arrival counter (ties in `sort_key`, §3.9).
    #[serde(default)]
    pub arrival: Option<u64>,
    /// Disappearing timer. `started_at` is honoured as given, live and in a history batch
    /// (the timer goes on from where the network had it). Without a start the core decides:
    ///
    /// - an incoming message's `AfterRead` timer starts when the message is read (here, or
    ///   by a read marker of another device); at once when it is already read;
    /// - an `AfterSend` timer, and any timer of an OWN message, counts from `sent_at`.
    ///   EXCEPT an own message that was not sent: when
    ///   [`initial_out_state`](Self::initial_out_state) says queued, sending or failed (a
    ///   history import of a message still pending on another device), the timer is kept
    ///   WITHOUT a start, so the message is not erased `timer` after a send that never
    ///   happened; it starts when a later copy of the message says it was sent (the echo
    ///   of the sync transcript). So a backend states such a timer and no start; it must
    ///   not drop the timer;
    /// - a notice somebody else caused (`Body::System` from a remote origin) is never
    ///   unread, so an `AfterRead` timer without a start starts when the notice is stored;
    /// - a call line: see [`IncomingMessage::call`] (live: the chat's timer, this field is
    ///   not read; in a history batch: this field).
    ///
    /// A backend that does not want a row erased by this computer's clock gives no timer;
    /// one whose start would lie in the future must replace it itself.
    #[serde(default)]
    pub expiry: Option<Expiry>,
    /// View-once.
    #[serde(default)]
    pub view_once: bool,
    /// We are mentioned.
    #[serde(default)]
    pub mentions_me: bool,
    /// Never notify (history, …). A message of a history batch never notifies, whatever
    /// this says.
    #[serde(default)]
    pub silent: bool,
    /// Arrived before the catch-up boundary: summarized, not notified one by one (§4.5).
    #[serde(default)]
    pub catch_up: bool,
    /// Own messages in history imports: their state.
    #[serde(default)]
    pub initial_out_state: Option<OutgoingState>,
    /// Already read elsewhere. Read for a message somebody else wrote (and for a call line
    /// that counts as unread); an own message and a notice have no read state.
    ///
    /// Live: `true` stores the message as read (no notification). In a HISTORY batch the
    /// flag is the truth about the message: `false` means the backend KNOWS it is unread,
    /// and the core counts it (unread count, mentions) like a live unread row, without ever
    /// notifying; `true` means read, or NOT KNOWN: a backend whose history does not say
    /// which messages are unread sets `true` on every row and gives the count as
    /// [`ConversationPatch::unread_hint`]. A copy of a message that is here already raises
    /// it to read when it says `true` and never makes a read message unread again, so a
    /// row delivered twice, or after the chat was read here, changes nothing.
    #[serde(default)]
    pub read: bool,
    /// Reactions (history imports).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<HistoryReaction>,
    /// The message was edited before it was imported: the later versions' ids and times,
    /// OLDEST FIRST. Then `local` and `sent_at` are the ORIGINAL's (the message's id never
    /// changes), `body` is the newest version's content, and the last entry is the newest
    /// version. The earlier versions' contents are not imported.
    ///
    /// Read ONLY for a message of a [`BackendEvent::HistoryBatch`] that makes a NEW row (or
    /// fills the row of an undecryptable placeholder) and is no notice: the core then marks
    /// the row as edited (the number of versions, the newest edit's time) and keeps each
    /// version's id as a key of the message, so a later reaction, receipt, quote or edit
    /// that names the newest version's id finds it, also one that arrived before the
    /// import. A row that is there already ignores the field (a second delivery, a live
    /// copy that came first), and so does a live message (an edit of a live message is
    /// [`BackendEvent::Edited`]). A version's `local` must not be the `local` of another
    /// message of the same author: a reference to an id means the message that has it once
    /// both are stored, but a modifier that waited for the id is taken by whichever of the
    /// two is stored first, and the core cannot tell them apart before. (Signal's version
    /// ids are the edits' sent times, which no two messages of one author share.) At most
    /// [`MAX_HISTORY_EDITS`] are read, of more the newest; each `local` is checked like
    /// every id of an envelope ([`Envelope::validate`]); an entry under the message's own
    /// `local` and a repeated `local` are passed over.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edits: Vec<HistoryEdit>,
    /// Per-member copy of this own broadcast send (Threema distribution lists).
    #[serde(default)]
    pub linked_local: Option<LocalMsgId>,
    /// Raw payload (kept for `Body::Unsupported`).
    #[serde(default)]
    pub raw: Option<BackendBlob>,
    /// Backend per-row metadata.
    #[serde(default)]
    pub meta: Option<BackendBlob>,
}

impl IncomingMessage {
    /// A live message with every optional field at its default.
    pub fn new(
        conv: ConvKey,
        local: impl Into<Arc<str>>,
        origin: Origin,
        body: Body,
        sent_at: i64,
        received_at: i64,
    ) -> Self {
        Self {
            conv,
            local: local.into(),
            origin,
            body,
            reply_to: None,
            sent_at,
            server_at: None,
            received_at,
            arrival: None,
            expiry: None,
            view_once: false,
            mentions_me: false,
            silent: false,
            catch_up: false,
            initial_out_state: None,
            read: false,
            reactions: Vec::new(),
            edits: Vec::new(),
            linked_local: None,
            raw: None,
            meta: None,
        }
    }

    /// One signal of a call, as the message that carries it. Kourier shows calls; it never
    /// takes one and never sends anything for one, and neither may a backend.
    ///
    /// - `conv`: the chat the line is shown in: `ConvKey::Direct(peer)` for a 1:1 call (the
    ///   peer in every signal, also in what our own devices sync), the group for a group call.
    /// - `call_id`: the network's id of the call, the same text in every signal of it
    ///   (Signal: the call id as an unsigned decimal; for a group the ring id, which is also
    ///   what an era id maps to). The row's `local` is
    ///   [`CallLine::local`](kourier_model::CallLine::local): `call:<call_id>` or
    ///   `gcall:<call_id>` by `line.group`. The id is chosen by the other side: the core
    ///   finds a line by (`conv`, `local`) on every network, whatever its id scope, so an id
    ///   named in one chat never touches, hides or blocks a call of another chat.
    /// - `origin`: WHO SAID IT, which bounds what it may say.
    ///   [`Origin::Remote`] is the other side of the call (the caller, the ringer, the member
    ///   who sent a group-call update): it may only say that an INCOMING call was started,
    ///   rings or was missed ([`CallLine::may_come_from_peer`](kourier_model::CallLine::may_come_from_peer)).
    ///   Anything else from it is a contract violation
    ///   ([`ContractViolation::CallLine`]): the envelope is refused.
    ///   [`Origin::OwnOtherDevice`] is one of the user's own devices (the phone's call-event
    ///   sync, a sync transcript): only it says answered, declined, unanswered and anything of
    ///   an outgoing call, and only its delete removes a line. Those states are above
    ///   everything the other side may say, so the other side never moves a line an own
    ///   device settled.
    /// - `line`: the signal ([`CallLine::signal`](kourier_model::CallLine::signal) with the
    ///   signal's time, plus `with_media` and `with_by` for what the signal says). A ringing
    ///   state carries a deadline in the terms of `at`: `deadline - at` is the time the ring
    ///   has LEFT when the signal was received (the ring time minus the signal's age, as the
    ///   network measures age; zero or less when it is over). The core never compares that
    ///   deadline with its own clock: it adds the time left, at least nothing and at most
    ///   [`MAX_RING_MS`](kourier_model::MAX_RING_MS) (two minutes), to `received_at`
    ///   ([`CallLine::on_local_clock`](kourier_model::CallLine::on_local_clock)). A ring with
    ///   no time left never rings: its line is a missed call at once.
    /// - `at`: when the signal was made (ms): `sent_at`. The line shows the earliest `sent_at`
    ///   of its signals. `received_at` is the LOCAL time stage 1 stored the signal (the same
    ///   value on every replay of the signal).
    ///
    /// The signals, as Signal has them (other networks map theirs to the same lines):
    ///
    /// | Signal on the wire | `origin` | The line it makes or joins |
    /// |---|---|---|
    /// | 1:1 Offer | `Remote` (the caller) | incoming, ringing (`deadline - at` = time left), voice or video, by the caller |
    /// | 1:1 Offer with no time left (older than the ring time) | `Remote` | the same signal: the core makes it a missed call; set `catch_up` when it is from before the catch-up boundary |
    /// | the caller's Hangup NORMAL (the caller gave up) | `Remote` | incoming, missed. A Hangup of another type (accepted, declined or busy on another of our devices, need-permission) is NOT reported: the peer cannot say what the user did, and reporting it as missed would announce every call answered on the phone as missed until the phone's sync arrives |
    /// | sync `CallEvent` INCOMING ACCEPTED | `OwnOtherDevice` | incoming, answered, voice or video, by the peer |
    /// | sync `CallEvent` INCOMING NOT_ACCEPTED | `OwnOtherDevice` | incoming, declined, voice or video, by the peer |
    /// | sync `CallEvent` OUTGOING ACCEPTED | `OwnOtherDevice` | outgoing, answered, voice or video |
    /// | sync `CallEvent` OUTGOING NOT_ACCEPTED | `OwnOtherDevice` | outgoing, unanswered, voice or video |
    /// | sync `CallEvent` DELETE | (own devices only) | [`BackendEvent::Deleted`] of [`IncomingMessage::call_target`], `for_everyone: false`, `by: None` |
    /// | group ring RING | `Remote` (the ringer) | incoming group call, ringing, by the ringer |
    /// | group ring CANCELLED | `Remote` (the ringer) | incoming group call, missed |
    /// | `groupCallUpdate` of a member | `Remote` (its sender) | incoming group call, started, by its sender |
    /// | `groupCallUpdate` in our own sync transcript | `OwnOtherDevice` | incoming group call, answered (the user is in it), names nobody |
    /// | sync `CallEvent` GROUP JOINED or ACCEPTED | `OwnOtherDevice` | incoming group call, answered, names nobody |
    /// | sync `CallEvent` GROUP NOT_ACCEPTED | `OwnOtherDevice` | incoming group call, declined, names nobody |
    ///
    /// A missed call that the phone never reports is derived: the caller's hang-up, or the
    /// deadline of a ring that no other signal followed (the core's clock).
    ///
    /// The row of a call line has NO author: who rang is `line.by`, and the group sync of a
    /// call names nobody, so the id cannot depend on it. Of `origin` only "the other side or
    /// one of our own devices" is read. A backend reports what the user's other devices say
    /// (the phone's `CallEvent` syncs) as `OwnOtherDevice`, never as `Remote` with one of our
    /// ids: `Envelope::validate` cannot know our ids and refuses an own-only state from any
    /// `Remote` origin. The caller's hang-up is reported only when the caller gave up
    /// (Signal: `Hangup` NORMAL); a hang-up that says another of our devices took or declined
    /// the call is NOT reported as missed (the phone's sync, or the deadline, settles the
    /// line), or every call answered on the phone would first be announced as missed. In a
    /// history batch `at` and `received_at` are the times of the original signal, never the
    /// time of the import: a ring of the past then has no time left and is a missed call,
    /// while a time of the import would make it ring again.
    ///
    /// Not read: `reply_to`, `view_once`, `mentions_me`, `initial_out_state`, `reactions`,
    /// `edits`, `linked_local`, `raw` and `meta`. What is read besides the line: `catch_up`
    /// (a missed call from before the catch-up boundary is counted in the summary, not
    /// announced alone), `silent` (never announced), `read` (this signal never makes the
    /// line unread), `server_at`, `arrival`, and `expiry`, which is read only in a history
    /// batch:
    ///
    /// - LIVE, the core gives the line the chat's disappearing timer itself, started when
    ///   the line is stored, as for a message that is read at once; `expiry` is not read.
    /// - In a HISTORY BATCH the line takes the timer its `expiry` states, as an imported
    ///   message does, and never the chat's: a start that is given stands; without one an
    ///   `AfterSend` timer counts from the call's time, and an `AfterRead` timer starts
    ///   when the line is read, which for a line that is not unread is when it is stored.
    ///   And the line follows its `read` flag: a missed call (or a group call somebody
    ///   started) that says `read: false` is unread and counts for the chat like an unread
    ///   message; with `read: true` it is read. It is never announced. A backend that does
    ///   not know whether the user saw a past call says `read: true`. A line that still
    ///   RINGS when it is imported is the exception: a ringing line has no read state, and
    ///   when its deadline settles it as a missed call the core has no `read` to hand (the
    ///   deadline task knows no signal), so it is unread, silently. A backend reports the
    ///   state after a call, not a ring, in a history batch; and a later history signal of
    ///   the same call that says `read: true` does not raise a line that is unread here
    ///   (unlike a copy of a message).
    ///
    /// Nothing of a call line is ever sent: no receipt, no read sync, no delete sync.
    pub fn call(
        conv: ConvKey,
        call_id: &str,
        origin: Origin,
        line: kourier_model::CallLine,
        at: i64,
        received_at: i64,
    ) -> Self {
        let local = kourier_model::CallLine::local(line.group, call_id);
        Self::new(
            conv,
            local,
            origin,
            Body::System(kourier_model::SystemEvent::Call(line)),
            at,
            received_at,
        )
    }

    /// The reference to the line of call `call_id` in `conv` (for [`BackendEvent::Deleted`]):
    /// the conversation, no author (`TargetAuthor::Own`) and the line's `local`.
    pub fn call_target(conv: ConvKey, group: bool, call_id: &str) -> MsgTarget {
        MsgTarget {
            conv: Some(conv),
            author: kourier_model::TargetAuthor::Own,
            local: kourier_model::CallLine::local(group, call_id).into(),
            alternates: Vec::new(),
        }
    }

    /// The rules of a call line (see [`IncomingMessage::call`]): its `local` has the prefix
    /// of its kind and a call id; a 1:1 line is in a direct chat, a group line in no direct
    /// chat; and the other side of a call says only what it may. A message that is not a
    /// call line passes.
    fn validate_call(&self) -> Result<(), &'static str> {
        let Some(line) = self.body.call_line() else {
            return Ok(());
        };
        if !self.origin.is_own() && !line.may_come_from_peer() {
            return Err(
                "the other side of a call may only say that an incoming call was started, \
                 rings or was missed",
            );
        }
        if !line.owns_local(&self.local) {
            return Err("the id of a call line is call:<id> (1:1) or gcall:<id> (group)");
        }
        match (&self.conv, line.group) {
            (ConvKey::Direct(_), true) => Err("a group call line in a direct chat"),
            (ConvKey::Direct(_), false) => Ok(()),
            (_, true) => Ok(()),
            (_, false) => Err("a 1:1 call line outside a direct chat"),
        }
    }

    /// The message id: the remote author (`None` for own messages, and for a call line, whose
    /// row has no author whoever sent the signal) and `local`.
    pub fn net_id(&self) -> NetMsgId {
        let author = match self.body.call_line() {
            Some(_) => None,
            None => self.origin.author().cloned(),
        };
        NetMsgId {
            author,
            local: self.local.clone(),
        }
    }

    fn validate_ids(&self) -> Result<(), IdError> {
        self.conv.validate()?;
        validate_local(&self.local)?;
        self.edits
            .iter()
            .try_for_each(|edit| validate_local(&edit.local))?;
        self.reply_to
            .as_ref()
            .map_or(Ok(()), |r| validate_target(&r.target))
    }
}

/// Something the user should see as a banner.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum BackendNotice {
    /// A contact's safety number changed; sends to them pause until `ConfirmIdentity`.
    IdentityChanged {
        /// Whose identity.
        user: UserId,
    },
    /// The network wants a challenge (captcha) solved, and the backend met it outside a
    /// `Transmit` (a job: receipts, a read sync, a timer update). The core shows the account
    /// as `ConnState::NeedsUserAction(SolveChallenge { url })`, exactly as for a challenged
    /// send, until the user's answer is accepted (`Command::SubmitChallenge`, with
    /// `token: None` unless a send was challenged too), `retry_after` has passed, the backend
    /// reports `Online` anew, or the actor restarts. Sends are not held for it: a send the
    /// network challenges says so itself (`ErrorKind::Challenge`).
    ChallengeRequired {
        /// Where the user solves it.
        url: String,
        /// When the network takes such requests again without an answer (its Retry-After),
        /// counted from when the core applies the notice; the core takes the state back
        /// then. `None`: until one of the other ends.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after: Option<Duration>,
    },
    /// Any other problem; `user_message` is the banner text.
    Error(BackendError),
}

/// One unit of delivery to the core, applied in one `core.db` transaction.
#[derive(Debug)]
pub struct Envelope {
    /// The events, in order.
    pub events: Vec<BackendEvent>,
    /// Completed by the core after its commit; `None` only for ephemeral events.
    pub ack: Option<Ack>,
}

impl Envelope {
    /// An envelope of durable events and the waiter that resolves after the core's commit.
    pub fn durable(events: Vec<BackendEvent>) -> (Self, AckWaiter) {
        let (ack, waiter) = Ack::pair();
        (
            Self {
                events,
                ack: Some(ack),
            },
            waiter,
        )
    }

    /// An envelope of one ephemeral event (no ack).
    pub fn ephemeral(event: BackendEvent) -> Self {
        Self {
            events: vec![event],
            ack: None,
        }
    }

    /// [`Envelope::validate`] plus the rule of a link in progress (fresh link or relink, until
    /// the core acked `AccountReady`): a durable envelope must start with
    /// [`BackendEvent::AccountReady`]. Ephemeral envelopes (login steps) pass.
    pub fn validate_linking(&self) -> Result<(), ContractViolation> {
        self.validate()?;
        if self.ack.is_none() {
            return Ok(());
        }
        match self.events.first() {
            Some(BackendEvent::AccountReady { .. }) | None => Ok(()),
            Some(first) => Err(ContractViolation::DurableBeforeAccountReady(first.name())),
        }
    }

    /// The contract rules the core (and the fake's conformance suite) enforce: not empty;
    /// durable events only with an ack; ephemeral events only without one; history batches
    /// within [`MAX_HISTORY_BATCH`] and of one conversation; well-formed ids; a call line
    /// under the id and in the kind of chat of its kind, and from the other side of the call
    /// only what it may say ([`IncomingMessage::call`]), live or in a history batch (an
    /// import may carry the calls of the past: such a line is never announced, is unread
    /// when its `read` flag says so (a line that still "rings" when it is imported is the
    /// exception, [`IncomingMessage::call`]), and one that still "rings" is settled by its
    /// deadline like any other); the ids of a history message's
    /// [`edits`](IncomingMessage::edits).
    pub fn validate(&self) -> Result<(), ContractViolation> {
        if self.events.is_empty() {
            return Err(ContractViolation::Empty);
        }
        for e in &self.events {
            match (e.is_ephemeral(), self.ack.is_some()) {
                (false, false) => return Err(ContractViolation::DurableWithoutAck(e.name())),
                (true, true) => return Err(ContractViolation::EphemeralWithAck(e.name())),
                _ => {}
            }
            if let BackendEvent::HistoryBatch { conv, messages, .. } = e {
                if messages.len() > MAX_HISTORY_BATCH {
                    return Err(ContractViolation::HistoryBatchTooLarge(messages.len()));
                }
                if messages.iter().any(|m| m.conv != *conv) {
                    return Err(ContractViolation::HistoryBatchMixesConversations);
                }
            }
            e.validate_ids()
                .map_err(|error| ContractViolation::InvalidId {
                    event: e.name(),
                    error,
                })?;
            match e {
                BackendEvent::Message(m) => m.validate_call(),
                BackendEvent::HistoryBatch { messages, .. } => {
                    messages.iter().try_for_each(IncomingMessage::validate_call)
                }
                _ => Ok(()),
            }
            .map_err(ContractViolation::CallLine)?;
        }
        Ok(())
    }
}

/// A broken contract rule in an [`Envelope`]. Log-safe.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ContractViolation {
    /// An envelope without events.
    #[error("empty envelope")]
    Empty,
    /// A durable event sent with `ack: None` (it bypassed `core_outbox`).
    #[error("durable event {0} without an ack")]
    DurableWithoutAck(&'static str),
    /// An ephemeral event sent with an ack (it went through `core_outbox`).
    #[error("ephemeral event {0} with an ack")]
    EphemeralWithAck(&'static str),
    /// A history batch over [`MAX_HISTORY_BATCH`] messages.
    #[error("history batch of {0} messages (max {MAX_HISTORY_BATCH})")]
    HistoryBatchTooLarge(usize),
    /// A history batch with a message of another conversation than the batch's.
    #[error("history batch with a message of another conversation")]
    HistoryBatchMixesConversations,
    /// A malformed conversation key or message id.
    #[error("invalid id in {event}: {error}")]
    InvalidId {
        /// The event.
        event: &'static str,
        /// What is wrong.
        error: IdError,
    },
    /// A call line under an id or in a chat that is not of its kind, or a signal of the
    /// other side of a call that says more than it may ([`IncomingMessage::call`]).
    #[error("call line: {0}")]
    CallLine(&'static str),
    /// During a link, a durable envelope that does not start with `AccountReady` (the named
    /// event came first). `AccountReady` is the first durable event of every link.
    #[error("durable event {0} before AccountReady while linking")]
    DurableBeforeAccountReady(&'static str),
}

/// The core's answer to an [`Envelope`]. Complete it only after the envelope's `core.db`
/// transaction committed (`Ok`) or failed (`Err`); the backend then deletes (or keeps) the
/// `core_outbox` rows. Dropping it counts as a retryable failure.
///
/// Poison policy (contract rule): `Err(PersistError { retryable: false, .. })` means the core
/// refused the content for good (a [`ContractViolation`], an undecodable payload). The backend
/// must not ship the same rows forever: it quarantines the refused row and continues with the
/// next one (`kourier_db::Forwarder::reject` splits a multi-row batch to find it). A retryable
/// `Err` keeps the rows; they ship again after a backoff.
#[derive(Debug)]
pub struct Ack(oneshot::Sender<Result<(), PersistError>>);

impl Ack {
    /// A new ack and the backend's waiter.
    pub fn pair() -> (Self, AckWaiter) {
        let (tx, rx) = oneshot::channel();
        (Self(tx), AckWaiter(rx))
    }

    /// Reports the outcome of the core transaction.
    pub fn complete(self, result: Result<(), PersistError>) {
        // The backend may have stopped waiting (shutdown); its rows are forwarded again.
        let _ = self.0.send(result);
    }
}

/// The backend's side of an [`Ack`].
#[derive(Debug)]
pub struct AckWaiter(oneshot::Receiver<Result<(), PersistError>>);

impl AckWaiter {
    fn dropped() -> PersistError {
        PersistError {
            retryable: true,
            detail: "ack dropped before completion".into(),
        }
    }

    /// Waits for the core's commit.
    pub async fn wait(self) -> Result<(), PersistError> {
        self.0.await.unwrap_or_else(|_| Err(Self::dropped()))
    }

    /// Blocking [`AckWaiter::wait`] for a synchronous forwarder thread. Panics if called from
    /// inside an async runtime (tokio's rule).
    pub fn blocking_wait(self) -> Result<(), PersistError> {
        self.0
            .blocking_recv()
            .unwrap_or_else(|_| Err(Self::dropped()))
    }
}

/// Format version written by [`encode_outbox_row`]: the first two bytes (little endian) of a
/// `core_outbox.envelope`, followed by the events as a JSON array.
pub const OUTBOX_FORMAT: u16 = 1;

/// Why a `core_outbox` row could not be encoded or decoded. Log-safe (never quotes the payload).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    /// A row needs at least one event.
    #[error("empty outbox row")]
    Empty,
    /// Ephemeral events never go through `core_outbox`.
    #[error("ephemeral event {0} cannot be stored")]
    Ephemeral(&'static str),
    /// The row is shorter than its format header.
    #[error("truncated outbox row")]
    Truncated,
    /// The row was written by a newer (or unknown) format.
    #[error("unknown outbox format {0}")]
    UnknownFormat(u16),
    /// The payload is not a list of events.
    #[error("outbox payload: {0}")]
    Payload(String),
}

impl CodecError {
    fn from_json(e: &serde_json::Error) -> Self {
        // serde_json messages can quote input; keep only the category and position.
        Self::Payload(format!(
            "{:?} error at line {} column {}",
            e.classify(),
            e.line(),
            e.column()
        ))
    }
}

/// What a `core_outbox` row may hold: at least one event, none of them ephemeral.
fn check_storable(events: &[BackendEvent]) -> Result<(), CodecError> {
    if events.is_empty() {
        return Err(CodecError::Empty);
    }
    match events.iter().find(|e| e.is_ephemeral()) {
        Some(e) => Err(CodecError::Ephemeral(e.name())),
        None => Ok(()),
    }
}

/// Encodes the durable events of one backend transaction as one `core_outbox` row
/// (`kourier_db::outbox::append`): format [`OUTBOX_FORMAT`], then the events in order.
pub fn encode_outbox_row(events: &[BackendEvent]) -> Result<Vec<u8>, CodecError> {
    check_storable(events)?;
    let mut row = OUTBOX_FORMAT.to_le_bytes().to_vec();
    serde_json::to_writer(&mut row, events).map_err(|e| CodecError::from_json(&e))?;
    Ok(row)
}

/// Decodes a row written by [`encode_outbox_row`]. A row that encoding would have refused
/// (no events, or an ephemeral one) is rejected too, so a forwarded envelope is never empty.
pub fn decode_outbox_row(row: &[u8]) -> Result<Vec<BackendEvent>, CodecError> {
    let (header, payload) = row.split_first_chunk::<2>().ok_or(CodecError::Truncated)?;
    let format = u16::from_le_bytes(*header);
    if format != OUTBOX_FORMAT {
        return Err(CodecError::UnknownFormat(format));
    }
    let events: Vec<BackendEvent> =
        serde_json::from_slice(payload).map_err(|e| CodecError::from_json(&e))?;
    check_storable(&events)?;
    Ok(events)
}

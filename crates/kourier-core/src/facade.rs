//! The UI facade (§8.5): [`CoreHandle`], [`UiCommand`], [`UiEvent`] and the query DTOs.
//!
//! The UI never touches a database. It sends [`UiCommand`]s (non-blocking) and receives
//! [`UiEvent`]s: invalidations after every commit, small payloads (connection state, login
//! steps, typing) and query results. A query carries the model's [`QueryToken`]; the UI applies a
//! result only if its generation is at least the last applied one, so out-of-order completions on
//! the two read connections never regress a model. `kourier-viewmodel` and `kourier-qt` may use
//! this module and nothing else of `kourier_core`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kourier_model::{
    AccountHold, AccountId, AccountProgress, AccountState, AccountUuid, AttachmentId,
    AttachmentKind, BlobRef, Body, ConnState, Contact, ConvKind, ConversationChange,
    ConversationFeatures, ConversationId, Deletion, Expiry, HistoryTransfer, LoginStep, Member,
    MemberRole, MessageId, NetworkKind, OutgoingState, QuoteSnapshot, ReadState, Redacted,
    SystemEvent, Text, UserId,
};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::error::Result;

/// The error of [`UiEvent::ActionFailed`] and of a failed query, so a UI can tell a refusal
/// from a network failure by its kind instead of its text, and one refusal from another by
/// its [`Refusal`] ([`CoreError::refusal`]).
pub use crate::error::{CoreError, Refusal};
pub use crate::expiry::ExpiryPolicy;
pub use crate::media::{MEDIA_HANDLES, MediaBlob, MediaEntry, MediaHandles, MediaVariant};
/// What [`UiEvent::Notice`] carries, so a front-end can name the notice it shows (the quiet line
/// for lost history, W4).
pub use kourier_backend_api::BackendNotice;
pub use kourier_blob::{SaveError, SaveMode};
pub use kourier_media::{Look, LookFormat};
/// Synthetic files that carry a made-up location (the Qt window tests and fixture pages of the
/// location question; nothing of a user's).
pub use kourier_media::{SampleLocated, sample_located};
/// The file-name extension of a MIME type and back: what the name of a saved file is judged by
/// (the viewmodel's `save_name`).
pub use kourier_media::{extension_for_mime, mime_from_extension};
pub use kourier_model as model;

/// Capacity of the [`UiEvent`] broadcast. A lagging receiver gets `RecvError::Lagged` and must
/// reload its visible models (Delta Chat pattern, §8.5).
pub const UI_EVENT_CAPACITY: usize = 4096;

/// Identifies one query of one model: `model` is chosen by the UI, `generation` increases with
/// every query of that model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QueryToken {
    /// The model (conversation list, a timeline, …).
    pub model: u32,
    /// Monotonic per model.
    pub generation: u64,
}

/// The id a UI gives one of its requests ([`UiCommand::SendFiles`]), echoed in everything the
/// core says about that request ([`UiEvent::FilesStored`], [`Progress::Import`],
/// [`RefusedFiles::request`]). The UI chooses it; the core only hands it back, so it need be
/// unique only among the requests the UI still waits for (a counter does). `RequestId(0)`,
/// the default, is as good as any other.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(pub u64);

/// Where [`UiCommand::SaveAttachment`] puts a copy: a folder the user chose and a file name.
/// Both are personal data: `Debug` shows neither (§9.1).
#[derive(Clone, PartialEq, Eq)]
pub struct SaveTarget {
    /// The folder the user chose (an absolute path).
    pub dir: PathBuf,
    /// The file name: ONE path component (the viewmodel cleans it, the writer refuses
    /// anything else).
    pub name: String,
}

impl std::fmt::Debug for SaveTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SaveTarget(<redacted>)")
    }
}

/// What the UI asks the core to do.
#[derive(Debug)]
pub enum UiCommand {
    /// The first frame is shown: connect every active account (§4.3 step 3).
    UiReady,
    /// Start linking a new account of `network` with login flow `flow` (§4.3 step 1). Any risk
    /// notice was accepted by the user before this: the consent belongs to the front-end (a
    /// page with a tick box, architecture §8.3); nothing below it checks that it was asked
    /// (§8.10 R3).
    AddAccount {
        /// The network.
        network: NetworkKind,
        /// A flow id from the network's descriptor (`"qr"`).
        flow: String,
    },
    /// Answer an `Input` login step.
    SubmitLoginInput {
        /// The linking account.
        account: AccountId,
        /// The step's `txn`.
        txn: u64,
        /// `(field id, value)`.
        values: Vec<(String, Redacted<String>)>,
    },
    /// Abort linking (the account is deleted, or stays logged out when relinking).
    CancelLogin {
        /// The linking account.
        account: AccountId,
    },
    /// "Link again" for a logged-out account (§4.3 step 6).
    Relink {
        /// The account.
        account: AccountId,
        /// Login flow.
        flow: String,
    },
    /// Remove the account and shred its keys (§4.3 step 7): the network is asked to forget the
    /// device (an actor is started for it when none runs), then every row and file of the
    /// account here goes. Answered with `LogoutOutcome` (what became of the device) and
    /// `AccountRemoved`.
    RemoveAccount {
        /// The account.
        account: AccountId,
    },
    /// "Unlink…": the device logs out on the network (an actor is started for it when none
    /// runs) and the account stays logged out with its chats readable, as after an unlink by
    /// the phone; `Relink` links it again. Answered with `LogoutOutcome`. Ignored for an
    /// account that is logged out or still linking. Durable like a removal: the account is
    /// `Unlinking` meanwhile, and a start that finds it makes one more attempt and finishes it.
    Unlink {
        /// The account.
        account: AccountId,
    },
    /// Restart an account the supervisor gave up on (§4.3 step 4). For an account whose
    /// backend runs, it is the user's "Reconnect": the backend is told to connect, which a
    /// connection that waits for the user needs (`UserAction::Reconnect`, §6.4).
    RestartAccount {
        /// The account.
        account: AccountId,
    },
    /// The window focus or the open conversation changed (notifications, presence).
    SetPresence {
        /// A Kourier window has focus.
        focused: bool,
        /// The open conversation.
        visible: Option<ConversationId>,
    },
    /// The user read the conversation up to its newest message (durable read sync and read
    /// receipts, §4.8). Every unread row is read, imported ones too, and our other devices
    /// are told about each unread MESSAGE of them (a call line is told to nobody); the read
    /// sync names the newest message of the chat (never a notice or a call line). Read
    /// receipts go to the senders of the live messages only: the sender of a message that
    /// came with a history import gets none.
    MarkRead {
        /// The conversation.
        conv: ConversationId,
    },
    /// A roaming change (archive, pin, mute, …) sent to the network; confirmed by events.
    ChangeConversation {
        /// The conversation.
        conv: ConversationId,
        /// The change.
        change: ConversationChange,
    },
    /// Queue a text message (§4.8 enqueue: message row + outbox row in one transaction, then
    /// per-conversation FIFO transmit). Answered with invalidations; failures with `ActionFailed`.
    /// The compose commands (`SendText`, `SendFiles`, `RetryMessage`, `CancelMessage`,
    /// `EditMessage`, `DeleteMessage`, `React`, `SaveDraft`, `SetTyping`) take effect in the
    /// order they were sent: messages typed in a row keep their order and ids, an edit or a
    /// reaction right after a send applies to that message, and a draft saved just before a send
    /// is cleared by it.
    ///
    /// A text longer than one message of the conversation carries
    /// (`ConversationFeatures::max_text_bytes`) is not refused: it becomes several messages
    /// (`Text::split`: cut between lines or words, formatting kept), stored together and sent
    /// in order, the first with the quote. `text.split(max).len()` is how many.
    SendText {
        /// The conversation (its account sends).
        conv: ConversationId,
        /// Body with formatting ranges (UTF-8 offsets).
        text: Text,
        /// The message this one replies to.
        reply_to: Option<MessageId>,
    },
    /// Send files (§4.8, §5.8): any number of them, of any kinds, in one go. Each file is
    /// encrypted into the account's blob store with its metadata (and a thumbnail and blurhash
    /// for pictures); then the core splits the selection into as many messages as the
    /// network's packing rule needs so that every client shows every file
    /// (`ConversationFeatures::file_packing`, [`model::ConversationFeatures::plan`]):
    /// neighbours that may share a message do, every other file is a message of its own, in
    /// the user's order. What shares a message is the network's statement, by kind and by
    /// format (a network whose desktop client cannot draw HEIC pictures keeps them out of its
    /// albums); the core reads both from each file's content. The caption and the quote go
    /// on the first message; where that message can carry no caption (a voice note) the
    /// caption goes first as a text of its own, and so does a caption longer than one message
    /// carries (split like a [`SendText`](Self::SendText) of that length; no caption is
    /// refused for its length). Each message has its own network id and outbox row; all of
    /// them are stored or none is, also across a crash, and they are sent in order through
    /// the conversation's queue, like [`SendText`](Self::SendText) (same ordering). What the
    /// selection will become is [`Query::FilePlan`].
    ///
    /// The selection may be thousands of files. They are copied one after another (memory
    /// does not follow the selection), a send of more than [`ENQUEUE_CHUNK`](crate::ENQUEUE_CHUNK)
    /// messages is stored in several short transactions, and only the conversation it goes
    /// to waits for it: what the user sends in other chats meanwhile goes out at once, what
    /// they type in this chat goes out behind the files. None of the messages starts to be
    /// sent before all of them are stored.
    ///
    /// The request is traceable by its `request` id. While the core copies the files it
    /// reports [`Progress::Import`]; once the messages exist it says so with
    /// [`UiEvent::FilesStored`], which lists them. Exactly one of the two ends every request:
    /// `FilesStored`, or `ActionFailed { action: "SendFiles", subject:
    /// ActionSubject::Files(..) }` with the same id ([`RefusedFiles::request`]). Until then
    /// the files must stay where `paths` say (a composer keeps pasted files that long).
    ///
    /// A file the send cannot take is never dropped silently and never sent short: when any
    /// file is refused (unreadable, not a file, above the network's size limit, a kind
    /// the chat does not take, a location the user did not agree to send) nothing is queued and the failure hands the paths, the
    /// caption and the quote back and names every refused file with its reason
    /// ([`RefusedFiles::refused`]). The UI decides what happens to the rest: it puts all of it
    /// back into the composer with those files marked, or sends the command again without
    /// them. Paths are personal data: `Debug` hides them and they are never logged.
    SendFiles {
        /// The conversation (its account sends).
        conv: ConversationId,
        /// The UI's id of this request, echoed in its events.
        request: RequestId,
        /// The files the user picked (`paths.into()` from a `Vec<PathBuf>`).
        paths: kourier_model::Redacted<Vec<PathBuf>>,
        /// Per file, by position in `paths`: send it as the file it is ("send as file",
        /// "send original") instead of as the picture, GIF or video it holds. Such a file
        /// keeps its bytes untouched: it leaves as an attachment of kind
        /// `AttachmentKind::File` with its real MIME type and its name, which a backend
        /// never re-encodes, and it packs like any other generic file (alone, where the
        /// network's rule says so), without a thumbnail. The chat's limits for generic files
        /// apply to it. Missing entries are `false`, so an empty list sends every file as
        /// what it is; `true` for a file that is no picture or video changes nothing.
        as_file: Vec<bool>,
        /// Per file, by position in `paths`: the user agreed that it leaves with where it was
        /// taken (LOC; the composer's question or the "Always send" setting). A file that
        /// would leave this PC with its location ([`leaves_with_location`], looked at again
        /// when the send reads it) and whose entry is `false` is refused with
        /// [`FileRefusal::Location`], like any other refusal. Missing entries are `false`.
        with_location: Vec<bool>,
        /// Caption with formatting ranges (UTF-8 offsets); clears the draft when sent.
        caption: Option<Text>,
        /// The message this one replies to (as for [`SendText`](Self::SendText)).
        reply_to: Option<MessageId>,
    },
    /// Share a person's contact card in a chat (P5c, architecture §4.2): the CORE builds the card
    /// from `user`'s contact row ([`PersonDetail::contact_card`]: their own profile name and the
    /// number their page shows, nothing else) and queues it in `conv` like a
    /// [`SendText`](Self::SendText) (same ordering; the chat's draft is left alone). Sent only
    /// on the user's click and choice of chat. Refused (`ActionFailed { action: "SendContact",
    /// subject: ActionSubject::Conversation(conv) }`) when `conv` belongs to another account than
    /// `account` (a card never leaves through another account), when the network sends no
    /// cards (`NetworkDescriptor::contact_cards`), when the person is the user (BQ-4), when
    /// their page may show no number or they have no profile name ([`ShareState`]), and when
    /// the chat cannot send.
    SendContact {
        /// Where the card goes.
        conv: ConversationId,
        /// The account of the person's page (it must be `conv`'s).
        account: AccountId,
        /// Whose card it is.
        user: UserId,
    },
    /// Download an attachment the policy left for a click, or retry a failed download (§5.8).
    /// Answered with invalidations (`Downloading`, then `Local` or `Failed`) and
    /// [`Progress::Download`]; a refusal comes as `ActionFailed { action: "DownloadAttachment",
    /// subject: ActionSubject::Attachment(..) }`.
    DownloadAttachment {
        /// The attachment.
        attachment: kourier_model::AttachmentId,
    },
    /// Decrypt a downloaded attachment into the tmpfs media cache for `AnimatedImage`, video or
    /// audio playback, or for "open with" (§8.6); answered with [`UiEvent::MediaOpened`], or
    /// with `ActionFailed { action: "OpenMedia", subject: ActionSubject::Attachment(..) }`.
    /// Each successful open needs one [`CloseMedia`](Self::CloseMedia); the plaintext file goes
    /// with the last. Opens and closes are served in the order they were sent (one queue), so
    /// the answers to several opens of one attachment arrive in request order; an answer names
    /// its attachment either way.
    OpenMedia {
        /// The attachment.
        attachment: kourier_model::AttachmentId,
    },
    /// The view that opened the attachment's plaintext file closed.
    CloseMedia {
        /// The attachment.
        attachment: kourier_model::AttachmentId,
    },
    /// Save a plaintext copy of a downloaded attachment as the file `name` in the folder `dir`
    /// (the user's choice in a file dialog): the core decrypts the blob straight into the
    /// folder (`kourier_blob::save_copy`: an anonymous file that gets its name only once every
    /// chunk authenticated, never an executable bit, a link of that name never followed) on a
    /// worker, never on the UI's thread. The file is the sender's original with its metadata.
    /// Answered with [`UiEvent::AttachmentSaved`], or with `ActionFailed { action:
    /// "SaveAttachment", subject: ActionSubject::Attachment(..) }`:
    /// `Refused(Gone)` for a message that is gone, deleted or hidden, `Refused(Deleted)` for a
    /// tombstone, `Refused(WrongKind)` for a view-once message or the rest of a long text,
    /// `InvalidInput` for a file that is not here, and `CoreError::Save(..)` for the writer's
    /// own answers: [`kourier_blob::SaveError::Exists`] ([`SaveMode::NoReplace`] and the name
    /// is taken), `BadName`, `NoFolder`, `NotWritable`, `NoSpace`, `Source`. The rule is the
    /// one behind [`MessageActions::save_attachments`], looked at again at the command (the
    /// row may have been deleted while the dialog was open). Saves are served in the order
    /// they were sent, on a queue of their own: a long one never delays an open or a send.
    SaveAttachment {
        /// The attachment.
        attachment: kourier_model::AttachmentId,
        /// Where the copy goes.
        target: SaveTarget,
        /// Whether a file of that name is replaced (the dialog asked) or answered with
        /// `Exists` (several files into one folder).
        mode: SaveMode,
    },
    /// Re-queue a failed (or paused) own message with its original network id, a fresh retry
    /// budget and only the recipients that do not have it yet (§4.8 step 5). It also lifts a
    /// challenge pause of the account's sends (the user solved it). Edits and reactions the
    /// user made while nobody had the message follow it, in order. A message whose earlier attempts may
    /// have reached the server (it waited for a retry, or gave up without a definite answer)
    /// is still not `take_back` after this: until the new send is answered, a delete for
    /// everyone of it is sent and an edit is a revision. The message is sent as it is now:
    /// a file that was deleted from it meanwhile (on another own device) is not part of the
    /// retry. A challenge that only a job of the backend met stays on show.
    RetryMessage {
        /// The failed message.
        message: MessageId,
    },
    /// "Cancel Sending" (§4.8 step 5): delete a queued or failed own message that no recipient
    /// has yet, with its outbox row; refused once anyone has it.
    ///
    /// A message whose attempt is in flight is STOPPED, not deleted, and only while
    /// [`MessageActions::cancel_sending`] says so: its first attempt, the backend reports an
    /// upload of it and no recipient has it (the same rule decides the flag and this command).
    /// The core writes the stop down durably and asks the backend; the answer decides. The row
    /// stays, its X greyed out, until the answer is in: `Stopping` means nothing of the message
    /// goes out any more and it is taken back (with its attachments' rows and files, also after
    /// a quit or a crash before the attempt's end is recorded); `TooLate` means it was going out
    /// anyway, the mark is cleared at once, the message ends sent and
    /// [`UiEvent::CancelTooLate`] says so after it. Any other message in flight is refused
    /// (`BeingSent`).
    CancelMessage {
        /// The queued, failed or uploading message.
        message: MessageId,
    },
    /// Edit the text of an own text message, or the caption of an own message with
    /// attachments (§3.5.3): checked against the chat's edit window and edit count (Signal:
    /// 24 h, 10 edits; neither in the chat with oneself, where the other clients take any
    /// edit), stored at once as a new revision, then sent through the outbox with its
    /// own network id. The attachments of a message stay as they are; an empty text removes
    /// its caption (a text message cannot become empty), and a caption is refused where the
    /// chat takes none with one of the files. Every revision keeps its text, so earlier
    /// captions are not lost. A message nobody has (queued and never attempted, or refused by
    /// the network for good, with no edit of it waiting) is edited in place, so nobody ever
    /// gets the earlier text; such an edit is local, and the chat's edit window, count and
    /// whether it has edits at all do not bind it. An edit cannot be split: a text longer
    /// than one message of the chat carries is refused here. Refused or
    /// permanently failed edits come back as `ActionFailed { action: "EditMessage", subject:
    /// ActionSubject::Message(..) }`; a failure rolls the text back. Whether a row can be edited
    /// right now is [`MessageActions::edit`].
    EditMessage {
        /// The own message.
        message: MessageId,
        /// The new text or caption with formatting ranges (UTF-8 offsets).
        text: Text,
    },
    /// Delete a message (§3.5.2). For everyone: own messages within the chat's delete window
    /// (or others' as a group admin where the network allows it); the tombstone is shown at once
    /// and the attachments are deleted from disk; if the network refuses for good it stays a
    /// tombstone for this account only (`ActionFailed { action: "DeleteMessage" }`). For me: a
    /// local tombstone at once, synced to our other devices where the network does that; it also
    /// hides the tombstone a delete for everyone left. A queued message nobody has yet is
    /// cancelled instead (§4.8 step 5), unless the delete is for everyone and the message is
    /// waiting for a retry, or its send gave up when the retry budget ran out (an earlier
    /// attempt got no answer or a temporary failure, so it may have reached the server): then
    /// it becomes a tombstone and the delete is sent. A message
    /// whose transmit is in flight cannot be deleted until the answer is there.
    DeleteMessage {
        /// The message.
        message: MessageId,
        /// For everyone (`false`: for this account only).
        for_everyone: bool,
    },
    /// Add or remove an own reaction (§3.5.1): shown at once, sent through the outbox, rolled
    /// back with `ActionFailed { action: "React" }` if the network refuses it. Where the chat
    /// allows one reaction per user a new emoji replaces the old one; where it allows a few, a
    /// reaction beyond the limit is refused. Adding a reaction that is there already, or
    /// removing one that is not, does nothing. A reaction to an own message whose send failed
    /// for everyone is shown and kept, and sent once the message is retried: nobody is told
    /// about a message they never got.
    React {
        /// The message.
        message: MessageId,
        /// The emoji.
        emoji: String,
        /// Remove it.
        remove: bool,
    },
    /// Change the conversation's disappearing-message timer (a roaming change, confirmed by
    /// events like [`ChangeConversation`](Self::ChangeConversation)). Any timer the chat's
    /// features take is accepted ([`model::ConversationFeatures::takes_timer`]): one of the
    /// values it offers (`ConversationFeatures.disappearing`, a menu) or any value of the
    /// range it states (`ConversationFeatures.disappearing_custom`, a "custom time"), in
    /// whole seconds; `None` turns it off. Anything else is refused with
    /// `Refusal::NotAllowedHere`.
    /// Nothing changes locally until the backend confirms: then `ConversationDetail.timer` has
    /// the new value and the timeline gets a `SystemEvent::TimerChanged` row written by the
    /// backend (`by: None` for our own change), as for a change made by someone else. Messages
    /// queued after that carry the new timer.
    SetConversationTimer {
        /// The conversation.
        conv: ConversationId,
        /// The timer (whole seconds); `None` = off.
        timer: Option<Duration>,
    },
    /// Delete every message of a conversation for this account ("clear chat history"). The
    /// conversation stays in the list, empty. Applied at once: the rows and their files are
    /// gone when the invalidations arrive (`TimelineInvalidated`, `ConversationsChanged`),
    /// and they do not come back with a late copy from the network. The account's other
    /// devices follow where the network syncs it (a durable command: it is sent again after
    /// a restart until the backend has it; nothing is sent for a logged-out account). Nobody
    /// else is told.
    ///
    /// What nobody has yet stays, as for the same delete made on another device: an own
    /// message that is queued, being sent, or failed for every recipient (cancel it with
    /// [`CancelMessage`](Self::CancelMessage) or delete it). A message that reached some
    /// recipients and failed for the others goes with the rest, and so does, for the user,
    /// the tombstone of a message whose delete for everyone is still on its way: its row
    /// stays unseen until that delete is sent, and never shows again. A failure comes as
    /// `ActionFailed { action: "ClearConversation", subject:
    /// ActionSubject::Conversation(..) }`.
    ClearConversation {
        /// The conversation.
        conv: ConversationId,
    },
    /// Delete a conversation for this account: its messages go as with
    /// [`ClearConversation`](Self::ClearConversation), its draft too, and when nothing a
    /// person sent is left the conversation leaves the list (`ConversationsRemoved`;
    /// [`ConversationRow::hidden`]). Its title, members and features are kept, so it is back
    /// with its next message, or when the user writes there. While a message is left (one
    /// nobody has yet, which still waits in the outbox) the conversation stays listed. The
    /// account's other
    /// devices follow like for a clear. This is not leaving a group and not blocking: both
    /// are [`ChangeConversation`](Self::ChangeConversation). A failure comes as
    /// `ActionFailed { action: "DeleteConversation", subject:
    /// ActionSubject::Conversation(..) }`.
    DeleteConversation {
        /// The conversation.
        conv: ConversationId,
    },
    /// Delete one attachment of a message for this account: its row and its files go at
    /// once, the rest of the message stays (`MessagesChanged`); a message left with neither
    /// an attachment nor a text is hidden like a delete for me. The account's other devices
    /// follow where the network syncs it and can name the attachment (a durable command).
    /// Whether a row takes it is [`MessageActions::delete_attachment`]: any message with an
    /// attachment that is not deleted and whose own send is not still on its way (it
    /// waits, is in flight, or reached nobody yet). A message that reached some recipients
    /// and failed for the others takes it; "Retry" then sends it without that file. A
    /// refusal comes as `ActionFailed { action: "DeleteAttachment", subject:
    /// ActionSubject::Attachment(..) }` (`Refusal::BeingSent`, `Refusal::Deleted`,
    /// `Refusal::Gone`).
    DeleteAttachment {
        /// The attachment ([`model::Attachment::id`] of a timeline row).
        attachment: AttachmentId,
    },
    /// Store or clear the composer draft (core-local state).
    SaveDraft {
        /// The conversation.
        conv: ConversationId,
        /// `None` clears it.
        draft: Option<Text>,
    },
    /// The user started or stopped typing (fire-and-forget, gated by the typing setting).
    /// Nothing is sent for a blocked chat, a message request that was not accepted, a chat we
    /// cannot send in, the chat with ourselves or a chat without typing notices.
    SetTyping {
        /// The conversation.
        conv: ConversationId,
        /// Typing or stopped.
        on: bool,
    },
    /// The user accepted `user`'s new safety number ("Confirm New Safety Number", also the
    /// choice of a paused send): the backend is told (`Command::ConfirmIdentity`) and the
    /// conversations whose queue paused on it continue (§4.8 `UntrustedIdentity`). BQ-2b
    /// (Desktop's "Send anyway"): on a person whose key changed after it was verified the
    /// backend sets DEFAULT, writes the quiet "not verified" notice and queues a Verified sync
    /// DEFAULT; on any other person it only approves the new key (no state change, no sync, no
    /// notice). `key` is the key of the safety number on screen ([`QueryData::SafetyNumber`],
    /// the lead's RC2, as `SetVerified`): on a person whose key changed after it was verified
    /// the backend refuses when its stored key is another one (the number changed meanwhile:
    /// nothing is confirmed or written, [`UiEvent::ActionFailed`] with [`ActionSubject::Person`],
    /// and the page shows the new number); on any other person it is ignored. The paused
    /// conversations continue only after the backend's `Ok` (the lead's RC6: a refusal leaves
    /// them paused); an account without a running actor keeps the confirmation for its next
    /// start and lets them continue at once.
    ConfirmIdentity {
        /// The account.
        account: AccountId,
        /// Whose identity.
        user: UserId,
        /// The key the user was shown.
        key: model::IdentityFingerprint,
    },
    /// Mark (`verified`) or clear the verification of `user`'s identity key (P5a: "Mark as
    /// Verified" and "Clear Verification" ONLY; "Confirm New Safety Number" is
    /// [`UiCommand::ConfirmIdentity`], never a clear: a clear always queues a Verified sync,
    /// BQ-2b). `key` is the
    /// key of the safety number on screen ([`QueryData::SafetyNumber`]); the backend refuses
    /// another (the number changed meanwhile: [`UiEvent::ActionFailed`] with
    /// [`ActionSubject::Person`]). Handed to the account in the order of the commands (a Mark
    /// then a Clear reach the network in that order); after the backend's `Ok` the
    /// conversations paused on `user` continue, whatever the state was (critic F2). Not durable:
    /// an account that is not running answers a failure.
    SetVerified {
        /// The account.
        account: AccountId,
        /// Whose identity.
        user: UserId,
        /// Mark (`true`) or clear (`false`).
        verified: bool,
        /// The key the user was shown.
        key: model::IdentityFingerprint,
    },
    /// Turn an account's switch for WRITING roaming settings (settings the network keeps for the
    /// account's other devices: archive, pin, mute, marked unread) on or off. Handed to the
    /// account in the order of the commands; the backend stores the switch, answers
    /// (`Command::SetRoamingWrites`) and states the new value in the account's caps
    /// (`AccountCaps::roaming_writes`), which is the only thing that changes what the UI is
    /// offered. A refusal (a network that cannot write them, an account that is not running) comes
    /// back as `ActionFailed { action: "SetRoamingWrites" }` with no subject.
    SetRoamingWrites {
        /// The account.
        account: AccountId,
        /// On (`true`) or off.
        on: bool,
    },
    /// A person's page is open: ask the network for a fresh profile of `user` (the about
    /// text), at most once a day per person (the backend decides; people pages P1). Not
    /// durable, best effort, like a mark.
    RefreshContact {
        /// The account.
        account: AccountId,
        /// Whose profile.
        user: UserId,
    },
    /// The user solved the challenge an account waits on (Signal: the captcha behind a 428).
    /// The account shows `ConnState::NeedsUserAction(UserAction::SolveChallenge { url })`
    /// ([`UiEvent::ConnState`], [`AccountRow::conn`]) while its outbox is paused: the UI opens
    /// `url` in the browser and passes on what the browser hands back (the solution token, or
    /// the whole redirect link it came in; the backend strips what it does not need). The
    /// backend submits it; when the network accepts it, the pause is lifted, the account shows
    /// its real connection state again and every waiting message is sent, also to the group
    /// members a partly delivered send still owes. A refused answer comes back as
    /// `ActionFailed { action: "SubmitChallenge" }` and the challenge stays. Without an answer
    /// the pause ends by itself when the network said how long it lasts (the server's retry
    /// time): the waiting messages are then tried once, and a challenge that still stands
    /// shows again. The same state shows when a background job of the backend met the
    /// challenge (`BackendNotice::ChallengeRequired`); it is answered the same way.
    SubmitChallenge {
        /// The account.
        account: AccountId,
        /// What the browser handed back (a secret: never logged).
        answer: Redacted<String>,
    },
    /// The user does not want to wait for the history transfer of a new link, or for its
    /// download, any longer ([`UiEvent::HistoryTransfer`] in a state that
    /// [`HistoryTransfer::can_skip`]). The backend ends the transfer as skipped and the
    /// account starts receiving; the answer is the next [`UiEvent::HistoryTransfer`].
    /// Nothing happens when no transfer waits or downloads.
    SkipHistoryTransfer {
        /// The account.
        account: AccountId,
    },
    /// The user has seen how the history transfer of an account ended (the UI showed
    /// [`UiEvent::HistoryTransfer`] in a state that [`HistoryTransfer::is_over`], and the
    /// user closed it or acted on it). The backend forgets the ended transfer: without this
    /// command the same end is reported again at every start of the application and at
    /// every connect. Nothing happens while a transfer is not over, and no event answers.
    DismissHistoryTransfer {
        /// The account.
        account: AccountId,
    },
    /// NetworkManager reported a connectivity change. Send it for every signal: the core
    /// collects the hints of a burst ([`HINT_SETTLE`](crate::HINT_SETTLE)) and tells each
    /// linked backend once.
    NetworkChanged,
    /// logind reported resume from suspend. Collected like
    /// [`NetworkChanged`](Self::NetworkChanged) (a burst with a resume in it reaches the
    /// backend as one `Resumed`); the core's own timers look at the clock again at once.
    Resumed,
    /// Choose what happens to disappearing messages whose time has come by this computer's
    /// clock while no backend has confirmed that clock (a start without a network, or a
    /// clock a backend reported as wrong; §9.6): hide them until the time is confirmed (the
    /// default), keep them until then, or erase them by the local clock. See
    /// [`ExpiryPolicy`] for the three options.
    ///
    /// The choice is one setting for the whole core, stored in `core.db`: it holds across
    /// restarts and at a start before any UI is up. A change takes effect at once, in both
    /// directions, in every account whose clock is unconfirmed right now: from "keep" to
    /// "hide", what is due by the local clock leaves the timeline, the list and the counters
    /// (`TimelineInvalidated`, `ConversationsChanged`); from "hide" to "keep" it shows again
    /// as it was; to "erase" it is gone from every read at once and deleted for good before
    /// the command returns (in the sweeper's batches): a change back right behind it finds
    /// it erased, never only hidden. The command is
    /// applied before anything sent after it is looked at: a [`Query::ExpiryPolicy`] right
    /// behind it answers the new option, and a timeline read right behind it already
    /// follows it. There is no event of its own; a failure to store it comes as
    /// `ActionFailed { account: None, action: "SetExpiryPolicy", subject:
    /// ActionSubject::None }`: the option is then what it was, and nothing was hidden, shown
    /// or erased by the one that was asked for.
    SetExpiryPolicy {
        /// The option.
        policy: ExpiryPolicy,
    },
    /// Show or hide the QUIET notices in the conversation list (user decision, 2026-10-15; a
    /// notice is quiet when [`kourier_model::SystemEvent::is_quiet`] says so: a safety number
    /// that changed, a refreshed session, a changed number, an accepted message request).
    /// Hidden is the default: such a notice is a row of its chat's timeline as any other, but
    /// the list shows the chat's last real message, its time, and puts the chat where that
    /// message puts it. Shown: the notice stands for its chat like any message.
    ///
    /// One setting for the whole core, stored in `core.db`. The command stores it and, in the
    /// same transaction, gives every conversation the sort time its newest listed row says;
    /// it is applied before anything sent after it is looked at (a [`Query::QuietNotices`] or
    /// [`Query::Conversations`] right behind it answers the new state). The conversations
    /// whose sort time changed come as `ConversationsChanged`; a PREVIEW can change without
    /// that (two rows with one sort key, a pinned chat), so a front-end reads the whole list
    /// again after this command. A failure to store it comes as `ActionFailed { account:
    /// None, action: "SetQuietNotices", subject: ActionSubject::None }`: the option is then
    /// what it was and the list is as it was.
    SetQuietNotices {
        /// `true`: quiet notices count for the list; `false`: they do not (the default).
        shown: bool,
    },
    /// The UI needs nothing fast for a while (its window was released after it sat hidden
    /// in the tray, §8.2): the core gives back the memory it only keeps for speed, the
    /// SQLite page caches and prepared statements of its connections (§1.3: up to 8 MiB of
    /// cache per connection, four connections). Everything works as before afterwards; the
    /// first queries read their pages again. Send it before the allocator is trimmed.
    ReleaseMemory,
    /// Run a query; answered with [`UiEvent::QueryResult`].
    Query {
        /// Echoed in the result.
        token: QueryToken,
        /// What to read.
        query: Query,
    },
}

/// What the core tells the UI. Invalidations name rows; the UI re-queries.
#[derive(Clone, Debug)]
pub enum UiEvent {
    /// The account list or an account row changed.
    AccountsChanged,
    /// `AddAccount` created this account (login steps follow).
    AccountAdded {
        /// The new account.
        account: AccountId,
    },
    /// The account is gone (removed, or a fresh link failed or was cancelled).
    AccountRemoved {
        /// The account.
        account: AccountId,
    },
    /// What became of the account's device after the user's "Remove Account…" (`removed`) or
    /// "Unlink…": sent once, after the removal is done (just before `AccountRemoved`). Also for
    /// a removal or unlink that a quit cut off and the next start finished: an outcome sent
    /// before the front-end said `UiReady` is sent again at `UiReady` (same `seq`).
    LogoutOutcome {
        /// Counts the outcomes of this run: a front-end that is told of one twice (live, and
        /// again when it says `UiReady`) shows it once.
        seq: u64,
        /// The account.
        account: AccountId,
        /// The account was removed (else it stays, logged out).
        removed: bool,
        /// The backend's report (no report = not reached).
        report: kourier_model::LogoutReport,
        /// Who the account was (its row may be gone by the time the notice is shown).
        who: AccountLabel,
    },
    /// The supervisor gave up after repeated failures, or an account action failed; the UI
    /// offers "Restart" when `restartable`.
    AccountError {
        /// The account.
        account: AccountId,
        /// Log-safe detail.
        detail: String,
        /// `RestartAccount` may help.
        restartable: bool,
    },
    /// Connection state of an account.
    ConnState {
        /// The account.
        account: AccountId,
        /// The state.
        state: ConnState,
    },
    /// The account's send hold changed ([`AccountHold`]; `None`: no hold now): messages,
    /// reactions, edits and deletes made in Kourier wait in the outbox while it stands. A
    /// state, so a front-end treats it as idempotent; [`AccountRow::hold`] has the current one.
    AccountHold {
        /// The account.
        account: AccountId,
        /// The hold now.
        hold: Option<AccountHold>,
    },
    /// A login step to show (§4.4).
    LoginStep {
        /// The linking account.
        account: AccountId,
        /// The step.
        step: LoginStep,
    },
    /// Conversation rows changed (list rows, counters, titles).
    ConversationsChanged {
        /// The rows.
        ids: Vec<ConversationId>,
    },
    /// Conversations are gone.
    ConversationsRemoved {
        /// The rows.
        ids: Vec<ConversationId>,
    },
    /// Message rows of one conversation changed.
    MessagesChanged {
        /// The conversation.
        conv: ConversationId,
        /// The rows.
        ids: Vec<MessageId>,
    },
    /// Reload the whole timeline of a conversation (merges, identity rewrites).
    TimelineInvalidated {
        /// The conversation.
        conv: ConversationId,
    },
    /// `from` was merged into `into` (§3.3): an open chat of `from` switches to `into`, its
    /// notifications are replaced.
    ConversationMerged {
        /// The merged-away conversation.
        from: ConversationId,
        /// The surviving conversation.
        into: ConversationId,
    },
    /// Contact names, avatars or trust changed.
    ContactsChanged {
        /// The account.
        account: AccountId,
    },
    /// Someone is typing (or stopped). Clear after `ttl` without a refresh.
    Typing {
        /// The account.
        account: AccountId,
        /// The conversation.
        conv: ConversationId,
        /// Who.
        user: UserId,
        /// Typing or stopped.
        on: bool,
        /// Time to live.
        ttl: Duration,
    },
    /// Notify about a new incoming message (policy applied, §8.7).
    Notify(Notification),
    /// One summary when an account leaves `Syncing` (§4.5). It also counts the missed calls
    /// that were not announced alone: a ring that ran out while Kourier was closed, the
    /// machine slept or the account was not connected and caught up, if it is still a missed
    /// call when the account is online (such calls of an account that is online already come
    /// as a summary of their own).
    CatchUpSummary {
        /// The account.
        account: AccountId,
        /// New catch-up messages.
        messages: u32,
        /// In this many conversations.
        conversations: u32,
    },
    /// Where the history transfer of a newly linked account stands (waiting for the phone,
    /// downloading with its bytes, reading, done, skipped, failed with a kind). While the
    /// state is not over ([`HistoryTransfer::is_over`]) the account receives nothing new;
    /// [`UiCommand::SkipHistoryTransfer`] ends a wait or a download.
    ///
    /// While the state is `Reading` the imported chats appear and fill
    /// ([`UiEvent::HistoryProgress`] carries the fraction): the import is final from its
    /// first chat on and is never taken back. A transfer that had begun to fill the chats
    /// and could not be finished ends as `Failed(HistoryTransferFailure::Incomplete)`: the
    /// chats and messages that arrived stay, the account receives again, and a UI offers
    /// to transfer the history once more (which replaces the account on this computer).
    ///
    /// Not stored by the core. The backend says the state at every change, a few times a
    /// second while a download moves, and again, whatever the state, every time the
    /// account's backend starts and every time it is told to connect (the core does that
    /// after the first UI frame). A state that is over is repeated like this until
    /// [`UiCommand::DismissHistoryTransfer`], so a UI that was not there when a transfer
    /// failed still learns of it and of its kind. A UI therefore treats every event as
    /// "this is the state now" and shows it idempotently: the same state can come many
    /// times, an end can come long after the transfer ended (at the next start), and the
    /// bytes of a download can step back after a restart.
    HistoryTransfer {
        /// The account.
        account: AccountId,
        /// The state.
        state: HistoryTransfer,
    },
    /// How far the work of a newly linked account is that the user waits for while its chats
    /// are already usable: the delivery of an imported history (messages delivered and in
    /// all), then the profiles and groups the link still fetches. Passed on from the backend
    /// as it comes; not stored. The numbers are "now" (they can go up, and start over after
    /// a restart); the report with [`AccountProgress::finished`] ends it.
    AccountProgress {
        /// The account.
        account: AccountId,
        /// The numbers.
        progress: AccountProgress,
    },
    /// History import progress (§4.10).
    HistoryProgress {
        /// The account.
        account: AccountId,
        /// The conversation.
        conv: ConversationId,
        /// 0.0 … 1.0, when known.
        progress: Option<f32>,
        /// More batches follow.
        has_more: bool,
    },
    /// The messages of a [`UiCommand::SendFiles`] exist: every file is copied and the whole
    /// send is stored and queued (the invalidations for the rows come with it). It ends the
    /// request: the UI can stop saying "preparing" and may delete the source files it made
    /// for the send (pasted pictures). Sent right after the commit; a core that is killed in
    /// between has the messages and sends them, and there is no UI left waiting for the
    /// event.
    FilesStored {
        /// The conversation.
        conv: ConversationId,
        /// The command's `request`.
        request: RequestId,
        /// The messages the send became, in sending order: the texts of a caption that
        /// went first, then the messages with files (`FilePlan::message_count()` of them).
        messages: Vec<MessageId>,
    },
    /// Upload, download, transmit or import progress.
    Progress(Progress),
    /// [`UiCommand::OpenMedia`]: the plaintext file in the tmpfs media cache (hand it to Qt as a
    /// `file://` URL), valid until the matching [`UiCommand::CloseMedia`].
    MediaOpened {
        /// The attachment.
        attachment: kourier_model::AttachmentId,
        /// The plaintext file (personal data, never logged).
        path: PathBuf,
    },
    /// [`UiCommand::SaveAttachment`]: the copy is in the folder under the name asked for, and
    /// on the disk.
    AttachmentSaved {
        /// The attachment.
        attachment: kourier_model::AttachmentId,
        /// The length of the copy in bytes.
        bytes: u64,
    },
    /// A banner from the backend.
    Notice {
        /// The account.
        account: AccountId,
        /// The notice.
        notice: BackendNotice,
    },
    /// A user action failed (for example a rejected roaming change). Not sent for an edit or
    /// a reaction the network refused after the user deleted its message: there is nothing
    /// left to tell them about.
    ///
    /// When the core itself refused the action for a reason a user can be told, `error` is
    /// `CoreError::Refused` and [`CoreError::refusal`] gives the [`Refusal`] (too old, edited
    /// too often, deleted, being sent, unchanged, not allowed here, …); the sentence in the
    /// error is for the log. `CoreError::Backend` is the network's answer,
    /// `CoreError::InvalidState` an account that is not linked, `CoreError::InvalidInput` a
    /// request that made no sense (no files, an attachment that is not downloaded).
    ActionFailed {
        /// The account.
        account: Option<AccountId>,
        /// Which command (log-safe name).
        action: &'static str,
        /// Why.
        error: Arc<CoreError>,
        /// What the command was about.
        subject: ActionSubject,
    },
    /// The user stopped a message that was uploading ([`UiCommand::CancelMessage`]) but the
    /// backend had gone past the point of no return ("too late"), and the message was sent. Said
    /// once, after the outcome was recorded as sent (or partly sent), so that the message's
    /// delete-for-everyone, which the core refuses while a send is in flight, works at once.
    /// Nothing is said when the attempt fails: the message then shows as failed and its menu
    /// as usual.
    CancelTooLate {
        /// The account.
        account: AccountId,
        /// The conversation of the message.
        conv: ConversationId,
        /// The message.
        message: MessageId,
    },
    /// The sends to `user` began or ended waiting for the user to confirm their changed safety
    /// number (P5a BQ-3: a conversation's queue paused on them, or its last such pause ended:
    /// confirmed, cancelled, given up). The rows come as `MessagesChanged` (the paused head
    /// names the person, [`MessageActions::safety_number`]); this tells an open safety-number
    /// page to ask again ([`QueryData::SafetyNumber`]'s `waits`).
    SafetyWaitChanged {
        /// The account.
        account: AccountId,
        /// Whose safety number.
        user: UserId,
    },
    /// The answer to [`UiCommand::Query`].
    QueryResult {
        /// The query's token.
        token: QueryToken,
        /// The rows, or why not.
        result: std::result::Result<QueryData, Arc<CoreError>>,
    },
}

/// What a failed command was about ([`UiEvent::ActionFailed`]).
#[derive(Clone, Debug, PartialEq)]
pub enum ActionSubject {
    /// Nothing more specific than the account.
    None,
    /// A conversation (`SendText`, `SetConversationTimer`, `ChangeConversation`, `MarkRead`,
    /// `SaveDraft`, `SetTyping`, `ClearConversation`, `DeleteConversation`).
    Conversation(ConversationId),
    /// A message (`EditMessage`, `DeleteMessage`, `React`, `RetryMessage`, `CancelMessage`).
    Message(MessageId),
    /// An attachment (`OpenMedia`, `DownloadAttachment`, `DeleteAttachment`).
    Attachment(AttachmentId),
    /// A refused `SendFiles`: the whole request, nothing of which was queued.
    Files(Arc<RefusedFiles>),
    /// A person (`SetVerified`).
    Person(UserId),
}

impl ActionSubject {
    /// The attachment, when the command was about one.
    pub fn attachment(&self) -> Option<AttachmentId> {
        match self {
            Self::Attachment(a) => Some(*a),
            _ => None,
        }
    }

    /// The message, when the command was about one.
    pub fn message(&self) -> Option<MessageId> {
        match self {
            Self::Message(m) => Some(*m),
            _ => None,
        }
    }
}

/// A [`UiCommand::SendFiles`] the core refused, handed back so the composer can restore its
/// file strip, caption and quote. `Debug` hides the paths and the caption.
#[derive(Clone, Debug, PartialEq)]
pub struct RefusedFiles {
    /// The conversation.
    pub conv: ConversationId,
    /// The command's `request`: which send this ends.
    pub request: RequestId,
    /// The files, as sent (personal data, never logged).
    pub paths: Redacted<Vec<PathBuf>>,
    /// The command's `as_file`, as sent (by position in `paths`).
    pub as_file: Vec<bool>,
    /// The command's `with_location`, as sent (by position in `paths`; missing = `false`).
    pub with_location: Vec<bool>,
    /// The caption, as sent.
    pub caption: Option<Text>,
    /// The quoted message, as sent.
    pub reply_to: Option<MessageId>,
    /// The files that made the core refuse the send, by position in `paths` (ascending), each
    /// with its reason. Every other file of `paths` can be sent: the same command without
    /// these positions goes through. Empty when the refusal was not about a file (the chat
    /// is blocked, the account is logged out, the quote cannot be sent, a storage error).
    pub refused: Vec<RefusedFile>,
}

/// One file a send refuses ([`RefusedFiles::refused`], [`FilesPreview::refused`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RefusedFile {
    /// Its position in the paths of the command or query.
    pub index: usize,
    /// Why.
    pub reason: FileRefusal,
}

/// Why a file cannot be sent. None of these is a limit of Kourier's own making: the first
/// two are about the file, the next two are what the network or its clients take, and
/// `Location` is the user's own choice (LOC). An empty file is no reason: it is sent like any
/// other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileRefusal {
    /// It could not be opened or read (gone, no permission, changed while it was read).
    Unreadable,
    /// Not a regular file (a folder, a pipe, a device).
    NotAFile,
    /// Larger than the network takes.
    TooLarge {
        /// The limit in bytes.
        max: u64,
        /// `true`: the conversation's limit for this kind of file
        /// (`ConversationFeatures::files`); `false`: the account's
        /// (`AccountCaps::max_attachment_bytes`).
        chat: bool,
    },
    /// The conversation takes no files of this kind (`ConversationFeatures::files`).
    KindNotAccepted {
        /// The kind, read from the file's content.
        kind: AttachmentKind,
    },
    /// It holds where it was taken, would leave this PC with it, and the user did not agree
    /// to that ([`UiCommand::SendFiles`] `with_location`). Also when it changed while it was
    /// read and was not agreed to (critic F24).
    Location,
}

/// [`Query::FilePlan`]: a pending selection of files, as the composer holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilePlanQuery {
    /// The conversation the files would be sent to.
    pub conv: ConversationId,
    /// The files, in the user's order (personal data, never logged).
    pub paths: Redacted<Vec<PathBuf>>,
    /// Per file, by position in `paths`: it would be sent as the file it is
    /// ([`UiCommand::SendFiles`] `as_file`). Missing entries are `false`.
    pub as_file: Vec<bool>,
    /// The user wrote a caption. With only this flag the plan counts the caption as one
    /// message at most; give `caption_text` for the exact count.
    pub caption: bool,
    /// The caption itself. With it the plan is exact for a caption of any length: one longer
    /// than one message of the conversation carries (`ConversationFeatures::max_text_bytes`)
    /// goes first as the texts it splits into, and `plan.caption_messages` counts them. When
    /// set it decides alone (a blank text is no caption) and `caption` is not looked at.
    pub caption_text: Option<Text>,
    /// The send would quote a message.
    pub reply: bool,
}

/// The answer to [`Query::FilePlan`]: what [`UiCommand::SendFiles`] would do with the
/// selection right now. The files are only looked at (their first bytes, for kind and
/// format), not copied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilesPreview {
    /// The kind of each file as the core reads it from the content, by position; `None` for a
    /// file that could not be read. It is what the file holds, whatever the query's
    /// `as_file` says: a picture marked "send as file" is still `Image` here (so a composer
    /// knows which files have that choice), and the plan packs it as a generic file.
    pub kinds: Vec<Option<AttachmentKind>>,
    /// The files the send would refuse, by position (ascending). While this is not empty,
    /// `SendFiles` with the whole selection is refused.
    pub refused: Vec<RefusedFile>,
    /// How the files that are not refused would be split into messages, in sending order;
    /// the positions are those of the query's paths. The split follows each file's kind and
    /// format as the network's packing rule states them. The composer's "5 files, 3
    /// messages" is `plan.message_count()`, which counts the texts of a caption that goes
    /// first (`plan.caption_messages`).
    pub plan: model::FilePlan,
    /// By position: the file would leave this PC with where it was taken
    /// ([`leaves_with_location`] for its look, kind, `as_file`, size and the network's
    /// statement). `false` for a refused or unreadable file. The composer asks before it sends
    /// a file marked here (LOC).
    pub location: Vec<bool>,
}

/// Whether a file would leave this PC with where it was taken (LOC). The one decision of the
/// plan and of the send (critic F13): the composer's question and the core's refusal follow
/// it alike.
///
/// - The look found no location (`None`, and `Unreadable`, which does not ask): `false`.
/// - Sent as the file it is (`as_file`): `true`, the bytes go unchanged.
/// - A picture (`content` `Image`) whose format the LOOK read by its magic (never the MIME,
///   which may come from the name: a TIFF named `x.jpg`, critic F10) is one the network
///   states it cleans (`NetworkDescriptor::cleaned_pictures`), within its size bound:
///   `false`.
/// - M1 extends THIS function with its own clause (an ISO-BMFF video M1 cleans at import ->
///   `false`), so that plan time and import time keep one answer; a clean that failed is
///   remembered by the core, which then asks as if `cleaned` were `None` (critic F13).
/// - Otherwise, and when the network states nothing (`cleaned` `None`): `true`.
pub fn leaves_with_location(
    look: &Look,
    content: AttachmentKind,
    as_file: bool,
    size: u64,
    cleaned: Option<&model::CleanedPictures>,
) -> bool {
    let Look::Found(evidence) = look else {
        return false;
    };
    if as_file {
        return true;
    }
    let Some(cleaned) = cleaned else {
        return true;
    };
    !(content == AttachmentKind::Image && cleaned.cleans(look_mime(evidence.format), size))
}

/// The MIME type of a format the look recognised, as a network's statement names it.
fn look_mime(format: LookFormat) -> &'static str {
    match format {
        LookFormat::Jpeg => "image/jpeg",
        LookFormat::Png => "image/png",
        LookFormat::WebP => "image/webp",
        LookFormat::Tiff => "image/tiff",
        LookFormat::Heif => "image/heic",
        LookFormat::Jxl => "image/jxl",
        LookFormat::Bmff => "video/mp4",
    }
}

/// Looks at a file for where it was taken (blocking): from its start, whatever the file's
/// position, at most 2 MiB read and 2 s spent (`kourier_media::look`). For a caller outside
/// the core that packs files the core cannot see into (a folder sent as one .zip, critic F9):
/// it reaches the look through the core, not through a crate edge of its own. The position
/// afterwards is unspecified.
pub fn look_file(f: &mut std::fs::File) -> Look {
    use std::io::Seek;
    let Ok(len) = f.metadata().map(|m| m.len()) else {
        return Look::Unreadable;
    };
    if f.rewind().is_err() {
        return Look::Unreadable;
    }
    crate::attachments::looks::look_noted(f, len)
}

/// The versions of a message ([`Query::Revisions`]), for an edit history.
#[derive(Clone, Debug, PartialEq)]
pub struct MessageRevisions {
    /// The message.
    pub message: MessageId,
    /// Its conversation.
    pub conv: ConversationId,
    /// Every version that is still here, oldest first: the message as first sent, then each
    /// edit. The last one is what the timeline row shows. One entry for a message that was
    /// never edited; none for a deleted message (its texts are gone).
    pub revisions: Vec<Revision>,
}

/// One version of a message. `Debug` hides the text.
#[derive(Clone, Debug, PartialEq)]
pub struct Revision {
    /// 0 for the message as first sent, counting up with each edit (by edit time).
    pub revision: u16,
    /// The text of this version: a text message's text, or the caption of a message with
    /// attachments (`None`: this version had no caption). The attachments are the same in
    /// every version, an edit never changes files.
    pub text: Option<Text>,
    /// When this version was written (ms): the message's send time for the first, the edit's
    /// time for the others, as their author's clock had it.
    pub at: i64,
}

/// A notification decided by the core's policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Notification {
    /// The account.
    pub account: AccountId,
    /// The conversation.
    pub conv: ConversationId,
    /// The message.
    pub message: MessageId,
    /// We are mentioned.
    pub mentions_me: bool,
}

/// Progress events (ephemeral).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Progress {
    /// Upload of one attachment of an own message while its attempt uploads (the backend's
    /// `AttachmentProgress`, whose `index` the core turned into the attachment's row: the UI
    /// knows blocks by attachment, as for downloads). `done` and `total` are plaintext bytes,
    /// `total` final from the first report, `done == total` only once that upload is over.
    Upload {
        /// The message.
        message: MessageId,
        /// The attachment.
        attachment: kourier_model::AttachmentId,
        /// Bytes done.
        done: u64,
        /// Total bytes.
        total: u64,
    },
    /// Download of an attachment.
    Download {
        /// The attachment.
        attachment: kourier_model::AttachmentId,
        /// Bytes done.
        done: u64,
        /// Total bytes.
        total: u64,
    },
    /// Transmit of an own message, while an attempt runs (ephemeral: lost with a restart,
    /// the attempt then starts over). The UI keeps the last one per message until the row's
    /// state changes (`MessagesChanged`).
    Transmit {
        /// The message.
        message: MessageId,
        /// The upload share of its attachments, 0.0 … 1.0 (0.0 without attachments).
        progress: f32,
        /// What the send is doing now; `None` when only the upload share is known.
        phase: Option<kourier_model::SendPhase>,
    },
    /// The core copies the files of a [`UiCommand::SendFiles`] into the blob store. The
    /// first comes before the first file is copied (`files_done` 0), the last when every
    /// file is copied (`files_done == files`), just before the messages are stored
    /// ([`UiEvent::FilesStored`]); in between at most a few per second, with `files_done`
    /// and `bytes_done` never going back. A request that is refused may have reported
    /// progress before.
    Import {
        /// The conversation.
        conv: ConversationId,
        /// The command's `request`.
        request: RequestId,
        /// Files copied so far.
        files_done: u32,
        /// Files of the request.
        files: u32,
        /// Bytes copied so far (of the files as they were measured before the copy).
        bytes_done: u64,
        /// Bytes of all files of the request.
        bytes: u64,
    },
}

/// What to read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Query {
    /// Every account.
    Accounts,
    /// A page of the conversation list.
    Conversations(ConversationListQuery),
    /// A page of a timeline.
    Timeline(TimelineQuery),
    /// One conversation with members and features.
    Conversation(ConversationId),
    /// The files of one attachment (media-handle misses, §8.6; runs on the third read
    /// connection).
    Attachment(kourier_model::AttachmentId),
    /// What the name a saved file is offered under is made of ([`SaveFacts`]): who sent the
    /// attachment's message and when, its place among the message's files, the stored name and
    /// type, and what the first bytes of the plaintext say the file is (`kourier_media::sniff_mime`;
    /// the stored MIME type is the sender's claim). Answered with [`QueryData::SaveFacts`];
    /// `None` for an attachment that does not exist, or whose message is hidden. The decryption of
    /// the first chunk runs on the blocking pool.
    SaveFacts(kourier_model::AttachmentId),
    /// How a pending selection of files would be sent: the kind of each file, the files the
    /// send would refuse and the split into messages. For the composer, before
    /// [`UiCommand::SendFiles`]. The split is [`model::ConversationFeatures::plan`], the
    /// function the send uses, on each file's kind and format as the core reads them from
    /// the content.
    FilePlan(FilePlanQuery),
    /// The versions of a message with their times, for the edit history behind the "edited"
    /// mark.
    Revisions(MessageId),
    /// One person for their page (people pages P1): the contact, the 1:1 chat with them, the
    /// groups both are in. `user` may be an alias; the answer names the canonical id.
    Person {
        /// The account.
        account: AccountId,
        /// The person (an own id gives the user's own page).
        user: UserId,
    },
    /// One group for its page (people pages P2): the description, the rules, the timer and
    /// every member row with the name the core has for it. Answers for a conversation of kind
    /// `Group` only: `None` for a 1:1 chat, a channel, a list or a conversation that is gone.
    Group(ConversationId),
    /// A page of the gallery of one chat, newest first (people pages P4, "Media and Files"):
    /// the attachments of messages that are shown, of the kind asked for.
    ChatMedia(ChatMediaQuery),
    /// The safety number of a person (P5a), from the account's backend (the one query that
    /// asks a backend; Signal computes it from its stored keys). An account that is not
    /// running or not linked answers `SafetyUnavailable::NotRunning`.
    SafetyNumber {
        /// The account.
        account: AccountId,
        /// The person.
        user: UserId,
    },
    /// The option [`UiCommand::SetExpiryPolicy`] chose
    /// ([`ExpiryPolicy::HideUntilConfirmed`] until the user chooses another).
    ExpiryPolicy,
    /// What [`UiCommand::SetQuietNotices`] chose: `true` when quiet notices are shown in the
    /// list (`false` until the user chooses so).
    QuietNotices,
}

/// The answer to a [`Query`].
#[derive(Clone, Debug)]
pub enum QueryData {
    /// [`Query::Accounts`].
    Accounts(Vec<AccountRow>),
    /// [`Query::Conversations`].
    Conversations(ConversationPage),
    /// [`Query::Timeline`].
    Timeline(TimelinePage),
    /// [`Query::Conversation`]; `None` when it does not exist.
    Conversation(Option<Box<ConversationDetail>>),
    /// [`Query::Attachment`]; `None` when it does not exist.
    Attachment(Option<AttachmentFile>),
    /// [`Query::SaveFacts`].
    SaveFacts(Option<SaveFacts>),
    /// [`Query::FilePlan`].
    FilePlan(FilesPreview),
    /// [`Query::Revisions`]; `None` when the message does not exist.
    Revisions(Option<MessageRevisions>),
    /// [`Query::Person`]; `None` when the account does not exist.
    Person(Option<Box<PersonDetail>>),
    /// [`Query::Group`]; `None` when the conversation does not exist or is not a group.
    Group(Option<Box<GroupDetail>>),
    /// [`Query::ChatMedia`].
    ChatMedia(ChatMediaPage),
    /// [`Query::SafetyNumber`]: the number, or why there is none. A failure of the backend
    /// itself (its database) is the query's `Err`.
    SafetyNumber {
        /// The number, or why there is none.
        answer: model::SafetyAnswer,
        /// Sends to this person wait for the user to confirm their new safety number (a
        /// conversation's queue paused on them, P5a BQ-3 / critic F2): the page offers
        /// "Confirm New Safety Number" also for a person who was never verified. Read when
        /// the backend's answer arrives; [`UiEvent::SafetyWaitChanged`] says when it changes.
        waits: bool,
    },
    /// [`Query::ExpiryPolicy`].
    ExpiryPolicy(ExpiryPolicy),
    /// [`Query::QuietNotices`]: `true` when quiet notices are shown in the list.
    QuietNotices(bool),
}

/// Where an attachment's encrypted files are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentFile {
    /// The attachment.
    pub id: kourier_model::AttachmentId,
    /// Its account (the blob paths are relative to that account's blob directory).
    pub account: AccountId,
    /// Its message.
    pub message: MessageId,
    /// MIME type.
    pub mime: Option<String>,
    /// Plaintext size.
    pub size: Option<u64>,
    /// Download state.
    pub state: kourier_model::AttachmentState,
    /// The encrypted file, once downloaded.
    pub file: Option<BlobRef>,
    /// The encrypted thumbnail.
    pub thumb: Option<BlobRef>,
}

/// What the name of a saved file is made of ([`Query::SaveFacts`]). Personal data: `Debug` shows
/// none of it.
#[derive(Clone, PartialEq, Eq)]
pub struct SaveFacts {
    /// The attachment's account.
    pub account: AccountId,
    /// The sender of its message; `None` for a message of our own.
    pub author: Option<UserId>,
    /// When the message was sent (ms since the Unix epoch).
    pub sent_at: i64,
    /// The file's place among ALL the files of its message, from 1 (whether or not the others
    /// are on this PC).
    pub position: u32,
    /// The file name the sender gave, as stored.
    pub file_name: Option<String>,
    /// The MIME type the sender stated.
    pub mime: Option<String>,
    /// What the first bytes of the plaintext say it is; `None` for a file that is not here, a
    /// type the sniffer does not know, or a blob that does not open.
    pub sniffed: Option<String>,
}

impl std::fmt::Debug for SaveFacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SaveFacts")
            .field("account", &self.account)
            .field("position", &self.position)
            .finish_non_exhaustive()
    }
}

/// One account.
#[derive(Clone, PartialEq)]
pub struct AccountRow {
    /// Row id.
    pub id: AccountId,
    /// Stable uuid.
    pub uuid: AccountUuid,
    /// Network.
    pub network: NetworkKind,
    /// Lifecycle state.
    pub state: AccountState,
    /// Own profile name (personal data).
    pub display_name: Option<String>,
    /// Current connection state, with what the user must do when the account needs them:
    /// `NeedsUserAction(SolveChallenge { url })` while the outbox waits for a solved challenge
    /// or a background job of the backend met one ([`UiCommand::SubmitChallenge`]; it also
    /// ends by itself when the network said how long it lasts),
    /// `NeedsUserAction(FixClock { skew_ms })` when the
    /// backend found the system clock off, `LoggedOut { reason }` once the device was unlinked
    /// (also after a restart; [`UiCommand::Relink`] links the same account again and keeps its
    /// conversations and messages), `Offline { retry_at_ms }` and `Connecting` in between.
    /// `Offline` when no actor runs for another reason.
    pub conn: ConnState,
    /// Own device id.
    pub own_device: Option<u32>,
    /// Own profile picture, when the backend supplied one: the avatar of the contact with the
    /// account's first own id (`ContactUpsert` for `own_ids[0]`). A file in the account's blob
    /// directory, like [`ConversationRow::avatar`].
    pub avatar: Option<BlobRef>,
    /// The account's own number or address as the network shows it to people (Signal: the
    /// E.164 phone number), when the backend reported one (`AccountReady.number`). It follows
    /// a number change made on another device; `AccountsChanged` announces it. Personal data.
    pub number: Option<String>,
    /// When this device was linked (ms since the epoch): the day the phone lists it under its
    /// linked devices. Moves to the time of a link that is accepted again (`Relink`).
    pub linked_at: i64,
    /// The account's send hold while one stands (a network's temporary refusal of what the
    /// user sends, with the connection up; [`AccountHold`]). Messages, reactions, edits and
    /// deletes made in Kourier wait in the outbox until it ends; receiving, downloads, read
    /// receipts and the missed-call summary go on.
    pub hold: Option<AccountHold>,
    /// Whether the account's network writes the settings it keeps for the account's other
    /// devices (archive, pin, mute, marked unread: "roaming"; [`kourier_model::AccountCaps::roaming_writes`],
    /// stated by the backend alone): `None` = the network cannot (nothing is offered), `Some(false)`
    /// = it can and this account's switch is off ([`UiCommand::SetRoamingWrites`]), `Some(true)` =
    /// on. Changes with `AccountsChanged`.
    pub roaming_writes: Option<bool>,
    /// How many chats this account may pin at most while it states [`Self::roaming_writes`]
    /// (the limit the network states, [`kourier_model::AccountCaps::max_pinned_chats`]); `None` =
    /// the network states NO limit (or does not state the flag at all): the core refuses no pin
    /// for a count and a front-end disables Pin only at a stated limit. The count is the pinned
    /// chats the core lists (a chat deleted here is not counted); the backend's gate is the real
    /// limit. The core refuses a pin beyond a stated limit ([`Refusal::TooMany`]); a front-end
    /// words the refusal with this number.
    pub pin_limit: Option<u32>,
}

kourier_model::redacting_debug!(AccountRow { id, uuid, network, state, conn, own_device, avatar, linked_at, hold, roaming_writes, pin_limit } redact { display_name, number });

/// Who an account was, read before its row went: what a notice about its removal or unlink
/// names it by (link feedback L4).
#[derive(Clone, PartialEq, Eq)]
pub struct AccountLabel {
    /// Network.
    pub network: NetworkKind,
    /// Own profile name (personal data).
    pub display_name: Option<String>,
    /// The account's number or address as the network shows it (personal data).
    pub number: Option<String>,
    /// Own device id.
    pub own_device: Option<u32>,
    /// When this device was linked (ms since the epoch), as [`AccountRow::linked_at`].
    pub linked_at: i64,
    /// Another account of the profile (same network) has the same identity or looks the same
    /// (same name and number): the notice tells them apart by device and link date.
    pub twin: bool,
}

kourier_model::redacting_debug!(AccountLabel { network, own_device, linked_at, twin } redact { display_name, number });

/// Conversation list filter and page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationListQuery {
    /// Only this account (`None` = all).
    pub account: Option<AccountId>,
    /// Which view of the list: `Some(false)` the inbox (the default), `Some(true)` the archived
    /// conversations instead, `None` both in one list order (the viewmodel reads the whole list
    /// once and splits it by each row's flag, so a chat that moves between the two is in exactly
    /// one of them in a snapshot; A1).
    pub archived: Option<bool>,
    /// Page size.
    pub limit: u32,
    /// Continue after this row.
    pub after: Option<ConversationCursor>,
}

impl Default for ConversationListQuery {
    fn default() -> Self {
        Self {
            account: None,
            archived: Some(false),
            limit: 200,
            after: None,
        }
    }
}

/// Keyset position in the conversation list (pinned first in pin order, then newest activity).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConversationCursor {
    /// Pin position (`None` = not pinned).
    pub pinned: Option<u32>,
    /// `last_activity` of the row.
    pub last_activity: i64,
    /// Row id.
    pub id: ConversationId,
}

/// One page of the conversation list.
#[derive(Clone, Debug)]
pub struct ConversationPage {
    /// The rows, in list order.
    pub rows: Vec<ConversationRow>,
    /// Cursor for the next page, when there is one.
    pub next: Option<ConversationCursor>,
}

/// One conversation list row.
#[derive(Clone, PartialEq)]
pub struct ConversationRow {
    /// Row id.
    pub id: ConversationId,
    /// Account.
    pub account: AccountId,
    /// Kind.
    pub kind: ConvKind,
    /// Note to self.
    pub is_self: bool,
    /// The title: group title, else the contact's nickname, address-book or profile name.
    pub title: Option<String>,
    /// The peer of a direct chat.
    pub peer: Option<UserId>,
    /// Avatar file.
    pub avatar: Option<BlobRef>,
    /// The newest LISTED message: a quiet notice (a safety number change, a refreshed session,
    /// a changed number, an accepted request) only when the user asked to see them
    /// ([`UiCommand::SetQuietNotices`]). Always `None` for a [`Self::protected`] chat.
    pub preview: Option<Preview>,
    /// `sort_key` of the newest LISTED message (0 when empty), the list's order.
    pub last_activity: i64,
    /// Unread messages: the conversation's unread rows, live and imported alike (a message
    /// of a history import that the other device had not read counts), plus the unread
    /// messages a backend reported without rows (`ConversationPatch::unread_hint`).
    /// [`UiCommand::MarkRead`] clears all of it; a read marker of another device lowers it
    /// by the rows it covers.
    pub unread: u32,
    /// Unread mentions (unread rows that mention us, live and imported alike).
    pub mentions: u32,
    /// Marked unread (roaming).
    pub marked_unread: bool,
    /// Muted until (roaming, ms).
    pub muted_until: Option<i64>,
    /// Muted on this computer until (ms).
    pub local_muted_until: Option<i64>,
    /// Archived.
    pub archived: bool,
    /// Pin position.
    pub pinned_order: Option<u32>,
    /// Blocked.
    pub blocked: bool,
    /// Message request pending.
    pub request_pending: bool,
    /// Hash of the conversation's features (UI caching).
    pub features_hash: u64,
    /// A draft exists.
    pub has_draft: bool,
    /// The conversation was deleted (here with [`UiCommand::DeleteConversation`], or on
    /// another own device) and nothing a person sent is left in it: it is in no page of
    /// [`Query::Conversations`] and comes back with its next message, or when the user
    /// writes there. Only [`Query::Conversation`] returns such a row (a chat reached by id:
    /// from a contact, a link, a search).
    pub hidden: bool,
    /// The user can send here: the conversation's features state no
    /// [`kourier_model::CannotSend`] (not a member, admins only, blocked, a request pending,
    /// ...). The share picker (P5c) lists only such chats; the outbox refuses the others.
    pub can_send: bool,
    /// What the chat offers of the settings its network keeps for the account's other devices
    /// (archive, pin, mute, marked unread): the chat's OWN account's statement and whether the
    /// network lists the chat ([`Roaming`]). In a list of several accounts one row can be
    /// [`Roaming::Off`] and the next [`Roaming::On`]. A row is read again when its account's
    /// statement changes (`ConversationsChanged` names the account's chats).
    pub roaming: Roaming,
    /// The user protects this chat on another device
    /// ([`kourier_model::ConversationPatch::protected`]): its messages show nowhere outside the
    /// open chat. The core gives such a row no [`Self::preview`]; a front-end shows a placeholder
    /// in its place, no message text in a notification (the name and a count only), and a
    /// search finds the chat by its title only. Its timeline reads as any other.
    pub protected: bool,
}

/// What a chat offers of the settings its network keeps for the account's other devices
/// ("roaming": archive, pin, mute, marked unread; architecture §8.3). Decided by the core from
/// the chat's own account and the chat's own features, so a list that blends several accounts
/// needs no rule of its own. The core refuses a change that is not [`Roaming::On`]
/// ([`Refusal::NotAllowedHere`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Roaming {
    /// The account's network states no such settings: the entries are not shown. Also the
    /// state of every chat of an account whose stored caps this build cannot read; a click
    /// there then fails (the core cannot read the account's statement) instead of being refused.
    #[default]
    Hidden,
    /// The network can and the account's switch is off: the entries are shown disabled, with
    /// the reason (how to turn the switch on).
    Off,
    /// The switch is on: the entries work.
    On,
    /// The account states the settings (the switch on or off) but the network does not list
    /// THIS chat yet (a message request, a person known only by a number): the entries are not
    /// shown until the network lists it.
    NotListed,
}

kourier_model::redacting_debug!(ConversationRow {
    id, account, kind, is_self, peer, avatar, preview, last_activity, unread, mentions,
    marked_unread, muted_until, local_muted_until, archived, pinned_order, blocked,
    request_pending, features_hash, has_draft, hidden, can_send, roaming, protected
} redact { title });

impl ConversationRow {
    /// The keyset cursor of this row.
    pub fn cursor(&self) -> ConversationCursor {
        ConversationCursor {
            pinned: self.pinned_order,
            last_activity: self.last_activity,
            id: self.id,
        }
    }
}

/// The newest message of a conversation, for the list.
#[derive(Clone, PartialEq)]
pub struct Preview {
    /// The message.
    pub message: MessageId,
    /// Author (`None` = own).
    pub author: Option<UserId>,
    /// Author display name.
    pub author_name: Option<String>,
    /// `Body` kind name (`Text`, `Media`, …).
    pub body_kind: String,
    /// Plain text (personal data): the text, or the caption of a message with attachments.
    pub text: Option<String>,
    /// Sent timestamp.
    pub sent_at: i64,
    /// Deleted for everyone, and by whom when the network said (`None`: not deleted). The list
    /// words it like the timeline's tombstone: a delete that names nobody
    /// ([`Deletion::Unattributed`], KT-2) is not "You deleted this message".
    pub deleted: Option<Deletion>,
    /// Own message state.
    pub out_state: Option<OutgoingState>,
    /// Kind of the first attachment of a `Media` or `Sticker` message (voice note, photo,
    /// video, GIF, audio or file; a sticker is `body_kind == "Sticker"`). A long-text
    /// attachment does not count.
    pub attachment: Option<AttachmentKind>,
    /// How many attachments the message has (long texts not counted).
    pub attachments: u32,
    /// The file name of that first attachment, when it has one (personal data): the list
    /// names a lone document instead of calling it "File".
    pub file_name: Option<String>,
    /// The event of a `System` row (member removed, timer changed, …), so the list can say
    /// what happened.
    pub system: Option<SystemEvent>,
    /// `kind_hint` of an `Unsupported` row (`"view-once"`, `"payment"`, …; log-safe).
    pub kind_hint: Option<String>,
}

kourier_model::redacting_debug!(Preview {
    message, author, body_kind, sent_at, deleted, out_state, attachment, attachments, system,
    kind_hint
} redact { author_name, text, file_name });

/// One conversation with its members and features.
#[derive(Clone, Debug)]
pub struct ConversationDetail {
    /// The list row.
    pub row: ConversationRow,
    /// Members (empty for direct chats).
    pub members: Vec<Member>,
    /// Features (§3.8).
    pub features: ConversationFeatures,
    /// Disappearing timer.
    pub timer: Option<Duration>,
    /// The composer draft (`UiCommand::SaveDraft`).
    pub draft: Option<Text>,
}

/// One person for their page (people pages P1; architecture §4.2). What the core keeps, not what
/// the page shows: whether a number is shown, which name wins and how the groups are ordered is
/// the front end's rule over these facts. `Debug` hides every name, text and number.
#[derive(Clone)]
pub struct PersonDetail {
    /// The account.
    pub account: AccountId,
    /// The canonical id (an alias is resolved).
    pub user: UserId,
    /// One of the account's own ids: this is the user's own page.
    pub is_self: bool,
    /// The contact row; `None` when the core knows the id and nothing else.
    pub contact: Option<Contact>,
    /// The account's own profile name (`is_self` only; the contact row of the own id may lack it).
    pub own_name: Option<String>,
    /// The account's own number as the network shows it (`is_self` only).
    pub own_number: Option<String>,
    /// The 1:1 chat with them, when the core has one.
    pub chat: Option<PersonChat>,
    /// The groups both are full members of (never one we left or are only invited to), in the
    /// order of their rows; archived ones included, deleted ones not.
    pub groups: Vec<CommonGroup>,
    /// The account's network has safety numbers (its descriptor's `safety_numbers`, P5a critic
    /// F5): a page of another person offers "View Safety Number"; what the number is, or why
    /// there is none, [`Query::SafetyNumber`] answers.
    pub safety_numbers: bool,
    /// Sends to this person wait for the user to confirm their changed safety number (the same
    /// fact as [`QueryData::SafetyNumber`]'s `waits`, read from [`crate::runtime::Shared::paused`]
    /// when the page is asked for; P5a U7): a person who was never verified still shows
    /// "Safety number changed" under the row while that holds.
    pub safety_waits: bool,
    /// "Copy Link" (P5b): the link to this person, or why there is none. Filled by the runtime
    /// from the network ([`kourier_backend_api::Network::contact_link`]) with a number only
    /// when the page may show it.
    pub link: LinkState,
    /// "Share Contact…" (P5c): whether a card can be sent, or why not. Filled by the runtime
    /// from the network's `contact_cards` ([`ShareState::of`]).
    pub share: ShareState,
}

kourier_model::redacting_debug!(PersonDetail {
    account, user, is_self, contact, chat, groups, safety_numbers, safety_waits, link, share
} redact { own_name, own_number });

impl PersonDetail {
    /// THE NUMBER RULE of a person's page (P1; P5b's link and P5c's card): the number the page
    /// may show, `+` first. The id itself when it is the number (it names them anyway);
    /// otherwise the contact's phone identifier, and only when the network says the number is
    /// shown (`number_shown == Some(true)`; `None`, never said, counts as not shown). Never the
    /// user's own page's concern: callers handle `is_self` first.
    pub fn shown_number(&self) -> Option<String> {
        if self.user.is_phone_number() {
            let v = self.user.value();
            return Some(if v.starts_with('+') {
                v.to_owned()
            } else {
                format!("+{v}")
            });
        }
        let contact = self.contact.as_ref()?;
        if contact.number_shown != Some(true) {
            return None;
        }
        contact
            .identifiers
            .iter()
            .find(|i| i.kind == kourier_model::IdentifierKind::Phone)
            .map(|i| i.value.trim().to_owned())
            .filter(|v| !v.is_empty())
    }

    /// Their own profile name as the network states it, trimmed; never the address-book name
    /// or the user's nickname for them (his TASTE answer (4), the lead's SC1).
    fn profile_name(&self) -> Option<&str> {
        self.contact
            .as_ref()
            .and_then(|c| c.profile_name.as_deref())
            .map(str::trim)
            .filter(|n| !n.is_empty())
    }

    /// The contact card of this person (P5c, BQ-7, SC1, SC3): their PROFILE name and the ONE
    /// number the page shows ([`Self::shown_number`]), nothing else (no picture, no other
    /// field). `None` whenever [`ShareState::of`] is not `Ready` for a network that sends
    /// cards. QML and the viewmodel never assemble a card: this is the one place.
    pub fn contact_card(&self) -> Option<kourier_model::ContactCard> {
        if ShareState::of(self, true) != ShareState::Ready {
            return None;
        }
        let name = self.profile_name()?;
        let number = self.shown_number()?;
        Some(kourier_model::ContactCard {
            name: Some(name.to_owned()),
            identifiers: vec![kourier_model::Identifier {
                kind: kourier_model::IdentifierKind::Phone,
                value: number,
            }],
            ..Default::default()
        })
    }
}

/// The largest profile name a card carries, in bytes: Kourier's own cap (architecture §13, the
/// lead's SC3; no Signal client states one). A longer name gives no card.
pub const MAX_CARD_NAME_BYTES: usize = 512;

/// The "Share Contact…" of a person's page (P5c, architecture §4.2): whether the card can be
/// sent, or why not. The number sentences are Copy Link's (call 1): a card carries only a
/// number the page may show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShareState {
    /// The network sends no cards, or this is the user's own page (read-only, BQ-4): no
    /// button.
    #[default]
    Hidden,
    /// A card can be sent.
    Ready,
    /// The network states the number is not shown (as [`LinkState::NotShared`]).
    NotShared,
    /// No number the page may use (as [`LinkState::NoNumber`]).
    NoNumber,
    /// They have no profile name (SC1: never the address-book name or a nickname), or one
    /// longer than [`MAX_CARD_NAME_BYTES`].
    NoName,
}

impl ShareState {
    /// The state of `detail`'s page on a network that sends cards (`cards`) or not.
    pub fn of(detail: &PersonDetail, cards: bool) -> Self {
        if !cards || detail.is_self {
            return Self::Hidden;
        }
        // One number rule with the link (SD3): a shown number that is not E.164 (`+`, 7 to 15
        // digits: what the receivers read as a number of their own region otherwise) is no
        // number for the card, as `Network::contact_link` of Signal checks it for the link.
        let Some(number) = detail.shown_number() else {
            let not_shared = !detail.user.is_phone_number()
                && detail.contact.as_ref().and_then(|c| c.number_shown) == Some(false);
            return if not_shared {
                Self::NotShared
            } else {
                Self::NoNumber
            };
        };
        if !kourier_backend_api::is_e164(&number) {
            return Self::NoNumber;
        }
        match detail.profile_name() {
            Some(name) if name.len() <= MAX_CARD_NAME_BYTES => Self::Ready,
            _ => Self::NoName,
        }
    }
}

/// The "Copy Link" of a person's page (P5b, architecture §4.2). The link holds a third
/// party's phone number: it is made only from a number the page may show (`number_shown ==
/// Some(true)`, or the id is the number), `Debug` hides it, and it never reaches a log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LinkState {
    /// The network has no links to people, or this is the user's own page (read-only, BQ-4):
    /// no button.
    #[default]
    Hidden,
    /// The link (Signal: `https://signal.me/#p/<+E164>`).
    Link(Redacted<String>),
    /// The network states the number is not shown (`number_shown == Some(false)`: the person does
    /// not share it, or Signal never fetched the profile and states `false` too, so the page's line
    /// only says "not shown", Z1).
    NotShared,
    /// No number the page may use: never said (`number_shown == None`, a profile not fetched
    /// yet: never "does not share", critic F20), shared but unknown, or not one a link takes.
    NoNumber,
}

impl LinkState {
    /// The state of `detail`'s page on a network with or without links. `link` makes the link
    /// of a number that may be shown (the network's [`contact_link`]); it is called with no
    /// other number.
    ///
    /// [`contact_link`]: kourier_backend_api::Network::contact_link
    pub fn of(
        detail: &PersonDetail,
        links: bool,
        link: impl FnOnce(&str) -> Option<String>,
    ) -> Self {
        if !links || detail.is_self {
            return Self::Hidden;
        }
        // Known by the number alone: it names them anyway (the page shows it).
        if !detail.user.is_phone_number()
            && detail.contact.as_ref().and_then(|c| c.number_shown) == Some(false)
        {
            return Self::NotShared;
        }
        match detail.shown_number().as_deref().and_then(link) {
            Some(l) => Self::Link(Redacted::new(l)),
            None => Self::NoNumber,
        }
    }
}

/// The 1:1 chat of a [`PersonDetail`].
#[derive(Clone, Debug)]
pub struct PersonChat {
    /// The chat.
    pub id: ConversationId,
    /// Disappearing timer.
    pub timer: Option<Duration>,
    /// Features (the timer options the page offers come from here).
    pub features: ConversationFeatures,
    /// Blocked in the chat's roaming state.
    pub blocked: bool,
    /// Deleted from the list (the state of a deleted chat: the row stays, nothing is shown). The
    /// page offers no timer for it: a change would send a message and bring the chat back.
    pub hidden: bool,
}

/// One group for its page (people pages P2; architecture §4.2). What the core keeps, not what the
/// page shows: the order of the members, the counts, the texts and the rule lines are the front
/// end's over these facts. `Debug` hides the name, the description and every member's name.
#[derive(Clone)]
pub struct GroupDetail {
    /// The account the group belongs to.
    pub account: AccountId,
    /// The group's conversation.
    pub id: ConversationId,
    /// Its name (personal data); `None` when it has none.
    pub title: Option<String>,
    /// Its picture.
    pub avatar: Option<BlobRef>,
    /// Its description (personal data), whole as the network stated it.
    pub description: Option<String>,
    /// Disappearing timer.
    pub timer: Option<Duration>,
    /// Features: the rules (`group_info`), why we cannot send, the timer options.
    pub features: ConversationFeatures,
    /// Blocked in the chat's roaming state (`conversations.blocked`; no backend states
    /// `CannotSend::Blocked`, so the page asks this fact for the links of the description).
    pub blocked: bool,
    /// Every member row, in no particular order: full members, admins, people invited and
    /// people who asked to join. Our own row is among them while we are in the list.
    pub members: Vec<GroupMember>,
}

kourier_model::redacting_debug!(GroupDetail {
    account, id, avatar, timer, features, blocked, members
} redact { title, description });

/// Which attachments a gallery page lists ([`Query::ChatMedia`]). Voice notes and stickers are
/// neither: a voice note is a message to listen to, a sticker is a picture the sender chose to
/// say something with, not a file or a photo the chat shares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MediaKind {
    /// Pictures, videos and GIFs: the squares of "Media".
    Visual,
    /// Files and audio files: the rows of "Files" (a picture, GIF or video sent as a file is
    /// a file: its kind is `File`).
    Files,
}

/// Where a gallery page goes on: after this attachment, in the order of the gallery (newest
/// message first, the attachments of one message in their order), or AT a message: a page that
/// starts with that message's first attachment (what the picture viewer asks for, to read the
/// whole album of the picture it shows).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MediaCursor {
    /// The message's `sort_key`.
    pub sort_key: i64,
    /// The message.
    pub message: MessageId,
    /// After the attachment at this place in its message (`Some`), or `None`: before all of the
    /// message's attachments, so the page starts WITH the message's first one.
    pub idx: Option<u32>,
}

/// [`Query::ChatMedia`]: a page of one chat's gallery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatMediaQuery {
    /// The chat.
    pub conv: ConversationId,
    /// Which kind of attachment.
    pub kind: MediaKind,
    /// After this attachment (`None`: from the newest).
    pub before: Option<MediaCursor>,
    /// How many attachments at most (clamped to 1 ..= [`MAX_MEDIA_PAGE`]).
    pub limit: u32,
}

/// The most attachments one gallery page holds.
pub const MAX_MEDIA_PAGE: u32 = 1_000;

/// One attachment of a chat's gallery. `Debug` shows no file name (the attachment redacts it).
#[derive(Clone, PartialEq, Eq)]
pub struct ChatMediaItem {
    /// The message it belongs to.
    pub message: MessageId,
    /// That message's `sort_key` (with `message` and `idx`: the cursor).
    pub sort_key: i64,
    /// The attachment's place in its message.
    pub idx: u32,
    /// The attachment as the timeline shows one: id, kind, type, size, file name, dimensions,
    /// duration, blurhash and download state; keys, files and network pointers are never here.
    pub attachment: kourier_model::Attachment,
    /// "Save As…" accepts it ([`crate::actions::may_save_file`]; the gallery lists only shown
    /// messages, so the message half of the rule holds): the file is on this PC.
    pub save: bool,
}

impl std::fmt::Debug for ChatMediaItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatMediaItem")
            .field("message", &self.message)
            .field("sort_key", &self.sort_key)
            .field("idx", &self.idx)
            .field("attachment", &self.attachment)
            .field("save", &self.save)
            .finish()
    }
}

/// The answer to [`Query::ChatMedia`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatMediaPage {
    /// The chat.
    pub conv: ConversationId,
    /// The kind asked for.
    pub kind: MediaKind,
    /// The attachments, newest message first.
    pub items: Vec<ChatMediaItem>,
    /// More of this kind lie after the last item.
    pub more: bool,
    /// The OTHER kind has at least one attachment in this chat (a section that has no pictures
    /// still says there are files). Only asked for with the first page (`before` is `None`);
    /// `false` on a later one.
    pub other: bool,
}

/// A member of a [`GroupDetail`].
#[derive(Clone)]
pub struct GroupMember {
    /// The member (as stored: the canonical id).
    pub user: UserId,
    /// Role: full member, admin, invited, asked to join.
    pub role: MemberRole,
    /// This is one of the account's own ids.
    pub is_self: bool,
    /// The core's name for them (nickname, address-book name, profile name); `None` without a
    /// contact row or a name.
    pub name: Option<String>,
    /// Their picture.
    pub avatar: Option<BlobRef>,
}

kourier_model::redacting_debug!(GroupMember { user, role, is_self, avatar } redact { name });

/// A group two people share.
#[derive(Clone, PartialEq)]
pub struct CommonGroup {
    /// The group's conversation.
    pub id: ConversationId,
    /// Its title (personal data).
    pub title: Option<String>,
    /// Its picture.
    pub avatar: Option<BlobRef>,
}

kourier_model::redacting_debug!(CommonGroup { id, avatar } redact { title });

/// A timeline page request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimelineQuery {
    /// The conversation.
    pub conv: ConversationId,
    /// Where the page is.
    pub anchor: TimelineAnchor,
    /// Page size.
    pub limit: u32,
}

/// Where a timeline page starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimelineAnchor {
    /// The newest messages.
    Newest,
    /// Older than the cursor.
    Before(TimelineCursor),
    /// Newer than the cursor.
    After(TimelineCursor),
    /// Centered on a message.
    Around(MessageId),
}

/// Keyset position `(sort_key, id)` in a timeline (§4.10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimelineCursor {
    /// `sort_key`.
    pub sort_key: i64,
    /// Row id.
    pub id: MessageId,
}

/// One timeline page, oldest first.
#[derive(Clone, Debug)]
pub struct TimelinePage {
    /// The conversation.
    pub conv: ConversationId,
    /// The rows, ascending by `(sort_key, id)`.
    pub rows: Vec<MessageRow>,
    /// Older rows exist.
    pub has_older: bool,
    /// Newer rows exist.
    pub has_newer: bool,
    /// The core clock (ms) when the page was read: the time [`MessageRow::actions`] were
    /// computed for, and the base for [`MessageActions::until`].
    pub now: i64,
}

/// Who sent a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OriginKind {
    /// Another user.
    Remote,
    /// This device.
    OwnThisDevice,
    /// Another own device.
    OwnOtherDevice,
}

/// One timeline row.
///
/// A call line (`body.call_line()`, `Body::System(SystemEvent::Call(..))`) is one row per
/// call that changes in place as the call does (`MessagesChanged` names it each time): it
/// has no `author` (who rang is in the line), its `origin` is `Remote` for an incoming call
/// and `OwnOtherDevice` for one the user made, `sent_at` is when the call began, and it has a
/// `read_state` exactly while it counts as unread (a missed incoming call, a group call
/// somebody started): `Unread` until the chat is read, and `None` again once the line moves
/// on (answered or declined on another device). A ringing line has none and no notification;
/// its `CallState::Ringing::deadline` is on this computer's clock. In a chat with a
/// disappearing timer the line has an `expiry` that started when it was stored. A line that
/// came with a history import (`history`) has the timer and the read state the import
/// stated instead: a missed call the other device had not seen is `Unread` and counts.
#[derive(Clone, Debug, PartialEq)]
pub struct MessageRow {
    /// Row id.
    pub id: MessageId,
    /// Timeline order.
    pub sort_key: i64,
    /// Author (`None` = own, and for a call line).
    pub author: Option<UserId>,
    /// Author display name. Of a call line (which has no author): the display name of who
    /// rang or started the call (`CallLine::by`), when a contact has one.
    pub author_name: Option<Redacted<String>>,
    /// The author's own profile picture (their contact's avatar), when the backend supplied
    /// one; `None` for own messages and for authors without a picture. A file in the account's
    /// blob directory, like [`ConversationRow::avatar`].
    pub author_avatar: Option<BlobRef>,
    /// Origin.
    pub origin: OriginKind,
    /// Sending device.
    pub device: Option<u32>,
    /// Content (latest revision), with attachment ids and states from the attachment rows.
    pub body: Body,
    /// Quote.
    pub reply: Option<ReplyPreview>,
    /// Sender's timestamp.
    pub sent_at: i64,
    /// Server timestamp.
    pub server_at: Option<i64>,
    /// Local arrival time.
    pub received_at: i64,
    /// Number of edits.
    pub revision: u16,
    /// Latest edit time.
    pub edited_at: Option<i64>,
    /// Tombstone.
    pub deleted: Option<Deletion>,
    /// Own message state.
    pub out_state: Option<OutgoingState>,
    /// Incoming read state (of a call line: only while it counts as unread).
    pub read_state: Option<ReadState>,
    /// Visible reactions (per-user limit of the conversation applied).
    pub reactions: Vec<ReactionGroup>,
    /// Disappearing timer.
    pub expiry: Option<Expiry>,
    /// View-once.
    pub view_once: bool,
    /// Imported history: it was never notified. Its `read_state` means what it means on a
    /// live row (an imported message the other device had not read is `Unread` and counts).
    pub history: bool,
    /// We are mentioned.
    pub mentions_me: bool,
    /// Where the own send of this message stands while it is still on its way: `Some` for an
    /// own message whose send row is in the outbox and not `Failed` (queued, being stored as
    /// part of a large send, in flight, or waiting for its next try); `None` otherwise. Durable:
    /// it is read from the outbox row, so it survives a restart. What happens inside an
    /// attempt (the phase, members done) is not here: it comes as
    /// [`Progress::Transmit`](crate::facade::Progress::Transmit) and is lost with a restart.
    pub outbox: Option<OutboxWait>,
    /// What the user may do with the message right now.
    pub actions: MessageActions,
}

/// The outbox row of an own message that is still on its way ([`MessageRow::outbox`]). The UI
/// counts the time since Send from [`MessageRow::sent_at`] (the request's own timestamp, set
/// once when the message was queued) and the countdown to the next try from
/// `next_attempt_at`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxWait {
    /// An attempt is running now.
    pub in_flight: bool,
    /// Attempts started so far (0: not tried yet).
    pub attempts: u32,
    /// When the next attempt may start (core clock, ms), for a row that waits for a retry
    /// after a failed attempt; `None` while in flight and for a row that is due as soon as
    /// the account can send (not tried yet, or held by a pause, offline, logged out).
    pub next_attempt_at: Option<i64>,
    /// The log-safe kind name of the last attempt's error (`"RateLimited"`, `"Transient"`,
    /// `"Challenge"`, `"UntrustedIdentity"` …: `ErrorKind::name`), `None` before any failed
    /// attempt.
    pub last_error: Option<String>,
}

/// What the user may do with a message at [`TimelinePage::now`]. Each flag is the answer of the
/// very check its command runs (edit window and count, delete window, admin deletes, reaction
/// and reply support, blocked or read-only chats, a logged-out account, tombstones), so the UI
/// shows an action exactly when the command would be accepted and re-implements none of it.
///
/// How the UI learns of a change. What the flags depend on changes in four ways, three of them
/// with an event: the row itself (an edit, a delete, a send that starts or ends) comes as
/// `MessagesChanged`; the conversation (new features, a block, our admin role) as
/// `TimelineInvalidated`; the account being linked or logged out as `AccountsChanged` (the open
/// timeline is then read again by the UI). The fourth is time, and it comes without an event:
/// `until` is the instant (core clock, ms) at which the first of the time-limited flags
/// (`edit`, `delete_for_everyone`) turns false. The UI arms one timer for the smallest `until`
/// of its loaded rows, `until - TimelinePage::now` from when the page arrived, and then
/// re-queries those rows (or clears the two flags itself). A command sent in the gap is
/// refused with `ActionFailed`, never applied late.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct MessageActions {
    /// A new message may quote it (`SendText`/`SendFiles` with `reply_to`).
    pub reply: bool,
    /// [`UiCommand::React`] is accepted (which emoji and how many is the command's check).
    pub react: bool,
    /// [`UiCommand::EditMessage`] is accepted: an own text, or an own message with
    /// attachments whose caption can be changed, inside the chat's edit window and count.
    pub edit: bool,
    /// [`UiCommand::DeleteMessage`] `for_everyone: true` is accepted.
    pub delete_for_everyone: bool,
    /// [`UiCommand::DeleteMessage`] `for_everyone: false` is accepted (false only while the
    /// message's own transmit is in flight).
    pub delete_for_me: bool,
    /// [`UiCommand::DeleteAttachment`] is accepted for the attachments of this row: it has
    /// one (a long text does not count), it is not a tombstone, and its own send is not
    /// still on its way (queued, in flight, or failed before anyone got it). True for a
    /// message that reached some recipients and failed for the others.
    pub delete_attachment: bool,
    /// The row has text to copy (a text, or a caption).
    pub copy: bool,
    /// No recipient has the message yet (queued, or refused by the network): either delete
    /// simply takes it back, so one "Delete" is enough. False for a message that waits for a
    /// retry or whose send gave up on attempts without an answer: it may be with the server,
    /// so a delete for everyone of it is sent.
    pub take_back: bool,
    /// "Cancel Sending" ([`UiCommand::CancelMessage`]) is accepted: `take_back` (nothing was
    /// handed to the network yet, so the message is simply removed), or the message is in
    /// flight and may be STOPPED: its attempt is the first, the backend reports an upload of
    /// it, and no recipient has it ([`crate::actions::stoppable`]). The menu entry and the X
    /// on a file's ring read it; `take_back` keeps its meaning ("Delete" alone is enough).
    /// A delete command still refuses a message in flight.
    pub cancel_sending: bool,
    /// The attachments of this row whose Download asks the account's other device (Signal: the
    /// phone) to upload them again: the very ones a click would ask for. The network lost the
    /// file ("Not available on this device", "No longer available", or an ask that ended
    /// "did not answer"), the account is linked, its network can ask, the message can be named
    /// and is not view-once or deleted, the chat and the author are not known by phone number
    /// alone, and the file (not the rest of a long text) is within the receive limit. An
    /// attachment being asked for already is not in it: a click would only join the request.
    /// The note of each attachment says what is going on (`AttachmentState::Asking`,
    /// `AskFailed`).
    pub ask_attachments: Vec<AttachmentId>,
    /// The attachments of this row that [`UiCommand::SaveAttachment`] accepts: the files that
    /// are on this PC (`AttachmentState::Local`; one that is not here yet has Download, and
    /// nothing is fetched for a save) of a message that is not deleted, not hidden because its
    /// time has come on a clock nobody confirmed, and not view-once (a view-once message is
    /// never saved, in any form). A message that disappears may be saved: the copy is the
    /// user's own act and outlives the message. The rest of a long text is no file. A sticker
    /// is one; a logged-out or paused account's files are the user's own and may be saved. One
    /// function ([`crate::actions::save_attachments`]) answers the flag and the command.
    pub save_attachments: Vec<AttachmentId>,
    /// [`UiCommand::RetryMessage`] is accepted: the message's own send row is in the outbox
    /// and not in flight, the network did not refuse it as a reach-out (WhatsApp's 463,
    /// [`kourier_model::FailReason::ReachOutRefused`]: a second try is one more reach-out) and
    /// the chat's features do not say we cannot send in it (it may have lost the right since
    /// the message failed). The same function ([`crate::actions::retry`]) answers the flag and
    /// the command. Whether "Send Again" is offered is the UI's own question on top of this:
    /// only a message that shows as failed.
    pub retry: bool,
    /// "Show Sender's Page" is offered (people pages P3, the keyboard's and the screen reader's
    /// way to what a click on the name or the picture opens): the message is another person's
    /// and the chat is a group (a 1:1 chat has the one person it is with, the page of its
    /// header). The UI opens `author` through the same path as the click.
    pub show_sender: bool,
    /// One "Show <name>'s Page" per DISTINCT person the message's text mentions, in the order of
    /// their first mention; the user's own mention (any of his ids) is one entry with `own`
    /// ("Show Your Page"). Left out: a message deleted, a mention that does not stand on its own
    /// placeholder (an invalid range), and a mention under a spoiler (the spoiler hides who it
    /// is; the core cannot know whether the user revealed it, so it never states one: a click on
    /// the revealed mention still opens it).
    pub show_people: Vec<ShownPerson>,
    /// "Show <name>'s Safety Number" is offered (P5a BQ-3, U4): this is an own message of a
    /// queue that waits for the user to confirm this person's changed safety number (the
    /// dispatcher's identity pause, `UntrustedIdentity`): the paused head AND every row behind
    /// it that still waits. The UI opens that person's safety-number page, which offers
    /// "Confirm New Safety Number"; the line under these messages names the person. `None` on
    /// every other row.
    pub safety_number: Option<UserId>,
    /// When the first time-limited flag turns false (core clock, ms); `None`: no flag here
    /// depends on time.
    pub until: Option<i64>,
}

/// One person a message's menu offers a page of ([`MessageActions::show_people`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ShownPerson {
    /// Who (the id the message names; for the user's own mention, the one it named).
    pub user: UserId,
    /// The user himself: the entry says "Show Your Page" and opens his own page.
    pub own: bool,
}

impl MessageRow {
    /// The keyset cursor of this row.
    pub fn cursor(&self) -> TimelineCursor {
        TimelineCursor {
            sort_key: self.sort_key,
            id: self.id,
        }
    }
}

/// Whether the message a quote points at is here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum QuoteState {
    /// The original is in this conversation.
    Known,
    /// The original is not here (older than this device's history, expired, or made up).
    #[default]
    Missing,
    /// The original was deleted (for everyone or on this account) after it was quoted.
    Deleted,
}

/// The quoted message of a reply: everything a quote block shows, resolved by the core.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplyPreview {
    /// The row to jump to (`TimelineAnchor::Around`): the original when it is a visible row
    /// of this conversation, also the tombstone a delete for everyone left. `None` when the
    /// original is missing or hidden.
    pub target: Option<MessageId>,
    /// Known, missing or deleted.
    pub state: QuoteState,
    /// The quoted author as the quote states it (`None` with `own`, or when unknown).
    pub author: Option<UserId>,
    /// The quoted author is this account.
    pub own: bool,
    /// The quoted author's display name.
    pub author_name: Option<Redacted<String>>,
    /// The sender's snapshot of the quote, as it arrived.
    pub snapshot: Option<QuoteSnapshot>,
    /// The quoted text to show: the snapshot's, else the original's text or caption as it is
    /// now. `None` when neither has one (a quoted picture without caption) or the original was
    /// deleted and the quote carried no text.
    pub text: Option<Text>,
    /// Kind of the first quoted attachment (snapshot, else the original).
    pub attachment: Option<AttachmentKind>,
    /// Its file name (personal data).
    pub file_name: Option<Redacted<String>>,
    /// The original's first attachment when it is a picture, a GIF, a video or a sticker: a
    /// media handle the image provider shows as a thumbnail (its thumbnail blob or blurhash,
    /// like any attachment of the timeline). `None` for a missing or deleted original.
    pub thumb: Option<AttachmentId>,
}

/// One emoji with its reactors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReactionGroup {
    /// The emoji.
    pub emoji: String,
    /// Who reacted with it (`None` = us).
    pub users: Vec<Option<UserId>>,
}

impl ReactionGroup {
    /// Whether we reacted with this emoji.
    pub fn by_me(&self) -> bool {
        self.users.iter().any(Option::is_none)
    }
}

/// A cloneable handle to the running core.
#[derive(Clone)]
pub struct CoreHandle {
    pub(crate) commands: mpsc::UnboundedSender<Internal>,
    pub(crate) events: broadcast::Sender<UiEvent>,
    pub(crate) media: MediaHandles,
    /// The `MarkRead`s sent and not finished ([`Self::reads_in_flight`]); shared with the core.
    pub(crate) reads: Arc<std::sync::atomic::AtomicUsize>,
}

impl std::fmt::Debug for CoreHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreHandle").finish_non_exhaustive()
    }
}

/// Messages to the core task.
#[derive(Debug)]
pub(crate) enum Internal {
    Ui(UiCommand),
    Query(Query, oneshot::Sender<Result<QueryData>>),
    /// A media handle miss (§8.6).
    Media(
        kourier_model::AttachmentId,
        oneshot::Sender<Result<Option<MediaEntry>>>,
    ),
    Shutdown(oneshot::Sender<()>),
}

impl CoreHandle {
    /// Sends a command without blocking. Fails only when the core has stopped.
    pub fn send(&self, cmd: UiCommand) -> Result<()> {
        use std::sync::atomic::Ordering;
        // Counted before it leaves: the core's task counts it down once its events are sent.
        let read = matches!(cmd, UiCommand::MarkRead { .. });
        if read {
            self.reads.fetch_add(1, Ordering::SeqCst);
        }
        let sent = self.commands.send(Internal::Ui(cmd));
        if sent.is_err() && read {
            self.reads.fetch_sub(1, Ordering::SeqCst);
        }
        sent.map_err(|_| CoreError::Stopped)
    }

    /// How many `UiCommand::MarkRead`s were sent and are not finished: from the send until
    /// the core has emitted what the reading changed (or found nothing to change, or failed).
    /// After that the events are counted by [`Self::events_unseen`], so a screenshot that
    /// waits for both never sees a chat between "read asked for" and "read shown".
    pub fn reads_in_flight(&self) -> usize {
        self.reads.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Subscribes to [`UiEvent`]s (from now on).
    pub fn subscribe(&self) -> broadcast::Receiver<UiEvent> {
        self.events.subscribe()
    }

    /// How many events were sent and not yet read by every subscriber: what is between the
    /// core's `emit` and the front-end's pump (a screenshot that waits for the final picture
    /// counts it with the pump's own queue and the queries in flight).
    pub fn events_unseen(&self) -> usize {
        self.events.len()
    }

    /// The media handle table for the UI's image provider (§8.6).
    pub fn media(&self) -> MediaHandles {
        self.media.clone()
    }

    /// Runs a query and awaits the rows (for the app and tests; the UI uses
    /// [`UiCommand::Query`]).
    pub async fn query(&self, query: Query) -> Result<QueryData> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Internal::Query(query, tx))
            .map_err(|_| CoreError::Stopped)?;
        rx.await.map_err(|_| CoreError::Stopped)?
    }
}

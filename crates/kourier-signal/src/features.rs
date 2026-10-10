//! Conversation features of Signal 1:1 chats (§3.8) and the M1 accepted-chat rule (§6.7):
//! a chat our phone has not accepted cannot be started from Kourier (`cannot_send =
//! NotAccepted`, "Start this chat on your phone"), because Kourier cannot write the Storage
//! Service record that sending would have to flip. Accepted (Desktop's `isTrustedContact`) means
//! profile sharing (our phone's transcripts, message-request accepts, the Storage Service
//! `whitelisted` flag) or a contact in the phone's address book (contact sync names, Storage
//! Service system names).
//!
//! The backend sends a chat's features with the first event that touches it and again whenever
//! they change (the chat accepted, the peer unregistered, a new Kourier version that computes
//! them differently); `peers.features_hash` remembers what the core has.
//!
//! # Limits, and who enforces each
//!
//! Signal Desktop and the phones are the reference for the protocol, not for their limits
//! (CLAUDE.md, architecture §1.1): a limit is stated here only when the server or the other
//! clients enforce it, or when it protects interop, privacy or security. `SD/` is the
//! Signal-Desktop checkout; `Android:` and `iOS:` name files of the public Signal-Android and
//! Signal-iOS repositories (main branch, read 2026-10-07).
//!
//! - **Files per message** ([`file_packing`]). Any number of files of any kind can be sent;
//!   this is only how they pack into messages so that every client shows all of them. The rule
//!   is stated by format (the MIME type read from the file's content), because the clients
//!   decide by the pointer's content type whether they draw an album at all:
//!   - Desktop draws a message's attachments as a grid only when every one is a picture or a
//!     video of a type it draws, and otherwise shows the first attachment alone with no sign
//!     that more was sent (`SD/ts/components/conversation/ImageGrid.dom.tsx`:
//!     `attachments.length === 1 || !areAllAttachmentsVisual(attachments)`;
//!     `SD/ts/util/Attachment.std.ts` `areAllAttachmentsVisual`, `isImageAttachment`,
//!     `isVideoAttachment`). Its types are `SD/ts/util/GoogleChrome.std.ts`
//!     `isImageTypeSupported` (`image/{avif,bmp,gif,jpeg,png,webp}`, icons, APNG) and
//!     `isVideoTypeSupported` (`video/{mp4,ogg,webm}`).
//!   - iOS builds an album only when every attachment is visual media and otherwise shows one
//!     generic attachment (iOS: `Signal/ConversationView/Components/CVComponentState.swift`
//!     `buildMediaAlbumItems`, "Well behaving clients should not send a mix of visual media
//!     (like JPG) and non-visual media (like PDF's)"). Before the download it decides by
//!     content type: `SignalServiceKit/Util/MimeTypeUtil.swift`
//!     `isSupportedVisualMediaMimeType`, which is `image/{jpeg,png,tiff,bmp}`, the animated
//!     `image/{gif,apng,webp}` and `video/{3gpp,3gpp2,mp4,quicktime,x-m4v,mpeg}`.
//!   - Android makes a slide of every `image/*` and `video/*` attachment and gives all of
//!     them to its album view (Android: `util/MediaUtil.java` `getSlideForAttachment`;
//!     `mms/SlideDeck.java` `getThumbnailSlides` filters `Slide::hasImage`, and
//!     `mms/VideoSlide.java` `hasImage` is true; `conversation/ConversationItem.java`
//!     `setMediaAttributes`). It never hides the rest of an album.
//!   - So one message may carry what all three treat as visual by content type
//!     ([`ALBUM_TYPES`]): `image/jpeg`, `image/png`, `image/gif`, `image/webp`, `image/bmp`
//!     and `video/mp4`. **MP4 videos share an album with pictures**: Desktop's grid draws a
//!     `video/mp4` tile with a play overlay, Android's album a `VideoSlide`, iOS's album a
//!     video item, and the phones send such mixed albums themselves. Every other format
//!     travels alone, where each client shows it the way it can (a picture, a video, or a
//!     file to save): **HEIC and HEIF** pictures (not in Desktop's list; Kourier cannot decode
//!     them, so they leave as they are), **AVIF** (not in iOS's list), **QuickTime** and
//!     Matroska (not in Desktop's), **WebM** (not in iOS's). An unknown type travels alone
//!     too: the list names what may share, so a format Kourier learns to sniff later is safe
//!     by default.
//!   - What goes on the wire is the sniffed type, or `image/jpeg` or `image/png` for a still
//!     picture Kourier re-encoded (`send::media`), so a file the rule lets into an album is
//!     always stated as one of the listed types.
//!   - An album has at most [`album_max`] files (31 by default). Receivers cut a
//!     message's attachment list: Android at `RemoteConfig.maxAttachmentCount` (remote key
//!     `android.attachments.maxCount`, default 32, "Maximum number of attachments allowed to
//!     be sent/received") before it stores them (Android: `messages/SignalServiceProtoUtil.kt`
//!     `toPointersWithinLimit`), and there the long-text attachment of a caption over 2 KiB
//!     is one of them; Desktop at the constant `ATTACHMENT_MAX` (32) after it took the long
//!     text out (`SD/ts/textsecure/processDataMessage.preload.ts`; Desktop has no remote key
//!     for it). Kourier reads Android's key from the remote config ([`album_max`]).
//!   - Every other file travels alone: Desktop renders only the first attachment of a message
//!     unless all of them are pictures or videos (`SD/ts/components/conversation/Message.dom.tsx`
//!     `renderAttachment`), Android renders the first audio file, else the first document,
//!     else the album (`setMediaAttributes`), and keeps only the voice note of a message that
//!     has one (`toPointersWithinLimit`).
//!   - A file sent "as file" (`AttachmentKind::File` with a picture's or a video's type) is
//!     packed as a generic file by the core and goes out untouched under its own name and its
//!     true content type (`send::media`): the protocol has no "document" marker, so a
//!     receiver that draws that type shows it as a picture or a video, and saves the original
//!     bytes.
//!   - The caption is the message's body and goes on the first message, which also carries the
//!     quote (`reply_needs_text` is false: a Signal message with files can quote).
//! - **Voice notes take no caption** ([`file_limits`]): no client can edit a voice message
//!   (`SD/ts/util/handleEditMessage.preload.ts`; Android: `messages/EditMessageProcessor.kt`
//!   `validTarget`; iOS: `EditManagerImpl.swift` `checkForValidEdit`), so a text sent inside
//!   one could never be corrected. The core sends the text first as a message of its own.
//! - **File size**: the server's `global.attachments.maxBytes` (100 MiB without a config),
//!   which it applies to the encrypted, padded upload (`GET /v4/attachments/form/upload
//!   ?uploadLength=`; `SD/ts/types/AttachmentSize.std.ts` `isAttachmentTooLargeToSend`; Android:
//!   `RemoteConfig.maxAttachmentSizeBytes`, "ciphertext size"). [`max_plaintext`] is the largest
//!   file whose upload fits, so the core refuses a larger one before it is copied. Desktop's
//!   separate video limit (`global.videoAttachments.transcodeTargetBytes`) is the size its
//!   transcoder aims for, not something the server or a receiver checks: not copied.
//! - **Picture quality** (`send::media::PICTURE_LEVEL`): still pictures are re-encoded without
//!   metadata (privacy) at Signal's level three, the "High" of the official composers (4096 px,
//!   3 MiB; `SD/ts/util/scaleImageToLevel.preload.ts`). Desktop's default of 1600 px and 1 MiB
//!   is its composer's choice, checked by no server and no receiver: not copied.
//! - **File kinds**: none is refused. Desktop's composer refuses to send "dangerous" file
//!   types (`SD/ts/util/isFileDangerous.std.ts`, by file name: `.exe`, `.apk`, `.jar`, `.js`,
//!   …); neither the server nor the phones do. A Desktop receiver shows such a file but will
//!   not save it (`SD/ts/state/ducks/conversations.preload.ts` `saveAttachment`); the phones
//!   save it. Kourier sends it under its own name.
//! - **Text**: at most 64 KiB, of which 2 KiB travel in the message and the whole in a
//!   long-text attachment (`SD/ts/util/longAttachment.std.ts`). Receivers fetch at most 64 KiB
//!   of long text (`SD/ts/jobs/AttachmentDownloadManager.preload.ts` `maxTextAttachmentSize`);
//!   a longer one would show cut at 2 KiB.
//! - **Edits, sending**: for 24 hours, or for the server's delete window when that is shorter
//!   ([`edit_send_secs`]), and at most [`MAX_EDITS`] times. Receivers ignore an edit whose
//!   server time is more than 48 hours after the original's (`SD/ts/util/isTooOldToEditMessage.std.ts`;
//!   iOS: `editWindowMilliseconds`) or, on Android, more than the delete window plus a day
//!   (`util/MessageConstraintsUtil.kt` `RECEIVE_THRESHOLD`); an edit accepted here may wait a
//!   day in the outbox (§4.8), so the send window is a day shorter than the shortest receive
//!   window. The count: every official client stops *sending* after 10 edits (`SD/ts/util/canEditMessage.dom.ts`
//!   `MESSAGE_MAX_EDIT_COUNT`, Android `MAX_EDIT_COUNT`, iOS `maxSendEdits`), but none of them
//!   refuses an eleventh on receive: Desktop and Android do not count
//!   (`handleEditMessage.preload.ts`, `EditMessageProcessor.kt`), iOS drops edits after the
//!   hundredth (`EditManagerImpl.swift` `maxReceiveEdits`). So 10 was only the reference
//!   composers' choice; 100 is what a receiver enforces. Note to Self has neither limit.
//! - **Edits, receiving**: for 48 hours (Desktop, iOS), any number (Desktop, Android).
//! - **Delete for everyone**: the server's `global.normalDeleteMaxAgeInSeconds` (one day
//!   without a config, `SD/ts/util/getDeleteMaxAgeMs.dom.ts`); receivers accept it a day
//!   longer (`MESSAGE_SEND_GRACE_PERIOD` in `SD/ts/util/canDeleteForEveryone.preload.ts`;
//!   Android `RECEIVE_THRESHOLD`). Never in Note to Self (`checkCommon`). A group admin's
//!   delete of other members' messages once the config turns it on
//!   (`desktop.adminDelete.send.prod`).
//! - **Delete for me**: no limit; synced to our other devices (`SyncMessage.deleteForMe`).
//! - **Reactions**: one per user is the protocol's meaning (receivers replace the earlier
//!   one); the reaction must be one emoji because receivers drop anything else
//!   ([`crate::emoji`]).
//! - **Timers**: the wire field is whole seconds (`DataMessage.expireTimer`, a `uint32`,
//!   0 = off), and a chat takes every value from one second to 2,147,483,647 seconds
//!   ([`TIMER_RANGE`], [`MAX_TIMER_SECS`]; about 68 years). The bound is Android's: it reads
//!   the field as a signed 32-bit integer, so a larger value arrives there as a negative
//!   number and shows as "off" (Android: `lib/libsignal-service/src/main/protowire/SignalService.proto`
//!   `optional uint32 expireTimer`, for which Wire generates a Kotlin `Int`;
//!   `messages/SignalServiceProtoUtil.kt` `expireTimerDuration` = `(expireTimer ?: 0).seconds`;
//!   `util/ExpirationUtil.java` `getExpirationDisplayValue(Context, int)`: `<= 0` is "off").
//!   It protects interop, so it is kept; `timer::change` enforces it.
//!   - **Four weeks is not a limit.** It is where the pickers of the official clients end
//!     (`SD/ts/components/DisappearingTimeDialog.dom.tsx`: seconds, minutes, hours, days and
//!     1-4 weeks; Android: `components/settings/app/privacy/expire/CustomExpireTimerSelectorView.kt`
//!     `TimerUnit` `WEEKS(1, 4, …)`; iOS: `Signal/src/ViewControllers/ThreadSettings/DisappearingMessagesCustomTimePickerViewController.swift`
//!     `Unit.allowedValues`, weeks `1...4`). No server sees the field, and none of the three
//!     refuses or shortens a longer timer it receives:
//!   - Desktop stores the number as it came (`SD/ts/textsecure/processDataMessage.preload.ts`
//!     `DurationInSeconds.fromSeconds(message.expireTimer ?? 0)`, no check), words any length
//!     in weeks or in days, hours, minutes and seconds (`SD/ts/util/expirationTimer.std.ts`
//!     `format`), and its sweeper waits in steps of at most 2^31 − 1 ms
//!     (`SD/ts/services/expiringMessagesDeletion.preload.ts`), which four weeks already
//!     exceed.
//!   - Android stores the seconds as an `Int` and the message's lifetime in milliseconds as
//!     a `Long` (`messages/DataMessageProcessor.kt` `handleExpirationUpdate`:
//!     `recipients.setExpireMessages(…, expiresIn.inWholeSeconds.toInt(), …)`, no check;
//!     `service/ExpiringMessageManager.java`), and words any number of weeks and days
//!     (`ExpirationUtil.getExpirationDisplayValue`). Its picker just leaves its default
//!     selection for a value it cannot show (`CustomExpireTimerSelectorView.setTimer`).
//!   - iOS stores a `UInt32` as it came (`SignalServiceKit/Contacts/DisappearingMessagesConfigurationRecord.swift`
//!     `DisappearingMessageToken.token(forProtoExpireTimerSeconds:)`, no check), computes the
//!     expiry in 64 bits (`SignalServiceKit/Messages/Interactions/TSMessage.m`
//!     `updateExpiresAt`), and words years, weeks and days (`SignalServiceKit/Util/String+SSK.swift`
//!     `formatDurationLossless`; `DateUtil.swift` `formatDuration`, its last branch). Its
//!     picker reckons with such a value and opens at four weeks for it; the chat's timer
//!     itself is untouched until the user sets another. iOS does not think well of the
//!     sender, though: the comment there puts a longer timer down to bugs and
//!     "poorly-behaved clients" (`DisappearingMessagesCustomTimePickerViewModel.init`).
//!     That is an opinion in a picker, not a check on receive, so it enforces nothing.
//!
//!   So a timer of eight weeks or of a year is stored, shown and run by all three; only
//!   their own pickers cannot produce it. Under the limits rule that is a choice of the
//!   reference composers, not copied. The backup format has no upper bound either
//!   (`LS/rust/message-backup/src/backup/chat.rs` checks only for timers that are too
//!   short). The offered list is Desktop's menu (`SD/ts/util/expirationTimer.std.ts`
//!   `DEFAULT_DURATIONS_IN_SECONDS`); it is a convenience, not a limit. A timer another
//!   client states is taken as it comes, whatever its size ([`crate::timer::incoming`]). A
//!   group's timer is part of the group state, which Kourier only reads in M1, so groups
//!   offer none.
//!
//! When the config changes a limit, every chat the core has gets its features again
//! ([`refresh_features`]).

use std::collections::{BTreeMap, BTreeSet};

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::Connection;
use kourier_model::{
    AttachmentKind, CannotSend, ConvKey, ConversationFeatures, ConversationPatch, DisappearKind,
    FileAlbum, FileLimits, FilePacking, FormatKind, HistorySource, Support, TimerRange, UserId,
    Window,
};
use libsignal_core::ServiceId;

use crate::attachment::max_blob_len;
use crate::db::Account;
use crate::error::Result;
use crate::peers;
use crate::service_id::{SignalUser, parse_user_id};

/// Longest body Kourier sends inline; longer ones go out with a long-text attachment
/// (`SD/ts/util/longAttachment.std.ts`).
pub(crate) const MAX_INLINE_BODY_BYTES: u32 = 2048;
/// Longest text a chat takes (`MAX_BODY_ATTACHMENT_BYTE_LENGTH`).
const MAX_TEXT_BYTES: u32 = crate::send::media::MAX_TEXT_BYTES as u32;
/// The send limit without a remote config (Desktop's `global.attachments.maxBytes` default).
pub(crate) const DEFAULT_MAX_ATTACHMENT_BYTES: u64 = 100 * 1024 * 1024;

/// Most attachment pointers of one message Android keeps without a remote config
/// (`RemoteConfig.maxAttachmentCount`, default 32; the long-text attachment of a caption over
/// 2 KiB is one of them).
pub(crate) const MAX_POINTERS: u16 = 32;
/// The remote-config name of that number ("Maximum number of attachments allowed to be
/// sent/received"; Android cuts a received message's list there, `toPointersWithinLimit`).
pub(crate) const CONFIG_MAX_POINTERS: &str = "android.attachments.maxCount";
/// Most attachments of one message Desktop keeps after it took the long text out
/// (`SD/ts/textsecure/processDataMessage.preload.ts` `ATTACHMENT_MAX`): a constant, Desktop
/// reads no remote key for it.
pub(crate) const DESKTOP_ATTACHMENT_MAX: u16 = 32;
/// Most files in one message without a remote config: [`MAX_POINTERS`] less the long-text
/// attachment of a caption over 2 KiB.
#[cfg(test)]
pub(crate) const ALBUM_MAX: u16 = MAX_POINTERS - 1;

/// The formats that may share one message: what Desktop, Android and iOS all treat as visual
/// media by the pointer's content type, so each of them draws every item of such an album
/// (the module doc has the three lists and their sources). `video/mp4` is among them;
/// `image/heic`, `image/heif`, `image/avif`, `video/quicktime`, `video/webm` and every type
/// not named here are not, and travel one per message.
pub(crate) const ALBUM_TYPES: [&str; 6] = [
    "image/jpeg",
    "image/png",
    "image/gif",
    "image/webp",
    "image/bmp",
    "video/mp4",
];

/// Most files of one album, given the remote config's `android.attachments.maxCount`
/// (`None`: Android's default): one less than Android keeps, for the long text of a caption,
/// and no more than Desktop keeps. Never below one (a file always goes out, alone if need be).
pub(crate) fn album_max(android_max: Option<u16>) -> u16 {
    android_max
        .unwrap_or(MAX_POINTERS)
        .saturating_sub(1)
        .clamp(1, DESKTOP_ATTACHMENT_MAX)
}

/// The attachment kinds a Signal chat takes, each up to the largest file whose encrypted upload
/// fits `max_bytes` ([`max_plaintext`]). A caption is the message's body; a voice note takes
/// none (no client can edit a voice message, so the core sends the text on its own).
pub(crate) fn file_limits(max_bytes: u64) -> BTreeMap<AttachmentKind, FileLimits> {
    let max_size = max_plaintext(max_bytes);
    [
        AttachmentKind::Image,
        AttachmentKind::Video,
        AttachmentKind::Gif,
        AttachmentKind::Audio,
        AttachmentKind::Voice,
        AttachmentKind::File,
    ]
    .into_iter()
    .map(|kind| {
        (
            kind,
            FileLimits {
                max_size,
                caption: kind != AttachmentKind::Voice,
            },
        )
    })
    .collect()
}

/// How files pack into Signal messages so that Desktop, Android and iOS show every one of them
/// (the module doc has the sources): files of the formats in [`ALBUM_TYPES`] (pictures every
/// client draws, and MP4 videos) share a message, at most `album_max` of them; every other
/// file travels one per message. The rule names formats, not kinds: a HEIC picture is a
/// picture by kind and still travels alone, and a file sent "as file" is planned by the core
/// as a generic file whatever it holds. A message with files can carry the quote, so a reply
/// needs no text.
pub(crate) fn file_packing(album_max: u16) -> FilePacking {
    FilePacking {
        albums: vec![FileAlbum {
            kinds: BTreeSet::new(),
            types: ALBUM_TYPES.iter().map(|t| (*t).to_owned()).collect(),
            except: BTreeSet::new(),
            max: album_max,
        }],
        reply_needs_text: false,
    }
}

/// The largest file (plaintext bytes) whose upload is at most `max_bytes`: the server applies
/// `global.attachments.maxBytes` to the encrypted, padded blob (the upload form's
/// `uploadLength`), which is up to 5 % longer than the file ([`max_blob_len`]).
pub(crate) fn max_plaintext(max_bytes: u64) -> u64 {
    if max_blob_len(max_bytes) <= max_bytes {
        return max_bytes;
    }
    // `max_blob_len` never decreases with the size: the answer is the last size that fits.
    let (mut fits, mut over) = (0u64, max_bytes);
    while over - fits > 1 {
        let middle = fits + (over - fits) / 2;
        if max_blob_len(middle) <= max_bytes {
            fits = middle;
        } else {
            over = middle;
        }
    }
    fits
}

/// The account's attachment send limit from the stored remote config.
#[cfg(test)]
pub(crate) fn send_limit(conn: &Connection) -> Result<u64> {
    Ok(Limits::load(conn)?.max_bytes)
}

/// How old a message may be for an incoming edit (`isTooOldToEditMessage`: 2 days; iOS
/// `editWindowMilliseconds`).
const EDIT_RECEIVE_SECS: u32 = 48 * 3600;
/// How old an own message may be to edit it (`canEditMessage`: `isMoreRecentThan(sent_at, DAY)`;
/// iOS `editSendWindowMilliseconds`); see [`edit_send_secs`].
const EDIT_SEND_SECS: u32 = 24 * 3600;
/// How often a message may be edited: the count after which a receiver drops further edits
/// (iOS `EditManagerImpl.Constants.maxReceiveEdits`; Desktop and Android do not count on
/// receive). The 10 of the official composers (`MESSAGE_MAX_EDIT_COUNT`, `MAX_EDIT_COUNT`,
/// `maxSendEdits`) is checked by none of them on receive, so it is not kept.
pub(crate) const MAX_EDITS: u16 = 100;
/// `global.normalDeleteMaxAgeInSeconds` without a config (`DEFAULT_DELETE_MAX_AGE_MS`: a day).
pub(crate) const DEFAULT_DELETE_SECS: u32 = 24 * 3600;
/// How much longer an incoming delete for everyone is accepted (`MESSAGE_SEND_GRACE_PERIOD`).
const DELETE_RECEIVE_GRACE_SECS: u32 = 24 * 3600;
/// The timers Desktop's menu offers (`DEFAULT_DURATIONS_IN_SECONDS` without "off"): 30 s,
/// 5 min, 1 h, 8 h, 1 day, 1 week, 4 weeks.
pub(crate) const TIMER_CHOICES: [u32; 7] = [30, 300, 3600, 28_800, 86_400, 604_800, 2_419_200];
/// The longest timer a chat takes: the largest number of seconds Android reads as it was
/// sent. The wire field is a `uint32`, and Android's generated class holds it in a signed
/// 32-bit `Int`, where anything above this is negative and shows as "off" (interop; the
/// module doc has the sources). The four weeks at which the official clients' own pickers
/// end are no limit: Desktop, Android and iOS store, show and run a longer timer they
/// receive (the module doc has the sources for that too).
pub(crate) const MAX_TIMER_SECS: u32 = i32::MAX as u32;
/// Every timer a 1:1 chat takes besides the offered list: whole seconds (the wire unit) from
/// one second to [`MAX_TIMER_SECS`].
pub(crate) const TIMER_RANGE: TimerRange = TimerRange {
    min_secs: 1,
    max_secs: MAX_TIMER_SECS,
    step_secs: 1,
};
/// File-name extensions Signal Desktop shows in a chat but refuses to save: the 47
/// alternatives of `DANGEROUS_FILE_TYPES` in `SD/ts/util/isFileDangerous.std.ts`, in its
/// order (compared with the checkout of 2026-10-01; [`DESKTOP_DANGEROUS_PATTERN`] is the
/// pattern as written there, and a test holds the two together).
///
/// The facts behind `ConversationFeatures::hard_to_save`:
/// - Desktop tests a file name against `\.(…)(\.|\s)*?$` without regard to case: the
///   extension at the end of the name, dots and white space after it passed over (so
///   `setup.EXE`, `setup.exe.` and `setup.exe ` count, `setup.exe.txt` does not). That is
///   the rule of `ConversationFeatures::is_hard_to_save`.
/// - On receive it refuses to save: `SD/ts/state/ducks/conversations.preload.ts`
///   `saveAttachment` and `saveAttachments` show the toast `DangerousFileType` and write
///   nothing (one such file stops the save of all the files of a message). The name tested
///   is the attachment's `fileName`, which Kourier always sends. The file is still shown,
///   with a warning mark (`SD/ts/components/FileThumbnail.dom.tsx`).
/// - Its own composer refuses to attach such a file
///   (`SD/ts/state/ducks/composer.preload.ts` `preProcessAttachment`).
/// - Nobody else does: the server never sees a name; Android saves any attachment under
///   its name (Android: `util/SaveAttachmentUtil.kt`, no test by type); no such list was
///   found in iOS's sources.
///
/// So it is not a limit and Kourier sends such a file under its own name. It is stated in
/// the features (`hard_to_save`) so a composer can tell the sender that a Desktop receiver
/// will not be able to save it.
pub(crate) const DESKTOP_WILL_NOT_SAVE: [&str; 47] = [
    "ade",
    "adp",
    "apk",
    "bat",
    "cab",
    "chm",
    "cmd",
    "com",
    "cpl",
    "diagcab",
    "dll",
    "dmg",
    "exe",
    "hta",
    "inf",
    "ins",
    "isp",
    "jar",
    "js",
    "jse",
    "lib",
    "lnk",
    "mde",
    "mht",
    "msc",
    "msi",
    "msp",
    "mst",
    "nsh",
    "pif",
    "ps1",
    "psc1",
    "psm1",
    "psrc",
    "reg",
    "scr",
    "sct",
    "settingcontent-ms",
    "shb",
    "sys",
    "vb",
    "vbe",
    "vbs",
    "vxd",
    "wsc",
    "wsf",
    "wsh",
];

/// The alternatives of Desktop's `DANGEROUS_FILE_TYPES` as `isFileDangerous.std.ts` writes
/// them, for the test that keeps [`DESKTOP_WILL_NOT_SAVE`] equal to them.
#[cfg(test)]
const DESKTOP_DANGEROUS_PATTERN: &str = "ADE|ADP|APK|BAT|CAB|CHM|CMD|COM|CPL|DIAGCAB|DLL|DMG|EXE|\
     HTA|INF|INS|ISP|JAR|JS|JSE|LIB|LNK|MDE|MHT|MSC|MSI|MSP|MST|NSH|PIF|PS1|PSC1|PSM1|PSRC|REG|\
     SCR|SCT|SETTINGCONTENT-MS|SHB|SYS|VB|VBE|VBS|VXD|WSC|WSF|WSH";

/// The remote-config name of the delete-for-everyone window.
pub(crate) const CONFIG_DELETE_MAX_AGE: &str = "global.normalDeleteMaxAgeInSeconds";
/// The remote-config flag that lets group admins delete other members' messages.
pub(crate) const CONFIG_ADMIN_DELETE: &str = "desktop.adminDelete.send.prod";

/// The account-wide limits the features of every chat depend on (from the remote config).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    /// The attachment send limit (`global.attachments.maxBytes`; what an upload may weigh).
    pub(crate) max_bytes: u64,
    /// How old an own message may be for a delete for everyone, in seconds.
    pub(crate) delete_secs: u32,
    /// Group admins may delete other members' messages.
    pub(crate) admin_delete: bool,
    /// Most files of one album ([`album_max`]; `android.attachments.maxCount`).
    pub(crate) album_max: u16,
}

impl Limits {
    /// The limits of the stored remote config (Desktop's defaults without one).
    pub(crate) fn load(conn: &Connection) -> Result<Self> {
        let config = crate::maintenance::load_remote_config(conn)?;
        Ok(Self::of(&config))
    }

    /// The limits `config` states.
    pub(crate) fn of(config: &BTreeMap<String, String>) -> Self {
        Self {
            max_bytes: crate::maintenance::account_caps(config)
                .max_attachment_bytes
                .filter(|max| *max > 0)
                .unwrap_or(DEFAULT_MAX_ATTACHMENT_BYTES),
            delete_secs: delete_max_age(config),
            admin_delete: flag(config, CONFIG_ADMIN_DELETE),
            album_max: album_max(
                config
                    .get(CONFIG_MAX_POINTERS)
                    .and_then(|v| v.trim().parse::<u16>().ok())
                    .filter(|max| *max > 0),
            ),
        }
    }

    /// Desktop's defaults with this attachment limit.
    #[cfg(test)]
    pub(crate) fn with_max_bytes(max_bytes: u64) -> Self {
        Self {
            max_bytes,
            delete_secs: DEFAULT_DELETE_SECS,
            admin_delete: false,
            album_max: ALBUM_MAX,
        }
    }
}

/// `global.normalDeleteMaxAgeInSeconds` (`parseMaxAgeSecsToMs`: a positive integer, else a day).
pub(crate) fn delete_max_age(config: &BTreeMap<String, String>) -> u32 {
    config
        .get(CONFIG_DELETE_MAX_AGE)
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_DELETE_SECS)
}

/// How old an own message may be to edit it: a day, or the delete-for-everyone window when the
/// server set a shorter one. Android accepts an edit until the delete window plus a day after
/// the original (`MessageConstraintsUtil.isValidEditMessageReceive`), Desktop and iOS for two
/// days, and an edit accepted here may wait a day in the outbox.
pub(crate) fn edit_send_secs(delete_secs: u32) -> u32 {
    EDIT_SEND_SECS.min(delete_secs)
}

/// Whether the config has the flag `name` (an entry that is not `false`).
pub(crate) fn flag(config: &BTreeMap<String, String>, name: &str) -> bool {
    config
        .get(name)
        .is_some_and(|v| !v.trim().eq_ignore_ascii_case("false"))
}

/// The features of a 1:1 chat. `accepted`: the phone shares our profile with them (or it is
/// Note to Self); `unregistered`: the server said they left Signal; `note_to_self`: the chat
/// with our own account.
pub(crate) fn direct_features(
    accepted: bool,
    unregistered: bool,
    note_to_self: bool,
    limits: &Limits,
) -> ConversationFeatures {
    // Mentions exist only in groups (Desktop offers none in 1:1 chats).
    let formatting = [
        FormatKind::Bold,
        FormatKind::Italic,
        FormatKind::Strike,
        FormatKind::Spoiler,
        FormatKind::Mono,
    ]
    .into_iter()
    .map(|kind| (kind, Support::Full))
    .collect();
    let cannot_send = if unregistered {
        Some(CannotSend::Unregistered)
    } else if !accepted {
        Some(CannotSend::NotAccepted)
    } else {
        None
    };
    let unlimited = Window {
        max_age_secs: None,
        max_count: None,
    };
    ConversationFeatures {
        max_text_bytes: Some(MAX_TEXT_BYTES),
        formatting,
        files: file_limits(limits.max_bytes),
        file_packing: file_packing(limits.album_max),
        reply: Support::Full,
        edit: Some(if note_to_self {
            unlimited
        } else {
            Window {
                max_age_secs: Some(edit_send_secs(limits.delete_secs)),
                max_count: Some(MAX_EDITS),
            }
        }),
        // None in Note to Self (as Desktop): a delete for me already reaches all our devices.
        delete_for_everyone: (!note_to_self).then_some(Window {
            max_age_secs: Some(limits.delete_secs),
            max_count: None,
        }),
        delete_for_me: true,
        edit_receive: Some(if note_to_self {
            unlimited
        } else {
            Window {
                max_age_secs: Some(EDIT_RECEIVE_SECS),
                max_count: None,
            }
        }),
        delete_receive: Some(Window {
            max_age_secs: Some(limits.delete_secs.saturating_add(DELETE_RECEIVE_GRACE_SECS)),
            max_count: None,
        }),
        admin_delete: false,
        reactions: Support::Full,
        reactions_per_user: Some(1),
        typing: true,
        read_receipts: true,
        delivery_receipts: true,
        disappearing: BTreeMap::from([(DisappearKind::AfterRead, TIMER_CHOICES.to_vec())]),
        disappearing_custom: BTreeMap::from([(DisappearKind::AfterRead, TIMER_RANGE)]),
        hard_to_save: DESKTOP_WILL_NOT_SAVE
            .iter()
            .map(|ext| (*ext).to_owned())
            .collect(),
        history: HistorySource::None,
        group_actions: Default::default(),
        cannot_send,
    }
}

/// The features of a group conversation (§6.8): like a 1:1 chat's, plus mentions and (once the
/// config allows it) admin deletes, and without a timer menu: the timer is part of the group
/// state, which Kourier cannot write in M1. `cannot_send` is `NotMember` when we are not a full
/// member and `AdminsOnly` in an announcement-only group we do not administer. Group sends go
/// out through `send::group` (S11).
pub(crate) fn group_features(
    snapshot: Option<&crate::groups::Snapshot>,
    membership: crate::groups::Membership,
    account: &Account,
    limits: &Limits,
) -> ConversationFeatures {
    let mut features = direct_features(true, false, false, limits);
    features
        .formatting
        .insert(FormatKind::Mention, Support::Full);
    features.admin_delete = limits.admin_delete;
    features.disappearing = BTreeMap::new();
    features.disappearing_custom = BTreeMap::new();
    let admin = snapshot
        .and_then(|s| s.member(account.aci))
        .is_some_and(|m| m.admin);
    features.cannot_send = if membership != crate::groups::Membership::Member
        || snapshot.is_none_or(|s| s.terminated)
    {
        Some(CannotSend::NotMember)
    } else if snapshot.is_some_and(|s| s.announcements_only) && !admin {
        Some(CannotSend::AdminsOnly)
    } else {
        None
    };
    features
}

/// Sends the current features of every group the core has whose features changed since they
/// were sent (a new Kourier version computes them differently: S11 lifted the "group send not
/// available" notice). Runs at the start of a linked account.
pub(crate) fn refresh_group_features(
    conn: &Connection,
    account: &Account,
) -> Result<Vec<BackendEvent>> {
    let rows: Vec<Vec<u8>> = conn
        .prepare("SELECT master_key FROM groups WHERE features_hash IS NOT NULL")?
        .query_map([], |r| r.get(0))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let limits = Limits::load(conn)?;
    let mut events = Vec::new();
    for master in rows {
        let Some(row) = crate::groups::load_by_master(conn, &master)? else {
            continue;
        };
        let features = group_features(row.snapshot.as_ref(), row.membership, account, &limits);
        let hash = hash_value(&features);
        if row.features_hash == Some(hash) {
            continue;
        }
        conn.prepare_cached("UPDATE groups SET features_hash = ?2 WHERE master_key = ?1")?
            .execute((&master, hash))?;
        let mut patch = ConversationPatch::new(row.keys.conv_key());
        patch.features = Some(features);
        events.push(BackendEvent::ConversationUpsert(patch));
    }
    Ok(events)
}

/// The features of every chat the core has whose features changed since they were sent (a new
/// Kourier version, a new attachment limit): groups and 1:1 chats.
pub(crate) fn refresh_features(
    conn: &Connection,
    account: &Account,
    now: i64,
) -> Result<Vec<BackendEvent>> {
    let mut events = refresh_group_features(conn, account)?;
    let peers: Vec<String> = conn
        .prepare("SELECT service_id FROM peers WHERE features_hash IS NOT NULL")?
        .query_map([], |r| r.get(0))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    for sid in peers {
        let Some(user) = ServiceId::parse_from_service_id_string(&sid)
            .and_then(|id| crate::service_id::service_id_user_id(id).ok())
        else {
            continue;
        };
        events.extend(features_event(conn, account, &user, now)?);
    }
    Ok(events)
}

/// `features_hash` as a column value.
pub(crate) fn hash_value(features: &ConversationFeatures) -> i64 {
    i64::from_ne_bytes(features.stable_hash().to_ne_bytes())
}

/// The peer of a 1:1 chat as a service id, and whether it is Note to Self.
fn direct_peer(account: &Account, user: &UserId) -> Option<(ServiceId, bool)> {
    let service_id = match parse_user_id(user).ok()? {
        SignalUser::Aci(aci) => ServiceId::from(aci),
        SignalUser::Pni(pni) => ServiceId::from(pni),
        SignalUser::E164(_) => return None,
    };
    Some((service_id, service_id == ServiceId::from(account.aci)))
}

/// Whether a 1:1 chat with `peer` is accepted: Note to Self always, others with profile sharing.
pub(crate) fn is_accepted(conn: &Connection, account: &Account, peer: ServiceId) -> Result<bool> {
    if peer == ServiceId::from(account.aci) {
        return Ok(true);
    }
    Ok(peers::load(conn, &peer.service_id_string())?.accepted())
}

/// Forgets that the core has the features of the 1:1 chat with `user`, so that the next
/// [`features_event`] states them again. For an event that carried them and was taken back
/// before the core took it (a history delivery row): the record of the zone that built it
/// said the core had them, and the core never did. Nothing for a chat without a service id.
pub(crate) fn forget_features(conn: &Connection, account: &Account, user: &UserId) -> Result<()> {
    if let Some((peer, _)) = direct_peer(account, user) {
        peers::clear_features_hash(conn, &peer.service_id_string())?;
    }
    Ok(())
}

/// The `ConversationUpsert{features}` the chat with `user` needs, if the core does not have its
/// current features yet (recorded in this zone).
pub(crate) fn features_event(
    conn: &Connection,
    account: &Account,
    user: &UserId,
    now: i64,
) -> Result<Option<BackendEvent>> {
    let Some((peer, own)) = direct_peer(account, user) else {
        return Ok(None);
    };
    let sid = peer.service_id_string();
    let row = peers::load(conn, &sid)?;
    let features = direct_features(
        own || row.accepted(),
        !own && row.unregistered,
        own,
        &Limits::load(conn)?,
    );
    let hash = hash_value(&features);
    if row.features_hash == Some(hash) {
        return Ok(None);
    }
    peers::set_features_hash(conn, &sid, hash, now)?;
    let mut patch = ConversationPatch::new(ConvKey::Direct(user.clone()));
    patch.features = Some(features);
    if own {
        patch.is_self = Some(true);
    }
    // A chat the core gets for the first time comes with the timer the backend knows (from the
    // phone's initial contact sync): the first message the user sends there must carry it.
    if row.features_hash.is_none() && row.expire_timer > 0 {
        patch.timer =
            kourier_model::Patch::Set(std::time::Duration::from_secs(u64::from(row.expire_timer)));
        patch.timer_version = Some(row.expire_timer_version.max(1));
    }
    Ok(Some(BackendEvent::ConversationUpsert(patch)))
}

/// Puts the features of every 1:1 chat `events` create or change in front of them, for chats
/// whose features the core does not have yet.
pub(crate) fn with_features(
    conn: &Connection,
    account: &Account,
    events: Vec<BackendEvent>,
    now: i64,
) -> Result<Vec<BackendEvent>> {
    let mut peers_seen = BTreeSet::new();
    for event in &events {
        let key = match event {
            BackendEvent::Message(message) => &message.conv,
            BackendEvent::ConversationUpsert(patch) if patch.features.is_none() => &patch.key,
            _ => continue,
        };
        if let ConvKey::Direct(user) = key {
            peers_seen.insert(user.clone());
        }
    }
    let mut out = Vec::with_capacity(events.len() + peers_seen.len());
    for user in &peers_seen {
        if let Some(event) = features_event(conn, account, user, now)? {
            out.push(event);
        }
    }
    out.extend(events);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use kourier_backend_api::IncomingMessage;
    use kourier_model::{Body, Origin};

    use super::*;
    use crate::service_id::aci_user_id;

    #[test]
    fn unaccepted_chats_cannot_send_and_the_rest_is_honest() {
        let limits = Limits::with_max_bytes(1000);
        let accepted = direct_features(true, false, false, &limits);
        assert_eq!(accepted.cannot_send, None);
        assert_eq!(
            accepted.max_text_bytes,
            Some(64 * 1024),
            "long texts as attachments"
        );
        assert!(accepted.format_support(FormatKind::Bold).is_usable());
        assert!(!accepted.format_support(FormatKind::Mention).is_usable());
        assert_eq!(accepted.files.len(), 6, "every attachment kind");
        assert_eq!(
            accepted.files.get(&AttachmentKind::Image),
            Some(&FileLimits {
                max_size: max_plaintext(1000),
                caption: true
            })
        );
        assert!(!accepted.files[&AttachmentKind::Voice].caption);
        assert_ne!(
            hash_value(&accepted),
            hash_value(&direct_features(
                true,
                false,
                false,
                &Limits::with_max_bytes(2000)
            )),
            "a new limit is a change"
        );
        assert!(accepted.typing && accepted.read_receipts && accepted.delivery_receipts);
        assert_eq!(
            direct_features(false, false, false, &limits).cannot_send,
            Some(CannotSend::NotAccepted)
        );
        assert_eq!(
            direct_features(true, true, false, &limits).cannot_send,
            Some(CannotSend::Unregistered)
        );
        assert_ne!(
            hash_value(&accepted),
            hash_value(&direct_features(false, false, false, &limits))
        );
    }

    /// The S13 values, each from the Desktop constant its doc cites.
    #[test]
    fn modifier_features_follow_desktop() {
        let limits = Limits::with_max_bytes(1000);
        let chat = direct_features(true, false, false, &limits);
        assert_eq!(chat.reply, Support::Full);
        assert_eq!(chat.reactions, Support::Full);
        assert_eq!(chat.reactions_per_user, Some(1));
        let window = |age: Option<u32>, count: Option<u16>| {
            Some(Window {
                max_age_secs: age,
                max_count: count,
            })
        };
        assert_eq!(chat.edit, window(Some(24 * 3600), Some(100)));
        assert_eq!(chat.edit_receive, window(Some(48 * 3600), None));
        assert_eq!(chat.delete_for_everyone, window(Some(24 * 3600), None));
        assert_eq!(chat.delete_receive, window(Some(48 * 3600), None));
        assert!(chat.delete_for_me);
        assert!(!chat.admin_delete);
        assert_eq!(
            chat.disappearing
                .get(&DisappearKind::AfterRead)
                .map(Vec::as_slice),
            Some(&[30, 300, 3600, 28_800, 86_400, 604_800, 2_419_200][..]),
            "Desktop's timer menu"
        );
        // The windows as the core applies them: a day is the edge, and the hundredth edit is
        // the last (iOS drops the next one on receive; no client drops the eleventh).
        let edit = chat.edit.expect("edit window");
        assert!(edit.allows(24 * 3600, 10) && !edit.allows(24 * 3600 + 1, 0));
        assert!(edit.allows(60, 99) && !edit.allows(60, 100));

        // Note to Self: edits without limits, no delete for everyone.
        let own = direct_features(true, false, true, &limits);
        assert_eq!(own.edit, window(None, None));
        assert_eq!(own.edit_receive, window(None, None));
        assert_eq!(own.delete_for_everyone, None);
        assert!(own.delete_for_me);

        // The delete window is the server's; receiving allows a day more.
        let mut config = BTreeMap::new();
        assert_eq!(Limits::of(&config).delete_secs, 24 * 3600);
        config.insert(CONFIG_DELETE_MAX_AGE.to_owned(), "7200".to_owned());
        let short = Limits::of(&config);
        assert_eq!(short.delete_secs, 7200);
        let chat = direct_features(true, false, false, &short);
        assert_eq!(chat.delete_for_everyone, window(Some(7200), None));
        assert_eq!(chat.delete_receive, window(Some(7200 + 24 * 3600), None));
        // Android accepts an edit for the delete window plus a day: with a shorter delete
        // window the edit window follows it; a longer one does not stretch Desktop's two days.
        assert_eq!(chat.edit, window(Some(7200), Some(100)));
        config.insert(CONFIG_DELETE_MAX_AGE.to_owned(), "172800".to_owned());
        let long = direct_features(true, false, false, &Limits::of(&config));
        assert_eq!(long.edit, window(Some(24 * 3600), Some(100)));
        for bad in ["0", "-5", "soon"] {
            config.insert(CONFIG_DELETE_MAX_AGE.to_owned(), bad.to_owned());
            assert_eq!(Limits::of(&config).delete_secs, 24 * 3600, "{bad}");
        }

        // Groups: no timer menu (group writes), admin deletes once the config has the flag.
        let fixture = crate::receive::tests::fixture();
        let group = |limits: &Limits| {
            group_features(
                None,
                crate::groups::Membership::Member,
                &fixture.account,
                limits,
            )
        };
        assert!(group(&limits).disappearing.is_empty());
        assert!(!group(&limits).admin_delete);
        assert_eq!(group(&limits).reply, Support::Full);
        assert_eq!(group(&limits).edit, window(Some(24 * 3600), Some(100)));
        config.insert(CONFIG_ADMIN_DELETE.to_owned(), "true".to_owned());
        assert!(group(&Limits::of(&config)).admin_delete);
        config.insert(CONFIG_ADMIN_DELETE.to_owned(), "false".to_owned());
        assert!(!group(&Limits::of(&config)).admin_delete);
    }

    #[test]
    fn features_go_out_once_per_change() {
        let fixture = crate::receive::tests::fixture();
        let account = &fixture.account;
        let alice = libsignal_core::Aci::from(uuid::Uuid::from_u128(0xa11ce));
        let user = aci_user_id(alice).unwrap();
        let message = || {
            BackendEvent::Message(IncomingMessage::new(
                ConvKey::Direct(user.clone()),
                "1".to_owned(),
                Origin::Remote {
                    author: user.clone(),
                    device: Some(1),
                },
                Body::text("hi"),
                1,
                1,
            ))
        };
        fixture
            .db
            .zone(|tx| -> Result<()> {
                let events = with_features(tx, account, vec![message()], 5)?;
                assert_eq!(events.len(), 2);
                let BackendEvent::ConversationUpsert(patch) = &events[0] else {
                    panic!("features first: {events:?}");
                };
                assert_eq!(
                    patch.features.as_ref().and_then(|f| f.cannot_send.clone()),
                    Some(CannotSend::NotAccepted)
                );
                assert_eq!(with_features(tx, account, vec![message()], 6)?.len(), 1);
                // The phone accepted the chat: new features once.
                peers::set_profile_sharing(
                    tx,
                    &ServiceId::from(alice).service_id_string(),
                    true,
                    7,
                )?;
                let event = features_event(tx, account, &user, 8)?.expect("changed");
                let BackendEvent::ConversationUpsert(patch) = event else {
                    panic!("{event:?}");
                };
                assert_eq!(patch.features.and_then(|f| f.cannot_send), None);
                assert!(features_event(tx, account, &user, 9)?.is_none());
                // A chat whose timer the phone's contact sync stated: its first features carry
                // the timer and its version; later ones do not repeat it.
                let bob = libsignal_core::Aci::from(uuid::Uuid::from_u128(0xb0b));
                let bob_user = aci_user_id(bob).unwrap();
                let bob_sid = ServiceId::from(bob).service_id_string();
                peers::reset_expire_timer(tx, &bob_sid, 3600, 4, 9)?;
                let BackendEvent::ConversationUpsert(patch) =
                    features_event(tx, account, &bob_user, 9)?.expect("first features")
                else {
                    panic!("upsert");
                };
                assert_eq!(
                    patch.timer,
                    kourier_model::Patch::Set(std::time::Duration::from_secs(3600))
                );
                assert_eq!(patch.timer_version, Some(4));
                peers::set_profile_sharing(tx, &bob_sid, true, 9)?;
                let BackendEvent::ConversationUpsert(patch) =
                    features_event(tx, account, &bob_user, 9)?.expect("changed features")
                else {
                    panic!("upsert");
                };
                assert_eq!(patch.timer, kourier_model::Patch::Unchanged);
                assert_eq!(patch.timer_version, None);
                // Note to Self is always accepted and marked as such.
                let me = aci_user_id(account.aci).unwrap();
                let BackendEvent::ConversationUpsert(patch) =
                    features_event(tx, account, &me, 10)?.expect("note to self")
                else {
                    panic!("upsert");
                };
                assert_eq!(patch.is_self, Some(true));
                assert_eq!(patch.features.and_then(|f| f.cannot_send), None);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn group_features_are_sent_again_when_this_version_computes_them_differently() {
        let fixture = crate::receive::tests::fixture();
        let account = fixture.account.clone();
        let keys = crate::groups::GroupKeys::from_master([4; 32]);
        let snapshot = crate::groups::Snapshot {
            revision: 3,
            members: vec![crate::groups::SnapMember {
                aci: account.aci.into(),
                admin: false,
                joined_at: 0,
            }],
            ..crate::groups::Snapshot::default()
        };
        fixture
            .db
            .zone(|tx| -> Result<()> {
                // The core has the group with the features an older version sent.
                tx.execute(
                    "INSERT INTO groups(master_key, group_id, updated_at, revision, state,
                       membership, features_hash) VALUES (?1, ?2, 0, 3, ?3, 'member', 42)",
                    (&keys.master[..], &keys.id[..], snapshot.to_json().unwrap()),
                )?;
                // A group the core does not have yet gets its features with its first upsert.
                let other = crate::groups::GroupKeys::from_master([5; 32]);
                crate::groups::ensure(tx, &other, 0)?;
                let events = refresh_group_features(tx, &account)?;
                assert_eq!(events.len(), 1, "{events:?}");
                let BackendEvent::ConversationUpsert(patch) = &events[0] else {
                    panic!("{events:?}");
                };
                assert_eq!(patch.key, keys.conv_key());
                assert_eq!(
                    patch.features.as_ref().map(|f| f.cannot_send.clone()),
                    Some(None),
                    "members write to the group"
                );
                assert!(
                    refresh_group_features(tx, &account)?.is_empty(),
                    "once per change"
                );
                Ok(())
            })
            .unwrap();
    }

    /// The attachment limit comes from the remote config, and a new limit reaches every chat
    /// the core has.
    #[test]
    fn file_limits_follow_the_remote_config() {
        let fixture = crate::receive::tests::fixture();
        let account = &fixture.account;
        let bob = libsignal_core::Aci::from(uuid::Uuid::from_u128(0xb0b));
        let user = aci_user_id(bob).unwrap();
        let limit = |event: Option<BackendEvent>| match event {
            Some(BackendEvent::ConversationUpsert(patch)) => patch
                .features
                .and_then(|f| f.files.get(&AttachmentKind::File).map(|l| l.max_size)),
            other => panic!("{other:?}"),
        };
        fixture
            .db
            .zone(|tx| -> Result<()> {
                assert_eq!(send_limit(tx)?, DEFAULT_MAX_ATTACHMENT_BYTES);
                assert_eq!(
                    limit(features_event(tx, account, &user, 1)?),
                    Some(max_plaintext(100 * 1024 * 1024))
                );
                tx.execute(
                    "INSERT INTO remote_config(name, value)
                     VALUES ('global.attachments.maxBytes', '5000')",
                    [],
                )?;
                assert_eq!(send_limit(tx)?, 5000);
                let events = refresh_features(tx, account, 2)?;
                assert_eq!(events.len(), 1, "the one chat the core has: {events:?}");
                assert_eq!(limit(events.into_iter().next()), Some(max_plaintext(5000)));
                assert!(refresh_features(tx, account, 3)?.is_empty(), "once");
                Ok(())
            })
            .unwrap();
    }

    /// The packing rule: what one Signal message can carry so that every client shows all of
    /// it. Mixed files of one send become an album per run of pictures and MP4 videos, and one
    /// message per other file, in the user's order, with the caption and the quote on the
    /// first. The rule goes by format: a HEIC, HEIF or AVIF picture and a QuickTime or WebM
    /// video are pictures and videos by kind and still travel alone, because Desktop or iOS
    /// would show only the first item of an album that held one.
    #[test]
    fn files_pack_into_what_every_signal_client_shows() {
        use AttachmentKind::{Audio, File, Gif, Image, Video, Voice};
        use kourier_model::PlanFile;
        let chat = direct_features(true, false, false, &Limits::with_max_bytes(1 << 20));
        let rule = &chat.file_packing;
        assert_eq!(rule.albums.len(), 1);
        let album = &rule.albums[0];
        assert!(album.kinds.is_empty(), "formats decide, not kinds");
        assert_eq!(
            album.types.iter().map(String::as_str).collect::<Vec<_>>(),
            [
                "image/bmp",
                "image/gif",
                "image/jpeg",
                "image/png",
                "image/webp",
                "video/mp4"
            ],
            "what Desktop's grid, Android's album and iOS's album all draw"
        );
        assert_eq!(album.max, 31, "32 pointers less the long text");
        assert!(!rule.reply_needs_text, "a message with files can quote");
        let jpeg = PlanFile::new(Image, "image/jpeg");
        let png = PlanFile::new(Image, "image/png");
        let gif = PlanFile::new(Gif, "image/gif");
        let mp4 = PlanFile::new(Video, "video/mp4");
        let pdf = PlanFile::new(File, "application/pdf");
        let mp3 = PlanFile::new(Audio, "audio/mpeg");
        let plan = |files: &[PlanFile<'_>], caption: bool, reply: bool| {
            let text = kourier_model::Text::plain("caption");
            chat.plan(files, caption.then_some(&text), reply)
        };
        // Two pictures and three documents: the album, then each document.
        let sent = plan(&[jpeg, png, pdf, pdf, pdf], true, true);
        assert_eq!(sent.messages, [vec![0, 1], vec![2], vec![3], vec![4]]);
        assert!(!sent.lead_text && !sent.reply_unsendable);
        // Audio and documents never share a message, and the order of the selection holds.
        let sent = plan(&[pdf, jpeg, gif, png, mp3, jpeg, pdf], false, false);
        assert_eq!(
            sent.messages,
            [vec![0], vec![1, 2, 3], vec![4], vec![5], vec![6]]
        );
        // MP4 videos share an album with pictures: every client draws such an album whole.
        let sent = plan(&[jpeg, mp4, png, mp4], true, true);
        assert_eq!(sent.messages, [vec![0, 1, 2, 3]]);
        assert!(
            !sent.lead_text,
            "the album carries the caption and the quote"
        );
        // Formats one of the clients does not draw travel alone, each between the albums of
        // its neighbours: HEIC and HEIF (Desktop), AVIF (iOS), QuickTime (Desktop), WebM (iOS),
        // Matroska.
        for (kind, mime) in [
            (Image, "image/heic"),
            (Image, "image/heif"),
            (Image, "image/avif"),
            (Video, "video/quicktime"),
            (Video, "video/webm"),
            (Video, "video/x-matroska"),
            (Image, "image/tiff"),
        ] {
            let lone = PlanFile::new(kind, mime);
            assert!(!album.takes(kind, mime), "{mime}");
            let sent = plan(&[jpeg, png, lone, mp4, jpeg], false, false);
            assert_eq!(sent.messages, [vec![0, 1], vec![2], vec![3, 4]], "{mime}");
        }
        // The type is compared without its parameters and without regard to case; a picture
        // whose type is unknown travels alone (the list says what may share).
        assert!(album.takes(Image, "Image/JPEG; charset=binary"));
        assert!(!album.takes(Image, ""));
        // A picture sent "as file": the core plans it as a generic file (import.rs), so it
        // never joins an album whatever it holds.
        let as_file = PlanFile::new(File, "application/octet-stream");
        let sent = plan(&[jpeg, as_file, jpeg], false, false);
        assert_eq!(sent.messages, [vec![0], vec![1], vec![2]]);
        // More pictures than a message keeps: 31 each (Desktop's composer stops at 32).
        let sent = plan(&[jpeg; 70], true, false);
        assert_eq!(
            sent.messages.iter().map(Vec::len).collect::<Vec<_>>(),
            [31, 31, 8]
        );
        // A voice note takes no caption: the text goes first, carrying the quote.
        let voice = PlanFile::new(Voice, "audio/ogg");
        let sent = plan(&[voice, jpeg], true, true);
        assert!(sent.lead_text);
        assert_eq!(sent.message_count(), 3);
        // Groups pack the same way.
        let fixture = crate::receive::tests::fixture();
        let group = group_features(
            None,
            crate::groups::Membership::Member,
            &fixture.account,
            &Limits::with_max_bytes(1 << 20),
        );
        assert_eq!(group.file_packing, chat.file_packing);
    }

    /// The album size follows `android.attachments.maxCount` from the remote config: one less
    /// than Android keeps (the long text of a caption is one of its pointers), never more
    /// than Desktop's constant 32, never less than one; a new value reaches the chats the core
    /// has.
    #[test]
    fn the_album_size_follows_the_remote_config() {
        assert_eq!(album_max(None), 31);
        assert_eq!(album_max(Some(32)), 31);
        assert_eq!(album_max(Some(16)), 15);
        assert_eq!(
            album_max(Some(64)),
            32,
            "Desktop cuts at 32 whatever Android takes"
        );
        assert_eq!(album_max(Some(33)), 32);
        assert_eq!(album_max(Some(1)), 1, "a file still goes out, alone");
        let mut config = BTreeMap::new();
        assert_eq!(Limits::of(&config).album_max, 31);
        config.insert(CONFIG_MAX_POINTERS.to_owned(), " 12 ".to_owned());
        assert_eq!(Limits::of(&config).album_max, 11);
        for bad in ["0", "-3", "many", ""] {
            config.insert(CONFIG_MAX_POINTERS.to_owned(), bad.to_owned());
            assert_eq!(Limits::of(&config).album_max, 31, "{bad:?}");
        }
        config.insert(CONFIG_MAX_POINTERS.to_owned(), "12".to_owned());
        let chat = direct_features(true, false, false, &Limits::of(&config));
        assert_eq!(chat.file_packing.albums[0].max, 11);
        let jpeg = kourier_model::PlanFile::new(AttachmentKind::Image, "image/jpeg");
        assert_eq!(
            chat.plan(&[jpeg; 30], None, false)
                .messages
                .iter()
                .map(Vec::len)
                .collect::<Vec<_>>(),
            [11, 11, 8]
        );

        // Through the stored config: the chat the core has gets its features again, once.
        let fixture = crate::receive::tests::fixture();
        let account = &fixture.account;
        let bob = libsignal_core::Aci::from(uuid::Uuid::from_u128(0xb0b));
        let user = aci_user_id(bob).unwrap();
        let max = |event: Option<BackendEvent>| match event {
            Some(BackendEvent::ConversationUpsert(patch)) => {
                patch.features.map(|f| f.file_packing.albums[0].max)
            }
            other => panic!("{other:?}"),
        };
        fixture
            .db
            .zone(|tx| -> Result<()> {
                assert_eq!(max(features_event(tx, account, &user, 1)?), Some(31));
                tx.execute(
                    "INSERT INTO remote_config(name, value)
                     VALUES ('android.attachments.maxCount', '20')",
                    [],
                )?;
                let events = refresh_features(tx, account, 2)?;
                assert_eq!(events.len(), 1, "{events:?}");
                assert_eq!(max(events.into_iter().next()), Some(19));
                assert!(refresh_features(tx, account, 3)?.is_empty(), "once");
                Ok(())
            })
            .unwrap();
    }

    /// Timers: Desktop's menu is offered, and a 1:1 chat takes every whole second from one
    /// second to the largest value Android reads as it was sent (a signed 32-bit number of
    /// seconds). Four weeks, where the official pickers end, is no limit: eight weeks, a
    /// year and ten years are taken. A group takes none (its timer is group state, which
    /// Kourier cannot write).
    #[test]
    fn a_chat_takes_any_timer_of_signals_range() {
        use std::time::Duration;
        const WEEK: u64 = 7 * 24 * 3600;
        const YEAR: u64 = 365 * 24 * 3600;
        let limits = Limits::with_max_bytes(1000);
        let chat = direct_features(true, false, false, &limits);
        assert_eq!(
            chat.disappearing_custom.get(&DisappearKind::AfterRead),
            Some(&TimerRange {
                min_secs: 1,
                max_secs: 2_147_483_647,
                step_secs: 1
            })
        );
        for secs in [
            1,
            45,
            90,
            12_345,
            4 * WEEK,
            4 * WEEK + 1,
            8 * WEEK,
            YEAR,
            10 * YEAR,
            2_147_483_647,
        ] {
            assert!(chat.takes_timer(Duration::from_secs(secs)), "{secs}");
        }
        assert!(
            !chat.takes_timer(Duration::from_secs(2_147_483_648)),
            "Android would read it as a negative number"
        );
        assert!(
            i32::try_from(MAX_TIMER_SECS).is_ok() && i32::try_from(MAX_TIMER_SECS + 1).is_err()
        );
        assert!(
            !chat.takes_timer(Duration::from_millis(1500)),
            "whole seconds"
        );
        assert!(
            !chat.takes_timer(Duration::ZERO),
            "off is not a timer value"
        );
        assert!(chat.has_timers());
        // The backend's own check agrees with what the features state.
        assert_eq!(TIMER_RANGE.max_secs, MAX_TIMER_SECS);
        assert!(
            TIMER_CHOICES
                .iter()
                .all(|s| TIMER_RANGE.contains(u64::from(*s)))
        );
        let fixture = crate::receive::tests::fixture();
        let group = group_features(
            None,
            crate::groups::Membership::Member,
            &fixture.account,
            &limits,
        );
        assert!(!group.has_timers());
        assert!(!group.takes_timer(Duration::from_secs(30)));
    }

    /// The files a Desktop receiver will not save: the list is Desktop's own, entry for entry,
    /// every Signal chat states it (1:1 and group), and a name is judged the way Desktop's
    /// pattern `\.(…)(\.|\s)*?$` judges it.
    #[test]
    fn the_files_desktop_will_not_save_are_desktops_list_and_rule() {
        let desktop: Vec<String> = DESKTOP_DANGEROUS_PATTERN
            .split('|')
            .map(str::to_lowercase)
            .collect();
        assert_eq!(desktop.len(), 47);
        assert_eq!(
            DESKTOP_WILL_NOT_SAVE.to_vec(),
            desktop,
            "Desktop's alternatives, in its order"
        );
        let limits = Limits::with_max_bytes(1000);
        let chat = direct_features(true, false, false, &limits);
        assert_eq!(chat.hard_to_save.len(), 47, "no duplicates");
        let fixture = crate::receive::tests::fixture();
        let group = group_features(
            None,
            crate::groups::Membership::Member,
            &fixture.account,
            &limits,
        );
        assert_eq!(group.hard_to_save, chat.hard_to_save, "groups state it too");
        // What Desktop's pattern matches: the extension at the very end, any case, dots and
        // white space behind it passed over; one of them in the middle does not count.
        for name in [
            "setup.exe",
            "Setup.EXE",
            "tool.Apk",
            "script.js",
            "a.settingcontent-ms",
            "run.ps1.",
            "run.bat  ",
            "run.cmd. .",
            "run.vbs\n",
            ".exe",
            "archive.tar.jar",
        ] {
            assert!(chat.is_hard_to_save(name), "{name:?}");
        }
        for name in [
            "notes.txt",
            "photo.jpg",
            "exe",
            "setup.exe.txt",
            "report.json",
            "page.jsx",
            "my.execs",
            "",
            "archive.zip",
            "installer.deb",
            "script.sh",
        ] {
            assert!(!chat.is_hard_to_save(name), "{name:?}");
        }
        // A chat that cannot be sent to states the list all the same (it is a fact about the
        // receivers, not a permission).
        assert_eq!(
            direct_features(false, false, false, &limits).hard_to_save,
            chat.hard_to_save
        );
    }

    /// The size limit the core checks is the largest file whose encrypted upload the server
    /// takes, not the server's number itself.
    #[test]
    fn the_file_limit_is_the_largest_file_whose_upload_fits() {
        for max_bytes in [1000u64, 5000, 1 << 20, DEFAULT_MAX_ATTACHMENT_BYTES] {
            let fits = max_plaintext(max_bytes);
            assert!(
                fits < max_bytes,
                "padding and the MAC take room: {max_bytes}"
            );
            assert!(max_blob_len(fits) <= max_bytes, "{max_bytes}");
            assert!(max_blob_len(fits + 1) > max_bytes, "{max_bytes}");
        }
        // 100 MiB: what Desktop's `isAttachmentTooLargeToSend` allows, within 5 %.
        let fits = max_plaintext(DEFAULT_MAX_ATTACHMENT_BYTES);
        assert!(fits > DEFAULT_MAX_ATTACHMENT_BYTES / 100 * 95);
        assert_eq!(
            max_plaintext(100),
            0,
            "nothing fits below the smallest blob"
        );
        let limits = file_limits(DEFAULT_MAX_ATTACHMENT_BYTES);
        assert!(limits.values().all(|l| l.max_size == fits));
    }
}

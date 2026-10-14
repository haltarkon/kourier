//! The one cxx-qt bridge of the crate: every QObject QML sees (§8.1). The Rust structs and the
//! method bodies live in [`crate::app`], [`crate::models`] and [`crate::controllers`]; this file
//! only declares what moc and qmltyperegistrar need.

// cxx drops the `# Safety` sections of extern functions from the code it generates (the same
// false positive the cxx-qt examples allow).
#![allow(clippy::missing_safety_doc)]

pub use crate::app::AppRust;
pub use crate::controllers::{ComposerRust, LoginRust, SettingsRust};
pub use crate::models::accounts::AccountsRust;
pub use crate::models::attachments::AttachmentsRust;
pub use crate::models::conversations::ConversationsRust;
pub use crate::models::timeline::TimelineRust;

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++Qt" {
        include!(<QtCore/QAbstractListModel>);
        /// Qt's list model base class.
        #[qobject]
        type QAbstractListModel;
    }

    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        /// Qt string.
        type QString = cxx_qt_lib::QString;
        include!("cxx-qt-lib/qvariant.h");
        /// Qt variant.
        type QVariant = cxx_qt_lib::QVariant;
        include!("cxx-qt-lib/qmodelindex.h");
        /// Model index.
        type QModelIndex = cxx_qt_lib::QModelIndex;
        include!("cxx-qt-lib/qhash.h");
        /// Role names.
        type QHash_i32_QByteArray = cxx_qt_lib::QHash<cxx_qt_lib::QHashPair_i32_QByteArray>;
        include!("cxx-qt-lib/qvector.h");
        /// Role lists of `dataChanged`.
        type QVector_i32 = cxx_qt_lib::QVector<i32>;
        include!("cxx-qt-lib/qcolor.h");
        /// Colors.
        type QColor = cxx_qt_lib::QColor;
        include!("cxx-qt-lib/qstringlist.h");
        /// String lists (the URLs of picked or dropped files).
        type QStringList = cxx_qt_lib::QStringList;
    }

    #[namespace = "kourier"]
    unsafe extern "C++" {
        include!("kourier-qt/cpp/devtools.h");
        /// Grabs `window` (a `QQuickWindow`) into a PNG at `path`; returns the mean luminance
        /// 0 … 255 of the image, or -1 on failure.
        ///
        /// # Safety
        ///
        /// `window` must be null or a live `QObject`.
        #[cxx_name = "grabWindowToFile"]
        unsafe fn grab_window_to_file(window: *mut QObject, path: &QString) -> i32;

        /// The images of `window` (a `QQuickWindow`) that are loading (-1: not a window).
        ///
        /// # Safety
        ///
        /// `window` must be null or a live `QObject`.
        #[cxx_name = "imagesLoading"]
        unsafe fn images_loading(window: *mut QObject) -> i32;

        /// The visible images of `window` that draw an attachment (`image://kourier/thumb/…`
        /// or `…/att/…`), whatever their status (-1: not a window).
        ///
        /// # Safety
        ///
        /// `window` must be null or a live `QObject`.
        #[cxx_name = "attachmentPictures"]
        unsafe fn attachment_pictures(window: *mut QObject) -> i32;

        /// Grabs `window` and saves it at `path` when it equals the previous grab: returns the
        /// luminance then, -2 when the frame differs from the previous one (it is kept), -1 on
        /// failure.
        ///
        /// # Safety
        ///
        /// `window` must be null or a live `QObject`.
        #[cxx_name = "grabIfStable"]
        unsafe fn grab_if_stable(window: *mut QObject, path: &QString) -> i32;

        /// Moves the pointer outside every window (headless screenshots).
        #[cxx_name = "parkCursor"]
        fn park_cursor();

        /// Recolors the links of a `TextEdit.textDocument`.
        ///
        /// # Safety
        ///
        /// `document` must be null or a live `QObject`.
        #[cxx_name = "colorLinks"]
        unsafe fn color_links_of(document: *mut QObject, color: &QColor);

        /// The plain text Qt's rich-text document makes of `html` (tests: displayed positions).
        #[cxx_name = "richTextPlain"]
        fn rich_text_plain(html: &QString) -> QString;

        /// Routes every Qt message to `sink(level, category, text)` from now on.
        #[cxx_name = "installMessageHandler"]
        fn install_message_handler(sink: fn(i32, &str, &str));
    }

    #[namespace = "kourier"]
    unsafe extern "C++" {
        include!("kourier-qt/cpp/desktop.h");
        /// Sets the taskbar badge (0 hides it).
        #[cxx_name = "setBadgeNumber"]
        fn set_badge_number(number: i64);

        /// Whether closing the last window quits (Kourier quits explicitly).
        #[cxx_name = "setQuitOnLastWindowClosed"]
        fn set_quit_on_last_window_closed(quit: bool);

        /// The bundled SVG at `path` as `size`×`size` ARGB32 bytes in network order, with an
        /// attention dot when `dot` (Qt thread only).
        #[cxx_name = "renderIconArgb"]
        fn render_icon_argb(path: &QString, size: i32, dot: bool) -> Vec<u8>;

        /// An avatar for `name` as `size`×`size` RGBA8888 bytes, a square with corners of
        /// `corner` pixels (half the side or more: a circle) (QImage only: callable off the
        /// Qt thread).
        #[cxx_name = "renderAvatarRgba"]
        fn render_avatar_rgba(name: &QString, anonymous: bool, size: i32, corner: i32) -> Vec<u8>;

        /// Returns free heap memory to the system.
        #[cxx_name = "trimMemory"]
        fn trim_memory();

        /// Notes the end of the desktop session when the session manager announces it (after
        /// the application exists).
        #[cxx_name = "watchSessionEnd"]
        fn watch_session_end();

        /// Whether the desktop session is ending.
        #[cxx_name = "sessionEnding"]
        fn session_ending() -> bool;

        /// Sets what [`session_ending`] says (tests: no session manager is there to ask).
        #[cxx_name = "setSessionEnding"]
        fn set_session_ending(ending: bool);
    }

    unsafe extern "C++" {
        include!("cxx-qt-lib/qqmlapplicationengine.h");
        /// The QML engine.
        type QQmlApplicationEngine = cxx_qt_lib::QQmlApplicationEngine;
    }

    #[namespace = "kourier"]
    unsafe extern "C++" {
        include!("kourier-qt/cpp/imageprovider.h");
        /// Registers the `image://kourier` provider on `engine`; `reader` answers its requests
        /// on the provider's worker threads ([`crate::media::read`]).
        #[cxx_name = "installImageProvider"]
        fn install_image_provider(
            engine: Pin<&mut QQmlApplicationEngine>,
            reader: fn(&str, i32, i32) -> Vec<u8>,
        );

        /// Requests of the provider that are queued or being decoded.
        #[cxx_name = "pendingImageRequests"]
        fn pending_image_requests() -> i32;

        /// Logs, while on, every request the provider gets as `id WxH` (the size Qt asked for:
        /// `sourceSize` times the device pixel ratio); turning it on or off clears the log
        /// (tests of picture quality).
        #[cxx_name = "logImageRequests"]
        fn log_image_requests(on: bool);

        /// The requests logged since, in order; empties the log.
        #[cxx_name = "takeImageRequestLog"]
        fn take_image_request_log() -> Vec<String>;

        /// A JPEG as stored (`width` x `height`, left half red, right half blue) with the EXIF
        /// orientation tag `orientation` (1 to 8: 6 = shown rotated by 90 degrees, 8 = by 270)
        /// and the pixels as stored, as a phone writes it; empty when it cannot be written
        /// (tests of the decoder).
        #[cxx_name = "orientedImage"]
        fn oriented_image(width: i32, height: i32, orientation: i32) -> Vec<u8>;

        /// `[width, height, pictures scaled after the decoder had decoded them whole (0: the
        /// decoder scaled), colour in the middle of the top tenth, of the bottom tenth, the pixels the
        /// decoder made]` of
        /// `file` decoded as the provider does for a request of `width` x `height` (cut to a
        /// square when `cornered`); empty when nothing was decoded (tests of the decoder).
        #[cxx_name = "decodeProbe"]
        fn decode_probe(
            file: Vec<u8>,
            codec: u8,
            width: i32,
            height: i32,
            cornered: bool,
        ) -> Vec<i32>;

        /// Whether the clipboard holds an image; if so it is encoded as PNG on a worker thread
        /// and handed to `done` there (Qt thread only).
        #[cxx_name = "pasteClipboardImage"]
        fn paste_clipboard_image(done: fn(Vec<u8>)) -> bool;

        /// Puts a PNG on the clipboard as an image (tests of pasting).
        #[cxx_name = "setClipboardImagePng"]
        fn set_clipboard_image_png(png: Vec<u8>) -> bool;

        /// The local files the clipboard holds (copied in a file manager), as paths; empty
        /// when it holds none (Qt thread only).
        #[cxx_name = "clipboardFiles"]
        fn clipboard_files() -> QStringList;

        /// Puts local files on the clipboard as a file manager's "Copy" does (tests of
        /// pasting).
        #[cxx_name = "setClipboardFiles"]
        fn set_clipboard_files(paths: &QStringList);

        /// Puts `text` on the clipboard ("Copy Text"; Qt thread only).
        #[cxx_name = "setClipboardText"]
        fn set_clipboard_text(text: &QString);

        /// The clipboard's text (tests of copying).
        #[cxx_name = "clipboardText"]
        fn clipboard_text() -> QString;

        /// Alpha of the corner pixel × 1000 + alpha of the center pixel of `bytes` decoded as
        /// a `side`-pixel avatar by the decoder of `codec` (`media::Codec as u8`) alone, or -1
        /// when nothing was decoded (tests: avatars come out round, other content is refused).
        #[cxx_name = "roundAvatarProbe"]
        fn round_avatar_probe(bytes: Vec<u8>, codec: u8, side: i32) -> i32;

        /// The same with a corner radius of `radius` pixels: the alpha of the corner pixel
        /// << 16, of the pixel a tenth of the side in on the diagonal << 8, and of the
        /// center pixel, or -1 (tests: a circle has 0, 0, 255; a square with small corners 0,
        /// 255, 255; a plain square 255, 255, 255).
        #[cxx_name = "avatarCornerProbe"]
        fn avatar_corner_probe(bytes: Vec<u8>, codec: u8, side: i32, radius: i32) -> i32;

        /// `[width, height, transparent area of the top left quarter in pixels]` of the PNG `grab`
        /// of a cornered avatar (the area is the sum of 1 - alpha/255; a quarter circle of radius
        /// r has r² (1 - pi/4)); empty when it does not decode (tests of picture quality).
        #[cxx_name = "cornerProbe"]
        fn corner_probe(grab: Vec<u8>) -> Vec<f64>;

        /// A gradient PNG (fixture screenshots of the composer).
        #[cxx_name = "samplePng"]
        fn sample_png(width: i32, height: i32, hue: i32) -> Vec<u8>;

        /// The same picture in another format Qt can write (`jpeg`, `webp`, …); empty when it
        /// cannot (tests of the decoders).
        #[cxx_name = "sampleImage"]
        fn sample_image(width: i32, height: i32, hue: i32, format: &str) -> Vec<u8>;

        /// A picture of fine detail (stripes, two lines of text, thin diagonals) in a
        /// Qt format; empty when it cannot be written (tests of picture quality).
        #[cxx_name = "detailImage"]
        fn detail_image(width: i32, height: i32, format: &str) -> Vec<u8>;

        /// `[coarse psnr in dB against the provider's answer, variance of the Laplacian of the grab,
        /// the same of the file scaled smoothly, grab width, grab height, coarse psnr in dB
        /// against that scaled file, the same variance of the provider's answer]` for the PNG `grab`
        /// of an item and the `file` (decoded by `codec`) it shows; empty when either does
        /// not decode (tests of picture quality).
        #[cxx_name = "pictureProbe"]
        fn picture_probe(file: Vec<u8>, codec: u8, grab: Vec<u8>) -> Vec<f64>;
    }

    // ---- App -------------------------------------------------------------------------------

    #[auto_cxx_name]
    extern "RustQt" {
        /// The root object QML talks to.
        #[qobject]
        #[qml_element]
        #[qml_singleton]
        #[qproperty(QString, version, READ, CONSTANT)]
        #[qproperty(bool, fixture, READ, CONSTANT)]
        #[qproperty(QString, start_page, READ, CONSTANT)]
        #[qproperty(QString, screenshot, READ, CONSTANT)]
        #[qproperty(i32, window_width, READ, CONSTANT)]
        #[qproperty(i32, window_height, READ, CONSTANT)]
        #[qproperty(bool, has_accounts)]
        #[qproperty(bool, loaded)]
        #[qproperty(bool, timers_active)]
        #[qproperty(i64, current_account)]
        #[qproperty(QString, current_title)]
        #[qproperty(bool, start_hidden, READ, CONSTANT)]
        #[qproperty(i32, release_delay_ms, READ, CONSTANT)]
        #[qproperty(bool, trace_frame)]
        #[qproperty(QString, perf_scenario, READ, CONSTANT)]
        #[qproperty(i32, tray_status)]
        #[qproperty(i32, unread_count)]
        #[qproperty(QString, banner_kind)]
        #[qproperty(QString, banner_text)]
        #[qproperty(QString, banner_action)]
        #[qproperty(i64, banner_account)]
        #[qproperty(QVariant, banner_transfer)]
        #[qproperty(QVariant, history_accounts)]
        #[qproperty(QVariant, account_actions)]
        #[qproperty(QVariant, revisions)]
        #[qproperty(QVariant, networks, READ, CONSTANT)]
        #[qproperty(*mut AccountListModel, accounts, READ, CONSTANT)]
        #[qproperty(*mut ConversationListModel, conversations, READ, CONSTANT)]
        #[qproperty(*mut TimelineModel, timeline, READ, CONSTANT)]
        #[qproperty(*mut ComposerController, composer, READ, CONSTANT)]
        #[qproperty(*mut LoginController, login, READ, CONSTANT)]
        #[qproperty(*mut SettingsController, settings, READ, CONSTANT)]
        type App = super::AppRust;

        /// Shows one account's conversations (`-1` = all chats).
        #[qinvokable]
        fn select_account(self: Pin<&mut App>, account: i64);

        /// The first frame is on screen: backends may connect (§8.2 step 4).
        #[qinvokable]
        fn ready(self: Pin<&mut App>);

        /// A frame was presented while `traceFrame` is set (timing marks, architecture §1.3):
        /// `list_shown` and `chat_shown` say whether the conversation list and the open
        /// chat's rows were on screen in it.
        #[qinvokable]
        fn frame_traced(self: Pin<&mut App>, list_shown: bool, chat_shown: bool);

        /// Writes a timing mark `name` with `fields` (`key=number` pairs separated by spaces)
        /// when tracing is on (`KOURIER_TRACE_STARTUP=1`). Names and keys are lower-case
        /// identifiers and values are numbers: a mark cannot carry text.
        #[qinvokable]
        fn trace_mark(self: &App, name: &QString, fields: &QString);

        /// The conversation at `row` of the list as it is filtered now (0 when there is
        /// none).
        #[qinvokable]
        fn conversation_at(self: &App, row: i32) -> i64;

        /// The conversation `step` rows below `from` in the list as it is filtered now (`-1`:
        /// the one above): the keyboard's next and previous chat. When `from` is not in the
        /// list the first row (`step` > 0) or the last (`step` < 0) answers. The ends do not
        /// wrap; 0 when there is no such row.
        #[qinvokable]
        fn adjacent_conversation(self: &App, from: i64, step: i32) -> i64;

        /// The user scrolled to the oldest loaded message: load the page before it.
        #[qinvokable]
        fn fetch_older(self: Pin<&mut App>);

        /// The user scrolled to the newest loaded message of a window that ends early.
        #[qinvokable]
        fn fetch_newer(self: Pin<&mut App>);

        /// Window focus and whether the chat page is on screen.
        #[qinvokable]
        fn set_presence(self: Pin<&mut App>, focused: bool, chat_visible: bool);

        /// What was last reported with `setPresence`: 0 nothing, 1 the window has the
        /// focus, 2 its chat page is on screen, 3 both. Tests read it.
        #[qinvokable]
        fn presence(self: &App) -> i32;

        /// The window is being let go after a while hidden in the tray (§8.2 step 6): the
        /// core gives back the memory it only keeps for speed (it reads it again when it is
        /// next needed). Returns whether the core was asked (`false` without one).
        #[qinvokable]
        fn window_releasing(self: Pin<&mut App>) -> bool;

        /// The window that was destroyed after a while hidden in the tray (§8.2 step 6) is
        /// gone: free heap memory is returned to the system. Presence is not changed: the
        /// shell reports "nothing on screen" with `setPresence` when it lets the window go,
        /// before a new window can exist.
        #[qinvokable]
        fn window_released(self: Pin<&mut App>);

        /// Show and raise the window with the XDG activation `token` (may be empty), and open
        /// `conversation` when it is not 0 (a notification click, another launch, the tray).
        #[qsignal]
        fn activation_requested(self: Pin<&mut App>, token: QString, conversation: i64);

        /// A tray click: hide a shown and active window, else show and raise it.
        #[qsignal]
        fn toggle_requested(self: Pin<&mut App>, token: QString);

        /// Quit (tray menu).
        #[qsignal]
        fn quit_requested(self: Pin<&mut App>);

        /// Expires timed state (typing indicators); QML calls it while `timersActive`.
        #[qinvokable]
        fn tick(self: Pin<&mut App>);

        /// Opens a conversation in the chat page.
        #[qinvokable]
        fn open_conversation(self: Pin<&mut App>, conversation: i64);

        /// Closes the chat page's conversation.
        #[qinvokable]
        fn close_conversation(self: Pin<&mut App>);

        /// Filters the conversation list.
        #[qinvokable]
        fn set_search(self: Pin<&mut App>, query: &QString);

        /// Downloads an attachment (or retries a failed download).
        #[qinvokable]
        fn download_attachment(self: Pin<&mut App>, attachment: i64);

        /// Asks for a plaintext copy of a downloaded attachment (GIFs, stickers, voice notes);
        /// returns a request token, answered with `mediaOpened(token, url, movie)` or
        /// `mediaFailed(token)`. Each token is released with `releaseMedia` when its view goes
        /// (before or after the answer).
        #[qinvokable]
        fn open_media(self: Pin<&mut App>, attachment: i64) -> i64;

        /// The view of an `openMedia` request went.
        #[qinvokable]
        fn release_media(self: Pin<&mut App>, token: i64);

        /// Opens a downloaded attachment with the desktop's default application (the caller
        /// confirmed first for types that are not pictures, sound or video): answered with
        /// `externalReady`. The copy lives until the conversation closes.
        #[qinvokable]
        fn open_externally(self: Pin<&mut App>, attachment: i64);

        /// The full-size URL of the newest picture in the open timeline (`--page=viewer`).
        #[qinvokable]
        fn latest_image(self: &App) -> QString;

        /// Adds our reaction `emoji` to a message of the open conversation, or with `remove`
        /// takes it back. A refusal shows as the composer's notice.
        #[qinvokable]
        fn react(self: Pin<&mut App>, message: i64, emoji: &QString, remove: bool);

        /// Deletes a message, for everyone or on this account only.
        #[qinvokable]
        fn delete_message(self: Pin<&mut App>, message: i64, for_everyone: bool);

        /// Sends a failed own message again.
        #[qinvokable]
        fn retry_message(self: Pin<&mut App>, message: i64);

        /// Takes back an own message nobody has yet (queued or failed).
        #[qinvokable]
        fn cancel_message(self: Pin<&mut App>, message: i64);

        /// Sets the open conversation's disappearing-message timer (`0` turns it off): one
        /// of `timeline.timerOptions`, or any value of the custom range the timeline states
        /// (`timerCustomMin` … `timerCustomMax`). A value the chat does not take is refused
        /// by the core and shows as a notice.
        #[qinvokable]
        fn set_timer(self: Pin<&mut App>, seconds: i32);

        /// Deletes every message of the open conversation for this account (the chat stays in
        /// the list). The caller asked first.
        #[qinvokable]
        fn clear_conversation(self: Pin<&mut App>);

        /// Deletes the open conversation for this account: its messages and its draft. It
        /// leaves the list until its next message. The caller asked first.
        #[qinvokable]
        fn delete_conversation(self: Pin<&mut App>);

        /// Marks the conversation `conversation` read up to its newest message, because the
        /// user asked (the list's "Mark as Read"): any chat, open or not, focused window or
        /// not. The list shows the result when the core has stored it.
        #[qinvokable]
        fn mark_read(self: Pin<&mut App>, conversation: i64);

        /// Removes one attachment of a message of the open conversation for this account.
        /// The caller asked first.
        #[qinvokable]
        fn delete_attachment(self: Pin<&mut App>, attachment: i64);

        /// What the picture viewer steps through for `message` of the open conversation, as
        /// the rows are now (see `models::timeline::viewer_items`): a list, empty when the
        /// message has no pictures (any more); `undefined` when the message is not among the
        /// loaded rows. The viewer asks again at `timelineRowsChanged`.
        #[qinvokable]
        fn viewer_items(self: &App, message: i64) -> QVariant;

        /// The rows of the open conversation changed (an attachment finished downloading, a
        /// message went away, another conversation was opened).
        #[qsignal]
        fn timeline_rows_changed(self: Pin<&mut App>);

        /// CPU time the Qt thread has used so far, in ms. Tests measure the work of the Qt
        /// thread with it: unlike wall time it does not grow while other processes keep
        /// the machine busy.
        #[qinvokable]
        fn thread_cpu_ms(self: &App) -> f64;

        /// Shows a message of the open conversation (a quote was activated): answered with
        /// the timeline's `jumpRequested(message, row)` once the message is among the rows.
        #[qinvokable]
        fn jump_to(self: Pin<&mut App>, message: i64);

        /// Loads the newest messages when the window ends before them ("Jump to Latest").
        #[qinvokable]
        fn jump_to_latest(self: Pin<&mut App>);

        /// Puts `text` on the clipboard ("Copy Text").
        #[qinvokable]
        fn copy_text(self: &App, text: &QString);

        /// The banner's button was pressed: "Link Again" starts the login of the logged-out
        /// account (`relinkStarted`), "Verify…" asks to open the challenge in the browser
        /// (`challengeRequested`), "Restart" restarts a stopped account.
        #[qinvokable]
        fn banner_activate(self: Pin<&mut App>);

        /// Hands the network what the browser gave back after the challenge of `account` was
        /// solved (the solution token or the whole link). A refusal shows as a notice.
        #[qinvokable]
        fn submit_challenge(self: Pin<&mut App>, account: i64, answer: &QString);

        /// The newest message of the open timeline that fits `what`: `editable` (an own message
        /// that can be edited), `deletable` (one that can be deleted for everyone),
        /// `photo` (one with a picture that can be replied to), `album` (one with several
        /// pictures), `removable` (an album one of whose pictures can be deleted),
        /// `sticker`, `file` (a document), `voice` (a voice note), `quote` (one that quotes
        /// a message that is known), `edited`, `reacted` (one with reactions), `own` (any
        /// own message); 0 when there is none. For the fixture pages and tests.
        #[qinvokable]
        fn find_message(self: &App, what: &QString) -> i64;

        /// Fixture pages only: answers the folder question of the open chat the way a page
        /// shows it. `add`: the files inside join the list. `zip`: the first folder is
        /// packed for real, the others stand still part way (a picture of packing must not
        /// depend on how fast the disk is). `false` outside fixture mode and without a
        /// question.
        #[qinvokable]
        fn fixture_folders(self: Pin<&mut App>, answer: &QString) -> bool;

        /// Asks for the versions of an edited message: `revisions` holds them, newest first,
        /// when `revisionsLoaded(message)` comes. `false` when there is no history to ask for.
        #[qinvokable]
        fn show_revisions(self: Pin<&mut App>, message: i64) -> bool;

        /// The versions asked for with `showRevisions` are in `revisions`. Comes again when
        /// the message changes while its history is shown (until `closeRevisions`).
        #[qsignal]
        fn revisions_loaded(self: Pin<&mut App>, message: i64);

        /// The edit history is no longer shown: it is not kept up to date any more.
        #[qinvokable]
        fn close_revisions(self: Pin<&mut App>);

        /// The login of a logged-out account started: show the link page.
        #[qsignal]
        fn relink_started(self: Pin<&mut App>);

        /// "Continue Without History": ends the wait for the history transfer of `account`,
        /// or its download (`bannerTransfer.skip`). The view stays
        /// until the backend reports the end.
        #[qinvokable]
        fn skip_transfer(self: Pin<&mut App>, account: i64);

        /// "Dismiss" on the banner of a failed history transfer of `account`.
        #[qinvokable]
        fn dismiss_transfer(self: Pin<&mut App>, account: i64);

        /// "Transfer History from Phone…" of `account`, and "Try Again…" of its failed
        /// transfer: asks for the question to be put (`replaceAsked`) before anything
        /// happens. Nothing is sent: only `confirmReplace` acts, and only once per
        /// question. Nothing happens when the account is gone.
        #[qinvokable]
        fn ask_replace(self: Pin<&mut App>, account: i64);

        /// Put this question to the user: a map of `account`, `title`, `about` (which
        /// account), `paragraphs` (what happens), `confirm` and `cancel` (the buttons; the
        /// default is to cancel). The answer is `confirmReplace` or `cancelReplace`.
        #[qsignal]
        fn replace_asked(self: Pin<&mut App>, question: QVariant);

        /// The user confirmed the question about `account` that `askReplace` opened: the
        /// account is removed from this computer and linked again (`replaceStarted`: show
        /// the link page). Without that question open nothing happens.
        #[qinvokable]
        fn confirm_replace(self: Pin<&mut App>, account: i64);

        /// The question was closed without confirming.
        #[qinvokable]
        fn cancel_replace(self: Pin<&mut App>);

        /// "Remove Account…" (`action` "remove") or "Unlink…" ("unlink") of `account`
        /// (`accountActions`, the drawer's menu): asks for the question to be put
        /// (`accountActionAsked`) before anything happens. Nothing is sent; nothing happens
        /// when the account is gone or does not offer the action.
        #[qinvokable]
        fn ask_account_action(self: Pin<&mut App>, account: i64, action: &QString);

        /// Put this question to the user: the map of `replaceAsked` and its `action`. The
        /// answer is `confirmAccountAction` or `cancelAccountAction`.
        #[qsignal]
        fn account_action_asked(self: Pin<&mut App>, question: QVariant);

        /// The user confirmed the question that `askAccountAction` opened about `account`
        /// and `action`: the command goes out, alone. Without that question open nothing
        /// happens.
        #[qinvokable]
        fn confirm_account_action(self: Pin<&mut App>, account: i64, action: &QString);

        /// The question about an account action was closed without confirming.
        #[qinvokable]
        fn cancel_account_action(self: Pin<&mut App>);

        /// What became of the device of a removed or unlinked account, told once: `text`
        /// (what happened here); when the network could not be told, `onPhone` (what to do
        /// on the phone) and `title`, and the notice stays until the user closes it.
        #[qsignal]
        fn logout_notice(self: Pin<&mut App>, text: QString, on_phone: QString, title: QString);

        /// The question to put before an action that would cancel the link in progress
        /// (opening Settings, "Link Again" of an account while a link waits at its QR
        /// code): a map of `title`, `text`, `confirm` (cancels the link and goes on) and
        /// `cancel` (keeps it, the default); `undefined` when nothing would be lost.
        #[qinvokable]
        fn abandon_question(self: &App) -> QVariant;

        /// The question before the application quits (`buttons.md` F8: closing the window,
        /// Ctrl+Q): while a history transfer runs or a new link still fetches its contacts
        /// and groups. The same map as `abandon_question` (`confirm` quits, `cancel` keeps
        /// the application open, the default); `undefined` when nothing runs.
        #[qinvokable]
        fn quit_question(self: &App) -> QVariant;

        /// A history transfer ended: say `text` once, in passing.
        #[qsignal]
        fn transfer_notice(self: Pin<&mut App>, text: QString);

        /// An account was removed to be linked again with its history: show the link page.
        #[qsignal]
        fn replace_started(self: Pin<&mut App>);

        /// The user asked to solve the challenge of `account`: hand `url` to the browser.
        #[qsignal]
        fn challenge_requested(self: Pin<&mut App>, account: i64, url: QString);

        /// The `file://` URL answering the `openMedia` request `token`. `movie`: the copy is a
        /// GIF or WebP by content, the only files `AnimatedImage` may be given (it would let
        /// any installed image plugin read anything else).
        #[qsignal]
        fn media_opened(self: Pin<&mut App>, token: i64, url: QString, movie: bool);

        /// The `openMedia` request `token` failed.
        #[qsignal]
        fn media_failed(self: Pin<&mut App>, token: i64);

        /// The copy for `openExternally` is ready: hand `url` to `Qt.openUrlExternally`.
        #[qsignal]
        fn external_ready(self: Pin<&mut App>, attachment: i64, url: QString);

        /// Gives the links of a message text (`TextEdit.textDocument`) the color `color`.
        ///
        /// # Safety
        ///
        /// `document` must be null or a live `QObject` (QML passes `textDocument`).
        #[qinvokable]
        unsafe fn color_links(self: &App, document: *mut QObject, color: &QColor);

        /// The icon theme's name for the application's icon when the theme has one (an
        /// installed application), else an empty string: `Style.appIcon` then takes the
        /// bundled file.
        #[qinvokable]
        fn themed_app_icon(self: &App) -> QString;

        /// Saves a screenshot of `window` and prints its luminance (`--screenshot`).
        ///
        /// # Safety
        ///
        /// `window` must be null or a live `QObject` (QML passes the root window).
        #[qinvokable]
        unsafe fn save_screenshot(
            self: Pin<&mut App>,
            window: *mut QObject,
            path: &QString,
        ) -> bool;

        /// What still keeps a `--screenshot` from being final, as the reasons ("images 2,
        /// requests 1, derive 3"); empty when nothing does: the images of `window` that are
        /// loading, the provider's unfinished requests and the thumbnails the core still derives.
        ///
        /// # Safety
        ///
        /// `window` must be null or a live `QObject`.
        #[qinvokable]
        unsafe fn screenshot_holds(self: &App, window: *mut QObject) -> QString;

        /// Saves a screenshot of `window` to `path` when the frame equals the previous call's
        /// (two equal frames in a row: nothing moves); prints `shot: <path> luma=<n>
        /// settle_ms=<settle_ms>` then and returns the luminance. -2: the frame differs from the
        /// previous one, call again; -1: failed.
        ///
        /// # Safety
        ///
        /// `window` must be null or a live `QObject`.
        #[qinvokable]
        unsafe fn save_screenshot_if_stable(
            self: Pin<&mut App>,
            window: *mut QObject,
            path: &QString,
            settle_ms: i32,
        ) -> i32;

        /// The last resort of a `--screenshot` that did not settle: saves the frame as it is,
        /// prints the `shot:` line and `shot: unsettled <holds>` on stderr (`cargo xtask shots`
        /// makes that a failure of the render). Returns the luminance, or -1.
        ///
        /// # Safety
        ///
        /// `window` must be null or a live `QObject`.
        #[qinvokable]
        unsafe fn save_unsettled_screenshot(
            self: Pin<&mut App>,
            window: *mut QObject,
            path: &QString,
            holds: &QString,
            settle_ms: i32,
        ) -> i32;
    }

    impl cxx_qt::Threading for App {}
    impl cxx_qt::Initialize for App {}
    impl cxx_qt::Threading for ComposerController {}

    // ---- AccountListModel -----------------------------------------------------------------

    #[auto_cxx_name]
    extern "RustQt" {
        /// Accounts in the drawer.
        #[qobject]
        #[base = QAbstractListModel]
        #[qml_element]
        #[qml_uncreatable]
        #[qproperty(i32, count)]
        type AccountListModel = super::AccountsRust;

        #[cxx_override]
        fn data(self: &AccountListModel, index: &QModelIndex, role: i32) -> QVariant;
        #[cxx_override]
        fn row_count(self: &AccountListModel, parent: &QModelIndex) -> i32;
        #[cxx_override]
        fn role_names(self: &AccountListModel) -> QHash_i32_QByteArray;
    }

    #[auto_cxx_name]
    unsafe extern "RustQt" {
        #[inherit]
        unsafe fn begin_insert_rows(
            self: Pin<&mut AccountListModel>,
            parent: &QModelIndex,
            first: i32,
            last: i32,
        );
        #[inherit]
        unsafe fn end_insert_rows(self: Pin<&mut AccountListModel>);
        #[inherit]
        unsafe fn begin_remove_rows(
            self: Pin<&mut AccountListModel>,
            parent: &QModelIndex,
            first: i32,
            last: i32,
        );
        #[inherit]
        unsafe fn end_remove_rows(self: Pin<&mut AccountListModel>);
        #[inherit]
        unsafe fn begin_move_rows(
            self: Pin<&mut AccountListModel>,
            source_parent: &QModelIndex,
            source_first: i32,
            source_last: i32,
            destination_parent: &QModelIndex,
            destination_child: i32,
        ) -> bool;
        #[inherit]
        unsafe fn end_move_rows(self: Pin<&mut AccountListModel>);
        #[inherit]
        unsafe fn begin_reset_model(self: Pin<&mut AccountListModel>);
        #[inherit]
        unsafe fn end_reset_model(self: Pin<&mut AccountListModel>);
        #[inherit]
        fn index(
            self: &AccountListModel,
            row: i32,
            column: i32,
            parent: &QModelIndex,
        ) -> QModelIndex;
        #[inherit]
        #[qsignal]
        fn data_changed(
            self: Pin<&mut AccountListModel>,
            top_left: &QModelIndex,
            bottom_right: &QModelIndex,
            roles: &QVector_i32,
        );
    }

    // ---- AttachmentListModel --------------------------------------------------------------

    #[auto_cxx_name]
    extern "RustQt" {
        /// The files waiting in the composer, in sending order.
        #[qobject]
        #[base = QAbstractListModel]
        #[qml_element]
        #[qml_uncreatable]
        #[qproperty(i32, count)]
        type AttachmentListModel = super::AttachmentsRust;

        #[cxx_override]
        fn data(self: &AttachmentListModel, index: &QModelIndex, role: i32) -> QVariant;
        #[cxx_override]
        fn row_count(self: &AttachmentListModel, parent: &QModelIndex) -> i32;
        #[cxx_override]
        fn role_names(self: &AttachmentListModel) -> QHash_i32_QByteArray;
    }

    #[auto_cxx_name]
    unsafe extern "RustQt" {
        #[inherit]
        unsafe fn begin_insert_rows(
            self: Pin<&mut AttachmentListModel>,
            parent: &QModelIndex,
            first: i32,
            last: i32,
        );
        #[inherit]
        unsafe fn end_insert_rows(self: Pin<&mut AttachmentListModel>);
        #[inherit]
        unsafe fn begin_remove_rows(
            self: Pin<&mut AttachmentListModel>,
            parent: &QModelIndex,
            first: i32,
            last: i32,
        );
        #[inherit]
        unsafe fn end_remove_rows(self: Pin<&mut AttachmentListModel>);
        #[inherit]
        unsafe fn begin_move_rows(
            self: Pin<&mut AttachmentListModel>,
            source_parent: &QModelIndex,
            source_first: i32,
            source_last: i32,
            destination_parent: &QModelIndex,
            destination_child: i32,
        ) -> bool;
        #[inherit]
        unsafe fn end_move_rows(self: Pin<&mut AttachmentListModel>);
        #[inherit]
        unsafe fn begin_reset_model(self: Pin<&mut AttachmentListModel>);
        #[inherit]
        unsafe fn end_reset_model(self: Pin<&mut AttachmentListModel>);
        #[inherit]
        fn index(
            self: &AttachmentListModel,
            row: i32,
            column: i32,
            parent: &QModelIndex,
        ) -> QModelIndex;
        #[inherit]
        #[qsignal]
        fn data_changed(
            self: Pin<&mut AttachmentListModel>,
            top_left: &QModelIndex,
            bottom_right: &QModelIndex,
            roles: &QVector_i32,
        );
    }

    // ---- ConversationListModel ------------------------------------------------------------

    #[auto_cxx_name]
    extern "RustQt" {
        /// The conversation list (sorted and filtered in Rust).
        #[qobject]
        #[base = QAbstractListModel]
        #[qml_element]
        #[qml_uncreatable]
        #[qproperty(i32, count)]
        #[qproperty(i64, current_id)]
        type ConversationListModel = super::ConversationsRust;

        #[cxx_override]
        fn data(self: &ConversationListModel, index: &QModelIndex, role: i32) -> QVariant;
        #[cxx_override]
        fn row_count(self: &ConversationListModel, parent: &QModelIndex) -> i32;
        #[cxx_override]
        fn role_names(self: &ConversationListModel) -> QHash_i32_QByteArray;
    }

    #[auto_cxx_name]
    unsafe extern "RustQt" {
        #[inherit]
        unsafe fn begin_insert_rows(
            self: Pin<&mut ConversationListModel>,
            parent: &QModelIndex,
            first: i32,
            last: i32,
        );
        #[inherit]
        unsafe fn end_insert_rows(self: Pin<&mut ConversationListModel>);
        #[inherit]
        unsafe fn begin_remove_rows(
            self: Pin<&mut ConversationListModel>,
            parent: &QModelIndex,
            first: i32,
            last: i32,
        );
        #[inherit]
        unsafe fn end_remove_rows(self: Pin<&mut ConversationListModel>);
        #[inherit]
        unsafe fn begin_move_rows(
            self: Pin<&mut ConversationListModel>,
            source_parent: &QModelIndex,
            source_first: i32,
            source_last: i32,
            destination_parent: &QModelIndex,
            destination_child: i32,
        ) -> bool;
        #[inherit]
        unsafe fn end_move_rows(self: Pin<&mut ConversationListModel>);
        #[inherit]
        unsafe fn begin_reset_model(self: Pin<&mut ConversationListModel>);
        #[inherit]
        unsafe fn end_reset_model(self: Pin<&mut ConversationListModel>);
        #[inherit]
        fn index(
            self: &ConversationListModel,
            row: i32,
            column: i32,
            parent: &QModelIndex,
        ) -> QModelIndex;
        #[inherit]
        #[qsignal]
        fn data_changed(
            self: Pin<&mut ConversationListModel>,
            top_left: &QModelIndex,
            bottom_right: &QModelIndex,
            roles: &QVector_i32,
        );
    }

    // ---- TimelineModel --------------------------------------------------------------------

    #[auto_cxx_name]
    extern "RustQt" {
        /// The open conversation: header properties and timeline rows (row 0 = newest).
        #[qobject]
        #[base = QAbstractListModel]
        #[qml_element]
        #[qml_uncreatable]
        #[qproperty(i32, count)]
        #[qproperty(i64, conversation_id)]
        #[qproperty(QString, title)]
        #[qproperty(QString, subtitle)]
        #[qproperty(bool, typing)]
        #[qproperty(bool, is_group)]
        #[qproperty(bool, is_self)]
        #[qproperty(bool, muted)]
        #[qproperty(QString, avatar_url)]
        #[qproperty(QString, cannot_send)]
        #[qproperty(QString, attach_kinds)]
        #[qproperty(QString, timer_label)]
        #[qproperty(i32, timer_secs)]
        #[qproperty(QVariant, timer_options)]
        #[qproperty(i32, timer_custom_min)]
        #[qproperty(i32, timer_custom_max)]
        #[qproperty(i32, timer_custom_step)]
        #[qproperty(bool, deletes_sync)]
        #[qproperty(QString, still_filling)]
        #[qproperty(bool, has_older)]
        #[qproperty(bool, has_newer)]
        #[qproperty(bool, loading)]
        type TimelineModel = super::TimelineRust;

        #[cxx_override]
        fn data(self: &TimelineModel, index: &QModelIndex, role: i32) -> QVariant;
        #[cxx_override]
        fn row_count(self: &TimelineModel, parent: &QModelIndex) -> i32;
        #[cxx_override]
        fn role_names(self: &TimelineModel) -> QHash_i32_QByteArray;

        /// Scroll to `row` (the row of `message`) and highlight it: a quote was activated.
        #[qsignal]
        fn jump_requested(self: Pin<&mut TimelineModel>, message: i64, row: i32);

        /// "Read more" on a long text: `message` shows one step more of it; its row changes
        /// in place.
        #[qinvokable]
        fn read_more(self: Pin<&mut TimelineModel>, message: i64);

        /// How many pictures, videos and files of the rows shown are downloading (a
        /// `--screenshot` waits for 0).
        #[qinvokable]
        fn downloading_attachments(self: &TimelineModel) -> i32;
    }

    #[auto_cxx_name]
    unsafe extern "RustQt" {
        #[inherit]
        unsafe fn begin_insert_rows(
            self: Pin<&mut TimelineModel>,
            parent: &QModelIndex,
            first: i32,
            last: i32,
        );
        #[inherit]
        unsafe fn end_insert_rows(self: Pin<&mut TimelineModel>);
        #[inherit]
        unsafe fn begin_remove_rows(
            self: Pin<&mut TimelineModel>,
            parent: &QModelIndex,
            first: i32,
            last: i32,
        );
        #[inherit]
        unsafe fn end_remove_rows(self: Pin<&mut TimelineModel>);
        #[inherit]
        unsafe fn begin_move_rows(
            self: Pin<&mut TimelineModel>,
            source_parent: &QModelIndex,
            source_first: i32,
            source_last: i32,
            destination_parent: &QModelIndex,
            destination_child: i32,
        ) -> bool;
        #[inherit]
        unsafe fn end_move_rows(self: Pin<&mut TimelineModel>);
        #[inherit]
        unsafe fn begin_reset_model(self: Pin<&mut TimelineModel>);
        #[inherit]
        unsafe fn end_reset_model(self: Pin<&mut TimelineModel>);
        #[inherit]
        fn index(self: &TimelineModel, row: i32, column: i32, parent: &QModelIndex) -> QModelIndex;
        #[inherit]
        #[qsignal]
        fn data_changed(
            self: Pin<&mut TimelineModel>,
            top_left: &QModelIndex,
            bottom_right: &QModelIndex,
            roles: &QVector_i32,
        );
    }

    // ---- Controllers ----------------------------------------------------------------------

    #[auto_cxx_name]
    extern "RustQt" {
        /// The composer: reply state and sending.
        #[qobject]
        #[qml_element]
        #[qml_uncreatable]
        #[qproperty(i64, reply_to_id)]
        #[qproperty(QString, reply_author)]
        #[qproperty(QString, reply_text)]
        #[qproperty(QString, reply_icon)]
        #[qproperty(QString, reply_thumb)]
        #[qproperty(i64, edit_id)]
        #[qproperty(bool, edit_caption)]
        #[qproperty(QString, edit_text)]
        #[qproperty(QString, error)]
        #[qproperty(QString, notice)]
        #[qproperty(*mut AttachmentListModel, files, READ, CONSTANT)]
        #[qproperty(i32, attachment_count)]
        #[qproperty(i32, sendable_count)]
        #[qproperty(i32, refused_count)]
        #[qproperty(i32, message_count)]
        #[qproperty(bool, planned)]
        #[qproperty(QString, summary)]
        #[qproperty(QString, hint)]
        #[qproperty(i32, preparing)]
        #[qproperty(i32, preparing_done)]
        #[qproperty(QVariant, folder_question)]
        #[qproperty(bool, folder_asking)]
        type ComposerController = super::ComposerRust;

        /// Answers the question about the folders that wait in the list (`folderAsking`,
        /// `folderQuestion`): `add` puts the files inside them into the list (asked about
        /// once more when they are very many: the question then has `confirming` set and
        /// the next `add` does it), `zip` packs each folder into one archive, `back` returns
        /// from the second question, `cancel` takes the folders out of the list.
        #[qinvokable]
        fn answer_folders(self: Pin<&mut ComposerController>, answer: &QString);

        /// Adds files (`file://` URLs or paths) to the strip in one go: any number, of any
        /// kinds. Nothing is refused here; what the network cannot take is marked in the
        /// list once the core has looked at the files. A folder among them stays in the
        /// list as a folder, is counted and asked about (`folderAsking`). Returns how many
        /// were added (URLs that are no local files are not).
        #[qinvokable]
        fn add_files(self: Pin<&mut ComposerController>, urls: &QStringList) -> i32;

        /// Adds one file (a `file://` URL or a path) to the strip.
        #[qinvokable]
        fn add_file(self: Pin<&mut ComposerController>, url: &QString) -> bool;

        /// Removes entry `index` of the strip.
        #[qinvokable]
        fn remove_attachment(self: Pin<&mut ComposerController>, index: i32);

        /// Empties the strip.
        #[qinvokable]
        fn clear_attachments(self: Pin<&mut ComposerController>);

        /// Adds what the clipboard holds to the strip: the files copied in a file manager,
        /// else an image; `false` (nothing of either) lets the text area paste text.
        #[qinvokable]
        fn paste(self: Pin<&mut ComposerController>) -> bool;

        /// The text area holds a text (`true`) or not: with files waiting it is their
        /// caption, which the plan of the send depends on.
        #[qinvokable]
        fn set_has_text(self: Pin<&mut ComposerController>, has_text: bool);

        /// What the text area holds, told when it rests for a moment while files wait: a
        /// caption longer than one message of the chat carries is sent first as several, and
        /// the plan counts them.
        #[qinvokable]
        fn set_caption(self: Pin<&mut ComposerController>, text: &QString);

        /// Chooses how entry `index` of the strip is sent: as the file it is (`true`: a
        /// picture is not re-encoded, a video not shown inline, neither joins an album), or
        /// as what it holds. Only pictures and videos have the choice (`canOriginal`).
        #[qinvokable]
        fn set_original(self: Pin<&mut ComposerController>, index: i32, original: bool);

        /// Sends `text` to `conversation` (as a reply when one is set), or the strip's files
        /// with `text` as their caption: every file the network can take goes, as many
        /// messages as it needs; the others stay in the strip with their reasons. `true`
        /// when accepted.
        #[qinvokable]
        fn send(self: Pin<&mut ComposerController>, conversation: i64, text: &QString) -> bool;

        /// Stores the text of a conversation the composer leaves (empty clears the draft).
        #[qinvokable]
        fn save_draft(self: Pin<&mut ComposerController>, conversation: i64, text: &QString);

        /// The draft of `conversation`.
        #[qinvokable]
        fn draft(self: &ComposerController, conversation: i64) -> QString;

        /// The user is typing (`true`) or stopped (`false`).
        #[qinvokable]
        fn set_typing(self: Pin<&mut ComposerController>, conversation: i64, on: bool);

        /// Hides the notice.
        #[qinvokable]
        fn clear_notice(self: Pin<&mut ComposerController>);

        /// The stored draft of `conversation` became known (restore it into an empty
        /// composer).
        #[qsignal]
        fn draft_loaded(self: Pin<&mut ComposerController>, conversation: i64);

        /// Replies to `message` of `conversation`: shows the reply banner with what is
        /// quoted. `false` when the message cannot be replied to.
        #[qinvokable]
        fn reply_to(self: Pin<&mut ComposerController>, conversation: i64, message: i64) -> bool;

        /// Hides the reply banner.
        #[qinvokable]
        fn cancel_reply(self: Pin<&mut ComposerController>);

        /// Edits `message` of `conversation`: the banner shows, `editStarted(text)` hands its
        /// text (or caption) to the text area, and `send` then replaces it. `false` when the
        /// message cannot be edited (any more).
        #[qinvokable]
        fn start_edit(self: Pin<&mut ComposerController>, conversation: i64, message: i64) -> bool;

        /// Edits the newest own message of `conversation` that can be edited (Up in an empty
        /// composer).
        #[qinvokable]
        fn edit_last(self: Pin<&mut ComposerController>, conversation: i64) -> bool;

        /// Leaves edit mode without changing the message.
        #[qinvokable]
        fn cancel_edit(self: Pin<&mut ComposerController>);

        /// Edit mode began: the text area takes `text`.
        #[qsignal]
        fn edit_started(self: Pin<&mut ComposerController>, text: QString);

        /// Edit mode ended (sent or cancelled): the text area takes back what it held before.
        #[qsignal]
        fn edit_finished(self: Pin<&mut ComposerController>);
    }

    #[auto_cxx_name]
    extern "RustQt" {
        /// The generic login page state (one login step at a time).
        #[qobject]
        #[qml_element]
        #[qml_uncreatable]
        #[qproperty(QString, network)]
        #[qproperty(QString, kind)]
        #[qproperty(QString, title)]
        #[qproperty(QString, hint)]
        #[qproperty(QString, qr)]
        #[qproperty(QString, code)]
        #[qproperty(f64, progress)]
        #[qproperty(QString, error)]
        #[qproperty(bool, retry)]
        #[qproperty(QString, leave)]
        #[qproperty(QVariant, fields)]
        type LoginController = super::LoginRust;

        /// Starts linking an account on `network` ("Signal").
        #[qinvokable]
        fn start(self: Pin<&mut LoginController>, network: &QString);

        /// Sets the value of an input field of the current step.
        #[qinvokable]
        fn set_field(self: Pin<&mut LoginController>, id: &QString, value: &QString);

        /// Answers the current input step with the field values.
        #[qinvokable]
        fn submit(self: Pin<&mut LoginController>);

        /// Abandons the login.
        #[qinvokable]
        fn cancel(self: Pin<&mut LoginController>);
    }

    #[auto_cxx_name]
    extern "RustQt" {
        /// App preferences (§8.9), saved in `kourier.toml` and applied by the desktop
        /// integration (tray, autostart entry, notification content).
        #[qobject]
        #[qml_element]
        #[qml_uncreatable]
        #[qproperty(bool, tray, READ, WRITE = set_tray_pref, NOTIFY = changed)]
        #[qproperty(bool, autostart, READ, WRITE = set_autostart_pref, NOTIFY = changed)]
        #[qproperty(bool, start_hidden, READ, WRITE = set_start_hidden_pref, NOTIFY = changed)]
        #[qproperty(bool, compact, READ, WRITE = set_compact_pref, NOTIFY = changed)]
        #[qproperty(bool, round_avatars, READ, WRITE = set_round_avatars_pref, NOTIFY = changed)]
        #[qproperty(f64, avatar_corner, READ, WRITE = set_avatar_corner_pref, NOTIFY = changed)]
        #[qproperty(i32, notification_content, READ, WRITE = set_notification_content_pref, NOTIFY = changed)]
        #[qproperty(QString, expiry_policy, READ, WRITE = set_expiry_policy_pref, NOTIFY = changed)]
        type SettingsController = super::SettingsRust;

        /// The settings page is shown: reads what the core keeps (`expiryPolicy`).
        #[qinvokable]
        fn load(self: Pin<&mut SettingsController>);

        /// What happens to disappearing messages whose time has come while this computer's
        /// clock is unconfirmed: `hide`, `keep` or `erase` (empty until the core has said).
        fn set_expiry_policy_pref(self: Pin<&mut SettingsController>, value: &QString);

        /// A preference changed.
        #[qsignal]
        fn changed(self: Pin<&mut SettingsController>);

        /// Shows Kourier in the tray (closing the window keeps it running there).
        fn set_tray_pref(self: Pin<&mut SettingsController>, value: bool);

        /// Starts Kourier with the session.
        fn set_autostart_pref(self: Pin<&mut SettingsController>, value: bool);

        /// Autostarts hidden in the tray.
        fn set_start_hidden_pref(self: Pin<&mut SettingsController>, value: bool);

        /// The compact conversation list.
        fn set_compact_pref(self: Pin<&mut SettingsController>, value: bool);

        /// Avatars are circles; off, squares with the theme's corners.
        fn set_round_avatars_pref(self: Pin<&mut SettingsController>, value: bool);

        /// The corner of a square avatar in the theme, as a share of the side of a list
        /// avatar. QML sets it from the theme; notification avatars (drawn in Rust, which
        /// cannot ask the theme) follow it. Not saved.
        fn set_avatar_corner_pref(self: Pin<&mut SettingsController>, value: f64);

        /// Notification content: 0 name and message, 1 name only, 2 nothing.
        fn set_notification_content_pref(self: Pin<&mut SettingsController>, value: i32);
    }

    // Constructors used by `App` (and tests) to own its sub-objects.
    #[namespace = "rust::cxxqtlib1"]
    unsafe extern "C++" {
        include!("cxx-qt-lib/common.h");

        #[cxx_name = "make_unique"]
        #[doc(hidden)]
        fn new_account_list_model() -> UniquePtr<AccountListModel>;
        #[cxx_name = "make_unique"]
        #[doc(hidden)]
        fn new_conversation_list_model() -> UniquePtr<ConversationListModel>;
        #[cxx_name = "make_unique"]
        #[doc(hidden)]
        fn new_timeline_model() -> UniquePtr<TimelineModel>;
        #[cxx_name = "make_unique"]
        #[doc(hidden)]
        fn new_attachment_list_model() -> UniquePtr<AttachmentListModel>;
        #[cxx_name = "make_unique"]
        #[doc(hidden)]
        fn new_composer_controller() -> UniquePtr<ComposerController>;
        #[cxx_name = "make_unique"]
        #[doc(hidden)]
        fn new_login_controller() -> UniquePtr<LoginController>;
        #[cxx_name = "make_unique"]
        #[doc(hidden)]
        fn new_settings_controller() -> UniquePtr<SettingsController>;
    }
}

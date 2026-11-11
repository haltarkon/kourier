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
pub use crate::models::media::MediaRust;
pub use crate::models::members::MembersRust;
pub use crate::models::share::ShareTargetsRust;
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
        include!("cxx-qt-lib/qurl.h");
        /// Qt URL (what Qt hands a URL handler).
        type QUrl = cxx_qt_lib::QUrl;
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

        /// The images of `window` that are loading, in words (name, parent, visible or hidden,
        /// size, `source`, `sourceSize` of each, joined by "; "): what a render that did not
        /// settle was waiting for.
        ///
        /// # Safety
        ///
        /// `window` must be null or a live `QObject`.
        #[cxx_name = "loadingImages"]
        unsafe fn loading_images(window: *mut QObject) -> QString;

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

        /// Recolors the links of a `TextEdit.textDocument` with `color`; a mention (the link
        /// `kourier:mention/<i>`) keeps the look of the text, `ink`, without an underline.
        ///
        /// # Safety
        ///
        /// `document` must be null or a live `QObject`.
        #[cxx_name = "colorLinks"]
        unsafe fn color_links_of(document: *mut QObject, color: &QColor, ink: &QColor);

        /// How the links of `html` look after `colorLinks`, one line per link: the href, the
        /// foreground as `#rrggbb`, `underline` and `bold` as 0 or 1 (tests).
        #[cxx_name = "richTextLinkLooks"]
        fn rich_text_link_looks(html: &QString, color: &QColor, ink: &QColor) -> QString;

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

        /// Makes every `QDesktopServices::openUrl` of the process with an http, https, mailto or
        /// file URL (Kirigami's own components call `Qt.openUrlExternally`) go to the slot
        /// `openExternalUrl(QUrl)` of `receiver` (the app).
        ///
        /// # Safety
        ///
        /// `receiver` must be a live `QObject` with that slot; Qt thread.
        #[cxx_name = "registerUrlHandlers"]
        unsafe fn register_url_handlers(receiver: *mut QObject);

        /// `url` as the program that opens it takes it (`QUrl::FullyEncoded`).
        #[cxx_name = "urlForTheOpener"]
        fn url_for_the_opener(url: &QUrl) -> QString;
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

        /// Registers `image://kourierart`: the app's own drawings, in a color of the request.
        #[cxx_name = "installArtProvider"]
        fn install_art_provider(engine: Pin<&mut QQmlApplicationEngine>);

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

        /// Test probe of the order in which a response's `finished` reaches Qt's pixmap reader
        /// (`raceImageFinishes` in `cpp/imageprovider.h`): 0 off; 1 a request returns only once
        /// its worker is done; 2 the same, and a `finished` emitted on a thread other than the
        /// response's is held inside Qt's emission until the reader has connected and checked
        /// (the order that loses it). False when 2 cannot be armed.
        #[cxx_name = "raceImageFinishes"]
        fn race_image_finishes(how: i32) -> bool;

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
        #[qproperty(QString, current_title_short)]
        #[qproperty(bool, blend)]
        #[qproperty(bool, all_shown)]
        #[qproperty(i32, shown_count)]
        #[qproperty(i32, hidden_unread)]
        #[qproperty(QVariant, banner_shown_transfer)]
        #[qproperty(bool, start_hidden, READ, CONSTANT)]
        #[qproperty(i32, release_delay_ms, READ, CONSTANT)]
        #[qproperty(bool, trace_frame)]
        #[qproperty(QString, perf_scenario, READ, CONSTANT)]
        #[qproperty(i32, tray_status)]
        #[qproperty(i32, unread_count)]
        #[qproperty(bool, archive_shown)]
        #[qproperty(i32, archived_count)]
        #[qproperty(QString, banner_kind)]
        #[qproperty(QString, banner_text)]
        #[qproperty(QString, banner_action)]
        #[qproperty(QString, banner_explanation)]
        #[qproperty(QString, banner_appeal_url)]
        #[qproperty(QString, banner_appeal_label)]
        #[qproperty(i64, banner_account)]
        #[qproperty(QVariant, banner_transfer)]
        #[qproperty(QVariant, history_accounts)]
        #[qproperty(QVariant, account_actions)]
        #[qproperty(QVariant, revisions)]
        #[qproperty(QVariant, person)]
        #[qproperty(QVariant, group)]
        #[qproperty(QVariant, safety)]
        #[qproperty(QVariant, networks, READ, CONSTANT)]
        #[qproperty(QString, networks_note, READ, CONSTANT)]
        #[qproperty(QString, privacy_note, READ, CONSTANT)]
        #[qproperty(*mut AccountListModel, accounts, READ, CONSTANT)]
        #[qproperty(*mut MemberListModel, group_members, READ, CONSTANT)]
        #[qproperty(*mut MemberListModel, group_preview, READ, CONSTANT)]
        #[qproperty(*mut ShareTargetListModel, share_targets, READ, CONSTANT)]
        #[qproperty(*mut MediaListModel, group_media, READ, CONSTANT)]
        #[qproperty(*mut MediaListModel, person_media, READ, CONSTANT)]
        #[qproperty(*mut MediaListModel, media_grid, READ, CONSTANT)]
        #[qproperty(*mut MediaListModel, media_files, READ, CONSTANT)]
        #[qproperty(*mut ConversationListModel, conversations, READ, CONSTANT)]
        #[qproperty(*mut TimelineModel, timeline, READ, CONSTANT)]
        #[qproperty(*mut ComposerController, composer, READ, CONSTANT)]
        #[qproperty(*mut LoginController, login, READ, CONSTANT)]
        #[qproperty(*mut SettingsController, settings, READ, CONSTANT)]
        type App = super::AppRust;

        /// A click on an account: the list shows only its conversations (`-1` = all chats,
        /// every account shown).
        #[qinvokable]
        fn select_account(self: Pin<&mut App>, account: i64);

        /// Shift+click, Ctrl+click, Ctrl+Space, the box or the menu item: adds the account to
        /// the accounts the list shows or takes it out (with `blend` off: as a click). Returns
        /// false when nothing changed because the account is the last one shown ("At least
        /// one account stays in the list").
        #[qinvokable]
        fn toggle_account(self: Pin<&mut App>, account: i64) -> bool;

        /// Shows the archive (the archived chats of the accounts shown, under the title
        /// "Archived Chats") or the chat list again. Showing the archive empties the search.
        #[qinvokable]
        fn show_archive(self: Pin<&mut App>, shown: bool);

        /// A chat was picked from the list (or the quick switch): when it is an archived chat
        /// that a search found, shows the archive with the chat in it.
        #[qinvokable]
        fn reveal_chat(self: Pin<&mut App>, conversation: i64);

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

        /// Kourier was asked to start again ("Restart Now", "Turn On and Restart", the banner's
        /// "Turn On"): the window quits, and the process starts anew once the core is down.
        #[qsignal]
        fn restart_requested(self: Pin<&mut App>);

        /// The warning to show before a NEW account of `kind` is linked
        /// (`title`, `paragraphs`, `confirm`, `accept`, `cancel`, `acceptHint`, `restartNote`);
        /// an empty map for a network that asks nothing first.
        #[qinvokable]
        fn network_risk(self: &App, kind: &QString) -> QVariant;

        /// Turns the experimental network `kind` on in the preferences (it takes effect at
        /// the next start) and restarts Kourier; with `link` the link page of that network
        /// opens right after the restart, once ("Turn On and Restart").
        #[qinvokable]
        fn turn_on_network_and_restart(self: Pin<&mut App>, kind: &QString, link: bool);

        /// The network whose link page opens now (the one-shot request "Turn On and Restart"
        /// left for this start), or empty; asking takes it: it counts once per start, also when
        /// the window is made again.
        #[qinvokable]
        fn take_pending_link(self: Pin<&mut App>) -> QString;

        /// Restarts Kourier ("Restart Now"): the window quits and the process starts anew.
        #[qinvokable]
        fn restart_now(self: Pin<&mut App>);

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

        /// Asks the name `attachment` is offered under in the "Save As" dialog (F3b): answered
        /// with `saveSuggested(attachment, url)`, or, when the file is gone, with a failed
        /// `saveFinished`. `false`: nothing was asked.
        #[qinvokable]
        fn ask_save(self: Pin<&mut App>, attachment: i64) -> bool;

        /// Saves `attachment` as the file `url` (the dialog's answer, which asked about replacing),
        /// answered with `saveFinished`. Off the Qt thread: the core writes the copy.
        #[qinvokable]
        fn save_to(self: Pin<&mut App>, attachment: i64, url: &QString);

        /// Saves the attachments `attachments` (their numbers, comma separated) into the folder
        /// `folder` (a URL), each under the name it is offered with, never replacing a file;
        /// ONE `saveFinished` when the last has ended.
        #[qinvokable]
        fn save_all(self: Pin<&mut App>, attachments: &QString, folder: &QString);

        /// Asks the folder URL the folder dialog of "Save All…" starts in (the last folder a save
        /// wrote into, else Downloads, else the home folder): answered with `saveFolderReady`.
        /// The file system is looked at off the Qt thread.
        #[qinvokable]
        fn ask_save_folder(self: Pin<&mut App>);

        /// The QML file that is the "Save As" dialog (a debug build can name another one in
        /// `KOURIER_DEV_SAVE_DIALOG`; a release build always answers its own).
        #[qinvokable]
        fn save_dialog_source(self: &App) -> QString;

        /// "Show in Folder": the file manager shows the saved file `url` selected, else its
        /// folder opens.
        #[qinvokable]
        fn show_in_folder(self: &App, url: &QString);

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

        /// The message the composer's notice offers "Delete for Everyone" for (the notice
        /// "Already sent": the X came after the message went out); 0 for every other notice.
        #[qinvokable]
        fn notice_message(self: &App) -> i64;

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

        /// Sets the disappearing-message timer of conversation `conversation` (`0` turns it
        /// off), whichever chat is open: the details panel's pages name their own chat (a
        /// member's 1:1 chat, the group). A value the chat does not take is refused by the core.
        #[qinvokable]
        fn set_timer_of(self: Pin<&mut App>, conversation: i64, seconds: i32);

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

        /// Archives (`on`) or unarchives the conversation `conversation` (any chat, open or not;
        /// the chat's menu and the keys). The notice ("Chat archived") comes as `chatTold`, once
        /// the core's row says so. Nothing for `conversation` 0 or less.
        #[qinvokable]
        fn set_archived(self: Pin<&mut App>, conversation: i64, on: bool);

        /// Pins (`on`) or unpins `conversation`.
        #[qinvokable]
        fn set_pinned(self: Pin<&mut App>, conversation: i64, on: bool);

        /// Marks `conversation` unread (`on`) or removes the mark. On the open chat the mark
        /// stays until a real reading.
        #[qinvokable]
        fn set_marked_unread(self: Pin<&mut App>, conversation: i64, on: bool);

        /// Takes an archive back (the notice's "Undo"): unarchives `conversation` and, when the
        /// archive took a pin with it (`wasPinned`), pins it again. Leaves no notice.
        #[qinvokable]
        fn undo_archive(self: Pin<&mut App>, conversation: i64, was_pinned: bool);

        /// Ends the mute of `conversation`.
        #[qinvokable]
        fn unmute(self: Pin<&mut App>, conversation: i64);

        /// Mutes `conversation` for `choice`: `"hour"`, `"eight"`, `"day"`, `"week"` or
        /// `"always"` (the time is computed in Rust from the source's clock). Any other word does
        /// nothing.
        #[qinvokable]
        fn mute_for(self: Pin<&mut App>, conversation: i64, choice: &QString);

        /// Mutes `conversation` until the local date and time given ("Until…"). `false` when
        /// the time is not allowed (it ends inside the next minute, or it is no date); nothing is
        /// sent then.
        #[qinvokable]
        fn mute_until(
            self: Pin<&mut App>,
            conversation: i64,
            year: i32,
            month: i32,
            day: i32,
            hour: i32,
            minute: i32,
        ) -> bool;

        /// Whether `muteUntil` would take this local date and time now (the dialog's Mute button
        /// follows it).
        #[qinvokable]
        fn mute_until_allowed(
            self: &App,
            year: i32,
            month: i32,
            day: i32,
            hour: i32,
            minute: i32,
        ) -> bool;

        /// Where the "Until…" dialog starts, tomorrow at 8:00 on the local clock: a map of `year`,
        /// `month`, `day`, `hour` and `minute`.
        #[qinvokable]
        fn mute_until_start(self: &App) -> QVariant;

        /// Turns the account's switch for writing its chats' archive, pin, mute and unread marks
        /// to its other devices on or off (Settings → Accounts). A refusal is told as
        /// `chatTold("switch-refused", …)`.
        #[qinvokable]
        fn set_roaming_writes(self: Pin<&mut App>, account: i64, on: bool);

        /// What the menu of `conversation` offers of Archive, Pin, Mute and Mark as Unread: a
        /// map of `shown`, `on`, `archived`, `pinned`, `pinAtLimit`, `pinWhy`, `markedUnread`,
        /// `hasUnread`, `muteOffered`, `muted`, `mutedLabel` and `why` (the chat's own account's
        /// statement, in the list or not). An empty map for `conversation` 0 or less.
        #[qinvokable]
        fn chat_offer(self: &App, conversation: i64) -> QVariant;

        /// A change of a chat or of the sync switch left a short notice, told once: `kind` is
        /// `"archived"`, `"unarchived"`, `"marked-unread"`, `"refused"` (a change of the chat was refused) or
        /// `"switch-refused"`, `conversation` the chat (0 for the switch's), `wasPinned` whether an archive took a pin with it (Undo
        /// pins the chat again), `text` the words. Emitted after the list has followed the change
        /// and an open chat that archived has closed.
        #[qsignal]
        fn chat_told(
            self: Pin<&mut App>,
            kind: QString,
            conversation: i64,
            was_pinned: bool,
            text: QString,
        );

        /// Removes one attachment of a message of the open conversation for this account.
        /// The caller asked first.
        #[qinvokable]
        fn delete_attachment(self: Pin<&mut App>, attachment: i64);

        /// What the picture viewer steps through for `message` of the open conversation, as
        /// the rows are now (see `models::timeline::viewer_items`): a list, empty when the
        /// message has no pictures (any more) AND when it is not among the loaded rows (never
        /// `undefined`: ask `viewerMessageLoaded` to tell the two apart). The viewer asks again
        /// at `timelineRowsChanged`.
        #[qinvokable]
        fn viewer_items(self: &App, message: i64) -> QVariant;

        /// Whether `message` is among the loaded rows of the open conversation (a viewer that
        /// follows a message of the timeline stays as it is when the window moved on).
        #[qinvokable]
        fn viewer_message_loaded(self: &App, message: i64) -> bool;

        /// What the picture viewer steps through for `message` as the gallery slot `slot` (`group`,
        /// `person`, `media`) has its rows now: every picture of the message the slot holds, in the
        /// shape of `viewerItems`; an empty list when the slot holds none of them (any more). A
        /// viewer opened from a square follows the message through this, not through the open
        /// timeline (the square may belong to another chat). It asks again at `galleryChanged`.
        #[qinvokable]
        fn gallery_viewer_items(self: &App, slot: &QString, message: i64) -> QVariant;

        /// Whether the gallery slot `slot` has had its first answer since it was opened.
        #[qinvokable]
        fn gallery_loaded(self: &App, slot: &QString) -> bool;

        /// Opens the picture viewer's own query (slot `viewer`) for the pictures of `message`, a
        /// message among the rows of the slot `from` (`group`, `person`, `media`): the whole album
        /// read AT the message, kept up to date as its chat changes; the viewer follows it through
        /// `galleryViewerItems("viewer", message)` at `galleryChanged("viewer")`, and closes only
        /// when the message has no picture left. Any earlier answer is dropped. `false` when `from`
        /// holds no row of the message. `closeChatMedia("viewer")` ends it.
        #[qinvokable]
        fn show_gallery_viewer(self: Pin<&mut App>, from: &QString, message: i64) -> bool;

        /// The rows of the gallery slot `slot` changed (an answer arrived; not when it was closed).
        #[qsignal]
        fn gallery_changed(self: Pin<&mut App>, slot: QString);

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

        /// Puts text that is on screen on the clipboard and says so (`copyTold`): a name, a
        /// number or an ID of a page.
        #[qinvokable]
        fn copy_shown(self: Pin<&mut App>, text: &QString);

        /// "Copy Number or ID" (`part` "number") or "Copy Link" (`part` "link") of a member's
        /// row. Rust finds it and copies it (the link never passes through QML); `copyTold` says
        /// what came of it. `false`: nothing was asked.
        #[qinvokable]
        fn copy_person(self: Pin<&mut App>, part: &QString, account: i64, user: &QString) -> bool;

        /// The same for the other side of a direct chat (a chat's row).
        #[qinvokable]
        fn copy_chat(self: Pin<&mut App>, part: &QString, conversation: i64) -> bool;

        /// The same for an account itself (an account's row).
        #[qinvokable]
        fn copy_account(self: Pin<&mut App>, part: &QString, account: i64) -> bool;

        /// An action on the invite link of a group's page (`action` "read", "reset", "on" or
        /// "off"): Rust asks the network and copies the link (it never passes through QML);
        /// `copyTold` says what came of it. `false`: nothing was asked.
        #[qinvokable]
        fn group_invite(self: Pin<&mut App>, action: &QString, conversation: i64) -> bool;

        /// "Copy Link" of the person's page on screen (P5b): Rust copies the link it holds for
        /// that page (the URL never passes through QML). `false` when there is none.
        #[qinvokable]
        fn copy_person_link(self: &App) -> bool;

        /// "Share Contact…" of the person's page on screen (P5c): fills `shareTargets` with the
        /// chats of that person's account a card may go to (the search emptied). `false` when no
        /// person's page offers a card now.
        #[qinvokable]
        fn prepare_share(self: Pin<&mut App>) -> bool;

        /// "Send" of the share picker (P5c): a contact card of the person on screen into the
        /// listed chat `conversation`. Rust holds who; the core builds the card and checks it.
        /// `false` when nothing was sent (no person, the chat is not listed, the core stopped).
        #[qinvokable]
        fn share_contact(self: Pin<&mut App>, conversation: i64) -> bool;

        /// Opens `url` in the person's browser or in the application a file opens in, with the
        /// environment the person gave Kourier. Whether a program was started. This is also the
        /// slot Qt's URL handlers for http, https, mailto and file call (`registerUrlHandlers`),
        /// so `Qt.openUrlExternally` anywhere in the process (Kirigami's About page too) ends
        /// here: a program started by Qt's own services would inherit the variables Kourier sets
        /// for Qt Multimedia. QML passes a string.
        #[qinvokable]
        #[cxx_name = "openExternalUrl"]
        fn open_external_url(self: &App, url: &QUrl) -> bool;

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

        /// Asks for the page of the other person of the open 1:1 chat (the user's own page in
        /// Note to Self): `person` holds it when `personLoaded` comes. `false` when the open
        /// chat has no person (a group, no chat open).
        #[qinvokable]
        fn show_person_of_chat(self: Pin<&mut App>) -> bool;

        /// A copy action is done: the toast's text, which holds no number and no link.
        #[qsignal]
        fn copy_told(self: Pin<&mut App>, text: QString);

        /// The page asked for with `showPersonOfChat` is in `person`. Comes again when the
        /// contact, their chat or their groups change while the page is shown.
        #[qsignal]
        fn person_loaded(self: Pin<&mut App>);

        /// The page can no longer be shown (the chat or the account is gone): the panel closes.
        #[qsignal]
        fn person_ended(self: Pin<&mut App>);

        /// The person's page is no longer on screen: it is not kept up to date any more.
        #[qinvokable]
        fn close_person(self: Pin<&mut App>);

        /// Asks for the page of `user` of `account` from the page of the open group (a member's
        /// row, or "You"): `person` holds it when `personLoaded` comes. The open chat is the
        /// page's origin (it ends with that chat). The map is empty until then. `false` when
        /// the page cannot be asked for (no chat open).
        #[qinvokable]
        fn show_person(self: Pin<&mut App>, account: i64, user: &QString) -> bool;

        /// The same for a person the OPEN CHAT names (a group's sender, a mention; the account is
        /// the chat's): `person` holds the page when `personLoaded` comes, the open chat is its
        /// origin. `false` when no chat is open or `user` is no id.
        #[qinvokable]
        fn show_person_in_chat(self: Pin<&mut App>, user: &QString) -> bool;

        /// Asks for the page of the open group: `group` holds it when `groupLoaded` comes.
        /// `false` when the open chat is not a group (a 1:1 chat, a channel, none open).
        #[qinvokable]
        fn show_group_of_chat(self: Pin<&mut App>) -> bool;

        /// The page asked for with `showGroupOfChat` is in `group`. Comes again when the group,
        /// a member's contact or the accounts change while the page is shown; an answer equal
        /// to the one on screen is not announced.
        #[qsignal]
        fn group_loaded(self: Pin<&mut App>);

        /// The page can no longer be shown (the group or its account is gone): the panel closes.
        #[qsignal]
        fn group_ended(self: Pin<&mut App>);

        /// The group's page is no longer on screen: it is not kept up to date any more.
        #[qinvokable]
        fn close_group(self: Pin<&mut App>);

        /// Asks for the safety number of `user` of `account` (P5a, the panel's "safety" level):
        /// `safety` holds the level when `safetyLoaded` comes (empty until then), and again
        /// whenever it changes while it is shown. `false` when `user` is no id.
        #[qinvokable]
        fn show_safety(self: Pin<&mut App>, account: i64, user: &QString) -> bool;

        /// The level asked for with `showSafety` is in `safety`.
        #[qsignal]
        fn safety_loaded(self: Pin<&mut App>);

        /// The safety-number level is no longer on screen: it is not kept up to date any more,
        /// and its map is emptied.
        #[qinvokable]
        fn close_safety(self: Pin<&mut App>);

        /// A button of the safety-number level was pressed (`action`: `mark`, `clear` or
        /// `confirm`, as the level's `buttons` name them). It acts on the person and the KEY of
        /// the number ON SCREEN, which Rust holds (QML passes nothing else); a button the level
        /// on screen does not offer, or no level, does nothing. `false` when nothing was sent.
        #[qinvokable]
        fn safety_action(self: Pin<&mut App>, action: &QString) -> bool;

        /// Whether a member row named `name` matches what was typed into the members search
        /// (the viewmodel's rule: case does not matter, spaces at the ends of the typed text do
        /// not count, nothing typed matches everyone).
        #[qinvokable]
        fn member_matches(self: &App, name: &QString, query: &QString) -> bool;

        /// Shows the gallery of the chat `conversation` of `account` in the slot named `slot` (`group` and
        /// `person`: the section of that page; `media` and `files`: the grid and the list of the
        /// level of all media). The slot's model is emptied when it showed another chat, and fills
        /// when the core answers; it follows the chat's messages until `closeMedia`. `false` when
        /// the slot is unknown or the gallery cannot be asked for.
        #[qinvokable]
        fn show_chat_media(
            self: Pin<&mut App>,
            slot: &QString,
            account: i64,
            conversation: i64,
        ) -> bool;

        /// Asks for the next page of the slot `media` or `files` (after its last row). A section
        /// never goes on.
        #[qinvokable]
        fn more_chat_media(self: Pin<&mut App>, slot: &QString);

        /// The slot shows nothing any more (`all`: every slot): its model is emptied and it is
        /// not kept up to date.
        #[qinvokable]
        fn close_chat_media(self: Pin<&mut App>, slot: &QString);

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

        /// Gives an account a color of its own (`#rrggbb`); empty: its network's again.
        #[qinvokable]
        fn set_account_tint(self: Pin<&mut App>, account: i64, tint: &QString);

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

        /// The copy for `openExternally` is ready: hand `url` to `openExternalUrl`.
        #[qsignal]
        fn external_ready(self: Pin<&mut App>, attachment: i64, url: QString);

        /// The name `askSave(attachment)` asked for: the dialog starts at `url` (the start folder
        /// and the offered name, built in Rust). Sent once both the name and the folder are known.
        #[qsignal]
        fn save_suggested(self: Pin<&mut App>, attachment: i64, url: QString);

        /// The folder `askSaveFolder()` asked for: the folder dialog starts at `url`.
        #[qsignal]
        fn save_folder_ready(self: Pin<&mut App>, url: QString);

        /// A save or a batch ended: `saved` files were written, `failed` were not; `name` is the
        /// last file's name and `url` its `file:` URL; `reason` is the class of the last failure
        /// ("nospace", "notwritable", "nofolder", "gone", "nothere", "other"), empty when none.
        #[qsignal]
        fn save_finished(
            self: Pin<&mut App>,
            saved: i32,
            failed: i32,
            name: QString,
            url: QString,
            reason: QString,
        );

        /// Gives the links of a message text (`TextEdit.textDocument`) the color `color`.
        ///
        /// # Safety
        ///
        /// `document` must be null or a live `QObject` (QML passes `textDocument`).
        #[qinvokable]
        unsafe fn color_links(self: &App, document: *mut QObject, color: &QColor, ink: &QColor);

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

    // ---- MemberListModel -----------------------------------------------------------------

    #[auto_cxx_name]
    extern "RustQt" {
        /// The member rows of the group's page (`App.groupMembers`, `App.groupPreview`).
        #[qobject]
        #[base = QAbstractListModel]
        #[qml_element]
        #[qml_uncreatable]
        #[qproperty(i32, count)]
        #[qproperty(QString, filter, READ, WRITE = set_filter_text, NOTIFY = filter_changed)]
        type MemberListModel = super::MembersRust;

        #[cxx_override]
        fn data(self: &MemberListModel, index: &QModelIndex, role: i32) -> QVariant;
        #[cxx_override]
        fn row_count(self: &MemberListModel, parent: &QModelIndex) -> i32;
        #[cxx_override]
        fn role_names(self: &MemberListModel) -> QHash_i32_QByteArray;

        /// The text of the members search: only the members that match it are listed.
        fn set_filter_text(self: Pin<&mut MemberListModel>, text: &QString);

        /// The filter changed.
        #[qsignal]
        fn filter_changed(self: Pin<&mut MemberListModel>);

        /// The user id of listed row `row` (empty when there is none).
        #[qinvokable]
        fn user_at(self: &MemberListModel, row: i32) -> QString;

        /// The listed row of member `user` (-1 when the member is not listed).
        #[qinvokable]
        fn row_of(self: &MemberListModel, user: &QString) -> i32;
    }

    #[auto_cxx_name]
    unsafe extern "RustQt" {
        #[inherit]
        unsafe fn begin_insert_rows(
            self: Pin<&mut MemberListModel>,
            parent: &QModelIndex,
            first: i32,
            last: i32,
        );
        #[inherit]
        unsafe fn end_insert_rows(self: Pin<&mut MemberListModel>);
        #[inherit]
        unsafe fn begin_remove_rows(
            self: Pin<&mut MemberListModel>,
            parent: &QModelIndex,
            first: i32,
            last: i32,
        );
        #[inherit]
        unsafe fn end_remove_rows(self: Pin<&mut MemberListModel>);
        #[inherit]
        unsafe fn begin_move_rows(
            self: Pin<&mut MemberListModel>,
            source_parent: &QModelIndex,
            source_first: i32,
            source_last: i32,
            destination_parent: &QModelIndex,
            destination_child: i32,
        ) -> bool;
        #[inherit]
        unsafe fn end_move_rows(self: Pin<&mut MemberListModel>);
        #[inherit]
        unsafe fn begin_reset_model(self: Pin<&mut MemberListModel>);
        #[inherit]
        unsafe fn end_reset_model(self: Pin<&mut MemberListModel>);
        #[inherit]
        fn index(
            self: &MemberListModel,
            row: i32,
            column: i32,
            parent: &QModelIndex,
        ) -> QModelIndex;
        #[inherit]
        #[qsignal]
        fn data_changed(
            self: Pin<&mut MemberListModel>,
            top_left: &QModelIndex,
            bottom_right: &QModelIndex,
            roles: &QVector_i32,
        );
    }

    // ---- ShareTargetListModel -----------------------------------------------------------------

    #[auto_cxx_name]
    extern "RustQt" {
        /// The chats of the share picker (people pages P5c, `App.shareTargets`).
        #[qobject]
        #[base = QAbstractListModel]
        #[qml_element]
        #[qml_uncreatable]
        #[qproperty(i32, count)]
        #[qproperty(QString, filter, READ, WRITE = set_filter_text, NOTIFY = filter_changed)]
        type ShareTargetListModel = super::ShareTargetsRust;

        #[cxx_override]
        fn data(self: &ShareTargetListModel, index: &QModelIndex, role: i32) -> QVariant;
        #[cxx_override]
        fn row_count(self: &ShareTargetListModel, parent: &QModelIndex) -> i32;
        #[cxx_override]
        fn role_names(self: &ShareTargetListModel) -> QHash_i32_QByteArray;

        /// The text of the picker's search: only the chats whose title contains it are listed.
        fn set_filter_text(self: Pin<&mut ShareTargetListModel>, text: &QString);

        /// The filter changed.
        #[qsignal]
        fn filter_changed(self: Pin<&mut ShareTargetListModel>);

        /// The conversation id of listed row `row` (0 when there is none).
        #[qinvokable]
        fn conversation_at(self: &ShareTargetListModel, row: i32) -> i64;

        /// The title of listed chat `conversation` (empty when it is not listed).
        #[qinvokable]
        fn title_of(self: &ShareTargetListModel, conversation: i64) -> QString;

        /// Whether chat `conversation` is listed now.
        #[qinvokable]
        fn is_listed(self: &ShareTargetListModel, conversation: i64) -> bool;
    }

    #[auto_cxx_name]
    unsafe extern "RustQt" {
        #[inherit]
        unsafe fn begin_insert_rows(
            self: Pin<&mut ShareTargetListModel>,
            parent: &QModelIndex,
            first: i32,
            last: i32,
        );
        #[inherit]
        unsafe fn end_insert_rows(self: Pin<&mut ShareTargetListModel>);
        #[inherit]
        unsafe fn begin_remove_rows(
            self: Pin<&mut ShareTargetListModel>,
            parent: &QModelIndex,
            first: i32,
            last: i32,
        );
        #[inherit]
        unsafe fn end_remove_rows(self: Pin<&mut ShareTargetListModel>);
        #[inherit]
        unsafe fn begin_move_rows(
            self: Pin<&mut ShareTargetListModel>,
            source_parent: &QModelIndex,
            source_first: i32,
            source_last: i32,
            destination_parent: &QModelIndex,
            destination_child: i32,
        ) -> bool;
        #[inherit]
        unsafe fn end_move_rows(self: Pin<&mut ShareTargetListModel>);
        #[inherit]
        unsafe fn begin_reset_model(self: Pin<&mut ShareTargetListModel>);
        #[inherit]
        unsafe fn end_reset_model(self: Pin<&mut ShareTargetListModel>);
        #[inherit]
        fn index(
            self: &ShareTargetListModel,
            row: i32,
            column: i32,
            parent: &QModelIndex,
        ) -> QModelIndex;
        #[inherit]
        #[qsignal]
        fn data_changed(
            self: Pin<&mut ShareTargetListModel>,
            top_left: &QModelIndex,
            bottom_right: &QModelIndex,
            roles: &QVector_i32,
        );
    }

    // ---- MediaListModel -------------------------------------------------------------------

    #[auto_cxx_name]
    extern "RustQt" {
        /// The rows of a chat's gallery (`App.groupMedia`, `personMedia`, `mediaGrid`,
        /// `mediaFiles`).
        #[qobject]
        #[base = QAbstractListModel]
        #[qml_element]
        #[qml_uncreatable]
        #[qproperty(i32, count)]
        #[qproperty(bool, more)]
        #[qproperty(bool, other)]
        #[qproperty(bool, loaded)]
        type MediaListModel = super::MediaRust;

        #[cxx_override]
        fn data(self: &MediaListModel, index: &QModelIndex, role: i32) -> QVariant;
        #[cxx_override]
        fn row_count(self: &MediaListModel, parent: &QModelIndex) -> i32;
        #[cxx_override]
        fn role_names(self: &MediaListModel) -> QHash_i32_QByteArray;
    }

    #[auto_cxx_name]
    unsafe extern "RustQt" {
        #[inherit]
        unsafe fn begin_insert_rows(
            self: Pin<&mut MediaListModel>,
            parent: &QModelIndex,
            first: i32,
            last: i32,
        );
        #[inherit]
        unsafe fn end_insert_rows(self: Pin<&mut MediaListModel>);
        #[inherit]
        unsafe fn begin_remove_rows(
            self: Pin<&mut MediaListModel>,
            parent: &QModelIndex,
            first: i32,
            last: i32,
        );
        #[inherit]
        unsafe fn end_remove_rows(self: Pin<&mut MediaListModel>);
        #[inherit]
        unsafe fn begin_move_rows(
            self: Pin<&mut MediaListModel>,
            source_parent: &QModelIndex,
            source_first: i32,
            source_last: i32,
            destination_parent: &QModelIndex,
            destination_child: i32,
        ) -> bool;
        #[inherit]
        unsafe fn end_move_rows(self: Pin<&mut MediaListModel>);
        #[inherit]
        unsafe fn begin_reset_model(self: Pin<&mut MediaListModel>);
        #[inherit]
        unsafe fn end_reset_model(self: Pin<&mut MediaListModel>);
        #[inherit]
        fn index(self: &MediaListModel, row: i32, column: i32, parent: &QModelIndex)
        -> QModelIndex;
        #[inherit]
        #[qsignal]
        fn data_changed(
            self: Pin<&mut MediaListModel>,
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
        #[qproperty(bool, has_details)]
        #[qproperty(bool, is_self)]
        #[qproperty(bool, muted)]
        #[qproperty(QString, avatar_url)]
        #[qproperty(QString, tint)]
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
        #[qproperty(QString, timer_held)]
        #[qproperty(QVariant, chat_offer)]
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

        /// How many upload rings of the rows shown have a figure in them (the fixture pages of
        /// upload progress wait for one before the picture is taken).
        #[qinvokable]
        fn uploads_reported(self: &TimelineModel) -> i32;

        /// Whether the newest own message of the rows shown has left the outbox (the server
        /// took it; not queued, not sending, not failed): the `media` fixture page sends from
        /// the window and takes its picture only then, never with the clock icon.
        #[qinvokable]
        fn newest_own_left_the_outbox(self: &TimelineModel) -> bool;
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
        #[qproperty(bool, location_asking)]
        #[qproperty(QVariant, location_question)]
        type ComposerController = super::ComposerRust;

        /// Answers the question Send asked about files that would leave with where they
        /// were taken (`locationAsking`, `locationQuestion`): `send` (Send with Location)
        /// sends them with the others, `hold` (Don't Send) keeps them in the list and sends
        /// the others with the text, or nothing when no file would go. A close without an
        /// answer (Escape, the close button, a click outside) dismisses: the composer then
        /// answers `dismiss`, which drops the question with nothing sent, held or remembered;
        /// the next Send asks anew with its own text. "Remember my choice" is stored by the
        /// dialog through `App.settings.locationFiles` AFTER an answer that applied (the one
        /// owner of the setting).
        /// Returns whether the answer applied: `false` when what the question was about moved
        /// meanwhile (the answer is stale: nothing was sent or held and the question is asked
        /// again), or for a dismissal. When files were sent, `sentFromQuestion` follows.
        #[qinvokable]
        fn answer_location(self: Pin<&mut ComposerController>, answer: &QString) -> bool;

        /// The setting "Files with location data" changed: the list shows it at once (the
        /// rows' notes, the counts), without waiting for its next change.
        #[qinvokable]
        fn refresh_location_pref(self: Pin<&mut ComposerController>);

        /// An answer to the location question sent the files and the text of that Send: the
        /// text area is cleared now (`send()` returned `false` when it asked).
        #[qsignal]
        fn sent_from_question(self: Pin<&mut ComposerController>);

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
        #[qproperty(QVariant, emoji)]
        #[qproperty(bool, rotates)]
        #[qproperty(QString, flow)]
        #[qproperty(QString, other_flow)]
        #[qproperty(QString, other_flow_label)]
        #[qproperty(QStringList, steps)]
        #[qproperty(QString, explain)]
        #[qproperty(QString, icon)]
        #[qproperty(QString, resend_label)]
        #[qproperty(i32, resend_after_ms)]
        type LoginController = super::LoginRust;

        /// Starts linking an account on `network` ("Signal").
        #[qinvokable]
        fn start(self: Pin<&mut LoginController>, network: &QString);

        /// Switches the page to the other way of linking the same network (`otherFlow`): the
        /// running login stops and the other starts at once, with no question.
        #[qinvokable]
        fn switch_flow(self: Pin<&mut LoginController>);

        /// Sets the value of an input field of the current step.
        #[qinvokable]
        fn set_field(self: Pin<&mut LoginController>, id: &QString, value: &QString);

        /// Answers the current input step with the field values.
        #[qinvokable]
        fn submit(self: Pin<&mut LoginController>);

        /// Asks the network for a new login code (the button `resendLabel` names): sends the
        /// request alone, never the typed fields.
        #[qinvokable]
        fn resend(self: Pin<&mut LoginController>);

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
        #[qproperty(bool, blend_accounts, READ, WRITE = set_blend_accounts_pref, NOTIFY = changed)]
        #[qproperty(bool, round_avatars, READ, WRITE = set_round_avatars_pref, NOTIFY = changed)]
        #[qproperty(bool, tint_rows, READ, WRITE = set_tint_rows_pref, NOTIFY = changed)]
        #[qproperty(bool, chat_wallpaper, READ, WRITE = set_chat_wallpaper_pref, NOTIFY = changed)]
        #[qproperty(f64, avatar_corner, READ, WRITE = set_avatar_corner_pref, NOTIFY = changed)]
        #[qproperty(i32, notification_content, READ, WRITE = set_notification_content_pref, NOTIFY = changed)]
        #[qproperty(i32, location_files, READ, WRITE = set_location_files_pref, NOTIFY = changed)]
        #[qproperty(QString, expiry_policy, READ, WRITE = set_expiry_policy_pref, NOTIFY = changed)]
        #[qproperty(QString, quiet_notices, READ, WRITE = set_quiet_notices_pref, NOTIFY = changed)]
        #[qproperty(QStringList, networks_on, READ, NOTIFY = changed)]
        type SettingsController = super::SettingsRust;

        /// Turns the experimental network `kind` on or off in the preferences. Nothing runs
        /// differently until Kourier starts again (the core has no runtime switch).
        #[qinvokable]
        fn set_network_on(self: Pin<&mut SettingsController>, kind: &QString, on: bool);

        /// How many accounts of the network `kind` exist now (the question before turning
        /// the network off is asked only when it is not 0).
        #[qinvokable]
        fn accounts_of(self: &SettingsController, kind: &QString) -> i32;

        /// The settings page is shown: reads what the core keeps (`expiryPolicy`,
        /// `quietNotices`).
        #[qinvokable]
        fn load(self: Pin<&mut SettingsController>);

        /// What happens to disappearing messages whose time has come while this computer's
        /// clock is unconfirmed: `hide`, `keep` or `erase` (empty until the core has said).
        fn set_expiry_policy_pref(self: Pin<&mut SettingsController>, value: &QString);

        /// Whether quiet notices (a safety number that changed, a refreshed session, a
        /// changed number, an accepted request) count for the conversation list: `hide` (the
        /// default: the list shows the last real message) or `show` (empty until the core
        /// has said).
        fn set_quiet_notices_pref(self: Pin<&mut SettingsController>, value: &QString);

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

        /// "Show chats of several accounts together": on, Shift/Ctrl+click adds an account to
        /// the list; off, a click switches the account (turning it off with several shown
        /// shows all chats).
        fn set_blend_accounts_pref(self: Pin<&mut SettingsController>, value: bool);

        /// Avatars are circles; off, squares with the theme's corners.
        fn set_round_avatars_pref(self: Pin<&mut SettingsController>, value: bool);

        /// Chat rows carry a faint wash of their account's color.
        fn set_tint_rows_pref(self: Pin<&mut SettingsController>, value: bool);

        /// The pattern of small drawings behind a chat's messages.
        fn set_chat_wallpaper_pref(self: Pin<&mut SettingsController>, value: bool);

        /// The corner of a square avatar in the theme, as a share of the side of a list
        /// avatar. QML sets it from the theme; notification avatars (drawn in Rust, which
        /// cannot ask the theme) follow it. Not saved.
        fn set_avatar_corner_pref(self: Pin<&mut SettingsController>, value: f64);

        /// Notification content: 0 name and message, 1 name only, 2 nothing.
        fn set_notification_content_pref(self: Pin<&mut SettingsController>, value: i32);

        /// Files with location data: 0 ask each time, 1 always send, 2 never send.
        fn set_location_files_pref(self: Pin<&mut SettingsController>, value: i32);
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
        fn new_member_list_model() -> UniquePtr<MemberListModel>;
        #[cxx_name = "make_unique"]
        #[doc(hidden)]
        fn new_share_target_list_model() -> UniquePtr<ShareTargetListModel>;
        #[cxx_name = "make_unique"]
        #[doc(hidden)]
        fn new_media_list_model() -> UniquePtr<MediaListModel>;
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

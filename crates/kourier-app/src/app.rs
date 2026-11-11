//! Starting and stopping Kourier (§2.5, §8.2): the tokio runtime, paths, the master key, the
//! network registry, the core; the [`AppHandle`] the UI talks through; orderly shutdown.

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{BackendConfig, Clock, NetworkRegistry, SystemClock};
use kourier_core::facade::{CoreHandle, Query, QueryData, UiCommand, UiEvent};
use kourier_core::{Core, CoreConfig, CoreError, Timeouts};
use kourier_desktop::secret::SecretService;
use kourier_model::{ConversationId, LoginFlow, NetworkKind, RiskNotice};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::config::{self, ConfigError};
use crate::desktop::{self, Connected, DesktopHandle, DesktopOptions};
use crate::keys::{KeyError, Keyring};
use crate::paths::{PathError, Paths, Profile};
use crate::prefs::Prefs;
use crate::vault::{AnyKeyring, AppVault};

/// What the application adds to the largest stop grace of the registered networks to get the time
/// the core may take to stop its accounts at shutdown before they are cancelled
/// ([`core_shutdown_timeout`]): the writes that finish and `core.db` closing after the last actor.
pub const CORE_SHUTDOWN_MARGIN: Duration = Duration::from_secs(5);
/// How long the runtime may take to finish its tasks after the core stopped (§8.2 step 5).
pub const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// How long closing notifications and the tray may take at shutdown.
pub const DESKTOP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the core may take to stop its accounts at shutdown before they are cancelled (R-Q1):
/// the largest stop grace of the registered networks (what a network states in its descriptor,
/// else the core's default of 5 s) plus [`CORE_SHUTDOWN_MARGIN`]. Signal alone: 10 s, as it
/// always was; with WhatsApp (20 s): 25 s. A quit with nothing in flight takes no longer than
/// before: this is only the bound.
pub fn core_shutdown_timeout(registry: &NetworkRegistry) -> Duration {
    let timeouts = Timeouts::default();
    let largest = registry
        .iter()
        .map(|n| timeouts.grace_of(n.descriptor().shutdown_grace))
        .max()
        .unwrap_or(timeouts.shutdown_grace);
    largest.saturating_add(CORE_SHUTDOWN_MARGIN)
}

/// Why Kourier could not start. Every variant is log-safe and fit to show the user.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// The tokio runtime could not be built.
    #[error("cannot start the runtime: {0}")]
    Runtime(std::io::Error),
    /// The data directories cannot be located.
    #[error(transparent)]
    Paths(#[from] PathError),
    /// The data directories cannot be created.
    #[error("cannot create the data directories: {0}")]
    Dirs(std::io::Error),
    /// No usable Secret Service. The passphrase dialog (§5.2) is not implemented yet.
    #[error(
        "no usable Secret Service ({0}); Kourier keeps its database key there. Unlock or install a Secret Service provider (KWallet or GNOME Keyring); passphrase mode is not available yet"
    )]
    NoSecretService(KeyError),
    /// The master key or the core.db key could not be loaded.
    #[error(transparent)]
    Keys(#[from] KeyError),
    /// The backend environment from `KOURIER_SIGNAL_ENV` is unusable.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The core did not start (core.db, migrations, reconciliation).
    #[error("cannot open the message store: {0}")]
    Core(#[from] CoreError),
    /// Fixture mode needs the `fake` cargo feature.
    #[error("fixture mode needs a build with the `fake` feature")]
    FixtureUnavailable,
    /// Fixture mode could not seed its data.
    #[error("fixture mode: {0}")]
    Fixture(String),
    /// Another process holds this profile's lock ([`crate::paths::ProfileLock`]) and could not
    /// be asked to show its window (no shared session bus). Nothing was opened or changed.
    #[error(
        "another Kourier process is using this profile; stop it first (two processes on one profile would break its sessions)"
    )]
    ProfileInUse,
    /// Kourier already runs for this profile; it was asked to show its window (or open the
    /// URIs; a `--background` launch asks nothing). The launch should exit successfully.
    #[error("Kourier is already running for this profile")]
    AlreadyRunning,
    /// The running instance could not be reached.
    #[error("Kourier is already running but did not answer: {0}")]
    Instance(kourier_desktop::DesktopError),
}

/// What [`App::start`] runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[expect(
    clippy::large_enum_variant,
    reason = "made once per start; the fixture options are a plain list of switches that grows with each fixture page"
)]
pub enum Launch {
    /// The user's profile with the compiled-in networks.
    #[default]
    Normal,
    /// The user's profile like [`Launch::Normal`], driven by `--dev-control` (a scripted start: the
    /// demo, the smoke and run tools, an agent's own run). Threema is absent in such a start unless
    /// the run tool made it (`prefs::ThreemaStart`).
    Scripted,
    /// A throwaway profile in a temporary `KOURIER_HOME` with only the fake network (§10.2).
    Fixture(FixtureOptions),
    /// The reference profile of the quality targets (§1.3) in a directory of its own, with
    /// only the fake network; an empty one is seeded first ([`crate::perf`]). For
    /// `cargo xtask perf`.
    #[cfg(feature = "perf")]
    Perf(crate::perf::PerfOptions),
}

/// Fixture mode settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixtureOptions {
    /// Start without an account (the welcome page) and link the fake account through its
    /// rotating QR codes; otherwise the account is linked and its data ingested before the UI
    /// starts.
    pub link: bool,
    /// How long each QR code of the linking walk stays valid.
    pub qr_valid_for: Duration,
    /// QR codes shown before the phone "scans" the last one.
    pub qr_codes: u32,
    /// Messages per fixture conversation.
    pub messages_per_conversation: usize,
    /// Generated contacts, each with a direct chat (the fixture's own 12 by default; a test
    /// that needs a list of about 200 chats asks for more).
    pub contacts: usize,
    /// Generated groups (4 by default).
    pub groups: usize,
    /// Adds the showcase chats on top of the generated ones: quotes of a known, a missing and
    /// a deleted message with an edited photo caption, a chat with a disappearing-message
    /// timer, and a chat with stand-ins for content Kourier does not show yet.
    pub showcase: bool,
    /// The state the account ends in once its data is delivered (the account banners).
    pub state: FixtureState,
    /// With the showcase chats: one more received and read message in the timer chat,
    /// [`EXPIRING_TEXT`], that disappears this long after it was delivered, a few seconds
    /// into the run (tests of what happens to a message whose time is up). `None`: no such
    /// message.
    pub expiring: Option<Duration>,
    /// With the showcase chats, a linked start and [`FixtureState::Online`]: the peer of the
    /// timer chat keeps writing after the start (a chat that receives while it is read).
    /// `None`: nothing arrives.
    pub arrivals: Option<Arrivals>,
    /// With the showcase chats: the missed call of the chat with calls is unread
    /// (`--page=calls`; in every other run it is read, so no other picture counts it).
    pub calls_unread: bool,
    /// With the showcase chats: a Note to Self chat is added (the tests of the user's own page;
    /// it is a chat more, so a fixture page never sets it).
    pub note_to_self: bool,
    /// With the showcase chats: how many groups the person of `--page=person` shares with the
    /// account (the tests of "Show All"; a fixture page leaves it 0 = three, because more
    /// groups would move the members of the groups other pages draw).
    pub person_groups: u8,
    /// With the showcase chats: how many members the group of `--page=group` has (the tests of
    /// its member list: "Show All", the search, a long list, a refresh of a long list; a
    /// fixture page leaves it 0 = the group as it is, because more members would change a
    /// group other pages draw). Two of the added members are made up: one with a contact row
    /// whose name is markup, one without a contact row.
    pub group_members: u16,
    /// With the showcase chats: the group of `--page=group` offers the disappearing timers and
    /// lets every member change them (the measurement of a refresh of a long list changes the
    /// group's timer, the only change a command can make to the page; a fixture page leaves
    /// it off, its picture shows the read-only value).
    pub group_timers: bool,
    /// With the showcase chats: the account is an administrator of the group of `--page=group`,
    /// so the page offers the invite link (`--page=groupadmin` and the tests of the link; the
    /// page of a plain member has no such part).
    pub group_admin: bool,
    /// With the showcase chats: seven messages are added to the group of `--page=group`, two members
    /// write them (a run of two by one of them among them) and four mention each other and the
    /// account, one under a spoiler, one inside a link, one beside a forged mention link (the tests of the senders and mentions that open a
    /// person; a fixture page leaves it off, the group would become the newest chat).
    pub group_mentions: bool,
    /// With the showcase chats: the safety number of the person of `--page=person` is in state
    /// Changed (verified once, then her key changed) instead of Default (`--page=safetychanged`).
    pub safety_changed: bool,
    /// With the showcase chats: a second person with a 1:1 chat gets safety numbers in state
    /// Changed (two keys: the first answer shows the first, the fake holds the second at once)
    /// and the first send to her fails `UntrustedIdentity`, so her chat pauses until Confirm
    /// (the `chat_ui` steps of a key change, P5a critic F15; a fixture page leaves it off). Her
    /// chat is [`FixtureInfo::safety_conversation`]; nothing is added to the list. A third
    /// person, never verified (state Default), pauses her chat the same way
    /// ([`FixtureInfo::safety_default_conversation`]).
    pub safety_peer: bool,
    /// With the showcase chats: the last call of the chat with calls starts at the fixture's
    /// "now", rings for this long (at most [`FixtureOptions::RING`]) and then turns into a
    /// missed call; its chat is then the first of the list below the pinned ones
    /// (`--page=calls`). `None`: no call
    /// rings and the chat keeps its place below the fold, so no other picture changes.
    pub calls_ring_for: Option<Duration>,
    /// The history transfer of the fixture account (architecture §14.2 item 14); `None`:
    /// none, as before.
    pub transfer: Option<FixtureTransfer>,
    /// The fixture network says that a fresh link can bring the account's history as one
    /// archive the phone uploads (`NetworkDescriptor::history_transfer`, with
    /// `HistorySource::PushedOnLink`): its account offers "Transfer History from Phone…".
    /// Always so when [`Self::transfer`] is set. Only for the test network: a fixture that
    /// plays WhatsApp never says it (WhatsApp pushes its history on a link, it has no archive
    /// to transfer).
    pub history_on_link: bool,
    /// How long each stage of a [`FixtureTransfer::Walk`] lasts (tests shorten it).
    pub transfer_stage: Duration,
    /// Without [`FixtureOptions::link`]: what the NEXT account added after the start does
    /// (the fixture account is the first). `None`: it links at once, as the first did. Tests
    /// of a second link (a link page beside a list with chats).
    pub second_link: Option<FixtureSecondLink>,
    /// Without [`FixtureOptions::link`]: the fixture account still updates its contacts and
    /// groups after its link and never finishes (`BackendEvent::AccountProgress`: 160
    /// profiles and 12 groups left, then 120 and 8), so the banner says so
    /// (`--page=transferupdate`). `false`: no such report, as before.
    pub updating: bool,
    /// The fixture account's logout does not reach the network (`NotReached`, the device
    /// stays on the phone) and names the device "Kourier fixture" (link feedback L4: the
    /// notice of a removal or an unlink the network did not confirm). `false`: the logout
    /// removes the device, as before.
    pub logout_unreachable: bool,
    /// Sends that take their time (link feedback L6: the line under a message on its way;
    /// `--fixture-send=<name>`, pictures looked at by hand). `None`: every send is answered
    /// at once, as before.
    pub sends: Option<FixtureSends>,
    /// The network the fixture account belongs to (`--fixture-network=<fake|whatsapp|threema>`).
    pub network: FixtureNetwork,
    /// With [`FixtureOptions::link`]: the link stays where it is shown. The QR code is valid
    /// for ten minutes and nobody scans it, and a pair code is never entered by the phone
    /// (the pages `waqr`, `waphone` and `wacode` of K-A, `thqr` of Threema). `false`: the codes
    /// rotate and the last one is scanned, as before.
    pub hold_link: bool,
    /// With [`FixtureOptions::hold_link`] and [`FixtureNetwork::Threema`]: the link stands at
    /// the EMOJI step instead of the QR code (the page `themoji`): the first code is scanned at
    /// once and the fake's phone does not confirm the emoji for ten minutes. `false`: the link
    /// stands at the QR code, as the held link of every network does.
    pub hold_emoji: bool,
    /// What the pages of the WhatsApp restrictions put into the fixture account (K-A:
    /// `waban`, `walock`, `wahistory`, `wafailed`; W4: `walost`). `None`: nothing, as before.
    pub whatsapp: Option<FixtureWhatsApp>,
    /// How many accounts the profile holds (blended chats, `--page=accounts`,
    /// `accountsdrawer`; see `fixture_accounts.rs`). 0 and 1: the one fixture account, as every
    /// other page has it. 2: a second account of the test network with five chats of its own
    /// (one pinned, three messages unread). 3: and a REGISTERED fake that plays WhatsApp (its
    /// network on, `networks-on` written before the start) with an account of three chats (one
    /// pinned, one message unread). The extra chats are dated among the fixture account's, older
    /// than its newest message. Only for the plain fixture ([`FixtureNetwork::Fake`]) without
    /// [`Self::link`]; no other page sets it, so no other picture changes.
    pub accounts: u8,
    /// The page sends an own message from the WINDOW once it is up (`--page=media`: the QML
    /// presses Send for the photo), so the core stamps it with the clock reading of that
    /// moment, which follows how long the start took. The core's clock then starts five
    /// seconds into the minute of the fixture's "now" ([`window_send_clock_start`]) and the
    /// time under the message (and in the chat list) is the same in every run, as long as the
    /// send comes within 55 s of the start. `false`: the clock starts at "now", as before.
    pub window_sends: bool,
    /// The plain fixture also lists a fake that plays Telegram as TURNED OFF, without an account
    /// (like the OFF WhatsApp it always has): the page `tgwarn` opens its warning from the
    /// Welcome list. PAGE-SCOPED (LC-KT1b-5): an off Telegram in the plain fixture would change
    /// the welcome list of every wide picture and the Settings "Networks" card, so no other page
    /// sets it. Only for [`FixtureNetwork::Fake`]; `false`: no Telegram in the off list, as
    /// before.
    pub telegram_off: bool,
    /// The fixture's build cannot log in to Telegram for this reason ([`AppHandle::telegram_missing`]):
    /// Settings > Networks lists Telegram as "Not available in this build" without a switch,
    /// Add Account skips it, and the banner of every Telegram account says what is missing. A
    /// knob of the window tests (`settings_ui`, `networks_ui`); no page sets it, so no picture
    /// has a missing Telegram. `None`: the build lacks nothing, as before.
    pub telegram_missing: Option<TelegramMissing>,
    /// The fixture's network rejects every change the person makes (`Script::reject_changes`,
    /// like a rejected Threema transaction): an archive, a pin, a mute, a mark and the Settings
    /// switch "Sync chat changes with my phone" all come back refused. A knob of the window tests
    /// (`settings_ui` `roaming-refused`, `roaming_ui`'s refusals); no page sets it, so no picture
    /// has a refused change. `false`: the network accepts, as before.
    pub reject_changes: bool,
}

/// What a page of WhatsApp's own states puts into the fixture account
/// ([`FixtureOptions::whatsapp`]). The account is a WhatsApp account (the fake plays one) in
/// every case; no chat is added, the rows go into a generated chat of the list's lower body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixtureWhatsApp {
    /// A temporary ban (101, too many strangers) for another 23 hours: the connection state
    /// `Restricted` with WhatsApp's own text and a page to appeal on (`--page=waban`).
    Banned,
    /// The reach-out lock: the connection stays up, sends made here wait until the lock ends in
    /// three days (`--page=walock`).
    Locked,
    /// The phone is still sending the chats' history: one batch that says more follows, at 40 %
    /// (`--page=wahistory`; it comes a moment after the start, as a live batch does).
    Filling,
    /// An own message to someone who never wrote to the account, refused by WhatsApp as a first
    /// contact (`--page=wafailed`; the fixture's chat of the page holds it).
    Refused,
    /// Part of the pushed history never arrived: the backend tells the account so once, with a
    /// count of two lost chunks, and the banner keeps a quiet line (`--page=walost`; like a live
    /// notice it comes a moment after the start).
    Lost,
}

/// Which network the fixture account is an account of ([`FixtureOptions::network`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FixtureNetwork {
    /// The scripted test network, as every fixture page has always had it. A second fake that
    /// plays WhatsApp stands beside it, TURNED OFF and without an account: this build holds an
    /// experimental network (as the release archive will), so the welcome page, Add Account and
    /// Settings show it as they show it to a person who has not turned it on.
    #[default]
    Fake,
    /// The fixture account is a WhatsApp account, played by the fake with the same script and
    /// the same chats as [`FixtureNetwork::Fake`] (so no list moves): the network is turned
    /// on (`networks-on` in the fixture's `kourier.toml`) and no other network is off.
    WhatsApp,
    /// The same account, but the network is TURNED OFF at this start (the page `waoff`): the
    /// fake plays WhatsApp so the data can be seeded, the UI is told it is in the off list and
    /// not registered, so the account is a paused one (its banner offers "Turn On", its chats
    /// are read-only). The preference is not written.
    WhatsAppOff,
    /// The fixture account is a Threema account, played by the fake with the same script and
    /// the same ids as [`FixtureNetwork::Fake`] (the pages `thqr` and `themoji`, TH4 KH1c): the
    /// network is turned on (`networks-on` in the fixture's `kourier.toml`) and no other network
    /// is off. Its QR link shows the emoji step after the scan.
    Threema,
    /// The fixture account is a Telegram account, played by the fake with the same script and the
    /// same ids as [`FixtureNetwork::Fake`] (the pages `tgqr`, `tgcode` and `tgpassword`, KT-1b):
    /// the network is turned on (`networks-on` in the fixture's `kourier.toml`) and no other
    /// network is off. Its link asks no device name; the phone-number way walks the row of
    /// [`FixtureOptions::TELEGRAM_NUMBER`]: a code through the Telegram app with a new one
    /// offered at once, then the two-step password with its hint. [`AppHandle::telegram_missing`]
    /// stays `None`: this build is not missing anything of the fake's Telegram.
    Telegram,
}

/// How the fixture account's sends behave ([`FixtureOptions::sends`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixtureSends {
    /// Every send is answered after [`FixtureSends::SLOW`] and reports this phase meanwhile
    /// (`None`: no phase, "Sending… 0:42").
    Slow(Option<kourier_model::SendPhase>),
    /// The first attempt of every send is refused as rate limited, with this wait.
    RateLimited(Duration),
    /// The messages with files sent into one generated chat (the one `--page=walock` uses, of
    /// the list's lower body, so no other picture moves) upload slowly and report each file
    /// (upload progress: the ring and the X, `--page=uploading`, `uploadfile`).
    Uploading(FixtureUpload),
}

/// What the fixture does with the uploads of [`FixtureSends::Uploading`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixtureUpload {
    /// An album of seven pictures goes into the chat as soon as the account is seeded and
    /// stays in its upload (held: the first picture is up, the second at 45 %, the rest
    /// waiting, so the fifth tile is a "+2" ring), and a file is queued behind it
    /// (`--page=uploading`).
    Album,
    /// A lone file of 30 MB goes into the chat and stays in its upload at 40 %
    /// (`--page=uploadfile`).
    File,
    /// Nothing is sent: the chat's uploads are held (every file at 30 %) for a test or the
    /// person at the keyboard to send into (`--fixture-send=uploading`).
    Idle,
    /// A lone file of 30 MB goes into the chat and stays in its upload at 40 %, but the
    /// network answers the stop `TooLate` (the X came after the point of no return): the
    /// message goes out and the composer says "Already sent" (a Qt window test, no page).
    Late,
}

impl FixtureSends {
    /// How long a slow send takes: long enough for its line to show (after 3 s) in a picture.
    pub const SLOW: Duration = Duration::from_secs(120);
}

/// The second link of a fixture profile ([`FixtureOptions::second_link`]). Both ask for no
/// device name and show one QR code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixtureSecondLink {
    /// The code stays on screen (a minute) and nobody scans it: the link waits at its QR code.
    AtQr,
    /// The phone scans at once and is the fixture account's own: the core refuses it
    /// (one identity is one account, `buttons.md` F2).
    SameAccount,
    /// The phone scans at once and is another account (its own identity, no chats): the
    /// profile then has two accounts (link feedback L4: removing one keeps the other).
    OtherAccount,
}

/// The history transfer of fixture mode ([`FixtureOptions::transfer`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixtureTransfer {
    /// With [`FixtureOptions::link`]: the link walks a whole transfer (waiting for the
    /// phone, a download whose size becomes known, the read; a few seconds each) up to the
    /// point where this end comes: `Done`, `Skipped` (the phone's user declined) or a
    /// failure of a kind. "Continue Without History" ends it sooner.
    Walk(kourier_model::HistoryTransfer),
    /// With [`FixtureOptions::link`]: the link reaches this state (waiting, downloading,
    /// reading) and stays there until it is skipped.
    Hold(kourier_model::HistoryTransfer),
    /// Without [`FixtureOptions::link`]: the linked fixture account stands at this state,
    /// as after a restart of the application in the middle of a transfer, or after one
    /// that ended and was not dismissed. The backend reports it once the UI is up.
    Left(kourier_model::HistoryTransfer),
}

impl FixtureTransfer {
    /// How long each stage of [`FixtureTransfer::Walk`] lasts unless
    /// [`FixtureOptions::transfer_stage`] says otherwise.
    pub const STAGE: Duration = Duration::from_secs(4);
}

impl FixtureOptions {
    /// How long the QR code of a held link ([`FixtureOptions::hold_link`]) stays valid: long
    /// enough for any picture.
    pub const HELD_CODE: Duration = Duration::from_secs(600);

    /// The longest a call rings (`kourier_model::MAX_RING_MS`): what `--page=calls` asks for,
    /// long enough for a picture.
    pub const RING: Duration = Duration::from_millis(kourier_model::MAX_RING_MS.unsigned_abs());

    /// The number the fake that plays Telegram has an account for in the Telegram fixture
    /// ([`FixtureNetwork::Telegram`]), as a person types it: synthetic, and none of the
    /// rehearsal's (`kourier_fake::telegram`). `MainWindow.qml` types the same text on the
    /// pages `tgcode` and `tgpassword` (a page that cannot get past it exits 1).
    pub const TELEGRAM_NUMBER: &'static str = "+999000510284";
    /// The code that account's number is sent (through the Telegram app), synthetic.
    pub const TELEGRAM_CODE: &'static str = "12345";
    /// That account's two-step password, synthetic.
    pub const TELEGRAM_PASSWORD: &'static str = "synthetic password";
    /// The hint its owner gave for the password (the password step shows it as the field's note).
    pub const TELEGRAM_HINT: &'static str = "synthetic hint";
}

/// Messages that arrive in the fixture's timer chat after the start
/// ([`FixtureOptions::arrivals`]), on the fixture's clock. They are not part of the data a
/// start waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arrivals {
    /// How many messages arrive; their texts are `"Arrival 1"`, `"Arrival 2"`, ….
    pub count: u32,
    /// The time between two of them (and before the first).
    pub every: Duration,
    /// Each one is read on arrival and disappears this long after it; `None`: they stay.
    pub lifetime: Option<Duration>,
}

/// The text of the fixture message that comes due shortly after the start
/// ([`FixtureOptions::expiring`]).
pub const EXPIRING_TEXT: &str = "Gone in a moment.";

impl Default for FixtureOptions {
    fn default() -> Self {
        Self {
            link: false,
            qr_valid_for: Duration::from_secs(5),
            qr_codes: 3,
            messages_per_conversation: 120,
            contacts: 12,
            groups: 4,
            showcase: true,
            state: FixtureState::Online,
            expiring: None,
            arrivals: None,
            calls_unread: false,
            note_to_self: false,
            person_groups: 0,
            group_members: 0,
            group_timers: false,
            group_admin: false,
            group_mentions: false,
            safety_changed: false,
            safety_peer: false,
            calls_ring_for: None,
            transfer: None,
            history_on_link: false,
            transfer_stage: FixtureTransfer::STAGE,
            second_link: None,
            updating: false,
            logout_unreachable: false,
            sends: None,
            network: FixtureNetwork::Fake,
            hold_link: false,
            hold_emoji: false,
            whatsapp: None,
            accounts: 0,
            window_sends: false,
            telegram_off: false,
            telegram_missing: None,
            reject_changes: false,
        }
    }
}

/// Where the fixture account ends up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FixtureState {
    /// Connected and caught up.
    #[default]
    Online,
    /// Not connected.
    Offline,
    /// Connecting.
    Connecting,
    /// The network asks for a challenge to be solved in the browser (any answer is accepted).
    Challenge,
    /// The system clock is two hours ahead.
    ClockSkew,
    /// Another session took the connection: the "error" banner with "Reconnect".
    Reconnect,
    /// The device was unlinked; its history stays and "Link Again" works.
    LoggedOut,
}

/// How to start.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StartOptions {
    /// `--profile <name>`.
    pub profile: Profile,
    /// Normal or fixture mode.
    pub launch: Launch,
    /// Desktop integration (single instance, notifications, tray).
    pub desktop: DesktopOptions,
}

/// The environment variable that switches the history transfer of a new link OFF for one run
/// (`KOURIER_HISTORY_TRANSFER=0`, exactly `0`), in every build. By default a new link
/// offers the transfer, as Signal Desktop's does: the linking URL carries `backup5`, the
/// phone asks its user whether to transfer the history and he can decline there, and
/// the descriptor says `HistorySource::PushedOnLink`, which makes the screens offer
/// "Transfer History from Phone…" (architecture §14.2 item 14). Any other value, or none,
/// leaves the offer on. The switch is for the runs that do not want a phone to be asked
/// (a link that is not about the history); there is no persistent setting.
pub const HISTORY_TRANSFER_ENV: &str = "KOURIER_HISTORY_TRANSFER";

/// Whether a new link offers the history transfer given the value of
/// [`HISTORY_TRANSFER_ENV`]: always, except when it is exactly `0`.
#[cfg_attr(not(feature = "signal"), allow(dead_code))]
fn history_transfer_offered(value: Option<&std::ffi::OsStr>) -> bool {
    !value.is_some_and(|v| v == "0")
}

/// The Signal network of the user's profile: a new link offers the history transfer unless
/// the value of [`HISTORY_TRANSFER_ENV`] says `0`. The offer also sets the descriptor's
/// `HistorySource::PushedOnLink`, which is what makes the screens offer "Transfer History
/// from Phone…".
#[cfg(feature = "signal")]
fn signal_network(history_transfer: Option<&std::ffi::OsStr>) -> kourier_signal::SignalNetwork {
    kourier_signal::SignalNetwork::new()
        .with_history_transfer_offer(history_transfer_offered(history_transfer))
}

/// A network the user can add an account of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkInfo {
    /// The network.
    pub kind: NetworkKind,
    /// Its display name from the descriptor.
    pub name: String,
    /// Its login flows (the first one is the default).
    pub flows: Vec<LoginFlow>,
    /// A notice the user must accept before linking (§7.4).
    pub risk_notice: Option<RiskNotice>,
    /// Where the network's history comes from, as its descriptor says
    /// (`HistorySource::PushedOnLink`: a newly linked device is handed the account's
    /// history, so a UI offers "Transfer History from Phone…" for its accounts).
    pub history: kourier_model::HistorySource,
    /// Whether a fresh link can bring the account's history as ONE archive the phone uploads
    /// (the descriptor's `history_transfer`): a UI offers "Transfer History from Phone…"
    /// (remove and link again) for such a network's accounts and for no other.
    pub history_transfer: bool,
    /// An experimental network (the descriptor's `experimental`): it is registered only while
    /// the user has it turned on, and a person is warned before the first account is added.
    pub experimental: bool,
    /// The descriptor's `receipts_always_sent`: the network's accounts always send read receipts,
    /// typing and online status, so Settings says so under those two rows while it is on.
    pub receipts_always_sent: bool,
}

/// What a Telegram account of this build cannot run without and does not have (critic H3, KT-1b
/// critic finding 19: ONE type, always compiled, so the front-end names it without a cfg;
/// `crate::telegram` re-exports it). [`AppHandle::telegram_missing`] hands it out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TelegramMissing {
    /// No API pair: none compiled in, and no developer's file in a run profile.
    ApiKey,
    /// The helper binary or the pinned TDLib library is not where the build looks.
    Library,
}

/// [`AppHandle::networks_missing`] over what [`AppHandle::telegram_missing`] says: Telegram when
/// this build cannot log in to it, else nothing.
fn missing_networks(telegram: Option<TelegramMissing>) -> Vec<(NetworkKind, TelegramMissing)> {
    telegram
        .map(|m| (NetworkKind::Telegram, m))
        .into_iter()
        .collect()
}

/// What fixture mode knows about its data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixtureInfo {
    /// A fixed "now" for time labels: screenshots do not depend on the day they are taken.
    pub now_ms: i64,
    /// The conversation `--page=chat` opens (the first group).
    pub demo_conversation: Option<ConversationId>,
    /// A conversation with a photo, a file and a voice note (`--page=media`, `viewer`,
    /// `compose`).
    pub media_conversation: Option<ConversationId>,
    /// The showcase chat with quotes, an edited caption and recent own messages
    /// ([`FixtureOptions::showcase`]).
    pub quotes_conversation: Option<ConversationId>,
    /// The showcase chat with a disappearing-message timer.
    pub timer_conversation: Option<ConversationId>,
    /// The showcase chat with stand-ins for content Kourier does not show yet.
    pub kinds_conversation: Option<ConversationId>,
    /// The showcase chat with albums of two, three, four and many pictures and a text that
    /// was edited twice (`--page=album`, `albumviewer`, `revisions`).
    pub albums_conversation: Option<ConversationId>,
    /// The showcase chat with one long text, shown cut with "Read more" (`--page=readmore`).
    pub readmore_conversation: Option<ConversationId>,
    /// The direct chat with a call line of every state of a 1:1 call, the last one ringing
    /// (`--page=calls`).
    pub calls_conversation: Option<ConversationId>,
    /// The showcase chat of imported attachments that cannot be had: "Not available on this
    /// device", "No longer available", and a picture that downloads on a click
    /// (`--page=unavailable`).
    pub unavailable_conversation: Option<ConversationId>,
    /// The direct chat of the contact `--page=person` shows: the generated contact in the most
    /// groups, with an about text and a number she shares (people pages P1).
    pub person_conversation: Option<ConversationId>,
    /// The direct chat of the second person of the safety steps
    /// ([`FixtureOptions::safety_peer`]): her safety number changed and her chat pauses at the
    /// first send until Confirm (P5a, `chat_ui`).
    pub safety_conversation: Option<ConversationId>,
    /// The direct chat of the third person of the safety steps (a key she never verified; the
    /// first send to her pauses until Confirm, an approval: `chat_ui` `svDef*`, U6).
    pub safety_default_conversation: Option<ConversationId>,
    /// The group `--page=group` shows: a group other than the one with the newest message,
    /// with a description, rules, and an invited member (people pages P2).
    pub group_conversation: Option<ConversationId>,
    /// The Note to Self chat ([`FixtureOptions::note_to_self`]).
    pub note_to_self_conversation: Option<ConversationId>,
    /// The generated chat the pages of WhatsApp's states put their rows into
    /// (`--page=wahistory`, `wafailed`).
    pub whatsapp_conversation: Option<ConversationId>,
    /// The generated chat whose messages with files upload slowly
    /// ([`FixtureSends::Uploading`]: `--page=uploading`, `uploadfile`).
    pub uploads_conversation: Option<ConversationId>,
}

/// Which network the debug switch `KOURIER_DEV_REHEARSAL` (`crate::dev::rehearsal_requested`)
/// plays with its scripted rehearsal fake INSTEAD of the real backend: at most one, named by the
/// switch's exact value. A variant exists only in a build that can play it (the test network
/// plus that network's backend), so a build without Telegram can never be asked to rehearse it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rehearsal {
    /// No rehearsal: every backend is the real one (every release build).
    None,
    /// The WhatsApp slot holds `kourier_fake::FakeNetwork::rehearsal`.
    #[cfg(all(feature = "fake", feature = "whatsapp"))]
    WhatsApp,
    /// The Telegram slot holds `kourier_fake::FakeNetwork::rehearsal_telegram`.
    #[cfg(all(feature = "fake", feature = "telegram"))]
    Telegram,
    /// The Threema slot holds `kourier_fake::FakeNetwork::rehearsal_threema`.
    #[cfg(all(feature = "fake", feature = "threema"))]
    Threema,
}

/// The kind of chat [`AppHandle::rehearsal_arrive`] writes into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RehearsalKind {
    /// The first 1:1 chat of the running account.
    Direct,
    /// The first group of the running account.
    Group,
}

/// What the rehearsal's phone does on cue besides writing a text ([`AppHandle::rehearsal_act`],
/// T2 T-5): the same acts as `kourier_fake::PhoneAct`, kept here so the front-end's protocol
/// names no fake type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RehearsalAct {
    /// The chat is archived on the phone.
    Archive,
    /// The chat is taken out of the archive on the phone.
    Unarchive,
    /// The chat is pinned.
    Pin,
    /// The chat is unpinned.
    Unpin,
    /// The chat is muted for ever.
    Mute,
    /// The mute is taken off.
    Unmute,
    /// The other person calls into the (1:1) chat: it rings, then it is answered on the phone.
    Call,
    /// The other person reacts to the text that arrived last in the chat.
    PeerReact,
    /// The other person edits it.
    PeerEdit,
    /// The other person deletes it for everyone.
    PeerDelete,
}

impl RehearsalAct {
    /// The name the dev control spells.
    pub fn name(self) -> &'static str {
        match self {
            Self::Archive => "archive",
            Self::Unarchive => "unarchive",
            Self::Pin => "pin",
            Self::Unpin => "unpin",
            Self::Mute => "mute",
            Self::Unmute => "unmute",
            Self::Call => "call",
            Self::PeerReact => "peer_react",
            Self::PeerEdit => "peer_edit",
            Self::PeerDelete => "peer_delete",
        }
    }

    /// Every act.
    pub const ALL: [Self; 10] = [
        Self::Archive,
        Self::Unarchive,
        Self::Pin,
        Self::Unpin,
        Self::Mute,
        Self::Unmute,
        Self::Call,
        Self::PeerReact,
        Self::PeerEdit,
        Self::PeerDelete,
    ];
}

/// Why [`AppHandle::rehearsal_arrive`] put nothing into a chat.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RehearsalArriveError {
    /// This run does not play a network with the rehearsal fake.
    #[error("this run has no rehearsal network")]
    NotARehearsal,
    /// The rehearsal network has no running account, or no chat of that kind.
    #[error("{0}")]
    Refused(String),
}

/// Facts about the running app, the same for every [`AppHandle`] clone.
#[derive(Debug)]
struct AppInfo {
    networks: Vec<NetworkInfo>,
    /// The experimental networks this build holds and this start did not register (the user
    /// has them turned off): the UI lists them as "Off".
    networks_off: Vec<NetworkInfo>,
    /// The network whose link page the UI opens once the window is up, once (the one-shot
    /// `pending-link` of the preferences, read and cleared at this start).
    pending_link: Option<NetworkKind>,
    /// The UI asked for a restart ([`AppHandle::request_restart`]).
    restart: std::sync::atomic::AtomicBool,
    hostname: String,
    fixture: Option<FixtureInfo>,
    fixture_mode: bool,
    /// The fake network's control, in fixture mode (what its accounts were told).
    #[cfg(feature = "fake")]
    fake: Option<Arc<kourier_fake::FakeControl>>,
    /// The control of the rehearsal network that plays WhatsApp, Telegram or Threema in this run (T1a,
    /// `KOURIER_DEV_REHEARSAL=whatsapp|telegram|threema`); `None` in every other run.
    #[cfg(feature = "fake")]
    rehearsal: Option<Arc<kourier_fake::FakeControl>>,
    /// What seeding the reference profile took, when this start seeded it.
    #[cfg(feature = "perf")]
    seeded: Option<crate::perf::SeedReport>,
    /// The core's clock (in fixture mode the fixture's running clock).
    clock: CoreClock,
    /// Whether this run was started with a number tail the backends check a first link
    /// against (`BackendConfig::expect_number_suffix`); never the tail itself.
    expected_number_check: bool,
    /// Whether the REAL WhatsApp backend (not the rehearsal fake, not the fixture's) is
    /// registered in this run: only then do the usage-statistics counters mean anything.
    whatsapp_real: bool,
    /// This build cannot log in to Telegram, whether it is turned on or off ([`TelegramMissing`]);
    /// always `None` in a build without the `telegram` feature and in fixture mode.
    telegram_missing: Option<TelegramMissing>,
}

/// What the real Signal backend of this process saw of the Storage Service
/// ([`AppHandle::storage_counts`], BQ11): numbers only, no name, identifier, key or byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageCounts {
    /// The version of the newest manifest this process saw (fetched, or written by us).
    pub version: u64,
    /// How many identifiers that manifest lists.
    pub identifiers: u64,
    /// How many of them had no record in the backend's database when it was seen (0 right after
    /// a write of ours; all of them at a fresh profile's first read).
    pub unknown: u64,
    /// How many of our Storage Service writes were answered 200 in this process.
    pub writes: u64,
}

/// Whether the backend configuration carries a number tail a first link is checked against
/// (what [`AppHandle::expected_number_check`] reports; never the tail itself).
fn number_check_on(backend: &BackendConfig) -> bool {
    backend.expect_number_suffix.is_some()
}

/// The core's clock, for [`AppHandle::core_now_ms`].
struct CoreClock(Arc<dyn Clock>);

impl std::fmt::Debug for CoreClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CoreClock")
    }
}

/// The UI's handle to the running app: commands, events, queries and a few facts. Cloneable
/// and cheap; never blocks.
#[derive(Clone, Debug)]
pub struct AppHandle {
    core: CoreHandle,
    runtime: tokio::runtime::Handle,
    shutdown: CancellationToken,
    info: Arc<AppInfo>,
    desktop: DesktopHandle,
    avatars: Arc<crate::media::AvatarHandles>,
    pasted: Arc<crate::media::PastedFiles>,
    archives: Arc<crate::folders::ArchiveFiles>,
}

impl AppHandle {
    /// The desktop integration: preferences, events (activation, tray), window state.
    pub fn desktop(&self) -> &DesktopHandle {
        &self.desktop
    }

    /// Sends a command to the core without blocking. Fails only when the core has stopped.
    pub fn send(&self, cmd: UiCommand) -> Result<(), CoreError> {
        self.core.send(cmd)
    }

    /// Subscribes to the core's events from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<UiEvent> {
        self.core.subscribe()
    }

    /// How many core events were sent and not yet read by every subscriber
    /// ([`CoreHandle::events_unseen`](kourier_core::facade::CoreHandle::events_unseen)).
    pub fn events_unseen(&self) -> usize {
        self.core.events_unseen()
    }

    /// How many `MarkRead`s were sent to the core and are not finished
    /// ([`CoreHandle::reads_in_flight`](kourier_core::facade::CoreHandle::reads_in_flight)).
    pub fn reads_in_flight(&self) -> usize {
        self.core.reads_in_flight()
    }

    /// Runs a query and awaits the rows (tests and app-side tasks; the UI uses
    /// `UiCommand::Query`).
    pub async fn query(&self, query: Query) -> Result<QueryData, CoreError> {
        self.core.query(query).await
    }

    /// The runtime UI-side tasks (the event pump) run on.
    pub fn runtime(&self) -> &tokio::runtime::Handle {
        &self.runtime
    }

    /// Cancelled when the app shuts down: producers feeding the UI stop on it (§8.2 step 5).
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// The compiled-in networks, in registration order.
    pub fn networks(&self) -> &[NetworkInfo] {
        &self.info.networks
    }

    /// The experimental networks this build holds that are turned OFF at this start (not in
    /// [`Self::networks`]): the user can turn them on in Settings, and an account of one that
    /// was linked earlier is paused (its chats stay readable, nothing connects).
    pub fn networks_off(&self) -> &[NetworkInfo] {
        &self.info.networks_off
    }

    /// The network whose link page the window opens right after its start, once: "Turn On and
    /// Restart" asked for it before the restart. `None` on every other start.
    pub fn pending_link(&self) -> Option<NetworkKind> {
        self.info.pending_link
    }

    /// Asks for Kourier to start again once the window is closed: the UI quits its event loop
    /// after this, `main` sees [`Self::restart_requested`] after the app shut down in order and
    /// runs [`crate::restart::Restart`]. A fixture, picture or measuring run only records it.
    pub fn request_restart(&self) {
        self.info
            .restart
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether the UI asked for a restart ([`Self::request_restart`]).
    pub fn restart_requested(&self) -> bool {
        self.info.restart.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// This computer's host name (the default device name when linking).
    pub fn hostname(&self) -> &str {
        &self.info.hostname
    }

    /// Whether a number tail was set for this run (`KOURIER_DEV_EXPECT_NUMBER_TAIL`, debug
    /// builds): the development state reports it so a run tool can refuse to say "scan now"
    /// when the check is off. The tail itself is never exposed.
    pub fn expected_number_check(&self) -> bool {
        self.info.expected_number_check
    }

    /// What the core's clock reads now (ms since the Unix epoch). In fixture mode this is the
    /// fixture's clock, which RUNS from [`FixtureInfo::now_ms`]; the UI's own "now" stays at
    /// that instant so pictures do not move, except where a debug run asks for it to run.
    pub fn core_now_ms(&self) -> i64 {
        i64::try_from(self.info.clock.0.now_ms()).unwrap_or(i64::MAX)
    }

    /// Fixture facts, once the fixture data is in `core.db`.
    pub fn fixture(&self) -> Option<FixtureInfo> {
        self.info.fixture
    }

    /// Whether this is fixture mode (with or without seeded data).
    pub fn is_fixture(&self) -> bool {
        self.info.fixture_mode
    }

    /// Fixture mode: the names of the commands the fake network's accounts received so far
    /// (`Connect`, `SkipHistoryTransfer`, `DismissHistoryTransfer`, …), in order. Tests of
    /// the UI count them; empty outside fixture mode.
    #[cfg(feature = "fake")]
    pub fn fixture_commands(&self) -> Vec<&'static str> {
        self.info
            .fake
            .as_ref()
            .map(|c| c.all_commands())
            .unwrap_or_default()
    }

    /// Fixture mode: the `(field id, value)` lists of the login inputs the fake network's
    /// accounts received so far (`SubmitLoginInput`), in order. A test of the link page reads
    /// what a click really sent (a request for a new code goes alone); empty outside fixture
    /// mode.
    #[cfg(feature = "fake")]
    pub fn fixture_login_inputs(&self) -> Vec<Vec<(String, String)>> {
        self.info
            .fake
            .as_ref()
            .map(|c| c.all_login_inputs())
            .unwrap_or_default()
    }

    /// What this build lacks to log in to Telegram, whether the person has it turned on or off,
    /// in a debug or a release build (critic H3, his Q4, KT-1b finding 2): Telegram is then not
    /// registered and not listed, its existing accounts are paused, Settings > Networks lists it
    /// as "Not available in this build" without a switch, and every account of it says what is
    /// missing in its banner (the viewmodel's words). `None` when this build can log in to it
    /// (registered, or listed as off), in a build WITHOUT the `telegram` feature (a release
    /// archive holds no Telegram code, so it has no Telegram account to speak for) and in fixture
    /// mode. Always compiled: `kourier-qt` builds without the feature and needs no cfg.
    pub fn telegram_missing(&self) -> Option<TelegramMissing> {
        self.info.telegram_missing
    }

    /// The networks of this build that cannot be logged in to, with what is missing for each:
    /// today Telegram only ([`AppHandle::telegram_missing`]); empty without it.
    pub fn networks_missing(&self) -> Vec<(NetworkKind, TelegramMissing)> {
        missing_networks(self.telegram_missing())
    }

    /// The WhatsApp backend's usage-statistics counters `(sent, looked)` of this process
    /// (`kourier_whatsapp::wam_sent`, `wam_looked`), for the development state: `Some` only when
    /// the REAL WhatsApp backend is registered in this run, else `None` (a rehearsal, a fixture, a
    /// release build and a build without the backend have no client that could send or look at a
    /// frame, so a 0 would prove nothing). A run that expects `sent == 0` must see `looked` grow.
    pub fn wam_counts(&self) -> Option<(u64, u64)> {
        #[cfg(feature = "whatsapp")]
        {
            self.info
                .whatsapp_real
                .then(|| (kourier_whatsapp::wam_sent(), kourier_whatsapp::wam_looked()))
        }
        #[cfg(not(feature = "whatsapp"))]
        {
            // A build without the backend never registers it.
            debug_assert!(!self.info.whatsapp_real);
            None
        }
    }

    /// The Signal backend's Storage Service counts of this process (BQ11, A10d D8;
    /// `kourier_signal::storage_counts`), for the development state: the version and identifier
    /// count of the newest manifest this process saw, how many of its identifiers had no record
    /// here, and how many of our writes were answered 200. `None` in a build without the real
    /// Signal backend, in fixture mode (the fake plays Signal there) and until the real backend
    /// read a manifest in this run: "not measured" is never a row of zeros (the rule of
    /// [`Self::wam_counts`]). Numbers only; process-wide, so it means what it says on a profile
    /// with one Signal account (the real check's test profile).
    pub fn storage_counts(&self) -> Option<StorageCounts> {
        #[cfg(feature = "signal")]
        {
            if self.info.fixture_mode {
                return None;
            }
            kourier_signal::storage_counts().map(|c| StorageCounts {
                version: c.version,
                identifiers: c.identifiers,
                unknown: c.unknown,
                writes: c.writes,
            })
        }
        #[cfg(not(feature = "signal"))]
        {
            // A build without the backend never reads a manifest.
            None
        }
    }

    /// Whether this run plays WhatsApp, Telegram or Threema with the rehearsal fake
    /// (`KOURIER_DEV_REHEARSAL=whatsapp|telegram|threema`): that network's real backend is not in it and
    /// nothing of it is contacted. Always false in a build without the test network.
    pub fn is_rehearsal(&self) -> bool {
        #[cfg(feature = "fake")]
        {
            self.info.rehearsal.is_some()
        }
        #[cfg(not(feature = "fake"))]
        {
            false
        }
    }

    /// How many frames the rehearsal fake has taken (T2, C3: every send, react, edit, delete,
    /// revoke and timer set; [`kourier_fake::FakeControl::frames`]): the dev control's
    /// `state.frames`. `None` in every run that is not a rehearsal, so "not measured" is never
    /// read as a 0 that proves nothing (the same rule as [`Self::wam_counts`]).
    pub fn rehearsal_frames(&self) -> Option<u64> {
        #[cfg(feature = "fake")]
        {
            self.info.rehearsal.as_ref().map(|c| c.frames())
        }
        #[cfg(not(feature = "fake"))]
        {
            None
        }
    }

    /// The rehearsal network puts one text from the other member into the first chat of that
    /// kind of its running account, as a live message (what the phone does on cue in a real run).
    ///
    /// # Errors
    /// [`RehearsalArriveError::NotARehearsal`] unless this run plays WhatsApp, Telegram or Threema with the
    /// rehearsal fake (the fakes share the control, whose `arrive` is network-neutral), else
    /// [`RehearsalArriveError::Refused`] (no running account, no chat of that kind).
    #[cfg_attr(not(feature = "fake"), allow(unused_variables))]
    pub fn rehearsal_arrive(&self, kind: RehearsalKind) -> Result<(), RehearsalArriveError> {
        #[cfg(feature = "fake")]
        {
            let control = self
                .info
                .rehearsal
                .as_ref()
                .ok_or(RehearsalArriveError::NotARehearsal)?;
            let kind = match kind {
                RehearsalKind::Direct => kourier_fake::Arrival::Direct,
                RehearsalKind::Group => kourier_fake::Arrival::Group,
            };
            control
                .arrive(kind)
                .map_err(|e| RehearsalArriveError::Refused(e.to_string()))
        }
        #[cfg(not(feature = "fake"))]
        {
            Err(RehearsalArriveError::NotARehearsal)
        }
    }

    /// The rehearsal's phone plays `act` in the first chat of `kind` of its running account
    /// (T2, T-5): a setting it changes, a call, or the other person's react, edit or delete of
    /// the text that arrived last there. Nothing is sent to the app but what the network tells
    /// it; the fake counts no frame for it.
    ///
    /// # Errors
    /// [`RehearsalArriveError::NotARehearsal`] unless this run plays WhatsApp with the rehearsal
    /// fake, else [`RehearsalArriveError::Refused`] (no running account, no chat of that kind,
    /// nothing arrived to act on, a call into a group).
    #[cfg_attr(not(feature = "fake"), allow(unused_variables))]
    pub fn rehearsal_act(
        &self,
        kind: RehearsalKind,
        act: RehearsalAct,
    ) -> Result<(), RehearsalArriveError> {
        #[cfg(feature = "fake")]
        {
            use kourier_fake::PhoneAct;
            let control = self
                .info
                .rehearsal
                .as_ref()
                .ok_or(RehearsalArriveError::NotARehearsal)?;
            let kind = match kind {
                RehearsalKind::Direct => kourier_fake::Arrival::Direct,
                RehearsalKind::Group => kourier_fake::Arrival::Group,
            };
            let act = match act {
                RehearsalAct::Archive => PhoneAct::Archive,
                RehearsalAct::Unarchive => PhoneAct::Unarchive,
                RehearsalAct::Pin => PhoneAct::Pin,
                RehearsalAct::Unpin => PhoneAct::Unpin,
                RehearsalAct::Mute => PhoneAct::Mute,
                RehearsalAct::Unmute => PhoneAct::Unmute,
                RehearsalAct::Call => PhoneAct::Call,
                RehearsalAct::PeerReact => PhoneAct::PeerReact,
                RehearsalAct::PeerEdit => PhoneAct::PeerEdit,
                RehearsalAct::PeerDelete => PhoneAct::PeerDelete,
            };
            control
                .phone(kind, act)
                .map_err(|e| RehearsalArriveError::Refused(e.to_string()))
        }
        #[cfg(not(feature = "fake"))]
        {
            Err(RehearsalArriveError::NotARehearsal)
        }
    }

    /// What seeding the reference profile took, when this start seeded it (`Launch::Perf` on
    /// an empty profile).
    #[cfg(feature = "perf")]
    pub fn seeded(&self) -> Option<crate::perf::SeedReport> {
        self.info.seeded
    }

    /// The provider URL (`image://kourier/avatar/<handle>?v=<rev>`) of a contact's or
    /// conversation's avatar file; `None` for a path outside the account's blob directory.
    /// Never blocks (see [`crate::media::AvatarHandles`]).
    pub fn avatar_url(
        &self,
        account: &kourier_model::AccountUuid,
        avatar: &kourier_model::BlobRef,
    ) -> Option<String> {
        self.avatars.url(account, avatar)
    }

    /// Decrypts the avatar `handle` of an `image://kourier/avatar/…` URL into memory.
    /// Blocking: for the image provider's worker thread only.
    pub fn read_avatar(
        &self,
        handle: u64,
    ) -> Result<zeroize::Zeroizing<Vec<u8>>, crate::media::MediaError> {
        self.avatars.read(handle)
    }

    /// The core's media handle table (attachments of the `image://kourier/{att,thumb,blurhash}`
    /// URLs, §8.6).
    pub fn media(&self) -> kourier_core::facade::MediaHandles {
        self.core.media()
    }

    /// Writes a file pasted into the composer (`name` is a plain file name) into the tmpfs
    /// media cache and returns its path for `UiCommand::SendFiles`. Blocking (small files).
    pub fn save_pasted(
        &self,
        name: &str,
        bytes: &[u8],
    ) -> Result<std::path::PathBuf, crate::media::MediaError> {
        self.pasted.save(name, bytes)
    }

    /// Deletes a file Kourier made for the composer (a pasted picture, the archive of a
    /// folder) once the composer no longer needs it; other paths are ignored. Never waits
    /// for a disk.
    pub fn release_pasted(&self, path: &std::path::Path) -> bool {
        if self.pasted.release(path) {
            return true;
        }
        // An archive is on the disk and can be large: it is forgotten now and deleted on a
        // worker thread (the UI thread calls this). Should the runtime be closing already,
        // the wipe at exit removes it.
        match self.archives.take(path) {
            Some(place) => {
                self.runtime
                    .spawn_blocking(move || crate::folders::remove_place(&place));
                true
            }
            None => false,
        }
    }

    /// Whether `path` is a file Kourier made for the composer: one
    /// [`save_pasted`](Self::save_pasted) wrote, or an archive at a path
    /// [`reserve_archive`](Self::reserve_archive) handed out (a refused send hands its files
    /// back: these are deleted when they leave the composer).
    pub fn is_pasted(&self, path: &std::path::Path) -> bool {
        self.pasted.owns(path) || self.archives.owns(path)
    }

    /// Where the archive of a folder called `folder` is to be written
    /// ([`crate::folders::pack`]): a new private place on the disk, deleted like a pasted
    /// file ([`release_pasted`](Self::release_pasted), at exit, at the next start).
    pub fn reserve_archive(
        &self,
        folder: &str,
    ) -> Result<std::path::PathBuf, crate::folders::FolderError> {
        self.archives.reserve(folder)
    }
}

/// The running app. Call [`App::shutdown`] after the UI's event loop returned; dropping it
/// shuts down too.
pub struct App {
    runtime: Option<tokio::runtime::Runtime>,
    core: Option<Core>,
    /// How long `stop` gives the core ([`core_shutdown_timeout`] of the registry it started with).
    core_timeout: Duration,
    handle: AppHandle,
    paths: Paths,
    desktop_task: Option<tokio::task::JoinHandle<()>>,
    #[cfg(feature = "fake")]
    _fixture_home: Option<tempfile::TempDir>,
    /// The profile's lock, released when the app is gone (after `stop`, which `Drop` runs).
    _profile_lock: Option<crate::paths::ProfileLock>,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("paths", &self.paths)
            .finish_non_exhaustive()
    }
}

/// Sets the process umask to 077: every file and directory Kourier creates is private (§9.4).
pub fn set_private_umask() {
    // SAFETY: umask only swaps the process file mode creation mask; it cannot fail.
    unsafe {
        libc::umask(0o077);
    }
}

/// This computer's host name, `None` when it has none or cannot be read. THE reader of it (W2b): the
/// text that names this device in the UI ([`hostname`]) and the label the WhatsApp backend is listed by
/// on the phone (`Kourier (<host>)`, [`whatsapp_network`]) both come from here, and no backend reads
/// the host name itself.
pub fn host_name() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for `buf.len()` bytes; gethostname writes at most that many.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).trim().to_owned();
    (!name.is_empty()).then_some(name)
}

/// This computer's host name, or `Kourier` when it has none.
pub fn hostname() -> String {
    host_name().unwrap_or_else(|| "Kourier".to_owned())
}

/// The device name fixture mode shows instead of the host name, so a picture of the link page does
/// not depend on the machine it was rendered on.
pub const FIXTURE_DEVICE_NAME: &str = "my-computer";

/// The default device name the UI offers: [`hostname`], or [`FIXTURE_DEVICE_NAME`] in fixture mode.
fn device_name(fixture_mode: bool) -> String {
    if fixture_mode {
        FIXTURE_DEVICE_NAME.to_owned()
    } else {
        hostname()
    }
}

/// The WhatsApp backend over `parts`, with the host name of [`host_name`] for its device label.
#[cfg(feature = "whatsapp")]
fn whatsapp_network(parts: kourier_whatsapp::NetParts) -> kourier_whatsapp::WhatsAppNetwork {
    kourier_whatsapp::WhatsAppNetwork::new(parts.with_host_name(host_name().unwrap_or_default()))
}

fn worker_threads() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(2)
        .clamp(1, 4)
}

fn network_info(registry: &NetworkRegistry) -> Vec<NetworkInfo> {
    registry.iter().map(|n| info_of(&**n)).collect()
}

/// What the UI is told about `network`, from its descriptor.
fn info_of(network: &dyn kourier_backend_api::Network) -> NetworkInfo {
    let d = network.descriptor();
    NetworkInfo {
        kind: network.kind(),
        name: d.display_name.clone(),
        flows: d.login_flows.clone(),
        risk_notice: d.risk_notice.clone(),
        history: d.history,
        history_transfer: d.history_transfer,
        experimental: d.experimental,
        receipts_always_sent: d.receipts_always_sent,
    }
}

/// Which kind of build this is, for the rules that differ between a developer's build and a user's.
#[cfg(any(feature = "whatsapp", feature = "telegram", feature = "threema"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Build {
    /// `debug_assertions` on: `cargo build`, the tests, the screenshots.
    Debug,
    /// What `cargo xtask dist` and `install` build, and so every archive.
    Release,
}

#[cfg(any(feature = "whatsapp", feature = "telegram", feature = "threema"))]
impl Build {
    fn current() -> Self {
        if cfg!(debug_assertions) {
            Self::Debug
        } else {
            Self::Release
        }
    }
}

/// What a start does with the WhatsApp backend of this build.
#[cfg(feature = "whatsapp")]
#[derive(Debug, PartialEq, Eq)]
enum WhatsAppRegistration {
    /// Registered: the network runs, accounts of it start.
    Registered,
    /// Compiled in and turned off: on K-A's "compiled in but off" list, not registered.
    Listed,
    /// Not offered at all: neither registered nor listed.
    Absent,
}

/// THE RULE for the WhatsApp backend (W2b, lane decision L1): a RELEASE build never registers it
/// and never lists it, so no archive (the user's install, a test archive) links a real account until
/// W3a, W4, W5 and the first real runs have landed and the user says so; a debug build registers it
/// while `networks-on` of `kourier.toml` names it, else lists it as off (K-A, §7.4). The release rule
/// goes away by one edit of this function, when that has happened (and `cargo xtask dist`'s check of
/// the packed binary, which fails on WhatsApp in `kourier --print-networks`, goes with it).
#[cfg(feature = "whatsapp")]
fn whatsapp_registration(build: Build, prefs: &Prefs) -> WhatsAppRegistration {
    match (build, prefs.network_on(NetworkKind::WhatsApp)) {
        (Build::Release, _) => WhatsAppRegistration::Absent,
        (Build::Debug, true) => WhatsAppRegistration::Registered,
        (Build::Debug, false) => WhatsAppRegistration::Listed,
    }
}

/// What a start does with the Telegram backend of this build.
#[cfg(feature = "telegram")]
#[derive(Debug, PartialEq, Eq)]
enum TelegramRegistration {
    /// Registered: the network runs, accounts of it start.
    Registered,
    /// Compiled in and turned off: on K-A's "compiled in but off" list, not registered.
    Listed,
    /// This build cannot log in (critic H3, KT-1b finding 2: whether the switch is on or off, in a
    /// debug or a release build): NOT registered and NOT listed, so Add Account offers no
    /// Telegram, an existing account stays readable as a paused one, Settings > Networks lists it
    /// as not available and the account's banner says what is missing
    /// (`AppHandle::telegram_missing`).
    Unavailable(crate::telegram::TelegramMissing),
    /// Not offered at all: neither registered nor listed, and nothing to say about it. Today
    /// only a release build whose parts could log in (none can: no API key is compiled in); the
    /// release rule goes away by one edit of [`telegram_registration`].
    Absent,
}

/// THE RULE for the Telegram backend (TG-1 decisions D2, TG-2 critic H3, KT-1b finding 2): a build
/// whose parts cannot log in (`state` Missing: no API pair, or no helper files) offers nothing and
/// names what is missing, on or off, debug or release (`Unavailable`): its Telegram accounts read
/// the missing banner and never "Offline". A build whose parts can log in: a RELEASE build never
/// registers it and never lists it (`Absent`); a debug build registers it while `networks-on` of
/// `kourier.toml` names it and lists it as off otherwise (WhatsApp's rule). Registered, it still
/// contacts nothing in this build: the gate's dialer refuses every dial (`kourier_telegram`).
#[cfg(feature = "telegram")]
fn telegram_registration(
    build: Build,
    prefs: &Prefs,
    state: crate::telegram::PartsState,
) -> TelegramRegistration {
    use crate::telegram::PartsState;
    match (build, prefs.network_on(NetworkKind::Telegram), state) {
        (_, _, PartsState::Missing(m)) => TelegramRegistration::Unavailable(m),
        (Build::Release, _, PartsState::Ready) => TelegramRegistration::Absent,
        (Build::Debug, true, PartsState::Ready) => TelegramRegistration::Registered,
        (Build::Debug, false, PartsState::Ready) => TelegramRegistration::Listed,
    }
}

/// What a start does with Telegram, the rehearsal included: in a debug build that was asked to
/// rehearse Telegram (`KOURIER_DEV_REHEARSAL=telegram`) the slot is the scripted fake, which needs no
/// API pair and no helper files, so the parts' state is not looked at (never `Unavailable`) and it
/// is registered or listed by the switch alone; every other start follows [`telegram_registration`].
#[cfg(feature = "telegram")]
fn telegram_slot(
    build: Build,
    prefs: &Prefs,
    state: crate::telegram::PartsState,
    rehearsal: Rehearsal,
) -> TelegramRegistration {
    if telegram_rehearsal_asked(build, rehearsal) {
        return if prefs.network_on(NetworkKind::Telegram) {
            TelegramRegistration::Registered
        } else {
            TelegramRegistration::Listed
        };
    }
    telegram_registration(build, prefs, state)
}

/// Whether this start plays Telegram with the rehearsal fake: the switch names it, in a debug
/// build that holds the test network. THE ONE place that says so ([`telegram_slot`] and the arm
/// of [`compiled_in_experimental`] both ask it).
#[cfg(feature = "telegram")]
fn telegram_rehearsal_asked(build: Build, rehearsal: Rehearsal) -> bool {
    #[cfg(feature = "fake")]
    {
        rehearsal == Rehearsal::Telegram && build == Build::Debug
    }
    #[cfg(not(feature = "fake"))]
    {
        let _ = (build, rehearsal);
        false
    }
}

/// The Telegram backend over `parts`, with the host name of [`host_name`] for its session's name
/// on Telegram (`Kourier (<host>)`, his answer U-4: WhatsApp's label, the one reader).
#[cfg(feature = "telegram")]
fn telegram_network(parts: kourier_telegram::TgParts) -> kourier_telegram::TelegramNetwork {
    kourier_telegram::TelegramNetwork::new(parts.with_host_name(host_name().unwrap_or_default()))
}

/// The environment variable a DEBUG build takes the TDLib library from (`cargo xtask tdlib path`
/// prints it); a release build never reads it.
#[cfg(feature = "telegram")]
const TDJSON_ENV: &str = "KOURIER_TDJSON";

/// Where the Telegram helper and the pinned TDLib library of the binary `exe` are: the helper beside
/// the binary (cargo builds every member's binaries into one directory), else in the installed
/// tree's `lib/kourier/`; the library from [`TDJSON_ENV`] (debug only), else `target/tdlib/` beside
/// `target/debug/` (where `cargo xtask tdlib build` puts it), else `lib/kourier/`. Only looks at
/// which files exist: nothing is opened or started. Without the binary's own place (`current_exe`
/// failed) or with one that is not an absolute path, the files are `HelperFiles::not_found`:
/// never a helper or a library found relative to the working directory (fix F15).
#[cfg(feature = "telegram")]
fn telegram_helper_files(
    exe: Option<&std::path::Path>,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> kourier_telegram::HelperFiles {
    let Some(dir) = exe
        .filter(|e| e.is_absolute())
        .and_then(std::path::Path::parent)
    else {
        return kourier_telegram::HelperFiles::not_found();
    };
    let installed = dir.join("../lib/kourier");
    let beside = dir.join("kourier-tdlib");
    let helper = if beside.is_file() {
        beside
    } else {
        installed.join("kourier-tdlib")
    };
    let name = format!("libtdjson.so.{}", kourier_telegram::PIN.version);
    let from_env = if cfg!(debug_assertions) {
        env(TDJSON_ENV)
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
    } else {
        None
    };
    let built = dir.join("../tdlib").join(&name);
    let tdjson = from_env.unwrap_or_else(|| {
        if built.is_file() {
            built
        } else {
            installed.join(&name)
        }
    });
    kourier_telegram::HelperFiles {
        helper,
        tdjson,
        no_dump: crate::process::asked_to_deny_dumps(&env),
    }
}

/// The application's Telegram parts and whether they can log in: the helper files of this binary,
/// the backend's dialer (`kourier_telegram::real::helper_parts`; the mode comes from the run
/// tool's confirmation, [`crate::telegram::dial_confirmation`]: the refusing dialer unless this
/// very start was confirmed) and the API pair
/// COMPILED INTO THIS BUILD (`ApiCredentials::compiled()`: none in any build today, because no
/// app id is registered, and never the dummy pair: TG-1b fix G1), else, in a DEBUG build inside a
/// kept run profile only, the developer's file there (`crate::telegram::dev_api`, critic M13).
/// Looks at which files exist and reads at most that one file (and, when the run tool's variable
/// is set, consumes its confirmation file); builds and starts nothing. Without
/// a pair a login answers "this build has no Telegram API key" before any gate, helper or library
/// exists.
///
/// ONE confirmation per start: this runs from [`Seams::real`], which a normal start calls once
/// (`normal_setup_in`). `--print-networks` (`networks_line`, a start that exits) calls
/// [`Seams::probe`] instead, which judges over an environment without the run tool's nonce and so
/// never takes a confirmation. The decision is captured by the maker below, so a restarted helper
/// of the same process keeps it. A second call in the same process finds the confirmation file gone
/// and refuses; it cannot widen the first.
#[cfg(feature = "telegram")]
fn real_telegram(
    build: Build,
) -> (
    crate::telegram::PartsState,
    Box<dyn FnOnce() -> kourier_telegram::TgParts>,
) {
    real_telegram_over(build, |k: &str| {
        // TR R0a fix G-1.3 (review F1): a unit test of this crate judges over an environment that
        // hides the run tool's nonce, whatever the test process inherited, so no test can take (or
        // unlink) a real profile's confirmation file and none can get parts that dial.
        #[cfg(test)]
        if k == crate::telegram::DIAL_ENV {
            return None;
        }
        std::env::var_os(k)
    })
}

/// [`real_telegram`] over the environment `env` (the process's, minus what the caller hides).
#[cfg(feature = "telegram")]
fn real_telegram_over<E>(
    build: Build,
    env: E,
) -> (
    crate::telegram::PartsState,
    Box<dyn FnOnce() -> kourier_telegram::TgParts>,
)
where
    E: Fn(&str) -> Option<std::ffi::OsString> + 'static,
{
    real_telegram_in(
        build,
        std::env::current_exe().ok(),
        env,
        unix_now_ms(),
        |exe, env, api, mode| real_telegram_parts_in(exe, env, api, mode),
    )
}

/// [`real_telegram`] over its inputs: the kind of build, where the binary is, the environment, the
/// time and the maker of the parts (`real_telegram_parts_in` in the application; a test hands one
/// that records the mode it was given).
#[cfg(feature = "telegram")]
fn real_telegram_in<E, M>(
    build: Build,
    exe: Option<std::path::PathBuf>,
    env: E,
    now_ms: u64,
    make: M,
) -> (
    crate::telegram::PartsState,
    Box<dyn FnOnce() -> kourier_telegram::TgParts>,
)
where
    E: Fn(&str) -> Option<std::ffi::OsString> + 'static,
    M: FnOnce(
            Option<&std::path::Path>,
            E,
            Option<kourier_telegram::ApiCredentials>,
            kourier_telegram::real::DialMode,
        ) -> kourier_telegram::TgParts
        + 'static,
{
    let decision = crate::telegram::dial_confirmation(build == Build::Debug, &env, now_ms);
    decision.announce();
    let api = kourier_telegram::ApiCredentials::compiled()
        .or_else(|| crate::telegram::dev_api(build == Build::Debug, &env));
    let files = telegram_helper_files(exe.as_deref(), &env);
    let state = crate::telegram::parts_state(api.is_some(), &files);
    (
        state,
        Box::new(move || make(exe.as_deref(), env, api, decision.mode)),
    )
}

/// The wall clock in milliseconds since the epoch (0 before it: every confirmation is then
/// refused as stale).
#[cfg(feature = "telegram")]
fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// [`real_telegram`]'s maker with its four inputs named: where the binary is, the environment, the
/// API pair (the ONLY seam for the pair: the application passes `compiled()` or the run profile's
/// file, a test passes the dummy pair) and the dialer mode. Everything else is the application's
/// own composition.
#[cfg(feature = "telegram")]
fn real_telegram_parts_in(
    exe: Option<&std::path::Path>,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
    api: Option<kourier_telegram::ApiCredentials>,
    mode: kourier_telegram::real::DialMode,
) -> kourier_telegram::TgParts {
    let parts = kourier_telegram::real::helper_parts(telegram_helper_files(exe, env), mode);
    match api {
        Some(api) => parts.with_api(api),
        None => parts,
    }
}

/// What a start replaces in a test: the kind of build, and where the WhatsApp backend gets its
/// transport and HTTP client from. The maker only makes the lazy `NetParts` (nothing is connected
/// or made until an account's actor first connects or starts a login); a test hands in parts that
/// refuse and count. The Threema backend's parts are lazy the same way (`ThreemaParts`: nothing is
/// made until an account's actor asks for a client).
struct Seams {
    #[cfg(any(feature = "whatsapp", feature = "telegram", feature = "threema"))]
    build: Build,
    #[cfg(feature = "whatsapp")]
    whatsapp_parts: Box<dyn FnOnce() -> kourier_whatsapp::NetParts>,
    /// Where the Telegram backend gets its helper spawner and its dialer from (lazy too: no helper
    /// starts until an account needs TDLib, and in this build none ever does).
    #[cfg(feature = "telegram")]
    telegram_parts: Box<dyn FnOnce() -> kourier_telegram::TgParts>,
    /// Whether those parts can log in (critic H3: decides Registered / Listed / Unavailable).
    #[cfg(feature = "telegram")]
    telegram_state: crate::telegram::PartsState,
    #[cfg(feature = "threema")]
    threema_parts: Box<dyn FnOnce() -> kourier_threema::ThreemaParts>,
}

impl Seams {
    /// What a normal start uses.
    fn real() -> Self {
        Self::made_of(
            #[cfg(feature = "telegram")]
            real_telegram(Build::current()),
        )
    }

    /// What `--print-networks` uses ([`networks_line`]): the real seams whose Telegram decision is
    /// judged over `env` WITHOUT the run tool's nonce in every build (TR R0a fix G-4, review F4): a
    /// probe never takes a confirmation, so its dialer line says `no-confirmation` and a pending
    /// order stays where it is.
    fn probe_over(env: impl Fn(&str) -> Option<std::ffi::OsString> + 'static) -> Self {
        #[cfg(not(feature = "telegram"))]
        let _ = env;
        Self::made_of(
            #[cfg(feature = "telegram")]
            real_telegram_over(Build::current(), move |k: &str| {
                (k != crate::telegram::DIAL_ENV).then(|| env(k)).flatten()
            }),
        )
    }

    /// [`probe_over`](Self::probe_over) over the process environment.
    fn probe() -> Self {
        Self::probe_over(|k: &str| std::env::var_os(k))
    }

    /// The real seams around the Telegram decision and parts `telegram` made.
    fn made_of(
        #[cfg(feature = "telegram")] telegram: (
            crate::telegram::PartsState,
            Box<dyn FnOnce() -> kourier_telegram::TgParts>,
        ),
    ) -> Self {
        Self {
            #[cfg(any(feature = "whatsapp", feature = "telegram", feature = "threema"))]
            build: Build::current(),
            #[cfg(feature = "whatsapp")]
            whatsapp_parts: Box::new(kourier_whatsapp::real::net_parts),
            #[cfg(feature = "telegram")]
            telegram_state: telegram.0,
            #[cfg(feature = "telegram")]
            telegram_parts: telegram.1,
            #[cfg(feature = "threema")]
            threema_parts: Box::new(kourier_threema::dial::live_parts),
        }
    }

    /// The same seams whose Telegram parts carry the files root of `paths` (TG-1b: TDLib's files
    /// directory of each account lies below it, on the tmpfs media cache).
    #[cfg(feature = "telegram")]
    fn with_telegram_files_root(mut self, paths: &Paths) -> Self {
        let root = telegram_files_root(paths);
        let parts = self.telegram_parts;
        self.telegram_parts = Box::new(move || parts().with_files_root(root));
        self
    }

    /// The same seams whose Telegram parts bind a NEW account to `servers` (a stored one wins;
    /// `crate::telegram::servers`).
    #[cfg(feature = "telegram")]
    fn with_telegram_servers(mut self, servers: kourier_telegram::TgEnv) -> Self {
        let parts = self.telegram_parts;
        self.telegram_parts = Box::new(move || parts().with_env(servers));
        self
    }
}

/// The root of the Telegram accounts' TDLib files directories: `telegram/` of the media cache
/// (`paths.media_dir()`, a tmpfs: TDLib writes its downloads there in plaintext).
#[cfg(feature = "telegram")]
fn telegram_files_root(paths: &Paths) -> std::path::PathBuf {
    paths.media_dir().join("telegram")
}

/// Threema parts for a test's [`Seams`]: they refuse every call and count into `made` how often a
/// client was made.
#[cfg(all(test, feature = "threema"))]
fn refusing_threema_parts(
    made: Arc<std::sync::atomic::AtomicUsize>,
) -> Box<dyn FnOnce() -> kourier_threema::ThreemaParts> {
    Box::new(move || kourier_threema::ThreemaParts::refusing(made))
}

/// The experimental networks this build holds, as backends ready to register: WhatsApp when
/// [`whatsapp_registration`] offers it (the caller registers it only while it is on; else it is
/// listed as off), Telegram when [`telegram_registration`] offers it, Threema when
/// `threema::threema_registration` does.
struct Experimental {
    networks: Vec<Arc<dyn kourier_backend_api::Network>>,
    /// The control of the rehearsal network (the WhatsApp or the Telegram slot), when `rehearsal`
    /// asked for it.
    #[cfg(feature = "fake")]
    rehearsal: Option<Arc<kourier_fake::FakeControl>>,
    /// The REAL WhatsApp backend is among `networks` (the slot did not hold the rehearsal fake).
    real_whatsapp: bool,
    /// This build cannot log in to Telegram (on or off): what is missing.
    telegram_missing: Option<TelegramMissing>,
}

/// The experimental networks of this build ([`Experimental`]). With `rehearsal` (the debug switch
/// `KOURIER_DEV_REHEARSAL=whatsapp|telegram|threema`, T1a, TR) the named network's slot holds the
/// scripted rehearsal fake INSTEAD of the real backend and that network's parts maker
/// (`seams.whatsapp_parts`, `seams.telegram_parts`, `seams.threema_parts`) is never called: no real client, helper or dialer is made.
#[cfg_attr(
    not(all(feature = "whatsapp", feature = "fake")),
    allow(unused_variables, unused_mut)
)]
fn compiled_in_experimental(
    seams: Seams,
    prefs: &Prefs,
    rehearsal: Rehearsal,
    #[cfg(feature = "threema")] threema_start: crate::prefs::ThreemaStart,
    #[cfg(feature = "threema")] threema_allowed: kourier_threema::AllowList,
) -> Experimental {
    let mut networks: Vec<Arc<dyn kourier_backend_api::Network>> = Vec::new();
    #[cfg(feature = "fake")]
    let mut rehearsal_control = None;
    let mut real_whatsapp = false;
    #[cfg(feature = "whatsapp")]
    if whatsapp_registration(seams.build, prefs) != WhatsAppRegistration::Absent {
        #[cfg(feature = "fake")]
        let fake = (rehearsal == Rehearsal::WhatsApp).then(kourier_fake::FakeNetwork::rehearsal);
        #[cfg(feature = "fake")]
        if let Some(fake) = fake {
            tracing::warn!(
                "the WhatsApp network is the REHEARSAL fake (KOURIER_DEV_REHEARSAL): the real backend is not made and nothing of WhatsApp is contacted"
            );
            rehearsal_control = Some(fake.control());
            networks.push(Arc::new(fake));
        } else {
            let parts = (seams.whatsapp_parts)();
            networks.push(Arc::new(whatsapp_network(parts)));
            real_whatsapp = true;
        }
        #[cfg(not(feature = "fake"))]
        {
            let parts = (seams.whatsapp_parts)();
            networks.push(Arc::new(whatsapp_network(parts)));
            real_whatsapp = true;
        }
    }
    #[cfg_attr(not(feature = "telegram"), allow(unused_mut))]
    let mut telegram_missing = None;
    #[cfg(feature = "telegram")]
    match telegram_slot(seams.build, prefs, seams.telegram_state, rehearsal) {
        TelegramRegistration::Registered | TelegramRegistration::Listed => {
            // THE REHEARSAL ARM (TR, A4): the slot holds the scripted fake and `seams.telegram_parts`
            // is NOT CALLED (not even to drop what it makes): no real parts, helper or dialer exist
            // in a rehearsal, which is what makes its confirmed mode harmless.
            #[cfg(feature = "fake")]
            let fake = telegram_rehearsal_asked(seams.build, rehearsal)
                .then(kourier_fake::FakeNetwork::rehearsal_telegram);
            #[cfg(feature = "fake")]
            if let Some(fake) = fake {
                tracing::warn!(
                    "the Telegram network is the REHEARSAL fake (KOURIER_DEV_REHEARSAL): the real backend is not made and nothing of Telegram is contacted"
                );
                rehearsal_control = Some(fake.control());
                networks.push(Arc::new(fake));
            } else {
                let parts = (seams.telegram_parts)();
                networks.push(Arc::new(telegram_network(parts)));
            }
            #[cfg(not(feature = "fake"))]
            {
                let parts = (seams.telegram_parts)();
                networks.push(Arc::new(telegram_network(parts)));
            }
        }
        TelegramRegistration::Unavailable(missing) => {
            tracing::info!(
                ?missing,
                "this build cannot log in to Telegram; it is not registered and not listed"
            );
            telegram_missing = Some(missing);
        }
        TelegramRegistration::Absent => {}
    }
    // Threema: every build registers it while it is on and lists it while the user has it off, and
    // any ID may link (`crate::threema`). A start by the run tool keeps TH4 J6 and decision E1:
    // listed or registered only when that start can link. The list goes to the network's parts:
    // with none, `StartLogin` is refused at once (B1's gate).
    #[cfg(feature = "threema")]
    if crate::threema::threema_registration(
        seams.build,
        threema_start,
        prefs.network_on(NetworkKind::Threema),
        crate::threema::linking(&threema_allowed),
    ) != crate::threema::ThreemaRegistration::Absent
    {
        // THE REHEARSAL ARM (TR, T8): the slot holds the scripted fake and `seams.threema_parts` is
        // NOT CALLED (no `live_parts`, no dialer, no client): nothing of Threema exists in a
        // rehearsal, whatever the list says (the fake judges no ID, `kourier_fake::threema`).
        #[cfg(feature = "fake")]
        let fake =
            (rehearsal == Rehearsal::Threema).then(kourier_fake::FakeNetwork::rehearsal_threema);
        #[cfg(feature = "fake")]
        if let Some(fake) = fake {
            tracing::warn!(
                "the Threema network is the REHEARSAL fake (KOURIER_DEV_REHEARSAL): the real backend is not made and nothing of Threema is contacted"
            );
            rehearsal_control = Some(fake.control());
            networks.push(Arc::new(fake));
        } else {
            let parts = (seams.threema_parts)();
            networks.push(Arc::new(crate::threema::threema_network(
                parts,
                threema_allowed,
            )));
        }
        #[cfg(not(feature = "fake"))]
        {
            let parts = (seams.threema_parts)();
            networks.push(Arc::new(crate::threema::threema_network(
                parts,
                threema_allowed,
            )));
        }
    }
    Experimental {
        networks,
        #[cfg(feature = "fake")]
        rehearsal: rehearsal_control,
        real_whatsapp,
        telegram_missing,
    }
}

/// Registers every experimental network of `networks` that `prefs` has turned ON and returns the
/// others as the off list the UI shows. A network that is not experimental is not this
/// function's: it is registered by the caller whatever the preferences say.
fn register_experimental(
    registry: &mut NetworkRegistry,
    prefs: &Prefs,
    networks: Vec<Arc<dyn kourier_backend_api::Network>>,
) -> Vec<NetworkInfo> {
    let mut off = Vec::new();
    for network in networks {
        if !network.descriptor().experimental {
            tracing::warn!("a network that is not experimental was offered as one; ignored");
            continue;
        }
        if prefs.network_on(network.kind()) {
            register(registry, network);
        } else {
            tracing::info!("an experimental network is turned off; it is not registered");
            off.push(info_of(&*network));
        }
    }
    off
}

/// Reads the one-shot link request of the preferences at `path` and CLEARS it (written back at
/// once, so a crash or a second start never opens the page again): the network it names, when
/// that network is registered and experimental now, else `None`. Called before the desktop task
/// loads the same file.
fn take_pending_link(path: &std::path::Path, registry: &NetworkRegistry) -> Option<NetworkKind> {
    let mut prefs = Prefs::load_or_default(path);
    let kind = prefs.pending_link_kind();
    prefs.pending_link.take()?;
    if let Err(e) = prefs.save(path) {
        // Not cleared: it would open the page at the next start too; better not at all.
        tracing::warn!(error = %e, "the link request was not cleared; ignored");
        return None;
    }
    kind.filter(|k| {
        registry
            .get(*k)
            .is_some_and(|n| n.descriptor().experimental)
    })
}

/// What `start` builds before the core runs.
struct Setup {
    paths: Paths,
    keyring: AnyKeyring,
    registry: NetworkRegistry,
    /// The experimental networks this build holds and `registry` lacks (turned off).
    networks_off: Vec<NetworkInfo>,
    /// The network whose link page opens at this start, once ([`take_pending_link`]).
    pending_link: Option<NetworkKind>,
    backend: BackendConfig,
    #[cfg(feature = "fake")]
    fake: Option<kourier_fake::FakeNetwork>,
    /// The control of the rehearsal network in the WhatsApp, Telegram or Threema slot
    /// (`KOURIER_DEV_REHEARSAL`).
    #[cfg(feature = "fake")]
    rehearsal: Option<Arc<kourier_fake::FakeControl>>,
    /// The real WhatsApp backend is registered ([`AppHandle::wam_counts`]).
    whatsapp_real: bool,
    /// This build cannot log in to Telegram ([`AppHandle::telegram_missing`]).
    telegram_missing: Option<TelegramMissing>,
    /// The desktop task reads the preferences as this start did: with the networks that are on
    /// by default ([`Prefs::with_default_networks`]). A normal start outside the run tool.
    default_networks: bool,
    #[cfg(feature = "fake")]
    home: Option<tempfile::TempDir>,
    /// Fixture profile pictures, written once the fixture account exists.
    #[cfg(feature = "fake")]
    avatars: crate::fixture_media::FixtureAvatars,
    /// The core's clock: the system clock, or in fixture mode one near the fixture's dates.
    clock: Arc<dyn Clock>,
    /// Fixture mode's "now" (ms): a little after the newest fixture message.
    #[cfg(feature = "fake")]
    fixture_now: Option<i64>,
    /// The steps of the fixture script that are its data, when more follow
    /// ([`FixtureOptions::arrivals`]).
    #[cfg(feature = "fake")]
    fixture_seed: Option<usize>,
    /// The peer of the fixture's chat with calls.
    #[cfg(feature = "fake")]
    fixture_calls: Option<kourier_model::UserId>,
    /// The peer of the fixture's chat of attachments that cannot be had.
    #[cfg(feature = "fake")]
    fixture_unavailable: Option<kourier_model::UserId>,
    /// The peer of the fixture's person page (`--page=person`).
    #[cfg(feature = "fake")]
    fixture_person: Option<kourier_model::UserId>,
    /// The second person of the safety steps ([`FixtureOptions::safety_peer`]).
    fixture_safety: Option<kourier_model::UserId>,
    /// The third person of the safety steps: never verified, her first send refused as untrusted.
    fixture_safety_default: Option<kourier_model::UserId>,
    /// The group of the fixture's group page (`--page=group`).
    #[cfg(feature = "fake")]
    fixture_group: Option<crate::fixture_media::FixtureGroup>,
    /// The peer of the Note to Self chat ([`FixtureOptions::note_to_self`]).
    #[cfg(feature = "fake")]
    fixture_note: Option<kourier_model::UserId>,
    /// The peer of the chat the pages of WhatsApp's states use ([`FixtureOptions::whatsapp`]).
    #[cfg(feature = "fake")]
    fixture_whatsapp: Option<kourier_model::UserId>,
    /// The peer of the chat whose uploads are slow ([`FixtureSends::Uploading`]).
    #[cfg(feature = "fake")]
    fixture_uploads: Option<kourier_model::UserId>,
    /// The script of the next account added after the fixture was seeded.
    #[cfg(feature = "fake")]
    fixture_second_link: Option<kourier_fake::Script>,
    /// The accounts [`FixtureOptions::accounts`] adds after the fixture account is seeded.
    #[cfg(feature = "fake")]
    fixture_extra: Option<crate::fixture_accounts::Extra>,
    /// The core's clock and the instant it stands at while the page's uploads are sent
    /// ([`FixturePin`]).
    #[cfg(feature = "fake")]
    fixture_pin: Option<FixturePin>,
    /// An empty reference profile to seed: what to remember of its script, and its root.
    #[cfg(feature = "perf")]
    perf_seed: Option<(crate::perf::Reference, std::path::PathBuf)>,
}

impl App {
    /// Starts Kourier: umask 077, the runtime "kourier-rt" (`min(4, cores)` workers), paths,
    /// logging, the master key, the networks, the core (§8.2 step 2). Backends do not connect
    /// until the UI sends `UiCommand::UiReady` after its first frame. Call it on the main
    /// thread before the UI starts; it blocks while the keyring answers and `core.db` opens
    /// (and, in fixture mode, while the fixture data is ingested).
    pub fn start(opts: StartOptions) -> Result<Self, AppError> {
        set_private_umask();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(worker_threads())
            .thread_name("kourier-rt")
            .enable_all()
            .build()
            .map_err(AppError::Runtime)?;
        // §8.2 step 1: a second launch of a profile hands over to the first and exits.
        let instance = match &opts.launch {
            Launch::Normal | Launch::Scripted => Some(desktop::instance_name(&Paths::resolve(
                opts.profile.clone(),
            )?)),
            Launch::Fixture(_) => None,
            // Like fixture mode: a measuring run claims no name and takes no requests.
            #[cfg(feature = "perf")]
            Launch::Perf(_) => None,
        };
        let connection =
            match runtime.block_on(desktop::connect(&opts.desktop, instance.as_deref())) {
                Ok(Connected::Run(c)) => c,
                Ok(Connected::Forwarded) => return Err(AppError::AlreadyRunning),
                Err(e) => return Err(AppError::Instance(e)),
            };
        // One process per profile, also where no bus name says so (offscreen, no session
        // bus): taken before a log file, a key or a database of the profile is opened, and
        // held until the app is gone. Fixture and measuring runs have directories of their
        // own per run.
        let profile_lock = match &opts.launch {
            Launch::Normal | Launch::Scripted => {
                let paths = Paths::resolve(opts.profile.clone())?;
                Some(
                    crate::paths::ProfileLock::acquire(&paths).map_err(|e| match e {
                        crate::paths::LockError::Held => AppError::ProfileInUse,
                        crate::paths::LockError::Io(e) => AppError::Dirs(e),
                    })?,
                )
            }
            Launch::Fixture(_) => None,
            #[cfg(feature = "perf")]
            Launch::Perf(_) => None,
        };
        #[allow(unused_mut)]
        let mut setup = match &opts.launch {
            Launch::Normal => runtime.block_on(normal_setup(&opts.profile, false))?,
            Launch::Scripted => runtime.block_on(normal_setup(&opts.profile, true))?,
            Launch::Fixture(f) => fixture_setup(f)?,
            #[cfg(feature = "perf")]
            Launch::Perf(p) => perf_setup(p)?,
        };
        // An empty reference profile is seeded once the core runs.
        #[cfg(feature = "perf")]
        let perf_seed = setup.perf_seed.take();
        let core_key = setup.keyring.core_db_key();
        let keyring = Arc::new(setup.keyring);
        let vault = Arc::new(AppVault::new(keyring, setup.paths.clone()));
        // (A network of the off list is not on, whatever the registry holds: a fixture seeds
        // the account of a turned-off network through the fake, which is registered.)
        let networks: Vec<NetworkInfo> = network_info(&setup.registry)
            .into_iter()
            .filter(|n| !setup.networks_off.iter().any(|off| off.kind == n.kind))
            .collect();
        let shutdown = CancellationToken::new();
        let core_timeout = core_shutdown_timeout(&setup.registry);
        let mut cfg = CoreConfig::new(
            setup.paths.core_db(),
            kourier_db::DbKey::from_bytes(*core_key),
            setup.registry,
            vault,
            setup.clock.clone(),
            runtime.handle().clone(),
        );
        let expected_number_check = number_check_on(&setup.backend);
        cfg.backend = setup.backend;
        // Plaintext playback copies live under $XDG_RUNTIME_DIR (tmpfs); the core uses its
        // `attachments/` subdirectory only (§5.8).
        cfg.media_cache = Some(setup.paths.media_dir());
        cfg.cancel = shutdown.child_token();
        let core = runtime.block_on(Core::start(cfg))?;
        let handle = core.handle();
        #[cfg(feature = "fake")]
        let fixture = match (&opts.launch, &setup.fake) {
            (Launch::Fixture(f), Some(fake)) if !f.link => Some(runtime.block_on(seed_fixture(
                &handle,
                fake,
                &setup.paths,
                &setup.avatars,
                setup.fixture_now,
                setup.fixture_seed,
                PagePeers {
                    calls: setup.fixture_calls.clone(),
                    unavailable: setup.fixture_unavailable.clone(),
                    person: setup.fixture_person.clone(),
                    safety: setup.fixture_safety.clone(),
                    safety_default: setup.fixture_safety_default.clone(),
                    group: setup.fixture_group.clone(),
                    note_to_self: setup.fixture_note.clone(),
                    whatsapp: setup.fixture_whatsapp.clone(),
                    uploads: setup.fixture_uploads.clone(),
                    upload: match f.sends {
                        Some(FixtureSends::Uploading(upload)) => Some(upload),
                        _ => None,
                    },
                    held_send: f.whatsapp == Some(FixtureWhatsApp::Locked),
                    pin: setup.fixture_pin.clone(),
                    extra: setup.fixture_extra.clone(),
                },
            ))?),
            _ => None,
        };
        // The next account added gets the second link's script (the seeding took the default
        // one).
        #[cfg(feature = "fake")]
        if let (Some(fake), Some(second)) = (&setup.fake, setup.fixture_second_link.take()) {
            fake.control().push_script(second);
        }
        // ("Link Again" of the fixture's unlinked account needs nothing here: the fake does
        // not replay a script the account already ran, so the relinked account comes online
        // with its history in `core.db`.)
        #[cfg(not(feature = "fake"))]
        let fixture = None;
        #[cfg(feature = "perf")]
        let seeded =
            match (perf_seed, &setup.fake) {
                (Some((reference, home)), Some(fake)) => Some(runtime.block_on(
                    crate::perf::seed(&handle, fake, &setup.paths, &reference, &home),
                )?),
                _ => None,
            };
        crate::trace::mark_with_cpu("startup.app_started", &[]);
        tracing::info!(
            networks = networks.len(),
            fixture = matches!(opts.launch, Launch::Fixture(_)),
            "core started"
        );
        let (desktop, desktop_task) = desktop::spawn(
            runtime.handle(),
            handle.clone(),
            connection,
            &opts.desktop,
            setup.default_networks,
            &setup.paths,
            shutdown.child_token(),
        );
        // Resume and network hints for the backends (§4.7); the task ends with `shutdown`.
        desktop::spawn_hints(
            runtime.handle(),
            handle.clone(),
            opts.desktop.system_bus.clone(),
            shutdown.child_token(),
        );
        let avatars = Arc::new(crate::media::AvatarHandles::new(
            &setup.paths.accounts_dir(),
        ));
        let pasted = Arc::new(crate::media::PastedFiles::new(&setup.paths.media_dir()));
        // (Emptied now: what a run that crashed left behind goes.)
        let archives = Arc::new(crate::folders::ArchiveFiles::new(
            &setup.paths.archives_dir(),
        ));
        let handle = AppHandle {
            core: handle,
            runtime: runtime.handle().clone(),
            desktop,
            avatars,
            pasted,
            archives,
            shutdown,
            info: Arc::new(AppInfo {
                networks,
                networks_off: setup.networks_off,
                pending_link: setup.pending_link,
                restart: std::sync::atomic::AtomicBool::new(false),
                hostname: device_name(matches!(opts.launch, Launch::Fixture(_))),
                fixture,
                fixture_mode: matches!(opts.launch, Launch::Fixture(_)),
                #[cfg(feature = "fake")]
                fake: setup.fake.as_ref().map(kourier_fake::FakeNetwork::control),
                #[cfg(feature = "fake")]
                rehearsal: setup.rehearsal.clone(),
                #[cfg(feature = "perf")]
                seeded,
                clock: CoreClock(setup.clock.clone()),
                expected_number_check,
                whatsapp_real: setup.whatsapp_real,
                telegram_missing: setup.telegram_missing,
            }),
        };
        Ok(Self {
            runtime: Some(runtime),
            core: Some(core),
            core_timeout,
            handle,
            paths: setup.paths,
            desktop_task: Some(desktop_task),
            #[cfg(feature = "fake")]
            _fixture_home: setup.home,
            _profile_lock: profile_lock,
        })
    }

    /// The handle the UI uses.
    pub fn handle(&self) -> AppHandle {
        self.handle.clone()
    }

    /// How long a shutdown gives the core: [`core_shutdown_timeout`] of the networks this start
    /// registered.
    pub fn core_shutdown_timeout(&self) -> Duration {
        self.core_timeout
    }

    /// The profile's directories.
    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// Orderly shutdown (§8.2 step 5): cancel the producers that feed the UI, stop the core
    /// (accounts get `Shutdown`, queued writes finish, `core.db` closes) within
    /// [`Self::core_shutdown_timeout`], then give the runtime [`RUNTIME_SHUTDOWN_TIMEOUT`]. When
    /// the UI asked for a restart ([`AppHandle::request_restart`]), the preferences in memory
    /// are on disk afterwards, whatever the desktop task did (see [`Self::keep_prefs`]).
    pub fn shutdown(mut self) {
        self.shut_down();
    }

    fn shut_down(&mut self) {
        let restart = self.handle.restart_requested();
        self.stop();
        if restart {
            self.keep_prefs();
        }
    }

    /// Writes the preferences in memory when the file differs. A restart reads the file: "Turn
    /// On and Restart" changes the preferences and quits at once, and the desktop task that
    /// saves them may be busy in another arm for longer than [`DESKTOP_SHUTDOWN_TIMEOUT`] and
    /// is then abandoned with the command unsaved, so the restart would come up on the old
    /// file. Runs after the runtime stopped, when no task of it writes any more.
    fn keep_prefs(&self) {
        let path = self.paths.config_file();
        let prefs = self.handle.desktop().prefs();
        if Prefs::load_or_default(&path) == prefs {
            return;
        }
        match prefs.save(&path) {
            Ok(()) => tracing::info!("preferences saved before the restart"),
            Err(e) => tracing::warn!(error = %e, "preferences not saved before the restart"),
        }
    }

    fn stop(&mut self) {
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        self.handle.shutdown.cancel();
        // The desktop task closes our notifications and removes the tray icon first.
        if let Some(task) = self.desktop_task.take() {
            let _ = runtime
                .block_on(async { tokio::time::timeout(DESKTOP_SHUTDOWN_TIMEOUT, task).await });
        }
        if let Some(core) = self.core.take() {
            let done = runtime
                .block_on(async { tokio::time::timeout(self.core_timeout, core.shutdown()).await });
            if done.is_err() {
                tracing::warn!("the core did not stop in time; cancelling its accounts");
            }
        }
        runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
        // Pasted composer files are plaintext in tmpfs: they go at exit (§5.8). So do the
        // archives of folders, which are plaintext on the disk.
        self.handle.pasted.clear();
        self.handle.archives.clear();
        tracing::info!("stopped");
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The user's profile: paths, logging, the Secret Service master key, compiled-in networks.
async fn normal_setup(profile: &Profile, scripted: bool) -> Result<Setup, AppError> {
    let paths = Paths::resolve(profile.clone())?;
    paths.create_dirs().map_err(AppError::Dirs)?;
    crate::logging::init(Some(&paths.log_dir()));
    normal_setup_in(paths, |k| std::env::var_os(k), scripted).await
}

/// What [`normal_setup`] does once the paths are known and logging runs, reading the
/// environment through `env` so that a test can start from a private one: the keyring, the
/// compiled-in networks and their registry. This is the one place the registry of a normal
/// start is built, and the test `a_normal_start_offers_the_history_transfer_by_default` runs
/// it.
async fn normal_setup_in(
    paths: Paths,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
    scripted: bool,
) -> Result<Setup, AppError> {
    // A rehearsal switch this build cannot honour fails the start BEFORE `Seams::real()` makes the
    // Telegram decision: that decision takes the run tool's confirmation and logs the dialer line,
    // and a start that fails must have taken no order (TR R0a re-check N-6; a source pin keeps the
    // order). `start_networks` judges it again for the callers that do not come through here.
    #[cfg(debug_assertions)]
    crate::dev::rehearsal_refused(&env).map_err(|e| AppError::Config(ConfigError::Rehearsal(e)))?;
    normal_setup_scripted_with(paths, env, Seams::real(), scripted).await
}

/// [`normal_setup_in`] of an unscripted start with the parts a test replaces ([`Seams`]).
#[cfg(test)]
async fn normal_setup_with(
    paths: Paths,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
    seams: Seams,
) -> Result<Setup, AppError> {
    normal_setup_scripted_with(paths, env, seams, false).await
}

/// [`normal_setup_in`] with the parts a test replaces ([`Seams`]); `scripted` is a start driven by
/// `--dev-control` ([`Launch::Scripted`]).
async fn normal_setup_scripted_with(
    paths: Paths,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
    seams: Seams,
    scripted: bool,
) -> Result<Setup, AppError> {
    let backend =
        config::backend_config(|k| env(k).and_then(|v| v.into_string().ok()), &paths.state)?;
    #[cfg(debug_assertions)]
    let dev_keyring = if crate::dev::wants_dev_keyring(&env) {
        tracing::info!("development keyring (KOURIER_DEV_KEYRING); the Secret Service is not used");
        Some(crate::dev::open_keyring(&paths)?)
    } else {
        None
    };
    #[cfg(not(debug_assertions))]
    let dev_keyring: Option<Keyring<crate::keys::NoSecretStore>> = None;
    let keyring = match dev_keyring {
        Some(k) => AnyKeyring::Passphrase(k),
        None => AnyKeyring::SecretService(secret_service_keyring(&paths).await?),
    };
    // The experimental networks of this build are registered only while the user has them
    // turned on (`networks-on` of `kourier.toml`, read here at the start: a change takes
    // effect at the next one). An account of one that is off is paused: the core keeps its
    // chats and starts nothing for it.
    let prefs_path = paths.config_file();
    let threema_start = crate::prefs::threema_start_of(&env, scripted, &paths);
    let prefs = start_prefs(&prefs_path, threema_start);
    let default_networks = threema_start == crate::prefs::ThreemaStart::Person;
    let StartNetworks {
        registry,
        networks_off,
        #[cfg(feature = "fake")]
        fake,
        #[cfg(feature = "fake")]
        rehearsal,
        whatsapp_real,
        telegram_missing,
    } = {
        #[cfg(feature = "telegram")]
        let seams = seams.with_telegram_files_root(&paths);
        start_networks(&env, &prefs, seams, threema_start)?
    };
    let pending_link = take_pending_link(&prefs_path, &registry);
    Ok(Setup {
        paths,
        keyring,
        registry,
        networks_off,
        pending_link,
        backend,
        #[cfg(feature = "fake")]
        fake,
        #[cfg(feature = "fake")]
        rehearsal,
        default_networks,
        whatsapp_real,
        telegram_missing,
        #[cfg(feature = "fake")]
        home: None,
        #[cfg(feature = "fake")]
        avatars: Default::default(),
        clock: Arc::new(SystemClock),
        #[cfg(feature = "fake")]
        fixture_now: None,
        #[cfg(feature = "fake")]
        fixture_seed: None,
        #[cfg(feature = "fake")]
        fixture_calls: None,
        #[cfg(feature = "fake")]
        fixture_unavailable: None,
        #[cfg(feature = "fake")]
        fixture_person: None,
        fixture_safety: None,
        fixture_safety_default: None,
        #[cfg(feature = "fake")]
        fixture_group: None,
        #[cfg(feature = "fake")]
        fixture_note: None,
        #[cfg(feature = "fake")]
        fixture_whatsapp: None,
        #[cfg(feature = "fake")]
        fixture_uploads: None,
        #[cfg(feature = "fake")]
        fixture_second_link: None,
        #[cfg(feature = "fake")]
        fixture_extra: None,
        #[cfg(feature = "fake")]
        fixture_pin: None,
        #[cfg(feature = "perf")]
        perf_seed: None,
    })
}

/// The networks a normal start puts in front of the core.
struct StartNetworks {
    registry: NetworkRegistry,
    /// The experimental networks this build holds and `registry` lacks (turned off).
    networks_off: Vec<NetworkInfo>,
    #[cfg(feature = "fake")]
    fake: Option<kourier_fake::FakeNetwork>,
    /// The control of the rehearsal network that plays WhatsApp, Telegram or Threema
    /// (`KOURIER_DEV_REHEARSAL`).
    #[cfg(feature = "fake")]
    rehearsal: Option<Arc<kourier_fake::FakeControl>>,
    /// The REAL WhatsApp backend was made AND is registered (the preferences have it on).
    whatsapp_real: bool,
    /// This build cannot log in to Telegram (on or off).
    telegram_missing: Option<TelegramMissing>,
}

/// THE place the registry of a normal start is built, from the build's features and `prefs`
/// (Signal, the test network where it is built in, the experimental networks that are on); it
/// opens nothing and contacts nothing. `normal_setup_with` and [`networks_line`] both call it, so
/// what `--print-networks` says is what a start would register.
#[cfg_attr(
    not(any(feature = "signal", feature = "fake")),
    allow(unused_variables, unused_mut)
)]
/// The preferences a normal start works from: the file's, with the networks that are on by default
/// (only for a person's start: the run tool's switch is `networks-on` alone, `crate::prefs::THREEMA_RUN_TOOL_ENV`,
/// and a shut start offers no Threema at all).
fn start_prefs(path: &std::path::Path, start: crate::prefs::ThreemaStart) -> Prefs {
    let prefs = Prefs::load_or_default(path);
    if start == crate::prefs::ThreemaStart::Person {
        prefs.with_default_networks()
    } else {
        prefs
    }
}

fn start_networks(
    env: &impl Fn(&str) -> Option<std::ffi::OsString>,
    prefs: &Prefs,
    seams: Seams,
    #[cfg_attr(not(feature = "threema"), allow(unused_variables))]
    threema_start: crate::prefs::ThreemaStart,
) -> Result<StartNetworks, AppError> {
    // The rehearsal switch is read first, so a wrong value fails the start before anything is made.
    // Debug builds only (`dev` exists only there): a release build with the test network, which is
    // what `cargo xtask perf` measures, never reads it. A debug build WITHOUT a rehearsal network
    // refuses a set switch (TR R0a fix G-5): the real backend must not run while the person
    // believes it is a rehearsal.
    #[cfg(debug_assertions)]
    crate::dev::rehearsal_refused(env).map_err(|e| AppError::Config(ConfigError::Rehearsal(e)))?;
    #[cfg(all(
        debug_assertions,
        feature = "fake",
        any(feature = "whatsapp", feature = "telegram", feature = "threema")
    ))]
    let rehearsal = crate::dev::rehearsal_requested(env)
        .map_err(|e| AppError::Config(ConfigError::Rehearsal(e)))?;
    #[cfg(not(all(
        debug_assertions,
        feature = "fake",
        any(feature = "whatsapp", feature = "telegram", feature = "threema")
    )))]
    let rehearsal = Rehearsal::None;
    let mut registry = NetworkRegistry::new();
    #[cfg(feature = "signal")]
    {
        let value = env(HISTORY_TRANSFER_ENV);
        if !history_transfer_offered(value.as_deref()) {
            tracing::warn!(
                "history transfer at link time is switched OFF for this run (KOURIER_HISTORY_TRANSFER=0): a new link does not offer it to the phone"
            );
        }
        register(&mut registry, Arc::new(signal_network(value.as_deref())));
    }
    #[cfg(feature = "fake")]
    let fake = {
        let demo = demo_network(&FixtureOptions {
            link: true,
            // Debug builds: the state the demo account ends in (tests of the account states
            // on a real profile, e.g. a challenge answered from a second launch).
            #[cfg(debug_assertions)]
            state: crate::dev::demo_state(env),
            ..FixtureOptions::default()
        })?;
        register(&mut registry, Arc::new(demo.network.clone()));
        Some(demo.network)
    };
    // Telegram's servers switch is read before anything is made, so a wrong value fails the
    // start first (debug + run profile only: `crate::telegram::servers`).
    #[cfg(feature = "telegram")]
    let seams = {
        let servers = crate::telegram::servers(seams.build == Build::Debug, env)?;
        seams.with_telegram_servers(servers)
    };
    // Which IDs a Threema link may bring is settled before the seams are used
    // (`crate::threema::allowed_ids`: any, except in a start by the run tool).
    #[cfg(feature = "threema")]
    let threema_allowed = crate::threema::allowed_ids(seams.build == Build::Debug, env);
    let experimental = compiled_in_experimental(
        seams,
        prefs,
        rehearsal,
        #[cfg(feature = "threema")]
        threema_start,
        #[cfg(feature = "threema")]
        threema_allowed,
    );
    let real_whatsapp = experimental.real_whatsapp;
    let telegram_missing = experimental.telegram_missing;
    let networks_off = register_experimental(&mut registry, prefs, experimental.networks);
    let whatsapp_real = real_whatsapp && registry.get(NetworkKind::WhatsApp).is_some();
    Ok(StartNetworks {
        registry,
        networks_off,
        #[cfg(feature = "fake")]
        fake,
        #[cfg(feature = "fake")]
        rehearsal: experimental.rehearsal,
        whatsapp_real,
        telegram_missing,
    })
}

/// The first word of the line [`networks_line`] makes (`cargo xtask dist` reads the line from the
/// packed binary; a test of `xtask` compares).
const NETWORKS_LINE_HEAD: &str = "networks";

/// The line `kourier --print-networks` (hidden; not in the usage text) prints and exits with: the
/// KINDS of the networks a normal start of `profile` would register, those it would list as off,
/// and those `networks-on` of the profile's `kourier.toml` names, as
/// `networks registered=<kinds> off=<kinds> on=<kinds>` (kinds by their stored names, separated by
/// commas, a list may be empty). It resolves the profile the way a start does (`KOURIER_HOME`, the
/// XDG directories), reads `kourier.toml` and builds the registry through the one function a start
/// uses ([`start_networks`]); it opens no window, no database and no keyring and contacts nothing.
/// `cargo xtask dist` runs it on the packed (release) binary with `networks-on = ["whatsapp",
/// "threema"]`, which is the only proof that a release build takes the release arm of
/// [`whatsapp_registration`] (the archive builds Threema in since `4d03eb2`, and a person's start
/// of a release build offers it; the run tool's start keeps the release arm that the unit tests
/// of `threema_registration` prove).
pub fn networks_line(profile: Profile) -> Result<String, AppError> {
    let paths = Paths::resolve(profile)?;
    networks_line_in(&paths, |k| std::env::var_os(k), Seams::probe())
}

/// [`networks_line`] over `paths`, reading the environment through `env`, with the parts a test
/// replaces ([`Seams`]).
fn networks_line_in(
    paths: &Paths,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
    seams: Seams,
) -> Result<String, AppError> {
    // `--print-networks` has no dev control: only the profile's marker can shut Threema out here.
    let threema_start = crate::prefs::threema_start_of(&env, false, paths);
    let prefs = start_prefs(&paths.config_file(), threema_start);
    #[cfg(feature = "telegram")]
    let seams = seams.with_telegram_files_root(paths);
    let networks = start_networks(&env, &prefs, seams, threema_start)?;
    let kinds = |kinds: Vec<NetworkKind>| {
        kinds
            .iter()
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join(",")
    };
    Ok(format!(
        "{NETWORKS_LINE_HEAD} registered={} off={} on={}",
        kinds(networks.registry.kinds().collect()),
        kinds(networks.networks_off.iter().map(|n| n.kind).collect()),
        kinds(
            NetworkKind::ALL
                .into_iter()
                .filter(|k| prefs.network_on(*k))
                .collect()
        ),
    ))
}

/// This profile's master key from the Secret Service (created for a fresh profile).
async fn secret_service_keyring(paths: &Paths) -> Result<Keyring<SecretService>, AppError> {
    let store = SecretService::connect(false)
        .await
        .map_err(|e| AppError::NoSecretService(KeyError::Secret(e)))?;
    Keyring::with_secret_service(store, &paths.secret_profile(), !paths.has_data())
        .await
        .map_err(|e| match e {
            KeyError::Secret(_) => AppError::NoSecretService(e),
            other => AppError::Keys(other),
        })
}

fn register(registry: &mut NetworkRegistry, network: Arc<dyn kourier_backend_api::Network>) {
    if let Err(e) = registry.register(network) {
        tracing::warn!(error = %e, "network registered twice; ignored");
    }
}

#[cfg(not(feature = "fake"))]
fn fixture_setup(_: &FixtureOptions) -> Result<Setup, AppError> {
    Err(AppError::FixtureUnavailable)
}

/// A throwaway profile below a temporary directory, a passphrase keyring with a fixed
/// passphrase (cheap KDF: nothing here is secret) and only the fake network.
#[cfg(feature = "fake")]
fn fixture_setup(f: &FixtureOptions) -> Result<Setup, AppError> {
    let home = tempfile::Builder::new()
        .prefix("kourier-fixture-")
        .tempdir()
        .map_err(AppError::Dirs)?;
    let home_os = home.path().as_os_str().to_owned();
    let paths = Paths::from_env(Profile::default(), |k| {
        (k == crate::paths::HOME_ENV).then(|| home_os.clone())
    })?;
    paths.create_dirs().map_err(AppError::Dirs)?;
    crate::logging::init(Some(&paths.log_dir()));
    let kdf = crate::keys::KdfParams {
        m_kib: 64,
        t: 1,
        p: 1,
    };
    let (keyring, _) = Keyring::create_passphrase(b"kourier fixture", kdf)?;
    let demo = demo_network(f)?;
    let extra_whatsapp = demo.extra.as_ref().and_then(|e| e.whatsapp.clone());
    let mut registry = NetworkRegistry::new();
    register(&mut registry, Arc::new(demo.network.clone()));
    if let Some(whatsapp) = &extra_whatsapp {
        register(&mut registry, Arc::new(whatsapp.clone()));
    }
    // This build holds an experimental network, as the release archive will, and the fixture
    // shows it the way a person who has not turned it on sees it: a second fake that plays
    // WhatsApp, off and without an account (no list moves for it). A fixture whose own account
    // IS a WhatsApp account has it on: the preference says so before the desktop task reads it.
    let networks_off = match f.network {
        // (More accounts, 3: the second fake that plays WhatsApp is REGISTERED, has an account
        // and is on: `networks-on` is written before the desktop task reads it, as for the
        // WhatsApp fixture.)
        FixtureNetwork::Fake if extra_whatsapp.is_some() => {
            let mut prefs = Prefs::default();
            prefs.set_network_on(NetworkKind::WhatsApp, true);
            prefs
                .save(&paths.config_file())
                .map_err(|e| AppError::Fixture(e.to_string()))?;
            Vec::new()
        }
        FixtureNetwork::Fake => {
            let whatsapp = kourier_fake::FakeNetwork::playing(
                NetworkKind::WhatsApp,
                kourier_fake::Script::new("whatsapp, turned off", Vec::new()),
            );
            let mut off = vec![info_of(&whatsapp)];
            // The page `tgwarn` only (`FixtureOptions::telegram_off`): Telegram listed as off
            // beside it, so the warning opens from the Welcome list.
            if f.telegram_off {
                let telegram = kourier_fake::FakeNetwork::playing(
                    NetworkKind::Telegram,
                    kourier_fake::Script::new("telegram, turned off", Vec::new()),
                );
                off.push(info_of(&telegram));
            }
            off
        }
        FixtureNetwork::WhatsApp | FixtureNetwork::Threema | FixtureNetwork::Telegram => {
            let mut prefs = Prefs::default();
            let kind = match f.network {
                FixtureNetwork::Threema => NetworkKind::Threema,
                FixtureNetwork::Telegram => NetworkKind::Telegram,
                _ => NetworkKind::WhatsApp,
            };
            prefs.set_network_on(kind, true);
            prefs
                .save(&paths.config_file())
                .map_err(|e| AppError::Fixture(e.to_string()))?;
            Vec::new()
        }
        // (The seed needs the network registered; `App::start` leaves a network of the off list
        // out of what the UI is told is on.)
        FixtureNetwork::WhatsAppOff => vec![info_of(&demo.network)],
    };
    // The fixture's messages carry fixed dates: the core's clock starts a little after the
    // newest one, so recent own messages are inside the edit and delete windows and timers
    // have not run out (with the system clock everything would be months old).
    let mut fixture_pin = None;
    let clock: Arc<dyn Clock> = match demo.now_ms {
        Some(now) if demo.held_send => {
            let at_ms = held_send_clock_start(now);
            let clock = crate::fixture_clock::FixtureClock::starting_at(at_ms);
            // The upload pages' messages are sent with the clock frozen at that instant (the
            // lines under them show the time since, and the start's length must not move it).
            if demo.uploads_peer.is_some() {
                fixture_pin = Some(FixturePin {
                    clock: clock.clone(),
                    at_ms,
                });
            }
            Arc::new(clock)
        }
        // A page whose window sends a message after the start (`--page=media`): the clock that
        // stamps it starts early in the minute of "now", so the stamp's minute does not follow
        // how long the start took.
        Some(now) if f.window_sends => Arc::new(crate::fixture_clock::FixtureClock::starting_at(
            window_send_clock_start(now),
        )),
        Some(now) => Arc::new(crate::fixture_clock::FixtureClock::starting_at(now)),
        None => Arc::new(SystemClock),
    };
    Ok(Setup {
        paths,
        keyring: AnyKeyring::Passphrase(keyring),
        registry,
        networks_off,
        pending_link: None,
        backend: BackendConfig::default(),
        fake: Some(demo.network),
        rehearsal: None,
        whatsapp_real: false,
        telegram_missing: f.telegram_missing,
        default_networks: false,
        home: Some(home),
        avatars: demo.avatars,
        clock,
        fixture_now: demo.now_ms,
        fixture_seed: demo.seed_steps,
        fixture_calls: demo.calls_peer,
        fixture_unavailable: demo.unavailable_peer,
        fixture_person: demo.person_peer,
        fixture_safety: demo.safety_peer,
        fixture_safety_default: demo.safety_default_peer,
        fixture_group: demo.group,
        fixture_note: demo.note_peer,
        fixture_whatsapp: demo.whatsapp_peer,
        fixture_uploads: demo.uploads_peer,
        fixture_second_link: demo.second_link,
        fixture_extra: demo.extra,
        fixture_pin,
        #[cfg(feature = "perf")]
        perf_seed: None,
    })
}

/// The reference profile of the quality targets below `p.home`: a passphrase keyring with a
/// fixed passphrase, only the fake network with the reference script, the system clock
/// ([`crate::perf`]).
#[cfg(feature = "perf")]
fn perf_setup(p: &crate::perf::PerfOptions) -> Result<Setup, AppError> {
    let fresh = crate::perf::is_fresh(p);
    let paths = crate::perf::paths(p)?;
    paths.create_dirs().map_err(AppError::Dirs)?;
    crate::logging::init(Some(&paths.log_dir()));
    let keyring = crate::perf::open_keyring(&paths.config)?;
    let net = crate::perf::network(p, fresh)?;
    let mut registry = NetworkRegistry::new();
    register(&mut registry, Arc::new(net.network.clone()));
    Ok(Setup {
        paths,
        keyring: AnyKeyring::Passphrase(keyring),
        registry,
        networks_off: Vec::new(),
        pending_link: None,
        backend: BackendConfig::default(),
        fake: Some(net.network),
        rehearsal: None,
        whatsapp_real: false,
        telegram_missing: None,
        default_networks: false,
        home: None,
        avatars: Default::default(),
        clock: crate::perf::clock(p),
        fixture_now: None,
        fixture_seed: None,
        fixture_calls: None,
        fixture_unavailable: None,
        fixture_person: None,
        fixture_safety: None,
        fixture_safety_default: None,
        fixture_group: None,
        fixture_note: None,
        fixture_whatsapp: None,
        fixture_uploads: None,
        fixture_second_link: None,
        fixture_extra: None,
        fixture_pin: None,
        perf_seed: net.seeding.map(|reference| (reference, p.home.clone())),
    })
}

/// How long after its newest message fixture mode's "now" is.
#[cfg(feature = "fake")]
const FIXTURE_NOW_AFTER_NEWEST_MS: i64 = 7 * 60_000;

/// Where the core's clock starts for a page that queues an own message while the account's
/// send hold stands (`--page=walock`): five seconds into the minute that ends two minutes
/// before the fixture's "now". The UI's "now" does not run, so the message (stamped by the
/// core's clock a few seconds after the start) is about two minutes old in the picture, past
/// the three seconds after which a waiting message shows its line, and the clock time under it
/// is the same in every run (a start that takes under 55 s stays in its minute).
#[cfg(feature = "fake")]
fn held_send_clock_start(now_ms: i64) -> i64 {
    early_in_minute(now_ms - 2 * 60_000)
}

/// Where the core's clock starts for a page whose window sends a message after the start
/// ([`FixtureOptions::window_sends`], `--page=media`): five seconds into the minute of the
/// fixture's "now". The core stamps the message with its clock when the window presses Send,
/// some seconds after the start; with the clock started at "now" the minute under the message
/// followed the seconds of "now" and how long the start took (05:23 in one render, 05:24 in
/// another). From here the stamp stays in the minute of "now" for any send within 55 s of the
/// start (the render's own limit is 60 s).
#[cfg(feature = "fake")]
fn window_send_clock_start(now_ms: i64) -> i64 {
    early_in_minute(now_ms)
}

/// The instant five seconds into the minute that holds `at_ms`.
#[cfg(feature = "fake")]
fn early_in_minute(at_ms: i64) -> i64 {
    const MINUTE: i64 = 60_000;
    at_ms.div_euclid(MINUTE) * MINUTE + 5_000
}

/// The core's clock of an upload page ([`FixtureSends::Uploading`] with files sent) and the
/// instant it is frozen at while the page's files are sent: the lines under the messages show
/// the time since they were sent, and without the freeze the digits would follow how long the
/// start took (`seed_fixture`, `queue_uploads`).
#[cfg(feature = "fake")]
#[derive(Clone)]
struct FixturePin {
    clock: crate::fixture_clock::FixtureClock,
    at_ms: i64,
}

/// The peers of the generated chats that a fixture page opens (the showcase chats have fixed
/// peers of their own).
#[cfg(feature = "fake")]
struct PagePeers {
    /// The chat with calls (`--page=calls`).
    calls: Option<kourier_model::UserId>,
    /// The chat of attachments that cannot be had (`--page=unavailable`).
    unavailable: Option<kourier_model::UserId>,
    /// The chat of the person page (`--page=person`).
    person: Option<kourier_model::UserId>,
    /// The chat of the second person of the safety steps ([`FixtureOptions::safety_peer`]).
    safety: Option<kourier_model::UserId>,
    /// The chat of the third person of the safety steps (never verified).
    safety_default: Option<kourier_model::UserId>,
    /// The group of the group page (`--page=group`).
    group: Option<crate::fixture_media::FixtureGroup>,
    /// The Note to Self chat ([`FixtureOptions::note_to_self`]).
    note_to_self: Option<kourier_model::UserId>,
    /// The chat of the pages of WhatsApp's states (`--page=walock`, `wahistory`, `wafailed`).
    whatsapp: Option<kourier_model::UserId>,
    /// The chat whose messages with files upload slowly ([`FixtureSends::Uploading`]).
    uploads: Option<kourier_model::UserId>,
    /// What the page puts into that chat once the account is seeded.
    upload: Option<FixtureUpload>,
    /// Send an own message into that chat once the account's send hold stands, so it waits
    /// (`--page=walock`).
    held_send: bool,
    /// The upload pages' clock ([`FixturePin`]).
    pin: Option<FixturePin>,
    /// The accounts linked after the fixture account ([`FixtureOptions::accounts`]).
    extra: Option<crate::fixture_accounts::Extra>,
}

/// The fake network of fixture mode and what belongs to its data.
#[cfg(feature = "fake")]
struct Demo {
    network: kourier_fake::FakeNetwork,
    /// Profile pictures to write once the account is linked.
    avatars: crate::fixture_media::FixtureAvatars,
    /// "Now" for this data: a little after its newest message.
    now_ms: Option<i64>,
    /// The steps of the script that are the fixture data, when more follow
    /// ([`FixtureOptions::arrivals`]): a start waits for these only.
    seed_steps: Option<usize>,
    /// The peer of the chat with calls (`--page=calls`).
    calls_peer: Option<kourier_model::UserId>,
    /// The peer of the chat of attachments that cannot be had (`--page=unavailable`).
    unavailable_peer: Option<kourier_model::UserId>,
    /// The peer of the person page (`--page=person`).
    person_peer: Option<kourier_model::UserId>,
    /// The second person of the safety steps ([`FixtureOptions::safety_peer`]).
    safety_peer: Option<kourier_model::UserId>,
    /// The third person of the safety steps: never verified, her first send refused as untrusted.
    safety_default_peer: Option<kourier_model::UserId>,
    /// The group of the group page (`--page=group`).
    group: Option<crate::fixture_media::FixtureGroup>,
    /// The Note to Self chat ([`FixtureOptions::note_to_self`]).
    note_peer: Option<kourier_model::UserId>,
    /// The peer of the chat of the pages of WhatsApp's states ([`FixtureOptions::whatsapp`]).
    whatsapp_peer: Option<kourier_model::UserId>,
    /// The peer of the chat whose uploads are slow ([`FixtureSends::Uploading`]).
    uploads_peer: Option<kourier_model::UserId>,
    /// An own message is sent into that chat once the account's send hold stands
    /// (`--page=walock`), and the core's clock starts [`HELD_SEND_CLOCK_BEHIND_MS`] behind
    /// the fixture's "now", so the message is older than the line's three seconds.
    held_send: bool,
    /// The script the next account added after the seeding gets
    /// ([`FixtureOptions::second_link`]).
    second_link: Option<kourier_fake::Script>,
    /// The accounts [`FixtureOptions::accounts`] adds.
    extra: Option<crate::fixture_accounts::Extra>,
}

/// The fake network with the fixture data set: an automatic login for seeded fixture mode
/// (with the fixture's media and the profile pictures to write once linked), otherwise the
/// device-name question and rotating QR codes, the last one "scanned".
#[cfg(feature = "fake")]
fn demo_network(f: &FixtureOptions) -> Result<Demo, AppError> {
    use kourier_fake::fixture::{FixtureSpec, FixtureState as FakeState, fixture};
    use kourier_fake::{FakeNetwork, Flavor, LoginScript};
    let spec = FixtureSpec {
        messages_per_conversation: f.messages_per_conversation,
        contacts: f.contacts,
        groups: f.groups,
        showcase: f.showcase,
        state: match f.state {
            FixtureState::Online => FakeState::Online,
            FixtureState::Offline => FakeState::Offline,
            FixtureState::Connecting => FakeState::Connecting,
            FixtureState::Challenge => FakeState::Challenge,
            FixtureState::ClockSkew => FakeState::ClockSkew,
            FixtureState::Reconnect => FakeState::Reconnect,
            FixtureState::LoggedOut => FakeState::LoggedOut,
        },
        ..FixtureSpec::default()
    };
    let mut script = fixture(&spec).map_err(|e| AppError::Fixture(e.to_string()))?;
    // A network that rejects the person's changes (a test knob; no page sets it).
    script.reject_changes = f.reject_changes;
    let now_ms =
        crate::fixture_media::newest_message(&script).map(|t| t + FIXTURE_NOW_AFTER_NEWEST_MS);
    let mut avatars = crate::fixture_media::FixtureAvatars::default();
    if f.showcase {
        script = crate::fixture_media::with_showcase_extras(script);
        if let Some(timer) = f.expiring {
            script = crate::fixture_media::with_expiring(script, timer);
        }
    }
    let mut seed_steps = None;
    let mut calls_peer = None;
    let mut unavailable_peer = None;
    let mut person_peer = None;
    let mut safety_peer = None;
    let mut safety_default_peer = None;
    let mut group = None;
    let mut note_peer = None;
    let mut whatsapp_peer = None;
    let mut uploads_peer = None;
    if f.showcase
        && let Some(now) = now_ms
    {
        (script, calls_peer) = crate::fixture_media::with_calls(
            script,
            now,
            crate::fixture_media::CallsOptions {
                unread: f.calls_unread,
                ring_for: f.calls_ring_for,
            },
        );
        (script, unavailable_peer) =
            crate::fixture_media::with_unavailable(script, calls_peer.as_ref());
        // The person of `--page=person`: a contact of the generated chats, no chat added.
        (script, person_peer) = crate::fixture_media::with_person(
            script,
            &[calls_peer.as_ref(), unavailable_peer.as_ref()],
            match f.person_groups {
                0 => crate::fixture_media::PERSON_GROUPS,
                n => usize::from(n),
            },
        );
        // Her safety number (P5a): one synthetic key in state Default, so her page offers "View
        // Safety Number" with a number and the level can Mark and Clear it. Added after the
        // generated stream (no random value drawn, no chat added).
        if let Some(p) = &person_peer {
            script = script.with_safety(
                p.clone(),
                crate::fixture_media::person_safety(f.safety_changed),
            );
        }
        // The second person of the safety steps (critic F15): a contact with a 1:1 chat that no
        // page uses, safety numbers in state Changed and her first send refused as untrusted.
        if f.safety_peer {
            (script, safety_peer) = crate::fixture_media::with_safety_peer(
                script,
                &[
                    calls_peer.as_ref(),
                    unavailable_peer.as_ref(),
                    person_peer.as_ref(),
                ],
            );
            // A third person, never verified (state Default), whose first send is refused as
            // untrusted too: a send to her pauses and only Confirm (an approval) lets it go (U6).
            (script, safety_default_peer) = crate::fixture_media::with_safety_default_peer(
                script,
                &[
                    calls_peer.as_ref(),
                    unavailable_peer.as_ref(),
                    person_peer.as_ref(),
                    safety_peer.as_ref(),
                ],
            );
        }
        // The group of `--page=group`: a group other than the newest, after the person's
        // deltas (they are in its member count), no chat added.
        (script, group) = crate::fixture_media::with_group(
            script,
            &[
                calls_peer.as_ref(),
                unavailable_peer.as_ref(),
                person_peer.as_ref(),
            ],
            usize::from(f.group_members),
        );
        if f.group_timers
            && let Some(g) = &group
        {
            script = crate::fixture_media::with_group_timers(script, &g.key);
        }
        if f.group_admin
            && let Some(g) = &group
        {
            script = crate::fixture_media::with_group_admin(script, &g.key);
        }
        if f.group_mentions
            && let Some(g) = &group
        {
            script = crate::fixture_media::with_group_mentions(script, g, person_peer.as_ref());
        }
        if f.note_to_self {
            (script, note_peer) = crate::fixture_media::with_note_to_self(script);
        }
        // The fixture account is a Signal account: its phone can be asked for the files the
        // server lost (the gone ones of the `unavailable` page offer Download), and says
        // nothing, as a phone that is not at hand.
        script = script.with_backfill(kourier_fake::BackfillScript::Silent);
    }
    if !f.link {
        (script, avatars) = crate::fixture_media::with_fixture_media(script);
        // What arrives later comes after everything a start waits for.
        if let (Some(arrivals), Some(now), true, FixtureState::Online) =
            (f.arrivals, now_ms, f.showcase, f.state)
        {
            script = script.step(kourier_fake::Step::WaitAcked);
            seed_steps = Some(script.steps.len());
            script = crate::fixture_media::with_arrivals(script, arrivals, now);
        }
    }
    // The pages of WhatsApp's own states: a ban, the reach-out lock, a history that goes on,
    // an own message refused as a first contact (K-A).
    if let (Some(page), Some(now)) = (f.whatsapp, now_ms)
        && !f.link
    {
        let taken = [
            calls_peer.as_ref(),
            unavailable_peer.as_ref(),
            person_peer.as_ref(),
        ];
        let (with, peer, steps) =
            crate::fixture_media::with_whatsapp_page(script, page, now, &taken);
        script = with;
        whatsapp_peer = peer;
        seed_steps = seed_steps.or(steps);
    }
    if f.link {
        // The link of a held page waits at its first code; the walk by hand rotates the codes
        // and the last one is scanned.
        let threema = f.network == FixtureNetwork::Threema;
        let telegram = f.network == FixtureNetwork::Telegram;
        // A held link of Threema's `themoji` page scans its first code at once and stands at the
        // emoji; every other held link stands at its first code (the second is never reached).
        let at_emoji = f.hold_link && f.hold_emoji && threema;
        let (codes, valid_for) = if at_emoji {
            (1, FixtureOptions::HELD_CODE)
        } else if f.hold_link {
            (2, FixtureOptions::HELD_CODE)
        } else if threema {
            // The real Threema actor shows ONE code for the whole link (`valid_for: None`, and the
            // page does not say that it changes): the fake that plays Threema walks one code too,
            // whatever `qr_codes` says (U5). (A held link keeps the first of its two codes on the
            // page: the second is the one the phone scans, and is never reached.)
            (1, f.qr_valid_for)
        } else {
            (f.qr_codes.max(1), f.qr_valid_for)
        };
        if at_emoji {
            script = script.with_emoji_for(FixtureOptions::HELD_CODE);
        }
        let whatsapp = matches!(
            f.network,
            FixtureNetwork::WhatsApp | FixtureNetwork::WhatsAppOff
        );
        script.login = LoginScript::Qr {
            // (WhatsApp names no device: the phone shows "Chrome (Linux)". Nor does Threema: the
            // phone lists the device as "Kourier (<host name>)". Nor does Telegram: the real
            // backend asks no device name, its session is named from the host, KT-1b finding 6:
            // with one asked, `tgqr` would stand at an input step.)
            ask_device_name: !whatsapp && !threema && !telegram,
            codes,
            valid_for,
            scanned_at: Some(codes - 1),
            linked_as: None,
        };
        if telegram {
            // The phone-number way: the account's row (code through the Telegram app, a new code
            // offered AT ONCE so the picture shows the button, two-step password with a hint).
            // A held page never types the number itself: `MainWindow.qml` does.
            use kourier_model::{CodeDelivery, CodeResend, CodeVia};
            let delivery = CodeDelivery {
                via: CodeVia::App,
                resend: Some(CodeResend {
                    after: Duration::ZERO,
                    via: CodeVia::Sms,
                }),
            };
            script = script.with_phone_login(kourier_fake::PhoneLoginScript::new(vec![
                kourier_fake::PhoneAccount::new(
                    FixtureOptions::TELEGRAM_NUMBER,
                    FixtureOptions::TELEGRAM_CODE,
                    delivery,
                )
                .with_password(
                    FixtureOptions::TELEGRAM_PASSWORD,
                    Some(FixtureOptions::TELEGRAM_HINT),
                ),
            ]));
        }
        if whatsapp {
            // The phone-number way: the code the phone is told to type (shown XXXX-XXXX),
            // typed ten seconds after it was shown, or never on a held page.
            let code = kourier_fake::PairCodeScript::new("ABCD1234");
            script = script.with_pair_code(if f.hold_link {
                code.never_entered()
            } else {
                code
            });
        }
    }
    // The history transfer: walked or held by the link, or left where a restart found it.
    {
        use kourier_fake::{Step, TransferScript};
        use kourier_model::{ConnState, HistoryTransfer};
        match (f.transfer, f.link) {
            (Some(FixtureTransfer::Walk(end)), true) => {
                let stage = f.transfer_stage;
                script = script.with_transfer(match end {
                    HistoryTransfer::Failed(kind) => TransferScript::failing(stage, kind),
                    HistoryTransfer::Skipped => TransferScript::ending(stage, end),
                    _ => TransferScript::walk(stage, HistoryTransfer::Done),
                });
            }
            (Some(FixtureTransfer::Hold(state)), true) => {
                script = script.with_transfer(TransferScript::holding(state));
            }
            (Some(FixtureTransfer::Left(state)), false) => {
                if !state.is_over() {
                    // (What a backend reports while its transfer holds receiving back.)
                    script = script.step(Step::Conn(ConnState::Syncing { fraction: None }));
                }
                script = script.step(Step::Transfer(state));
            }
            _ => {}
        }
    }
    // The update after a link that is still going on: profiles and groups left.
    // (Reports are not stored: they come after the seeding, once a second, so that the UI,
    // which subscribes after it, sees the most left and then what is left now.)
    if f.updating && !f.link {
        use kourier_backend_api::BackendEvent;
        use kourier_fake::Step;
        use kourier_model::AccountProgress;
        if seed_steps.is_none() {
            // A start waits for the data to be ACKED by the core, as the data of every other
            // page is (their script ends, or `arrivals` above waits): the steps having run
            // is not enough, a list read at that moment has some chats not yet in their final
            // place (the first group of the list, which the page opens, was Garden Swap in
            // most renders and another chat in some) and the account's profile picture is
            // not there yet (the rail's picture or its initials).
            script = script.step(Step::WaitAcked);
            seed_steps = Some(script.steps.len());
        }
        for _ in 0..30 {
            script = script.step(Step::Sleep(Duration::from_secs(1)));
            for (profiles_left, groups_left) in [(160, 12), (120, 8)] {
                script = script.step(Step::Ephemeral(Box::new(BackendEvent::AccountProgress(
                    AccountProgress {
                        profiles_left,
                        groups_left,
                        ..AccountProgress::default()
                    },
                ))));
            }
        }
    }
    match f.sends {
        Some(FixtureSends::Slow(phase)) => script = script.slow_sends(phase, FixtureSends::SLOW),
        Some(FixtureSends::Uploading(upload)) if !f.link => {
            let taken = [
                calls_peer.as_ref(),
                unavailable_peer.as_ref(),
                person_peer.as_ref(),
            ];
            (script, uploads_peer) = crate::fixture_media::with_uploads(script, upload, &taken);
        }
        Some(FixtureSends::Uploading(_)) => {}
        Some(FixtureSends::RateLimited(wait)) => {
            script = script.fail_transmit(kourier_fake::TransmitRule::attempts(
                kourier_model::BackendError::new(
                    kourier_model::ErrorKind::RateLimited {
                        retry_after: Some(wait),
                    },
                    "fixture: rate limited",
                ),
                1,
            ));
        }
        None => {}
    }
    if f.logout_unreachable {
        script.logout_unreachable = true;
        script.device_name = Some("Kourier fixture".to_owned());
    }
    // The second link: the same identity (never scanned, or scanned and refused by the core),
    // or another account linked at once.
    let other = kourier_model::UserId::parse("aci:00000000-0000-4000-8000-0000000f2000")
        .map_err(|e| AppError::Fixture(e.to_string()))?;
    let second_link = f.second_link.filter(|_| !f.link).map(|kind| {
        let own_ids = if kind == FixtureSecondLink::OtherAccount {
            vec![other]
        } else {
            script.own_ids.clone()
        };
        let mut second = kourier_fake::Script::new("second link", own_ids);
        second.login = LoginScript::Qr {
            ask_device_name: false,
            codes: 1,
            valid_for: Duration::from_secs(60),
            scanned_at: match kind {
                FixtureSecondLink::AtQr => None,
                FixtureSecondLink::SameAccount | FixtureSecondLink::OtherAccount => Some(0),
            },
            linked_as: None,
        };
        second
    });
    // More accounts (blended chats): only the plain fixture, linked at the start.
    let extra = if f.link || f.network != FixtureNetwork::Fake {
        None
    } else {
        crate::fixture_accounts::extra_accounts(f.accounts, &script)?
    };
    let mut network = match f.network {
        // The plain fixture account keeps the fake's default statement: its network writes the
        // settings it keeps for the account's other devices (archive, pin, mute, marked unread) and
        // its switch is on (the core refuses such a click of an account that does not, and the
        // archive page's phone sends them). The pages that show the other states of the switch
        // are A10d's.
        FixtureNetwork::Fake => FakeNetwork::new(Flavor::SignalLike, script),
        // The same script, the same ids: the same chats, row for row.
        // Every chat states WhatsApp's features (W5b, L8: the composer offers what WhatsApp's does).
        FixtureNetwork::WhatsApp | FixtureNetwork::WhatsAppOff => FakeNetwork::playing(
            NetworkKind::WhatsApp,
            kourier_fake::scripts::as_whatsapp(script),
        ),
        // The same script and ids, the chats as the plain fixture's (TH4 KH1c: the pages are the
        // link's, no chat of Threema's own is shown). Threema writes no roaming setting yet, so
        // its account states none (`as_whatsapp` does the same for WhatsApp: W6 turns both on).
        FixtureNetwork::Threema => {
            FakeNetwork::playing(NetworkKind::Threema, script.with_roaming_writes(None))
        }
        // The same script and ids (KT-1b: the pages are the link's and Settings'): Telegram's
        // roaming settings are not written either (TG-3 states none).
        FixtureNetwork::Telegram => {
            FakeNetwork::playing(NetworkKind::Telegram, script.with_roaming_writes(None))
        }
    };
    if (f.history_on_link || f.transfer.is_some()) && f.network == FixtureNetwork::Fake {
        network = network
            .with_history(kourier_model::HistorySource::PushedOnLink)
            .with_history_transfer(true);
    }
    // (`--page=walock`: the chat of the page gets the message that waits for the hold.)
    // (`--page=uploading`, `uploadfile`: the same clock, so the message that is being sent is
    // two minutes old in the picture and its line has shown for good.)
    let held_send = f.whatsapp == Some(FixtureWhatsApp::Locked) && whatsapp_peer.is_some()
        || matches!(
            f.sends,
            Some(FixtureSends::Uploading(
                FixtureUpload::Album | FixtureUpload::File
            ))
        ) && uploads_peer.is_some();
    Ok(Demo {
        network,
        avatars,
        now_ms,
        seed_steps,
        calls_peer,
        unavailable_peer,
        person_peer,
        safety_peer,
        safety_default_peer,
        group,
        note_peer,
        whatsapp_peer,
        uploads_peer,
        held_send,
        second_link,
        extra,
    })
}

/// Links the fixture account and waits until the fake delivered everything and the core acked
/// it (the first `seed_steps` steps of a script that goes on after its data); returns a fixed
/// "now" just after the newest message and the first group.
#[cfg(feature = "fake")]
async fn seed_fixture(
    core: &CoreHandle,
    fake: &kourier_fake::FakeNetwork,
    paths: &Paths,
    avatars: &crate::fixture_media::FixtureAvatars,
    now_ms: Option<i64>,
    seed_steps: Option<usize>,
    peers: PagePeers,
) -> Result<FixtureInfo, AppError> {
    use kourier_backend_api::Network as _;
    use kourier_core::facade::ConversationListQuery;
    use kourier_model::ConvKind;
    const SEED_TIMEOUT: Duration = Duration::from_secs(120);
    let control = fake.control();
    core.send(UiCommand::AddAccount {
        network: fake.kind(),
        flow: "qr".into(),
    })?;
    let started = std::time::Instant::now();
    let seeded = |c: &kourier_fake::FakeControl| {
        !c.finished().is_empty() || seed_steps.is_some_and(|steps| c.ran_steps(steps))
    };
    tokio::time::timeout(SEED_TIMEOUT, control.wait_for(seeded))
        .await
        .map_err(|_| AppError::Fixture("the fixture data did not arrive in time".into()))?;
    let page = match core
        .query(Query::Conversations(ConversationListQuery {
            limit: 500,
            ..ConversationListQuery::default()
        }))
        .await?
    {
        QueryData::Conversations(p) => p,
        _ => return Err(AppError::Fixture("unexpected query answer".into())),
    };
    // The profile pictures the fake's contacts point at, in the account's blob directory.
    if let QueryData::Accounts(accounts) = core.query(Query::Accounts).await?
        && let Some(account) = accounts.first()
    {
        avatars
            .write(&paths.blobs_dir(&account.uuid))
            .map_err(|e| AppError::Fixture(format!("fixture avatars: {e}")))?;
    }
    let newest = page.rows.iter().map(|r| r.last_activity).max().unwrap_or(0);
    let newest_sent = page
        .rows
        .iter()
        .filter_map(|r| r.preview.as_ref().map(|p| p.sent_at))
        .max()
        .unwrap_or(newest);
    tracing::info!(
        conversations = page.rows.len(),
        ms = started.elapsed().as_millis(),
        "fixture data ingested"
    );
    let mut media_conversation = None;
    for row in page.rows.iter().filter(|r| r.preview.is_some()) {
        let q = Query::Timeline(kourier_core::facade::TimelineQuery {
            conv: row.id,
            anchor: kourier_core::facade::TimelineAnchor::Newest,
            limit: 10,
        });
        if let QueryData::Timeline(t) = core.query(q).await?
            && t.rows.iter().any(|m| {
                matches!(&m.body, kourier_model::Body::Media { attachments, .. }
                    if attachments.iter().any(|a| a.kind == kourier_model::AttachmentKind::File))
            })
        {
            media_conversation = Some(row.id);
            break;
        }
    }
    // The showcase chats, by their peers.
    let direct = |peer: Option<kourier_model::UserId>| {
        let peer = peer?;
        page.rows
            .iter()
            .find(|r| r.kind == ConvKind::Direct && r.peer.as_ref() == Some(&peer))
            .map(|r| r.id)
    };
    let peer_of = |key: Result<kourier_model::ConvKey, kourier_fake::ScriptError>| {
        key.ok().and_then(|k| k.direct_peer().cloned())
    };
    // `--page=walock`: the account's send hold stands, and an own message to the chat of the
    // page waits for it, so the line under the message is pictured.
    if peers.held_send
        && let Some(conv) = direct(peers.whatsapp.clone())
    {
        queue_held_message(core, conv).await?;
    }
    // `--page=uploading`, `uploadfile`: the files go into the chat of the page and stay in their
    // upload, the rings and the line drawn by what the fake reports.
    let uploads_conv = direct(peers.uploads.clone());
    if let (Some(upload), Some(conv)) = (peers.upload, uploads_conv) {
        queue_uploads(
            core,
            conv,
            upload,
            &paths.archives_dir().join("fixture-upload"),
            peers.pin.as_ref(),
        )
        .await?;
    }
    // More accounts (`FixtureOptions::accounts`): linked last, so every conversation id above
    // is the fixture account's, read before their chats exist.
    if let Some(extra) = &peers.extra {
        crate::fixture_accounts::seed(core, fake, extra).await?;
    }
    Ok(FixtureInfo {
        // The same instant the core's fixture clock started at.
        now_ms: now_ms.unwrap_or(newest_sent + FIXTURE_NOW_AFTER_NEWEST_MS),
        demo_conversation: page
            .rows
            .iter()
            .find(|r| r.kind != ConvKind::Direct)
            .map(|r| r.id),
        media_conversation,
        quotes_conversation: direct(peer_of(kourier_fake::fixture::showcase_quotes())),
        timer_conversation: direct(peer_of(kourier_fake::fixture::showcase_timer())),
        kinds_conversation: direct(crate::fixture_media::kinds_peer()),
        albums_conversation: direct(crate::fixture_media::albums_peer()),
        readmore_conversation: direct(crate::fixture_media::readmore_peer()),
        calls_conversation: direct(peers.calls),
        unavailable_conversation: direct(peers.unavailable),
        person_conversation: direct(peers.person),
        safety_conversation: direct(peers.safety),
        safety_default_conversation: direct(peers.safety_default),
        group_conversation: peers.group.as_ref().and_then(|g| {
            page.rows
                .iter()
                .find(|r| r.kind == ConvKind::Group && r.title.as_deref() == Some(g.title.as_str()))
                .map(|r| r.id)
        }),
        note_to_self_conversation: direct(peers.note_to_self),
        whatsapp_conversation: direct(peers.whatsapp),
        uploads_conversation: uploads_conv,
    })
}

/// Sends the files of an upload page into `conv` ([`FixtureUpload`]) and waits until the first
/// message is in its upload and the core offers its X (the fake has reported its files), and,
/// for the album, until the file queued behind it shows its empty ring. With `pin` the core's
/// clock stands still at the pinned instant from before the first send to the end of this
/// function, so every message carries the same send time whatever the start took (and then runs
/// on from there).
#[cfg(feature = "fake")]
async fn queue_uploads(
    core: &CoreHandle,
    conv: ConversationId,
    upload: FixtureUpload,
    dir: &std::path::Path,
    pin: Option<&FixturePin>,
) -> Result<(), AppError> {
    use kourier_core::facade::{TimelineAnchor, TimelineQuery};
    const WAIT: Duration = Duration::from_secs(60);
    let give_up = std::time::Instant::now() + WAIT;
    let late =
        |what: &str| AppError::Fixture(format!("the upload page: {what} did not happen in time"));
    let files = crate::fixture_media::upload_files(dir, upload)
        .map_err(|e| AppError::Fixture(format!("the upload page's files: {e}")))?;
    if files.is_empty() {
        return Ok(());
    }
    let send = |paths: Vec<std::path::PathBuf>| {
        core.send(UiCommand::SendFiles {
            conv,
            request: Default::default(),
            paths: paths.into(),
            as_file: Vec::new(),
            with_location: Vec::new(),
            caption: None,
            reply_to: None,
        })
    };
    // (`KOURIER_DEV_UPLOAD_PAGE_DELAY_MS`, debug builds: a pause before the send, to show that
    // the picture's digits do not move with the start's length.)
    if cfg!(debug_assertions)
        && let Some(ms) = std::env::var("KOURIER_DEV_UPLOAD_PAGE_DELAY_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
    {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
    // Thaws the clock when this function ends, however it ends.
    struct Thaw<'a>(Option<&'a FixturePin>);
    impl Drop for Thaw<'_> {
        fn drop(&mut self) {
            if let Some(pin) = self.0 {
                pin.clock.thaw();
            }
        }
    }
    let _thaw = Thaw(pin);
    if let Some(pin) = pin {
        pin.clock.freeze_at(pin.at_ms);
    }
    send(files)?;
    // Own messages of the page's chat, newest first: (stoppable, queued behind).
    let own = |t: &kourier_core::facade::TimelinePage| -> Vec<(bool, bool)> {
        t.rows
            .iter()
            .filter(|m| m.outbox.is_some())
            .map(|m| {
                (
                    m.actions.cancel_sending,
                    !m.outbox.as_ref().is_some_and(|o| o.in_flight),
                )
            })
            .collect()
    };
    let read = || async {
        let q = Query::Timeline(TimelineQuery {
            conv,
            anchor: TimelineAnchor::Newest,
            limit: 6,
        });
        match core.query(q).await? {
            QueryData::Timeline(t) => Ok::<_, AppError>(own(&t)),
            _ => Ok(Vec::new()),
        }
    };
    loop {
        // (The first message to leave the queue is the one in flight; it is stoppable once
        // the fake has reported a file.)
        if read().await?.iter().any(|(stoppable, _)| *stoppable) {
            break;
        }
        if std::time::Instant::now() > give_up {
            return Err(late("the first upload"));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if upload == FixtureUpload::Album {
        let document = crate::fixture_media::upload_document(dir, "Budget 2027.xlsx", 4 << 20)
            .map_err(|e| AppError::Fixture(format!("the upload page's file: {e}")))?;
        send(vec![document])?;
        loop {
            if read().await?.iter().filter(|(_, queued)| *queued).count() >= 1 {
                break;
            }
            if std::time::Instant::now() > give_up {
                return Err(late("the queued file"));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    Ok(())
}

/// Sends an own message into `conv` once the account's send hold stands (the script states it
/// last; the core takes it a moment later), and waits until the message is in the timeline
/// with its outbox row: it stays there, held, and the UI draws the line under it.
#[cfg(feature = "fake")]
async fn queue_held_message(core: &CoreHandle, conv: ConversationId) -> Result<(), AppError> {
    use kourier_core::facade::{TimelineAnchor, TimelineQuery};
    const WAIT: Duration = Duration::from_secs(20);
    let give_up = std::time::Instant::now() + WAIT;
    let late = || AppError::Fixture("the send hold of the page did not take effect in time".into());
    loop {
        let held = match core.query(Query::Accounts).await? {
            QueryData::Accounts(rows) => rows.iter().any(|a| a.hold.is_some()),
            _ => false,
        };
        if held {
            break;
        }
        if std::time::Instant::now() > give_up {
            return Err(late());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    core.send(UiCommand::SendText {
        conv,
        text: kourier_model::Text::plain("Hi! Are you free this evening?"),
        reply_to: None,
    })?;
    loop {
        let q = Query::Timeline(TimelineQuery {
            conv,
            anchor: TimelineAnchor::Newest,
            limit: 3,
        });
        if let QueryData::Timeline(t) = core.query(q).await?
            && t.rows.iter().any(|m| m.outbox.is_some())
        {
            return Ok(());
        }
        if std::time::Instant::now() > give_up {
            return Err(late());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod transfer_switch_tests {
    use std::ffi::OsStr;

    use super::{HISTORY_TRANSFER_ENV, history_transfer_offered};

    /// The history transfer of a new link is offered by default and is off only when the
    /// run's environment says exactly `0`: no other value switches it off.
    #[test]
    fn the_history_transfer_is_offered_unless_the_value_is_exactly_zero() {
        assert_eq!(HISTORY_TRANSFER_ENV, "KOURIER_HISTORY_TRANSFER");
        assert!(history_transfer_offered(None));
        for other in [
            "", "1", "true", "false", "no", "off", "00", "0 ", " 0", "01", "10",
        ] {
            assert!(
                history_transfer_offered(Some(OsStr::new(other))),
                "{other:?}"
            );
        }
        assert!(!history_transfer_offered(Some(OsStr::new("0"))));
    }

    /// The Signal network the app builds offers the transfer by default (and says so in its
    /// descriptor, which is what shows "Transfer History from Phone…"), and with the switch
    /// at `0` it offers nothing.
    #[cfg(feature = "signal")]
    #[test]
    fn the_signal_network_of_the_app_offers_the_transfer_by_default() {
        use kourier_backend_api::Network as _;
        use kourier_model::HistorySource;

        use super::signal_network;

        for on in [None, Some(OsStr::new("1")), Some(OsStr::new("yes"))] {
            assert_eq!(
                signal_network(on).descriptor().history,
                HistorySource::PushedOnLink,
                "{on:?}"
            );
        }
        assert_eq!(
            signal_network(Some(OsStr::new("0"))).descriptor().history,
            HistorySource::None
        );
    }

    /// THE WIRING: the registry a normal start builds (`normal_setup_in`, which
    /// `normal_setup` calls with the process environment) holds a Signal network that
    /// offers the transfer by default, and none that offers it when the run's environment
    /// says `0`. A `normal_setup` that registered `SignalNetwork::new()` (the library's
    /// default, offer off) passes every test of `signal_network` and fails this one. Debug
    /// builds only: the test starts from the development keyring, which release builds lack.
    #[cfg(all(debug_assertions, feature = "signal"))]
    #[tokio::test]
    async fn a_normal_start_offers_the_history_transfer_by_default() {
        use std::ffi::OsString;

        use kourier_model::{HistorySource, NetworkKind};

        use super::normal_setup_in;
        use crate::dev::DEV_KEYRING_ENV;
        use crate::paths::{HOME_ENV, Paths, Profile};

        for (switch, expected) in [
            (None, HistorySource::PushedOnLink),
            (Some("1"), HistorySource::PushedOnLink),
            (Some("0"), HistorySource::None),
        ] {
            let tmp = tempfile::tempdir().expect("tmp");
            let env = |k: &str| -> Option<OsString> {
                match k {
                    HOME_ENV => Some(tmp.path().into()),
                    DEV_KEYRING_ENV => Some("passphrase".into()),
                    HISTORY_TRANSFER_ENV => switch.map(OsString::from),
                    _ => None,
                }
            };
            let paths = Paths::from_env(Profile::default(), env).expect("paths");
            paths.create_dirs().expect("dirs");
            let setup = normal_setup_in(paths, env, false)
                .await
                .expect("a normal setup");
            let signal = setup
                .registry
                .get(NetworkKind::Signal)
                .expect("the Signal network is registered");
            assert_eq!(signal.descriptor().history, expected, "{switch:?}");
        }
    }

    /// W2c F2: a normal start reports whether the number tail reached the backend
    /// configuration (the development state's `expected_number_check`). With the variable set
    /// the check is on; unset or empty, off. Mutant: `number_check_on` always false. Debug
    /// builds only: the tail is read in debug builds only.
    #[cfg(all(debug_assertions, feature = "signal"))]
    #[tokio::test]
    async fn the_number_check_is_on_when_the_tail_reached_the_configuration() {
        use std::ffi::OsString;

        use super::{normal_setup_in, number_check_on};
        use crate::dev::{DEV_KEYRING_ENV, EXPECT_NUMBER_TAIL_ENV};
        use crate::paths::{HOME_ENV, Paths, Profile};

        for (tail, expected) in [(None, false), (Some(""), false), (Some("20260017"), true)] {
            let tmp = tempfile::tempdir().expect("tmp");
            let env = |k: &str| -> Option<OsString> {
                match k {
                    HOME_ENV => Some(tmp.path().into()),
                    DEV_KEYRING_ENV => Some("passphrase".into()),
                    EXPECT_NUMBER_TAIL_ENV => tail.map(OsString::from),
                    _ => None,
                }
            };
            let paths = Paths::from_env(Profile::default(), env).expect("paths");
            paths.create_dirs().expect("dirs");
            let setup = normal_setup_in(paths, env, false)
                .await
                .expect("a normal setup");
            assert_eq!(number_check_on(&setup.backend), expected, "{tail:?}");
        }
    }
}

#[cfg(all(test, feature = "fake"))]
mod update_fixture_tests {
    use kourier_backend_api::StartMode;
    use kourier_fake::Step;
    use kourier_model::AccountUuid;

    use super::{FixtureOptions, demo_network};

    /// `--page=transferupdate` (the banner of the update after a link) reports once a second
    /// after its data. A start waits for the first `seed_steps` steps, and the last of them
    /// must be the wait for the core's ack: with only "the steps ran" (T2 fix round) a list
    /// read at that moment had chats not yet in their final place (the page opens the first
    /// group of the list: one render in twenty opened another chat) and no profile picture
    /// of the account (the rail showed its initials), a different picture each time.
    #[test]
    fn the_update_fixture_is_seeded_when_the_core_acked_its_data() {
        let demo = demo_network(&FixtureOptions {
            updating: true,
            ..FixtureOptions::default()
        })
        .expect("the fixture network");
        let seed = demo
            .seed_steps
            .expect("a fixture that goes on after its data says where the data ends");
        let script = demo
            .network
            .control()
            .script_for(AccountUuid::new_random(), StartMode::Resume);
        assert!(seed > 10, "the data come first: {seed} steps");
        assert!(
            matches!(script.steps[seed - 1], Step::WaitAcked),
            "the start must wait for the ack of the data, the last of the seed steps is {:?}",
            script.steps[seed - 1]
        );
        assert!(
            matches!(script.steps[seed], Step::Sleep(_)),
            "what follows the seed is the once-a-second reports, not data: {:?}",
            script.steps[seed]
        );
    }
}

/// A10a fix round (F2): which fixture accounts state that their network writes the roaming settings.
#[cfg(all(test, feature = "fake"))]
mod fixture_roaming_tests {
    use kourier_backend_api::StartMode;
    use kourier_fake::FakeNetwork;
    use kourier_model::AccountUuid;

    use super::{FixtureNetwork, FixtureOptions, demo_network};

    /// What the default script of `network` states (`Script::roaming_writes`).
    fn stated(network: &FakeNetwork) -> Option<bool> {
        network
            .control()
            .script_for(AccountUuid::new_random(), StartMode::Resume)
            .roaming_writes
    }

    /// Only the plain fake's accounts state the flag (its switch on): the fixture account of the
    /// plain network, the second account of the blended pages, and nothing that plays WhatsApp,
    /// Threema or Telegram, the WhatsApp account of the 3-account pages included (the core would let a click
    /// through to a network that writes none, and the pages would draw enabled entries on its rows).
    /// Mutants: `as_whatsapp` keeps the flag; the Threema fixture keeps it; the Telegram fixture keeps it.
    #[test]
    fn only_the_plain_fixture_accounts_state_roaming_writes() {
        for (network, want) in [
            (FixtureNetwork::Fake, Some(true)),
            (FixtureNetwork::WhatsApp, None),
            (FixtureNetwork::WhatsAppOff, None),
            (FixtureNetwork::Threema, None),
            (FixtureNetwork::Telegram, None),
        ] {
            let demo = demo_network(&FixtureOptions {
                network,
                ..FixtureOptions::default()
            })
            .expect("the fixture network");
            assert_eq!(stated(&demo.network), want, "{network:?}");
        }
        let demo = demo_network(&FixtureOptions {
            accounts: 3,
            ..FixtureOptions::default()
        })
        .expect("the fixture network");
        let extra = demo.extra.expect("the two more accounts");
        assert_eq!(
            extra.second.roaming_writes,
            Some(true),
            "the second account of the test network"
        );
        let whatsapp = extra.whatsapp.expect("the WhatsApp account");
        assert_eq!(
            stated(&whatsapp),
            None,
            "the WhatsApp account of 3 accounts"
        );
    }
}

/// S3 (2026-10-28): the clock of a page whose window sends a message after the start.
#[cfg(all(test, feature = "fake"))]
mod window_send_clock_tests {
    use super::{held_send_clock_start, window_send_clock_start};

    const MINUTE: i64 = 60_000;

    /// `--page=media` sends its photo from the window some seconds after the start, and the core
    /// stamps it with its clock then. Whatever second of its minute the fixture's "now" is at,
    /// and whatever the start took up to 55 s, the stamp must stay in the minute of "now"
    /// (with the clock started AT "now", a "now" at :30 and a send 40 s later read the next
    /// minute: 05:23 in one render of `media-narrow-dark`, 05:24 in another).
    #[test]
    fn the_stamp_of_a_window_send_stays_in_the_minute_of_now() {
        let base = 1_761_000_000_000_i64.div_euclid(MINUTE) * MINUTE;
        for second in 0..60 {
            for ms in [0, 1, 999] {
                let now = base + second * 1_000 + ms;
                let start = window_send_clock_start(now);
                for delay in [0, 1, 1_000, 20_000, 54_000, 54_994] {
                    let stamp = start + delay;
                    assert_eq!(
                        stamp.div_euclid(MINUTE),
                        now.div_euclid(MINUTE),
                        "now {second} s {ms} ms, sent {delay} ms after the start"
                    );
                }
            }
        }
        // The same minute for every "now" of the minute, so the picture does not depend on
        // the seconds of the newest message.
        assert_eq!(window_send_clock_start(base), base + 5_000);
        assert_eq!(window_send_clock_start(base + 59_999), base + 5_000);
        assert_eq!(
            window_send_clock_start(base + MINUTE),
            base + MINUTE + 5_000
        );
    }

    /// The walock page's start is what it was before the helper was shared: five seconds into
    /// the minute that ends two minutes before "now".
    #[test]
    fn the_held_send_clock_start_is_unchanged() {
        let base = 1_761_000_000_000_i64.div_euclid(MINUTE) * MINUTE;
        for (now, want) in [
            (base, base - 2 * MINUTE + 5_000),
            (base + 59_999, base - 2 * MINUTE + 5_000),
            (base + 61_000, base - MINUTE + 5_000),
            (base + 7 * MINUTE + 12_345, base + 5 * MINUTE + 5_000),
        ] {
            assert_eq!(held_send_clock_start(now), want, "now {}", now - base);
        }
    }
}

/// R-Q1: the time a shutdown gives the core follows the registered networks: the largest stop grace
/// among them (what a network states, else the core's 5 s) plus the margin.
#[cfg(test)]
mod device_name_tests {
    /// Fixture mode names the device the same on every machine (pictures of the link page); a
    /// real start offers the host name.
    #[test]
    fn fixture_mode_offers_a_fixed_device_name() {
        assert_eq!(super::device_name(true), super::FIXTURE_DEVICE_NAME);
        assert_eq!(super::device_name(false), super::hostname());
    }
}

/// KT-1b C5a: what [`AppHandle::networks_missing`] hands the UI.
#[cfg(test)]
mod missing_networks_tests {
    use kourier_model::NetworkKind;

    use super::{TelegramMissing, missing_networks};

    /// Telegram is named, with what is missing, when this build cannot log in to it, and nothing
    /// is named otherwise (a build that can, a build without the feature, a fixture).
    /// Mutant: the list is empty for `Library`.
    #[test]
    fn telegram_is_named_with_what_is_missing_and_nothing_else_is() {
        assert!(missing_networks(None).is_empty());
        for missing in [TelegramMissing::ApiKey, TelegramMissing::Library] {
            assert_eq!(
                missing_networks(Some(missing)),
                vec![(NetworkKind::Telegram, missing)],
                "{missing:?}"
            );
        }
    }
}

#[cfg(test)]
mod shutdown_timeout_tests {
    use std::time::Duration;

    use kourier_backend_api::NetworkRegistry;

    use super::{CORE_SHUTDOWN_MARGIN, core_shutdown_timeout};

    #[test]
    fn an_empty_registry_gets_the_default_grace_and_the_margin() {
        assert_eq!(CORE_SHUTDOWN_MARGIN, Duration::from_secs(5));
        assert_eq!(
            core_shutdown_timeout(&NetworkRegistry::new()),
            Duration::from_secs(10)
        );
    }

    /// Signal states no grace: the bound is what it always was (10 s), WhatsApp states 20 s and
    /// the bound is 25 s, whatever the order of registration. Mutant: a fixed 10 s.
    #[cfg(all(feature = "signal", feature = "whatsapp"))]
    #[test]
    fn the_bound_follows_the_largest_grace_of_the_registered_networks() {
        use std::sync::Arc;

        let signal = || -> Arc<dyn kourier_backend_api::Network> {
            Arc::new(kourier_signal::SignalNetwork::new())
        };
        let whatsapp = || -> Arc<dyn kourier_backend_api::Network> {
            Arc::new(super::whatsapp_network(
                kourier_whatsapp::NetParts::refusing(Default::default()),
            ))
        };
        let with = |networks: Vec<Arc<dyn kourier_backend_api::Network>>| {
            let mut registry = NetworkRegistry::new();
            for network in networks {
                registry.register(network).expect("registered once");
            }
            core_shutdown_timeout(&registry)
        };
        assert_eq!(with(vec![signal()]), Duration::from_secs(10));
        assert_eq!(with(vec![whatsapp()]), Duration::from_secs(25));
        assert_eq!(with(vec![signal(), whatsapp()]), Duration::from_secs(25));
        assert_eq!(with(vec![whatsapp(), signal()]), Duration::from_secs(25));
    }
}

#[cfg(all(test, feature = "fake"))]
mod experimental_network_tests {
    use std::sync::Arc;

    use kourier_backend_api::{Network, NetworkRegistry};
    use kourier_fake::{FakeNetwork, Flavor, Script};
    use kourier_model::{HistorySource, NetworkKind};

    use super::{Prefs, register_experimental, take_pending_link};

    fn whatsapp() -> Arc<dyn Network> {
        Arc::new(FakeNetwork::playing(
            NetworkKind::WhatsApp,
            Script::new("test", Vec::new()),
        ))
    }

    fn turned_on() -> Prefs {
        let mut prefs = Prefs::default();
        prefs.set_network_on(NetworkKind::WhatsApp, true);
        prefs
    }

    /// An experimental network is registered while the preferences have it on and is on the
    /// off list, described for the UI, while they do not (a profile that never chose: off).
    #[test]
    fn an_experimental_network_is_registered_only_while_it_is_on() {
        let mut registry = NetworkRegistry::new();
        let off = register_experimental(&mut registry, &Prefs::default(), vec![whatsapp()]);
        assert!(registry.get(NetworkKind::WhatsApp).is_none());
        assert_eq!(off.len(), 1);
        assert_eq!(off[0].kind, NetworkKind::WhatsApp);
        assert_eq!(off[0].name, "WhatsApp");
        assert!(off[0].experimental);
        assert!(off[0].risk_notice.is_some(), "it asks first");
        let flows: Vec<_> = off[0].flows.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(flows, ["qr", "pair-code"]);
        assert_eq!(off[0].history, HistorySource::PushedOnLink);
        assert!(
            !off[0].history_transfer,
            "WhatsApp has no archive to transfer"
        );

        let mut registry = NetworkRegistry::new();
        let off = register_experimental(&mut registry, &turned_on(), vec![whatsapp()]);
        assert!(registry.get(NetworkKind::WhatsApp).is_some());
        assert!(off.is_empty());
        // The info of a registered network says the same.
        let info = super::network_info(&registry);
        assert_eq!(info.len(), 1);
        assert!(info[0].experimental && !info[0].history_transfer);
    }

    /// Only an experimental network goes through this: a backend that says it is not is
    /// neither registered nor listed as off (the caller registers those, whatever the
    /// preferences say).
    #[test]
    fn a_network_that_is_not_experimental_is_not_this_ones_to_register() {
        let plain: Arc<dyn Network> = Arc::new(FakeNetwork::new(
            Flavor::SignalLike,
            Script::new("test", Vec::new()),
        ));
        let mut registry = NetworkRegistry::new();
        let off = register_experimental(&mut registry, &turned_on(), vec![plain]);
        assert!(registry.iter().next().is_none() && off.is_empty());
    }

    /// The one-shot link request is honoured once, when its network is registered and
    /// experimental, and is gone from the file after the first read whatever came of it.
    #[test]
    fn the_link_request_counts_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kourier.toml");
        let mut registry = NetworkRegistry::new();
        register_experimental(&mut registry, &turned_on(), vec![whatsapp()]);
        let wanted = Prefs {
            pending_link: Some("WhatsApp".to_owned()),
            compact: true,
            ..turned_on()
        };
        wanted.save(&path).expect("save");
        assert_eq!(
            take_pending_link(&path, &registry),
            Some(NetworkKind::WhatsApp)
        );
        let after = Prefs::load(&path).expect("load");
        assert_eq!(after.pending_link, None, "cleared at once");
        assert!(
            after.compact && after.network_on(NetworkKind::WhatsApp),
            "the rest stays"
        );
        assert_eq!(take_pending_link(&path, &registry), None, "once");
    }

    /// A request for a network that is not on now (the user turned it off again, or the build
    /// lacks it) opens nothing, and is cleared too: it must not wait for a later start.
    #[test]
    fn a_link_request_for_a_network_that_is_not_there_is_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kourier.toml");
        let off = NetworkRegistry::new();
        for name in ["WhatsApp", "Fake", "Nonesuch"] {
            Prefs {
                pending_link: Some(name.to_owned()),
                ..Prefs::default()
            }
            .save(&path)
            .expect("save");
            assert_eq!(take_pending_link(&path, &off), None, "{name}");
            assert_eq!(Prefs::load(&path).expect("load").pending_link, None);
        }
        // Not experimental: a registered test network is no network to open a link page of.
        let mut registry = NetworkRegistry::new();
        registry
            .register(Arc::new(FakeNetwork::new(
                Flavor::SignalLike,
                Script::new("test", Vec::new()),
            )))
            .expect("register");
        Prefs {
            pending_link: Some("Fake".to_owned()),
            ..Prefs::default()
        }
        .save(&path)
        .expect("save");
        assert_eq!(take_pending_link(&path, &registry), None);
    }

    /// A file that cannot be read is left as it is: the request is not honoured and the
    /// file is not overwritten by defaults.
    #[test]
    fn a_damaged_preferences_file_is_not_overwritten_by_the_link_request() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kourier.toml");
        std::fs::write(&path, "tray = \"yes\"\npending-link = \"WhatsApp\"\n").expect("write");
        let mut registry = NetworkRegistry::new();
        register_experimental(&mut registry, &turned_on(), vec![whatsapp()]);
        assert_eq!(take_pending_link(&path, &registry), None);
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("tray = \"yes\""), "{text}");
    }
}

/// TH1 (lane decision Q2), TH4 J6 (decision E1): the Threema backend is registered by a debug build
/// while it is on, listed while it is off only in a PREPARED run profile (the backend can link AND
/// the profile's list of test IDs is not empty), and absent from a release build. Every expectation
/// here is written out, never taken from `threema_registration` or the build's constants.
#[cfg(all(test, feature = "threema"))]
mod threema_registration_tests {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{Build, Prefs, Seams};
    use crate::paths::HOME_ENV;
    use kourier_model::NetworkKind;

    fn prefs(on: bool) -> Prefs {
        let mut prefs = Prefs::default();
        prefs.set_network_on(NetworkKind::Threema, on);
        prefs
    }

    /// A test's seams: `build`, and parts that refuse and count into `made` (WhatsApp's count apart).
    fn seams(build: Build, made: &Arc<AtomicUsize>) -> Seams {
        Seams {
            build,
            #[cfg(feature = "whatsapp")]
            whatsapp_parts: Box::new(|| kourier_whatsapp::NetParts::refusing(Default::default())),
            // Telegram is off in these cases: its maker is never called.
            #[cfg(feature = "telegram")]
            telegram_parts: Box::new(|| kourier_telegram::TgParts::refusing(Arc::default())),
            #[cfg(feature = "telegram")]
            telegram_state: crate::telegram::PartsState::Missing(
                crate::telegram::TelegramMissing::ApiKey,
            ),
            threema_parts: super::refusing_threema_parts(made.clone()),
        }
    }

    /// What the rule says, written out: registered by a debug build while on; listed while off only
    /// by a debug build in a prepared run profile (decision E1: a plain debug start keeps Threema
    /// absent); nothing in a release build, prepared or not (it never reads the list).
    fn expected(build: Build, on: bool, prepared: bool) -> (bool, bool) {
        let registered = build == Build::Debug && on;
        let listed = build == Build::Debug && !on && prepared;
        (registered, listed)
    }

    /// Every build, switch and kind of profile.
    const CASES: [(Build, bool, bool); 8] = [
        (Build::Debug, false, false),
        (Build::Debug, true, false),
        (Build::Debug, false, true),
        (Build::Debug, true, true),
        (Build::Release, false, false),
        (Build::Release, true, false),
        (Build::Release, false, true),
        (Build::Release, true, true),
    ];

    /// `printf 'kourier-threema-allow-v1:TESTID01' | sha256sum`: the one line of a prepared profile's
    /// list (a synthetic ID).
    const LIST_LINE: &str =
        "sha256:871ad9802d67bec50e0d5a9d2c4113efec962437723daac9d86b8f1e0ab82a87\n";

    /// The profile a case starts in: PREPARED is a kept run profile (`p` below the keeper root
    /// `<data>/kourier-threema-run`, 0700) whose list names one test ID; else a plain profile
    /// directory with no keeper root at all. The environment names the data directory and
    /// `KOURIER_HOME`.
    struct Scratch {
        _tmp: tempfile::TempDir,
        data: PathBuf,
        home: PathBuf,
        run_tool: bool,
    }

    impl Scratch {
        /// A normal start of a profile, without the run tool.
        fn normal() -> Self {
            Self {
                run_tool: false,
                ..Self::new(false)
            }
        }

        /// A start by the run tool.
        fn new(prepared: bool) -> Self {
            use std::os::unix::fs::PermissionsExt as _;

            let tmp = tempfile::tempdir().expect("tmp");
            // The temp dir may sit below a symbolic link: work on its canonical path.
            let base = std::fs::canonicalize(tmp.path()).expect("canonical");
            let data = base.join("data");
            let home = if prepared {
                let root = data.join(crate::threema::RUN_DIR);
                let home = root.join("p");
                std::fs::create_dir_all(&home).expect("dirs");
                std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
                    .expect("chmod");
                let list = root.join(crate::threema::ALLOW_FILE);
                std::fs::write(&list, LIST_LINE).expect("list");
                std::fs::set_permissions(&list, std::fs::Permissions::from_mode(0o600))
                    .expect("chmod");
                home
            } else {
                let home = base.join("home");
                std::fs::create_dir_all(&home).expect("dirs");
                home
            };
            Self {
                _tmp: tmp,
                data,
                home,
                run_tool: true,
            }
        }

        fn env(&self, key: &str) -> Option<OsString> {
            match key {
                crate::prefs::THREEMA_RUN_TOOL_ENV if self.run_tool => Some("1".into()),
                HOME_ENV => Some(self.home.clone().into()),
                "XDG_DATA_HOME" => Some(self.data.clone().into()),
                _ => None,
            }
        }
    }

    /// The kinds a list of the `--print-networks` line names (`key=a,b`).
    fn listed<'a>(line: &'a str, key: &str) -> Vec<&'a str> {
        let value = line
            .split(' ')
            .find_map(|word| word.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("no {key}= in {line:?}"));
        value.split(',').filter(|k| !k.is_empty()).collect()
    }

    /// The twin of `the_networks_line_registers_or_lists_whatsapp_per_the_switch` (critic H1): the
    /// line a start of the profile would print, for each build, switch and kind of profile, against
    /// literal expectations. A release build names Threema in neither list whatever the switch says
    /// and whatever the profile holds, and `on=` shows that the file was read (which is what makes
    /// the empty lists a proof). Nothing is made, opened or changed. Mutants (J6): `LINKING_BUILT =
    /// false` and `(Debug, false, true) -> Absent` in `threema_registration` leave the prepared
    /// profile's Threema unlisted; `&&` -> `||` in `threema::linking` lists it in a plain one.
    #[test]
    fn the_networks_line_registers_threema_only_in_a_debug_build_that_has_it_on() {
        use super::networks_line_in;
        use crate::paths::{Paths, Profile};

        for (build, on, prepared) in CASES {
            let scratch = Scratch::new(prepared);
            let env = |k: &str| scratch.env(k);
            let paths = Paths::from_env(Profile::default(), env).expect("paths");
            paths.create_dirs().expect("dirs");
            prefs(on).save(&paths.config_file()).expect("save prefs");
            let file = std::fs::read_to_string(paths.config_file()).expect("prefs file");
            let made = Arc::new(AtomicUsize::new(0));
            let line = networks_line_in(&paths, env, seams(build, &made)).expect("the line");
            let case = format!("{build:?}, on={on}, prepared={prepared}: {line:?}");
            let (registered_here, listed_here) = expected(build, on, prepared);
            assert_eq!(
                listed(&line, "registered").contains(&"Threema"),
                registered_here,
                "{case}"
            );
            assert_eq!(
                listed(&line, "off").contains(&"Threema"),
                listed_here,
                "{case}"
            );
            assert_eq!(
                listed(&line, "on"),
                if on { vec!["Threema"] } else { vec![] },
                "{case}"
            );
            assert_eq!(made.load(Ordering::SeqCst), 0, "{case}: a client was made");
            assert!(!paths.core_db().exists(), "{case}: a database was opened");
            assert_eq!(
                std::fs::read_to_string(paths.config_file()).expect("prefs file"),
                file,
                "{case}: the preferences were changed"
            );
        }
    }

    /// The user's decision of 2026-11-09: a normal start of EVERY build has Threema on with nothing
    /// in the file, keeps it off once the user turned it off (listed then, so Settings can turn it
    /// on again), and the probe changes no file and makes no client. Mutants: `start_prefs` without
    /// the defaults -> the first half; the release arm back in `threema_registration` -> Release.
    #[test]
    fn a_normal_start_of_every_build_has_threema_on_until_it_is_turned_off() {
        use super::networks_line_in;
        use crate::paths::{Paths, Profile};

        for build in [Build::Debug, Build::Release] {
            let scratch = Scratch::normal();
            let env = |k: &str| scratch.env(k);
            let paths = Paths::from_env(Profile::default(), env).expect("paths");
            paths.create_dirs().expect("dirs");
            let made = Arc::new(AtomicUsize::new(0));
            let line = networks_line_in(&paths, env, seams(build, &made)).expect("the line");
            let case = format!("{build:?}: {line:?}");
            assert!(listed(&line, "registered").contains(&"Threema"), "{case}");
            assert!(!listed(&line, "off").contains(&"Threema"), "{case}");
            assert_eq!(listed(&line, "on"), vec!["Threema"], "{case}");
            assert!(!paths.config_file().exists(), "{case}: the probe wrote");

            prefs(false).save(&paths.config_file()).expect("save prefs");
            let line = networks_line_in(&paths, env, seams(build, &made)).expect("the line");
            let case = format!("{build:?}, off: {line:?}");
            assert!(!listed(&line, "registered").contains(&"Threema"), "{case}");
            assert!(listed(&line, "off").contains(&"Threema"), "{case}");
            assert!(listed(&line, "on").is_empty(), "{case}");
            assert_eq!(made.load(Ordering::SeqCst), 0, "{case}: a client was made");
        }
    }

    /// G2: a RELEASE start does not even READ the list. The profile is prepared and its list is one a
    /// debug start would warn about (mode 0644), so a read is visible as a warning: none in a release
    /// build (also with Threema on), exactly one, with the problem's fixed words, in a debug build
    /// (the control: the capture sees the warning, and the bad list lists nothing). Mutant: the call
    /// in `start_networks` reads for every build (`allowed_ids(true, env)`) -> the release cases warn.
    #[test]
    fn a_release_start_does_not_read_the_list() {
        use std::os::unix::fs::PermissionsExt as _;

        use super::networks_line_in;
        use crate::paths::{Paths, Profile};
        use crate::threema::AllowFileError;
        use crate::threema::capture::events_of;

        for (build, on) in [
            (Build::Release, false),
            (Build::Release, true),
            (Build::Debug, false),
            (Build::Debug, true),
        ] {
            let scratch = Scratch::new(true);
            let list = scratch
                .home
                .parent()
                .expect("the keeper root")
                .join(crate::threema::ALLOW_FILE);
            std::fs::set_permissions(&list, std::fs::Permissions::from_mode(0o644)).expect("chmod");
            let env = |k: &str| scratch.env(k);
            let paths = Paths::from_env(Profile::default(), env).expect("paths");
            paths.create_dirs().expect("dirs");
            prefs(on).save(&paths.config_file()).expect("save prefs");
            let made = Arc::new(AtomicUsize::new(0));
            let (line, events) = events_of(|| networks_line_in(&paths, env, seams(build, &made)));
            let line = line.expect("the line");
            let case = format!("{build:?}, on={on}: {line:?}");
            let told: Vec<_> = events
                .iter()
                .filter(|(_, fields)| fields.contains("Threema test IDs"))
                .collect();
            if build == Build::Release {
                assert!(
                    told.is_empty(),
                    "{case}: a release start read the list: {told:?}"
                );
            } else {
                assert_eq!(told.len(), 1, "{case}: {events:?}");
                assert_eq!(told[0].0, tracing::Level::WARN, "{case}");
                assert!(
                    told[0].1.contains(&AllowFileError::Mode.to_string()),
                    "{case}: {told:?}"
                );
                // A list that is not used lists nothing: fail closed.
                assert!(
                    !listed(&line, "off").contains(&"Threema"),
                    "{case}: a bad list listed Threema"
                );
            }
            assert_eq!(made.load(Ordering::SeqCst), 0, "{case}: a client was made");
        }
    }

    /// THE WIRING (debug builds only: the test starts from the development keyring): a normal start
    /// registers the Threema backend only in a debug build that has it on, lists it only in a
    /// prepared run profile (E1), offers nothing in a release build, and in none of them makes a
    /// client.
    #[cfg(debug_assertions)]
    #[tokio::test]
    async fn a_normal_start_registers_threema_only_when_on() {
        use super::normal_setup_with;
        use crate::dev::DEV_KEYRING_ENV;
        use crate::paths::{Paths, Profile};

        for (build, on, prepared) in CASES {
            let scratch = Scratch::new(prepared);
            let env = |k: &str| -> Option<OsString> {
                match k {
                    DEV_KEYRING_ENV => Some("passphrase".into()),
                    _ => scratch.env(k),
                }
            };
            let paths = Paths::from_env(Profile::default(), env).expect("paths");
            paths.create_dirs().expect("dirs");
            prefs(on).save(&paths.config_file()).expect("save prefs");
            let made = Arc::new(AtomicUsize::new(0));
            let setup = normal_setup_with(paths, env, seams(build, &made))
                .await
                .expect("a normal setup");
            let case = format!("{build:?}, on={on}, prepared={prepared}");
            let (registered_here, listed_here) = expected(build, on, prepared);
            let network = setup.registry.get(NetworkKind::Threema);
            assert_eq!(network.is_some(), registered_here, "{case}: registered");
            let listed: Vec<_> = setup
                .networks_off
                .iter()
                .filter(|n| n.kind == NetworkKind::Threema)
                .collect();
            assert_eq!(listed.len(), usize::from(listed_here), "{case}: listed");
            if let Some(network) = network {
                // Described for the UI: experimental, no history transfer, and it asks first.
                let d = network.descriptor();
                assert!(d.experimental && !d.history_transfer, "{case}");
                assert!(d.risk_notice.is_some(), "{case}: it asks first");
            }
            for info in listed {
                assert!(info.experimental && !info.history_transfer, "{case}");
                assert!(info.risk_notice.is_some(), "{case}: it asks first");
            }
            assert_eq!(
                made.load(Ordering::SeqCst),
                0,
                "{case}: a client was made at the start"
            );
            #[cfg(feature = "signal")]
            assert!(setup.registry.get(NetworkKind::Signal).is_some(), "{case}");
        }
    }

    /// THE SAFETY GATE (`prefs::ThreemaStart`): a scripted start (`--dev-control`) outside the run tool, and
    /// a start whose profile holds the run tool's `threema-run/` (as a directory, a symbolic link or a
    /// file; in `KOURIER_HOME` or in the data directory) outside the run tool, offer no Threema at all:
    /// neither registered nor listed, whatever `networks-on` says. The same profile unscripted and
    /// unmarked registers it (on) or lists it (off); the run tool's start keeps its old table, also in
    /// a profile with the marker (its own). Mutants: `scripted || marker` -> `scripted` (the marker
    /// cases fail) or `marker` (the scripted cases fail); `Shut` arm of `threema_registration`
    /// dropped; `normal_setup_in` passing `false` for `scripted`.
    #[cfg(debug_assertions)]
    #[tokio::test]
    async fn a_scripted_start_or_a_run_tools_profile_outside_the_tool_offers_no_threema() {
        use super::normal_setup_scripted_with;
        use crate::dev::DEV_KEYRING_ENV;
        use crate::paths::{Paths, Profile};

        #[derive(Clone, Copy, Debug)]
        enum Marker {
            None,
            Dir,
            Link,
            File,
            InData,
        }
        // (scripted, run tool, marker, on) -> (registered, listed), written out.
        let cases = [
            (false, false, Marker::None, true, (true, false)),
            (false, false, Marker::None, false, (false, true)),
            (true, false, Marker::None, true, (false, false)),
            (true, false, Marker::None, false, (false, false)),
            (false, false, Marker::Dir, true, (false, false)),
            (false, false, Marker::Dir, false, (false, false)),
            (false, false, Marker::Link, true, (false, false)),
            (false, false, Marker::File, true, (false, false)),
            (false, false, Marker::InData, true, (false, false)),
            (true, false, Marker::Dir, true, (false, false)),
            // The run tool's start: the old table (a debug build registers while on, lists nothing
            // while off with no list), scripted or not, in its own profile or not.
            (false, true, Marker::None, true, (true, false)),
            (false, true, Marker::Dir, true, (true, false)),
            (true, true, Marker::Dir, true, (true, false)),
            (true, true, Marker::None, false, (false, false)),
        ];
        for (scripted, run_tool, marker, on, (want_registered, want_listed)) in cases {
            let mut scratch = Scratch::normal();
            scratch.run_tool = run_tool;
            let tool_dir = crate::prefs::THREEMA_RUN_TOOL_DIR;
            match marker {
                Marker::None => {}
                Marker::Dir => std::fs::create_dir(scratch.home.join(tool_dir)).expect("dir"),
                Marker::Link => {
                    std::os::unix::fs::symlink("/nonexistent-target", scratch.home.join(tool_dir))
                        .expect("link")
                }
                Marker::File => std::fs::write(scratch.home.join(tool_dir), b"x").expect("file"),
                Marker::InData => {
                    let env = |k: &str| scratch.env(k);
                    let paths = Paths::from_env(Profile::default(), env).expect("paths");
                    std::fs::create_dir_all(paths.data.join(tool_dir)).expect("data dir");
                }
            }
            let env = |k: &str| -> Option<OsString> {
                match k {
                    DEV_KEYRING_ENV => Some("passphrase".into()),
                    _ => scratch.env(k),
                }
            };
            let paths = Paths::from_env(Profile::default(), env).expect("paths");
            paths.create_dirs().expect("dirs");
            prefs(on).save(&paths.config_file()).expect("save prefs");
            let made = Arc::new(AtomicUsize::new(0));
            let setup =
                normal_setup_scripted_with(paths, env, seams(Build::Debug, &made), scripted)
                    .await
                    .expect("a start");
            let case = format!("scripted={scripted}, run_tool={run_tool}, {marker:?}, on={on}");
            assert_eq!(
                setup.registry.get(NetworkKind::Threema).is_some(),
                want_registered,
                "{case}: registered"
            );
            assert_eq!(
                setup
                    .networks_off
                    .iter()
                    .any(|n| n.kind == NetworkKind::Threema),
                want_listed,
                "{case}: listed"
            );
            assert_eq!(made.load(Ordering::SeqCst), 0, "{case}: a client was made");
        }
    }

    /// `start_prefs` applies the on-by-default networks to a person's start only: a shut start and the
    /// run tool's read the file alone. Mutant: the defaults for everyone (`if true`).
    #[test]
    fn only_a_persons_start_gets_the_networks_that_are_on_by_default() {
        use crate::prefs::ThreemaStart::{Person, RunTool, Shut};
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("kourier.toml");
        for (start, on) in [(Person, true), (RunTool, false), (Shut, false)] {
            assert_eq!(
                super::start_prefs(&path, start).network_on(NetworkKind::Threema),
                on,
                "{start:?}"
            );
        }
    }

    /// The two joints of the safety gate above `normal_setup_scripted_with`: `normal_setup_in` hands
    /// its `scripted` on (the real seams, a profile with nothing in it: Threema is on by default
    /// for a person, absent for a scripted start), and `App::start` makes `Launch::Scripted` a
    /// scripted setup and `Launch::Normal` an unscripted one (a source pin, the form of the
    /// confirmation test below). Mutants: `normal_setup_in` passing `false`; the `Scripted` arm of
    /// `App::start` passing `false`.
    #[cfg(debug_assertions)]
    #[tokio::test]
    async fn the_scripted_launch_reaches_the_gate_through_both_joints() {
        use super::normal_setup_in;
        use crate::dev::DEV_KEYRING_ENV;
        use crate::paths::{Paths, Profile};

        for (scripted, expected) in [(false, true), (true, false)] {
            let tmp = tempfile::tempdir().expect("tmp");
            let env = |k: &str| -> Option<OsString> {
                match k {
                    HOME_ENV => Some(tmp.path().into()),
                    DEV_KEYRING_ENV => Some("passphrase".into()),
                    _ => None,
                }
            };
            let paths = Paths::from_env(Profile::default(), env).expect("paths");
            paths.create_dirs().expect("dirs");
            let setup = normal_setup_in(paths, env, scripted)
                .await
                .expect("a start");
            assert_eq!(
                setup.registry.get(NetworkKind::Threema).is_some(),
                expected,
                "scripted={scripted}"
            );
        }
        let text = include_str!("app.rs");
        let cut = text
            .find("\n#[cfg(test)]\nmod ")
            .expect("the first test module");
        let code = &text[..cut];
        for (arm, flag) in [("Launch::Normal", "false"), ("Launch::Scripted", "true")] {
            let line = code
                .lines()
                .find(|l| {
                    l.trim_start()
                        .starts_with(&format!("{arm} => runtime.block_on("))
                })
                .unwrap_or_else(|| panic!("no `{arm}` setup arm in App::start"));
            assert!(
                line.contains(&format!("normal_setup(&opts.profile, {flag})")),
                "{line}"
            );
        }
    }

    /// THE LIST REACHES THE BACKEND (decision C10, B1's gate): the registered Threema network of a
    /// debug start in a PREPARED run profile lets `StartLogin` go on to make its client (the refusing
    /// parts count it, and refuse every dial), while the same network of a plain profile refuses at
    /// once with `NotTheExpectedAccount` at step `qr` and makes none. Mutant: the list not given to
    /// the parts in `threema_network` or at the call (`AllowList::default()`) -> the prepared case
    /// is refused.
    ///
    /// The body runs the REAL actor, so it runs in the OFFLINE CHILD like every test that runs an
    /// actor (`.claude/rules/threema.md`): this test binary again, this test only, under `unshare -rn`
    /// (`kourier_telegram::offline`, the form of the Telegram test at the end of this file; the parts
    /// stay `refusing`). The parent skips with a `SKIPPED:` line where `unshare` or `ip` is missing
    /// and fails instead under `KOURIER_REQUIRE_UNSHARE=1`.
    #[cfg(debug_assertions)]
    #[test]
    fn a_prepared_profile_hands_its_list_to_the_network_and_a_plain_one_does_not() {
        use std::time::Duration;

        use kourier_telegram::offline;

        const NAME: &str =
            "a_prepared_profile_hands_its_list_to_the_network_and_a_plain_one_does_not";
        /// The line the child prints when the body is through.
        const DONE: &str = "THREEMA wiring: both profiles checked";

        if offline::in_offline_child() {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(the_wiring_of_both_profiles());
            println!("{DONE}");
            return;
        }
        if !offline::offline_available() {
            assert!(
                !std::env::var("KOURIER_REQUIRE_UNSHARE").is_ok_and(|v| v == "1"),
                "`unshare -rn` (or `ip`) is unavailable and KOURIER_REQUIRE_UNSHARE=1"
            );
            println!("SKIPPED: `unshare -rn` or `ip` is unavailable");
            return;
        }
        // libtest's name of this test: the module path without the crate's name.
        let module = module_path!().split_once("::").map_or("", |(_, rest)| rest);
        let run =
            offline::run_self_offline(&format!("{module}::{NAME}"), &[], Duration::from_secs(240))
                .expect("start the offline child");
        assert!(
            run.status.success() && run.output.contains("1 passed") && run.output.contains(DONE),
            "the offline child failed ({:?}):\n{}",
            run.status,
            run.output
        );
    }

    /// The child's body: both profiles against the real actor over the refusing parts.
    #[cfg(debug_assertions)]
    async fn the_wiring_of_both_profiles() {
        use kourier_backend_api::{
            AccountContext, Command, ContextParams, DbKey, Exit, StartMode, SystemClock,
        };
        use kourier_model::{AccountId, AccountUuid, ErrorKind, LoginStepKind};

        use super::normal_setup_with;
        use crate::dev::DEV_KEYRING_ENV;
        use crate::paths::{Paths, Profile};

        for prepared in [false, true] {
            let scratch = Scratch::new(prepared);
            let env = |k: &str| -> Option<OsString> {
                match k {
                    DEV_KEYRING_ENV => Some("passphrase".into()),
                    _ => scratch.env(k),
                }
            };
            let paths = Paths::from_env(Profile::default(), env).expect("paths");
            paths.create_dirs().expect("dirs");
            prefs(true).save(&paths.config_file()).expect("save prefs");
            let made = Arc::new(AtomicUsize::new(0));
            let setup = normal_setup_with(paths, env, seams(Build::Debug, &made))
                .await
                .expect("a normal setup");
            let network = setup
                .registry
                .get(NetworkKind::Threema)
                .expect("Threema is on, so registered");
            let dir = tempfile::tempdir().expect("account dir");
            let (ctx, mut ends) = AccountContext::new(ContextParams {
                account: AccountId(1),
                uuid: AccountUuid::new_random(),
                start: StartMode::Fresh,
                backend_db: dir.path().join("backend.db"),
                db_key: DbKey::new([0x42; 32]),
                blob_dir: dir.path().join("blobs"),
                net_rt: tokio::runtime::Handle::current(),
                cancel: tokio_util::sync::CancellationToken::new(),
                clock: Arc::new(SystemClock),
                config: Default::default(),
                ui: Default::default(),
                local_prefs: Default::default(),
            });
            let mut handle = network.spawn(ctx).expect("the actor starts");
            ends.commands
                .send(Command::StartLogin {
                    flow: "qr".to_owned(),
                    relink: None,
                    taken: Vec::new(),
                })
                .await
                .expect("the actor takes commands");
            let envelope =
                tokio::time::timeout(std::time::Duration::from_secs(10), ends.events.recv())
                    .await
                    .expect("a step in time")
                    .expect("the actor's events channel is open");
            let kinds: Vec<_> = envelope
                .events
                .iter()
                .map(|e| match e {
                    kourier_backend_api::BackendEvent::Login(step) => {
                        Some((step.step_id.clone(), step.kind.clone()))
                    }
                    _ => None,
                })
                .collect();
            let refused_by_the_gate = matches!(
                kinds.as_slice(),
                [Some((id, LoginStepKind::Failed { error }))]
                    if id == "qr" && error.kind == ErrorKind::NotTheExpectedAccount
            );
            let case = format!("prepared={prepared}: {kinds:?}");
            assert_eq!(refused_by_the_gate, !prepared, "{case}");
            assert_eq!(
                made.load(Ordering::SeqCst),
                usize::from(prepared),
                "{case}: the client was made, or not, against the gate"
            );
            ends.commands
                .send(Command::Shutdown)
                .await
                .expect("the actor takes commands");
            let exited =
                tokio::time::timeout(std::time::Duration::from_secs(10), &mut handle.exited)
                    .await
                    .expect("the actor ends");
            assert_eq!(Exit::from_channel(exited), Exit::Shutdown, "{case}");
            if let Some(thread) = handle.thread.take() {
                thread.join().expect("the actor's thread joins");
            }
        }
    }

    /// The seams of a rehearsal test: [`seams`] whose Threema maker counts into `calls` how often it
    /// was called (the maker of the real backend's parts) and into `made` how often a client was made.
    #[cfg(all(debug_assertions, feature = "fake"))]
    fn counting_seams(build: Build, calls: &Arc<AtomicUsize>, made: &Arc<AtomicUsize>) -> Seams {
        let mut seams = seams(build, made);
        let (calls, made) = (calls.clone(), made.clone());
        seams.threema_parts = Box::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            kourier_threema::ThreemaParts::refusing(made)
        });
        seams
    }

    /// What a normal start of a profile (`prepared`: a kept run profile with a list of test IDs) with
    /// `rehearsal` as the value of `KOURIER_DEV_REHEARSAL` (or none) did: the result, how often the
    /// real parts' maker was called, how often a client was made.
    #[cfg(all(debug_assertions, feature = "fake"))]
    async fn start_rehearsing_threema(
        build: Build,
        on: bool,
        prepared: bool,
        rehearsal: Option<&'static str>,
    ) -> (Result<super::Setup, super::AppError>, usize, usize) {
        use super::normal_setup_with;
        use crate::dev::{DEV_KEYRING_ENV, REHEARSAL_ENV};
        use crate::paths::{Paths, Profile};

        let scratch = Scratch::new(prepared);
        let env = |k: &str| -> Option<OsString> {
            match k {
                DEV_KEYRING_ENV => Some("passphrase".into()),
                REHEARSAL_ENV => rehearsal.map(OsString::from),
                _ => scratch.env(k),
            }
        };
        let paths = Paths::from_env(Profile::default(), env).expect("paths");
        paths.create_dirs().expect("dirs");
        prefs(on).save(&paths.config_file()).expect("save prefs");
        let (calls, made) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let result = normal_setup_with(paths, env, counting_seams(build, &calls, &made)).await;
        (
            result,
            calls.load(Ordering::SeqCst),
            made.load(Ordering::SeqCst),
        )
    }

    /// TR T8, THE REHEARSAL SEAM: with `KOURIER_DEV_REHEARSAL=threema` in a debug build the Threema
    /// slot holds the scripted fake (`rehearsal_threema`: Threema's descriptor without the risk
    /// notice) INSTEAD of the real backend, and the real parts' maker (`dial::live_parts` in a real
    /// start) is never called: no dialer, client or socket exists in a rehearsal, whatever the run
    /// profile's list says. The rule of WHEN Threema is offered is the real one (the switch's
    /// registration, E1's listing in a prepared profile only): the rehearsal changes WHAT is
    /// registered or listed, never whether. It hands the dev channel the fake's control
    /// (`state.rehearsal`). A release build offers nothing of it; the control (no switch) calls the
    /// maker once and has the real descriptor. Mutants: the rehearsal arm also calls
    /// `seams.threema_parts` -> `calls` is 1 -> fails; the arm taken without the switch -> the control
    /// has no risk notice -> fails.
    #[cfg(all(debug_assertions, feature = "fake"))]
    #[tokio::test]
    async fn a_rehearsal_start_never_makes_the_real_threema_parts() {
        // The control: without the switch the real parts' maker IS called (once), and the network
        // is the real one, which asks first.
        let (real, calls, made) = start_rehearsing_threema(Build::Debug, true, false, None).await;
        let real = real.expect("a normal setup");
        assert_eq!(
            (calls, made),
            (1, 0),
            "the real maker is called without the switch"
        );
        assert!(real.rehearsal.is_none());
        let network = real.registry.get(NetworkKind::Threema).expect("registered");
        assert!(
            network.descriptor().risk_notice.is_some(),
            "the real one asks first"
        );

        // The rehearsal, switched on, in a plain and in a prepared profile: registered, no risk
        // notice, a control, no maker call.
        for prepared in [false, true] {
            let case = format!("on, prepared={prepared}");
            let (setup, calls, made) =
                start_rehearsing_threema(Build::Debug, true, prepared, Some("threema")).await;
            let setup = setup.expect("a rehearsal setup");
            assert_eq!(
                calls, 0,
                "{case}: the real parts' maker was called in a rehearsal"
            );
            assert_eq!(made, 0, "{case}");
            let network = setup
                .registry
                .get(NetworkKind::Threema)
                .unwrap_or_else(|| panic!("{case}: not registered"));
            assert_eq!(network.descriptor().display_name, "Threema", "{case}");
            assert!(network.descriptor().experimental, "{case}");
            assert!(
                network.descriptor().risk_notice.is_none(),
                "{case}: the rehearsal asks nothing"
            );
            assert!(
                setup.rehearsal.is_some(),
                "{case}: the dev channel needs the control"
            );
            assert!(
                !setup
                    .networks_off
                    .iter()
                    .any(|n| n.kind == NetworkKind::Threema),
                "{case}"
            );
        }

        // Turned off: a plain profile keeps Threema absent (E1), a prepared one lists it as off, and
        // what is listed is the fake; still no maker call.
        let (plain, calls, made) =
            start_rehearsing_threema(Build::Debug, false, false, Some("threema")).await;
        let plain = plain.expect("a rehearsal setup with the network off");
        assert_eq!((calls, made), (0, 0));
        assert!(plain.registry.get(NetworkKind::Threema).is_none());
        assert!(
            plain
                .networks_off
                .iter()
                .all(|n| n.kind != NetworkKind::Threema),
            "a plain profile lists nothing of Threema"
        );
        assert!(plain.rehearsal.is_none());
        let (prepared, calls, made) =
            start_rehearsing_threema(Build::Debug, false, true, Some("threema")).await;
        let prepared = prepared.expect("a rehearsal setup with the network off");
        assert_eq!((calls, made), (0, 0));
        assert!(prepared.registry.get(NetworkKind::Threema).is_none());
        let listed: Vec<_> = prepared
            .networks_off
            .iter()
            .filter(|n| n.kind == NetworkKind::Threema)
            .collect();
        assert_eq!(listed.len(), 1, "listed as off once");
        assert!(
            listed[0].risk_notice.is_none(),
            "the fake is what is listed"
        );
        assert!(prepared.rehearsal.is_some());

        // Another network's word leaves Threema real.
        #[cfg(feature = "whatsapp")]
        {
            let (other, calls, _) =
                start_rehearsing_threema(Build::Debug, true, false, Some("whatsapp")).await;
            let other = other.expect("a WhatsApp rehearsal setup");
            assert_eq!(calls, 1, "Threema stays real in a WhatsApp rehearsal");
            let network = other
                .registry
                .get(NetworkKind::Threema)
                .expect("registered");
            assert!(network.descriptor().risk_notice.is_some());
        }

        // A release build offers nothing of Threema, the rehearsal's either, prepared or not.
        for prepared in [false, true] {
            let (release, calls, made) =
                start_rehearsing_threema(Build::Release, true, prepared, Some("threema")).await;
            let release = release.expect("a release setup");
            assert_eq!((calls, made), (0, 0), "prepared={prepared}");
            assert!(release.registry.get(NetworkKind::Threema).is_none());
            assert!(
                release
                    .networks_off
                    .iter()
                    .all(|n| n.kind != NetworkKind::Threema)
            );
            assert!(release.rehearsal.is_none());
        }
    }

    /// A value other than exactly `threema` (or another network's word) fails the start before
    /// anything of Threema is made (the same error as the other networks': a typo must not start the
    /// real backend while the person believes it is a rehearsal), and the error does not repeat the
    /// value.
    #[cfg(all(debug_assertions, feature = "fake"))]
    #[tokio::test]
    async fn a_rehearsal_value_that_is_not_a_network_fails_the_start_before_threema_is_made() {
        let mut words = std::collections::BTreeSet::new();
        for bad in [
            "Threema",
            "threema ",
            "thr",
            "real",
            "1",
            "threema,telegram",
        ] {
            let (result, calls, made) =
                start_rehearsing_threema(Build::Debug, true, true, Some(bad)).await;
            let error = match result {
                Err(e) => e,
                Ok(_) => panic!("{bad:?}: the start went on"),
            };
            assert!(
                matches!(
                    error,
                    super::AppError::Config(super::ConfigError::Rehearsal(_))
                ),
                "{bad:?}: {error}"
            );
            assert_eq!((calls, made), (0, 0), "{bad:?}: something was made");
            words.insert(error.to_string());
        }
        // One sentence for every value: nothing of the value is repeated.
        assert_eq!(words.len(), 1, "{words:?}");
    }

    /// TR T8, `--print-networks` (the run tool's probe) with the rehearsal switch: Threema is in
    /// `registered=` and `on=` while `networks-on` names it, in `off=` while it is off in a PREPARED
    /// profile (the run tool writes the switch off at every end: between runs the profile connects to
    /// nothing, and a hand start finds Threema off) and in neither list in a plain profile, with the
    /// switch and without it alike: the rehearsal changes which network stands in the slot, never
    /// whether the slot is offered. The real parts' maker is called only without the switch, and no
    /// client is made, a database opened or a preference changed in any case. Mutants: the rehearsal
    /// arm lists Threema regardless of the registration rule (a plain profile's `off=`); the maker
    /// called in the arm.
    #[cfg(all(debug_assertions, feature = "fake"))]
    #[test]
    fn the_networks_line_of_a_rehearsal_follows_the_switch_and_the_profile_and_makes_nothing() {
        use super::networks_line_in;
        use crate::dev::REHEARSAL_ENV;
        use crate::paths::{Paths, Profile};

        for (on, prepared) in [(false, false), (false, true), (true, false), (true, true)] {
            for rehearsal in [None, Some("threema")] {
                let scratch = Scratch::new(prepared);
                let env = |k: &str| -> Option<OsString> {
                    match k {
                        REHEARSAL_ENV => rehearsal.map(OsString::from),
                        _ => scratch.env(k),
                    }
                };
                let paths = Paths::from_env(Profile::default(), env).expect("paths");
                paths.create_dirs().expect("dirs");
                prefs(on).save(&paths.config_file()).expect("save prefs");
                let file = std::fs::read_to_string(paths.config_file()).expect("prefs file");
                let (calls, made) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
                let line =
                    networks_line_in(&paths, env, counting_seams(Build::Debug, &calls, &made))
                        .expect("the line");
                let case =
                    format!("on={on}, prepared={prepared}, rehearsal={rehearsal:?}: {line:?}");
                let (registered_here, listed_here) = expected(Build::Debug, on, prepared);
                assert_eq!(
                    listed(&line, "registered").contains(&"Threema"),
                    registered_here,
                    "{case}"
                );
                assert_eq!(
                    listed(&line, "off").contains(&"Threema"),
                    listed_here,
                    "{case}"
                );
                assert_eq!(
                    listed(&line, "on"),
                    if on { vec!["Threema"] } else { vec![] },
                    "{case}"
                );
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    usize::from(rehearsal.is_none() && (registered_here || listed_here)),
                    "{case}: the real parts' maker"
                );
                assert_eq!(made.load(Ordering::SeqCst), 0, "{case}: a client was made");
                assert!(!paths.core_db().exists(), "{case}: a database was opened");
                assert_eq!(
                    std::fs::read_to_string(paths.config_file()).expect("prefs file"),
                    file,
                    "{case}: the preferences were changed"
                );
            }
        }
    }
}

/// W2b B2: the WhatsApp backend is registered by a debug build while it is on, listed while it is
/// off, and absent from a release build.
#[cfg(all(test, feature = "whatsapp"))]
mod whatsapp_registration_tests {
    use super::{Build, Prefs, WhatsAppRegistration, whatsapp_registration};
    use kourier_model::NetworkKind;

    fn prefs(on: bool) -> Prefs {
        let mut prefs = Prefs::default();
        prefs.set_network_on(NetworkKind::WhatsApp, on);
        prefs
    }

    /// The rule for every case: a release build neither registers nor lists it, whatever the
    /// preferences say (a test archive must not link a real account); a debug build registers it
    /// while `networks-on` names it and lists it as off otherwise.
    #[test]
    fn a_release_build_never_offers_whatsapp_and_a_debug_build_follows_the_switch() {
        assert_eq!(
            whatsapp_registration(Build::Release, &prefs(true)),
            WhatsAppRegistration::Absent
        );
        assert_eq!(
            whatsapp_registration(Build::Release, &prefs(false)),
            WhatsAppRegistration::Absent
        );
        assert_eq!(
            whatsapp_registration(Build::Debug, &prefs(true)),
            WhatsAppRegistration::Registered
        );
        assert_eq!(
            whatsapp_registration(Build::Debug, &prefs(false)),
            WhatsAppRegistration::Listed
        );
        // The name in the file is meant to be edited by hand too.
        let mut by_hand = Prefs::default();
        by_hand.networks_on.push("whatsapp".to_owned());
        assert_eq!(
            whatsapp_registration(Build::Debug, &by_hand),
            WhatsAppRegistration::Registered
        );
    }

    /// W2b B5: the label the phone lists the device by is made from the host name the APPLICATION
    /// reads (`host_name`, which also names the device in the UI's texts), not read a second way.
    #[test]
    fn the_device_label_comes_from_the_apps_host_name() {
        let parts = kourier_whatsapp::NetParts::refusing(Default::default());
        let network = super::whatsapp_network(parts);
        let expected = match super::host_name() {
            Some(host) if host.chars().all(|c| c.is_ascii_graphic()) && host.len() <= 48 => {
                format!("Kourier ({host})")
            }
            // An exotic host name: how it is cleaned is the backend's own test.
            Some(_) => return,
            None => "Kourier".to_owned(),
        };
        assert_eq!(network.device_label(), expected);
        // The UI's text and the label name the same host.
        if super::host_name().is_some() {
            assert!(expected.contains(&super::hostname()));
        }
    }

    /// The test binary is a debug build, so `Build::current` is `Debug` here; a release build's
    /// is the other value (the compiler decides, `cfg!(debug_assertions)`).
    #[test]
    fn the_build_is_told_by_debug_assertions() {
        assert_eq!(
            Build::current(),
            if cfg!(debug_assertions) {
                Build::Debug
            } else {
                Build::Release
            }
        );
    }

    /// The kinds a list of the `--print-networks` line names (`key=a,b`).
    fn listed<'a>(line: &'a str, key: &str) -> Vec<&'a str> {
        let value = line
            .split(' ')
            .find_map(|word| word.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("no {key}= in {line:?}"));
        value.split(',').filter(|k| !k.is_empty()).collect()
    }

    /// `kourier --print-networks` (hidden; `cargo xtask dist` runs it on the packed binary): the line
    /// of kinds a normal start of the profile would register and list as off, built by the same
    /// function a start uses. A debug build registers WhatsApp while `networks-on` names it and
    /// lists it as off otherwise; a release build names it in neither list whatever the switch says
    /// (`on=` still shows that the file was read, which is what makes the empty lists a proof). The
    /// line is one line of kinds, it makes no client, opens no database and does not clear the
    /// one-shot `pending-link` a start would.
    #[test]
    fn the_networks_line_registers_or_lists_whatsapp_per_the_switch() {
        use std::ffi::OsString;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use super::{Seams, networks_line_in};
        use crate::paths::{HOME_ENV, Paths, Profile};

        for (build, on) in [
            (Build::Debug, false),
            (Build::Debug, true),
            (Build::Release, false),
            (Build::Release, true),
        ] {
            let tmp = tempfile::tempdir().expect("tmp");
            let env = |k: &str| -> Option<OsString> { (k == HOME_ENV).then(|| tmp.path().into()) };
            let paths = Paths::from_env(Profile::default(), env).expect("paths");
            paths.create_dirs().expect("dirs");
            let mut asked = prefs(on);
            asked.pending_link = Some("WhatsApp".to_owned());
            asked.save(&paths.config_file()).expect("save prefs");
            let file = std::fs::read_to_string(paths.config_file()).expect("prefs file");
            let made = Arc::new(AtomicUsize::new(0));
            let seams = Seams {
                build,
                whatsapp_parts: {
                    let made = made.clone();
                    Box::new(move || kourier_whatsapp::NetParts::refusing(made))
                },
                // Telegram is off in these cases: its maker is never called.
                #[cfg(feature = "telegram")]
                telegram_parts: Box::new(|| kourier_telegram::TgParts::refusing(Arc::default())),
                #[cfg(feature = "telegram")]
                telegram_state: crate::telegram::PartsState::Missing(
                    crate::telegram::TelegramMissing::ApiKey,
                ),
                #[cfg(feature = "threema")]
                threema_parts: super::refusing_threema_parts(Default::default()),
            };
            let line = networks_line_in(&paths, env, seams).expect("the line");
            let case = format!("{build:?}, on={on}: {line:?}");
            let (registered, off, switched) = (
                listed(&line, "registered"),
                listed(&line, "off"),
                listed(&line, "on"),
            );
            let registered_here = build == Build::Debug && on;
            let listed_here = build == Build::Debug && !on;
            assert_eq!(registered.contains(&"WhatsApp"), registered_here, "{case}");
            assert_eq!(off.contains(&"WhatsApp"), listed_here, "{case}");
            // The switch as the file has it, in a release build too. Threema is on by default and
            // has its own test.
            assert_eq!(
                switched
                    .iter()
                    .copied()
                    .filter(|kind| *kind != "Threema")
                    .collect::<Vec<_>>(),
                if on { vec!["WhatsApp"] } else { vec![] },
                "{case}"
            );
            // One line of kinds and nothing else: no path, no account.
            assert!(line.starts_with("networks registered="), "{case}");
            assert!(!line.contains('\n') && !line.contains(tmp.path().to_str().unwrap()));
            let names = NetworkKind::ALL.map(NetworkKind::as_str);
            for kind in registered.iter().chain(&off).chain(&switched) {
                assert!(names.contains(kind), "{case}: {kind}");
            }
            #[cfg(feature = "signal")]
            assert!(registered.contains(&"Signal"), "{case}");
            // Nothing was made, opened or consumed.
            assert_eq!(made.load(Ordering::SeqCst), 0, "{case}: a client was made");
            assert!(!paths.core_db().exists(), "{case}: a database was opened");
            assert_eq!(
                std::fs::read_to_string(paths.config_file()).expect("prefs file"),
                file,
                "{case}: the preferences were changed (pending-link cleared?)"
            );
        }
    }

    /// THE WIRING (debug builds only: the test starts from the development keyring, which release
    /// builds lack): a normal start registers the WhatsApp backend only while `networks-on` names
    /// it, lists it as off otherwise (described for the UI, and asking first), offers nothing in a
    /// release build, and in none of them makes a client: the parts' maker is not called until an
    /// account connects or starts a login.
    #[cfg(debug_assertions)]
    #[tokio::test]
    async fn a_normal_start_registers_whatsapp_only_when_on() {
        use std::ffi::OsString;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use super::{Seams, normal_setup_with};
        use crate::dev::DEV_KEYRING_ENV;
        use crate::paths::{HOME_ENV, Paths, Profile};

        for (build, on) in [
            (Build::Debug, false),
            (Build::Debug, true),
            (Build::Release, false),
            (Build::Release, true),
        ] {
            let tmp = tempfile::tempdir().expect("tmp");
            let env = |k: &str| -> Option<OsString> {
                match k {
                    HOME_ENV => Some(tmp.path().into()),
                    DEV_KEYRING_ENV => Some("passphrase".into()),
                    _ => None,
                }
            };
            let paths = Paths::from_env(Profile::default(), env).expect("paths");
            paths.create_dirs().expect("dirs");
            prefs(on).save(&paths.config_file()).expect("save prefs");
            let made = Arc::new(AtomicUsize::new(0));
            let seams = Seams {
                build,
                whatsapp_parts: {
                    let made = made.clone();
                    Box::new(move || kourier_whatsapp::NetParts::refusing(made))
                },
                // Telegram is off in these cases: its maker is never called.
                #[cfg(feature = "telegram")]
                telegram_parts: Box::new(|| kourier_telegram::TgParts::refusing(Arc::default())),
                #[cfg(feature = "telegram")]
                telegram_state: crate::telegram::PartsState::Missing(
                    crate::telegram::TelegramMissing::ApiKey,
                ),
                #[cfg(feature = "threema")]
                threema_parts: super::refusing_threema_parts(Default::default()),
            };
            let setup = normal_setup_with(paths, env, seams)
                .await
                .expect("a normal setup");
            let registered = setup.registry.get(NetworkKind::WhatsApp).is_some();
            let listed: Vec<_> = setup
                .networks_off
                .iter()
                .filter(|n| n.kind == NetworkKind::WhatsApp)
                .collect();
            let case = format!("{build:?}, on={on}");
            match whatsapp_registration(build, &prefs(on)) {
                WhatsAppRegistration::Registered => {
                    assert!(registered && listed.is_empty(), "{case}");
                }
                WhatsAppRegistration::Listed => {
                    assert!(!registered && listed.len() == 1, "{case}");
                    // Described for the UI: experimental, and K-A's page asks first.
                    let info = listed[0];
                    assert!(info.experimental && !info.history_transfer, "{case}");
                    assert!(info.risk_notice.is_some(), "{case}: it asks first");
                }
                WhatsAppRegistration::Absent => {
                    assert!(!registered && listed.is_empty(), "{case}");
                }
            }
            assert_eq!(
                made.load(Ordering::SeqCst),
                0,
                "{case}: a client was made at the start"
            );
            // The Signal network is registered whatever WhatsApp does.
            #[cfg(feature = "signal")]
            assert!(setup.registry.get(NetworkKind::Signal).is_some(), "{case}");
        }
    }

    /// What a start with `rehearsal` as the value of `KOURIER_DEV_REHEARSAL` (or none) did: the
    /// result, how often the real parts' maker was called, how often a transport was made.
    #[cfg(all(debug_assertions, feature = "fake"))]
    async fn start_with_rehearsal(
        build: Build,
        on: bool,
        rehearsal: Option<&'static str>,
    ) -> (Result<super::Setup, super::AppError>, usize, usize) {
        use std::ffi::OsString;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use super::{Seams, normal_setup_with};
        use crate::dev::{DEV_KEYRING_ENV, REHEARSAL_ENV};
        use crate::paths::{HOME_ENV, Paths, Profile};

        let tmp = tempfile::tempdir().expect("tmp");
        let env = |k: &str| -> Option<OsString> {
            match k {
                HOME_ENV => Some(tmp.path().into()),
                DEV_KEYRING_ENV => Some("passphrase".into()),
                REHEARSAL_ENV => rehearsal.map(OsString::from),
                _ => None,
            }
        };
        let paths = Paths::from_env(Profile::default(), env).expect("paths");
        paths.create_dirs().expect("dirs");
        prefs(on).save(&paths.config_file()).expect("save prefs");
        let (calls, made) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let seams = Seams {
            build,
            whatsapp_parts: {
                let (calls, made) = (calls.clone(), made.clone());
                Box::new(move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    kourier_whatsapp::NetParts::refusing(made)
                })
            },
            // Telegram is off in these cases: its maker is never called.
            #[cfg(feature = "telegram")]
            telegram_parts: Box::new(|| kourier_telegram::TgParts::refusing(Arc::default())),
            #[cfg(feature = "telegram")]
            telegram_state: crate::telegram::PartsState::Missing(
                crate::telegram::TelegramMissing::ApiKey,
            ),
            #[cfg(feature = "threema")]
            threema_parts: super::refusing_threema_parts(Default::default()),
        };
        let result = normal_setup_with(paths, env, seams).await;
        (
            result,
            calls.load(Ordering::SeqCst),
            made.load(Ordering::SeqCst),
        )
    }

    /// T1a (A3): with `KOURIER_DEV_REHEARSAL=whatsapp` the WhatsApp slot holds the scripted
    /// rehearsal fake, and the real parts' maker is not called at all (so no real transport or HTTP
    /// client can ever be made: the rehearsal runs in a namespace with no network, but the point
    /// is that no real code path is taken). The rehearsal network has NO risk notice, has its
    /// control for the dev channel, and is registered only while `networks-on` names it. Without
    /// the switch the maker IS called (the control case). Mutant: call the maker and drop the parts.
    #[cfg(all(debug_assertions, feature = "fake"))]
    #[tokio::test]
    async fn a_rehearsal_start_never_makes_the_real_whatsapp_parts() {
        // The control: the real backend's maker is called once when the switch is absent.
        let (real, calls, made) = start_with_rehearsal(Build::Debug, true, None).await;
        let real = real.expect("a normal setup");
        assert_eq!(calls, 1, "the real maker is called without the switch");
        assert_eq!(made, 0);
        assert!(real.rehearsal.is_none());
        let network = real
            .registry
            .get(NetworkKind::WhatsApp)
            .expect("registered");
        assert!(
            network.descriptor().risk_notice.is_some(),
            "the real one asks first"
        );

        // The rehearsal, switched on: registered, no risk notice, a control, no maker call.
        let (setup, calls, made) = start_with_rehearsal(Build::Debug, true, Some("whatsapp")).await;
        let setup = setup.expect("a rehearsal setup");
        assert_eq!(calls, 0, "the real parts' maker was called in a rehearsal");
        assert_eq!(made, 0);
        let network = setup
            .registry
            .get(NetworkKind::WhatsApp)
            .expect("registered");
        assert!(network.descriptor().experimental);
        assert!(
            network.descriptor().risk_notice.is_none(),
            "the rehearsal asks nothing"
        );
        assert!(
            setup.rehearsal.is_some(),
            "the dev channel needs the control"
        );
        assert!(setup.networks_off.is_empty());

        // Turned off in the preferences: not registered, listed as off, still no maker call.
        let (off, calls, _) = start_with_rehearsal(Build::Debug, false, Some("whatsapp")).await;
        let off = off.expect("a rehearsal setup with the network off");
        assert_eq!(calls, 0);
        assert!(off.registry.get(NetworkKind::WhatsApp).is_none());
        assert_eq!(off.networks_off.len(), 1);

        // A release build never offers WhatsApp, the rehearsal's either.
        let (release, calls, _) =
            start_with_rehearsal(Build::Release, true, Some("whatsapp")).await;
        let release = release.expect("a release setup");
        assert_eq!(calls, 0);
        assert!(release.registry.get(NetworkKind::WhatsApp).is_none());
        assert!(release.networks_off.is_empty() && release.rehearsal.is_none());
    }

    /// Anything but exactly `whatsapp` fails the start: a typo must not start a real run while the
    /// person believes it is a rehearsal, and the error does not repeat the value.
    #[cfg(all(debug_assertions, feature = "fake"))]
    #[tokio::test]
    async fn a_rehearsal_value_other_than_whatsapp_fails_the_start() {
        for bad in ["signal", "WhatsApp", "1", "real"] {
            let (result, calls, made) = start_with_rehearsal(Build::Debug, true, Some(bad)).await;
            let error = match result {
                Err(e) => e,
                Ok(_) => panic!("{bad}: the start went on"),
            };
            assert!(
                matches!(
                    error,
                    super::AppError::Config(super::ConfigError::Rehearsal(_))
                ),
                "{bad}: {error}"
            );
            assert!(
                !error.to_string().contains(bad),
                "{bad}: the value is repeated: {error}"
            );
            assert_eq!((calls, made), (0, 0), "{bad}: something was made");
        }
    }

    /// T1a (A4): the usage-statistics counters of the development state are the REAL backend's,
    /// so `whatsapp_real` is true only when the real WhatsApp backend is made AND registered. A
    /// rehearsal (its fake has no client), the backend listed as off, a release build and a start
    /// with the backend not asked for all say false: a counter read there would be a 0 that proves
    /// nothing. Mutants: the rehearsal's branch sets it; the registry check is dropped.
    #[cfg(all(debug_assertions, feature = "fake"))]
    #[tokio::test]
    async fn the_statistics_counters_belong_to_the_real_backend_only() {
        let (real, ..) = start_with_rehearsal(Build::Debug, true, None).await;
        assert!(real.expect("a normal setup").whatsapp_real, "the real one");
        let (rehearsal, ..) = start_with_rehearsal(Build::Debug, true, Some("whatsapp")).await;
        assert!(
            !rehearsal.expect("a rehearsal setup").whatsapp_real,
            "a rehearsal has no client to count"
        );
        let (off, calls, _) = start_with_rehearsal(Build::Debug, false, None).await;
        assert_eq!(calls, 1, "the backend is made and listed as off");
        assert!(
            !off.expect("a setup with the network off").whatsapp_real,
            "listed as off is not registered"
        );
        let (release, ..) = start_with_rehearsal(Build::Release, true, None).await;
        assert!(!release.expect("a release setup").whatsapp_real);
    }
}

/// TG-1a (decisions D2): the Telegram backend is registered by a debug build while it is on, and
/// neither registered nor listed otherwise (Add Account offers no Telegram until a login works).
#[cfg(all(test, feature = "telegram"))]
mod telegram_registration_tests {
    use std::ffi::OsString;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{
        Build, Prefs, Seams, TDJSON_ENV, TelegramRegistration, telegram_helper_files,
        telegram_registration,
    };
    use crate::telegram::{PartsState, TelegramMissing};
    use kourier_model::NetworkKind;

    fn prefs(on: bool) -> Prefs {
        let mut prefs = Prefs::default();
        prefs.set_network_on(NetworkKind::Telegram, on);
        prefs
    }

    /// Seams for `build` whose Telegram parts refuse and count every helper spawn in `made` (and
    /// whose WhatsApp parts, where built, refuse and count in `made` too), as parts that can log
    /// in (`PartsState::Ready`).
    fn seams(build: Build, made: &Arc<AtomicUsize>) -> Seams {
        seams_in(build, made, PartsState::Ready)
    }

    /// [`seams`] whose Telegram parts are in `state`.
    fn seams_in(build: Build, made: &Arc<AtomicUsize>, state: PartsState) -> Seams {
        Seams {
            build,
            #[cfg(feature = "whatsapp")]
            whatsapp_parts: {
                let made = made.clone();
                Box::new(move || kourier_whatsapp::NetParts::refusing(made))
            },
            telegram_parts: {
                let made = made.clone();
                Box::new(move || kourier_telegram::TgParts::refusing(made))
            },
            telegram_state: state,
            #[cfg(feature = "threema")]
            threema_parts: super::refusing_threema_parts(Default::default()),
        }
    }

    /// TG-1b: the Telegram parts of a start carry `telegram/` of the media cache as their files
    /// root (TDLib's files directory of each account lies below it); adding it makes nothing.
    #[test]
    fn the_telegram_parts_get_the_files_root_on_the_media_cache() {
        use crate::paths::{HOME_ENV, Paths, Profile};

        let tmp = tempfile::tempdir().expect("tmp");
        let env = |k: &str| -> Option<OsString> { (k == HOME_ENV).then(|| tmp.path().into()) };
        let paths = Paths::from_env(Profile::default(), env).expect("paths");
        let made = Arc::new(AtomicUsize::new(0));
        let seams = seams(Build::Debug, &made).with_telegram_files_root(&paths);
        let parts = (seams.telegram_parts)();
        assert_eq!(
            parts.files_root(),
            Some(&paths.media_dir().join("telegram")),
            "the files root is not the media cache's telegram directory"
        );
        assert_eq!(
            made.load(Ordering::SeqCst),
            0,
            "adding the root made a helper"
        );
    }

    /// Critic H3 and KT-1b finding 2, every row: a build whose parts cannot log in names what is
    /// missing (`Unavailable`) whatever the build and the switch; a RELEASE build whose parts
    /// could log in offers nothing (`Absent`); a debug build whose parts can log in registers
    /// Telegram while it is on and lists it as off otherwise. Mutants: the `Missing` arm of a
    /// release build or of a switch that is off returns `Absent` (what KT-1b replaced: those
    /// accounts read "Offline") -> fails; the `Missing` arm while on returns `Registered` -> fails.
    #[test]
    fn the_registration_follows_the_build_the_switch_and_the_parts() {
        use TelegramMissing::{ApiKey, Library};
        let missing = PartsState::Missing;
        for (build, on, state, expected) in [
            (
                Build::Release,
                true,
                PartsState::Ready,
                TelegramRegistration::Absent,
            ),
            (
                Build::Release,
                false,
                PartsState::Ready,
                TelegramRegistration::Absent,
            ),
            (
                Build::Release,
                true,
                missing(ApiKey),
                TelegramRegistration::Unavailable(ApiKey),
            ),
            (
                Build::Release,
                false,
                missing(Library),
                TelegramRegistration::Unavailable(Library),
            ),
            (
                Build::Debug,
                true,
                PartsState::Ready,
                TelegramRegistration::Registered,
            ),
            (
                Build::Debug,
                false,
                PartsState::Ready,
                TelegramRegistration::Listed,
            ),
            (
                Build::Debug,
                true,
                missing(ApiKey),
                TelegramRegistration::Unavailable(ApiKey),
            ),
            (
                Build::Debug,
                true,
                missing(Library),
                TelegramRegistration::Unavailable(Library),
            ),
            (
                Build::Debug,
                false,
                missing(ApiKey),
                TelegramRegistration::Unavailable(ApiKey),
            ),
            (
                Build::Debug,
                false,
                missing(Library),
                TelegramRegistration::Unavailable(Library),
            ),
        ] {
            assert_eq!(
                telegram_registration(build, &prefs(on), state),
                expected,
                "{build:?}, on={on}, {state:?}"
            );
        }
    }

    /// T-d1, THE WIRING (debug builds only: the start uses the development keyring): a normal start
    /// with parts that can log in registers the Telegram backend only in a debug build while
    /// `networks-on` names it, lists it as off in a debug build otherwise (TG-2, critic H3), and in
    /// no case starts a helper (the refusing parts count every spawn).
    /// Signal is registered whatever Telegram does, and WhatsApp's own rule is unchanged (off here:
    /// listed in a debug build, absent in a release build).
    #[cfg(debug_assertions)]
    #[tokio::test]
    async fn a_normal_start_registers_telegram_only_when_on() {
        use super::normal_setup_with;
        use crate::dev::DEV_KEYRING_ENV;
        use crate::paths::{HOME_ENV, Paths, Profile};

        for (build, on) in [
            (Build::Debug, false),
            (Build::Debug, true),
            (Build::Release, false),
            (Build::Release, true),
        ] {
            let tmp = tempfile::tempdir().expect("tmp");
            let env = |k: &str| -> Option<OsString> {
                match k {
                    HOME_ENV => Some(tmp.path().into()),
                    DEV_KEYRING_ENV => Some("passphrase".into()),
                    _ => None,
                }
            };
            let paths = Paths::from_env(Profile::default(), env).expect("paths");
            paths.create_dirs().expect("dirs");
            prefs(on).save(&paths.config_file()).expect("save prefs");
            let made = Arc::new(AtomicUsize::new(0));
            let setup = normal_setup_with(paths, env, seams(build, &made))
                .await
                .expect("a normal setup");
            let case = format!("{build:?}, on={on}");
            let registered = setup.registry.get(NetworkKind::Telegram);
            assert_eq!(
                registered.is_some(),
                build == Build::Debug && on,
                "{case}: registered"
            );
            if let Some(network) = registered {
                assert!(network.descriptor().experimental, "{case}: experimental");
            }
            assert_eq!(
                setup
                    .networks_off
                    .iter()
                    .any(|n| n.kind == NetworkKind::Telegram),
                build == Build::Debug && !on,
                "{case}: Telegram listed as off"
            );
            assert_eq!(
                setup.telegram_missing, None,
                "{case}: parts that can log in"
            );
            assert_eq!(
                made.load(Ordering::SeqCst),
                0,
                "{case}: a helper was started"
            );
            #[cfg(feature = "signal")]
            assert!(setup.registry.get(NetworkKind::Signal).is_some(), "{case}");
            #[cfg(feature = "whatsapp")]
            {
                assert!(
                    setup.registry.get(NetworkKind::WhatsApp).is_none(),
                    "{case}"
                );
                let listed = setup
                    .networks_off
                    .iter()
                    .any(|n| n.kind == NetworkKind::WhatsApp);
                assert_eq!(listed, build == Build::Debug, "{case}: WhatsApp's rule");
            }
        }
    }

    /// `--print-networks` names Telegram under `registered=` only in a debug build with the switch
    /// on, under `off=` only in a debug build with it off (parts that can log in), and always under
    /// `on=` while the file names it.
    #[test]
    fn the_networks_line_names_telegram_only_when_registered() {
        use super::networks_line_in;
        use crate::paths::{HOME_ENV, Paths, Profile};

        for (build, on) in [
            (Build::Debug, false),
            (Build::Debug, true),
            (Build::Release, false),
            (Build::Release, true),
        ] {
            let tmp = tempfile::tempdir().expect("tmp");
            let env = |k: &str| -> Option<OsString> { (k == HOME_ENV).then(|| tmp.path().into()) };
            let paths = Paths::from_env(Profile::default(), env).expect("paths");
            paths.create_dirs().expect("dirs");
            prefs(on).save(&paths.config_file()).expect("save prefs");
            let made = Arc::new(AtomicUsize::new(0));
            let line = networks_line_in(&paths, env, seams(build, &made)).expect("the line");
            let case = format!("{build:?}, on={on}: {line:?}");
            let list = |key: &str| -> Vec<String> {
                line.split(' ')
                    .find_map(|w| w.strip_prefix(&format!("{key}=")))
                    .unwrap_or_else(|| panic!("no {key}= in the line"))
                    .split(',')
                    .filter(|k| !k.is_empty())
                    .map(str::to_owned)
                    .collect()
            };
            let telegram = "Telegram".to_owned();
            assert_eq!(
                list("registered").contains(&telegram),
                build == Build::Debug && on,
                "{case}"
            );
            assert_eq!(
                list("off").contains(&telegram),
                build == Build::Debug && !on,
                "{case}"
            );
            assert_eq!(list("on").contains(&telegram), on, "{case}");
            assert_eq!(
                made.load(Ordering::SeqCst),
                0,
                "{case}: a helper was started"
            );
        }
    }

    /// Critic H3 and KT-1b finding 2, THE WIRING of the missing parts (debug builds only: the
    /// development keyring): a start whose parts cannot log in registers nothing of Telegram,
    /// lists nothing as off, keeps what is missing for the banner and the Settings row, and makes
    /// no parts at all, with the switch ON or OFF and in a debug or a release build (an off switch
    /// or a release build used to read nothing, so those accounts said "Offline").
    /// Mutants: `Unavailable` pushed like `Registered` -> Telegram is registered and the maker
    /// ran; the missing state kept only while the switch is on -> the off cases fail.
    #[cfg(debug_assertions)]
    #[tokio::test]
    async fn a_start_whose_parts_cannot_log_in_registers_no_telegram_and_keeps_why() {
        use super::normal_setup_with;
        use crate::dev::DEV_KEYRING_ENV;
        use crate::paths::{HOME_ENV, Paths, Profile};

        for missing in [TelegramMissing::ApiKey, TelegramMissing::Library] {
            for (build, on) in [
                (Build::Debug, true),
                (Build::Debug, false),
                (Build::Release, true),
                (Build::Release, false),
            ] {
                let case = format!("{missing:?}, {build:?}, on={on}");
                let tmp = tempfile::tempdir().expect("tmp");
                let env = |k: &str| -> Option<OsString> {
                    match k {
                        HOME_ENV => Some(tmp.path().into()),
                        DEV_KEYRING_ENV => Some("passphrase".into()),
                        _ => None,
                    }
                };
                let paths = Paths::from_env(Profile::default(), env).expect("paths");
                paths.create_dirs().expect("dirs");
                prefs(on).save(&paths.config_file()).expect("save prefs");
                let made = Arc::new(AtomicUsize::new(0));
                let maker_ran = Arc::new(AtomicUsize::new(0));
                let mut seams = seams_in(build, &made, PartsState::Missing(missing));
                let ran = maker_ran.clone();
                seams.telegram_parts = Box::new(move || {
                    ran.fetch_add(1, Ordering::SeqCst);
                    kourier_telegram::TgParts::refusing(Arc::default())
                });
                let setup = normal_setup_with(paths, env, seams)
                    .await
                    .expect("a normal setup");
                assert!(
                    setup.registry.get(NetworkKind::Telegram).is_none(),
                    "{case}"
                );
                assert!(
                    setup
                        .networks_off
                        .iter()
                        .all(|n| n.kind != NetworkKind::Telegram),
                    "{case}: listed as off"
                );
                assert_eq!(setup.telegram_missing, Some(missing), "{case}");
                assert_eq!(maker_ran.load(Ordering::SeqCst), 0, "{case}: parts made");
            }
        }
    }

    /// His Q2 answer at the start: `KOURIER_DEV_TELEGRAM_ENV=test` outside a run profile fails a
    /// normal start typed before anything is made (the rules: `crate::telegram` tests).
    #[cfg(debug_assertions)]
    #[tokio::test]
    async fn a_start_asked_for_the_test_servers_outside_the_run_profile_fails() {
        use super::{AppError, normal_setup_with};
        use crate::config::ConfigError;
        use crate::dev::DEV_KEYRING_ENV;
        use crate::paths::{HOME_ENV, Paths, Profile};
        use crate::telegram::TELEGRAM_ENV;

        let tmp = tempfile::tempdir().expect("tmp");
        let env = |k: &str| -> Option<OsString> {
            match k {
                HOME_ENV => Some(tmp.path().into()),
                DEV_KEYRING_ENV => Some("passphrase".into()),
                TELEGRAM_ENV => Some("test".into()),
                _ => None,
            }
        };
        let paths = Paths::from_env(Profile::default(), env).expect("paths");
        paths.create_dirs().expect("dirs");
        prefs(true).save(&paths.config_file()).expect("save prefs");
        let made = Arc::new(AtomicUsize::new(0));
        let refused = normal_setup_with(paths, env, seams(Build::Debug, &made)).await;
        assert!(
            matches!(refused, Err(AppError::Config(ConfigError::Telegram(_)))),
            "the test servers were taken outside the run profile"
        );
        assert_eq!(made.load(Ordering::SeqCst), 0);
    }

    /// The helper files: the helper beside the binary when it is there (a cargo target directory),
    /// else in the installed tree; the library from `KOURIER_TDJSON` in a debug build, else
    /// `target/tdlib/` when it holds the pinned file, else the installed tree. Nothing is opened.
    #[test]
    fn the_helper_files_are_found_beside_the_binary_or_in_the_installed_tree() {
        let tmp = tempfile::tempdir().expect("tmp");
        let bin = tmp.path().join("debug");
        std::fs::create_dir_all(&bin).expect("bin dir");
        let exe = bin.join("kourier");
        let name = format!("libtdjson.so.{}", kourier_telegram::PIN.version);
        let none = |_: &str| -> Option<OsString> { None };

        // Nothing beside the binary: the installed tree's places.
        let files = telegram_helper_files(Some(&exe), none);
        assert_eq!(files.helper, bin.join("../lib/kourier/kourier-tdlib"));
        assert_eq!(files.tdjson, bin.join("../lib/kourier").join(&name));
        assert!(!files.no_dump, "no dump switch was asked for");

        // A cargo target directory: the helper beside the binary, the library in target/tdlib/.
        std::fs::write(bin.join("kourier-tdlib"), b"").expect("helper");
        std::fs::create_dir_all(tmp.path().join("tdlib")).expect("tdlib dir");
        std::fs::write(tmp.path().join("tdlib").join(&name), b"").expect("library");
        let files = telegram_helper_files(Some(&exe), none);
        assert_eq!(files.helper, bin.join("kourier-tdlib"));
        assert_eq!(files.tdjson, bin.join("../tdlib").join(&name));

        // KOURIER_TDJSON wins in a debug build and is ignored in a release build (F13: the test
        // binary is one or the other); the dump switch passes through in a debug build only.
        let env = |k: &str| -> Option<OsString> {
            match k {
                TDJSON_ENV => Some("/elsewhere/libtdjson.so".into()),
                crate::process::NO_DUMP_ENV => Some("1".into()),
                _ => None,
            }
        };
        let files = telegram_helper_files(Some(&exe), env);
        let expected = if cfg!(debug_assertions) {
            std::path::PathBuf::from("/elsewhere/libtdjson.so")
        } else {
            bin.join("../tdlib").join(&name)
        };
        assert_eq!(files.tdjson, expected);
        assert_eq!(files.no_dump, cfg!(debug_assertions));
    }

    /// F15: without `current_exe` (or with a path that is not absolute) the files cannot exist; a
    /// helper or a library is never looked for relative to the working directory.
    #[test]
    fn without_the_binarys_own_place_the_helper_files_cannot_exist() {
        let none = |_: &str| -> Option<OsString> { None };
        for exe in [
            None,
            Some(std::path::Path::new("")),
            Some(std::path::Path::new("kourier")),
            Some(std::path::Path::new("./target/debug/kourier")),
            Some(std::path::Path::new("/")),
        ] {
            let files = telegram_helper_files(exe, none);
            assert_eq!(files, kourier_telegram::HelperFiles::not_found(), "{exe:?}");
            for path in [&files.helper, &files.tdjson] {
                assert!(path.is_absolute(), "{path:?} is relative: {exe:?}");
                assert!(
                    path.starts_with(kourier_telegram::HelperFiles::NOT_FOUND_DIR),
                    "{path:?}: {exe:?}"
                );
                assert!(!path.exists(), "{path:?} exists");
            }
        }
    }

    /// TG-1b fix G1: the application's own Telegram parts (`Seams::real()`, the maker the shipped
    /// application uses) carry NO API pair (none is compiled into any build; never the dummy
    /// pair), so a debug start with Telegram on does not register it (TG-2 critic H3:
    /// `Unavailable(ApiKey)`), and the network over those very parts (the maker of
    /// `Seams::real()`, not the registration) answers "this build has no Telegram API key" at
    /// `StartLogin` and starts NOTHING: no account directory is made (they are made after the key
    /// is looked at, before the gate and the helper), no helper is looked for, no library opened.
    /// Mutant: the dummy pair wired into `real_telegram` -> the start goes on and the answer
    /// is another one. (A test process run inside a kept Telegram run profile with a developer's
    /// API file would read that file: the test is not run there.)
    #[cfg(debug_assertions)]
    #[test]
    fn the_shipped_telegram_parts_answer_no_api_key_and_start_nothing() {
        use super::normal_setup_with;
        use crate::dev::DEV_KEYRING_ENV;
        use crate::paths::{HOME_ENV, Paths, Profile};
        use kourier_backend_api::{
            AccountContext, BackendEvent, Command, ContextParams, DbKey, StartMode, SystemClock,
        };
        use kourier_model::{AccountId, AccountUuid, ErrorKind, LoginStepKind};
        use std::time::Duration;
        use tokio_util::sync::CancellationToken;

        let tmp = tempfile::tempdir().expect("tmp");
        let env = |k: &str| -> Option<OsString> {
            match k {
                HOME_ENV => Some(tmp.path().into()),
                DEV_KEYRING_ENV => Some("passphrase".into()),
                _ => None,
            }
        };
        let paths = Paths::from_env(Profile::default(), env).expect("paths");
        paths.create_dirs().expect("dirs");
        prefs(true).save(&paths.config_file()).expect("save prefs");
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let setup = rt
            .block_on(normal_setup_with(paths.clone(), env, Seams::real()))
            .expect("a normal setup");
        assert!(
            setup.registry.get(NetworkKind::Telegram).is_none(),
            "Telegram is registered without an API pair"
        );
        assert_eq!(
            setup.telegram_missing,
            Some(TelegramMissing::ApiKey),
            "the missing pair is not kept"
        );
        let shipped = Seams::real().with_telegram_files_root(&paths);
        let network: Arc<dyn kourier_backend_api::Network> =
            Arc::new(super::telegram_network((shipped.telegram_parts)()));
        let account = tmp.path().join("account");
        std::fs::create_dir_all(&account).expect("account dir");
        let (ctx, mut ends) = AccountContext::new(ContextParams {
            account: AccountId(1),
            uuid: AccountUuid::new_random(),
            start: StartMode::Fresh,
            backend_db: account.join("backend.db"),
            db_key: DbKey::new([7; 32]),
            blob_dir: account.join("blobs"),
            net_rt: rt.handle().clone(),
            cancel: CancellationToken::new(),
            clock: Arc::new(SystemClock),
            config: Default::default(),
            ui: Default::default(),
            local_prefs: Default::default(),
        });
        let handle = network.spawn(ctx).expect("the account starts");
        let detail = rt.block_on(async {
            ends.commands
                .send(Command::StartLogin {
                    flow: "phone".to_owned(),
                    relink: None,
                    taken: Vec::new(),
                })
                .await
                .expect("queued");
            let envelope = tokio::time::timeout(Duration::from_secs(20), ends.events.recv())
                .await
                .expect("no login step in time")
                .expect("events ended");
            let detail = match &envelope.events[..] {
                [BackendEvent::Login(step)] => match &step.kind {
                    LoginStepKind::Failed { error } if error.kind == ErrorKind::Unsupported => {
                        error.detail.clone()
                    }
                    _ => panic!("the login step is not a typed unsupported"),
                },
                _ => panic!("not exactly one login step"),
            };
            ends.commands.send(Command::Shutdown).await.expect("queued");
            detail
        });
        let exit = rt.block_on(handle.exited).expect("the actor ends");
        assert_eq!(exit, kourier_backend_api::Exit::Shutdown);
        assert_eq!(
            detail,
            kourier_telegram::offline::NO_API_KEY,
            "the shipped parts went past the API key"
        );
        // Nothing was made for the start: no files directory below the Telegram files root.
        let made = std::fs::read_dir(paths.media_dir().join("telegram"))
            .map(|dir| dir.count())
            .unwrap_or(0);
        assert_eq!(made, 0, "a start made files before the API key was checked");
    }

    /// A parts maker for `real_telegram_in` that records the mode it was handed (as `DialMode::word`:
    /// a test never names the variant that dials) and makes parts that refuse.
    fn recording<E>(
        seen: &Arc<std::sync::Mutex<Vec<&'static str>>>,
    ) -> impl FnOnce(
        Option<&std::path::Path>,
        E,
        Option<kourier_telegram::ApiCredentials>,
        kourier_telegram::real::DialMode,
    ) -> kourier_telegram::TgParts
    + 'static {
        let seen = seen.clone();
        move |_exe: Option<&std::path::Path>,
              _env: E,
              _api: Option<kourier_telegram::ApiCredentials>,
              mode: kourier_telegram::real::DialMode| {
            seen.lock().expect("lock").push(mode.word());
            kourier_telegram::TgParts::refusing(Arc::default())
        }
    }

    /// TR R0a A3: the dialer mode of the application's Telegram parts follows the confirmation of
    /// THIS start, and the decision is told once in the INFO line of fixed words. A confirmed debug
    /// start tells `mode=telegram reason=confirmed` and its maker gets that mode; an unconfirmed
    /// one (no variable) tells `refusing`/`no-confirmation`, leaves the file and its maker gets
    /// the refusing mode; a RELEASE build with the very same files and variable tells
    /// `refusing`/`release`, never reads the file (it stays) and gets the refusing mode; a second
    /// call in the same process after a confirmed one finds no file and refuses (it cannot widen the
    /// first). The decision is taken at once and handed to the lazy maker: nothing is made before
    /// the maker is called. Mutant: `real_telegram_in` hands the maker the mode that dials for a
    /// debug build whatever the confirmation said -> the unconfirmed case fails.
    #[cfg(debug_assertions)]
    #[test]
    fn the_dialer_mode_follows_the_confirmation() {
        use std::os::unix::fs::PermissionsExt as _;

        use super::real_telegram_in;
        use crate::paths::HOME_ENV;
        use crate::process::EXPECT_NUMBER_TAIL_ENV;
        use crate::telegram::capture::events_of;
        use crate::telegram::{DIAL_DIR, DIAL_ENV, DIAL_FILE, RUN_DIR, TELEGRAM_ENV, sha256_hex};

        const NOW: u64 = 1_800_000_000_000;
        let tmp = tempfile::tempdir().expect("tmp");
        let data = std::fs::canonicalize(tmp.path())
            .expect("canonical")
            .join("data");
        let root = data.join(RUN_DIR);
        let profile = root.join("p");
        let dir = profile.join(DIAL_DIR);
        std::fs::create_dir_all(&dir).expect("dirs");
        for private in [&root, &dir] {
            std::fs::set_permissions(private, std::fs::Permissions::from_mode(0o700))
                .expect("chmod");
        }
        let nonce = "ab".repeat(32);
        let hash = sha256_hex(nonce.as_bytes());
        let file = dir.join(DIAL_FILE);
        let write = || {
            let text = format!(
                r#"{{"version":1,"issued_ms":{NOW},"env":"test","nonce_sha256":"{hash}"}}"#
            );
            let _ = std::fs::remove_file(&file);
            std::fs::write(&file, text).expect("write");
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        };
        let env_with = |variable: bool| {
            let mut vars: Vec<(&'static str, OsString)> = vec![
                ("XDG_DATA_HOME", data.clone().into_os_string()),
                (HOME_ENV, profile.clone().into_os_string()),
                (TELEGRAM_ENV, "test".into()),
                (EXPECT_NUMBER_TAIL_ENV, "0".repeat(15).into()),
            ];
            if variable {
                vars.push((DIAL_ENV, nonce.clone().into()));
            }
            move |k: &str| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
        };
        // One start: the words the INFO line told, and the modes the maker was handed.
        let start = |build: Build, variable: bool| {
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let ((_, maker), events) = events_of(|| {
                real_telegram_in(build, None, env_with(variable), NOW, recording(&seen))
            });
            assert!(
                seen.lock().expect("lock").is_empty(),
                "the parts were made before the maker was called"
            );
            let _parts = maker();
            let told: Vec<String> = events
                .iter()
                .filter(|(_, fields)| fields.contains("telegram dialer"))
                .map(|(level, fields)| {
                    assert_eq!(*level, tracing::Level::INFO);
                    fields.clone()
                })
                .collect();
            for (_, fields) in &events {
                for secret in [
                    nonce.as_str(),
                    hash.as_str(),
                    profile.to_str().expect("utf8"),
                ] {
                    assert!(!fields.contains(secret), "an event holds a value: {fields}");
                }
            }
            let modes = seen.lock().expect("lock").clone();
            (told, modes)
        };
        let line = |mode: &str, reason: &str| format!("mode=\"{mode}\" reason=\"{reason}\"");

        // Confirmed: the line, the mode, and the file is consumed.
        write();
        let (told, modes) = start(Build::Debug, true);
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].contains(&line("telegram", "confirmed")), "{told:?}");
        assert_eq!(modes, ["telegram"]);
        assert!(!file.exists(), "the confirmation was not consumed");
        // A second start in the same process: no file, so no confirmation.
        let (told, modes) = start(Build::Debug, true);
        assert!(told[0].contains(&line("refusing", "file")), "{told:?}");
        assert_eq!(modes, ["refusing"], "a second call widened the first");

        // Not confirmed (no variable): refusing, and the file is left alone.
        write();
        let (told, modes) = start(Build::Debug, false);
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(
            told[0].contains(&line("refusing", "no-confirmation")),
            "{told:?}"
        );
        assert_eq!(modes, ["refusing"], "an unconfirmed start dials");
        assert!(file.exists(), "a start without the variable took the file");

        // A release build, the same files and the variable: refusing, the file is not read.
        let (told, modes) = start(Build::Release, true);
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].contains(&line("refusing", "release")), "{told:?}");
        assert_eq!(modes, ["refusing"], "a release build dials");
        assert!(file.exists(), "a release build read the file");
        // (and the debug build with the variable still finds it whole)
        let (_, modes) = start(Build::Debug, true);
        assert_eq!(modes, ["telegram"]);
    }

    /// TR R0a (critic: one confirmation per start; fix G-4): the Telegram maker takes the
    /// confirmation in exactly ONE entry point of a process, the normal start (`Seams::real()` in
    /// `normal_setup_in`, once per `App::run`). `--print-networks` (`networks_line`, which exits)
    /// calls `Seams::probe()` instead, whose decision is judged over an environment without the
    /// nonce, so a probe never takes a confirmation. A restart is a new process with a new
    /// environment and finds no file. A third caller would be a second confirmation per start:
    /// this test makes whoever adds one argue for it. Mutants: a second `Seams::real()` call in
    /// non-test code; `networks_line` calling `Seams::real()` again; a third place that makes the
    /// Telegram decision (`real_telegram(`, `real_telegram_over(`, `real_telegram_in(` or
    /// `dial_confirmation(`) -> fails.
    #[test]
    fn the_confirmation_is_taken_by_one_entry_point_of_a_process_only() {
        let text = include_str!("app.rs");
        let cut = text
            .find("\n#[cfg(test)]\nmod ")
            .expect("the first test module");
        let code: Vec<&str> = text[..cut]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect();
        let calls_of = |needle: &str| -> Vec<&str> {
            code.iter()
                .copied()
                .filter(|l| l.contains(needle))
                .collect()
        };
        let real = calls_of(&["Seams::", "real()"].concat());
        assert_eq!(real.len(), 1, "{real:#?}");
        assert!(
            real[0].contains("normal_setup_scripted_with(paths, env, "),
            "{real:#?}"
        );
        let probe = calls_of(&["Seams::", "probe()"].concat());
        assert_eq!(probe.len(), 1, "{probe:#?}");
        assert!(probe[0].contains("networks_line_in(&paths, "), "{probe:#?}");
        let maker = calls_of(&["real_telegram(", "Build::current())"].concat());
        assert_eq!(
            maker.len(),
            1,
            "real_telegram is called from one place only"
        );
        // The judgment over an environment: `real_telegram` (the process's, nonce hidden in a
        // test) and the probe (nonce hidden always), and nowhere else.
        let over = calls_of(&["real_telegram_", "over("].concat());
        assert_eq!(over.len(), 2, "{over:#?}");
        // Re-check N-5: the two steps below it each have ONE caller too, and it is the step above:
        // `real_telegram_in(` (the decision over an environment) is called inside
        // `real_telegram_over` only, and `dial_confirmation(` (the decision itself) inside
        // `real_telegram_in` only. A third way to decide names none of the checked spellings
        // above and would pass rule (b); it cannot pass this.
        let at = |needle: &str| -> Vec<usize> {
            code.iter()
                .enumerate()
                .filter(|(_, l)| l.contains(needle))
                .map(|(i, _)| i)
                .collect()
        };
        let one = |needle: &str| -> usize {
            let found = at(needle);
            assert_eq!(found.len(), 1, "`{needle}` is not in one place: {found:?}");
            found[0]
        };
        let over_fn = one(&["fn real_telegram_", "over<"].concat());
        let in_fn = one(&["fn real_telegram_", "in<"].concat());
        let after_in_fn = one(&["fn unix_now_", "ms("].concat());
        let in_call = one(&["real_telegram_", "in("].concat());
        let decision = one(&["dial_", "confirmation("].concat());
        assert!(
            over_fn < in_call && in_call < in_fn,
            "`real_telegram_in(` is called outside `real_telegram_over`"
        );
        assert!(
            in_fn < decision && decision < after_in_fn,
            "`dial_confirmation(` is called outside `real_telegram_in`"
        );
    }

    /// Re-check N-6: a start whose rehearsal switch this build cannot honour is refused before
    /// `Seams::real()` is made (the maker's decision takes the confirmation and logs the dialer
    /// line). In a build WITH a rehearsal network the judgment is a no-op, and the unit test cannot
    /// show the order (a test hides the nonce), so the order is pinned in the source: in
    /// `normal_setup_in`, `rehearsal_refused(` comes before `Seams::real()`. Mutant: the two
    /// swapped; the refusal removed.
    #[test]
    fn a_refused_rehearsal_is_judged_before_the_telegram_decision_is_made() {
        let text = include_str!("app.rs");
        let cut = text
            .find("\n#[cfg(test)]\nmod ")
            .expect("the first test module");
        let code = &text[..cut];
        let start = code
            .find(&["async fn normal_setup_", "in("].concat())
            .expect("normal_setup_in");
        let end = start
            + code[start..]
                .find(&["async fn normal_setup_", "with("].concat())
                .expect("normal_setup_with");
        let body: String = code[start..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let refused = body
            .find(&["rehearsal_", "refused("].concat())
            .expect("normal_setup_in judges the rehearsal switch");
        let made = body
            .find(&["Seams::", "real()"].concat())
            .expect("normal_setup_in makes the real seams");
        assert!(
            refused < made,
            "the rehearsal switch is judged after the Telegram decision was made"
        );
    }

    /// What a normal start with `value` as `KOURIER_DEV_REHEARSAL` did: the result, how often the
    /// Telegram parts' maker was CALLED (a call that drops what it makes counts too) and how many
    /// helpers the parts it made spawned. The Telegram parts are in `state`.
    #[cfg(all(debug_assertions, feature = "fake"))]
    async fn start_rehearsing(
        build: Build,
        on: bool,
        state: PartsState,
        value: Option<&'static str>,
    ) -> (Result<super::Setup, super::AppError>, usize, usize) {
        use super::normal_setup_with;
        use crate::dev::{DEV_KEYRING_ENV, REHEARSAL_ENV};
        use crate::paths::{HOME_ENV, Paths, Profile};

        let tmp = tempfile::tempdir().expect("tmp");
        let env = |k: &str| -> Option<OsString> {
            match k {
                HOME_ENV => Some(tmp.path().into()),
                DEV_KEYRING_ENV => Some("passphrase".into()),
                REHEARSAL_ENV => value.map(OsString::from),
                _ => None,
            }
        };
        let paths = Paths::from_env(Profile::default(), env).expect("paths");
        paths.create_dirs().expect("dirs");
        prefs(on).save(&paths.config_file()).expect("save prefs");
        let (calls, made) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let mut seams = seams_in(build, &made, state);
        seams.telegram_parts = {
            let (calls, made) = (calls.clone(), made.clone());
            Box::new(move || {
                calls.fetch_add(1, Ordering::SeqCst);
                kourier_telegram::TgParts::refusing(made)
            })
        };
        let result = normal_setup_with(paths, env, seams).await;
        (
            result,
            calls.load(Ordering::SeqCst),
            made.load(Ordering::SeqCst),
        )
    }

    /// TR R0a A4, THE REHEARSAL SEAM: with `KOURIER_DEV_REHEARSAL=telegram` in a debug build the
    /// Telegram slot holds the scripted fake (`rehearsal_telegram`: Telegram's descriptor without
    /// the risk notice) INSTEAD of the real backend, and the Telegram parts' maker is never called:
    /// no real parts, helper or dialer exist in a rehearsal (the run tool's confirmed mode is then
    /// harmless). It needs no API pair and no helper files (every state of the parts gives the same
    /// slot, `telegram_missing` stays none), is registered while `networks-on` names Telegram and
    /// listed as off otherwise, and hands the dev channel the fake's control (`state.rehearsal`).
    /// `--print-networks` names Telegram in `registered=` with the same inputs, which is what the
    /// run tool's probe expects. A release build offers nothing of it and a missing key there still
    /// says so; the control (no switch) calls the maker once and has the real descriptor. Mutant:
    /// the rehearsal arm also calls `seams.telegram_parts` -> `calls` is 1 -> fails.
    #[cfg(all(debug_assertions, feature = "fake"))]
    #[tokio::test]
    async fn a_rehearsal_start_never_makes_the_real_telegram_parts() {
        use TelegramMissing::{ApiKey, Library};

        // The control: without the switch the real parts' maker IS called (once), and the network
        // is the real one, which asks first.
        let (real, calls, made) =
            start_rehearsing(Build::Debug, true, PartsState::Ready, None).await;
        let real = real.expect("a normal setup");
        assert_eq!(
            (calls, made),
            (1, 0),
            "the real maker is called without the switch"
        );
        assert!(real.rehearsal.is_none());
        let network = real
            .registry
            .get(NetworkKind::Telegram)
            .expect("registered");
        assert!(
            network.descriptor().risk_notice.is_some(),
            "the real one asks first"
        );

        // The rehearsal: whatever the parts' state (no API pair, no helper files, or both fine).
        for state in [
            PartsState::Ready,
            PartsState::Missing(ApiKey),
            PartsState::Missing(Library),
        ] {
            let (setup, calls, made) =
                start_rehearsing(Build::Debug, true, state, Some("telegram")).await;
            let case = format!("{state:?}");
            let setup = setup.expect("a rehearsal setup");
            assert_eq!(
                calls, 0,
                "{case}: the real parts' maker was called in a rehearsal"
            );
            assert_eq!(made, 0, "{case}");
            let network = setup
                .registry
                .get(NetworkKind::Telegram)
                .unwrap_or_else(|| panic!("{case}: not registered"));
            assert_eq!(network.descriptor().display_name, "Telegram", "{case}");
            assert!(network.descriptor().experimental, "{case}");
            assert!(
                network.descriptor().risk_notice.is_none(),
                "{case}: the rehearsal asks nothing"
            );
            assert!(
                setup.rehearsal.is_some(),
                "{case}: the dev channel needs the control"
            );
            assert_eq!(setup.telegram_missing, None, "{case}");
            assert!(
                !setup
                    .networks_off
                    .iter()
                    .any(|n| n.kind == NetworkKind::Telegram),
                "{case}"
            );
            assert!(!setup.whatsapp_real, "{case}");
        }

        // Turned off in the preferences: not registered, listed as off (as the fake), still no call.
        let (off, calls, made) = start_rehearsing(
            Build::Debug,
            false,
            PartsState::Missing(ApiKey),
            Some("telegram"),
        )
        .await;
        let off = off.expect("a rehearsal setup with the network off");
        assert_eq!((calls, made), (0, 0));
        assert!(off.registry.get(NetworkKind::Telegram).is_none());
        let listed: Vec<_> = off
            .networks_off
            .iter()
            .filter(|n| n.kind == NetworkKind::Telegram)
            .collect();
        assert_eq!(listed.len(), 1, "listed as off once");
        assert!(
            listed[0].risk_notice.is_none(),
            "the fake is what is listed"
        );
        assert_eq!(off.telegram_missing, None);

        // Another network's word leaves Telegram real.
        #[cfg(feature = "whatsapp")]
        {
            let (other, calls, _) =
                start_rehearsing(Build::Debug, true, PartsState::Ready, Some("whatsapp")).await;
            let other = other.expect("a WhatsApp rehearsal setup");
            assert_eq!(calls, 1, "Telegram stays real in a WhatsApp rehearsal");
            let network = other
                .registry
                .get(NetworkKind::Telegram)
                .expect("registered");
            assert!(network.descriptor().risk_notice.is_some());
        }

        // A release build offers no rehearsal, and a missing key there is still named.
        let (release, calls, _) =
            start_rehearsing(Build::Release, true, PartsState::Ready, Some("telegram")).await;
        let release = release.expect("a release setup");
        assert_eq!(calls, 0);
        assert!(release.registry.get(NetworkKind::Telegram).is_none());
        assert!(
            release
                .networks_off
                .iter()
                .all(|n| n.kind != NetworkKind::Telegram)
        );
        assert!(release.rehearsal.is_none());
        let (release, calls, _) = start_rehearsing(
            Build::Release,
            true,
            PartsState::Missing(ApiKey),
            Some("telegram"),
        )
        .await;
        let release = release.expect("a release setup");
        assert_eq!(calls, 0);
        assert_eq!(release.telegram_missing, Some(ApiKey));
        assert!(release.rehearsal.is_none());

        // `--print-networks` (the run tool's probe) says Telegram is registered in a rehearsal
        // without any API pair or helper file, and not without the switch.
        {
            use super::networks_line_in;
            use crate::dev::REHEARSAL_ENV;
            use crate::paths::{HOME_ENV, Paths, Profile};

            let registered = |value: Option<&'static str>| {
                let tmp = tempfile::tempdir().expect("tmp");
                let env = |k: &str| -> Option<OsString> {
                    match k {
                        HOME_ENV => Some(tmp.path().into()),
                        REHEARSAL_ENV => value.map(OsString::from),
                        _ => None,
                    }
                };
                let paths = Paths::from_env(Profile::default(), env).expect("paths");
                paths.create_dirs().expect("dirs");
                prefs(true).save(&paths.config_file()).expect("save prefs");
                let calls = Arc::new(AtomicUsize::new(0));
                let mut seams =
                    seams_in(Build::Debug, &Arc::default(), PartsState::Missing(ApiKey));
                seams.telegram_parts = {
                    let calls = calls.clone();
                    Box::new(move || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        kourier_telegram::TgParts::refusing(Arc::default())
                    })
                };
                let line = networks_line_in(&paths, env, seams).expect("the line");
                let registered = line
                    .split(' ')
                    .find_map(|w| w.strip_prefix("registered="))
                    .expect("registered=")
                    .split(',')
                    .any(|k| k == "Telegram");
                (registered, calls.load(Ordering::SeqCst), line)
            };
            let (yes, calls, line) = registered(Some("telegram"));
            assert!(yes, "{line:?}");
            assert_eq!(calls, 0, "{line:?}");
            let (no, _, line) = registered(None);
            assert!(!no, "no key and no switch: {line:?}");
        }
    }

    /// A value other than exactly `telegram` fails the start before anything of Telegram is made
    /// (the same error as WhatsApp's: a typo must not start the real backend while the person
    /// believes it is a rehearsal), and the error does not repeat the value.
    #[cfg(all(debug_assertions, feature = "fake"))]
    #[tokio::test]
    async fn a_rehearsal_value_that_is_not_a_network_fails_the_start_before_telegram_is_made() {
        let mut words = std::collections::BTreeSet::new();
        for bad in [
            "Telegram",
            "telegram ",
            "tg",
            "real",
            "1",
            "telegram,whatsapp",
        ] {
            let (result, calls, made) =
                start_rehearsing(Build::Debug, true, PartsState::Ready, Some(bad)).await;
            let error = match result {
                Err(e) => e,
                Ok(_) => panic!("{bad:?}: the start went on"),
            };
            assert!(
                matches!(
                    error,
                    super::AppError::Config(super::ConfigError::Rehearsal(_))
                ),
                "{bad:?}: {error}"
            );
            assert_eq!((calls, made), (0, 0), "{bad:?}: something was made");
            words.insert(error.to_string());
        }
        // One sentence for every value: nothing of the value is repeated.
        assert_eq!(words.len(), 1, "{words:?}");
    }

    /// TG-1b B-5 (TG-1a Q4), THE APPLICATION OFFLINE: a debug start with Telegram on, the
    /// application's own helper parts (`real_telegram_parts_in` over the helper files
    /// `telegram_helper_files` finds, with the dummy API pair the test adds: the application has
    /// none), the REAL helper binary and library;
    /// a login reaches TDLib's phone-number step and nothing goes past the gate. The body runs in
    /// the offline child (`kourier_telegram::offline`: this test binary under `unshare -rn`, every
    /// address routed to `lo`, the traps counting every direct attempt); the parent skips with a
    /// `SKIPPED:` line where the library (`KOURIER_TDJSON`) or `unshare` is missing, and fails
    /// instead under `KOURIER_REQUIRE_TDLIB=1` / `KOURIER_REQUIRE_UNSHARE=1`.
    #[cfg(debug_assertions)]
    mod real_offline {
        use std::ffi::OsString;
        use std::sync::Arc;
        use std::time::Duration;

        use kourier_backend_api::{
            AccountContext, BackendEvent, Command, ContextParams, DbKey, StartMode, SystemClock,
        };
        use kourier_model::{AccountId, AccountUuid, LoginStepKind, NetworkKind};
        use kourier_telegram::offline;
        use tokio_util::sync::CancellationToken;

        use super::super::{
            Build, Seams, TDJSON_ENV, normal_setup_with, real_telegram_parts_in,
            telegram_helper_files,
        };
        use super::prefs;

        const CASE_ENV: &str = "KOURIER_APP_TG_OFFLINE";
        const CHILD: &str = "app::telegram_registration_tests::real_offline::app_child";
        /// The logins of the child: each starts a helper and TDLib behind the gate once more (a
        /// `CancelLogin` after each `phone` step; the next `StartLogin` stops that idle helper and
        /// starts a new one, TG-2 Q-13).
        const LOGINS: usize = 3;

        fn required(var: &str) -> bool {
            std::env::var(var).is_ok_and(|v| v == "1")
        }

        /// The parent half.
        #[test]
        fn a_debug_start_with_the_real_parts_reaches_the_phone_number_and_dials_nothing() {
            if !std::env::var_os(TDJSON_ENV).is_some_and(|v| !v.is_empty()) {
                assert!(
                    !required("KOURIER_REQUIRE_TDLIB"),
                    "{TDJSON_ENV} is unset and KOURIER_REQUIRE_TDLIB=1: this test must run"
                );
                println!("SKIPPED: {TDJSON_ENV} is unset (`cargo xtask tdlib path`)");
                return;
            }
            if !offline::offline_available() {
                assert!(
                    !required("KOURIER_REQUIRE_UNSHARE"),
                    "`unshare -rn` (or `ip`) is unavailable and KOURIER_REQUIRE_UNSHARE=1"
                );
                println!("SKIPPED: `unshare -rn` or `ip` is unavailable");
                return;
            }
            let run =
                offline::run_self_offline(CHILD, &[(CASE_ENV, "1")], Duration::from_secs(240))
                    .expect("start the offline child");
            // (libtest writes "test <name> ... " before the body's first line.)
            for line in run
                .output
                .lines()
                .filter_map(|l| l.find("REAL ").map(|i| &l[i..]))
            {
                println!("{line}");
            }
            assert!(
                run.status.success()
                    && run.output.contains("1 passed")
                    && run.output.contains("REAL app:"),
                "the offline child failed ({:?}):\n{}",
                run.status,
                run.output
            );
        }

        /// The offline child's body (a no-op in a normal run).
        #[test]
        fn app_child() {
            use crate::dev::DEV_KEYRING_ENV;
            use crate::paths::{HOME_ENV, Paths, Profile};

            if std::env::var_os(CASE_ENV).is_none() {
                return;
            }
            assert!(
                offline::in_offline_child(),
                "not in the offline child: the real library is not loaded"
            );
            let traps = offline::Traps::arm();
            let tmp = tempfile::tempdir().expect("tmp");
            let env = |k: &str| -> Option<OsString> {
                match k {
                    HOME_ENV => Some(tmp.path().into()),
                    DEV_KEYRING_ENV => Some("passphrase".into()),
                    TDJSON_ENV => std::env::var_os(TDJSON_ENV),
                    _ => None,
                }
            };
            let paths = Paths::from_env(Profile::default(), env).expect("paths");
            paths.create_dirs().expect("dirs");
            prefs(true).save(&paths.config_file()).expect("save prefs");
            // The application's binary is `target/<profile>/kourier`; this test binary lies in
            // `target/<profile>/deps/`.
            let exe = std::env::current_exe().expect("test binary");
            let bin = exe
                .parent()
                .and_then(std::path::Path::parent)
                .expect("target dir")
                .join("kourier");
            // The application's own composition (`real_telegram_parts_in`); the dummy pair is the
            // one thing the test adds, because the application has no pair of its own.
            let parts = real_telegram_parts_in(
                Some(&bin),
                env,
                Some(kourier_telegram::ApiCredentials::DUMMY),
                // The refusing mode (the type's default): this file names the type only as a type
                // (a source check), so a test asks for the default and names no variant.
                Default::default(),
            );
            assert!(
                telegram_helper_files(Some(&bin), env).helper.is_file(),
                "the helper binary is not built"
            );
            let seams = Seams {
                build: Build::Debug,
                #[cfg(feature = "whatsapp")]
                whatsapp_parts: {
                    let made = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                    Box::new(move || kourier_whatsapp::NetParts::refusing(made))
                },
                telegram_parts: Box::new(move || parts),
                telegram_state: crate::telegram::PartsState::Ready,
                #[cfg(feature = "threema")]
                threema_parts: super::super::refusing_threema_parts(Default::default()),
            };
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime");
            let setup = rt
                .block_on(normal_setup_with(paths.clone(), env, seams))
                .expect("a normal setup");
            let network = setup
                .registry
                .get(NetworkKind::Telegram)
                .expect("Telegram is registered");
            let account = tmp.path().join("account");
            std::fs::create_dir_all(&account).expect("account dir");
            let (ctx, mut ends) = AccountContext::new(ContextParams {
                account: AccountId(1),
                uuid: AccountUuid::new_random(),
                start: StartMode::Fresh,
                backend_db: account.join("backend.db"),
                db_key: DbKey::new([7; 32]),
                blob_dir: account.join("blobs"),
                net_rt: rt.handle().clone(),
                cancel: CancellationToken::new(),
                clock: Arc::new(SystemClock),
                config: Default::default(),
                ui: Default::default(),
                local_prefs: Default::default(),
            });
            let handle = network.spawn(ctx).expect("the account starts");
            rt.block_on(async {
                for _ in 0..LOGINS {
                    ends.commands
                        .send(Command::StartLogin {
                            flow: "phone".to_owned(),
                            relink: None,
                            taken: Vec::new(),
                        })
                        .await
                        .expect("queued");
                    let envelope =
                        tokio::time::timeout(Duration::from_secs(60), ends.events.recv())
                            .await
                            .expect("no login step in time")
                            .expect("events ended");
                    // TG-2: a start that reached TDLib's phone-number step shows the `phone`
                    // input step (no number is ever typed here).
                    let phone = matches!(
                        &envelope.events[..],
                        [BackendEvent::Login(step)]
                            if step.step_id == "phone"
                                && matches!(step.kind, LoginStepKind::Input { .. })
                    );
                    assert!(phone, "TDLib's start did not reach the phone step");
                    // The next `StartLogin` must start a helper once more: the login is
                    // cancelled first (that helper is stopped by the next start).
                    ends.commands
                        .send(Command::CancelLogin)
                        .await
                        .expect("queued");
                }
                // What TDLib still does after the last stop reaches the traps too.
                tokio::time::sleep(Duration::from_secs(2)).await;
                ends.commands.send(Command::Shutdown).await.expect("queued");
            });
            let exit = rt.block_on(handle.exited).expect("the actor ends");
            let hits = traps.hits();
            // The namespace's own counters see every port (G3): no UDP datagram and no ICMP
            // message left, and every TCP connection started reached a listener (the gate: the
            // application's dialer refuses and dials nothing, so a connection to a port nobody
            // holds is the one thing that rises the first counter alone).
            let opens = traps.opens();
            println!(
                "REAL app: logins={LOGINS} tcp_traps={} dns_traps={} exit={exit:?} tcp_opens={} tcp_passive={} datagrams={}",
                hits.0,
                hits.1,
                opens.tcp_active_opens,
                opens.tcp_passive_opens,
                opens.datagrams()
            );
            assert_eq!(exit, kourier_backend_api::Exit::Shutdown);
            assert_eq!(hits, (0, 0), "a connection went past the gate");
            assert_eq!(opens.datagrams(), 0, "a datagram left the namespace");
            assert_eq!(
                opens.tcp_active_opens, opens.tcp_passive_opens,
                "a TCP connection went to a port nobody holds"
            );
            let files_root = paths.media_dir().join("telegram");
            assert!(
                files_root.is_dir(),
                "the files root of the app was not used"
            );
        }
    }
}

/// TR R0a fix round, part A: what stops a TEST (or a probe) from dialling or from taking a run
/// tool's confirmation (G-1.2, G-1.3, G-4, G-5). No test here names the dialing mode or an address
/// of Telegram: the probes of `kourier_telegram::offline` derive the address from the gate's own
/// list, and the decision is read from the INFO line of fixed words.
#[cfg(all(test, feature = "telegram"))]
mod dial_fence_tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::{Build, real_telegram, unix_now_ms};

    /// G-1.2 (review F1): the tripwire of the TCP dialer covers THIS test binary (kourier-app's
    /// unit tests link the crate as a dependency, so `cfg(test)` of the crate is never set here;
    /// its `test-support` feature is). Outside the offline child a listed address on a listed port
    /// is NOT permitted; inside it (the answer is then `true`) the same address is. Mutant: the
    /// tripwire back to `#[cfg(test)]` in `kourier-telegram` -> the permission is granted here.
    #[test]
    fn a_dial_to_a_listed_address_is_not_permitted_outside_the_offline_child() {
        use kourier_telegram::offline::{a_dial_is_permitted, a_listed_address, in_offline_child};

        let to = a_listed_address();
        assert_eq!(
            a_dial_is_permitted(to),
            in_offline_child(),
            "the app's test binary may dial a listed address outside the offline child"
        );
    }

    /// A scratch keeper root with a run profile `p` and a GOOD confirmation file in its `tg-run`
    /// (fresh at the moment of the call), and the environment a confirmed start would carry.
    struct Order {
        _tmp: tempfile::TempDir,
        data: PathBuf,
        profile: PathBuf,
        file: PathBuf,
        nonce: String,
    }

    fn order() -> Order {
        use std::os::unix::fs::PermissionsExt as _;

        use crate::telegram::{DIAL_DIR, DIAL_FILE, RUN_DIR, sha256_hex};

        let tmp = tempfile::tempdir().expect("tmp");
        let data = std::fs::canonicalize(tmp.path())
            .expect("canonical")
            .join("data");
        let root = data.join(RUN_DIR);
        let profile = root.join("p");
        let dir = profile.join(DIAL_DIR);
        std::fs::create_dir_all(&dir).expect("dirs");
        for private in [&root, &dir] {
            std::fs::set_permissions(private, std::fs::Permissions::from_mode(0o700))
                .expect("chmod");
        }
        let nonce = "ab".repeat(32);
        let file = dir.join(DIAL_FILE);
        let text = format!(
            r#"{{"version":1,"issued_ms":{},"env":"test","nonce_sha256":"{}"}}"#,
            unix_now_ms(),
            sha256_hex(nonce.as_bytes())
        );
        std::fs::write(&file, text).expect("write");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        Order {
            _tmp: tmp,
            data,
            profile,
            file,
            nonce,
        }
    }

    impl Order {
        /// The environment of the run tool's app command: the scratch keeper root, the profile, the
        /// nonce, the test servers and the sentinel tail.
        fn env(&self) -> impl Fn(&str) -> Option<OsString> + Clone + 'static {
            use crate::paths::HOME_ENV;
            use crate::process::EXPECT_NUMBER_TAIL_ENV;
            use crate::telegram::{DIAL_ENV, TELEGRAM_ENV};

            let vars: Vec<(&'static str, OsString)> = vec![
                ("XDG_DATA_HOME", self.data.clone().into_os_string()),
                (HOME_ENV, self.profile.clone().into_os_string()),
                (DIAL_ENV, self.nonce.clone().into()),
                (TELEGRAM_ENV, "test".into()),
                (EXPECT_NUMBER_TAIL_ENV, "0".repeat(15).into()),
            ];
            move |k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
        }
    }

    /// The words of the INFO lines `telegram dialer` the closure made, as `mode=.. reason=..`.
    fn told(events: &[(tracing::Level, String)]) -> Vec<String> {
        events
            .iter()
            .filter(|(_, fields)| fields.contains("telegram dialer"))
            .map(|(_, fields)| fields.clone())
            .collect()
    }

    const CASE_ENV: &str = "KOURIER_APP_REAL_TELEGRAM_CHILD";
    const CHILD: &str = "app::dial_fence_tests::the_child_of_real_telegram";

    /// G-1.3 (review F1): `real_telegram`, which `Seams::real()` calls, judges the confirmation over
    /// the PROCESS environment, and in a unit test of this crate that environment HIDES the run
    /// tool's nonce, whatever the test process inherited. The child (this test binary again, with a
    /// process environment that holds a run profile, a GOOD fresh confirmation file and its nonce)
    /// asks for the decision: it says `no-confirmation` and the file is still there. Mutant: the
    /// `cfg(test)` filter removed -> the child takes the file and the decision says `confirmed`.
    #[test]
    fn real_telegram_in_a_test_never_sees_the_nonce_of_the_process() {
        use crate::paths::HOME_ENV;
        use crate::process::EXPECT_NUMBER_TAIL_ENV;
        use crate::telegram::{DIAL_ENV, TELEGRAM_ENV};

        let order = order();
        let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", CHILD, "--nocapture"])
            .env(CASE_ENV, "1")
            .env("XDG_DATA_HOME", &order.data)
            .env(HOME_ENV, &order.profile)
            .env(DIAL_ENV, &order.nonce)
            .env(TELEGRAM_ENV, "test")
            .env(EXPECT_NUMBER_TAIL_ENV, "0".repeat(15))
            .output()
            .expect("the child starts");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(&format!("test {CHILD} ... ok")),
            "the child failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            order.file.exists(),
            "a test's real_telegram took the confirmation file"
        );
    }

    /// The child of [`real_telegram_in_a_test_never_sees_the_nonce_of_the_process`]: a no-op unless
    /// the parent set the case variable.
    #[test]
    fn the_child_of_real_telegram() {
        use crate::telegram::DIAL_ENV;

        if std::env::var_os(CASE_ENV).is_none() {
            return;
        }
        assert!(
            std::env::var_os(DIAL_ENV).is_some(),
            "the child's process holds the nonce, or the case proves nothing"
        );
        let (_parts, events) = crate::telegram::capture::events_of(|| real_telegram(Build::Debug));
        let told = told(&events);
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(
            told[0].contains(r#"reason="no-confirmation""#),
            "the decision of a test is not the plain one: {told:?}"
        );
        assert!(
            told[0].contains(r#"mode="refusing""#),
            "a test's real_telegram chose the mode that dials: {told:?}"
        );
    }

    /// G-4 (review F4): `--print-networks` judges the Telegram decision over an environment WITHOUT
    /// the nonce in every build ([`Seams::probe_over`](super::Seams)): with a GOOD fresh order in a
    /// scratch run profile and the nonce in the environment, the probe's dialer line says
    /// `refusing`/`no-confirmation` and the order file is still there. The control: the very same
    /// environment, judged as a normal start does, confirms and takes the file (so the order was
    /// good and complete). Mutant: the filter in `probe_over` removed -> the probe takes the file.
    #[test]
    fn a_probe_never_takes_a_confirmation() {
        use super::{Seams, networks_line_in};
        use crate::paths::{Paths, Profile};
        use crate::telegram::capture::events_of;

        let order = order();
        let env = order.env();
        let paths = Paths::from_env(Profile::default(), env.clone()).expect("paths");
        paths.create_dirs().expect("dirs");
        let (line, events) =
            events_of(|| networks_line_in(&paths, env.clone(), Seams::probe_over(env.clone())));
        line.expect("the line");
        let told = told(&events);
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(
            told[0].contains(r#"mode="refusing""#)
                && told[0].contains(r#"reason="no-confirmation""#),
            "{told:?}"
        );
        assert!(order.file.exists(), "the probe took the confirmation file");

        // The control: a start that judges over the same environment confirms, and takes the file.
        let decision = crate::telegram::dial_confirmation(true, &env, unix_now_ms());
        assert_eq!(
            (decision.mode.word(), decision.reason),
            ("telegram", crate::telegram::CONFIRMED),
            "the order of the case is not a good one"
        );
        assert!(!order.file.exists(), "the control left the file");
    }
}

/// TR R0a fix G-5, the start-level wiring: only a debug build WITHOUT a rehearsal network (no
/// `fake`, or none of WhatsApp, Telegram and Threema) holds this test, so ci's default build never
/// runs it; the judgment itself is a pure function with its own test in `dev.rs`. Run once with
/// `--no-default-features --features telegram -E 'test(/rehearsal/)'`.
#[cfg(all(
    test,
    debug_assertions,
    not(all(
        feature = "fake",
        any(feature = "whatsapp", feature = "telegram", feature = "threema")
    ))
))]
mod fakeless_rehearsal_tests {
    use std::ffi::OsString;

    use super::{AppError, ConfigError, Prefs, Seams, start_networks};
    use crate::dev::REHEARSAL_ENV;

    /// A debug build without a rehearsal network fails a start that sets the switch, with the one
    /// fixed sentence and before anything is made; an unset switch starts as before. Mutant: the
    /// refusal call removed from `start_networks` -> the start goes on with the real backend.
    #[test]
    fn a_rehearsal_switch_in_a_build_without_the_network_fails_the_start() {
        for value in ["telegram", "whatsapp", "real", "1"] {
            let env = move |k: &str| (k == REHEARSAL_ENV).then(|| OsString::from(value));
            let Err(error) = start_networks(
                &env,
                &Prefs::default(),
                Seams::real(),
                crate::prefs::ThreemaStart::Person,
            ) else {
                panic!("{value}: the start went on without a rehearsal network");
            };
            assert!(
                matches!(error, AppError::Config(ConfigError::Rehearsal(_))),
                "{value}: {error}"
            );
            let text = error.to_string();
            assert!(
                text.contains("this build has no rehearsal network"),
                "{text}"
            );
            assert!(
                !text.contains(value),
                "{value}: the value is repeated: {text}"
            );
        }
        let none = |_: &str| -> Option<OsString> { None };
        assert!(
            start_networks(
                &none,
                &Prefs::default(),
                Seams::real(),
                crate::prefs::ThreemaStart::Person
            )
            .is_ok()
        );
    }

    /// Re-check N-6, the normal start: it refuses the switch itself, before the Telegram seams are
    /// made (the order is pinned in `telegram_registration_tests`), also with a profile that does
    /// not exist (nothing is read before the refusal). Mutant: the call removed from
    /// `normal_setup_in` -> the start goes on to the keyring.
    #[test]
    fn a_normal_start_refuses_a_rehearsal_switch_before_it_reads_anything() {
        use crate::dev::DEV_KEYRING_ENV;
        use crate::paths::{HOME_ENV, Paths, Profile};

        let tmp = tempfile::tempdir().expect("tmp");
        for value in ["telegram", "whatsapp", "1"] {
            let home = tmp.path().join("no-such-home");
            let env = move |k: &str| match k {
                HOME_ENV => Some(OsString::from(&home)),
                // (the development keyring, so a start that was not refused never asks the Secret Service)
                DEV_KEYRING_ENV => Some(OsString::from("passphrase")),
                REHEARSAL_ENV => Some(OsString::from(value)),
                _ => None,
            };
            let paths = Paths::from_env(Profile::default(), &env).expect("paths");
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let Err(error) = rt.block_on(super::normal_setup_in(paths, env, false)) else {
                panic!("{value}: the normal start went on without a rehearsal network");
            };
            assert!(
                matches!(error, AppError::Config(ConfigError::Rehearsal(_))),
                "{value}: {error}"
            );
            assert!(!tmp.path().join("no-such-home").exists());
        }
    }
}

#[cfg(all(test, feature = "fake"))]
mod restart_prefs_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use kourier_model::NetworkKind;

    use super::{App, FixtureOptions, Launch, Prefs, StartOptions};
    use crate::desktop::{Presenter, Texts};

    /// A presenter whose `texts()` keeps the desktop task that asks for them busy (the task
    /// calls it inside its own poll when the UI attaches).
    struct Busy {
        entered: Arc<AtomicBool>,
        hold: Duration,
    }

    impl Presenter for Busy {
        fn texts(&self) -> Texts {
            self.entered.store(true, Ordering::SeqCst);
            std::thread::sleep(self.hold);
            Texts::default()
        }
    }

    fn fixture() -> StartOptions {
        StartOptions {
            launch: Launch::Fixture(FixtureOptions::default()),
            ..StartOptions::default()
        }
    }

    /// K-A fix 2 (C5): "Turn On and Restart" never restarts into the old preferences, also
    /// when the desktop task is busy for longer than the shutdown waits for it
    /// ([`super::DESKTOP_SHUTDOWN_TIMEOUT`], 2 s) and is abandoned with the preferences it
    /// was sent unsaved. The app saves the newest preferences it holds itself once everything
    /// has stopped, for a restart.
    #[test]
    fn a_restart_saves_the_newest_preferences_when_the_desktop_task_never_got_to_them() {
        let mut app = App::start(fixture()).expect("start");
        let h = app.handle();
        let path = app.paths().config_file();
        assert!(!Prefs::load_or_default(&path).network_on(NetworkKind::WhatsApp));
        // The task is inside the presenter for longer than the shutdown waits for it ...
        let entered = Arc::new(AtomicBool::new(false));
        h.desktop().attach(Arc::new(Busy {
            entered: entered.clone(),
            hold: super::DESKTOP_SHUTDOWN_TIMEOUT + Duration::from_millis(800),
        }));
        let begun = Instant::now();
        while !entered.load(Ordering::SeqCst) {
            assert!(
                begun.elapsed() < Duration::from_secs(30),
                "the desktop task never took the presenter"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // ... and the preferences "Turn On and Restart" sets queue up behind it.
        h.desktop()
            .update_prefs(|p| p.set_network_on(NetworkKind::WhatsApp, true));
        h.desktop()
            .update_prefs(|p| p.pending_link = Some("WhatsApp".to_owned()));
        h.request_restart();
        app.shut_down();
        let saved = Prefs::load_or_default(&path);
        assert!(
            saved.network_on(NetworkKind::WhatsApp),
            "the restart would start with WhatsApp off"
        );
        assert_eq!(saved.pending_link.as_deref(), Some("WhatsApp"));
    }
}

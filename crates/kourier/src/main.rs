//! The `kourier` binary: parses the command line, starts the app (runtime, keys, core), runs
//! the Qt UI and shuts down in order (§8.2).

use std::path::PathBuf;
use std::process::ExitCode;

use kourier_app::desktop::{DesktopMode, DesktopOptions, SystemBus, handled_uris};
use kourier_app::{
    App, AppError, FixtureNetwork, FixtureOptions, FixtureSends, FixtureState, FixtureTransfer,
    FixtureUpload, FixtureWhatsApp, HistoryTransfer, HistoryTransferFailure, Launch, Profile,
    StartOptions,
};
use kourier_qt::{PAGES, UiContext, UiOptions};

/// A build without the test network (the cargo feature `fake`: what a release archive
/// holds) has no fixture mode, so its usage does not offer one.
#[cfg(not(feature = "fake"))]
const USAGE: &str = "usage: kourier [--version] [--profile=<name>] [--background] \
[--size=<width>x<height>] [sgnl://… | signalcaptcha://…]

  --version       print the name and the version, then exit
  --background    start hidden in the system tray (autostart); a running Kourier is activated";

#[cfg(feature = "fake")]
const USAGE: &str =
    "usage: kourier [--version] [--profile=<name>] [--background] [--fixture | --fixture-link] \
[--page=<welcome|link|qr|list|chat|history|settings|drawer|media|viewer|compose|files|album|\
albumviewer|revisions|compact|quotes|menu|react|reactions|edit|reply|delete|timer|placeholders|\
offline|loggedout|challenge|clock|originals|timercustom|clearchat|mediamenu|deleteattachment|\
folder|folderfiles|folderzip|expiry|readmore|about|calls|unavailable|transferwait|\
transferdownload|transferread|transferupdate|transferbanner|transferfailed|transferask|\
networks|warisk|waqr|waphone|wacode|thqr|themoji|tgwarn|tgqr|tgcode|tgpassword|wahistory|waban|\
walock|waoff|wafailed|walost|uploading|uploadfile|archive|person|group|groupadmin|chatmedia|safety|safetychanged|share|timerpending|save|location|\
locationmany|locationnever|accounts|accountsdrawer|rowmenu|rowmenuoff|rowmenumuted|mutemenu|muteuntil|chatmenu|chatmenuoff>] \
[--fixture-transfer=<walk|declined|wait|download|read|link-again|update-app|corrupt|too-large|\
download-failed|internal>] [--fixture-send=<slow|keys|members|retry|uploading>] \
[--fixture-network=<fake|whatsapp|threema|telegram>] [--screenshot=<png>] \
[--size=<width>x<height>] [sgnl://… | signalcaptcha://…]

  --version       print the name and the version, then exit
  --background    start hidden in the system tray (autostart); a running Kourier is activated
  --fixture       run the real core on a temporary profile with the fake network's demo data
  --fixture-link  the same, starting on the welcome page: link the demo account through its
                  rotating QR codes
  --fixture-transfer  with --fixture-link: after the link the phone transfers the message
                  history. walk: the whole of it; declined: the phone's user says no;
                  wait, download, read: it stays in that state; the others: it fails that way
  --fixture-send  with --fixture: every send takes two minutes (slow: no step known; keys:
                  setting up encryption with 12 members; members: sent to 8 of 20) or its
                  first attempt is rate limited for 30 seconds (retry); uploading: a message with
                  files sent into the demo chat of the lower list uploads for two minutes (every
                  file at 30 %, with its ring and X)
  --fixture-network  with --fixture or --fixture-link: the network the demo account belongs to.
                  fake: the test network (the default); whatsapp: the fake plays WhatsApp, turned
                  on, with the same chats (the link goes through the warning page); threema: the
                  fake plays Threema, turned on, with the same chats (its link shows the three
                  emoji after the scan); telegram: the fake plays Telegram, turned on, with the
                  same chats (its link walks the phone-number way: the code, then the password)";

/// Debug builds: the development control channel on stdin/stdout (`kourier_qt::dev`).
#[cfg(debug_assertions)]
const DEV_CONTROL: &str = "--dev-control";
#[cfg(not(debug_assertions))]
const DEV_CONTROL: &str = "";

#[cfg(debug_assertions)]
const DEV_USAGE: &str = "
  --dev-control   (debug builds) JSON-lines control channel on stdin/stdout for scripted runs";
#[cfg(not(debug_assertions))]
const DEV_USAGE: &str = "";

/// The command line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Args {
    ui: UiOptions,
    profile: Option<String>,
    fixture: bool,
    fixture_link: bool,
    /// `--fixture-transfer=<name>`: the history transfer the fixture link walks.
    fixture_transfer: Option<FixtureTransfer>,
    /// `--fixture-send=<name>`: how the fixture account's sends behave.
    fixture_send: Option<FixtureSends>,
    /// `--fixture-network=<name>`: the network the fixture account belongs to.
    fixture_network: Option<FixtureNetwork>,
    background: bool,
    /// `--print-networks` (hidden, not in the usage): print which networks a normal start of the
    /// profile would register and list as off, then exit (`kourier_app::networks_line`).
    print_networks: bool,
    uris: Vec<String>,
    /// Builds with the cargo feature `perf` (`cargo xtask perf`): the reference profile.
    #[cfg(feature = "perf")]
    perf: PerfArgs,
}

/// The flags of a measuring build (cargo feature `perf`; architecture §1.3):
/// `--perf-profile=<dir>` runs the reference profile in `<dir>` (seeding it when empty),
/// `--perf-size=<contacts>,<groups>,<messages per conversation>,<media percent>` sets its
/// size, `--perf-seed` exits once the profile is seeded, `--perf-scenario=<name>` runs a
/// scripted session in the UI and `--perf-live-ms=<n>` delivers one message `n` ms after the
/// backend is online.
#[cfg(feature = "perf")]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PerfArgs {
    profile: Option<PathBuf>,
    size: Option<kourier_app::perf::PerfSpec>,
    seed: bool,
    live_ms: Option<u64>,
}

#[cfg(feature = "perf")]
impl PerfArgs {
    /// Takes `arg` when it is a perf flag.
    fn take(&mut self, ui: &mut UiOptions, arg: &str) -> Result<bool, String> {
        if let Some(dir) = arg.strip_prefix("--perf-profile=") {
            if dir.is_empty() {
                return Err("--perf-profile needs a directory".into());
            }
            self.profile = Some(PathBuf::from(dir));
        } else if let Some(size) = arg.strip_prefix("--perf-size=") {
            let n: Vec<usize> = size.split(',').filter_map(|v| v.parse().ok()).collect();
            let [contacts, groups, messages, media] = n[..] else {
                return Err(format!("bad perf size {size:?}"));
            };
            let media_percent = u8::try_from(media).ok().filter(|m| *m <= 100);
            let (Some(media_percent), true) = (media_percent, contacts >= 6 && messages >= 20)
            else {
                return Err(format!("bad perf size {size:?}"));
            };
            self.size = Some(kourier_app::perf::PerfSpec {
                contacts,
                groups,
                messages_per_conversation: messages,
                media_percent,
            });
        } else if arg == "--perf-seed" {
            self.seed = true;
        } else if let Some(name) = arg.strip_prefix("--perf-scenario=") {
            ui.perf_scenario = Some(name.to_owned());
        } else if let Some(ms) = arg.strip_prefix("--perf-live-ms=") {
            self.live_ms = Some(ms.parse().map_err(|_| format!("bad delay {ms:?}"))?);
        } else {
            return Ok(false);
        }
        Ok(true)
    }

    fn launch(&self) -> Option<Launch> {
        let home = self.profile.clone()?;
        Some(Launch::Perf(kourier_app::perf::PerfOptions {
            home,
            spec: self.size.unwrap_or_default(),
            live_after: self.live_ms.map(std::time::Duration::from_millis),
        }))
    }
}

/// Parses our flags; Qt's own arguments (`-platform …`, `--qmljsdebugger=…`) and `sgnl://`
/// URLs pass through untouched.
fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut out = Args::default();
    let mut rest = Vec::new();
    for arg in args {
        if arg == "--background" {
            out.background = true;
        } else if arg == "--print-networks" {
            out.print_networks = true;
        } else if arg == "--fixture" {
            out.fixture = true;
        } else if arg == "--fixture-link" {
            out.fixture_link = true;
        } else if let Some(name) = arg.strip_prefix("--fixture-transfer=") {
            out.fixture_transfer = Some(
                link_transfer(name).ok_or_else(|| format!("unknown fixture transfer {name:?}"))?,
            );
        } else if let Some(name) = arg.strip_prefix("--fixture-send=") {
            out.fixture_send =
                Some(fixture_send(name).ok_or_else(|| format!("unknown fixture send {name:?}"))?);
        } else if let Some(name) = arg.strip_prefix("--fixture-network=") {
            out.fixture_network = Some(match name {
                "fake" => FixtureNetwork::Fake,
                "whatsapp" => FixtureNetwork::WhatsApp,
                "threema" => FixtureNetwork::Threema,
                "telegram" => FixtureNetwork::Telegram,
                _ => return Err(format!("unknown fixture network {name:?}")),
            });
        } else if let Some(name) = arg.strip_prefix("--profile=") {
            out.profile = Some(name.to_owned());
        } else if let Some(page) = arg.strip_prefix("--page=") {
            if !PAGES.contains(&page) {
                return Err(format!("unknown page {page:?}"));
            }
            out.ui.page = Some(page.to_owned());
        } else if let Some(path) = arg.strip_prefix("--screenshot=") {
            if path.is_empty() {
                return Err("--screenshot needs a file name".into());
            }
            out.ui.screenshot = Some(PathBuf::from(path));
        } else if let Some(size) = arg.strip_prefix("--size=") {
            let parsed = size
                .split_once('x')
                .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
                .filter(|(w, h)| (200..=8192).contains(w) && (200..=8192).contains(h));
            out.ui.size = Some(parsed.ok_or_else(|| format!("bad size {size:?}"))?);
        } else if cfg!(debug_assertions) && arg == DEV_CONTROL {
            #[cfg(debug_assertions)]
            {
                out.ui.dev_control = true;
            }
        } else if arg == "--help" || arg == "-h" {
            return Err(String::new());
        } else {
            #[cfg(feature = "perf")]
            if out.perf.take(&mut out.ui, &arg)? {
                continue;
            }
            rest.push(arg);
        }
    }
    out.uris = handled_uris(rest);
    if out.fixture && out.fixture_link {
        return Err("--fixture and --fixture-link exclude each other".into());
    }
    if out.print_networks && (out.fixture || out.fixture_link || out.background) {
        return Err("--print-networks stands alone (it takes only --profile)".into());
    }
    if out.fixture_send.is_some() && !out.fixture {
        return Err("--fixture-send needs --fixture".into());
    }
    if out.fixture_transfer.is_some() && !out.fixture_link {
        return Err("--fixture-transfer needs --fixture-link".into());
    }
    if out.fixture_network.is_some() && !(out.fixture || out.fixture_link) {
        return Err("--fixture-network needs --fixture or --fixture-link".into());
    }
    // A page that stands at a link page ticks the network's risk warning for the person and starts the
    // link at once (`MainWindow.qml` `start()`): on a real profile that would open a connection to
    // WhatsApp or Threema without the warning, so these pages exist on the throwaway fixture only.
    let page = out.ui.page.as_deref();
    if page_holds_link(page) && !(out.fixture || out.fixture_link) {
        return Err(format!(
            "--page={} starts a link on its own and needs --fixture or --fixture-link",
            page.unwrap_or_default()
        ));
    }
    Ok(out)
}

/// The download the transfer pages show: 12.3 MB of 45.6 MB.
const TRANSFER_DOWNLOAD: HistoryTransfer = HistoryTransfer::Downloading {
    done: 12_300_000,
    total: Some(45_600_000),
};

/// How `--fixture-send=<name>` makes the fixture account's sends behave.
fn fixture_send(name: &str) -> Option<FixtureSends> {
    use kourier_app::SendPhase;
    Some(match name {
        "slow" => FixtureSends::Slow(None),
        "keys" => FixtureSends::Slow(Some(SendPhase::SettingUpKeys { done: 3, of: 12 })),
        "members" => FixtureSends::Slow(Some(SendPhase::Sending { done: 8, of: 20 })),
        "retry" => FixtureSends::RateLimited(std::time::Duration::from_secs(30)),
        "uploading" => FixtureSends::Uploading(FixtureUpload::Idle),
        _ => return None,
    })
}

/// The pages of upload progress: what each sends into the chat of the lower list and holds in
/// its upload (an album of seven pictures with a file queued behind it; a lone file).
fn page_uploads(page: Option<&str>) -> Option<FixtureSends> {
    Some(FixtureSends::Uploading(match page? {
        "uploading" => FixtureUpload::Album,
        "uploadfile" => FixtureUpload::File,
        _ => return None,
    }))
}

/// The pages whose WINDOW presses Send (`MainWindow.qml` `pageSetup`: `App.composer.send(conv,
/// ...)`): the message is stamped by the core's clock when the window sends it, some seconds
/// after the start, so the page asks for a clock that starts early in the minute of "now"
/// (`FixtureOptions::window_sends`). A page that sends from QML must be named here (a test
/// reads `MainWindow.qml` and fails for one that is not).
const WINDOW_SENDS: &[&str] = &["media"];

fn page_window_sends(page: Option<&str>) -> bool {
    page.is_some_and(|p| WINDOW_SENDS.contains(&p))
}

/// The history transfer `--fixture-transfer=<name>` asks a fixture link to walk.
fn link_transfer(name: &str) -> Option<FixtureTransfer> {
    use HistoryTransferFailure::{Corrupt, Download, Internal, LinkAgain, TooLarge, UpdateApp};
    let failed = |kind| FixtureTransfer::Walk(HistoryTransfer::Failed(kind));
    Some(match name {
        "walk" => FixtureTransfer::Walk(HistoryTransfer::Done),
        "declined" => FixtureTransfer::Walk(HistoryTransfer::Skipped),
        "wait" => FixtureTransfer::Hold(HistoryTransfer::Waiting),
        "download" => FixtureTransfer::Hold(TRANSFER_DOWNLOAD),
        "read" => FixtureTransfer::Hold(HistoryTransfer::Reading),
        "link-again" => failed(LinkAgain),
        "update-app" => failed(UpdateApp),
        "corrupt" => failed(Corrupt),
        "too-large" => failed(TooLarge),
        "download-failed" => failed(Download),
        "internal" => failed(Internal),
        _ => return None,
    })
}

/// The history transfer a page shows: a link that reaches one of three states (the page
/// links the fixture account itself, and the transfer stays in that state, in the banner
/// once the link page closed), or the main
/// window after a restart in the middle of a download or after a failure.
fn page_transfer(page: Option<&str>) -> Option<FixtureTransfer> {
    Some(match page? {
        "transferwait" => FixtureTransfer::Hold(HistoryTransfer::Waiting),
        "transferdownload" => FixtureTransfer::Hold(TRANSFER_DOWNLOAD),
        "transferread" => FixtureTransfer::Hold(HistoryTransfer::Reading),
        "transferbanner" => FixtureTransfer::Left(TRANSFER_DOWNLOAD),
        "transferfailed" => {
            FixtureTransfer::Left(HistoryTransfer::Failed(HistoryTransferFailure::Download))
        }
        _ => return None,
    })
}

/// The pages of WhatsApp's own states (K-A): what each puts into the fixture account. They are
/// the fixture of a WhatsApp account (the fake plays one).
fn page_whatsapp(page: Option<&str>) -> Option<FixtureWhatsApp> {
    Some(match page? {
        "waban" => FixtureWhatsApp::Banned,
        "walock" => FixtureWhatsApp::Locked,
        "wahistory" => FixtureWhatsApp::Filling,
        "wafailed" => FixtureWhatsApp::Refused,
        "walost" => FixtureWhatsApp::Lost,
        _ => return None,
    })
}

/// The pages of blended chats: how many accounts the fixture profile holds for each (three: the
/// fixture account, a second account of the test network and a registered fake that plays
/// WhatsApp, `FixtureOptions::accounts`). Every other page has the one account.
fn page_accounts(page: Option<&str>) -> u8 {
    match page {
        Some("accounts" | "accountsdrawer") => 3,
        _ => 0,
    }
}

/// The pages that stand at a link page of WhatsApp (its QR code, its phone-number question and
/// its pair code), of Threema (its QR code and its emoji) or of Telegram (its QR code, its code
/// and its password). The link waits there, and the page walks to its step by itself. (`tgwarn`
/// is not one: like `warisk` it opens the warning and stops, the link never starts.)
fn page_holds_link(page: Option<&str>) -> bool {
    matches!(
        page,
        Some("waqr" | "waphone" | "wacode" | "thqr" | "themoji" | "tgqr" | "tgcode" | "tgpassword")
    )
}

/// The pages of Threema's link: the fake that plays Threema links the fixture account.
fn page_threema(page: Option<&str>) -> bool {
    matches!(page, Some("thqr" | "themoji"))
}

/// The pages of Telegram's link: the fake that plays Telegram links the fixture account.
fn page_telegram(page: Option<&str>) -> bool {
    matches!(page, Some("tgqr" | "tgcode" | "tgpassword"))
}

/// The network the fixture account belongs to: Threema for its link pages, Telegram for its link
/// pages, WhatsApp for the other link pages and the pages of WhatsApp's own states, else the one
/// asked for, else the test network. `waoff` turns WhatsApp off; `warisk` and `tgwarn` are the
/// warning of a network that is off, on the plain fixture.
fn fixture_network(asked: Option<FixtureNetwork>, page: Option<&str>) -> FixtureNetwork {
    if page == Some("waoff") {
        return FixtureNetwork::WhatsAppOff;
    }
    if page_threema(page) {
        return FixtureNetwork::Threema;
    }
    if page_telegram(page) {
        return FixtureNetwork::Telegram;
    }
    if page_whatsapp(page).is_some() || page_holds_link(page) {
        return FixtureNetwork::WhatsApp;
    }
    asked.unwrap_or_default()
}

/// The state the fixture account ends in for a page: the account-state pages show its banner.
fn fixture_state(page: Option<&str>) -> FixtureState {
    match page {
        // (A paused account is not connected.)
        Some("offline" | "waoff") => FixtureState::Offline,
        Some("loggedout") => FixtureState::LoggedOut,
        Some("challenge") => FixtureState::Challenge,
        Some("clock") => FixtureState::ClockSkew,
        _ => FixtureState::Online,
    }
}

fn start_options(args: &Args) -> Result<StartOptions, String> {
    let profile = match &args.profile {
        Some(name) => Profile::named(name).map_err(|e| e.to_string())?,
        None => Profile::default(),
    };
    let page = args.ui.page.as_deref();
    let transfer = args.fixture_transfer.or(page_transfer(page));
    // A page of the link page's transfer states links the account itself: one code, scanned
    // at once.
    let page_links = matches!(page_transfer(page), Some(FixtureTransfer::Hold(_)));
    // The warning page of a network that is off starts from the welcome page, with no account;
    // so do the link pages of WhatsApp, Threema and Telegram.
    let held_links = matches!(page, Some("warisk" | "tgwarn")) || page_holds_link(page);
    // A start with the development control channel (debug builds only: the flag and its field
    // exist only there, so a release build never makes a scripted start).
    #[cfg(debug_assertions)]
    let scripted = args.ui.dev_control;
    #[cfg(not(debug_assertions))]
    let scripted = false;
    let launch = if args.fixture || args.fixture_link {
        Launch::Fixture(FixtureOptions {
            link: args.fixture_link || page_links || held_links,
            qr_codes: if page_links {
                1
            } else {
                FixtureOptions::default().qr_codes
            },
            transfer,
            // The banner of the update after a link.
            updating: page == Some("transferupdate"),
            // The page of the question needs an account that offers the transfer.
            history_on_link: page == Some("transferask"),
            state: fixture_state(args.ui.page.as_deref()),
            // The missed call of the chat with calls counts as unread there only.
            calls_unread: args.ui.page.as_deref() == Some("calls"),
            // And there its last call rings, from the start, for as long as a call may ring.
            calls_ring_for: (args.ui.page.as_deref() == Some("calls"))
                .then_some(FixtureOptions::RING),
            sends: args.fixture_send.or(page_uploads(page)),
            network: fixture_network(args.fixture_network, page),
            hold_link: page_holds_link(page),
            // The warning of Telegram, off: the page lists it off beside WhatsApp (page-scoped,
            // LC-KT1b-5: no other page has an off Telegram in its welcome list).
            telegram_off: page == Some("tgwarn"),
            // Threema's emoji page stands one step further on than its QR page.
            hold_emoji: page == Some("themoji"),
            whatsapp: page_whatsapp(page),
            accounts: page_accounts(page),
            // The person's safety number in state Changed (the level of `--page=safetychanged`).
            safety_changed: page == Some("safetychanged"),
            // The account administers the group of the group page (`--page=groupadmin`).
            group_admin: page == Some("groupadmin"),
            // The window sends this page's photo itself (`MainWindow.pageSetup`): the core's
            // clock starts early in the minute of "now", so the time under it never follows the
            // length of the start.
            window_sends: page_window_sends(page),
            ..FixtureOptions::default()
        })
    } else if scripted {
        // A scripted start: the user's profile like a normal one, but Threema is absent unless the
        // run tool made the start (`kourier_app::Launch::Scripted`).
        Launch::Scripted
    } else {
        Launch::Normal
    };
    #[cfg(feature = "perf")]
    let launch = args.perf.launch().unwrap_or(launch);
    let mode = DesktopMode::from_env(|k| std::env::var(k).ok());
    let desktop = DesktopOptions {
        mode,
        system_bus: SystemBus::from_env(mode, |k| std::env::var(k).ok()),
        background: args.background,
        uris: args.uris.clone(),
        activation_token: std::env::var("XDG_ACTIVATION_TOKEN")
            .ok()
            .filter(|t| !t.is_empty()),
    };
    Ok(StartOptions {
        profile,
        launch,
        desktop,
    })
}

/// The line `--version` prints: the name and the version of the workspace.
fn version_line() -> String {
    format!("Kourier {}", env!("CARGO_PKG_VERSION"))
}

/// Whether the command line asks for the version. Qt's own arguments pass through [`parse`], so
/// this looks at the raw arguments, before anything else is looked at.
fn wants_version(args: &[String]) -> bool {
    args.iter().any(|a| a == "--version")
}

fn main() -> ExitCode {
    // `--version` comes before everything else: no trace clock, no hook, no profile, no Qt.
    // `cargo xtask dist` runs it on the packed binary.
    if wants_version(&std::env::args().skip(1).collect::<Vec<_>>()) {
        println!("{}", version_line());
        return ExitCode::SUCCESS;
    }
    // Before anything starts a thread (`App::start` makes the runtime): Qt's FFmpeg backend
    // would probe every hardware video device at the first voice note (`QT_ENV_DEFAULTS`).
    // SAFETY: nothing has started a thread yet.
    unsafe { kourier_qt::prepare_environment() };
    // The trace clock starts here (`KOURIER_TRACE_STARTUP=1`, architecture §1.3).
    kourier_app::trace::init();
    // Release builds: no core dumps and no reading of this process's memory (§9.4).
    kourier_app::process::deny_dumps();
    // A panic is logged as one redacted line without its message (§9.1); the front-end shows
    // that line in the desktop's crash reporter when one is running.
    kourier_app::process::install_panic_hook(kourier_qt::kde::panic_sink);
    let args = match parse(std::env::args().skip(1)) {
        Ok(a) => a,
        // `--help` (an empty error) prints the usage and succeeds.
        Err(e) if e.is_empty() => {
            println!("{USAGE}{DEV_USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("kourier: {e}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    // Hidden: what `cargo xtask dist` asks of the packed binary. Starts nothing (no window, no
    // database, no keyring, no network).
    if args.print_networks {
        return print_networks(&args);
    }
    let opts = match start_options(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("kourier: {e}");
            return ExitCode::from(2);
        }
    };
    let app = match App::start(opts) {
        Ok(app) => app,
        // §8.2 step 1: the running instance shows its window (or opens the links).
        Err(AppError::AlreadyRunning) => {
            eprintln!("kourier: already running; the request went to it");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("kourier: {e}");
            return ExitCode::from(1);
        }
    };
    // A measuring build that was only asked to create the reference profile is done.
    #[cfg(feature = "perf")]
    if args.perf.seed {
        let seeded = app.handle().seeded();
        app.shutdown();
        match seeded {
            Some(report) => report.print(),
            None => eprintln!("kourier: the profile has data already; nothing was seeded"),
        }
        return ExitCode::SUCCESS;
    }
    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext { app: app.handle() });
    let code = kourier_qt::run(args.ui);
    // Asked before the app shuts down, which consumes it; acted on after, so the new process
    // finds neither the profile's lock nor the single-instance name taken.
    let restart = app.handle().restart_requested();
    app.shutdown();
    if code == 0 && restart {
        restart_now();
    }
    match code {
        0 => ExitCode::SUCCESS,
        code => ExitCode::from(u8::try_from(code).unwrap_or(1)),
    }
}

/// `--print-networks`: the line of [`kourier_app::networks_line`] for the profile of the command
/// line, on stdout.
fn print_networks(args: &Args) -> ExitCode {
    let profile = match &args.profile {
        Some(name) => Profile::named(name).map_err(|e| e.to_string()),
        None => Ok(Profile::default()),
    };
    match profile.and_then(|p| kourier_app::networks_line(p).map_err(|e| e.to_string())) {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("kourier: {e}");
            ExitCode::from(1)
        }
    }
}

/// "Restart Now": replaces this process by a fresh Kourier of the same profile. Returns only
/// when that is not possible (a fixture or picture run, or a program that cannot be started):
/// the run then ends as an ordinary quit does, and says why when it could have restarted.
fn restart_now() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let Ok(exe) = std::env::current_exe() else {
        eprintln!("kourier: the restart was asked for, but this program's path is unknown");
        return;
    };
    // (A run that is not a session never restarts: its request is only recorded.)
    if let Some(restart) = kourier_app::restart::Restart::of_run(&exe, &args) {
        let unset = kourier_qt::env_set_by_kourier();
        let error = restart.exec(&unset);
        eprintln!("kourier: cannot start again: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Args, String> {
        parse(args.iter().map(|s| (*s).to_owned()))
    }

    /// `main` sets the variables Qt reads (`kourier_qt::prepare_environment`) before it starts
    /// anything that has a thread: `trace::init`, the panic hook, `App::start` (the runtime),
    /// Qt. `std::env::set_var` is a data race with any other thread that reads the environment.
    #[test]
    fn main_prepares_the_environment_before_anything_starts_a_thread() {
        let source = include_str!("main.rs");
        let body = source
            .split_once("fn main() -> ExitCode {")
            .expect("main")
            .1;
        let body = body.split_once("\n}\n").expect("the end of main").0;
        let at = |needle: &str| {
            body.find(needle)
                .unwrap_or_else(|| panic!("main does not contain {needle}"))
        };
        let prepare = at("kourier_qt::prepare_environment()");
        for later in [
            "kourier_app::trace::init()",
            "kourier_app::process::install_panic_hook(",
            "App::start(",
            "kourier_qt::run(",
        ] {
            assert!(
                prepare < at(later),
                "prepare_environment comes after {later}"
            );
        }
        // Only the `--version` early return, which starts nothing, comes first.
        assert!(prepare > at("wants_version("), "after the --version check");
    }

    /// A build without the test network does not offer what it refuses.
    #[cfg(not(feature = "fake"))]
    #[test]
    fn the_usage_of_a_build_without_the_test_network_offers_no_fixture() {
        assert!(!USAGE.contains("fixture") && !USAGE.contains("--page"));
        assert!(USAGE.contains("--version") && USAGE.contains("--background"));
    }

    #[cfg(feature = "fake")]
    #[test]
    fn the_usage_names_every_page() {
        // (`--help` listed 29 pages while the UI had more: the list is checked from here on.)
        for page in PAGES {
            assert!(
                USAGE.contains(&format!("|{page}|"))
                    || USAGE.contains(&format!("<{page}|"))
                    || USAGE.contains(&format!("|{page}>")),
                "--page={page} is not in the usage text"
            );
        }
    }

    /// The two pages of blended chats start a profile of three accounts and no other page adds one
    /// (the option is the pages' alone, so no other picture moves); the page's own network stays
    /// the plain fixture's, whatever else the page sets.
    #[test]
    fn only_the_blended_pages_seed_more_accounts() {
        for page in PAGES {
            let a = p(&["--fixture", &format!("--page={page}")]).expect("page");
            let Launch::Fixture(f) = start_options(&a).expect("options").launch else {
                panic!("{page}: not a fixture run");
            };
            let blended = matches!(page, "accounts" | "accountsdrawer");
            assert_eq!(f.accounts, if blended { 3 } else { 0 }, "{page}");
            if blended {
                assert!(!f.link && f.network == FixtureNetwork::Fake, "{page}");
            }
        }
    }

    /// The pages whose Send in the window stops at the question about the files' location
    /// (`MainWindow.qml`: `chatPage.composer.send()` of `location` and `locationmany`; nothing
    /// is sent, so no message is stamped). `locationnever` has no send call at all.
    const SENDS_THAT_ASK: &[&str] = &["location", "locationmany"];

    /// The invokables of the composer (`ComposerController`, `kourier-qt/src/bridge.rs`) that put
    /// a message on the wire, by their QML names: `send` (a text, the strip's files, an edit)
    /// and `answerLocation` (its answer `send` sends the files the question held back). A name
    /// that starts with `send` counts as one too (a future `sendFiles`, `sendContact`).
    const COMPOSER_SENDS: &[&str] = &["send", "answerLocation"];

    /// The invokables of the composer that send nothing (snake case, as the bridge writes them).
    /// Every invokable of the bridge is in one of the two lists, so a new one must be sorted
    /// here (`every_composer_invokable_is_sorted_as_a_send_or_not`).
    const COMPOSER_NO_SENDS: &[&str] = &[
        "answer_folders",
        "refresh_location_pref",
        "add_files",
        "add_file",
        "remove_attachment",
        "clear_attachments",
        "paste",
        "set_has_text",
        "set_caption",
        "set_original",
        "save_draft",
        "draft",
        "set_typing",
        "clear_notice",
        "reply_to",
        "cancel_reply",
        "start_edit",
        "edit_last",
        "cancel_edit",
    ];

    /// The calls of the application object that make or re-stamp an own message without the
    /// composer.
    const APP_SENDS: &[&str] = &["shareContact", "retryMessage"];

    fn camel(snake: &str) -> String {
        let mut out = String::new();
        for (i, part) in snake.split('_').enumerate() {
            if i == 0 {
                out.push_str(part);
            } else {
                out.extend(part.chars().take(1).flat_map(char::to_uppercase));
                out.extend(part.chars().skip(1));
            }
        }
        out
    }

    /// The names of the sending calls on a line of QML: every `<anything>composer.<name>(` whose
    /// name is one of `COMPOSER_SENDS` or starts with `send` (`App.composer.send(`,
    /// `chatPage.composer.send()`, a bare `composer.send()`, `App.composer.sendFiles(`), and the
    /// `APP_SENDS`. A comment line and the comment at the end of a line are not code.
    fn window_send_calls(line: &str) -> Vec<String> {
        let code = line.split(" //").next().unwrap_or(line);
        if code.trim_start().starts_with("//") {
            return Vec::new();
        }
        let ident = |rest: &str| -> String {
            rest.chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect()
        };
        let mut calls = Vec::new();
        for (at, _) in code.match_indices("composer.") {
            let rest = &code[at + "composer.".len()..];
            let name = ident(rest);
            let called = rest[name.len()..].starts_with('(');
            if called && (COMPOSER_SENDS.contains(&name.as_str()) || name.starts_with("send")) {
                calls.push(name);
            }
        }
        for name in APP_SENDS {
            for (at, _) in code.match_indices(&format!("{name}(")) {
                // Whole word only (`reshareContact(` is another call).
                let before = code[..at].chars().next_back();
                if !before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    calls.push((*name).to_owned());
                }
            }
        }
        calls
    }

    /// The scan sees every form of a window send, and nothing that only reads the composer.
    #[test]
    fn the_scan_for_window_sends_sees_every_form_of_a_send() {
        let calls = |l: &str| window_send_calls(l);
        assert_eq!(
            calls("if (App.composer.send(conv, \"Sent from Kourier\")) {"),
            ["send"]
        );
        assert_eq!(calls("    chatPage.composer.send();"), ["send"]);
        assert_eq!(calls("root.chatPage.composer.send()"), ["send"]);
        assert_eq!(calls("composer.send();"), ["send"]);
        assert_eq!(calls("App.composer.sendFiles(urls);"), ["sendFiles"]);
        assert_eq!(calls("App.composer.sendContact(conv)"), ["sendContact"]);
        assert_eq!(
            calls("App.composer.answerLocation(\"send\");"),
            ["answerLocation"]
        );
        assert_eq!(calls("App.shareContact(conv);"), ["shareContact"]);
        assert_eq!(calls("App.retryMessage(m)"), ["retryMessage"]);
        assert_eq!(
            calls("a.composer.send(); b.composer.sendFiles(x); // composer.send()"),
            ["send", "sendFiles"]
        );
        // Reads of the composer, other calls, comments and other words are no send.
        for quiet in [
            "done = App.composer.attachmentCount > 0 && App.composer.planned;",
            "done = App.composer.sendableCount > 1;",
            "chatPage.composer.canSend",
            "App.composer.replyTo(conv, App.findMessage(\"photo\"));",
            "App.composer.answerFolders(\"add\");",
            "// App.composer.send(conv, \"x\")",
            "    // chatPage.composer.send();",
            "x = 1; // App.composer.send(conv, \"x\")",
            "App.reshareContact(conv)",
        ] {
            assert!(calls(quiet).is_empty(), "{quiet}");
        }
    }

    /// The list of sending calls is whole: every invokable of the composer is sorted as a send
    /// (in `COMPOSER_SENDS`, or named `send...`) or as one that sends nothing, so a new one that
    /// sends cannot slip past the scan of `MainWindow.qml`.
    #[test]
    fn every_composer_invokable_is_sorted_as_a_send_or_not() {
        let bridge =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kourier-qt/src/bridge.rs");
        let text = std::fs::read_to_string(&bridge).expect("bridge.rs");
        let from = text
            .find("type ComposerController = ")
            .expect("the composer's bridge");
        // The invokables of that block: up to the next `type ` of another object.
        let block = &text[from..];
        let block = &block[..block[1..]
            .find("\n        type ")
            .map_or(block.len(), |i| i + 1)];
        let mut names = Vec::new();
        let mut lines = block.lines();
        while let Some(line) = lines.next() {
            if line.trim() != "#[qinvokable]" {
                continue;
            }
            let signature = lines.next().expect("a signature").trim();
            let name = signature
                .strip_prefix("fn ")
                .and_then(|r| r.split('(').next())
                .expect("fn name");
            names.push(name.to_owned());
        }
        assert!(names.len() >= 20, "the scan of the bridge found {names:?}");
        for name in &names {
            let qml = camel(name);
            let sends = COMPOSER_SENDS.contains(&qml.as_str()) || qml.starts_with("send");
            let quiet = COMPOSER_NO_SENDS.contains(&name.as_str());
            assert!(
                sends != quiet,
                "the composer's `{name}` must be in exactly one of COMPOSER_SENDS (or named \
                 `send...`) and COMPOSER_NO_SENDS (sends: {sends}, listed as quiet: {quiet})"
            );
        }
        for listed in COMPOSER_NO_SENDS {
            assert!(
                names.iter().any(|n| n == listed),
                "COMPOSER_NO_SENDS names `{listed}`, which the composer no longer has"
            );
        }
        for listed in COMPOSER_SENDS {
            assert!(
                names.iter().any(|n| camel(n) == *listed),
                "COMPOSER_SENDS names `{listed}`, which the composer no longer has"
            );
        }
    }

    /// S3 (2026-10-28), widened by the review (LOW 7): a page whose window sends a message
    /// (`MainWindow.qml` `pageSetup`) has the core stamp it with the clock of that moment, some
    /// seconds after the start, so the page asks for a clock that starts early in the minute of
    /// "now" (`FixtureOptions::window_sends`); no other page does (its core clock starts at
    /// "now", as before). A page that sends from the window and is not in `WINDOW_SENDS` would
    /// show a time that follows the length of its start, and fails here.
    #[test]
    fn only_the_pages_whose_window_sends_ask_for_an_early_clock() {
        for page in PAGES {
            let a = p(&["--fixture", &format!("--page={page}")]).expect("page");
            let Launch::Fixture(f) = start_options(&a).expect("options").launch else {
                panic!("{page}: not a fixture run");
            };
            assert_eq!(f.window_sends, page == "media", "{page}");
        }
        assert_eq!(WINDOW_SENDS, ["media"]);
        // EVERY send of the window (`window_send_calls`: the composer's `send` in any spelling,
        // `answerLocation`, the `send...` names, the application's contact and retry calls).
        // Each one stands in the `case` of a page that is listed in `WINDOW_SENDS`, or is the
        // `send` of a page whose Send stops at the location question (`SENDS_THAT_ASK`: it
        // raises the question and sends nothing).
        let qml = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../kourier-qt/qml/MainWindow.qml");
        let text = std::fs::read_to_string(&qml).expect("MainWindow.qml");
        let lines: Vec<&str> = text.lines().collect();
        let (mut listed, mut asking) = (0, 0);
        for (n, line) in lines.iter().enumerate() {
            for call in window_send_calls(line) {
                // The `case "<page>":` labels directly above the nearest block of statements.
                let at = lines[..n]
                    .iter()
                    .rposition(|l| l.trim_start().starts_with("case \""))
                    .unwrap_or_else(|| {
                        panic!("MainWindow.qml line {}: `{call}` outside a case", n + 1)
                    });
                let mut labels = Vec::new();
                for l in lines[..=at].iter().rev() {
                    let l = l.trim();
                    match l
                        .strip_prefix("case \"")
                        .and_then(|r| r.strip_suffix("\":"))
                    {
                        Some(name) => labels.push(name),
                        None if l.starts_with("//") => {}
                        None => break,
                    }
                }
                assert!(!labels.is_empty(), "line {}", n + 1);
                for page in labels {
                    if WINDOW_SENDS.contains(&page) {
                        listed += 1;
                    } else {
                        assert!(
                            SENDS_THAT_ASK.contains(&page) && call == "send",
                            "MainWindow.qml sends (`{call}`) from the page `{page}` (line {}): \
                             add it to WINDOW_SENDS (a page that sends is stamped by the core's \
                             clock), or, for a Send that stops at a question, to SENDS_THAT_ASK",
                            n + 1
                        );
                        asking += 1;
                    }
                }
            }
        }
        assert_eq!(
            (listed, asking),
            (1, 2),
            "the scan found the media page's send and the Send of the two location pages, and \
             no other"
        );
    }

    /// Review MEDIUM 3 (2026-10-28): the `media` page sends from the window, and its picture is
    /// taken when the message stands in the timeline, the list is empty AND the newest own
    /// message has left the outbox (`TimelineModel.newestOwnLeftTheOutbox`; the rule itself is
    /// tested in `kourier-viewmodel`): a render grabbed while it is queued or sending shows the
    /// clock icon and "Sending...". The page's `done` line must keep asking.
    #[test]
    fn the_media_page_waits_for_its_send_to_leave_the_outbox() {
        let qml = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../kourier-qt/qml/MainWindow.qml");
        let text = std::fs::read_to_string(&qml).expect("MainWindow.qml");
        let lines: Vec<&str> = text.lines().collect();
        let from = lines
            .iter()
            .position(|l| l.trim() == "case \"media\":")
            .expect("the media case");
        let to = lines[from + 1..]
            .iter()
            .position(|l| l.trim_start().starts_with("case \""))
            .map_or(lines.len(), |i| from + 1 + i);
        let dones: Vec<&&str> = lines[from..to]
            .iter()
            .filter(|l| l.trim_start().starts_with("done = ") && !l.contains("done = false"))
            .collect();
        assert_eq!(
            dones.len(),
            1,
            "the media case ends in one `done =`: {dones:?}"
        );
        for need in [
            "App.composer.attachmentCount === 0",
            "App.timeline.count > root.mediaSentAt",
            "App.timeline.newestOwnLeftTheOutbox()",
        ] {
            assert!(
                dones[0].contains(need),
                "the media page's `done` must hold `{need}`: {}",
                dones[0].trim()
            );
        }
    }

    /// `[workspace.package] version` of the root `Cargo.toml`, read from the file: the one
    /// place the version is typed (a copy of the formula under test would prove nothing).
    fn workspace_manifest_version() -> String {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
        let text = std::fs::read_to_string(manifest).expect("the workspace Cargo.toml");
        let mut in_table = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                in_table = line == "[workspace.package]";
            } else if in_table && let Some(rest) = line.strip_prefix("version") {
                let value = rest.trim_start().strip_prefix('=').map(str::trim);
                if let Some((version, _)) = value.and_then(|v| v.strip_prefix('"')?.split_once('"'))
                {
                    return version.to_owned();
                }
            }
        }
        panic!("no version in [workspace.package] of the root Cargo.toml");
    }

    #[test]
    fn the_version_line_names_the_workspace_version() {
        let version = workspace_manifest_version();
        assert!(
            version.starts_with(|c: char| c.is_ascii_digit()),
            "{version:?}"
        );
        assert_eq!(version_line(), format!("Kourier {version}"));
        // One line, starting with the name: `cargo xtask dist` compares it whole.
        let line = version_line();
        assert!(
            line.starts_with("Kourier ") && !line.contains('\n'),
            "{line:?}"
        );
    }

    #[test]
    fn only_the_version_flag_asks_for_the_version() {
        let args = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(wants_version(&args(&["--version"])));
        assert!(wants_version(&args(&["--fixture", "--version"])));
        assert!(!wants_version(&args(&[])));
        assert!(!wants_version(&args(&["--fixture", "--page=chat"])));
        assert!(!wants_version(&args(&[
            "--version=1",
            "version",
            "-version"
        ])));
    }

    #[test]
    fn flags() {
        let a = p(&[
            "--fixture",
            "--page=chat",
            "--screenshot=/tmp/x.png",
            "--size=1200x800",
            "--profile=work",
            "-platform",
            "offscreen",
            "sgnl://linkdevice",
            "--background",
        ])
        .expect("parse");
        assert!(a.background);
        assert_eq!(a.uris, vec!["sgnl://linkdevice".to_owned()]);
        assert!(a.fixture && !a.fixture_link);
        assert_eq!(a.ui.page.as_deref(), Some("chat"));
        assert_eq!(a.ui.screenshot, Some(PathBuf::from("/tmp/x.png")));
        assert_eq!(a.ui.size, Some((1200, 800)));
        assert_eq!(a.profile.as_deref(), Some("work"));
        let o = start_options(&a).expect("options");
        assert!(matches!(
            o.launch,
            Launch::Fixture(FixtureOptions { link: false, .. })
        ));
        let a = p(&["--fixture-link", "--page=qr"]).expect("link");
        assert!(matches!(
            start_options(&a).expect("options").launch,
            Launch::Fixture(FixtureOptions { link: true, .. })
        ));
        assert_eq!(
            start_options(&p(&[]).expect("empty"))
                .expect("options")
                .launch,
            Launch::Normal
        );
        // A scripted start (`--dev-control`) without a fixture is the user's profile, scripted.
        assert_eq!(
            start_options(&p(&["--dev-control"]).expect("dev control"))
                .expect("options")
                .launch,
            Launch::Scripted
        );
        assert!(matches!(
            start_options(&p(&["--dev-control", "--fixture"]).expect("fixture"))
                .expect("options")
                .launch,
            Launch::Fixture(_)
        ));
        // The account-state pages put the fixture account into their state.
        for (page, state) in [
            ("offline", FixtureState::Offline),
            ("loggedout", FixtureState::LoggedOut),
            ("challenge", FixtureState::Challenge),
            ("clock", FixtureState::ClockSkew),
            ("quotes", FixtureState::Online),
            ("list", FixtureState::Online),
        ] {
            let a = p(&["--fixture", &format!("--page={page}")]).expect("page");
            assert!(
                matches!(
                    start_options(&a).expect("options").launch,
                    Launch::Fixture(FixtureOptions { state: s, showcase: true, .. }) if s == state
                ),
                "{page}"
            );
        }
        // The transfer pages: three link the account themselves and hold the transfer, two
        // leave the linked account in a state, one only needs an account that offers it.
        for (page, link, transfer, offers) in [
            (
                "transferwait",
                true,
                Some(FixtureTransfer::Hold(HistoryTransfer::Waiting)),
                false,
            ),
            (
                "transferdownload",
                true,
                Some(FixtureTransfer::Hold(TRANSFER_DOWNLOAD)),
                false,
            ),
            (
                "transferread",
                true,
                Some(FixtureTransfer::Hold(HistoryTransfer::Reading)),
                false,
            ),
            (
                "transferbanner",
                false,
                Some(FixtureTransfer::Left(TRANSFER_DOWNLOAD)),
                false,
            ),
            (
                "transferfailed",
                false,
                Some(FixtureTransfer::Left(HistoryTransfer::Failed(
                    HistoryTransferFailure::Download,
                ))),
                false,
            ),
            ("transferask", false, None, true),
            ("transferupdate", false, None, false),
            ("settings", false, None, false),
        ] {
            let a = p(&["--fixture", &format!("--page={page}")]).expect("page");
            let Launch::Fixture(f) = start_options(&a).expect("options").launch else {
                panic!("{page}: not a fixture run");
            };
            assert_eq!(
                (f.link, f.transfer, f.history_on_link),
                (link, transfer, offers),
                "{page}"
            );
            assert_eq!(f.qr_codes, if link { 1 } else { 3 }, "{page}");
            assert_eq!(f.updating, page == "transferupdate", "{page}");
        }
        // The pages of WhatsApp (K-A): which network the fixture account belongs to, what the
        // page puts into it, and whether the link waits where it is shown. `warisk` is the
        // warning of a network that is off, on the plain fixture from the welcome page; `waoff`
        // is the same account with the network turned off, not connected.
        for (page, network, whatsapp, link, hold, state) in [
            (
                "networks",
                FixtureNetwork::Fake,
                None,
                false,
                false,
                FixtureState::Online,
            ),
            (
                "warisk",
                FixtureNetwork::Fake,
                None,
                true,
                false,
                FixtureState::Online,
            ),
            // Telegram's warning (KT-1b): like `warisk`, the plain fixture from the welcome page, no
            // account; the one page that lists Telegram as off (`telegram_off` below).
            (
                "tgwarn",
                FixtureNetwork::Fake,
                None,
                true,
                false,
                FixtureState::Online,
            ),
            (
                "waqr",
                FixtureNetwork::WhatsApp,
                None,
                true,
                true,
                FixtureState::Online,
            ),
            (
                "waphone",
                FixtureNetwork::WhatsApp,
                None,
                true,
                true,
                FixtureState::Online,
            ),
            (
                "wacode",
                FixtureNetwork::WhatsApp,
                None,
                true,
                true,
                FixtureState::Online,
            ),
            // Threema's link (TH4 KH1c): the fake plays Threema; the QR page and the emoji page
            // both wait where they are (`hold_emoji` below tells them apart).
            (
                "thqr",
                FixtureNetwork::Threema,
                None,
                true,
                true,
                FixtureState::Online,
            ),
            (
                "themoji",
                FixtureNetwork::Threema,
                None,
                true,
                true,
                FixtureState::Online,
            ),
            // Telegram's link (KT-1b): the fake plays Telegram; the QR code, the code step and the
            // password step all wait where they are (the page walks to its step).
            (
                "tgqr",
                FixtureNetwork::Telegram,
                None,
                true,
                true,
                FixtureState::Online,
            ),
            (
                "tgcode",
                FixtureNetwork::Telegram,
                None,
                true,
                true,
                FixtureState::Online,
            ),
            (
                "tgpassword",
                FixtureNetwork::Telegram,
                None,
                true,
                true,
                FixtureState::Online,
            ),
            (
                "waban",
                FixtureNetwork::WhatsApp,
                Some(FixtureWhatsApp::Banned),
                false,
                false,
                FixtureState::Online,
            ),
            (
                "walock",
                FixtureNetwork::WhatsApp,
                Some(FixtureWhatsApp::Locked),
                false,
                false,
                FixtureState::Online,
            ),
            (
                "wahistory",
                FixtureNetwork::WhatsApp,
                Some(FixtureWhatsApp::Filling),
                false,
                false,
                FixtureState::Online,
            ),
            (
                "wafailed",
                FixtureNetwork::WhatsApp,
                Some(FixtureWhatsApp::Refused),
                false,
                false,
                FixtureState::Online,
            ),
            (
                "walost",
                FixtureNetwork::WhatsApp,
                Some(FixtureWhatsApp::Lost),
                false,
                false,
                FixtureState::Online,
            ),
            (
                "waoff",
                FixtureNetwork::WhatsAppOff,
                None,
                false,
                false,
                FixtureState::Offline,
            ),
        ] {
            let a = p(&["--fixture", &format!("--page={page}")]).expect("page");
            let Launch::Fixture(f) = start_options(&a).expect("options").launch else {
                panic!("{page}: not a fixture run");
            };
            assert_eq!(
                (f.network, f.whatsapp, f.link, f.hold_link, f.state),
                (network, whatsapp, link, hold, state),
                "{page}"
            );
            // (No transfer, update or second link rides on these pages.)
            assert_eq!((f.transfer, f.updating, f.second_link), (None, false, None));
            // The emoji is the one step of the page `themoji`; no other page holds there.
            assert_eq!(f.hold_emoji, page == "themoji", "{page}");
            // Telegram is listed as off for the page `tgwarn` ONLY (LC-KT1b-5): in any other
            // page's welcome list or Settings card it would move every wide picture.
            assert_eq!(f.telegram_off, page == "tgwarn", "{page}");
        }
        // And on no page at all in the table of pages (the loop above covers the ones with a
        // row; this one every page there is).
        for page in PAGES {
            let a = p(&["--fixture", &format!("--page={page}")]).expect("page");
            let Launch::Fixture(f) = start_options(&a).expect("options").launch else {
                panic!("{page}: not a fixture run");
            };
            assert_eq!(f.telegram_off, page == "tgwarn", "{page}");
            assert_eq!(
                f.network == FixtureNetwork::Telegram,
                matches!(page, "tgqr" | "tgcode" | "tgpassword"),
                "{page}"
            );
        }
        // A fixture link walks the transfer it is asked for.
        let a = p(&["--fixture-link", "--fixture-transfer=too-large"]).expect("transfer");
        assert!(matches!(
            start_options(&a).expect("options").launch,
            Launch::Fixture(FixtureOptions {
                link: true,
                transfer: Some(FixtureTransfer::Walk(HistoryTransfer::Failed(
                    HistoryTransferFailure::TooLarge
                ))),
                ..
            })
        ));
        for name in [
            "walk",
            "declined",
            "wait",
            "download",
            "read",
            "link-again",
            "update-app",
            "corrupt",
            "too-large",
            "download-failed",
            "internal",
        ] {
            assert!(link_transfer(name).is_some(), "{name}");
            // (A build without the test network offers no fixture in its usage.)
            #[cfg(feature = "fake")]
            assert!(USAGE.contains(name), "{name} is not in the usage text");
        }
        assert!(p(&["--fixture-link", "--fixture-transfer=nope"]).is_err());
        assert!(p(&["--fixture", "--fixture-transfer=walk"]).is_err());
        assert!(p(&["--fixture-transfer=walk"]).is_err());
        // Slow sends (link feedback L6) need the fixture.
        let a = p(&["--fixture", "--fixture-send=keys"]).expect("send");
        assert!(matches!(
            start_options(&a).expect("options").launch,
            Launch::Fixture(FixtureOptions {
                sends: Some(FixtureSends::Slow(Some(_))),
                ..
            })
        ));
        // The network of the fixture account.
        for (name, network) in [
            ("fake", FixtureNetwork::Fake),
            ("whatsapp", FixtureNetwork::WhatsApp),
            ("threema", FixtureNetwork::Threema),
            ("telegram", FixtureNetwork::Telegram),
        ] {
            let a = p(&["--fixture", &format!("--fixture-network={name}")]).expect("network");
            assert!(matches!(
                start_options(&a).expect("options").launch,
                Launch::Fixture(FixtureOptions { network: n, .. }) if n == network
            ));
            #[cfg(feature = "fake")]
            assert!(USAGE.contains(name), "{name} is not in the usage text");
        }
        let a = p(&["--fixture-link", "--fixture-network=whatsapp"]).expect("link");
        assert!(matches!(
            start_options(&a).expect("options").launch,
            Launch::Fixture(FixtureOptions {
                link: true,
                network: FixtureNetwork::WhatsApp,
                ..
            })
        ));
        let a = p(&["--fixture"]).expect("plain");
        assert!(matches!(
            start_options(&a).expect("options").launch,
            Launch::Fixture(FixtureOptions {
                network: FixtureNetwork::Fake,
                ..
            })
        ));
        assert!(p(&["--fixture-network=whatsapp"]).is_err());
        assert!(p(&["--fixture", "--fixture-network=nope"]).is_err());
        // The pages of upload progress send their files themselves; `--fixture-send=uploading`
        // only holds the uploads of the lower chat.
        for (page, upload) in [
            ("uploading", FixtureUpload::Album),
            ("uploadfile", FixtureUpload::File),
        ] {
            let a = p(&["--fixture", &format!("--page={page}")]).expect("page");
            assert!(
                matches!(
                    start_options(&a).expect("options").launch,
                    Launch::Fixture(FixtureOptions { sends: Some(FixtureSends::Uploading(u)), .. }) if u == upload
                ),
                "{page}"
            );
        }
        let a = p(&["--fixture", "--fixture-send=uploading"]).expect("send");
        assert!(matches!(
            start_options(&a).expect("options").launch,
            Launch::Fixture(FixtureOptions {
                sends: Some(FixtureSends::Uploading(FixtureUpload::Idle)),
                ..
            })
        ));
        assert!(p(&["--fixture-send=slow"]).is_err());
        assert!(p(&["--fixture", "--fixture-send=nope"]).is_err());
        assert!(start_options(&p(&["--profile=a/b"]).expect("parse")).is_err());
        assert!(p(&["--fixture", "--fixture-link"]).is_err());
        // G4: the pages that stand at a link page of WhatsApp, Threema or Telegram start the link on their own
        // and tick the network's risk warning for the person: they exist on a fixture only, never on
        // the profile of a plain start (the page names are written out here, not asked of the code).
        for page in [
            "waqr",
            "waphone",
            "wacode",
            "thqr",
            "themoji",
            "tgqr",
            "tgcode",
            "tgpassword",
        ] {
            let flag = format!("--page={page}");
            let refused = p(&[&flag]).expect_err(page);
            assert!(
                refused.contains(&flag) && refused.contains("--fixture"),
                "{page}: {refused}"
            );
            assert!(p(&["--profile=work", &flag]).is_err(), "{page}");
            assert!(p(&["--fixture", &flag]).is_ok(), "{page}");
            assert!(p(&["--fixture-link", &flag]).is_ok(), "{page}");
        }
        // (the warning page of a network that is off opens the warning, it does not skip it)
        assert!(p(&["--page=warisk"]).is_ok());
        assert!(p(&["--page=tgwarn"]).is_ok());
        assert!(p(&["--page=nope"]).is_err());
        assert!(p(&["--size=12x"]).is_err());
        assert!(p(&["--size=100x100"]).is_err());
        assert!(p(&["--screenshot="]).is_err());
        assert_eq!(p(&[]).expect("empty"), Args::default());
        assert_eq!(p(&["--help"]), Err(String::new()));
    }

    #[cfg(feature = "perf")]
    #[test]
    fn perf_flags() {
        use kourier_app::perf::{PerfOptions, PerfSpec};
        let a = p(&[
            "--perf-profile=/x/profile",
            "--perf-scenario=session",
            "--perf-live-ms=1777",
            "--perf-size=12,3,40,10",
        ])
        .expect("parse");
        assert_eq!(a.ui.perf_scenario.as_deref(), Some("session"));
        assert!(!a.perf.seed);
        let spec = PerfSpec {
            contacts: 12,
            groups: 3,
            messages_per_conversation: 40,
            media_percent: 10,
        };
        assert_eq!(
            start_options(&a).expect("options").launch,
            Launch::Perf(PerfOptions {
                home: PathBuf::from("/x/profile"),
                spec,
                live_after: Some(std::time::Duration::from_millis(1777)),
            })
        );
        let a = p(&["--perf-profile=/x", "--perf-seed"]).expect("parse");
        assert!(a.perf.seed);
        assert!(matches!(
            start_options(&a).expect("options").launch,
            Launch::Perf(PerfOptions { spec, live_after: None, .. }) if spec == PerfSpec::default()
        ));
        // Without a profile directory the other flags change nothing about the launch.
        let a = p(&["--perf-scenario=startup"]).expect("parse");
        assert_eq!(start_options(&a).expect("options").launch, Launch::Normal);
        for bad in [
            "--perf-profile=",
            "--perf-size=1,2,3",
            "--perf-size=2,1,40,10",
            "--perf-size=12,3,40,101",
            "--perf-live-ms=soon",
        ] {
            assert!(p(&[bad]).is_err(), "{bad}");
        }
    }

    // (The flag and its field exist in debug builds only.)
    #[cfg(debug_assertions)]
    #[test]
    fn dev_control_flag() {
        assert!(p(&["--dev-control"]).expect("parse").ui.dev_control);
        assert!(!p(&["--fixture"]).expect("parse").ui.dev_control);
    }

    /// `--print-networks` is the hidden flag `cargo xtask dist` runs on the packed binary: it is
    /// parsed, takes `--profile` and nothing that starts a session, and the usage does not offer it.
    #[test]
    fn print_networks_is_a_hidden_flag_that_stands_alone() {
        assert!(!p(&[]).expect("plain").print_networks);
        assert!(p(&["--print-networks"]).expect("parse").print_networks);
        let a = p(&["--print-networks", "--profile=work"]).expect("with a profile");
        assert!(a.print_networks && a.profile.as_deref() == Some("work"));
        for other in ["--fixture", "--fixture-link", "--background"] {
            assert!(p(&["--print-networks", other]).is_err(), "{other}");
        }
        assert!(!USAGE.contains("print-networks"), "the flag is hidden");
    }
}

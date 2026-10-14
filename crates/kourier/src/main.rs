//! The `kourier` binary: parses the command line, starts the app (runtime, keys, core), runs
//! the Qt UI and shuts down in order (§8.2).

use std::path::PathBuf;
use std::process::ExitCode;

use kourier_app::desktop::{DesktopMode, DesktopOptions, SystemBus, handled_uris};
use kourier_app::{
    App, AppError, FixtureOptions, FixtureSends, FixtureState, FixtureTransfer, HistoryTransfer,
    HistoryTransferFailure, Launch, Profile, StartOptions,
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
transferdownload|transferread|transferupdate|transferbanner|transferfailed|transferask>] \
[--fixture-transfer=<walk|declined|wait|download|read|link-again|update-app|corrupt|too-large|\
download-failed|internal>] [--fixture-send=<slow|keys|members|retry>] [--screenshot=<png>] \
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
                  first attempt is rate limited for 30 seconds (retry)";

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
    background: bool,
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
    if out.fixture_send.is_some() && !out.fixture {
        return Err("--fixture-send needs --fixture".into());
    }
    if out.fixture_transfer.is_some() && !out.fixture_link {
        return Err("--fixture-transfer needs --fixture-link".into());
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
        _ => return None,
    })
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

/// The state the fixture account ends in for a page: the account-state pages show its banner.
fn fixture_state(page: Option<&str>) -> FixtureState {
    match page {
        Some("offline") => FixtureState::Offline,
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
    let launch = if args.fixture || args.fixture_link {
        Launch::Fixture(FixtureOptions {
            link: args.fixture_link || page_links,
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
            sends: args.fixture_send,
            ..FixtureOptions::default()
        })
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
    app.shutdown();
    match code {
        0 => ExitCode::SUCCESS,
        code => ExitCode::from(u8::try_from(code).unwrap_or(1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Args, String> {
        parse(args.iter().map(|s| (*s).to_owned()))
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
        assert!(p(&["--fixture-send=slow"]).is_err());
        assert!(p(&["--fixture", "--fixture-send=nope"]).is_err());
        assert!(start_options(&p(&["--profile=a/b"]).expect("parse")).is_err());
        assert!(p(&["--fixture", "--fixture-link"]).is_err());
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
}

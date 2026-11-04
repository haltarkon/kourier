//! [`TgParts`]: what a Telegram account cannot run without, as a REQUIRED argument of
//! [`TelegramNetwork::new`] (TG-1 plan §2.10, decisions D3 H2/H4): a LAZY maker of the helper
//! process, the dialer the SOCKS5 gate hands TDLib's connections to (TG-1b), the files root, the
//! environment switch and the API credentials. There is no default and no `Default` impl.
//!
//! Only `kourier-app` asks the one module that builds the application's parts; every test hands
//! in parts that refuse and count, or a scripted helper. The maker is called on the account's own
//! thread when the account first needs TDLib, never at the start of the application or of an
//! actor: a login does (TG-1b). The application's dialer REFUSES every connection unless the run
//! tool's confirmation chose the dialing mode (`real.rs`, TR R0a); `Dialer` and `from_makers` are
//! crate-private, so no other crate can hand the gate a dialer of its own.
//!
//! [`TelegramNetwork::new`]: crate::TelegramNetwork

use std::fmt;
use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use kourier_model::AccountUuid;

use crate::api::ApiCredentials;
use crate::pin::{PIN, Pin};

/// Which Telegram servers an account lives on. Stored in `backend.db` at the first login and
/// binding afterwards; a test-server account exists only in a debug build's run profile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TgEnv {
    /// Telegram's production servers.
    #[default]
    Production,
    /// Telegram's test servers (`use_test_dc`).
    Test,
}

impl TgEnv {
    /// The stored code.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Production => "production",
            Self::Test => "test",
        }
    }

    /// The value of a stored code; `None` for any other text.
    pub fn parse(code: &str) -> Option<Self> {
        match code {
            "production" => Some(Self::Production),
            "test" => Some(Self::Test),
            _ => None,
        }
    }
}

/// What the maker is asked for: the helper of one account. Not `Copy` since TG-2b (two paths;
/// critic L-d): makers take it by reference.
#[derive(Clone, Debug)]
pub struct HelperArgs {
    /// The account the helper serves.
    pub account: AccountUuid,
    /// The port of the account's gate (started before the helper): the helper is locked down to
    /// it (`kourier_tdlib::lockdown`) and refuses a `G` frame for any other.
    pub gate_port: u16,
    /// The account's `tdlib/` and files directories (the parameters' `database_directory` and
    /// `files_directory`): the helper's filesystem lock-down allows only these two (TG-2b), and
    /// it refuses parameters that name others. Absolute and valid UTF-8 (the supervisor refuses
    /// any other path before the spawn).
    pub database_dir: PathBuf,
    pub files_dir: PathBuf,
}

/// A running helper as the supervisor sees it: a real child process, or the in-process serve
/// thread of the scripted layer.
pub trait HelperChild: Send {
    /// Whether it has ended, without waiting.
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
    /// Ends it at once (SIGKILL for a process).
    fn kill(&mut self) -> io::Result<()>;
    /// Waits for its end and reaps it.
    fn wait(&mut self) -> io::Result<ExitStatus>;
}

impl HelperChild for std::process::Child {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        std::process::Child::try_wait(self)
    }

    fn kill(&mut self) -> io::Result<()> {
        std::process::Child::kill(self)
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        std::process::Child::wait(self)
    }
}

/// A started helper: the app's end of the protocol socket, its stderr (counted, never shown) and
/// the child.
pub struct HelperProcess {
    /// The app's end of the socket the helper speaks the frame protocol on.
    pub sock: UnixStream,
    /// The helper's stderr; its lines are counted, never logged.
    pub stderr: Option<Box<dyn Read + Send>>,
    /// The helper itself.
    pub child: Box<dyn HelperChild>,
}

impl fmt::Debug for HelperProcess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HelperProcess")
    }
}

/// Where the application finds the helper binary and the pinned library. Resolving them opens and
/// starts nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperFiles {
    /// The helper binary `kourier-tdlib`.
    pub helper: PathBuf,
    /// The pinned `libtdjson.so.<version>`.
    pub tdjson: PathBuf,
    /// Pass the debug no-dump switch to the helper (the application's own rule for a debug build).
    pub no_dump: bool,
}

impl HelperFiles {
    /// The directory no file can exist in: where "no helper was found" points (fix F15).
    pub const NOT_FOUND_DIR: &'static str = "/nonexistent/kourier-telegram-helper-not-found";

    /// Files that cannot exist, for an application that could not tell where it is installed
    /// (no `current_exe`): a spawn from them fails with "not found", typed by the operating system,
    /// and never finds a `kourier-tdlib` relative to the working directory.
    pub fn not_found() -> Self {
        let dir = PathBuf::from(Self::NOT_FOUND_DIR);
        Self {
            helper: dir.join("kourier-tdlib"),
            tdjson: dir.join("libtdjson.so"),
            no_dump: false,
        }
    }
}

use crate::gate::dialer::Dialer;
pub(crate) use crate::gate::dialer::RefusingDialer;

/// What the helper's lock-down hello must say (TR R0a D-11; it replaces a plain yes/no).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LockdownNeed {
    /// Nothing is required: a test's helper says `off`.
    Off,
    /// `on`, or `unavailable` where the kernel has no Landlock (TG-1b G5): the refusing parts of
    /// the application.
    OnOrUnavailable,
    /// `on` only: a hello that says `unavailable` fails the start. The parts that dial need it: no
    /// real connection without Landlock and seccomp around TDLib.
    On,
}

/// The maker of a helper process.
type Spawn = dyn Fn(&HelperArgs) -> io::Result<HelperProcess> + Send + Sync;

/// What a Telegram account runs with. Cheap to clone (the maker and the dialer are shared); its
/// `Debug` prints nothing of them.
#[derive(Clone)]
pub struct TgParts {
    spawn: Arc<Spawn>,
    dialer: Arc<dyn Dialer>,
    files_root: Option<PathBuf>,
    api: Option<ApiCredentials>,
    env: TgEnv,
    /// This computer's host name as the application read it (empty: none); Telegram's list of
    /// sessions shows `Kourier (<host>)` (his answer U-4, WhatsApp's label).
    host_name: String,
    /// What the helper must report of its lock-down: [`LockdownNeed::Off`] except for the parts of
    /// `real.rs` (TG-1b fix G5, TR R0a D-11); a test's helper says `off`.
    lockdown: LockdownNeed,
    /// The TDLib the helper's hello must name: [`PIN`] in every build; only `cfg(test)` code can
    /// name another (the fake DC's test-only library reports the commit as 40 zeros, TD-4a
    /// `FAKEDC_PIN`), so the application's parts always refuse that library (`WRONG_HELPER`).
    pin: &'static Pin,
    /// Whether the dialer of these parts connects to Telegram: true ONLY for the parts of
    /// `real.rs`' Telegram mode (TR R0a fix G-3). The supervisor's start refuses such parts with
    /// the production servers or with the test-only API pair, before the gate and the helper.
    dials: bool,
}

impl TgParts {
    /// Parts from a helper maker and a dialer. Nothing is called here. Crate-private (TR R0a, critic
    /// 6): no other crate can build parts around a dialer of its own.
    pub(crate) fn from_makers(
        spawn: impl Fn(&HelperArgs) -> io::Result<HelperProcess> + Send + Sync + 'static,
        dialer: Arc<dyn Dialer>,
    ) -> Self {
        Self {
            spawn: Arc::new(spawn),
            dialer,
            files_root: None,
            api: None,
            env: TgEnv::Production,
            host_name: String::new(),
            lockdown: LockdownNeed::Off,
            pin: &PIN,
            dials: false,
        }
    }

    /// Marks the parts as ones whose dialer connects to Telegram (`real.rs`' Telegram arm, beside
    /// [`LockdownNeed::On`], and nowhere else): see [`TgParts::dials`].
    #[must_use]
    pub(crate) fn dialing(mut self) -> Self {
        self.dials = true;
        self
    }

    /// Parts whose helper's hello must name `pin` instead of [`PIN`]: the fake-DC tests' parts
    /// over the test-only library (TD-4a). Test code only: no build of the application has it.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn expecting_pin(mut self, pin: &'static Pin) -> Self {
        self.pin = pin;
        self
    }

    /// Parts whose helper must report a lock-down in its hello as `need` says (`on`, or also
    /// `unavailable` where the kernel has no Landlock): a helper that says anything else (`off`: a
    /// test helper, or a stale TG-1a binary that ignores `--gate-port`) fails the start typed and
    /// is stopped. Only the real parts call this (`real.rs`); a test helper has no lock-down.
    #[must_use]
    pub(crate) fn requiring_lockdown(mut self, need: LockdownNeed) -> Self {
        self.lockdown = need;
        self
    }

    /// Parts that refuse every helper spawn and every dial, for a test of the APPLICATION that
    /// must start the network and show that nothing was made: `made` counts the spawn attempts.
    #[doc(hidden)]
    pub fn refusing(made: Arc<AtomicUsize>) -> Self {
        Self::from_makers(
            move |_args| {
                made.fetch_add(1, Ordering::SeqCst);
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "the helper is refused in this test",
                ))
            },
            Arc::new(RefusingDialer::new(Arc::new(AtomicUsize::new(0)), false)),
        )
    }

    /// The root under which each account gets its TDLib files directory (the application's tmpfs
    /// media cache).
    #[must_use]
    pub fn with_files_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.files_root = Some(root.into());
        self
    }

    /// The API credentials the accounts use. Without them a login answers a typed "this build has
    /// no Telegram API key" (TG-1b).
    #[must_use]
    pub fn with_api(mut self, api: ApiCredentials) -> Self {
        self.api = Some(api);
        self
    }

    /// The environment a NEW account binds to at its first login (a stored one wins).
    #[must_use]
    pub fn with_env(mut self, env: TgEnv) -> Self {
        self.env = env;
        self
    }

    /// The host name the application read (`kourier_app::app::host_name`, its one reader): the
    /// session is listed on Telegram as `Kourier (<host>)`. The backend never reads it itself.
    #[must_use]
    pub fn with_host_name(mut self, host: impl Into<String>) -> Self {
        self.host_name = host.into();
        self
    }

    /// What Telegram's list of sessions shows for this computer (`device_model`):
    /// `Kourier (<host>)`, `Kourier` when the host name is empty once control and bidi marks are
    /// gone; the host is cut at [`HOST_MAX_CHARS`] characters on a character border (the rule of
    /// WhatsApp's `props::device_name`, written again: the crates share no code).
    pub(crate) fn device_model(&self) -> String {
        let host: String = self
            .host_name
            .chars()
            .filter(|c| !c.is_control() && !is_bidi_control(*c))
            .collect();
        let host: String = host.trim().chars().take(HOST_MAX_CHARS).collect();
        let host = host.trim();
        if host.is_empty() {
            "Kourier".to_owned()
        } else {
            format!("Kourier ({host})")
        }
    }

    /// Starts one helper (the account thread calls it, never the network's `spawn`).
    pub(crate) fn spawn_helper(&self, args: &HelperArgs) -> io::Result<HelperProcess> {
        (self.spawn)(args)
    }

    /// The gate's dialer.
    pub(crate) fn dialer(&self) -> &Arc<dyn Dialer> {
        &self.dialer
    }

    /// The files root, if the application gave one.
    pub fn files_root(&self) -> Option<&PathBuf> {
        self.files_root.as_ref()
    }

    /// The API credentials, if any.
    pub(crate) fn api(&self) -> Option<&ApiCredentials> {
        self.api.as_ref()
    }

    /// Whether these parts dial Telegram (set only by the Telegram mode of `real.rs`).
    pub(crate) fn dials(&self) -> bool {
        self.dials
    }

    /// What the hello must report of the lock-down.
    pub(crate) fn lockdown_need(&self) -> LockdownNeed {
        self.lockdown
    }

    /// The TDLib the hello must name ([`PIN`] outside the fake-DC tests).
    pub(crate) fn pin(&self) -> &'static Pin {
        self.pin
    }

    /// The environment of a new account.
    pub(crate) fn env(&self) -> TgEnv {
        self.env
    }
}

/// The longest host name used in the device model, in characters.
pub(crate) const HOST_MAX_CHARS: usize = 48;

/// The marks that reorder or isolate text (U+061C, U+200E, U+200F, U+202A-202E, U+2066-2069).
pub(crate) fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

impl fmt::Debug for TgParts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TgParts")
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;

    fn account() -> HelperArgs {
        HelperArgs {
            account: AccountUuid::new_random(),
            gate_port: 40001,
            database_dir: PathBuf::from("/nowhere/tdlib"),
            files_dir: PathBuf::from("/nowhere/files"),
        }
    }

    #[test]
    fn refusing_parts_count_every_spawn_and_refuse_every_dial() {
        let made = Arc::new(AtomicUsize::new(0));
        let parts = TgParts::refusing(made.clone());
        assert_eq!(made.load(Ordering::SeqCst), 0, "building the parts spawned");
        for n in 1..=2 {
            assert!(
                parts.spawn_helper(&account()).is_err(),
                "a spawn was not refused"
            );
            assert_eq!(made.load(Ordering::SeqCst), n, "a spawn was not counted");
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let to = SocketAddr::from(([127, 0, 0, 1], 9));
        let refused = rt.block_on(parts.dialer().dial(to)).err().map(|e| e.kind());
        assert_eq!(
            refused,
            Some(io::ErrorKind::PermissionDenied),
            "a dial got through"
        );
        assert_eq!(format!("{parts:?}"), "TgParts");
    }

    #[test]
    fn the_refusing_dialer_counts_each_dial() {
        let refused = Arc::new(AtomicUsize::new(0));
        let dialer = RefusingDialer::new(refused.clone(), true);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let to = SocketAddr::from(([127, 0, 0, 1], 9));
        for n in 1..=3 {
            assert!(rt.block_on(dialer.dial(to)).is_err(), "a dial got through");
            assert_eq!(refused.load(Ordering::SeqCst), n, "a dial was not counted");
        }
    }

    #[test]
    fn the_env_codes_round_trip_and_nothing_else_parses() {
        for env in [TgEnv::Production, TgEnv::Test] {
            assert_eq!(TgEnv::parse(env.as_str()), Some(env));
        }
        for bad in ["", "prod", "Test", "staging"] {
            assert_eq!(TgEnv::parse(bad), None, "an unknown env parsed");
        }
        assert_eq!(TgEnv::default(), TgEnv::Production);
        let parts = TgParts::refusing(Arc::new(AtomicUsize::new(0)));
        assert_eq!(parts.env(), TgEnv::Production);
        let parts = parts
            .with_env(TgEnv::Test)
            .with_api(ApiCredentials::DUMMY)
            .with_files_root("/run/x");
        assert_eq!(parts.env(), TgEnv::Test);
        assert!(parts.api().is_some_and(ApiCredentials::is_dummy));
        assert_eq!(parts.files_root(), Some(&PathBuf::from("/run/x")));
    }

    /// His answer U-4: the session is `Kourier (<host>)`, cleaned and cut like WhatsApp's label.
    /// Mutant: `device_model` returns `"Kourier"` always -> fails.
    #[test]
    fn the_device_model_names_the_host_the_app_gave() {
        let parts = || TgParts::refusing(Arc::new(AtomicUsize::new(0)));
        assert_eq!(parts().device_model(), "Kourier");
        assert_eq!(
            parts().with_host_name("ada-desk").device_model(),
            "Kourier (ada-desk)"
        );
        assert_eq!(
            parts()
                .with_host_name(" \u{202E}ada\n-desk\u{7} ")
                .device_model(),
            "Kourier (ada-desk)"
        );
        assert_eq!(
            parts().with_host_name("\u{200E} \t").device_model(),
            "Kourier"
        );
        let long = "\u{e9}".repeat(HOST_MAX_CHARS + 5);
        let cut = parts().with_host_name(long).device_model();
        assert_eq!(
            cut,
            format!("Kourier ({})", "\u{e9}".repeat(HOST_MAX_CHARS))
        );
    }
}

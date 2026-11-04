//! Shared by the Signal e2e tests: the mock server with our phone and a contact ([`World`]),
//! and one run of the app's core over a profile directory ([`App`]), driven through the facade
//! as the UI drives it.

#![allow(
    dead_code,
    unused_macros,
    unused_imports,
    reason = "each test file uses a part of the shared helpers (no test of this crate needs its own process yet)"
)]

#[path = "../../../kourier-signal/tests/support/harness.rs"]
pub mod harness;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use harness::{CreatePrimary, DeviceInfo, Harness, Primary, SendText};
use kourier_backend_api::{
    BackendConfig, BackendEnv, InitialSync, LocalTestEnv, NetworkRegistry, SystemClock,
};
use kourier_core::facade::{
    ConversationListQuery, CoreHandle, MessageRow, Query, QueryData, TimelineAnchor, TimelineQuery,
    UiCommand, UiEvent,
};
use kourier_core::testing::DirVault;
use kourier_core::{Core, CoreConfig};
use kourier_model::{
    AccountId, AccountState, Body, CannotSend, ConversationFeatures, ConversationId, LoginStepKind,
    NetworkKind, Redacted, UserId,
};
use kourier_signal::SignalNetwork;
use tokio::sync::broadcast;

/// The longest single wait.
pub const WAIT: Duration = Duration::from_secs(60);
/// `core.db` key of the test app.
pub const CORE_KEY: [u8; 32] = [0x5c; 32];

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

/// The local test environment of a running harness; the CA PEM is written into `dir`.
pub fn local_env(handshake: &harness::Handshake, dir: &Path) -> BackendEnv {
    let ca = dir.join("mock-ca.pem");
    std::fs::write(&ca, &handshake.ca_pem).expect("write the mock CA");
    BackendEnv::LocalTest(LocalTestEnv {
        host: handshake.host.clone(),
        port: handshake.port,
        ca_cert: Some(ca),
        trust_root: Some(handshake.trust_root.clone()),
        zk_params: Some(handshake.zk_public_params.clone()),
    })
}

pub fn user(primary: &Primary) -> UserId {
    UserId::parse(&format!("aci:{}", primary.aci)).expect("user id")
}

/// The mock server with our phone ("Myself") and one contact ("Alice").
pub struct World {
    /// The harness.
    pub harness: Harness,
    /// The app's profile directory.
    pub tmp: tempfile::TempDir,
    /// The mock's environment.
    pub env: BackendEnv,
    /// Our phone ("Myself").
    pub phone: Primary,
    /// A contact.
    pub alice: Primary,
}

impl World {
    pub async fn start() -> Option<Self> {
        init_tracing();
        let harness = Harness::start_or_skip().await.expect("mock harness")?;
        let tmp = tempfile::tempdir().expect("temp dir");
        let env = local_env(harness.handshake(), tmp.path());
        let alice = harness
            .create_primary(&CreatePrimary::named("Alice"))
            .await
            .expect("Alice");
        // Alice is not in the phone's address book (contact sync): a system contact's chat is
        // accepted from the start (§6.7, S9), and these tests accept it on the phone.
        let phone = harness
            .create_primary(&CreatePrimary::named("Myself"))
            .await
            .expect("Myself");
        Some(Self {
            harness,
            tmp,
            env,
            phone,
            alice,
        })
    }

    /// Sends a text from Alice to our device and waits until the device acked it.
    pub async fn alice_says(&self, device: &DeviceInfo, text: &str) -> u64 {
        self.harness
            .send_text(&SendText {
                await_delivery: true,
                timeout: Some(60_000),
                ..SendText::new(&self.alice.aci, &device.device_ref, text)
            })
            .await
            .expect("sendText")
            .timestamp
    }

    pub async fn shutdown(self) {
        drop(self.tmp);
        self.harness.shutdown().await.expect("harness shutdown");
    }
}

/// One run of the app's core over the files in `dir`.
pub struct App {
    core: Option<Core>,
    /// The facade.
    pub handle: CoreHandle,
    /// UI events since the start.
    pub events: broadcast::Receiver<UiEvent>,
}

impl App {
    /// Starts the core with Signal on the mock and tells it the UI is up (so it connects).
    /// Links report `Done` right after the sync requests (`InitialSync::LinkOnly`).
    pub async fn start(dir: &Path, env: &BackendEnv) -> Self {
        Self::start_with(dir, env, InitialSync::LinkOnly).await
    }

    /// [`Self::start`] with the initial sync of a link (the app uses `ContactsAndStorage`).
    pub async fn start_with(dir: &Path, env: &BackendEnv, initial_sync: InitialSync) -> Self {
        Self::start_full(dir, env, initial_sync, None).await
    }

    /// [`Self::start`] with a plaintext media cache at `media` (the app's is on the tmpfs
    /// `$XDG_RUNTIME_DIR`, outside the profile), so `OpenMedia` works.
    pub async fn start_with_media(dir: &Path, env: &BackendEnv, media: &Path) -> Self {
        Self::start_full(dir, env, InitialSync::LinkOnly, Some(media.to_owned())).await
    }

    /// [`Self::start_with_media`] with a Signal backend whose timings a test shortened
    /// (`SignalNetwork::with_download_resume_pause` and the like).
    pub async fn start_tuned(
        dir: &Path,
        env: &BackendEnv,
        network: SignalNetwork,
        media: &Path,
    ) -> Self {
        Self::start_on(
            dir,
            env,
            InitialSync::LinkOnly,
            Some(media.to_owned()),
            network,
        )
        .await
    }

    /// [`Self::start_with_media`] with the waits for our phone's answer to a request for lost
    /// files shortened (`Timeouts::restore_answer`, `restore_pending`): a phone that never
    /// answers ends as "did not answer" in seconds.
    pub async fn start_with_restore_waits(
        dir: &Path,
        env: &BackendEnv,
        media: &Path,
        answer: Duration,
        pending: Duration,
    ) -> Self {
        Self::start_cfg(
            dir,
            env,
            InitialSync::LinkOnly,
            Some(media.to_owned()),
            SignalNetwork::new(),
            |cfg| {
                cfg.timeouts.restore_answer = answer;
                cfg.timeouts.restore_pending = pending;
            },
        )
        .await
    }

    async fn start_full(
        dir: &Path,
        env: &BackendEnv,
        initial_sync: InitialSync,
        media: Option<std::path::PathBuf>,
    ) -> Self {
        Self::start_on(dir, env, initial_sync, media, SignalNetwork::new()).await
    }

    async fn start_on(
        dir: &Path,
        env: &BackendEnv,
        initial_sync: InitialSync,
        media: Option<std::path::PathBuf>,
        network: SignalNetwork,
    ) -> Self {
        Self::start_cfg(dir, env, initial_sync, media, network, |_| {}).await
    }

    async fn start_cfg(
        dir: &Path,
        env: &BackendEnv,
        initial_sync: InitialSync,
        media: Option<std::path::PathBuf>,
        network: SignalNetwork,
        tune: impl FnOnce(&mut CoreConfig),
    ) -> Self {
        let data = dir.join("data");
        std::fs::create_dir_all(&data).expect("data dir");
        let networks = NetworkRegistry::new()
            .with(Arc::new(network))
            .expect("registry");
        let mut cfg = CoreConfig::new(
            data.join("core.db"),
            kourier_db::DbKey::from(CORE_KEY),
            networks,
            Arc::new(DirVault::new(&data)),
            Arc::new(SystemClock),
            tokio::runtime::Handle::current(),
        );
        cfg.backend = BackendConfig {
            env: env.clone(),
            initial_sync,
            ..BackendConfig::default()
        };
        cfg.media_cache = media;
        tune(&mut cfg);
        let core = Core::start(cfg).await.expect("core starts");
        let handle = core.handle();
        let events = handle.subscribe();
        handle.send(UiCommand::UiReady).expect("UiReady");
        Self {
            core: Some(core),
            handle,
            events,
        }
    }

    pub async fn stop(mut self) {
        if let Some(core) = self.core.take() {
            core.shutdown().await;
        }
    }

    pub fn send(&self, command: UiCommand) {
        self.handle.send(command).expect("the core is running");
    }

    /// The next event `f` accepts (others are skipped).
    pub async fn event<T>(&mut self, what: &str, mut f: impl FnMut(&UiEvent) -> Option<T>) -> T {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(Ok(event)) => {
                    if let Some(found) = f(&event) {
                        return found;
                    }
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    panic!("the core stopped while waiting for {what}")
                }
                Err(_) => panic!("timed out waiting for {what}"),
            }
        }
    }

    /// The account's row.
    pub async fn account(&self, account: AccountId) -> Option<kourier_core::facade::AccountRow> {
        match self.handle.query(Query::Accounts).await.expect("accounts") {
            QueryData::Accounts(rows) => rows.into_iter().find(|row| row.id == account),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Waits until the account is `Active` and `Online`.
    pub async fn wait_online(&self, account: AccountId) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let row = self.account(account).await;
            if let Some(row) = &row
                && row.state == AccountState::Active
                && row.conn.is_online()
            {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "account not Active and Online: {row:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The 1:1 conversation with `peer`, once it exists.
    pub async fn conversation_with(&self, account: AccountId, peer: &UserId) -> ConversationId {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let query = ConversationListQuery {
                account: Some(account),
                limit: 100,
                ..ConversationListQuery::default()
            };
            match self
                .handle
                .query(Query::Conversations(query))
                .await
                .expect("conversations")
            {
                QueryData::Conversations(page) => {
                    if let Some(row) = page.rows.iter().find(|r| r.peer.as_ref() == Some(peer)) {
                        return row.id;
                    }
                }
                other => panic!("unexpected {other:?}"),
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no conversation with {peer:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The conversation's rows whose text is `text`.
    pub async fn rows_with_text(&self, conv: ConversationId, text: &str) -> Vec<MessageRow> {
        let query = TimelineQuery {
            conv,
            anchor: TimelineAnchor::Newest,
            limit: 500,
        };
        match self
            .handle
            .query(Query::Timeline(query))
            .await
            .expect("timeline")
        {
            QueryData::Timeline(page) => page
                .rows
                .into_iter()
                .filter(|row| matches!(&row.body, Body::Text(t) if t.body == text))
                .collect(),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Waits until `text` is in the timeline and returns its rows.
    pub async fn wait_text(&self, conv: ConversationId, text: &str) -> Vec<MessageRow> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let rows = self.rows_with_text(conv, text).await;
            if !rows.is_empty() {
                return rows;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{text:?} never reached the timeline"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The conversation's features (§3.8; the default set until the backend sent them).
    pub async fn features(&self, conv: ConversationId) -> ConversationFeatures {
        match self
            .handle
            .query(Query::Conversation(conv))
            .await
            .expect("conversation")
        {
            QueryData::Conversation(Some(detail)) => detail.features,
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Waits until the backend sent the 1:1 chat's features and `cannot_send` is `expected`.
    pub async fn wait_cannot_send(&self, conv: ConversationId, expected: Option<CannotSend>) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let features = self.features(conv).await;
            // `max_text_bytes` is set once the backend reported the features.
            if features.max_text_bytes.is_some() && features.cannot_send == expected {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "cannot_send never became {expected:?}: {features:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Our phone accepts the chat with `contact` (a `MessageRequestResponse` sync to `device`);
    /// returns the conversation once the core can send there.
    pub async fn accept(
        &self,
        world: &World,
        account: AccountId,
        device: &DeviceInfo,
        contact: &Primary,
    ) -> ConversationId {
        world
            .harness
            .send_message_request_response(
                &world.phone.device_ref,
                &device.device_ref,
                &contact.aci,
                "accept",
            )
            .await
            .expect("message request response");
        let conv = self.conversation_with(account, &user(contact)).await;
        self.wait_cannot_send(conv, None).await;
        conv
    }

    /// The conversation's newest rows (≤ 500), newest first.
    pub async fn rows(&self, conv: ConversationId) -> Vec<MessageRow> {
        let query = TimelineQuery {
            conv,
            anchor: TimelineAnchor::Newest,
            limit: 500,
        };
        match self
            .handle
            .query(Query::Timeline(query))
            .await
            .expect("timeline")
        {
            QueryData::Timeline(page) => page.rows,
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Waits until a row of the conversation satisfies `pred` and returns it.
    pub async fn wait_row_where(
        &self,
        conv: ConversationId,
        what: &str,
        pred: impl Fn(&MessageRow) -> bool,
    ) -> MessageRow {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let rows = self.rows(conv).await;
            if let Some(row) = rows.iter().find(|row| pred(row)) {
                return row.clone();
            }
            assert!(tokio::time::Instant::now() < deadline, "{what}: {rows:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Waits until the row with `text` satisfies `pred` and returns it.
    pub async fn wait_row(
        &self,
        conv: ConversationId,
        text: &str,
        what: &str,
        pred: impl Fn(&MessageRow) -> bool,
    ) -> MessageRow {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let rows = self.rows_with_text(conv, text).await;
            if let Some(row) = rows.iter().find(|row| pred(row)) {
                return row.clone();
            }
            assert!(tokio::time::Instant::now() < deadline, "{what}: {rows:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Starts linking a new Signal account through the facade, the way the link page does:
    /// adds the account, answers the device-name step and returns the account with the URL
    /// of its first QR code.
    pub async fn begin_link(&mut self) -> (AccountId, String) {
        self.send(UiCommand::AddAccount {
            network: NetworkKind::Signal,
            flow: "qr".to_owned(),
        });
        let account = self
            .event("AccountAdded", |e| match e {
                UiEvent::AccountAdded { account } => Some(*account),
                _ => None,
            })
            .await;
        let (txn, field) = self
            .event("the device-name step", |e| match e {
                UiEvent::LoginStep { account: a, step } if *a == account => match &step.kind {
                    LoginStepKind::Input { fields } => Some((step.txn, fields[0].id.clone())),
                    LoginStepKind::Failed { error } => panic!("login failed: {error:?}"),
                    _ => None,
                },
                _ => None,
            })
            .await;
        self.send(UiCommand::SubmitLoginInput {
            account,
            txn,
            values: vec![(field, Redacted::new("Kourier e2e".to_owned()))],
        });
        let url = self
            .event("a QR code", |e| match e {
                UiEvent::LoginStep { account: a, step } if *a == account => match &step.kind {
                    LoginStepKind::ShowQr { data, .. } => Some(data.expose().clone()),
                    LoginStepKind::Failed { error } => panic!("login failed: {error:?}"),
                    _ => None,
                },
                _ => None,
            })
            .await;
        assert!(url.starts_with("sgnl://linkdevice?uuid="), "QR URL");
        (account, url)
    }

    /// Links a new Signal account to our phone through the facade, the way the link page does.
    pub async fn link(&mut self, world: &World) -> (AccountId, DeviceInfo) {
        let (account, url) = self.begin_link().await;

        // The phone scans the code; the login runs to `Done`.
        let (device, ()) = tokio::join!(
            world
                .harness
                .complete_provision(&url, &world.phone.device_ref, WAIT),
            self.event("Done", |e| match e {
                UiEvent::LoginStep { account: a, step } if *a == account => match &step.kind {
                    LoginStepKind::Done => Some(()),
                    LoginStepKind::Failed { error } => panic!("login failed: {error:?}"),
                    _ => None,
                },
                _ => None,
            })
        );
        let device = device.expect("completeProvision");
        assert_eq!(device.aci, world.phone.aci);
        self.wait_online(account).await;
        let row = self.account(account).await.expect("account row");
        assert_eq!(row.network, NetworkKind::Signal);
        assert_eq!(row.own_device, Some(device.device_id));
        // The link's sync requests reached the phone (§6.5 step 8).
        world
            .harness
            .wait_for_sync(&world.phone.device_ref, &device.device_ref, WAIT)
            .await
            .expect("sync requests reached the phone");
        (account, device)
    }
}

/// The name libtest knows a test by in the merged binary `tests/it.rs`: the tests that start
/// this binary again do it with `--exact <name>`. `module` is `module_path!()` of the calling
/// file (`it::groups`), `test` the function (`child_actor_process`); the result is
/// `groups::child_actor_process`. The crate's own name is not part of a libtest name.
pub fn child_filter(module: &str, test: &str) -> String {
    match module.split_once("::") {
        Some((_crate, path)) => format!("{path}::{test}"),
        None => test.to_owned(),
    }
}

/// Set in the child of [`in_own_process!`].
const OWN_PROCESS: &str = "KOURIER_OWN_PROCESS";

/// The libtest name of the test inside which an item was declared, from the item's
/// `type_name`: `it::robustness::a_test::{{closure}}::here` (the body of a tokio test is a
/// closure) is `robustness::a_test`. The crate (`it`, the merged binary) and the item itself
/// are not part of a libtest name.
pub fn test_name_of(item_path: &str) -> String {
    let mut parts: Vec<&str> = item_path.split("::").collect();
    parts.pop();
    if !parts.is_empty() {
        parts.remove(0);
    }
    parts.retain(|part| *part != "{{closure}}");
    parts.join("::")
}

/// The libtest name of the test that calls this (`robustness::a_test`), found from the path of
/// an item declared in its body: no string to keep in step with the function's name.
macro_rules! test_name {
    () => {{
        fn here() {}
        $crate::support::test_name_of(std::any::type_name_of_val(&here))
    }};
}
pub(crate) use test_name;

/// For a test that reads a process-wide log or needs anything else a second test of the
/// process would disturb; the first statement of the test:
/// `if support::in_own_process!() { return; }`. Under nextest every test is a process of its
/// own (`NEXTEST` is set) and this is `false`: run the body. Under `cargo test` the tests of this
/// binary are threads of ONE process (and the first global tracing subscriber wins): the call
/// starts the binary again for this test alone ([`run_in_child`]), is `true` once that passed
/// (the caller returns) and panics with the child's output when it failed; in the child it is
/// `false`. The name of the test comes from `test_name!`, so renaming the function cannot
/// leave the child running another (or no) test.
macro_rules! in_own_process {
    () => {
        $crate::support::in_own_process_named(&$crate::support::test_name!())
    };
}
pub(crate) use in_own_process;

/// What [`in_own_process!`] does with the name it found.
pub fn in_own_process_named(name: &str) -> bool {
    if std::env::var_os("NEXTEST").is_some() || std::env::var_os(OWN_PROCESS).is_some() {
        return false;
    }
    if let Err(why) = run_in_child(name) {
        panic!("{name} failed in a process of its own: {why}");
    }
    true
}

/// Starts this binary again for the one test `name` (`--exact`); the reason when it did not
/// pass exactly that test.
pub fn run_in_child(name: &str) -> Result<(), String> {
    let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .arg("--exact")
        .arg(name)
        .args(["--nocapture", "--test-threads=1"])
        .env(OWN_PROCESS, "1")
        .output()
        .expect("spawn the test again");
    let stdout = String::from_utf8_lossy(&out.stdout);
    child_verdict(out.status.success(), &stdout).map_err(|why| {
        format!(
            "{why} ({:?}):\n{stdout}{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// What the child's exit status and output say. A name that matches no test makes libtest exit
/// 0 after "running 0 tests"; that is no pass here (T3 review F3).
fn child_verdict(success: bool, stdout: &str) -> Result<(), &'static str> {
    if !success {
        return Err("the child failed");
    }
    if !stdout.contains("test result: ok. 1 passed;") {
        return Err("the child did not pass exactly one test (a name that matches no test?)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{child_filter, child_verdict, run_in_child, test_name_of};

    #[test]
    fn a_child_is_named_with_its_module() {
        assert_eq!(
            child_filter("it::groups", "child_actor_process"),
            "groups::child_actor_process"
        );
        assert_eq!(child_filter("it", "x"), "x");
    }

    #[test]
    fn a_test_finds_its_own_name() {
        assert_eq!(
            super::test_name!(),
            "support::tests::a_test_finds_its_own_name"
        );
        // A tokio test's body is a closure; a test of the binary's root has no module.
        assert_eq!(
            test_name_of("it::robustness::a_test::{{closure}}::here"),
            "robustness::a_test"
        );
        assert_eq!(test_name_of("it::a_root_test::here"), "a_root_test");
    }

    #[test]
    fn a_child_that_ran_no_test_is_not_a_pass() {
        // libtest exits 0 after "running 0 tests" for a name that matches nothing.
        let why = run_in_child("support::tests::there_is_no_such_test").expect_err("no pass");
        assert!(why.contains("exactly one test"), "{why}");
        let why = run_in_child("there_is_no_such_test").expect_err("no pass");
        assert!(why.contains("exactly one test"), "{why}");
    }

    #[test]
    fn a_child_that_passed_one_test_is_a_pass() {
        run_in_child("support::tests::a_test_that_passes").expect("a pass");
    }

    #[test]
    fn a_test_that_passes() {}

    #[test]
    fn the_verdict_wants_exactly_one_passed_test() {
        let one = "running 1 test\ntest x ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored";
        let none = "running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored";
        assert_eq!(child_verdict(true, one), Ok(()));
        assert!(child_verdict(true, none).is_err());
        assert!(child_verdict(false, one).is_err());
        assert!(child_verdict(true, "").is_err());
    }
}

//! THE OFFLINE FAKE of Threema's mediator (D2M) and of the chat server (CSP inside the mediator's
//! Proxy payloads), TH2; the rendezvous relay ([`relay`]), the scripted existing device of a join
//! ([`joiner`]) and its `EssentialData` ([`essential`]), TH4 J2. Test-only by construction: it
//! lives in the one test binary `tests/it.rs`.
//!
//! - Sans-I/O: the servers take bytes and answer with actions; the client (libthreema's own
//!   protocol code) and the fake run in one thread, with no runtime and no socket. The async shell
//!   over `async_channel` that implements the crate's `WsConnector` is [`connector`]; the script
//!   layer (imperative calls and the small declarative [`script::Script`]) is [`script`].
//! - INDEPENDENT of libthreema (lane plan §2.4): this directory imports from libthreema only its
//!   generated protobuf types (the spec's `.proto` files); its crypto is its own
//!   ([`crypto`], registry crates). `tests/builder.rs` rule 9 holds both, and that nothing here
//!   names a socket.
//! - TH5 A2 adds the PHONE's builders for the follower receive tests: [`body`] (CSP E2E message bodies,
//!   from `TP/csp.struct.yml` and `TP/csp-e2e.proto`) and [`envelope`] (`d2d.Envelope`s of every kind,
//!   the sync deltas, and the helper that seals one with the fake's DGRK and reflects it through a
//!   [`phone::Phone`]); the mediator's `reflect_during_join` knob and `on_reflected_ack` hook are in [`d2m`].
//! - TH5a A3 adds the PARENT-RESIDENT fake ([`pipe`]): the fake stays in the parent test process and
//!   an actor in an `unshare -rn` child reaches it over two inherited pipes (fds 3 and 4), so a kill
//!   keeps the fake's state; a child's EOF is a connection LOST with 1006 (`Mediator::lost`). The
//!   kill-test harness over it is `tests/support/piped.rs`.
//! - TH6a S3 adds the CSP PEER ([`peer`]): a receiver written from the spec that opens what the client sent
//!   (`Mediator::csp_sent()`) with the fake's own crypto and applies the receiver's rules in the spec's
//!   order (nonce used, metadata id, message id received); the knobs `CspKnobs::{close_on_box_to,
//!   withhold_message_ack_of, drop_repeats}`, `Knobs::withhold_reflect_acks` with
//!   `Mediator::{release_reflect_acks, reflects, release_message_acks}` and `Net::stall_mediator_sends`.
//! - The fake is NOT more forgiving than the real server: what the client does wrong is recorded in
//!   [`Violations`] and every mock test ENDS (its last statement) with
//!   `world.violations().assert_empty()` or a comparison of `violations().rules()`; the sweep
//!   `mock::every_server_test_checks_the_violations` holds it.
//! - Every behaviour the spec does not fix is marked `// [U: real server]` (the list for RT1 is in
//!   `target/threema/th2-build.md`).
//! - Nothing here writes a key, a Threema ID or a message text: the trace and the violations carry
//!   frame types, rule names and numeric ids only, by the types of their arguments.

pub mod body;
pub mod connector;
pub mod crypto;
pub mod csp;
pub mod d2m;
pub mod desktop_vectors;
pub mod envelope;
pub mod essential;
pub mod joiner;
pub mod peer;
pub mod phone;
pub mod pipe;
pub mod relay;
pub mod script;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{RngCore as _, SeedableRng as _};

use connector::Net;
pub use connector::parts;
use crypto::KeyPair;
use d2m::Mediator;
use relay::Relay;

/// Where every world's virtual clock starts (ms since the Unix epoch, 2025-10-09).
pub const START_MS: u64 = 1_760_000_000_000;

/// Locks a mutex of the fake; a test that panicked while holding it has failed already.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The virtual clock of a world: moves only when a test moves it (no wall clock in the fake).
#[derive(Clone)]
pub struct Clock(Arc<AtomicU64>);

impl Clock {
    fn new() -> Self {
        Self(Arc::new(AtomicU64::new(START_MS)))
    }

    /// Now, in ms since the Unix epoch.
    pub fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    pub fn advance(&self, by: Duration) {
        let ms = u64::try_from(by.as_millis()).expect("a test's step fits u64 ms");
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

/// One protocol rule the client broke, with numeric ids only (a device id, a reflect id).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    pub rule: &'static str,
    pub ids: Vec<(&'static str, u64)>,
}

/// The client's breaches the fake saw (S-mock §9): asserted empty at the end of every mock test.
#[derive(Clone, Default)]
pub struct Violations(Arc<Mutex<Vec<Violation>>>);

impl Violations {
    pub fn record(&self, rule: &'static str, ids: &[(&'static str, u64)]) {
        lock(&self.0).push(Violation {
            rule,
            ids: ids.to_vec(),
        });
    }

    pub fn list(&self) -> Vec<Violation> {
        lock(&self.0).clone()
    }

    /// The rule names, in the order they were broken.
    pub fn rules(&self) -> Vec<&'static str> {
        lock(&self.0).iter().map(|v| v.rule).collect()
    }

    /// Fails the test with a sentence naming the rules (never a key or a text).
    #[track_caller]
    pub fn assert_empty(&self) {
        let rules = self.rules();
        assert!(
            rules.is_empty(),
            "the client broke {} rule(s) of the fake: {rules:?}",
            rules.len()
        );
    }
}

/// Which way a traced frame went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// From the client to the fake.
    In,
    /// From the fake to the client.
    Out,
}

#[derive(Default)]
struct TraceInner {
    lines: Vec<String>,
    file: Option<File>,
}

/// One synthetic line per frame (`<n> <connection> <in|out> <type> [<name>=<number> ...]`): types
/// and numeric ids only. With a file (the kill tests, TH5) every line is written and fsynced before
/// the frame goes on, so what a killed process left behind is on disk.
#[derive(Clone, Default)]
pub struct Trace(Arc<Mutex<TraceInner>>);

impl Trace {
    /// Writes every later line also to `path` (created or appended to), fsynced per line.
    pub fn to_file(&self, path: &Path) -> std::io::Result<()> {
        let file = File::options().create(true).append(true).open(path)?;
        lock(&self.0).file = Some(file);
        Ok(())
    }

    pub fn frame(
        &self,
        dir: Dir,
        connection: u64,
        kind: &'static str,
        ids: &[(&'static str, u64)],
    ) {
        let mut inner = lock(&self.0);
        let mut line = format!(
            "{} {connection} {} {kind}",
            inner.lines.len() + 1,
            match dir {
                Dir::In => "in",
                Dir::Out => "out",
            }
        );
        for (name, value) in ids {
            line.push_str(&format!(" {name}={value}"));
        }
        if let Some(file) = inner.file.as_mut() {
            writeln!(file, "{line}").expect("write the trace");
            file.sync_data().expect("fsync the trace");
        }
        inner.lines.push(line);
    }

    pub fn lines(&self) -> Vec<String> {
        lock(&self.0).lines.clone()
    }
}

/// A Threema identity of a test: its ID and its client key pair (CK). Debug prints neither.
#[derive(Clone)]
pub struct TestIdentity {
    pub id: String,
    pub ck: KeyPair,
}

impl std::fmt::Debug for TestIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TestIdentity")
    }
}

/// Whether `id` has the form of a Threema ID: eight of `A-Z`, `0-9` and `*` (the gateway prefix).
pub fn is_threema_id(id: &str) -> bool {
    id.len() == 8
        && id
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'*')
}

/// The world's identity table, shared with the chat server (who may log in, with which CK).
pub type Identities = Arc<Mutex<BTreeMap<String, TestIdentity>>>;

/// One world of the fake: a seeded RNG (a failure is reproducible: ids and nonces differ per
/// connection, not per run), the virtual clock, the identity table, the chat server's permanent key
/// pair (SK), the violations and the trace.
pub struct MockWorld {
    seed: u64,
    rng: Mutex<ChaCha20Rng>,
    clock: Clock,
    identities: Identities,
    server_key: KeyPair,
    violations: Violations,
    trace: Trace,
    mediator: Mediator,
    relay: Relay,
    net: Net,
}

impl std::fmt::Debug for MockWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockWorld")
            .field("seed", &self.seed)
            .finish()
    }
}

/// A new world from `seed`.
pub fn world(seed: u64) -> MockWorld {
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    let mut sk = [0u8; 32];
    rng.fill_bytes(&mut sk);
    let (clock, violations, trace) = (Clock::new(), Violations::default(), Trace::default());
    let server_key = KeyPair::from_secret(sk);
    let identities = Identities::default();
    let mediator = Mediator::new(
        clock.clone(),
        violations.clone(),
        trace.clone(),
        rng.next_u64(),
        server_key.clone(),
        identities.clone(),
    );
    let relay = Relay::new(clock.clone(), violations.clone(), trace.clone());
    let net = Net::new(mediator.clone(), relay.clone(), violations.clone());
    MockWorld {
        seed,
        rng: Mutex::new(rng),
        clock,
        identities,
        server_key,
        violations,
        trace,
        mediator,
        relay,
        net,
    }
}

impl MockWorld {
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// `N` bytes from the world's RNG.
    pub fn random<const N: usize>(&self) -> [u8; N] {
        let mut out = [0u8; N];
        lock(&self.rng).fill_bytes(&mut out);
        out
    }

    pub fn random_u64(&self) -> u64 {
        lock(&self.rng).next_u64()
    }

    /// A fresh device group key (the test side's: the mediator never sees it).
    pub fn new_device_group_key(&self) -> [u8; 32] {
        self.random()
    }

    /// A fresh X25519 key pair from the RNG.
    pub fn new_key_pair(&self) -> KeyPair {
        KeyPair::from_secret(self.random())
    }

    /// Adds `id` with a fresh CK pair to the identity table (the chat server's directory of who
    /// may log in) and returns it. An ID of the wrong form or one that is there already fails the
    /// test.
    #[track_caller]
    pub fn add_identity(&self, id: &str) -> TestIdentity {
        assert!(
            is_threema_id(id),
            "a test identity must look like a Threema ID"
        );
        let identity = TestIdentity {
            id: id.to_owned(),
            ck: self.new_key_pair(),
        };
        let fresh = lock(&self.identities)
            .insert(id.to_owned(), identity.clone())
            .is_none();
        assert!(fresh, "a test identity was added twice");
        identity
    }

    /// The identity `id`, if the table holds it.
    pub fn identity(&self, id: &str) -> Option<TestIdentity> {
        lock(&self.identities).get(id).cloned()
    }

    /// How many identities the table holds.
    pub fn identity_count(&self) -> usize {
        lock(&self.identities).len()
    }

    /// The chat server's permanent key pair (SK); its public half is what the client is configured
    /// with (`ServerParams::chat_server_keys`).
    pub fn server_key(&self) -> &KeyPair {
        &self.server_key
    }

    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    pub fn violations(&self) -> &Violations {
        &self.violations
    }

    pub fn trace(&self) -> &Trace {
        &self.trace
    }

    /// The world's mediator (every device group and connection of the world).
    pub fn mediator(&self) -> &Mediator {
        &self.mediator
    }

    /// The world's rendezvous relay (TH4 J2).
    pub fn relay(&self) -> &Relay {
        &self.relay
    }

    /// Moves the virtual clock by `by` and runs every timer of the fake (the mediator's, the
    /// relay's init timeout, the scripted joiners'), without tokio's clock: the sans-I/O form of
    /// [`connector::Net::advance`].
    pub fn advance(&self, by: Duration) {
        self.net.advance_virtual(by);
    }
}

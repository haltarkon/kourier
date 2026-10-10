//! The reference profile of the quality targets (architecture §1.3), for `cargo xtask perf`.
//! Only in builds with the cargo feature `perf` (which implies `dumpable` and `fake`); never in
//! a build a user runs.
//!
//! [`Launch::Perf`](crate::Launch) opens a profile below a directory of its own: a passphrase
//! keyring with a fixed passphrase (nothing in it is secret; the Secret Service is not used),
//! only the fake network, the system clock. An empty profile is seeded first: the fake
//! account is linked and delivers about 200 conversations with 50,000 messages, a share of
//! them pictures and files that the core downloads through its attachment pipeline, exactly
//! as it does for a real backend. Later starts find the account in `core.db` and replay
//! nothing (the fake's script cursor is durable), so they measure a normal start. They do not
//! even build the reference script again: it would cost a tenth of a second and tens of
//! megabytes that a real start does not have. The seeded profile remembers how many steps the
//! script had (`perf-reference.json`), and a later start runs a script of that many empty
//! steps, which the cursor is already past.
//!
//! A start may ask for one live message a while after the UI is ready
//! ([`PerfOptions::live_after`]): the script then ends with a sleep and one more delivery, and
//! the clock ([`PerfClock`]) notes the instant that sleep ends as the envelope's arrival, the
//! start of the "receive → notification" time.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kourier_backend_api::{BackendEvent, Clock, Sleep, SystemClock};
use kourier_core::facade::{CoreHandle, Query, QueryData, UiCommand};
use kourier_fake::attachments::{synthetic_file, synthetic_image};
use kourier_fake::fixture::{FixtureSpec, Rng, fixture};
use kourier_fake::{FakeNetwork, Flavor, Script, Step};
use kourier_model::{Body, ConvKey, NetworkKind, Text};

use crate::app::AppError;
use crate::keys::{KdfParams, KeyError, Keyring, NoSecretStore, PassphraseConfig};
use crate::paths::{HOME_ENV, Paths, Profile};

/// The size and make-up of the reference profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PerfSpec {
    /// Contacts, each with a direct chat.
    pub contacts: usize,
    /// Groups.
    pub groups: usize,
    /// Messages per conversation.
    pub messages_per_conversation: usize,
    /// Share of messages that carry a picture or a file, in percent.
    pub media_percent: u8,
}

impl Default for PerfSpec {
    /// 200 conversations and 50,000 messages, 8 % of them with a picture or a file (§1.3).
    fn default() -> Self {
        Self {
            contacts: 180,
            groups: 20,
            messages_per_conversation: 250,
            media_percent: 8,
        }
    }
}

impl PerfSpec {
    /// Conversations in the profile.
    pub fn conversations(&self) -> usize {
        self.contacts + self.groups
    }

    /// Messages in the profile.
    pub fn messages(&self) -> usize {
        self.conversations() * self.messages_per_conversation
    }
}

/// How a perf launch starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PerfOptions {
    /// The profile's root (laid out like `KOURIER_HOME`).
    pub home: PathBuf,
    /// What an empty profile is seeded with. Must be the same at every start of a profile:
    /// the fake continues its script by position.
    pub spec: PerfSpec,
    /// Deliver one more message this long after the backend is online (the notification
    /// measurement). Works once per copy of a profile.
    pub live_after: Option<Duration>,
}

/// What seeding an empty profile took, for the "catch-up ingest" row of the perf table.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeedReport {
    /// Messages delivered.
    pub messages: usize,
    /// Events delivered (messages, send states, receipts, reactions, contacts, …).
    pub events: usize,
    /// From the link command until the core had acked every delivery.
    pub ingest: Duration,
    /// Attachment files on disk once the downloads settled.
    pub files: usize,
    /// From the last ack until the downloads settled.
    pub downloads: Duration,
}

/// Share of the media messages that are files instead of pictures, in percent.
const FILE_PERCENT: usize = 15;
/// Share of the media messages that keep their text as a caption, in percent.
const CAPTION_PERCENT: usize = 30;
/// Share of pictures in the image-heavy conversation (the last group), in percent.
const HEAVY_PERCENT: usize = 45;
/// The newest messages of a conversation stay texts: the fixture edits, deletes and reacts
/// to them.
const KEEP_NEWEST_TEXTS: usize = 8;
/// The seed of the reference data: another seed is another profile.
const SEED: u64 = 16;

/// Picture sizes of the reference profile (width, height), small on purpose: 4,000 pictures
/// must not need gigabytes.
const PICTURES: [(u32, u32); 5] = [(320, 240), (240, 320), (480, 360), (360, 480), (640, 480)];

/// The fixed passphrase: a perf profile is throwaway test data.
const PASSPHRASE: &[u8] = b"kourier perf profile";
/// The keyring's settings file, below the profile's config directory.
const KEYRING_FILE: &str = "perf-keyring.json";
/// Cheap Argon2id settings (nothing here is secret, and a start must not measure the KDF of a
/// passphrase the real app never asks for on a Secret Service profile).
const KDF: KdfParams = KdfParams {
    m_kib: 64,
    t: 1,
    p: 1,
};

/// How long seeding may take before it counts as failed.
const SEED_TIMEOUT: Duration = Duration::from_secs(1800);
/// How long the downloads may take to settle.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(1800);

fn fixture_error(e: impl std::fmt::Display) -> AppError {
    AppError::Fixture(e.to_string())
}

/// The profile's directories.
pub(crate) fn paths(opts: &PerfOptions) -> Result<Paths, AppError> {
    let home = opts.home.as_os_str().to_owned();
    Ok(Paths::from_env(Profile::default(), |k| {
        (k == HOME_ENV).then(|| home.clone())
    })?)
}

/// Whether the profile has no data yet (it is seeded at this start).
pub(crate) fn is_fresh(opts: &PerfOptions) -> bool {
    paths(opts).is_ok_and(|p| !p.has_data())
}

/// Opens the profile's keyring, creating it for a new profile.
pub(crate) fn open_keyring(config_dir: &Path) -> Result<Keyring<NoSecretStore>, KeyError> {
    let invalid = |e: serde_json::Error| {
        KeyError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    };
    let file = config_dir.join(KEYRING_FILE);
    match std::fs::read(&file) {
        Ok(bytes) => {
            let config: PassphraseConfig = serde_json::from_slice(&bytes).map_err(invalid)?;
            Keyring::with_passphrase(PASSPHRASE, &config)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let (keyring, config) = Keyring::create_passphrase(PASSPHRASE, KDF)?;
            let json = serde_json::to_vec(&config).map_err(invalid)?;
            crate::paths::create_private_dir(config_dir)?;
            let mut f = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&file)?;
            f.write_all(&json)?;
            f.sync_all()?;
            Ok(keyring)
        }
        Err(e) => Err(KeyError::Io(e)),
    }
}

/// The conversation of a delivered message, when the event is one.
fn message_conv(e: &BackendEvent) -> Option<&ConvKey> {
    match e {
        BackendEvent::Message(m) => Some(&m.conv),
        _ => None,
    }
}

/// Counts the messages and all events a script delivers.
pub(crate) fn count(script: &Script) -> (usize, usize) {
    let mut messages = 0;
    let mut events = 0;
    for step in &script.steps {
        if let Step::Deliver(d) = step {
            events += d.events.len();
            messages += d
                .events
                .iter()
                .filter(|e| matches!(e, BackendEvent::Message(_)))
                .count();
        }
    }
    (messages, events)
}

/// Turns a share of the script's text messages into pictures and files: `media_percent` of
/// every conversation, [`HEAVY_PERCENT`] of the last one (the image-heavy history the scroll
/// measurement flicks through). Deterministic: the same script and spec give the same result.
fn with_bulk_media(mut script: Script, spec: &PerfSpec) -> Script {
    // Conversations are generated one after the other: the last message belongs to the last.
    let heavy: Option<ConvKey> = script
        .steps
        .iter()
        .rev()
        .find_map(|s| match s {
            Step::Deliver(d) => d.events.iter().rev().find_map(message_conv),
            _ => None,
        })
        .cloned();
    let per_conv = spec.messages_per_conversation;
    let mut rng = Rng::new(SEED ^ 0x006d_6564_6961);
    let mut current: Option<ConvKey> = None;
    let mut index = 0usize;
    let mut serial = 0u64;
    for step in &mut script.steps {
        let Step::Deliver(delivery) = step else {
            continue;
        };
        for event in &mut delivery.events {
            let BackendEvent::Message(m) = event else {
                continue;
            };
            if current.as_ref() != Some(&m.conv) {
                current = Some(m.conv.clone());
                index = 0;
            }
            let position = index;
            index += 1;
            let percent = if heavy.as_ref() == Some(&m.conv) {
                HEAVY_PERCENT
            } else {
                usize::from(spec.media_percent)
            };
            // One draw per message whatever happens to it: the stream stays aligned.
            let roll = rng.below(100);
            let kind = rng.below(100);
            let caption = rng.below(100);
            let size = rng.below(PICTURES.len());
            if roll >= percent || position + KEEP_NEWEST_TEXTS >= per_conv {
                continue;
            }
            let Body::Text(text) = &m.body else {
                continue;
            };
            let text: Text = text.clone();
            serial += 1;
            let attachment = if kind < FILE_PERCENT && heavy.as_ref() != Some(&m.conv) {
                let len = 4 * 1024 + u64::try_from(size).unwrap_or(0) * 12 * 1024;
                synthetic_file(
                    &format!("document-{serial}.pdf"),
                    "application/pdf",
                    len,
                    SEED ^ serial,
                )
            } else {
                let (w, h) = PICTURES.get(size).copied().unwrap_or((320, 240));
                synthetic_image(w, h, SEED ^ serial)
            };
            m.body = Body::Media {
                attachments: vec![attachment],
                caption: (caption < CAPTION_PERCENT).then_some(text),
            };
        }
    }
    script
}

/// How long before "now" the newest message of the reference profile was sent.
const NEWEST_BEFORE_NOW_MS: i64 = 10 * 60_000;

/// The newest message of a script (ms).
fn newest_message(script: &Script) -> Option<i64> {
    script
        .steps
        .iter()
        .filter_map(|s| match s {
            Step::Deliver(d) => Some(d.events.iter()),
            _ => None,
        })
        .flatten()
        .filter_map(|e| match e {
            BackendEvent::Message(m) => Some(m.sent_at),
            _ => None,
        })
        .max()
}

/// The script of the reference profile: a history that ends ten minutes before `now_ms`
/// (50,000 messages span some four years), so dates, the order of the chats and a message
/// that arrives now are what they would be in a profile that was used until a moment ago.
pub(crate) fn reference_script(spec: &PerfSpec, now_ms: i64) -> Result<Script, AppError> {
    let generate = |start_ms: i64| {
        fixture(&FixtureSpec {
            seed: SEED,
            contacts: spec.contacts,
            groups: spec.groups,
            messages_per_conversation: spec.messages_per_conversation,
            start_ms,
            // The fixture's three photos are placed for screenshots; the bulk media replaces
            // them.
            images: false,
            modifiers: true,
            showcase: false,
            ..FixtureSpec::default()
        })
        .map_err(fixture_error)
    };
    // The same seed gives the same steps between messages wherever the history starts: one
    // pass measures how long it is, the second places its end.
    let span = newest_message(&generate(0)?).unwrap_or(0);
    let script = generate(now_ms - NEWEST_BEFORE_NOW_MS - span)?;
    Ok(with_bulk_media(script, spec))
}

/// The contact the live message comes from: the sixth of the fixture (its first four chats
/// are pinned, pinned, muted and archived, and every third has unread messages already).
fn live_peer() -> Option<kourier_model::UserId> {
    kourier_model::UserId::parse("aci:00000000-0000-4000-8000-000000f10006").ok()
}

/// Appends the live message: a sleep of exactly `after` (the clock recognizes it) and one text
/// in a plain direct chat, sent "now".
fn with_live_message(mut script: Script, after: Duration, now_ms: i64) -> Script {
    let Some(peer) = live_peer() else {
        return script;
    };
    let conv = ConvKey::Direct(peer.clone());
    let mut message = kourier_fake::scripts::remote(
        &conv,
        &peer,
        &now_ms.to_string(),
        now_ms,
        "A message that arrives while Kourier runs.",
    );
    message.received_at = now_ms;
    message.read = false;
    script.steps.push(Step::Sleep(after));
    script
        .steps
        .push(Step::Deliver(kourier_fake::Delivery::new(vec![
            BackendEvent::Message(message),
        ])));
    script
}

/// What a seeded profile remembers of the script that seeded it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Reference {
    /// What it was seeded with.
    pub(crate) spec: PerfSpec,
    /// Steps of the reference script (where the fake's cursor stands).
    pub(crate) steps: usize,
    /// Messages the script delivered.
    pub(crate) messages: usize,
    /// Events the script delivered.
    pub(crate) events: usize,
}

/// The file a seeded profile keeps its [`Reference`] in.
const REFERENCE_FILE: &str = "perf-reference.json";

impl Reference {
    fn file(home: &Path) -> PathBuf {
        home.join(REFERENCE_FILE)
    }

    /// Reads the reference of the profile below `home`.
    fn load(home: &Path) -> Result<Self, AppError> {
        let file = Self::file(home);
        let bytes = std::fs::read(&file).map_err(|_| {
            fixture_error(
                "the profile has data but was not seeded to the end; delete it and seed again",
            )
        })?;
        serde_json::from_slice(&bytes).map_err(fixture_error)
    }

    /// Writes the reference once the profile below `home` is seeded.
    fn save(&self, home: &Path) -> Result<(), AppError> {
        let json = serde_json::to_vec(self).map_err(fixture_error)?;
        std::fs::write(Self::file(home), json).map_err(AppError::Dirs)
    }
}

/// The script of a start on a seeded profile: the head of the reference script (the same
/// account) and as many empty steps as that script had, all behind the fake's cursor.
fn resumed_script(reference: &Reference) -> Result<Script, AppError> {
    let mut script = fixture(&FixtureSpec {
        seed: SEED,
        contacts: 0,
        groups: 0,
        messages_per_conversation: 0,
        images: false,
        modifiers: false,
        showcase: false,
        ..FixtureSpec::default()
    })
    .map_err(fixture_error)?;
    script.steps = vec![Step::WaitAcked; reference.steps];
    Ok(script)
}

/// The fake network of a perf launch.
pub(crate) struct PerfNetwork {
    /// The network.
    pub(crate) network: FakeNetwork,
    /// The reference to seed an empty profile with (`None` on a seeded one).
    pub(crate) seeding: Option<Reference>,
}

/// The fake network of a perf launch: the reference script for an empty profile; for a seeded
/// one a script the cursor is past, plus the live message when the launch asks for one.
pub(crate) fn network(opts: &PerfOptions, fresh: bool) -> Result<PerfNetwork, AppError> {
    if fresh {
        let now = i64::try_from(SystemClock.now_ms()).unwrap_or(i64::MAX);
        let script = reference_script(&opts.spec, now)?;
        let (messages, events) = count(&script);
        let reference = Reference {
            spec: opts.spec,
            steps: script.steps.len(),
            messages,
            events,
        };
        return Ok(PerfNetwork {
            network: FakeNetwork::new(Flavor::SignalLike, script),
            seeding: Some(reference),
        });
    }
    let reference = Reference::load(&opts.home)?;
    if reference.spec != opts.spec {
        return Err(fixture_error(
            "the profile was seeded with another size; delete it or give that size",
        ));
    }
    let mut script = resumed_script(&reference)?;
    if let Some(after) = opts.live_after {
        let now = i64::try_from(SystemClock.now_ms()).unwrap_or(i64::MAX);
        script = with_live_message(script, after, now);
    }
    Ok(PerfNetwork {
        network: FakeNetwork::new(Flavor::SignalLike, script),
        seeding: None,
    })
}

/// The system clock, which also notes when the sleep before the live message ends
/// ([`PerfOptions::live_after`]): that instant stands for the envelope's arrival
/// ([`crate::trace::envelope_arrived`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PerfClock {
    /// The length of the marked sleep (ms).
    mark_ms: Option<u64>,
}

impl PerfClock {
    pub(crate) fn new(live_after: Option<Duration>) -> Self {
        Self {
            mark_ms: live_after.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        }
    }

    /// Whether a sleep of `wait_ms` is the marked one (the fake computes the deadline a few
    /// instructions before the clock sees it, so a millisecond may have passed).
    fn is_marked(&self, wait_ms: u64) -> bool {
        self.mark_ms
            .is_some_and(|mark| wait_ms <= mark && wait_ms + 3 >= mark)
    }
}

impl Clock for PerfClock {
    fn now_ms(&self) -> u64 {
        SystemClock.now_ms()
    }

    fn sleep_until(&self, deadline_ms: u64) -> Sleep {
        let marked = self.is_marked(deadline_ms.saturating_sub(self.now_ms()));
        let sleep = SystemClock.sleep_until(deadline_ms);
        if !marked {
            return sleep;
        }
        Box::pin(async move {
            sleep.await;
            crate::trace::envelope_arrived();
        })
    }
}

/// The clock of a perf launch.
pub(crate) fn clock(opts: &PerfOptions) -> Arc<dyn Clock> {
    Arc::new(PerfClock::new(opts.live_after))
}

fn count_files(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| {
            let path = e.path();
            if path.is_dir() { count_files(&path) } else { 1 }
        })
        .sum()
}

/// Links the fake account of an empty profile and waits until the core has everything and the
/// attachment downloads have settled; then the profile below `home` is marked as seeded.
pub(crate) async fn seed(
    core: &CoreHandle,
    fake: &FakeNetwork,
    paths: &Paths,
    reference: &Reference,
    home: &Path,
) -> Result<SeedReport, AppError> {
    let (messages, events) = (reference.messages, reference.events);
    let control = fake.control();
    let started = Instant::now();
    core.send(UiCommand::AddAccount {
        network: NetworkKind::Fake,
        flow: "qr".into(),
    })?;
    tokio::time::timeout(SEED_TIMEOUT, control.wait_for(|c| !c.finished().is_empty()))
        .await
        .map_err(|_| fixture_error("the reference data did not arrive in time"))?;
    let ingest = started.elapsed();
    // The pictures and files are downloaded by the core's own policy after their messages
    // are stored: wait until no new file has appeared for three seconds.
    let blobs = match core.query(Query::Accounts).await? {
        QueryData::Accounts(accounts) => accounts.first().map(|a| paths.blobs_dir(&a.uuid)),
        _ => None,
    }
    .ok_or_else(|| fixture_error("the reference account is missing"))?;
    let settle = Instant::now();
    let mut last_change = Instant::now();
    let mut files = count_files(&blobs);
    while last_change.elapsed() < Duration::from_secs(3) {
        if settle.elapsed() > DOWNLOAD_TIMEOUT {
            return Err(fixture_error("the attachment downloads did not settle"));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        let now = count_files(&blobs);
        if now != files {
            files = now;
            last_change = Instant::now();
        }
    }
    let downloads = last_change.duration_since(settle);
    reference.save(home)?;
    tracing::info!(
        messages,
        events,
        files,
        ms = ingest.as_millis(),
        "reference profile seeded"
    );
    Ok(SeedReport {
        messages,
        events,
        ingest,
        files,
        downloads,
    })
}

impl SeedReport {
    /// Writes the report as trace marks (`perf.seed`), whether tracing is on or not: a seeding
    /// run exists to print them.
    pub fn print(&self) {
        let secs = self.ingest.as_secs_f64().max(f64::EPSILON);
        #[allow(clippy::cast_precision_loss)]
        let line = crate::trace::format_line(
            "perf.seed",
            crate::trace::since_start_ms(),
            &[
                ("messages", self.messages as f64),
                ("events", self.events as f64),
                ("ingest_ms", self.ingest.as_secs_f64() * 1000.0),
                ("messages_per_s", self.messages as f64 / secs),
                ("events_per_s", self.events as f64 / secs),
                ("files", self.files as f64),
                ("downloads_ms", self.downloads.as_secs_f64() * 1000.0),
            ],
        );
        eprintln!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "Now" of the tests (ms).
    const NOW: i64 = 1_900_000_000_000;

    fn small() -> PerfSpec {
        PerfSpec {
            contacts: 6,
            groups: 2,
            messages_per_conversation: 60,
            media_percent: 10,
        }
    }

    fn media(script: &Script) -> (usize, usize, usize) {
        let mut pictures = 0;
        let mut files = 0;
        let mut texts = 0;
        for e in script.deliveries().iter().flatten() {
            if let BackendEvent::Message(m) = e {
                match &m.body {
                    Body::Media { attachments, .. } => {
                        for a in attachments {
                            if a.kind == kourier_model::AttachmentKind::Image {
                                pictures += 1;
                            } else {
                                files += 1;
                            }
                        }
                    }
                    _ => texts += 1,
                }
            }
        }
        (pictures, files, texts)
    }

    #[test]
    fn the_default_profile_is_the_one_of_the_quality_targets() {
        let spec = PerfSpec::default();
        assert_eq!(spec.conversations(), 200);
        assert_eq!(spec.messages(), 50_000);
    }

    #[test]
    fn the_reference_script_is_deterministic_and_has_its_share_of_media() {
        let spec = small();
        let a = reference_script(&spec, NOW).expect("script");
        let b = reference_script(&spec, NOW).expect("script");
        assert_eq!(a, b, "the same instant gives the same script");
        // The history ends ten minutes before "now", wherever "now" is.
        assert_eq!(newest_message(&a), Some(NOW - NEWEST_BEFORE_NOW_MS));
        let later = reference_script(&spec, NOW + 86_400_000).expect("script");
        assert_eq!(
            newest_message(&later),
            Some(NOW + 86_400_000 - NEWEST_BEFORE_NOW_MS)
        );
        assert_eq!(count(&later), count(&a));
        let (messages, events) = count(&a);
        assert_eq!(messages, spec.messages());
        assert!(events > messages, "send states and receipts come with them");
        let (pictures, files, texts) = media(&a);
        assert_eq!(pictures + files + texts, messages);
        // 7 conversations at 10 % and one at 45 %, minus the newest texts of each.
        assert!(
            (35..=110).contains(&(pictures + files)),
            "{pictures} + {files}"
        );
        assert!(pictures > files, "{pictures} pictures, {files} files");
        assert!(files > 0);
    }

    #[test]
    fn the_last_conversation_is_image_heavy_and_its_newest_messages_stay_texts() {
        let spec = small();
        let script = reference_script(&spec, NOW).expect("script");
        let all: Vec<&kourier_backend_api::IncomingMessage> = script
            .steps
            .iter()
            .filter_map(|s| match s {
                Step::Deliver(d) => Some(d.events.iter()),
                _ => None,
            })
            .flatten()
            .filter_map(|e| match e {
                BackendEvent::Message(m) => Some(m),
                _ => None,
            })
            .collect();
        let last = all.last().expect("messages").conv.clone();
        let heavy: Vec<_> = all.iter().filter(|m| m.conv == last).collect();
        assert_eq!(heavy.len(), spec.messages_per_conversation);
        let pictures = heavy
            .iter()
            .filter(|m| matches!(m.body, Body::Media { .. }))
            .count();
        assert!(pictures * 100 >= heavy.len() * 25, "{pictures} pictures");
        for m in &heavy[heavy.len() - KEEP_NEWEST_TEXTS..] {
            assert!(matches!(m.body, Body::Text(_)), "the fixture edits these");
        }
        assert!(
            matches!(last, ConvKey::Group(_)),
            "the last conversation is a group"
        );
    }

    #[test]
    fn the_live_message_follows_a_marked_sleep_in_a_plain_direct_chat() {
        let spec = small();
        let base = reference_script(&spec, NOW).expect("script");
        let after = Duration::from_millis(1777);
        let live = with_live_message(base.clone(), after, NOW);
        assert_eq!(live.steps.len(), base.steps.len() + 2);
        assert_eq!(live.steps[..base.steps.len()], base.steps[..]);
        assert_eq!(live.steps[base.steps.len()], Step::Sleep(after));
        let Some(Step::Deliver(d)) = live.steps.last() else {
            panic!("a delivery ends the script");
        };
        let [BackendEvent::Message(m)] = d.events.as_slice() else {
            panic!("one message");
        };
        assert!(!m.read && !m.catch_up && !m.silent, "it must notify");
        assert_eq!(m.sent_at, NOW);
        assert!(
            newest_message(&base) < Some(NOW),
            "it is the newest message"
        );
        assert_eq!(
            m.conv.direct_peer(),
            live_peer().as_ref(),
            "the sixth contact"
        );

        let clock = PerfClock::new(Some(after));
        assert!(clock.is_marked(1777) && clock.is_marked(1775));
        assert!(!clock.is_marked(1778) && !clock.is_marked(1770) && !clock.is_marked(250));
        assert!(!PerfClock::new(None).is_marked(1777));
    }

    #[test]
    fn a_seeded_profile_resumes_with_empty_steps_behind_the_cursor() {
        let tmp = tempfile::tempdir().expect("tmp");
        let mut opts = PerfOptions {
            home: tmp.path().to_path_buf(),
            spec: small(),
            live_after: Some(Duration::from_millis(1777)),
        };
        // An empty profile: the whole script, and what to remember of it. No live message
        // while seeding.
        let fresh = network(&opts, true).expect("fresh");
        let reference = fresh.seeding.expect("an empty profile is seeded");
        let full = reference_script(&opts.spec, NOW).expect("script");
        assert_eq!(reference.steps, full.steps.len());
        assert_eq!((reference.messages, reference.events), count(&full));
        assert_eq!(reference.spec, opts.spec);

        // Data without the reference file: seeding did not finish.
        assert!(network(&opts, false).is_err());
        reference.save(&opts.home).expect("save");
        assert_eq!(Reference::load(&opts.home).expect("load"), reference);

        // A seeded profile: the same account, nothing to deliver but the live message.
        let resumed = resumed_script(&reference).expect("resumed");
        assert_eq!(resumed.own_ids, full.own_ids);
        assert_eq!(resumed.display_name, full.display_name);
        assert_eq!(resumed.login, full.login);
        assert_eq!(resumed.steps.len(), full.steps.len());
        assert_eq!(count(&resumed), (0, 0));
        assert!(network(&opts, false).expect("seeded").seeding.is_none());

        // Another size than the one that seeded it is refused.
        opts.spec.media_percent = 50;
        assert!(network(&opts, false).is_err());
    }

    #[test]
    fn the_keyring_reopens_with_the_same_key() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path().join("config");
        let first = open_keyring(&dir).expect("create");
        let again = open_keyring(&dir).expect("reopen");
        assert_eq!(*first.core_db_key(), *again.core_db_key());
    }

    #[test]
    fn a_profile_is_fresh_until_it_has_data() {
        let tmp = tempfile::tempdir().expect("tmp");
        let opts = PerfOptions {
            home: tmp.path().to_path_buf(),
            spec: small(),
            live_after: None,
        };
        assert!(is_fresh(&opts));
        let paths = paths(&opts).expect("paths");
        paths.create_dirs().expect("dirs");
        assert!(is_fresh(&opts), "empty directories are no data");
        std::fs::write(paths.core_db(), b"x").expect("write");
        assert!(!is_fresh(&opts));
    }
}

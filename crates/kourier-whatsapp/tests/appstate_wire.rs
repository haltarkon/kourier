//! W6a1 A1, THE APP-STATE WIRE PROBE (verdict in `target/wa/w6/scout-appstate.md`): what upstream's
//! app-state sync hands Kourier's ONE handler at the pin, over the scripted server (the probe was written before
//! W6a1 mapped any of it; the A8 cases at the end of the file run Kourier's side). A real client, built by Kourier's
//! own `build_bot` over the account's `KourierStore`, holds a sync key (put into the store before the start); the
//! scripted server answers `w:sync:app:state` with patches a PHONE side built with upstream's own
//! `AppStateProcessor::build_patch` over the same key (MACs on, the client checks every patch MAC and the LTHash);
//! a raw handler of the TEST, registered right after Kourier's (`wire::parts_observing`), records what the bus hands every raw handler (the kinds and the
//! fields W6a1 maps: `from_full_sync`, `action_timestamp`, the call record's `start_time`, `from_me`,
//! participants), never a JID: the chat and the creator are recorded as "ours" / "the peer" / "us".
//!
//! The paths, one connection, in order:
//! 0. the FIRST BURST: what the client asks of app state between `<success>` and `Online` when nobody
//!    pushes anything (a resumed account whose device has a push name);
//! 1. a `server_sync` for three collections (two bootstrapped at version 0 before the start, `regular`
//!    never synced): the live path;
//! 2. `resync_app_state(Snapshot)` of the three: the rebuild Kourier would ask (D2);
//! 3. a Snapshot the server answers with an EMPTY collection (H1's "replayed nothing");
//! 4. a Snapshot the server answers 404 (M7's fatal answer);
//! 5. a `server_sync` after 3 and 4 (what the next server-raised sync of those collections is);
//! 6. a `server_sync` the server answers 404 (what upstream reports for a fatal it raised itself).
//!
//! After each path the books of the account's store: the stored version, `bootstrapped`, the dirty flag.
//!
//! The facts are printed as `probe:` lines (kinds, counts, flags, the synthetic times; never a JID, a
//! number or a name), and each path's expectation is checked; the run ends with every mismatch listed.
//! Every body runs in ONE offline child under `unshare -rn`; nothing here touches a network.

use std::collections::HashMap;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kourier_backend_api::{BackendEvent, Command, Exit, StartMode};
use kourier_db::rusqlite::params;
use kourier_model::{CallLine, ConnState, ConvKey, Patch, UserId};
use kourier_whatsapp::{StoreOwner, failpoints as fp, fault};
use wacore::appstate::hash::HashState;
use wacore::appstate::{encode_record, expand_app_state_keys};
use wacore::appstate_sync::AppStateProcessor;
use wacore::runtime::{AbortHandle, BoxFuture};
use wacore::store::traits::{AppStateSyncKey, AppSyncStore, Backend};
use wacore::types::events::{Event, EventHandler};
use wacore_binary::Node;
use wacore_binary::builder::NodeBuilder;
use waproto::buffa::MessageField;
use waproto::whatsapp as wa;
use whatsapp_rust::{AppStateResyncMode, WAPatchName};

use crate::actor_real::{Running, record, wait_for_states};
use crate::receive::{account_dir, count, raw};
use crate::support::capture::Capture;
use crate::support::peer;
use crate::support::wire::{self, Answer};
use crate::support::{self, db_path, key};
use crate::wire::{start_actor_handing_client, start_actor_observing};
use crate::wire_kill::copy_dir;

/// The sync key's id and bytes (synthetic; the phone side and the account share it).
const KEY_ID: &[u8] = b"kourier-probe-sync-key";
const KEY_DATA: [u8; 32] = [0x5a; 32];

/// The group of the group call record (a tail that is a substring of no other synthetic value).
const GROUP_USER: &str = "120363000064738291";

/// The call ids of the three call records (no digit run).
const CALL_IN: &str = "PROBECALLINAAAA";
const CALL_OUT: &str = "PROBECALLOUTBBBB";
const CALL_GROUP: &str = "PROBECALLGRPCCCC";

/// The mutations' own times (ms) and the calls' start times (s): distinct, so a field read from the
/// wrong place shows.
const ARCHIVE_MS: i64 = 1_760_000_001_000;
const PIN_MS: i64 = 1_760_000_002_000;
const MUTE_MS: i64 = 1_760_000_003_000;
const UNARCHIVE_MS: i64 = 1_760_000_004_000;
const CALL_IN_MS: i64 = 1_760_000_005_000;
const CALL_OUT_MS: i64 = 1_760_000_006_000;
const CALL_GROUP_MS: i64 = 1_760_000_007_000;
const CALL_IN_START: i64 = 1_759_990_001;
const CALL_OUT_START: i64 = 1_759_990_002;
const CALL_GROUP_START: i64 = 1_759_990_003;

/// The three collections the probe's patches live in.
const LOW: &str = "regular_low";
const HIGH: &str = "regular_high";
const REGULAR: &str = "regular";
/// The collection of the address book's `contact` verb (W6a2, PRIV-9).
const BOOK: &str = "critical_unblock_low";
const BOOK_MS: i64 = 1_760_000_008_000;
/// What the phone's address book calls the peer, its first name and the number it names: synthetic, and none of them may be
/// in a log line (`secrets()`).
const BOOK_NAME: &str = "Synthetic Addressbook";
const BOOK_FIRST: &str = "Synthfirst";
const BOOK_PN: &str = "15559260417";

/// The chat of the chat actions and of the 1:1 calls: the peer, by its LID.
fn chat() -> String {
    format!("{}@lid", peer::PEER_LID)
}

fn group() -> String {
    format!("{GROUP_USER}@g.us")
}

// ---- what the bus hands the raw handlers ---------------------------------------------------------

/// Who a JID of an event is, without keeping it.
fn who(jid: &wacore_binary::Jid) -> &'static str {
    let user = jid.user.as_str();
    if user == peer::PEER_LID {
        "peer"
    } else if user == peer::ACCOUNT_LID || user == peer::ACCOUNT_PN {
        "us"
    } else if user == GROUP_USER {
        "group"
    } else {
        "other"
    }
}

fn result_name(result: Option<wa::call_log_record::CallResult>) -> &'static str {
    use wa::call_log_record::CallResult;
    match result {
        Some(CallResult::CONNECTED) => "connected",
        Some(CallResult::MISSED) => "missed",
        Some(CallResult::ACCEPTEDELSEWHERE) => "accepted-elsewhere",
        Some(CallResult::REJECTED) => "rejected",
        Some(_) => "other",
        None => "none",
    }
}

fn which_call(id: &str) -> &'static str {
    match id {
        CALL_IN => "in",
        CALL_OUT => "out",
        CALL_GROUP => "group",
        _ => "unknown",
    }
}

/// One app-state event as the probe keeps it: kinds, flags, synthetic times; no JID.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    Archive {
        chat: &'static str,
        archived: Option<bool>,
        at_ms: Option<i64>,
        full: bool,
    },
    Pin {
        chat: &'static str,
        pinned: Option<bool>,
        at_ms: Option<i64>,
        full: bool,
    },
    Mute {
        chat: &'static str,
        muted: Option<bool>,
        end: Option<i64>,
        at_ms: Option<i64>,
        full: bool,
    },
    Unarchive {
        unarchive: bool,
        full: bool,
    },
    Call {
        which: &'static str,
        creator: &'static str,
        from_me: bool,
        result: &'static str,
        start: Option<i64>,
        participants: Vec<&'static str>,
        group: bool,
        written_ms: i64,
        full: bool,
    },
    SyncFailed {
        fatal: Vec<String>,
        retryable: Vec<String>,
        skipped: Vec<String>,
    },
}

impl Seen {
    fn of(event: &Event) -> Option<Self> {
        Some(match event {
            Event::ArchiveUpdate(e) => Seen::Archive {
                chat: who(&e.jid),
                archived: e.action.archived,
                at_ms: e.action_timestamp.map(|t| t.timestamp_millis()),
                full: e.from_full_sync,
            },
            Event::PinUpdate(e) => Seen::Pin {
                chat: who(&e.jid),
                pinned: e.action.pinned,
                at_ms: e.action_timestamp.map(|t| t.timestamp_millis()),
                full: e.from_full_sync,
            },
            Event::MuteUpdate(e) => Seen::Mute {
                chat: who(&e.jid),
                muted: e.action.muted,
                end: e.action.mute_end_timestamp,
                at_ms: e.action_timestamp.map(|t| t.timestamp_millis()),
                full: e.from_full_sync,
            },
            Event::UnarchiveChatsSettingUpdate(e) => Seen::Unarchive {
                unarchive: e.unarchive_chats,
                full: e.from_full_sync,
            },
            Event::CallLogSync(e) => Seen::Call {
                which: which_call(&e.call_id),
                creator: who(&e.call_creator_jid),
                from_me: e.from_me,
                result: result_name(e.record.call_result),
                start: e.record.start_time,
                participants: e
                    .record
                    .participants
                    .iter()
                    .map(|p| {
                        p.user_jid
                            .as_deref()
                            .and_then(|j| j.parse::<wacore_binary::Jid>().ok())
                            .map_or("unreadable", |j| who(&j))
                    })
                    .collect(),
                group: e.record.group_jid.as_deref() == Some(group().as_str()),
                written_ms: e.timestamp.timestamp_millis(),
                full: e.from_full_sync,
            },
            Event::AppStateSyncFailed(e) => Seen::SyncFailed {
                fatal: e.fatal.clone(),
                retryable: e.retryable.clone(),
                skipped: e.skipped.clone(),
            },
            _ => return None,
        })
    }

    fn is_mutation(&self) -> bool {
        !matches!(self, Seen::SyncFailed { .. })
    }
}

/// The test's raw handler: records what it is handed; never waits.
#[derive(Default)]
struct Observer {
    seen: Mutex<Vec<Seen>>,
    /// The names of the threads the app-state events were handed on (Kourier's handler parks the thread it is
    /// called on until its zone commits: A4 must know whose thread that is).
    threads: Mutex<std::collections::BTreeSet<String>>,
}

impl EventHandler for Observer {
    fn handle_event(&self, event: Arc<Event>) {
        if let Some(seen) = Seen::of(&event) {
            self.seen.lock().expect("lock").push(seen);
            let name = std::thread::current()
                .name()
                .unwrap_or("unnamed")
                .to_owned();
            self.threads.lock().expect("lock").insert(name);
        }
    }
}

impl Observer {
    fn take(&self) -> Vec<Seen> {
        std::mem::take(&mut *self.seen.lock().expect("lock"))
    }

    /// The thread names since the last call.
    fn threads(&self) -> Vec<String> {
        std::mem::take(&mut *self.threads.lock().expect("lock"))
            .into_iter()
            .collect()
    }

    /// Waits (at most 15 s) for `n` mutation events, then 300 ms more for stragglers; takes them all.
    async fn mutations(&self, n: usize) -> Vec<Seen> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let have = self
                .seen
                .lock()
                .expect("lock")
                .iter()
                .filter(|s| s.is_mutation())
                .count();
            if have >= n || Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        self.take()
    }

    /// What came in the next 300 ms (a resync has dispatched everything when it returns: the raw handlers
    /// run inline).
    async fn settle(&self) -> Vec<Seen> {
        tokio::time::sleep(Duration::from_millis(300)).await;
        self.take()
    }
}

// ---- the phone side: patches built with upstream's processor over the same key ---------------------

/// The runtime of the phone side's processor: blocking work inline (upstream's own repro does the same).
struct InlineRuntime;

impl wacore::runtime::Runtime for InlineRuntime {
    fn spawn(&self, _: BoxFuture<'static, ()>) -> AbortHandle {
        panic!("the phone side spawns nothing")
    }
    fn sleep(&self, _: Duration) -> BoxFuture<'static, ()> {
        panic!("the phone side sleeps nowhere")
    }
    fn spawn_blocking(&self, f: Box<dyn FnOnce() + Send + 'static>) -> BoxFuture<'static, ()> {
        Box::pin(async move { f() })
    }
    fn yield_now(&self) -> Option<BoxFuture<'static, ()>> {
        None
    }
}

fn sync_key() -> AppStateSyncKey {
    AppStateSyncKey {
        key_data: KEY_DATA.to_vec(),
        fingerprint: Vec::new(),
        timestamp: 1_700_000_000,
    }
}

fn value(at_ms: i64, fill: impl FnOnce(&mut wa::SyncActionValue)) -> wa::SyncActionValue {
    let mut value = wa::SyncActionValue::default();
    value.timestamp = Some(at_ms);
    fill(&mut value);
    value
}

fn mutation(
    index: &[&str],
    value: &wa::SyncActionValue,
    version: i32,
    iv: u8,
) -> wa::SyncdMutation {
    let index = serde_json::to_vec(index).expect("an index");
    encode_record(
        wa::syncd_mutation::SyncdOperation::Set,
        &index,
        value,
        &expand_app_state_keys(&KEY_DATA),
        KEY_ID,
        &[iv; 16],
        version,
    )
    .0
}

fn call_record(
    id: &str,
    result: wa::call_log_record::CallResult,
    start: i64,
    edit: impl FnOnce(&mut wa::CallLogRecord),
) -> wa::CallLogRecord {
    let mut record = wa::CallLogRecord::default();
    record.call_id = Some(id.to_owned());
    record.call_result = Some(result);
    record.start_time = Some(start);
    record.duration = Some(0);
    edit(&mut record);
    record
}

fn participant(
    jid: &str,
    result: wa::call_log_record::CallResult,
) -> wa::call_log_record::ParticipantInfo {
    let mut p = wa::call_log_record::ParticipantInfo::default();
    p.user_jid = Some(jid.to_owned());
    p.call_result = Some(result);
    p
}

fn call_log(at_ms: i64, record: wa::CallLogRecord) -> wa::SyncActionValue {
    value(at_ms, |v| {
        v.call_log_action = MessageField::some({
            let mut action = wa::sync_action_value::CallLogAction::default();
            action.call_log_record = MessageField::some(record);
            action
        });
    })
}

/// The phone's mutations, per collection: archive, pin and the "keep chats archived" setting in
/// `regular_low`, the mute in `regular_high`, three call records in `regular` (an incoming 1:1 call the
/// peer made, an outgoing 1:1 call we made, an incoming group call).
fn the_phones_mutations() -> Vec<(&'static str, Vec<wa::SyncdMutation>)> {
    use wa::call_log_record::CallResult;
    let chat = chat();
    let us_lid = format!("{}@lid", peer::ACCOUNT_LID);
    let us_pn = format!("{}@s.whatsapp.net", peer::ACCOUNT_PN);
    let low = vec![
        mutation(
            &["archive", &chat],
            &value(ARCHIVE_MS, |v| {
                v.archive_chat_action = MessageField::some({
                    let mut a = wa::sync_action_value::ArchiveChatAction::default();
                    a.archived = Some(true);
                    a
                });
            }),
            3,
            1,
        ),
        mutation(
            &["pin_v1", &chat],
            &value(PIN_MS, |v| {
                v.pin_action = MessageField::some({
                    let mut a = wa::sync_action_value::PinAction::default();
                    a.pinned = Some(true);
                    a
                });
            }),
            5,
            2,
        ),
        mutation(
            &["setting_unarchiveChats"],
            &value(UNARCHIVE_MS, |v| {
                v.unarchive_chats_setting = MessageField::some({
                    let mut a = wa::sync_action_value::UnarchiveChatsSetting::default();
                    a.unarchive_chats = Some(true);
                    a
                });
            }),
            4,
            3,
        ),
    ];
    let high = vec![mutation(
        &["mute", &chat],
        &value(MUTE_MS, |v| {
            v.mute_action = MessageField::some({
                let mut a = wa::sync_action_value::MuteAction::default();
                a.muted = Some(true);
                a.mute_end_timestamp = Some(-1);
                a
            });
        }),
        2,
        4,
    )];
    let regular = vec![
        mutation(
            &["call_log", &chat, CALL_IN, "0"],
            &call_log(
                CALL_IN_MS,
                call_record(CALL_IN, CallResult::MISSED, CALL_IN_START, |r| {
                    r.is_incoming = Some(true);
                    r.call_creator_jid = Some(chat.clone());
                }),
            ),
            1,
            5,
        ),
        mutation(
            &["call_log", &us_lid, CALL_OUT, "1"],
            &call_log(
                CALL_OUT_MS,
                call_record(CALL_OUT, CallResult::CONNECTED, CALL_OUT_START, |r| {
                    r.is_incoming = Some(false);
                    r.participants = vec![participant(&chat, CallResult::CONNECTED)];
                }),
            ),
            1,
            6,
        ),
        mutation(
            &["call_log", &chat, CALL_GROUP, "0"],
            &call_log(
                CALL_GROUP_MS,
                call_record(
                    CALL_GROUP,
                    CallResult::ACCEPTEDELSEWHERE,
                    CALL_GROUP_START,
                    |r| {
                        r.group_jid = Some(group());
                        r.participants = vec![
                            participant(&chat, CallResult::CONNECTED),
                            participant(&us_pn, CallResult::ACCEPTEDELSEWHERE),
                        ];
                    },
                ),
            ),
            1,
            7,
        ),
    ];
    vec![(LOW, low), (HIGH, high), (REGULAR, regular)]
}

/// What the scripted server answers for one collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Serve {
    /// The genesis patch (version 1) to a client below it, an empty collection to one at it.
    Patches,
    /// An empty collection: nothing to apply, nothing more to come.
    Empty,
    /// `<collection type="error"><error code="404"/>`.
    Fatal,
}

/// One `<collection>` the client asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Asked {
    name: String,
    version: u64,
    snapshot: bool,
}

/// The server's side of app state: the phone's genesis patches, what to serve, what was asked.
struct Phone {
    patches: HashMap<&'static str, Vec<u8>>,
    serve: Mutex<HashMap<String, Serve>>,
    asked: Mutex<Vec<Asked>>,
}

impl Phone {
    /// Builds the genesis patch of each collection with upstream's processor over a store of its own
    /// that holds the same sync key (MACs and LTHash as a phone computes them).
    async fn new() -> Arc<Self> {
        Self::with(the_phones_mutations()).await
    }

    /// As [`Phone::new`], for the mutations the case gives (W6a1 A8: the 5,000 of the load case).
    async fn with(collections: Vec<(&'static str, Vec<wa::SyncdMutation>)>) -> Arc<Self> {
        let dir = tempfile::tempdir().expect("temp dir");
        let owner = StoreOwner::open(&db_path(dir.path()), &key(), "wa-probe-phone").expect("open");
        let store = owner.store();
        store
            .set_sync_key(KEY_ID, sync_key())
            .await
            .expect("the phone's key");
        let backend: Arc<dyn Backend> = Arc::new(store);
        let processor = AppStateProcessor::new(backend, Arc::new(InlineRuntime));
        let mut patches = HashMap::new();
        for (name, mutations) in collections {
            let (bytes, base) = processor
                .build_patch(name, mutations)
                .await
                .expect("the phone builds a patch");
            assert_eq!(base, 0, "a genesis patch");
            let mut patch = waproto::codec::syncd_patch_decode(&bytes).expect("the patch decodes");
            let mut version = wa::SyncdVersion::default();
            version.version = Some(1);
            patch.version = MessageField::some(version);
            patches.insert(name, waproto::codec::syncd_patch_to_vec(&patch));
        }
        drop(processor);
        owner.close();
        Arc::new(Self {
            patches,
            serve: Mutex::new(HashMap::new()),
            asked: Mutex::new(Vec::new()),
        })
    }

    fn serve(&self, name: &str, serve: Serve) {
        self.serve
            .lock()
            .expect("lock")
            .insert(name.to_owned(), serve);
    }

    fn asked(&self) -> Vec<Asked> {
        std::mem::take(&mut *self.asked.lock().expect("lock"))
    }

    fn answer(&self, collection: &Node) -> Option<Node> {
        let name = collection.attrs.get("name")?.as_str().into_owned();
        let version: u64 = collection
            .attrs
            .get("version")
            .and_then(|v| v.as_str().parse().ok())
            .unwrap_or(0);
        let snapshot = collection
            .attrs
            .get("return_snapshot")
            .is_some_and(|v| v.as_str() == "true");
        self.asked.lock().expect("lock").push(Asked {
            name: name.clone(),
            version,
            snapshot,
        });
        let serve = self
            .serve
            .lock()
            .expect("lock")
            .get(&name)
            .copied()
            .unwrap_or(Serve::Empty);
        let node = NodeBuilder::new("collection").attr("name", name.clone());
        Some(match (serve, self.patches.get(name.as_str())) {
            (Serve::Fatal, _) => node
                .attr("type", "error")
                .children([NodeBuilder::new("error")
                    .attr("code", "404")
                    .attr("text", "the fixture refuses")
                    .build()])
                .build(),
            (Serve::Patches, Some(patch)) if version < 1 => node
                .attr("version", "1")
                .children([NodeBuilder::new("patches")
                    .children([NodeBuilder::new("patch").bytes(patch.clone()).build()])
                    .build()])
                .build(),
            _ => node.attr("version", version.to_string()).build(),
        })
    }
}

/// The scripted server's rule for `w:sync:app:state`: one `<collection>` answer per `<collection>` asked.
fn app_state(phone: Arc<Phone>) -> impl Fn(&Node) -> Option<Answer> + Send + Sync {
    move |request| {
        let sync = request.get_optional_child("sync")?;
        let answers: Vec<Node> = sync
            .children()
            .into_iter()
            .flatten()
            .filter(|c| c.tag == "collection")
            .filter_map(|c| phone.answer(c))
            .collect();
        Some(Answer::Result(vec![
            NodeBuilder::new("sync").children(answers).build(),
        ]))
    }
}

/// `<notification type="server_sync">` naming collections at a server version.
fn server_sync(id: &str, collections: &[(&str, u64)]) -> Node {
    NodeBuilder::new("notification")
        .attr("type", "server_sync")
        .attr("from", "s.whatsapp.net")
        .attr("id", id.to_owned())
        .attr("t", "1760000100")
        .children(collections.iter().map(|(name, version)| {
            NodeBuilder::new("collection")
                .attr("name", (*name).to_owned())
                .attr("version", version.to_string())
                .build()
        }))
        .build()
}

// ---- the account's books ------------------------------------------------------------------------

/// One collection's books in the account's store: the stored version and `bootstrapped` (`None`: no
/// row), and the dirty flag's version (`None`: clean).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Books {
    version: Option<u64>,
    bootstrapped: bool,
    dirty: Option<i64>,
}

fn books(dir: &Path, name: &str) -> Books {
    let conn = raw(dir);
    let state: Option<Vec<u8>> = conn
        .query_row(
            "SELECT state FROM app_state_versions WHERE name = ?1",
            [name],
            |r| r.get(0),
        )
        .ok();
    let state: Option<HashState> = state.map(|s| serde_json::from_slice(&s).expect("a state"));
    let dirty: Option<i64> = conn
        .query_row(
            "SELECT version FROM appstate_dirty WHERE name = ?1",
            [name],
            |r| r.get(0),
        )
        .ok();
    Books {
        version: state.as_ref().map(|s| s.version),
        bootstrapped: state.is_some_and(|s| s.bootstrapped),
        dirty,
    }
}

fn print_books(dir: &Path, when: &str) {
    for name in [LOW, HIGH, REGULAR] {
        eprintln!("probe: books {when}: {name} {:?}", books(dir, name));
    }
}

// ---- the probe -----------------------------------------------------------------------------------

/// Gathers what did not come out as expected, so one run shows every path.
#[derive(Default)]
struct Expect(Vec<String>);

impl Expect {
    fn eq<T: PartialEq + std::fmt::Debug>(&mut self, what: &str, got: T, want: T) {
        if got != want {
            let line = format!("{what}: got {got:?}, expected {want:?}");
            eprintln!("probe: MISMATCH {line}");
            self.0.push(line);
        }
    }
}

/// The eight events of the phone's three patches, with the replay flag `full_low`/`full_high`/`full_reg`
/// per collection, in the order the probe sorts them.
fn the_eight(full_low: bool, full_high: bool, full_reg: bool) -> Vec<Seen> {
    vec![
        Seen::Archive {
            chat: "peer",
            archived: Some(true),
            at_ms: Some(ARCHIVE_MS),
            full: full_low,
        },
        Seen::Pin {
            chat: "peer",
            pinned: Some(true),
            at_ms: Some(PIN_MS),
            full: full_low,
        },
        Seen::Unarchive {
            unarchive: true,
            full: full_low,
        },
        Seen::Mute {
            chat: "peer",
            muted: Some(true),
            end: Some(-1),
            at_ms: Some(MUTE_MS),
            full: full_high,
        },
        Seen::Call {
            which: "in",
            creator: "peer",
            from_me: false,
            result: "missed",
            start: Some(CALL_IN_START),
            participants: vec![],
            group: false,
            written_ms: CALL_IN_MS,
            full: full_reg,
        },
        Seen::Call {
            which: "out",
            creator: "us",
            from_me: true,
            result: "connected",
            start: Some(CALL_OUT_START),
            participants: vec!["peer"],
            group: false,
            written_ms: CALL_OUT_MS,
            full: full_reg,
        },
        Seen::Call {
            which: "group",
            creator: "peer",
            from_me: false,
            result: "accepted-elsewhere",
            start: Some(CALL_GROUP_START),
            participants: vec!["peer", "us"],
            group: true,
            written_ms: CALL_GROUP_MS,
            full: full_reg,
        },
    ]
}

/// The mutation events, sorted (the probe prints the order it saw; the expectations compare the sets).
fn mutations_of(seen: &[Seen]) -> Vec<Seen> {
    sorted(seen.iter().filter(|s| s.is_mutation()).cloned().collect())
}

fn sorted(mut seen: Vec<Seen>) -> Vec<Seen> {
    seen.sort_by_key(|s| format!("{s:?}"));
    seen
}

/// The asks, sorted by collection name.
fn by_name(mut asked: Vec<Asked>) -> Vec<Asked> {
    asked.sort_by(|a, b| a.name.cmp(&b.name));
    asked
}

fn failures_of(seen: &[Seen]) -> Vec<Seen> {
    seen.iter().filter(|s| !s.is_mutation()).cloned().collect()
}

fn print_seen(path: &str, seen: &[Seen]) {
    for s in seen {
        eprintln!("probe: {path}: {s:?}");
    }
}

fn print_asked(path: &str, asked: &[Asked]) {
    for a in asked {
        eprintln!("probe: {path}: asked {a:?}");
    }
}

/// How many `w:sync:app:state` IQs the client sent in all.
fn app_state_iqs(server: &wire::Wire) -> usize {
    server
        .frames()
        .iter()
        .filter(|f| f.tag == "iq" && f.xmlns.as_deref() == Some("w:sync:app:state"))
        .count()
}

/// The account: the paired device of the receive tests, the sync key, and `regular_low` and
/// `regular_high` bootstrapped at version 0 (`regular` never synced); every flag clear.
async fn account() -> tempfile::TempDir {
    let (dir, _device) = account_dir().await;
    let owner = StoreOwner::open(&db_path(dir.path()), &key(), "wa-test").expect("open");
    let store = owner.store();
    store
        .set_sync_key(KEY_ID, sync_key())
        .await
        .expect("the account's key");
    for name in [LOW, HIGH] {
        store
            .set_version(
                name,
                HashState {
                    bootstrapped: true,
                    ..HashState::default()
                },
            )
            .await
            .expect("a bootstrapped collection");
    }
    owner.close();
    raw(dir.path())
        .execute("DELETE FROM appstate_dirty", [])
        .expect("the flags cleared");
    dir
}

async fn resync(
    client: &whatsapp_rust::Client,
    names: Vec<WAPatchName>,
) -> whatsapp_rust::AppStateResyncReport {
    tokio::time::timeout(
        Duration::from_secs(30),
        client.resync_app_state(names, AppStateResyncMode::Snapshot),
    )
    .await
    .expect("the resync answers within 30 s")
    .expect("the resync reached the server")
}

async fn the_probe() {
    let dir = account().await;
    let phone = Phone::new().await;
    let server = wire::server();
    server.on_iq("w:sync:app:state", app_state(phone.clone()));
    let observer = Arc::new(Observer::default());
    let slot = wire::ClientSlot::default();
    let mut actor = start_actor_observing(
        dir.path(),
        StartMode::Resume,
        &server,
        &slot,
        observer.clone(),
    );
    let states = record(actor.conn());
    // The test plays the core: every envelope is taken and acknowledged.
    let (_, empty) = tokio::sync::mpsc::channel(1);
    let mut envelopes = std::mem::replace(&mut actor.ends.events, empty);
    let core_events = Arc::new(Mutex::new(0usize));
    let sink = core_events.clone();
    let drain = tokio::spawn(async move {
        while let Some(envelope) = envelopes.recv().await {
            *sink.lock().expect("lock") += envelope.events.len();
            if let Some(ack) = envelope.ack {
                ack.complete(Ok(()));
            }
        }
    });
    let mut expect = Expect::default();

    // 0. The first burst: a resumed account whose device has a push name, nobody pushes anything.
    actor.send(Command::Connect).await;
    assert!(
        server.wait_opened(1, Duration::from_secs(15)).await,
        "the handshake never completed"
    );
    server.send(&wire::success_as_the_server_writes_it(peer::ACCOUNT_LID));
    server.send(&wire::hand_over(&format!("{}:0@lid", peer::PEER_LID)));
    let client = slot
        .wait(Duration::from_secs(20))
        .await
        .expect("the client was never handed over");
    wait_for_states(&states, 20, &[ConnState::Online { detail: None }]).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let burst = app_state_iqs(&server);
    let seen = observer.take();
    eprintln!("probe: first burst: {burst} app-state IQs between <success> and Online + 2 s");
    print_seen("first burst", &seen);
    print_asked("first burst", &phone.asked());
    print_books(dir.path(), "after the first burst");
    expect.eq("first burst IQs", burst, 0);
    expect.eq("first burst events", seen, vec![]);

    // 1. The live path: a server_sync for the three collections at version 1.
    for name in [LOW, HIGH, REGULAR] {
        phone.serve(name, Serve::Patches);
    }
    server.send(&server_sync(
        "probe-sync-1",
        &[(LOW, 1), (HIGH, 1), (REGULAR, 1)],
    ));
    let seen = observer.mutations(7).await;
    print_seen("server_sync", &seen);
    eprintln!(
        "probe: server_sync: handed on threads {:?}",
        observer.threads()
    );
    let asked = phone.asked();
    print_asked("server_sync", &asked);
    print_books(dir.path(), "after the server_sync");
    eprintln!(
        "probe: server_sync: {} app-state IQs so far",
        app_state_iqs(&server)
    );
    expect.eq(
        "server_sync events",
        mutations_of(&seen),
        sorted(the_eight(false, false, true)),
    );
    expect.eq("server_sync failures", failures_of(&seen), vec![]);
    expect.eq(
        "server_sync asks",
        by_name(asked),
        vec![
            Asked {
                name: REGULAR.into(),
                version: 0,
                snapshot: true,
            },
            Asked {
                name: HIGH.into(),
                version: 0,
                snapshot: false,
            },
            Asked {
                name: LOW.into(),
                version: 0,
                snapshot: false,
            },
        ],
    );
    for name in [LOW, HIGH, REGULAR] {
        expect.eq(
            &format!("books after the server_sync, {name}"),
            books(dir.path(), name),
            Books {
                version: Some(1),
                bootstrapped: true,
                dirty: Some(1),
            },
        );
    }

    // 2. The rebuild Kourier would ask (D2): a Snapshot of the three.
    let report = resync(
        &client,
        vec![
            WAPatchName::RegularLow,
            WAPatchName::RegularHigh,
            WAPatchName::Regular,
        ],
    )
    .await;
    let seen = observer.mutations(7).await;
    eprintln!("probe: snapshot: report {report:?}");
    print_seen("snapshot", &seen);
    eprintln!(
        "probe: snapshot: handed on threads {:?}",
        observer.threads()
    );
    let asked = phone.asked();
    print_asked("snapshot", &asked);
    print_books(dir.path(), "after the snapshot");
    expect.eq("snapshot all synced", report.all_synced(), true);
    expect.eq(
        "snapshot events",
        mutations_of(&seen),
        sorted(the_eight(true, true, true)),
    );
    expect.eq("snapshot failures", failures_of(&seen), vec![]);
    expect.eq(
        "snapshot asks every collection for a snapshot from version 0",
        asked.iter().all(|a| a.snapshot && a.version == 0) && asked.len() == 3,
        true,
    );
    for name in [LOW, HIGH, REGULAR] {
        expect.eq(
            &format!("books after the snapshot, {name}"),
            books(dir.path(), name),
            Books {
                version: Some(1),
                bootstrapped: true,
                dirty: Some(1),
            },
        );
    }

    // 3. A Snapshot the server answers with an empty collection (H1: "replayed nothing").
    raw(dir.path())
        .execute("DELETE FROM appstate_dirty", [])
        .expect("the flags cleared");
    phone.serve(LOW, Serve::Empty);
    let report = resync(&client, vec![WAPatchName::RegularLow]).await;
    let seen = observer.settle().await;
    eprintln!("probe: empty snapshot: report {report:?}");
    print_seen("empty snapshot", &seen);
    print_asked("empty snapshot", &phone.asked());
    print_books(dir.path(), "after the empty snapshot");
    expect.eq("empty snapshot all synced", report.all_synced(), true);
    expect.eq("empty snapshot events", seen, vec![]);
    expect.eq(
        "books after the empty snapshot, regular_low",
        books(dir.path(), LOW),
        Books {
            version: Some(0),
            bootstrapped: true,
            dirty: Some(0),
        },
    );

    // 4. A Snapshot the server answers 404 (M7).
    phone.serve(HIGH, Serve::Fatal);
    let report = resync(&client, vec![WAPatchName::RegularHigh]).await;
    let seen = observer.settle().await;
    eprintln!("probe: fatal snapshot: report {report:?}");
    print_seen("fatal snapshot", &seen);
    print_asked("fatal snapshot", &phone.asked());
    print_books(dir.path(), "after the fatal snapshot");
    expect.eq(
        "fatal snapshot report",
        (report.fatal.clone(), report.all_synced()),
        (vec![WAPatchName::RegularHigh], false),
    );
    expect.eq("fatal snapshot events", seen, vec![]);
    expect.eq(
        "books after the fatal snapshot, regular_high",
        books(dir.path(), HIGH),
        Books {
            version: None,
            bootstrapped: false,
            dirty: Some(0),
        },
    );

    // 5. The next server-raised sync of those two collections.
    phone.serve(LOW, Serve::Patches);
    phone.serve(HIGH, Serve::Patches);
    server.send(&server_sync("probe-sync-2", &[(LOW, 1), (HIGH, 1)]));
    let seen = observer.mutations(4).await;
    print_seen("server_sync after", &seen);
    eprintln!(
        "probe: server_sync after: handed on threads {:?}",
        observer.threads()
    );
    let asked = phone.asked();
    print_asked("server_sync after", &asked);
    print_books(dir.path(), "after the second server_sync");
    let mut want = the_eight(false, true, false);
    want.truncate(4);
    expect.eq(
        "second server_sync events",
        mutations_of(&seen),
        sorted(want),
    );
    expect.eq(
        "second server_sync asks",
        by_name(asked),
        vec![
            Asked {
                name: HIGH.into(),
                version: 0,
                snapshot: true,
            },
            Asked {
                name: LOW.into(),
                version: 0,
                snapshot: false,
            },
        ],
    );

    // 6. A server-raised sync the server answers 404 (D3: what upstream reports, whether it asks again).
    phone.serve(REGULAR, Serve::Fatal);
    let before = app_state_iqs(&server);
    server.send(&server_sync("probe-sync-3", &[(REGULAR, 2)]));
    tokio::time::sleep(Duration::from_secs(3)).await;
    let seen = observer.take();
    let asked = phone.asked();
    print_seen("server_sync fatal", &seen);
    print_asked("server_sync fatal", &asked);
    print_books(dir.path(), "after the fatal server_sync");
    eprintln!(
        "probe: server_sync fatal: {} app-state IQs in 3 s",
        app_state_iqs(&server) - before
    );
    expect.eq(
        "fatal server_sync events",
        seen,
        vec![Seen::SyncFailed {
            fatal: vec![REGULAR.to_owned()],
            retryable: vec![],
            skipped: vec![],
        }],
    );
    expect.eq(
        "fatal server_sync asks",
        asked,
        vec![Asked {
            name: REGULAR.into(),
            version: 1,
            snapshot: false,
        }],
    );
    expect.eq(
        "books after the fatal server_sync, regular",
        books(dir.path(), REGULAR),
        Books {
            version: Some(1),
            bootstrapped: true,
            dirty: None,
        },
    );

    // The connection held through every path (no failed write ended it, upstream did not reconnect).
    eprintln!(
        "probe: connections {} app-state IQs {} core events {}",
        server.opened(),
        app_state_iqs(&server),
        *core_events.lock().expect("lock")
    );
    expect.eq("connections", server.opened(), 1);
    drop(client);
    actor.send(Command::Shutdown).await;
    assert_eq!(actor.exit().await.0, Exit::Shutdown);
    drain.abort();
    assert_eq!(expect.0, Vec::<String>::new(), "the probe's expectations");
}

// ---- W6a1 A8: the app state end to end ---------------------------------------------------------------
//
// The same fixture, now with Kourier's side switched on: the handler's zone (mirror + `core_outbox` rows in one commit),
// rule (b) (the start-up resync of what a kill left, its reconcile, the quiet quit that clears the flags), and the call
// log. The test plays the CORE: every envelope is taken, its events kept in order and acknowledged. What it asserts is
// what the core was told, in plain words that name no id (`words`), and the books of the account's store.

/// A second chat the cases know (synthetic; a tail that is a substring of no other synthetic value).
const OTHER_USER: &str = "271828182845904";

/// The load case's chats run up from here, `BIG` of them.
const BIG_BASE: u64 = 900_000_000_000_000;
const BIG: u64 = 5_000;

/// `features::VERSION` (crate-private; 4 since W6a2): the seeded chats count as stated with it, started or not. It must EQUAL the
/// crate's: an older number makes the start sweep state the chats once more, and `a_kill_between_a_patch_commit_and_its_dispatch...`
/// asserts the core was told nothing before the kill.
const FEATURES_VERSION: i64 = 4;

fn user_chat(user: &str) -> ConvKey {
    ConvKey::Direct(UserId::from_parts("lid", user).expect("a user id"))
}

/// Which chat of the fixture a key is, without keeping the key.
fn name_of(key: &ConvKey) -> &'static str {
    let text = key.encode();
    let user = text.rsplit(':').next().unwrap_or("");
    if user == peer::PEER_LID {
        "peer"
    } else if user == OTHER_USER {
        "other"
    } else if user == GROUP_USER {
        "group"
    } else if user
        .parse::<u64>()
        .is_ok_and(|n| (BIG_BASE..BIG_BASE + BIG).contains(&n))
    {
        "big"
    } else {
        "unknown"
    }
}

/// What an event tells the core about the settings and the call lines, as words: the archive, the mute, the pin list,
/// a chat's statement, a call line. No id, no number.
fn words(event: &BackendEvent) -> Vec<String> {
    let mut out = Vec::new();
    match event {
        BackendEvent::ConversationUpsert(patch) => {
            let who = name_of(&patch.key);
            if patch.features.is_some() {
                out.push(format!("stated {who}"));
            }
            if let Some(archived) = patch.archived {
                out.push(format!("archived {who} {archived}"));
            }
            match patch.muted_until {
                Patch::Set(i64::MAX) => out.push(format!("muted {who} for-ever")),
                Patch::Set(_) => out.push(format!("muted {who} until")),
                Patch::Clear => out.push(format!("unmuted {who}")),
                Patch::Unchanged => {}
            }
        }
        BackendEvent::PinnedConversations(keys) => {
            let who: Vec<&str> = keys.iter().map(name_of).collect();
            out.push(format!("pins {} {}", keys.len(), who.join(",")));
        }
        // The address book's name for a person the core knows (W6a2, PRIV-9): which person, never the name.
        BackendEvent::ContactUpsert(patch) if matches!(patch.system_name, Patch::Set(_)) => {
            out.push(format!(
                "named {}",
                name_of(&ConvKey::Direct(patch.user.clone()))
            ));
        }
        BackendEvent::HistoryBatch { conv, messages, .. } => {
            for message in messages {
                out.push(format!("line {} {}", name_of(conv), message.local));
            }
        }
        _ => {}
    }
    out
}

/// Everything the core was told, in order.
type Told = Arc<Mutex<Vec<BackendEvent>>>;

fn words_of(told: &Told) -> Vec<String> {
    told.lock().expect("lock").iter().flat_map(words).collect()
}

fn count_of(all: &[String], word: &str) -> usize {
    all.iter().filter(|w| w.as_str() == word).count()
}

/// The test plays the core: every envelope is taken, its events kept and acknowledged. With `log`, every word is
/// also written (one fsynced line each) to that file BEFORE the acknowledgement, for a child a kill may end: the file
/// then holds at least everything the core was told.
fn play_core(actor: &mut Running, log: Option<PathBuf>) -> (Told, tokio::task::JoinHandle<()>) {
    let (_, empty) = tokio::sync::mpsc::channel(1);
    let mut envelopes = std::mem::replace(&mut actor.ends.events, empty);
    let told: Told = Arc::default();
    let sink = told.clone();
    let task = tokio::spawn(async move {
        while let Some(envelope) = envelopes.recv().await {
            if let Some(path) = &log {
                for event in &envelope.events {
                    for word in words(event) {
                        support::kill_log::note(path, &word);
                    }
                }
            }
            sink.lock()
                .expect("lock")
                .extend(envelope.events.iter().cloned());
            if let Some(ack) = envelope.ack {
                ack.complete(Ok(()));
            }
        }
    });
    (told, task)
}

/// Waits until `ready` holds; the wait is for the thing itself, never a window of time.
async fn wait_until(what: &str, secs: u64, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The core knows these chats (their features were stated): an app-state value for them is stated at once (H5).
fn know(dir: &Path, chats: &[ConvKey]) {
    let mut conn = raw(dir);
    let tx = conn.transaction().expect("transaction");
    for chat in chats {
        tx.execute(
            "INSERT OR REPLACE INTO features_stated(conv, version, started) VALUES (?1, ?2, 0)",
            params![chat.encode(), FEATURES_VERSION],
        )
        .expect("the core knows the chat");
    }
    tx.commit().expect("commit");
}

/// The mirror's row of a chat: (value, action_ms, generation, stated), if any.
fn mirror(dir: &Path, chat: &ConvKey, field: &str) -> Option<(i64, i64, i64, i64)> {
    raw(dir)
        .query_row(
            "SELECT value, action_ms, gen, stated FROM chat_state WHERE chat = ?1 AND field = ?2",
            params![chat.encode(), field],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .ok()
}

/// The connection, up to `Online`, with the client handed over.
async fn connect(
    actor: &Running,
    server: &wire::Wire,
    slot: &wire::ClientSlot,
) -> Arc<whatsapp_rust::Client> {
    let states = record(actor.conn());
    actor.send(Command::Connect).await;
    assert!(
        server.wait_opened(1, Duration::from_secs(15)).await,
        "the handshake never completed"
    );
    server.send(&wire::success_as_the_server_writes_it(peer::ACCOUNT_LID));
    server.send(&wire::hand_over(&format!("{}:0@lid", peer::PEER_LID)));
    let client = slot
        .wait(Duration::from_secs(20))
        .await
        .expect("the client was never handed over");
    wait_for_states(&states, 20, &[ConnState::Online { detail: None }]).await;
    client
}

async fn quit(mut actor: Running, drain: tokio::task::JoinHandle<()>) {
    actor.send(Command::Shutdown).await;
    assert_eq!(actor.exit().await.0, Exit::Shutdown);
    drain.abort();
}

/// CASE "pushed": a patch the phone pushes reaches the core once, in one zone with the mirror; a Snapshot replay of
/// the same state changes nothing twice; the call log lands as quiet lines and adds the group's chat; a quiet quit
/// clears the flags (every collection is on `EVENTS_WRITTEN`).
async fn the_pushed_patch() {
    let dir = account().await;
    let peer_chat = user_chat(peer::PEER_LID);
    know(dir.path(), std::slice::from_ref(&peer_chat));
    let phone = Phone::new().await;
    let server = wire::server();
    server.on_iq("w:sync:app:state", app_state(phone.clone()));
    let observer = Arc::new(Observer::default());
    let slot = wire::ClientSlot::default();
    let mut actor = start_actor_observing(
        dir.path(),
        StartMode::Resume,
        &server,
        &slot,
        observer.clone(),
    );
    let (told, drain) = play_core(&mut actor, None);
    let client = connect(&actor, &server, &slot).await;

    for name in [LOW, HIGH, REGULAR] {
        phone.serve(name, Serve::Patches);
    }
    server.send(&server_sync(
        "a8-sync-1",
        &[(LOW, 1), (HIGH, 1), (REGULAR, 1)],
    ));
    let line_in = format!("line peer {}", CallLine::local(false, CALL_IN));
    let line_out = format!("line peer {}", CallLine::local(false, CALL_OUT));
    let line_group = format!("line group {}", CallLine::local(true, CALL_GROUP));
    wait_until("the pushed patches in the core", 30, || {
        let all = words_of(&told);
        [
            "archived peer true",
            "pins 1 peer",
            "muted peer for-ever",
            line_in.as_str(),
            line_out.as_str(),
            line_group.as_str(),
        ]
        .iter()
        .all(|w| count_of(&all, w) >= 1)
    })
    .await;
    // The patches' mutations were all handed to the handler; what the core was told is final once they were.
    let seen = observer.mutations(7).await;
    assert_eq!(mutations_of(&seen), sorted(the_eight(false, false, true)));
    let first = words_of(&told);
    eprintln!("probe: pushed: the core was told {} words", first.len());
    for (word, times) in [
        ("archived peer true", 1),
        ("pins 1 peer", 1),
        ("muted peer for-ever", 1),
        (line_in.as_str(), 1),
        (line_out.as_str(), 1),
        (line_group.as_str(), 1),
    ] {
        assert_eq!(count_of(&first, word), times, "the core was told {word:?}");
    }
    // Nothing else about a setting reached the core (the unarchive setting is the account's, not a chat's).
    for kind in ["archived", "pins", "muted"] {
        assert_eq!(
            first.iter().filter(|w| w.starts_with(kind)).count(),
            1,
            "one {kind} word"
        );
    }
    assert!(
        !first.iter().any(|w| w.starts_with("unmuted")),
        "nothing was unmuted"
    );
    // The group had no chat: it is ADDED, and its statement comes before its line (listed, never hidden).
    let stated = first.iter().position(|w| w == "stated group");
    let line = first.iter().position(|w| *w == line_group);
    assert!(
        stated.is_some() && stated < line,
        "the group's chat is added before its call line"
    );
    // The mirror and the call lines in the account's store.
    let chat_dir = dir.path();
    assert_eq!(
        mirror(chat_dir, &peer_chat, "archived"),
        Some((1, ARCHIVE_MS, 0, 1))
    );
    assert_eq!(
        mirror(chat_dir, &peer_chat, "pinned"),
        Some((1, PIN_MS, 0, 1))
    );
    assert_eq!(
        mirror(chat_dir, &peer_chat, "muted"),
        Some((i64::MAX, MUTE_MS, 0, 1))
    );
    assert_eq!(
        raw(chat_dir)
            .query_row(
                "SELECT value FROM account_appstate WHERE key = 'unarchive_chats'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .ok(),
        Some(1),
        "the account's setting is kept"
    );
    assert_eq!(
        count(&raw(chat_dir), "SELECT count(*) FROM call_lines"),
        3,
        "a line was filed per call"
    );

    // Rule (b): the events are in the core and the mirror, and the flags STAND: they go only at a quiet point (the
    // quit below), never in the events' zone, whatever the handler has written.
    for name in [LOW, HIGH, REGULAR] {
        assert_eq!(
            books(chat_dir, name).dirty,
            Some(1),
            "the flag of {name} stands while the session runs"
        );
    }

    // A Snapshot replay of the same state (Kourier did not ask it: no window, no reset) changes nothing twice.
    let before = told.lock().expect("lock").len();
    let report = resync(
        &client,
        vec![
            WAPatchName::RegularLow,
            WAPatchName::RegularHigh,
            WAPatchName::Regular,
        ],
    )
    .await;
    assert!(report.all_synced());
    let replayed = observer.mutations(7).await;
    assert_eq!(mutations_of(&replayed), sorted(the_eight(true, true, true)));
    // The handler returned for every mutation only after its zone committed, so the core's side is final too.
    let after: Vec<String> = told
        .lock()
        .expect("lock")
        .iter()
        .skip(before)
        .flat_map(words)
        .collect();
    for word in &after {
        assert!(
            word.starts_with("line "),
            "a replay told the core something new ({} words in all)",
            after.len()
        );
        assert!(
            first.contains(word),
            "a replay made a line the core had not"
        );
    }

    drop(client);
    quit(actor, drain).await;
    for name in [LOW, HIGH, REGULAR] {
        assert_eq!(
            books(dir.path(), name),
            Books {
                version: Some(1),
                bootstrapped: true,
                dirty: None,
            },
            "a quiet quit clears the flag of {name}"
        );
    }
}

/// CASE "resync": what a kill left (a flag set when the store opens) is asked ONCE after `Online` on the client's
/// runtime; the replay's events are handed on `wa-client`; the setting the phone no longer has is reset (the REAL
/// reconcile: it needs the replay's events to be handed before the resync returns) and the core told; the flag goes
/// once the connection is quiet.
async fn the_start_up_resync() {
    let dir = account().await;
    let peer_chat = user_chat(peer::PEER_LID);
    let other = user_chat(OTHER_USER);
    know(dir.path(), &[peer_chat.clone(), other.clone()]);
    // The mirror holds a mute of `other` the core was told of and the phone no longer has; `regular_high` was
    // left dirty by the run that ended unquietly.
    raw(dir.path())
        .execute(
            "INSERT INTO chat_state(chat, field, value, action_ms, gen, collection, stated) \
             VALUES (?1, 'muted', ?2, 1760000000500, 0, 'regular_high', 1)",
            params![other.encode(), i64::MAX],
        )
        .expect("a stale mute");
    raw(dir.path())
        .execute(
            "INSERT INTO appstate_dirty(name, version) VALUES ('regular_high', 1)",
            [],
        )
        .expect("the flag a kill left");
    let phone = Phone::new().await;
    phone.serve(HIGH, Serve::Patches);
    let server = wire::server();
    server.on_iq("w:sync:app:state", app_state(phone.clone()));
    let observer = Arc::new(Observer::default());
    let slot = wire::ClientSlot::default();
    let mut actor = start_actor_observing(
        dir.path(),
        StartMode::Resume,
        &server,
        &slot,
        observer.clone(),
    );
    let (told, drain) = play_core(&mut actor, None);
    let client = connect(&actor, &server, &slot).await;

    wait_until("the reset of the stale mute", 30, || {
        count_of(&words_of(&told), "unmuted other") >= 1
    })
    .await;
    wait_until("the flag to go once the connection is quiet", 30, || {
        books(dir.path(), HIGH).dirty.is_none()
    })
    .await;
    let all = words_of(&told);
    let set = all.iter().position(|w| w == "muted peer for-ever");
    let reset = all.iter().position(|w| w == "unmuted other");
    assert_eq!(count_of(&all, "muted peer for-ever"), 1);
    assert_eq!(count_of(&all, "unmuted other"), 1);
    assert!(set < reset, "the reset follows the replay it came from");
    // Exactly one request: the collection the flag named, a snapshot from version 0, once.
    assert_eq!(
        phone.asked(),
        vec![Asked {
            name: HIGH.into(),
            version: 0,
            snapshot: true,
        }]
    );
    // The replay's events were handed on the client's runtime, never on the actor's thread.
    let threads = observer.threads();
    eprintln!(
        "probe: resync: the replay was handed on {} thread(s)",
        threads.len()
    );
    assert_eq!(threads, vec!["wa-client".to_owned()]);
    // The books: the new generation on both rows, the request stamped, no fatal mark.
    assert_eq!(
        mirror(dir.path(), &other, "muted"),
        Some((0, 1_760_000_000_500, 1, 1)),
        "the stale mute went back to the default in the new generation"
    );
    assert_eq!(
        mirror(dir.path(), &peer_chat, "muted"),
        Some((i64::MAX, MUTE_MS, 1, 1))
    );
    let book: (i64, bool, bool) = raw(dir.path())
        .query_row(
            "SELECT gen, asked_at > 0, fatal_at IS NULL FROM appstate_resyncs WHERE collection = 'regular_high'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("the request was stamped");
    assert_eq!(book, (1, true, true));
    assert_eq!(
        count(&raw(dir.path()), "SELECT count(*) FROM appstate_resyncs"),
        1,
        "only the collection with a flag was asked"
    );
    drop(client);
    quit(actor, drain).await;
}

/// CASE "big": a patch of `BIG` mutations (each archives another known chat) is replayed through the handler: one
/// zone and one group commit per mutation on the client's two workers. TIMED and printed; every one reaches the core
/// exactly once.
async fn the_big_replay() {
    let dir = account().await;
    let chats: Vec<ConvKey> = (0..BIG)
        .map(|i| user_chat(&(BIG_BASE + i).to_string()))
        .collect();
    know(dir.path(), &chats);
    let mutations: Vec<wa::SyncdMutation> = (0..BIG)
        .map(|i| {
            let chat = format!("{}@lid", BIG_BASE + i);
            mutation(
                &["archive", &chat],
                &value(ARCHIVE_MS + i64::try_from(i).expect("small"), |v| {
                    v.archive_chat_action = MessageField::some({
                        let mut a = wa::sync_action_value::ArchiveChatAction::default();
                        a.archived = Some(true);
                        a
                    });
                }),
                1,
                9,
            )
        })
        .collect();
    let built = Instant::now();
    let phone = Phone::with(vec![(LOW, mutations)]).await;
    eprintln!(
        "probe: big: the phone built its patch of {BIG} mutations in {} ms",
        built.elapsed().as_millis()
    );
    phone.serve(LOW, Serve::Patches);
    let server = wire::server();
    server.on_iq("w:sync:app:state", app_state(phone.clone()));
    let slot = wire::ClientSlot::default();
    let mut actor = start_actor_handing_client(dir.path(), StartMode::Resume, &server, &slot);
    let (told, drain) = play_core(&mut actor, None);
    let client = connect(&actor, &server, &slot).await;

    let started = Instant::now();
    server.send(&server_sync("a8-sync-big", &[(LOW, 1)]));
    wait_until("every archive of the replay in the core", 180, || {
        let events = told.lock().expect("lock");
        events
            .iter()
            .flat_map(words)
            .filter(|w| w == "archived big true")
            .count()
            >= usize::try_from(BIG).expect("small")
    })
    .await;
    let took = started.elapsed();
    eprintln!(
        "probe: big: a replay of {BIG} mutations reached the core in {} ms ({} us per mutation)",
        took.as_millis(),
        took.as_micros() / u128::from(BIG)
    );
    let all = words_of(&told);
    assert_eq!(
        count_of(&all, "archived big true"),
        usize::try_from(BIG).expect("small"),
        "every mutation once"
    );
    assert!(
        took < Duration::from_secs(120),
        "a replay of {BIG} mutations took {} s",
        took.as_secs()
    );
    drop(client);
    quit(actor, drain).await;
    assert_eq!(
        books(dir.path(), LOW),
        Books {
            version: Some(1),
            bootstrapped: true,
            dirty: None,
        }
    );
}

// ---- W6a1 A8: the kill between upstream's commit_patch and the dispatch ---------------------------------------
//
// Upstream commits a patch (the version row AND Kourier's dirty flag, one transaction) and THEN hands the mutations to
// the handler. A process that dies between the two leaves the version moved and the mirror and the core without the
// change: the server will not offer it again. The flag is what remembers: at the next start the dirty set read when the
// store opens is the reason for ONE resync after `Online`, and its replay tells the core the change once.
//
// Two roles of one child over the scripted server: `first` pushes the server_sync (armed with a failpoint it dies at the
// commit's end), `recover` pushes nothing. Each child plays the core and notes every word it was told in `core.log`
// BEFORE the acknowledgement, so the parent counts across both.

const KILL_ROLE: &str = "WA_APPSTATE_KILL_ROLE";
const KILL_DIR: &str = "WA_APPSTATE_KILL_DIR";
/// The most hits of the failpoint one patch makes (one). A loop that goes past it is a bug.
const KILL_MAX_HITS: u32 = 6;

/// CASE "contact" (W6a2, PRIV-9): the phone's address book names the peer (a `contact` mutation of `critical_unblock_low`
/// with a full name, a first name and a number, all synthetic) for a person the core knows. The core is told the name ONCE,
/// the mirror holds the contact, and no log line of the whole child holds the name, the first name or the number (the file's
/// `secrets()`, checked by the child's recorder AND the parent's grep of its output). The book path logs nothing of a
/// name today: this is the guard that keeps it so.
async fn an_address_book_name_reaches_the_core_and_no_log() {
    let dir = account().await;
    let peer_chat = user_chat(peer::PEER_LID);
    know(dir.path(), std::slice::from_ref(&peer_chat));
    // The core was told of the person (as the sweep that states his chat does): a name for him is stated at once.
    let person = UserId::from_parts("lid", peer::PEER_LID).expect("a user id");
    raw(dir.path())
        .execute(
            "INSERT INTO persons_known(person) VALUES (?1)",
            params![person.as_str()],
        )
        .expect("the core knows the peer");
    let contact = mutation(
        &["contact", &chat()],
        &value(BOOK_MS, |v| {
            v.contact_action = MessageField::some({
                let mut a = wa::sync_action_value::ContactAction::default();
                a.full_name = Some(BOOK_NAME.to_owned());
                a.first_name = Some(BOOK_FIRST.to_owned());
                a.pn_jid = Some(format!("{BOOK_PN}@s.whatsapp.net"));
                a
            });
        }),
        1,
        9,
    );
    let phone = Phone::with(vec![(BOOK, vec![contact])]).await;
    let server = wire::server();
    server.on_iq("w:sync:app:state", app_state(phone.clone()));
    let observer = Arc::new(Observer::default());
    let slot = wire::ClientSlot::default();
    let mut actor = start_actor_observing(dir.path(), StartMode::Resume, &server, &slot, observer);
    let (told, drain) = play_core(&mut actor, None);
    let client = connect(&actor, &server, &slot).await;
    phone.serve(BOOK, Serve::Patches);
    server.send(&server_sync("pc-sync-1", &[(BOOK, 1)]));
    wait_until("the address book's name in the core", 30, || {
        count_of(&words_of(&told), "named peer") >= 1
    })
    .await;
    // Nothing more follows: the name was stated once, and the mirror holds the contact.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        count_of(&words_of(&told), "named peer"),
        1,
        "the core was told the name once"
    );
    assert_eq!(
        count(&raw(dir.path()), "SELECT count(*) FROM address_book"),
        1,
        "the mirror holds the contact"
    );
    drop(client);
    quit(actor, drain).await;
}

async fn kill_child_body() {
    let dir = PathBuf::from(std::env::var(KILL_DIR).expect("the parent names the directory"));
    let role = std::env::var(KILL_ROLE).expect("the parent names the role");
    let core_log = dir.join("core.log");
    let phone = Phone::new().await;
    phone.serve(LOW, Serve::Patches);
    let server = wire::server();
    server.on_iq("w:sync:app:state", app_state(phone.clone()));
    let slot = wire::ClientSlot::default();
    let mut actor = start_actor_handing_client(&dir, StartMode::Resume, &server, &slot);
    let (_told, drain) = play_core(&mut actor, Some(core_log.clone()));
    let client = connect(&actor, &server, &slot).await;
    if role == "first" {
        server.send(&server_sync("a8-sync-kill", &[(LOW, 1)]));
    }
    // `recover`: nobody pushes anything; the start-up resync is the only road to the change.
    let told_by_now = || support::kill_log::lines(&core_log);
    wait_until("the change in the core", 30, || {
        let words = told_by_now();
        count_of(&words, "archived peer true") >= 1 && count_of(&words, "pins 1 peer") >= 1
    })
    .await;
    if role == "recover" {
        assert_eq!(
            phone.asked(),
            vec![Asked {
                name: LOW.into(),
                version: 0,
                snapshot: true,
            }],
            "one snapshot of the collection the flag named"
        );
        wait_until("the flag to go once the connection is quiet", 30, || {
            books(&dir, LOW).dirty.is_none()
        })
        .await;
    }
    drop(client);
    quit(actor, drain).await;
}

fn run_kill_child(
    dir: &Path,
    role: &str,
    point: Option<&str>,
) -> (std::process::ExitStatus, String) {
    let mut command = support::offline::child_command(module_path!(), "appstate_kill_child");
    command
        .env(KILL_ROLE, role)
        .env(KILL_DIR, dir)
        .env_remove("KOURIER_FAILPOINT")
        .env_remove(fault::ENV);
    if let Some(point) = point {
        command.env("KOURIER_FAILPOINT", point);
    }
    let out = command.output().expect("spawn the child");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status, said)
}

/// The end state of every iteration: the change told to the core exactly once, the mirror holding it, the flag gone.
fn assert_told_once(dir: &Path, what: &str) {
    let words = support::kill_log::lines(&dir.join("core.log"));
    assert_eq!(
        count_of(&words, "archived peer true"),
        1,
        "{what}: the archive was told to the core exactly once"
    );
    assert_eq!(
        count_of(&words, "pins 1 peer"),
        1,
        "{what}: the pin list was told exactly once"
    );
    let peer_chat = user_chat(peer::PEER_LID);
    assert_eq!(
        mirror(dir, &peer_chat, "archived").map(|(v, ms, _, stated)| (v, ms, stated)),
        Some((1, ARCHIVE_MS, 1)),
        "{what}: the mirror holds the archive"
    );
    assert_eq!(
        books(dir, LOW),
        Books {
            version: Some(1),
            bootstrapped: true,
            dirty: None,
        },
        "{what}: the version stands and the flag is gone"
    );
}

#[tokio::test]
async fn a_kill_between_a_patch_commit_and_its_dispatch_is_repaired_by_the_next_starts_resync_once()
{
    if !support::offline::unshare_or_skip("this test never runs an actor on the real network") {
        return;
    }
    let template = account().await;
    know(template.path(), &[user_chat(peer::PEER_LID)]);
    let mut kills = 0;
    for n in 1..=KILL_MAX_HITS + 1 {
        assert!(
            n <= KILL_MAX_HITS,
            "the failpoint is hit more than {KILL_MAX_HITS} times"
        );
        let dir = tempfile::tempdir().expect("temp dir");
        copy_dir(template.path(), dir.path());
        let what = format!("{}:{n}", fp::AFTER_COMMIT_PATCH);
        let (status, said) = run_kill_child(dir.path(), "first", Some(&what));
        if status.success() {
            assert_told_once(dir.path(), &what);
            break;
        }
        assert_eq!(
            status.signal(),
            Some(6),
            "{what} must abort: {status:?} {said}"
        );
        assert!(said.contains("hit: aborting"), "{what}: {said}");
        // Committed, not dispatched: the version and the flag are in, the core and the mirror were told nothing.
        assert_eq!(
            books(dir.path(), LOW),
            Books {
                version: Some(1),
                bootstrapped: true,
                dirty: Some(1),
            },
            "{what}: the patch is committed with its flag"
        );
        assert!(
            support::kill_log::lines(&dir.path().join("core.log")).is_empty(),
            "{what}: the kill came before the dispatch, so the core was told nothing"
        );
        assert_eq!(
            mirror(dir.path(), &user_chat(peer::PEER_LID), "archived"),
            None
        );
        let (status, said) = run_kill_child(dir.path(), "recover", None);
        assert!(status.success(), "{what}: the recovery failed: {said}");
        assert_told_once(dir.path(), &what);
        kills += 1;
    }
    assert!(
        kills >= 1,
        "{} was never hit: the kill test saw nothing",
        fp::AFTER_COMMIT_PATCH
    );
}

/// The child of the kill loop (a no-op as an ordinary test).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn appstate_kill_child() {
    if !support::offline::in_child() {
        return;
    }
    let log = Capture::default();
    log.install();
    kill_child_body().await;
    assert_eq!(
        log.leaks(&secrets().iter().map(String::as_str).collect::<Vec<_>>()),
        Vec::<String>::new(),
        "a log line holds a secret of the fixture (the targets are named)"
    );
    assert_eq!(
        log.token_shaped(),
        Vec::<String>::new(),
        "a log line holds something shaped like a key or a token"
    );
}

/// What the child's output and logs must never hold (D10): the fixture's numbers, LIDs, group and name, the second
/// chat and the load case's chats (the first, a middle one and the last).
fn secrets() -> Vec<String> {
    let mut all: Vec<String> = [
        peer::ACCOUNT_PN,
        peer::ACCOUNT_LID,
        peer::PEER_LID,
        GROUP_USER,
        OTHER_USER,
        "Synthetic",
        BOOK_NAME,
        BOOK_FIRST,
        BOOK_PN,
    ]
    .map(str::to_owned)
    .to_vec();
    all.extend([BIG_BASE, BIG_BASE + BIG / 2, BIG_BASE + BIG - 1].map(|n| n.to_string()));
    all
}

const CASE: &str = "WA_APPSTATE_WIRE_CASE";

/// One parent per case: the case runs in the offline child below, and nothing the child printed holds a
/// secret of the fixture. The probe's facts are printed again here (`--success-output immediate` shows them).
fn parent(case: &str) {
    let output =
        support::offline::run_child_output(module_path!(), "appstate_wire_child", &[(CASE, case)]);
    for secret in secrets() {
        assert!(
            !output.contains(secret.as_str()),
            "the child printed one of the fixture's secrets"
        );
    }
    for line in output.lines().filter(|l| l.starts_with("probe:")) {
        println!("{line}");
    }
}

#[test]
fn probe_what_app_state_hands_the_handler_on_each_path() {
    parent("probe");
}

#[test]
fn a_pushed_patch_reaches_the_core_once_and_its_replay_changes_nothing_twice() {
    parent("pushed");
}

#[test]
fn a_start_up_resync_resets_what_the_phone_no_longer_has_on_the_clients_runtime() {
    parent("resync");
}

#[test]
fn a_replay_of_five_thousand_mutations_reaches_the_core_once_each_and_is_timed() {
    parent("big");
}

#[test]
fn an_address_book_name_reaches_the_core_and_no_log_over_the_wire() {
    parent("contact");
}

/// The child (a no-op as an ordinary test): the case the parent named, with every log line recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn appstate_wire_child() {
    if !support::offline::in_child() {
        return;
    }
    let log = Capture::default();
    log.install();
    let case = std::env::var(CASE).expect("the parent names the case");
    eprintln!("case {case}");
    match case.as_str() {
        "probe" => the_probe().await,
        "pushed" => the_pushed_patch().await,
        "resync" => the_start_up_resync().await,
        "big" => the_big_replay().await,
        "contact" => an_address_book_name_reaches_the_core_and_no_log().await,
        other => panic!("unknown case {other}"),
    }
    assert!(
        log.own_lines() >= 1,
        "the recorder saw no line of the crate"
    );
    assert_eq!(
        log.leaks(&secrets().iter().map(String::as_str).collect::<Vec<_>>()),
        Vec::<String>::new(),
        "a log line holds a secret of the fixture (the targets are named)"
    );
    assert_eq!(
        log.token_shaped(),
        Vec::<String>::new(),
        "a log line holds something shaped like a key or a token"
    );
}

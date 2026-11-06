//! THE RE-MAP of `deferred` (TH5 A8; decision M3): a build whose [`classify::MAP_VERSION`] is higher than
//! the one that deferred a row maps that row again, at the actor's start.
//!
//! - WHEN: in `Actor::start` of a linked file (Resume and Rebuild; after a Rebuild's `AccountReady` rows,
//!   BEFORE the forwarder is spawned and before any connection exists): the pages. (The start's AGE BOUND is
//!   not here: round B, S9 + S11, [`expire_at_a_proved_clock`] runs it once per start after the first
//!   `ServerInfo` whose clock check passed, so a clock set far ahead cannot empty `deferred` before any
//!   server said what time it is.) Synchronous, before the first socket, because (1) the
//!   re-mapped rows are OLDER than anything the next connection brings, so `core_outbox` keeps arrival order
//!   (a re-mapped message before the live edit of it); (2) they sit below the catch-up boundary of the first
//!   socket (A5b: `max_seq` at Dry), so `Online` waits until the core has them; (3) nothing else writes the
//!   file then, so the pages are not interleaved with batch zones. The cost is paid once, after an upgrade
//!   that raised `MAP_VERSION`: at most 20,000 rows = 667 zones (decision C14).
//! - THE CHEAP CHECK (TH5b S-L1): before the first page, [`store::has_deferred_below`] (`SELECT EXISTS(...
//!   WHERE map_version < ?1)`) asks the covering index `deferred_by_version` (0003's) and stops at its first
//!   entry, whatever the table holds; false: no page is read and nothing is decrypted. A start with nothing
//!   to map costs that one search (without it, a table of rows at the current version, the poll votes and
//!   calls of up to 45 days, was read leaf page by leaf page at every start: the page query goes by rowid
//!   for its `ORDER BY id`). The pages themselves stay rowid ranges (forcing the version index there would
//!   sort every remaining row per page).
//! - THE PAGES: [`store::deferred_page`] = the rows with `map_version` BELOW the version (a row AT it is never
//!   mapped again), by id, at most [`store::REMAP_PAGE`]; ONE zone per page ([`store::REMAP_POINTS`]). Per
//!   row, inside that zone: the plaintext decoded, [`map::map`] with `remap = true` (round B, M-3: H5's
//!   bring-back is never run here, the phone's later archive is the truth) and `catch_up = true` (a belt),
//!   `server_at` and `received_at` from the row; its BLOB writes, and the row DELETED, or, when the map
//!   defers it again, its `kind` and `map_version` set to this build's (so it is not mapped again at every
//!   start). A row the map now DROPS is deleted, with one counted WARN per rule and zone (the rule's name
//!   only). Events and delete commit together or not at all (`tests/kill.rs`).
//! - THE EVENTS (round B, S6): the page's events in ONE `core_outbox` row (or, past 500 events, in rows of at
//!   most 500 in order, all in the page's zone: TH5b S-L3, `outbox::append_in_rows`; a settings delta over
//!   many stored contacts), each row's in id order, but every MESSAGE as a HISTORY row: one `HistoryBatch` per conversation of the page (`has_more: false`, no
//!   progress; at most 30 messages, one conversation, as the contract wants it), placed where the page's
//!   first message of that conversation was, so E5's patch before it stays before it. WHY: the core's cut
//!   of a chat cleared here or on the phone REFUSES history rows at or below it and remembers their keys
//!   (`kourier-core/src/ingest/clear.rs`), and a history row of a deleted chat is dropped, while a live or
//!   catch-up row is never refused: a file deferred in a chat the person cleared since must not show again
//!   when a later build maps it. A history row also never notifies and sorts by its `sent_at`. Sync deltas
//!   and every event that is not a message stay as they are.
//! - A PAGE THAT FAILS (round A, S4): the error's CODE decides ([`StoreError::is_content`], as H3's loop).
//!   The STORE's failure (a full disk, an I/O error, a busy file, ...) stops the re-map at once: the rows stay
//!   and [`at_start`] returns the error, so THE START FAILS (TH5b S-M1: `Actor::start` ends with "the deferred
//!   rows could not be mapped again", no task spawned, no socket; the core starts the actor again after its
//!   back-off and the re-map runs again before anything is applied live). WHY: since TH5b the batch zone
//!   applies the settings, profile and list deltas LIVE, so a socket opened after a stopped re-map could apply
//!   a NEWER delta that the older row left below the version would overwrite at the next start (a contact the
//!   phone unblocked blocked again, a list `Update` dropped `list-unknown` before its `Create`); the same holds
//!   for every later raise that maps a kind whose order matters. A CONTENT failure retries the page's rows ONE per zone; a row whose own zone fails
//!   is LEFT as it is (counted WARN `remap-failed`; the next start tries it again, until the age bound takes
//!   it) and the cursor moves on, whether one row of the page committed or none: a page of rows a mapper
//!   refuses never stops the rows after it, and it does not fail the start. LIMIT (S-M1): a SYNC row that
//!   fails by its content stays below the version (the sync kinds never expire) and is tried again at every
//!   start; the failure is the content's, so it fails the same way each time and never lands late over a
//!   newer delta.
//! - THE CANCEL (round A, S5): the actor's cancel token is checked before every page, so a Shutdown during a
//!   large re-map is answered within the core's budget (the pages done stay done; the next start goes on).
//! - A row of source `csp` (TH6's leader receive) holds the `d2d.Envelope { incoming_message }` the leader reflected, the
//!   same form as a reflection: it is mapped like a `reflected` row (TH6 decided).
//!
//! Nothing here logs a body, an ID or a key: rule names and counts.

use std::rc::Rc;

use kourier_backend_api::BackendEvent;
use kourier_db::ZoneDb;
use libthreema::protobuf::d2d;
use prost::Message as _;
use sha2::{Digest as _, Sha256};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::{RULE_UNDECODABLE, StoreView, apply_writes, classify, map};
use crate::db;
use crate::error::{Result, StoreError};
use crate::outbox;
use crate::store::{self, Deferred, DeferredBounds};

/// Rule: a row whose own zone failed (left as it is; the next start tries again).
const RULE_REMAP_FAILED: &str = "remap-failed";
/// The domain of a re-mapped envelope's notice tag (the live path takes the hashed D2D nonce, which the
/// row does not keep).
const NOTICE_DOMAIN: &[u8] = b"kourier-threema remap notice\0";

/// What a re-map did: counts only.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Remapped {
    /// The re-map zones that committed.
    pub zones: u32,
    /// Rows mapped: deleted in the zone of their events (or with nothing to state).
    pub mapped: u32,
    /// Rows deferred again: their kind and version set to this build's.
    pub deferred_again: u32,
    /// Rows the map now drops: deleted with a counted WARN.
    pub dropped: u32,
    /// Rows left as they are (a zone that failed).
    pub left: u32,
}

impl Remapped {
    fn add(&mut self, page: &Page) {
        self.zones += 1;
        self.mapped += page.mapped;
        self.deferred_again += page.again;
        self.dropped += page.dropped.iter().map(|(_, n)| n).sum::<u32>();
        self.left += page.left;
    }
}

/// One zone's counts.
#[derive(Default)]
struct Page {
    mapped: u32,
    again: u32,
    left: u32,
    dropped: Vec<(&'static str, u32)>,
    warned: Vec<(&'static str, u32)>,
    /// Parts of update envelopes skipped (R10, R11).
    skipped: Vec<(&'static str, u32)>,
}

fn count(list: &mut Vec<(&'static str, u32)>, rule: &'static str) {
    count_n(list, rule, 1);
}

fn count_n(list: &mut Vec<(&'static str, u32)>, rule: &'static str, by: u32) {
    match list.iter_mut().find(|(r, _)| *r == rule) {
        Some((_, n)) => *n += by,
        None => list.push((rule, by)),
    }
}

impl Page {
    /// The WARN lines of a committed zone (rule names and counts).
    fn warn(&self) {
        for (rule, n) in &self.dropped {
            // The protocol's normal paths (TH5b B3: our own "received" receipt of a TH5a row) are DEBUG.
            if classify::is_quiet(rule) {
                tracing::debug!(
                    rule,
                    dropped = n,
                    "deferred rows dropped by the re-map: {n} by {rule}"
                );
            } else {
                tracing::warn!(
                    rule,
                    dropped = n,
                    "deferred rows dropped by the re-map: {n} by {rule}"
                );
            }
        }
        for (rule, n) in &self.skipped {
            // Quiet rules (P-M1's `sent-notes-group`) are DEBUG, as in the batch zone.
            if classify::is_quiet(rule) {
                tracing::debug!(
                    rule,
                    skipped = n,
                    "re-mapped message updates skipped: {n} by {rule}"
                );
            } else {
                tracing::warn!(
                    rule,
                    skipped = n,
                    "re-mapped message updates skipped: {n} by {rule}"
                );
            }
        }
        for (rule, n) in &self.warned {
            tracing::warn!(
                rule,
                applied = n,
                "re-mapped syncs applied without a field: {n} by {rule}"
            );
        }
    }
}

/// Whether `deferred` is empty (the usual start): then no zone at all (the kept count is one read).
fn empty(db: &ZoneDb) -> bool {
    matches!(db.read(store::deferred_total), Ok(total) if total.rows == 0)
}

/// THE START'S AGE BOUND (round B, S9 + S11): the connection runs it ONCE per actor, at the first
/// `ServerInfo` whose clock check passed (within the skew bound: not under "Connect Anyway", never before
/// the first socket), so a local clock set far ahead cannot empty `deferred` (its rows are the only copy).
/// [`store::expire_deferred`] with the real bounds in its own zone (the sync kinds never expire); an empty
/// table costs one read and no zone. A failure is logged; the next start tries again.
pub(crate) fn expire_at_a_proved_clock(db: &ZoneDb, now_ms: i64) {
    if empty(db) {
        return;
    }
    if store::expire_deferred(db, now_ms, &DeferredBounds::REAL).is_err() {
        tracing::warn!(
            "the age bound of the deferred rows failed; it runs again at the next start"
        );
    }
}

/// The start's part (module doc): the re-map under this build's [`classify::MAP_VERSION`]; `cancel` is the
/// actor's token (S5: a cancelled one is a clean stop, `Ok`). `Err` when the STORE failed (TH5b S-M1: the rows
/// stay and the caller fails the start, so nothing is applied live before they are mapped).
pub(crate) fn at_start(
    db: &ZoneDb,
    own: &str,
    now_ms: i64,
    wake: &Rc<Notify>,
    cancel: &CancellationToken,
) -> Result<()> {
    match run(db, own, classify::MAP_VERSION, now_ms, wake, cancel) {
        Ok(done) if done == Remapped::default() => Ok(()),
        Ok(done) => {
            tracing::info!(
                zones = done.zones,
                mapped = done.mapped,
                deferred_again = done.deferred_again,
                dropped = done.dropped,
                left = done.left,
                "deferred rows mapped again"
            );
            Ok(())
        }
        Err(error) => {
            tracing::warn!(
                "the re-map of deferred rows stopped on the store's failure; the start fails and runs again"
            );
            Err(error)
        }
    }
}

/// THE RE-MAP under `version` (module doc); `own` is the own Threema ID; `now_ms` the local time of its
/// `core_outbox` rows; `cancel` the actor's token, checked first and before every page (S5: the pages done
/// stay done). THE CHEAP CHECK (S-L1) comes before the first page. `Err` only when the STORE fails (S4: by
/// the error's code; the rows stay).
pub(crate) fn run(
    db: &ZoneDb,
    own: &str,
    version: u32,
    now_ms: i64,
    wake: &Rc<Notify>,
    cancel: &CancellationToken,
) -> Result<Remapped> {
    let mut done = Remapped::default();
    if cancel.is_cancelled() || !db.read(|c| store::has_deferred_below(c, version))? {
        return Ok(done);
    }
    let mut after = 0;
    loop {
        if cancel.is_cancelled() {
            return Ok(done);
        }
        let rows = db.read(|c| store::deferred_page(c, version, after))?;
        let Some(last) = rows.last().map(|r| r.id) else {
            return Ok(done);
        };
        match zone(db, own, version, now_ms, wake, &rows) {
            Ok(page) => {
                page.warn();
                done.add(&page);
            }
            // S4: the store itself fails: stop, the rows stay for the next start.
            Err(error) if !error.is_content() => return Err(error),
            Err(_) => {
                tracing::warn!("a page of deferred rows was not re-mapped; one row per zone");
                let mut failed = 0u32;
                for row in &rows {
                    match zone(db, own, version, now_ms, wake, std::slice::from_ref(row)) {
                        Ok(page) => {
                            page.warn();
                            done.add(&page);
                        }
                        Err(error) if !error.is_content() => return Err(error),
                        Err(_) => failed += 1,
                    }
                }
                if failed > 0 {
                    done.left += failed;
                    tracing::warn!(
                        rule = RULE_REMAP_FAILED,
                        left = failed,
                        "deferred rows left as they are: {failed} by {RULE_REMAP_FAILED}"
                    );
                }
            }
        }
        after = last;
    }
}

/// ONE re-map zone over `rows` (the REMAP kill points): each row's writes, events and delete (or new
/// version) together.
fn zone(
    db: &ZoneDb,
    own: &str,
    version: u32,
    now_ms: i64,
    wake: &Rc<Notify>,
    rows: &[Deferred],
) -> Result<Page> {
    db::zone(db, store::REMAP_POINTS, |tx| {
        let mut page = Page::default();
        let mut events = PageEvents::default();
        for row in rows {
            // Both sources: a `csp` row (TH6, the leader's) holds the `d2d.Envelope` it reflected, the same form.
            // The test door (`store::fail_deferred_of_length`): this row's zone fails with its code.
            if let Some(error) =
                super::door_error(row.envelope.len(), "re-map a deferred row failed")
            {
                return Err(error);
            }
            let there = match d2d::Envelope::decode(row.envelope.as_slice()) {
                // Decodable when it was deferred; a belt.
                Err(_) => {
                    count(&mut page.dropped, RULE_UNDECODABLE);
                    store::delete_deferred(tx, row.id)?
                }
                Ok(envelope) => {
                    let cx = map::Cx {
                        own,
                        now_ms: row.received_at,
                        server_at: row.server_at,
                        catch_up: true,
                        notice_tag: notice_tag(&row.envelope),
                        remap: true,
                    };
                    let mapped = map::map(&envelope, &cx, &StoreView(tx))?;
                    let created_ms = i64::try_from(row.server_at).unwrap_or(row.received_at);
                    apply_writes(tx, &mapped.writes, created_ms)?;
                    for event in mapped.events {
                        events.push(event);
                    }
                    if let Some(rule) = mapped.warned {
                        count(&mut page.warned, rule);
                    }
                    for &(rule, n) in &mapped.skipped {
                        count_n(&mut page.skipped, rule, n);
                    }
                    match (mapped.deferred, mapped.dropped) {
                        (Some(kind), _) => {
                            page.again += 1;
                            store::set_deferred_version(tx, row.id, kind, version)?
                        }
                        (None, Some(rule)) => {
                            count(&mut page.dropped, rule);
                            store::delete_deferred(tx, row.id)?
                        }
                        (None, None) => {
                            page.mapped += 1;
                            store::delete_deferred(tx, row.id)?
                        }
                    }
                }
            };
            if !there {
                return Err(StoreError::Failed("re-map a deferred row failed"));
            }
        }
        // ONE row for the page (rows of at most 500 past that, S-L3), in the zone that deletes its rows.
        outbox::append_in_rows(tx, &events.0, now_ms, wake)?;
        Ok(page)
    })
}

/// A page's events (module doc, THE EVENTS): in row order, every message inside its conversation's ONE
/// `HistoryBatch`, made where the page's first message of that conversation came.
#[derive(Default)]
struct PageEvents(Vec<BackendEvent>);

impl PageEvents {
    fn push(&mut self, event: BackendEvent) {
        let BackendEvent::Message(message) = event else {
            self.0.push(event);
            return;
        };
        let batch = self.0.iter_mut().find_map(|e| match e {
            BackendEvent::HistoryBatch { conv, messages, .. } if *conv == message.conv => {
                Some(messages)
            }
            _ => None,
        });
        match batch {
            Some(messages) => messages.push(message),
            None => self.0.push(BackendEvent::HistoryBatch {
                conv: message.conv.clone(),
                messages: vec![message],
                has_more: false,
                progress: None,
            }),
        }
    }
}

/// A re-mapped envelope's notice tag (`map::Cx::notice_tag`): stable for the row (a killed zone maps it again
/// with the same tag), from its plaintext.
fn notice_tag(plaintext: &[u8]) -> [u8; 8] {
    let digest = Sha256::new()
        .chain_update(NOTICE_DOMAIN)
        .chain_update(plaintext)
        .finalize();
    let mut tag = [0; 8];
    tag.copy_from_slice(&digest[..8]);
    tag
}

//! The fake RENDEZVOUS RELAY (TH4 J2), sans-I/O like the mediator: one WebSocket path per join,
//! written from `TP/md-d2d-rendezvous.proto` ("WebSocket Close Codes") on the world's clock.
//!
//! - A path is `wss://rendezvous-<prefix4>.invalid/<prefix8>/<64 lower-case hex>` (the template the
//!   fake's parts hand out, filled as libthreema's patched `RendezvousServerBaseUrl::path` and
//!   Desktop fill it). A one-digit prefix8 and a path that is not 64 hex digits are recorded with
//!   their own rules, and the side is closed with `4000` right after the upgrade (P10: a real relay
//!   upgrades, then closes "Invalid WebSocket path"), as the mediator refuses them for P9.
//! - Two sides per path: the RID (the client, through the fake's connector) and the RRD (the
//!   scripted joiner, `joiner.rs`). Bytes pass unchanged, one WebSocket message for one.
//! - `1000` and `41xx` from one side are reflected to the other; any other end of a side gives the
//!   other `4004`; a second RID, or a third side, is closed with `4000`; a path whose second side
//!   does not come within [`RelayKnobs::init_timeout`] is closed with `4003`.
//! - What the CLIENT (the RID side) does wrong is recorded as a `rendezvous-*` violation; the
//!   decrypted protocol is the joiner's business (it records the client's protocol errors).
//!
//! Nothing here formats a path or a byte: the trace carries side numbers and codes only.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::d2m::Out;
use super::{Clock, Dir, Trace, Violations, lock};

/// The relay's host template (what `ServerParams::rendezvous_url` carries in the fake's parts).
pub const RELAY_URL: &str =
    "wss://rendezvous-{rendezvousPathPrefix4}.invalid/{rendezvousPathPrefix8}/";

/// H3 (`th4-decisions.md`): ten minutes of virtual time, above the join's own scan window, so
/// that by default the client's deadline ends a join nobody scans. [U: real server] the real
/// relay's init timeout is not in the spec.
pub const DEFAULT_INIT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// One side of a relay path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SideId(pub u64);

/// Which device a side is. The relay itself cannot tell them apart [U: real server]; the fake's
/// connector connects the client as the RID (Kourier always requests to join), the joiner as
/// the RRD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Rid,
    Rrd,
}

/// Why a URL is no relay path (the connect is upgraded and closed `4000`, P10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelayRefused {
    /// Not `wss://rendezvous-<one hex digit>.invalid/<prefix8>/<path>`, or the prefixes do not
    /// match the path.
    NotTheRelaysUrl,
    /// `{rendezvousPathPrefix8}` is ONE hex digit although the path starts below `0x10` (the form
    /// before libthreema's patch P9 / P11's two-digit fill).
    PrefixEightHasOneDigit,
    /// The rendezvous path is not 32 bytes as 64 lower-case hex digits (`TP/md-d2d-rendezvous.proto`,
    /// "a random 32 byte hex-encoded rendezvous path").
    PathLength,
}

impl RelayRefused {
    fn rule(self) -> &'static str {
        match self {
            Self::NotTheRelaysUrl => "rendezvous-connect-refused",
            Self::PrefixEightHasOneDigit => "rendezvous-url-prefix8-one-digit",
            Self::PathLength => "rendezvous-url-path-length",
        }
    }
}

/// The relay's knobs (defaults as the real relay is assumed to behave).
#[derive(Clone, Debug)]
pub struct RelayKnobs {
    /// How long a path waits for its second side before `4003`.
    pub init_timeout: Duration,
}

impl Default for RelayKnobs {
    fn default() -> Self {
        Self {
            init_timeout: DEFAULT_INIT_TIMEOUT,
        }
    }
}

/// Whether `host` is the fake relay's: `rendezvous-<one hex digit>.invalid`.
pub fn is_relay_host(host: &str) -> bool {
    host.strip_prefix("rendezvous-")
        .and_then(|h| h.strip_suffix(".invalid"))
        .is_some_and(|p4| p4.len() == 1 && p4.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn is_lower_hex(text: &str) -> bool {
    text.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The rendezvous path (64 hex digits) of a relay URL, with both prefixes checked against it.
pub fn relay_path(url: &str) -> Result<String, RelayRefused> {
    let rest = url
        .strip_prefix("wss://")
        .ok_or(RelayRefused::NotTheRelaysUrl)?;
    let (host, path) = rest.split_once('/').ok_or(RelayRefused::NotTheRelaysUrl)?;
    if !is_relay_host(host) {
        return Err(RelayRefused::NotTheRelaysUrl);
    }
    let prefix4 = &host["rendezvous-".len().."rendezvous-".len() + 1];
    let (prefix8, hex) = path.split_once('/').ok_or(RelayRefused::NotTheRelaysUrl)?;
    if hex.contains('/') || !is_lower_hex(hex) || hex.is_empty() {
        return Err(RelayRefused::NotTheRelaysUrl);
    }
    if hex.len() != 64 {
        return Err(RelayRefused::PathLength);
    }
    if prefix4 != &hex[..1] {
        return Err(RelayRefused::NotTheRelaysUrl);
    }
    if prefix8 != &hex[..2] {
        // `0a...` written as `a`: the one-digit form.
        return Err(if hex.starts_with('0') && prefix8 == &hex[1..2] {
            RelayRefused::PrefixEightHasOneDigit
        } else {
            RelayRefused::NotTheRelaysUrl
        });
    }
    Ok(hex.to_owned())
}

struct Side {
    path: String,
    role: Role,
    out: Vec<Out>,
    /// The code this side's connection ended with (sent by it, or by the relay to it).
    closed: Option<u16>,
}

struct Path {
    rid: Option<SideId>,
    rrd: Option<SideId>,
    opened_ms: u64,
    /// Both sides came once (the init timeout no longer applies).
    joined: bool,
}

struct State {
    clock: Clock,
    violations: Violations,
    trace: Trace,
    knobs: RelayKnobs,
    paths: BTreeMap<String, Path>,
    sides: BTreeMap<SideId, Side>,
    next: u64,
    /// Every frame that passed, as (from, to, length): the bytes are never kept.
    passed: Vec<(SideId, SideId, usize)>,
}

/// The fake relay of one world: a cloneable handle.
#[derive(Clone)]
pub struct Relay(Arc<Mutex<State>>);

impl std::fmt::Debug for Relay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Relay")
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).expect("a timeout fits u64 ms")
}

impl State {
    fn other(&self, side: SideId) -> Option<SideId> {
        let s = self.sides.get(&side)?;
        let p = self.paths.get(&s.path)?;
        match s.role {
            Role::Rid => p.rrd,
            Role::Rrd => p.rid,
        }
        .filter(|o| *o != side)
    }

    /// Ends `side` with `code` sent TO it (when it did not close itself), and leaves its path.
    fn end(&mut self, side: SideId, code: u16, tell: bool) {
        let Some(s) = self.sides.get_mut(&side) else {
            return;
        };
        if s.closed.is_some() {
            return;
        }
        s.closed = Some(code);
        if tell {
            s.out.push(Out::Close(code));
            self.trace.frame(
                Dir::Out,
                side.0,
                "rendezvous-close",
                &[("code", u64::from(code))],
            );
        }
        let (path, role) = (s.path.clone(), s.role);
        if let Some(p) = self.paths.get_mut(&path) {
            match role {
                Role::Rid if p.rid == Some(side) => p.rid = None,
                Role::Rrd if p.rrd == Some(side) => p.rrd = None,
                _ => {}
            }
            if p.rid.is_none() && p.rrd.is_none() {
                self.paths.remove(&path);
            }
        }
    }
}

impl Relay {
    pub fn new(clock: Clock, violations: Violations, trace: Trace) -> Self {
        Self(Arc::new(Mutex::new(State {
            clock,
            violations,
            trace,
            knobs: RelayKnobs::default(),
            paths: BTreeMap::new(),
            sides: BTreeMap::new(),
            next: 1,
            passed: Vec::new(),
        })))
    }

    /// Changes the knobs (for the paths that open afterwards; the init timeout is read at each
    /// [`Relay::tick`]).
    pub fn knobs(&self, change: impl FnOnce(&mut RelayKnobs)) {
        change(&mut lock(&self.0).knobs);
    }

    /// A WebSocket connect to `url` as `role`: a new side. A URL that is not a relay path (recorded
    /// with the rule of [`RelayRefused`]) is UPGRADED like any WebSocket and closed with `4000` at once
    /// ("Invalid WebSocket path", `TP/md-d2d-rendezvous.proto`), as a real relay does (P10) [U: real
    /// server]; the side belongs to no path. A second RID or a third side is opened and closed with
    /// `4000` at once the same way ([U: real server] the real relay might refuse the upgrade instead).
    pub fn connect(&self, url: &str, role: Role) -> SideId {
        let mut s = lock(&self.0);
        let id = SideId(s.next);
        s.next += 1;
        let path = match relay_path(url) {
            Ok(path) => path,
            Err(why) => {
                s.violations.record(why.rule(), &[]);
                s.sides.insert(
                    id,
                    Side {
                        path: String::new(),
                        role,
                        out: Vec::new(),
                        closed: None,
                    },
                );
                s.trace.frame(
                    Dir::In,
                    id.0,
                    "rendezvous-connect",
                    &[("rrd", u64::from(role == Role::Rrd))],
                );
                s.end(id, 4000, true);
                return id;
            }
        };
        let now = s.clock.now_ms();
        s.sides.insert(
            id,
            Side {
                path: path.clone(),
                role,
                out: Vec::new(),
                closed: None,
            },
        );
        s.trace.frame(
            Dir::In,
            id.0,
            "rendezvous-connect",
            &[("rrd", u64::from(role == Role::Rrd))],
        );
        let p = s.paths.entry(path).or_insert(Path {
            rid: None,
            rrd: None,
            opened_ms: now,
            joined: false,
        });
        let taken = match role {
            Role::Rid => p.rid.is_some(),
            Role::Rrd => p.rrd.is_some(),
        };
        // A second RID (the client opened the path twice), a second RRD, or a third side after
        // one of the two left.
        let full = taken || (p.joined && (p.rid.is_none() || p.rrd.is_none()));
        if !full {
            match role {
                Role::Rid => p.rid = Some(id),
                Role::Rrd => p.rrd = Some(id),
            }
            if p.rid.is_some() && p.rrd.is_some() {
                p.joined = true;
            }
        }
        if full {
            if role == Role::Rid {
                s.violations
                    .record("rendezvous-second-rid", &[("side", id.0)]);
            }
            s.end(id, 4000, true);
        }
        id
    }

    /// One WebSocket message from `side`: passed on unchanged to the other side. A message after
    /// the side's connection ended is the client's breach (`rendezvous-frame-after-close`); a
    /// message while the other side is not there is dropped ([U: real server] whether the real
    /// relay buffers it; the protocol has the RID speak only after the RRD's Hello).
    pub fn receive(&self, side: SideId, message: Vec<u8>) {
        let mut s = lock(&self.0);
        let Some(this) = s.sides.get(&side) else {
            return;
        };
        let (closed, role) = (this.closed, this.role);
        if closed.is_some() {
            if role == Role::Rid {
                s.violations
                    .record("rendezvous-frame-after-close", &[("side", side.0)]);
            }
            return;
        }
        s.trace.frame(
            Dir::In,
            side.0,
            "rendezvous-message",
            &[("bytes", message.len() as u64)],
        );
        let Some(other) = s.other(side) else {
            if role == Role::Rid {
                s.violations
                    .record("rendezvous-message-before-the-peer", &[("side", side.0)]);
            }
            return;
        };
        s.passed.push((side, other, message.len()));
        if let Some(o) = s.sides.get_mut(&other) {
            o.out.push(Out::Datagram(message));
        }
    }

    /// `side` closes with `code`: `1000` and `41xx` are reflected to the other side, anything else
    /// gives it `4004`. The client's own codes are those of the spec (1000, 4000, 41xx); any
    /// other is recorded ([U: real server] whether the real relay minds).
    pub fn client_close(&self, side: SideId, code: u16) {
        let mut s = lock(&self.0);
        let Some(this) = s.sides.get(&side) else {
            return;
        };
        if this.closed.is_some() {
            return;
        }
        let role = this.role;
        s.trace.frame(
            Dir::In,
            side.0,
            "rendezvous-close",
            &[("code", u64::from(code))],
        );
        if role == Role::Rid && !matches!(code, 1000 | 4000 | 4100..=4199) {
            s.violations
                .record("rendezvous-client-close-code", &[("code", u64::from(code))]);
        }
        let other = s.other(side);
        s.end(side, code, false);
        if let Some(other) = other {
            let reflected = if code == 1000 || (4100..=4199).contains(&code) {
                code
            } else {
                4004
            };
            s.end(other, reflected, true);
        }
    }

    /// The relay closes `side` with `code` (a script call); the other side gets `4004`.
    pub fn close(&self, side: SideId, code: u16) {
        let mut s = lock(&self.0);
        let other = s.other(side);
        s.end(side, code, true);
        if let Some(other) = other {
            s.end(other, 4004, true);
        }
    }

    /// What the relay sent to `side` since the last call, in order.
    pub fn take(&self, side: SideId) -> Vec<Out> {
        lock(&self.0)
            .sides
            .get_mut(&side)
            .map(|s| std::mem::take(&mut s.out))
            .unwrap_or_default()
    }

    /// Whether `side` has something to take.
    pub fn has_out(&self, side: SideId) -> bool {
        lock(&self.0)
            .sides
            .get(&side)
            .is_some_and(|s| !s.out.is_empty())
    }

    /// Runs the relay's timer at the clock's time: a path still waiting for its second side
    /// `init_timeout` after it opened is closed with `4003`.
    pub fn tick(&self) {
        let mut s = lock(&self.0);
        let now = s.clock.now_ms();
        let timeout = millis(s.knobs.init_timeout);
        let waiting: Vec<SideId> = s
            .paths
            .values()
            .filter(|p| !p.joined && now.saturating_sub(p.opened_ms) >= timeout)
            .filter_map(|p| p.rid.or(p.rrd))
            .collect();
        for side in waiting {
            s.end(side, 4003, true);
        }
    }

    /// The code `side` ended with, if it did.
    pub fn closed(&self, side: SideId) -> Option<u16> {
        lock(&self.0).sides.get(&side).and_then(|s| s.closed)
    }

    /// The role of `side`.
    pub fn role(&self, side: SideId) -> Option<Role> {
        lock(&self.0).sides.get(&side).map(|s| s.role)
    }

    /// The sides of the path of `side` that are still open: (RID, RRD).
    pub fn peers(&self, side: SideId) -> (Option<SideId>, Option<SideId>) {
        let s = lock(&self.0);
        s.sides
            .get(&side)
            .and_then(|x| s.paths.get(&x.path))
            .map_or((None, None), |p| (p.rid, p.rrd))
    }

    /// How many sides ever connected (a bad URL's side, closed 4000 at once, included).
    pub fn side_count(&self) -> usize {
        lock(&self.0).sides.len()
    }

    /// Every message that passed, as (from, to, length).
    pub fn passed(&self) -> Vec<(SideId, SideId, usize)> {
        lock(&self.0).passed.clone()
    }
}

//! One client connection: the destination line, the obfuscated2 header, the handshake, then the
//! MTProto 2.0 encrypted session (containers, acks, ping, future salts with their BARE vector,
//! `new_session_created`, the wrappers peeled). Auth keys live for the process's life and serve every
//! connection (TDLib opens several, and a reconnect or a helper restart reuses its key).

use crate::handshake::{AuthKey, Handshake, Step};
use crate::keys::RsaKey;
use crate::rpc::{self, Ctx};
use crate::transport::{self, Ctr, Header, PREAMBLE_MAX};
use crate::wire::{self, i32_at, i64_at, read_tl_bytes, u32_at};
use grammers_tl_types::Identifiable as _;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;

/// The intermediate framing's tag (TDLib's only transport to a DC through a SOCKS5 proxy).
const INTERMEDIATE: u32 = 0xeeee_eeee;
/// The largest frame the fake DC accepts.
const MAX_FRAME: usize = 1 << 20;

// Service messages and wrappers.
const MSG_CONTAINER: u32 = 0x73f1f8dc;
const MSGS_ACK: u32 = 0x62d6b459;
const PING: u32 = 0x7abe77ec;
const PING_DELAY_DISCONNECT: u32 = 0xf3427b8c;
const PONG: u32 = 0x347773c5;
const GET_FUTURE_SALTS: u32 = 0xb921bd04;
const FUTURE_SALTS: u32 = 0xae500895;
const NEW_SESSION_CREATED: u32 = 0x9ec20908;
/// `destroy_auth_key` (TDLib sends it after a logout, TD-4a A4) and its `destroy_auth_key_ok`.
const DESTROY_AUTH_KEY: u32 = 0xd1435160;
const DESTROY_AUTH_KEY_OK: u32 = 0xf660e1d4;
const IGNORED_SERVICE: [u32; 4] = [0xda69fb52, 0x58e4a740, 0xe7512126, 0x9a5f3a8d];
const VECTOR: u32 = 0x1cb5c415;
const INVOKE_WITH_LAYER: u32 = 0xda9b0d0d;
const INIT_CONNECTION: u32 = 0xc1cd5ea9;
const INVOKE_AFTER_MSG: u32 = 0xcb9f372d;
const INVOKE_AFTER_MSGS: u32 = 0x3dc4b4f0;
const INVOKE_WITHOUT_UPDATES: u32 = 0xbf9459b7;

/// What every connection shares.
pub struct Shared {
    pub rsa: RsaKey,
    keys: Mutex<HashMap<[u8; 8], AuthKey>>,
    wire: Option<Mutex<std::fs::File>>,
    next_conn: AtomicU64,
    /// The world every connection answers from (C4a).
    pub world: Mutex<crate::world::World>,
    /// Where the world's originals are written after every control command (C7's scrub).
    world_json: Option<std::path::PathBuf>,
    /// The open client connections (pushes, `drop`).
    conns: Mutex<BTreeMap<u64, Arc<tokio::sync::Mutex<Out>>>>,
    /// The connection updates go to: the last one that asked `updates.getState`,
    /// `updates.getDifference` or `auth.exportLoginToken` (TDLib's main session).
    updates_conn: Mutex<Option<u64>>,
    pushed: AtomicU64,
    unsent: AtomicU64,
    queued: AtomicU64,
    drops: AtomicU64,
    /// Requests whose constructor name starts with `phone.` (answered 400; the 0002 proof).
    phone_requests: AtomicU64,
    /// `messages.readHistory` and `channels.readHistory` requests that reached the fake DC (held
    /// ones too).
    read_requests: AtomicU64,
    /// Every TCP connection the listener accepted (S-M2: the driver's equation compares it with
    /// the remap's dials, so a second connection behind the gate's back shows).
    accepted: AtomicU64,
    /// The MTProto sessions seen: (auth key id, session id) -> connections that carried it (H-M1).
    /// A real server announces `new_session_created` once per session it has not seen and keeps a
    /// live session across its client's reconnects.
    sessions: Sessions,
    /// The session that last asked `updates.getState`/`getDifference`/`auth.exportLoginToken`
    /// (TDLib's main session): a reconnect on it gets the held changes.
    updates_session: Mutex<Option<SessionKey>>,
    /// The `Updates` objects of QUEUED changes: stored, not pushed. A real server holds what a
    /// live session has not taken and sends it when the session's client is back (H-M1); a
    /// difference answered meanwhile carries them too, so it empties this list.
    held: Mutex<Vec<Vec<u8>>>,
    announced: AtomicU64,
    kept: AtomicU64,
    resent: AtomicU64,
}

/// A session: the auth key's id and the session id the client chose.
type SessionKey = ([u8; 8], i64);

/// The sessions seen so far (see [`Shared::sessions`]).
#[derive(Default)]
struct Sessions(Mutex<HashMap<SessionKey, u32>>);

impl Sessions {
    /// A connection carried `key`: `New` the first time, `Kept` every time after.
    fn seen(&self, key: SessionKey) -> Seen {
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let n = m.entry(key).or_insert(0);
        *n += 1;
        if *n == 1 { Seen::New } else { Seen::Kept }
    }

    fn len(&self) -> usize {
        self.0.lock().map_or(0, |m| m.len())
    }
}

/// What the first message of a connection says of its session.
#[derive(Debug, PartialEq)]
enum Seen {
    /// Not seen before: the server announces it.
    New,
    /// The client came back on a session the server keeps.
    Kept,
}

/// The requests that mark TDLib's updates connection.
const UPDATES_MARKERS: [u32; 3] = [
    grammers_tl_types::functions::updates::GetState::CONSTRUCTOR_ID,
    grammers_tl_types::functions::updates::GetDifference::CONSTRUCTOR_ID,
    grammers_tl_types::functions::auth::ExportLoginToken::CONSTRUCTOR_ID,
];

/// Takes a connection out of [`Shared::conns`] when its task ends, however it ends.
struct ConnGuard<'a> {
    sh: &'a Shared,
    conn: u64,
}

impl Drop for ConnGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut c) = self.sh.conns.lock() {
            c.remove(&self.conn);
        }
    }
}

impl Shared {
    pub fn new(
        rsa: RsaKey,
        wire: Option<std::fs::File>,
        world_json: Option<std::path::PathBuf>,
    ) -> Shared {
        Shared {
            rsa,
            keys: Mutex::new(HashMap::new()),
            wire: wire.map(Mutex::new),
            next_conn: AtomicU64::new(1),
            world: Mutex::new(crate::world::World::new(crate::now_secs() as i32)),
            world_json,
            conns: Mutex::new(BTreeMap::new()),
            updates_conn: Mutex::new(None),
            pushed: AtomicU64::new(0),
            unsent: AtomicU64::new(0),
            queued: AtomicU64::new(0),
            drops: AtomicU64::new(0),
            phone_requests: AtomicU64::new(0),
            read_requests: AtomicU64::new(0),
            accepted: AtomicU64::new(0),
            sessions: Sessions::default(),
            updates_session: Mutex::new(None),
            held: Mutex::new(Vec::new()),
            announced: AtomicU64::new(0),
            kept: AtomicU64::new(0),
            resent: AtomicU64::new(0),
        }
    }

    /// The first message of a connection arrived on session `key`.
    fn session_seen(&self, key: SessionKey) -> Seen {
        let seen = self.sessions.seen(key);
        let counter = if seen == Seen::New {
            &self.announced
        } else {
            &self.kept
        };
        counter.fetch_add(1, Ordering::Relaxed);
        seen
    }

    /// Stores the `Updates` of a queued change for the session that is back (H-M1).
    pub fn hold(&self, updates: Vec<u8>) {
        if let Ok(mut h) = self.held.lock() {
            h.push(updates);
        }
    }

    /// A difference or a state answer carries every stored change: nothing is held any more.
    fn clear_held(&self) {
        if let Ok(mut h) = self.held.lock() {
            h.clear();
        }
    }

    /// A reconnect on the updates session: the held changes go to this connection, in order.
    async fn resend_held(&self, conn: u64) {
        let held: Vec<Vec<u8>> = self
            .held
            .lock()
            .map(|mut h| std::mem::take(&mut *h))
            .unwrap_or_default();
        if held.is_empty() {
            return;
        }
        if let Ok(mut u) = self.updates_conn.lock() {
            *u = Some(conn);
        }
        for p in held {
            if self.push(&p).await {
                self.resent.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Runs `f` on the world (an error word when the lock is poisoned).
    pub fn with_world<T>(
        &self,
        f: impl FnOnce(&mut crate::world::World) -> Result<T, &'static str>,
    ) -> Result<T, &'static str> {
        let mut w = self.world.lock().map_err(|_| "world-poisoned")?;
        f(&mut w)
    }

    /// Sends one `Updates` object to TDLib's updates connection (else the newest connection with
    /// a key); `false` when none took it (the change stays in the world: a difference carries it).
    pub async fn push(&self, updates: &[u8]) -> bool {
        let target = {
            let marked = self.updates_conn.lock().ok().and_then(|c| *c);
            let conns = self.conns.lock().ok();
            conns.and_then(|c| {
                marked
                    .and_then(|m| c.get(&m).cloned().map(|o| (m, o)))
                    .or_else(|| c.iter().next_back().map(|(id, o)| (*id, o.clone())))
            })
        };
        let sent = match target {
            Some((conn, out)) => {
                let ok = out.lock().await.enc_msg(updates, true).await;
                if ok {
                    self.log(
                        conn,
                        "push",
                        crate::wire::u32_at(updates, 0).map_or("?", tl_name),
                    );
                }
                ok
            }
            None => false,
        };
        let counter = if sent { &self.pushed } else { &self.unsent };
        counter.fetch_add(1, Ordering::Relaxed);
        sent
    }

    /// Closes every client connection (TDLib reconnects; the auth keys stay).
    pub async fn drop_all(&self) {
        let outs: Vec<(u64, Arc<tokio::sync::Mutex<Out>>)> = self
            .conns
            .lock()
            .map(|c| c.iter().map(|(k, v)| (*k, v.clone())).collect())
            .unwrap_or_default();
        for (conn, out) in outs {
            let mut o = out.lock().await;
            o.closed = true;
            let _ = o.wr.shutdown().await;
            self.log(conn, "out", "DROPPED");
        }
        if let Ok(mut u) = self.updates_conn.lock() {
            *u = None;
        }
        self.drops.fetch_add(1, Ordering::Relaxed);
    }

    pub fn count_queued(&self) {
        self.queued.fetch_add(1, Ordering::Relaxed);
    }

    /// A marker line in the wire log (`label` is checked by the control socket).
    pub fn mark(&self, label: &str) {
        self.log(0, "mark", label);
    }

    /// The counts `ok` answers carry.
    pub fn stats_json(&self) -> String {
        let (pts, qts, seq) = self
            .world
            .lock()
            .map(|w| (w.pts(), w.qts, w.seq))
            .unwrap_or((0, 0, 0));
        let conns = self.conns.lock().map(|c| c.len()).unwrap_or(0);
        let sessions = self.sessions.len();
        format!(
            "{{\"conns\":{conns},\"pts\":{pts},\"qts\":{qts},\"seq\":{seq},\"pushed\":{},\"unsent\":{},\"queued\":{},\"drops\":{},\"phone_requests\":{},\"read_requests\":{},\"accepted\":{},\"sessions\":{sessions},\"announced\":{},\"kept\":{},\"resent\":{}}}",
            self.pushed.load(Ordering::Relaxed),
            self.unsent.load(Ordering::Relaxed),
            self.queued.load(Ordering::Relaxed),
            self.drops.load(Ordering::Relaxed),
            self.phone_requests.load(Ordering::Relaxed),
            self.read_requests.load(Ordering::Relaxed),
            self.accepted.load(Ordering::Relaxed),
            self.announced.load(Ordering::Relaxed),
            self.kept.load(Ordering::Relaxed),
            self.resent.load(Ordering::Relaxed),
        )
    }

    /// Writes `world.json` (complete or not at all) when the serve command named one.
    pub fn write_world_json(&self) {
        let Some(path) = &self.world_json else {
            return;
        };
        let Ok(text) = self.world.lock().map(|w| w.to_json()) else {
            return;
        };
        let tmp = path.with_extension("tmp");
        let _ = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, path));
    }

    /// One wire-log line: time, connection, direction and a constructor NAME; never a field value.
    fn log(&self, conn: u64, dir: &str, name: &str) {
        if let Some(w) = &self.wire {
            let line = format!(
                "{{\"t\":{},\"conn\":{conn},\"dir\":\"{dir}\",\"name\":\"{name}\"}}\n",
                crate::now_millis()
            );
            if let Ok(mut f) = w.lock() {
                let _ = f.write_all(line.as_bytes());
            }
        }
    }
}

fn tl_name(id: u32) -> &'static str {
    grammers_tl_types::name_for_id(id)
}

/// Everything needed to write to one client.
struct Out {
    wr: OwnedWriteHalf,
    to_client: Ctr,
    key: Option<AuthKey>,
    session_id: i64,
    last_msg_id: i64,
    seq: i32,
    /// The session this connection's first message named (H-M1: announced once per session).
    seen: Option<SessionKey>,
    /// `drop` closed it: nothing more is written.
    closed: bool,
}

impl Out {
    async fn raw(&mut self, payload: &[u8]) -> bool {
        if self.closed {
            return false;
        }
        let f = transport::frame(&mut self.to_client, payload);
        self.wr.write_all(&f).await.is_ok()
    }

    fn next_msg_id(&mut self) -> i64 {
        let mut id = (crate::now_secs() << 32) | 1;
        if id <= self.last_msg_id {
            id = self.last_msg_id + 4;
        }
        self.last_msg_id = id;
        id
    }

    /// An unencrypted message (the handshake's answers).
    async fn plain(&mut self, body: &[u8]) {
        let mut p = vec![0u8; 8];
        p.extend(self.next_msg_id().to_le_bytes());
        p.extend((body.len() as i32).to_le_bytes());
        p.extend(body);
        let _ = self.raw(&p).await;
    }

    /// An encrypted server -> client message.
    async fn enc_msg(&mut self, body: &[u8], content_related: bool) -> bool {
        let Some(k) = self.key else {
            return false;
        };
        let msg_id = self.next_msg_id();
        let seq_no = if content_related {
            let n = self.seq;
            self.seq += 2;
            n + 1
        } else {
            self.seq
        };
        let mut pt = Vec::with_capacity(32 + body.len());
        pt.extend(k.salt.to_le_bytes());
        pt.extend(self.session_id.to_le_bytes());
        pt.extend(msg_id.to_le_bytes());
        pt.extend(seq_no.to_le_bytes());
        pt.extend((body.len() as i32).to_le_bytes());
        pt.extend(body);
        let frame = wire::seal(&k.key, &k.id, pt);
        self.raw(&frame).await
    }
}

/// Skips one `JSONValue` (initConnection's params).
fn skip_json(b: &[u8], o: &mut usize) -> Option<()> {
    let c = u32_at(b, *o)?;
    *o += 4;
    match c {
        0x3f6d7b68 => {}       // jsonNull
        0xc7345e6a => *o += 4, // jsonBool value:Bool
        0x2be0dfa4 => *o += 8, // jsonNumber value:double
        0xb71e767a => {
            read_tl_bytes(b, o)?; // jsonString
        }
        0xf7444763 => {
            // jsonArray value:Vector<JSONValue>
            *o += 4;
            let n = u32_at(b, *o)?;
            *o += 4;
            for _ in 0..n {
                skip_json(b, o)?;
            }
        }
        0x99c1d49d => {
            // jsonObject value:Vector<JSONObjectValue key:string value:JSONValue>
            *o += 4;
            let n = u32_at(b, *o)?;
            *o += 4;
            for _ in 0..n {
                *o += 4;
                read_tl_bytes(b, o)?;
                skip_json(b, o)?;
            }
        }
        _ => return None,
    }
    Some(())
}

/// The offset of the real query inside its wrappers (invokeWithLayer, initConnection,
/// invokeAfterMsg(s), invokeWithoutUpdates), or `None` for a wrapper it cannot read.
pub fn peel(b: &[u8]) -> Option<usize> {
    let mut off = 0;
    loop {
        match u32_at(b, off)? {
            INVOKE_WITH_LAYER => off += 8,
            INVOKE_AFTER_MSG => off += 12,
            INVOKE_WITHOUT_UPDATES => off += 4,
            INVOKE_AFTER_MSGS => {
                if u32_at(b, off + 4)? != VECTOR {
                    return None;
                }
                let n = u32_at(b, off + 8)? as usize;
                off += 12 + 8 * n;
            }
            INIT_CONNECTION => {
                // flags, the app's id, six strings, proxy (flag 0), params (flag 1), query
                let flags = u32_at(b, off + 4)?;
                let mut o = off + 12;
                for _ in 0..6 {
                    read_tl_bytes(b, &mut o)?;
                }
                if flags & 1 != 0 {
                    o += 4;
                    read_tl_bytes(b, &mut o)?;
                    o += 4;
                }
                if flags & 2 != 0 {
                    skip_json(b, &mut o)?;
                }
                off = o;
            }
            _ => return Some(off),
        }
    }
}

async fn handle_msg(
    sh: &Shared,
    (conn, session): (u64, SessionKey),
    ctx: &Ctx,
    out: &tokio::sync::Mutex<Out>,
    msg_id: i64,
    body: &[u8],
    needs_ack: &mut Vec<i64>,
) {
    let Some(c) = u32_at(body, 0) else {
        return;
    };
    match c {
        MSGS_ACK => {}
        PING | PING_DELAY_DISCONNECT => {
            let Some(ping_id) = i64_at(body, 4) else {
                return;
            };
            let mut v = PONG.to_le_bytes().to_vec();
            v.extend(msg_id.to_le_bytes());
            v.extend(ping_id.to_le_bytes());
            out.lock().await.enc_msg(&v, false).await;
        }
        GET_FUTURE_SALTS => {
            let mut o = out.lock().await;
            let Some(k) = o.key else {
                return;
            };
            let now = crate::now_secs();
            let mut v = FUTURE_SALTS.to_le_bytes().to_vec();
            v.extend(msg_id.to_le_bytes());
            v.extend((now as i32).to_le_bytes());
            // `salts:vector<future_salt>` is a BARE vector (lower-case `vector`): count, items.
            v.extend(1i32.to_le_bytes());
            v.extend(((now - 60) as i32).to_le_bytes());
            v.extend(((now + 86_400) as i32).to_le_bytes());
            v.extend(k.salt.to_le_bytes());
            o.enc_msg(&v, false).await;
        }
        DESTROY_AUTH_KEY => {
            sh.log(conn, "in", "destroy_auth_key");
            let mut o = out.lock().await;
            o.enc_msg(&DESTROY_AUTH_KEY_OK.to_le_bytes(), false).await;
            sh.log(conn, "out", "destroy_auth_key_ok");
            // The key is gone for every later connection (a new login makes a new one).
            if let (Some(k), Ok(mut keys)) = (o.key, sh.keys.lock()) {
                keys.remove(&k.id);
            }
        }
        c if IGNORED_SERVICE.contains(&c) => {}
        _ => {
            needs_ack.push(msg_id);
            let res = match peel(body) {
                Some(off) => {
                    let q = &body[off..];
                    let id = u32_at(q, 0);
                    let name = id.map_or("?", tl_name);
                    sh.log(conn, "in", name);
                    if id.is_some_and(|c| UPDATES_MARKERS.contains(&c)) {
                        if let Ok(mut u) = sh.updates_conn.lock() {
                            *u = Some(conn);
                        }
                        if let Ok(mut u) = sh.updates_session.lock() {
                            *u = Some(session);
                        }
                    }
                    if name.starts_with("updates.get") {
                        // The answer carries every stored change (the world holds them all).
                        sh.clear_held();
                    }
                    if name.starts_with("phone.") {
                        sh.phone_requests.fetch_add(1, Ordering::Relaxed);
                    }
                    if matches!(name, "messages.readHistory" | "channels.readHistory") {
                        sh.read_requests.fetch_add(1, Ordering::Relaxed);
                    }
                    rpc::answer(ctx, &sh.world, q)
                }
                None => rpc::rpc_error(400, "FAKE_BAD_WRAPPER"),
            };
            if res.as_slice() == rpc::HELD {
                // A held request (`hold-reads on`): nothing goes back.
                sh.log(conn, "out", "held");
                return;
            }
            sh.log(conn, "out", u32_at(&res, 0).map_or("?", tl_name));
            out.lock()
                .await
                .enc_msg(&rpc::rpc_result(msg_id, &res), true)
                .await;
        }
    }
}

/// Reads the destination line byte by byte (at most [`PREAMBLE_MAX`]).
async fn read_preamble(rd: &mut tokio::net::tcp::OwnedReadHalf) -> Option<SocketAddr> {
    let mut line = Vec::new();
    loop {
        let b = rd.read_u8().await.ok()?;
        if b == b'\n' {
            return transport::parse_preamble(&line);
        }
        if line.len() >= PREAMBLE_MAX {
            return None;
        }
        line.push(b);
    }
}

pub async fn handle_conn(s: TcpStream, sh: Arc<Shared>) {
    let conn = sh.next_conn.fetch_add(1, Ordering::Relaxed);
    sh.accepted.fetch_add(1, Ordering::Relaxed);
    let (mut rd, wr) = s.into_split();
    let Some(dialled) = read_preamble(&mut rd).await else {
        sh.log(conn, "in", "BAD_PREAMBLE");
        return;
    };
    let mut hdr = [0u8; 64];
    if rd.read_exact(&mut hdr).await.is_err() {
        return;
    }
    let header = Header::read(&hdr);
    let this_dc = header.dc_id();
    let Header {
        mut from_client,
        to_client,
        tag,
        ..
    } = header;
    if tag != INTERMEDIATE {
        sh.log(conn, "in", "BAD_TRANSPORT");
        return;
    }
    sh.log(conn, "in", "connect");
    let out = Arc::new(tokio::sync::Mutex::new(Out {
        wr,
        to_client,
        key: None,
        session_id: 0,
        last_msg_id: 0,
        seq: 0,
        seen: None,
        closed: false,
    }));
    if let Ok(mut c) = sh.conns.lock() {
        c.insert(conn, out.clone());
    }
    let _guard = ConnGuard { sh: &sh, conn };
    let mut hs = Handshake::default();
    loop {
        let mut lb = [0u8; 4];
        if rd.read_exact(&mut lb).await.is_err() {
            sh.log(conn, "in", "closed");
            return;
        }
        use ctr::cipher::StreamCipher;
        from_client.apply_keystream(&mut lb);
        let len = (u32::from_le_bytes(lb) & 0x7fff_ffff) as usize;
        if len > MAX_FRAME {
            sh.log(conn, "in", "FRAME_TOO_LONG");
            return;
        }
        let mut pl = vec![0u8; len];
        if rd.read_exact(&mut pl).await.is_err() {
            return;
        }
        from_client.apply_keystream(&mut pl);
        if pl.len() < 24 {
            continue;
        }
        if pl[..8] == [0u8; 8] {
            let Some(mlen) = i32_at(&pl, 16) else {
                continue;
            };
            let Some(body) = pl.get(20..20 + mlen.max(0) as usize) else {
                continue;
            };
            let name = match u32_at(body, 0) {
                Some(crate::handshake::REQ_PQ_MULTI) => "req_pq_multi",
                Some(crate::handshake::REQ_DH_PARAMS) => "req_DH_params",
                Some(crate::handshake::SET_CLIENT_DH_PARAMS) => "set_client_DH_params",
                _ => "unexpected_plain",
            };
            sh.log(conn, "in", name);
            match hs.step(&sh.rsa, body) {
                Ok(Step::Reply(r)) => out.lock().await.plain(&r).await,
                Ok(Step::Done(k, r)) => {
                    if let Ok(mut keys) = sh.keys.lock() {
                        keys.insert(k.id, *k);
                    }
                    let mut o = out.lock().await;
                    o.key = Some(*k);
                    o.plain(&r).await;
                    sh.log(conn, "out", "dh_gen_ok");
                }
                Err(_) => {
                    sh.log(conn, "in", "HANDSHAKE_REFUSED");
                    return;
                }
            }
            continue;
        }
        let kid: [u8; 8] = pl[..8].try_into().expect("8");
        let found = sh.keys.lock().ok().and_then(|k| k.get(&kid).copied());
        let Some(k) = found else {
            sh.log(conn, "out", "UNKNOWN_AUTH_KEY");
            let _ = out.lock().await.raw(&(-404i32).to_le_bytes()).await;
            continue;
        };
        let Some(ptx) = wire::open(&k.key, &pl[8..]) else {
            sh.log(conn, "in", "BAD_MSG_KEY");
            continue;
        };
        let (Some(session_id), Some(msg_id), Some(blen)) =
            (i64_at(&ptx, 8), i64_at(&ptx, 16), i32_at(&ptx, 28))
        else {
            continue;
        };
        let Some(body) = ptx.get(32..32 + blen.max(0) as usize) else {
            continue;
        };
        let session = (k.id, session_id);
        let mut back_on_updates_session = false;
        {
            let mut o = out.lock().await;
            if o.key.is_none() {
                o.key = Some(k);
            }
            o.session_id = session_id;
            if o.seen != Some(session) {
                o.seen = Some(session);
                match sh.session_seen(session) {
                    Seen::New => {
                        let mut unique = [0u8; 8];
                        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut unique);
                        let mut v = NEW_SESSION_CREATED.to_le_bytes().to_vec();
                        v.extend(msg_id.to_le_bytes());
                        v.extend(unique);
                        v.extend(k.salt.to_le_bytes());
                        o.enc_msg(&v, false).await;
                        sh.log(conn, "out", "new_session_created");
                    }
                    Seen::Kept => {
                        sh.log(conn, "in", "session_kept");
                        back_on_updates_session =
                            sh.updates_session.lock().is_ok_and(|u| *u == Some(session));
                    }
                }
            }
        }
        if back_on_updates_session {
            sh.resend_held(conn).await;
        }
        let ctx = Ctx {
            now: crate::now_secs() as i32,
            this_dc,
            dialled,
        };
        let mut needs_ack = Vec::new();
        if u32_at(body, 0) == Some(MSG_CONTAINER) {
            let count = i32_at(body, 4).unwrap_or(0);
            let mut o = 8;
            for _ in 0..count {
                let (Some(id), Some(bl)) = (i64_at(body, o), i32_at(body, o + 12)) else {
                    break;
                };
                let Some(inner) = body.get(o + 16..o + 16 + bl.max(0) as usize) else {
                    break;
                };
                handle_msg(&sh, (conn, session), &ctx, &out, id, inner, &mut needs_ack).await;
                o += 16 + bl.max(0) as usize;
            }
        } else {
            handle_msg(
                &sh,
                (conn, session),
                &ctx,
                &out,
                msg_id,
                body,
                &mut needs_ack,
            )
            .await;
        }
        if !needs_ack.is_empty() {
            let mut v = MSGS_ACK.to_le_bytes().to_vec();
            v.extend(VECTOR.to_le_bytes());
            v.extend((needs_ack.len() as i32).to_le_bytes());
            for id in &needs_ack {
                v.extend(id.to_le_bytes());
            }
            out.lock().await.enc_msg(&v, false).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_is_announced_once_per_key_and_id_and_kept_after() {
        let s = Sessions::default();
        let (a, b) = (([1; 8], 7i64), ([1; 8], 8i64));
        assert_eq!(s.seen(a), Seen::New);
        assert_eq!(s.seen(a), Seen::Kept, "a reconnect on the same session");
        assert_eq!(s.seen(b), Seen::New, "another session id");
        assert_eq!(s.seen(([2; 8], 7)), Seen::New, "another auth key");
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn the_wrappers_are_peeled_to_the_query() {
        let query = 0x1111_2222u32.to_le_bytes();
        // invokeWithLayer(232, invokeWithoutUpdates(invokeAfterMsg(7, query)))
        let mut b = INVOKE_WITH_LAYER.to_le_bytes().to_vec();
        b.extend(232i32.to_le_bytes());
        b.extend(INVOKE_WITHOUT_UPDATES.to_le_bytes());
        b.extend(INVOKE_AFTER_MSG.to_le_bytes());
        b.extend(7i64.to_le_bytes());
        b.extend(INVOKE_AFTER_MSGS.to_le_bytes());
        b.extend(VECTOR.to_le_bytes());
        b.extend(2i32.to_le_bytes());
        b.extend(1i64.to_le_bytes());
        b.extend(2i64.to_le_bytes());
        // initConnection with flag 1 (params: a jsonObject of one jsonString)
        b.extend(INIT_CONNECTION.to_le_bytes());
        b.extend(2u32.to_le_bytes());
        b.extend(1i32.to_le_bytes());
        for s in ["a", "b", "c", "d", "e", "f"] {
            b.extend(wire::tl_bytes(s.as_bytes()));
        }
        b.extend(0x99c1d49du32.to_le_bytes());
        b.extend(VECTOR.to_le_bytes());
        b.extend(1i32.to_le_bytes());
        b.extend(0xc0de1bd9u32.to_le_bytes());
        b.extend(wire::tl_bytes(b"k"));
        b.extend(0xb71e767au32.to_le_bytes());
        b.extend(wire::tl_bytes(b"v"));
        let at = b.len();
        b.extend(query);
        assert_eq!(peel(&b), Some(at));
        assert_eq!(peel(&query), Some(0));
        assert_eq!(peel(&INVOKE_WITH_LAYER.to_le_bytes()), None);
    }
}

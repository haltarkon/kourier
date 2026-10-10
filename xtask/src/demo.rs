//! `cargo xtask demo [--out <dir>] [--interactive]`: the real Kourier app (debug build, Signal
//! backend) linked to Signal's mock server, driven like a user would and photographed.
//!
//! The mock harness (`tests/mock-harness`, Node 24) plays a phone and three contacts with
//! synthetic names and avatars: the phone's contact sync and Storage Service state name them
//! (S9), their profiles carry the avatars (drawn here, see [`avatar_png`]). Kourier runs
//! offscreen on a throwaway profile below the target directory
//! (`KOURIER_HOME`, the development keyring instead of the desktop's Secret Service,
//! `KOURIER_SIGNAL_ENV=local:<handshake>`) and is driven through its development control channel
//! (`--dev-control`, `crates/kourier-qt/src/dev`): welcome page → add a Signal account → device
//! name → the real QR code → the phone links it → contacts write (sealed and unsealed sender, a
//! burst of 20 in one chat) → conversation list → chat → a reply from Kourier that the contact's
//! phone receives → its Sent state → a group reply over sender keys → a photo received and a
//! file sent → several files of mixed kinds in one go ([`files_scene`]: two pictures and three
//! documents; the run fails unless the contact's phone gets the album and each document, byte
//! for byte) → the message actions of S13 ([`modifiers_scene`]: a contact reacts to a message of
//! Kourier's, Kourier corrects a typo, replies to her photo with a quote, and deletes a message
//! for everyone; the run fails unless the contact's phone and our own phone get the edit, the
//! quote and the delete, and unless Kourier's own timeline shows the reaction, the edited mark,
//! the quote and the tombstone); then a restart in the dark color scheme. Screenshots go to `<out>`
//! (default `target/shots/demo`) and, as each is written, to
//! `target/shots-progress/d1-<nn>-<what>.png`.
//!
//! `--interactive` sets up the same world but runs Kourier on the real display without any
//! automation: the user adds the account, the phone scans the QR code by itself, and a small
//! prompt sends messages "from the phone's contacts".
//!
//! Nothing here talks to a real Signal server: the backend only knows the mock's address.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::{cargo, root, run, target_dir, theme_home};

/// Window sizes of the shots (the same as `cargo xtask shots`).
const WIDE: (u32, u32) = (1200, 800);
const NARROW: (u32, u32) = (420, 800);
/// The device name typed on the link page.
const DEVICE_NAME: &str = "Kourier demo";
/// The phone's profile name (our own profile in its Storage Service account record).
const PHONE_NAME: &str = "Demo Phone";
/// Synthetic contacts: prompt key and profile name (also their address-book name on the phone).
const CONTACTS: [(&str, &str); 3] = [
    ("ada", "Ada Lindqvist"),
    ("bruno", "Bruno Okafor"),
    ("chiara", "Chiara Velasco"),
];
/// Index of the contact the phone pins (first in the list).
const PINNED_CONTACT: usize = 0;
/// Index of the contact whose chat gets the burst and the reply.
const BURST_CONTACT: usize = 2;
/// The reply Kourier sends.
const REPLY: &str = "Sounds great, count me in! Sent from Kourier on the desktop.";
/// The reply Kourier sends in the group (one sender-key message to every member, §6.7 step 5).
const GROUP_REPLY: &str = "Count me in! I'll bring the chalk and a spare harness.";
/// The demo group and its members (indices into [`CONTACTS`]).
const GROUP_TITLE: &str = "Climbing club";
/// The caption of the photo Ada sends (S12).
const PHOTO_CAPTION: &str = "Sunset from the ridge last night 📷";
/// The caption of the file Kourier sends to Ada (S12).
const FILE_CAPTION: &str = "Here is the route plan for Saturday.";
/// The file Kourier sends to Ada.
const ROUTE_PLAN: &str = "Route plan\n\n08:00 trailhead\n09:30 first pitch\n13:00 summit\n";
/// The caption of the files Kourier sends in one go.
const FILES_CAPTION: &str = "Everything for Saturday: two photos of the wall and the paperwork.";
/// The documents of that send: name and content.
const DOCUMENTS: [(&str, &str); 3] = [
    (
        "Packing list.txt",
        "Packing list\n\n- harness\n- chalk\n- 60 m rope\n- helmet\n",
    ),
    (
        "Costs.csv",
        "item,who,eur\nfuel,Ada,38.50\nhut,Bruno,54.00\n",
    ),
    (
        "Approach.gpx",
        "<gpx><trk><name>Approach</name><trkseg><trkpt lat=\"46.5\" lon=\"8.0\"/></trkseg></trk></gpx>\n",
    ),
];
/// What Kourier sends with a typo, and what its edit makes of it (S13).
const TYPO: &str = "Saturday works for me. See you at the trailhaed!";
const FIXED: &str = "Saturday works for me. See you at the trailhead!";
/// Kourier's reply that quotes Ada's photo.
const QUOTE_REPLY: &str = "What a view! Is that the east ridge?";
/// What Kourier sends and then deletes for everyone.
const WRONG_CHAT: &str = "Don't forget: surprise party for Ada on Sunday!";
/// What Kourier writes while the server wants a captcha solved first (S15).
const HELD_BACK: &str = "Running ten minutes late, start without me!";
const GROUP_MEMBERS: [usize; 2] = [0, 1];
/// Oldest Node major version the harness runs on.
const MIN_NODE_MAJOR: u32 = 24;
const NVM_NODE: &str = ".nvm/versions/node/v24.21.0/bin/node";

const BURST: [&str; 20] = [
    "Photo walk on Saturday, who's in?",
    "I was thinking the old harbour first",
    "Then up the hill to the lighthouse",
    "Golden hour is around 18:40",
    "So we should start at 16:30",
    "Bring a tripod if you have one",
    "The wind up there can be wild",
    "I'll pack thermoses with tea",
    "Marta might join with her new camera",
    "Plan B if it rains: the glasshouse",
    "They open until 20:00 on weekends",
    "Tickets are cheaper online",
    "Parking is a pain, take the tram",
    "Line 4 stops right at the harbour",
    "Meet at the fish market clock?",
    "It's hard to miss, big and green",
    "I'll print a little map for everyone",
    "Dinner afterwards at the noodle place?",
    "They have a table for six at 20:30",
    "Let me know by Thursday!",
];

/// `cargo xtask demo` options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DemoArgs {
    /// Where the PNGs go (default `target/shots/demo`).
    pub out: Option<PathBuf>,
    /// Run Kourier on the real display and hand the phone's contacts to the user.
    pub interactive: bool,
}

/// The usage line.
pub const USAGE: &str = "usage: cargo xtask demo [--out <dir>] [--interactive]

  --out <dir>     where the screenshots go (default target/shots/demo); every PNG is also
                  copied to target/shots-progress/d1-<nn>-<what>.png
  --interactive   run Kourier on your display without automation; a prompt sends messages
                  from the mock phone's contacts (no screenshots)";

/// Parses the arguments after `demo`; `Err("")` for `--help`.
pub fn parse_args(args: &[String]) -> Result<DemoArgs, String> {
    let mut out = DemoArgs::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let dir = if arg == "--out" {
            Some(it.next().ok_or("--out needs a directory")?.as_str())
        } else {
            arg.strip_prefix("--out=")
        };
        if let Some(dir) = dir {
            if dir.is_empty() {
                return Err("--out needs a directory".into());
            }
            if out.out.is_some() {
                return Err("--out given twice".into());
            }
            out.out = Some(PathBuf::from(dir));
        } else if arg == "--interactive" {
            out.interactive = true;
        } else if arg == "--help" || arg == "-h" {
            return Err(String::new());
        } else {
            return Err(format!("unknown argument {arg:?}"));
        }
    }
    if out.interactive && out.out.is_some() {
        return Err("--interactive takes no screenshots; drop --out".into());
    }
    Ok(out)
}

/// Runs `cargo xtask demo <args>`.
pub fn demo(args: &[String]) -> Result<()> {
    let args = match parse_args(args) {
        Ok(a) => a,
        Err(e) if e.is_empty() => {
            println!("{USAGE}");
            return Ok(());
        }
        Err(e) => bail!("{e}\n{USAGE}"),
    };
    let node = find_node()?;
    let harness_dir = root().join("tests/mock-harness");
    if !harness_dir
        .join("node_modules/@signalapp/mock-server")
        .is_dir()
    {
        bail!(
            "the mock harness is not installed: cd tests/mock-harness && {} ci",
            node.with_file_name("npm").display()
        );
    }
    run(
        "build kourier (debug, Signal backend)",
        cargo().args(["build", "-p", "kourier", "--features", "signal"]),
    )?;
    let bin = target_dir().join("debug/kourier");
    if !bin.is_file() {
        bail!("{} not found", bin.display());
    }
    // A fresh world every run, on the home disk (not the /tmp tmpfs).
    let env = target_dir().join("demo-env");
    if env.exists() {
        std::fs::remove_dir_all(&env).with_context(|| format!("clear {}", env.display()))?;
    }
    for d in ["home", "tmp", "harness-state"] {
        std::fs::create_dir_all(env.join(d))?;
    }
    let world = World::start(&node, &harness_dir, &env)?;
    if args.interactive {
        interactive(world, &bin, &env)
    } else {
        // Only the default directory is ours to clear; a directory the user names keeps its
        // other files (the run overwrites its own names).
        let (out, clear_out) = match args.out {
            Some(dir) => (dir, false),
            None => (root().join("target/shots/demo"), true),
        };
        scripted(world, &bin, &env, &out, clear_out)
    }
}

// ---- Node ------------------------------------------------------------------------------------

/// The major version in `node --version` output (`v24.21.0`).
fn node_major(version: &str) -> Option<u32> {
    version
        .trim()
        .strip_prefix('v')?
        .split('.')
        .next()?
        .parse()
        .ok()
}

/// Node ≥ 24: `$KOURIER_NODE`, the nvm install, or `node` on `PATH`.
fn find_node() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(n) = std::env::var_os("KOURIER_NODE").filter(|n| !n.is_empty()) {
        candidates.push(PathBuf::from(n));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(Path::new(&home).join(NVM_NODE));
    }
    candidates.push(PathBuf::from("node"));
    let mut tried = Vec::new();
    for node in candidates {
        match Command::new(&node).arg("--version").output() {
            Ok(o) if o.status.success() => match node_major(&String::from_utf8_lossy(&o.stdout)) {
                Some(major) if major >= MIN_NODE_MAJOR => return Ok(node),
                Some(major) => tried.push(format!("{} is Node {major}", node.display())),
                None => tried.push(format!("{}: unknown version", node.display())),
            },
            Ok(o) => tried.push(format!("{}: {}", node.display(), o.status)),
            Err(e) => tried.push(format!("{}: {e}", node.display())),
        }
    }
    bail!(
        "no Node ≥ {MIN_NODE_MAJOR} found ({}); set KOURIER_NODE",
        tried.join("; ")
    )
}

// ---- JSON-lines peers ------------------------------------------------------------------------

/// A child process that speaks the harness/dev-control JSON lines on stdin/stdout (stderr goes
/// to a log file).
struct Peer {
    name: &'static str,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Value>,
    events: VecDeque<Value>,
    next_id: u64,
    log: PathBuf,
}

impl Peer {
    fn spawn(name: &'static str, mut cmd: Command, log: &Path) -> Result<Self> {
        let err = File::create(log).with_context(|| format!("create {}", log.display()))?;
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(err))
            .spawn()
            .with_context(|| format!("start {name}"))?;
        let stdout = child.stdout.take().context("child stdout")?;
        let stdin = child.stdin.take();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // Protocol lines are JSON objects; anything else (Qt noise) is skipped.
                if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        Ok(Self {
            name,
            child,
            stdin,
            lines,
            events: VecDeque::new(),
            next_id: 0,
            log: log.to_owned(),
        })
    }

    fn recv(&mut self, deadline: Instant, what: &str) -> Result<Value> {
        let left = deadline.saturating_duration_since(Instant::now());
        match self.lines.recv_timeout(left) {
            Ok(v) => {
                if v["type"] == "fatal" {
                    bail!(
                        "{} crashed: {} (log: {})",
                        self.name,
                        v["error"],
                        self.log.display()
                    );
                }
                Ok(v)
            }
            Err(RecvTimeoutError::Timeout) => bail!(
                "{}: timed out waiting for {what} (log: {})",
                self.name,
                self.log.display()
            ),
            Err(RecvTimeoutError::Disconnected) => bail!(
                "{} exited while waiting for {what} (log: {})",
                self.name,
                self.log.display()
            ),
        }
    }

    fn keep_event(&mut self, v: Value) {
        if v["event"] == "error" {
            eprintln!("demo: {} reported an error: {}", self.name, v["data"]);
        }
        if self.events.len() >= 1000 {
            self.events.pop_front();
        }
        self.events.push_back(v);
    }

    /// Sends a request and returns its `result`; a failed response is an error.
    fn request(&mut self, cmd: &str, args: Value, timeout: Duration) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let line = json!({"id": id, "cmd": cmd, "args": args});
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("{}: stdin is closed", self.name))?;
        writeln!(stdin, "{line}")
            .and_then(|()| stdin.flush())
            .with_context(|| format!("{}: send {cmd}", self.name))?;
        let deadline = Instant::now() + timeout;
        loop {
            let v = self.recv(deadline, cmd)?;
            if v["type"] == "response" && v["id"] == id {
                if v["ok"] == true {
                    return Ok(v["result"].clone());
                }
                bail!(
                    "{} {cmd} failed: {} ({})",
                    self.name,
                    v["error"]["message"],
                    v["error"]["code"]
                );
            }
            if v["type"] == "event" {
                self.keep_event(v);
            }
        }
    }

    /// The data of the next event `name` whose data `pred` accepts (earlier ones included).
    fn event(
        &mut self,
        name: &str,
        pred: impl Fn(&Value) -> bool,
        timeout: Duration,
    ) -> Result<Value> {
        if let Some(pos) = self
            .events
            .iter()
            .position(|e| e["event"] == name && pred(&e["data"]))
        {
            let e = self.events.remove(pos).unwrap_or_default();
            return Ok(e["data"].clone());
        }
        let deadline = Instant::now() + timeout;
        loop {
            let v = self.recv(deadline, name)?;
            if v["type"] == "event" {
                if v["event"] == name && pred(&v["data"]) {
                    return Ok(v["data"].clone());
                }
                self.keep_event(v);
            }
        }
    }

    /// Waits for the process to exit.
    fn wait_exit(&mut self, timeout: Duration) -> Result<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                bail!("{} did not exit in time", self.name);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        // Closing stdin stops both peers (the harness and the dev channel quit on EOF).
        drop(self.stdin.take());
        if self.wait_exit(Duration::from_secs(5)).is_err() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

// ---- The mock world ----------------------------------------------------------------------------

struct Contact {
    key: &'static str,
    /// The name Kourier shows (contact sync, Storage Service and profile agree on it).
    name: &'static str,
    device_ref: String,
}

struct World {
    harness: Peer,
    handshake: PathBuf,
    phone: String,
    contacts: Vec<Contact>,
    /// The demo group (id and title).
    group: Group,
    /// Our linked device, once linked.
    device: Option<String>,
}

/// The demo group on the mock.
struct Group {
    id: String,
    title: String,
}

const HARNESS_TIMEOUT: Duration = Duration::from_secs(120);

impl World {
    fn start(node: &Path, harness_dir: &Path, env: &Path) -> Result<Self> {
        let mut cmd = Command::new(node);
        cmd.arg("harness.mjs")
            .arg("--state-dir")
            .arg(env.join("harness-state"))
            .current_dir(harness_dir);
        let mut harness = Peer::spawn("harness", cmd, &env.join("harness.log"))?;
        let ready = harness.recv(Instant::now() + Duration::from_secs(60), "the ready line")?;
        if ready["type"] != "ready" {
            bail!("the harness did not start: {ready}");
        }
        let handshake = env.join("handshake.json");
        std::fs::write(&handshake, ready.to_string())?;
        eprintln!(
            "demo: mock server on {}:{}",
            ready["ip"].as_str().unwrap_or("?"),
            ready["port"]
        );
        let mut contacts = Vec::new();
        for (i, (key, name)) in CONTACTS.into_iter().enumerate() {
            let c = harness.request(
                "createPrimary",
                json!({"profileName": name, "avatar": base64(&avatar_png(i))}),
                HARNESS_TIMEOUT,
            )?;
            contacts.push(Contact {
                key,
                name,
                device_ref: str_field(&c, "ref")?,
            });
        }
        let refs: Vec<&str> = contacts.iter().map(|c| c.device_ref.as_str()).collect();
        let phone = harness.request(
            "createPrimary",
            json!({"profileName": PHONE_NAME, "contacts": refs}),
            HARNESS_TIMEOUT,
        )?;
        // The phone's Storage Service state: every chat accepted (Kourier sends only there,
        // architecture §6.7), one pinned; read receipts and typing indicators on.
        let storage_contacts: Vec<Value> = contacts
            .iter()
            .enumerate()
            .map(|(i, c)| {
                json!({"ref": c.device_ref, "whitelisted": true, "pinned": i == PINNED_CONTACT})
            })
            .collect();
        let phone_ref = str_field(&phone, "ref")?;
        // A group the phone administers, with two of the contacts as members and an avatar.
        let members: Vec<&str> = GROUP_MEMBERS
            .iter()
            .map(|&i| contacts[i].device_ref.as_str())
            .collect();
        let group = harness.request(
            "createGroup",
            json!({
                "from": phone_ref,
                "title": GROUP_TITLE,
                "members": members,
                "avatar": base64(&avatar_png(CONTACTS.len())),
            }),
            HARNESS_TIMEOUT,
        )?;
        let group = Group {
            id: str_field(&group, "id")?,
            title: GROUP_TITLE.to_owned(),
        };
        harness.request(
            "setStorageState",
            json!({
                "primary": phone_ref,
                "contacts": storage_contacts,
                "groups": [{"id": group.id, "whitelisted": true}],
                "account": {"readReceipts": true, "typingIndicators": true},
            }),
            HARNESS_TIMEOUT,
        )?;
        Ok(Self {
            harness,
            handshake,
            phone: phone_ref,
            contacts,
            group,
            device: None,
        })
    }

    /// The phone scans the QR code `url`; returns our device's ref.
    fn link(&mut self, url: &str) -> Result<String> {
        let device = self.harness.request(
            "completeProvision",
            json!({"url": url, "primary": self.phone, "timeout": 110_000}),
            HARNESS_TIMEOUT,
        )?;
        let device = str_field(&device, "ref")?;
        self.device = Some(device.clone());
        Ok(device)
    }

    fn device(&self) -> Result<&str> {
        self.device.as_deref().context("not linked yet")
    }

    /// Contact `i` writes to our device.
    fn say(
        &mut self,
        i: usize,
        text: &str,
        sealed: bool,
        at: Option<u64>,
        wait: bool,
    ) -> Result<()> {
        let device = self.device()?.to_owned();
        let mut args = json!({
            "from": self.contacts[i].device_ref,
            "to": device,
            "text": text,
            "sealed": sealed,
            "awaitDelivery": wait,
            "timeout": 60_000,
        });
        if let Some(at) = at {
            args["timestamp"] = json!(at);
        }
        self.harness.request("sendText", args, HARNESS_TIMEOUT)?;
        Ok(())
    }

    /// Contact `i` sends a photo, with a caption or without, to our device (encrypted like
    /// Desktop and stored on the mock's CDN 3), and waits until it was delivered.
    fn send_photo(
        &mut self,
        i: usize,
        png: &[u8],
        size: (u32, u32),
        caption: Option<&str>,
    ) -> Result<()> {
        let device = self.device()?.to_owned();
        let mut args = json!({
            "from": self.contacts[i].device_ref,
            "to": device,
            "attachments": [{
                "data": base64(png),
                "contentType": "image/png",
                "width": size.0,
                "height": size.1,
            }],
            "awaitDelivery": true,
            "timeout": 60_000,
        });
        if let Some(caption) = caption {
            args["text"] = json!(caption);
        }
        self.harness
            .request("sendAttachments", args, HARNESS_TIMEOUT)?;
        Ok(())
    }

    /// Downloads and decrypts, as a phone does, an attachment our device uploaded.
    fn fetch_attachment(&mut self, pointer: &Value) -> Result<Value> {
        self.harness.request(
            "fetchAttachment",
            json!({"pointer": pointer}),
            HARNESS_TIMEOUT,
        )
    }

    /// Member `i` writes `text` to the group (delivered to our device).
    fn group_say(&mut self, i: usize, text: &str, at: Option<u64>) -> Result<()> {
        let device = self.device()?.to_owned();
        let mut args = json!({
            "from": self.contacts[i].device_ref,
            "group": self.group.id,
            "to": [device],
            "text": text,
        });
        if let Some(at) = at {
            args["timestamp"] = json!(at);
        }
        self.harness
            .request("sendGroupText", args, HARNESS_TIMEOUT)?;
        Ok(())
    }

    /// The next group message member `i`'s phone got from our device.
    fn group_received_by(&mut self, i: usize, timeout: Duration) -> Result<Value> {
        let device = self.device()?.to_owned();
        let ms = u64::try_from(timeout.as_millis()).unwrap_or(60_000).max(1);
        self.harness.request(
            "waitForMessage",
            json!({
                "primary": self.contacts[i].device_ref,
                "from": device,
                "group": self.group.id,
                "timeout": ms,
            }),
            timeout + Duration::from_secs(10),
        )
    }

    /// Waits until contact `i`'s phone got a read receipt from our device and checks that it
    /// names exactly the messages sent at `expected` (delivery receipts, which Kourier sends
    /// on its own for sealed messages, are passed over).
    fn read_receipt_by(&mut self, i: usize, expected: &[u64]) -> Result<()> {
        let device = self.device()?.to_owned();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let ms = u64::try_from(left.as_millis()).unwrap_or(60_000).max(1);
            let receipt = self
                .harness
                .request(
                    "waitForReceipt",
                    json!({
                        "primary": self.contacts[i].device_ref,
                        "from": device,
                        "timeout": ms,
                    }),
                    left + Duration::from_secs(10),
                )
                .with_context(|| {
                    format!(
                        "{} never got a read receipt for the group messages",
                        self.contacts[i].name
                    )
                })?;
            if receipt["type"] != "read" {
                continue;
            }
            let mut got: Vec<u64> = receipt["timestamps"]
                .as_array()
                .map(|stamps| stamps.iter().filter_map(Value::as_u64).collect())
                .unwrap_or_default();
            got.sort_unstable();
            if got != expected {
                bail!(
                    "{} got a read receipt for {got:?}, not for their group messages {expected:?}",
                    self.contacts[i].name
                );
            }
            if receipt["sealed"] != true {
                bail!("the group read receipt did not go sealed: {receipt}");
            }
            return Ok(());
        }
    }

    /// The phone removes member `i` from the group and tells our device (a system message).
    fn remove_from_group(&mut self, i: usize) -> Result<()> {
        let device = self.device()?.to_owned();
        let member = self.contacts[i].device_ref.clone();
        self.harness.request(
            "modifyGroup",
            json!({
                "from": self.phone,
                "group": self.group.id,
                "removeMembers": [member],
                "to": [device],
            }),
            HARNESS_TIMEOUT,
        )?;
        Ok(())
    }

    /// Contact `i` reacts with `emoji` to our message sent at `target` (delivered to our
    /// device).
    fn react(&mut self, i: usize, emoji: &str, target: u64) -> Result<()> {
        let device = self.device()?.to_owned();
        self.harness.request(
            "sendData",
            json!({
                "from": self.contacts[i].device_ref,
                "to": device,
                "sealed": true,
                "reaction": {
                    "emoji": emoji,
                    "targetAuthor": self.phone,
                    "targetTimestamp": target,
                },
                "awaitDelivery": true,
                "timeout": 60_000,
            }),
            HARNESS_TIMEOUT,
        )?;
        Ok(())
    }

    /// The next `DataMessage` of `kind` (`message`, `delete`, …) contact `i`'s phone got from
    /// our device.
    fn received_kind_by(&mut self, i: usize, kind: &str, timeout: Duration) -> Result<Value> {
        let device = self.device()?.to_owned();
        let ms = u64::try_from(timeout.as_millis()).unwrap_or(60_000).max(1);
        self.harness.request(
            "waitForMessage",
            json!({
                "primary": self.contacts[i].device_ref, "from": device, "kind": kind,
                "timeout": ms,
            }),
            timeout + Duration::from_secs(10),
        )
    }

    /// The next edit contact `i`'s phone got from our device.
    fn edit_received_by(&mut self, i: usize, timeout: Duration) -> Result<Value> {
        let device = self.device()?.to_owned();
        let ms = u64::try_from(timeout.as_millis()).unwrap_or(60_000).max(1);
        self.harness.request(
            "waitForEdit",
            json!({"primary": self.contacts[i].device_ref, "from": device, "timeout": ms}),
            timeout + Duration::from_secs(10),
        )
    }

    /// Waits until our phone got a sync transcript (`SyncMessage.Sent`) from our device that
    /// `is` what we look for; earlier transcripts are skipped.
    fn phone_saw(&mut self, what: &str, is: impl Fn(&Value) -> bool) -> Result<Value> {
        let device = self.device()?.to_owned();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("our phone never saw {what}");
            }
            let ms = u64::try_from(left.as_millis()).unwrap_or(60_000).max(1);
            let sync = self
                .harness
                .request(
                    "waitForSyncMessage",
                    json!({"primary": self.phone, "from": device, "kind": "sent", "timeout": ms}),
                    left + Duration::from_secs(10),
                )
                .with_context(|| format!("our phone never saw {what}"))?;
            let sent = sync["syncMessage"]["content"]["sent"].clone();
            if is(&sent) {
                return Ok(sent);
            }
        }
    }

    /// Waits until our device acknowledged everything addressed to it.
    fn settle(&mut self) -> Result<()> {
        let device = self.device()?.to_owned();
        self.harness.request(
            "waitForDeliveries",
            json!({"device": device, "timeout": 60_000}),
            HARNESS_TIMEOUT,
        )?;
        Ok(())
    }

    /// The next message contact `i`'s phone got from our device.
    fn received_by(&mut self, i: usize, timeout: Duration) -> Result<Value> {
        let device = self.device()?.to_owned();
        let ms = u64::try_from(timeout.as_millis()).unwrap_or(60_000).max(1);
        self.harness.request(
            "waitForMessage",
            json!({"primary": self.contacts[i].device_ref, "from": device, "timeout": ms}),
            timeout + Duration::from_secs(10),
        )
    }

    fn shutdown(mut self) {
        let _ = self
            .harness
            .request("shutdown", json!({}), Duration::from_secs(10));
    }
}

fn str_field(v: &Value, name: &str) -> Result<String> {
    v[name]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("missing {name:?} in {v}"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

// ---- Kourier -----------------------------------------------------------------------------------

/// How Kourier is shown.
enum Display<'a> {
    /// Offscreen with this config home (its `kdeglobals` picks the color scheme).
    Offscreen(&'a Path),
    /// The user's display and settings.
    Real,
}

fn start_kourier(
    bin: &Path,
    env: &Path,
    world: &World,
    display: Display<'_>,
    tag: &str,
) -> Result<Peer> {
    let mut cmd = Command::new(bin);
    cmd.arg("--dev-control")
        .arg(format!("--size={}x{}", WIDE.0, WIDE.1))
        .env("KOURIER_HOME", env.join("home"))
        .env(
            "KOURIER_SIGNAL_ENV",
            format!("local:{}", world.handshake.display()),
        )
        .env("KOURIER_DEV_KEYRING", "passphrase")
        .env("TMPDIR", env.join("tmp"))
        .env("QT_FORCE_STDERR_LOGGING", "1");
    if let Display::Offscreen(config_home) = display {
        cmd.env("QT_QPA_PLATFORM", "offscreen")
            .env("QT_QUICK_BACKEND", "software")
            .env("QT_QPA_PLATFORMTHEME", "kde")
            .env("QT_QUICK_CONTROLS_STYLE", "org.kde.desktop")
            .env("QT_SCALE_FACTOR", "1")
            .env("XDG_CONFIG_HOME", config_home)
            .env("LANG", "en_US.UTF-8")
            .env("LANGUAGE", "en");
    }
    let mut k = Peer::spawn("kourier", cmd, &env.join(format!("kourier-{tag}.log")))?;
    k.event("channel", |_| true, Duration::from_secs(90))?;
    Ok(k)
}

const UI_TIMEOUT: Duration = Duration::from_secs(120);

/// `wait` on the dev channel.
fn wait(k: &mut Peer, until: &str, timeout: Duration) -> Result<Value> {
    let ms = u64::try_from(timeout.as_millis()).unwrap_or(60_000);
    k.request(
        "wait",
        json!({"until": until, "timeout_ms": ms}),
        timeout + Duration::from_secs(10),
    )
}

fn ui(k: &mut Peer, cmd: &str, args: Value) -> Result<Value> {
    k.request(cmd, args, UI_TIMEOUT)
}

/// Waits until the open conversation shows a media block of the given kind (`"image"` or
/// `"file"`) from us (`own`) or from the peer whose attachment is on this device (`"local"`)
/// and, for a picture, handed to QML to draw (dev-control `state.media`). Fails with what the
/// timeline holds instead when that does not happen in time.
fn wait_media(k: &mut Peer, what: &str, block: &str, own: bool, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let state = ui(k, "state", json!({}))?;
        if media_shown(&state, block, own) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "{what}: the timeline does not show it (media blocks: {})",
                state["media"]
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Whether a dev-control `state` lists a media block of kind `block` in a message of ours
/// (`own`) or the peer's that is on this device and shown.
fn media_shown(state: &Value, block: &str, own: bool) -> bool {
    state["media"].as_array().is_some_and(|media| {
        media.iter().any(|m| {
            m["block"] == block && m["own"] == own && m["state"] == "local" && m["shown"] == true
        })
    })
}

/// Screenshots of one run: numbered, written to `out` and copied to the progress folder.
struct Shots {
    out: PathBuf,
    progress: PathBuf,
    n: u32,
    luma: Vec<(String, i64)>,
}

impl Shots {
    fn new(out: &Path, clear_out: bool) -> Result<Self> {
        std::fs::create_dir_all(out)?;
        let progress = root().join("target/shots-progress");
        std::fs::create_dir_all(&progress)?;
        // Stale shots of an earlier run would be forwarded as new ones; only files named
        // like ours (`<nn>-<what>.png` in the default `out`, `d1-<nn>-<what>.png`) are removed.
        let mut dirs = vec![(progress.as_path(), "d1-")];
        if clear_out {
            dirs.push((out, ""));
        }
        for (dir, prefix) in dirs {
            for entry in std::fs::read_dir(dir)?.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if is_shot_name(&name, prefix) {
                    std::fs::remove_file(entry.path())?;
                }
            }
        }
        Ok(Self {
            out: out.to_owned(),
            progress,
            n: 0,
            luma: Vec::new(),
        })
    }

    /// Resizes to `size`, lets the UI settle, grabs the window into `<nn>-<what>.png`.
    fn take(
        &mut self,
        k: &mut Peer,
        what: &str,
        size: (u32, u32),
        settle_ms: u64,
    ) -> Result<PathBuf> {
        self.take_named(k, what, size, settle_ms, None)
    }

    /// [`Self::take`] that also copies the shot to `target/shots-progress/<e_name>` (a stable
    /// name the lead forwards for a lane's progress), overwritten on re-run.
    fn take_named(
        &mut self,
        k: &mut Peer,
        what: &str,
        size: (u32, u32),
        settle_ms: u64,
        e_name: Option<&str>,
    ) -> Result<PathBuf> {
        self.n += 1;
        let name = format!("{:02}-{what}.png", self.n);
        let path = self.out.join(&name);
        let r = ui(
            k,
            "screenshot",
            json!({"path": path, "size": [size.0, size.1], "settle_ms": settle_ms}),
        )?;
        let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        if bytes.len() < 1000 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            bail!("{} is not a PNG", path.display());
        }
        std::fs::copy(&path, self.progress.join(format!("d1-{name}")))?;
        if let Some(e) = e_name {
            std::fs::copy(&path, self.progress.join(e))?;
        }
        let luma = r["luma"].as_i64().unwrap_or(-1);
        self.luma.push((what.to_owned(), luma));
        eprintln!("demo: {name} ({}x{}, luma {luma})", size.0, size.1);
        Ok(path)
    }

    /// The dark shots must be clearly darker than their light twins (the scheme applied).
    fn check_themes(&self) -> Result<()> {
        let mut bad = Vec::new();
        for (what, light) in &self.luma {
            let Some(base) = what.strip_suffix("-light") else {
                continue;
            };
            let twin = format!("{base}-dark");
            if let Some((_, dark)) = self.luma.iter().find(|(w, _)| *w == twin)
                && *light <= dark + 40
            {
                bad.push(format!("{base}: light {light} vs dark {dark}"));
            }
        }
        if bad.is_empty() {
            Ok(())
        } else {
            bail!("the color scheme did not apply: {}", bad.join(", "))
        }
    }
}

/// Whether `name` is `<prefix><nn>-<what>.png` (two digits).
fn is_shot_name(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix(".png"))
        .is_some_and(|stem| {
            let b = stem.as_bytes();
            b.len() > 3 && b[0].is_ascii_digit() && b[1].is_ascii_digit() && b[2] == b'-'
        })
}

/// The scripted run: two Kourier runs (light, then dark after a restart) against one mock world.
fn scripted(mut world: World, bin: &Path, env: &Path, out: &Path, clear_out: bool) -> Result<()> {
    let themes = env.join("themes");
    let light = theme_home(&themes, "light", "BreezeLight", "breeze")?;
    let dark = theme_home(&themes, "dark", "BreezeDark", "breeze-dark")?;
    let mut shots = Shots::new(out, clear_out)?;

    // -- Run 1 (light): link, receive, reply. --
    let mut k = start_kourier(bin, env, &world, Display::Offscreen(&light), "light")?;
    wait(&mut k, "loaded", Duration::from_secs(90))?;
    shots.take(&mut k, "welcome", WIDE, 1500)?;

    ui(&mut k, "add_account", json!({"network": "Signal"}))?;
    wait(&mut k, "login:input", UI_TIMEOUT)?;
    ui(&mut k, "fill_login", json!({"values": [DEVICE_NAME]}))?;
    shots.take(&mut k, "device-name", WIDE, 800)?;
    ui(&mut k, "submit_login", json!({}))?;
    let qr = k.event("login_qr", |_| true, UI_TIMEOUT)?;
    let url = str_field(&qr, "url")?;
    wait(&mut k, "login:qr", UI_TIMEOUT)?;
    shots.take(&mut k, "link-qr", WIDE, 1000)?;

    eprintln!("demo: the phone scans the QR code");
    world.link(&url)?;
    k.event("login_step", |d| d["kind"] == "done", UI_TIMEOUT)?;
    wait(&mut k, "account_active", UI_TIMEOUT)?;
    eprintln!("demo: linked and online (contacts and Storage Service synced)");

    // Contacts write: Ada and Bruno a few lines (sealed and unsealed sender), Chiara a burst.
    let base = now_ms().saturating_sub(40 * 60_000);
    let script: [(usize, &str, bool, u64); 5] = [
        (
            0,
            "Hi! Is this the new desktop app you told me about?",
            false,
            0,
        ),
        (0, "Looks like Signal, but native. Nice!", true, 60_000),
        (1, "Are we still on for Friday?", true, 9 * 60_000),
        (1, "I'll bring the board games.", false, 10 * 60_000),
        (0, "Send me a screenshot later 🙂", false, 14 * 60_000),
    ];
    for (who, text, sealed, offset) in script {
        world.say(who, text, sealed, Some(base + offset), true)?;
    }
    for (i, text) in BURST.iter().enumerate() {
        let at = base + 20 * 60_000 + u64::try_from(i).unwrap_or(0) * 30_000;
        world.say(BURST_CONTACT, text, i % 3 != 1, Some(at), false)?;
    }
    // The group gets a short exchange too (two members write).
    let group_base = base + 16 * 60_000;
    world.group_say(
        GROUP_MEMBERS[0],
        "Booked the cabin for the climbing weekend!",
        Some(group_base),
    )?;
    world.group_say(
        GROUP_MEMBERS[1],
        "Perfect. I'll sort out the ropes and helmets.",
        Some(group_base + 60_000),
    )?;
    world.group_say(
        GROUP_MEMBERS[0],
        "See you all at the trailhead at 8.",
        Some(group_base + 120_000),
    )?;
    world.settle()?;
    eprintln!(
        "demo: {} messages delivered (incl. the group)",
        5 + BURST.len() + 3
    );

    let burst_name = world.contacts[BURST_CONTACT].name;
    let group_title = world.group.title.clone();
    // All four chats: the group is listed from the Storage Service sync before it has a
    // message, so three would be met by the first two contacts and the group while the
    // burst contact's messages are still on their way from the backend to the list (the
    // server counts a message as delivered once the backend has stored it).
    let chats = CONTACTS.len() + 1;
    wait(
        &mut k,
        &format!("conversations_at_least:{chats}"),
        UI_TIMEOUT,
    )?;
    ui(&mut k, "page", json!({"name": "list"}))?;
    shots.take(&mut k, "list-wide-light", WIDE, 1200)?;
    ui(&mut k, "open", json!({"title": burst_name}))?;
    wait(
        &mut k,
        &format!("messages_at_least:{}", BURST.len()),
        UI_TIMEOUT,
    )?;
    shots.take(&mut k, "chat-wide-light", WIDE, 1200)?;
    shots.take(&mut k, "chat-narrow-light", NARROW, 1200)?;
    ui(&mut k, "page", json!({"name": "list"}))?;
    shots.take(&mut k, "list-narrow-light", NARROW, 1000)?;

    // Kourier replies; the contact's phone must get it.
    ui(&mut k, "page", json!({"name": "chat"}))?;
    ui(&mut k, "send", json!({"text": REPLY}))?;
    let got = world.received_by(BURST_CONTACT, Duration::from_secs(60))?;
    if got["text"] != REPLY {
        bail!("the contact's phone got another message: {}", got["text"]);
    }
    eprintln!(
        "demo: the contact's phone received the reply ({} envelope)",
        got["envelopeType"]
    );
    let state = wait(&mut k, "outgoing_state:Sent", UI_TIMEOUT)?;
    eprintln!("demo: reply state {}", state["observed"]);
    shots.take(&mut k, "reply-sent-narrow-light", NARROW, 1000)?;
    shots.take(&mut k, "reply-sent-wide-light", WIDE, 1200)?;

    // The group chat: title, members and the exchange, with each author.
    ui(&mut k, "page", json!({"name": "list"}))?;
    ui(&mut k, "open", json!({"title": group_title}))?;
    wait(&mut k, "messages_at_least:3", UI_TIMEOUT)?;
    shots.take_named(
        &mut k,
        "group-chat-wide-light",
        WIDE,
        1200,
        Some("e-signal-10-group-chat-light.png"),
    )?;
    shots.take(&mut k, "group-chat-narrow-light", NARROW, 1200)?;

    // Opening the group read its three messages: each author's phone hears of their own
    // messages, as Signal Desktop tells it (a sealed read receipt per author).
    world.read_receipt_by(GROUP_MEMBERS[0], &[group_base, group_base + 120_000])?;
    world.read_receipt_by(GROUP_MEMBERS[1], &[group_base + 60_000])?;
    eprintln!("demo: both authors got a read receipt for their group messages");

    // Kourier replies in the group: one sender-key message reaches every member's phone.
    ui(&mut k, "send", json!({"text": GROUP_REPLY}))?;
    for i in GROUP_MEMBERS {
        let got = world.group_received_by(i, Duration::from_secs(60))?;
        if got["text"] != GROUP_REPLY {
            bail!("a member's phone got another message: {}", got["text"]);
        }
        if got["messageType"] != "SenderKey" || got["sealed"] != true {
            bail!(
                "the group reply did not arrive as a sealed sender-key message: {} / {}",
                got["envelopeType"],
                got["messageType"]
            );
        }
        eprintln!(
            "demo: {} received the group reply ({} envelope, {} inside)",
            world.contacts[i].name, got["envelopeType"], got["messageType"]
        );
    }
    let state = wait(&mut k, "outgoing_state:Sent", UI_TIMEOUT)?;
    eprintln!("demo: group reply state {}", state["observed"]);
    shots.take_named(
        &mut k,
        "group-reply-wide-light",
        WIDE,
        1200,
        Some("f-signal-01-group-reply-light.png"),
    )?;
    shots.take_named(
        &mut k,
        "group-reply-narrow-light",
        NARROW,
        1200,
        Some("f-signal-02-group-reply-narrow-light.png"),
    )?;

    // Attachments (S12): Ada sends a photo, Kourier shows it and sends her a file back.
    let ada_name = world.contacts[0].name;
    let photo = scene_png(SCENE.0, SCENE.1);
    world.send_photo(0, &photo, SCENE, Some(PHOTO_CAPTION))?;
    world.settle()?;
    eprintln!("demo: Ada's photo delivered ({} bytes)", photo.len());
    ui(&mut k, "page", json!({"name": "list"}))?;
    ui(&mut k, "open", json!({"title": ada_name}))?;
    wait(&mut k, "messages_at_least:4", UI_TIMEOUT)?;
    // Downloaded by the core's policy, decrypted into the blob store, drawn from there.
    wait_media(&mut k, "Ada's photo", "image", false, UI_TIMEOUT)?;
    eprintln!("demo: Ada's photo is in the timeline");
    shots.take_named(
        &mut k,
        "photo-received-wide-light",
        WIDE,
        3000,
        Some("f-signal-04-photo-received-light.png"),
    )?;
    let plan = env.join("Route plan.txt");
    std::fs::write(&plan, ROUTE_PLAN)?;
    ui(&mut k, "attach", json!({"path": plan}))?;
    ui(&mut k, "send", json!({"text": FILE_CAPTION}))?;
    let got = world.received_by(0, Duration::from_secs(60))?;
    if got["text"] != FILE_CAPTION {
        bail!("Ada's phone got another message: {}", got["text"]);
    }
    let pointer = got["dataMessage"]["attachments"]
        .get(0)
        .cloned()
        .context("the file message has no attachment")?;
    let fetched = world.fetch_attachment(&pointer)?;
    if fetched["data"] != base64(ROUTE_PLAN.as_bytes())
        || fetched["macValid"] != true
        || fetched["digestValid"] != true
    {
        bail!("the file did not arrive intact on Ada's phone: {fetched}");
    }
    eprintln!("demo: Ada's phone decrypted the file byte for byte");
    let state = wait(&mut k, "outgoing_state:Sent", UI_TIMEOUT)?;
    eprintln!("demo: file message state {}", state["observed"]);
    wait_media(&mut k, "the sent file", "file", true, UI_TIMEOUT)?;
    shots.take_named(
        &mut k,
        "file-sent-wide-light",
        WIDE,
        2000,
        Some("f-signal-05-file-sent-light.png"),
    )?;
    shots.take_named(
        &mut k,
        "file-sent-narrow-light",
        NARROW,
        2000,
        Some("f-signal-06-media-narrow-light.png"),
    )?;
    files_scene(&mut world, &mut k, &mut shots, env)?;
    modifiers_scene(&mut world, &mut k, &mut shots)?;
    ui(&mut k, "quit", json!({}))?;
    let status = k.wait_exit(Duration::from_secs(30))?;
    if !status.success() {
        bail!("kourier exited with {status} (log: {})", k.log.display());
    }
    drop(k);

    // -- Run 2 (dark): restart on the same profile, reconnect, receive live. --
    let mut k = start_kourier(bin, env, &world, Display::Offscreen(&dark), "dark")?;
    wait(&mut k, "loaded", Duration::from_secs(90))?;
    wait(&mut k, "account_active", UI_TIMEOUT)?;
    world.say(
        0,
        "Back online? This one arrived after the restart.",
        true,
        None,
        true,
    )?;
    wait(&mut k, "conversations_at_least:3", UI_TIMEOUT)?;
    ui(&mut k, "page", json!({"name": "list"}))?;
    shots.take(&mut k, "list-wide-dark", WIDE, 1500)?;
    ui(&mut k, "open", json!({"title": burst_name}))?;
    wait(
        &mut k,
        &format!("messages_at_least:{}", BURST.len() + 1),
        UI_TIMEOUT,
    )?;
    wait(&mut k, "outgoing_state:Sent", UI_TIMEOUT)?;
    shots.take(&mut k, "chat-wide-dark", WIDE, 1200)?;
    shots.take(&mut k, "chat-narrow-dark", NARROW, 1200)?;
    ui(&mut k, "page", json!({"name": "list"}))?;
    shots.take(&mut k, "list-narrow-dark", NARROW, 1000)?;
    // Ada's chat: sealed and unsealed sender, and the message that arrived after the restart.
    ui(&mut k, "open", json!({"title": ada_name}))?;
    wait(&mut k, "messages_at_least:6", UI_TIMEOUT)?;
    // Both survive the restart: the photo comes back out of the encrypted blob store.
    wait_media(
        &mut k,
        "Ada's photo after the restart",
        "image",
        false,
        UI_TIMEOUT,
    )?;
    wait_media(
        &mut k,
        "the sent file after the restart",
        "file",
        true,
        UI_TIMEOUT,
    )?;
    shots.take_named(
        &mut k,
        "chat-after-restart-wide-dark",
        WIDE,
        2500,
        Some("f-signal-07-media-after-restart-dark.png"),
    )?;
    shots.take_named(
        &mut k,
        "chat-after-restart-narrow-dark",
        NARROW,
        2000,
        Some("f-signal-08-media-narrow-dark.png"),
    )?;
    // The reaction, the edit, the quote and the tombstone are all still there.
    shots.take_named(
        &mut k,
        "modifiers-after-restart-wide-dark",
        WIDE,
        1500,
        Some("g-signal-06-modifiers-after-restart-dark.png"),
    )?;

    // The group in dark: a live member message, then the phone removes a member (a system row).
    world.group_say(
        GROUP_MEMBERS[1],
        "Weather looks clear for Saturday. 🧗",
        None,
    )?;
    world.remove_from_group(GROUP_MEMBERS[1])?;
    world.settle()?;
    ui(&mut k, "page", json!({"name": "list"}))?;
    ui(&mut k, "open", json!({"title": group_title}))?;
    wait(&mut k, "messages_at_least:5", UI_TIMEOUT)?;
    shots.take_named(
        &mut k,
        "group-chat-wide-dark",
        WIDE,
        1400,
        Some("f-signal-03-group-chat-dark.png"),
    )?;
    robustness_scene(&mut world, &mut k, &mut shots)?;
    ui(&mut k, "quit", json!({}))?;
    let status = k.wait_exit(Duration::from_secs(30))?;
    if !status.success() {
        bail!("kourier exited with {status} (log: {})", k.log.display());
    }
    drop(k);
    world.shutdown();

    shots.check_themes()?;
    report_qml_warnings(env);
    eprintln!(
        "demo: {} screenshots in {} (copies in target/shots-progress/d1-*.png)",
        shots.n,
        out.display()
    );
    Ok(())
}

/// The message rows of the open conversation as Kourier shows them, newest first (dev-control
/// `state.rows`: marks only, no texts).
fn rows(k: &mut Peer) -> Result<Vec<Value>> {
    let state = ui(k, "state", json!({}))?;
    state["rows"]
        .as_array()
        .cloned()
        .context("dev-control state has no \"rows\" (the row marks of the open chat)")
}

/// Waits until the rows of the open conversation satisfy `shows`; fails with `what` and the
/// rows Kourier has instead when they never do.
fn wait_rows(k: &mut Peer, what: &str, shows: impl Fn(&[Value]) -> bool) -> Result<Vec<Value>> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let rows = rows(k)?;
        if shows(&rows) {
            return Ok(rows);
        }
        if Instant::now() >= deadline {
            let newest: Vec<&Value> = rows.iter().take(6).collect();
            bail!("Kourier's timeline does not show {what} (newest rows: {newest:?})");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Many files in one go (architecture §1.1 goal 2, §3.8): the user picks two pictures and
/// three documents and sends them with one caption. Signal's clients show a message's
/// pictures as an album and only the first of any other files, so Kourier sends the album
/// (with the caption) and each document as a message of its own. Fails unless the composer
/// announces the four messages, Ada's phone gets exactly the album with both pictures and
/// then each document, intact and in the order picked, and Kourier's timeline shows the same
/// four messages as sent.
fn files_scene(world: &mut World, k: &mut Peer, shots: &mut Shots, env: &Path) -> Result<()> {
    let minute = Duration::from_secs(60);
    let dir = env.join("picked");
    std::fs::create_dir_all(&dir)?;
    let mut paths = Vec::new();
    for (name, size) in [
        ("Wall, left.png", (800, 600)),
        ("Wall, right.png", (600, 800)),
    ] {
        let path = dir.join(name);
        std::fs::write(&path, scene_png(size.0, size.1))?;
        paths.push(path);
    }
    for (name, content) in DOCUMENTS {
        let path = dir.join(name);
        std::fs::write(&path, content)?;
        paths.push(path);
    }
    let added = ui(k, "attach", json!({"paths": paths}))?;
    if added["attachments"] != 5 {
        bail!("the composer did not take all five files: {added}");
    }
    let planned = wait(k, "files_planned", UI_TIMEOUT)?;
    let state = ui(k, "state", json!({}))?;
    let composer = &state["composer"];
    if composer["files"] != 5 || composer["refused"] != 0 || composer["messages"] != 4 {
        bail!(
            "the composer does not announce five files as four messages: {composer} ({})",
            planned["observed"]
        );
    }
    eprintln!("demo: the composer holds 5 files and announces 4 messages");
    shots.take_named(
        k,
        "files-picked-wide-light",
        WIDE,
        1500,
        Some("h-signal-01-files-picked-light.png"),
    )?;
    ui(k, "send", json!({"text": FILES_CAPTION}))?;

    // Ada's phone: the album first, with the caption and both pictures.
    let intact = |world: &mut World, pointer: &Value, what: &str| -> Result<Value> {
        let fetched = world.fetch_attachment(pointer)?;
        if fetched["macValid"] != true || fetched["digestValid"] != true {
            bail!("{what} did not arrive intact on Ada's phone: {fetched}");
        }
        Ok(fetched)
    };
    let album = world.received_kind_by(0, "message", minute)?;
    let pictures = album["dataMessage"]["attachments"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if album["text"] != FILES_CAPTION || pictures.len() != 2 {
        bail!(
            "Ada's phone did not get the album first: {} attachment(s), text {}",
            pictures.len(),
            album["text"]
        );
    }
    for (n, picture) in pictures.iter().enumerate() {
        if !picture["contentType"]
            .as_str()
            .is_some_and(|t| t.starts_with("image/"))
        {
            bail!("the album holds something that is no picture: {picture}");
        }
        intact(world, picture, &format!("picture {}", n + 1))?;
    }
    let mut last_at = album["timestamp"].as_u64().unwrap_or(0);
    // Then each document, alone, in the order picked, byte for byte.
    for (name, content) in DOCUMENTS {
        let got = world.received_kind_by(0, "message", minute)?;
        let files = got["dataMessage"]["attachments"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if files.len() != 1 || files[0]["fileName"] != name || got["text"] != "" {
            bail!(
                "Ada's phone did not get \"{name}\" as a message of its own: {}",
                got["dataMessage"]["attachments"]
            );
        }
        let fetched = intact(world, &files[0], name)?;
        if fetched["data"] != base64(content.as_bytes()) {
            bail!("\"{name}\" is not the same file on Ada's phone");
        }
        let at = got["timestamp"].as_u64().unwrap_or(0);
        if at <= last_at {
            bail!("\"{name}\" did not arrive after the message before it");
        }
        last_at = at;
    }
    if let Ok(more) = world.received_kind_by(0, "message", Duration::from_millis(1500)) {
        bail!("Ada's phone got a fifth message: {}", more["dataMessage"]);
    }
    eprintln!("demo: Ada's phone got the album (2 pictures) and each of the 3 documents");

    // Kourier's own timeline: the same four messages, all sent.
    wait(k, "none_pending", UI_TIMEOUT)?;
    let sent = |row: &Value| matches!(row["state"].as_str(), Some("sent" | "delivered" | "read"));
    let shown = wait_rows(k, "the album and the three documents as sent", |rows| {
        let own: Vec<&Value> = rows.iter().filter(|r| r["own"] == true).take(4).collect();
        own.len() == 4
            && own.iter().all(|r| sent(r))
            && own[3]["attachments"] == 2
            && own[..3].iter().all(|r| r["attachments"] == 1)
    })?;
    let own: Vec<&Value> = shown.iter().filter(|r| r["own"] == true).take(4).collect();
    eprintln!(
        "demo: Kourier shows the album ({}) and the documents ({}, {}, {})",
        own[3]["kinds"], own[2]["kinds"], own[1]["kinds"], own[0]["kinds"]
    );
    wait_media(k, "the sent pictures", "image", true, UI_TIMEOUT)?;
    shots.take_named(
        k,
        "files-sent-wide-light",
        WIDE,
        2500,
        Some("h-signal-02-files-sent-light.png"),
    )?;
    shots.take_named(
        k,
        "files-sent-narrow-light",
        NARROW,
        2000,
        Some("h-signal-03-files-sent-narrow-light.png"),
    )?;
    Ok(())
}

/// S15 with the real app (last scene: it ends with the device unlinked). The server sets a send
/// challenge for Ada's chat: Kourier's message waits and the account banner asks for the
/// verification. The demo has no browser to solve a captcha in, so the server stops asking and
/// drops the socket; Kourier reconnects by itself and the message goes out. Then the phone
/// unlinks this device: the banner says so and offers to link again, the chats stay readable.
/// Fails unless the app reports both states, Ada's phone gets nothing while the challenge
/// stands, and gets the held message once it is lifted.
fn robustness_scene(world: &mut World, k: &mut Peer, shots: &mut Shots) -> Result<()> {
    let minute = Duration::from_secs(60);
    let device = world.device()?.to_owned();
    let ada = world.contacts[0].device_ref.clone();
    let ada_name = world.contacts[0].name;
    ui(k, "page", json!({"name": "list"}))?;
    ui(k, "open", json!({"title": ada_name}))?;
    wait(k, "messages_at_least:3", UI_TIMEOUT)?;

    world.harness.request(
        "challenge",
        json!({"target": ada, "token": "demo-challenge"}),
        HARNESS_TIMEOUT,
    )?;
    ui(k, "send", json!({"text": HELD_BACK}))?;
    k.event("account_state", |d| d["conn"] == "needs_action", minute)?;
    if let Ok(got) = world.received_kind_by(0, "message", Duration::from_millis(1500))
        && got["text"] == HELD_BACK
    {
        bail!("the message left while the server asked for a challenge");
    }
    eprintln!("demo: the send challenge pauses the outbox");
    shots.take_named(
        k,
        "challenge-wide-dark",
        WIDE,
        1500,
        Some("g-signal-07-challenge-dark.png"),
    )?;
    world.harness.request(
        "challenge",
        json!({"target": ada, "stop": true}),
        HARNESS_TIMEOUT,
    )?;
    world.harness.request(
        "closeSocket",
        json!({"device": device, "code": 1001}),
        HARNESS_TIMEOUT,
    )?;
    let got = world.received_kind_by(0, "message", minute)?;
    if got["text"] != HELD_BACK {
        bail!("Ada's phone got another message: {}", got["text"]);
    }
    wait(k, "outgoing_state:Sent", UI_TIMEOUT)?;
    eprintln!("demo: reconnected after the server dropped the socket; the held message went out");
    world.settle()?;

    let phone = world.phone.clone();
    world.harness.request(
        "unlink",
        json!({"primary": phone, "device": device}),
        HARNESS_TIMEOUT,
    )?;
    k.event("account_state", |d| d["state"] == "logged_out", minute)?;
    eprintln!("demo: unlinked from the phone; the account is logged out");
    shots.take_named(
        k,
        "logged-out-chat-wide-dark",
        WIDE,
        1500,
        Some("g-signal-08-logged-out-dark.png"),
    )?;
    ui(k, "page", json!({"name": "list"}))?;
    shots.take_named(
        k,
        "logged-out-list-narrow-dark",
        NARROW,
        1200,
        Some("g-signal-09-logged-out-narrow-dark.png"),
    )?;
    Ok(())
}

/// S13 in Ada's chat (open in `k`): Ada reacts to a message of Kourier's, Kourier corrects its
/// typo, replies to her photo with a quote, and deletes a message it sent to the wrong chat
/// for everyone. Fails unless Ada's phone and our own phone see each of Kourier's three.
fn modifiers_scene(world: &mut World, k: &mut Peer, shots: &mut Shots) -> Result<()> {
    let minute = Duration::from_secs(60);
    let stamp = |v: &Value, what: &str| -> Result<u64> {
        v.as_u64()
            .with_context(|| format!("{what} has no timestamp"))
    };

    // Kourier writes (with a typo); Ada reacts to it.
    ui(k, "send", json!({"text": TYPO}))?;
    let got = world.received_kind_by(0, "message", minute)?;
    if got["text"] != TYPO {
        bail!("Ada's phone got another message: {}", got["text"]);
    }
    let typo_at = stamp(&got["timestamp"], "the message")?;
    wait(k, "outgoing_state:Sent", UI_TIMEOUT)?;
    world.react(0, "❤️", typo_at)?;
    world.settle()?;
    wait_rows(k, "Ada's reaction on our newest message", |rows| {
        rows.iter().find(|r| r["own"] == true).is_some_and(|row| {
            row["reactions"].as_array().is_some_and(|chips| {
                chips.len() == 1
                    && chips[0]["emoji"] == "❤️"
                    && chips[0]["count"] == 1
                    && chips[0]["mine"] == false
            })
        })
    })?;
    eprintln!("demo: Ada reacted to Kourier's message, and Kourier shows her reaction");
    shots.take_named(
        k,
        "reaction-wide-light",
        WIDE,
        1500,
        Some("g-signal-01-reaction-light.png"),
    )?;

    // Kourier corrects the typo: an edit that names the message it replaces.
    ui(
        k,
        "act",
        json!({"on": "own", "action": "edit", "text": FIXED}),
    )?;
    let edit = world.edit_received_by(0, minute)?;
    if edit["text"] != FIXED || edit["targetTimestamp"] != typo_at {
        bail!(
            "Ada's phone got another edit: target {} (expected {typo_at})",
            edit["targetTimestamp"]
        );
    }
    world.phone_saw("the edit", |sent| {
        sent["editMessage"]["targetSentTimestamp"] == typo_at
            && sent["editMessage"]["dataMessage"]["body"] == FIXED
    })?;
    wait_rows(k, "the edited mark on the corrected message", |rows| {
        rows.iter().find(|r| r["own"] == true).is_some_and(|row| {
            row["edited"] == true
                && row["deleted"] == false
                && row["reactions"]
                    .as_array()
                    .is_some_and(|chips| chips.len() == 1)
        })
    })?;
    eprintln!("demo: Ada's phone and our phone received the edit; Kourier marks it as edited");
    shots.take_named(
        k,
        "edited-wide-light",
        WIDE,
        1500,
        Some("g-signal-02-edited-light.png"),
    )?;

    // Kourier replies to Ada's photo: the quote names her message and shows its caption.
    ui(
        k,
        "act",
        json!({"on": "other", "action": "reply", "text": QUOTE_REPLY}),
    )?;
    let got = world.received_kind_by(0, "message", minute)?;
    let quote = &got["dataMessage"]["quote"];
    if got["text"] != QUOTE_REPLY || quote["text"] != PHOTO_CAPTION || !quote["id"].is_u64() {
        bail!(
            "Ada's phone got the reply without its quote: {}",
            got["text"]
        );
    }
    let quoted_file = &quote["attachments"][0];
    if !quoted_file["contentType"]
        .as_str()
        .is_some_and(|t| t.starts_with("image/"))
    {
        bail!("the quote does not name the quoted photo");
    }
    let thumbnail = if quoted_file["thumbnail"].is_object() {
        "with its thumbnail"
    } else {
        "without a thumbnail"
    };
    let quoted_at = stamp(&quote["id"], "the quote")?;
    world.phone_saw("the quote", |sent| {
        sent["message"]["quote"]["id"] == quoted_at && sent["message"]["body"] == QUOTE_REPLY
    })?;
    wait(k, "outgoing_state:Sent", UI_TIMEOUT)?;
    wait_rows(k, "the quote of Ada's photo above our reply", |rows| {
        rows.iter()
            .find(|r| r["own"] == true)
            .is_some_and(|row| row["quote"] == "known" && row["text"] == true)
    })?;
    eprintln!(
        "demo: Ada's phone and our phone received the reply quoting her photo ({thumbnail}); \
         Kourier shows the quote"
    );
    shots.take_named(
        k,
        "quote-wide-light",
        WIDE,
        1500,
        Some("g-signal-03-quote-light.png"),
    )?;

    // A message to the wrong chat, deleted for everyone.
    ui(k, "send", json!({"text": WRONG_CHAT}))?;
    let got = world.received_kind_by(0, "message", minute)?;
    if got["text"] != WRONG_CHAT {
        bail!("Ada's phone got another message: {}", got["text"]);
    }
    let wrong_at = stamp(&got["timestamp"], "the message")?;
    wait(k, "outgoing_state:Sent", UI_TIMEOUT)?;
    ui(
        k,
        "act",
        json!({"on": "own", "action": "delete", "for_everyone": true}),
    )?;
    let delete = world.received_kind_by(0, "delete", minute)?;
    if delete["dataMessage"]["delete"]["targetSentTimestamp"] != wrong_at {
        bail!("Ada's phone got a delete for another message");
    }
    world.phone_saw("the delete", |sent| {
        sent["message"]["delete"]["targetSentTimestamp"] == wrong_at
    })?;
    wait_rows(k, "the tombstone of the deleted message", |rows| {
        let own: Vec<&Value> = rows.iter().filter(|r| r["own"] == true).take(3).collect();
        // Newest first: the tombstone, the reply with its quote, the edited message with
        // Ada's reaction. All four marks at once.
        own.len() == 3
            && own[0]["deleted"] == true
            && own[1]["quote"] == "known"
            && own[2]["edited"] == true
            && own[2]["reactions"]
                .as_array()
                .is_some_and(|chips| chips.len() == 1)
    })?;
    eprintln!(
        "demo: Ada's phone and our phone received the delete for everyone; Kourier shows the \
         tombstone, the quote, the edited mark and the reaction"
    );
    shots.take_named(
        k,
        "deleted-wide-light",
        WIDE,
        1500,
        Some("g-signal-04-deleted-light.png"),
    )?;
    shots.take_named(
        k,
        "modifiers-narrow-light",
        NARROW,
        1500,
        Some("g-signal-05-modifiers-narrow-light.png"),
    )?;
    Ok(())
}

/// Lists the QML warnings of the Kourier runs (they do not fail the demo; `cargo xtask shots`
/// is the strict check).
fn report_qml_warnings(env: &Path) {
    let mut lines: Vec<String> = ["light", "dark"]
        .iter()
        .filter_map(|tag| std::fs::read_to_string(env.join(format!("kourier-{tag}.log"))).ok())
        .flat_map(|log| {
            super::qml_problems(&log)
                .into_iter()
                .filter_map(|l| l.split_once(" qt: ").map(|(_, w)| w.to_owned()))
                .collect::<Vec<_>>()
        })
        .collect();
    if lines.is_empty() {
        return;
    }
    let total = lines.len();
    lines.sort();
    lines.dedup();
    eprintln!("demo: {total} QML warning(s) in the Kourier logs:");
    for l in lines {
        eprintln!("  {l}");
    }
}

// ---- Interactive --------------------------------------------------------------------------------

/// A line typed at the interactive prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplCommand {
    /// `<contact> <text>` (or `sealed <contact> <text>`): the contact writes to Kourier.
    Say {
        /// Contact key prefix.
        contact: String,
        /// The text.
        text: String,
        /// Sealed sender.
        sealed: bool,
    },
    /// `burst <contact> [n]`: n numbered messages (default 20).
    Burst {
        /// Contact key prefix.
        contact: String,
        /// How many.
        n: u32,
    },
    /// `photo <contact> [caption]`: the contact sends a photo.
    Photo {
        /// Contact key prefix.
        contact: String,
        /// The caption, if any.
        caption: Option<String>,
    },
    /// `inbox`: messages Kourier sent to the contacts.
    Inbox,
    /// `contacts`.
    Contacts,
    /// `help`.
    Help,
    /// `quit` / `exit`.
    Quit,
    /// An empty line.
    Nothing,
}

const REPL_HELP: &str =
    "  <contact> <text>          the contact writes to Kourier (e.g. `ada hello there`)
  sealed <contact> <text>   the same with sealed sender
  burst <contact> [n]       n numbered messages at once (default 20, at most 500)
  photo <contact> [caption] the contact sends a photo
  inbox                     what Kourier sent to the contacts' phones (files are downloaded
                            and decrypted as a phone does)
  contacts                  the contacts
  quit                      stop (Kourier and the mock server)";

/// Parses a prompt line.
pub fn parse_repl(line: &str) -> Result<ReplCommand, String> {
    let line = line.trim();
    let (word, rest) = match line.split_once(char::is_whitespace) {
        Some((w, r)) => (w, r.trim()),
        None => (line, ""),
    };
    let contact_and_text = |s: &str| -> Result<(String, String), String> {
        match s.split_once(char::is_whitespace) {
            Some((c, t)) if !t.trim().is_empty() => Ok((c.to_lowercase(), t.trim().to_owned())),
            _ => Err("expected <contact> <text>".into()),
        }
    };
    Ok(match word.to_lowercase().as_str() {
        "" => ReplCommand::Nothing,
        "quit" | "exit" => ReplCommand::Quit,
        "help" | "?" => ReplCommand::Help,
        "inbox" => ReplCommand::Inbox,
        "contacts" => ReplCommand::Contacts,
        "sealed" => {
            let (contact, text) = contact_and_text(rest)?;
            ReplCommand::Say {
                contact,
                text,
                sealed: true,
            }
        }
        "photo" => {
            let (contact, caption) = match rest.split_once(char::is_whitespace) {
                Some((c, t)) => (c, Some(t.trim().to_owned()).filter(|t| !t.is_empty())),
                None => (rest, None),
            };
            if contact.is_empty() {
                return Err("expected photo <contact> [caption]".into());
            }
            ReplCommand::Photo {
                contact: contact.to_lowercase(),
                caption,
            }
        }
        "burst" => {
            let mut parts = rest.split_whitespace();
            let contact = parts
                .next()
                .ok_or("expected burst <contact> [n]")?
                .to_lowercase();
            let n = match parts.next() {
                None => 20,
                Some(n) => n
                    .parse::<u32>()
                    .ok()
                    .filter(|n| (1..=500).contains(n))
                    .ok_or("n must be 1 … 500")?,
            };
            if parts.next().is_some() {
                return Err("expected burst <contact> [n]".into());
            }
            ReplCommand::Burst { contact, n }
        }
        _ => {
            let (contact, text) = contact_and_text(line)?;
            ReplCommand::Say {
                contact,
                text,
                sealed: false,
            }
        }
    })
}

/// The contact whose key starts with `prefix`.
fn find_contact(world: &World, prefix: &str) -> Result<usize> {
    let hits: Vec<usize> = (0..world.contacts.len())
        .filter(|i| world.contacts[*i].key.starts_with(prefix))
        .collect();
    match hits.as_slice() {
        [i] => Ok(*i),
        [] => bail!("no contact {prefix:?} (try `contacts`)"),
        _ => bail!("{prefix:?} matches several contacts"),
    }
}

fn interactive(mut world: World, bin: &Path, env: &Path) -> Result<()> {
    let mut k = start_kourier(bin, env, &world, Display::Real, "interactive")?;
    println!(
        "\nKourier is open on your display with a fresh profile, connected to the local mock server
only. Add the account: Signal → Continue (any device name). The mock phone scans the QR
code by itself.\n"
    );
    let qr = k.event("login_qr", |_| true, Duration::from_secs(30 * 60))?;
    let url = str_field(&qr, "url")?;
    println!("demo: the phone scans the QR code…");
    world.link(&url)?;
    k.event(
        "account_state",
        |d| d["state"] == "active" && d["conn"] == "online",
        UI_TIMEOUT,
    )?;
    let names: Vec<String> = world
        .contacts
        .iter()
        .map(|c| format!("{} ({})", c.key, c.name))
        .collect();
    world.say(
        0,
        "Hi! Kourier is linked to the mock phone.",
        false,
        None,
        true,
    )?;
    world.say(
        1,
        "Write back from Kourier, I'll see it in `inbox`.",
        true,
        None,
        true,
    )?;
    println!("demo: linked. Contacts: {}.\n{REPL_HELP}", names.join(", "));

    let (tx, lines) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    print!("> ");
    let _ = std::io::stdout().flush();
    loop {
        if !k.running() {
            println!("\ndemo: Kourier was closed");
            break;
        }
        let line = match lines.recv_timeout(Duration::from_millis(300)) {
            Ok(l) => l,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match parse_repl(&line) {
            Ok(ReplCommand::Quit) => break,
            Ok(cmd) => {
                if let Err(e) = repl_step(&mut world, cmd) {
                    println!("error: {e:#}");
                }
            }
            Err(e) => println!("error: {e} (type `help`)"),
        }
        print!("> ");
        let _ = std::io::stdout().flush();
    }
    drop(k);
    world.shutdown();
    Ok(())
}

fn repl_step(world: &mut World, cmd: ReplCommand) -> Result<()> {
    match cmd {
        ReplCommand::Say {
            contact,
            text,
            sealed,
        } => {
            let i = find_contact(world, &contact)?;
            world.say(i, &text, sealed, None, true)?;
            println!("sent by {}", world.contacts[i].key);
        }
        ReplCommand::Burst { contact, n } => {
            let i = find_contact(world, &contact)?;
            let base = now_ms();
            for m in 1..=n {
                world.say(
                    i,
                    &format!("Message {m} of {n}"),
                    m % 2 == 0,
                    Some(base + u64::from(m)),
                    false,
                )?;
            }
            world.settle()?;
            println!("{n} messages delivered");
        }
        ReplCommand::Photo { contact, caption } => {
            let i = find_contact(world, &contact)?;
            world.send_photo(i, &scene_png(SCENE.0, SCENE.1), SCENE, caption.as_deref())?;
            println!("photo sent by {}", world.contacts[i].key);
        }
        ReplCommand::Inbox => {
            let mut any = false;
            for i in 0..world.contacts.len() {
                while let Ok(m) = world.received_by(i, Duration::from_millis(200)) {
                    any = true;
                    println!(
                        "{} got: {}",
                        world.contacts[i].key,
                        m["text"].as_str().unwrap_or("")
                    );
                    let pointers = m["dataMessage"]["attachments"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    for pointer in &pointers {
                        println!("    + {}", attachment_line(world, pointer));
                    }
                }
            }
            if !any {
                println!("nothing new");
            }
        }
        ReplCommand::Contacts => {
            for c in &world.contacts {
                println!("{} — shown as {}", c.key, c.name);
            }
        }
        ReplCommand::Help => println!("{REPL_HELP}"),
        ReplCommand::Quit | ReplCommand::Nothing => {}
    }
    Ok(())
}

/// One line about an attachment Kourier sent: its type, name and size, and whether the
/// contact's phone could download and decrypt it.
fn attachment_line(world: &mut World, pointer: &Value) -> String {
    let kind = pointer["contentType"].as_str().unwrap_or("file");
    let name = pointer["fileName"].as_str().unwrap_or("(no name)");
    match world.fetch_attachment(pointer) {
        Ok(got) if got["macValid"] == true && got["digestValid"] == true => format!(
            "{kind} {name}: {} bytes, downloaded and decrypted",
            got["size"]
        ),
        Ok(_) => format!("{kind} {name}: downloaded, but its MAC or digest is WRONG"),
        Err(e) => format!("{kind} {name}: not downloadable ({e})"),
    }
}

// ---- Synthetic avatars -------------------------------------------------------------------------

/// Side of the avatar images.
const AVATAR_SIZE: u32 = 160;

/// A synthetic avatar for contact `index` (the group is `CONTACTS.len()`): a 160 × 160 RGB PNG
/// of a small motif (mountains at sunrise, a board of tiles, a camera aperture, a climbing wall),
/// so the screenshots show real images and no two chats in the list look alike.
fn avatar_png(index: usize) -> Vec<u8> {
    let size = AVATAR_SIZE as usize;
    let mut raw = Vec::with_capacity(size * (size * 3 + 1));
    for y in 0..AVATAR_SIZE {
        raw.push(0); // filter: none
        for x in 0..AVATAR_SIZE {
            raw.extend_from_slice(&avatar_pixel(index % 4, x, y));
        }
    }
    png(AVATAR_SIZE, AVATAR_SIZE, &raw)
}

/// Size of the demo photo.
const SCENE: (u32, u32) = (960, 640);

/// A synthetic landscape photo: a dusk sky, a low sun and two mountain ridges.
fn scene_png(width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as f32, height as f32);
    let (sun_x, sun_y, sun_r) = (w * 0.68, h * 0.58, h * 0.09);
    let mut raw = Vec::with_capacity(height as usize * (width as usize * 3 + 1));
    for y in 0..height {
        raw.push(0); // filter: none
        let fy = y as f32;
        for x in 0..width {
            let fx = x as f32;
            let t = fy / h;
            let mut px = if t < 0.5 {
                mix([38, 44, 112], [214, 96, 88], t * 2.0)
            } else {
                mix([214, 96, 88], [252, 186, 92], (t - 0.5) * 2.0)
            };
            let d = ((fx - sun_x).powi(2) + (fy - sun_y).powi(2)).sqrt();
            if d < sun_r {
                px = [255, 236, 170];
            } else if d < sun_r * 2.2 {
                px = mix([255, 214, 140], px, (d - sun_r) / (sun_r * 1.2));
            }
            let far = h * 0.62 + (fx / w * 9.0).sin() * h * 0.05 + (fx / w * 23.0).cos() * h * 0.02;
            let near = h * 0.76
                + (fx / w * 5.0 + 1.3).sin() * h * 0.07
                + (fx / w * 31.0).sin() * h * 0.015;
            if fy > near {
                px = mix(
                    [40, 34, 52],
                    [22, 18, 30],
                    (fy - near) / (h - near).max(1.0),
                );
            } else if fy > far {
                px = mix(
                    [96, 62, 92],
                    [70, 48, 76],
                    (fy - far) / (near - far).max(1.0),
                );
            }
            raw.extend_from_slice(&px);
        }
    }
    png(width, height, &raw)
}

fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    let channel = |i: usize| (f32::from(a[i]) * (1.0 - t) + f32::from(b[i]) * t).round() as u8;
    [channel(0), channel(1), channel(2)]
}

fn avatar_pixel(motif: usize, x: u32, y: u32) -> [u8; 3] {
    let (fx, fy) = (x as f32, y as f32);
    let size = AVATAR_SIZE as f32;
    match motif {
        // Mountains at sunrise.
        0 => {
            let sky = mix([255, 196, 140], [90, 150, 220], fy / (size * 0.7));
            let sun = ((fx - 104.0).powi(2) + (fy - 58.0).powi(2)).sqrt() < 20.0;
            let far = fy > 96.0 - (fx - 40.0).abs() * 0.55;
            let near = fy > 124.0 - (fx - 118.0).abs() * 0.7;
            if near {
                mix([46, 110, 74], [24, 70, 48], (fy - 90.0) / 70.0)
            } else if far {
                mix([96, 120, 160], [70, 92, 130], (fy - 60.0) / 80.0)
            } else if sun {
                [255, 244, 200]
            } else {
                sky
            }
        }
        // A board of tiles.
        1 => {
            let base = mix([255, 170, 70], [214, 70, 60], (fx + fy) / (2.0 * size));
            let (cx, cy) = (x / 20, y / 20);
            let inside = (2..6).contains(&cx) && (2..6).contains(&cy);
            if inside && (cx + cy) % 2 == 0 {
                [255, 236, 210]
            } else if inside {
                mix(base, [120, 40, 40], 0.35)
            } else {
                base
            }
        }
        // A climbing wall: holds on a teal panel.
        3 => {
            let base = mix(
                [64, 184, 176],
                [24, 104, 128],
                (fy + fx * 0.3) / (size * 1.2),
            );
            let holds: [(f32, f32, f32, [u8; 3]); 6] = [
                (44.0, 40.0, 11.0, [250, 204, 72]),
                (100.0, 54.0, 9.0, [236, 92, 84]),
                (66.0, 86.0, 12.0, [255, 255, 255]),
                (118.0, 104.0, 10.0, [250, 204, 72]),
                (40.0, 122.0, 9.0, [236, 92, 84]),
                (92.0, 134.0, 11.0, [255, 255, 255]),
            ];
            holds
                .iter()
                .find(|(hx, hy, r, _)| ((fx - hx).powi(2) + (fy - hy).powi(2)).sqrt() < *r)
                .map_or(base, |(_, _, _, color)| *color)
        }
        // A camera aperture.
        _ => {
            let d = ((fx - size / 2.0).powi(2) + (fy - size / 2.0).powi(2)).sqrt();
            let base = mix([236, 120, 200], [96, 60, 170], d / (size * 0.7));
            if d < 18.0 {
                [30, 24, 48]
            } else if ((d / 12.0).floor() as u32).is_multiple_of(2) && d < 60.0 {
                mix(base, [255, 255, 255], 0.35)
            } else {
                base
            }
        }
    }
}

/// An 8-bit RGB PNG of `raw` scanlines (each starting with its filter byte).
fn png(width: u32, height: u32, raw: &[u8]) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    png_chunk(&mut out, b"IHDR", &header);
    png_chunk(&mut out, b"IDAT", &zlib_stored(raw));
    png_chunk(&mut out, b"IEND", &[]);
    out
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// zlib with uncompressed ("stored") deflate blocks: small code, valid PNG data.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = data.chunks(0xffff).collect();
    if blocks.is_empty() {
        z.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    for (i, block) in blocks.iter().enumerate() {
        z.push(u8::from(i + 1 == blocks.len()));
        let len = u16::try_from(block.len()).unwrap_or(u16::MAX);
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(block);
    }
    z.extend_from_slice(&adler32(data).to_be_bytes());
    z
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for byte in data {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

/// Standard base64 with padding (the harness takes avatars as base64).
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Result<DemoArgs, String> {
        parse_args(&a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
    }

    /// The demo's media check passes only for a block of the right kind and side whose
    /// attachment is on this device and shown.
    #[test]
    fn media_checks_need_a_local_shown_block_on_the_right_side() {
        let state = |media: Value| json!({"messages": 5, "media": media});
        let photo = json!({"own": false, "block": "image", "state": "local", "shown": true});
        let file = json!({"own": true, "block": "file", "state": "local", "shown": true});
        let both = state(json!([file, photo]));
        assert!(media_shown(&both, "image", false));
        assert!(media_shown(&both, "file", true));
        assert!(!media_shown(&both, "image", true), "not our photo");
        assert!(!media_shown(&both, "file", false), "not the peer's file");
        for (key, value) in [("state", json!("remote")), ("shown", json!(false))] {
            let mut pending = photo.clone();
            pending[key] = value;
            assert!(
                !media_shown(&state(json!([pending])), "image", false),
                "{key}"
            );
        }
        assert!(!media_shown(&state(json!([])), "image", false));
        assert!(
            !media_shown(&json!({"messages": 5}), "image", false),
            "an app without the field"
        );
    }

    #[test]
    fn demo_arguments() {
        assert_eq!(args(&[]), Ok(DemoArgs::default()));
        assert_eq!(
            args(&["--out", "x/y"]),
            Ok(DemoArgs {
                out: Some("x/y".into()),
                interactive: false
            })
        );
        assert_eq!(
            args(&["--out=z"]).map(|a| a.out),
            Ok(Some(PathBuf::from("z")))
        );
        assert_eq!(
            args(&["--interactive"]),
            Ok(DemoArgs {
                out: None,
                interactive: true
            })
        );
        assert_eq!(args(&["--help"]), Err(String::new()));
        assert!(args(&["--out"]).is_err());
        assert!(args(&["--out="]).is_err());
        assert!(args(&["--out", "a", "--out", "b"]).is_err());
        assert!(args(&["--interactive", "--out", "a"]).is_err());
        assert!(args(&["--fast"]).is_err());
    }

    #[test]
    fn prompt_lines() {
        assert_eq!(
            parse_repl("ada hello there "),
            Ok(ReplCommand::Say {
                contact: "ada".into(),
                text: "hello there".into(),
                sealed: false
            })
        );
        assert_eq!(
            parse_repl("sealed Bru psst"),
            Ok(ReplCommand::Say {
                contact: "bru".into(),
                text: "psst".into(),
                sealed: true
            })
        );
        assert_eq!(
            parse_repl("burst chiara"),
            Ok(ReplCommand::Burst {
                contact: "chiara".into(),
                n: 20
            })
        );
        assert_eq!(
            parse_repl("burst c 5"),
            Ok(ReplCommand::Burst {
                contact: "c".into(),
                n: 5
            })
        );
        assert_eq!(
            parse_repl("photo Ada  Look at this "),
            Ok(ReplCommand::Photo {
                contact: "ada".into(),
                caption: Some("Look at this".into())
            })
        );
        assert_eq!(
            parse_repl("photo bruno"),
            Ok(ReplCommand::Photo {
                contact: "bruno".into(),
                caption: None
            })
        );
        assert_eq!(parse_repl("  "), Ok(ReplCommand::Nothing));
        assert_eq!(parse_repl("inbox"), Ok(ReplCommand::Inbox));
        assert_eq!(parse_repl("EXIT"), Ok(ReplCommand::Quit));
        assert_eq!(parse_repl("contacts"), Ok(ReplCommand::Contacts));
        for bad in [
            "ada",
            "sealed ada",
            "photo",
            "burst",
            "burst c 0",
            "burst c 501",
            "burst c 2 3",
        ] {
            assert!(parse_repl(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn helpers() {
        assert_eq!(node_major("v24.21.0\n"), Some(24));
        assert_eq!(node_major("22.1"), None);
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
        assert_eq!(crc32(b"IEND"), 0xae42_6082);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
        for i in 0..CONTACTS.len() {
            let png = avatar_png(i);
            assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
            assert!(
                png.ends_with(&[0xae, 0x42, 0x60, 0x82]),
                "IEND with its CRC"
            );
        }
        // Every contact and the group (index `CONTACTS.len()`) look different in the list.
        let all: Vec<Vec<u8>> = (0..=CONTACTS.len()).map(avatar_png).collect();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b, "avatar {i} repeats");
            }
        }
        if let Some(dir) = std::env::var_os("KOURIER_DEMO_AVATARS_OUT") {
            for i in 0..=CONTACTS.len() {
                std::fs::write(
                    Path::new(&dir).join(format!("avatar-{i}.png")),
                    avatar_png(i),
                )
                .unwrap();
            }
        }
        assert!(is_shot_name("03-link-qr.png", ""));
        assert!(is_shot_name("d1-03-link-qr.png", "d1-"));
        assert!(!is_shot_name("d1-03-link-qr.png", ""));
        assert!(!is_shot_name("holiday.png", ""));
        assert!(!is_shot_name("03-.png", ""));
        assert!(!is_shot_name("s1-03-x.png", "d1-"));
    }
}
